/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The emulation of SpiderMonkey's `Debugger` API (`engine/debugger_api.js`), as Servo's
//! devtools (`debugger.js`) use it: a debugger global with `Debugger`, and a debuggee global.

use std::ffi::CStr;
use std::ptr::{self, NonNull};
use std::sync::{Arc, OnceLock};

use mozjs::context::{JSContext, RawJSContext};
use mozjs::conversions::jsstr_to_string;
use mozjs::glue::{CreateWrapperProxyHandler, GetWindowProxyClass, NewWindowProxy, ProxyTraps};
use mozjs::jsapi::{
    self, GCReason, JSObject, OnNewGlobalHookOption, SetWindowProxy, SetWindowProxyClass, Value,
};
use mozjs::jsval::{JSVal, ObjectValue, UndefinedValue};
use mozjs::offthread::compile_to_stencil_offthread;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    ClearKeptObjects, Compile, CompileFunction, CompileModule, CompileModule1, Evaluate,
    InstantiateGlobalStencil, JS_DefineDebuggerObject, JS_ExecuteScript, JS_NewGlobalObject,
    JS_SetProperty, JS_GC,
};
use mozjs::rust::{
    evaluate_script, transform_str_to_source_text, transform_u16_to_source_text,
    CompileOptionsWrapper, EnvironmentChain, HandleObject as SafeHandleObject, JSEngine,
    JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};

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

unsafe extern "C" fn terminate(_: *mut RawJSContext) -> bool {
    false
}

/// `requestTermination()`: requests an interrupt, which terminates the script at the next poll.
unsafe extern "C" fn request_termination(cx: *mut RawJSContext, _: u32, _: *mut Value) -> bool {
    jsapi::JS_RequestInterruptCallback(cx);
    true
}

/// Assertions for the test scripts, which return "ok" or the failure.
const PRELUDE: &str = "
function assertEq(actual, expected, what) {
  if (actual !== expected) throw new Error(`${what}: ${String(actual)}, expected ${String(expected)}`);
}
function assertThrows(f, what) {
  try { f(); } catch (e) { return e; }
  throw new Error(`${what}: did not throw`);
}
function test(f) {
  try { f(); return 'ok'; } catch (e) { return `${e}\\n${e.stack}`; }
}";

/// Evaluates `source` in `global` as a script of `url` with an introduction type.
fn eval_as(
    cx: &mut JSContext,
    global: SafeHandleObject,
    source: &str,
    url: &CStr,
    introduction_type: Option<&'static CStr>,
) -> Result<JSVal, ()> {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let mut options = CompileOptionsWrapper::new(cx, url.to_owned(), 1);
    if let Some(introduction_type) = introduction_type {
        options.set_introduction_type(introduction_type);
    }
    evaluate_script(cx, global, source, rval.handle_mut(), options)?;
    Ok(rval.get())
}

