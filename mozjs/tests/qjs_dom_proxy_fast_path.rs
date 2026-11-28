/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Once `SetDOMProxyInformation` is called, gets and sets of DOM proxy properties that resolve on
//! the prototype chain skip the traps, as SpiderMonkey's JIT does; the results must be the traps'.
//! (A separate test binary: the setting is process-wide.)

use std::cell::Cell;
use std::ffi::{c_void, CString};
use std::ptr::{self, NonNull};

use mozjs::context::JSContext;
use mozjs::conversions::jsstr_to_string;
use mozjs::glue::{
    CreateProxyHandler, GetProxyHandlerFamily, GetProxyPrivate, NewProxyObject, ProxyTraps,
    SetProxyPrivate,
};
use mozjs::jsapi::{
    Handle, HandleId, HandleObject as RawHandleObject, HandleValue as RawHandleValue, JSClass,
    JSContext as RawJSContext, JSObject, JS_DefinePropertyById, JS_ForwardGetPropertyTo,
    JS_GetOwnPropertyDescriptorById, JS_GetPrototype, JS_HasOwnPropertyById, JS_HasPropertyById,
    JS_NewObjectWithGivenProto, MutableHandle, MutableHandleValue as RawMutableHandleValue,
    ObjectOpResult, OnNewGlobalHookOption, PropertyDescriptor, SetDOMProxyInformation,
    JSCLASS_IS_PROXY,
};
use mozjs::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{JS_NewGlobalObject, JS_SetProperty};
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS,
};

static EXTRA: u8 = 0;

thread_local! {
    static TRAPS: Cell<usize> = const { Cell::new(0) };
}

fn traps() -> usize {
    TRAPS.with(Cell::get)
}

