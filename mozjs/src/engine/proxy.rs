/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Proxies (DOM proxy handlers, the WindowProxy), wrappers, compartments, principals and the
//! resolve hooks of global classes.
//!
//! A SpiderMonkey proxy is a QuickJS object whose exotic methods call the handler's `ProxyTraps`,
//! falling back to `js::BaseProxyHandler` (`CreateProxyHandler`) or `js::Wrapper`
//! (`CreateWrapperProxyHandler`: forward to the target, the private object). QuickJS consults
//! ordinary properties before exotic methods, so proxies have none (DOM proxies keep expandos in
//! a separate object). The private value and the reserved slots live in `ProxyData`, each holding
//! a reference that `gc_mark` marks.
//!
//! There are two QuickJS classes. A proxy with a static prototype keeps it in its shape and has no
//! prototype hooks, so `GetStaticPrototype`, `JS_SetImmutablePrototype` and `[[SetPrototypeOf]]`
//! work as for ordinary objects. A proxy with a lazy (dynamic) prototype has a null shape
//! prototype and `getPrototype`/`setPrototype` hooks, as QuickJS requires.
//!
//! There are no compartments beyond realms and no cross-compartment wrappers: wrapping is the
//! identity except that Window globals become their WindowProxy, and `IsWrapper` holds only for
//! `js::Wrapper` proxies (the WindowProxy). Cross-origin isolation relies on origin-keyed event
//! loops, see the design doc.

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_uint, c_void, CString};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

use servo_quickjs as q;

use super::class::{self, Tracer};
use super::realm::{for_each_realm, realm_of_global};
use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::glue;
use crate::sys::jsapi as root;
use crate::sys::jsid::from_atom;
use crate::sys::jsval::{JSVal, ObjectValue, UndefinedValue};

fn cx_of<'a>(ctx: *mut q::JSContext) -> &'a Cx {
    unsafe { &*(q::JS_GetRuntimeOpaque(q::JS_GetRuntime(ctx)) as *const Cx) }
}

fn handle<T>(p: &T) -> root::JS::Handle<T> {
    unsafe { root::JS::Handle::from_marked_location(p) }
}

fn handle_mut<T>(p: &mut T) -> root::JS::MutableHandle<T> {
    unsafe { root::JS::MutableHandle::from_marked_location(p) }
}

unsafe fn dup(ctx: *mut q::JSContext, v: q::JSValue) -> q::JSValue {
    q::JS_DupValueRT(q::JS_GetRuntime(ctx), v)
}

fn obj_or_null(v: q::JSValue) -> *mut root::JSObject {
    if v.tag() == q::JS_TAG_OBJECT {
        val_obj(v)
    } else {
        ptr::null_mut()
    }
}

fn obj_or_null_val(obj: *mut root::JSObject) -> q::JSValue {
    if obj.is_null() {
        q::JS_NULL
    } else {
        obj_val(obj)
    }
}

/// A failed SpiderMonkey operation as a QuickJS exception (`false` without a pending exception
/// is uncatchable termination).
unsafe fn trap_failed(ctx: *mut q::JSContext) {
    if q::JS_HasException(ctx) == 0 {
        q::JS_ThrowInternalError(ctx, c"script terminated".as_ptr());
        q::JS_SetUncatchableException(ctx, 1);
    }
}

unsafe fn throw_type_error(ctx: *mut q::JSContext, msg: &str) {
    let msg = CString::new(msg).unwrap_or_default();
    q::JS_ThrowTypeError(ctx, c"%s".as_ptr(), msg.as_ptr());
}

fn op_ok(result: &root::JS::ObjectOpResult) -> bool {
    result.code_ == root::JS::ObjectOpResult_SpecialCodes::OkCode as usize
}

fn new_op_result() -> root::JS::ObjectOpResult {
    root::JS::ObjectOpResult {
        code_: root::JS::ObjectOpResult_SpecialCodes::Uninitialized as usize,
    }
}

/// Reports a failed `ObjectOpResult` as QuickJS does for `flags` (throw, or return false).
unsafe fn throw_or_false(
    ctx: *mut q::JSContext,
    flags: c_int,
    what: &str,
    atom: q::JSAtom,
) -> c_int {
    let flags = flags as u32;
    let strict = flags & q::JS_PROP_THROW_STRICT != 0 && q::JS_IsStrictMode(ctx) != 0;
    if flags & q::JS_PROP_THROW == 0 && !strict {
        return 0;
    }
    let name = q::JS_AtomToCString(ctx, atom);
    let msg = if name.is_null() {
        format!("cannot {what} property")
    } else {
        let msg = format!(
            "cannot {what} property '{}'",
            std::ffi::CStr::from_ptr(name).to_string_lossy()
        );
        q::JS_FreeCString(ctx, name);
        msg
    };
    throw_type_error(ctx, &msg);
    -1
}

// Handlers.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `CreateProxyHandler`: missing traps behave as in `js::BaseProxyHandler`.
    Base,
    /// `CreateForwardingProxyHandler`: missing traps forward to the target, as in
    /// `js::ForwardingProxyHandler`.
    Forwarding,
    /// `CreateWrapperProxyHandler`: missing traps forward to the target.
    Wrapper,
    /// `JS_NewDeadWrapper`: every operation throws.
    Dead,
}

/// What `CreateProxyHandler`, `CreateForwardingProxyHandler` and `CreateWrapperProxyHandler`
/// return.
pub(crate) struct ProxyHandler {
    traps: glue::ProxyTraps,
    extra: *const c_void,
    kind: Kind,
}

unsafe impl Sync for ProxyHandler {}

impl ProxyHandler {
    const fn empty(kind: Kind) -> ProxyHandler {
        ProxyHandler {
            traps: unsafe { std::mem::zeroed() },
            extra: ptr::null(),
            kind,
        }
    }
}

static DEAD_HANDLER: ProxyHandler = ProxyHandler::empty(Kind::Dead);
static PLAIN_WRAPPER_HANDLER: ProxyHandler = ProxyHandler::empty(Kind::Wrapper);

/// The family of `CreateProxyHandler` handlers; only its address matters. `IsProxyHandlerFamily`
/// also holds for `CreateForwardingProxyHandler` handlers, as in mozjs.
static HANDLER_FAMILY: u8 = 0;

const fn proxy_class_def(name: &'static std::ffi::CStr, slots: u32) -> root::JSClass {
    root::JSClass {
        name: name.as_ptr(),
        flags: root::JSClass_NON_NATIVE
            | root::JSCLASS_IS_PROXY
            | root::JSCLASS_DELAY_METADATA_BUILDER
            | ((slots & root::JSCLASS_RESERVED_SLOTS_MASK) << root::JSCLASS_RESERVED_SLOTS_SHIFT),
        cOps: ptr::null(),
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    }
}

/// `js::ProxyClass`, for proxies created without a class.
static PROXY_CLASS: root::JSClass = proxy_class_def(c"Proxy", 2);
/// mozjs's `WindowProxyClass`, with `js::SwappableProxyReservedSlots` slots.
static WINDOW_PROXY_CLASS: root::JSClass = proxy_class_def(c"Proxy", 2);

// Proxy objects.

struct ProxyData {
    handler: *const ProxyHandler,
    clasp: *const root::JSClass,
    /// Held (`hold_realm`) and marked.
    realm: *mut RealmData,
    /// The target of wrappers, the expando object of DOM proxies.
    private: Cell<q::JSValue>,
    slots: Box<[Cell<q::JSValue>]>,
}

/// QuickJS class ids of proxies with a static and with a lazy prototype.
static PROXY_CLASS_IDS: OnceLock<[q::JSClassID; 2]> = OnceLock::new();

unsafe fn proxy_class_id(rt: *mut q::JSRuntime, lazy_proto: bool) -> q::JSClassID {
    let ids = *PROXY_CLASS_IDS.get_or_init(|| [class::new_class_id(), class::new_class_id()]);
    for (id, exotic) in ids.iter().zip([&STATIC_PROTO_EXOTIC, &LAZY_PROTO_EXOTIC]) {
        if q::JS_IsRegisteredClass(rt, *id) == 0 {
            let def = q::JSClassDef {
                class_name: c"Proxy".as_ptr(),
                finalizer: Some(proxy_finalize),
                gc_mark: Some(proxy_gc_mark),
                call: None,
                exotic: exotic as *const _ as *mut _,
            };
            assert_eq!(q::JS_NewClass(rt, *id, &def), 0);
        }
    }
    ids[lazy_proto as usize]
}

