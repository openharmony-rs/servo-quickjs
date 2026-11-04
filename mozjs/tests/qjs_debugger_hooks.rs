/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS's debugger hooks, which the emulation of SpiderMonkey's `Debugger` builds on, through
//! QuickJS's own API: breakpoints (Q45), and frame and native call hooks (Q46).

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use mozjs::context::{JSContext, RawJSContext};
use mozjs::conversions::jsstr_to_string;
use mozjs::jsapi::{self, JSScript, OnNewGlobalHookOption, Value};
use mozjs::jsval::UndefinedValue;
use mozjs::qjs;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{Compile1, JS_ExecuteScript, JS_NewGlobalObject};
use mozjs::rust::{
    transform_str_to_source_text, CompileOptionsWrapper, JSEngine, JSEngineHandle, RealmOptions,
    Runtime, SIMPLE_GLOBAL_CLASS,
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

/// Runs `f` in the realm of a new global.
fn with_global(f: impl FnOnce(&mut JSContext)) {
    let mut runtime = Runtime::new(engine());
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
        f(&mut realm);
    }
}

fn compile(cx: &mut JSContext, source: &str) -> *mut JSScript {
    let options = CompileOptionsWrapper::new(cx, c"hooks.js".to_owned(), 1);
    let script = unsafe { Compile1(cx, options.ptr, &mut transform_str_to_source_text(source)) };
    assert!(!script.is_null());
    script
}

/// Runs a compiled script and returns its completion value as a string, or `None` if it threw.
fn execute(cx: &mut JSContext, script: *mut JSScript) -> Option<String> {
    rooted!(&in(cx) let script = script);
    rooted!(&in(cx) let mut rval = UndefinedValue());
    unsafe {
        if !JS_ExecuteScript(cx, script.handle(), rval.handle_mut()) {
            jsapi::JS_ClearPendingException(cx.raw_cx());
            return None;
        }
        let s = jsapi::JS_ValueToSource(cx.raw_cx(), rval.handle().into());
        Some(jsstr_to_string(cx, NonNull::new(s).unwrap()))
    }
}

fn script_value(script: *mut JSScript) -> qjs::JSValue {
    qjs::JSValue::mkptr(qjs::JS_TAG_FUNCTION_BYTECODE, script as *mut c_void)
}

fn script_ctx(script: *mut JSScript) -> *mut qjs::JSContext {
    unsafe { qjs::JS_GetScriptRealm(script_value(script)) }
}

thread_local! {
    static HITS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static THROW_AT: Cell<Option<u32>> = const { Cell::new(None) };
}

unsafe extern "C" fn on_breakpoint(
    ctx: *mut qjs::JSContext,
    _script: qjs::JSValue,
    pc: u32,
    _opaque: *mut c_void,
) -> c_int {
    HITS.with(|h| h.borrow_mut().push(pc));
    if THROW_AT.with(Cell::get) == Some(pc) {
        qjs::JS_ThrowTypeError(ctx, c"breakpoint".as_ptr());
        return -1;
    }
    0
}

/// Sets a breakpoint at every offset of `script` where one can be set, and returns them.
fn break_everywhere(script: *mut JSScript) -> Vec<u32> {
    let ctx = script_ctx(script);
    (0..200_000)
        .filter(|&pc| unsafe { qjs::JS_SetBreakpoint(ctx, script_value(script), pc) } == 0)
        .collect()
}

fn hits() -> Vec<u32> {
    HITS.with(|h| h.take())
}

