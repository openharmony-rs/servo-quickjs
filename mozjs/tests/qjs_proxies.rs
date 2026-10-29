/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Proxies with `ProxyTraps` (DOM-style named properties and expandos), proxy slots and
//! finalization, the WindowProxy and `JS_TransplantObject`, globals with reserved slots, trace and
//! resolve hooks, the global `this`, and principals.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{c_void, CStr, CString};
use std::ptr::{self, NonNull};
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::conversions::jsstr_to_string;
use mozjs::glue::{
    CreateForwardingProxyHandler, CreateProxyHandler, CreateRustJSPrincipals,
    CreateWrapperProxyHandler, DestroyRustJSPrincipals, GetProxyHandler, GetProxyHandlerExtra,
    GetProxyHandlerFamily, GetProxyPrivate, GetProxyReservedSlot, GetRustJSPrincipalsPrivate,
    GetWindowProxyClass, IsProxyHandlerFamily, IsWrapper, JSPrincipalsCallbacks,
    JS_GetReservedSlot, NewProxyObject, NewWindowProxy, ProxyTraps, SetProxyPrivate,
    SetProxyReservedSlot, UncheckedUnwrapObject,
};
use mozjs::jsapi::{
    CheckedUnwrapStatic, Compartment, CompartmentIterResult, GCContext, GCReason,
    GetObjectRealmOrNull, GetRealmPrincipals, Handle, HandleId, HandleObject as RawHandleObject,
    HandleValue as RawHandleValue, IsWindowProxy, IsWindowSlow, JSClass, JSClassOps,
    JSContext as RawJSContext, JSErrNum, JSObject, JSPrincipals, JSTracer, JS_DefinePropertyById,
    JS_DeletePropertyById, JS_DropPrincipals, JS_GetOwnPropertyDescriptorById,
    JS_GlobalObjectTraceHook, JS_HasOwnPropertyById, JS_HoldPrincipals,
    JS_InitDestroyPrincipalsCallback, JS_IterateCompartments, JS_NewObjectWithGivenProto,
    JS_SetReservedSlot, JS_TransplantObject, JS_WrapObject, MutableHandle, MutableHandleIdVector,
    MutableHandleValue as RawMutableHandleValue, ObjectOpResult, OnNewGlobalHookOption,
    PropertyDescriptor, PropertyKey, SetWindowProxy, SetWindowProxyClass, ToWindowIfWindowProxy,
    ToWindowProxyIfWindowSlow, JSCLASS_GLOBAL_SLOT_COUNT, JSCLASS_IS_GLOBAL, JSCLASS_IS_PROXY,
    JSCLASS_RESERVED_SLOTS_MASK, JSCLASS_RESERVED_SLOTS_SHIFT, JSITER_HIDDEN, JSITER_OWNONLY,
    JSITER_SYMBOLS, JSPROP_ENUMERATE,
};
use mozjs::jsval::{Int32Value, JSVal, ObjectValue, PrivateValue, UndefinedValue};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    AppendToIdVector, GetPropertyKeys, GetRealmObjectPrototype, JS_AtomizeAndPinString,
    JS_DefineProperty, JS_GetProperty, JS_GetPropertyById, JS_NewGlobalObject, JS_NewPlainObject,
    JS_GC, RUST_INTERNED_STRING_TO_JSID,
};
use mozjs::rust::{
    evaluate_script, get_object_class, CompileOptionsWrapper, HandleObject, JSEngine,
    JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};

/// The engine can only be initialized once per process; it is shared by the tests.
static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

unsafe fn eval(cx: &mut JSContext, global: HandleObject, src: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_proxies".to_owned(), 1);
    assert!(
        evaluate_script(cx, global, src, rval.handle_mut(), options).is_ok(),
        "{src}"
    );
    rval.get()
}

unsafe fn eval_true(cx: &mut JSContext, global: HandleObject, src: &str) {
    let v = eval(cx, global, src);
    assert!(v.is_boolean() && v.to_boolean(), "{src}");
}

unsafe fn set_global(cx: &mut JSContext, global: HandleObject, name: &CStr, v: JSVal) {
    rooted!(&in(cx) let v = v);
    assert!(JS_DefineProperty(
        cx,
        global,
        name.as_ptr(),
        v.handle(),
        JSPROP_ENUMERATE as u32
    ));
}

unsafe fn get_int(cx: &mut JSContext, obj: *mut JSObject, name: &CStr) -> JSVal {
    rooted!(&in(cx) let obj = obj);
    rooted!(&in(cx) let mut v = UndefinedValue());
    assert!(JS_GetProperty(
        cx,
        obj.handle(),
        name.as_ptr(),
        v.handle_mut()
    ));
    v.get()
}

