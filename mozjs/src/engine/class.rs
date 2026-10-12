/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! SpiderMonkey `JSClass`es as QuickJS classes, native functions, write barriers and tracing.
//!
//! Every object of an SM class carries an `ObjectData` (QuickJS opaque) with its reserved slots.
//! Slot values hold a reference count and are marked by `gc_mark`, like `Heap<T>` fields, whose
//! write barriers maintain their counts. Native functions are objects of one QuickJS class whose
//! call hook builds SpiderMonkey's `vp` array and calls the `JSNative`.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use servo_quickjs as q;

use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;
use crate::sys::jsval::{JSVal, TAG_MAGIC};

/// Per-object state of objects whose class is a SpiderMonkey `JSClass`.
pub(crate) struct ObjectData {
    pub clasp: *const root::JSClass,
    /// Held (`hold_realm`) and marked.
    pub realm: *mut RealmData,
    /// Reserved slots; reference-counted values.
    pub slots: Box<[Cell<q::JSValue>]>,
}

impl ObjectData {
    pub fn new(clasp: *const root::JSClass, realm: *mut RealmData) -> Box<ObjectData> {
        let count = unsafe { reserved_slot_count(clasp) };
        Box::new(ObjectData {
            clasp,
            realm: unsafe { super::hold_realm(realm) },
            slots: vec![Cell::new(q::JS_UNDEFINED); count].into_boxed_slice(),
        })
    }

    /// The slots the embedder may use: all but a global's key object slot.
    pub fn embedder_slot_count(&self) -> usize {
        let is_global = unsafe { (*self.clasp).flags } & root::JSCLASS_IS_GLOBAL != 0;
        self.slots.len() - is_global as usize
    }

    pub unsafe fn free(self: Box<Self>, rt: *mut q::JSRuntime) {
        for v in self.slots.iter() {
            q::JS_FreeValueRT(rt, v.get());
        }
        super::release_realm(self.realm);
    }
}

unsafe fn reserved_slot_count(clasp: *const root::JSClass) -> usize {
    let flags = (*clasp).flags;
    let mut count = ((flags >> root::JSCLASS_RESERVED_SLOTS_SHIFT)
        & root::JSCLASS_RESERVED_SLOTS_MASK) as usize;
    if flags & root::JSCLASS_IS_GLOBAL != 0 {
        // The last one holds the realm's key object (`intrinsics.rs`).
        count += root::JSCLASS_GLOBAL_SLOT_COUNT as usize + 1;
    }
    count
}

/// SpiderMonkey `JSClass` addresses <-> QuickJS class ids (QuickJS ids are process-global).
#[derive(Default)]
struct Registry {
    by_clasp: HashMap<usize, q::JSClassID>,
    by_id: HashMap<q::JSClassID, usize>,
}

/// The QuickJS class of native functions, readable without the lock: every DOM call looks its
/// jit info up.
static FUNCTION_CLASS_ID: AtomicU32 = AtomicU32::new(0);

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

/// `Registry::by_id` for small ids, readable without the lock: `object_data` is on every DOM call.
static CLASPS_BY_ID: [AtomicUsize; 8192] = [const { AtomicUsize::new(0) }; 8192];

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = REGISTRY.lock().unwrap();
    f(guard.get_or_insert_with(Default::default))
}

/// A new QuickJS class id (QuickJS's allocator is not thread-safe).
pub(crate) fn new_class_id() -> q::JSClassID {
    with_registry(|_| {
        let mut id = 0;
        unsafe { q::JS_NewClassID(&mut id) };
        id
    })
}

