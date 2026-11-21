/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Error reports, scripted callers and saved-frame stacks.

use std::cell::RefCell;
use std::ffi::CStr;
#[cfg(unix)]
use std::ffi::{c_int, c_void};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::FromRawFd;
use std::ptr::{self, NonNull};

use js::context::{JSContext, RawJSContext};
use js::jsapi::{
    self, Handle, HandleValueArray, JSObject, JSString, MutableHandleValue, OnNewGlobalHookOption,
    SavedFrameResult, SavedFrameSelfHosted, StackCapture, StackFormat, TaggedColumnNumberOneOrigin,
    Value,
};
use js::jsval::{JSVal, NullValue, ObjectValue, StringValue, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::JS_NewGlobalObject;
use js::rust::{
    CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS, describe_scripted_caller, error_info_from_exception_stack,
    evaluate_script, transform_str_to_source_text,
};

use crate::support::{Value as Observed, observe_field};

#[cfg(unix)]
unsafe extern "C" {
    fn pipe(fds: *mut c_int) -> c_int;
    fn dup(fd: c_int) -> c_int;
    fn dup2(old: c_int, new: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn fflush(stream: *mut c_void) -> c_int;
    static mut stdout: *mut c_void;
}

/// Captures a C `printf`/Rust `println!` stack dump by redirecting fd 1 to a pipe.
#[cfg(unix)]
fn capture_stdout(f: impl FnOnce()) -> String {
    struct RestoreStdout {
        saved: c_int,
        active: bool,
    }

    impl Drop for RestoreStdout {
        fn drop(&mut self) {
            if self.active {
                unsafe {
                    fflush(stdout);
                    let _ = dup2(self.saved, 1);
                    close(self.saved);
                }
            }
        }
    }

    unsafe {
        let mut fds = [0; 2];
        assert_eq!(pipe(fds.as_mut_ptr()), 0);
        fflush(stdout);
        let saved = dup(1);
        assert!(saved >= 0);
        // From here on a panic must still put fd 1 back: otherwise later output goes into a pipe
        // nobody reads and the suite hangs once that pipe fills.
        let mut restore = RestoreStdout {
            saved,
            active: true,
        };
        assert_eq!(dup2(fds[1], 1), 1);
        close(fds[1]);

        // Drain while the producer runs. A dump larger than the pipe buffer would otherwise
        // block the writer forever because nobody reads before `f` returns.
        let mut pipe = std::fs::File::from_raw_fd(fds[0]);
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            pipe.read_to_end(&mut output).expect("reading dump");
            output
        });

        f();

        let _ = std::io::stdout().flush();
        fflush(stdout);
        assert_eq!(dup2(saved, 1), 1);
        restore.active = false;
        close(restore.saved);

        let output = reader.join().expect("joining the stdout reader");
        String::from_utf8_lossy(&output).into_owned()
    }
}

#[cfg(not(unix))]
fn capture_stdout(f: impl FnOnce()) -> String {
    f();
    String::new()
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

unsafe extern "C" fn dump_stack_from_script(
    cx: *mut RawJSContext,
    _argc: u32,
    vp: *mut Value,
) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    js::rust::wrappers2::DumpJSStack(&mut cx, false, false, false);
    jsapi::CallArgs::from_vp(vp, _argc)
        .rval()
        .set(UndefinedValue());
    true
}

fn dump_stack(cx: &mut JSContext, global: SafeHandleObject) -> String {
    unsafe {
        let function = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
            cx.raw_cx(),
            Some(dump_stack_from_script),
            0,
            0,
            c"dumpTheStack".as_ptr(),
        ));
        assert!(jsapi::JS_SetProperty(
            cx.raw_cx(),
            handle(&global.get()),
            c"dumpTheStack".as_ptr(),
            handle(&ObjectValue(function)),
        ));
    }
    capture_stdout(|| {
        eval_in(
            cx,
            global,
            c"dump-stack.js",
            "function outer() { dumpTheStack(); }\nouter();",
        )
        .expect("dump stack script");
    })
}

