/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Arbitrary JavaScript in a realm shaped like Servo's: a DOM-style proxy (`dom`), host objects
//! holding values in reserved slots (`host(v)`), structured clone (`clone(v, transfer)`), other
//! realms (`newGlobal()`), promise jobs, and `gc()` with the tracing verifier on.

#![no_main]

#[path = "common.rs"]
mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};
use std::sync::LazyLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use mozjs::context::JSContext;
use mozjs::conversions::jsstr_to_string;
use mozjs::glue::{
    CreateProxyHandler, GetProxyPrivate, GetProxyReservedSlot, NewProxyObject, ProxyTraps,
    SetProxyPrivate, SetProxyReservedSlot,
};
use mozjs::jsapi::{
    CallArgs, GCContext, GCReason, Handle, HandleId, HandleObject as RawHandleObject,
    HandleValue as RawHandleValue, JSClass, JSContext as RawJSContext, JSErrNum, JSObject,
    JSTracer, JS_DefineFunction, JS_DefinePropertyById, JS_DeletePropertyById,
    JS_GetOwnPropertyDescriptorById, JS_HasOwnPropertyById, JS_NewObjectWithGivenProto,
    MutableHandle, MutableHandleIdVector, MutableHandleValue as RawMutableHandleValue,
    ObjectOpResult, OnNewGlobalHookOption, PropertyDescriptor, PropertyKey, Value,
    JSCLASS_IS_PROXY, JSCLASS_RESERVED_SLOTS_MASK, JSCLASS_RESERVED_SLOTS_SHIFT, JSITER_HIDDEN,
    JSITER_OWNONLY, JSITER_SYMBOLS, JSPROP_ENUMERATE,
};
use mozjs::jsval::{Int32Value, ObjectValue, PrivateValue, UndefinedValue};
use mozjs::qjs;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    AppendToIdVector, GetPropertyKeys, GetRealmObjectPrototype, JS_AtomizeAndPinString,
    JS_DefineProperty, JS_NewGlobalObject, JS_GC, RUST_INTERNED_STRING_TO_JSID,
};
use mozjs::rust::{evaluate_script, CompileOptionsWrapper, RealmOptions, SIMPLE_GLOBAL_CLASS};

// The DOM-style proxy of the shim tests: named properties in a Rust map, others in an expando.

type Named = RefCell<BTreeMap<String, i32>>;

static EXTRA: u8 = 0;

unsafe fn context(cx: *mut RawJSContext) -> JSContext {
    JSContext::from_ptr(NonNull::new(cx).unwrap())
}

unsafe fn id_name(cx: *mut RawJSContext, id: HandleId) -> Option<String> {
    if !(*id).is_string() {
        return None;
    }
    Some(jsstr_to_string(
        &context(cx),
        NonNull::new((*id).to_string()).unwrap(),
    ))
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
    if !obj.is_null() {
        SetProxyPrivate(proxy, &ObjectValue(obj));
    }
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
        let v = (*desc).value_;
        named(*proxy)
            .borrow_mut()
            .insert(name, if v.is_int32() { v.to_int32() } else { 0 });
        (*result).code_ = 0;
        return true;
    }
    let exp = ensure_expando(cx, *proxy);
    if exp.is_null() {
        return false;
    }
    JS_DefinePropertyById(cx, Handle::from_marked_location(&exp), id, desc, result)
}

unsafe fn string_id(cx: &mut JSContext, name: &str) -> Option<PropertyKey> {
    let name = CString::new(name).ok()?;
    let s = JS_AtomizeAndPinString(cx, name.as_ptr());
    if s.is_null() {
        return None;
    }
    rooted!(&in(cx) let mut id = PropertyKey::default());
    RUST_INTERNED_STRING_TO_JSID(cx, s, id.handle_mut());
    Some(id.get())
}