/// The QuickJS class for `clasp`, registered in `rt` on first use.
pub(crate) unsafe fn class_id(rt: *mut q::JSRuntime, clasp: *const root::JSClass) -> q::JSClassID {
    let id = with_registry(|r| {
        *r.by_clasp.entry(clasp as usize).or_insert_with(|| {
            let mut id = 0;
            q::JS_NewClassID(&mut id);
            r.by_id.insert(id, clasp as usize);
            if let Some(slot) = CLASPS_BY_ID.get(id as usize) {
                slot.store(clasp as usize, Ordering::Release);
            }
            id
        })
    });
    if q::JS_IsRegisteredClass(rt, id) == 0 {
        let ops = (*clasp).cOps;
        let callable = !ops.is_null() && ((*ops).call.is_some() || (*ops).construct.is_some());
        let def = q::JSClassDef {
            class_name: (*clasp).name,
            finalizer: Some(finalize),
            gc_mark: Some(gc_mark),
            call: if callable { Some(class_call) } else { None },
            // QuickJS: resolve and enumerate hooks (globals) run from exotic methods.
            exotic: super::proxy::resolve_exotic_methods(clasp),
        };
        assert_eq!(q::JS_NewClass(rt, id, &def), 0);
    }
    id
}

#[inline]
fn clasp_of_id(id: q::JSClassID) -> *const root::JSClass {
    match CLASPS_BY_ID.get(id as usize) {
        Some(slot) => slot.load(Ordering::Acquire) as *const root::JSClass,
        None => with_registry(|r| r.by_id.get(&id).copied().unwrap_or(0) as *const root::JSClass),
    }
}

/// The object data of `obj`, if it is an object of a SpiderMonkey class (globals included).
#[inline]
pub(crate) unsafe fn object_data<'a>(obj: *mut root::JSObject) -> Option<&'a ObjectData> {
    let v = obj_val(obj);
    let id = q::JS_GetClassID(v);
    if clasp_of_id(id).is_null() {
        return None;
    }
    let data = q::JS_GetOpaque(v, id) as *const ObjectData;
    data.as_ref()
}

/// Placeholder class reported for objects QuickJS created itself (plain objects, arrays, ...).
static PLAIN_CLASS: root::JSClass = root::JSClass {
    name: c"Object".as_ptr(),
    flags: 0,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static FUNCTION_CLASS: root::JSClass = root::JSClass {
    name: c"Function".as_ptr(),
    flags: 0,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

/// `js::GetObjectClass` / mozjs `get_object_class`.
pub(crate) unsafe fn object_class(obj: *mut root::JSObject) -> *const root::JSClass {
    if let Some(data) = object_data(obj) {
        return data.clasp;
    }
    // QuickJS: proxies report the class they were created with.
    if let Some(clasp) = super::proxy::proxy_class(obj) {
        return clasp;
    }
    if q::JS_IsFunction(super::current().base_ctx, obj_val(obj)) != 0 {
        return &FUNCTION_CLASS;
    }
    &PLAIN_CLASS
}

/// The realm an object was created in, where known.
pub(crate) unsafe fn object_realm(cx: &Cx, obj: *mut root::JSObject) -> *mut RealmData {
    let realm = known_object_realm(obj);
    if !realm.is_null() {
        return realm;
    }
    super::realm::realm_of_object(cx, obj)
}

/// The realm recorded by shim objects, functions, proxies and globals, or null.
pub(crate) unsafe fn known_object_realm(obj: *mut root::JSObject) -> *mut RealmData {
    if let Some(data) = object_data(obj) {
        if !data.realm.is_null() {
            return data.realm;
        }
    }
    if let Some(data) = function_data(obj) {
        return data.realm;
    }
    // QuickJS: proxies record their realm.
    if let Some(realm) = super::proxy::proxy_realm(obj) {
        return realm;
    }
    super::realm::realm_of_global(obj)
}

unsafe extern "C" fn finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let id = q::JS_GetClassID(val);
    let data = q::JS_GetOpaque(val, id) as *mut ObjectData;
    if data.is_null() {
        return;
    }
    let ops = (*(*data).clasp).cOps;
    if !ops.is_null() {
        if let Some(finalize) = (*ops).finalize {
            finalize(ptr::null_mut(), val_obj(val));
        }
    }
    Box::from_raw(data).free(rt);
}

/// What a SpiderMonkey `JSTracer*` points to during QuickJS's `gc_mark`.
#[repr(C)]
pub(crate) struct Tracer {
    pub rt: *mut q::JSRuntime,
    pub mark: q::JS_MarkFunc,
}

impl Tracer {
    unsafe fn mark(trc: *mut root::JSTracer, v: q::JSValue) {
        let trc = &*(trc as *mut Tracer);
        if v.has_ref_count() {
            super::verify::mark(v, trc.mark);
            q::JS_MarkValue(trc.rt, v, trc.mark);
        }
    }
}

unsafe extern "C" fn gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    let id = q::JS_GetClassID(val);
    let data = q::JS_GetOpaque(val, id) as *mut ObjectData;
    if data.is_null() {
        return;
    }
    for v in (*data).slots.iter() {
        if v.get().has_ref_count() {
            q::JS_MarkValue(rt, v.get(), mark);
        }
    }
    super::mark_realm(rt, (*data).realm, mark);
    let ops = (*(*data).clasp).cOps;
    if !ops.is_null() {
        if let Some(trace) = (*ops).trace {
            let mut tracer = Tracer { rt, mark };
            trace(
                &mut tracer as *mut Tracer as *mut root::JSTracer,
                val_obj(val),
            );
        }
    }
}

