/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Rooting patterns SpiderMonkey allows, which the shim must keep working: values written to a
//! root without `set`, roots whose traced edges change, and raw values held across a microtask
//! checkpoint. Counted roots (servo-quickjs `pool-removal-phase1`) turned each of them into a
//! use-after-free; the autorelease pool keeps them valid until the task ends.

use std::ptr;
use std::rc::Rc;
use std::sync::LazyLock;

use mozjs::context::{JSContext, RawJSContext};
use mozjs::glue::{CreateJobQueue, DeleteJobQueue, JobQueueTraps};
use mozjs::jsapi::{self, Handle, JSObject, OnNewGlobalHookOption, Value};
use mozjs::jsval::{ObjectValue, UndefinedValue};
use mozjs::quickjs::{
    autorelease_pool_len, drain_autorelease_pool, sys as qjs, AutoreleasePoolScope,
};
use mozjs::realm::AutoRealm;
use mozjs::rust::wrappers2::{JS_NewGlobalObject, JS_NewPlainObject};
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine,
    JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};
use mozjs::{auto_root, rooted};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

fn with_global(f: impl FnOnce(&mut JSContext, SafeHandleObject)) {
    let mut runtime = Runtime::new(ENGINE.clone());
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
        let (global, cx) = realm.global_and_reborrow();
        drain_autorelease_pool();
        f(cx, global);
    }
}

fn refs(obj: *mut JSObject) -> i32 {
    unsafe { qjs::JS_GetRefCount(qjs::JSValue::mkptr(qjs::JS_TAG_OBJECT, obj as *mut _)) }
}

fn eval(cx: &mut JSContext, global: SafeHandleObject, source: &str) {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_root_contract.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
}

#[test]
fn writes_through_as_mut_ref() {
    with_global(|cx, _| unsafe {
        let obj = JS_NewPlainObject(cx);
        {
            rooted!(&in(cx) let mut r = ptr::null_mut::<JSObject>());
            *r.as_mut_ref(cx.no_gc()) = obj;
        }
        assert_eq!(refs(obj), 1);
        drain_autorelease_pool();
        assert_eq!(autorelease_pool_len(), 0);
    });
}

#[test]
fn writes_through_custom_auto_rooter() {
    with_global(|cx, _| unsafe {
        let obj = JS_NewPlainObject(cx);
        {
            auto_root!(&in(cx) let mut g = ptr::null_mut::<JSObject>());
            *g = obj;
        }
        assert_eq!(refs(obj), 1);
        drain_autorelease_pool();
        assert_eq!(autorelease_pool_len(), 0);
    });
}

#[test]
fn rooted_vec_changed_through_as_mut_ref() {
    with_global(|cx, _| unsafe {
        let obj = JS_NewPlainObject(cx);
        {
            rooted!(&in(cx) let mut v = Vec::<Value>::new());
            v.as_mut_ref(cx.no_gc()).push(ObjectValue(obj));
        }
        assert_eq!(refs(obj), 1);
        drain_autorelease_pool();
        assert_eq!(autorelease_pool_len(), 0);
    });
}

/// `Traceable for Rc<T>` traces only while the `Rc` is not shared, so a root's traced edges change
/// when another `Rc` to the same value goes away.
#[test]
fn rooted_rc_whose_sharing_changes() {
    with_global(|cx, _| unsafe {
        rooted!(&in(cx) let keep = JS_NewPlainObject(cx));
        let obj = keep.get();
        let rc = Rc::new(ObjectValue(obj));
        let other = rc.clone();
        let before = refs(obj);
        {
            rooted!(&in(cx) let v = vec![rc]);
            drop(other);
            assert_eq!(v.len(), 1);
        }
        assert_eq!(refs(obj), before);
    });
}

/// Servo's checkpoint loop, reduced to running the JS microtasks, with a scope per microtask.
unsafe extern "C" fn run_jobs(cx: *mut RawJSContext) {
    rooted!(in(cx) let mut task = UndefinedValue());
    while jsapi::HasAnyMicroTasks(cx) {
        let _scope = AutoreleasePoolScope::new();
        mozjs::glue::JS_DequeueNextMicroTask(cx, task.handle_mut().into());
        let task = jsapi::ToMaybeWrappedJSMicroTask(&*task);
        assert!(jsapi::RunJSMicroTask(
            cx,
            Handle::from_marked_location(&task)
        ));
    }
}

unsafe extern "C" fn get_host_defined_data(
    _: *mut RawJSContext,
    _incumbent: jsapi::MutableHandleObject,
    _data: jsapi::MutableHandleObject,
) -> bool {
    true
}

static TRAPS: JobQueueTraps = JobQueueTraps {
    getHostDefinedData: Some(get_host_defined_data),
    getHostDefinedGlobal: None,
    runJobs: Some(run_jobs),
    traceNonGCThingMicroTask: None,
};

/// A raw API result held across a microtask checkpoint stays valid until the task ends.
#[test]
fn raw_value_across_checkpoints() {
    with_global(|cx, global| unsafe {
        let queue = CreateJobQueue(&TRAPS);
        jsapi::SetJobQueue(cx.raw_cx(), queue as *mut jsapi::JobQueue);
        eval(cx, global, "Promise.resolve().then(() => {})");
        jsapi::RunJobs(cx.raw_cx());
        let obj = JS_NewPlainObject(cx);
        let entries = autorelease_pool_len();
        eval(cx, global, "Promise.resolve().then(() => {})");
        jsapi::RunJobs(cx.raw_cx());
        assert!(autorelease_pool_len() >= entries);
        assert_eq!(refs(obj), 1);
        jsapi::SetJobQueue(cx.raw_cx(), ptr::null_mut());
        DeleteJobQueue(queue);
    });
}