/// Top-level code that runs many kinds of instructions: loops, branches, exceptions, closures,
/// classes, destructuring, spread, optional chaining, generators and template literals. A block,
/// so that it can run more than once in a global.
const PROGRAM: &str = r#"{
var log = [];
for (let i = 0; i < 3; i++) { if (i % 2) log.push(i); else log.push(-i); }
let { a, b: [c, ...d] } = { a: 1, b: [2, 3, 4] };
try { null.x; } catch (e) { log.push(e instanceof TypeError); } finally { log.push('f'); }
switch (a + c) { case 3: log.push('three'); break; default: log.push('other'); }
const add = (x, y = 10) => x + y;
class P { #p = 1; get p() { return this.#p; } static s() { return 's'; } }
log.push(add(1), add(1, 2), new P().p, P.s(), [...d, ...'ab'].length, a?.b?.c ?? 'n');
function* g() { yield 1; yield* [2, 3]; }
log.push([...g()].join(), `t${a}${c}`, typeof undefined, 2 ** 10, 7 >>> 1, !0, delete log.nothing);
label: for (const k in { x: 1, y: 2 }) { for (const v of [1, 2]) { if (v === 2) continue label; log.push(k + v); } }
log.join()
}"#;

const PROGRAM_RESULT: &str =
    r#""0,1,-2,true,f,three,11,3,1,s,4,n,1,2,3,t12,undefined,1024,3,true,true,x1,y1""#;

#[test]
fn breakpoints() {
    with_global(|cx| {
        let script = compile(cx, "var n = 0; n += 1; n += 2; n");
        let ctx = script_ctx(script);
        let rt = unsafe { qjs::JS_GetRuntime(ctx) };
        unsafe { qjs::JS_SetBreakpointHandler(rt, Some(on_breakpoint), ptr::null_mut()) };
        let v = script_value(script);
        unsafe {
            assert_eq!(qjs::JS_SetBreakpoint(ctx, v, 0), 0);
            assert_eq!(qjs::JS_SetBreakpoint(ctx, v, 0), 0, "a second time");
            assert_eq!(qjs::JS_SetBreakpoint(ctx, v, 100_000), -1, "past the end");
        }
        assert_eq!(execute(cx, script).as_deref(), Some("3"));
        assert_eq!(hits(), [0]);
        assert_eq!(execute(cx, script).as_deref(), Some("3"));
        assert_eq!(hits(), [0], "each time the instruction runs");
        unsafe {
            assert_eq!(qjs::JS_ClearBreakpoint(rt, v, 0), 0);
            assert_eq!(qjs::JS_ClearBreakpoint(rt, v, 0), -1, "cleared");
        }
        assert_eq!(execute(cx, script).as_deref(), Some("3"));
        assert!(hits().is_empty());

        // Every instruction start takes a breakpoint, and nothing else does.
        let all = break_everywhere(script);
        assert!(all.len() > 4 && all[0] == 0);
        assert_eq!(execute(cx, script).as_deref(), Some("3"));
        assert_eq!(hits(), all, "each instruction runs once, in order");

        // The handler throws instead of running the instruction.
        THROW_AT.with(|t| t.set(Some(all[2])));
        assert_eq!(execute(cx, script), None);
        assert_eq!(hits(), &all[..3]);
        THROW_AT.with(|t| t.set(None));
        for pc in all {
            unsafe { assert_eq!(qjs::JS_ClearBreakpoint(rt, v, pc), 0) };
        }
        assert_eq!(execute(cx, script).as_deref(), Some("3"));
        assert!(hits().is_empty());
        unsafe { qjs::JS_SetBreakpointHandler(rt, None, ptr::null_mut()) };
    });
}

#[test]
fn breakpoints_everywhere_keep_results() {
    with_global(|cx| {
        let plain = compile(cx, PROGRAM);
        assert_eq!(execute(cx, plain).as_deref(), Some(PROGRAM_RESULT));
        let script = compile(cx, PROGRAM);
        let rt = unsafe { qjs::JS_GetRuntime(script_ctx(script)) };
        unsafe { qjs::JS_SetBreakpointHandler(rt, Some(on_breakpoint), ptr::null_mut()) };
        let all = break_everywhere(script);
        assert!(all.len() > 100);
        assert_eq!(execute(cx, script).as_deref(), Some(PROGRAM_RESULT));
        let hit = hits();
        assert!(
            hit.len() > all.len(),
            "loops run instructions more than once"
        );
        assert!(hit.iter().all(|pc| all.contains(pc)));
        // The runtime is destroyed with the breakpoints still set.
        unsafe { qjs::JS_SetBreakpointHandler(rt, None, ptr::null_mut()) };
    });
}

