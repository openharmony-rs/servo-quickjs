/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Event-handler style compilation through an environment chain.

use std::ptr;

use js::jsapi::{
    HandleValueArray, JS_GetFunctionObject, JS_NewPlainObject, OnNewGlobalHookOption,
    SupportUnscopables,
};
use js::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use js::rooted;
use js::rust::wrappers2::{Call, CompileFunction, JS_NewGlobalObject, JS_SetProperty};
use js::rust::{
    CompileOptionsWrapper, EnvironmentChain, HandleObject, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS, evaluate_script, transform_u16_to_source_text,
};

fn eval(cx: &mut js::context::JSContext, global: HandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"test.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
    rval.get()
}

fn compile_function_with_environment_chain(engine: &JSEngine) {
    let mut runtime = Runtime::new(engine.handle());
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
        let mut realm = js::realm::AutoRealm::new_from_handle(cx, global.handle());
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
        // SpiderMonkey gives earlier appends precedence, so the inner scope is appended first.
        chain.append(inner.get());
        chain.append(outer.get());

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

        rooted!(&in(cx) let funval = js::jsval::ObjectValue(JS_GetFunctionObject(fun.get())));
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
        crate::support::observe_field(
            "environment_chain_result",
            crate::support::Value::Number(rval.get().to_int32() as f64),
        );
        crate::support::observe_field(
            "environment_chain_precedence",
            crate::support::Value::Boolean(true),
        );
    }
}

/// Compiles `body` as the function `onload(event)` with an empty environment chain.
unsafe fn compile_onload(cx: &mut js::context::JSContext, body: &str) -> JSVal {
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
fn the_name_of_a_compiled_function_is_not_bound_in_its_scope(engine: &JSEngine) {
    let mut runtime = Runtime::new(engine.handle());
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
        let mut realm = js::realm::AutoRealm::new_from_handle(cx, global.handle());
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

        let is = |cx: &mut js::context::JSContext, check: &str| {
            eval(cx, global.handle(), check).to_boolean()
        };
        // The function sees the global; a named function expression in its body keeps its own
        // binding.
        assert!(is(cx, "handler() === '42,function'"));
        crate::support::observe_field("handler_global_lookup", crate::support::Value::Number(42.0));
        // Its name and source text are those of `function onload(event)`.
        assert!(is(cx, "handler.name === 'onload'"));
        let source = format!("function onload(event) {{\n{body}\n}}");
        assert!(is(cx, &format!("handler.toString() === {source:?}")));
        // Assigning to the name assigns to the global.
        assert!(is(cx, "assigns() === 'number' && onload === 7"));
        crate::support::observe_field(
            "handler_name_scope_is_global",
            crate::support::Value::Number(7.0),
        );
    }
}

