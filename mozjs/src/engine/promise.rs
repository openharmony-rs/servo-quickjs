/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Promises, the job (microtask) queue, interrupts and security callbacks.
//!
//! With a `JS::JobQueue` installed, QuickJS hands every promise job to the shim (patch Q4), which
//! queues it as a microtask for the embedder to run at its microtask checkpoint. Without one, jobs
//! stay in QuickJS's internal list and `RunJobs` drains it.

use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use servo_quickjs as q;

use super::reentrant::{Queue, Stack};
use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;
use crate::sys::jsval::{JSVal, UndefinedValue};

/// Per-runtime state of this module (a field of `Cx`).
pub(crate) struct RuntimeHooks {
    job_queue: Cell<*mut JobQueue>,
    /// The microtask queue; objects are counted.
    microtasks: Queue<q::JSValue>,
    rejection_tracker: Cell<root::JS::PromiseRejectionTrackerCallback>,
    rejection_tracker_data: Cell<*mut c_void>,
    interrupt_callbacks: Stack<root::JSInterruptCallback>,
    /// Set by `JS_RequestInterruptCallback`, possibly from another thread.
    interrupt_requested: Arc<AtomicBool>,
    /// While a promise operation defers interrupts: the innermost stack frame when it began.
    interrupts_deferred_at: Cell<Option<usize>>,
    security_callbacks: Cell<*const root::JSSecurityCallbacks>,
}

impl Default for RuntimeHooks {
    fn default() -> Self {
        RuntimeHooks {
            job_queue: Cell::new(ptr::null_mut()),
            microtasks: Default::default(),
            rejection_tracker: Cell::new(None),
            rejection_tracker_data: Cell::new(ptr::null_mut()),
            interrupt_callbacks: Default::default(),
            interrupt_requested: Default::default(),
            interrupts_deferred_at: Cell::new(None),
            security_callbacks: Cell::new(ptr::null()),
        }
    }
}

/// The runtime state from a QuickJS hook's opaque pointer (set to the `Cx`).
unsafe fn hook_cx<'a>(opaque: *mut c_void) -> &'a Cx {
    &*(opaque as *const Cx)
}

/// Enters the realm of QuickJS context `ctx` for an embedder callback, which expects the realm
/// that is running script to be current. Returns whether a realm was pushed.
unsafe fn enter_qctx(cx: &Cx, ctx: *mut q::JSContext) -> bool {
    let realm = q::JS_GetContextOpaque(ctx) as *mut RealmData;
    if realm.is_null() {
        return false;
    }
    cx.enter_realm(realm);
    true
}

unsafe fn leave_qctx(cx: &Cx, entered: bool) {
    if entered {
        cx.leave_realm();
    }
}

unsafe fn handle<T>(ptr: *const T) -> root::JS::Handle<T> {
    root::JS::Handle::from_marked_location(ptr)
}

fn object_or_undefined(obj: *mut root::JSObject) -> q::JSValue {
    if obj.is_null() {
        q::JS_UNDEFINED
    } else {
        obj_val(obj)
    }
}

// Promises.

unsafe fn promise_state(promise: *mut root::JSObject) -> Option<q::JSPromiseStateEnum> {
    let state = q::JS_PromiseState(super::current().base_ctx, obj_val(promise));
    (state <= q::JS_PROMISE_REJECTED).then_some(state)
}

/// Runs `f`, a promise operation, with a pending interrupt left pending while only built-in and
/// native functions run (Q36). QuickJS polls for interrupts on every call, of its own built-ins
/// too, where SpiderMonkey checks only in script, so creating or settling a promise never fails
/// there because an interrupt was requested. Script that `f` runs, such as a `then` getter or an
/// executor, stays interruptible.
unsafe fn deferring_interrupts<R>(cx: *mut root::JSContext, f: impl FnOnce() -> R) -> R {
    struct Restore<'a>(&'a Cell<Option<usize>>, Option<usize>);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.set(self.1);
        }
    }
    let cx = Cx::from_raw(cx);
    let deferred_at = &cx.hooks.interrupts_deferred_at;
    let mark = super::error::innermost_frame(cx);
    let _restore = Restore(deferred_at, deferred_at.replace(Some(mark)));
    f()
}

pub(crate) unsafe fn NewPromiseObject(
    cx: *mut root::JSContext,
    executor: root::JS::HandleObject,
) -> *mut root::JSObject {
    deferring_interrupts(cx, || new_promise_object(cx, executor))
}