fn eval(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> JSVal {
    eval_as(cx, global, source, c"test.js", None).expect("evaluation failed")
}

/// Runs `body` (which may use `dbg`-free test helpers) in the debugger global inside `test()`.
fn check(cx: &mut JSContext, global: SafeHandleObject, body: &str) {
    let v = eval(cx, global, &format!("test(() => {{ {body} }})"));
    let s = unsafe { jsstr_to_string(cx, NonNull::new(v.to_string()).unwrap()) };
    assert_eq!(s, "ok");
}

unsafe fn new_global(cx: &mut JSContext) -> *mut JSObject {
    JS_NewGlobalObject(
        cx,
        &SIMPLE_GLOBAL_CLASS,
        ptr::null_mut(),
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*RealmOptions::default(),
    )
}

/// Defines `name` on `global` as `value`.
unsafe fn set_global(
    cx: &mut JSContext,
    global: SafeHandleObject,
    name: &CStr,
    value: *mut JSObject,
) {
    rooted!(&in(cx) let v = ObjectValue(value));
    assert!(JS_SetProperty(cx, global, name.as_ptr(), v.handle()));
}

/// Runs `f` with a debugger global (with `Debugger`, the test helpers and `debuggee`), the debuggee
/// global (with `requestTermination`) and a third global (`other` in the debugger global), in the
/// debugger global's realm.
fn with_globals(
    f: impl FnOnce(&mut JSContext, SafeHandleObject, SafeHandleObject, SafeHandleObject),
) {
    let mut runtime = Runtime::new(engine());
    let context = runtime.cx();
    unsafe {
        rooted!(&in(context) let debugger = new_global(context));
        rooted!(&in(context) let debuggee = new_global(context));
        rooted!(&in(context) let other = new_global(context));
        let mut realm = AutoRealm::new_from_handle(context, debugger.handle());
        let (global, cx) = realm.global_and_reborrow();
        assert!(JS_DefineDebuggerObject(cx, global));
        eval(cx, global, PRELUDE);
        set_global(cx, global, c"debuggee", debuggee.get());
        set_global(cx, global, c"other", other.get());
        assert!(jsapi::JS_AddInterruptCallback(cx.raw_cx(), Some(terminate)));
        {
            let mut inner = AutoRealm::new_from_handle(cx, debuggee.handle());
            let (debuggee_global, cx) = inner.global_and_reborrow();
            assert!(!jsapi::JS_DefineFunction(
                cx.raw_cx(),
                debuggee_global.into(),
                c"requestTermination".as_ptr(),
                Some(request_termination),
                0,
                0
            )
            .is_null());
        }
        f(cx, global, debuggee.handle(), other.handle());
    }
}

/// Evaluates `source` in the realm of `target`.
fn eval_in(
    cx: &mut JSContext,
    target: SafeHandleObject,
    source: &str,
    url: &CStr,
    introduction_type: Option<&'static CStr>,
) -> Result<JSVal, ()> {
    let mut realm = AutoRealm::new_from_handle(cx, target);
    let (target, cx) = realm.global_and_reborrow();
    eval_as(cx, target, source, url, introduction_type)
}

#[test]
fn debuggees() {
    with_globals(|cx, global, _, _| {
        check(
            cx,
            global,
            "
            assertEq(typeof Debugger, 'function', 'Debugger');
            assertThrows(() => Debugger(), 'call without new');
            const dbg = new Debugger;
            const g = dbg.addDebuggee(debuggee);
            assertEq(dbg.addDebuggee(debuggee), g, 'one Debugger.Object per global');
            assertEq(g.unsafeDereference(), debuggee, 'referent');
            assertEq(dbg.hasDebuggee(debuggee), true, 'hasDebuggee');
            assertEq(dbg.hasDebuggee(g), true, 'hasDebuggee of the Debugger.Object');
            assertEq(dbg.getDebuggees().length, 1, 'debuggees');
            assertThrows(() => dbg.addDebuggee({}), 'a plain object is no debuggee');
            const other = new Debugger(debuggee);
            assertEq(other.hasDebuggee(debuggee), true, 'constructor arguments');
            assertEq(dbg.adoptDebuggeeValue(other.addDebuggee(debuggee)), g, 'adoptDebuggeeValue');
            assertThrows(() => g.call.call(other.addDebuggee(debuggee)), 'Debugger.Object of another debugger');
            dbg.removeAllDebuggees();
            assertEq(dbg.getDebuggees().length, 0, 'removeAllDebuggees');
            assertThrows(() => { dbg.onNewScript = 1; }, 'hooks are functions');
            ",
        );
    });
}

#[test]
fn new_scripts() {
    with_globals(|cx, global, debuggee, other| {
        eval(
            cx,
            global,
            "var dbg = new Debugger; var g = dbg.addDebuggee(debuggee); var scripts = [];
             dbg.onNewScript = s => scripts.push(s);",
        );
        eval_in(
            cx,
            debuggee,
            "var a = 1;",
            c"https://example.com/a.js",
            Some(c"srcScript"),
        )
        .unwrap();
        eval_in(cx, other, "var b = 1;", c"https://example.com/b.js", None).unwrap();
        // A classic script compiled and executed separately, and a module.
        unsafe {
            let mut realm = AutoRealm::new_from_handle(cx, debuggee);
            let cx = &mut *realm;
            let mut options =
                CompileOptionsWrapper::new(cx, c"https://example.com/c.js".to_owned(), 3);
            options.set_introduction_type(c"inlineScript");
            let script = mozjs::rust::wrappers2::Compile1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("var c = 1;\nc"),
            );
            assert!(!script.is_null());
            rooted!(&in(cx) let script = script);
            rooted!(&in(cx) let mut rval = UndefinedValue());
            assert!(JS_ExecuteScript(cx, script.handle(), rval.handle_mut()));
            let mut options =
                CompileOptionsWrapper::new(cx, c"https://example.com/m.js".to_owned(), 1);
            options.set_introduction_type(c"importedModule");
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("export const m = 1;"),
            );
            assert!(!module.is_null());
        }
        check(
            cx,
            global,
            "
            assertEq(scripts.length, 3, 'scripts of the debuggee only');
            const [a, c, m] = scripts;
            assertEq(a.url, 'https://example.com/a.js', 'url');
            assertEq(a.source.url, 'https://example.com/a.js', 'source url');
            assertEq(a.source.text, 'var a = 1;', 'text');
            assertEq(a.source.introductionType, 'srcScript', 'introductionType');
            assertEq(a.global, g, 'global');
            assertEq(a.source, a.source, 'one Debugger.Source per source');
            assertEq(typeof a.source.id, 'number', 'source id');
            assertEq(c.source.text, 'var c = 1;\\nc', 'text without the line padding');
            assertEq(c.source.introductionType, 'inlineScript', 'introductionType of a compiled script');
            assertEq(m.source.text, 'export const m = 1;', 'module text');
            assertEq(m.source.introductionType, 'importedModule', 'module introductionType');
            assertEq(new Set(scripts.map(s => s.source.id)).size, 3, 'distinct source ids');
            assertEq(debuggee.c, 1, 'the script ran');
            dbg.removeAllDebuggees();
            ",
        );
        eval_in(
            cx,
            debuggee,
            "var d = 1;",
            c"https://example.com/d.js",
            None,
        )
        .unwrap();
        check(
            cx,
            global,
            "assertEq(scripts.length, 3, 'no debuggees, no scripts');",
        );
    });
}

