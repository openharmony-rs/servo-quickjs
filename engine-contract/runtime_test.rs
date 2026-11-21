/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Runtime/context ownership and warning-reporter contract.

use std::ptr;

use js::jsapi::{self, JSErrorReport, JSGCParamKey};
use js::rust::JSEngine;

use crate::support::{Observation, Value, record};

unsafe extern "C" fn warning_reporter(_cx: *mut jsapi::JSContext, _report: *mut JSErrorReport) {}

pub fn run(_engine: &JSEngine) {
    let mut fields = Vec::new();
    crate::support::isolate_case("runtime.context_lifecycle", || unsafe {
        let cx = jsapi::JS_NewContext(1 << 20, ptr::null_mut());
        assert!(!cx.is_null(), "JS_NewContext");
        let runtime = jsapi::JS_GetRuntime(cx);
        assert!(!runtime.is_null(), "JS_GetRuntime");
        let runtime_non_null = !runtime.is_null();
        let initial_private = jsapi::JS_GetContextPrivate(cx);
        assert!(initial_private.is_null(), "new context private is not null");

        jsapi::JS_SetNativeStackQuota(cx, 256 * 1024, 192 * 1024, 128 * 1024);

        let reporter: jsapi::WarningReporter = Some(warning_reporter);
        let previous = jsapi::SetWarningReporter(cx, reporter);
        let installed_reporter = jsapi::GetWarningReporter(cx).map(|f| f as usize);
        let expected_reporter = Some(warning_reporter as *const () as usize);
        let reporter_installed = installed_reporter == expected_reporter;
        assert_eq!(
            installed_reporter, expected_reporter,
            "SetWarningReporter did not install the callback"
        );
        let reporter_slot = if reporter_installed {
            "contract_warning_reporter".to_owned()
        } else {
            format!("{:?}", jsapi::GetWarningReporter(cx))
        };
        let restored_reporter = jsapi::SetWarningReporter(cx, previous).map(|f| f as usize);
        let previous_roundtrip = restored_reporter == expected_reporter;
        assert_eq!(
            restored_reporter, expected_reporter,
            "SetWarningReporter did not return the previously installed callback"
        );

        let private = 0x5a5a_u32 as usize as *mut std::ffi::c_void;
        jsapi::JS_SetContextPrivate(cx, private);
        let private_roundtrip = jsapi::JS_GetContextPrivate(cx) == private;
        assert_eq!(
            jsapi::JS_GetContextPrivate(cx),
            private,
            "JS_SetContextPrivate did not store the pointer"
        );
        let private_marker = jsapi::JS_GetContextPrivate(cx) as usize;
        jsapi::JS_SetContextPrivate(cx, ptr::null_mut());
        let private_cleared = jsapi::JS_GetContextPrivate(cx).is_null();
        assert_eq!(
            jsapi::JS_GetContextPrivate(cx),
            ptr::null_mut(),
            "JS_SetContextPrivate(NULL) did not clear the pointer"
        );
        jsapi::JS_DestroyContext(cx);

        fields.extend([
            ("context_non_null", Value::Boolean(true)),
            ("runtime_non_null", Value::Boolean(runtime_non_null)),
            (
                "initial_private_null",
                Value::Boolean(initial_private.is_null()),
            ),
            ("reporter_installed", Value::Boolean(reporter_installed)),
            ("reporter_slot", Value::String(reporter_slot)),
            (
                "previous_reporter_roundtrip",
                Value::Boolean(previous_roundtrip),
            ),
            ("private_roundtrip", Value::Boolean(private_roundtrip)),
            ("private_marker", Value::Number(private_marker as f64)),
            ("private_cleared", Value::Boolean(private_cleared)),
        ]);

        let cx = jsapi::JS_NewContext(1 << 20, ptr::null_mut());
        assert!(!cx.is_null(), "second JS_NewContext returned null");
        let second_private = jsapi::JS_GetContextPrivate(cx);
        assert!(
            second_private.is_null(),
            "second context private is not null"
        );
        let second_runtime = jsapi::JS_GetRuntime(cx);
        assert!(
            !second_runtime.is_null(),
            "second JS_GetRuntime returned null"
        );
        let previous = jsapi::JS_GetGCParameter(cx, JSGCParamKey::JSGC_MAX_BYTES);
        let target = previous.saturating_sub(1);
        jsapi::JS_SetGCParameter(cx, JSGCParamKey::JSGC_MAX_BYTES, target);
        let applied_value = jsapi::JS_GetGCParameter(cx, JSGCParamKey::JSGC_MAX_BYTES);
        let applied = applied_value == target;
        jsapi::JS_SetGCParameter(cx, JSGCParamKey::JSGC_MAX_BYTES, previous);
        let restored_value = jsapi::JS_GetGCParameter(cx, JSGCParamKey::JSGC_MAX_BYTES);
        let restored = restored_value == previous;
        assert_eq!(
            applied_value, target,
            "JS_SetGCParameter was not reflected by JS_GetGCParameter"
        );
        assert_eq!(
            restored_value, previous,
            "JS_SetGCParameter did not restore the previous value"
        );
        fields.extend([
            (
                "second_context_private_null",
                Value::Boolean(second_private.is_null()),
            ),
            ("gc_parameter_previous", Value::Number(previous as f64)),
            ("gc_parameter_target", Value::Number(target as f64)),
            ("gc_parameter_applied", Value::Number(applied_value as f64)),
            (
                "gc_parameter_restored",
                Value::Number(restored_value as f64),
            ),
            ("gc_parameter_applied_ok", Value::Boolean(applied)),
            ("gc_parameter_restored_ok", Value::Boolean(restored)),
        ]);
        jsapi::JS_DestroyContext(cx);

        // `JS_DestroyContext` has no return value. A fresh context with a clean private slot is
        // the strongest observable lifecycle check available through this API; it cannot prove
        // that a no-op destroy would have failed.
        let cx = jsapi::JS_NewContext(1 << 20, ptr::null_mut());
        assert!(!cx.is_null(), "post-destroy JS_NewContext returned null");
        let recreated_private = jsapi::JS_GetContextPrivate(cx);
        assert!(recreated_private.is_null());
        jsapi::JS_DestroyContext(cx);
        fields.extend([
            ("post_destroy_context_non_null", Value::Boolean(true)),
            (
                "post_destroy_private_null",
                Value::Boolean(recreated_private.is_null()),
            ),
        ]);

        record(
            "runtime.context_lifecycle",
            &Observation::new(std::mem::take(&mut fields)),
            &[
                "JS_DestroyContext",
                "JS_GetContextPrivate",
                "JS_GetGCParameter",
                "JS_GetRuntime",
                "JS_NewContext",
                "JS_SetContextPrivate",
                "JS_SetGCParameter",
                "JS_SetNativeStackQuota",
                "GetWarningReporter",
                "SetWarningReporter",
            ],
        );
    });
}