unsafe fn new_promise_object(
    cx: *mut root::JSContext,
    executor: root::JS::HandleObject,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let mut funcs = [q::JS_UNDEFINED; 2];
    let promise = q::JS_NewPromiseCapability(qctx, funcs.as_mut_ptr());
    if promise.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    let mut ok = true;
    if !(*executor.ptr).is_null() {
        // The Promise constructor's steps, without looking up a possibly replaced constructor.
        let r = q::JS_Call(
            qctx,
            obj_val(*executor.ptr),
            q::JS_UNDEFINED,
            2,
            funcs.as_mut_ptr(),
        );
        if r.tag() != q::JS_TAG_EXCEPTION {
            q::JS_FreeValueRT(cx.rt, r);
        } else if q::JS_HasUncatchableException(qctx) != 0 {
            ok = false;
        } else {
            let mut error = q::JS_GetException(qctx);
            let r = q::JS_Call(qctx, funcs[1], q::JS_UNDEFINED, 1, &mut error);
            q::JS_FreeValueRT(cx.rt, error);
            ok = r.tag() != q::JS_TAG_EXCEPTION;
            q::JS_FreeValueRT(cx.rt, r);
        }
    }
    for f in funcs {
        q::JS_FreeValueRT(cx.rt, f);
    }
    if !ok {
        q::JS_FreeValueRT(cx.rt, promise);
        return ptr::null_mut();
    }
    cx.adopt(promise).to_object()
}

pub(crate) unsafe fn IsPromiseObject(obj: root::JS::HandleObject) -> bool {
    promise_state(*obj.ptr).is_some()
}

pub(crate) unsafe fn GetPromiseState(promise: root::JS::HandleObject) -> root::JS::PromiseState {
    match promise_state(*promise.ptr) {
        Some(q::JS_PROMISE_FULFILLED) => root::JS::PromiseState::Fulfilled,
        Some(q::JS_PROMISE_REJECTED) => root::JS::PromiseState::Rejected,
        _ => root::JS::PromiseState::Pending,
    }
}

pub(crate) unsafe fn JS_GetPromiseResult(
    promise: root::JS::HandleObject,
    dest: root::JS::MutableHandleValue,
) {
    let cx = super::current();
    let result = q::JS_PromiseResult(cx.base_ctx, obj_val(*promise.ptr));
    *dest.ptr = cx.adopt(result);
}

pub(crate) unsafe fn GetPromiseIsHandled(promise: root::JS::HandleObject) -> bool {
    q::JS_GetPromiseIsHandled(obj_val(*promise.ptr)) != 0
}

pub(crate) unsafe fn SetAnyPromiseIsHandled(
    cx: *mut root::JSContext,
    promise: root::JS::HandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let p = obj_val(*promise.ptr);
    let Some(state) = promise_state(*promise.ptr) else {
        q::JS_ThrowTypeError(cx.qctx(), c"not a promise".as_ptr());
        return false;
    };
    q::JS_SetPromiseIsHandled(p, 1);
    // Like SpiderMonkey, a settled promise is reported as handled to the rejection tracker.
    if state != q::JS_PROMISE_PENDING {
        let qctx = cx.qctx();
        notify_rejection_tracker(
            cx,
            qctx,
            *promise.ptr,
            root::JS::PromiseRejectionHandlingState::Handled,
        );
    }
    true
}

unsafe fn settle(
    cx: *mut root::JSContext,
    promise: root::JS::HandleObject,
    value: root::JS::HandleValue,
    reject: bool,
) -> bool {
    deferring_interrupts(cx, || {
        let cx = Cx::from_raw(cx);
        q::JS_SettlePromise(
            cx.qctx(),
            obj_val(*promise.ptr),
            (*value.ptr).to_qjs(),
            reject as c_int,
        ) >= 0
    })
}

pub(crate) unsafe fn ResolvePromise(
    cx: *mut root::JSContext,
    promiseObj: root::JS::HandleObject,
    resolutionValue: root::JS::HandleValue,
) -> bool {
    settle(cx, promiseObj, resolutionValue, false)
}

pub(crate) unsafe fn RejectPromise(
    cx: *mut root::JSContext,
    promiseObj: root::JS::HandleObject,
    rejectionValue: root::JS::HandleValue,
) -> bool {
    settle(cx, promiseObj, rejectionValue, true)
}

unsafe fn original_resolve(
    cx: *mut root::JSContext,
    value: root::JS::HandleValue,
    reject: bool,
) -> *mut root::JSObject {
    deferring_interrupts(cx, || {
        let cx = Cx::from_raw(cx);
        let p = q::JS_PromiseResolve(cx.qctx(), (*value.ptr).to_qjs(), reject as c_int);
        super::class::adopt_object(cx, p)
    })
}