fn assert_dump_stack_output(output: &str) {
    assert!(
        output.contains("outer"),
        "DumpJSStack did not print the scripted frame: {output:?}"
    );
    assert!(
        output.contains("dump-stack.js"),
        "DumpJSStack did not print the source name: {output:?}"
    );
}

#[cfg(unix)]
fn capture_stdout_large() -> String {
    let body = "x".repeat(200 * 1024);
    capture_stdout(|| {
        let mut out = std::io::stdout();
        out.write_all(body.as_bytes())
            .expect("writing a large dump");
        out.write_all(b"\n").expect("finishing the large dump");
    })
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
    expected_name: Option<&str>,
) -> js::rust::ErrorInfo {
    assert!(eval_in(cx, global, file, source).is_err());
    assert!(unsafe { jsapi::JS_IsExceptionPending(cx.raw_cx()) });
    rooted!(&in(cx) let mut exception = UndefinedValue());
    let info = error_info_from_exception_stack(cx, exception.handle_mut()).unwrap();
    assert!(!unsafe { jsapi::JS_IsExceptionPending(cx.raw_cx()) });
    if let Some(expected) = expected_name {
        let name = crate::support::observe_property(cx, exception.get(), c"name");
        assert_eq!(
            name,
            Observed::String(expected.to_owned()),
            "unexpected exception name"
        );
    }
    info
}

fn report_thrown_error_location(cx: &mut JSContext, global: SafeHandleObject) {
    // Observation is assembled by `stacks`; this helper only establishes the thrown report.
    let info = thrown(
        cx,
        global,
        c"err.js",
        "function f() {\n  throw new TypeError('boom');\n}\nf();",
        Some("TypeError"),
    );
    assert_eq!(info.message, "boom");
    assert_eq!(info.filename, "err.js");
    assert_eq!((info.line, info.col), (2, 9));
}

fn report_primitive_throw(cx: &mut JSContext, global: SafeHandleObject) {
    let info = thrown(cx, global, c"err.js", "\nthrow 42;", None);
    assert_eq!(info.message, "uncaught exception: 42");
    assert_eq!(
        (info.filename.as_str(), info.line, info.col),
        ("err.js", 2, 1)
    );
}

fn report_native_throws(cx: &mut JSContext, global: SafeHandleObject) {
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
    let info = thrown(cx, global, c"native.js", "\n\nthrowValue();", None);
    assert_eq!(info.message, "uncaught exception: 5");
    assert_eq!((info.filename.as_str(), info.line), ("native.js", 3));
    let info = thrown(
        cx,
        global,
        c"native.js",
        "function g() {\n  throw { toString() { return 'obj'; } };\n}\ng();",
        None,
    );
    assert_eq!(info.message, "uncaught exception: obj");
    assert_eq!((info.filename.as_str(), info.line), ("native.js", 2));
}

fn report_syntax_error(cx: &mut JSContext, global: SafeHandleObject) {
    let info = thrown(cx, global, c"syntax.js", "\nvar x = ;", Some("SyntaxError"));
    assert!(!info.message.is_empty());
    assert_eq!(info.filename, "syntax.js");
    assert_eq!(info.line, 2);
}

