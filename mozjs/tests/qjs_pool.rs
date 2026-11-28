/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The autorelease pool: what a drain frees, and scopes (`AutoreleasePoolScope`).

use std::cell::{Cell, RefCell};
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::{JSContext, RawJSContext};
use mozjs::glue::{CreateJobQueue, DeleteJobQueue, JS_GetReservedSlot, JobQueueTraps};
use mozjs::jsapi::{
    self, GCContext, Handle, JSClass, JSClassOps, JSObject, MutableHandleObject,
    OnNewGlobalHookOption,
};
use mozjs::jsval::{PrivateValue, UndefinedValue};
use mozjs::quickjs::{
    autorelease_pool_len, drain_autorelease_pool, release_dom_stub_count, release_object,
    retain_object, retain_object_count, sys as qjs, AutoreleasePoolScope,
};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    JS_GetDomStubPoolRefCount, JS_GetDomStubRefCount, JS_IsDomStub, JS_NewDomStub,
    JS_NewGlobalObject, JS_NewObject, JS_NewPlainObject, JS_UnbindDomStub,
};
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

#[test]
fn batch_dom_stub_references_are_counted_and_released() {
    with_global(|cx, _| unsafe {
        let reflector = Box::leak(Box::new(FakeReflector {
            dom_js_class: Cell::new(&STUB_CLASS),
        }));
        let stub = JS_NewDomStub(
            cx,
            &STUB_CLASS,
            reflector as *mut _ as *mut std::ffi::c_void,
        );
        assert!(!stub.is_null());

        retain_object_count(stub, 4);
        assert_eq!(refs(stub), 5);
        drain_autorelease_pool();
        assert_eq!(refs(stub), 4);

        release_dom_stub_count(stub, 3);
        assert_eq!(refs(stub), 1);
        release_dom_stub_count(stub, 1);
    });
}

#[test]
#[should_panic(expected = "release_dom_stub_count called on a non-JSDomStub object")]
fn release_dom_stub_count_requires_a_stub() {
    with_global(|cx, _| unsafe {
        let obj = JS_NewPlainObject(cx);
        release_dom_stub_count(obj, 1);
    });
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
    /// Reflectors dropped by `finalize_reflector`.
    static REFLECTOR_FREED: Cell<usize> = const { Cell::new(0) };
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

#[test]
fn batch_free_at_zero_releases_immediately() {
    with_global(|cx, _| unsafe {
        OWNED_FREED.with(|f| f.set(0));
        let obj = JS_NewObject(cx, &OWNED_CLASS);
        assert!(!obj.is_null());
        retain_object_count(obj, 2);
        drain_autorelease_pool();
        assert_eq!(refs(obj), 2);

        let rt = jsapi::JS_GetRuntime(cx.raw_cx()) as *mut qjs::JSRuntime;
        qjs::JS_FreeValueRTCount(
            rt,
            qjs::JSValue::mkptr(qjs::JS_TAG_OBJECT, obj as *mut _),
            2,
        );
        assert_eq!(OWNED_FREED.with(Cell::get), 1);
    });
}

/// `STUB_CLASS`'s finalize hook: frees the fake DOM node's `Reflector`, like an interface's
/// `finalize_common` drops the real node. `JSDomStub` finalization (`engine/class.rs`) calls the
/// class's `finalize` with the stub object, whose `dom_ptr` is the `Reflector`; it is only called
/// while the stub still owns the node, so an unbound stub does not reach here. A materialized
/// object of the same class keeps the node in `DOM_OBJECT_SLOT` instead (`wrap_object` and
/// `materialize_if_needed` put it there), the way `private_from_object` reads it.
unsafe extern "C" fn finalize_reflector(_: *mut GCContext, obj: *mut JSObject) {
    // Like `private_from_object`: a `JSDomStub` stores its node directly, a materialized object
    // of the same class keeps it in `DOM_OBJECT_SLOT`.
    let dom_ptr = if JS_IsDomStub(obj) {
        qjs::JS_GetDomStubDomPtr(qjs::JSValue::mkptr(qjs::JS_TAG_OBJECT, obj as *mut _))
    } else {
        let mut value = UndefinedValue();
        JS_GetReservedSlot(obj, 0, &mut value);
        if value.is_undefined() {
            ptr::null_mut()
        } else {
            value.to_private()
        }
    };
    if !dom_ptr.is_null() {
        drop(Box::from_raw(dom_ptr as *mut FakeReflector));
        REFLECTOR_FREED.with(|f| f.set(f.get() + 1));
    }
}

static STUB_OPS: JSClassOps = ops(finalize_reflector);

static STUB_CLASS: JSClass = JSClass {
    name: c"Stub".as_ptr(),
    // One reserved slot: `DOM_OBJECT_SLOT`, where a materialized object keeps its node.
    flags: (1 & jsapi::JSCLASS_RESERVED_SLOTS_MASK) << jsapi::JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: &STUB_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

#[repr(C)]
struct FakeReflector {
    dom_js_class: Cell<*const JSClass>,
}

#[test]
fn dom_stub_pool_entries_are_counted() {
    with_global(|cx, _| unsafe {
        let reflector = Box::leak(Box::new(FakeReflector {
            dom_js_class: Cell::new(&STUB_CLASS),
        }));
        let pool_len = autorelease_pool_len();
        // The wrapper adopts the raw allocation result before returning it, moving the new
        // object's initial count into the pool.
        let stub = JS_NewDomStub(
            cx,
            &STUB_CLASS,
            reflector as *mut _ as *mut std::ffi::c_void,
        );
        assert!(!stub.is_null());
        assert_eq!(autorelease_pool_len(), pool_len + 1);
        assert_eq!(refs(stub), 1);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 1);

        retain_object(stub);
        assert_eq!(refs(stub), 2);

        drain_autorelease_pool();
        assert_eq!(refs(stub), 1);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 0);

        // The last count goes to the pool as `Deferred`, so the stub must be counted once more.
        release_object(stub);
        assert_eq!(refs(stub), 1);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 1);
        drain_autorelease_pool();
    });
}

