/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Error reports, scripted callers and saved-frame stacks on QuickJS.

use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr::{self, NonNull};

use mozjs::context::{JSContext, RawJSContext};
use mozjs::jsapi::{
    self, Handle, HandleValueArray, JSObject, JSString, MutableHandleValue, OnNewGlobalHookOption,
    SavedFrameResult, SavedFrameSelfHosted, StackCapture, StackFormat, TaggedColumnNumberOneOrigin,
    Value,
};
use mozjs::jsval::{JSVal, NullValue, ObjectValue, StringValue, UndefinedValue};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::JS_NewGlobalObject;
use mozjs::rust::{
    describe_scripted_caller, error_info_from_exception_stack, evaluate_script,
    CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS,
};

fn with_global(engine: &JSEngine, f: impl FnOnce(&mut JSContext, SafeHandleObject)) {
    let mut runtime = Runtime::new(engine.handle());
    let context = runtime.cx();
    unsafe {
        rooted!(&in(context) let global = JS_NewGlobalObject(
            context,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = AutoRealm::new_from_handle(context, global.handle());
        let (global, realm) = realm.global_and_reborrow();
        f(realm, global);
    }
}

fn eval_in(
    cx: &mut JSContext,
    global: SafeHandleObject,
    file: &CStr,
    source: &str,
) -> Result<JSVal, ()> {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, file.to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options)?;
    Ok(rval.get())
}

fn eval_bool(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> bool {
    let v = eval_in(cx, global, c"check.js", source).unwrap_or_else(|_| panic!("{source} threw"));
    assert!(v.is_boolean());
    v.to_boolean()
}

unsafe fn handle<T>(ptr: &T) -> Handle<T> {
    Handle::from_marked_location(ptr)
}

/// The report of the exception thrown by `source`, which is taken.
fn thrown(
    cx: &mut JSContext,
    global: SafeHandleObject,
    file: &CStr,
    source: &str,
) -> mozjs::rust::ErrorInfo {
    assert!(eval_in(cx, global, file, source).is_err());
    assert!(unsafe { jsapi::JS_IsExceptionPending(cx.raw_cx()) });
    rooted!(&in(cx) let mut exception = UndefinedValue());
    let info = error_info_from_exception_stack(cx, exception.handle_mut()).unwrap();
    assert!(!unsafe { jsapi::JS_IsExceptionPending(cx.raw_cx()) });
    info
}

fn reports(cx: &mut JSContext, global: SafeHandleObject) {
    let info = thrown(
        cx,
        global,
        c"err.js",
        "function f() {\n  throw new TypeError('boom');\n}\nf();",
    );
    assert_eq!(info.message, "boom");
    assert_eq!(info.filename, "err.js");
    // QuickJS locates calls and `new` at the opening parenthesis.
    assert_eq!((info.line, info.col), (2, 22));

    let info = thrown(cx, global, c"err.js", "\nthrow 42;");
    assert_eq!(info.message, "uncaught exception: 42");
    assert_eq!(
        (info.filename.as_str(), info.line, info.col),
        ("err.js", 2, 1)
    );

    // Exceptions thrown by natives are located at the calling frame.
    unsafe {
        let f = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
            cx.raw_cx(),
            Some(throw_value),
            0,
            0,
            c"throwValue".as_ptr(),
        ));
        let global_obj = global.get();
        assert!(jsapi::JS_SetProperty(
            cx.raw_cx(),
            handle(&global_obj),
            c"throwValue".as_ptr(),
            handle(&ObjectValue(f))
        ));
    }
    let info = thrown(cx, global, c"native.js", "\n\nthrowValue();");
    assert_eq!(info.message, "uncaught exception: 5");
    assert_eq!((info.filename.as_str(), info.line), ("native.js", 3));
    let info = thrown(
        cx,
        global,
        c"native.js",
        "function g() {\n  throw { toString() { return 'obj'; } };\n}\ng();",
    );
    assert_eq!(info.message, "uncaught exception: obj");
    assert_eq!((info.filename.as_str(), info.line), ("native.js", 2));

    let info = thrown(cx, global, c"syntax.js", "\nvar x = ;");
    assert!(!info.message.is_empty());
    assert_eq!(info.filename, "syntax.js");
    assert_eq!(info.line, 2);

    unsafe {
        let error = eval_in(cx, global, c"report.js", "\n\n  new RangeError('bad')")
            .unwrap()
            .to_object();
        mozjs::rust::borrowed_error_report(cx, |cx, report| {
            assert!(jsapi::JS_ErrorFromException(
                cx.raw_cx_no_gc(),
                handle(&error),
                report
            ));
            let report = &(*report.report_)._base;
            assert_eq!(CStr::from_ptr(report.message_.data_), c"bad");
            assert_eq!(CStr::from_ptr(report.filename.data_), c"report.js");
            assert_eq!((report.lineno, report.column._base), (3, 17));
        });

        let plain = eval_in(cx, global, c"report.js", "({ message: 'no' })")
            .unwrap()
            .to_object();
        mozjs::rust::borrowed_error_report(cx, |cx, report| {
            assert!(!jsapi::JS_ErrorFromException(
                cx.raw_cx_no_gc(),
                handle(&plain),
                report
            ));
        });
    }
}