unsafe fn proxy_data<'a>(obj: *mut root::JSObject) -> Option<&'a ProxyData> {
    let ids = PROXY_CLASS_IDS.get()?;
    let v = obj_val(obj);
    let id = q::JS_GetClassID(v);
    if !ids.contains(&id) {
        return None;
    }
    (q::JS_GetOpaque(v, id) as *const ProxyData).as_ref()
}

unsafe fn proxy_handler<'a>(obj: *mut root::JSObject) -> Option<&'a ProxyHandler> {
    proxy_data(obj).map(|data| &*data.handler)
}

/// The class `obj` was created with, if it is a proxy.
pub(crate) unsafe fn proxy_class(obj: *mut root::JSObject) -> Option<*const root::JSClass> {
    proxy_data(obj).map(|data| data.clasp)
}

pub(crate) unsafe fn proxy_realm(obj: *mut root::JSObject) -> Option<*mut RealmData> {
    proxy_data(obj)
        .map(|data| data.realm)
        .filter(|realm| !realm.is_null())
}

fn reserved_slots(clasp: *const root::JSClass) -> usize {
    let flags = unsafe { (*clasp).flags };
    ((flags >> root::JSCLASS_RESERVED_SLOTS_SHIFT) & root::JSCLASS_RESERVED_SLOTS_MASK) as usize
}

unsafe fn new_proxy(
    cx: &Cx,
    handler: *const ProxyHandler,
    private: q::JSValue,
    proto: *mut root::JSObject,
    clasp: *const root::JSClass,
    lazy_proto: bool,
) -> *mut root::JSObject {
    let clasp = if clasp.is_null() { &PROXY_CLASS } else { clasp };
    let proto = if lazy_proto {
        q::JS_NULL
    } else {
        obj_or_null_val(proto)
    };
    let obj = q::JS_NewObjectProtoClass(cx.qctx(), proto, proxy_class_id(cx.rt, lazy_proto));
    if obj.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    let data = Box::new(ProxyData {
        handler,
        clasp,
        realm: super::hold_realm(cx.current_realm()),
        private: Cell::new(q::JS_DupValueRT(cx.rt, private)),
        slots: vec![Cell::new(q::JS_UNDEFINED); reserved_slots(clasp)].into_boxed_slice(),
    });
    q::JS_SetOpaque(obj, Box::into_raw(data) as *mut c_void);
    class::adopt_object(cx, obj)
}

unsafe extern "C" fn proxy_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = q::JS_GetOpaque(val, q::JS_GetClassID(val)) as *mut ProxyData;
    if data.is_null() {
        return;
    }
    if let Some(finalize) = (*(*data).handler).traps.finalize {
        finalize(ptr::null_mut(), val_obj(val));
    }
    let data = Box::from_raw(data);
    q::JS_FreeValueRT(rt, data.private.get());
    for v in data.slots.iter() {
        q::JS_FreeValueRT(rt, v.get());
    }
    super::release_realm(data.realm);
}

unsafe extern "C" fn proxy_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    let data = q::JS_GetOpaque(val, q::JS_GetClassID(val)) as *mut ProxyData;
    if data.is_null() {
        return;
    }
    for v in std::iter::once(&(*data).private).chain((*data).slots.iter()) {
        if v.get().has_ref_count() {
            q::JS_MarkValue(rt, v.get(), mark);
        }
    }
    super::mark_realm(rt, (*data).realm, mark);
    if let Some(trace) = (*(*data).handler).traps.trace {
        let mut tracer = Tracer { rt, mark };
        trace(
            &mut tracer as *mut Tracer as *mut root::JSTracer,
            val_obj(val),
        );
    }
}

/// A proxy and its handler: SpiderMonkey's proxy operations, with the handler's traps or their
/// defaults.
struct ProxyRef {
    cx: *mut root::JSContext,
    proxy: *mut root::JSObject,
    handler: *const ProxyHandler,
}

impl ProxyRef {
    unsafe fn new(cx: *mut root::JSContext, proxy: *mut root::JSObject) -> ProxyRef {
        let handler = proxy_data(proxy).expect("not a proxy").handler;
        ProxyRef { cx, proxy, handler }
    }

    fn traps(&self) -> &glue::ProxyTraps {
        unsafe { &(*self.handler).traps }
    }

    fn kind(&self) -> Kind {
        unsafe { (*self.handler).kind }
    }

    /// Whether missing traps forward to the target.
    fn forwards(&self) -> bool {
        matches!(self.kind(), Kind::Forwarding | Kind::Wrapper)
    }

    fn proxy(&self) -> root::JS::HandleObject {
        handle(&self.proxy)
    }

    unsafe fn lazy_proto(&self) -> bool {
        q::JS_GetClassID(obj_val(self.proxy)) == PROXY_CLASS_IDS.get().unwrap()[1]
    }

    unsafe fn qctx(&self) -> *mut q::JSContext {
        Cx::from_raw(self.cx).qctx()
    }

    unsafe fn target(&self) -> *mut root::JSObject {
        obj_or_null(proxy_data(self.proxy).unwrap().private.get())
    }

    /// Fails for operations a handler can't perform (dead wrappers, missing required traps).
    unsafe fn unsupported(&self, what: &str) -> bool {
        let msg = match self.kind() {
            Kind::Dead => "can't access dead object".to_owned(),
            _ => format!("proxy handler has no {what} trap"),
        };
        throw_type_error(self.qctx(), &msg);
        false
    }

    unsafe fn get_own_property_descriptor(
        &self,
        id: &root::jsid,
        desc: &mut root::JS::PropertyDescriptor,
        is_none: &mut bool,
    ) -> bool {
        *is_none = true;
        match self.traps().getOwnPropertyDescriptor {
            Some(trap) => trap(self.cx, self.proxy(), handle(id), handle_mut(desc), is_none),
            None if self.forwards() => {
                let target = self.target();
                super::JS_GetOwnPropertyDescriptorById(
                    self.cx,
                    handle(&target),
                    handle(id),
                    handle_mut(desc),
                    is_none,
                )
            }
            None => self.unsupported("getOwnPropertyDescriptor"),
        }
    }

    unsafe fn has_own(&self, id: &root::jsid, bp: &mut bool) -> bool {
        if let Some(trap) = self.traps().hasOwn {
            return trap(self.cx, self.proxy(), handle(id), bp);
        }
        if self.forwards() {
            let target = self.target();
            return super::JS_HasOwnPropertyById(self.cx, handle(&target), handle(id), bp);
        }
        let mut desc = root::JS::PropertyDescriptor::default();
        let mut is_none = true;
        if !self.get_own_property_descriptor(id, &mut desc, &mut is_none) {
            return false;
        }
        *bp = !is_none;
        true
    }

    unsafe fn get_prototype(&self, proto: &mut *mut root::JSObject) -> bool {
        if !self.lazy_proto() {
            return super::JS_GetPrototype(self.cx, self.proxy(), handle_mut(proto));
        }
        match self.traps().getPrototype {
            Some(trap) => trap(self.cx, self.proxy(), handle_mut(proto)),
            None if self.forwards() => {
                let target = self.target();
                super::JS_GetPrototype(self.cx, handle(&target), handle_mut(proto))
            }
            None => self.unsupported("getPrototype"),
        }
    }

    unsafe fn is_extensible(&self, extensible: &mut bool) -> bool {
        match self.traps().isExtensible {
            Some(trap) => trap(self.cx, self.proxy(), extensible),
            None if self.forwards() => {
                let r = q::JS_IsExtensible(self.qctx(), obj_val(self.target()));
                *extensible = r > 0;
                r >= 0
            }
            None => self.unsupported("isExtensible"),
        }
    }

    unsafe fn has(&self, id: &root::jsid, bp: &mut bool) -> bool {
        if let Some(trap) = self.traps().has {
            return trap(self.cx, self.proxy(), handle(id), bp);
        }
        if self.forwards() {
            let target = self.target();
            return super::JS_HasPropertyById(self.cx, handle(&target), handle(id), bp);
        }
        if !self.has_own(id, bp) {
            return false;
        }
        if *bp {
            return true;
        }
        let mut proto = ptr::null_mut();
        if !self.get_prototype(&mut proto) {
            return false;
        }
        if proto.is_null() {
            *bp = false;
            return true;
        }
        super::JS_HasPropertyById(self.cx, handle(&proto), handle(id), bp)
    }