unsafe fn new_global(
    cx: &mut JSContext,
    clasp: &JSClass,
    options: &RealmOptions,
    principals: *mut JSPrincipals,
) -> *mut JSObject {
    JS_NewGlobalObject(
        cx,
        clasp,
        principals,
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &**options,
    )
}

unsafe fn string_id(cx: &mut JSContext, name: &str) -> PropertyKey {
    let name = CString::new(name).unwrap();
    let s = JS_AtomizeAndPinString(cx, name.as_ptr());
    rooted!(&in(cx) let mut id = PropertyKey::default());
    RUST_INTERNED_STRING_TO_JSID(cx, s, id.handle_mut());
    id.get()
}

unsafe fn id_name(cx: *mut RawJSContext, id: HandleId) -> Option<String> {
    if !(*id).is_string() {
        return None;
    }
    let cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    Some(jsstr_to_string(
        &cx,
        NonNull::new((*id).to_string()).unwrap(),
    ))
}

// A DOM-style proxy: named properties in a Rust map (reserved slot 0), other properties in an
// expando object (the private value), as Servo's generated proxy handlers do.

type Named = RefCell<BTreeMap<String, i32>>;

static EXTRA: u8 = 0;

thread_local! {
    static TRACED: Cell<usize> = const { Cell::new(0) };
    static FINALIZED: Cell<usize> = const { Cell::new(0) };
}

unsafe fn named(proxy: *mut JSObject) -> &'static Named {
    let mut slot = UndefinedValue();
    GetProxyReservedSlot(proxy, 0, &mut slot);
    &*(slot.to_private() as *const Named)
}

unsafe fn expando(proxy: *mut JSObject) -> *mut JSObject {
    let mut v = UndefinedValue();
    GetProxyPrivate(proxy, &mut v);
    if v.is_object() {
        v.to_object()
    } else {
        ptr::null_mut()
    }
}

unsafe fn ensure_expando(cx: *mut RawJSContext, proxy: *mut JSObject) -> *mut JSObject {
    let existing = expando(proxy);
    if !existing.is_null() {
        return existing;
    }
    let null = ptr::null_mut::<JSObject>();
    let obj = JS_NewObjectWithGivenProto(cx, ptr::null(), Handle::from_marked_location(&null));
    SetProxyPrivate(proxy, &ObjectValue(obj));
    obj
}

unsafe extern "C" fn get_own_property_descriptor(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
    desc: MutableHandle<PropertyDescriptor>,
    is_none: *mut bool,
) -> bool {
    let exp = expando(*proxy);
    if !exp.is_null() {
        if !JS_GetOwnPropertyDescriptorById(
            cx,
            Handle::from_marked_location(&exp),
            id,
            desc,
            is_none,
        ) {
            return false;
        }
        if !*is_none {
            return true;
        }
    }
    *is_none = true;
    if let Some(value) = id_name(cx, id).and_then(|name| named(*proxy).borrow().get(&name).copied())
    {
        let d = &mut *desc.ptr;
        *d = PropertyDescriptor::default();
        d.set_hasConfigurable_(true);
        d.set_configurable_(true);
        d.set_hasEnumerable_(true);
        d.set_enumerable_(true);
        d.set_hasWritable_(true);
        d.set_writable_(true);
        d.set_hasValue_(true);
        d.value_ = Int32Value(value);
        *is_none = false;
    }
    true
}

unsafe extern "C" fn define_property(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
    desc: Handle<PropertyDescriptor>,
    result: *mut ObjectOpResult,
) -> bool {
    let name = id_name(cx, id);
    if name.as_deref() == Some("frozen") {
        (*result).code_ = JSErrNum::JSMSG_READ_ONLY as usize;
        return true;
    }
    if let Some(name) = name.filter(|name| name.starts_with('n')) {
        named(*proxy)
            .borrow_mut()
            .insert(name, (*desc).value_.to_int32());
        (*result).code_ = 0;
        return true;
    }
    let exp = ensure_expando(cx, *proxy);
    JS_DefinePropertyById(cx, Handle::from_marked_location(&exp), id, desc, result)
}

unsafe extern "C" fn own_property_keys(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    props: MutableHandleIdVector,
) -> bool {
    let mut context = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let names: Vec<String> = named(*proxy).borrow().keys().cloned().collect();
    for name in names {
        let id = string_id(&mut context, &name);
        AppendToIdVector(props, mozjs::rust::Handle::from_marked_location(&id));
    }
    let exp = expando(*proxy);
    if exp.is_null() {
        return true;
    }
    let flags = JSITER_OWNONLY | JSITER_HIDDEN | JSITER_SYMBOLS;
    GetPropertyKeys(
        &mut context,
        mozjs::rust::Handle::from_marked_location(&exp),
        flags,
        props,
    )
}

