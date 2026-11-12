/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! SpiderMonkey's `Debugger` API, for Servo's devtools (`debugger.js`). It is emulated in
//! JavaScript (`debugger_api.js`), over the natives defined here, in each global that
//! `JS_DefineDebuggerObject` is called on. Design: `docs/design.md`, "Debugger".
//!
//! QuickJS's hooks for debuggers (patches Q45 to Q49) call the emulations (their "host" objects)
//! only for frames of debuggee realms, and only while they want them: breakpoints are patched in,
//! the frame hooks are installed while an emulation wants to see frames entered or holds a live
//! frame, and the native call hook while one wants native calls.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

use servo_quickjs as q;

use super::proxy::{proxy_class_name, ToWindowIfWindowProxy};
use super::realm::{
    hold_realm, realm_of_global, realm_of_object, realm_of_plain_object, realm_of_qctx,
    release_realm,
};
use super::reentrant::Stack;
use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;

const SOURCE: &str = concat!(include_str!("debugger_api.js"), "\0");

/// The function of `frame.eval`: direct eval of the code inside a `with` statement over a proxy of
/// the frame's variables (its argument), which serves the realm's `eval` and the code
/// (`debuggerCode`) to the evaluator's own lookups, so that it has no bindings code could see.
/// Debuggers don't see its frames, which have its file name.
const FRAME_EVALUATOR: &CStr = c"(function () {\n\
    with (arguments[0]) { return eval(debuggerCode); }\n})";
const FRAME_EVALUATOR_FILE: &CStr = c"resource:///debugger_frame_eval.js";

const CLASS_BYTECODE_FUNCTION: q::JSClassID = 13;
const CLASS_BOUND_FUNCTION: q::JSClassID = 14;
const CLASS_GENERATOR_FUNCTION: q::JSClassID = 16;
const CLASS_PROXY: q::JSClassID = 51;
const CLASS_ASYNC_FUNCTION: q::JSClassID = 55;
const CLASS_ASYNC_GENERATOR_FUNCTION: q::JSClassID = 59;

/// Kinds of completion returned by the natives that run debuggee code, as `[kind, value]`.
const COMPLETION_RETURN: i32 = 0;
const COMPLETION_THROW: i32 = 1;
const COMPLETION_TERMINATED: i32 = 2;

/// What a host's hook callback returns: go on, or terminate the debuggee.
const RESUME_CONTINUE: i32 = 0;
const RESUME_TERMINATE: i32 = 1;

/// The status of a variable read by `frameVariable` and `globalLexicalVariable`.
const VARIABLE_VALUE: i32 = 0;
const VARIABLE_UNINITIALIZED: i32 = 1;
const VARIABLE_MISSING: i32 = 2;

/// The debugger state of a runtime. The `RefCell`s are only borrowed around Rust code.
#[derive(Default)]
pub(crate) struct State {
    /// The host object of each emulation (counted) and the realm of its global (held), which the
    /// shim notifies of new scripts. Empty unless devtools installed a debugger.
    hosts: Stack<(q::JSValue, *mut RealmData)>,
    /// How many emulations have a debugger with a debuggee: Servo installs one in every script
    /// thread, but only adds debuggees while the devtools server runs.
    active_hosts: Cell<usize>,
    next_source_id: Cell<u32>,
    /// The live script handle of each function bytecode, by bytecode pointer (not counted: a
    /// handle removes itself when it is finalized).
    handles: RefCell<HashMap<usize, q::JSValue>>,
    /// The realms of debuggee globals, with how many debuggers debug them.
    debuggee_realms: RefCell<HashMap<usize, usize>>,
    /// The source of the functions of the scripts the emulations were told about, by bytecode
    /// pointer, while the handle of the script lives (see `ScriptData::functions`).
    function_sources: RefCell<HashMap<usize, u32>>,
    /// The breakpoints the emulations set, by bytecode pointer and pc: how many times each is set,
    /// and the bytecode (counted, so that it outlives its breakpoints).
    breakpoints: RefCell<HashMap<(usize, u32), (usize, q::JSValue)>>,
    /// The live frame handles, by frame pointer (not counted).
    frames: RefCell<HashMap<usize, q::JSValue>>,
    /// The atom of `FRAME_EVALUATOR_FILE` (counted), once made.
    evaluator_file: Cell<q::JSAtom>,
    /// How many emulations want to see frames entered, and native calls.
    want_enter: Cell<usize>,
    want_native_calls: Cell<usize>,
    /// Set for the eval() of `frame.eval`: it skips the CSP check (taken by the check) and
    /// introduces "debugger eval" code (taken when the code is reported).
    frame_eval_check: Cell<bool>,
    frame_eval_label: Cell<bool>,
}

impl State {
    pub(crate) fn is_active(&self) -> bool {
        self.active_hosts.get() > 0
    }

    /// Whether the CSP check of eval() is skipped, once, for `frame.eval`.
    pub(crate) fn take_frame_eval_check(&self) -> bool {
        self.frame_eval_check.replace(false)
    }

    unsafe fn is_debuggee_realm(&self, qctx: *mut q::JSContext) -> bool {
        let realm = if qctx.is_null() {
            ptr::null_mut()
        } else {
            realm_of_qctx(qctx)
        };
        !realm.is_null()
            && self
                .debuggee_realms
                .borrow()
                .contains_key(&(realm as usize))
    }
}

/// The realm of a script's code.
unsafe fn script_realm(code: q::JSValue) -> *mut RealmData {
    let qctx = q::JS_GetScriptRealm(code);
    if qctx.is_null() {
        ptr::null_mut()
    } else {
        realm_of_qctx(qctx)
    }
}

/// The global of a realm, as a new reference, or `undefined`.
unsafe fn realm_global(qctx: *mut q::JSContext, realm: *mut RealmData) -> q::JSValue {
    if realm.is_null() || (*realm).global.is_null() {
        q::JS_UNDEFINED
    } else {
        q::JS_DupValue(qctx, obj_val((*realm).global))
    }
}

/// A new class, registered in the runtime.
unsafe fn class(
    rt: *mut q::JSRuntime,
    id: &'static std::sync::OnceLock<q::JSClassID>,
    name: &'static CStr,
    finalizer: q::JSClassFinalizer,
    gc_mark: q::JSClassGCMark,
) -> q::JSClassID {
    let id = *id.get_or_init(|| {
        let mut id = 0;
        q::JS_NewClassID(&mut id);
        id
    });
    if q::JS_IsRegisteredClass(rt, id) == 0 {
        let def = q::JSClassDef {
            class_name: name.as_ptr(),
            finalizer,
            gc_mark,
            call: None,
            exotic: ptr::null_mut(),
        };
        assert_eq!(q::JS_NewClass(rt, id, &def), 0);
    }
    id
}

// Script handles.

/// Opaque data of script handles: the emulation's reference to a function bytecode.
struct ScriptData {
    /// Counted and marked.
    code: q::JSValue,
    /// The `Debugger.Source` id of the code, 0 if unknown.
    source: u32,
    /// For the code of a script the emulations were told about: its functions, whose entries in
    /// `function_sources` this handle owns.
    functions: Vec<usize>,
}

static SCRIPT_CLASS: std::sync::OnceLock<q::JSClassID> = std::sync::OnceLock::new();

unsafe fn script_data<'a>(v: q::JSValue) -> Option<&'a ScriptData> {
    let id = *SCRIPT_CLASS.get()?;
    (q::JS_GetOpaque(v, id) as *const ScriptData).as_ref()
}

unsafe extern "C" fn script_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = q::JS_GetOpaque(val, *SCRIPT_CLASS.get().unwrap()) as *mut ScriptData;
    if data.is_null() {
        return;
    }
    let data = Box::from_raw(data);
    let state = &super::current().debugger;
    let key = data.code.ptr() as usize;
    {
        let mut handles = state.handles.borrow_mut();
        if handles.get(&key).is_some_and(|h| h.ptr() == val.ptr()) {
            handles.remove(&key);
        }
    }
    {
        let mut sources = state.function_sources.borrow_mut();
        for function in &data.functions {
            if sources.get(function) == Some(&data.source) {
                sources.remove(function);
            }
        }
    }
    q::JS_FreeValueRT(rt, data.code);
}