mod extra_contract {
    /* This Source Code Form is subject to the terms of the Mozilla Public
     * License, v. 2.0. If a copy of the MPL was not distributed with this
     * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

    //! Script compilation, evaluation and script-private contract.

    use js::jsapi::{HandleValueArray, SupportUnscopables};
    use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
    use js::rooted;
    use js::rust::wrappers2::{
        Call, Compile, Compile1, CompileFunctionUtf8, Evaluate, Evaluate2, JS_ExecuteScript,
        JS_ExecuteScript1, JS_GetScriptPrivate,
    };
    use js::rust::{
        CompileOptionsWrapper, EnvironmentChain, JSEngine, transform_str_to_source_text,
        transform_u16_to_source_text,
    };

    use crate::support::{Observation, Value as Observed, record_case, take_exception_named};

    pub fn run(engine: &JSEngine) {
        record_case(
            engine,
            "eval.compile_evaluate_execute",
            &[
                "Compile",
                "Compile1",
                "CompileFunctionUtf8",
                "DeleteCompileOptions",
                "Evaluate",
                "Evaluate2",
                "JS_ExecuteScript",
                "JS_ExecuteScript1",
                "JS_GetGlobalFromScript",
                "JS_ClearPendingException",
                "JS_GetPendingException",
                "JS_IsExceptionPending",
                "JS_GetScriptPrivate",
                "NewCompileOptions",
                "SetScriptPrivate",
            ],
            |cx, global| unsafe {
                let options = CompileOptionsWrapper::new(cx, c"eval-ext.js".to_owned(), 1);

                let utf8 = "1 + 2";
                let mut source = transform_str_to_source_text(utf8);
                rooted!(&in(cx) let script_utf8 = Compile1(cx, options.ptr, &mut source));
                assert!(!script_utf8.get().is_null());
                assert_eq!(
                    js::jsapi::JS_GetGlobalFromScript(script_utf8.get()),
                    global.get()
                );

                rooted!(&in(cx) let mut rval = UndefinedValue());
                assert!(JS_ExecuteScript(
                    cx,
                    script_utf8.handle(),
                    rval.handle_mut()
                ));
                assert_eq!(rval.get().to_int32(), 3);
                let compiled_result = rval.get().to_int32();

                let mut source = transform_str_to_source_text("throw new TypeError('boom')");
                rooted!(&in(cx) let throwing = Compile1(cx, options.ptr, &mut source));
                assert!(!throwing.get().is_null());
                assert!(!JS_ExecuteScript(cx, throwing.handle(), rval.handle_mut()));
                take_exception_named(cx, "TypeError");

                let utf16: Vec<u16> = "20 + 22".encode_utf16().collect();
                let mut source = transform_u16_to_source_text(&utf16);
                rooted!(&in(cx) let script_utf16 = Compile(cx, options.ptr, &mut source));
                assert!(!script_utf16.get().is_null());
                assert!(JS_ExecuteScript1(cx, script_utf16.handle()));
                rooted!(&in(cx) let mut check = UndefinedValue());
                assert!(js::rust::wrappers2::JS_GetProperty(
                    cx,
                    global,
                    c"globalThis".as_ptr(),
                    check.handle_mut()
                ));

                let mut source = transform_str_to_source_text("globalThis.evaluated = 7; 11");
                assert!(Evaluate2(cx, options.ptr, &mut source, rval.handle_mut()));
                assert_eq!(rval.get().to_int32(), 11);
                let evaluated_result = rval.get().to_int32();

                let utf16: Vec<u16> = "globalThis.evaluated + 9".encode_utf16().collect();
                let mut source = transform_u16_to_source_text(&utf16);
                assert!(Evaluate(cx, options.ptr, &mut source, rval.handle_mut()));
                assert_eq!(rval.get().to_int32(), 16);
                let evaluated_utf16_result = rval.get().to_int32();

                let utf16: Vec<u16> = "throw new RangeError('boom')".encode_utf16().collect();
                let mut source = transform_u16_to_source_text(&utf16);
                rooted!(&in(cx) let throwing = Compile(cx, options.ptr, &mut source));
                assert!(!throwing.get().is_null());
                assert!(!JS_ExecuteScript1(cx, throwing.handle()));
                take_exception_named(cx, "RangeError");

                let private = Int32Value(99);
                js::jsapi::SetScriptPrivate(script_utf8.get(), &private);
                rooted!(&in(cx) let mut private_out = UndefinedValue());
                JS_GetScriptPrivate(script_utf8.get(), private_out.handle_mut());
                assert_eq!(private_out.get().to_int32(), 99);
                let script_private = private_out.get().to_int32();

                let chain = EnvironmentChain::new(cx, SupportUnscopables::Yes);
                let body = c"return event + 1;";
                let args = [c"event".as_ptr()];
                rooted!(&in(cx) let function = CompileFunctionUtf8(
                    cx,
                    chain.get(),
                    options.ptr,
                    c"onmessage".as_ptr(),
                    1,
                    args.as_ptr(),
                    body.as_ptr() as *const _,
                    16,
                ));
                assert!(!function.get().is_null());
                rooted!(&in(cx) let function_value = ObjectValue(js::jsapi::JS_GetFunctionObject(function.get())));
                rooted!(&in(cx) let this = UndefinedValue());
                let argv = [Int32Value(41)];
                let argv = HandleValueArray {
                    length_: 1,
                    elements_: argv.as_ptr(),
                };
                assert!(Call(
                    cx,
                    this.handle(),
                    function_value.handle(),
                    &argv,
                    rval.handle_mut()
                ));
                assert_eq!(rval.get().to_int32(), 42);
                Observation::new(vec![
                    ("compiled_result", Observed::Number(compiled_result as f64)),
                    (
                        "evaluated_result",
                        Observed::Number(evaluated_result as f64),
                    ),
                    (
                        "evaluated_utf16_result",
                        Observed::Number(evaluated_utf16_result as f64),
                    ),
                    ("script_private", Observed::Number(script_private as f64)),
                    ("compiled_function_result", Observed::Number(42.0)),
                ])
            },
        );
    }
}

mod edge_eval_deep_contract {
    fn eval_compile_and_environment(engine: &JSEngine) {
        record_case(
            engine,
            "edge.eval.compile_and_environment",
            &[
                "AppendToEnvironmentChain",
                "Call",
                "Compile",
                "Compile1",
                "CompileFunction",
                "DeleteCompileOptions",
                "DeleteEnvironmentChain",
                "Evaluate",
                "Evaluate2",
                "JS_ExecuteScript",
                "JS_ExecuteScript1",
                "JS_GetGlobalFromScript",
                "JS_GetScriptPrivate",
                "NewCompileOptions",
                "NewEnvironmentChain",
                "SetScriptPrivate",
            ],
            |cx, global| unsafe {
                let options = CompileOptionsWrapper::new(cx, c"deep-eval.js".to_owned(), 1);
                let mut source = transform_str_to_source_text("1 + 2");
                rooted!(&in(cx) let script_utf8 = Compile1(cx, options.ptr, &mut source));
                let script_nonnull = !script_utf8.get().is_null();
                let script_global_matches =
                    jsapi::JS_GetGlobalFromScript(script_utf8.get()) == global.get();
                rooted!(&in(cx) let mut rval = UndefinedValue());
                assert!(JS_ExecuteScript(
                    cx,
                    script_utf8.handle(),
                    rval.handle_mut()
                ));
                let utf8_result = rval.get().to_int32();

                let private = Int32Value(99);
                jsapi::SetScriptPrivate(script_utf8.get(), &private);
                rooted!(&in(cx) let mut private_out = UndefinedValue());
                JS_GetScriptPrivate(script_utf8.get(), private_out.handle_mut());
                let script_private = private_out.get().to_int32();

                let utf16: Vec<u16> = "globalThis.deepEval = 20 + 22".encode_utf16().collect();
                let mut source = transform_u16_to_source_text(&utf16);
                rooted!(&in(cx) let script_utf16 = Compile(cx, options.ptr, &mut source));
                assert!(JS_ExecuteScript1(cx, script_utf16.handle()));
                let utf16_global = eval(cx, global, "deepEval").to_int32();

                let mut source = transform_str_to_source_text("globalThis.deepEval + 9");
                assert!(Evaluate2(cx, options.ptr, &mut source, rval.handle_mut()));
                let evaluate2_result = rval.get().to_int32();
                let utf16: Vec<u16> = "deepEval + 1".encode_utf16().collect();
                let mut source = transform_u16_to_source_text(&utf16);
                assert!(Evaluate(cx, options.ptr, &mut source, rval.handle_mut()));
                let evaluate_result = rval.get().to_int32();

                rooted!(&in(cx) let outer = js::rust::wrappers2::JS_NewPlainObject(cx));
                rooted!(&in(cx) let inner = js::rust::wrappers2::JS_NewPlainObject(cx));
                rooted!(&in(cx) let one = Int32Value(1));
                rooted!(&in(cx) let two = Int32Value(2));
                assert!(js::rust::wrappers2::JS_SetProperty(
                    cx,
                    outer.handle(),
                    c"a".as_ptr(),
                    one.handle()
                ));
                assert!(js::rust::wrappers2::JS_SetProperty(
                    cx,
                    outer.handle(),
                    c"b".as_ptr(),
                    one.handle()
                ));
                assert!(js::rust::wrappers2::JS_SetProperty(
                    cx,
                    inner.handle(),
                    c"b".as_ptr(),
                    two.handle()
                ));

                let chain = EnvironmentChain::new(cx, jsapi::JS::SupportUnscopables::Yes);
                chain.append(inner.get());
                chain.append(outer.get());
                let body: Vec<u16> = "return a + b + event;".encode_utf16().collect();
                let args = [c"event".as_ptr()];
                let handler_options =
                    CompileOptionsWrapper::new(cx, c"deep-handler.html".to_owned(), 7);
                rooted!(&in(cx) let function = CompileFunction(
                    cx,
                    chain.get(),
                    handler_options.ptr,
                    c"onclick".as_ptr(),
                    1,
                    args.as_ptr(),
                    &mut transform_u16_to_source_text(&body),
                ));
                rooted!(&in(cx) let function_value = ObjectValue(JS_GetFunctionObject(function.get())));
                rooted!(&in(cx) let this = UndefinedValue());
                let argv = [Int32Value(10)];
                let argv = jsapi::HandleValueArray {
                    length_: 1,
                    elements_: argv.as_ptr(),
                };
                assert!(Call(
                    cx,
                    this.handle(),
                    function_value.handle(),
                    &argv,
                    rval.handle_mut()
                ));
                let env_call_result = rval.get().to_int32();
                assert_eq!(
                    env_call_result, 13,
                    "inner scope must shadow the later appended outer scope"
                );
                drop(chain);

                Observation::new(vec![
                    ("script_nonnull", Value::Boolean(script_nonnull)),
                    (
                        "script_global_matches",
                        Value::Boolean(script_global_matches),
                    ),
                    ("utf8_result", Value::Number(utf8_result as f64)),
                    ("script_private", Value::Number(script_private as f64)),
                    ("utf16_global", Value::Number(utf16_global as f64)),
                    ("evaluate2_result", Value::Number(evaluate2_result as f64)),
                    ("evaluate_result", Value::Number(evaluate_result as f64)),
                    (
                        "environment_call_result",
                        Value::Number(env_call_result as f64),
                    ),
                ])
            },
        );
    }

    // ---------------------------------------------------------------------------
    // Symbols, integer ids and Latin-1 encoding.
    // ---------------------------------------------------------------------------

    use js::jsapi::{self, JS_GetFunctionObject};
    use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
    use js::rooted;
    use js::rust::wrappers2::{
        Call, Compile, Compile1, CompileFunction, Evaluate, Evaluate2, JS_ExecuteScript,
        JS_ExecuteScript1, JS_GetScriptPrivate,
    };
    use js::rust::{
        CompileOptionsWrapper, EnvironmentChain, JSEngine, transform_str_to_source_text,
        transform_u16_to_source_text,
    };

    use crate::support::{Observation, Value, eval, record_case};

    pub fn run(engine: &JSEngine) {
        eval_compile_and_environment(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_eval_deep_contract::run(engine);
    extra_contract::run(engine);
    crate::support::isolate_case("eval.environment_chain", || {
        let mut steps = crate::support::Steps::new(
            "eval.environment_chain",
            &[
                "AppendToEnvironmentChain",
                "CompileFunction",
                "NewEnvironmentChain",
                "DeleteEnvironmentChain",
                "JS_GetFunctionObject",
                "Call",
                "JS_NewGlobalObject",
                "JS_NewPlainObject",
                "JS_SetProperty",
            ],
        );
        steps.step("environment_chain", || {
            compile_function_with_environment_chain(engine)
        });
        steps.step("handler_name_scope", || {
            the_name_of_a_compiled_function_is_not_bound_in_its_scope(engine)
        });
        steps.finish();
    });
}
