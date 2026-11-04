/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS's debugger hooks, which the emulation of SpiderMonkey's `Debugger` builds on, through
//! QuickJS's own API: breakpoints (Q45), frame and native call hooks (Q46), frames and scripts
//! (Q47), the code of eval() and Function() with sourceURL directives (Q48), and statement
//! positions (Q49).

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

thread_local! {
    static ON_BREAK: RefCell<Option<Box<dyn FnMut(*mut qjs::JSContext)>>> = RefCell::new(None);
}

unsafe extern "C" fn run_on_break(
    ctx: *mut qjs::JSContext,
    _script: qjs::JSValue,
    _pc: u32,
    _opaque: *mut c_void,
) -> c_int {
    let mut f = ON_BREAK.with(|b| b.borrow_mut().take()).expect("one break");
    f(ctx);
    0
}

unsafe fn atom_name(ctx: *mut qjs::JSContext, atom: qjs::JSAtom) -> String {
    if atom == 0 {
        return String::new();
    }
    let s = qjs::JS_AtomToCString(ctx, atom);
    let r = std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned();
    qjs::JS_FreeCString(ctx, s);
    r
}

unsafe fn script_info(script: qjs::JSValue) -> qjs::JSScriptInfo {
    let mut info = std::mem::zeroed();
    assert_eq!(qjs::JS_GetScriptInfo(script, &mut info), 0);
    info
}

unsafe extern "C" fn collect_child(opaque: *mut c_void, child: qjs::JSValue) -> c_int {
    (*(opaque as *mut Vec<qjs::JSValue>)).push(child);
    0
}

unsafe fn children(script: qjs::JSValue) -> Vec<qjs::JSValue> {
    let mut v = Vec::new();
    assert_eq!(
        qjs::JS_IterateScriptChildren(script, Some(collect_child), &mut v as *mut _ as *mut c_void),
        0
    );
    v
}

unsafe extern "C" fn collect_position(
    opaque: *mut c_void,
    pc: u32,
    line: c_int,
    col: c_int,
) -> c_int {
    (*(opaque as *mut Vec<(u32, i32, i32)>)).push((pc, line, col));
    0
}

unsafe fn positions(script: qjs::JSValue) -> Vec<(u32, i32, i32)> {
    let mut v = Vec::new();
    assert_eq!(
        qjs::JS_IterateScriptPositions(
            script,
            Some(collect_position),
            &mut v as *mut _ as *mut c_void
        ),
        0
    );
    v
}

unsafe extern "C" fn collect_offset(opaque: *mut c_void, pc: u32) -> c_int {
    (*(opaque as *mut Vec<u32>)).push(pc);
    0
}

unsafe fn effectful_offsets(rt: *mut qjs::JSRuntime, script: qjs::JSValue) -> Vec<u32> {
    let mut v = Vec::new();
    assert_eq!(
        qjs::JS_IterateScriptEffectfulOffsets(
            rt,
            script,
            Some(collect_offset),
            &mut v as *mut _ as *mut c_void
        ),
        0
    );
    v
}

/// The names of the variables of a kind of a script.
unsafe fn variable_names(
    ctx: *mut qjs::JSContext,
    script: qjs::JSValue,
    kind: qjs::JSVariableKind,
    count: i32,
) -> Vec<String> {
    (0..count)
        .map(|i| {
            let mut var = std::mem::zeroed();
            assert_eq!(qjs::JS_GetScriptVariable(script, kind, i, &mut var), 0);
            atom_name(ctx, var.name)
        })
        .collect()
}

unsafe fn frame_variable(
    ctx: *mut qjs::JSContext,
    sf: *mut qjs::JSStackFrame,
    kind: qjs::JSVariableKind,
    idx: i32,
) -> String {
    let mut v = qjs::JS_UNDEFINED;
    assert_eq!(qjs::JS_GetFrameVariable(ctx, sf, kind, idx, &mut v), 0);
    if v.tag() == qjs::JS_TAG_UNINITIALIZED {
        return "<uninitialized>".to_owned();
    }
    let s = describe(ctx, v);
    qjs::JS_FreeValue(ctx, v);
    s
}