unsafe extern "C" fn delete(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
    result: *mut ObjectOpResult,
) -> bool {
    if let Some(name) = id_name(cx, id) {
        if named(*proxy).borrow_mut().remove(&name).is_some() {
            (*result).code_ = 0;
            return true;
        }
    }
    let exp = expando(*proxy);
    if exp.is_null() {
        (*result).code_ = 0;
        return true;
    }
    JS_DeletePropertyById(cx, Handle::from_marked_location(&exp), id, result)
}

unsafe extern "C" fn get(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    receiver: RawHandleValue,
    id: HandleId,
    vp: RawMutableHandleValue,
) -> bool {
    let exp = expando(*proxy);
    if !exp.is_null() {
        let mut found = false;
        if !JS_HasOwnPropertyById(cx, Handle::from_marked_location(&exp), id, &mut found) {
            return false;
        }
        if found {
            return mozjs::jsapi::JS_ForwardGetPropertyTo(
                cx,
                Handle::from_marked_location(&exp),
                id,
                receiver,
                vp,
            );
        }
    }
    if let Some(value) = id_name(cx, id).and_then(|name| named(*proxy).borrow().get(&name).copied())
    {
        *vp.ptr = Int32Value(value);
        return true;
    }
    let mut proto = ptr::null_mut();
    if !mozjs::jsapi::JS_GetPrototype(cx, proxy, MutableHandle::from_marked_location(&mut proto)) {
        return false;
    }
    if proto.is_null() {
        *vp.ptr = UndefinedValue();
        return true;
    }
    mozjs::jsapi::JS_ForwardGetPropertyTo(
        cx,
        Handle::from_marked_location(&proto),
        id,
        receiver,
        vp,
    )
}

unsafe extern "C" fn prevent_extensions(
    _: *mut RawJSContext,
    _: RawHandleObject,
    result: *mut ObjectOpResult,
) -> bool {
    (*result).code_ = JSErrNum::JSMSG_CANT_PREVENT_EXTENSIONS as usize;
    true
}

unsafe extern "C" fn is_extensible(
    _: *mut RawJSContext,
    _: RawHandleObject,
    extensible: *mut bool,
) -> bool {
    *extensible = true;
    true
}

unsafe extern "C" fn trace(_trc: *mut JSTracer, _proxy: *mut JSObject) {
    TRACED.with(|c| c.set(c.get() + 1));
}

unsafe extern "C" fn finalize(_gcx: *mut GCContext, proxy: *mut JSObject) {
    FINALIZED.with(|c| c.set(c.get() + 1));
    let mut slot = UndefinedValue();
    GetProxyReservedSlot(proxy, 0, &mut slot);
    if !slot.is_undefined() {
        drop(Box::from_raw(slot.to_private() as *mut Named));
    }
}

fn dom_traps() -> ProxyTraps {
    ProxyTraps {
        getOwnPropertyDescriptor: Some(get_own_property_descriptor),
        defineProperty: Some(define_property),
        ownPropertyKeys: Some(own_property_keys),
        delete_: Some(delete),
        preventExtensions: Some(prevent_extensions),
        isExtensible: Some(is_extensible),
        get: Some(get),
        trace: Some(trace),
        finalize: Some(finalize),
        ..Default::default()
    }
}

