/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The emulation of SpiderMonkey's `Debugger` API (`engine/debugger_api.js`), as Servo's
//! devtools (`debugger.js`) use it: a debugger global with `Debugger`, and a debuggee global.

#![cfg(feature = "debugger")]

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
    JS_SetNativeStackQuota, JS_SetProperty, ModuleEvaluate, ModuleLink, JS_GC,
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
// Exceptions in hooks of debuggers that record them fail the test.
var hookErrors = [];
function recordHookErrors(dbg) {
  dbg.uncaughtExceptionHook = e => { hookErrors.push(`${e}\\n${e.stack}`); };
  return dbg;
}
function test(f) {
  try {
    f();
    return hookErrors.length ? `in hooks: ${hookErrors.splice(0).join('\\n')}` : 'ok';
  } catch (e) { return `${e}\\n${e.stack}`; }
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
            assertEq(g.executeInGlobal('var z = 0'), null, 'declarations are not run');
            assertEq('z' in debuggee, false, 'nothing ran');
            assertEq(g.executeInGlobal('x + 1').return, 41, 'other code runs');
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
            g.executeInGlobal('let lexical = 1; const constant = 2;');
            const env = g.asEnvironment();
            assertEq(env.type, 'declarative', 'global lexical environment');
            assertEq(env.names().join(), 'lexical,constant', 'its names');
            assertEq(env.getVariable('constant'), 2, 'getVariable');
            assertEq(env.parent.type, 'object', 'global environment');
            assertEq(env.parent.object, g, 'its object');
            assertEq(env.parent.names().includes('requestTermination'), true, 'names');
            assertEq(env.parent.parent, null, 'outermost');
            assertEq(g.asEnvironment(), env, 'one Debugger.Environment per environment');
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
            "var dbg = new Debugger(debuggee), once = false;
             dbg.onNewScript = () => {
               if (once) return;
               once = true;
               dbg.addDebuggee(debuggee).executeInGlobal('requestTermination()');
               for (;;) {}
             };",
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
            assertEq(wp.asEnvironment().parent.object, g, 'asEnvironment');
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

/// Compiles `source` as a classic script of `target` starting at `line` (as Servo's inline
/// scripts), and runs it.
fn run_at(cx: &mut JSContext, target: SafeHandleObject, source: &str, url: &CStr, line: u32) {
    unsafe {
        let mut realm = AutoRealm::new_from_handle(cx, target);
        let cx = &mut *realm;
        let mut options = CompileOptionsWrapper::new(cx, url.to_owned(), line);
        options.set_introduction_type(c"inlineScript");
        let script = mozjs::rust::wrappers2::Compile1(
            cx,
            options.ptr,
            &mut transform_str_to_source_text(source),
        );
        assert!(!script.is_null());
        rooted!(&in(cx) let script = script);
        rooted!(&in(cx) let mut rval = UndefinedValue());
        assert!(JS_ExecuteScript(cx, script.handle(), rval.handle_mut()));
    }
}

/// Compiles `source` as a classic script of `target` starting at `line` off the main thread, as
/// Servo's external scripts, and instantiates it.
fn instantiate_at(
    cx: &mut JSContext,
    target: SafeHandleObject,
    source: &str,
    url: &CStr,
    line: u32,
) {
    unsafe {
        let mut realm = AutoRealm::new_from_handle(cx, target);
        let cx = &mut *realm;
        let mut options = CompileOptionsWrapper::new(cx, url.to_owned(), line);
        options.set_introduction_type(c"srcScript");
        let mut result =
            compile_to_stencil_offthread(options.ptr, Arc::new(source.to_owned()), Some)
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
}

#[test]
fn stencils_instantiate_where_their_source_does_not_compile() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = new Debugger(debuggee), scripts = []; dbg.onNewScript = s => scripts.push(s);",
        );
        let depth = 600;
        let source = format!("var nested = {}1{};", "[".repeat(depth), "]".repeat(depth));
        // Too little stack left to compile the script again here.
        unsafe { JS_SetNativeStackQuota(cx, 64 * 1024, 64 * 1024, 64 * 1024) };
        instantiate_at(cx, debuggee, &source, c"https://example.com/deep.js", 1);
        unsafe { JS_SetNativeStackQuota(cx, 0, 0, 0) };
        check(
            cx,
            global,
            "assertEq(scripts.length, 1, 'onNewScript');
             assertEq(scripts[0].source.url, 'https://example.com/deep.js', 'its url');",
        );
    });
}

