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

/// QuickJS: the first field of Servo's `Reflector` (`dom_js_class: Cell<*const DOMJSClass>`),
/// which is the `Cell` holding the interface's `DOMJSClass`. `DOMJSClass::base` is a `JSClass`
/// and comes first, so the pointer also names the `JSClass`.
///
/// QuickJS calls `servo_dom_stub_class_id` with a stub's `dom_ptr` (the address of the node's
/// `Reflector`, and thus of this field), so QuickJS itself never has to know the layout of
/// `Reflector`.
#[repr(C)]
struct ReflectorHeader {
    dom_js_class: Cell<*const root::JSClass>,
}

/// The `JSClass` of the DOM node a `JSDomStub` owns, or null once the stub has been unbound.
///
/// A stub carries no class id: the interface's class is the `DOMJSClass` recorded first on the
/// Rust `Reflector` that the node pointer points to (`components/script_bindings/reflector.rs`).
/// Its `base` field is the `JSClass` the interface was wrapped with.
#[inline]
unsafe fn stub_class_from_ptr(dom_ptr: *mut c_void) -> *const root::JSClass {
    if dom_ptr.is_null() {
        return ptr::null();
    }
    (*(dom_ptr as *const ReflectorHeader)).dom_js_class.get()
}

/// The QuickJS class id of the interface whose DOM node is `dom_ptr` (a `Reflector`), or
/// `JS_INVALID_CLASS_ID` if it is unknown.
///
/// QuickJS calls this from its own stub handling (`JS_GetClassID`, `free_dom_stub` and the GC
/// mark branch), where the class id it registered the interface's class with is not recorded on
/// the stub. `rt` is NULL from `JS_GetClassID`, which has no runtime at hand; the current
/// context's runtime is then used. `class_id` registers the interface's class in `rt` on first
/// use, which `JS_NewDomStub` has always done by the time a stub is finalized or traced.
#[no_mangle]
pub unsafe extern "C" fn servo_dom_stub_class_id(
    rt: *mut q::JSRuntime,
    dom_ptr: *mut c_void,
) -> q::JSClassID {
    let clasp = stub_class_from_ptr(dom_ptr);
    if clasp.is_null() {
        return q::JS_INVALID_CLASS_ID;
    }
    let rt = if rt.is_null() {
        match super::try_current() {
            Some(cx) => cx.rt,
            None => return q::JS_INVALID_CLASS_ID,
        }
    } else {
        rt
    };
    class_id(rt, clasp)
}

/// The `JSClass` of the DOM node a `JSDomStub` owns.
#[inline]
unsafe fn stub_class(val: q::JSValue) -> *const root::JSClass {
    stub_class_from_ptr(q::JS_GetDomStubDomPtr(val))
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
    // QuickJS: a `JSDomStub`'s interface class lives on the Rust `Reflector` the stub points to.
    // This must come before `object_data`/`JS_IsFunction`, which would read the stub's fields as
    // a `JSObject`'s.
    if is_dom_stub(obj_val(obj)) {
        let clasp = stub_class(obj_val(obj));
        return if clasp.is_null() { &PLAIN_CLASS } else { clasp };
    }
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

/// The realm recorded by stub objects, shim objects, functions, proxies and globals, or null.
pub(crate) unsafe fn known_object_realm(obj: *mut root::JSObject) -> *mut RealmData {
    // QuickJS: a `JSDomStub` records its realm directly (and has no `ObjectData`).
    if is_dom_stub(obj_val(obj)) {
        return q::JS_GetDomStubRealm(obj_val(obj)) as *mut RealmData;
    }
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
    // QuickJS: a `JSDomStub` has no `ObjectData`; it owns the DOM node and one realm reference.
    // `free_dom_stub` only calls a class finalizer while `dom_ptr` is set.
    if is_dom_stub(val) {
        let realm = q::JS_GetDomStubRealm(val) as *mut RealmData;
        if !q::JS_GetDomStubDomPtr(val).is_null() {
            // The interface class, from the `Reflector` the stub owns.
            let clasp = stub_class(val);
            if !clasp.is_null() {
                let ops = (*clasp).cOps;
                if !ops.is_null() {
                    if let Some(finalize) = (*ops).finalize {
                        finalize(ptr::null_mut(), val_obj(val));
                    }
                }
            }
        }
        super::release_realm(realm);
        return;
    }
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
            super::verify::mark(trc.rt, v, trc.mark);
            q::JS_MarkValue(trc.rt, v, trc.mark);
        }
    }
}