const fn proxy_class(slots: u32) -> JSClass {
    JSClass {
        name: c"TestProxy".as_ptr(),
        flags: JSCLASS_IS_PROXY
            | ((slots & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT),
        cOps: ptr::null(),
        spec: ptr::null(),
        ext: ptr::null(),
        oOps: ptr::null(),
    }
}

static PROXY_CLASS: JSClass = proxy_class(3);

unsafe fn new_dom_proxy(
    cx: &mut JSContext,
    handler: *const c_void,
    names: &[(&str, i32)],
) -> *mut JSObject {
    let proto = GetRealmObjectPrototype(cx);
    let undefined = UndefinedValue();
    let proxy = NewProxyObject(
        cx.raw_cx(),
        handler,
        Handle::from_marked_location(&undefined),
        proto,
        &PROXY_CLASS,
        false,
    );
    assert!(!proxy.is_null());
    let map: Named = RefCell::new(names.iter().map(|(k, v)| (k.to_string(), *v)).collect());
    SetProxyReservedSlot(
        proxy,
        0,
        &PrivateValue(Box::into_raw(Box::new(map)) as *const c_void),
    );
    proxy
}

fn with_runtime(f: impl FnOnce(&mut JSContext)) {
    let mut runtime = Runtime::new(ENGINE.clone());
    f(runtime.cx());
}

#[test]
fn dom_proxy_traps() {
    with_runtime(|cx| unsafe {
        rooted!(&in(cx) let global = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), ptr::null_mut()));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        let handler = CreateProxyHandler(&dom_traps(), &EXTRA as *const u8 as *const c_void);
        rooted!(&in(cx) let o = new_dom_proxy(cx, handler, &[("n1", 1), ("n2", 2)]));
        set_global(cx, global.handle(), c"o", ObjectValue(o.get()));

        assert!(IsProxyHandlerFamily(o.get()));
        assert!(!IsWrapper(o.get()));
        assert_eq!(GetProxyHandler(o.get()), handler);
        assert_eq!(
            GetProxyHandlerExtra(o.get()),
            &EXTRA as *const u8 as *const c_void
        );
        assert!(!GetProxyHandlerFamily().is_null());
        assert_ne!((*get_object_class(o.get())).flags & JSCLASS_IS_PROXY, 0);

        // Named getter, `in`, keys and the prototype.
        eval_true(
            cx,
            global.handle(),
            "o.n1 === 1 && o.n2 === 2 && o.zz === undefined",
        );
        eval_true(
            cx,
            global.handle(),
            "'n1' in o && !('zz' in o) && 'toString' in o",
        );
        eval_true(cx, global.handle(), "Object.keys(o).join() === 'n1,n2'");
        eval_true(
            cx,
            global.handle(),
            "Object.getPrototypeOf(o) === Object.prototype && typeof o === 'object'",
        );
        eval_true(
            cx,
            global.handle(),
            "o.hasOwnProperty('n1') && !o.hasOwnProperty('toString')",
        );
        eval_true(cx, global.handle(), "String(o) === '[object Object]'");

        // Sets: expandos and named setters through defineProperty, then delete.
        eval_true(
            cx,
            global.handle(),
            "o.y = 5; o.y === 5 && o.hasOwnProperty('y')",
        );
        eval_true(cx, global.handle(), "o.n3 = 7; o.n3 === 7");
        assert_eq!(named(o.get()).borrow().get("n3"), Some(&7));
        assert!(!expando(o.get()).is_null());
        eval_true(
            cx,
            global.handle(),
            "Object.keys(o).join() === 'n1,n2,n3,y'",
        );
        eval_true(
            cx,
            global.handle(),
            "JSON.stringify(o) === '{\"n1\":1,\"n2\":2,\"n3\":7,\"y\":5}'",
        );
        eval_true(
            cx,
            global.handle(),
            "let keys = []; for (let k in o) keys.push(k); keys.join() === 'n1,n2,n3,y'",
        );
        eval_true(
            cx,
            global.handle(),
            "delete o.n1 && !('n1' in o) && o.n1 === undefined",
        );
        eval_true(cx, global.handle(), "delete o.y && !('y' in o)");

        // Rejected definitions: silent in sloppy mode, TypeError in strict mode.
        eval_true(cx, global.handle(), "o.frozen = 1; o.frozen === undefined");
        eval_true(cx, global.handle(), "(function() { 'use strict'; try { o.frozen = 1; } catch (e) { return e instanceof TypeError; } })()");
        eval_true(cx, global.handle(), "try { Object.defineProperty(o, 'frozen', { value: 1 }); false } catch (e) { e instanceof TypeError }");
        eval_true(
            cx,
            global.handle(),
            "Reflect.defineProperty(o, 'frozen', { value: 1 }) === false",
        );
        eval_true(
            cx,
            global.handle(),
            "Object.isExtensible(o) && Reflect.preventExtensions(o) === false",
        );

        // The traps are reachable from Rust too.
        let id = string_id(cx, "n2");
        rooted!(&in(cx) let id = id);
        rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
        let mut is_none = true;
        assert!(mozjs::glue::InvokeGetOwnPropertyDescriptor(
            handler,
            cx.raw_cx(),
            o.handle().into(),
            id.handle().into(),
            desc.handle_mut().into(),
            &mut is_none
        ));
        assert!(!is_none);
        assert_eq!(desc.get().value_.to_int32(), 2);
        rooted!(&in(cx) let mut v = UndefinedValue());
        assert!(JS_GetPropertyById(
            cx,
            o.handle(),
            id.handle(),
            v.handle_mut()
        ));
        assert_eq!(v.get().to_int32(), 2);
    });
}

thread_local! {
    static FORWARDED_DEFINES: Cell<usize> = const { Cell::new(0) };
}