#[test]
fn execute_in_global() {
    with_globals(|cx, global, _, _| {
        check(
            cx,
            global,
            "
            const dbg = new Debugger;
            const g = dbg.addDebuggee(debuggee);
            const scripts = [];
            dbg.onNewScript = s => scripts.push(s);
            let c = g.executeInGlobal('var x = 40; x + 2');
            assertEq(c.return, 42, 'return');
            assertEq(debuggee.x, 40, 'a global var');
            assertEq(scripts.length, 1, 'onNewScript');
            assertEq(scripts[0].source.introductionType, 'debugger eval', 'introductionType');
            assertEq(scripts[0].url, 'debugger eval code', 'default url');
            assertEq(scripts[0].source.text, 'var x = 40; x + 2', 'text');
            g.executeInGlobal('1', { url: 'https://example.com/eval.js' });
            assertEq(scripts[1].url, 'https://example.com/eval.js', 'url option');
            c = g.executeInGlobal('throw new TypeError(\"no\")');
            assertEq(c.throw.class, 'Error', 'thrown error');
            assertEq(c.throw.unsafeDereference().message, 'no', 'message');
            c = g.executeInGlobal('throw 1');
            assertEq(c.throw, 1, 'thrown primitive');
            c = g.executeInGlobal('(');
            assertEq(c.throw.unsafeDereference() instanceof debuggee.SyntaxError, true, 'syntax error');
            c = g.executeInGlobal('({ a: 1 })');
            assertEq(c.return.class, 'Object', 'object class');
            const obj = c.return.unsafeDereference();
            assertEq(obj.a, 1, 'debuggee object');
            assertEq(g.makeDebuggeeValue(obj), c.return, 'one Debugger.Object per referent');
            assertEq(g.executeInGlobal('let y = 3; y').return, 3, 'let');
            assertEq(g.executeInGlobal('y').return, 3, 'global lexical bindings persist');
            assertEq(g.executeInGlobal('requestTermination(); for (;;) {}'), null, 'terminated');
            assertEq(g.executeInGlobal('x').return, 40, 'runs after a termination');
            assertThrows(() => dbg.addDebuggee(debuggee).executeInGlobal.call(c.return, '1'), 'not a global');
            dbg.shouldAvoidSideEffects = true;
            assertEq(g.executeInGlobal('x = 0'), null, 'side effects are not run');
            assertEq(debuggee.x, 40, 'nothing ran');
            ",
        );
    });
}

