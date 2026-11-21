/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Realm entry/exit, global lookup, private storage and trace-hook contract.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;

use js::glue::{CreateRustJSPrincipals, JSPrincipalsCallbacks};
use js::jsapi::{
    self, GCReason, JS_InitDestroyPrincipalsCallback, JSPrincipals, JSTracer, OnNewGlobalHookOption,
};
use js::rooted;
use js::rust::wrappers2::{JS_GC, JS_NewGlobalObject};
use js::rust::{JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

use crate::support::{Observation, Value, record};

thread_local! {
    /// How many times the realm's global trace hook ran.
    static GLOBAL_TRACE_HOOK_CALLS: Cell<u32> = const { Cell::new(0) };
}

unsafe extern "C" fn trace_global(_trc: *mut JSTracer, _global: *mut jsapi::JSObject) {
    GLOBAL_TRACE_HOOK_CALLS.with(|calls| calls.set(calls.get() + 1));
}

// ---------------------------------------------------------------------------
// Compartment iteration and flags.
// ---------------------------------------------------------------------------

/// One pass over every compartment of the context, counting the flags each one reports.
///
/// Only the system count is recorded: the shim iterates every realm of the process, so the total
/// depends on which cases ran before this one, while a system compartment stays a property of the
/// context that created it.
#[derive(Default)]
struct CompartmentFlags {
    total: u32,
    system: u32,
    sharable: u32,
}

unsafe extern "C" fn count_compartment(
    _cx: *mut jsapi::JSContext,
    data: *mut c_void,
    comp: *mut jsapi::Compartment,
) -> jsapi::CompartmentIterResult {
    let flags = &mut *(data as *mut CompartmentFlags);
    flags.total += 1;
    if jsapi::IsSystemCompartment(comp) {
        flags.system += 1;
    }
    if jsapi::IsSharableCompartment(comp) {
        flags.sharable += 1;
    }
    jsapi::CompartmentIterResult::KeepGoing
}

unsafe extern "C" fn is_system_principal(_principals: *mut JSPrincipals) -> bool {
    true
}

unsafe extern "C" fn destroy_principal(_principals: *mut JSPrincipals) {}

/// A system-zone realm is created with `JS_SetTrustedPrincipals` in effect, so
/// `IsSystemCompartment` must report it. An ordinary realm in the same context must not.
fn compartments(engine: &JSEngine) -> Observation {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();

    let flags = unsafe {
        rooted!(&in(cx) let ordinary = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(!ordinary.get().is_null(), "creating an ordinary global");

        JS_InitDestroyPrincipalsCallback(cx.raw_cx(), Some(destroy_principal));
        let callbacks = JSPrincipalsCallbacks {
            write: None,
            isSystemPrincipal: Some(is_system_principal),
            isAddonPrincipal: None,
        };
        let principals = CreateRustJSPrincipals(&callbacks, ptr::null_mut());
        assert!(!principals.is_null());
        jsapi::JS_HoldPrincipals(principals);
        jsapi::JS_SetTrustedPrincipals(cx.raw_cx(), principals);

        let mut system_options = RealmOptions::default();
        system_options.creationOptions_.compSpec_ =
            jsapi::CompartmentSpecifier::NewCompartmentInSystemZone;
        rooted!(&in(cx) let system = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            principals,
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*system_options,
        ));
        assert!(!system.get().is_null(), "creating a system global");

        let mut flags = CompartmentFlags::default();
        jsapi::JS_IterateCompartments(
            cx.raw_cx(),
            &mut flags as *mut _ as *mut c_void,
            Some(count_compartment),
        );

        jsapi::AssertSameCompartment(cx.raw_cx(), ordinary.get());
        flags
    };

    assert!(flags.total >= 2, "both realms are compartments");
    assert_eq!(
        flags.sharable, flags.total,
        "every realm in this context is sharable"
    );
    Observation::new(vec![("system", Value::Number(flags.system as f64))])
}

mod edge_realm_deep_contract {
    fn record_case_result(case: &str, observation: Observation, apis: &[&str]) {
        crate::support::record(case, &observation, apis);
    }

    thread_local! {
        static DEEP_TRACE_CALLS: Cell<u32> = const { Cell::new(0) };
    }

    unsafe extern "C" fn trace_global(trc: *mut jsapi::JSTracer, global: *mut JSObject) {
        let nested = DEEP_TRACE_CALLS.with(|calls| {
            let count = calls.get();
            calls.set(count + 1);
            count > 0
        });
        if !nested {
            jsapi::JS_GlobalObjectTraceHook(trc, global);
        }
    }

    fn realm_entry_identity_and_private(engine: &JSEngine) {
        crate::support::isolate_case("edge.realm.entry_identity_and_private", || {
            realm_entry_identity_and_private_inner(engine);
        });
    }

    fn realm_entry_identity_and_private_inner(engine: &JSEngine) {
        let mut runtime = Runtime::new(engine.handle());
        let cx = runtime.cx();
        unsafe {
            let options = jsapi::JS_NewRealmOptions();
            assert!(!options.is_null());
            (*options).creationOptions_.traceGlobal_ = Some(trace_global);
            DEEP_TRACE_CALLS.with(|calls| calls.set(0));
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                jsapi::OnNewGlobalHookOption::FireOnNewGlobalHook,
                options,
            ));
            let global_ptr = global.get();
            assert!(!global_ptr.is_null());

            let current_before = jsapi::CurrentGlobalOrNull(cx.raw_cx());
            let realm = jsapi::GetObjectRealmOrNull(global_ptr);
            assert!(!realm.is_null());
            let realm_global_roundtrip = jsapi::GetRealmGlobalOrNull(realm) == global_ptr;
            let non_ccw_global = jsapi::GetNonCCWObjectGlobal(global_ptr) == global_ptr;

            let previous = jsapi::EnterRealm(cx.raw_cx(), global_ptr);
            let entered_previous_null = previous.is_null();
            let current_after_enter = jsapi::CurrentGlobalOrNull(cx.raw_cx()) == global_ptr;
            let current_global_pointer = jsapi::CurrentGlobal(cx.raw_cx());
            let current_global_nonnull =
                !current_global_pointer.is_null() && !(*current_global_pointer).is_null();
            assert_eq!(entered_previous_null, true, "a realm was already entered");
            assert_eq!(
                current_after_enter, true,
                "CurrentGlobalOrNull did not return the entered global"
            );
            assert_eq!(
                current_global_nonnull, true,
                "CurrentGlobal did not return the entered global"
            );

            let private = 0x1234_u32 as usize as *mut c_void;
            jsapi::SetRealmPrivate(realm, private);
            let private_roundtrip = jsapi::GetRealmPrivate(realm) == private;
            jsapi::SetRealmPrivate(realm, ptr::null_mut());
            let private_cleared = jsapi::GetRealmPrivate(realm).is_null();
            assert_eq!(private_roundtrip, true, "realm private did not round trip");
            assert_eq!(private_cleared, true, "realm private was not cleared");

            js::rust::wrappers2::JS_GC(cx, jsapi::GCReason::API);
            let trace_calls = DEEP_TRACE_CALLS.with(Cell::get);
            let trace_state = if trace_calls > 0 {
                "called"
            } else {
                "not_called"
            };
            assert_ne!(
                trace_state, "not_called",
                "the global trace hook did not run"
            );

            jsapi::LeaveRealm(cx.raw_cx(), previous);
            let current_after_leave = jsapi::CurrentGlobalOrNull(cx.raw_cx()).is_null();
            assert_eq!(
                current_after_leave, true,
                "LeaveRealm did not leave the realm"
            );
            jsapi::DeleteRealmOptions(options);

            record_case_result(
                "edge.realm.entry_identity_and_private",
                Observation::new(vec![
                    (
                        "current_before",
                        Value::String(if current_before.is_null() {
                            "none".to_owned()
                        } else {
                            "entered".to_owned()
                        }),
                    ),
                    (
                        "entered_previous",
                        Value::String(if entered_previous_null {
                            "none".to_owned()
                        } else {
                            "previous_realm".to_owned()
                        }),
                    ),
                    (
                        "current_after_enter",
                        Value::String(if current_after_enter {
                            "entered_global".to_owned()
                        } else {
                            format!("{current_global_pointer:p}")
                        }),
                    ),
                    (
                        "realm_global_roundtrip",
                        Value::String(if realm_global_roundtrip {
                            "same_global".to_owned()
                        } else {
                            "different_global".to_owned()
                        }),
                    ),
                    (
                        "non_ccw_global",
                        Value::String(if non_ccw_global {
                            "same_global".to_owned()
                        } else {
                            "different_global".to_owned()
                        }),
                    ),
                    ("private_marker", Value::Number(0x1234_u32 as f64)),
                    (
                        "private_after_clear",
                        Value::String(if private_cleared {
                            "cleared".to_owned()
                        } else {
                            "retained".to_owned()
                        }),
                    ),
                    ("trace_hook", Value::String(trace_state.to_owned())),
                    (
                        "current_after_leave",
                        Value::String(if current_after_leave {
                            "none".to_owned()
                        } else {
                            "entered".to_owned()
                        }),
                    ),
                ]),
                &[
                    "CurrentGlobal",
                    "CurrentGlobalOrNull",
                    "DeleteRealmOptions",
                    "EnterRealm",
                    "GetNonCCWObjectGlobal",
                    "GetObjectRealmOrNull",
                    "GetRealmGlobalOrNull",
                    "GetRealmPrivate",
                    "JS_GlobalObjectTraceHook",
                    "JS_NewRealmOptions",
                    "LeaveRealm",
                    "SetRealmPrivate",
                ],
            );
        }
    }

    // ---------------------------------------------------------------------------
    // Script compilation, environment chains and script private values.
    // ---------------------------------------------------------------------------

    use std::cell::Cell;
    use std::ffi::c_void;
    use std::ptr;

    use js::jsapi::{self, JSObject};
    use js::rooted;
    use js::rust::wrappers2::JS_NewGlobalObject;
    use js::rust::{JSEngine, Runtime, SIMPLE_GLOBAL_CLASS};

    use crate::support::{Observation, Value};

    pub fn run(engine: &JSEngine) {
        realm_entry_identity_and_private(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_realm_deep_contract::run(engine);
    crate::support::run_case(
        "realm.compartments",
        &[
            "AssertSameCompartment",
            "IsSharableCompartment",
            "IsSystemCompartment",
            "JS_IterateCompartments",
            "JS_SetTrustedPrincipals",
        ],
        || compartments(engine),
    );
    crate::support::isolate_case("realm.enter_exit_and_private", || {
        let mut runtime = Runtime::new(engine.handle());
        let cx = runtime.cx();
        let mut trace_hook_calls = 0;
        unsafe {
            let options = jsapi::JS_NewRealmOptions();
            assert!(!options.is_null());
            (*options).creationOptions_.traceGlobal_ = Some(trace_global);
            GLOBAL_TRACE_HOOK_CALLS.with(|calls| calls.set(0));

            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &js::rust::SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                options,
            ));
            assert!(!global.get().is_null());

            let global_realm = jsapi::GetObjectRealmOrNull(global.get());
            assert!(!global_realm.is_null());
            assert_eq!(
                jsapi::GetRealmGlobalOrNull(global_realm),
                global.get(),
                "realm global round trip"
            );
            assert_eq!(
                jsapi::GetNonCCWObjectGlobal(global.get()),
                global.get(),
                "non-CCW global lookup"
            );
            assert!(jsapi::CurrentGlobalOrNull(cx.raw_cx()).is_null());

            let previous = jsapi::EnterRealm(cx.raw_cx(), global.get());
            assert!(previous.is_null(), "no realm was entered before");
            assert_eq!(jsapi::GetCurrentRealmOrNull(cx.raw_cx()), global_realm);
            assert_eq!(jsapi::CurrentGlobalOrNull(cx.raw_cx()), global.get());
            let current = jsapi::CurrentGlobal(cx.raw_cx());
            assert!(!current.is_null());
            assert!(!(*current).is_null());

            let private = 0x1234_u32 as usize as *mut std::ffi::c_void;
            jsapi::SetRealmPrivate(global_realm, private);
            assert_eq!(jsapi::GetRealmPrivate(global_realm), private);
            jsapi::SetRealmPrivate(global_realm, ptr::null_mut());
            assert!(jsapi::GetRealmPrivate(global_realm).is_null());

            // The creation options install `trace_global` as the global's trace hook. An
            // explicit collection must reach it, so the hook is observable, not just stored.
            JS_GC(cx, GCReason::API);
            trace_hook_calls = GLOBAL_TRACE_HOOK_CALLS.with(Cell::get);
            assert!(
                trace_hook_calls > 0,
                "the global trace hook did not run during an explicit collection"
            );

            jsapi::LeaveRealm(cx.raw_cx(), previous);
            assert!(jsapi::CurrentGlobalOrNull(cx.raw_cx()).is_null());
            assert!(jsapi::GetCurrentRealmOrNull(cx.raw_cx()).is_null());

            jsapi::DeleteRealmOptions(options);
        }

        record(
            "realm.enter_exit_and_private",
            &Observation::new(vec![
                (
                    "global_trace_hook_calls",
                    Value::Number(trace_hook_calls as f64),
                ),
                ("entered_previous_null", Value::Boolean(true)),
                ("current_global_after_enter", Value::Boolean(true)),
                ("realm_global_roundtrip", Value::Boolean(true)),
                ("private_cleared", Value::Boolean(true)),
            ]),
            &[
                "CurrentGlobal",
                "CurrentGlobalOrNull",
                "DeleteRealmOptions",
                "EnterRealm",
                "GetCurrentRealmOrNull",
                "GetNonCCWObjectGlobal",
                "GetObjectRealmOrNull",
                "GetRealmGlobalOrNull",
                "GetRealmPrivate",
                "JS_GlobalObjectTraceHook",
                "JS_NewRealmOptions",
                "LeaveRealm",
                "SetRealmPrivate",
            ],
        );
    });
}