/// What one collection found of a shared `Rc`.
#[derive(Default)]
struct SharedRc {
    /// Owners traced in the decrement pass.
    owners_traced: usize,
    /// The contents were marked in the decrement pass.
    decremented: bool,
    /// The contents were marked in the scan pass.
    restored: bool,
    /// With the tracing verifier: the strong count and the handles of the decrement pass's visits.
    owners: usize,
    handles: Vec<usize>,
}

/// The shared `Rc`s that one collection met, by address.
struct SharedRcs {
    rt: *mut q::JSRuntime,
    gc_number: u32,
    rcs: HashMap<usize, SharedRc>,
}

thread_local! {
    static SHARED_RCS: std::cell::RefCell<SharedRcs> = std::cell::RefCell::new(SharedRcs {
        rt: ptr::null_mut(),
        gc_number: 0,
        rcs: HashMap::new(),
    });
}

/// Whether a trace hook that reached the `Rc` at `rc` through the handle at `handle` traces its
/// contents now; `owners` is its strong count.
///
/// The cycle collector subtracts every mark of its decrement pass from the marked object's
/// reference count and adds every mark of its scan pass back, and the contents hold one count per
/// reference however many owners share them. So they are marked in the decrement pass only when
/// the last owner is traced, which proves that no owner is outside the objects the collector sees,
/// and then once more on the first visit of the scan pass, from a live owner if there is one.
/// While an owner is held elsewhere (a Rust local, a task), the contents are never marked and count
/// as referenced from outside. Outside a collection (debugging walks) they are always traced.
///
/// This counts owners by visits, so a trace hook must trace each of its handles once per pass, and
/// only handles it owns: tracing one twice, or a temporary clone, while an owner is outside the
/// heap would free the contents under that owner. The tracing verifier checks both.
pub(crate) unsafe fn trace_shared_rc(
    trc: *mut root::JSTracer,
    rc: usize,
    handle: usize,
    owners: usize,
) -> bool {
    let rt = (*(trc as *mut Tracer)).rt;
    let mut number = 0;
    let pass = q::JS_GetGCPass(rt, &mut number) as u32;
    if pass != q::JS_GC_PASS_DECREF && pass != q::JS_GC_PASS_SCAN {
        return true;
    }
    // Without the table (thread exit), neither pass marks the contents.
    SHARED_RCS
        .try_with(|shared| {
            let mut shared = shared.borrow_mut();
            if shared.rt != rt || shared.gc_number != number {
                shared.rt = rt;
                shared.gc_number = number;
                shared.rcs.clear();
            }
            let entry = shared.rcs.entry(rc).or_default();
            if pass == q::JS_GC_PASS_DECREF {
                if super::verify::enabled() {
                    verify_shared_rc_visit(entry, rc, handle, owners);
                }
                entry.owners_traced += 1;
                if entry.owners_traced == owners && !entry.decremented {
                    entry.decremented = true;
                    return true;
                }
                false
            } else if entry.decremented && !entry.restored {
                entry.restored = true;
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
}

fn verify_shared_rc_visit(entry: &mut SharedRc, rc: usize, handle: usize, owners: usize) {
    let problem = if entry.handles.contains(&handle) {
        format!("traced through the handle at {handle:#x} twice")
    } else if entry.owners != 0 && entry.owners != owners {
        format!("traced with {} and with {owners} owners", entry.owners)
    } else {
        entry.owners = owners;
        entry.handles.push(handle);
        return;
    };
    eprintln!(
        "QuickJS tracing verifier: the shared Rc at {rc:#x} was {problem} in one GC pass; \
         its contents could be freed while an owner outside the heap holds them\n{}",
        std::backtrace::Backtrace::force_capture()
    );
    std::process::abort();
}

/// Forgets the shared `Rc`s of a runtime being destroyed: a later runtime may get its address and
/// its collection numbers.
pub(crate) fn forget_shared_rcs() {
    let _ = SHARED_RCS.try_with(|shared| {
        let mut shared = shared.borrow_mut();
        shared.rt = ptr::null_mut();
        shared.rcs = HashMap::new();
    });
}

unsafe extern "C" fn gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    // QuickJS: a `JSDomStub` has no slots. Mark its realm and trace the DOM node like an
    // `ObjectData`-backed object did.
    if is_dom_stub(val) {
        if q::JS_GetDomStubDomPtr(val).is_null() {
            return;
        }
        super::mark_realm(rt, q::JS_GetDomStubRealm(val) as *mut RealmData, mark);
        // The interface class, from the `Reflector` the stub owns.
        let clasp = stub_class(val);
        if !clasp.is_null() {
            let ops = (*clasp).cOps;
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
        return;
    }
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

/// Creates a `JSDomStub` owning `dom_ptr` (a DOM node) and one reference to the current realm.
/// Unlike a materialized object, the stub carries no `ObjectData` and no slots: its fields are
/// read back through `JS_GetDomStub*`. `clasp` is the interface's class, the one `wrap` recorded
/// on the `Reflector` that `dom_ptr` points to; `QuickJS` later resolves the class id from it via
/// `servo_dom_stub_class_id`.
pub(crate) unsafe fn JS_NewDomStub(
    cx: *mut root::JSContext,
    clasp: *const root::JSClass,
    dom_ptr: *mut c_void,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    // Registers the interface's class in this runtime on first use; QuickJS gets the id back
    // through `servo_dom_stub_class_id` when it finalizes or traces the stub, so the stub itself
    // needs no class id.
    assert!(!clasp.is_null(), "JSDomStub without an interface class");
    class_id(cx.rt, clasp);
    let realm = super::hold_realm(cx.current_realm());
    let obj = q::JS_NewDomStub(cx.qctx(), dom_ptr, realm as *mut c_void);
    if obj.tag() == q::JS_TAG_EXCEPTION {
        // `js_malloc` failed: the stub was not created, so drop the reference it would own.
        super::release_realm(realm);
        return ptr::null_mut();
    }
    adopt_object(cx, obj)
}

/// Detaches the DOM node and realm from its stub and releases the stub's realm reference. The
/// stub stays alive until its references are dropped, but no longer finalizes the node. The caller must have handed the node to its new owner (the `JSObject` bound in place of
/// the stub) beforehand, so that the new object holds its own realm reference.
pub(crate) unsafe fn JS_UnbindDomStub(cx: *mut root::JSContext, obj: *mut root::JSObject) {
    let cx = Cx::from_raw(cx);
    let v = obj_val(obj);
    let realm = q::JS_GetDomStubRealm(v) as *mut RealmData;
    // Once detached, this stub cannot be materialized again. Drop the auxiliary pool count now
    // so draining its remaining pool entries only performs a cheap map miss.
    super::forget_dom_stub_pool_refs(obj);
    q::JS_UnbindDomStub(cx.qctx(), v);
    super::release_realm(realm);
}

/// Rust layout of the prefix of QuickJS's private `JSMallocBlockHeader`.
///
/// `gc_obj_type_and_mark` represents the adjacent C bitfields `gc_obj_type: 7` and `mark: 1`.
/// Keep the field order in sync with `JSMallocBlockHeader` in `quickjs.c`.
#[repr(C, align(8))]
struct JSMallocBlockHeader {
    _block_idx_or_free_next: u16,
    _block_size_idx: u8,
    gc_obj_type_and_mark: u8,
    _ref_count: c_int,
}

const _: () = {
    assert!(std::mem::size_of::<JSMallocBlockHeader>() == 8);
    assert!(std::mem::align_of::<JSMallocBlockHeader>() == 8);
    assert!(std::mem::offset_of!(JSMallocBlockHeader, gc_obj_type_and_mark) == 3);
    assert!(std::mem::offset_of!(JSMallocBlockHeader, _ref_count) == 4);
};

#[cfg(target_endian = "little")]
const JS_MALLOC_BLOCK_GC_OBJ_TYPE_MASK: u8 = 0x7f;
#[cfg(target_endian = "big")]
const JS_MALLOC_BLOCK_GC_OBJ_TYPE_MASK: u8 = 0xfe;
#[cfg(target_endian = "little")]
const JS_MALLOC_BLOCK_GC_OBJ_TYPE_SHIFT: u32 = 0;
#[cfg(target_endian = "big")]
const JS_MALLOC_BLOCK_GC_OBJ_TYPE_SHIFT: u32 = 1;
const JS_GC_OBJ_TYPE_DOM_STUB: u8 = 7;

/// Rust-side implementation of QuickJS's `JS_IsDomStub`.
///
/// Keep this check in sync with `JS_IsDomStub` in `quickjs.c`: a stub is a GC object that is not
/// a `JSObject`, so its `gc_obj_type` must be read from the allocator header instead of using
/// `JS_GetClassID`. The type and `mark` share the header byte before `ref_count`; their bit order
/// follows the target ABI (little-endian starts at the least-significant bit).
#[inline(always)]
pub(crate) unsafe fn is_dom_stub(v: q::JSValue) -> bool {
    if v.tag() != q::JS_TAG_OBJECT {
        return false;
    }
    let ptr = v.ptr();
    if ptr.is_null() {
        return false;
    }
    let header = unsafe { &*ptr.cast::<JSMallocBlockHeader>().sub(1) };
    let gc_obj_type = (header.gc_obj_type_and_mark & JS_MALLOC_BLOCK_GC_OBJ_TYPE_MASK)
        >> JS_MALLOC_BLOCK_GC_OBJ_TYPE_SHIFT;
    gc_obj_type == JS_GC_OBJ_TYPE_DOM_STUB
}

/// Whether `obj` is a `JSDomStub` that has not been materialized into a real `JSObject` yet.
///
/// This is the Rust-side implementation of QuickJS's `JS_IsDomStub`, inlined without calling the
/// C FFI function. Keep it in sync with `JS_IsDomStub` in `quickjs.c`.
#[inline(always)]
pub(crate) unsafe fn JS_IsDomStub(obj: *mut root::JSObject) -> bool {
    !obj.is_null() && is_dom_stub(obj_val(obj))
}

/// The DOM node a `JSDomStub` owns (null once it has been unbound).
pub(crate) unsafe fn JS_GetDomStubDomPtr(obj: *mut root::JSObject) -> *mut c_void {
    q::JS_GetDomStubDomPtr(obj_val(obj))
}

/// The reference count of a `JSDomStub` (or of any other GC object).
pub(crate) unsafe fn JS_GetDomStubRefCount(stub: *mut root::JSObject) -> i32 {
    q::JS_GetRefCount(obj_val(stub))
}

/// The number of autorelease-pool entries currently holding `stub`.
pub(crate) unsafe fn JS_GetDomStubPoolRefCount(stub: *mut root::JSObject) -> u32 {
    super::dom_stub_pool_ref_count(stub)
}

/// What a `JSDomStub` recorded at wrap time and needs to materialize: the global object of its
/// realm and the shim context that owns the realm. Must be called before `JS_UnbindDomStub`,
/// which clears the stub's realm. The interface's class comes from the `Reflector` instead.
pub(crate) unsafe fn JS_GetDomStubInfo(
    stub: *mut root::JSObject,
    global: *mut *mut root::JSObject,
    cx: *mut *mut root::JSContext,
) {
    let v = obj_val(stub);
    let realm = q::JS_GetDomStubRealm(v) as *mut RealmData;
    assert!(!realm.is_null(), "JS_GetDomStubInfo on an unbound stub");
    *global = (*realm).global;
    *cx = super::context_of_realm(realm);
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