    unsafe fn get(&self, receiver: &JSVal, id: &root::jsid, vp: &mut JSVal) -> bool {
        if let Some(trap) = self.traps().get {
            return trap(
                self.cx,
                self.proxy(),
                handle(receiver),
                handle(id),
                handle_mut(vp),
            );
        }
        if self.forwards() {
            let target = self.target();
            return super::JS_ForwardGetPropertyTo(
                self.cx,
                handle(&target),
                handle(id),
                handle(receiver),
                handle_mut(vp),
            );
        }
        let mut desc = root::JS::PropertyDescriptor::default();
        let mut is_none = true;
        if !self.get_own_property_descriptor(id, &mut desc, &mut is_none) {
            return false;
        }
        if is_none {
            let mut proto = ptr::null_mut();
            if !self.get_prototype(&mut proto) {
                return false;
            }
            if proto.is_null() {
                *vp = UndefinedValue();
                return true;
            }
            return super::JS_ForwardGetPropertyTo(
                self.cx,
                handle(&proto),
                handle(id),
                handle(receiver),
                handle_mut(vp),
            );
        }
        if !desc.hasGetter_() && !desc.hasSetter_() {
            *vp = desc.value_;
            return true;
        }
        if desc.getter_.is_null() {
            *vp = UndefinedValue();
            return true;
        }
        let getter = ObjectValue(desc.getter_);
        super::Call(
            self.cx,
            handle(receiver),
            handle(&getter),
            &root::JS::HandleValueArray::empty(),
            handle_mut(vp),
        )
    }

    unsafe fn set(
        &self,
        id: &root::jsid,
        v: &JSVal,
        receiver: &JSVal,
        result: &mut root::JS::ObjectOpResult,
    ) -> bool {
        if let Some(trap) = self.traps().set {
            return trap(
                self.cx,
                self.proxy(),
                handle(id),
                handle(v),
                handle(receiver),
                result,
            );
        }
        if self.forwards() {
            let target = self.target();
            return super::JS_ForwardSetPropertyTo(
                self.cx,
                handle(&target),
                handle(id),
                handle(v),
                handle(receiver),
                result,
            );
        }
        let mut desc = root::JS::PropertyDescriptor::default();
        let mut is_none = true;
        if !self.get_own_property_descriptor(id, &mut desc, &mut is_none) {
            return false;
        }
        let desc_handle = handle(&desc);
        let own_desc = if is_none {
            ptr::null()
        } else {
            &desc_handle as *const _
        };
        super::SetPropertyIgnoringNamedGetter(
            self.cx,
            self.proxy(),
            handle(id),
            handle(v),
            handle(receiver),
            own_desc,
            result,
        )
    }

    unsafe fn define_property(
        &self,
        id: &root::jsid,
        desc: &root::JS::PropertyDescriptor,
        result: &mut root::JS::ObjectOpResult,
    ) -> bool {
        match self.traps().defineProperty {
            Some(trap) => trap(self.cx, self.proxy(), handle(id), handle(desc), result),
            None if self.forwards() => {
                let target = self.target();
                super::JS_DefinePropertyById(
                    self.cx,
                    handle(&target),
                    handle(id),
                    handle(desc),
                    result,
                )
            }
            None => self.unsupported("defineProperty"),
        }
    }

    unsafe fn delete(&self, id: &root::jsid, result: &mut root::JS::ObjectOpResult) -> bool {
        match self.traps().delete_ {
            Some(trap) => trap(self.cx, self.proxy(), handle(id), result),
            None if self.forwards() => {
                let target = self.target();
                super::JS_DeletePropertyById(self.cx, handle(&target), handle(id), result)
            }
            None => self.unsupported("delete"),
        }
    }

    unsafe fn own_property_keys(&self, props: root::JS::MutableHandleIdVector) -> bool {
        match self.traps().ownPropertyKeys {
            Some(trap) => trap(self.cx, self.proxy(), props),
            None if self.forwards() => {
                let target = self.target();
                let flags = root::JSITER_OWNONLY | root::JSITER_HIDDEN | root::JSITER_SYMBOLS;
                super::GetPropertyKeys(self.cx, handle(&target), flags as c_uint, props)
            }
            None => self.unsupported("ownPropertyKeys"),
        }
    }

    /// `[[SetPrototypeOf]]` of a proxy with a lazy prototype.
    unsafe fn set_prototype(&self, proto: *mut root::JSObject) -> c_int {
        let ctx = self.qctx();
        if let Some(trap) = self.traps().setPrototype {
            let mut result = new_op_result();
            if !trap(self.cx, self.proxy(), handle(&proto), &mut result) {
                trap_failed(ctx);
                return -1;
            }
            return op_ok(&result) as c_int;
        }
        if self.forwards() {
            return q::JS_SetPrototype(ctx, obj_val(self.target()), obj_or_null_val(proto));
        }
        throw_type_error(ctx, "can't set prototype of this object");
        -1
    }
}

/// An exotic method call: the proxy, with the realm of the calling code entered (SpiderMonkey
/// runs traps in the caller's realm). Values handed to Rust during the trap are released when it
/// returns, as SpiderMonkey's rooted values die with the trap's frame.
struct Trap {
    ctx: *mut q::JSContext,
    proxy: ProxyRef,
    entered: bool,
    pool_mark: (usize, usize),
}

impl Trap {
    unsafe fn new(ctx: *mut q::JSContext, obj: q::JSValue) -> Trap {
        let cx = cx_of(ctx);
        let realm = q::JS_GetContextOpaque(ctx) as *mut RealmData;
        let entered = !realm.is_null();
        if entered {
            cx.enter_realm(realm);
        }
        let pool_mark = cx.pool_mark();
        Trap {
            ctx,
            proxy: ProxyRef::new(cx.as_raw(), val_obj(obj)),
            entered,
            pool_mark,
        }
    }

    unsafe fn failed(&self) -> c_int {
        trap_failed(self.ctx);
        -1
    }
}

impl Drop for Trap {
    fn drop(&mut self) {
        let cx = cx_of(self.ctx);
        if self.entered {
            unsafe { cx.leave_realm() };
        }
        cx.drain_pool(self.pool_mark);
    }
}

/// A SpiderMonkey descriptor as a QuickJS one, holding its own references.
unsafe fn to_qjs_desc(
    ctx: *mut q::JSContext,
    desc: &root::JS::PropertyDescriptor,
) -> q::JSPropertyDescriptor {
    let mut flags = 0;
    if desc.configurable_() {
        flags |= q::JS_PROP_CONFIGURABLE;
    }
    if desc.enumerable_() {
        flags |= q::JS_PROP_ENUMERABLE;
    }
    let accessor = |present: bool, f: *mut root::JSObject| {
        if present && !f.is_null() {
            dup(ctx, obj_val(f))
        } else {
            q::JS_UNDEFINED
        }
    };
    if desc.hasGetter_() || desc.hasSetter_() {
        q::JSPropertyDescriptor {
            flags: (flags | q::JS_PROP_GETSET) as c_int,
            value: q::JS_UNDEFINED,
            getter: accessor(desc.hasGetter_(), desc.getter_),
            setter: accessor(desc.hasSetter_(), desc.setter_),
        }
    } else {
        if desc.writable_() {
            flags |= q::JS_PROP_WRITABLE;
        }
        let value = if desc.hasValue_() {
            dup(ctx, desc.value_.to_qjs())
        } else {
            q::JS_UNDEFINED
        };
        q::JSPropertyDescriptor {
            flags: flags as c_int,
            value,
            getter: q::JS_UNDEFINED,
            setter: q::JS_UNDEFINED,
        }
    }
}

/// A QuickJS `[[DefineOwnProperty]]` request as a SpiderMonkey descriptor (borrowing the values).
fn from_qjs_desc(
    flags: c_int,
    val: q::JSValue,
    getter: q::JSValue,
    setter: q::JSValue,
) -> root::JS::PropertyDescriptor {
    let flags = flags as u32;
    let mut desc = root::JS::PropertyDescriptor::default();
    if flags & q::JS_PROP_HAS_CONFIGURABLE != 0 {
        desc.set_hasConfigurable_(true);
        desc.set_configurable_(flags & q::JS_PROP_CONFIGURABLE != 0);
    }
    if flags & q::JS_PROP_HAS_ENUMERABLE != 0 {
        desc.set_hasEnumerable_(true);
        desc.set_enumerable_(flags & q::JS_PROP_ENUMERABLE != 0);
    }
    if flags & q::JS_PROP_HAS_WRITABLE != 0 {
        desc.set_hasWritable_(true);
        desc.set_writable_(flags & q::JS_PROP_WRITABLE != 0);
    }
    if flags & q::JS_PROP_HAS_VALUE != 0 {
        desc.set_hasValue_(true);
        desc.value_ = JSVal::from_qjs(val);
    }
    if flags & q::JS_PROP_HAS_GET != 0 {
        desc.set_hasGetter_(true);
        desc.getter_ = obj_or_null(getter);
    }
    if flags & q::JS_PROP_HAS_SET != 0 {
        desc.set_hasSetter_(true);
        desc.setter_ = obj_or_null(setter);
    }
    desc
}