unsafe extern "C" fn class_call(
    ctx: *mut q::JSContext,
    func_obj: q::JSValue,
    this_val: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    flags: c_int,
) -> q::JSValue {
    let data = object_data(val_obj(func_obj)).expect("callable SM class without data");
    let ops = (*data.clasp).cOps;
    let constructing = flags & q::JS_CALL_FLAG_CONSTRUCTOR as c_int != 0;
    let native = if constructing {
        (*ops).construct
    } else {
        (*ops).call
    };
    let realm = data.realm;
    call_native(
        ctx,
        native,
        realm,
        func_obj,
        this_val,
        argc,
        argv,
        constructing,
    )
}

/// Calls a SpiderMonkey `JSNative` with QuickJS arguments; returns a new reference or
/// `JS_EXCEPTION`.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn call_native(
    ctx: *mut q::JSContext,
    native: root::JSNative,
    realm: *mut RealmData,
    func_obj: q::JSValue,
    this_val: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    constructing: bool,
) -> q::JSValue {
    let Some(native) = native else {
        return q::JS_ThrowTypeError(ctx, c"not a function".as_ptr());
    };
    let cx = &*(q::JS_GetRuntimeOpaque(q::JS_GetRuntime(ctx)) as *const Cx);
    // Values handed to Rust during the call are released when it returns, as SpiderMonkey's
    // rooted values die with the native's frame; otherwise a loop calling natives would grow the
    // pool until the end of the task.
    let pool_mark = cx.pool_mark();
    // callee, this, the arguments and new.target; on the stack for most calls.
    let len = argc as usize + 2 + constructing as usize;
    let mut on_stack = [JSVal::default(); 8];
    let mut on_heap = Vec::new();
    let vp: &mut [JSVal] = if len <= on_stack.len() {
        &mut on_stack[..len]
    } else {
        on_heap.resize(len, JSVal::default());
        &mut on_heap
    };
    vp[0] = JSVal::from_qjs(func_obj);
    vp[1] = if constructing {
        JSVal {
            asBits_: root::JSWhyMagic::JS_IS_CONSTRUCTING as u64,
            tag: TAG_MAGIC,
        }
    } else {
        JSVal::from_qjs(this_val)
    };
    for i in 0..argc as usize {
        let arg = *argv.add(i);
        // Ropes are flattened (and the flat copy retained) so Rust never sees them.
        vp[2 + i] = if arg.tag() == q::JS_TAG_STRING_ROPE {
            cx.retain(JSVal::from_qjs(arg))
        } else {
            JSVal::from_qjs(arg)
        };
    }
    if constructing {
        // new.target
        vp[2 + argc as usize] = JSVal::from_qjs(this_val);
    }
    let entered = !realm.is_null();
    if entered {
        cx.enter_realm(realm);
    }
    let ok = native(cx.as_raw(), argc as u32, vp.as_mut_ptr());
    if entered {
        cx.leave_realm();
    }
    let result = if ok {
        q::JS_DupValueRT(cx.rt, vp[0].to_qjs())
    } else {
        if q::JS_HasException(ctx) == 0 {
            // SpiderMonkey's uncatchable termination: false without a pending exception.
            q::JS_ThrowInternalError(ctx, c"script terminated".as_ptr());
            q::JS_SetUncatchableException(ctx, 1);
        }
        q::JS_EXCEPTION
    };
    cx.drain_pool(pool_mark);
    result
}