pub(crate) unsafe fn CallOriginalPromiseResolve(
    cx: *mut root::JSContext,
    resolutionValue: root::JS::HandleValue,
) -> *mut root::JSObject {
    original_resolve(cx, resolutionValue, false)
}

pub(crate) unsafe fn CallOriginalPromiseReject(
    cx: *mut root::JSContext,
    rejectionValue: root::JS::HandleValue,
) -> *mut root::JSObject {
    original_resolve(cx, rejectionValue, true)
}

pub(crate) unsafe fn AddPromiseReactions(
    cx: *mut root::JSContext,
    promise: root::JS::HandleObject,
    onFulfilled: root::JS::HandleObject,
    onRejected: root::JS::HandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = q::JS_PerformPromiseThen(
        cx.qctx(),
        obj_val(*promise.ptr),
        object_or_undefined(*onFulfilled.ptr),
        object_or_undefined(*onRejected.ptr),
        0,
    );
    r.tag() != q::JS_TAG_EXCEPTION
}

pub(crate) unsafe fn SetPromiseUserInputEventHandlingState(
    promise: root::JS::HandleObject,
    state: root::JS::PromiseUserInputEventHandlingState,
) -> bool {
    q::JS_SetPromiseUserInputState(obj_val(*promise.ptr), state as c_int);
    true
}

pub(crate) unsafe fn GetPromiseUserInputEventHandlingState(
    promise: *mut root::JSObject,
) -> root::JS::PromiseUserInputEventHandlingState {
    use root::JS::PromiseUserInputEventHandlingState as State;
    match q::JS_GetPromiseUserInputState(obj_val(promise)) {
        1 => State::HadUserInteractionAtCreation,
        2 => State::DidntHaveUserInteractionAtCreation,
        _ => State::DontCare,
    }
}

pub(crate) unsafe fn SetPromiseRejectionTrackerCallback(
    cx: *mut root::JSContext,
    callback: root::JS::PromiseRejectionTrackerCallback,
    data: *mut ::std::os::raw::c_void,
) {
    let cx = Cx::from_raw(cx);
    cx.hooks.rejection_tracker.set(callback);
    cx.hooks.rejection_tracker_data.set(data);
    let opaque = cx.as_raw() as *mut c_void;
    q::JS_SetHostPromiseRejectionTracker(cx.rt, callback.map(|_| track_rejection as _), opaque);
}

unsafe extern "C" fn track_rejection(
    ctx: *mut q::JSContext,
    promise: q::JSValue,
    reason: q::JSValue,
    is_handled: c_int,
    opaque: *mut c_void,
) {
    let _ = reason;
    let state = if is_handled != 0 {
        root::JS::PromiseRejectionHandlingState::Handled
    } else {
        root::JS::PromiseRejectionHandlingState::Unhandled
    };
    notify_rejection_tracker(hook_cx(opaque), ctx, val_obj(promise), state);
}

unsafe fn notify_rejection_tracker(
    cx: &Cx,
    ctx: *mut q::JSContext,
    promise: *mut root::JSObject,
    state: root::JS::PromiseRejectionHandlingState,
) {
    let Some(callback) = cx.hooks.rejection_tracker.get() else {
        return;
    };
    let data = cx.hooks.rejection_tracker_data.get();
    let muted = super::error::current_script_muted(cx);
    let entered = enter_qctx(cx, ctx);
    callback(cx.as_raw(), muted, handle(&promise), state, data);
    leave_qctx(cx, entered);
}

// The job queue.

/// What a `JS::JobQueue*` points to: mozjs's `RustJobQueue`, whose `draining` flag Servo reads.
type JobQueue = crate::glue::RustJobQueue;

pub(crate) unsafe fn CreateJobQueue(
    aTraps: *const crate::glue::JobQueueTraps,
) -> *mut crate::glue::RustJobQueue {
    Box::into_raw(Box::new(JobQueue {
        vtable_: ptr::null(),
        mTraps: *aTraps,
        draining: false,
    }))
}

pub(crate) unsafe fn DeleteJobQueue(queue: *mut crate::glue::RustJobQueue) {
    // Like SpiderMonkey, the runtime must not run jobs with a deleted queue still installed.
    drop(Box::from_raw(queue));
}

