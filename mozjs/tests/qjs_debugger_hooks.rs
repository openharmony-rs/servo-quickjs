/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS's debugger hooks, which the emulation of SpiderMonkey's `Debugger` builds on, through
//! QuickJS's own API: breakpoints (Q45).

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use mozjs::context::JSContext;
use mozjs::conversions::jsstr_to_string;
use mozjs::jsapi::{self, JSScript, OnNewGlobalHookOption};
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