unsafe extern "C" fn own_property_keys(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    props: MutableHandleIdVector,
) -> bool {
    let mut context = context(cx);
    let names: Vec<String> = named(*proxy).borrow().keys().cloned().collect();
    for name in names {
        let Some(id) = string_id(&mut context, &name) else {
            continue;
        };
        if !AppendToIdVector(props, mozjs::rust::Handle::from_marked_location(&id)) {
            return false;
        }
    }
    let exp = expando(*proxy);
    if exp.is_null() {
        return true;
    }
    GetPropertyKeys(
        &mut context,
        mozjs::rust::Handle::from_marked_location(&exp),
        JSITER_OWNONLY | JSITER_HIDDEN | JSITER_SYMBOLS,
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
    rooted!(in(cx) let mut proto = ptr::null_mut::<JSObject>());
    if !mozjs::jsapi::JS_GetPrototype(cx, proxy, proto.handle_mut().into()) {
        return false;
    }
    if proto.is_null() {
        *vp.ptr = UndefinedValue();
        return true;
    }
    mozjs::jsapi::JS_ForwardGetPropertyTo(cx, proto.handle().into(), id, receiver, vp)
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

unsafe extern "C" fn trace(_trc: *mut JSTracer, _proxy: *mut JSObject) {}

unsafe extern "C" fn finalize(_gcx: *mut GCContext, proxy: *mut JSObject) {
    let mut slot = UndefinedValue();
    GetProxyReservedSlot(proxy, 0, &mut slot);
    if !slot.is_undefined() {
        drop(Box::from_raw(slot.to_private() as *mut Named));
    }
}

struct Handler(*const c_void);
unsafe impl Send for Handler {}
unsafe impl Sync for Handler {}

static HANDLER: LazyLock<Handler> = LazyLock::new(|| {
    let traps = ProxyTraps {
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
    };
    Handler(unsafe { CreateProxyHandler(&traps, &EXTRA as *const u8 as *const c_void) })
});

static PROXY_CLASS: JSClass = JSClass {
    name: c"DomProxy".as_ptr(),
    flags: JSCLASS_IS_PROXY | ((3 & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT),
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

unsafe fn new_dom_proxy(cx: &mut JSContext) -> *mut JSObject {
    let proto = GetRealmObjectPrototype(cx);
    let undefined = UndefinedValue();
    let proxy = NewProxyObject(
        cx.raw_cx(),
        HANDLER.0,
        Handle::from_marked_location(&undefined),
        proto,
        &PROXY_CLASS,
        false,
    );
    if !proxy.is_null() {
        let map: Named = RefCell::new(BTreeMap::from([("n1".to_owned(), 1), ("n2".to_owned(), 2)]));
        SetProxyReservedSlot(
            proxy,
            0,
            &PrivateValue(Box::into_raw(Box::new(map)) as *const c_void),
        );
    }
    proxy
}

// Natives.

unsafe extern "C" fn gc(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    JS_GC(&mut context(cx), GCReason::API);
    args.rval().set(UndefinedValue());
    true
}

unsafe extern "C" fn host(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    let obj = common::new_host(
        cx,
        if argc > 0 {
            *args.get(0).ptr
        } else {
            UndefinedValue()
        },
    );
    args.rval().set(common::object_value(obj));
    !obj.is_null()
}

unsafe extern "C" fn clone(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    let v = if argc > 0 {
        *args.get(0).ptr
    } else {
        UndefinedValue()
    };
    let transfer = if argc > 1 {
        *args.get(1).ptr
    } else {
        UndefinedValue()
    };
    let mut context = context(cx);
    let Some(bytes) = common::write(&mut context, v, transfer) else {
        return false;
    };
    // Seeds for the clone_read target.
    if let Some(dir) = std::env::var_os("FUZZ_DUMP_CLONES") {
        let name = format!(
            "seed-{:016x}",
            bytes
                .iter()
                .fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64)
                    .wrapping_mul(0x100000001b3))
        );
        let _ = std::fs::write(std::path::Path::new(&dir).join(name), &bytes);
    }
    let Some(out) = common::read(&mut context, &bytes) else {
        return false;
    };
    args.rval().set(out);
    true
}

unsafe extern "C" fn dom(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    let proxy = new_dom_proxy(&mut context(cx));
    args.rval().set(common::object_value(proxy));
    !proxy.is_null()
}

unsafe extern "C" fn new_global(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    let mut context = context(cx);
    let global = JS_NewGlobalObject(
        &mut context,
        &SIMPLE_GLOBAL_CLASS,
        ptr::null_mut(),
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*RealmOptions::default(),
    );
    if !global.is_null() {
        define_natives(cx, global);
    }
    args.rval().set(common::object_value(global));
    !global.is_null()
}

unsafe fn define_natives(cx: *mut RawJSContext, global: *mut JSObject) {
    let global = Handle::from_marked_location(&global);
    JS_DefineFunction(cx, global, c"gc".as_ptr(), Some(gc), 0, 0);
    JS_DefineFunction(cx, global, c"host".as_ptr(), Some(host), 1, 0);
    JS_DefineFunction(cx, global, c"clone".as_ptr(), Some(clone), 2, 0);
    JS_DefineFunction(cx, global, c"dom".as_ptr(), Some(dom), 0, 0);
    JS_DefineFunction(cx, global, c"newGlobal".as_ptr(), Some(new_global), 0, 0);
}

/// Runs QuickJS's job queue (no host queue is installed), at most `limit` jobs.
unsafe fn run_jobs(cx: &mut JSContext, limit: usize) {
    let rt = mozjs::jsapi::JS_GetRuntime(cx.raw_cx()) as *mut qjs::JSRuntime;
    let mut ctx = ptr::null_mut();
    for _ in 0..limit {
        if qjs::JS_ExecutePendingJob(rt, &mut ctx) == 0 {
            break;
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    common::with_realm(|cx, global| unsafe {
        define_natives(cx.raw_cx(), global.get());
        let proxy = new_dom_proxy(cx);
        rooted!(&in(cx) let proxy = common::object_value(proxy));
        JS_DefineProperty(
            cx,
            global,
            c"document".as_ptr(),
            proxy.handle(),
            JSPROP_ENUMERATE as u32,
        );
        common::with_deadline(Duration::from_millis(200), || {
            rooted!(&in(cx) let mut rval = UndefinedValue());
            let options = CompileOptionsWrapper::new(cx, c"fuzz.js".to_owned(), 1);
            let _ = evaluate_script(cx, global, &source, rval.handle_mut(), options);
            mozjs::rust::wrappers2::JS_ClearPendingException(cx);
            run_jobs(cx, 10_000);
        });
    });
});
