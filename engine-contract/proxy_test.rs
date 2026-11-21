/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Proxy handlers, private values and reserved slots, the WindowProxy, principals,
//! compartments, DOM callbacks and dead wrappers.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use std::ptr::NonNull;

use js::glue::{
    CreateForwardingProxyHandler, CreateProxyHandler, CreateRustJSPrincipals,
    CreateWrapperProxyHandler, DeleteWrapperProxyHandler, DestroyRustJSPrincipals,
    GetCrossCompartmentWrapper, GetProxyHandler, GetProxyHandlerExtra, GetProxyHandlerFamily,
    GetProxyPrivate, GetProxyReservedSlot, GetRustJSPrincipalsPrivate, GetSecurityWrapper,
    GetWindowProxyClass, IsProxyHandlerFamily, IsWrapper, JSPrincipalsCallbacks, NewWindowProxy,
    ProxyTraps, SetProxyPrivate, SetProxyReservedSlot, UncheckedUnwrapObject, UnwrapObjectStatic,
};
use js::jsapi::{
    self, AutoRequireNoGC, CheckedUnwrapStatic, Compartment, CompartmentIterResult, DOMCallbacks,
    GetFirstGlobalInCompartment, GetObjectRealmOrNull, GetRealmPrincipals, IsWindowProxy,
    IsWindowSlow, JS_DropPrincipals, JS_GetOwnPropertyDescriptorById, JS_HasOwnPropertyById,
    JS_HoldPrincipals, JS_InitDestroyPrincipalsCallback, JS_InitReadPrincipalsCallback,
    JS_IsDeadWrapper, JS_NewGlobalObject, JS_TransplantObject, JS_WrapObject, JS_WrapValue,
    JSContext as RawJSContext, JSObject, JSPROP_ENUMERATE, JSPrincipals, OnNewGlobalHookOption,
    PropertyDescriptor, Realm, RealmBehaviorsRef, RealmBehaviorsRef1, RealmCreationOptionsRef,
    RealmCreationOptionsRef1, SetDOMCallbacks, SetDOMProxyInformation, SetRealmPrincipals,
    SetWindowProxy, SetWindowProxyClass, ToWindowIfWindowProxy, ToWindowProxyIfWindowSlow,
    UncheckedUnwrap, UnwrapOneCheckedStatic,
};
use js::jsval::{Int32Value, ObjectValue, PrivateValue, StringValue, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    CheckedUnwrapDynamic, InvokeGetOwnPropertyDescriptor, InvokeHasOwn, IterateRealmsInCompartment,
    JS_NewDeadWrapper, JS_NewPlainObject, JS_NewStringCopyN, JS_ValueToId, NewProxyObject,
    SetPreserveWrapperCallbacks, UnwrapObjectDynamic, UnwrapOneCheckedDynamic, WrapperNew,
};
use js::rust::{JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

use crate::support::{Observation, Value, eval, record, set_global, with_global};

const EXTRA: u8 = 0xa5;

unsafe extern "C" fn ignore_finalize(_gcx: *mut jsapi::GCContext, _proxy: *mut JSObject) {}

unsafe extern "C" fn forwarding_get_own_property_descriptor(
    cx: *mut RawJSContext,
    proxy: js::jsapi::HandleObject,
    id: js::jsapi::HandleId,
    desc: js::jsapi::MutableHandle<PropertyDescriptor>,
    is_none: *mut bool,
) -> bool {
    let mut private = UndefinedValue();
    GetProxyPrivate(*proxy.ptr, &mut private);
    if !private.is_object() {
        *is_none = true;
        return true;
    }
    let target = private.to_object();
    JS_GetOwnPropertyDescriptorById(
        cx,
        js::jsapi::Handle::from_marked_location(&target),
        id,
        desc,
        is_none,
    )
}

unsafe extern "C" fn forwarding_has_own(
    cx: *mut RawJSContext,
    proxy: js::jsapi::HandleObject,
    id: js::jsapi::HandleId,
    found: *mut bool,
) -> bool {
    let mut private = UndefinedValue();
    GetProxyPrivate(*proxy.ptr, &mut private);
    if !private.is_object() {
        *found = false;
        return true;
    }
    let target = private.to_object();
    JS_HasOwnPropertyById(
        cx,
        js::jsapi::Handle::from_marked_location(&target),
        id,
        found,
    )
}

fn handlers_private_and_slots(engine: &JSEngine) {
    let observation = with_global(engine, |cx, _global| unsafe {
        let traps = ProxyTraps::default();
        let extra = &EXTRA as *const u8 as *const c_void;
        let base = CreateProxyHandler(&traps, extra);
        let forwarding = CreateForwardingProxyHandler(&traps, extra);
        assert!(!base.is_null());
        assert!(!forwarding.is_null());
        assert!(!GetProxyHandlerFamily().is_null());

        rooted!(&in(cx) let target = JS_NewPlainObject(cx));
        assert!(!target.get().is_null());
        rooted!(&in(cx) let target_value = ObjectValue(target.get()));
        let proxy = NewProxyObject(
            cx,
            base,
            target_value.handle(),
            ptr::null_mut(),
            ptr::null(),
            false,
        );
        assert!(!proxy.is_null());
        rooted!(&in(cx) let proxy = proxy);

        assert_eq!(GetProxyHandler(proxy.get()), base);
        assert_eq!(GetProxyHandlerExtra(proxy.get()), extra);
        assert!(IsProxyHandlerFamily(proxy.get()));

        let mut private = UndefinedValue();
        GetProxyPrivate(proxy.get(), &mut private);
        assert!(private.is_object());
        assert_eq!(private.to_object(), target.get());
        let initial_private_is_target = private.to_object() == target.get();

        rooted!(&in(cx) let replacement = JS_NewPlainObject(cx));
        let replacement_value = ObjectValue(replacement.get());
        SetProxyPrivate(proxy.get(), &replacement_value);
        GetProxyPrivate(proxy.get(), &mut private);
        assert_eq!(private.to_object(), replacement.get());
        let replacement_private_is_replacement = private.to_object() == replacement.get();

        let first = Int32Value(11);
        let second = Int32Value(22);
        SetProxyReservedSlot(proxy.get(), 0, &first);
        SetProxyReservedSlot(proxy.get(), 1, &second);
        let mut slot = UndefinedValue();
        GetProxyReservedSlot(proxy.get(), 0, &mut slot);
        assert_eq!(slot.to_int32(), 11);
        GetProxyReservedSlot(proxy.get(), 1, &mut slot);
        assert_eq!(slot.to_int32(), 22);
        let slot_0_value = 11;
        let slot_1_value = 22;

        let forwarding_proxy = NewProxyObject(
            cx,
            forwarding,
            target_value.handle(),
            ptr::null_mut(),
            ptr::null(),
            false,
        );
        assert!(!forwarding_proxy.is_null());
        rooted!(&in(cx) let forwarding_proxy = forwarding_proxy);
        assert_eq!(GetProxyHandlerExtra(forwarding_proxy.get()), extra);
        assert!(IsProxyHandlerFamily(forwarding_proxy.get()));
        assert_ne!(GetProxyHandler(forwarding_proxy.get()), base);
        let forwarding_extra_matches = GetProxyHandlerExtra(forwarding_proxy.get()) == extra;
        let forwarding_handler_differs = GetProxyHandler(forwarding_proxy.get()) != base;
        let forwarding_is_family = IsProxyHandlerFamily(forwarding_proxy.get());

        let owned = CreateWrapperProxyHandler(&ProxyTraps {
            finalize: Some(ignore_finalize),
            ..Default::default()
        });
        assert!(!owned.is_null());
        DeleteWrapperProxyHandler(owned);
        Observation::new(vec![
            (
                "handler_initial_private_is_target",
                Value::Boolean(initial_private_is_target),
            ),
            (
                "handler_replacement_private_is_replacement",
                Value::Boolean(replacement_private_is_replacement),
            ),
            ("handler_slot_0", Value::Number(slot_0_value as f64)),
            ("handler_slot_1", Value::Number(slot_1_value as f64)),
            (
                "handler_forwarding_extra_matches",
                Value::Boolean(forwarding_extra_matches),
            ),
            (
                "handler_forwarding_handler_differs",
                Value::Boolean(forwarding_handler_differs),
            ),
            (
                "handler_forwarding_is_family",
                Value::Boolean(forwarding_is_family),
            ),
            ("handler_owned_created", Value::Boolean(true)),
        ])
    });
    record(
        "proxy.handlers_private_and_slots",
        &observation,
        &[
            "CreateForwardingProxyHandler",
            "CreateProxyHandler",
            "CreateWrapperProxyHandler",
            "DeleteWrapperProxyHandler",
            "GetProxyHandler",
            "GetProxyHandlerExtra",
            "GetProxyHandlerFamily",
            "GetProxyPrivate",
            "GetProxyReservedSlot",
            "IsProxyHandlerFamily",
            "JS_NewGlobalObject",
            "JS_NewPlainObject",
            "NewProxyObject",
            "SetProxyPrivate",
            "SetProxyReservedSlot",
        ],
    );
}

fn wrapper_create_and_unwrap(engine: &JSEngine) {
    let observation = with_global(engine, |cx, _global| unsafe {
        let traps = ProxyTraps::default();
        let wrapper = CreateWrapperProxyHandler(&traps);
        assert!(!wrapper.is_null());

        rooted!(&in(cx) let inner = JS_NewPlainObject(cx));
        rooted!(&in(cx) let inner_for_wrapper = ObjectValue(inner.get()));
        let proxy = NewProxyObject(
            cx,
            wrapper,
            inner_for_wrapper.handle(),
            ptr::null_mut(),
            ptr::null(),
            false,
        );
        assert!(!proxy.is_null());
        rooted!(&in(cx) let proxy = proxy);

        assert!(IsWrapper(proxy.get()));
        assert_eq!(UncheckedUnwrapObject(proxy.get(), false), inner.get());
        let mut flags: u32 = u32::MAX;
        assert_eq!(UncheckedUnwrap(proxy.get(), false, &mut flags), inner.get());
        assert_eq!(flags, 0);
        assert_eq!(CheckedUnwrapStatic(proxy.get()), inner.get());
        assert_eq!(CheckedUnwrapDynamic(proxy.get(), cx, false), inner.get());
        assert_eq!(UnwrapObjectStatic(proxy.get()), inner.get());
        assert_eq!(UnwrapObjectDynamic(proxy.get(), cx, false), inner.get());
        assert_eq!(
            UnwrapOneCheckedDynamic(proxy.handle().into(), cx, false),
            inner.get()
        );
        assert_eq!(UnwrapOneCheckedStatic(proxy.get()), inner.get());

        rooted!(&in(cx) let mut wrapped = proxy.get());
        assert!(JS_WrapObject(cx.raw_cx(), wrapped.handle_mut().into()));
        assert_eq!(wrapped.get(), proxy.get());
        rooted!(&in(cx) let mut wrapped_value = ObjectValue(proxy.get()));
        assert!(JS_WrapValue(cx.raw_cx(), wrapped_value.handle_mut().into()));
        assert_eq!(wrapped_value.get().to_object(), proxy.get());

        let cross = GetCrossCompartmentWrapper();
        let security = GetSecurityWrapper();
        assert!(!cross.is_null());
        assert!(!security.is_null());
        let copied = WrapperNew(cx, inner.handle().into(), wrapper, ptr::null());
        assert!(!copied.is_null());
        rooted!(&in(cx) let copied = copied);
        assert!(IsWrapper(copied.get()));
        assert_eq!(UncheckedUnwrapObject(copied.get(), false), inner.get());

        rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
        assert!(!IsWrapper(plain.get()));
        assert_eq!(UncheckedUnwrapObject(plain.get(), false), plain.get());
        assert_eq!(CheckedUnwrapStatic(plain.get()), plain.get());
        Observation::new(vec![
            ("proxy_kind", Value::String("wrapper".into())),
            ("is_wrapper", Value::Boolean(IsWrapper(proxy.get()))),
            (
                "unchecked_unwrap",
                Value::Boolean(UncheckedUnwrapObject(proxy.get(), false) == inner.get()),
            ),
            (
                "checked_unwrap",
                Value::Boolean(CheckedUnwrapStatic(proxy.get()) == inner.get()),
            ),
            (
                "one_unwrap",
                Value::Boolean(UnwrapOneCheckedStatic(proxy.get()) == inner.get()),
            ),
            (
                "wrap_object_identity",
                Value::Boolean(wrapped.get() == proxy.get()),
            ),
            (
                "wrap_value_identity",
                Value::Boolean(wrapped_value.get().to_object() == proxy.get()),
            ),
            (
                "wrapper_new",
                Value::Boolean(UncheckedUnwrapObject(copied.get(), false) == inner.get()),
            ),
            (
                "plain_unwrap_identity",
                Value::Boolean(UncheckedUnwrapObject(plain.get(), false) == plain.get()),
            ),
        ])
    });
    record(
        "proxy.wrapper_create_and_unwrap",
        &observation,
        &[
            "CheckedUnwrapDynamic",
            "CheckedUnwrapStatic",
            "CreateWrapperProxyHandler",
            "GetCrossCompartmentWrapper",
            "GetSecurityWrapper",
            "IsWrapper",
            "JS_NewGlobalObject",
            "JS_NewPlainObject",
            "JS_WrapObject",
            "JS_WrapValue",
            "NewProxyObject",
            "UncheckedUnwrap",
            "UncheckedUnwrapObject",
            "UnwrapObjectDynamic",
            "UnwrapObjectStatic",
            "UnwrapOneCheckedDynamic",
            "UnwrapOneCheckedStatic",
            "WrapperNew",
        ],
    );
}

fn window_proxy(engine: &JSEngine) {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    let observation = unsafe {
        let mut traps = ProxyTraps::default();
        traps.finalize = Some(ignore_finalize);
        let handler = CreateWrapperProxyHandler(&traps);
        assert!(!handler.is_null());
        SetWindowProxyClass(cx.raw_cx(), GetWindowProxyClass());

        rooted!(&in(cx) let g1 = JS_NewGlobalObject(
            cx.raw_cx(),
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        rooted!(&in(cx) let g2 = JS_NewGlobalObject(
            cx.raw_cx(),
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        rooted!(&in(cx) let mut window_proxy = ptr::null_mut::<JSObject>());
        let mut wrap_object_identity = false;
        let mut wrap_value_identity = false;
        {
            let mut realm = AutoRealm::new_from_handle(cx, g1.handle());
            let (global, cx) = realm.global_and_reborrow();
            window_proxy.set(NewWindowProxy(cx.raw_cx(), global.into(), handler));
            assert!(!window_proxy.get().is_null());
            SetProxyReservedSlot(window_proxy.get(), 0, &PrivateValue(1 as *const c_void));
            SetProxyReservedSlot(window_proxy.get(), 1, &UndefinedValue());
            SetWindowProxy(cx.raw_cx(), global.into(), window_proxy.handle().into());

            // Wrapping resolves against the current compartment, so it must run in g1's realm.
            rooted!(&in(cx) let mut wrapped = global.get());
            assert!(JS_WrapObject(cx.raw_cx(), wrapped.handle_mut().into()));
            assert_eq!(wrapped.get(), window_proxy.get());
            wrap_object_identity = wrapped.get() == window_proxy.get();
            rooted!(&in(cx) let mut wrapped_value = ObjectValue(global.get()));
            assert!(JS_WrapValue(cx.raw_cx(), wrapped_value.handle_mut().into()));
            assert_eq!(wrapped_value.get().to_object(), window_proxy.get());
            wrap_value_identity = wrapped_value.get().to_object() == window_proxy.get();

            set_global(cx, global, c"contract_a", Int32Value(7));
            assert!(eval(cx, global, "this.contract_a === 7 && globalThis === this").to_boolean());
        }
        let window_proxy_object = window_proxy.get();
        assert!(IsWindowProxy(window_proxy_object));
        assert!(IsWindowSlow(g1.get()));
        assert!(!IsWindowSlow(g2.get()));
        assert_eq!(ToWindowIfWindowProxy(window_proxy_object), g1.get());
        assert_eq!(ToWindowProxyIfWindowSlow(g1.get()), window_proxy_object);
        assert_eq!(ToWindowProxyIfWindowSlow(g2.get()), g2.get());
        Observation::new(vec![
            ("proxy_kind", Value::String("window-proxy".into())),
            (
                "is_window_proxy",
                Value::Boolean(IsWindowProxy(window_proxy_object)),
            ),
            ("window_slow", Value::Boolean(IsWindowSlow(g1.get()))),
            (
                "to_window",
                Value::Boolean(ToWindowIfWindowProxy(window_proxy_object) == g1.get()),
            ),
            (
                "to_window_proxy",
                Value::Boolean(ToWindowProxyIfWindowSlow(g1.get()) == window_proxy_object),
            ),
            ("wrap_object", Value::Boolean(wrap_object_identity)),
            ("wrap_value", Value::Boolean(wrap_value_identity)),
        ])
    };
    record(
        "proxy.window_proxy",
        &observation,
        &[
            "CreateWrapperProxyHandler",
            "GetWindowProxyClass",
            "IsWindowProxy",
            "IsWindowSlow",
            "JS_NewGlobalObject",
            "JS_WrapObject",
            "JS_WrapValue",
            "NewWindowProxy",
            "SetProxyReservedSlot",
            "SetWindowProxy",
            "SetWindowProxyClass",
            "ToWindowIfWindowProxy",
            "ToWindowProxyIfWindowSlow",
        ],
    );
}

fn transplant(engine: &JSEngine) {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    let observation = unsafe {
        let mut traps = ProxyTraps::default();
        traps.finalize = Some(ignore_finalize);
        let handler = CreateWrapperProxyHandler(&traps);
        SetWindowProxyClass(cx.raw_cx(), GetWindowProxyClass());

        rooted!(&in(cx) let g1 = JS_NewGlobalObject(
            cx.raw_cx(),
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        rooted!(&in(cx) let g2 = JS_NewGlobalObject(
            cx.raw_cx(),
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        rooted!(&in(cx) let mut old_proxy = ptr::null_mut::<JSObject>());
        {
            let mut realm = AutoRealm::new_from_handle(cx, g1.handle());
            let (global, cx) = realm.global_and_reborrow();
            old_proxy.set(NewWindowProxy(cx.raw_cx(), global.into(), handler));
            SetWindowProxy(cx.raw_cx(), global.into(), old_proxy.handle().into());
            set_global(cx, global, c"from_g1", Int32Value(11));
            assert!(eval(cx, global, "this.from_g1 === 11").to_boolean());
        }

        {
            let mut realm = AutoRealm::new_from_handle(cx, g2.handle());
            let (global, cx) = realm.global_and_reborrow();
            rooted!(&in(cx) let new_proxy = NewWindowProxy(cx.raw_cx(), global.into(), handler));
            let result = JS_TransplantObject(
                cx.raw_cx(),
                old_proxy.handle().into(),
                new_proxy.handle().into(),
            );
            // SpiderMonkey returns the object now in the target compartment; the tested contract
            // is that `old_proxy` keeps forwarding to its new target and its old proxy data.
            let transplanted = result;
            assert!(!transplanted.is_null());
            SetWindowProxy(cx.raw_cx(), global.into(), old_proxy.handle().into());

            assert_eq!(UncheckedUnwrapObject(old_proxy.get(), false), g2.get());
            assert_eq!(UncheckedUnwrapObject(transplanted, false), g2.get());

            set_global(cx, global, c"from_g2", Int32Value(22));
            assert!(
                eval(
                    cx,
                    global,
                    "this.from_g2 === 22 && this.from_g1 === undefined"
                )
                .to_boolean()
            );
            Observation::new(vec![
                ("transplant_target", Value::String("g2".into())),
                (
                    "result_unwraps_to_target",
                    Value::Boolean(UncheckedUnwrapObject(transplanted, false) == g2.get()),
                ),
                (
                    "old_proxy_retargeted",
                    Value::Boolean(UncheckedUnwrapObject(old_proxy.get(), false) == g2.get()),
                ),
                (
                    "navigation",
                    Value::Boolean(ToWindowProxyIfWindowSlow(g2.get()) == old_proxy.get()),
                ),
            ])
        }
    };
    record(
        "proxy.transplant",
        &observation,
        &[
            "CreateWrapperProxyHandler",
            "GetWindowProxyClass",
            "JS_NewGlobalObject",
            "JS_TransplantObject",
            "NewWindowProxy",
            "SetWindowProxy",
            "SetWindowProxyClass",
            "ToWindowProxyIfWindowSlow",
            "UncheckedUnwrapObject",
        ],
    );
}

thread_local! {
    static DESTROYED: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn destroy_principals(principals: *mut JSPrincipals) {
    DESTROYED.with(|count| count.set(count.get() + 1));
    let private = GetRustJSPrincipalsPrivate(principals) as *mut u32;
    if !private.is_null() {
        drop(Box::from_raw(private));
    }
    DestroyRustJSPrincipals(principals);
}

unsafe extern "C" fn is_system(_principals: *mut JSPrincipals) -> bool {
    false
}

unsafe extern "C" fn is_addon(_principals: *mut JSPrincipals) -> bool {
    false
}

fn principals(engine: &JSEngine) {
    DESTROYED.with(|count| count.set(0));
    let observation = with_global(engine, |cx, global| unsafe {
        JS_InitDestroyPrincipalsCallback(cx.raw_cx(), Some(destroy_principals));
        JS_InitReadPrincipalsCallback(cx.raw_cx(), None);
        let callbacks = JSPrincipalsCallbacks {
            write: None,
            isSystemPrincipal: Some(is_system),
            isAddonPrincipal: Some(is_addon),
        };
        let private = Box::into_raw(Box::new(0x51u32));
        let principals = CreateRustJSPrincipals(&callbacks, private as *mut c_void);
        assert!(!principals.is_null());
        assert_eq!(
            GetRustJSPrincipalsPrivate(principals),
            private as *mut c_void
        );
        let private_roundtrip = GetRustJSPrincipalsPrivate(principals) == private as *mut c_void;
        // A fresh principal has no reference. The test holds the only one.
        JS_HoldPrincipals(principals);

        let realm = GetObjectRealmOrNull(global.get());
        assert!(!realm.is_null());
        // The `*Ref` entries take the realm pointer and the `*Ref1` entries read the current realm
        // of a context: both must resolve to the same fields of that realm.
        assert_eq!(RealmBehaviorsRef(realm), RealmBehaviorsRef1(cx.raw_cx()));
        let behaviors_same_realm = RealmBehaviorsRef(realm) == RealmBehaviorsRef1(cx.raw_cx());
        assert_eq!(
            RealmCreationOptionsRef(realm),
            RealmCreationOptionsRef1(cx.raw_cx())
        );
        let creation_same_realm =
            RealmCreationOptionsRef(realm) == RealmCreationOptionsRef1(cx.raw_cx());
        assert!(GetRealmPrincipals(realm).is_null());
        SetRealmPrincipals(realm, principals);
        assert_eq!(GetRealmPrincipals(realm), principals);
        let realm_roundtrip = GetRealmPrincipals(realm) == principals;
        JS_DropPrincipals(cx.raw_cx(), principals);
        assert_eq!(DESTROYED.with(Cell::get), 0);

        SetRealmPrincipals(realm, ptr::null_mut());
        assert!(GetRealmPrincipals(realm).is_null());
        Observation::new(vec![
            ("principal_private", Value::String(format!("{}", 0x51))),
            ("private_roundtrip", Value::Boolean(private_roundtrip)),
            ("realm_roundtrip", Value::Boolean(realm_roundtrip)),
            ("refcount_release", Value::Boolean(true)),
            (
                "destroy_callback",
                Value::Boolean(DESTROYED.with(Cell::get) == 0),
            ),
            (
                "realm_refs",
                Value::Boolean(behaviors_same_realm && creation_same_realm),
            ),
        ])
    });
    assert_eq!(DESTROYED.with(Cell::get), 1);

    record(
        "proxy.principals",
        &observation,
        &[
            "CreateRustJSPrincipals",
            "DestroyRustJSPrincipals",
            "GetRealmPrincipals",
            "GetRustJSPrincipalsPrivate",
            "JS_DropPrincipals",
            "JS_HoldPrincipals",
            "JS_InitDestroyPrincipalsCallback",
            "JS_InitReadPrincipalsCallback",
            "JS_NewGlobalObject",
            "RealmBehaviorsRef",
            "RealmBehaviorsRef1",
            "RealmCreationOptionsRef",
            "RealmCreationOptionsRef1",
            "SetRealmPrincipals",
        ],
    );
}

thread_local! {
    static REALM_CALLBACKS: Cell<usize> = const { Cell::new(0) };
    static COMPARTMENT_CALLBACKS: Cell<usize> = const { Cell::new(0) };
    static EXPECTED_GLOBAL: Cell<*mut JSObject> = const { Cell::new(ptr::null_mut()) };
    static FOUND_GLOBAL: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" fn count_realm(
    _cx: *mut RawJSContext,
    _data: *mut c_void,
    _realm: *mut Realm,
    _nogc: *const AutoRequireNoGC,
) {
    REALM_CALLBACKS.with(|count| count.set(count.get() + 1));
}

unsafe extern "C" fn visit_compartment(
    cx: *mut RawJSContext,
    _data: *mut c_void,
    compartment: *mut Compartment,
) -> CompartmentIterResult {
    COMPARTMENT_CALLBACKS.with(|count| count.set(count.get() + 1));
    let global = GetFirstGlobalInCompartment(compartment);
    assert!(!global.is_null());
    if global == EXPECTED_GLOBAL.with(Cell::get) {
        FOUND_GLOBAL.with(|found| found.set(true));
        let cx = js::context::JSContext::from_ptr(NonNull::new(cx).expect("non-null context"));
        IterateRealmsInCompartment(&cx, compartment, ptr::null_mut(), Some(count_realm));
    }
    CompartmentIterResult::KeepGoing
}

fn dom_proxy(engine: &JSEngine) {
    let observation = with_global(engine, |cx, _global| unsafe {
        SetDOMProxyInformation(GetProxyHandlerFamily(), None, ptr::null());

        static CALLBACKS: DOMCallbacks = DOMCallbacks {
            instanceClassMatchesProto: None,
            instanceClassIsError: None,
            extractExceptionInfo: None,
        };
        SetDOMCallbacks(cx.raw_cx(), &CALLBACKS);
        let callbacks = js::rust::wrappers2::GetDOMCallbacks(cx);
        assert_eq!(callbacks, &CALLBACKS);
        SetDOMCallbacks(cx.raw_cx(), ptr::null());
        assert!(js::rust::wrappers2::GetDOMCallbacks(cx).is_null());

        let traps = ProxyTraps {
            getOwnPropertyDescriptor: Some(forwarding_get_own_property_descriptor),
            hasOwn: Some(forwarding_has_own),
            ..Default::default()
        };
        let handler = CreateForwardingProxyHandler(&traps, ptr::null());
        assert!(!handler.is_null());
        rooted!(&in(cx) let target = JS_NewPlainObject(cx));
        rooted!(&in(cx) let value = Int32Value(42));
        assert!(js::rust::wrappers2::JS_DefineProperty(
            cx,
            target.handle(),
            c"answer".as_ptr(),
            value.handle(),
            JSPROP_ENUMERATE as u32,
        ));
        rooted!(&in(cx) let target_value = ObjectValue(target.get()));
        let proxy = NewProxyObject(
            cx,
            handler,
            target_value.handle(),
            ptr::null_mut(),
            ptr::null(),
            false,
        );
        assert!(!proxy.is_null());
        rooted!(&in(cx) let proxy = proxy);

        rooted!(&in(cx) let name = JS_NewStringCopyN(cx, c"answer".as_ptr(), 6));
        rooted!(&in(cx) let value = StringValue(&*name.get()));
        rooted!(&in(cx) let mut id = js::jsapi::PropertyKey::default());
        assert!(JS_ValueToId(
            cx,
            value.handle().into(),
            id.handle_mut().into()
        ));
        rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
        let mut is_none = true;
        assert!(InvokeGetOwnPropertyDescriptor(
            handler,
            cx,
            proxy.handle(),
            id.handle(),
            desc.handle_mut(),
            &mut is_none,
        ));
        assert!(!is_none);
        assert_eq!(desc.get().value_.to_int32(), 42);
        let descriptor_value = desc.get().value_.to_int32();

        let mut found = false;
        assert!(InvokeHasOwn(
            handler,
            cx,
            proxy.handle(),
            id.handle(),
            &mut found,
        ));
        assert!(found);
        // Pair with SetDOMCallbacks above; the process-wide family stays as Servo sets it.
        SetDOMProxyInformation(GetProxyHandlerFamily(), None, ptr::null());
        Observation::new(vec![
            ("set_info", Value::Boolean(true)),
            ("callbacks_roundtrip", Value::Boolean(true)),
            ("descriptor", Value::Number(descriptor_value as f64)),
            ("has_own", Value::Boolean(found)),
        ])
    });
    record(
        "proxy.dom_callbacks",
        &observation,
        &[
            "CreateForwardingProxyHandler",
            "GetDOMCallbacks",
            "GetProxyHandlerFamily",
            "InvokeGetOwnPropertyDescriptor",
            "InvokeHasOwn",
            "JS_NewGlobalObject",
            "JS_NewPlainObject",
            "JS_NewStringCopyN",
            "JS_ValueToId",
            "NewProxyObject",
            "SetDOMCallbacks",
            "SetDOMProxyInformation",
        ],
    );
}

fn dead_wrapper(engine: &JSEngine) {
    let observation = with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let original = JS_NewPlainObject(cx));
        let dead = JS_NewDeadWrapper(cx, original.get());
        assert!(!dead.is_null());
        rooted!(&in(cx) let dead = dead);
        let dead_is_dead = JS_IsDeadWrapper(dead.get());
        let original_is_dead = JS_IsDeadWrapper(original.get());
        let dead_is_distinct = dead.get() != original.get();
        assert_eq!(dead_is_dead, true, "JS_IsDeadWrapper(dead wrapper)");
        assert_eq!(original_is_dead, false, "JS_IsDeadWrapper(original)");
        assert_eq!(
            dead_is_distinct, true,
            "JS_NewDeadWrapper returned the original object"
        );
        Observation::new(vec![
            (
                "dead_state",
                Value::String(if dead_is_dead { "dead" } else { "live" }.to_owned()),
            ),
            (
                "original_state",
                Value::String(if original_is_dead { "dead" } else { "live" }.to_owned()),
            ),
            (
                "distinct_objects",
                Value::String(if dead_is_distinct { "distinct" } else { "same" }.to_owned()),
            ),
        ])
    });
    record(
        "proxy.dead_wrapper",
        &observation,
        &[
            "JS_NewDeadWrapper",
            "JS_IsDeadWrapper",
            "JS_NewGlobalObject",
            "JS_NewPlainObject",
        ],
    );
}

// ---------------------------------------------------------------------------
// Trusted principals and preserve-wrapper callbacks.
// ---------------------------------------------------------------------------

unsafe extern "C" fn preserve_wrapper(
    _cx: *mut jsapi::JSContext,
    _obj: jsapi::HandleObject,
) -> bool {
    false
}

unsafe extern "C" fn has_released_wrapper(_obj: jsapi::HandleObject) -> bool {
    false
}

/// `SetPreserveWrapperCallbacks` is only invoked when SpiderMonkey preserves a cross-compartment
/// wrapper, i.e. when a DOM proxy or an Xray-wrapped object is exposed to script. None of that
/// exists in this harness, and the suite has no DOM, so neither callback can fire. The case pins
/// the registration and the fact that installing them is side-effect free.
fn wrapper_callbacks(engine: &JSEngine) -> Observation {
    with_global(engine, |cx, global| unsafe {
        SetPreserveWrapperCallbacks(cx, Some(preserve_wrapper), Some(has_released_wrapper));
        jsapi::AssertSameCompartment(cx.raw_cx(), global.get());
    });

    Observation::new(vec![
        ("preserve_wrapper_installed", Value::Boolean(true)),
        ("has_released_wrapper_installed", Value::Boolean(true)),
        ("same_compartment_after_setup", Value::Boolean(true)),
    ])
}

pub fn run(engine: &JSEngine) {
    crate::support::isolate_case("proxy.handlers_private_and_slots", || {
        handlers_private_and_slots(engine)
    });
    crate::support::isolate_case("proxy.wrapper_create_and_unwrap", || {
        wrapper_create_and_unwrap(engine)
    });
    crate::support::isolate_case("proxy.window_proxy", || window_proxy(engine));
    crate::support::isolate_case("proxy.transplant", || transplant(engine));
    crate::support::isolate_case("proxy.principals", || principals(engine));
    crate::support::isolate_case("proxy.dom_callbacks", || dom_proxy(engine));
    crate::support::isolate_case("proxy.dead_wrapper", || dead_wrapper(engine));
    crate::support::run_case(
        "proxy.wrapper_callbacks",
        &["SetPreserveWrapperCallbacks"],
        || wrapper_callbacks(engine),
    );
}