#[test]
fn objects() {
    with_globals(|cx, global, _, _| {
        check(
            cx,
            global,
            "
            const dbg = new Debugger;
            const g = dbg.addDebuggee(debuggee);
            const v = code => g.executeInGlobal(code).return;
            assertEq(v('[1, 2]').class, 'Array', 'Array');
            assertEq(v('new Map').class, 'Map', 'Map');
            assertEq(v('new Error').class, 'Error', 'Error');
            assertEq(v('(function f(a, b) {})').class, 'Function', 'Function');
            assertEq(v('Math.max').class, 'Function', 'native Function');
            const f = v('(function f(a, b) { return a + b; })');
            assertEq(f.callable, true, 'callable');
            assertEq(f.name, 'f', 'name');
            assertEq(f.script === undefined, false, 'scripted functions have a script');
            assertEq(v('Math.max').script, undefined, 'native functions have no script');
            assertEq(v('Math.max.bind(null)').isBoundFunction, true, 'bound');
            assertEq(v('Math.max.bind(null)').script, undefined, 'bound functions have no script');
            assertEq(v('(async function () {})').isAsyncFunction, true, 'async');
            assertEq(v('(function* () {})').isGeneratorFunction, true, 'generator');
            assertEq(v('(async function* () {})').isGeneratorFunction, true, 'async generator');
            assertEq(v('({})').isAsyncFunction, undefined, 'not a function');
            assertEq(f.call(undefined, 1, 2).return, 3, 'call');
            assertEq(f.apply(undefined, [2, 3]).return, 5, 'apply');
            const thrower = v('(function () { throw new RangeError(\"r\"); })');
            assertEq(thrower.call().throw.unsafeDereference().message, 'r', 'call throws');
            assertEq(v('(function () { requestTermination(); for (;;) {} })').call(), null, 'call terminated');
            const o = v('({ a: { b: 1 }, get c() { return 2; }, [Symbol.iterator]: 1 })');
            assertEq(o.getOwnPropertyNames().join(), 'a,c', 'names');
            assertEq(o.getOwnPropertyNamesLength(), 2, 'names length');
            assertEq(o.getOwnPropertySymbols().length, 1, 'symbols');
            const a = o.getOwnPropertyDescriptor('a');
            assertEq(a.value, o.getOwnPropertyDescriptor('a').value, 'values are wrapped once');
            assertEq(a.value.getOwnPropertyDescriptor('b').value, 1, 'primitive values');
            assertEq(a.writable && a.enumerable && a.configurable, true, 'attributes');
            const c = o.getOwnPropertyDescriptor('c');
            assertEq(c.get.call(o).return, 2, 'getter');
            assertEq(c.set, undefined, 'no setter');
            assertEq(o.getOwnPropertyDescriptor('missing'), undefined, 'missing');
            assertEq(v('[5]').getOwnPropertyDescriptor(0).value, 5, 'index keys');
            assertEq(o.proto, v('Object.prototype'), 'proto');
            assertEq(o.proto.proto, null, 'end of the chain');
            assertEq(o.getProperty('c').return, 2, 'getProperty');
            assertEq(o.global, g, 'global');
            assertThrows(() => f.call({}), 'debugger objects are not debuggee values');
            assertEq(g.asEnvironment().type, 'object', 'global environment');
            assertEq(g.asEnvironment().object, g, 'its object');
            assertEq(g.asEnvironment().names().includes('requestTermination'), true, 'names');
            ",
        );
    });
}

#[test]
fn hook_errors() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = new Debugger(debuggee); var reported = [];
             dbg.uncaughtExceptionHook = e => { reported.push(e.message); };
             dbg.onNewScript = () => { throw new Error('hook'); };",
        );
        assert_eq!(
            eval_in(cx, debuggee, "1 + 1", c"https://example.com/a.js", None)
                .unwrap()
                .to_int32(),
            2
        );
        check(cx, global, "assertEq(reported.join(), 'hook', 'reported');");
    });
}

#[test]
fn termination_in_hook() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = new Debugger(debuggee);
             dbg.onNewScript = () => { dbg.addDebuggee(debuggee).executeInGlobal('requestTermination()'); for (;;) {} };",
        );
        assert_eq!(
            eval_in(cx, debuggee, "1 + 1", c"https://example.com/a.js", None)
                .unwrap()
                .to_int32(),
            2
        );
        assert!(!unsafe { jsapi::JS_IsExceptionPending(cx.raw_cx()) });
    });
}

/// Collects garbage, after the end of the task (the pool and the targets of WeakRefs it kept).
fn collect(cx: &mut JSContext) {
    unsafe {
        mozjs::quickjs::drain_autorelease_pool();
        ClearKeptObjects(cx);
        JS_GC(cx, GCReason::API);
    }
}