/// The breakpoint positions of a source, as devtools lists them: `line: [column, …]`, 0-based
/// columns, over the script and its functions.
const POSITIONS: &str = "
function positions(script) {
  const lines = new Map;
  const walk = s => {
    for (const p of s.getPossibleBreakpoints()) {
      if (!lines.has(p.lineNumber)) lines.set(p.lineNumber, []);
      lines.get(p.lineNumber).push(p.columnNumber - 1);
    }
    s.getChildScripts().forEach(walk);
  };
  walk(script);
  return [...lines].sort((a, b) => a[0] - b[0])
    .map(([line, columns]) => `${line}: ${columns.sort((a, b) => a - b).join(' ')}`).join(', ');
}";

#[test]
fn breakpoint_positions() {
    with_globals(|cx, global, debuggee, _| {
        eval_in(
            cx,
            debuggee,
            "var console = { log() {} }; function setTimeout() {}",
            c"https://example.com/stubs.js",
            None,
        )
        .unwrap();
        eval(cx, global, POSITIONS);
        eval(
            cx,
            global,
            "var dbg = new Debugger(debuggee), scripts = []; dbg.onNewScript = s => scripts.push(s);",
        );
        // Servo's devtools tests `test_source_breakable_lines_and_positions(_with_functions)`.
        run_at(
            cx,
            debuggee,
            "\n    console.log(1); console.log(2);\n    /*💖💖💖*/ console.log(1); console.log(2);\n    \
             /*🏳️‍⚧️*/ console.log(1); console.log(2);\n",
            c"https://example.com/test.html",
            3,
        );
        run_at(
            cx,
            debuggee,
            "\n    function runContinuously() {\n        setTimeout(() => {\n            \
             runContinuously();\n        }, 1000);\n    }\n    runContinuously();\n",
            c"https://example.com/test_with_functions.html",
            3,
        );
        instantiate_at(
            cx,
            debuggee,
            "\n    function runContinuously() {\n        setTimeout(() => {\n            \
             runContinuously();\n        }, 1000);\n    }\n    runContinuously();\n\
             //# sourceURL=https://example.com/named.js\n",
            c"https://example.com/external.js",
            3,
        );
        check(
            cx,
            global,
            "
            assertEq(positions(scripts[0]), '4: 4 12 20 28, 5: 15 23 31 39, 6: 15 23 31 39, 7: 0', 'statements and calls');
            assertEq(positions(scripts[1]), '5: 8 18, 6: 12, 7: 8, 8: 4, 9: 4, 10: 0', 'functions');
            const [f] = scripts[1].getChildScripts();
            assertEq(f.displayName, 'runContinuously', 'displayName');
            assertEq(f.startLine, 4, 'startLine');
            assertEq(f.getChildScripts()[0].displayName, undefined, 'arrow functions have no name');
            assertEq(scripts[1].sourceStart, 0, 'sourceStart of the script');
            assertEq(scripts[1].source.text.slice(f.sourceStart, f.sourceStart + 8), 'function', 'sourceStart of a function');
            const [p] = f.getPossibleBreakpoints({ line: 5 });
            const meta = f.getOffsetMetadata(p.offset);
            assertEq(`${meta.lineNumber}:${meta.columnNumber} ${meta.isBreakpoint} ${meta.isStepStart}`, '5:9 true true', 'a statement');
            const call = f.getPossibleBreakpoints({ line: 5 })[1];
            assertEq(f.getOffsetMetadata(call.offset).isStepStart, false, 'a call');
            assertEq(positions(scripts[2]), '5: 8 18, 6: 12, 7: 8, 8: 4, 9: 4, 11: 0', 'a script instantiated from a stencil');
            assertEq(scripts[2].source.displayURL, 'https://example.com/named.js', 'its sourceURL');
            ",
        );
    });
}