unsafe extern "C" fn script_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    if let Some(data) = script_data(val) {
        q::JS_MarkValue(rt, data.code, mark);
    }
}

unsafe extern "C" fn collect_child(opaque: *mut c_void, child: q::JSValue) -> c_int {
    (*(opaque as *mut Vec<q::JSValue>)).push(child);
    0
}

/// The scripts of the functions a script defines (borrowed).
unsafe fn children(code: q::JSValue) -> Vec<q::JSValue> {
    let mut children = Vec::new();
    q::JS_IterateScriptChildren(
        code,
        Some(collect_child),
        &mut children as *mut _ as *mut c_void,
    );
    children
}

/// The handle of the function bytecode `code` (a new reference). A new handle gets `source`, or
/// the source its functions were registered with if `source` is 0. With `register`, the handle
/// of the code of a script registers its functions as being of `source`.
unsafe fn script_handle(
    cx: &Cx,
    qctx: *mut q::JSContext,
    code: q::JSValue,
    source: u32,
    register: bool,
) -> q::JSValue {
    let key = code.ptr() as usize;
    let existing = cx.debugger.handles.borrow().get(&key).copied();
    if let Some(handle) = existing {
        return q::JS_DupValue(qctx, handle);
    }
    let id = class(
        cx.rt,
        &SCRIPT_CLASS,
        c"DebuggerScriptHandle",
        Some(script_finalize),
        Some(script_gc_mark),
    );
    let handle = q::JS_NewObjectClass(qctx, id as c_int);
    if handle.tag() == q::JS_TAG_EXCEPTION {
        return handle;
    }
    let source = if source != 0 {
        source
    } else {
        cx.debugger
            .function_sources
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(0)
    };
    let mut functions = Vec::new();
    if register {
        let mut pending = children(code);
        while let Some(function) = pending.pop() {
            functions.push(function.ptr() as usize);
            pending.extend(children(function));
        }
        let mut sources = cx.debugger.function_sources.borrow_mut();
        for function in &functions {
            sources.insert(*function, source);
        }
    }
    let data = Box::new(ScriptData {
        code: q::JS_DupValue(qctx, code),
        source,
        functions,
    });
    q::JS_SetOpaque(handle, Box::into_raw(data) as *mut c_void);
    cx.debugger.handles.borrow_mut().insert(key, handle);
    handle
}

// Frame handles.

/// Opaque data of frame handles: a running frame of a debuggee realm, until it stops (returns,
/// throws or suspends).
struct FrameData {
    /// The frame, 0 once it stopped.
    frame: Cell<usize>,
    /// Its code (counted and marked), which outlives the frame.
    code: q::JSValue,
    /// How many emulations want `popFrame` called when it stops.
    on_pop: Cell<usize>,
    /// How many emulations step in it, and the step positions it has breakpoints at meanwhile.
    steppers: Cell<usize>,
    steps: RefCell<Vec<u32>>,
}

static FRAME_CLASS: std::sync::OnceLock<q::JSClassID> = std::sync::OnceLock::new();

unsafe fn frame_data<'a>(v: q::JSValue) -> Option<&'a FrameData> {
    let id = *FRAME_CLASS.get()?;
    (q::JS_GetOpaque(v, id) as *const FrameData).as_ref()
}

unsafe extern "C" fn frame_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = q::JS_GetOpaque(val, *FRAME_CLASS.get().unwrap()) as *mut FrameData;
    if data.is_null() {
        return;
    }
    let data = Box::from_raw(data);
    let cx = super::current();
    let frame = data.frame.get();
    if frame != 0 {
        let mut frames = cx.debugger.frames.borrow_mut();
        if frames.get(&frame).is_some_and(|h| h.ptr() == val.ptr()) {
            frames.remove(&frame);
        }
    }
    for pc in data.steps.take() {
        remove_breakpoint(cx, data.code, pc);
    }
    q::JS_FreeValueRT(rt, data.code);
    update_hooks(cx);
}

unsafe extern "C" fn frame_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    if let Some(data) = frame_data(val) {
        q::JS_MarkValue(rt, data.code, mark);
    }
}

/// The handle of a running frame (a new reference).
unsafe fn frame_handle(
    cx: &Cx,
    qctx: *mut q::JSContext,
    frame: *mut q::JSStackFrame,
) -> q::JSValue {
    let key = frame as usize;
    let existing = cx.debugger.frames.borrow().get(&key).copied();
    if let Some(handle) = existing {
        return q::JS_DupValue(qctx, handle);
    }
    let id = class(
        cx.rt,
        &FRAME_CLASS,
        c"DebuggerFrameHandle",
        Some(frame_finalize),
        Some(frame_gc_mark),
    );
    let handle = q::JS_NewObjectClass(qctx, id as c_int);
    if handle.tag() == q::JS_TAG_EXCEPTION {
        return handle;
    }
    let data = Box::new(FrameData {
        frame: Cell::new(key),
        code: q::JS_DupValue(qctx, q::JS_GetFrameScript(frame)),
        on_pop: Cell::new(0),
        steppers: Cell::new(0),
        steps: RefCell::new(Vec::new()),
    });
    q::JS_SetOpaque(handle, Box::into_raw(data) as *mut c_void);
    cx.debugger.frames.borrow_mut().insert(key, handle);
    update_hooks(cx);
    handle
}

/// The frame stopped: its handle no longer reaches it, and its step breakpoints are removed.
unsafe fn frame_stopped(cx: &Cx, handle: q::JSValue) {
    let Some(data) = frame_data(handle) else {
        return;
    };
    let frame = data.frame.replace(0);
    if frame != 0 {
        cx.debugger.frames.borrow_mut().remove(&frame);
    }
    for pc in data.steps.take() {
        remove_breakpoint(cx, data.code, pc);
    }
    update_hooks(cx);
}

/// Whether debuggers see a frame: one of a function written in JavaScript of a debuggee realm,
/// not one of `frame.eval`'s function.
unsafe fn is_debuggee_frame(cx: &Cx, frame: *mut q::JSStackFrame) -> bool {
    let code = q::JS_GetFrameScript(frame);
    if code.tag() != q::JS_TAG_FUNCTION_BYTECODE
        || !cx.debugger.is_debuggee_realm(q::JS_GetScriptRealm(code))
    {
        return false;
    }
    let evaluator = cx.debugger.evaluator_file.get();
    if evaluator == 0 {
        return true;
    }
    let file = q::JS_GetScriptFileName(cx.base_ctx, code);
    q::JS_FreeAtom(cx.base_ctx, file);
    file != evaluator
}

/// The first frame debuggers see from `frame` on, towards older ones, or null.
unsafe fn debuggee_frame_from(cx: &Cx, mut frame: *mut q::JSStackFrame) -> *mut q::JSStackFrame {
    while !frame.is_null() && !is_debuggee_frame(cx, frame) {
        frame = q::JS_GetOlderFrame(frame);
    }
    frame
}

// Breakpoints, counted per bytecode and pc.

unsafe fn add_breakpoint(cx: &Cx, qctx: *mut q::JSContext, code: q::JSValue, pc: u32) -> bool {
    let key = (code.ptr() as usize, pc);
    if let Some(entry) = cx.debugger.breakpoints.borrow_mut().get_mut(&key) {
        entry.0 += 1;
        return true;
    }
    if q::JS_SetBreakpoint(qctx, code, pc) < 0 {
        if q::JS_HasException(qctx) != 0 {
            q::JS_FreeValue(qctx, q::JS_GetException(qctx));
        }
        return false;
    }
    cx.debugger
        .breakpoints
        .borrow_mut()
        .insert(key, (1, q::JS_DupValue(qctx, code)));
    true
}