unsafe extern "C" fn throw_value(cx: *mut RawJSContext, _argc: u32, _vp: *mut Value) -> bool {
    jsapi::JS_SetPendingException(
        cx,
        handle(&mozjs::jsval::Int32Value(5)),
        jsapi::ExceptionStackBehavior::Capture,
    );
    false
}

thread_local! {
    static GLOBAL: RefCell<*mut JSObject> = const { RefCell::new(ptr::null_mut()) };
    static CALLERS: RefCell<Vec<Option<(String, u32, u32)>>> = const { RefCell::new(Vec::new()) };
    static LINES: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
}

fn record_caller(cx: &mut JSContext) {
    let caller = describe_scripted_caller(cx)
        .ok()
        .map(|c| (c.filename, c.line, c.col));
    let global = unsafe { jsapi::GetScriptedCallerGlobal(cx.raw_cx()) };
    assert_eq!(global.is_null(), caller.is_none());
    if !global.is_null() {
        assert_eq!(global, GLOBAL.with(|g| *g.borrow()));
    }
    CALLERS.with(|c| c.borrow_mut().push(caller));
}

unsafe fn set_global(cx: &mut JSContext, name: &CStr, value: JSVal) {
    let global = GLOBAL.with(|g| *g.borrow());
    assert!(jsapi::JS_SetProperty(
        cx.raw_cx(),
        handle(&global),
        name.as_ptr(),
        handle(&value)
    ));
}

unsafe fn capture(cx: &mut JSContext, max: Option<u32>) -> *mut JSObject {
    let mut capture = std::mem::MaybeUninit::<StackCapture>::uninit();
    match max {
        Some(max) => jsapi::JS_StackCapture_MaxFrames(max, capture.as_mut_ptr()),
        None => jsapi::JS_StackCapture_AllFrames(capture.as_mut_ptr()),
    }
    let mut stack: *mut JSObject = ptr::null_mut();
    let none: *mut JSObject = ptr::null_mut();
    assert!(jsapi::CaptureCurrentStack(
        cx.raw_cx(),
        jsapi::MutableHandleObject::from_marked_location(&mut stack),
        capture.as_mut_ptr(),
        handle(&none),
    ));
    stack
}

unsafe fn stack_string(cx: &mut JSContext, stack: *mut JSObject, format: StackFormat) -> JSVal {
    let mut s: *mut JSString = ptr::null_mut();
    assert!(jsapi::BuildStackString(
        cx.raw_cx(),
        ptr::null_mut(),
        handle(&stack),
        jsapi::MutableHandle::from_marked_location(&mut s),
        0,
        format,
    ));
    StringValue(&*s)
}