#[test]
fn frames_and_environments() {
    with_globals(|cx, global, debuggee, _| {
        // Scripts compiled while a debugger has a debuggee have SpiderMonkey's positions.
        eval(
            cx,
            global,
            "var dbg = recordHookErrors(new Debugger), g = dbg.addDebuggee(debuggee);",
        );
        eval_in(
            cx,
            debuggee,
            "function add(a, b) {
               let sum = a + b;
               const twice = sum * 2;
               return twice;
             }
             var obj = { m(x) { return add(x, 1); } };
             function outer() { let captured = 5; return function inner() { return captured; }; }
             var inner = outer();
             let lexical = 'l';",
            c"https://example.com/f.js",
            None,
        )
        .unwrap();
        check(
            cx,
            global,
            "
            const add = g.getOwnPropertyDescriptor('add').value.script;
            const at = line => add.getPossibleBreakpoints({ line })[0].offset;
            const seen = [];
            let saved;
            add.setBreakpoint(at(2), { hit(frame) {
              const env = frame.environment;
              seen.push(`tdz ${env.getVariable('sum').uninitialized} ${env.getVariable('twice').uninitialized}`);
            } });
            add.setBreakpoint(at(4), { hit(frame) {
              saved = frame;
              assertEq(dbg.getNewestFrame(), frame, 'getNewestFrame');
              assertEq(frame.type, 'call', 'type');
              assertEq(frame.callee.name, 'add', 'callee');
              assertEq(frame.script, add, 'script');
              assertEq(frame.offset, at(4), 'offset');
              assertEq(frame.this, g, 'the global is the this of sloppy calls');
              const meta = frame.script.getOffsetMetadata(frame.offset);
              assertEq(`${meta.lineNumber}:${meta.columnNumber}`, '4:16', 'location');
              const older = frame.older;
              assertEq(older.callee.name, 'm', 'older');
              assertEq(older.this.class, 'Object', 'its this');
              assertEq(older.script.getOffsetMetadata(older.offset).lineNumber, 6, 'its location: the call');
              assertEq(older.older.type, 'global', 'the evaluated code');
              assertEq(older.older.older, null, 'oldest');
              const env = frame.environment;
              assertEq(frame.environment, env, 'one Debugger.Environment per frame');
              assertEq(env.type, 'declarative', 'environment type');
              assertEq(env.calleeScript, add, 'calleeScript');
              assertEq(env.names().join(), 'a,b,sum,twice', 'names');
              assertEq(env.getVariable('sum'), 3, 'getVariable');
              env.setVariable('sum', 10);
              assertEq(frame.eval('sum').return, 10, 'setVariable');
              assertEq(frame.eval('a + b + twice').return, 9, 'frame.eval');
              assertEq(frame.eval('b = 4; b').return, 4, 'assignment');
              assertEq(env.getVariable('b'), 4, 'assignment reaches the frame');
              assertEq(frame.eval('twice = 1').throw.unsafeDereference() instanceof debuggee.TypeError, true, 'const');
              assertEq(frame.eval('nope').throw.unsafeDereference() instanceof debuggee.ReferenceError, true, 'unbound');
              assertEq(frame.eval('typeof arguments').return, 'undefined', 'no arguments of the evaluator');
              assertEq(frame.eval('typeof debuggerScope').return, 'undefined', 'no evaluator parameters');
              assertEq(frame.eval('lexical + obj.m.name').return, 'lm', 'global bindings');
              const global = env.parent;
              assertEq(global.type, 'declarative', 'global lexical environment');
              assertEq(global, g.asEnvironment(), 'the global environment');
              assertEq(global.names().includes('lexical'), true, 'global lexical names');
              assertEq(global.parent.object, g, 'global object environment');
            } });
            assertEq(g.executeInGlobal('obj.m(2)').return, 6, 'result');
            assertEq(seen.join(), 'tdz true true', 'uninitialized');
            assertEq(saved.onStack, false, 'the frame stopped');
            assertThrows(() => saved.offset, 'dead frames have no offset');
            assertEq(saved.script, add, 'dead frames keep their script');
            add.clearAllBreakpoints();
            assertEq(g.executeInGlobal('add(1, 1)').return, 4, 'cleared');

            const inner = g.getOwnPropertyDescriptor('inner').value.script;
            let closure;
            inner.setBreakpoint(inner.getPossibleBreakpoints()[0].offset, { hit(frame) {
              const env = frame.environment;
              closure = `${env.names().join()}|${env.parent.names().join()}|${env.parent.getVariable('captured')}`;
              assertEq(frame.eval('captured = 7').return, 7, 'assigning a closure variable');
            } });
            assertEq(g.executeInGlobal('inner()').return, 7, 'closure variables');
            assertEq(closure, '|captured|5', 'closure environment');
            ",
        );
    });
}