static STATIC_PROTO_EXOTIC: q::JSClassExoticMethods = q::JSClassExoticMethods {
    get_own_property: Some(proxy_get_own_property),
    get_own_property_names: Some(proxy_get_own_property_names),
    delete_property: Some(proxy_delete_property),
    define_own_property: Some(proxy_define_own_property),
    has_property: Some(proxy_has_property),
    get_property: Some(proxy_get_property),
    set_property: Some(proxy_set_property),
    get_prototype: None,
    set_prototype: None,
    is_extensible: Some(proxy_is_extensible),
    prevent_extensions: Some(proxy_prevent_extensions),
};

static LAZY_PROTO_EXOTIC: q::JSClassExoticMethods = q::JSClassExoticMethods {
    get_prototype: Some(proxy_get_prototype),
    set_prototype: Some(proxy_set_prototype),
    ..STATIC_PROTO_EXOTIC
};

unsafe extern "C" fn proxy_get_own_property(
    ctx: *mut q::JSContext,
    desc: *mut q::JSPropertyDescriptor,
    obj: q::JSValue,
    prop: q::JSAtom,
) -> c_int {
    let t = Trap::new(ctx, obj);
    let id = from_atom(prop);
    let mut sm_desc = root::JS::PropertyDescriptor::default();
    let mut is_none = true;
    if !t
        .proxy
        .get_own_property_descriptor(&id, &mut sm_desc, &mut is_none)
    {
        return t.failed();
    }
    if is_none {
        return 0;
    }
    if !desc.is_null() {
        *desc = to_qjs_desc(ctx, &sm_desc);
    }
    1
}

unsafe extern "C" fn proxy_get_own_property_names(
    ctx: *mut q::JSContext,
    ptab: *mut *mut q::JSPropertyEnum,
    plen: *mut u32,
    obj: q::JSValue,
) -> c_int {
    let t = Trap::new(ctx, obj);
    *ptab = ptr::null_mut();
    *plen = 0;
    let mut ids: Vec<root::jsid> = Vec::new();
    if !t.proxy.own_property_keys(id_vector_handle(&mut ids)) {
        return t.failed();
    }
    if ids.is_empty() {
        return 0;
    }
    let tab =
        q::js_malloc(ctx, size_of::<q::JSPropertyEnum>() * ids.len()) as *mut q::JSPropertyEnum;
    if tab.is_null() {
        return -1;
    }
    for (i, id) in ids.iter().enumerate() {
        *tab.add(i) = q::JSPropertyEnum {
            is_enumerable: 0,
            atom: q::JS_DupAtom(ctx, id.to_atom()),
        };
    }
    *ptab = tab;
    *plen = ids.len() as u32;
    0
}

unsafe extern "C" fn proxy_delete_property(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    prop: q::JSAtom,
) -> c_int {
    let t = Trap::new(ctx, obj);
    let mut result = new_op_result();
    if !t.proxy.delete(&from_atom(prop), &mut result) {
        return t.failed();
    }
    op_ok(&result) as c_int
}

unsafe extern "C" fn proxy_define_own_property(
    ctx: *mut q::JSContext,
    this_obj: q::JSValue,
    prop: q::JSAtom,
    val: q::JSValue,
    getter: q::JSValue,
    setter: q::JSValue,
    flags: c_int,
) -> c_int {
    let t = Trap::new(ctx, this_obj);
    let desc = from_qjs_desc(flags, val, getter, setter);
    let mut result = new_op_result();
    if !t
        .proxy
        .define_property(&from_atom(prop), &desc, &mut result)
    {
        return t.failed();
    }
    if op_ok(&result) {
        1
    } else {
        throw_or_false(ctx, flags, "define", prop)
    }
}

unsafe extern "C" fn proxy_has_property(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    atom: q::JSAtom,
) -> c_int {
    let t = Trap::new(ctx, obj);
    let mut found = false;
    if !t.proxy.has(&from_atom(atom), &mut found) {
        return t.failed();
    }
    found as c_int
}

/// Set by `SetDOMProxyInformation`: DOM proxies follow SpiderMonkey's DOM proxy rules.
static DOM_PROXIES: AtomicBool = AtomicBool::new(false);

enum Unshadowed {
    /// The traps must decide.
    No,
    /// The name resolves on this prototype (borrowed).
    Proto(q::JSValue),
    Exception,
}

/// Whether `atom` resolves on the prototype chain of the DOM proxy `obj` without its traps, as
/// SpiderMonkey's JIT inline caches assume (`SetDOMProxyInformation`): a DOM proxy shadows a name
/// that isn't an index only through its expando object, and its named properties come after its
/// prototype's.
unsafe fn unshadowed(ctx: *mut q::JSContext, obj: q::JSValue, atom: q::JSAtom) -> Unshadowed {
    // Integer indices are tagged atoms.
    if !DOM_PROXIES.load(Ordering::Relaxed) || atom & (1 << 31) != 0 {
        return Unshadowed::No;
    }
    let Some(&[static_proto_class, _]) = PROXY_CLASS_IDS.get() else {
        return Unshadowed::No;
    };
    if q::JS_GetClassID(obj) != static_proto_class {
        return Unshadowed::No;
    }
    let data = &*(q::JS_GetOpaque(obj, static_proto_class) as *const ProxyData);
    let handler = &*data.handler;
    // Generated DOM proxies have their DOM class as extra; the window's named properties object
    // has none and different rules.
    if handler.kind != Kind::Base || handler.extra.is_null() {
        return Unshadowed::No;
    }
    let proto = q::JS_GetObjectShapeProto(obj.ptr());
    if proto.is_null() {
        return Unshadowed::No;
    }
    let proto = q::JSValue::mkptr(q::JS_TAG_OBJECT, proto);
    // Chains with exotic objects (e.g. a JS Proxy) keep the traps' order of lookups.
    let flags = (q::JS_HAS_CACHED_ORDINARY_ONLY | q::JS_HAS_CACHED_NO_INDEX) as i32;
    if q::JS_HasPropertyCached(ctx, proto, atom, flags) != 1 {
        return Unshadowed::No;
    }
    let expando = data.private.get();
    if expando.tag() == q::JS_TAG_OBJECT {
        match q::JS_GetOwnProperty(ctx, ptr::null_mut(), expando, atom) {
            0 => {}
            1 => return Unshadowed::No,
            _ => return Unshadowed::Exception,
        }
    }
    Unshadowed::Proto(proto)
}

unsafe extern "C" fn proxy_get_property(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    atom: q::JSAtom,
    receiver: q::JSValue,
) -> q::JSValue {
    match unshadowed(ctx, obj, atom) {
        Unshadowed::Proto(proto) => {
            return q::JS_GetPropertyInternal(ctx, proto, atom, receiver, 0)
        }
        Unshadowed::Exception => return q::JS_EXCEPTION,
        Unshadowed::No => {}
    }
    let t = Trap::new(ctx, obj);
    let mut vp = UndefinedValue();
    if !t
        .proxy
        .get(&JSVal::from_qjs(receiver), &from_atom(atom), &mut vp)
    {
        t.failed();
        return q::JS_EXCEPTION;
    }
    dup(ctx, vp.to_qjs())
}

unsafe extern "C" fn proxy_set_property(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    atom: q::JSAtom,
    value: q::JSValue,
    receiver: q::JSValue,
    flags: c_int,
) -> c_int {
    match unshadowed(ctx, obj, atom) {
        Unshadowed::Proto(proto) => {
            let value = q::JS_DupValue(ctx, value);
            return q::JS_SetPropertyInternal(ctx, proto, atom, value, receiver, flags);
        }
        Unshadowed::Exception => return -1,
        Unshadowed::No => {}
    }
    let t = Trap::new(ctx, obj);
    let mut result = new_op_result();
    if !t.proxy.set(
        &from_atom(atom),
        &JSVal::from_qjs(value),
        &JSVal::from_qjs(receiver),
        &mut result,
    ) {
        return t.failed();
    }
    if op_ok(&result) {
        1
    } else {
        throw_or_false(ctx, flags, "set", atom)
    }
}

unsafe extern "C" fn proxy_get_prototype(ctx: *mut q::JSContext, obj: q::JSValue) -> q::JSValue {
    let t = Trap::new(ctx, obj);
    let mut proto = ptr::null_mut();
    if !t.proxy.get_prototype(&mut proto) {
        t.failed();
        return q::JS_EXCEPTION;
    }
    dup(ctx, obj_or_null_val(proto))
}