/// Defines on the target, counting the calls.
unsafe extern "C" fn counting_define_property(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
    desc: Handle<PropertyDescriptor>,
    result: *mut ObjectOpResult,
) -> bool {
    FORWARDED_DEFINES.with(|c| c.set(c.get() + 1));
    let mut target = UndefinedValue();
    GetProxyPrivate(*proxy, &mut target);
    let target = target.to_object();
    JS_DefinePropertyById(cx, Handle::from_marked_location(&target), id, desc, result)
}

#[test]
fn forwarding_proxy_handler() {
    with_runtime(|cx| unsafe {
        rooted!(&in(cx) let global = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), ptr::null_mut()));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        let traps = ProxyTraps {
            defineProperty: Some(counting_define_property),
            ..Default::default()
        };
        let handler = CreateForwardingProxyHandler(&traps, &EXTRA as *const u8 as *const c_void);
        rooted!(&in(cx) let target = eval(cx, global.handle(), "[1, 2, 3]"));
        let proxy = NewProxyObject(
            cx.raw_cx(),
            handler,
            target.handle().into(),
            ptr::null_mut(),
            ptr::null(),
            true,
        );
        assert!(!proxy.is_null());
        rooted!(&in(cx) let p = proxy);
        set_global(cx, global.handle(), c"p", ObjectValue(p.get()));
        set_global(cx, global.handle(), c"t", target.get());

        // In the handler family, with its extra, and not a wrapper.
        assert!(IsProxyHandlerFamily(p.get()));
        assert!(!IsWrapper(p.get()));
        assert_eq!(
            GetProxyHandlerExtra(p.get()),
            &EXTRA as *const u8 as *const c_void
        );

        // Missing traps forward to the target.
        eval_true(
            cx,
            global.handle(),
            "p[0] === 1 && p.length === 3 && p[5] === undefined",
        );
        eval_true(cx, global.handle(), "1 in p && !(5 in p) && 'push' in p");
        eval_true(cx, global.handle(), "Object.keys(p).join() === '0,1,2'");
        eval_true(
            cx,
            global.handle(),
            "Object.getPrototypeOf(p) === Array.prototype",
        );
        eval_true(
            cx,
            global.handle(),
            "Object.getOwnPropertyDescriptor(p, 'length').value === 3",
        );
        eval_true(
            cx,
            global.handle(),
            "Object.isExtensible(p) && p.hasOwnProperty(2) && !p.hasOwnProperty(3)",
        );
        eval_true(cx, global.handle(), "delete p[2] && !(2 in t)");

        // A trap it has runs instead.
        FORWARDED_DEFINES.with(|c| c.set(0));
        eval_true(
            cx,
            global.handle(),
            "Object.defineProperty(p, 'x', { value: 7, configurable: true }); t.x === 7 && p.x === 7",
        );
        assert_eq!(FORWARDED_DEFINES.with(Cell::get), 1);
        eval_true(
            cx,
            global.handle(),
            "Object.preventExtensions(p); !Object.isExtensible(t)",
        );
    });
}

#[test]
fn proxy_slots_gc_and_finalize() {
    FINALIZED.with(|c| c.set(0));
    TRACED.with(|c| c.set(0));
    with_runtime(|cx| unsafe {
        rooted!(&in(cx) let global = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), ptr::null_mut()));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        let handler = CreateProxyHandler(&dom_traps(), ptr::null());
        {
            let proxy = new_dom_proxy(cx, handler, &[("n1", 1)]);
            let slot_obj = JS_NewPlainObject(cx);
            rooted!(&in(cx) let slot_obj = slot_obj);
            set_global(cx, slot_obj.handle(), c"v", Int32Value(42));
            SetProxyReservedSlot(proxy, 1, &ObjectValue(slot_obj.get()));
            let exp = ensure_expando(cx.raw_cx(), proxy);
            rooted!(&in(cx) let exp = exp);
            set_global(cx, exp.handle(), c"w", Int32Value(43));
            set_global(cx, global.handle(), c"p", ObjectValue(proxy));
        }
        // Only the slot, the private value and the global property keep these objects alive.
        mozjs::quickjs::drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert!(TRACED.with(|c| c.get()) > 0);

        let proxy = eval(cx, global.handle(), "p").to_object();
        let mut slot = UndefinedValue();
        GetProxyReservedSlot(proxy, 1, &mut slot);
        assert_eq!(get_int(cx, slot.to_object(), c"v").to_int32(), 42);
        eval_true(cx, global.handle(), "p.w === 43 && p.n1 === 1");
        assert_eq!(FINALIZED.with(|c| c.get()), 0);

        // A cycle through a reserved slot is collected, and finalized once.
        eval(
            cx,
            global.handle(),
            "var cyc = {}; cyc.p = p; p = undefined; 0",
        );
        let mut slot = UndefinedValue();
        GetProxyReservedSlot(proxy, 1, &mut slot);
        rooted!(&in(cx) let slot_obj = slot.to_object());
        let cyc = eval(cx, global.handle(), "cyc");
        set_global(cx, slot_obj.handle(), c"cyc", cyc);
        eval(cx, global.handle(), "cyc = undefined; 0");
        mozjs::quickjs::drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(FINALIZED.with(|c| c.get()), 1);
    });
    assert_eq!(FINALIZED.with(|c| c.get()), 1);
}