/// Native functions: `JSNative` plus jit info, realm and SpiderMonkey's two extended slots.
pub(crate) struct FunctionData {
    pub native: root::JSNative,
    pub info: *const root::JSJitInfo,
    /// Held (`hold_realm`) and marked.
    pub realm: *mut RealmData,
    pub slots: [Cell<q::JSValue>; 2],
}

unsafe fn function_class(rt: *mut q::JSRuntime) -> q::JSClassID {
    let mut id = FUNCTION_CLASS_ID.load(Ordering::Acquire);
    if id == 0 {
        id = with_registry(|_| {
            let mut id = FUNCTION_CLASS_ID.load(Ordering::Acquire);
            if id == 0 {
                q::JS_NewClassID(&mut id);
                FUNCTION_CLASS_ID.store(id, Ordering::Release);
            }
            id
        });
    }
    if q::JS_IsRegisteredClass(rt, id) == 0 {
        let def = q::JSClassDef {
            class_name: c"Function".as_ptr(),
            finalizer: Some(function_finalize),
            gc_mark: Some(function_gc_mark),
            call: Some(function_call),
            exotic: ptr::null_mut(),
        };
        assert_eq!(q::JS_NewClass(rt, id, &def), 0);
    }
    id
}

pub(crate) unsafe fn function_data<'a>(obj: *mut root::JSObject) -> Option<&'a FunctionData> {
    let v = obj_val(obj);
    let id = q::JS_GetClassID(v);
    let fid = FUNCTION_CLASS_ID.load(Ordering::Relaxed);
    if fid == 0 || id != fid {
        return None;
    }
    (q::JS_GetOpaque(v, id) as *const FunctionData).as_ref()
}

unsafe extern "C" fn function_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = q::JS_GetOpaque(val, q::JS_GetClassID(val)) as *mut FunctionData;
    if !data.is_null() {
        let data = Box::from_raw(data);
        for v in data.slots.iter() {
            q::JS_FreeValueRT(rt, v.get());
        }
        super::release_realm(data.realm);
    }
}

unsafe extern "C" fn function_gc_mark(
    rt: *mut q::JSRuntime,
    val: q::JSValue,
    mark: q::JS_MarkFunc,
) {
    let data = q::JS_GetOpaque(val, q::JS_GetClassID(val)) as *mut FunctionData;
    if !data.is_null() {
        for v in (*data).slots.iter() {
            if v.get().has_ref_count() {
                q::JS_MarkValue(rt, v.get(), mark);
            }
        }
        super::mark_realm(rt, (*data).realm, mark);
    }
}

unsafe extern "C" fn function_call(
    ctx: *mut q::JSContext,
    func_obj: q::JSValue,
    this_val: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    flags: c_int,
) -> q::JSValue {
    let data = &*(q::JS_GetOpaque(func_obj, q::JS_GetClassID(func_obj)) as *mut FunctionData);
    let constructing = flags & q::JS_CALL_FLAG_CONSTRUCTOR as c_int != 0;
    call_native(
        ctx,
        data.native,
        data.realm,
        func_obj,
        this_val,
        argc,
        argv,
        constructing,
    )
}