unsafe extern "C" fn proxy_set_prototype(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    proto_val: q::JSValue,
) -> c_int {
    let t = Trap::new(ctx, obj);
    t.proxy.set_prototype(obj_or_null(proto_val))
}

unsafe extern "C" fn proxy_is_extensible(ctx: *mut q::JSContext, obj: q::JSValue) -> c_int {
    let t = Trap::new(ctx, obj);
    let mut extensible = false;
    if !t.proxy.is_extensible(&mut extensible) {
        return t.failed();
    }
    extensible as c_int
}

unsafe extern "C" fn proxy_prevent_extensions(ctx: *mut q::JSContext, obj: q::JSValue) -> c_int {
    let t = Trap::new(ctx, obj);
    let mut result = new_op_result();
    let ok = match t.proxy.traps().preventExtensions {
        Some(trap) => trap(t.proxy.cx, t.proxy.proxy(), &mut result),
        None if t.proxy.forwards() => {
            return q::JS_PreventExtensions(ctx, obj_val(t.proxy.target()));
        }
        None => t.proxy.unsupported("preventExtensions"),
    };
    if !ok {
        return t.failed();
    }
    op_ok(&result) as c_int
}

// The proxy API.

pub(crate) unsafe fn CreateProxyHandler(
    aTraps: *const glue::ProxyTraps,
    aExtra: *const ::std::os::raw::c_void,
) -> *const ::std::os::raw::c_void {
    let handler = ProxyHandler {
        traps: *aTraps,
        extra: aExtra,
        kind: Kind::Base,
    };
    Box::into_raw(Box::new(handler)) as *const c_void
}

pub(crate) unsafe fn CreateForwardingProxyHandler(
    aTraps: *const glue::ProxyTraps,
    aExtra: *const ::std::os::raw::c_void,
) -> *const ::std::os::raw::c_void {
    let handler = ProxyHandler {
        traps: *aTraps,
        extra: aExtra,
        kind: Kind::Forwarding,
    };
    Box::into_raw(Box::new(handler)) as *const c_void
}

pub(crate) unsafe fn CreateWrapperProxyHandler(
    aTraps: *const glue::ProxyTraps,
) -> *const ::std::os::raw::c_void {
    let handler = ProxyHandler {
        traps: *aTraps,
        extra: ptr::null(),
        kind: Kind::Wrapper,
    };
    Box::into_raw(Box::new(handler)) as *const c_void
}

pub(crate) unsafe fn DeleteWrapperProxyHandler(handler: *const ::std::os::raw::c_void) {
    drop(Box::from_raw(handler as *mut ProxyHandler));
}

pub(crate) unsafe fn GetCrossCompartmentWrapper() -> *const ::std::os::raw::c_void {
    &PLAIN_WRAPPER_HANDLER as *const ProxyHandler as *const c_void
}

pub(crate) unsafe fn GetSecurityWrapper() -> *const ::std::os::raw::c_void {
    &PLAIN_WRAPPER_HANDLER as *const ProxyHandler as *const c_void
}

/// The glue `NewProxyObject` (SpiderMonkey's own, with `ProxyOptions`, stays a stub).
pub(crate) unsafe fn NewProxyObject(
    aCx: *mut root::JSContext,
    aHandler: *const ::std::os::raw::c_void,
    aPriv: root::JS::HandleValue,
    proto: *mut root::JSObject,
    aClass: *const root::JSClass,
    aLazyProto: bool,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(aCx);
    new_proxy(
        cx,
        aHandler as *const ProxyHandler,
        (*aPriv.ptr).to_qjs(),
        proto,
        aClass,
        aLazyProto,
    )
}

pub(crate) unsafe fn WrapperNew(
    aCx: *mut root::JSContext,
    aObj: root::JS::HandleObject,
    aHandler: *const ::std::os::raw::c_void,
    aClass: *const root::JSClass,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(aCx);
    new_proxy(
        cx,
        aHandler as *const ProxyHandler,
        obj_val(*aObj.ptr),
        ptr::null_mut(),
        aClass,
        true,
    )
}

pub(crate) unsafe fn NewWindowProxy(
    aCx: *mut root::JSContext,
    aObj: root::JS::HandleObject,
    aHandler: *const ::std::os::raw::c_void,
) -> *mut root::JSObject {
    WrapperNew(aCx, aObj, aHandler, &WINDOW_PROXY_CLASS)
}

pub(crate) unsafe fn GetWindowProxyClass() -> *const root::JSClass {
    &WINDOW_PROXY_CLASS
}

pub(crate) unsafe fn GetProxyHandler(obj: *mut root::JSObject) -> *const ::std::os::raw::c_void {
    proxy_data(obj)
        .expect("GetProxyHandler on a non-proxy")
        .handler as *const c_void
}

pub(crate) unsafe fn GetProxyHandlerExtra(
    obj: *mut root::JSObject,
) -> *const ::std::os::raw::c_void {
    let handler = proxy_handler(obj).expect("GetProxyHandlerExtra on a non-proxy");
    assert!(matches!(handler.kind, Kind::Base | Kind::Forwarding));
    handler.extra
}

pub(crate) unsafe fn GetProxyHandlerFamily() -> *const ::std::os::raw::c_void {
    &HANDLER_FAMILY as *const u8 as *const c_void
}

pub(crate) unsafe fn IsProxyHandlerFamily(obj: *mut root::JSObject) -> bool {
    proxy_handler(obj).is_some_and(|handler| matches!(handler.kind, Kind::Base | Kind::Forwarding))
}

/// Stores `v` (counted) in `slot`, and frees the old value: that can run finalizers that read this
/// proxy's slots.
unsafe fn set_counted(slot: &Cell<q::JSValue>, v: JSVal) {
    let rt = super::current().rt;
    let old = slot.replace(q::JS_DupValueRT(rt, v.to_qjs()));
    q::JS_FreeValueRT(rt, old);
}

pub(crate) unsafe fn GetProxyPrivate(obj: *mut root::JSObject, dest: *mut root::JS::Value) {
    *dest = JSVal::from_qjs(
        proxy_data(obj)
            .expect("GetProxyPrivate on a non-proxy")
            .private
            .get(),
    );
}

pub(crate) unsafe fn SetProxyPrivate(obj: *mut root::JSObject, expando: *const root::JS::Value) {
    set_counted(
        &proxy_data(obj)
            .expect("SetProxyPrivate on a non-proxy")
            .private,
        *expando,
    );
}

/// Whether the running cycle collection already finalized `obj`, which QuickJS gave class id 0.
/// It finalizes garbage in any order, and Servo's ObservableArray owners clear the owner slot of
/// their array's proxy from their finalizer. The owner's reference keeps the proxy's structure
/// until the collection ends, and the proxy is garbage too, so nothing can read the slot again.
unsafe fn finalized_in_collection(obj: *mut root::JSObject) -> bool {
    q::JS_GetClassID(obj_val(obj)) == 0
        && super::try_current().is_some_and(|cx| q::JS_IsCollectingCycles(cx.rt) != 0)
}

pub(crate) unsafe fn GetProxyReservedSlot(
    obj: *mut root::JSObject,
    slot: u32,
    dest: *mut root::JS::Value,
) {
    *dest = JSVal::from_qjs(
        proxy_data(obj)
            .expect("GetProxyReservedSlot on a non-proxy")
            .slots[slot as usize]
            .get(),
    );
}

pub(crate) unsafe fn SetProxyReservedSlot(
    obj: *mut root::JSObject,
    slot: u32,
    val: *const root::JS::Value,
) {
    if finalized_in_collection(obj) {
        return;
    }
    let data = proxy_data(obj).expect("SetProxyReservedSlot on a non-proxy");
    set_counted(&data.slots[slot as usize], *val);
}

pub(crate) unsafe fn SetDOMProxyInformation(
    domProxyHandlerFamily: *const ::std::os::raw::c_void,
    domProxyShadowsCheck: root::JS::DOMProxyShadowsCheck,
    domRemoteProxyHandlerFamily: *const ::std::os::raw::c_void,
) {
    // The shadows check is Servo's expando lookup, which `unshadowed` does natively.
    let _ = (domProxyShadowsCheck, domRemoteProxyHandlerFamily);
    if domProxyHandlerFamily == GetProxyHandlerFamily() {
        DOM_PROXIES.store(true, Ordering::Relaxed);
    }
}

