/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! SpiderMonkey's `Debugger` API, for Servo's devtools (`debugger.js`). It is emulated in
//! JavaScript (`debugger_api.js`), over the natives defined here, in each global that
//! `JS_DefineDebuggerObject` is called on. Design: `docs/design.md`, "Debugger".

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

use servo_quickjs as q;

use super::proxy::{proxy_class_name, ToWindowIfWindowProxy};
use super::realm::{
    hold_realm, realm_of_global, realm_of_object, realm_of_plain_object, release_realm,
};
use super::reentrant::Stack;
use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;

const SOURCE: &str = concat!(include_str!("debugger_api.js"), "\0");

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

/// The debugger state of a runtime.
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
    /// handle removes itself when it is finalized). Only borrowed around Rust code.
    handles: RefCell<HashMap<usize, q::JSValue>>,
}

impl State {
    pub(crate) fn is_active(&self) -> bool {
        self.active_hosts.get() > 0
    }
}

/// Opaque data of script handles: the emulation's reference to a function bytecode.
struct ScriptData {
    /// Counted and marked.
    code: q::JSValue,
    /// The `Debugger.Source` id of the code.
    source: u32,
}

static SCRIPT_CLASS: std::sync::OnceLock<q::JSClassID> = std::sync::OnceLock::new();

unsafe fn script_class(rt: *mut q::JSRuntime) -> q::JSClassID {
    let id = *SCRIPT_CLASS.get_or_init(|| {
        let mut id = 0;
        q::JS_NewClassID(&mut id);
        id
    });
    if q::JS_IsRegisteredClass(rt, id) == 0 {
        let def = q::JSClassDef {
            class_name: c"DebuggerScriptHandle".as_ptr(),
            finalizer: Some(script_finalize),
            gc_mark: Some(script_gc_mark),
            call: None,
            exotic: ptr::null_mut(),
        };
        assert_eq!(q::JS_NewClass(rt, id, &def), 0);
    }
    id
}

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
    let key = data.code.ptr() as usize;
    {
        let mut handles = super::current().debugger.handles.borrow_mut();
        if handles.get(&key).is_some_and(|h| h.ptr() == val.ptr()) {
            handles.remove(&key);
        }
    }
    q::JS_FreeValueRT(rt, data.code);
}

unsafe extern "C" fn script_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    if let Some(data) = script_data(val) {
        q::JS_MarkValue(rt, data.code, mark);
    }
}

/// The handle of the function bytecode `code` (a new reference), created with `source` if there
/// is none yet.
unsafe fn script_handle(
    cx: &Cx,
    qctx: *mut q::JSContext,
    code: q::JSValue,
    source: u32,
) -> q::JSValue {
    let key = code.ptr() as usize;
    let existing = cx.debugger.handles.borrow().get(&key).copied();
    if let Some(handle) = existing {
        return q::JS_DupValue(qctx, handle);
    }
    let handle = q::JS_NewObjectClass(qctx, script_class(cx.rt) as c_int);
    if handle.tag() == q::JS_TAG_EXCEPTION {
        return handle;
    }
    let data = Box::new(ScriptData {
        code: q::JS_DupValue(qctx, code),
        source,
    });
    q::JS_SetOpaque(handle, Box::into_raw(data) as *mut c_void);
    cx.debugger.handles.borrow_mut().insert(key, handle);
    handle
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
    true
}

/// Releases the emulations, before the runtime is destroyed.
pub(crate) unsafe fn free_debuggers(cx: &Cx) {
    while let Some((host, realm)) = cx.debugger.hosts.pop() {
        q::JS_FreeValueRT(cx.rt, host);
        release_realm(realm);
    }
}

/// Calls `method` of each emulation's host with `args` (borrowed) made by `make_args` in its
/// context. The calls can't fail: exceptions, and terminations, are dropped.
unsafe fn notify(cx: &Cx, method: &CStr, make_args: impl Fn(*mut q::JSContext) -> Vec<q::JSValue>) {
    for (host, realm) in cx.debugger.hosts.to_vec() {
        let qctx = (*realm).qctx;
        let mut args = make_args(qctx);
        let f = q::JS_GetPropertyStr(qctx, host, method.as_ptr());
        // Out of memory.
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
            // A termination of the hook terminates the script the hook interrupted, as in
            // SpiderMonkey: the interrupt it took is requested again.
            if q::JS_HasUncatchableException(qctx) != 0 {
                super::JS_RequestInterruptCallback(cx.as_raw());
            }
            q::JS_FreeValue(qctx, q::JS_GetException(qctx));
        }
        q::JS_FreeValue(qctx, r);
        q::JS_FreeValue(qctx, f);
        for arg in args {
            q::JS_FreeValue(qctx, arg);
        }
    }
}

