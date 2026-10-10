/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Autorelease pool statistics, in a build with `QJS_POOL_STATS=1` (`just test-pool-stats`).

#![cfg(qjs_pool_stats)]

use std::ptr;

use mozjs::jsapi::{Heap, JSObject, OnNewGlobalHookOption};
use mozjs::quickjs::{drain_autorelease_pool, pool_stats_json};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{JS_NewGlobalObject, JS_NewPlainObject};
use mozjs::rust::{JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

#[inline(never)]
unsafe fn store_unrooted(cx: &mut mozjs::context::JSContext) -> Box<Heap<*mut JSObject>> {
    Heap::boxed(JS_NewPlainObject(cx))
}

#[test]
fn fates_of_raw_results() {
    std::env::set_var("QJS_POOL_STATS_SAMPLE", "1");
    std::env::set_var("QJS_POOL_STATS_OUT", "/dev/null");
    let engine = JSEngine::init().unwrap();
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        drain_autorelease_pool();
        let before = pool_stats_json();
        assert!(!before.contains("\"api\":\"JS_NewPlainObject\""));

        rooted!(&in(cx) let rooted = JS_NewPlainObject(cx));
        let kept = store_unrooted(cx);
        assert!(!JS_NewPlainObject(cx).is_null());
        drain_autorelease_pool();

        let json = pool_stats_json();
        assert!(
            json.contains(
                "\"api\":\"JS_NewPlainObject\",\"raw\":true,\"site\":null,\"kind\":\"result\",\
                 \"total\":3,\"fresh\":3,\"rooted\":1,\"kept\":1,\"dropped\":1"
            ),
            "{json}"
        );
        // The unrooted ones name their caller; the rooted one isn't sampled.
        assert!(
            json.contains("\"fate\":\"kept\",\"frames\":[\"qjs_pool_stats::store_unrooted"),
            "{json}"
        );
        assert!(
            json.contains(
                "\"fate\":\"dropped\",\"frames\":[\"qjs_pool_stats::fates_of_raw_results"
            ),
            "{json}"
        );
        assert!(!json.contains("\"fate\":\"rooted\""), "{json}");
        drop(kept);
    }
}