pub(crate) unsafe fn SetJobQueue(cx: *mut root::JSContext, queue: *mut root::JS::JobQueue) {
    let cx = Cx::from_raw(cx);
    cx.hooks.job_queue.set(queue as *mut JobQueue);
    let opaque = cx.as_raw() as *mut c_void;
    if queue.is_null() {
        q::JS_SetEnqueueJobHook(cx.rt, None, opaque);
        q::JS_SetHostMakeJobCallbackHook(cx.rt, None, opaque);
    } else {
        q::JS_SetEnqueueJobHook(cx.rt, Some(enqueue_job), opaque);
        q::JS_SetHostMakeJobCallbackHook(cx.rt, Some(make_job_callback), opaque);
    }
}

pub(crate) unsafe fn GetJobQueue(cx: *mut root::JSContext) -> *mut root::JS::JobQueue {
    Cx::from_raw(cx).hooks.job_queue.get() as *mut root::JS::JobQueue
}

pub(crate) unsafe fn UseInternalJobQueues(cx: *mut root::JSContext) -> bool {
    SetJobQueue(cx, ptr::null_mut());
    true
}

pub(crate) unsafe fn RunJobs(cx: *mut root::JSContext) {
    let cx = Cx::from_raw(cx);
    let queue = cx.hooks.job_queue.get();
    if !queue.is_null() {
        if let Some(run_jobs) = (*queue).mTraps.runJobs {
            run_jobs(cx.as_raw());
        }
        return;
    }
    loop {
        let mut ctx = ptr::null_mut();
        let r = q::JS_ExecutePendingJob(cx.rt, &mut ctx);
        if r == 0 {
            break;
        }
        if r < 0 {
            // SpiderMonkey's internal queue reports and clears job exceptions.
            let e = q::JS_GetException(cx.base_ctx);
            q::JS_FreeValueRT(cx.rt, e);
        }
    }
}

pub(crate) unsafe fn JobQueueIsEmpty(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn JobQueueMayNotBeEmpty(cx: *mut root::JSContext) {
    let _ = cx;
}

// The microtask queue. As in SpiderMonkey 153, it belongs to the context and the embedder runs
// it at its microtask checkpoints. Promise jobs (patch Q4's hook) become `MicroTask` objects,
// SpiderMonkey's "JS microtasks"; any other entry is the embedder's own, and only passed back.
// Both share one queue, so their order is kept.

/// Opaque data of `MicroTask` objects: a QuickJS promise job.
struct MicroTaskData {
    job_func: q::JSJobFunc,
    /// Counted; taken when the job runs. Only moved in and out of the cell, never borrowed.
    args: Cell<Vec<q::JSValue>>,
    /// Held (`hold_realm`) and marked: the realm the job was enqueued in, which it runs in.
    realm: *mut RealmData,
    /// The incumbent global (counted), or `JS_UNDEFINED`: for a reaction, when it was created
    /// (Q32), for other jobs when they were enqueued.
    incumbent: q::JSValue,
    /// The promise of the reaction (Q30), for its user input state (counted), or `JS_UNDEFINED`.
    promise: q::JSValue,
}

static MICROTASK_CLASS: std::sync::OnceLock<q::JSClassID> = std::sync::OnceLock::new();

unsafe fn microtask_class(rt: *mut q::JSRuntime) -> q::JSClassID {
    let id = *MICROTASK_CLASS.get_or_init(|| {
        let mut id = 0;
        q::JS_NewClassID(&mut id);
        id
    });
    if q::JS_IsRegisteredClass(rt, id) == 0 {
        let def = q::JSClassDef {
            class_name: c"MicroTask".as_ptr(),
            finalizer: Some(microtask_finalize),
            gc_mark: Some(microtask_gc_mark),
            call: None,
            exotic: ptr::null_mut(),
        };
        assert_eq!(q::JS_NewClass(rt, id, &def), 0);
    }
    id
}

unsafe fn microtask_data<'a>(task: *mut root::JSObject) -> Option<&'a MicroTaskData> {
    let id = *MICROTASK_CLASS.get()?;
    (q::JS_GetOpaque(obj_val(task), id) as *const MicroTaskData).as_ref()
}

unsafe extern "C" fn microtask_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = q::JS_GetOpaque(val, *MICROTASK_CLASS.get().unwrap()) as *mut MicroTaskData;
    if !data.is_null() {
        let data = Box::from_raw(data);
        let args = data.args.take();
        for v in args.into_iter().chain([data.incumbent, data.promise]) {
            q::JS_FreeValueRT(rt, v);
        }
        super::release_realm(data.realm);
    }
}