// The WindowProxy: a wrapper that forwards to its global, retargeted by transplanting.

unsafe extern "C" fn window_finalize(_gcx: *mut GCContext, _proxy: *mut JSObject) {}

#[test]
fn window_proxy_and_transplant() {
    with_runtime(|cx| unsafe {
        let traps = ProxyTraps {
            finalize: Some(window_finalize),
            ..Default::default()
        };
        let handler = CreateWrapperProxyHandler(&traps);
        SetWindowProxyClass(cx.raw_cx(), GetWindowProxyClass());

        rooted!(&in(cx) let g1 = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), ptr::null_mut()));
        rooted!(&in(cx) let g2 = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), ptr::null_mut()));
        rooted!(&in(cx) let mut wp = ptr::null_mut::<JSObject>());
        {
            let mut realm = AutoRealm::new_from_handle(cx, g1.handle());
            let cx = &mut *realm;
            wp.set(NewWindowProxy(cx.raw_cx(), g1.handle().into(), handler));
            SetProxyReservedSlot(wp.get(), 0, &PrivateValue(1 as *const c_void));
            // Servo keeps its cross-origin property holders in the second slot.
            SetProxyReservedSlot(wp.get(), 1, &UndefinedValue());
            SetWindowProxy(cx.raw_cx(), g1.handle().into(), wp.handle().into());
        }
        let wp = wp.get();
        assert!(IsWindowProxy(wp));
        assert!(IsWrapper(wp));
        assert!(!IsProxyHandlerFamily(wp));
        assert_eq!(UncheckedUnwrapObject(wp, false), g1.get());
        assert_eq!(UncheckedUnwrapObject(wp, true), wp);
        assert_eq!(CheckedUnwrapStatic(wp), g1.get());
        assert!(IsWindowSlow(g1.get()));
        assert!(!IsWindowSlow(g2.get()));
        assert_eq!(ToWindowProxyIfWindowSlow(g1.get()), wp);
        assert_eq!(ToWindowIfWindowProxy(wp), g1.get());
        rooted!(&in(cx) let mut wrapped = g1.get());
        assert!(JS_WrapObject(cx.raw_cx(), wrapped.handle_mut().into()));
        assert_eq!(wrapped.get(), wp);

        // Top-level and sloppy-mode `this` is the WindowProxy; properties forward to the global.
        assert_eq!(eval(cx, g1.handle(), "var a = 1; this").to_object(), wp);
        eval_true(
            cx,
            g1.handle(),
            "this === globalThis && this.a === 1 && (function() { return this; })() === this",
        );
        eval_true(
            cx,
            g1.handle(),
            "(0, eval)('this') === globalThis && Object.getPrototypeOf(this) === Object.prototype",
        );
        eval_true(cx, g1.handle(), "'a' in this && Object.keys(this).includes('a') && Object.getOwnPropertyDescriptor(this, 'a').value === 1");
        eval_true(
            cx,
            g1.handle(),
            "this.b = 2; b === 2 && delete this.b && typeof b === 'undefined'",
        );
        assert_eq!(get_int(cx, wp, c"a").to_int32(), 1);

        // Navigation: the same WindowProxy object now forwards to the second global.
        {
            let mut realm = AutoRealm::new_from_handle(cx, g2.handle());
            let cx = &mut *realm;
            let new_proxy = NewWindowProxy(cx.raw_cx(), g2.handle().into(), handler);
            SetProxyReservedSlot(new_proxy, 0, &PrivateValue(2 as *const c_void));
            rooted!(&in(cx) let wp_root = wp);
            rooted!(&in(cx) let new_proxy = new_proxy);
            let result = JS_TransplantObject(
                cx.raw_cx(),
                wp_root.handle().into(),
                new_proxy.handle().into(),
            );
            assert_eq!(result, wp);
            SetWindowProxy(cx.raw_cx(), g2.handle().into(), wp_root.handle().into());
            let mut slot = UndefinedValue();
            GetProxyReservedSlot(wp, 0, &mut slot);
            assert_eq!(slot.to_private(), 2 as *const c_void);
            GetProxyReservedSlot(new_proxy.get(), 0, &mut slot);
            assert_eq!(slot.to_private(), 1 as *const c_void);
            assert_eq!(UncheckedUnwrapObject(new_proxy.get(), false), g1.get());
        }
        assert_eq!(UncheckedUnwrapObject(wp, false), g2.get());
        assert_eq!(ToWindowProxyIfWindowSlow(g2.get()), wp);
        assert_eq!(eval(cx, g2.handle(), "var c = 3; this").to_object(), wp);
        assert_eq!(get_int(cx, wp, c"c").to_int32(), 3);
        assert!(get_int(cx, wp, c"a").is_undefined());
        // The old global's `this` keeps its identity and follows the navigation.
        assert_eq!(eval(cx, g1.handle(), "this").to_object(), wp);
        eval_true(cx, g1.handle(), "this.c === 3 && a === 1 && !('a' in this)");
    });
}

