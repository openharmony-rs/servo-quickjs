/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! QuickJS shim: classic scripts compiled off the main thread with `mozjs::offthread`, as Servo
//! compiles them with its `dom_script_asynch` preference, and instantiated from their stencils.

use std::cell::RefCell;
use std::ffi::{c_void, CString};
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::{Arc, OnceLock};

use mozjs::context::JSContext;
use mozjs::glue::CompileGlobalScriptToStencil_C;
use mozjs::jsapi::{
    self, HadFrontendErrors, HandleObject, JSContext as RawJSContext, JSScript,
    OnNewGlobalHookOption, PromiseRejectionHandlingState,
};
use mozjs::jsval::{JSVal, PrivateValue, UndefinedValue};
use mozjs::offthread::{compile_to_stencil_offthread, CompilationResult, FrontendContext};
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    Compile1, ConvertFrontendErrorsToRuntimeErrors, InstantiateGlobalStencil,
    JS_ClearPendingException, JS_ExecuteScript, JS_GetPendingException, JS_NewGlobalObject,
    JS_SetProperty,
};
use mozjs::rust::{
    evaluate_script, transform_str_to_source_text, CompileOptionsWrapper,
    HandleObject as SafeHandleObject, JSEngine, JSEngineHandle, OwningCompileOptionsWrapper,
    RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};

/// The engine can be initialized once per process; the tests share it and never shut it down.
fn engine() -> JSEngineHandle {
    static ENGINE: OnceLock<JSEngineHandle> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let engine = JSEngine::init().unwrap();
            let handle = engine.handle();
            std::mem::forget(engine);
            handle
        })
        .clone()
}

/// Runs `f` in a new global's realm (and runtime).
fn with_global(f: impl FnOnce(&mut JSContext, SafeHandleObject)) {
    let mut runtime = Runtime::new(engine());
    let cx = runtime.cx();
    let options = RealmOptions::default();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*options,
        ));
        let mut realm = mozjs::realm::AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        f(cx, global.handle());
    }
}

fn options(cx: &mut JSContext, url: &str, line: u32, muted: bool) -> CompileOptionsWrapper {
    let options = CompileOptionsWrapper::new(cx, CString::new(url).unwrap(), line);
    unsafe { (*options.ptr)._base.mutedErrors_ = muted };
    options
}

/// Compiles `source` on another thread and waits for the result.
fn compile_off_thread(
    cx: &mut JSContext,
    url: &str,
    line: u32,
    muted: bool,
    source: &str,
) -> CompilationResult {
    let options = options(cx, url, line, muted);
    compile_to_stencil_offthread(options.ptr, Arc::new(source.to_owned()), Some)
        .finish()
        .expect("the compilation thread ended without a result")
}

/// Instantiates the stencil of `result` in the current realm.
unsafe fn instantiate(cx: &mut JSContext, result: &mut CompilationResult) -> *mut JSScript {
    let instantiate_options = (&result.compile_options).into();
    let script = InstantiateGlobalStencil(
        cx,
        &instantiate_options,
        **result.stencil.as_ref().expect("no stencil"),
        result.storage.as_mut_ptr(),
    );
    assert!(!script.is_null());
    script
}

unsafe fn execute(cx: &mut JSContext, script: *mut JSScript) -> Result<JSVal, ()> {
    rooted!(&in(cx) let script = script);
    rooted!(&in(cx) let mut rval = UndefinedValue());
    if JS_ExecuteScript(cx, script.handle(), rval.handle_mut()) {
        Ok(rval.get())
    } else {
        Err(())
    }
}

fn eval(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"test.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
    rval.get()
}

/// Moves the pending exception to the global property `name`.
unsafe fn take_exception(cx: &mut JSContext, global: SafeHandleObject, name: &str) {
    rooted!(&in(cx) let mut exception = UndefinedValue());
    assert!(JS_GetPendingException(cx, exception.handle_mut()));
    JS_ClearPendingException(cx);
    let name = CString::new(name).unwrap();
    assert!(JS_SetProperty(
        cx,
        global,
        name.as_ptr(),
        exception.handle()
    ));
}

#[test]
fn runs_a_script_compiled_off_thread() {
    with_global(|cx, global| unsafe {
        let source = "var double = function (a) { /* twice */ return a * 2; };\ndouble(21)";
        let mut result = compile_off_thread(cx, "https://test/a.js", 1, false, source);
        assert!(!HadFrontendErrors(*result.fc));
        let script = instantiate(cx, &mut result);
        assert_eq!(execute(cx, script).unwrap().to_int32(), 42);
        // Global declarations and function source texts are those of the script.
        let check = "double.toString() === 'function (a) { /* twice */ return a * 2; }'";
        assert!(eval(cx, global, check).to_boolean());
    });
}

#[test]
fn template_objects_are_frozen() {
    with_global(|cx, _| unsafe {
        let source = "const strings = (s => s)`a${1}b`;\n\
                      Object.isFrozen(strings) && Object.isFrozen(strings.raw)";
        let mut result = compile_off_thread(cx, "https://test/h.js", 1, false, source);
        let script = instantiate(cx, &mut result);
        assert!(execute(cx, script).unwrap().to_boolean());
    });
}