unsafe extern "C" fn microtask_gc_mark(
    rt: *mut q::JSRuntime,
    val: q::JSValue,
    mark: q::JS_MarkFunc,
) {
    let data = q::JS_GetOpaque(val, *MICROTASK_CLASS.get().unwrap()) as *mut MicroTaskData;
    if let Some(data) = data.as_ref() {
        let args = data.args.take();
        for &v in args.iter().chain([&data.incumbent, &data.promise]) {
            if v.has_ref_count() {
                q::JS_MarkValue(rt, v, mark);
            }
        }
        data.args.set(args);
        super::mark_realm(rt, data.realm, mark);
    }
}

/// Frees the queued microtasks, before the runtime is destroyed.
pub(crate) unsafe fn free_microtasks(cx: &Cx) {
    for task in cx.hooks.microtasks.take() {
        q::JS_FreeValueRT(cx.rt, task);
    }
}

/// The incumbent global from the queue's `getHostDefinedData` (null if none), or `None` if it
/// failed.
unsafe fn incumbent_global(cx: &Cx, queue: &JobQueue) -> Option<*mut root::JSObject> {
    let mut incumbent: *mut root::JSObject = ptr::null_mut();
    let mut host_defined_data: *mut root::JSObject = ptr::null_mut();
    if let Some(get_host_defined_data) = queue.mTraps.getHostDefinedData {
        let ok = get_host_defined_data(
            cx.as_raw(),
            root::JS::MutableHandle::from_marked_location(&mut incumbent),
            root::JS::MutableHandle::from_marked_location(&mut host_defined_data),
        );
        if !ok {
            return None;
        }
    }
    Some(incumbent)
}

/// Patch Q32's hook, HostMakeJobCallback: the incumbent global when `then()` or `await` creates
/// a reaction, as SpiderMonkey takes it (`JS_NULL` for none, `JS_UNDEFINED` if it failed).
unsafe extern "C" fn make_job_callback(ctx: *mut q::JSContext, opaque: *mut c_void) -> q::JSValue {
    let cx = hook_cx(opaque);
    let queue = cx.hooks.job_queue.get();
    if queue.is_null() {
        return q::JS_UNDEFINED;
    }
    let entered = enter_qctx(cx, ctx);
    let mark = cx.pool_mark();
    let incumbent = incumbent_global(cx, &*queue);
    leave_qctx(cx, entered);
    let incumbent = match incumbent {
        Some(incumbent) if !incumbent.is_null() => q::JS_DupValueRT(cx.rt, obj_val(incumbent)),
        Some(_) => q::JS_NULL,
        None => q::JS_UNDEFINED,
    };
    // What the trap pooled (its out-parameters) would otherwise stay until the task ends, one
    // entry per `then()` or `await`.
    cx.drain_pool(mark);
    incumbent
}

/// Patch Q4's hook: queues a QuickJS promise job as a `MicroTask`.
unsafe extern "C" fn enqueue_job(
    ctx: *mut q::JSContext,
    job_func: q::JSJobFunc,
    argc: c_int,
    argv: *mut q::JSValue,
    promise: q::JSValue,
    host_defined: q::JSValue,
    opaque: *mut c_void,
) -> c_int {
    let cx = hook_cx(opaque);
    let queue = cx.hooks.job_queue.get();
    assert!(!queue.is_null());
    let realm = q::JS_GetContextOpaque(ctx) as *mut RealmData;
    let entered = enter_qctx(cx, ctx);
    let result = enqueue_microtask(
        cx,
        &*queue,
        realm,
        job_func,
        argc,
        argv,
        promise,
        host_defined,
    );
    leave_qctx(cx, entered);
    result
}

unsafe fn enqueue_microtask(
    cx: &Cx,
    queue: &JobQueue,
    realm: *mut RealmData,
    job_func: q::JSJobFunc,
    argc: c_int,
    argv: *mut q::JSValue,
    promise: q::JSValue,
    host_defined: q::JSValue,
) -> c_int {
    // A reaction's incumbent was taken when it was created (Q32); other jobs take it now.
    let incumbent = match host_defined.tag() {
        q::JS_TAG_OBJECT => q::JS_DupValueRT(cx.rt, host_defined),
        q::JS_TAG_NULL => q::JS_UNDEFINED,
        _ => {
            let mark = cx.pool_mark();
            let incumbent = incumbent_global(cx, queue)
                .map(|incumbent| q::JS_DupValueRT(cx.rt, object_or_undefined(incumbent)));
            cx.drain_pool(mark);
            match incumbent {
                Some(incumbent) => incumbent,
                None => return -1,
            }
        }
    };
    let task = q::JS_NewObjectProtoClass(cx.qctx(), q::JS_NULL, microtask_class(cx.rt));
    if task.tag() == q::JS_TAG_EXCEPTION {
        q::JS_FreeValueRT(cx.rt, incumbent);
        return -1;
    }
    let data = Box::new(MicroTaskData {
        job_func,
        args: Cell::new(
            (0..argc as usize)
                .map(|i| q::JS_DupValueRT(cx.rt, *argv.add(i)))
                .collect(),
        ),
        realm: super::hold_realm(realm),
        incumbent,
        // The promise of the reaction (Q30), for its user input state.
        promise: if promise.tag() == q::JS_TAG_OBJECT {
            q::JS_DupValueRT(cx.rt, promise)
        } else {
            q::JS_UNDEFINED
        },
    });
    q::JS_SetOpaque(task, Box::into_raw(data) as *mut c_void);
    cx.hooks.microtasks.push_back(task);
    0
}