/// Tells the debuggers about the code of a new script or module, before it runs, as
/// SpiderMonkey's `onNewScript`. `text` is its source (UTF-8) without the shim's line padding.
pub(crate) unsafe fn new_script(
    cx: &Cx,
    code: q::JSValue,
    url: &CStr,
    text: &[u8],
    introduction_type: *const c_char,
) {
    if !cx.debugger.is_active() || code.tag() != q::JS_TAG_FUNCTION_BYTECODE {
        return;
    }
    let id = cx.debugger.next_source_id.get() + 1;
    cx.debugger.next_source_id.set(id);
    notify(cx, c"newScript", |qctx| {
        let info = q::JS_NewObject(qctx);
        q::JS_SetPropertyStr(qctx, info, c"id".as_ptr(), q::JS_NewInt32(qctx, id as i32));
        q::JS_SetPropertyStr(
            qctx,
            info,
            c"url".as_ptr(),
            q::JS_NewString(qctx, url.as_ptr()),
        );
        q::JS_SetPropertyStr(
            qctx,
            info,
            c"text".as_ptr(),
            q::JS_NewStringLen(qctx, text.as_ptr() as *const c_char, text.len()),
        );
        let introduction_type = if introduction_type.is_null() {
            q::JS_UNDEFINED
        } else {
            q::JS_NewString(qctx, introduction_type)
        };
        q::JS_SetPropertyStr(qctx, info, c"introductionType".as_ptr(), introduction_type);
        vec![script_handle(cx, qctx, code, id), info]
    });
}

// Natives.