/// A new native function object (a new reference).
pub(crate) unsafe fn new_function(
    cx: &Cx,
    native: root::JSNative,
    info: *const root::JSJitInfo,
    nargs: u32,
    constructor: bool,
    name: q::JSAtom,
) -> q::JSValue {
    let qctx = cx.qctx();
    let proto = q::JS_GetClassProto(qctx, QJS_CLASS_BYTECODE_FUNCTION);
    let f = q::JS_NewObjectProtoClass(qctx, proto, function_class(cx.rt));
    q::JS_FreeValue(qctx, proto);
    if f.tag() == q::JS_TAG_EXCEPTION {
        return f;
    }
    let data = Box::new(FunctionData {
        native,
        info,
        realm: super::hold_realm(cx.current_realm()),
        slots: [const { Cell::new(q::JS_UNDEFINED) }; 2],
    });
    q::JS_SetOpaque(f, Box::into_raw(data) as *mut c_void);
    let flags = q::JS_PROP_CONFIGURABLE as c_int;
    q::JS_DefinePropertyValueStr(
        qctx,
        f,
        c"length".as_ptr(),
        q::JS_NewInt32(qctx, nargs as i32),
        flags,
    );
    let name_val = if name == 0 {
        q::JS_NewString(qctx, c"".as_ptr())
    } else {
        q::JS_AtomToString(qctx, name)
    };
    q::JS_DefinePropertyValueStr(qctx, f, c"name".as_ptr(), name_val, flags);
    if constructor {
        q::JS_SetConstructorBit(qctx, f, 1);
    }
    f
}

/// QuickJS's internal class id of bytecode functions, whose prototype is `Function.prototype`.
pub(crate) const QJS_CLASS_BYTECODE_FUNCTION: q::JSClassID = 13;
/// QuickJS's internal class id of plain objects, whose prototype is `Object.prototype`.
pub(crate) const QJS_CLASS_OBJECT: q::JSClassID = 1;

/// Creates an object of SpiderMonkey class `clasp` with prototype `proto` (a new reference).
pub(crate) unsafe fn new_object(
    cx: &Cx,
    clasp: *const root::JSClass,
    proto: q::JSValue,
) -> q::JSValue {
    let qctx = cx.qctx();
    if clasp.is_null() || ptr::eq(clasp, &PLAIN_CLASS) {
        return q::JS_NewObjectProto(qctx, proto);
    }
    let id = class_id(cx.rt, clasp);
    let obj = q::JS_NewObjectProtoClass(qctx, proto, id);
    if obj.tag() != q::JS_TAG_EXCEPTION {
        let data = ObjectData::new(clasp, cx.current_realm());
        q::JS_SetOpaque(obj, Box::into_raw(data) as *mut c_void);
        let ops = (*clasp).cOps;
        if !ops.is_null() && (*ops).construct.is_some() {
            q::JS_SetConstructorBit(qctx, obj, 1);
        }
    }
    obj
}

pub(crate) unsafe fn JS_NewObject(
    cx: *mut root::JSContext,
    clasp: *const root::JSClass,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let proto = q::JS_GetClassProto(cx.qctx(), QJS_CLASS_OBJECT);
    let obj = new_object(cx, clasp, proto);
    q::JS_FreeValueRT(cx.rt, proto);
    adopt_object(cx, obj)
}

pub(crate) unsafe fn JS_NewObjectWithGivenProto(
    cx: *mut root::JSContext,
    clasp: *const root::JSClass,
    proto: root::JS::Handle<*mut root::JSObject>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let proto = if (*proto.ptr).is_null() {
        q::JS_NULL
    } else {
        obj_val(*proto.ptr)
    };
    let obj = new_object(cx, clasp, proto);
    adopt_object(cx, obj)
}

pub(crate) unsafe fn JS_NewPlainObject(cx: *mut root::JSContext) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let obj = q::JS_NewObject(cx.qctx());
    adopt_object(cx, obj)
}

/// Retains a new object reference in the pool and returns the pointer (null on exception).
pub(crate) unsafe fn adopt_object(cx: &Cx, obj: q::JSValue) -> *mut root::JSObject {
    if obj.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    cx.adopt(obj).to_object()
}

pub(crate) unsafe fn JS_GetReservedSlot(
    obj: *mut root::JSObject,
    index: u32,
    dest: *mut root::JS::Value,
) {
    let data = object_data(obj).expect("JS_GetReservedSlot on an object without slots");
    assert!(
        (index as usize) < data.embedder_slot_count(),
        "reserved slot {index} out of range"
    );
    *dest = JSVal::from_qjs(data.slots[index as usize].get());
}