pub(crate) unsafe fn EnqueueMicroTask(
    cx: *mut root::JSContext,
    entry: *const root::JS::GenericMicroTask,
) -> bool {
    let cx = Cx::from_raw(cx);
    let entry = (*entry).to_qjs();
    cx.hooks
        .microtasks
        .push_back(q::JS_DupValueRT(cx.rt, entry));
    true
}

pub(crate) unsafe fn PrependMicroTask(
    cx: *mut root::JSContext,
    entry: *const root::JS::GenericMicroTask,
) -> bool {
    let cx = Cx::from_raw(cx);
    let entry = (*entry).to_qjs();
    cx.hooks
        .microtasks
        .push_front(q::JS_DupValueRT(cx.rt, entry));
    true
}

pub(crate) unsafe fn HasAnyMicroTasks(cx: *mut root::JSContext) -> bool {
    !Cx::from_raw(cx).hooks.microtasks.is_empty()
}

pub(crate) unsafe fn HasRegularMicroTasks(cx: *mut root::JSContext) -> bool {
    HasAnyMicroTasks(cx)
}

pub(crate) unsafe fn GetRegularMicroTaskCount(cx: *mut root::JSContext) -> usize {
    Cx::from_raw(cx).hooks.microtasks.len()
}

pub(crate) unsafe fn PeekNextMicroTask(cx: *mut root::JSContext) -> root::JS::GenericMicroTask {
    let cx = Cx::from_raw(cx);
    match cx.hooks.microtasks.front() {
        Some(entry) => cx.retain(JSVal::from_qjs(entry)),
        None => UndefinedValue(),
    }
}

pub(crate) unsafe fn DequeueNextRegularMicroTask(
    cx: *mut root::JSContext,
) -> root::JS::GenericMicroTask {
    let cx = Cx::from_raw(cx);
    match cx.hooks.microtasks.pop_front() {
        Some(entry) => cx.adopt(entry),
        None => UndefinedValue(),
    }
}

pub(crate) unsafe fn JS_DequeueNextMicroTask(
    cx: *mut root::JSContext,
    task: root::JS::MutableHandle<root::JS::GenericMicroTask>,
) {
    *task.ptr = DequeueNextRegularMicroTask(cx);
}

pub(crate) unsafe fn IsJSMicroTask(hv: *const root::JS::GenericMicroTask) -> bool {
    (*hv).is_object()
}

pub(crate) unsafe fn ToMaybeWrappedJSMicroTask(
    genericMicroTask: *const root::JS::GenericMicroTask,
) -> *mut root::JS::JSMicroTask {
    if (*genericMicroTask).is_object() {
        (*genericMicroTask).to_object()
    } else {
        ptr::null_mut()
    }
}

pub(crate) unsafe fn GetExecutionGlobalFromJSMicroTask(
    entry: *mut root::JS::JSMicroTask,
) -> *mut root::JSObject {
    match microtask_data(entry) {
        Some(data) if !data.realm.is_null() => (*data.realm).global,
        _ => ptr::null_mut(),
    }
}

pub(crate) unsafe fn MaybeGetHostDefinedDataFromJSMicroTask(
    entry: *mut root::JS::JSMicroTask,
    incumbentGlobal: root::JS::MutableHandleObject,
    optionalHostDefinedData: root::JS::MutableHandleObject,
) -> bool {
    *optionalHostDefinedData.ptr = ptr::null_mut();
    *incumbentGlobal.ptr = match microtask_data(entry) {
        Some(data) if data.incumbent.tag() == q::JS_TAG_OBJECT => val_obj(data.incumbent),
        _ => ptr::null_mut(),
    };
    true
}

pub(crate) unsafe fn MaybeGetPromiseFromJSMicroTask(
    entry: *mut root::JS::JSMicroTask,
) -> *mut root::JSObject {
    match microtask_data(entry) {
        Some(data) if data.promise.tag() == q::JS_TAG_OBJECT => val_obj(data.promise),
        _ => ptr::null_mut(),
    }
}