pub(crate) unsafe fn InvokeGetOwnPropertyDescriptor(
    handler: *const ::std::os::raw::c_void,
    cx: *mut root::JSContext,
    proxy: root::JS::HandleObject,
    id: root::JS::HandleId,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    isNone: *mut bool,
) -> bool {
    let proxy = ProxyRef {
        cx,
        proxy: *proxy.ptr,
        handler: handler as *const ProxyHandler,
    };
    proxy.get_own_property_descriptor(&*id.ptr, &mut *desc.ptr, &mut *isNone)
}

pub(crate) unsafe fn InvokeHasOwn(
    handler: *const ::std::os::raw::c_void,
    cx: *mut root::JSContext,
    proxy: root::JS::HandleObject,
    id: root::JS::HandleId,
    bp: *mut bool,
) -> bool {
    let proxy = ProxyRef {
        cx,
        proxy: *proxy.ptr,
        handler: handler as *const ProxyHandler,
    };
    proxy.has_own(&*id.ptr, &mut *bp)
}

// Dead wrappers and wrapper maintenance (nothing to do without cross-compartment wrappers).

pub(crate) unsafe fn JS_NewDeadWrapper(
    cx: *mut root::JSContext,
    origObject: *mut root::JSObject,
) -> *mut root::JSObject {
    let _ = origObject;
    new_proxy(
        Cx::from_raw(cx),
        &DEAD_HANDLER,
        q::JS_UNDEFINED,
        ptr::null_mut(),
        ptr::null(),
        false,
    )
}

pub(crate) unsafe fn JS_IsDeadWrapper(obj: *mut root::JSObject) -> bool {
    proxy_handler(obj).is_some_and(|handler| handler.kind == Kind::Dead)
}

pub(crate) unsafe fn RemapRemoteWindowProxies(
    cx: *mut root::JSContext,
    callback: *mut root::js::CompartmentTransplantCallback,
    newTarget: root::JS::MutableHandleObject,
) {
    let _ = (cx, callback, newTarget);
}

pub(crate) unsafe fn RemapAllWrappersForObject(
    cx: *mut root::JSContext,
    oldTarget: root::JS::HandleObject,
    newTarget: root::JS::HandleObject,
) -> bool {
    let _ = (cx, oldTarget, newTarget);
    true
}

pub(crate) unsafe fn RecomputeWrappers(
    cx: *mut root::JSContext,
    sourceFilter: *const root::js::CompartmentFilter,
    targetFilter: *const root::js::CompartmentFilter,
) -> bool {
    let _ = (cx, sourceFilter, targetFilter);
    true
}

pub(crate) unsafe fn JS_RefreshCrossCompartmentWrappers(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
) -> bool {
    let _ = (cx, obj);
    true
}

pub(crate) unsafe fn JS_SetWrapObjectCallbacks(
    cx: *mut root::JSContext,
    callbacks: *const root::JSWrapObjectCallbacks,
) {
    // Wrapping is the identity plus outerization, see `JS_WrapObject`.
    let _ = (cx, callbacks);
}

// Wrapping and unwrapping.

unsafe fn is_wrapper(obj: *mut root::JSObject) -> bool {
    proxy_handler(obj).is_some_and(|handler| handler.kind == Kind::Wrapper)
}

unsafe fn unwrap_one(obj: *mut root::JSObject, stop_at_window_proxy: bool) -> *mut root::JSObject {
    if !is_wrapper(obj) || (stop_at_window_proxy && IsWindowProxy(obj)) {
        return obj;
    }
    obj_or_null(proxy_data(obj).unwrap().private.get())
}

unsafe fn unwrap(mut obj: *mut root::JSObject, stop_at_window_proxy: bool) -> *mut root::JSObject {
    loop {
        let next = unwrap_one(obj, stop_at_window_proxy);
        if next == obj || next.is_null() {
            return next;
        }
        obj = next;
    }
}

pub(crate) unsafe fn IsWrapper(obj: *mut root::JSObject) -> bool {
    is_wrapper(obj)
}

pub(crate) unsafe fn UncheckedUnwrapObject(
    obj: *mut root::JSObject,
    stopAtWindowProxy: bool,
) -> *mut root::JSObject {
    unwrap(obj, stopAtWindowProxy)
}

pub(crate) unsafe fn UncheckedUnwrap(
    obj: *mut root::JSObject,
    stopAtWindowProxy: bool,
    flagsp: *mut ::std::os::raw::c_uint,
) -> *mut root::JSObject {
    if !flagsp.is_null() {
        *flagsp = 0;
    }
    unwrap(obj, stopAtWindowProxy)
}

pub(crate) unsafe fn CheckedUnwrapStatic(obj: *mut root::JSObject) -> *mut root::JSObject {
    unwrap(obj, false)
}

pub(crate) unsafe fn CheckedUnwrapDynamic(
    obj: *mut root::JSObject,
    cx: *mut root::JSContext,
    stopAtWindowProxy: bool,
) -> *mut root::JSObject {
    let _ = cx;
    unwrap(obj, stopAtWindowProxy)
}

pub(crate) unsafe fn UnwrapObjectStatic(obj: *mut root::JSObject) -> *mut root::JSObject {
    unwrap(obj, false)
}

pub(crate) unsafe fn UnwrapObjectDynamic(
    obj: *mut root::JSObject,
    cx: *mut root::JSContext,
    stopAtWindowProxy: bool,
) -> *mut root::JSObject {
    let _ = cx;
    unwrap(obj, stopAtWindowProxy)
}

pub(crate) unsafe fn UnwrapOneCheckedDynamic(
    obj: root::JS::HandleObject,
    cx: *mut root::JSContext,
    stopAtWindowProxy: bool,
) -> *mut root::JSObject {
    let _ = cx;
    unwrap_one(*obj.ptr, stopAtWindowProxy)
}

pub(crate) unsafe fn UnwrapOneCheckedStatic(obj: *mut root::JSObject) -> *mut root::JSObject {
    unwrap_one(obj, false)
}

/// The WindowProxy for a Window global; objects of other realms are used directly.
unsafe fn outerize(obj: *mut root::JSObject) -> *mut root::JSObject {
    ToWindowProxyIfWindowSlow(obj)
}

pub(crate) unsafe fn JS_WrapObject(
    cx: *mut root::JSContext,
    objp: root::JS::MutableHandleObject,
) -> bool {
    let _ = cx;
    if !(*objp.ptr).is_null() {
        *objp.ptr = outerize(*objp.ptr);
    }
    true
}

pub(crate) unsafe fn JS_WrapValue(
    cx: *mut root::JSContext,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let _ = cx;
    if (*vp.ptr).is_object() {
        *vp.ptr = ObjectValue(outerize((*vp.ptr).to_object()));
    }
    true
}

pub(crate) unsafe fn JS_WrapPropertyDescriptor(
    cx: *mut root::JSContext,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
) -> bool {
    let _ = (cx, desc);
    true
}

pub(crate) unsafe fn JS_WrapPropertyDescriptor1(cx: *mut root::JSContext, desc: u64) -> bool {
    let _ = (cx, desc);
    true
}

// WindowProxy.

thread_local! {
    static WINDOW_PROXY_CLASS_OF_RUNTIME: Cell<*const root::JSClass> = const { Cell::new(ptr::null()) };
}

pub(crate) unsafe fn SetWindowProxyClass(cx: *mut root::JSContext, clasp: *const root::JSClass) {
    let _ = cx;
    WINDOW_PROXY_CLASS_OF_RUNTIME.with(|c| c.set(clasp));
}

pub(crate) unsafe fn IsWindowProxy(obj: *mut root::JSObject) -> bool {
    let Some(clasp) = proxy_class(obj) else {
        return false;
    };
    let window_proxy_class = WINDOW_PROXY_CLASS_OF_RUNTIME.with(|c| c.get());
    if window_proxy_class.is_null() {
        ptr::eq(clasp, &WINDOW_PROXY_CLASS)
    } else {
        ptr::eq(clasp, window_proxy_class)
    }
}

/// Makes `windowProxy` the WindowProxy of `global`: `this` in its global code (patch Q2),
/// `globalThis`, and the result of outerizing the global.
pub(crate) unsafe fn SetWindowProxy(
    cx: *mut root::JSContext,
    global: root::JS::Handle<*mut root::JSObject>,
    windowProxy: root::JS::Handle<*mut root::JSObject>,
) {
    let _ = cx;
    let realm = realm_of_global(*global.ptr);
    assert!(!realm.is_null(), "SetWindowProxy on a non-global");
    (*realm).window_proxy = *windowProxy.ptr;
    let qctx = (*realm).qctx;
    q::JS_SetGlobalThis(qctx, dup(qctx, obj_val(*windowProxy.ptr)));
}

pub(crate) unsafe fn IsWindowSlow(obj: *mut root::JSObject) -> bool {
    let realm = realm_of_global(obj);
    !realm.is_null() && !(*realm).window_proxy.is_null()
}