const NESTED: &str = "function outer(a) {
  let captured = a * 2;
  return function inner(b) {
    var sum = b + captured;
    let later = 1;
    return sum + later;
  };
}
var f = outer(5);
f(1)";

#[test]
fn frames_and_variables() {
    with_global(|cx| unsafe {
        let script = compile(cx, NESTED);
        let top = script_value(script);
        let ctx = script_ctx(script);
        let rt = qjs::JS_GetRuntime(ctx);
        let top_info = script_info(top);
        assert!(top_info.is_toplevel != 0 && top_info.is_eval == 0);
        let outer = children(top);
        assert_eq!(outer.len(), 1);
        let inner = children(outer[0]);
        assert_eq!(inner.len(), 1);
        let (outer, inner) = (outer[0], inner[0]);
        let info = script_info(inner);
        assert_eq!(atom_name(ctx, info.func_name), "inner");
        assert!(info.is_toplevel == 0 && info.func_kind == 0 && info.has_debug != 0);
        assert_eq!((info.line_num, info.col_num), (3, 10));
        assert_eq!(info.arg_count, 1);
        assert_eq!(
            variable_names(ctx, inner, qjs::JS_VARIABLE_ARG, info.arg_count),
            ["b"]
        );
        let locals = variable_names(ctx, inner, qjs::JS_VARIABLE_LOCAL, info.var_count);
        let closures = variable_names(ctx, inner, qjs::JS_VARIABLE_CLOSURE, info.closure_var_count);
        let sum = locals.iter().position(|n| n == "sum").unwrap() as i32;
        let later = locals.iter().position(|n| n == "later").unwrap() as i32;
        let captured = closures.iter().position(|n| n == "captured").unwrap() as i32;
        let mut var = std::mem::zeroed();
        assert_eq!(
            qjs::JS_GetScriptVariable(inner, qjs::JS_VARIABLE_LOCAL, later, &mut var),
            0
        );
        assert!(var.is_lexical != 0 && var.is_const == 0);
        assert_eq!(
            qjs::JS_GetScriptVariable(inner, qjs::JS_VARIABLE_CLOSURE, captured, &mut var),
            0
        );
        assert_eq!(var.binding, qjs::JS_BINDING_FUNCTION);
        assert_eq!(
            qjs::JS_GetScriptVariable(inner, qjs::JS_VARIABLE_ARG, 1, &mut var),
            -1
        );

        // Positions: in order, from the function's start, within its lines.
        let pos = positions(inner);
        assert_eq!(pos[0], (0, 3, 10));
        assert!(pos.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(pos.iter().all(|&(_, line, _)| (3..=7).contains(&line)));
        assert!([4, 5, 6].iter().all(|l| pos.iter().any(|p| p.1 == *l)));
        let (last_pc, last_line, last_col) = *pos.last().unwrap();
        let mut col = 0;
        assert_eq!(
            qjs::JS_GetScriptLocation(inner, last_pc, &mut col),
            last_line
        );
        assert_eq!(col, last_col);

        // At the first instruction of `var sum = …` in `inner`, from `f(1)`: lexical variables
        // are uninitialized from the start of their scope, after the function's first instruction.
        let at = pos.iter().find(|p| p.1 == 4).unwrap().0;
        ON_BREAK.with(|b| {
            *b.borrow_mut() = Some(Box::new(move |ctx| {
                let sf = qjs::JS_GetCurrentFrame(rt);
                assert!(!sf.is_null());
                assert_eq!(qjs::JS_GetFrameScript(sf).ptr(), inner.ptr());
                assert_eq!(qjs::JS_GetFramePC(sf), at);
                assert_eq!(qjs::JS_GetFrameThis(sf).tag(), qjs::JS_TAG_UNDEFINED);
                assert_eq!(
                    describe(ctx, qjs::JS_GetFrameFunction(sf))
                        .split('(')
                        .next(),
                    Some("function inner")
                );
                assert_eq!(frame_variable(ctx, sf, qjs::JS_VARIABLE_ARG, 0), "1");
                assert_eq!(
                    frame_variable(ctx, sf, qjs::JS_VARIABLE_LOCAL, sum),
                    "undefined"
                );
                assert_eq!(
                    frame_variable(ctx, sf, qjs::JS_VARIABLE_LOCAL, later),
                    "<uninitialized>"
                );
                assert_eq!(
                    frame_variable(ctx, sf, qjs::JS_VARIABLE_CLOSURE, captured),
                    "10"
                );
                let mut v = qjs::JS_UNDEFINED;
                assert_eq!(
                    qjs::JS_GetFrameVariable(ctx, sf, qjs::JS_VARIABLE_ARG, 5, &mut v),
                    -1
                );
                assert_eq!(
                    qjs::JS_SetFrameVariable(
                        ctx,
                        sf,
                        qjs::JS_VARIABLE_ARG,
                        0,
                        qjs::JS_NewInt32(ctx, 100)
                    ),
                    0
                );
                assert_eq!(
                    qjs::JS_SetFrameVariable(
                        ctx,
                        sf,
                        qjs::JS_VARIABLE_CLOSURE,
                        captured,
                        qjs::JS_NewInt32(ctx, 20)
                    ),
                    0
                );
                let older = qjs::JS_GetOlderFrame(sf);
                assert_eq!(qjs::JS_GetFrameScript(older).ptr(), top.ptr());
                let mut col = 0;
                assert_eq!(
                    qjs::JS_GetScriptLocation(top, qjs::JS_GetFramePC(older), &mut col),
                    10
                );
                // The frames of native functions have no script.
                let mut n = 0;
                let mut f = older;
                while !f.is_null() {
                    n += 1;
                    f = qjs::JS_GetOlderFrame(f);
                }
                assert_eq!(n, 1, "the script's is the oldest frame");
            }));
        });
        qjs::JS_SetBreakpointHandler(rt, Some(run_on_break), ptr::null_mut());
        assert_eq!(qjs::JS_SetBreakpoint(ctx, inner, at), 0);
        assert_eq!(execute(cx, script).as_deref(), Some("121"), "100 + 20 + 1");
        assert!(
            ON_BREAK.with(|b| b.borrow().is_none()),
            "the breakpoint hit"
        );
        assert_eq!(qjs::JS_ClearBreakpoint(rt, inner, at), 0);
        qjs::JS_SetBreakpointHandler(rt, None, ptr::null_mut());
        let _ = outer;
    });
}

#[test]
fn effectful_offsets_and_global_lexicals() {
    with_global(|cx| unsafe {
        let script = compile(
            cx,
            "function g(o) { let z = 2; z++; o.y = z; return z; }
             function h(o) { return o.y + 1; }
             let early = 1; const fixed = 2;
             g({}) + h({ y: 1 })",
        );
        let top = script_value(script);
        let ctx = script_ctx(script);
        let rt = qjs::JS_GetRuntime(ctx);
        let fns = children(top);
        assert_eq!(fns.len(), 2);
        assert_eq!(effectful_offsets(rt, fns[0]).len(), 1, "o.y = z");
        assert!(effectful_offsets(rt, fns[1]).is_empty());
        // The declarations of the script are closure variables bound to it.
        let info = script_info(top);
        let mut decls = Vec::new();
        for i in 0..info.closure_var_count {
            let mut var = std::mem::zeroed();
            assert_eq!(
                qjs::JS_GetScriptVariable(top, qjs::JS_VARIABLE_CLOSURE, i, &mut var),
                0
            );
            if var.binding == qjs::JS_BINDING_GLOBAL_DECL {
                decls.push(atom_name(ctx, var.name));
            }
        }
        decls.sort();
        assert_eq!(decls, ["early", "fixed", "g", "h"]);

        ON_BREAK.with(|b| {
            *b.borrow_mut() = Some(Box::new(|ctx| {
                let atom = qjs::JS_NewAtom(ctx, c"early".as_ptr());
                let mut v = qjs::JS_UNDEFINED;
                assert_eq!(qjs::JS_GetGlobalLexicalVariable(ctx, atom, &mut v), 0);
                assert_eq!(
                    v.tag(),
                    qjs::JS_TAG_UNINITIALIZED,
                    "declared, not initialized"
                );
                qjs::JS_FreeAtom(ctx, atom);
            }));
        });
        qjs::JS_SetBreakpointHandler(rt, Some(run_on_break), ptr::null_mut());
        assert_eq!(qjs::JS_SetBreakpoint(ctx, top, 0), 0);
        assert_eq!(execute(cx, script).as_deref(), Some("5"));
        qjs::JS_SetBreakpointHandler(rt, None, ptr::null_mut());

        let mut tab = ptr::null_mut();
        let mut len = 0;
        assert_eq!(qjs::JS_GetGlobalLexicalNames(ctx, &mut tab, &mut len), 0);
        let mut names: Vec<String> = (0..len as usize)
            .map(|i| atom_name(ctx, (*tab.add(i)).atom))
            .collect();
        qjs::JS_FreePropertyEnum(ctx, tab, len);
        names.sort();
        assert_eq!(names, ["early", "fixed"]);
        let atom = qjs::JS_NewAtom(ctx, c"fixed".as_ptr());
        let mut v = qjs::JS_UNDEFINED;
        assert_eq!(qjs::JS_GetGlobalLexicalVariable(ctx, atom, &mut v), 0);
        assert_eq!(v.int(), 2);
        qjs::JS_FreeAtom(ctx, atom);
        let atom = qjs::JS_NewAtom(ctx, c"g".as_ptr());
        assert_eq!(
            qjs::JS_GetGlobalLexicalVariable(ctx, atom, &mut v),
            -1,
            "g is a global var"
        );
        qjs::JS_FreeAtom(ctx, atom);
    });
}

unsafe fn loop_offsets(rt: *mut qjs::JSRuntime, script: qjs::JSValue) -> Vec<u32> {
    unsafe extern "C" fn collect(opaque: *mut c_void, pc: u32) -> c_int {
        (*(opaque as *mut Vec<u32>)).push(pc);
        0
    }
    let mut offsets = Vec::new();
    assert_eq!(
        qjs::JS_IterateScriptLoopOffsets(
            rt,
            script,
            Some(collect),
            &mut offsets as *mut _ as *mut c_void
        ),
        0
    );
    offsets
}

#[test]
fn loops_and_eval() {
    with_global(|cx| unsafe {
        let script = compile(
            cx,
            "function none(a) { if (a) return 1; return 2; }
             function empty() { for (;;) {} }
             function loops(n) { while (n > 0) n--; do { n++; } while (n < 3); for (const x of [1]) {} }
             function direct(s) { return eval(s); }
             0",
        );
        let top = script_value(script);
        let ctx = script_ctx(script);
        let rt = qjs::JS_GetRuntime(ctx);
        let fns = children(top);
        assert!(loop_offsets(rt, fns[0]).is_empty(), "no loop");
        assert_eq!(
            loop_offsets(rt, fns[1]).len(),
            1,
            "an empty loop jumps back"
        );
        assert!(
            loop_offsets(rt, fns[2]).len() >= 3,
            "while, do-while and for-of"
        );
        assert!(!effectful_offsets(rt, fns[3]).is_empty(), "direct eval");
        let global = qjs::JS_GetGlobalObject(ctx);
        let eval = qjs::JS_GetPropertyStr(ctx, global, c"eval".as_ptr());
        let intrinsic = qjs::JS_GetIntrinsicEval(ctx);
        assert_eq!(eval.ptr(), intrinsic.ptr(), "the realm's eval");
        for v in [eval, intrinsic, global] {
            qjs::JS_FreeValue(ctx, v);
        }
    });
}

unsafe fn source_url(ctx: *mut qjs::JSContext, script: qjs::JSValue) -> Option<String> {
    let atom = qjs::JS_GetScriptSourceURL(ctx, script);
    if atom == 0 {
        return None;
    }
    let name = atom_name(ctx, atom);
    qjs::JS_FreeAtom(ctx, atom);
    Some(name)
}

#[test]
fn source_url_directives() {
    with_global(|cx| unsafe {
        let cases: [(&str, Option<&str>); 13] = [
            (
                "var x = 1;\n//# sourceURL=http://a/b.js\n",
                Some("http://a/b.js"),
            ),
            ("//@ sourceURL=old.js\n1", Some("old.js")),
            ("/*# sourceURL=block.js */ 1", Some("block.js")),
            ("/*# sourceURL=end.js*/ 1", Some("end.js")),
            (
                "//# sourceURL=first.js\n//# sourceURL=last.js",
                Some("last.js"),
            ),
            (
                "function f() {\n//# sourceURL=nested.js\n}",
                Some("nested.js"),
            ),
            ("'//# sourceURL=string.js'", None),
            ("//#sourceURL=nospace.js\n1", None),
            ("1", None),
            // As SpiderMonkey: anywhere in a block comment, empty ones ignored, the URL ends at
            // Unicode white space.
            ("/*\n//# sourceURL=inside.js\n*/ 1", Some("inside.js")),
            ("//# sourceURL=kept.js\n//# sourceURL=\n1", Some("kept.js")),
            ("//# sourceURL=ls.js\u{2028}1", Some("ls.js")),
            ("//# sourceURL=nbsp.js\u{a0}x\n1", Some("nbsp.js")),
        ];
        for (source, expected) in cases {
            let script = compile(cx, source);
            assert_eq!(
                source_url(script_ctx(script), script_value(script)).as_deref(),
                expected,
                "{source:?}"
            );
        }
    });
}

#[test]
fn global_closure_variables() {
    with_global(|cx| unsafe {
        let accessor = compile(
            cx,
            "Object.defineProperty(globalThis, 'acc', { get() { return 'getter'; }, configurable: true })",
        );
        assert!(execute(cx, accessor).is_some());
        // `var acc` doesn't replace the accessor, which is no variable.
        let script = compile(
            cx,
            "var acc, plain = 'var';
             function k() { return acc + ' ' + plain; }
             k()",
        );
        let top = script_value(script);
        let ctx = script_ctx(script);
        let rt = qjs::JS_GetRuntime(ctx);
        let k = children(top)[0];
        let info = script_info(k);
        let names = variable_names(ctx, k, qjs::JS_VARIABLE_CLOSURE, info.closure_var_count);
        let acc = names.iter().position(|n| n == "acc").unwrap() as i32;
        let plain = names.iter().position(|n| n == "plain").unwrap() as i32;
        ON_BREAK.with(|b| {
            *b.borrow_mut() = Some(Box::new(move |ctx| {
                let sf = qjs::JS_GetCurrentFrame(rt);
                let mut v = qjs::JS_UNDEFINED;
                assert_eq!(
                    qjs::JS_GetFrameVariable(ctx, sf, qjs::JS_VARIABLE_CLOSURE, acc, &mut v),
                    -1,
                    "an accessor is no variable"
                );
                assert_eq!(
                    qjs::JS_SetFrameVariable(
                        ctx,
                        sf,
                        qjs::JS_VARIABLE_CLOSURE,
                        acc,
                        qjs::JS_NewInt32(ctx, 5)
                    ),
                    -1
                );
                assert_eq!(
                    frame_variable(ctx, sf, qjs::JS_VARIABLE_CLOSURE, plain),
                    "var"
                );
            }));
        });
        qjs::JS_SetBreakpointHandler(rt, Some(run_on_break), ptr::null_mut());
        assert_eq!(qjs::JS_SetBreakpoint(ctx, k, 0), 0);
        assert_eq!(
            execute(cx, script).as_deref(),
            Some("\"getter var\""),
            "the accessor is untouched"
        );
        qjs::JS_SetBreakpointHandler(rt, None, ptr::null_mut());
    });
}

#[test]
fn calls_and_function_scripts() {
    with_global(|cx| unsafe {
        let script = compile(
            cx,
            "var o = { m() {} }; function F() {} var a = [];
             o.m(1); F(); new F(2); F(...a); o[0];
             F",
        );
        let top = script_value(script);
        let rt = qjs::JS_GetRuntime(script_ctx(script));
        let info = script_info(top);
        let calls: Vec<(qjs::JSCallKind, i32)> = (0..info.byte_code_len)
            .filter_map(|pc| {
                let mut argc = 0;
                let kind = qjs::JS_GetScriptCall(rt, top, pc, &mut argc);
                (kind != qjs::JS_CALL_NONE).then_some((kind, argc))
            })
            .collect();
        assert_eq!(
            calls,
            [
                (qjs::JS_CALL_METHOD, 1),
                (qjs::JS_CALL_FUNCTION, 0),
                (qjs::JS_CALL_CONSTRUCTOR, 1),
                (qjs::JS_CALL_FUNCTION, -1)
            ]
        );
        assert!(execute(cx, script).is_some());
        let fns = children(top);
        let ctx = script_ctx(script);
        let global = qjs::JS_GetGlobalObject(ctx);
        let fun = qjs::JS_GetPropertyStr(ctx, global, c"F".as_ptr());
        let fun_script = qjs::JS_GetFunctionScript(fun);
        assert!(fns.iter().any(|c| c.ptr() == fun_script.ptr()));
        let math = qjs::JS_GetPropertyStr(ctx, global, c"Math".as_ptr());
        let max = qjs::JS_GetPropertyStr(ctx, math, c"max".as_ptr());
        assert_eq!(qjs::JS_GetFunctionScript(max).tag(), qjs::JS_TAG_UNDEFINED);
        for v in [fun, math, max, global] {
            qjs::JS_FreeValue(ctx, v);
        }
    });
}

thread_local! {
    static NEW_SCRIPTS: RefCell<Vec<(bool, String, Option<String>)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "C" fn on_new_script(
    ctx: *mut qjs::JSContext,
    script: qjs::JSValue,
    is_function: c_int,
    source: *const std::ffi::c_char,
    len: usize,
    _: *mut c_void,
) -> c_int {
    let text =
        String::from_utf8_lossy(std::slice::from_raw_parts(source as *const u8, len)).into_owned();
    let url = source_url(ctx, script);
    let info = script_info(script);
    assert!(info.is_toplevel != 0 && info.is_eval != 0);
    let refuse = text == "'refused'";
    NEW_SCRIPTS.with(|n| n.borrow_mut().push((is_function != 0, text, url)));
    if refuse {
        qjs::JS_ThrowTypeError(ctx, c"refused".as_ptr());
        return -1;
    }
    0
}

#[test]
fn new_script_hook() {
    with_global(|cx| unsafe {
        let script = compile(
            cx,
            "[eval('1 + 1 //# sourceURL=e.js'), new Function('return 2')(), (0, eval)('3'),
              (function () { var local = 4; return eval('local'); })()].join()",
        );
        let rt = qjs::JS_GetRuntime(script_ctx(script));
        qjs::JS_SetNewScriptHook(rt, Some(on_new_script), ptr::null_mut());
        let result = execute(cx, script);
        // Code the embedder compiles is not eval code.
        let other = compile(cx, "5");
        assert_eq!(execute(cx, other).as_deref(), Some("5"));
        // A hook that fails throws instead of running the code.
        let refused = compile(
            cx,
            "var ran = false; try { eval(\"ran = true, 'refused'\".slice(12)); } catch (e) { ran = e.message; } ran",
        );
        assert_eq!(execute(cx, refused).as_deref(), Some("\"refused\""));
        NEW_SCRIPTS.with(|n| n.borrow_mut().pop());
        qjs::JS_SetNewScriptHook(rt, None, ptr::null_mut());
        assert_eq!(result.as_deref(), Some("\"2,2,3,4\""));
        let scripts = NEW_SCRIPTS.with(|n| n.take());
        assert_eq!(
            scripts,
            [
                (
                    false,
                    "1 + 1 //# sourceURL=e.js".to_owned(),
                    Some("e.js".to_owned())
                ),
                (
                    true,
                    "(function anonymous(\n) {\nreturn 2\n})".to_owned(),
                    None
                ),
                (false, "3".to_owned(), None),
                (false, "local".to_owned(), None),
            ]
        );
    });
}

unsafe fn statement_positions(
    rt: *mut qjs::JSRuntime,
    script: qjs::JSValue,
) -> Option<Vec<(u32, u32)>> {
    let mut tab = ptr::null();
    let n = qjs::JS_GetScriptStatementPositions(rt, script, &mut tab);
    if n < 0 {
        return None;
    }
    Some(
        (0..n as usize)
            .map(|i| (*tab.add(2 * i), *tab.add(2 * i + 1)))
            .collect(),
    )
}

const STEPPING: &str = "
function end() {
    console.log('end');
    let a = 1;
}
var console = { log() {} };
end();
";

#[test]
fn statement_positions_for_debuggers() {
    with_global(|cx| unsafe {
        let plain = compile(cx, STEPPING);
        let rt = qjs::JS_GetRuntime(script_ctx(plain));
        assert_eq!(
            statement_positions(rt, script_value(plain)),
            None,
            "not recorded by default"
        );

        qjs::JS_SetDebuggerPositions(rt, 1);
        let script = compile(cx, STEPPING);
        qjs::JS_SetDebuggerPositions(rt, 0);
        let top = script_value(script);
        // Statements (declarations at their initializers), the closing braces of `end` and of
        // the method `log`, and the end of the script, in source order; not the function
        // declaration.
        assert_eq!(
            statement_positions(rt, top).unwrap(),
            [(3, 5), (4, 13), (5, 1), (6, 15), (6, 24), (7, 1), (8, 1)]
        );
        let end = children(top)[0];
        let pos = positions(end);
        for (line, col) in [(3, 5), (4, 13), (5, 1)] {
            assert!(
                pos.iter().any(|p| p.1 == line && p.2 == col as i32),
                "a position at {line}:{col} in {pos:?}"
            );
        }
        let top_pos = positions(top);
        assert!(
            top_pos.iter().any(|p| p.1 == 8 && p.2 == 1),
            "the end of the script"
        );
        assert_eq!(execute(cx, script), execute(cx, plain), "the same result");

        // The first instruction of a statement has the statement's position, not that of its
        // first expression.
        qjs::JS_SetDebuggerPositions(rt, 1);
        let script = compile(
            cx,
            "function g(a, b) {\n  let sum = a + b;\n  const t = q.r;\n  return sum;\n}\n\
             async function h() {}\nvar x;\n",
        );
        qjs::JS_SetDebuggerPositions(rt, 0);
        let top = script_value(script);
        assert_eq!(
            statement_positions(rt, top).unwrap(),
            [(2, 13), (3, 13), (4, 3), (6, 21), (8, 1)],
            "not function declarations, declarations without initializer or dead code"
        );
        let pos = positions(children(top)[0]);
        for (line, col) in [(2, 13), (3, 13), (4, 3)] {
            assert!(
                pos.iter().any(|p| p.1 == line && p.2 == col as i32),
                "a position at {line}:{col} in {pos:?}"
            );
        }

        // Conditions, loop heads and jumps, as SpiderMonkey.
        qjs::JS_SetDebuggerPositions(rt, 1);
        let script = compile(
            cx,
            "function loops(n) {\n  for (let i = 0; i < n; i++) {\n    if (i === 2) break;\n  }\n\
             \x20 while (n > 0) n--;\n  for (const k of [1]) {}\n  switch (n) { default: throw 1; }\n}\n",
        );
        qjs::JS_SetDebuggerPositions(rt, 0);
        let top = script_value(script);
        let expected: [(u32, u32); 10] = [
            (2, 16),
            (2, 19),
            (2, 26),
            (3, 9),
            (3, 18),
            (5, 10),
            (5, 17),
            (6, 19),
            (7, 11),
            (7, 25),
        ];
        let listed = statement_positions(rt, top).unwrap();
        assert_eq!(&listed[..expected.len()], expected);
        let pos = positions(children(top)[0]);
        for (line, col) in expected {
            assert!(
                pos.iter().any(|p| p.1 == line as i32 && p.2 == col as i32),
                "a position at {line}:{col} in {pos:?}"
            );
        }
    });
}
