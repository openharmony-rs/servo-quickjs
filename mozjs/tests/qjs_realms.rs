/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Realm lifetime: a realm is freed with its global once nothing outside the engine refers to
//! either, and stays alive while its functions are reachable or while it is entered.

use std::cell::Cell;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::glue::{
    CreateRustJSPrincipals, CreateWrapperProxyHandler, DestroyRustJSPrincipals,
    GetWindowProxyClass, JSPrincipalsCallbacks, NewWindowProxy, ProxyTraps,
};
use mozjs::jsapi::{
    self, CurrentGlobalOrNull, EnterRealm, GCContext, GCReason, GetCurrentRealmOrNull,
    GetObjectRealmOrNull, Heap, JSClass, JSClassOps, JSContext as RawJSContext, JSObject,
    JSPrincipals, JSTracer, JS_DropPrincipals, JS_GetFunctionObject, JS_HoldPrincipals,
    JS_InitDestroyPrincipalsCallback, JS_NewFunction, JS_TransplantObject, LeaveRealm,
    OnNewGlobalHookOption, Realm, SetWindowProxy, SetWindowProxyClass, Value,
    JSCLASS_GLOBAL_SLOT_COUNT, JSCLASS_IS_GLOBAL, JSCLASS_RESERVED_SLOTS_MASK,
    JSCLASS_RESERVED_SLOTS_SHIFT, JSPROP_ENUMERATE,
};
use mozjs::jsval::{BooleanValue, JSVal, ObjectValue, UndefinedValue};
use mozjs::quickjs::{drain_autorelease_pool, realm_count};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{JS_DefineProperty, JS_NewGlobalObject, JS_GC};
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject, JSEngine, JSEngineHandle, RealmOptions,
    Runtime,
};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

thread_local! {
    static TRACED: Cell<usize> = const { Cell::new(0) };
    static FINALIZED: Cell<usize> = const { Cell::new(0) };
    static PROXIES_FINALIZED: Cell<usize> = const { Cell::new(0) };
    static PRINCIPALS_DESTROYED: Cell<usize> = const { Cell::new(0) };
    static EXPECTED_REALM: Cell<*mut Realm> = const { Cell::new(ptr::null_mut()) };
}

fn count(counter: &'static std::thread::LocalKey<Cell<usize>>) -> usize {
    counter.with(Cell::get)
}

unsafe extern "C" fn trace_global(_trc: *mut JSTracer, _global: *mut JSObject) {
    TRACED.with(|c| c.set(c.get() + 1));
}

unsafe extern "C" fn finalize_global(_gcx: *mut GCContext, _global: *mut JSObject) {
    FINALIZED.with(|c| c.set(c.get() + 1));
}

static GLOBAL_CLASS_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize_global),
    call: None,
    construct: None,
    trace: Some(trace_global),
};