#[test]
fn window_proxy_debuggee() {
    with_globals(|cx, global, debuggee, _| unsafe {
        let traps = ProxyTraps::default();
        let handler = CreateWrapperProxyHandler(&traps);
        SetWindowProxyClass(cx.raw_cx(), GetWindowProxyClass());
        rooted!(&in(cx) let mut wp = ptr::null_mut::<JSObject>());
        {
            let mut realm = AutoRealm::new_from_handle(cx, debuggee);
            let cx = &mut *realm;
            wp.set(NewWindowProxy(cx.raw_cx(), debuggee.into(), handler));
            SetWindowProxy(cx.raw_cx(), debuggee.into(), wp.handle().into());
        }
        set_global(cx, global, c"windowProxy", wp.get());
        check(
            cx,
            global,
            "
            const dbg = new Debugger;
            const g = dbg.addDebuggee(debuggee);
            assertEq(dbg.addDebuggee(windowProxy), g, 'the WindowProxy designates its global');
            assertEq(dbg.hasDebuggee(windowProxy), true, 'hasDebuggee');
            const wp = g.executeInGlobal('this').return;
            assertEq(wp.unsafeDereference(), windowProxy, 'this is the WindowProxy');
            assertEq(wp.class, g.class, 'its class is that of its window');
            assertEq(wp.executeInGlobal('typeof requestTermination').return, 'function', 'executeInGlobal');
            assertEq(wp.asEnvironment().object, g, 'asEnvironment');
            dbg.removeDebuggee(windowProxy);
            assertEq(dbg.hasDebuggee(debuggee), false, 'removeDebuggee');
            ",
        );
    });
}

#[test]
fn proxies_and_names() {
    with_globals(|cx, global, _, _| {
        check(
            cx,
            global,
            "
            const dbg = new Debugger;
            const g = dbg.addDebuggee(debuggee);
            assertThrows(() => dbg.addDebuggee(globalThis), 'the debugger global is no debuggee');
            const v = code => g.executeInGlobal(code).return;
            const p = v('new Proxy({}, {})');
            assertEq(p.isProxy, true, 'a JS Proxy');
            assertEq(p.class, 'Object', 'its class');
            assertEq(v('new Proxy(function () {}, {})').class, 'Function', 'a callable JS Proxy');
            assertEq(v('({})').isProxy, false, 'not a proxy');
            g.executeInGlobal('var trapped = 0');
            const f = v('new Proxy(function f() {}, { getOwnPropertyDescriptor() { trapped++; } })');
            assertEq(f.name, undefined, 'a proxy has no name');
            assertEq(v('trapped'), 0, 'reading it ran no trap');
            assertEq(v('Object.create(null)').global, undefined, 'unknown global');
            assertEq(v('[]').global, g, 'known global');
            ",
        );
    });
}

#[test]
fn unreferenced_objects_are_collected() {
    with_globals(|cx, global, _, _| {
        eval(
            cx,
            global,
            "var dbg = new Debugger(debuggee); var g = dbg.addDebuggee(debuggee);
             var target = new WeakRef(g.executeInGlobal('({ a: { b: 1 } })').return.unsafeDereference());
             var eager = new WeakRef(new Debugger(debuggee));
             eager.deref().addDebuggee(debuggee).executeInGlobal('1');
             eager.deref().removeAllDebuggees();",
        );
        collect(cx);
        check(
            cx,
            global,
            "
            assertEq(target.deref(), undefined, 'a referent nothing else references');
            assertEq(eager.deref(), undefined, 'a Debugger without debuggees');
            const o = g.executeInGlobal('({})').return;
            assertEq(g.executeInGlobal('this').return.makeDebuggeeValue(o.unsafeDereference()), o, 'identity while alive');
            ",
        );
    });
}

