/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS shim: event-handler style compilation through an environment chain.

use std::ptr;
use std::sync::OnceLock;

use mozjs::jsapi::{
    HandleValueArray, JS_GetFunctionObject, JS_NewPlainObject, OnNewGlobalHookOption,
    SupportUnscopables,
};
use mozjs::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use mozjs::rooted;
use mozjs::rust::wrappers2::{Call, CompileFunction, JS_NewGlobalObject, JS_SetProperty};
use mozjs::rust::{
    evaluate_script, transform_u16_to_source_text, CompileOptionsWrapper, EnvironmentChain,
    HandleObject, JSEngine, JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
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

fn eval(cx: &mut mozjs::context::JSContext, global: HandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"test.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
    rval.get()
}

#[test]
fn compile_function_with_environment_chain() {
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

        // outer: { a: 1, b: 1 }, inner: { b: 2 } -> a + b == 3; `event` is the argument.
        rooted!(&in(cx) let outer = JS_NewPlainObject(cx.raw_cx()));
        rooted!(&in(cx) let inner = JS_NewPlainObject(cx.raw_cx()));
        rooted!(&in(cx) let one = Int32Value(1));
        rooted!(&in(cx) let two = Int32Value(2));
        assert!(JS_SetProperty(
            cx,
            outer.handle(),
            c"a".as_ptr(),
            one.handle()
        ));
        assert!(JS_SetProperty(
            cx,
            outer.handle(),
            c"b".as_ptr(),
            one.handle()
        ));
        assert!(JS_SetProperty(
            cx,
            inner.handle(),
            c"b".as_ptr(),
            two.handle()
        ));

        let chain = EnvironmentChain::new(cx, SupportUnscopables::Yes);
        chain.append(outer.get());
        chain.append(inner.get());

        let body: Vec<u16> = "return a + b + event;".encode_utf16().collect();
        let args = [c"event".as_ptr()];
        let options = CompileOptionsWrapper::new(cx, c"handler.html".to_owned(), 7);
        rooted!(&in(cx) let fun = CompileFunction(
            cx,
            chain.get(),
            options.ptr,
            c"onclick".as_ptr(),
            1,
            args.as_ptr(),
            &mut transform_u16_to_source_text(&body),
        ));
        assert!(!fun.get().is_null());

        rooted!(&in(cx) let funval = mozjs::jsval::ObjectValue(JS_GetFunctionObject(fun.get())));
        rooted!(&in(cx) let this = UndefinedValue());
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let argv = [Int32Value(10)];
        let argv = HandleValueArray {
            length_: 1,
            elements_: argv.as_ptr(),
        };
        assert!(Call(
            cx,
            this.handle(),
            funval.handle(),
            &argv,
            rval.handle_mut()
        ));
        assert_eq!(rval.get().to_int32(), 13);
    }
}

/// Compiles `body` as the function `onload(event)` with an empty environment chain.
unsafe fn compile_onload(cx: &mut mozjs::context::JSContext, body: &str) -> JSVal {
    let source: Vec<u16> = body.encode_utf16().collect();
    let args = [c"event".as_ptr()];
    let chain = EnvironmentChain::new(cx, SupportUnscopables::Yes);
    let options = CompileOptionsWrapper::new(cx, c"handler.html".to_owned(), 1);
    let fun = CompileFunction(
        cx,
        chain.get(),
        options.ptr,
        c"onload".as_ptr(),
        1,
        args.as_ptr(),
        &mut transform_u16_to_source_text(&source),
    );
    assert!(!fun.is_null());
    ObjectValue(JS_GetFunctionObject(fun))
}

/// SpiderMonkey compiles the function as a standalone function, whose name is not bound in its
/// own scope, so a page's `<body onload="onload()">` calls the page's global `onload` function
/// instead of the handler itself.
#[test]
fn the_name_of_a_compiled_function_is_not_bound_in_its_scope() {
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
        eval(cx, global.handle(), "var onload = 42;");

        let body = "return [onload, (function g() { return typeof g; })()].join();";
        rooted!(&in(cx) let handler = compile_onload(cx, body));
        assert!(JS_SetProperty(
            cx,
            global.handle(),
            c"handler".as_ptr(),
            handler.handle()
        ));
        rooted!(&in(cx) let assigns = compile_onload(cx, "onload = 7; return typeof onload;"));
        assert!(JS_SetProperty(
            cx,
            global.handle(),
            c"assigns".as_ptr(),
            assigns.handle()
        ));

        let is = |cx: &mut mozjs::context::JSContext, check: &str| {
            eval(cx, global.handle(), check).to_boolean()
        };
        // The function sees the global; a named function expression in its body keeps its own
        // binding.
        assert!(is(cx, "handler() === '42,function'"));
        // Its name and source text are those of `function onload(event)`.
        assert!(is(cx, "handler.name === 'onload'"));
        let source = format!("function onload(event) {{\n{body}\n}}");
        assert!(is(cx, &format!("handler.toString() === {source:?}")));
        // Assigning to the name assigns to the global.
        assert!(is(cx, "assigns() === 'number' && onload === 7"));
    }
}