unsafe fn remove_breakpoint(cx: &Cx, code: q::JSValue, pc: u32) {
    let key = (code.ptr() as usize, pc);
    let removed = {
        let mut breakpoints = cx.debugger.breakpoints.borrow_mut();
        match breakpoints.get_mut(&key) {
            Some(entry) if entry.0 > 1 => {
                entry.0 -= 1;
                None
            }
            Some(_) => breakpoints.remove(&key).map(|entry| entry.1),
            None => None,
        }
    };
    if let Some(code) = removed {
        q::JS_ClearBreakpoint(cx.rt, code, pc);
        q::JS_FreeValueRT(cx.rt, code);
    }
}

// Hooks.

/// Installs the hooks the emulations need now.
unsafe fn update_hooks(cx: &Cx) {
    let state = &cx.debugger;
    let enter: q::JSFrameEnterHook = (state.want_enter.get() > 0).then_some(frame_enter as _);
    let exit: q::JSFrameExitHook = (!state.frames.borrow().is_empty()).then_some(frame_exit as _);
    q::JS_SetFrameHooks(cx.rt, enter, exit, ptr::null_mut());
    let native: q::JSNativeCallHook =
        (state.want_native_calls.get() > 0).then_some(native_call as _);
    q::JS_SetNativeCallHook(cx.rt, native, ptr::null_mut());
}

/// Calls `method` of each host with the arguments `make_args` makes in its context (consumed).
/// Returns -1 to terminate the debuggee, with an uncatchable exception pending: a host asked for
/// it, or the call was terminated.
unsafe fn call_hosts(
    cx: &Cx,
    ctx: *mut q::JSContext,
    method: &CStr,
    make_args: impl Fn(*mut q::JSContext) -> Vec<q::JSValue>,
) -> c_int {
    let mut terminate = false;
    let mut terminated = false;
    for (host, realm) in cx.debugger.hosts.to_vec() {
        let qctx = (*realm).qctx;
        let mut args = make_args(qctx);
        let f = q::JS_GetPropertyStr(qctx, host, method.as_ptr());
        let r = if q::JS_HasException(qctx) != 0 {
            q::JS_EXCEPTION
        } else {
            cx.enter_realm(realm);
            let mark = cx.pool_mark();
            let r = q::JS_Call(qctx, f, host, args.len() as c_int, args.as_mut_ptr());
            cx.drain_pool(mark);
            cx.leave_realm();
            r
        };
        if r.tag() == q::JS_TAG_EXCEPTION {
            terminated |= q::JS_HasUncatchableException(qctx) != 0;
            q::JS_FreeValue(qctx, q::JS_GetException(qctx));
        } else if r.tag() == q::JS_TAG_INT && r.int() == RESUME_TERMINATE {
            terminate = true;
        }
        q::JS_FreeValue(qctx, r);
        q::JS_FreeValue(qctx, f);
        for arg in args {
            q::JS_FreeValue(qctx, arg);
        }
        if terminate || terminated {
            break;
        }
    }
    if terminate || terminated {
        q::JS_ThrowInternalError(ctx, c"terminated by the debugger".as_ptr());
        q::JS_SetUncatchableException(ctx, 1);
        return -1;
    }
    RESUME_CONTINUE
}

unsafe extern "C" fn breakpoint_hit(
    ctx: *mut q::JSContext,
    script: q::JSValue,
    pc: u32,
    _opaque: *mut c_void,
) -> c_int {
    let cx = super::current();
    let frame = q::JS_GetCurrentFrame(cx.rt);
    if !is_debuggee_frame(cx, frame) {
        return 0;
    }
    let realm = script_realm(script);
    call_hosts(cx, ctx, c"breakpoint", |qctx| {
        vec![
            frame_handle(cx, qctx, frame),
            script_handle(cx, qctx, script, 0, false),
            q::JS_NewInt32(qctx, pc as i32),
            realm_global(qctx, realm),
        ]
    })
}

unsafe extern "C" fn frame_enter(ctx: *mut q::JSContext, _opaque: *mut c_void) -> c_int {
    let cx = super::current();
    let frame = q::JS_GetCurrentFrame(cx.rt);
    if !is_debuggee_frame(cx, frame) {
        return 0;
    }
    let realm = script_realm(q::JS_GetFrameScript(frame));
    call_hosts(cx, ctx, c"enterFrame", |qctx| {
        vec![frame_handle(cx, qctx, frame), realm_global(qctx, realm)]
    })
}

unsafe extern "C" fn frame_exit(
    ctx: *mut q::JSContext,
    kind: q::JSFrameExitKind,
    value: q::JSValue,
    _opaque: *mut c_void,
) -> c_int {
    let cx = super::current();
    let frame = q::JS_GetCurrentFrame(cx.rt);
    let handle = cx.debugger.frames.borrow().get(&(frame as usize)).copied();
    let Some(handle) = handle else {
        return 0;
    };
    let handle = q::JS_DupValue(ctx, handle);
    let mut r = 0;
    if frame_data(handle).is_some_and(|data| data.on_pop.get() > 0) {
        let realm = script_realm(q::JS_GetFrameScript(frame));
        // A termination replaces the frame's completion.
        r = call_hosts(cx, ctx, c"popFrame", |qctx| {
            vec![
                q::JS_DupValue(qctx, handle),
                q::JS_NewInt32(qctx, kind as i32),
                q::JS_DupValue(qctx, value),
                realm_global(qctx, realm),
            ]
        });
    }
    frame_stopped(cx, handle);
    q::JS_FreeValue(ctx, handle);
    r
}

unsafe extern "C" fn native_call(
    ctx: *mut q::JSContext,
    func: q::JSValue,
    _opaque: *mut c_void,
) -> c_int {
    let cx = super::current();
    // The calling code: the innermost scripted frame.
    let mut frame = q::JS_GetCurrentFrame(cx.rt);
    while !frame.is_null() && q::JS_GetFrameScript(frame).tag() != q::JS_TAG_FUNCTION_BYTECODE {
        frame = q::JS_GetOlderFrame(frame);
    }
    if frame.is_null() || !is_debuggee_frame(cx, frame) {
        return 0;
    }
    let realm = script_realm(q::JS_GetFrameScript(frame));
    call_hosts(cx, ctx, c"nativeCall", |qctx| {
        vec![q::JS_DupValue(qctx, func), realm_global(qctx, realm)]
    })
}

/// eval() and Function() compiled code (patch Q48).
unsafe extern "C" fn eval_code(
    ctx: *mut q::JSContext,
    script: q::JSValue,
    is_function: c_int,
    source: *const c_char,
    len: usize,
    _opaque: *mut c_void,
) -> c_int {
    let cx = super::current();
    if !cx.debugger.is_active() {
        return 0;
    }
    let mut text = std::slice::from_raw_parts(source as *const u8, len);
    // Without the parentheses around Function's code.
    if is_function != 0 && text.len() >= 2 {
        text = &text[1..text.len() - 1];
    }
    let introduction_type = if cx.debugger.frame_eval_label.replace(false) {
        c"debugger eval"
    } else if is_function != 0 {
        c"Function"
    } else {
        c"eval"
    };
    let atom = q::JS_GetScriptFileName(ctx, script);
    let url = if atom == 0 {
        ptr::null()
    } else {
        q::JS_AtomToCString(ctx, atom)
    };
    if atom != 0 {
        q::JS_FreeAtom(ctx, atom);
    }
    let url_cstr = if url.is_null() {
        c"<input>"
    } else {
        CStr::from_ptr(url)
    };
    // A termination of the debugger's hook terminates the eval.
    let r = new_script(cx, script, url_cstr, text, introduction_type.as_ptr(), 1);
    if !url.is_null() {
        q::JS_FreeCString(ctx, url);
    }
    r
}

// Installation and notifications.