#[test]
fn stepping_and_hooks() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = recordHookErrors(new Debugger), g = dbg.addDebuggee(debuggee);",
        );
        eval_in(
            cx,
            debuggee,
            "function step() {
               let x = 1;
               x += 1;
               return x;
             }
             function thrower() { throw new Error('t'); }",
            c"https://example.com/s.js",
            None,
        )
        .unwrap();
        check(
            cx,
            global,
            "
            const lines = [], pops = [];
            dbg.onEnterFrame = frame => {
              if (frame.type !== 'call') return;
              frame.onStep = function () {
                lines.push(this.script.getOffsetMetadata(this.offset).lineNumber);
              };
              frame.onPop = function (c) {
                pops.push(c.return !== undefined ? `return ${c.return}` : `throw ${c.throw.class}`);
              };
            };
            assertEq(g.executeInGlobal('step()').return, 2, 'result');
            assertEq(lines.join(), '2,3,4', 'steps');
            assertEq(g.executeInGlobal('try { thrower(); } catch (e) { 1 }').return, 1, 'caught');
            assertEq(pops.join(), 'return 2,throw Error', 'onPop');
            dbg.onEnterFrame = undefined;
            lines.length = 0;
            g.executeInGlobal('step()');
            assertEq(lines.length, 0, 'steps end with the frame');
            dbg.onEnterFrame = () => null;
            assertEq(g.executeInGlobal('step()'), null, 'onEnterFrame terminates');
            dbg.onEnterFrame = undefined;
            const step = g.getOwnPropertyDescriptor('step').value.script;
            step.setBreakpoint(step.getPossibleBreakpoints({ line: 3 })[0].offset, { hit: () => null });
            assertEq(g.executeInGlobal('step()'), null, 'a breakpoint terminates');
            assertEq(g.executeInGlobal('1 + 1').return, 2, 'runs after');
            dbg.removeDebuggee(debuggee);
            dbg.addDebuggee(debuggee);
            assertEq(g.executeInGlobal('step()').return, 2, 'removing the debuggee clears breakpoints');
            ",
        );
    });
}

#[test]
fn side_effect_free_evaluation() {
    with_globals(|cx, global, debuggee, _| {
        eval_in(
            cx,
            debuggee,
            "var counter = 0;
             function pure(a) { return a * 2; }
             function impure() { counter++; return counter; }",
            c"https://example.com/e.js",
            None,
        )
        .unwrap();
        check(
            cx,
            global,
            "
            const main = new Debugger(debuggee), seen = [];
            main.onNewScript = s => seen.push(s.source.text);
            // As devtools' createSideEffectFreeDebugger.
            const eager = new Debugger;
            eager.exclusiveDebuggerOnEval = true;
            const g = eager.addDebuggee(debuggee);
            const handler = { hit: () => null };
            eager.onEnterFrame = frame => {
              frame.onStep = () => undefined;
              for (const offset of frame.script.getEffectfulOffsets()) frame.script.setBreakpoint(offset, handler);
            };
            eager.onNativeCall = () => null;
            eager.shouldAvoidSideEffects = true;
            assertEq(g.executeInGlobal('pure(2)').return, 4, 'pure code runs');
            assertEq(g.executeInGlobal('impure()'), null, 'effects are terminated');
            assertEq(g.executeInGlobal('counter = 5'), null, 'global assignments are effects');
            assertEq(g.executeInGlobal('Math.max(1, 2)'), null, 'native calls are terminated');
            assertEq(g.executeInGlobal('var v = 1'), null, 'declarations are terminated');
            assertEq(debuggee.counter, 0, 'nothing changed');
            assertEq('v' in debuggee, false, 'no declaration');
            eager.onNativeCall = undefined;
            eager.removeAllDebuggees();
            assertEq(seen.length, 0, 'other debuggers miss eager evaluations');
            assertEq(main.addDebuggee(debuggee).executeInGlobal('impure()').return, 1, 'its breakpoints are gone');
            ",
        );
    });
}