fn report_error_from_exception(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        let error = eval_in(cx, global, c"report.js", "\n\n  new RangeError('bad')")
            .unwrap()
            .to_object();
        js::rust::borrowed_error_report(cx, |cx, report| {
            assert!(jsapi::JS_ErrorFromException(
                cx.raw_cx_no_gc(),
                handle(&error),
                report
            ));
            let report = &(*(*report).report_)._base;
            assert_eq!(CStr::from_ptr(report.message_.data_), c"bad");
            assert_eq!(CStr::from_ptr(report.filename.data_), c"report.js");
            assert_eq!((report.lineno, report.column._base), (3, 3));
        });

        let plain = eval_in(cx, global, c"report.js", "({ message: 'no' })")
            .unwrap()
            .to_object();
        js::rust::borrowed_error_report(cx, |cx, report| {
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
        handle(&js::jsval::Int32Value(5)),
        jsapi::ExceptionStackBehavior::Capture,
    );
    false
}

thread_local! {
    static GLOBAL: RefCell<*mut JSObject> = const { RefCell::new(ptr::null_mut()) };
    static CALLERS: RefCell<Vec<Option<(String, u32, u32)>>> = const { RefCell::new(Vec::new()) };
    static CALLER_PRIVATES: RefCell<Vec<Option<i32>>> = const { RefCell::new(Vec::new()) };
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
    rooted!(&in(cx) let mut private = UndefinedValue());
    unsafe {
        js::rust::wrappers2::JS_GetScriptedCallerPrivate(cx, private.handle_mut());
    }
    let private = if private.get().is_undefined() {
        None
    } else {
        Some(private.get().to_int32())
    };
    CALLER_PRIVATES.with(|p| p.borrow_mut().push(private));
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
    let mut subsumed = std::mem::MaybeUninit::<StackCapture>::uninit();
    jsapi::JS_StackCapture_FirstSubsumedFrame(cx.raw_cx(), true, subsumed.as_mut_ptr());
    let mut subsumed_stack: *mut JSObject = ptr::null_mut();
    let none: *mut JSObject = ptr::null_mut();
    assert!(jsapi::CaptureCurrentStack(
        cx.raw_cx(),
        jsapi::MutableHandleObject::from_marked_location(&mut subsumed_stack),
        subsumed.as_mut_ptr(),
        handle(&none),
    ));
    assert!(!subsumed_stack.is_null());
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

thread_local! {
    static ERRORS_OBSERVATION: RefCell<Vec<(&'static str, Observed)>> = const { RefCell::new(Vec::new()) };
}

fn stacks(cx: &mut JSContext, global: SafeHandleObject) -> Vec<(&'static str, Observed)> {
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
        let source = "var names = [];
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
        let options = CompileOptionsWrapper::new(cx, c"stack.js".to_owned(), 1);
        rooted!(&in(cx) let script = jsapi::Compile1(
            cx.raw_cx(),
            options.ptr,
            &mut transform_str_to_source_text(source),
        ));
        assert!(!script.get().is_null(), "compiling the stack script");
        jsapi::SetScriptPrivate(script.get(), &js::jsval::Int32Value(77));
        rooted!(&in(cx) let mut rval = UndefinedValue());
        assert!(js::rust::wrappers2::JS_ExecuteScript(
            cx,
            script.handle(),
            rval.handle_mut()
        ));
    }
    let callers = CALLERS.with(|c| c.borrow().clone());
    let privates = CALLER_PRIVATES.with(|p| p.borrow().clone());
    assert_eq!(callers.len(), 4);
    assert_eq!(
        privates,
        [Some(77), Some(77), Some(77), Some(77)],
        "JS_GetScriptedCallerPrivate must return the script's private value"
    );
    assert_eq!(callers[1], None);
    for (index, caller) in callers.into_iter().enumerate() {
        if index == 1 {
            continue;
        }
        let (filename, line, col) = caller.expect("scripted caller");
        assert_eq!(filename, "stack.js");
        assert!(line > 0 && col > 0);
    }
    assert_eq!(
        LINES.with(|l| l.borrow().clone()),
        [(5, 7), (7, 5), (9, 3), (12, 1)]
    );
    assert!(eval_bool(
        cx,
        global,
        "names.join() === 'baz,stack.js,bar,stack.js,foo,stack.js,,stack.js'"
    ));
    assert!(eval_bool(
        cx,
        global,
        "stack === 'baz@stack.js:5:7\\nbar@stack.js:7:5\\nfoo@stack.js:9:3\\n@stack.js:12:1\\n'"
    ));
    assert!(eval_bool(
        cx,
        global,
        "v8Stack.split('\\n')[0] === '    at baz (stack.js:5:7)' && v8Stack.split('\\n')[3] === '    at stack.js:12:1'"
    ));
    assert!(eval_bool(
        cx,
        global,
        "topStack === 'baz@stack.js:5:7\\nbar@stack.js:7:5\\n'"
    ));
    unsafe {
        // Outside of script there is no caller.
        assert!(jsapi::GetScriptedCallerGlobal(cx.raw_cx()).is_null());
    }
    let callers = CALLERS.with(|c| c.borrow().clone());
    let lines = LINES.with(|l| l.borrow().clone());
    vec![
        ("callers", Observed::String(format!("{callers:?}"))),
        ("caller_privates", Observed::String(format!("{privates:?}"))),
        ("lines", Observed::String(format!("{lines:?}"))),
        (
            "names",
            Observed::String("baz,stack.js,bar,stack.js,foo,stack.js,,stack.js".to_owned()),
        ),
        (
            "spidermonkey_stack",
            Observed::String(
                "baz@stack.js:5:7\nbar@stack.js:7:5\nfoo@stack.js:9:3\n@stack.js:12:1\n".to_owned(),
            ),
        ),
        (
            "v8_stack_first",
            Observed::String("    at baz (stack.js:5:7)".to_owned()),
        ),
        (
            "max_frames_stack",
            Observed::String("baz@stack.js:5:7\nbar@stack.js:7:5\n".to_owned()),
        ),
    ]
}

const STACK_STEP_SCRIPT: &str = "function probeTarget() {\n  probe();\n}\nprobeTarget();";

thread_local! {
    static STACK_STEP_RESULT: RefCell<String> = const { RefCell::new(String::new()) };
}

fn stack_capture_or_error(cx: &mut JSContext) -> Result<*mut JSObject, String> {
    unsafe {
        let mut capture = std::mem::MaybeUninit::<StackCapture>::uninit();
        jsapi::JS_StackCapture_AllFrames(capture.as_mut_ptr());
        let mut stack: *mut JSObject = ptr::null_mut();
        let none: *mut JSObject = ptr::null_mut();
        if !jsapi::CaptureCurrentStack(
            cx.raw_cx(),
            jsapi::MutableHandleObject::from_marked_location(&mut stack),
            capture.as_mut_ptr(),
            handle(&none),
        ) {
            return Err("CaptureCurrentStack returned false".to_owned());
        }
        if stack.is_null() {
            Err("CaptureCurrentStack returned null".to_owned())
        } else {
            Ok(stack)
        }
    }
}

fn set_stack_step_result(value: &str) {
    STACK_STEP_RESULT.with(|result| *result.borrow_mut() = value.to_owned());
}

fn take_stack_step_result() -> String {
    STACK_STEP_RESULT.with(|result| std::mem::take(&mut *result.borrow_mut()))
}

/// Runs `probe` directly from a scripted call site, so each split stack step exercises exactly
/// one JSAPI and records the value that API produced.
fn eval_stack_probe(
    engine: &JSEngine,
    native: unsafe extern "C" fn(*mut RawJSContext, u32, *mut Value) -> bool,
) -> String {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(!global.get().is_null(), "JS_NewGlobalObject returned null");
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let (global, cx) = realm.global_and_reborrow();
        let function = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
            cx.raw_cx(),
            Some(native),
            0,
            0,
            c"probe".as_ptr(),
        ));
        assert!(jsapi::JS_SetProperty(
            cx.raw_cx(),
            handle(&global.get()),
            c"probe".as_ptr(),
            handle(&ObjectValue(function)),
        ));
        eval_in(cx, global, c"capture.js", STACK_STEP_SCRIPT).expect("stack probe script");
    }
    take_stack_step_result()
}