/// Installs the emulation in the global `obj`: defines `Debugger` there, and registers the
/// emulation for notifications.
pub(crate) unsafe fn JS_DefineDebuggerObject(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let global = *obj.ptr;
    let realm = realm_of_global(global);
    if realm.is_null() {
        q::JS_ThrowTypeError(cx.qctx(), c"JS_DefineDebuggerObject: not a global".as_ptr());
        return false;
    }
    let qctx = (*realm).qctx;
    let factory = q::JS_Eval(
        qctx,
        SOURCE.as_ptr() as *const c_char,
        SOURCE.len() - 1,
        c"resource:///debugger_api.js".as_ptr(),
        q::JS_EVAL_TYPE_GLOBAL as c_int,
    );
    if factory.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let mut natives = [new_natives(qctx)];
    let result = q::JS_Call(qctx, factory, q::JS_UNDEFINED, 1, natives.as_mut_ptr());
    q::JS_FreeValue(qctx, factory);
    q::JS_FreeValue(qctx, natives[0]);
    if result.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let ctor = q::JS_GetPropertyStr(qctx, result, c"Debugger".as_ptr());
    let host = q::JS_GetPropertyStr(qctx, result, c"host".as_ptr());
    q::JS_FreeValue(qctx, result);
    let flags = (q::JS_PROP_WRITABLE | q::JS_PROP_CONFIGURABLE) as c_int;
    if q::JS_DefinePropertyValueStr(qctx, obj_val(global), c"Debugger".as_ptr(), ctor, flags) < 0 {
        q::JS_FreeValue(qctx, host);
        return false;
    }
    cx.debugger.hosts.push((host, hold_realm(realm)));
    q::JS_SetBreakpointHandler(cx.rt, Some(breakpoint_hit), ptr::null_mut());
    q::JS_SetNewScriptHook(cx.rt, Some(eval_code), ptr::null_mut());
    true
}

/// Releases the emulations, before the runtime is destroyed.
pub(crate) unsafe fn free_debuggers(cx: &Cx) {
    let state = &cx.debugger;
    q::JS_SetFrameHooks(cx.rt, None, None, ptr::null_mut());
    q::JS_SetNativeCallHook(cx.rt, None, ptr::null_mut());
    q::JS_SetBreakpointHandler(cx.rt, None, ptr::null_mut());
    q::JS_SetNewScriptHook(cx.rt, None, ptr::null_mut());
    let breakpoints: Vec<_> = state.breakpoints.borrow_mut().drain().collect();
    for ((_, pc), (_, code)) in breakpoints {
        q::JS_ClearBreakpoint(cx.rt, code, pc);
        q::JS_FreeValueRT(cx.rt, code);
    }
    let frames: Vec<_> = state.frames.borrow_mut().drain().collect();
    for (_, handle) in frames {
        if let Some(data) = frame_data(handle) {
            data.frame.set(0);
            data.steps.take();
        }
    }
    while let Some((host, realm)) = state.hosts.pop() {
        q::JS_FreeValueRT(cx.rt, host);
        release_realm(realm);
    }
    q::JS_FreeAtomRT(cx.rt, state.evaluator_file.replace(0));
    state.want_enter.set(0);
    state.want_native_calls.set(0);
}

/// Requests the interrupt again after a debugger's hook was terminated where the termination
/// can't be thrown (the exception pending in `qctx` is dropped), so that it terminates the script
/// the hook interrupted.
pub(crate) unsafe fn reinterrupt(cx: &Cx, qctx: *mut q::JSContext) {
    q::JS_FreeValue(qctx, q::JS_GetException(qctx));
    super::JS_RequestInterruptCallback(cx.as_raw());
}

/// Sets a property of a new object, consuming `value`.
unsafe fn set(qctx: *mut q::JSContext, obj: q::JSValue, name: &CStr, value: q::JSValue) {
    q::JS_SetPropertyStr(qctx, obj, name.as_ptr(), value);
}

unsafe fn new_array(
    qctx: *mut q::JSContext,
    values: impl IntoIterator<Item = q::JSValue>,
) -> q::JSValue {
    let array = q::JS_NewArray(qctx);
    for (i, v) in values.into_iter().enumerate() {
        q::JS_SetPropertyUint32(qctx, array, i as u32, v);
    }
    array
}

unsafe fn int(qctx: *mut q::JSContext, v: i64) -> q::JSValue {
    q::JS_NewInt64(qctx, v)
}

/// The string of an atom, or `null`.
unsafe fn atom_string(qctx: *mut q::JSContext, atom: q::JSAtom) -> q::JSValue {
    if atom == 0 {
        q::JS_NULL
    } else {
        q::JS_AtomToString(qctx, atom)
    }
}

/// Tells the debuggers about the code of a new script, module, event handler or eval code, before
/// it runs, as SpiderMonkey's `onNewScript`. `text` is its source (UTF-8) without the shim's line
/// padding, which starts at `start_line`. Returns -1 if a hook was terminated, with an uncatchable
/// exception pending in the current context (see `reinterrupt`).
#[must_use]
pub(crate) unsafe fn new_script(
    cx: &Cx,
    code: q::JSValue,
    url: &CStr,
    text: &[u8],
    introduction_type: *const c_char,
    start_line: u32,
) -> c_int {
    if !cx.debugger.is_active() || code.tag() != q::JS_TAG_FUNCTION_BYTECODE {
        return 0;
    }
    let id = cx.debugger.next_source_id.get() + 1;
    cx.debugger.next_source_id.set(id);
    let mut statements = ptr::null();
    let count = q::JS_GetScriptStatementPositions(cx.rt, code, &mut statements).max(0) as usize;
    let statements: Vec<i64> = (0..2 * count).map(|i| *statements.add(i) as i64).collect();
    call_hosts(cx, cx.qctx(), c"newScript", |qctx| {
        let info = q::JS_NewObject(qctx);
        set(qctx, info, c"id", int(qctx, id as i64));
        set(qctx, info, c"url", q::JS_NewString(qctx, url.as_ptr()));
        set(
            qctx,
            info,
            c"text",
            q::JS_NewStringLen(qctx, text.as_ptr() as *const c_char, text.len()),
        );
        let introduction_type = if introduction_type.is_null() {
            q::JS_UNDEFINED
        } else {
            q::JS_NewString(qctx, introduction_type)
        };
        set(qctx, info, c"introductionType", introduction_type);
        set(
            qctx,
            info,
            c"startLine",
            int(qctx, start_line.max(1) as i64),
        );
        let atom = q::JS_GetScriptSourceURL(qctx, code);
        set(qctx, info, c"displayURL", atom_string(qctx, atom));
        if atom != 0 {
            q::JS_FreeAtom(qctx, atom);
        }
        let statements = if count == 0 {
            q::JS_NULL
        } else {
            new_array(qctx, statements.iter().map(|v| int(qctx, *v)))
        };
        set(qctx, info, c"statements", statements);
        vec![script_handle(cx, qctx, code, id, true), info]
    })
}

// Natives.

