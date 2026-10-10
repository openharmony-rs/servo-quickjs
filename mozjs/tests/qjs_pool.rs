/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The autorelease pool: what a drain frees, and scopes (`AutoreleasePoolScope`).

use std::cell::{Cell, RefCell};
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::{JSContext, RawJSContext};
use mozjs::glue::{CreateJobQueue, DeleteJobQueue, JobQueueTraps};
use mozjs::jsapi::{
    self, GCContext, Handle, JSClass, JSClassOps, JSObject, MutableHandleObject,
    OnNewGlobalHookOption,
};
use mozjs::jsval::UndefinedValue;
use mozjs::quickjs::{
    autorelease_pool_len, drain_autorelease_pool, release_object, retain_object, sys as qjs,
    AutoreleasePoolScope,
};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{JS_NewGlobalObject, JS_NewObject, JS_NewPlainObject};
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

fn with_global(f: impl FnOnce(&mut JSContext, HandleObject)) {
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

fn eval_bool(cx: &mut JSContext, global: HandleObject, source: &str) -> bool {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_pool.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
    rval.get().to_boolean()
}

thread_local! {
    /// Objects `Owner`s hold counted references to; each owner's finalizer releases one.
    static OWNED: RefCell<Vec<*mut JSObject>> = const { RefCell::new(Vec::new()) };
    static OWNED_FREED: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn finalize_owner(_: *mut GCContext, _: *mut JSObject) {
    if let Some(owned) = OWNED.with(|o| o.borrow_mut().pop()) {
        release_object(owned);
    }
}

unsafe extern "C" fn finalize_owned(_: *mut GCContext, _: *mut JSObject) {
    OWNED_FREED.with(|f| f.set(f.get() + 1));
}

const fn ops(finalize: unsafe extern "C" fn(*mut GCContext, *mut JSObject)) -> JSClassOps {
    JSClassOps {
        addProperty: None,
        delProperty: None,
        enumerate: None,
        newEnumerate: None,
        resolve: None,
        mayResolve: None,
        finalize: Some(finalize),
        call: None,
        construct: None,
        trace: None,
    }
}

static OWNER_OPS: JSClassOps = ops(finalize_owner);
static OWNED_OPS: JSClassOps = ops(finalize_owned);

static OWNER_CLASS: JSClass = JSClass {
    name: c"Owner".as_ptr(),
    flags: 0,
    cOps: &OWNER_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static OWNED_CLASS: JSClass = JSClass {
    name: c"Owned".as_ptr(),
    flags: 0,
    cOps: &OWNED_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

/// The drain frees the owned objects' entries first, then the owners, whose finalizers drop the
/// owned objects' last references, which go to the pool in the middle of the drain. Optimized
/// builds lost those entries when the drain went through `&mut Cx`; CI runs this test in an
/// optimized ASan build too.
#[test]
fn drains_free_what_finalizers_release() {
    with_global(|cx, _| unsafe {
        const N: usize = 1000;
        for _ in 0..N {
            assert!(!JS_NewObject(cx, &OWNER_CLASS).is_null());
        }
        for _ in 0..N {
            let owned = JS_NewObject(cx, &OWNED_CLASS);
            assert!(!owned.is_null());
            retain_object(owned);
            OWNED.with(|o| o.borrow_mut().push(owned));
        }
        drain_autorelease_pool();
        assert_eq!(OWNED_FREED.with(Cell::get), N);
        assert_eq!(autorelease_pool_len(), 0);
    });
}

#[test]
fn scopes_release_only_their_own_entries() {
    with_global(|cx, _| unsafe {
        let outer = JS_NewPlainObject(cx);
        let entries = autorelease_pool_len();
        {
            let _scope = AutoreleasePoolScope::new();
            for _ in 0..100 {
                assert!(!JS_NewPlainObject(cx).is_null());
            }
            assert_eq!(autorelease_pool_len(), entries + 100);
        }
        assert_eq!(autorelease_pool_len(), entries);
        assert_eq!(refs(outer), 1);
    });
}

/// Servo's trap: the incumbent global is the current one.
unsafe extern "C" fn get_host_defined_data(
    cx: *mut RawJSContext,
    incumbent: MutableHandleObject,
    data: MutableHandleObject,
) -> bool {
    incumbent.set(jsapi::CurrentGlobalOrNull(cx));
    data.set(ptr::null_mut());
    true
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

static TRAPS: JobQueueTraps = JobQueueTraps {
    getHostDefinedData: Some(get_host_defined_data),
    getHostDefinedGlobal: None,
    runJobs: Some(run_jobs),
    traceNonGCThingMicroTask: None,
};

fn with_job_queue(f: impl FnOnce(&mut JSContext, HandleObject)) {
    with_global(|cx, global| unsafe {
        let queue = CreateJobQueue(&TRAPS);
        jsapi::SetJobQueue(cx.raw_cx(), queue as *mut jsapi::JobQueue);
        f(cx, global);
        jsapi::SetJobQueue(cx.raw_cx(), ptr::null_mut());
        DeleteJobQueue(queue);
    });
}

/// Each `then()` and `await` asks the trap for the incumbent global (patch Q32's hook), whose
/// out-parameters go to the pool; the hook releases them again.
#[test]
fn then_calls_leave_no_pool_entries() {
    with_job_queue(|cx, global| {
        let entries = autorelease_pool_len();
        assert!(eval_bool(
            cx,
            global,
            "let p = Promise.resolve(); for (let i = 0; i < 10000; i++) p.then(() => {}); true"
        ));
        assert!(autorelease_pool_len() - entries < 100);
        unsafe { jsapi::RunJobs(cx.raw_cx()) };
    });
}

#[test]
fn microtask_scopes_free_finished_jobs() {
    with_job_queue(|cx, global| {
        let entries = autorelease_pool_len();
        assert!(eval_bool(
            cx,
            global,
            "var done = false; (async () => { for (let i = 0; i < 10000; i++) await null; done = true; })(); true"
        ));
        unsafe { jsapi::RunJobs(cx.raw_cx()) };
        assert!(eval_bool(cx, global, "done"));
        assert!(
            autorelease_pool_len() - entries < 100,
            "{}",
            autorelease_pool_len() - entries
        );
    });
}