fn run_stack_step(name: &'static str, probe: impl FnOnce() -> String, expected: &str) {
    take_stack_step_result();
    let observed = probe();
    assert!(!observed.is_empty(), "{name} probe did not record a result");
    observe_field(name, Observed::String(observed.clone()));
    assert_eq!(observed, expected, "{name}");
}

unsafe extern "C" fn capture_current_stack_probe(
    cx: *mut RawJSContext,
    argc: u32,
    vp: *mut Value,
) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let value = match stack_capture_or_error(&mut cx) {
        Ok(stack) => {
            let mut line = 0;
            let result = jsapi::GetSavedFrameLine(
                cx.raw_cx(),
                ptr::null_mut(),
                handle(&stack),
                &mut line,
                SavedFrameSelfHosted::Include,
            );
            if result == SavedFrameResult::Ok {
                format!("{line}")
            } else {
                format!("{result:?}")
            }
        },
        Err(error) => error,
    };
    set_stack_step_result(&value);
    jsapi::CallArgs::from_vp(vp, argc)
        .rval()
        .set(UndefinedValue());
    true
}

unsafe extern "C" fn build_stack_string_probe(
    cx: *mut RawJSContext,
    argc: u32,
    vp: *mut Value,
) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let value = match stack_capture_or_error(&mut cx) {
        Ok(stack) => {
            let mut string: *mut JSString = ptr::null_mut();
            if jsapi::BuildStackString(
                cx.raw_cx(),
                ptr::null_mut(),
                handle(&stack),
                jsapi::MutableHandle::from_marked_location(&mut string),
                0,
                StackFormat::SpiderMonkey,
            ) && !string.is_null()
            {
                js::conversions::jsstr_to_string(&cx, NonNull::new(string).unwrap())
            } else {
                "BuildStackString returned null".to_owned()
            }
        },
        Err(error) => error,
    };
    set_stack_step_result(&value);
    jsapi::CallArgs::from_vp(vp, argc)
        .rval()
        .set(UndefinedValue());
    true
}