/// Records the scripted caller (also while hidden, and from a callback run while hidden), and
/// the current stack as saved frames.
unsafe extern "C" fn probe(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let cx = &mut cx;
    record_caller(cx);

    jsapi::HideScriptedCaller(cx.raw_cx());
    record_caller(cx);
    let global = GLOBAL.with(|g| *g.borrow());
    let mut callback = UndefinedValue();
    assert!(jsapi::JS_GetProperty(
        cx.raw_cx(),
        handle(&global),
        c"callback".as_ptr(),
        MutableHandleValue::from_marked_location(&mut callback)
    ));
    let args = HandleValueArray {
        length_: 0,
        elements_: ptr::null(),
    };
    let mut rval = UndefinedValue();
    assert!(jsapi::Call(
        cx.raw_cx(),
        handle(&UndefinedValue()),
        handle(&callback),
        &args,
        MutableHandleValue::from_marked_location(&mut rval)
    ));
    jsapi::UnhideScriptedCaller(cx.raw_cx());
    record_caller(cx);

    let stack = capture(cx, None);
    let mut frame = stack;
    let mut names = Vec::new();
    while !frame.is_null() {
        let mut line = 0;
        let mut column = TaggedColumnNumberOneOrigin { value_: 0 };
        let mut name: *mut JSString = ptr::null_mut();
        let mut source: *mut JSString = ptr::null_mut();
        let mut parent: *mut JSObject = ptr::null_mut();
        let include = SavedFrameSelfHosted::Include;
        let null = ptr::null_mut();
        assert_eq!(
            jsapi::GetSavedFrameLine(cx.raw_cx(), null, handle(&frame), &mut line, include),
            SavedFrameResult::Ok
        );
        assert_eq!(
            jsapi::GetSavedFrameColumn(cx.raw_cx(), null, handle(&frame), &mut column, include),
            SavedFrameResult::Ok
        );
        assert_eq!(
            jsapi::GetSavedFrameFunctionDisplayName(
                cx.raw_cx(),
                null,
                handle(&frame),
                jsapi::MutableHandle::from_marked_location(&mut name),
                include
            ),
            SavedFrameResult::Ok
        );
        assert_eq!(
            jsapi::GetSavedFrameSource(
                cx.raw_cx(),
                null,
                handle(&frame),
                jsapi::MutableHandle::from_marked_location(&mut source),
                include
            ),
            SavedFrameResult::Ok
        );
        assert_eq!(
            jsapi::GetSavedFrameParent(
                cx.raw_cx(),
                null,
                handle(&frame),
                jsapi::MutableHandle::from_marked_location(&mut parent),
                include
            ),
            SavedFrameResult::Ok
        );
        LINES.with(|l| l.borrow_mut().push((line, column.value_)));
        names.push(if name.is_null() {
            NullValue()
        } else {
            StringValue(&*name)
        });
        names.push(StringValue(&*source));
        frame = parent;
    }
    let mut line = 0;
    let none: *mut JSObject = ptr::null_mut();
    assert_eq!(
        jsapi::GetSavedFrameLine(
            cx.raw_cx(),
            ptr::null_mut(),
            handle(&none),
            &mut line,
            SavedFrameSelfHosted::Include
        ),
        SavedFrameResult::AccessDenied
    );

    let names_array = {
        let mut array = UndefinedValue();
        assert!(jsapi::JS_GetProperty(
            cx.raw_cx(),
            handle(&global),
            c"names".as_ptr(),
            MutableHandleValue::from_marked_location(&mut array)
        ));
        array.to_object()
    };
    for (i, name) in names.iter().enumerate() {
        assert!(jsapi::JS_SetElement(
            cx.raw_cx(),
            handle(&names_array),
            i as u32,
            handle(name)
        ));
    }
    let s = stack_string(cx, stack, StackFormat::SpiderMonkey);
    set_global(cx, c"stack", s);
    let s = stack_string(cx, stack, StackFormat::V8);
    set_global(cx, c"v8Stack", s);
    let top = capture(cx, Some(2));
    let s = stack_string(cx, top, StackFormat::Default);
    set_global(cx, c"topStack", s);

    let args = jsapi::CallArgs::from_vp(vp, argc);
    args.rval().set(UndefinedValue());
    true
}

unsafe extern "C" fn probe_callback(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    record_caller(&mut cx);
    jsapi::CallArgs::from_vp(vp, argc)
        .rval()
        .set(UndefinedValue());
    true
}