/// Runs the job once; false with the job's exception pending if it threw.
pub(crate) unsafe fn RunJSMicroTask(
    cx: *mut root::JSContext,
    entry: root::JS::Handle<*mut root::JS::JSMicroTask>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let Some(data) = microtask_data(*entry.ptr) else {
        q::JS_ThrowTypeError(cx.qctx(), c"not a microtask".as_ptr());
        return false;
    };
    let mut args = data.args.take();
    // Every QuickJS job takes 2 to 5 arguments, so none means the microtask ran already. The API
    // runs each dequeued microtask once; a second run would call the job without its arguments.
    debug_assert!(!args.is_empty(), "microtask already ran");
    let ctx = if data.realm.is_null() {
        cx.base_ctx
    } else {
        (*data.realm).qctx
    };
    let job_func = data.job_func.unwrap();
    let result = job_func(ctx, args.len() as c_int, args.as_mut_ptr());
    for v in args {
        q::JS_FreeValueRT(cx.rt, v);
    }
    if result.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    q::JS_FreeValueRT(cx.rt, result);
    true
}

// Interrupts.

pub(crate) unsafe fn JS_AddInterruptCallback(
    cx: *mut root::JSContext,
    callback: root::JSInterruptCallback,
) -> bool {
    let cx = Cx::from_raw(cx);
    cx.hooks.interrupt_callbacks.push(callback);
    q::JS_SetInterruptHandler(cx.rt, Some(interrupt_handler), cx.as_raw() as *mut c_void);
    true
}

/// Thread-safe: only touches the shared request flag.
unsafe fn request_interrupt(cx: *mut root::JSContext) {
    (*(cx as *const Cx))
        .hooks
        .interrupt_requested
        .store(true, Ordering::SeqCst);
}

pub(crate) unsafe fn JS_RequestInterruptCallback(cx: *mut root::JSContext) {
    request_interrupt(cx);
}

pub(crate) unsafe fn JS_RequestInterruptCallbackCanWait(cx: *mut root::JSContext) {
    request_interrupt(cx);
}

/// QuickJS polls this while running script and on every call; a positive result terminates the
/// script with an uncatchable exception. While a promise operation defers interrupts and no script
/// runs in it, a requested interrupt stays pending, and the negative result makes QuickJS poll again
/// at its next check (Q36), so a loop that keeps creating promises can't defer it forever.
unsafe extern "C" fn interrupt_handler(rt: *mut q::JSRuntime, opaque: *mut c_void) -> c_int {
    let _ = rt;
    let cx = hook_cx(opaque);
    if !cx.hooks.interrupt_requested.load(Ordering::SeqCst) {
        return 0;
    }
    if let Some(mark) = cx.hooks.interrupts_deferred_at.get() {
        if !super::error::script_since(cx, mark) {
            return -1;
        }
    }
    if !cx.hooks.interrupt_requested.swap(false, Ordering::SeqCst) {
        return 0;
    }
    let mut entered = false;
    if cx.realm_stack.is_empty() {
        let realm = super::error::innermost_script_realm(cx);
        if !realm.is_null() {
            entered = enter_qctx(cx, realm);
        }
    }
    let callbacks = cx.hooks.interrupt_callbacks.to_vec();
    let mut keep_running = true;
    for callback in callbacks.into_iter().flatten() {
        keep_running &= callback(cx.as_raw());
    }
    leave_qctx(cx, entered);
    (!keep_running) as c_int
}

// Security callbacks.

pub(crate) unsafe fn JS_SetSecurityCallbacks(
    cx: *mut root::JSContext,
    callbacks: *const root::JSSecurityCallbacks,
) {
    let cx = Cx::from_raw(cx);
    cx.hooks.security_callbacks.set(callbacks);
    let checks = !callbacks.is_null() && (*callbacks).contentSecurityPolicyAllows.is_some();
    let hook: q::JSCanCompileStringsHook = if checks {
        Some(can_compile_strings)
    } else {
        None
    };
    q::JS_SetCanCompileStringsHook(cx.rt, hook, cx.as_raw() as *mut c_void);
    let code_for_eval_hook: q::JSCodeForEvalHook =
        if !callbacks.is_null() && (*callbacks).codeForEvalGets.is_some() {
            Some(code_for_eval)
        } else {
            None
        };
    q::JS_SetCodeForEvalHook(cx.rt, code_for_eval_hook, cx.as_raw() as *mut c_void);
}