#[test]
fn many_breakpoints() {
    with_global(|cx| {
        // As stepping does: a breakpoint at each instruction of large code, in ascending order
        // (cubic in the first version of the table), and in many scripts.
        let body: String = (0..8000).map(|i| format!("s += {i};\n")).collect();
        let big = compile(cx, &format!("var s = 0;\n{body}s"));
        let small: Vec<_> = (0..64).map(|i| compile(cx, &format!("{i} + 1"))).collect();
        let rt = unsafe { qjs::JS_GetRuntime(script_ctx(big)) };
        unsafe { qjs::JS_SetBreakpointHandler(rt, Some(on_breakpoint), ptr::null_mut()) };
        let all = break_everywhere(big);
        assert!(all.len() > 16_000);
        let last = small.iter().map(|s| break_everywhere(*s)).last().unwrap();
        assert_eq!(execute(cx, big).as_deref(), Some("31996000"));
        assert_eq!(hits(), all, "each instruction runs once, in order");
        assert_eq!(execute(cx, small[63]).as_deref(), Some("64"));
        assert_eq!(hits(), last);
        for pc in all.iter().rev() {
            unsafe { assert_eq!(qjs::JS_ClearBreakpoint(rt, script_value(big), *pc), 0) };
        }
        assert_eq!(execute(cx, big).as_deref(), Some("31996000"));
        assert!(hits().is_empty());
        unsafe { qjs::JS_SetBreakpointHandler(rt, None, ptr::null_mut()) };
    });
}

thread_local! {
    static EVENTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static ENTERS: Cell<usize> = const { Cell::new(0) };
    static THROW_ON_ENTER: Cell<usize> = const { Cell::new(0) };
    static CLOBBER: Cell<bool> = const { Cell::new(false) };
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    static FAIL_EXIT: Cell<u8> = const { Cell::new(0) };
}

fn events() -> Vec<String> {
    EVENTS.with(|e| e.take())
}

unsafe fn describe(ctx: *mut qjs::JSContext, v: qjs::JSValue) -> String {
    if v.tag() == qjs::JS_TAG_INT {
        return v.int().to_string();
    }
    let s = qjs::JS_ToCStringLen(ctx, ptr::null_mut(), v);
    if s.is_null() {
        qjs::JS_FreeValue(ctx, qjs::JS_GetException(ctx));
        return "?".to_owned();
    }
    let r = std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned();
    qjs::JS_FreeCString(ctx, s);
    r
}

unsafe extern "C" fn on_enter(ctx: *mut qjs::JSContext, _: *mut c_void) -> c_int {
    if IN_HOOK.with(Cell::get) {
        return 0;
    }
    EVENTS.with(|e| e.borrow_mut().push("enter".to_owned()));
    let n = ENTERS.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    if THROW_ON_ENTER.with(Cell::get) == n {
        qjs::JS_ThrowTypeError(ctx, c"enter".as_ptr());
        return -1;
    }
    0
}