/// devtools' createSideEffectFreeDebugger, cancelling after `limit` steps.
const EAGER: &str = "
function eagerDebugger(global, limit) {
  const eager = recordHookErrors(new Debugger);
  eager.exclusiveDebuggerOnEval = true;
  const g = eager.addDebuggee(global);
  let steps = 0;
  const handler = { hit: () => null };
  eager.onEnterFrame = frame => {
    frame.onStep = () => (++steps > limit ? null : undefined);
    for (const offset of frame.script.getEffectfulOffsets()) frame.script.setBreakpoint(offset, handler);
  };
  eager.onNativeCall = () => null;
  eager.shouldAvoidSideEffects = true;
  return [eager, g];
}";

#[test]
fn side_effect_free_evaluation_limits() {
    with_globals(|cx, global, _, _| {
        eval(cx, global, EAGER);
        check(
            cx,
            global,
            "
            const main = new Debugger(debuggee);
            const [eager, g] = eagerDebugger(debuggee, 1000);
            for (const code of ['for (;;) {}', 'for (;;);', 'while (true) {}', 'do {} while (true)']) {
              assertEq(g.executeInGlobal(code), null, `${code} is cancelled`);
            }
            assertEq(g.executeInGlobal('eval(\"var leakedVar = 1\")'), null, 'direct eval');
            assertEq(g.executeInGlobal('eval(\"function leakedFn() {}\")'), null, 'direct eval of a function');
            assertEq('leakedVar' in debuggee || 'leakedFn' in debuggee, false, 'no declarations');
            eager.removeAllDebuggees();
            ",
        );
    });
}

#[test]
fn frame_eval_scoping() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = recordHookErrors(new Debugger), g = dbg.addDebuggee(debuggee);",
        );
        eval_in(
            cx,
            debuggee,
            "var hijacked;
             function shadow(debuggerCode, eval) { return debuggerCode + eval; }
             var holder = { m() { return [1].map(() => { return this; })[0]; } };",
            c"https://example.com/s.js",
            None,
        )
        .unwrap();
        check(
            cx,
            global,
            "
            g.executeInGlobal('globalThis.eval = c => (hijacked = c, \"hijacked\")');
            const shadow = g.getOwnPropertyDescriptor('shadow').value.script;
            let shadowed;
            shadow.setBreakpoint(shadow.getPossibleBreakpoints()[0].offset, { hit(frame) {
              shadowed = [frame.eval('debuggerCode').return, frame.eval('typeof eval').return,
                frame.eval('debuggerCode + 1').return].join();
            } });
            assertEq(g.executeInGlobal('shadow(\"code\", 42)').return, 'code42', 'result');
            assertEq(shadowed, 'code,number,code1', 'names of the frame, not of the evaluator');
            assertEq(debuggee.hijacked, undefined, 'the eval of the realm, not of the page');
            const holder = g.getOwnPropertyDescriptor('holder').value;
            const arrow = holder.getOwnPropertyDescriptor('m').value.script.getChildScripts()[0];
            let arrowThis;
            arrow.setBreakpoint(arrow.getPossibleBreakpoints()[0].offset, { hit(frame) {
              arrowThis = `${frame.this === holder} ${frame.eval('this').return === holder}`;
            } });
            g.executeInGlobal('holder.m()');
            assertEq(arrowThis, 'true true', 'the lexical this of arrow functions');
            ",
        );
    });
}