pub(crate) unsafe fn ToWindowProxyIfWindowSlow(obj: *mut root::JSObject) -> *mut root::JSObject {
    if IsWindowSlow(obj) {
        (*realm_of_global(obj)).window_proxy
    } else {
        obj
    }
}

pub(crate) unsafe fn ToWindowIfWindowProxy(obj: *mut root::JSObject) -> *mut root::JSObject {
    if IsWindowProxy(obj) {
        obj_or_null(proxy_data(obj).unwrap().private.get())
    } else {
        obj
    }
}

/// Swaps the handlers, targets and slots of two proxies, so that `origobj` keeps its identity
/// and gets `target`'s behaviour (`target` is left with the old one).
pub(crate) unsafe fn JS_TransplantObject(
    cx: *mut root::JSContext,
    origobj: root::JS::HandleObject,
    target: root::JS::HandleObject,
) -> *mut root::JSObject {
    let _ = cx;
    let (orig, new) = (obj_val(*origobj.ptr), obj_val(*target.ptr));
    assert!(
        proxy_data(*origobj.ptr).is_some() && q::JS_GetClassID(orig) == q::JS_GetClassID(new),
        "JS_TransplantObject is only supported between proxies of the same kind"
    );
    let orig_data = q::JS_GetOpaque(orig, q::JS_GetClassID(orig));
    q::JS_SetOpaque(orig, q::JS_GetOpaque(new, q::JS_GetClassID(new)));
    q::JS_SetOpaque(new, orig_data);
    *origobj.ptr
}

// Compartments: one per realm.

/// No realm uses the trusted principals (`JS_SetTrustedPrincipals` is a no-op).
pub(crate) unsafe fn IsSystemCompartment(comp: *mut root::JS::Compartment) -> bool {
    let _ = comp;
    false
}

pub(crate) unsafe fn IsSharableCompartment(comp: *mut root::JS::Compartment) -> bool {
    let _ = comp;
    true
}

pub(crate) unsafe fn CompartmentHasLiveGlobal(comp: *mut root::JS::Compartment) -> bool {
    let _ = comp;
    true
}

pub(crate) unsafe fn GetFirstGlobalInCompartment(
    comp: *mut root::JS::Compartment,
) -> *mut root::JSObject {
    (*(comp as *mut RealmData)).global
}

pub(crate) unsafe fn JS_IterateCompartments(
    cx: *mut root::JSContext,
    data: *mut ::std::os::raw::c_void,
    compartmentCallback: root::JSIterateCompartmentCallback,
) {
    let Some(callback) = compartmentCallback else {
        return;
    };
    for_each_realm(|realm| {
        callback(cx, data, realm as *mut root::JS::Compartment)
            != root::JS::CompartmentIterResult::Stop
    });
}

pub(crate) unsafe fn JS_IterateCompartmentsInZone(
    cx: *mut root::JSContext,
    zone: *mut root::JS::Zone,
    data: *mut ::std::os::raw::c_void,
    compartmentCallback: root::JSIterateCompartmentCallback,
) {
    let _ = zone;
    JS_IterateCompartments(cx, data, compartmentCallback);
}

unsafe fn iterate_realms(
    cx: *mut root::JSContext,
    data: *mut c_void,
    callback: root::JS::IterateRealmCallback,
    filter: impl Fn(*mut RealmData) -> bool,
) {
    let Some(callback) = callback else {
        return;
    };
    for_each_realm(|realm| {
        if filter(realm) {
            callback(cx, data, realm as *mut root::JS::Realm, ptr::null());
        }
        true
    });
}

pub(crate) unsafe fn IterateRealms(
    cx: *mut root::JSContext,
    data: *mut ::std::os::raw::c_void,
    realmCallback: root::JS::IterateRealmCallback,
) {
    iterate_realms(cx, data, realmCallback, |_| true);
}

pub(crate) unsafe fn IterateRealmsInCompartment(
    cx: *mut root::JSContext,
    compartment: *mut root::JS::Compartment,
    data: *mut ::std::os::raw::c_void,
    realmCallback: root::JS::IterateRealmCallback,
) {
    iterate_realms(cx, data, realmCallback, |realm| {
        realm as *mut root::JS::Compartment == compartment
    });
}

pub(crate) unsafe fn IterateRealmsWithPrincipals(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    data: *mut ::std::os::raw::c_void,
    realmCallback: root::JS::IterateRealmCallback,
) {
    iterate_realms(cx, data, realmCallback, |realm| unsafe {
        (*realm).principals == principals
    });
}

pub(crate) unsafe fn AssertSameCompartment(cx: *mut root::JSContext, obj: *mut root::JSObject) {
    let _ = (cx, obj);
}

pub(crate) unsafe fn AssertSameCompartment1(cx: *mut root::JSContext, v: root::JS::HandleValue) {
    let _ = (cx, v);
}

/// Every object can be used from every realm.
pub(crate) unsafe fn IsObjectInContextCompartment(
    obj: *mut root::JSObject,
    cx: *const root::JSContext,
) -> bool {
    let _ = (obj, cx);
    true
}

// Principals.

/// mozjs's `RustJSPrincipals`.
#[repr(C)]
struct RustPrincipals {
    base: root::JSPrincipals,
    callbacks: glue::JSPrincipalsCallbacks,
    private: *mut c_void,
}

thread_local! {
    static DESTROY_PRINCIPALS: Cell<root::JSDestroyPrincipalsOp> = const { Cell::new(None) };
    static READ_PRINCIPALS: Cell<root::JSReadPrincipalsOp> = const { Cell::new(None) };
}

unsafe fn refcount<'a>(principals: *mut root::JSPrincipals) -> &'a AtomicU32 {
    AtomicU32::from_ptr(ptr::addr_of_mut!((*principals).refcount))
}

pub(crate) unsafe fn CreateRustJSPrincipals(
    callbacks: *const glue::JSPrincipalsCallbacks,
    privateData: *mut ::std::os::raw::c_void,
) -> *mut root::JSPrincipals {
    let principals = RustPrincipals {
        base: root::JSPrincipals {
            vtable_: ptr::null(),
            refcount: 0,
        },
        callbacks: *callbacks,
        private: privateData,
    };
    Box::into_raw(Box::new(principals)) as *mut root::JSPrincipals
}

pub(crate) unsafe fn DestroyRustJSPrincipals(principals: *mut root::JSPrincipals) {
    drop(Box::from_raw(principals as *mut RustPrincipals));
}

pub(crate) unsafe fn GetRustJSPrincipalsPrivate(
    principals: *mut root::JSPrincipals,
) -> *mut ::std::os::raw::c_void {
    if principals.is_null() {
        ptr::null_mut()
    } else {
        (*(principals as *mut RustPrincipals)).private
    }
}

pub(crate) unsafe fn JS_HoldPrincipals(principals: *mut root::JSPrincipals) {
    refcount(principals).fetch_add(1, Ordering::Relaxed);
}

pub(crate) unsafe fn JS_DropPrincipals(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
) {
    let _ = cx;
    if refcount(principals).fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    match DESTROY_PRINCIPALS.with(|c| c.get()) {
        Some(destroy) => destroy(principals),
        None => DestroyRustJSPrincipals(principals),
    }
}

pub(crate) unsafe fn JS_InitReadPrincipalsCallback(
    cx: *mut root::JSContext,
    read: root::JSReadPrincipalsOp,
) {
    let _ = cx;
    READ_PRINCIPALS.with(|c| c.set(read));
}

pub(crate) unsafe fn JS_InitDestroyPrincipalsCallback(
    cx: *mut root::JSContext,
    destroyPrincipals: root::JSDestroyPrincipalsOp,
) {
    let _ = cx;
    DESTROY_PRINCIPALS.with(|c| c.set(destroyPrincipals));
}

pub(crate) unsafe fn GetRealmPrincipals(realm: *mut root::JS::Realm) -> *mut root::JSPrincipals {
    (*(realm as *mut RealmData)).principals
}

pub(crate) unsafe fn SetRealmPrincipals(
    realm: *mut root::JS::Realm,
    principals: *mut root::JSPrincipals,
) {
    let realm = realm as *mut RealmData;
    let old = std::mem::replace(&mut (*realm).principals, principals);
    if old == principals {
        return;
    }
    if !principals.is_null() {
        JS_HoldPrincipals(principals);
    }
    if !old.is_null() {
        JS_DropPrincipals(super::current().as_raw(), old);
    }
}

// Runtime and realm callbacks. Servo sets no realm or compartment destroy callbacks, so they are
// not called.