// A global class with reserved slots, a trace hook (via the realm options) and resolve hooks.

thread_local! {
    static GLOBAL_TRACED: Cell<usize> = const { Cell::new(0) };
    static GLOBAL_FINALIZED: Cell<usize> = const { Cell::new(0) };
    static RESOLVED: Cell<usize> = const { Cell::new(0) };
    static ONCE_DEFINED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" fn trace_global(_trc: *mut JSTracer, _global: *mut JSObject) {
    GLOBAL_TRACED.with(|c| c.set(c.get() + 1));
}

unsafe extern "C" fn finalize_global(_gcx: *mut GCContext, _global: *mut JSObject) {
    GLOBAL_FINALIZED.with(|c| c.set(c.get() + 1));
}

unsafe extern "C" fn resolve_global(
    cx: *mut RawJSContext,
    obj: RawHandleObject,
    id: HandleId,
    resolved: *mut bool,
) -> bool {
    *resolved = false;
    let name = id_name(cx, id);
    // Like Servo's interface objects, defined by the first resolution only.
    if name.as_deref() == Some("onceThing") {
        *resolved = true;
        if ONCE_DEFINED.with(|c| c.replace(true)) {
            return true;
        }
        let value = Int32Value(1);
        return mozjs::jsapi::JS_DefinePropertyById2(
            cx,
            obj,
            id,
            Handle::from_marked_location(&value),
            JSPROP_ENUMERATE as u32,
        );
    }
    if name.as_deref() != Some("lazyThing") {
        return true;
    }
    RESOLVED.with(|c| c.set(c.get() + 1));
    let value = Int32Value(42);
    if !mozjs::jsapi::JS_DefinePropertyById2(cx, obj, id, Handle::from_marked_location(&value), 0) {
        return false;
    }
    *resolved = true;
    true
}

unsafe extern "C" fn may_resolve_global(
    _names: *const mozjs::jsapi::JSAtomState,
    id: PropertyKey,
    _obj: *mut JSObject,
) -> bool {
    id.is_string()
}

unsafe extern "C" fn enumerate_global(
    cx: *mut RawJSContext,
    _obj: RawHandleObject,
    props: MutableHandleIdVector,
    _enumerable_only: bool,
) -> bool {
    let mut context = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let id = string_id(&mut context, "lazyThing");
    AppendToIdVector(props, mozjs::rust::Handle::from_marked_location(&id))
}

static GLOBAL_CLASS_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: Some(enumerate_global),
    resolve: Some(resolve_global),
    mayResolve: Some(may_resolve_global),
    finalize: Some(finalize_global),
    call: None,
    construct: None,
    trace: Some(JS_GlobalObjectTraceHook),
};