unsafe extern "C" fn get_saved_frame_line_probe(
    cx: *mut RawJSContext,
    argc: u32,
    vp: *mut Value,
) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let value = match stack_capture_or_error(&mut cx) {
        Ok(stack) => {
            let mut line = 0;
            let result = jsapi::GetSavedFrameLine(
                cx.raw_cx(),
                ptr::null_mut(),
                handle(&stack),
                &mut line,
                SavedFrameSelfHosted::Include,
            );
            if result == SavedFrameResult::Ok {
                format!("{line}")
            } else {
                format!("{result:?}")
            }
        },
        Err(error) => error,
    };
    set_stack_step_result(&value);
    jsapi::CallArgs::from_vp(vp, argc)
        .rval()
        .set(UndefinedValue());
    true
}

unsafe extern "C" fn get_saved_frame_column_probe(
    cx: *mut RawJSContext,
    argc: u32,
    vp: *mut Value,
) -> bool {
    let mut cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let value = match stack_capture_or_error(&mut cx) {
        Ok(stack) => {
            let mut column = TaggedColumnNumberOneOrigin { value_: 0 };
            let result = jsapi::GetSavedFrameColumn(
                cx.raw_cx(),
                ptr::null_mut(),
                handle(&stack),
                &mut column,
                SavedFrameSelfHosted::Include,
            );
            if result == SavedFrameResult::Ok {
                format!("{}", column.value_)
            } else {
                format!("{result:?}")
            }
        },
        Err(error) => error,
    };
    set_stack_step_result(&value);
    jsapi::CallArgs::from_vp(vp, argc)
        .rval()
        .set(UndefinedValue());
    true
}

unsafe extern "C" fn describe_scripted_caller_probe(
    cx: *mut RawJSContext,
    argc: u32,
    vp: *mut Value,
) -> bool {
    let cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let value = match describe_scripted_caller(&cx) {
        Ok(caller) => format!("{}:{}:{}", caller.filename, caller.line, caller.col),
        Err(()) => "DescribeScriptedCaller failed".to_owned(),
    };
    set_stack_step_result(&value);
    jsapi::CallArgs::from_vp(vp, argc)
        .rval()
        .set(UndefinedValue());
    true
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
///
/// The callbacks stay installed for the rest of this runtime: SpiderMonkey dereferences
/// `Runtime::DOMcallbacks` whenever it sees a `JSCLASS_IS_DOMJSCLASS` object, so clearing them
/// while such an object is still reachable is a null-pointer crash. Servo installs them once at
/// runtime setup, which is what this mirrors; `with_global` gives each step its own runtime.
fn dom_errors(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        js::rust::wrappers2::SetDOMCallbacks(cx, &DOM_CALLBACKS);
        rooted!(&in(cx) let obj = js::rust::wrappers2::JS_NewObject(cx, &DOM_ERROR_CLASS));
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
    }
}