pub(crate) unsafe fn JS_SetReservedSlot(
    obj: *mut root::JSObject,
    index: u32,
    v: *const root::JS::Value,
) {
    let cx = super::current();
    let data = object_data(obj).expect("JS_SetReservedSlot on an object without slots");
    assert!(
        (index as usize) < data.embedder_slot_count(),
        "reserved slot {index} out of range"
    );
    let new = q::JS_DupValueRT(cx.rt, (*v).to_qjs());
    let old = data.slots[index as usize].replace(new);
    q::JS_FreeValueRT(cx.rt, old);
}

pub(crate) unsafe fn JS_NewFunction(
    cx: *mut root::JSContext,
    call: root::JSNative,
    nargs: ::std::os::raw::c_uint,
    flags: ::std::os::raw::c_uint,
    name: *const ::std::os::raw::c_char,
) -> *mut root::JSFunction {
    let cx = Cx::from_raw(cx);
    let atom = if name.is_null() {
        0
    } else {
        q::JS_NewAtom(cx.qctx(), name)
    };
    let f = new_function(
        cx,
        call,
        ptr::null(),
        nargs,
        flags & root::JSFUN_CONSTRUCTOR != 0,
        atom,
    );
    if atom != 0 {
        q::JS_FreeAtomRT(cx.rt, atom);
    }
    adopt_object(cx, f) as *mut root::JSFunction
}

pub(crate) unsafe fn JS_GetFunctionObject(fun: *mut root::JSFunction) -> *mut root::JSObject {
    fun as *mut root::JSObject
}

pub(crate) unsafe fn CurrentGlobal(cx: *mut root::JSContext) -> *const *mut root::JSObject {
    let realm = Cx::from_raw(cx).current_realm();
    assert!(!realm.is_null(), "CurrentGlobal outside a realm");
    &(*realm).global
}

pub(crate) unsafe fn InitRealmStandardClasses(cx: *mut root::JSContext) -> bool {
    // QuickJS contexts are created with all standard classes.
    let _ = cx;
    true
}

// Write barriers: `Heap<T>` holds one reference to its value.

unsafe fn barrier(prev: q::JSValue, next: q::JSValue) {
    let Some(cx) = super::try_current() else {
        return;
    };
    if next.has_ref_count() {
        q::JS_DupValueRT(cx.rt, next);
        super::verify::count(next, 1);
    }
    if prev.has_ref_count() {
        super::verify::count(prev, -1);
        q::JS_FreeValueRT(cx.rt, prev);
    }
}

unsafe fn ptr_val(tag: i32, p: *mut c_void) -> q::JSValue {
    if p.is_null() {
        q::JS_UNDEFINED
    } else {
        q::JSValue::mkptr(tag, p)
    }
}

pub(crate) unsafe fn HeapObjectWriteBarriers(
    objp: *mut *mut root::JSObject,
    prev: *mut root::JSObject,
    next: *mut root::JSObject,
) {
    let _ = objp;
    barrier(
        ptr_val(q::JS_TAG_OBJECT, prev as _),
        ptr_val(q::JS_TAG_OBJECT, next as _),
    );
}

pub(crate) unsafe fn HeapValueWriteBarriers(
    valuep: *mut root::JS::Value,
    prev: *const root::JS::Value,
    next: *const root::JS::Value,
) {
    let _ = valuep;
    barrier((*prev).to_qjs(), (*next).to_qjs());
}

pub(crate) unsafe fn HeapStringWriteBarriers(
    objp: *mut *mut root::JSString,
    prev: *mut root::JSString,
    next: *mut root::JSString,
) {
    let _ = objp;
    barrier(
        ptr_val(q::JS_TAG_STRING, prev as _),
        ptr_val(q::JS_TAG_STRING, next as _),
    );
}

pub(crate) unsafe fn HeapScriptWriteBarriers(
    objp: *mut *mut root::JSScript,
    prev: *mut root::JSScript,
    next: *mut root::JSScript,
) {
    let _ = objp;
    barrier(
        ptr_val(q::JS_TAG_FUNCTION_BYTECODE, prev as _),
        ptr_val(q::JS_TAG_FUNCTION_BYTECODE, next as _),
    );
}