#[test]
fn unbind_clears_stub_pool_tracking() {
    with_global(|cx, _| unsafe {
        let reflector = Box::leak(Box::new(FakeReflector {
            dom_js_class: Cell::new(&STUB_CLASS),
        }));
        let stub = JS_NewDomStub(
            cx,
            &STUB_CLASS,
            reflector as *mut _ as *mut std::ffi::c_void,
        );
        retain_object(stub);
        assert_eq!(refs(stub), 2);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 1);

        JS_UnbindDomStub(cx, stub);
        assert_eq!(refs(stub), 2);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 0);

        drain_autorelease_pool();
        assert_eq!(refs(stub), 1);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 0);

        release_object(stub);
        assert_eq!(refs(stub), 1);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 1);
        drain_autorelease_pool();

        // Unbinding clears the stub's `dom_ptr`, so `finalize_reflector` no longer runs and the
        // fake node, which the test has no JSObject to hand over to, is freed here.
        drop(Box::from_raw(reflector as *mut FakeReflector));
    });
}

/// What `Reflector::materialize_if_needed` does in `components/script_bindings/reflector.rs`:
/// the stub's non-pool references move to a newly created `JSObject` before the stub is unbound,
/// and only the transferred ones are released from the stub afterwards. The stub's remaining
/// pool entry still drains normally, and the new object's own pool entry does too, leaving the
/// transferred reference as its only one.
#[test]
fn materialization_transfers_stub_counts_to_a_new_object() {
    with_global(|cx, _| unsafe {
        OWNED_FREED.with(|f| f.set(0));
        REFLECTOR_FREED.with(|f| f.set(0));
        let reflector = Box::leak(Box::new(FakeReflector {
            dom_js_class: Cell::new(&STUB_CLASS),
        }));
        let stub = JS_NewDomStub(
            cx,
            &STUB_CLASS,
            reflector as *mut _ as *mut std::ffi::c_void,
        );
        retain_object(stub);
        assert_eq!(refs(stub), 2);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 1);

        // Stands in for the object `materialize_if_needed` binds to; its initial count is
        // adopted into the pool, as any object created by the engine is.
        let obj = JS_NewObject(cx, &OWNED_CLASS);
        assert!(!obj.is_null());
        assert_eq!(refs(obj), 1);

        // Transfer the stub's non-pool references without adding pool entries: the new object's
        // adopted count will be released by the drain.
        let transferred_ref_count =
            JS_GetDomStubRefCount(stub) - JS_GetDomStubPoolRefCount(stub) as i32;
        assert_eq!(transferred_ref_count, 1);
        retain_object_count(obj, transferred_ref_count as u32);
        assert_eq!(refs(obj), 2);

        // The stub no longer owns its node, so its pool entries must only free the stub.
        JS_UnbindDomStub(cx, stub);
        assert_eq!(refs(stub), 2);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 0);

        // Release exactly what was handed to the new object.
        release_dom_stub_count(stub, transferred_ref_count as u32);
        assert_eq!(refs(stub), 1);
        assert_eq!(refs(obj), 2);

        // The drain releases the stub's last (pool) reference, freeing the unbound stub without
        // finalizing the reflector, and the new object's adopted count, leaving only the
        // transferred reference.
        drain_autorelease_pool();
        assert_eq!(refs(obj), 1);
        assert_eq!(REFLECTOR_FREED.with(Cell::get), 0);

        release_object(obj);
        assert_eq!(refs(obj), 1);
        drain_autorelease_pool();
        assert_eq!(OWNED_FREED.with(Cell::get), 1);

        // Unbinding cleared the stub's `dom_ptr`, so `finalize_reflector` no longer runs and the
        // fake node, which the test has no JSObject to hand over to, is freed here.
        drop(Box::from_raw(reflector as *mut FakeReflector));
    });
}