fn stacks(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        GLOBAL.with(|g| *g.borrow_mut() = global.get());
        let global_obj = global.get();
        for (name, native) in [
            (c"probe", probe as unsafe extern "C" fn(_, _, _) -> _),
            (c"probeCallback", probe_callback),
        ] {
            let f = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
                cx.raw_cx(),
                Some(native),
                0,
                0,
                name.as_ptr(),
            ));
            assert!(jsapi::JS_SetProperty(
                cx.raw_cx(),
                handle(&global_obj),
                name.as_ptr(),
                handle(&ObjectValue(f))
            ));
        }
        let script = "var names = [];
function foo() {
  function bar() {
    function baz() {
      probe();
    }
    baz();
  }
  bar();
}
function callback() { probeCallback(); }
foo();";
        eval_in(cx, global, c"stack.js", script).unwrap();
    }
    let callers = CALLERS.with(|c| c.borrow().clone());
    assert_eq!(
        callers,
        [
            Some(("stack.js".to_owned(), 5, 11)),
            None,
            Some(("stack.js".to_owned(), 11, 35)),
            Some(("stack.js".to_owned(), 5, 11)),
        ]
    );
    assert_eq!(
        LINES.with(|l| l.borrow().clone()),
        [(5, 12), (7, 8), (9, 6), (12, 4)]
    );
    assert!(eval_bool(
        cx,
        global,
        "names.join() === 'baz,stack.js,bar,stack.js,foo,stack.js,,stack.js'"
    ));
    assert!(eval_bool(
        cx,
        global,
        "stack === 'baz@stack.js:5:12\\nbar@stack.js:7:8\\nfoo@stack.js:9:6\\n@stack.js:12:4\\n'"
    ));
    assert!(eval_bool(
        cx,
        global,
        "v8Stack.split('\\n')[0] === '    at baz (stack.js:5:12)' && v8Stack.split('\\n')[3] === '    at stack.js:12:4'"
    ));
    assert!(eval_bool(
        cx,
        global,
        "topStack === 'baz@stack.js:5:12\\nbar@stack.js:7:8\\n'"
    ));
    unsafe {
        // Outside of script there is no caller.
        assert!(jsapi::GetScriptedCallerGlobal(cx.raw_cx()).is_null());
    }
}

static DOM_ERROR_CLASS: jsapi::JSClass = jsapi::JSClass {
    name: c"DOMError".as_ptr(),
    flags: jsapi::JSCLASS_IS_DOMJSCLASS,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

unsafe extern "C" fn instance_class_is_error(clasp: *const jsapi::JSClass) -> bool {
    clasp == &DOM_ERROR_CLASS as *const _
}

static DOM_CALLBACKS: jsapi::DOMCallbacks = jsapi::DOMCallbacks {
    instanceClassMatchesProto: None,
    instanceClassIsError: Some(instance_class_is_error),
    extractExceptionInfo: None,
};

/// `Error.isError` asks the embedder about DOM objects, which may have [[ErrorData]].
fn dom_errors(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        mozjs::rust::wrappers2::SetDOMCallbacks(cx, &DOM_CALLBACKS);
        rooted!(&in(cx) let obj = mozjs::rust::wrappers2::JS_NewObject(cx, &DOM_ERROR_CLASS));
        rooted!(&in(cx) let v = ObjectValue(obj.get()));
        assert!(jsapi::JS_SetProperty(
            cx.raw_cx(),
            Handle::from_marked_location(&global.get()),
            c"domError".as_ptr(),
            Handle::from_marked_location(&v.get()),
        ));
        assert!(eval_bool(
            cx,
            global,
            "Error.isError(domError) && Error.isError(new TypeError()) && !Error.isError({})"
        ));
        mozjs::rust::wrappers2::SetDOMCallbacks(cx, ptr::null());
        assert!(eval_bool(cx, global, "!Error.isError(domError)"));
    }
}

#[test]
fn errors() {
    let engine = JSEngine::init().unwrap();
    with_global(&engine, reports);
    with_global(&engine, stacks);
    with_global(&engine, dom_errors);
}