pub(crate) unsafe fn HeapBigIntWriteBarriers(
    bip: *mut *mut root::JS::BigInt,
    prev: *mut root::JS::BigInt,
    next: *mut root::JS::BigInt,
) {
    let _ = bip;
    barrier(
        ptr_val(q::JS_TAG_BIG_INT, prev as _),
        ptr_val(q::JS_TAG_BIG_INT, next as _),
    );
}

/// Barrier for `Heap<*mut JS::Symbol>` (SpiderMonkey needs none; QuickJS symbols are counted).
pub(crate) unsafe fn heap_symbol_barrier(prev: *mut root::JS::Symbol, next: *mut root::JS::Symbol) {
    barrier(
        ptr_val(q::JS_TAG_SYMBOL, prev as _),
        ptr_val(q::JS_TAG_SYMBOL, next as _),
    );
}

/// Barrier for `Heap<jsid>`: string and symbol ids hold an atom reference.
pub(crate) unsafe fn heap_id_barrier(prev: root::jsid, next: root::jsid) {
    let Some(cx) = super::try_current() else {
        return;
    };
    if next.is_string() || next.is_symbol() {
        q::JS_DupAtom(cx.base_ctx, next.to_atom());
    }
    if prev.is_string() || prev.is_symbol() {
        q::JS_FreeAtomRT(cx.rt, prev.to_atom());
    }
}

// Tracing: only `Heap<T>` fields (which hold a count) are marked. Root tracers are for
// uncounted locations and must not be marked.

pub(crate) unsafe fn CallValueTracer(
    trc: *mut root::JSTracer,
    valuep: *mut root::JS::Heap<root::JS::Value>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = name;
    Tracer::mark(trc, (*(*valuep).get_unsafe()).to_qjs());
}

pub(crate) unsafe fn CallObjectTracer(
    trc: *mut root::JSTracer,
    objp: *mut root::JS::Heap<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = name;
    Tracer::mark(trc, ptr_val(q::JS_TAG_OBJECT, *(*objp).get_unsafe() as _));
}

pub(crate) unsafe fn CallFunctionTracer(
    trc: *mut root::JSTracer,
    funp: *mut root::JS::Heap<*mut root::JSFunction>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = name;
    Tracer::mark(trc, ptr_val(q::JS_TAG_OBJECT, *(*funp).get_unsafe() as _));
}

pub(crate) unsafe fn CallStringTracer(
    trc: *mut root::JSTracer,
    strp: *mut root::JS::Heap<*mut root::JSString>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = name;
    Tracer::mark(trc, ptr_val(q::JS_TAG_STRING, *(*strp).get_unsafe() as _));
}

pub(crate) unsafe fn CallScriptTracer(
    trc: *mut root::JSTracer,
    scriptp: *mut root::JS::Heap<*mut root::JSScript>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = name;
    Tracer::mark(
        trc,
        ptr_val(q::JS_TAG_FUNCTION_BYTECODE, *(*scriptp).get_unsafe() as _),
    );
}

pub(crate) unsafe fn CallIdTracer(
    trc: *mut root::JSTracer,
    idp: *mut root::JS::Heap<root::jsid>,
    name: *const ::std::os::raw::c_char,
) {
    // Atoms are not cycle-collected; the count held by the Heap<jsid> is enough.
    let _ = (trc, idp, name);
}

pub(crate) unsafe fn CallObjectRootTracer(
    trc: *mut root::JSTracer,
    objp: *mut *mut root::JSObject,
    name: *const ::std::os::raw::c_char,
) {
    let _ = (trc, objp, name);
}

pub(crate) unsafe fn CallValueRootTracer(
    trc: *mut root::JSTracer,
    valp: *mut root::JS::Value,
    name: *const ::std::os::raw::c_char,
) {
    let _ = (trc, valp, name);
}

pub(crate) unsafe fn TraceValueArray(
    trc: *mut root::JSTracer,
    length: usize,
    elements: *mut root::JS::Value,
) {
    let _ = (trc, length, elements);
}