/// The same transfer as above, but a drain happens between unbinding and releasing the
/// transferred count. The drain only frees the stub's pool entry, so the stub is still alive
/// afterwards; the count release then drops its last reference and frees the unbound stub on the
/// spot. The new object shares the stub's class and, like `wrap_object`, takes the node in
/// `DOM_OBJECT_SLOT`; freeing it later is what reclaims the fake `Reflector`.
#[test]
fn drain_before_the_count_release_frees_the_stub_but_not_the_reflector() {
    with_global(|cx, _| unsafe {
        REFLECTOR_FREED.with(|f| f.set(0));
        let reflector = Box::leak(Box::new(FakeReflector {
            dom_js_class: Cell::new(&STUB_CLASS),
        }));
        let stub = JS_NewDomStub(
            cx,
            &STUB_CLASS,
            reflector as *mut _ as *mut std::ffi::c_void,
        );
        retain_object(stub);
        let obj = JS_NewObject(cx, &STUB_CLASS);
        assert!(!obj.is_null());
        // As `materialize_if_needed` does before it unbinds the stub, hand the node to the new
        // object through `DOM_OBJECT_SLOT`.
        jsapi::JS_SetReservedSlot(
            obj,
            0,
            &PrivateValue(reflector as *mut _ as *mut std::ffi::c_void),
        );
        let transferred_ref_count =
            JS_GetDomStubRefCount(stub) - JS_GetDomStubPoolRefCount(stub) as i32;
        assert_eq!(transferred_ref_count, 1);
        retain_object_count(obj, transferred_ref_count as u32);
        assert_eq!(refs(stub), 2);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 1);

        JS_UnbindDomStub(cx, stub);
        assert_eq!(JS_GetDomStubPoolRefCount(stub), 0);

        // The drain releases only the stub's pool entry: the not-yet-released transferred count
        // keeps the stub alive. The new object's adopted count goes too.
        drain_autorelease_pool();
        assert_eq!(refs(stub), 1);
        assert_eq!(refs(obj), 1);

        // The transferred count is the stub's last reference, so this frees the stub immediately
        // instead of parking it in the pool. Unbound, it does not finalize the reflector.
        release_dom_stub_count(stub, transferred_ref_count as u32);
        assert_eq!(autorelease_pool_len(), 0);
        assert_eq!(REFLECTOR_FREED.with(Cell::get), 0);

        // Freeing the new object finalizes the class, which reads the node from
        // `DOM_OBJECT_SLOT` and reclaims the reflector.
        release_object(obj);
        drain_autorelease_pool();
        assert_eq!(REFLECTOR_FREED.with(Cell::get), 1);
    });
}

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