pub(crate) unsafe fn JS_GetSecurityCallbacks(
    cx: *mut root::JSContext,
) -> *const root::JSSecurityCallbacks {
    Cx::from_raw(cx).hooks.security_callbacks.get()
}

pub(crate) unsafe fn JS_SetTrustedPrincipals(
    cx: *mut root::JSContext,
    prin: *mut root::JSPrincipals,
) {
    // There are no system compartments to select.
    let _ = (cx, prin);
}

/// Patch Q22's hook: SpiderMonkey's `codeForEvalGets`, the code of an object passed to eval (a
/// TrustedScript).
unsafe extern "C" fn code_for_eval(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    opaque: *mut c_void,
) -> q::JSValue {
    let cx = hook_cx(opaque);
    let Some(gets) = (*cx.hooks.security_callbacks.get()).codeForEvalGets else {
        return q::JS_UNDEFINED;
    };
    let entered = enter_qctx(cx, ctx);
    let code = val_obj(obj);
    let mut out: *mut root::JSString = ptr::null_mut();
    let ok = gets(
        cx.as_raw(),
        handle(&code),
        root::JS::MutableHandle::from_marked_location(&mut out),
    );
    leave_qctx(cx, entered);
    if !ok {
        if q::JS_HasException(ctx) == 0 {
            q::JS_ThrowInternalError(ctx, c"script terminated".as_ptr());
            q::JS_SetUncatchableException(ctx, 1);
        }
        return q::JS_EXCEPTION;
    }
    if out.is_null() {
        return q::JS_UNDEFINED;
    }
    q::JS_DupValueRT(
        cx.rt,
        q::JSValue::mkptr(q::JS_TAG_STRING, out as *mut c_void),
    )
}

/// Patch Q5's hook: SpiderMonkey's `contentSecurityPolicyAllows` for eval and `Function`.
unsafe extern "C" fn can_compile_strings(
    ctx: *mut q::JSContext,
    kind: q::JSCompileKindEnum,
    code: q::JSValue,
    argc: c_int,
    arg_strings: *mut q::JSValue,
    args: *mut q::JSValue,
    opaque: *mut c_void,
) -> c_int {
    let cx = hook_cx(opaque);
    let Some(allows) = (*cx.hooks.security_callbacks.get()).contentSecurityPolicyAllows else {
        return 1;
    };
    let entered = enter_qctx(cx, ctx);
    let code_string = cx.retain(JSVal::from_qjs(code)).to_string();
    let mut parameter_strings: Vec<*mut root::JSString> = Vec::new();
    let mut parameter_args: Vec<JSVal> = Vec::new();
    let (compilation_type, body_string, body_arg) = if kind == q::JS_COMPILE_FUNCTION {
        let params = (argc as usize).saturating_sub(1);
        for i in 0..params {
            parameter_strings.push(cx.retain(JSVal::from_qjs(*arg_strings.add(i))).to_string());
            parameter_args.push(JSVal::from_qjs(*args.add(i)));
        }
        let (body_string, body_arg) = if argc > 0 {
            let body = cx
                .retain(JSVal::from_qjs(*arg_strings.add(params)))
                .to_string();
            (body, JSVal::from_qjs(*args.add(params)))
        } else {
            let empty = cx
                .adopt(q::JS_NewStringLen(ctx, c"".as_ptr(), 0))
                .to_string();
            (empty, UndefinedValue())
        };
        (root::JS::CompilationType::Function, body_string, body_arg)
    } else {
        let compilation_type = if kind == q::JS_COMPILE_DIRECT_EVAL {
            root::JS::CompilationType::DirectEval
        } else {
            root::JS::CompilationType::IndirectEval
        };
        // The argument itself, which differs from the code for an object such as a TrustedScript.
        let body_arg = if argc > 0 { *args } else { code };
        (compilation_type, code_string, JSVal::from_qjs(body_arg))
    };
    let mut can_compile = false;
    let ok = allows(
        cx.as_raw(),
        root::JS::RuntimeCode::JS,
        handle(&code_string),
        compilation_type,
        handle(&parameter_strings as *const Vec<_> as *const _),
        handle(&body_string),
        handle(&parameter_args as *const Vec<_> as *const _),
        handle(&body_arg),
        &mut can_compile,
    );
    leave_qctx(cx, entered);
    if !ok {
        if q::JS_HasException(ctx) == 0 {
            q::JS_ThrowInternalError(ctx, c"script terminated".as_ptr());
            q::JS_SetUncatchableException(ctx, 1);
        }
        return -1;
    }
    can_compile as c_int
}