unsafe extern "C" fn on_exit(
    ctx: *mut qjs::JSContext,
    kind: qjs::JSFrameExitKind,
    value: qjs::JSValue,
    _: *mut c_void,
) -> c_int {
    if IN_HOOK.with(Cell::get) {
        return 0;
    }
    let event = match kind {
        qjs::JS_FRAME_EXIT_RETURN => format!("return:{}", describe(ctx, value)),
        qjs::JS_FRAME_EXIT_THROW => format!("throw:{}", describe(ctx, value)),
        qjs::JS_FRAME_EXIT_TERMINATE => "terminate".to_owned(),
        _ => format!("suspend:{}", describe(ctx, value)),
    };
    EVENTS.with(|e| e.borrow_mut().push(event));
    if CLOBBER.with(Cell::get) {
        // Script that throws and catches, and an exception left behind, while one is pending.
        IN_HOOK.with(|h| h.set(true));
        let src = c"try { throw 1; } catch (e) {}";
        let v = qjs::JS_Eval(ctx, src.as_ptr(), src.count_bytes(), c"hook.js".as_ptr(), 0);
        qjs::JS_FreeValue(ctx, v);
        qjs::JS_ThrowRangeError(ctx, c"left by the hook".as_ptr());
        IN_HOOK.with(|h| h.set(false));
    }
    // The frame of `f` throws, or is terminated, instead.
    match FAIL_EXIT.with(Cell::get) {
        1 if kind == qjs::JS_FRAME_EXIT_RETURN && describe(ctx, value) == "2" => {
            qjs::JS_ThrowTypeError(ctx, c"exit".as_ptr());
            -1
        }
        2 if kind == qjs::JS_FRAME_EXIT_RETURN && describe(ctx, value) == "2" => {
            qjs::JS_ThrowInternalError(ctx, c"terminated".as_ptr());
            qjs::JS_SetUncatchableException(ctx, 1);
            -1
        }
        _ => 0,
    }
}

unsafe extern "C" fn on_native(
    ctx: *mut qjs::JSContext,
    func: qjs::JSValue,
    _: *mut c_void,
) -> c_int {
    let name = qjs::JS_GetPropertyStr(ctx, func, c"name".as_ptr());
    let name = describe(ctx, name);
    let throw = name == "max";
    EVENTS.with(|e| e.borrow_mut().push(format!("native:{name}")));
    if throw {
        qjs::JS_ThrowTypeError(ctx, c"native".as_ptr());
        return -1;
    }
    0
}

unsafe extern "C" fn terminate(_: *mut RawJSContext) -> bool {
    false
}

unsafe extern "C" fn request_termination(cx: *mut RawJSContext, _: u32, _: *mut Value) -> bool {
    jsapi::JS_RequestInterruptCallback(cx);
    true
}

/// Installs the frame hooks, runs `source` as a script, removes them and returns the result.
fn run_with_frame_hooks(cx: &mut JSContext, source: &str) -> Option<String> {
    let script = compile(cx, source);
    let rt = unsafe { qjs::JS_GetRuntime(script_ctx(script)) };
    ENTERS.with(|c| c.set(0));
    unsafe { qjs::JS_SetFrameHooks(rt, Some(on_enter), Some(on_exit), ptr::null_mut()) };
    let result = execute(cx, script);
    unsafe { qjs::JS_SetFrameHooks(rt, None, None, ptr::null_mut()) };
    result
}

#[test]
fn frame_hooks() {
    with_global(|cx| {
        assert_eq!(
            run_with_frame_hooks(cx, "function f(x) { return x + 1; } f(1)").as_deref(),
            Some("2")
        );
        assert_eq!(events(), ["enter", "enter", "return:2", "return:2"]);

        assert_eq!(
            run_with_frame_hooks(cx, "function t() { throw 7; } try { t(); } catch (e) { e }")
                .as_deref(),
            Some("7")
        );
        assert_eq!(events(), ["enter", "enter", "throw:7", "return:7"]);

        assert_eq!(
            run_with_frame_hooks(
                cx,
                "function* g() { yield 1; return 2; } let it = g(); [it.next().value, it.next().value].join()"
            )
            .as_deref(),
            Some("\"1,2\"")
        );
        assert_eq!(
            events(),
            [
                "enter",
                "enter",
                "suspend:undefined",
                "enter",
                "suspend:1",
                "enter",
                "return:2",
                "return:1,2"
            ]
        );

        assert_eq!(
            run_with_frame_hooks(cx, "async function a() { await 5; return 6; } a(); 0").as_deref(),
            Some("0")
        );
        let rt =
            unsafe { qjs::JS_GetRuntime(qjs::JS_GetScriptRealm(script_value(compile(cx, "0")))) };
        unsafe {
            qjs::JS_SetFrameHooks(rt, Some(on_enter), Some(on_exit), ptr::null_mut());
            jsapi::RunJobs(cx.raw_cx());
            qjs::JS_SetFrameHooks(rt, None, None, ptr::null_mut());
        }
        assert_eq!(
            events(),
            [
                "enter",
                "enter",
                "suspend:5",
                "return:0",
                "enter",
                "return:6"
            ]
        );
    });
}