pub fn run(engine: &JSEngine) {
    let mut steps = crate::support::Steps::new(
        "errors.core",
        &[
            "BuildStackString",
            "CaptureCurrentStack",
            "DescribeScriptedCaller",
            "GetSavedFrameColumn",
            "GetSavedFrameFunctionDisplayName",
            "GetSavedFrameLine",
            "GetSavedFrameParent",
            "GetSavedFrameSource",
            "GetScriptedCallerGlobal",
            "HideScriptedCaller",
            "JS_ErrorFromException",
            "JS_NewObject",
            "JS_GetFunctionObject",
            "JS_GetScriptedCallerPrivate",
            "JS_NewFunction",
            "SetDOMCallbacks",
            "JS_SetPendingException",
            "DumpJSStack",
            "JS_StackCapture_AllFrames",
            "JS_StackCapture_FirstSubsumedFrame",
            "JS_StackCapture_MaxFrames",
            "PendingExceptionStackInfo",
            "UnhideScriptedCaller",
        ],
    );
    steps.step("report_thrown_error_location", || {
        crate::support::with_global(engine, report_thrown_error_location)
    });
    steps.step("report_primitive_throw", || {
        crate::support::with_global(engine, report_primitive_throw)
    });
    steps.step("report_native_throws", || {
        crate::support::with_global(engine, report_native_throws)
    });
    steps.step("report_syntax_error", || {
        crate::support::with_global(engine, report_syntax_error)
    });
    steps.step("report_error_from_exception", || {
        crate::support::with_global(engine, report_error_from_exception)
    });
    steps.step("dom_errors", || {
        crate::support::with_global(engine, dom_errors)
    });
    steps.step("stacks", || {
        crate::support::with_global(engine, |cx, global| {
            let stack_fields = stacks(cx, global);
            ERRORS_OBSERVATION.with(|observation| *observation.borrow_mut() = stack_fields);
        })
    });
    steps.step("capture_current_stack", || {
        run_stack_step(
            "capture_current_stack",
            || eval_stack_probe(engine, capture_current_stack_probe),
            "2",
        )
    });
    steps.step("build_stack_string", || {
        run_stack_step(
            "build_stack_string",
            || eval_stack_probe(engine, build_stack_string_probe),
            "probeTarget@capture.js:2:3\n@capture.js:4:1\n",
        )
    });
    steps.step("get_saved_frame_line", || {
        run_stack_step(
            "get_saved_frame_line",
            || eval_stack_probe(engine, get_saved_frame_line_probe),
            "2",
        )
    });
    steps.step("get_saved_frame_column", || {
        run_stack_step(
            "get_saved_frame_column",
            || eval_stack_probe(engine, get_saved_frame_column_probe),
            "3",
        )
    });
    steps.step("describe_scripted_caller", || {
        run_stack_step(
            "describe_scripted_caller",
            || eval_stack_probe(engine, describe_scripted_caller_probe),
            "capture.js:2:2",
        )
    });
    let dump_output = std::cell::RefCell::new(String::new());
    steps.step("dump_stack", || {
        crate::support::with_global(engine, |cx, global| {
            let output = dump_stack(cx, global);
            assert_dump_stack_output(&output);
            *dump_output.borrow_mut() = output;
        })
    });
    #[cfg(unix)]
    steps.step("capture_stdout_large", || {
        let output = capture_stdout_large();
        assert_eq!(output.len(), 200 * 1024 + 1, "large dump was truncated");
    });
    let mut fields =
        ERRORS_OBSERVATION.with(|observation| std::mem::take(&mut *observation.borrow_mut()));
    fields.push((
        "dump_contains_outer",
        Observed::Boolean(dump_output.borrow().contains("outer")),
    ));
    fields.push((
        "dump_contains_source",
        Observed::Boolean(dump_output.borrow().contains("dump-stack.js")),
    ));
    steps.finish_with_fields(fields);
}