#[test]
fn module_frames() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = recordHookErrors(new Debugger(debuggee)), seen;
             dbg.onNewScript = script => {
               if (!script.isModule) return;
               const [p] = script.getPossibleBreakpoints({ line: 2 });
               script.setBreakpoint(p.offset, { hit(frame) {
                 seen = `${frame.type} ${frame.environment.names().join()} ${frame.eval('modLocal + 1').return}`;
               } });
             };",
        );
        unsafe {
            let mut realm = AutoRealm::new_from_handle(cx, debuggee);
            let cx = &mut *realm;
            let options = CompileOptionsWrapper::new(cx, c"https://example.com/m.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text(
                    "const modLocal = 1;\nglobalThis.out = modLocal;\n",
                ),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut rval = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), rval.handle_mut()));
        }
        check(
            cx,
            global,
            "assertEq(seen, 'module modLocal 2', 'module variables'); assertEq(debuggee.out, 1, 'ran');",
        );
    });
}

#[test]
fn on_pop_terminates() {
    with_globals(|cx, global, _, _| {
        check(
            cx,
            global,
            "
            const dbg = recordHookErrors(new Debugger), g = dbg.addDebuggee(debuggee);
            dbg.onEnterFrame = frame => {
              if (frame.type === 'call') frame.onPop = () => null;
            };
            assertEq(g.executeInGlobal('var after = 0; function five() { return 5; } five(); after = 1'), null, 'terminated');
            assertEq(debuggee.after, 0, 'at the pop');
            dbg.onEnterFrame = undefined;
            assertEq(g.executeInGlobal('after + 1').return, 1, 'runs after');
            ",
        );
    });
}

#[test]
fn two_emulations() {
    with_globals(|cx, global, debuggee, other| unsafe {
        eval(
            cx,
            global,
            "var dbgA = recordHookErrors(new Debugger(debuggee)), pops = 0;
             dbgA.onEnterFrame = f => { if (f.type === 'call') f.onPop = () => { pops++; }; };",
        );
        {
            let mut realm = AutoRealm::new_from_handle(cx, other);
            let (other_global, cx) = realm.global_and_reborrow();
            assert!(JS_DefineDebuggerObject(cx, other_global));
            set_global(cx, other_global, c"debuggee", debuggee.get());
            eval(
                cx,
                other_global,
                "var dbgB = new Debugger(debuggee);
                 dbgB.onEnterFrame = f => { if (f.type === 'call') { f.onStep = () => {}; f.onStep = undefined; } };",
            );
        }
        eval_in(
            cx,
            debuggee,
            "function two() { let x = 1; x++; return x; } two()",
            c"https://example.com/t.js",
            None,
        )
        .unwrap();
        check(
            cx,
            global,
            "assertEq(pops, 1, 'the other emulation keeps the hooks of this one');",
        );
    });
}

#[test]
fn positions_of_long_lines() {
    with_globals(|cx, global, debuggee, _| {
        eval(
            cx,
            global,
            "var dbg = recordHookErrors(new Debugger(debuggee)), scripts = [];
             dbg.onNewScript = s => scripts.push(s);",
        );
        // A minified script: one line of 200 KB, with a surrogate pair.
        let body: String = (0..20_000).map(|i| format!("a += {};", i % 10)).collect();
        let source =
            format!("var s = '💖'; function big() {{ var a = 0; {body} return a; }} big();");
        eval_in(cx, debuggee, &source, c"https://example.com/min.js", None).unwrap();
        let start = std::time::Instant::now();
        check(
            cx,
            global,
            "
            const [big] = scripts[0].getChildScripts();
            const positions = big.getPossibleBreakpoints();
            assertEq(positions.length > 20000, true, 'positions');
            assertEq(positions[0].columnNumber, scripts[0].source.text.indexOf('0; a') + 1, 'UTF-16 columns');
            ",
        );
        assert!(
            start.elapsed().as_secs() < 10,
            "linear in the line length: {:?}",
            start.elapsed()
        );
    });
}