static GLOBAL_CLASS: JSClass = JSClass {
    name: c"RealmsGlobal".as_ptr(),
    flags: JSCLASS_IS_GLOBAL
        | ((JSCLASS_GLOBAL_SLOT_COUNT & JSCLASS_RESERVED_SLOTS_MASK)
            << JSCLASS_RESERVED_SLOTS_SHIFT),
    cOps: &GLOBAL_CLASS_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

unsafe extern "C" fn finalize_window_proxy(_gcx: *mut GCContext, _proxy: *mut JSObject) {
    PROXIES_FINALIZED.with(|c| c.set(c.get() + 1));
}

unsafe extern "C" fn destroy_principals(principals: *mut JSPrincipals) {
    PRINCIPALS_DESTROYED.with(|c| c.set(c.get() + 1));
    DestroyRustJSPrincipals(principals);
}

/// A native that returns whether it runs in `EXPECTED_REALM`.
unsafe extern "C" fn in_expected_realm(cx: *mut RawJSContext, _argc: u32, vp: *mut Value) -> bool {
    *vp = BooleanValue(GetCurrentRealmOrNull(cx) == EXPECTED_REALM.with(Cell::get));
    true
}

/// A native that returns a new global; the values it handed to Rust are released when it returns.
unsafe extern "C" fn make_global(cx: *mut RawJSContext, _argc: u32, vp: *mut Value) -> bool {
    let options = RealmOptions::default();
    let global = jsapi::JS_NewGlobalObject(
        cx,
        &GLOBAL_CLASS,
        ptr::null_mut(),
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*options,
    );
    *vp = ObjectValue(global);
    true
}

fn with_runtime(f: impl FnOnce(&mut JSContext)) {
    for counter in [
        &TRACED,
        &FINALIZED,
        &PROXIES_FINALIZED,
        &PRINCIPALS_DESTROYED,
    ] {
        counter.with(|c| c.set(0));
    }
    let mut runtime = Runtime::new(ENGINE.clone());
    f(runtime.cx());
}

unsafe fn new_global(cx: &mut JSContext, principals: *mut JSPrincipals) -> *mut JSObject {
    let options = RealmOptions::default();
    JS_NewGlobalObject(
        cx,
        &GLOBAL_CLASS,
        principals,
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*options,
    )
}

unsafe fn eval(cx: &mut JSContext, global: HandleObject, src: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_realms".to_owned(), 1);
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

/// Globals that must survive `collect` are kept in a `Heap`, as rooted values only live until the
/// autorelease pool is drained.
unsafe fn eval_in(cx: &mut JSContext, global: &Heap<*mut JSObject>, src: &str) -> JSVal {
    rooted!(&in(cx) let global = global.get());
    eval(cx, global.handle(), src)
}

unsafe fn eval_true_in(cx: &mut JSContext, global: &Heap<*mut JSObject>, src: &str) {
    rooted!(&in(cx) let global = global.get());
    eval_true(cx, global.handle(), src);
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

/// Releases everything the test's Rust code still holds, then collects cycles.
fn collect(cx: &mut JSContext) {
    drain_autorelease_pool();
    unsafe { JS_GC(cx, GCReason::API) };
}

#[test]
fn unreachable_globals_are_freed() {
    const GLOBALS: usize = 20;
    with_runtime(|cx| unsafe {
        JS_InitDestroyPrincipalsCallback(cx.raw_cx(), Some(destroy_principals));
        let callbacks = JSPrincipalsCallbacks {
            write: None,
            isSystemPrincipal: None,
            isAddonPrincipal: None,
        };
        let baseline = realm_count();
        for _ in 0..GLOBALS {
            let principals = CreateRustJSPrincipals(&callbacks, ptr::null_mut());
            JS_HoldPrincipals(principals);
            rooted!(&in(cx) let global = new_global(cx, principals));
            JS_DropPrincipals(cx.raw_cx(), principals);
            // Cycles through the global: functions, closures and objects of the realm.
            eval_true(
                cx,
                global.handle(),
                "var self = this; function f() { return self; } var o = { f, g: globalThis, h: () => o }; f() === this",
            );
        }
        assert_eq!(realm_count(), baseline + GLOBALS);
        assert_eq!(count(&FINALIZED), 0);
        collect(cx);
        assert!(count(&TRACED) > 0);
        assert_eq!(count(&FINALIZED), GLOBALS);
        assert_eq!(count(&PRINCIPALS_DESTROYED), GLOBALS);
        assert_eq!(realm_count(), baseline);
        collect(cx);
        assert_eq!(count(&FINALIZED), GLOBALS);
    });
    assert_eq!(count(&FINALIZED), GLOBALS);
}

#[test]
fn reachable_function_keeps_its_realm() {
    with_runtime(|cx| unsafe {
        let baseline = realm_count();
        let b = Heap::boxed(new_global(cx, ptr::null_mut()));
        {
            rooted!(&in(cx) let b = b.get());
            rooted!(&in(cx) let a = new_global(cx, ptr::null_mut()));
            EXPECTED_REALM.with(|r| r.set(GetObjectRealmOrNull(a.get())));
            let f = eval(
                cx,
                a.handle(),
                "var secret = 'A'; function f() { return secret + ':' + (globalThis.secret === 'A'); } f",
            );
            set_global(cx, b.handle(), c"scripted", f);
            let mut realm = AutoRealm::new_from_handle(cx, a.handle());
            let cx = &mut *realm;
            let native = JS_GetFunctionObject(JS_NewFunction(
                cx.raw_cx(),
                Some(in_expected_realm),
                0,
                0,
                c"native".as_ptr(),
            ));
            set_global(cx, b.handle(), c"native", ObjectValue(native));
        }
        collect(cx);
        assert_eq!(count(&FINALIZED), 0);
        assert_eq!(realm_count(), baseline + 2);
        eval_true_in(cx, &b, "scripted() === 'A:true' && native() === true");
        eval_true_in(
            cx,
            &b,
            "Object.getPrototypeOf(scripted) !== Function.prototype",
        );

        eval_in(cx, &b, "scripted = undefined; 0");
        collect(cx);
        assert_eq!(count(&FINALIZED), 0);
        eval_true_in(cx, &b, "native()");

        eval_in(cx, &b, "native = undefined; 0");
        collect(cx);
        assert_eq!(count(&FINALIZED), 1);
        assert_eq!(realm_count(), baseline + 1);

        b.set(ptr::null_mut());
        collect(cx);
        assert_eq!(count(&FINALIZED), 2);
        assert_eq!(realm_count(), baseline);
    });
    assert_eq!(count(&FINALIZED), 2);
}

#[test]
fn global_created_by_a_native_lives_while_referenced() {
    with_runtime(|cx| unsafe {
        let baseline = realm_count();
        let b = Heap::boxed(new_global(cx, ptr::null_mut()));
        {
            rooted!(&in(cx) let b = b.get());
            let mut realm = AutoRealm::new_from_handle(cx, b.handle());
            let cx = &mut *realm;
            let native = JS_GetFunctionObject(JS_NewFunction(
                cx.raw_cx(),
                Some(make_global),
                0,
                0,
                c"makeGlobal".as_ptr(),
            ));
            set_global(cx, b.handle(), c"makeGlobal", ObjectValue(native));
        }
        eval_true_in(cx, &b, "var g = makeGlobal(); g.Object !== Object");
        collect(cx);
        assert_eq!(count(&FINALIZED), 0);
        assert_eq!(realm_count(), baseline + 2);
        eval_true_in(
            cx,
            &b,
            "typeof g.Object === 'function' && g.Object !== Object",
        );

        eval_in(cx, &b, "g = undefined; 0");
        collect(cx);
        assert_eq!(count(&FINALIZED), 1);
        assert_eq!(realm_count(), baseline + 1);
    });
}

#[test]
fn window_proxy_is_freed_with_its_global() {
    with_runtime(|cx| unsafe {
        let baseline = realm_count();
        let traps = ProxyTraps {
            finalize: Some(finalize_window_proxy),
            ..Default::default()
        };
        let handler = CreateWrapperProxyHandler(&traps);
        SetWindowProxyClass(cx.raw_cx(), GetWindowProxyClass());
        {
            rooted!(&in(cx) let global = new_global(cx, ptr::null_mut()));
            {
                let mut realm = AutoRealm::new_from_handle(cx, global.handle());
                let cx = &mut *realm;
                rooted!(&in(cx) let wp = NewWindowProxy(cx.raw_cx(), global.handle().into(), handler));
                SetWindowProxy(cx.raw_cx(), global.handle().into(), wp.handle().into());
            }
            eval_true(
                cx,
                global.handle(),
                "var a = 1; var self = this; this.a === 1 && globalThis === self",
            );
        }
        collect(cx);
        assert_eq!(count(&FINALIZED), 1);
        assert_eq!(count(&PROXIES_FINALIZED), 1);
        assert_eq!(realm_count(), baseline);
    });
    assert_eq!(count(&FINALIZED), 1);
    assert_eq!(count(&PROXIES_FINALIZED), 1);
}

#[test]
fn navigated_window_proxy_frees_the_old_global() {
    with_runtime(|cx| unsafe {
        let baseline = realm_count();
        let traps = ProxyTraps {
            finalize: Some(finalize_window_proxy),
            ..Default::default()
        };
        let handler = CreateWrapperProxyHandler(&traps);
        SetWindowProxyClass(cx.raw_cx(), GetWindowProxyClass());
        let g2 = Heap::boxed(new_global(cx, ptr::null_mut()));
        let wp = Heap::boxed(ptr::null_mut::<JSObject>());
        {
            rooted!(&in(cx) let g1 = new_global(cx, ptr::null_mut()));
            {
                let mut realm = AutoRealm::new_from_handle(cx, g1.handle());
                let cx = &mut *realm;
                wp.set(NewWindowProxy(cx.raw_cx(), g1.handle().into(), handler));
                SetWindowProxy(cx.raw_cx(), g1.handle().into(), wp.handle());
            }
            eval_true(cx, g1.handle(), "var a = 1; this.a === 1");
            rooted!(&in(cx) let g2 = g2.get());
            let mut realm = AutoRealm::new_from_handle(cx, g2.handle());
            let cx = &mut *realm;
            rooted!(&in(cx) let new_proxy = NewWindowProxy(cx.raw_cx(), g2.handle().into(), handler));
            assert_eq!(
                JS_TransplantObject(cx.raw_cx(), wp.handle(), new_proxy.handle().into()),
                wp.get()
            );
            SetWindowProxy(cx.raw_cx(), g2.handle().into(), wp.handle());
        }
        // The old global and the proxy left with its old data are freed; the WindowProxy now
        // forwards to the second global.
        collect(cx);
        assert_eq!(count(&FINALIZED), 1);
        assert_eq!(count(&PROXIES_FINALIZED), 1);
        assert_eq!(realm_count(), baseline + 1);
        assert_eq!(eval_in(cx, &g2, "var c = 3; this").to_object(), wp.get());
        eval_true_in(cx, &g2, "this.c === 3 && !('a' in this)");

        wp.set(ptr::null_mut());
        g2.set(ptr::null_mut());
        collect(cx);
        assert_eq!(count(&FINALIZED), 2);
        assert_eq!(count(&PROXIES_FINALIZED), 2);
        assert_eq!(realm_count(), baseline);
    });
}

#[test]
fn entered_realm_is_kept() {
    with_runtime(|cx| unsafe {
        let baseline = realm_count();
        let global = new_global(cx, ptr::null_mut());
        let old = EnterRealm(cx.raw_cx(), global);
        collect(cx);
        assert_eq!(count(&FINALIZED), 0);
        assert_eq!(realm_count(), baseline + 1);
        assert_eq!(CurrentGlobalOrNull(cx.raw_cx()), global);
        {
            rooted!(&in(cx) let global = global);
            eval_true(cx, global.handle(), "var x = 2; x * 2 === 4");
        }
        LeaveRealm(cx.raw_cx(), old);
        collect(cx);
        assert_eq!(count(&FINALIZED), 1);
        assert_eq!(realm_count(), baseline);
    });
}

#[test]
fn realms_iterate_while_collecting() {
    unsafe extern "C" fn collect_and_count(
        cx: *mut RawJSContext,
        data: *mut c_void,
        _comp: *mut jsapi::Compartment,
    ) -> jsapi::CompartmentIterResult {
        drain_autorelease_pool();
        jsapi::JS_GC(cx, GCReason::API);
        *(data as *mut usize) += 1;
        jsapi::CompartmentIterResult::KeepGoing
    }
    with_runtime(|cx| unsafe {
        let baseline = realm_count();
        for _ in 0..3 {
            new_global(cx, ptr::null_mut());
        }
        let mut visited = 0usize;
        jsapi::JS_IterateCompartments(
            cx.raw_cx(),
            &mut visited as *mut usize as *mut c_void,
            Some(collect_and_count),
        );
        assert_eq!(visited, baseline + 3);
        collect(cx);
        assert_eq!(count(&FINALIZED), 3);
        assert_eq!(realm_count(), baseline);
    });
}