#[test]
fn new_scripts_of_other_compile_paths() {
    with_globals(|cx, global, debuggee, other| unsafe {
        eval(
            cx,
            global,
            "var dbg = new Debugger(debuggee); var scripts = [];
             dbg.onNewScript = s => scripts.push(s);",
        );
        // A second debugger global, with its own emulation, debugging the same global.
        {
            let mut realm = AutoRealm::new_from_handle(cx, other);
            let (other_global, cx) = realm.global_and_reborrow();
            assert!(JS_DefineDebuggerObject(cx, other_global));
            set_global(cx, other_global, c"debuggee", debuggee.get());
            eval(
                cx,
                other_global,
                "var dbg = new Debugger(debuggee); var scripts = [];
                 dbg.onNewScript = s => scripts.push(s);",
            );
        }
        {
            let mut realm = AutoRealm::new_from_handle(cx, debuggee);
            let cx = &mut *realm;
            let utf16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
            let mut options =
                CompileOptionsWrapper::new(cx, c"https://example.com/handler".to_owned(), 7);
            options.set_introduction_type(c"eventHandler");
            let chain = EnvironmentChain::new(cx, jsapi::SupportUnscopables::Yes);
            let args = [c"event".as_ptr()];
            let body = utf16("return 1; //# sourceURL=http://handler/");
            assert!(!CompileFunction(
                cx,
                chain.get(),
                options.ptr,
                c"onclick".as_ptr(),
                1,
                args.as_ptr(),
                &mut transform_u16_to_source_text(&body),
            )
            .is_null());
            let mut options =
                CompileOptionsWrapper::new(cx, c"https://example.com/u16.js".to_owned(), 1);
            options.set_introduction_type(c"srcScript");
            let text = utf16("var u = 16;");
            rooted!(&in(cx) let mut rval = UndefinedValue());
            assert!(Evaluate(
                cx,
                options.ptr,
                &mut transform_u16_to_source_text(&text),
                rval.handle_mut()
            ));
            assert!(!Compile(cx, options.ptr, &mut transform_u16_to_source_text(&text)).is_null());
            let text = utf16("export const m = 16;");
            assert!(
                !CompileModule(cx, options.ptr, &mut transform_u16_to_source_text(&text)).is_null()
            );
            let mut options =
                CompileOptionsWrapper::new(cx, c"https://example.com/off-thread.js".to_owned(), 1);
            options.set_introduction_type(c"srcScript");
            let mut result =
                compile_to_stencil_offthread(options.ptr, Arc::new("var o = 1;".to_owned()), Some)
                    .finish()
                    .unwrap();
            let instantiate_options = (&result.compile_options).into();
            assert!(!InstantiateGlobalStencil(
                cx,
                &instantiate_options,
                **result.stencil.as_ref().unwrap(),
                result.storage.as_mut_ptr(),
            )
            .is_null());
        }
        check(
            cx,
            global,
            "
            assertEq(scripts.length, 5, 'event handler, UTF-16 script twice, module, stencil');
            assertEq(scripts[0].source.introductionType, 'eventHandler', 'event handler');
            assertEq(scripts[0].source.text, 'return 1; //# sourceURL=http://handler/', 'its text');
            assertEq(scripts[1].source.text, 'var u = 16;', 'UTF-16 text');
            assertEq(scripts[3].source.text, 'export const m = 16;', 'UTF-16 module text');
            assertEq(scripts[4].source.text, 'var o = 1;', 'text of a stencil');
            assertEq(scripts[4].source.url, 'https://example.com/off-thread.js', 'its url');
            assertEq(scripts[4].source.introductionType, 'srcScript', 'its introduction type');
            assertEq(debuggee.u, 16, 'the script ran');
            ",
        );
        let mut realm = AutoRealm::new_from_handle(cx, other);
        let (other_global, cx) = realm.global_and_reborrow();
        set_global(cx, other_global, c"debuggee", debuggee.get());
        eval(cx, other_global, PRELUDE);
        check(
            cx,
            other_global,
            "assertEq(scripts.length, 5, 'the other emulation sees them too');
             assertEq(new Set(scripts.map(s => s.source.id)).size, 5, 'one source each');",
        );
    });
}

#[test]
fn exclusive_debugger_on_eval() {
    with_globals(|cx, global, _, _| {
        check(
            cx,
            global,
            "
            const main = new Debugger(debuggee), eager = new Debugger;
            const seen = [];
            main.onNewScript = s => seen.push('main ' + s.source.text);
            eager.onNewScript = s => seen.push('eager ' + s.source.text);
            const g = eager.addDebuggee(debuggee);
            eager.exclusiveDebuggerOnEval = true;
            g.executeInGlobal('1');
            eager.exclusiveDebuggerOnEval = false;
            g.executeInGlobal('2');
            assertEq(seen.join(), 'eager 1,main 2,eager 2', 'other debuggers miss exclusive evaluations');
            ",
        );
    });
}
