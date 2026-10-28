/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! While the embedder runs the jobs, FinalizationRegistry cleanup jobs are dropped instead of
//! being queued on QuickJS's own job list, where each held its realm forever (patch Q42).

use std::ptr;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::glue::{CreateJobQueue, DeleteJobQueue, JobQueueTraps};
use mozjs::jsapi::{self, GCReason, JSContext as RawJSContext, JSObject, OnNewGlobalHookOption};
use mozjs::jsval::UndefinedValue;
use mozjs::quickjs::{drain_autorelease_pool, realm_count};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{JS_NewGlobalObject, JS_GC};
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject, JSEngine, JSEngineHandle, RealmOptions,
    Runtime, SIMPLE_GLOBAL_CLASS,
};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

unsafe fn new_global(cx: &mut JSContext) -> *mut JSObject {
    JS_NewGlobalObject(
        cx,
        &SIMPLE_GLOBAL_CLASS,
        ptr::null_mut(),
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*RealmOptions::default(),
    )
}

unsafe fn eval(cx: &mut JSContext, global: HandleObject, src: &str) {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_finalization_jobs".to_owned(), 1);
    assert!(
        evaluate_script(cx, global, src, rval.handle_mut(), options).is_ok(),
        "{src}"
    );
}

unsafe extern "C" fn run_jobs(_: *mut RawJSContext) {}

static TRAPS: JobQueueTraps = JobQueueTraps {
    getHostDefinedData: None,
    getHostDefinedGlobal: None,
    runJobs: Some(run_jobs),
    traceNonGCThingMicroTask: None,
};

/// Lets a realm register 10 targets that die at once with a FinalizationRegistry it keeps, then
/// drops the realm; returns how many more realms are alive after that than before.
unsafe fn realms_left_behind(cx: &mut JSContext) -> usize {
    let baseline = realm_count();
    {
        rooted!(&in(cx) let global = new_global(cx));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let (global, cx) = realm.global_and_reborrow();
        eval(
            cx,
            global,
            "globalThis.registry = new FinalizationRegistry(() => {});
             for (let i = 0; i < 10; i++) registry.register({}, i);",
        );
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
    }
    drain_autorelease_pool();
    JS_GC(cx, GCReason::API);
    JS_GC(cx, GCReason::API);
    realm_count() - baseline
}

#[test]
fn cleanup_jobs_under_a_host_queue_do_not_keep_their_realm() {
    let mut runtime = Runtime::new(ENGINE.clone());
    let cx = runtime.cx();
    unsafe {
        // Like Servo: the embedder runs the jobs, so QuickJS's own job list is never drained.
        let queue = CreateJobQueue(&TRAPS);
        jsapi::SetJobQueue(cx.raw_cx(), queue as *mut jsapi::JobQueue);
        assert_eq!(realms_left_behind(cx), 0);
        jsapi::SetJobQueue(cx.raw_cx(), ptr::null_mut());
        DeleteJobQueue(queue);
    }
}

#[test]
fn cleanup_jobs_on_the_internal_queue_run() {
    let mut runtime = Runtime::new(ENGINE.clone());
    let cx = runtime.cx();
    unsafe {
        // Without a host queue, QuickJS's own list keeps the jobs, and RunJobs runs them.
        assert!(jsapi::UseInternalJobQueues(cx.raw_cx()));
        rooted!(&in(cx) let global = new_global(cx));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let (global, cx) = realm.global_and_reborrow();
        eval(
            cx,
            global,
            "globalThis.cleaned = [];
             globalThis.registry = new FinalizationRegistry(held => cleaned.push(held));
             for (let i = 0; i < 3; i++) registry.register({}, i);",
        );
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        jsapi::RunJobs(cx.raw_cx());
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let options = CompileOptionsWrapper::new(cx, c"qjs_finalization_jobs".to_owned(), 1);
        assert!(evaluate_script(
            cx,
            global,
            "cleaned.sort().join() === '0,1,2'",
            rval.handle_mut(),
            options
        )
        .is_ok());
        assert!(rval.get().to_boolean());
    }
}