thread_local! {
    static DOM_CALLBACKS: Cell<*const root::js::DOMCallbacks> = const { Cell::new(ptr::null()) };
}

pub(crate) unsafe fn SetDOMCallbacks(
    cx: *mut root::JSContext,
    callbacks: *const root::js::DOMCallbacks,
) {
    DOM_CALLBACKS.with(|c| c.set(callbacks));
    let hook: q::JSIsErrorHook = match callbacks.as_ref() {
        Some(callbacks) if callbacks.instanceClassIsError.is_some() => Some(is_dom_error),
        _ => None,
    };
    q::JS_SetIsErrorHook(Cx::from_raw(cx).rt, hook);
}

/// Patch Q27's hook: `Error.isError` asks `instanceClassIsError` about DOM objects, as in
/// SpiderMonkey.
unsafe extern "C" fn is_dom_error(_ctx: *mut q::JSContext, obj: q::JSValue) -> c_int {
    let callbacks = DOM_CALLBACKS.with(|c| c.get());
    let Some(is_error) = callbacks.as_ref().and_then(|c| c.instanceClassIsError) else {
        return 0;
    };
    let clasp = class::object_class(val_obj(obj));
    ((*clasp).flags & root::JSCLASS_IS_DOMJSCLASS != 0 && is_error(clasp)) as c_int
}

pub(crate) unsafe fn GetDOMCallbacks(cx: *mut root::JSContext) -> *const root::js::DOMCallbacks {
    let _ = cx;
    DOM_CALLBACKS.with(|c| c.get())
}

pub(crate) unsafe fn SetPreserveWrapperCallbacks(
    cx: *mut root::JSContext,
    preserveWrapper: root::js::PreserveWrapperCallback,
    hasReleasedWrapper: root::js::HasReleasedWrapperCallback,
) {
    let _ = (cx, preserveWrapper, hasReleasedWrapper);
}

pub(crate) unsafe fn SetRealmNameCallback(
    cx: *mut root::JSContext,
    callback: root::JS::RealmNameCallback,
) {
    let _ = (cx, callback);
}

pub(crate) unsafe fn SetDestroyRealmCallback(
    cx: *mut root::JSContext,
    callback: root::JS::DestroyRealmCallback,
) {
    let _ = (cx, callback);
}

pub(crate) unsafe fn JS_SetDestroyCompartmentCallback(
    cx: *mut root::JSContext,
    callback: root::JSDestroyCompartmentCallback,
) {
    let _ = (cx, callback);
}

pub(crate) unsafe fn RealmCreationOptionsRef(
    realm: *mut root::JS::Realm,
) -> *const root::JS::RealmCreationOptions {
    &(*(realm as *mut RealmData)).creation_options
}

pub(crate) unsafe fn RealmCreationOptionsRef1(
    cx: *mut root::JSContext,
) -> *const root::JS::RealmCreationOptions {
    RealmCreationOptionsRef(Cx::from_raw(cx).current_realm() as *mut root::JS::Realm)
}

pub(crate) unsafe fn RealmBehaviorsRef(
    realm: *mut root::JS::Realm,
) -> *const root::JS::RealmBehaviors {
    &(*(realm as *mut RealmData)).behaviors
}

pub(crate) unsafe fn RealmBehaviorsRef1(
    cx: *mut root::JSContext,
) -> *const root::JS::RealmBehaviors {
    RealmBehaviorsRef(Cx::from_raw(cx).current_realm() as *mut root::JS::Realm)
}

// Globals: lazy properties from the class's resolve and enumerate hooks.

static RESOLVE_EXOTIC: q::JSClassExoticMethods = q::JSClassExoticMethods {
    get_own_property: Some(resolve_own_property),
    get_own_property_names: Some(resolve_all),
    delete_property: Some(resolve_delete_property),
    define_own_property: None,
    has_property: None,
    get_property: None,
    set_property: None,
    get_prototype: None,
    set_prototype: None,
    is_extensible: None,
    prevent_extensions: None,
};

/// Exotic methods for SpiderMonkey classes with resolve or enumerate hooks (globals), or null.
pub(crate) unsafe fn resolve_exotic_methods(
    clasp: *const root::JSClass,
) -> *mut q::JSClassExoticMethods {
    let ops = (*clasp).cOps;
    if ops.is_null()
        || ((*ops).resolve.is_none() && (*ops).newEnumerate.is_none() && (*ops).enumerate.is_none())
    {
        return ptr::null_mut();
    }
    &RESOLVE_EXOTIC as *const _ as *mut _
}

thread_local! {
    /// Properties being resolved, so that the resolve hook's own lookups don't recurse.
    static RESOLVING: RefCell<Vec<(usize, q::JSAtom)>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` in the realm of the object `obj` of an SpiderMonkey class.
unsafe fn in_object_realm<R>(
    ctx: *mut q::JSContext,
    obj: *mut root::JSObject,
    f: impl FnOnce(*mut root::JSContext) -> R,
) -> R {
    let cx = cx_of(ctx);
    let realm = class::object_realm(cx, obj);
    cx.enter_realm(realm);
    let r = f(cx.as_raw());
    cx_of(ctx).leave_realm();
    r
}

unsafe extern "C" fn resolve_own_property(
    ctx: *mut q::JSContext,
    desc: *mut q::JSPropertyDescriptor,
    obj: q::JSValue,
    prop: q::JSAtom,
) -> c_int {
    let global = val_obj(obj);
    let ops = (*class::object_class(global)).cOps;
    let Some(resolve) = (*ops).resolve else {
        return 0;
    };
    let id = from_atom(prop);
    if let Some(may_resolve) = (*ops).mayResolve {
        if !may_resolve(ptr::null(), id, global) {
            return 0;
        }
    }
    let key = (global as usize, prop);
    if RESOLVING.with(|r| r.borrow().contains(&key)) {
        return 0;
    }
    RESOLVING.with(|r| r.borrow_mut().push(key));
    let mut resolved = false;
    let ok = in_object_realm(ctx, global, |cx| {
        resolve(cx, handle(&global), handle(&id), &mut resolved)
    });
    let r = if !ok {
        trap_failed(ctx);
        -1
    } else if resolved {
        q::JS_GetOwnProperty(ctx, desc, obj, prop)
    } else {
        0
    };
    RESOLVING.with(|r| r.borrow_mut().retain(|k| *k != key));
    r
}

/// Deletes a property that isn't an own property yet (QuickJS calls this hook only then): as
/// SpiderMonkey's lookup does, the resolve hook runs first. Otherwise a resolve hook that defines
/// a property only once (Servo's interface objects) would define a deleted property later.
unsafe extern "C" fn resolve_delete_property(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    prop: q::JSAtom,
) -> c_int {
    match resolve_own_property(ctx, ptr::null_mut(), obj, prop) {
        r if r < 0 => -1,
        0 => 1,
        _ => q::JS_DeleteProperty(ctx, obj, prop, 0),
    }
}

/// Resolves every lazy property (QuickJS reads the shape afterwards, patch Q1), so there are no
/// exotic keys.
unsafe extern "C" fn resolve_all(
    ctx: *mut q::JSContext,
    ptab: *mut *mut q::JSPropertyEnum,
    plen: *mut u32,
    obj: q::JSValue,
) -> c_int {
    *ptab = ptr::null_mut();
    *plen = 0;
    let global = val_obj(obj);
    let ops = (*class::object_class(global)).cOps;
    let ok = in_object_realm(ctx, global, |cx| {
        if let Some(new_enumerate) = (*ops).newEnumerate {
            let mut ids: Vec<root::jsid> = Vec::new();
            new_enumerate(cx, handle(&global), id_vector_handle(&mut ids), false)
                && ids
                    .iter()
                    .all(|id| q::JS_GetOwnProperty(ctx, ptr::null_mut(), obj, id.to_atom()) >= 0)
        } else if let Some(enumerate) = (*ops).enumerate {
            enumerate(cx, handle(&global))
        } else {
            true
        }
    });
    if ok {
        0
    } else {
        trap_failed(ctx);
        -1
    }
}

pub(crate) unsafe fn JS_NewEnumerateStandardClassesIncludingResolved(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    properties: root::JS::MutableHandleIdVector,
    enumerableOnly: bool,
) -> bool {
    let _ = (cx, obj, properties, enumerableOnly);
    true
}

/// A handle to `ids` for SpiderMonkey functions filling a `MutableHandleIdVector`.
fn id_vector_handle(ids: &mut Vec<root::jsid>) -> root::JS::MutableHandleIdVector {
    root::JS::MutableHandleIdVector {
        ptr: ids as *mut Vec<root::jsid> as *mut c_void,
    }
}