fn count() {
    TRAPS.with(|c| c.set(c.get() + 1));
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

/// The named properties: `named1` and `named2`, the latter also on the prototype.
unsafe fn named(cx: *mut RawJSContext, id: HandleId) -> Option<i32> {
    if !(*id).is_string() {
        return None;
    }
    let cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    match jsstr_to_string(&cx, NonNull::new((*id).to_string()).unwrap()).as_str() {
        "named1" => Some(1),
        "named2" => Some(2),
        _ => None,
    }
}

unsafe fn has_on_proto(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
) -> Option<bool> {
    let mut proto = ptr::null_mut();
    if !JS_GetPrototype(cx, proxy, MutableHandle::from_marked_location(&mut proto)) {
        return None;
    }
    let mut found = false;
    if !proto.is_null()
        && !JS_HasPropertyById(cx, Handle::from_marked_location(&proto), id, &mut found)
    {
        return None;
    }
    Some(found)
}

/// Servo's order: the expando, indices, the prototype, then named properties.
unsafe extern "C" fn get(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    receiver: RawHandleValue,
    id: HandleId,
    vp: RawMutableHandleValue,
) -> bool {
    count();
    let exp = expando(*proxy);
    if !exp.is_null() {
        let mut found = false;
        if !JS_HasOwnPropertyById(cx, Handle::from_marked_location(&exp), id, &mut found) {
            return false;
        }
        if found {
            return JS_ForwardGetPropertyTo(
                cx,
                Handle::from_marked_location(&exp),
                id,
                receiver,
                vp,
            );
        }
    }
    if (*id).is_int() {
        *vp.ptr = Int32Value(100 + (*id).to_int());
        return true;
    }
    let Some(on_proto) = has_on_proto(cx, proxy, id) else {
        return false;
    };
    if on_proto {
        let mut proto = ptr::null_mut();
        JS_GetPrototype(cx, proxy, MutableHandle::from_marked_location(&mut proto));
        return JS_ForwardGetPropertyTo(cx, Handle::from_marked_location(&proto), id, receiver, vp);
    }
    *vp.ptr = match named(cx, id) {
        Some(value) => Int32Value(value),
        None => UndefinedValue(),
    };
    true
}

unsafe extern "C" fn get_own_property_descriptor(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
    desc: mozjs::jsapi::MutableHandle<PropertyDescriptor>,
    is_none: *mut bool,
) -> bool {
    count();
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
    true
}

unsafe extern "C" fn define_property(
    cx: *mut RawJSContext,
    proxy: RawHandleObject,
    id: HandleId,
    desc: Handle<PropertyDescriptor>,
    result: *mut ObjectOpResult,
) -> bool {
    count();
    let mut exp = expando(*proxy);
    if exp.is_null() {
        let null = ptr::null_mut::<JSObject>();
        exp = JS_NewObjectWithGivenProto(cx, ptr::null(), Handle::from_marked_location(&null));
        SetProxyPrivate(*proxy, &ObjectValue(exp));
    }
    JS_DefinePropertyById(cx, Handle::from_marked_location(&exp), id, desc, result)
}

static PROXY_CLASS: JSClass = JSClass {
    name: c"FastPathProxy".as_ptr(),
    flags: JSCLASS_IS_PROXY,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

unsafe fn eval(cx: &mut JSContext, global: HandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"test.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
    rval.get()
}

unsafe fn eval_true(cx: &mut JSContext, global: HandleObject, source: &str) {
    assert!(eval(cx, global, source).to_boolean(), "{source}");
}

unsafe fn set_global(cx: &mut JSContext, global: HandleObject, name: &str, value: JSVal) {
    rooted!(&in(cx) let value = value);
    let name = CString::new(name).unwrap();
    assert!(JS_SetProperty(cx, global, name.as_ptr(), value.handle()));
}

#[test]
fn unshadowed_names_resolve_on_the_prototype() {
    let engine = JSEngine::init().unwrap();
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        SetDOMProxyInformation(GetProxyHandlerFamily(), None, ptr::null());
        rooted!(&in(cx) let global = JS_NewGlobalObject(cx, &SIMPLE_GLOBAL_CLASS, ptr::null_mut(), OnNewGlobalHookOption::FireOnNewGlobalHook, &*RealmOptions::default()));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let (global, cx) = realm.global_and_reborrow();

        rooted!(&in(cx) let proto = eval(cx, global, "globalThis.log = []; \
            ({ get acc() { return this; }, set s(v) { log.push(v, this === o); }, d: 'proto', named2: 'proto', 0: 'proto0' })").to_object());
        let traps_ = ProxyTraps {
            get: Some(get),
            getOwnPropertyDescriptor: Some(get_own_property_descriptor),
            defineProperty: Some(define_property),
            ..Default::default()
        };
        let undefined = UndefinedValue();
        let new_proxy = |cx: &mut JSContext, extra: *const c_void| {
            let handler = CreateProxyHandler(&traps_, extra);
            NewProxyObject(
                cx.raw_cx(),
                handler,
                Handle::from_marked_location(&undefined),
                proto.get(),
                &PROXY_CLASS,
                false,
            )
        };
        rooted!(&in(cx) let o = new_proxy(cx, &EXTRA as *const u8 as *const c_void));
        set_global(cx, global, "o", ObjectValue(o.get()));

        // Prototype accessors and data, with the proxy as receiver: no trap.
        let before = traps();
        eval_true(
            cx,
            global,
            "o.acc === o && o.d === 'proto' && o.named2 === 'proto' && typeof o.toString === 'function'",
        );
        eval_true(cx, global, "o.s = 7; log.join() === '7,true'");
        assert_eq!(traps(), before);

        // Indices, named properties and missing names go to the traps.
        eval_true(
            cx,
            global,
            "o[0] === 100 && o.named1 === 1 && o.zz === undefined",
        );
        assert!(traps() > before);

        // An expando shadows the prototype.
        eval_true(cx, global, "o.x = 1; o.x === 1");
        eval_true(
            cx,
            global,
            "Object.defineProperty(o, 'd', { value: 'expando', configurable: true }); o.d === 'expando'",
        );
        eval_true(
            cx,
            global,
            "Object.defineProperty(o, 's', { value: 0, writable: true, configurable: true }); o.s = 8; o.s === 8 && log.length === 2",
        );

        // Cached prototype lookups see every change to the prototype chain.
        set_global(cx, global, "P", ObjectValue(proto.get()));
        eval_true(
            cx,
            global,
            "o.zz === undefined && o.named1 === 1 && o.zz === undefined",
        );
        eval_true(
            cx,
            global,
            "P.zz = 5; P.named1 = 'p'; o.zz === 5 && o.named1 === 'p'",
        );
        eval_true(
            cx,
            global,
            "delete P.zz; delete P.named1; o.zz === undefined && o.named1 === 1",
        );
        eval_true(cx, global, "Object.prototype.yy = 3; o.yy === 3");
        eval_true(cx, global, "delete Object.prototype.yy; o.yy === undefined");
        eval_true(
            cx,
            global,
            "const old = Object.getPrototypeOf(P); const r0 = o.ww === undefined && o.named1 === 1; \
            Object.setPrototypeOf(P, { ww: 1, named1: 'q' }); const r1 = o.ww === 1 && o.named1 === 'q'; \
            Object.setPrototypeOf(P, old); r0 && r1 && o.ww === undefined && o.named1 === 1",
        );
        eval_true(
            cx,
            global,
            "Object.defineProperty(Object.prototype, 'vv', { get() { return this; }, configurable: true }); \
            const r2 = o.vv === o; delete Object.prototype.vv; r2 && o.vv === undefined",
        );

        // An exotic object in the chain: the traps look the name up, once.
        eval_true(
            cx,
            global,
            "const hasLog = []; const old2 = Object.getPrototypeOf(P); \
            Object.setPrototypeOf(P, new Proxy({ pp: 1 }, { has(t, k) { hasLog.push(k); return k in t; } })); \
            const r3 = o.qq === undefined && o.pp === 1 && hasLog.join() === 'qq,pp' && o.acc === o; \
            Object.setPrototypeOf(P, old2); r3 && hasLog.length === 2",
        );

        // Array indices on the prototype go to the traps, which try the indexed getter first.
        eval_true(cx, global, "P['3000000000'] = 'pi'; P['01'] = 'p01'; true");
        for _ in 0..2 {
            let before = traps();
            eval_true(cx, global, "o['01'] === 'p01'");
            assert_eq!(traps(), before);
            eval_true(cx, global, "o['3000000000'] === 'pi'");
            assert!(traps() > before);
        }

        // Proxies without DOM class (the window's named properties object) keep their traps.
        rooted!(&in(cx) let p = new_proxy(cx, ptr::null()));
        set_global(cx, global, "p", ObjectValue(p.get()));
        let before = traps();
        eval_true(cx, global, "p.acc === p");
        assert!(traps() > before);
    }
}
