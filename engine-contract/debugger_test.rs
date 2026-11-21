/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! `Debugger` object installation on a global.

use std::ptr;

use js::jsapi::{self, OnNewGlobalHookOption};
use js::rooted;
use js::rust::wrappers2::JS_NewGlobalObject;
use js::rust::{JSEngine, RealmOptions, SIMPLE_GLOBAL_CLASS};

use crate::support::{Observation, Value, eval, observe, run_case, with_global};

fn ran() -> Observation {
    Observation::new(vec![("ran", Value::Boolean(true))])
}

/// `JS_DefineDebuggerObject` installs the `Debugger` constructor on a global, and returns whether
/// the global was one. Servo calls it from the debugger global scope to hand `debugger.js` the
/// same `Debugger` entry point on both engines.
fn global_object(engine: &JSEngine) -> Observation {
    let debugger_type = with_global(engine, |cx, global| unsafe {
        assert!(
            js::rust::wrappers2::JS_DefineDebuggerObject(cx, global),
            "JS_DefineDebuggerObject failed"
        );
        let value = eval(cx, global, "typeof Debugger");
        assert_eq!(observe(cx, value), Value::String("function".to_owned()));
        observe(cx, value)
    });
    Observation::new(vec![("debugger_type", debugger_type)])
}

/// `JS_FireOnNewGlobalObject` delivers the hook that a `DontFireOnNewGlobalHook` global skipped
/// to every debugger observing it. The case creates such a global and counts the
/// `Debugger.onNewGlobalObject` calls the manual fire produces.
///
/// `DebugAPI::slowPathOnNewGlobalObject` interrupts the debugger's job queue, so this needs a
/// context built with `UseInternalJobQueues`, which `Runtime::new` does not do.
fn new_global_object_hook(_engine: &JSEngine) -> Observation {
    let hits = unsafe {
        let raw = jsapi::JS_NewContext(1 << 20, ptr::null_mut());
        assert!(!raw.is_null(), "JS_NewContext");
        let mut cx = js::context::JSContext::from_ptr(std::ptr::NonNull::new(raw).unwrap());
        assert!(js::rust::wrappers2::UseInternalJobQueues(&mut cx));
        let cache = jsapi::__BindgenOpaqueArray::<u64, 2>::default();
        assert!(jsapi::InitSelfHostedCode(cx.raw_cx(), cache, None));
        jsapi::JS_SetNativeStackQuota(cx.raw_cx(), 256 * 1024, 192 * 1024, 128 * 1024);

        let hits = {
            rooted!(&in(cx) let debugger_global = JS_NewGlobalObject(
                &mut cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*RealmOptions::default(),
            ));
            {
                let mut realm =
                    js::realm::AutoRealm::new_from_handle(&mut cx, debugger_global.handle());
                let (global, cx) = realm.global_and_reborrow();
                assert!(
                    js::rust::wrappers2::JS_DefineDebuggerObject(cx, global),
                    "JS_DefineDebuggerObject failed"
                );
                let setup = eval(
                    cx,
                    global,
                    "globalThis.hits = 0;
                     globalThis.dbg = new Debugger();
                     globalThis.dbg.onNewGlobalObject = function () { hits++; };
                     typeof dbg.onNewGlobalObject",
                );
                assert_eq!(observe(cx, setup), Value::String("function".to_owned()));
            }

            rooted!(&in(cx) let target = JS_NewGlobalObject(
                &mut cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::DontFireOnNewGlobalHook,
                &*RealmOptions::default(),
            ));
            assert!(!target.get().is_null(), "creating the observed global");
            jsapi::JS_FireOnNewGlobalObject(cx.raw_cx(), target.handle().into());

            let mut realm =
                js::realm::AutoRealm::new_from_handle(&mut cx, debugger_global.handle());
            let (global, cx) = realm.global_and_reborrow();
            let hits = eval(cx, global, "hits");
            observe(cx, hits)
        };

        jsapi::JS_DestroyContext(cx.raw_cx());
        hits
    };

    #[cfg(not(feature = "js-quickjs"))]
    assert!(
        matches!(hits, Value::Number(1.0)),
        "JS_FireOnNewGlobalObject must deliver exactly one hook call: {hits:?}"
    );
    Observation::new(vec![("on_new_global_object", hits)])
}

pub fn run(engine: &JSEngine) {
    run_case(
        "debugger.global_object",
        &["JS_DefineDebuggerObject"],
        || global_object(engine),
    );
    run_case(
        "debugger.new_global_object_hook",
        &["JS_FireOnNewGlobalObject"],
        || new_global_object_hook(engine),
    );
}