unsafe fn new_natives(qctx: *mut q::JSContext) -> q::JSValue {
    let natives = q::JS_NewObject(qctx);
    let functions: [(&CStr, q::JSCFunction, c_int); 11] = [
        (c"setActive", Some(n_set_active), 1),
        (c"className", Some(n_class_name), 1),
        (c"isProxy", Some(n_is_proxy), 1),
        (c"functionKind", Some(n_function_kind), 1),
        (c"toGlobal", Some(n_to_global), 1),
        (c"globalOf", Some(n_global_of), 1),
        (c"scriptGlobal", Some(n_script_global), 1),
        (c"scriptUrl", Some(n_script_url), 1),
        (c"scriptSource", Some(n_script_source), 1),
        (c"evalInGlobal", Some(n_eval_in_global), 3),
        (c"call", Some(n_call), 3),
    ];
    for (name, f, length) in functions {
        let f = q::JS_NewCFunction2(qctx, f, name.as_ptr(), length, q::JS_CFUNC_generic, 0);
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

/// `[kind, value]`, consuming `value`.
unsafe fn completion(qctx: *mut q::JSContext, kind: i32, value: q::JSValue) -> q::JSValue {
    let array = q::JS_NewArray(qctx);
    q::JS_SetPropertyUint32(qctx, array, 0, q::JS_NewInt32(qctx, kind));
    q::JS_SetPropertyUint32(qctx, array, 1, value);
    array
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

/// `setActive(active)`: whether a debugger of the emulation has a debuggee, so that the shim tells
/// it about new scripts. Called when that changes.
unsafe extern "C" fn n_set_active(
    _qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let active = &super::current().debugger.active_hosts;
    if arg(argv, 0).tag() == q::JS_TAG_BOOL && arg(argv, 0).int() != 0 {
        active.set(active.get() + 1);
    } else {
        active.set(active.get().saturating_sub(1));
    }
    q::JS_UNDEFINED
}

/// The class name of an object, as `Debugger.Object.prototype.class`: that of a proxy is the one
/// its handler reports (a WindowProxy's is its window's).
unsafe extern "C" fn n_class_name(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
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

/// -1 for a value that is not a function, 0 for a native function, 1 for an ordinary function, 2
/// for a generator, 3 for an async function, 4 for an async generator and 5 for a bound function.
unsafe extern "C" fn n_function_kind(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
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

/// Whether a value is a JS Proxy (not one of the shim's proxies, such as Servo's DOM proxies).
unsafe extern "C" fn n_is_proxy(
    _qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let v = arg(argv, 0);
    if is_object(v) && q::JS_GetClassID(v) == CLASS_PROXY {
        q::JS_TRUE
    } else {
        q::JS_FALSE
    }
}

/// The global a value is (that of a WindowProxy), or `undefined`.
unsafe extern "C" fn n_to_global(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let v = arg(argv, 0);
    if !is_object(v) {
        return q::JS_UNDEFINED;
    }
    let obj = ToWindowIfWindowProxy(val_obj(v));
    if obj.is_null() || realm_of_global(obj).is_null() {
        return q::JS_UNDEFINED;
    }
    q::JS_DupValue(qctx, obj_val(obj))
}

/// The global of the realm of an object, where known (see `realm_of_plain_object`), or
/// `undefined`.
unsafe extern "C" fn n_global_of(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let v = arg(argv, 0);
    if !is_object(v) {
        return q::JS_UNDEFINED;
    }
    let obj = val_obj(v);
    let mut realm = realm_of_global(obj);
    if realm.is_null() {
        realm = realm_of_plain_object(super::current(), obj);
    }
    if realm.is_null() || (*realm).global.is_null() {
        return q::JS_UNDEFINED;
    }
    q::JS_DupValue(qctx, obj_val((*realm).global))
}

unsafe fn handle_data<'a>(
    qctx: *mut q::JSContext,
    v: q::JSValue,
) -> Result<&'a ScriptData, q::JSValue> {
    script_data(v).ok_or_else(|| q::JS_ThrowTypeError(qctx, c"not a script handle".as_ptr()))
}

/// The global of the realm a script was compiled in, or `undefined`.
unsafe extern "C" fn n_script_global(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let data = match handle_data(qctx, arg(argv, 0)) {
        Ok(data) => data,
        Err(e) => return e,
    };
    let realm = q::JS_GetScriptRealm(data.code);
    if realm.is_null() {
        return q::JS_UNDEFINED;
    }
    let realm = super::realm::realm_of_qctx(realm);
    if realm.is_null() || (*realm).global.is_null() {
        return q::JS_UNDEFINED;
    }
    q::JS_DupValue(qctx, obj_val((*realm).global))
}

/// The file name a script was compiled with.
unsafe extern "C" fn n_script_url(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
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

/// The source id of a script.
unsafe extern "C" fn n_script_source(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    match handle_data(qctx, arg(argv, 0)) {
        Ok(data) => q::JS_NewInt32(qctx, data.source as i32),
        Err(e) => e,
    }
}

/// `evalInGlobal(global, code, url)`: compiles `code` as a classic script of `global`, tells the
/// debuggers about it (introduction type "debugger eval"), and runs it. Returns its completion.
unsafe extern "C" fn n_eval_in_global(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let cx = super::current();
    let global = arg(argv, 0);
    let realm = if is_object(global) {
        realm_of_global(ToWindowIfWindowProxy(val_obj(global)))
    } else {
        ptr::null_mut()
    };
    if realm.is_null() {
        return q::JS_ThrowTypeError(qctx, c"evalInGlobal: not a global".as_ptr());
    }
    let mut len = 0;
    let code = q::JS_ToCStringLen(qctx, &mut len, arg(argv, 1));
    if code.is_null() {
        return q::JS_EXCEPTION;
    }
    let url = q::JS_ToCStringLen(qctx, ptr::null_mut(), arg(argv, 2));
    if url.is_null() {
        q::JS_FreeCString(qctx, code);
        return q::JS_EXCEPTION;
    }
    let mut src = Vec::with_capacity(len + 1);
    src.extend_from_slice(std::slice::from_raw_parts(code as *const u8, len));
    src.push(0);
    q::JS_FreeCString(qctx, code);

    let target = (*realm).qctx;
    cx.enter_realm(realm);
    let flags = (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as c_int;
    let compiled = q::JS_Eval(target, src.as_ptr() as *const c_char, len, url, flags);
    let result = if compiled.tag() == q::JS_TAG_EXCEPTION {
        compiled
    } else {
        new_script(
            cx,
            compiled,
            CStr::from_ptr(url),
            &src[..len],
            c"debugger eval".as_ptr(),
        );
        q::JS_EvalFunction(target, compiled)
    };
    cx.leave_realm();
    q::JS_FreeCString(qctx, url);
    completion_of(qctx, result)
}

/// `call(f, thisArg, args)`: calls `f` in its realm, and returns the completion.
unsafe extern "C" fn n_call(
    qctx: *mut q::JSContext,
    _this: q::JSValue,
    _argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let f = arg(argv, 0);
    let args = arg(argv, 2);
    let mut len = 0i64;
    let length = q::JS_GetPropertyStr(qctx, args, c"length".as_ptr());
    let r = q::JS_ToInt64(qctx, &mut len, length);
    q::JS_FreeValue(qctx, length);
    if r < 0 {
        return q::JS_EXCEPTION;
    }
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