/// Defines the natives `name(qctx, argv) body`, each a `JSCFunction`.
macro_rules! natives {
    ($($(#[$meta:meta])* fn $name:ident($qctx:ident, $argv:ident) $body:block)*) => {
        $(
            $(#[$meta])*
            #[allow(unused_variables)]
            unsafe extern "C" fn $name(
                $qctx: *mut q::JSContext,
                _this: q::JSValue,
                _argc: c_int,
                $argv: *mut q::JSValue,
            ) -> q::JSValue {
                #[allow(unused_unsafe)]
                unsafe { $body }
            }
        )*
    };
}

unsafe fn new_natives(qctx: *mut q::JSContext) -> q::JSValue {
    let natives = q::JS_NewObject(qctx);
    let functions: &[(&CStr, q::JSCFunction, c_int)] = &[
        (c"setActive", Some(n_set_active), 1),
        (c"setDebuggee", Some(n_set_debuggee), 2),
        (c"wantFrameEntries", Some(n_want_frame_entries), 1),
        (c"wantNativeCalls", Some(n_want_native_calls), 1),
        (c"className", Some(n_class_name), 1),
        (c"isProxy", Some(n_is_proxy), 1),
        (c"functionKind", Some(n_function_kind), 1),
        (c"functionScript", Some(n_function_script), 1),
        (c"toGlobal", Some(n_to_global), 1),
        (c"globalOf", Some(n_global_of), 1),
        (c"globalThisOf", Some(n_global_this_of), 1),
        (c"globalLexicalNames", Some(n_global_lexical_names), 1),
        (c"globalLexicalVariable", Some(n_global_lexical_variable), 2),
        (c"scriptGlobal", Some(n_script_global), 1),
        (c"scriptUrl", Some(n_script_url), 1),
        (c"scriptSource", Some(n_script_source), 1),
        (c"scriptInfo", Some(n_script_info), 1),
        (c"scriptVariables", Some(n_script_variables), 1),
        (c"scriptPositions", Some(n_script_positions), 1),
        (c"scriptLocation", Some(n_script_location), 2),
        (c"scriptCall", Some(n_script_call), 2),
        (c"scriptChildren", Some(n_script_children), 1),
        (
            c"scriptEffectfulOffsets",
            Some(n_script_effectful_offsets),
            1,
        ),
        (c"scriptLoopOffsets", Some(n_script_loop_offsets), 1),
        (c"intrinsicEval", Some(n_intrinsic_eval), 1),
        (c"newError", Some(n_new_error), 3),
        (c"setBreakpoint", Some(n_set_breakpoint), 2),
        (c"clearBreakpoint", Some(n_clear_breakpoint), 2),
        (c"frames", Some(n_frames), 0),
        (c"frameOlder", Some(n_frame_older), 1),
        (c"frameAlive", Some(n_frame_alive), 1),
        (c"frameScript", Some(n_frame_script), 1),
        (c"frameOffset", Some(n_frame_offset), 1),
        (c"frameThis", Some(n_frame_this), 1),
        (c"frameCallee", Some(n_frame_callee), 1),
        (c"frameVariable", Some(n_frame_variable), 3),
        (c"setFrameVariable", Some(n_set_frame_variable), 4),
        (c"setOnPop", Some(n_set_on_pop), 2),
        (c"setStepping", Some(n_set_stepping), 2),
        (c"frameEvaluator", Some(n_frame_evaluator), 1),
        (c"evalInFrame", Some(n_eval_in_frame), 3),
        (c"evalInGlobal", Some(n_eval_in_global), 4),
        (c"call", Some(n_call), 3),
    ];
    for (name, f, length) in functions {
        let f = q::JS_NewCFunction2(qctx, *f, name.as_ptr(), *length, q::JS_CFUNC_generic, 0);
        q::JS_SetPropertyStr(qctx, natives, name.as_ptr(), f);
    }
    natives
}

unsafe fn arg(argv: *mut q::JSValue, i: usize) -> q::JSValue {
    *argv.add(i)
}

unsafe fn is_object(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_OBJECT
}

unsafe fn is_true(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_BOOL && v.int() != 0
}

/// An integer argument, or -1.
unsafe fn int_arg(qctx: *mut q::JSContext, v: q::JSValue) -> i64 {
    let mut n = -1i64;
    if q::JS_ToInt64(qctx, &mut n, v) < 0 {
        q::JS_FreeValue(qctx, q::JS_GetException(qctx));
        return -1;
    }
    n
}

/// `[kind, value]`, consuming `value`.
unsafe fn completion(qctx: *mut q::JSContext, kind: i32, value: q::JSValue) -> q::JSValue {
    new_array(qctx, [q::JS_NewInt32(qctx, kind), value])
}

/// The completion of `result`, the result of running debuggee code: its value, the exception it
/// threw, or a termination (an uncatchable exception), which the debugger survives.
unsafe fn completion_of(qctx: *mut q::JSContext, result: q::JSValue) -> q::JSValue {
    if result.tag() != q::JS_TAG_EXCEPTION {
        return completion(qctx, COMPLETION_RETURN, result);
    }
    if q::JS_HasUncatchableException(qctx) != 0 {
        q::JS_FreeValue(qctx, q::JS_GetException(qctx));
        return completion(qctx, COMPLETION_TERMINATED, q::JS_UNDEFINED);
    }
    completion(qctx, COMPLETION_THROW, q::JS_GetException(qctx))
}

unsafe fn handle_data<'a>(
    qctx: *mut q::JSContext,
    v: q::JSValue,
) -> Result<&'a ScriptData, q::JSValue> {
    script_data(v).ok_or_else(|| q::JS_ThrowTypeError(qctx, c"not a script handle".as_ptr()))
}

/// The frame of a frame handle, or null once it stopped.
unsafe fn live_frame(v: q::JSValue) -> *mut q::JSStackFrame {
    frame_data(v).map_or(ptr::null_mut(), |data| {
        data.frame.get() as *mut q::JSStackFrame
    })
}

/// A counter of emulations that want something, changed by `true` or `false`.
unsafe fn count(counter: &Cell<usize>, v: q::JSValue) {
    if is_true(v) {
        counter.set(counter.get() + 1);
    } else {
        counter.set(counter.get().saturating_sub(1));
    }
}

/// The realm of a global argument (or of a WindowProxy), or null.
unsafe fn global_realm(v: q::JSValue) -> *mut RealmData {
    if !is_object(v) {
        return ptr::null_mut();
    }
    let obj = ToWindowIfWindowProxy(val_obj(v));
    if obj.is_null() {
        ptr::null_mut()
    } else {
        realm_of_global(obj)
    }
}

/// A UTF-8 copy of a string argument, NUL-terminated, or `None` with an exception pending.
unsafe fn string_arg(qctx: *mut q::JSContext, v: q::JSValue) -> Option<Vec<u8>> {
    let mut len = 0;
    let s = q::JS_ToCStringLen(qctx, &mut len, v);
    if s.is_null() {
        return None;
    }
    let mut buf = Vec::with_capacity(len + 1);
    buf.extend_from_slice(std::slice::from_raw_parts(s as *const u8, len));
    buf.push(0);
    q::JS_FreeCString(qctx, s);
    Some(buf)
}

natives! {
    /// `setActive(active)`: whether a debugger of the emulation has a debuggee, so that the shim
    /// tells it about new scripts and the parser records statement positions. Called when that
    /// changes.
    fn n_set_active(_qctx, argv) {
        let state = &super::current().debugger;
        count(&state.active_hosts, arg(argv, 0));
        q::JS_SetDebuggerPositions(super::current().rt, state.is_active() as c_int);
        q::JS_UNDEFINED
    }

    /// `setDebuggee(global, debuggee)`: a debugger starts or stops debugging a global, whose
    /// frames hooks then see.
    fn n_set_debuggee(_qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        if !realm.is_null() {
            let mut realms = super::current().debugger.debuggee_realms.borrow_mut();
            let key = realm as usize;
            if is_true(arg(argv, 1)) {
                *realms.entry(key).or_insert(0) += 1;
            } else if let Some(n) = realms.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    realms.remove(&key);
                }
            }
        }
        q::JS_UNDEFINED
    }

    /// `wantFrameEntries(want)`: whether the emulation wants `enterFrame` calls. Counted.
    fn n_want_frame_entries(_qctx, argv) {
        let cx = super::current();
        count(&cx.debugger.want_enter, arg(argv, 0));
        update_hooks(cx);
        q::JS_UNDEFINED
    }

    /// `wantNativeCalls(want)`: whether the emulation wants `nativeCall` calls. Counted.
    fn n_want_native_calls(_qctx, argv) {
        let cx = super::current();
        count(&cx.debugger.want_native_calls, arg(argv, 0));
        update_hooks(cx);
        q::JS_UNDEFINED
    }

    /// The class name of an object, as `Debugger.Object.prototype.class`: that of a proxy is the
    /// one its handler reports (a WindowProxy's is its window's).
    fn n_class_name(qctx, argv) {
        let v = arg(argv, 0);
        if !is_object(v) {
            return q::JS_ThrowTypeError(qctx, c"not an object".as_ptr());
        }
        if q::JS_IsFunction(qctx, v) != 0 {
            return q::JS_NewString(qctx, c"Function".as_ptr());
        }
        if let Some(name) = proxy_class_name(super::current(), val_obj(v)) {
            if !name.is_null() {
                return q::JS_NewString(qctx, name);
            }
        }
        let mut buf = [0 as c_char; 64];
        let name = q::JS_GetClassNameRT(
            q::JS_GetRuntime(qctx),
            q::JS_GetClassID(v),
            buf.as_mut_ptr(),
            buf.len() as c_int,
        );
        q::JS_NewString(qctx, name)
    }

    /// Whether a value is a JS Proxy (not one of the shim's proxies, such as Servo's DOM proxies).
    fn n_is_proxy(_qctx, argv) {
        let v = arg(argv, 0);
        if is_object(v) && q::JS_GetClassID(v) == CLASS_PROXY {
            q::JS_TRUE
        } else {
            q::JS_FALSE
        }
    }

    /// -1 for a value that is not a function, 0 for a native function, 1 for an ordinary
    /// function, 2 for a generator, 3 for an async function, 4 for an async generator and 5 for a
    /// bound function.
    fn n_function_kind(qctx, argv) {
        let v = arg(argv, 0);
        let kind = if !is_object(v) || q::JS_IsFunction(qctx, v) == 0 {
            -1
        } else {
            match q::JS_GetClassID(v) {
                CLASS_BYTECODE_FUNCTION => 1,
                CLASS_GENERATOR_FUNCTION => 2,
                CLASS_ASYNC_FUNCTION => 3,
                CLASS_ASYNC_GENERATOR_FUNCTION => 4,
                CLASS_BOUND_FUNCTION => 5,
                _ => 0,
            }
        };
        q::JS_NewInt32(qctx, kind)
    }

    /// The script handle of a function written in JavaScript, or `undefined`.
    fn n_function_script(qctx, argv) {
        let code = q::JS_GetFunctionScript(arg(argv, 0));
        if code.tag() != q::JS_TAG_FUNCTION_BYTECODE {
            return q::JS_UNDEFINED;
        }
        script_handle(super::current(), qctx, code, 0, false)
    }

    /// The global a value is (that of a WindowProxy), or `undefined`.
    fn n_to_global(qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        realm_global(qctx, realm)
    }

    /// The global of the realm of an object, where known (see `realm_of_plain_object`), or
    /// `undefined`.
    fn n_global_of(qctx, argv) {
        let v = arg(argv, 0);
        if !is_object(v) {
            return q::JS_UNDEFINED;
        }
        let obj = val_obj(v);
        let mut realm = realm_of_global(obj);
        if realm.is_null() {
            realm = realm_of_plain_object(super::current(), obj);
        }
        realm_global(qctx, realm)
    }

    /// The `this` of the global code of a global (its WindowProxy for a Window).
    fn n_global_this_of(qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        if realm.is_null() {
            return q::JS_UNDEFINED;
        }
        q::JS_GetGlobalThis((*realm).qctx)
    }

    /// The names of the global lexical declarations of a global.
    fn n_global_lexical_names(qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        if realm.is_null() {
            return new_array(qctx, []);
        }
        let target = (*realm).qctx;
        let mut tab = ptr::null_mut();
        let mut len = 0;
        if q::JS_GetGlobalLexicalNames(target, &mut tab, &mut len) < 0 {
            return q::JS_EXCEPTION;
        }
        let names: Vec<q::JSValue> = (0..len as usize)
            .map(|i| q::JS_AtomToString(qctx, (*tab.add(i)).atom))
            .collect();
        q::JS_FreePropertyEnum(target, tab, len);
        new_array(qctx, names)
    }

    /// `globalLexicalVariable(global, name)`: `[status, value]` of a global lexical declaration.
    fn n_global_lexical_variable(qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        if realm.is_null() {
            return completion(qctx, VARIABLE_MISSING, q::JS_UNDEFINED);
        }
        let target = (*realm).qctx;
        let atom = q::JS_ValueToAtom(qctx, arg(argv, 1));
        if atom == 0 {
            return q::JS_EXCEPTION;
        }
        let mut value = q::JS_UNDEFINED;
        let found = q::JS_GetGlobalLexicalVariable(target, atom, &mut value) == 0;
        q::JS_FreeAtom(qctx, atom);
        if !found {
            completion(qctx, VARIABLE_MISSING, q::JS_UNDEFINED)
        } else if value.tag() == q::JS_TAG_UNINITIALIZED {
            completion(qctx, VARIABLE_UNINITIALIZED, q::JS_UNDEFINED)
        } else {
            completion(qctx, VARIABLE_VALUE, value)
        }
    }

    /// The global of the realm a script was compiled in, or `undefined`.
    fn n_script_global(qctx, argv) {
        match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => realm_global(qctx, script_realm(data.code)),
            Err(e) => e,
        }
    }

    /// The file name a script was compiled with.
    fn n_script_url(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let atom = q::JS_GetScriptFileName(qctx, data.code);
        if atom == 0 {
            return q::JS_UNDEFINED;
        }
        let v = q::JS_AtomToString(qctx, atom);
        q::JS_FreeAtom(qctx, atom);
        v
    }

    /// The source id of a script, 0 if unknown.
    fn n_script_source(qctx, argv) {
        match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => int(qctx, data.source as i64),
            Err(e) => e,
        }
    }

    /// The name, kind and start of the function of a script (see `JSScriptInfo`).
    fn n_script_info(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let mut info: q::JSScriptInfo = std::mem::zeroed();
        q::JS_GetScriptInfo(data.code, &mut info);
        let obj = q::JS_NewObject(qctx);
        set(qctx, obj, c"name", atom_string(qctx, info.func_name));
        set(qctx, obj, c"kind", int(qctx, info.func_kind as i64));
        set(qctx, obj, c"isToplevel", q::JS_NewBool(qctx, info.is_toplevel));
        set(qctx, obj, c"isEval", q::JS_NewBool(qctx, info.is_eval));
        set(qctx, obj, c"isStrict", q::JS_NewBool(qctx, info.is_strict));
        let module = !q::JS_GetScriptModule(data.code).is_null() && info.is_toplevel != 0;
        set(qctx, obj, c"isModule", q::JS_NewBool(qctx, module as c_int));
        set(qctx, obj, c"line", int(qctx, info.line_num as i64));
        set(qctx, obj, c"column", int(qctx, info.col_num as i64));
        set(qctx, obj, c"length", int(qctx, info.byte_code_len as i64));
        obj
    }

    /// The variables of a script: `{args, locals, closures}`, arrays of `[name, lexical, const]`
    /// (and `binding` for closures, see `JSBindingKind`), names `null` when unknown.
    fn n_script_variables(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let mut info: q::JSScriptInfo = std::mem::zeroed();
        q::JS_GetScriptInfo(data.code, &mut info);
        let vars = |kind: q::JSVariableKind, n: c_int, with_binding: bool| {
            let items: Vec<q::JSValue> = (0..n)
                .map(|i| {
                    let mut var: q::JSScriptVariable = std::mem::zeroed();
                    q::JS_GetScriptVariable(data.code, kind, i, &mut var);
                    let mut fields = vec![
                        atom_string(qctx, var.name),
                        q::JS_NewBool(qctx, var.is_lexical),
                        q::JS_NewBool(qctx, var.is_const),
                    ];
                    if with_binding {
                        fields.push(int(qctx, var.binding as i64));
                    }
                    new_array(qctx, fields)
                })
                .collect();
            new_array(qctx, items)
        };
        let obj = q::JS_NewObject(qctx);
        set(qctx, obj, c"args", vars(q::JS_VARIABLE_ARG, info.arg_count, false));
        set(qctx, obj, c"locals", vars(q::JS_VARIABLE_LOCAL, info.var_count, false));
        set(
            qctx,
            obj,
            c"closures",
            vars(q::JS_VARIABLE_CLOSURE, info.closure_var_count, true),
        );
        obj
    }

    /// The positions of a script, `[pc, line, column, …]` (see `JS_IterateScriptPositions`).
    fn n_script_positions(qctx, argv) {
        unsafe extern "C" fn collect(opaque: *mut c_void, pc: u32, line: c_int, col: c_int) -> c_int {
            (*(opaque as *mut Vec<i64>)).extend([pc as i64, line as i64, col as i64]);
            0
        }
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let mut positions: Vec<i64> = Vec::new();
        q::JS_IterateScriptPositions(
            data.code,
            Some(collect),
            &mut positions as *mut _ as *mut c_void,
        );
        new_array(qctx, positions.into_iter().map(|v| int(qctx, v)))
    }

    /// `scriptLocation(script, pc)`: `[line, column]` of the instruction at `pc`.
    fn n_script_location(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let pc = int_arg(qctx, arg(argv, 1)).max(0) as u32;
        let mut col = 0;
        let line = q::JS_GetScriptLocation(data.code, pc, &mut col);
        new_array(qctx, [int(qctx, line as i64), int(qctx, col as i64)])
    }

    /// `scriptCall(script, pc)`: `[kind, argc]` of the call at `pc` (see `JS_GetScriptCall`).
    fn n_script_call(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let pc = int_arg(qctx, arg(argv, 1));
        let mut argc = -1;
        let kind = if pc < 0 {
            q::JS_CALL_NONE
        } else {
            q::JS_GetScriptCall(q::JS_GetRuntime(qctx), data.code, pc as u32, &mut argc)
        };
        new_array(qctx, [int(qctx, kind as i64), int(qctx, argc as i64)])
    }

    /// The handles of the scripts of the functions a script defines.
    fn n_script_children(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let cx = super::current();
        let children: Vec<q::JSValue> = children(data.code)
            .into_iter()
            .map(|child| script_handle(cx, qctx, child, data.source, false))
            .collect();
        new_array(qctx, children)
    }

    /// The offsets of the backward jumps of a script, one of which each loop iteration runs.
    fn n_script_loop_offsets(qctx, argv) {
        unsafe extern "C" fn collect(opaque: *mut c_void, pc: u32) -> c_int {
            (*(opaque as *mut Vec<i64>)).push(pc as i64);
            0
        }
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let mut offsets: Vec<i64> = Vec::new();
        q::JS_IterateScriptLoopOffsets(
            q::JS_GetRuntime(qctx),
            data.code,
            Some(collect),
            &mut offsets as *mut _ as *mut c_void,
        );
        new_array(qctx, offsets.into_iter().map(|v| int(qctx, v)))
    }

    /// The realm's own `eval` of a global, whatever the global's property is.
    fn n_intrinsic_eval(qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        if realm.is_null() {
            return q::JS_ThrowTypeError(qctx, c"intrinsicEval: not a global".as_ptr());
        }
        q::JS_GetIntrinsicEval((*realm).qctx)
    }

    /// `newError(global, kind, message)`: a new TypeError (kind 0) or ReferenceError (1) of the
    /// realm of a global.
    fn n_new_error(qctx, argv) {
        let realm = global_realm(arg(argv, 0));
        let Some(message) = string_arg(qctx, arg(argv, 2)) else {
            return q::JS_EXCEPTION;
        };
        if realm.is_null() {
            return q::JS_ThrowTypeError(qctx, c"newError: not a global".as_ptr());
        }
        let target = (*realm).qctx;
        let fmt = c"%s".as_ptr();
        let message = message.as_ptr() as *const c_char;
        if int_arg(qctx, arg(argv, 1)) == 1 {
            q::JS_ThrowReferenceError(target, fmt, message);
        } else {
            q::JS_ThrowTypeError(target, fmt, message);
        }
        q::JS_GetException(target)
    }

    /// The offsets of the instructions of a script that change state outside the frame.
    fn n_script_effectful_offsets(qctx, argv) {
        unsafe extern "C" fn collect(opaque: *mut c_void, pc: u32) -> c_int {
            (*(opaque as *mut Vec<i64>)).push(pc as i64);
            0
        }
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let mut offsets: Vec<i64> = Vec::new();
        q::JS_IterateScriptEffectfulOffsets(
            q::JS_GetRuntime(qctx),
            data.code,
            Some(collect),
            &mut offsets as *mut _ as *mut c_void,
        );
        new_array(qctx, offsets.into_iter().map(|v| int(qctx, v)))
    }

    /// `setBreakpoint(script, pc)`: sets a breakpoint (counted); false if `pc` is not an
    /// instruction of the script.
    fn n_set_breakpoint(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let pc = int_arg(qctx, arg(argv, 1));
        let ok = pc >= 0 && add_breakpoint(super::current(), qctx, data.code, pc as u32);
        q::JS_NewBool(qctx, ok as c_int)
    }

    /// `clearBreakpoint(script, pc)`: removes a breakpoint `setBreakpoint` set.
    fn n_clear_breakpoint(qctx, argv) {
        let data = match handle_data(qctx, arg(argv, 0)) {
            Ok(data) => data,
            Err(e) => return e,
        };
        let pc = int_arg(qctx, arg(argv, 1));
        if pc >= 0 {
            remove_breakpoint(super::current(), data.code, pc as u32);
        }
        q::JS_UNDEFINED
    }

    /// The handles of the running frames debuggers see, innermost first.
    fn n_frames(qctx, _argv) {
        let cx = super::current();
        let mut handles = Vec::new();
        let mut frame = debuggee_frame_from(cx, q::JS_GetCurrentFrame(cx.rt));
        while !frame.is_null() {
            handles.push(frame_handle(cx, qctx, frame));
            frame = debuggee_frame_from(cx, q::JS_GetOlderFrame(frame));
        }
        new_array(qctx, handles)
    }

    /// The next older frame debuggers see, or `null`.
    fn n_frame_older(qctx, argv) {
        let cx = super::current();
        let frame = live_frame(arg(argv, 0));
        if frame.is_null() {
            return q::JS_NULL;
        }
        let older = debuggee_frame_from(cx, q::JS_GetOlderFrame(frame));
        if older.is_null() {
            q::JS_NULL
        } else {
            frame_handle(cx, qctx, older)
        }
    }

    /// Whether a frame is still running.
    fn n_frame_alive(qctx, argv) {
        q::JS_NewBool(qctx, !live_frame(arg(argv, 0)).is_null() as c_int)
    }

    /// The script handle of a frame's code.
    fn n_frame_script(qctx, argv) {
        match frame_data(arg(argv, 0)) {
            Some(data) => script_handle(super::current(), qctx, data.code, 0, false),
            None => q::JS_UNDEFINED,
        }
    }

    /// The offset of the instruction a frame runs (see `JS_GetFramePC`): that of the breakpoint
    /// in a breakpoint hook, in the call in older frames.
    fn n_frame_offset(qctx, argv) {
        let frame = live_frame(arg(argv, 0));
        if frame.is_null() {
            return q::JS_UNDEFINED;
        }
        int(qctx, q::JS_GetFramePC(frame) as i64)
    }

    /// A frame's `this`.
    fn n_frame_this(qctx, argv) {
        let frame = live_frame(arg(argv, 0));
        if frame.is_null() {
            return q::JS_UNDEFINED;
        }
        q::JS_DupValue(qctx, q::JS_GetFrameThis(frame))
    }

    /// A frame's function.
    fn n_frame_callee(qctx, argv) {
        let frame = live_frame(arg(argv, 0));
        if frame.is_null() {
            return q::JS_UNDEFINED;
        }
        q::JS_DupValue(qctx, q::JS_GetFrameFunction(frame))
    }

    /// `frameVariable(frame, kind, index)`: `[status, value]` of a variable (see
    /// `JSVariableKind`).
    fn n_frame_variable(qctx, argv) {
        let frame = live_frame(arg(argv, 0));
        let kind = int_arg(qctx, arg(argv, 1));
        let idx = int_arg(qctx, arg(argv, 2));
        let mut value = q::JS_UNDEFINED;
        if frame.is_null()
            || q::JS_GetFrameVariable(qctx, frame, kind as q::JSVariableKind, idx as c_int, &mut value)
                < 0
        {
            return completion(qctx, VARIABLE_MISSING, q::JS_UNDEFINED);
        }
        if value.tag() == q::JS_TAG_UNINITIALIZED {
            completion(qctx, VARIABLE_UNINITIALIZED, q::JS_UNDEFINED)
        } else {
            completion(qctx, VARIABLE_VALUE, value)
        }
    }

    /// `setFrameVariable(frame, kind, index, value)`: whether the variable was set.
    fn n_set_frame_variable(qctx, argv) {
        let frame = live_frame(arg(argv, 0));
        let kind = int_arg(qctx, arg(argv, 1));
        let idx = int_arg(qctx, arg(argv, 2));
        let ok = !frame.is_null()
            && q::JS_SetFrameVariable(
                qctx,
                frame,
                kind as q::JSVariableKind,
                idx as c_int,
                q::JS_DupValue(qctx, arg(argv, 3)),
            ) == 0;
        q::JS_NewBool(qctx, ok as c_int)
    }

    /// `setOnPop(frame, onPop)`: an emulation starts (`true`) or stops wanting `popFrame` called
    /// when the frame stops. Counted.
    fn n_set_on_pop(_qctx, argv) {
        if let Some(data) = frame_data(arg(argv, 0)) {
            count(&data.on_pop, arg(argv, 1));
        }
        q::JS_UNDEFINED
    }

    /// `setStepping(frame, offsets)`: an emulation starts stepping in the frame, with breakpoints
    /// at `offsets` (the same for every emulation) while it runs, or stops (`null`). Counted.
    fn n_set_stepping(qctx, argv) {
        let cx = super::current();
        let Some(data) = frame_data(arg(argv, 0)) else {
            return q::JS_UNDEFINED;
        };
        let offsets = arg(argv, 1);
        let before = data.steppers.get();
        count(&data.steppers, q::JS_NewBool(qctx, is_object(offsets) as c_int));
        if data.steppers.get() == 0 {
            for pc in data.steps.take() {
                remove_breakpoint(cx, data.code, pc);
            }
        }
        if before > 0 || data.steppers.get() == 0 || data.frame.get() == 0 {
            return q::JS_UNDEFINED;
        }
        let length = q::JS_GetPropertyStr(qctx, offsets, c"length".as_ptr());
        let n = int_arg(qctx, length);
        q::JS_FreeValue(qctx, length);
        let mut steps = Vec::new();
        for i in 0..n.max(0) as u32 {
            let v = q::JS_GetPropertyUint32(qctx, offsets, i);
            let pc = int_arg(qctx, v);
            q::JS_FreeValue(qctx, v);
            if pc >= 0 && add_breakpoint(cx, qctx, data.code, pc as u32) {
                steps.push(pc as u32);
            }
        }
        *data.steps.borrow_mut() = steps;
        q::JS_UNDEFINED
    }

    /// The `frame.eval` function of a global (a new one).
    fn n_frame_evaluator(qctx, argv) {
        let cx = super::current();
        let realm = global_realm(arg(argv, 0));
        if realm.is_null() {
            return q::JS_ThrowTypeError(qctx, c"frameEvaluator: not a global".as_ptr());
        }
        let target = (*realm).qctx;
        if cx.debugger.evaluator_file.get() == 0 {
            cx.debugger
                .evaluator_file
                .set(q::JS_NewAtom(cx.base_ctx, FRAME_EVALUATOR_FILE.as_ptr()));
        }
        cx.enter_realm(realm);
        let f = q::JS_Eval(
            target,
            FRAME_EVALUATOR.as_ptr(),
            FRAME_EVALUATOR.count_bytes(),
            FRAME_EVALUATOR_FILE.as_ptr(),
            q::JS_EVAL_TYPE_GLOBAL as c_int,
        );
        cx.leave_realm();
        f
    }

    /// `evalInFrame(evaluator, this, scope)`: calls a `frameEvaluator` function, whose eval()
    /// skips the CSP check and introduces "debugger eval" code. Returns the completion.
    fn n_eval_in_frame(qctx, argv) {
        let cx = super::current();
        let f = arg(argv, 0);
        let mut args = [arg(argv, 2)];
        let realm = if is_object(f) {
            realm_of_object(cx, val_obj(f))
        } else {
            ptr::null_mut()
        };
        if realm.is_null() {
            return q::JS_ThrowTypeError(qctx, c"evalInFrame: not an evaluator".as_ptr());
        }
        cx.debugger.frame_eval_check.set(true);
        cx.debugger.frame_eval_label.set(true);
        cx.enter_realm(realm);
        let result = q::JS_Call(qctx, f, arg(argv, 1), 1, args.as_mut_ptr());
        cx.leave_realm();
        cx.debugger.frame_eval_check.set(false);
        cx.debugger.frame_eval_label.set(false);
        completion_of(qctx, result)
    }

    /// `evalInGlobal(global, code, url, noDeclarations)`: compiles `code` as a classic script of
    /// `global`, tells the debuggers about it (introduction type "debugger eval"), and runs it.
    /// With `noDeclarations`, code that declares global bindings doesn't run and is terminated.
    /// Returns its completion.
    fn n_eval_in_global(qctx, argv) {
        let cx = super::current();
        let realm = global_realm(arg(argv, 0));
        if realm.is_null() {
            return q::JS_ThrowTypeError(qctx, c"evalInGlobal: not a global".as_ptr());
        }
        let Some(src) = string_arg(qctx, arg(argv, 1)) else {
            return q::JS_EXCEPTION;
        };
        let Some(url) = string_arg(qctx, arg(argv, 2)) else {
            return q::JS_EXCEPTION;
        };
        let url = CStr::from_bytes_with_nul(&url).unwrap_or(c"debugger eval code");
        let len = src.len() - 1;
        let target = (*realm).qctx;
        cx.enter_realm(realm);
        let flags = (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as c_int;
        let compiled = q::JS_Eval(target, src.as_ptr() as *const c_char, len, url.as_ptr(), flags);
        let result = if compiled.tag() == q::JS_TAG_EXCEPTION {
            compiled
        } else if is_true(arg(argv, 3)) && declares_globals(compiled) {
            q::JS_FreeValue(target, compiled);
            q::JS_ThrowInternalError(target, c"declarations".as_ptr());
            q::JS_SetUncatchableException(target, 1);
            q::JS_EXCEPTION
        } else {
            // A termination of a hook terminates the debugger's code that runs.
            if new_script(cx, compiled, url, &src[..len], c"debugger eval".as_ptr(), 1) < 0 {
                reinterrupt(cx, target);
            }
            q::JS_EvalFunction(target, compiled)
        };
        cx.leave_realm();
        completion_of(qctx, result)
    }

    /// `call(f, thisArg, args)`: calls `f` in its realm, and returns the completion.
    fn n_call(qctx, argv) {
        let f = arg(argv, 0);
        let args = arg(argv, 2);
        let length = q::JS_GetPropertyStr(qctx, args, c"length".as_ptr());
        let len = int_arg(qctx, length);
        q::JS_FreeValue(qctx, length);
        let mut values = Vec::with_capacity(len.max(0) as usize);
        for i in 0..len.max(0) as u32 {
            let v = q::JS_GetPropertyUint32(qctx, args, i);
            if v.tag() == q::JS_TAG_EXCEPTION {
                for v in values {
                    q::JS_FreeValue(qctx, v);
                }
                return q::JS_EXCEPTION;
            }
            values.push(v);
        }
        let cx = super::current();
        let realm = if is_object(f) {
            realm_of_object(cx, val_obj(f))
        } else {
            ptr::null_mut()
        };
        if !realm.is_null() {
            cx.enter_realm(realm);
        }
        let result = q::JS_Call(
            qctx,
            f,
            arg(argv, 1),
            values.len() as c_int,
            values.as_mut_ptr(),
        );
        if !realm.is_null() {
            cx.leave_realm();
        }
        for v in values {
            q::JS_FreeValue(qctx, v);
        }
        completion_of(qctx, result)
    }
}

/// Whether the code of a script declares global bindings (var, let, const, class or function at
/// its top level), which its closure creation does before its first instruction.
unsafe fn declares_globals(code: q::JSValue) -> bool {
    let mut info: q::JSScriptInfo = std::mem::zeroed();
    if q::JS_GetScriptInfo(code, &mut info) < 0 {
        return false;
    }
    (0..info.closure_var_count).any(|i| {
        let mut var: q::JSScriptVariable = std::mem::zeroed();
        q::JS_GetScriptVariable(code, q::JS_VARIABLE_CLOSURE, i, &mut var) == 0
            && var.binding == q::JS_BINDING_GLOBAL_DECL
    })
}