#[test]
fn reports_positions_as_a_script_compiled_here() {
    with_global(|cx, global| unsafe {
        let source = "\n\n(function thrower() {\n  throw new Error('x');\n})();";
        let mut result = compile_off_thread(cx, "https://test/b.js", 5, false, source);
        let script = instantiate(cx, &mut result);
        assert!(execute(cx, script).is_err());
        take_exception(cx, global, "offThread");

        let options = options(cx, "https://test/b.js", 5, false);
        let script = Compile1(cx, options.ptr, &mut transform_str_to_source_text(source));
        assert!(!script.is_null());
        assert!(execute(cx, script).is_err());
        take_exception(cx, global, "here");

        assert!(eval(cx, global, "offThread.stack === here.stack").to_boolean());
        assert!(eval(cx, global, "offThread.stack.includes('b.js:8')").to_boolean());
    });
}

#[test]
fn syntax_errors_throw_as_compiling_here_would() {
    with_global(|cx, global| unsafe {
        let source = "var ok = 1;\nvar broken = ;";
        let result = compile_off_thread(cx, "https://test/c.js", 3, false, source);
        assert!(result.stencil.is_none());
        assert!(HadFrontendErrors(*result.fc));
        ConvertFrontendErrorsToRuntimeErrors(cx, *result.fc, result.compile_options.read_only());
        take_exception(cx, global, "offThread");

        let options = options(cx, "https://test/c.js", 3, false);
        let script = Compile1(cx, options.ptr, &mut transform_str_to_source_text(source));
        assert!(script.is_null());
        take_exception(cx, global, "here");

        let check = "offThread instanceof SyntaxError && String(offThread) === String(here) && \
                     offThread.stack === here.stack";
        assert!(eval(cx, global, check).to_boolean());
    });
}

#[test]
fn failures_that_compile_here_throw_their_message() {
    with_global(|cx, global| unsafe {
        // Too deep for the stack of the compilation, not for this thread's, also under ASan.
        let source = format!("{}1{}", "(".repeat(100), ")".repeat(100));
        let options = options(cx, "https://test/d.js", 1, false);
        let here = Compile1(cx, options.ptr, &mut transform_str_to_source_text(&source));
        assert!(!here.is_null());
        let fc = FrontendContext::new();
        fc.set_stack_quota(4 * 1024);
        let owning = OwningCompileOptionsWrapper::new_for_fc(&fc, options.ptr);
        let mut stencil = MaybeUninit::uninit();
        CompileGlobalScriptToStencil_C(
            *fc,
            owning.read_only(),
            &mut transform_str_to_source_text(&source),
            stencil.as_mut_ptr(),
        );
        assert!(stencil.assume_init().mRawPtr.is_null());
        assert!(HadFrontendErrors(*fc));
        ConvertFrontendErrorsToRuntimeErrors(cx, *fc, owning.read_only());
        take_exception(cx, global, "e");
        // QuickJS's parser throws a SyntaxError when it runs out of stack.
        let check = "e instanceof SyntaxError && e.message === 'stack overflow'";
        assert!(eval(cx, global, check).to_boolean());
    });
}

#[test]
fn each_instantiation_is_a_script_of_its_own() {
    with_global(|cx, _| unsafe {
        let mut result = compile_off_thread(cx, "https://test/e.js", 1, false, "1");
        let first = instantiate(cx, &mut result);
        let second = instantiate(cx, &mut result);
        jsapi::SetScriptPrivate(first, &PrivateValue(1 as *const c_void));
        let private = |script| {
            let mut private = UndefinedValue();
            mozjs::glue::JS_GetScriptPrivate(
                script,
                jsapi::MutableHandle {
                    ptr: &mut private,
                    _phantom_0: std::marker::PhantomData,
                },
            );
            private
        };
        assert_eq!(private(first).to_private(), 1 as *const c_void);
        assert!(private(second).is_undefined());
        drop(result);
        assert_eq!(execute(cx, second).unwrap().to_int32(), 1);
    });
}

thread_local!(static MUTED: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) });

unsafe extern "C" fn track_rejection(
    _: *mut RawJSContext,
    muted_errors: bool,
    _: HandleObject,
    _: PromiseRejectionHandlingState,
    _: *mut c_void,
) {
    MUTED.with(|m| m.borrow_mut().push(muted_errors));
}

#[test]
fn muted_errors_stay_muted() {
    with_global(|cx, global| unsafe {
        jsapi::SetPromiseRejectionTrackerCallback(
            cx.raw_cx(),
            Some(track_rejection),
            ptr::null_mut(),
        );
        for muted in [true, false] {
            let source = "Promise.reject(1)";
            let mut result = compile_off_thread(cx, "https://test/f.js", 1, muted, source);
            let script = instantiate(cx, &mut result);
            execute(cx, script).unwrap();
        }
        // A function of a muted script stays muted when another script calls it.
        let source = "function later() { Promise.reject(2); }";
        let mut result = compile_off_thread(cx, "https://test/g.js", 1, true, source);
        let script = instantiate(cx, &mut result);
        execute(cx, script).unwrap();
        eval(cx, global, "later()");
        assert_eq!(MUTED.with(|m| m.borrow().clone()), [true, false, true]);
        jsapi::SetPromiseRejectionTrackerCallback(cx.raw_cx(), None, ptr::null_mut());
    });
}