#[test]
fn frame_hooks_keep_the_pending_exception() {
    with_global(|cx| {
        CLOBBER.with(|c| c.set(true));
        let result =
            run_with_frame_hooks(cx, "function t() { throw 7; } try { t(); } catch (e) { e }");
        CLOBBER.with(|c| c.set(false));
        assert_eq!(result.as_deref(), Some("7"));
        assert_eq!(events(), ["enter", "enter", "throw:7", "return:7"]);
    });
}

#[test]
fn exit_hook_fails() {
    with_global(|cx| {
        let source =
            "function f() { return 2; } let r; try { r = f(); } catch (e) { r = e.message; } r";
        FAIL_EXIT.with(|f| f.set(1));
        let thrown = run_with_frame_hooks(cx, source);
        FAIL_EXIT.with(|f| f.set(2));
        let terminated = run_with_frame_hooks(cx, source);
        FAIL_EXIT.with(|f| f.set(0));
        assert_eq!(
            thrown.as_deref(),
            Some("\"exit\""),
            "the frame throws the hook's exception"
        );
        assert_eq!(terminated, None, "an uncatchable one terminates the script");
        events();
    });
}

#[test]
fn enter_hook_throws() {
    with_global(|cx| {
        THROW_ON_ENTER.with(|t| t.set(2));
        let result = run_with_frame_hooks(
            cx,
            "function f() { return 1; } let r; try { f(); } catch (e) { r = e.message; } r",
        );
        THROW_ON_ENTER.with(|t| t.set(0));
        assert_eq!(result.as_deref(), Some("\"enter\""));
        assert_eq!(
            events(),
            ["enter", "enter", "throw:TypeError: enter", "return:enter"]
        );
    });
}

#[test]
fn terminated_frames() {
    with_global(|cx| unsafe {
        assert!(jsapi::JS_AddInterruptCallback(cx.raw_cx(), Some(terminate)));
        rooted!(&in(cx) let global = jsapi::CurrentGlobalOrNull(cx.raw_cx()));
        assert!(!jsapi::JS_DefineFunction(
            cx.raw_cx(),
            global.handle().into(),
            c"requestTermination".as_ptr(),
            Some(request_termination),
            0,
            0
        )
        .is_null());
        let result = run_with_frame_hooks(
            cx,
            "function loop() { requestTermination(); for (;;) {} } loop()",
        );
        assert_eq!(result, None);
        assert_eq!(events(), ["enter", "enter", "terminate", "terminate"]);
    });
}

#[test]
fn native_call_hook() {
    with_global(|cx| {
        let script = compile(
            cx,
            "let r = [[3].map(x => x * 2)[0], new Map().size, (function () { return 4; }).call()];
             try { Math.max(1, 2); } catch (e) { r.push(e.message); }
             r.join()",
        );
        let rt = unsafe { qjs::JS_GetRuntime(script_ctx(script)) };
        unsafe { qjs::JS_SetNativeCallHook(rt, Some(on_native), ptr::null_mut()) };
        let result = execute(cx, script);
        unsafe { qjs::JS_SetNativeCallHook(rt, None, ptr::null_mut()) };
        assert_eq!(result.as_deref(), Some("\"6,0,4,native\""));
        // Native getters are native calls too.
        assert_eq!(
            events(),
            [
                "native:map",
                "native:get [Symbol.species]",
                "native:Map",
                "native:get size",
                "native:call",
                "native:max",
                "native:push",
                "native:join"
            ]
        );
    });
}