static GLOBAL_CLASS: JSClass = JSClass {
    name: c"TestGlobal".as_ptr(),
    flags: JSCLASS_IS_GLOBAL
        | (((JSCLASS_GLOBAL_SLOT_COUNT + 2) & JSCLASS_RESERVED_SLOTS_MASK)
            << JSCLASS_RESERVED_SLOTS_SHIFT),
    cOps: &GLOBAL_CLASS_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

#[test]
fn global_class_slots_trace_and_resolve() {
    GLOBAL_FINALIZED.with(|c| c.set(0));
    with_runtime(|cx| unsafe {
        let mut options = RealmOptions::default();
        options.creationOptions_.traceGlobal_ = Some(trace_global);
        rooted!(&in(cx) let global = new_global(cx, &GLOBAL_CLASS, &options, ptr::null_mut()));
        assert_eq!(
            get_object_class(global.get()),
            &GLOBAL_CLASS as *const JSClass
        );
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;

        let slot = JSCLASS_GLOBAL_SLOT_COUNT + 1;
        {
            let obj = JS_NewPlainObject(cx);
            rooted!(&in(cx) let obj = obj);
            set_global(cx, obj.handle(), c"x", Int32Value(42));
            JS_SetReservedSlot(global.get(), slot, &ObjectValue(obj.get()));
        }
        JS_SetReservedSlot(
            global.get(),
            JSCLASS_GLOBAL_SLOT_COUNT,
            &PrivateValue(7 as *const c_void),
        );
        mozjs::quickjs::drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert!(GLOBAL_TRACED.with(|c| c.get()) > 0);
        let mut v = UndefinedValue();
        JS_GetReservedSlot(global.get(), slot, &mut v);
        assert_eq!(get_int(cx, v.to_object(), c"x").to_int32(), 42);
        JS_GetReservedSlot(global.get(), JSCLASS_GLOBAL_SLOT_COUNT, &mut v);
        assert_eq!(v.to_private(), 7 as *const c_void);

        // Lazy properties: global variable access, enumeration, and `this` without a WindowProxy.
        assert_eq!(eval(cx, global.handle(), "this").to_object(), global.get());
        eval_true(
            cx,
            global.handle(),
            "lazyThing === 42 && this.lazyThing === 42 && 'lazyThing' in this",
        );
        eval_true(
            cx,
            global.handle(),
            "delete lazyThing && Object.getOwnPropertyNames(this).includes('lazyThing')",
        );
        eval_true(
            cx,
            global.handle(),
            "delete lazyThing && (function() { return lazyThing; })() === 42",
        );
        eval_true(cx, global.handle(), "typeof notLazy === 'undefined' && Object.keys(this).filter(k => k === 'lazyThing').length === 0");
        // Deleting a property that wasn't resolved yet resolves it first, so it stays deleted.
        eval_true(
            cx,
            global.handle(),
            "delete this.onceThing && typeof onceThing === 'undefined' && \
             Object.getOwnPropertyDescriptor(this, 'onceThing') === undefined",
        );
        assert!(RESOLVED.with(|c| c.get()) >= 3);
        assert_eq!(GLOBAL_FINALIZED.with(|c| c.get()), 0);
    });
    assert_eq!(GLOBAL_FINALIZED.with(|c| c.get()), 1);
}

// Principals and compartments.

thread_local! {
    static PRINCIPALS_DESTROYED: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn destroy_principals(principals: *mut JSPrincipals) {
    PRINCIPALS_DESTROYED.with(|c| c.set(c.get() + 1));
    drop(Box::from_raw(
        GetRustJSPrincipalsPrivate(principals) as *mut u32
    ));
    DestroyRustJSPrincipals(principals);
}

unsafe extern "C" fn count_compartment(
    _cx: *mut RawJSContext,
    data: *mut c_void,
    _comp: *mut Compartment,
) -> CompartmentIterResult {
    *(data as *mut usize) += 1;
    CompartmentIterResult::KeepGoing
}

#[test]
fn principals_and_compartments() {
    PRINCIPALS_DESTROYED.with(|c| c.set(0));
    with_runtime(|cx| unsafe {
        JS_InitDestroyPrincipalsCallback(cx.raw_cx(), Some(destroy_principals));
        let callbacks = JSPrincipalsCallbacks {
            write: None,
            isSystemPrincipal: None,
            isAddonPrincipal: None,
        };
        let principals =
            CreateRustJSPrincipals(&callbacks, Box::into_raw(Box::new(5u32)) as *mut c_void);
        JS_HoldPrincipals(principals);
        assert_eq!(*(GetRustJSPrincipalsPrivate(principals) as *const u32), 5);

        rooted!(&in(cx) let g1 = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), principals));
        rooted!(&in(cx) let _g2 = new_global(cx, &SIMPLE_GLOBAL_CLASS, &RealmOptions::default(), ptr::null_mut()));
        assert_eq!(
            GetRealmPrincipals(GetObjectRealmOrNull(g1.get())),
            principals
        );
        JS_DropPrincipals(cx.raw_cx(), principals);
        assert_eq!(PRINCIPALS_DESTROYED.with(|c| c.get()), 0);

        let mut count = 0usize;
        JS_IterateCompartments(
            cx.raw_cx(),
            &mut count as *mut usize as *mut c_void,
            Some(count_compartment),
        );
        assert_eq!(count, 2);
    });
    // The realm's reference is dropped with the runtime.
    assert_eq!(PRINCIPALS_DESTROYED.with(|c| c.get()), 1);
}
