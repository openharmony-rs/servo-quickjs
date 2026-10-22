/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Promises, the job queue, the rejection tracker, interrupts and CSP checks on QuickJS.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use mozjs::context::{JSContext, RawJSContext};
use mozjs::conversions::jsstr_to_string;
use mozjs::gc::StackGCVector;
use mozjs::glue::{CreateJobQueue, DeleteJobQueue, JobQueueTraps};
use mozjs::jsapi::{
    self, CallArgs, CompilationType, Handle, HandleObject, HandleString, HandleValue, JSObject,
    JSSecurityCallbacks, JSString, MutableHandleObject, MutableHandleString, MutableHandleValue,
    OnNewGlobalHookOption, PromiseRejectionHandlingState, PromiseState,
    PromiseUserInputEventHandlingState, RuntimeCode, Value,
};
use mozjs::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::JS_NewGlobalObject;
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine,
    RealmOptions, Runtime, ThreadSafeJSContext, SIMPLE_GLOBAL_CLASS,
};

/// Runs `f` in the realm of a new global of a new runtime.
fn with_global(
    engine: &JSEngine,
    f: impl FnOnce(ThreadSafeJSContext, &mut JSContext, SafeHandleObject),
) {
    let mut runtime = Runtime::new(engine.handle());
    let thread_safe = runtime.thread_safe_js_context();
    let context = runtime.cx();
    unsafe {
        rooted!(&in(context) let global = JS_NewGlobalObject(
            context,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = AutoRealm::new_from_handle(context, global.handle());
        let (global, realm) = realm.global_and_reborrow();
        f(thread_safe, realm, global);
    }
}

fn eval(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> Result<JSVal, ()> {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"test.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options)?;
    Ok(rval.get())
}

fn eval_bool(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> bool {
    let v = eval(cx, global, source).unwrap_or_else(|_| panic!("{source} threw"));
    assert!(v.is_boolean(), "{source} is not a boolean");
    v.to_boolean()
}

unsafe fn handle<T>(ptr: &T) -> Handle<T> {
    Handle::from_marked_location(ptr)
}

unsafe fn new_promise(cx: &mut JSContext, executor: *mut JSObject) -> *mut JSObject {
    let executor = executor;
    let p = jsapi::NewPromiseObject(cx.raw_cx(), handle(&executor));
    assert!(!p.is_null());
    p
}

unsafe fn promise_state(p: *mut JSObject) -> PromiseState {
    jsapi::GetPromiseState(handle(&p))
}

unsafe fn promise_result(p: *mut JSObject) -> JSVal {
    let mut v = UndefinedValue();
    mozjs::glue::JS_GetPromiseResult(handle(&p), MutableHandleValue::from_marked_location(&mut v));
    v
}

/// Sets `global[name] = value`.
unsafe fn set_global(
    cx: &mut JSContext,
    global: SafeHandleObject,
    name: &std::ffi::CStr,
    value: JSVal,
) {
    let global = global.get();
    assert!(jsapi::JS_SetProperty(
        cx.raw_cx(),
        handle(&global),
        name.as_ptr(),
        handle(&value)
    ));
}

fn resolve_and_reject(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        let p = new_promise(cx, ptr::null_mut());
        assert!(jsapi::IsPromiseObject(handle(&p)));
        assert_eq!(promise_state(p), PromiseState::Pending);
        assert!(jsapi::ResolvePromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(42))
        ));
        assert_eq!(promise_state(p), PromiseState::Fulfilled);
        assert_eq!(promise_result(p).to_int32(), 42);
        // Settling again is a no-op.
        assert!(jsapi::RejectPromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(1))
        ));
        assert_eq!(promise_state(p), PromiseState::Fulfilled);

        let p = new_promise(cx, ptr::null_mut());
        assert!(jsapi::RejectPromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(7))
        ));
        assert_eq!(promise_state(p), PromiseState::Rejected);
        assert_eq!(promise_result(p).to_int32(), 7);

        // Resolving with a thenable adopts its state through jobs.
        let p = jsapi::CallOriginalPromiseResolve(cx.raw_cx(), handle(&Int32Value(5)));
        assert_eq!(promise_state(p), PromiseState::Fulfilled);
        assert_eq!(promise_result(p).to_int32(), 5);
        let same = jsapi::CallOriginalPromiseResolve(cx.raw_cx(), handle(&ObjectValue(p)));
        assert_eq!(same, p, "%Promise%.resolve(promise) is the promise itself");
        let p = jsapi::CallOriginalPromiseReject(cx.raw_cx(), handle(&Int32Value(6)));
        assert_eq!(promise_state(p), PromiseState::Rejected);

        let executor = eval(cx, global, "(function(resolve) { resolve(3); })").unwrap();
        let p = new_promise(cx, executor.to_object());
        assert_eq!(promise_state(p), PromiseState::Fulfilled);
        assert_eq!(promise_result(p).to_int32(), 3);
        let executor = eval(cx, global, "(function() { throw 4; })").unwrap();
        let p = new_promise(cx, executor.to_object());
        assert_eq!(promise_state(p), PromiseState::Rejected);
        assert_eq!(promise_result(p).to_int32(), 4);
        // Promises settled from Rust are visible to script.
        set_global(cx, global, c"p", ObjectValue(p));
        assert!(eval_bool(cx, global, "p instanceof Promise"));

        let plain = eval(cx, global, "({})").unwrap().to_object();
        assert!(!jsapi::IsPromiseObject(handle(&plain)));
        assert!(!jsapi::JS_IsExceptionPending(cx.raw_cx()));
    }
}

// The user input state of the promise of each job run, as Servo reads it.
thread_local!(static STATES: RefCell<Vec<PromiseUserInputEventHandlingState>> = const { RefCell::new(Vec::new()) });

// The incumbent global the job queue reports, as Servo's settings stack would.
thread_local!(static INCUMBENT: std::cell::Cell<*mut JSObject> = const { std::cell::Cell::new(ptr::null_mut()) });
// The incumbent of each job run.
thread_local!(static INCUMBENTS: RefCell<Vec<*mut JSObject>> = const { RefCell::new(Vec::new()) });

unsafe extern "C" fn get_host_defined_data(
    _: *mut RawJSContext,
    incumbent: MutableHandleObject,
    data: MutableHandleObject,
) -> bool {
    incumbent.set(INCUMBENT.with(|i| i.get()));
    data.set(ptr::null_mut());
    true
}

/// Runs the microtask queue like Servo's checkpoint.
unsafe extern "C" fn run_jobs(cx: *mut RawJSContext) {
    while jsapi::HasAnyMicroTasks(cx) {
        let mut task = UndefinedValue();
        mozjs::glue::JS_DequeueNextMicroTask(
            cx,
            MutableHandleValue::from_marked_location(&mut task),
        );
        assert!(jsapi::IsJSMicroTask(&task));
        let task = jsapi::ToMaybeWrappedJSMicroTask(&task);
        let promise = jsapi::MaybeGetPromiseFromJSMicroTask(task);
        STATES.with(|s| {
            s.borrow_mut().push(if promise.is_null() {
                PromiseUserInputEventHandlingState::DontCare
            } else {
                jsapi::GetPromiseUserInputEventHandlingState(promise)
            })
        });
        assert!(!jsapi::GetExecutionGlobalFromJSMicroTask(task).is_null());
        let mut incumbent = ptr::null_mut();
        let mut data = ptr::null_mut();
        assert!(jsapi::MaybeGetHostDefinedDataFromJSMicroTask(
            task,
            MutableHandleObject::from_marked_location(&mut incumbent),
            MutableHandleObject::from_marked_location(&mut data),
        ));
        INCUMBENTS.with(|i| i.borrow_mut().push(incumbent));
        assert!(jsapi::RunJSMicroTask(cx, handle(&task)));
    }
}

static TRAPS: JobQueueTraps = JobQueueTraps {
    getHostDefinedData: Some(get_host_defined_data),
    getHostDefinedGlobal: None,
    runJobs: Some(run_jobs),
    traceNonGCThingMicroTask: None,
};

thread_local!(static FULFILLED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) });

unsafe extern "C" fn on_fulfilled(_: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    FULFILLED.with(|f| f.borrow_mut().push(args.get(0).to_int32()));
    args.rval().set(UndefinedValue());
    true
}

fn job_queue(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        let job_queue = CreateJobQueue(&TRAPS);
        jsapi::SetJobQueue(cx.raw_cx(), job_queue as *mut jsapi::JobQueue);
        assert_eq!(
            jsapi::GetJobQueue(cx.raw_cx()),
            job_queue as *mut jsapi::JobQueue
        );
        let queued = |cx: &mut JSContext| jsapi::GetRegularMicroTaskCount(cx.raw_cx());

        assert!(eval_bool(
            cx,
            global,
            "
            var log = [];
            Promise.resolve(1).then(v => log.push(v)).then(() => log.push(3));
            Promise.resolve(2).then(v => log.push(v));
            (async () => { await null; log.push('a'); })();
            log.push('sync');
            log.join() === 'sync'
        "
        ));
        assert_eq!(queued(cx), 3);
        jsapi::RunJobs(cx.raw_cx());
        assert!(!jsapi::HasAnyMicroTasks(cx.raw_cx()));
        assert!(eval_bool(cx, global, "log.join() === 'sync,1,2,a,3'"));

        // A promise's user input state goes to the promises then() derives from it, and reaction
        // jobs are enqueued with them; awaiting it doesn't propagate the state (Q30).
        let p = new_promise(cx, ptr::null_mut());
        assert!(jsapi::SetPromiseUserInputEventHandlingState(
            handle(&p),
            PromiseUserInputEventHandlingState::HadUserInteractionAtCreation
        ));
        set_global(cx, global, c"interacted", ObjectValue(p));
        eval(
            cx,
            global,
            "interacted.then(() => 1).then(() => 2); (async () => { await interacted; })();",
        )
        .unwrap();
        STATES.with(|s| s.borrow_mut().clear());
        assert!(jsapi::ResolvePromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(1))
        ));
        jsapi::RunJobs(cx.raw_cx());
        use PromiseUserInputEventHandlingState::{DontCare, HadUserInteractionAtCreation as Had};
        assert_eq!(STATES.with(|s| s.borrow().clone()), [Had, DontCare, Had]);

        // Arrays filled internally don't run inherited setters.
        assert!(eval_bool(
            cx,
            global,
            "
            Object.defineProperty(Array.prototype, 1, { get() { throw 1; }, configurable: true });
            var reached = false;
            Promise.resolve(1).then(() => reached = true);
            var a = new Array(4, 5, 6);
            var json = JSON.stringify({ x: 1, y: 2 }, ['x', 'y']);
            delete Array.prototype[1];
            a.join() === '4,5,6' && json === '{\"x\":1,\"y\":2}'
        "
        ));
        jsapi::RunJobs(cx.raw_cx());
        assert!(eval_bool(cx, global, "reached"));

        // Reactions added from Rust, settled from Rust.
        let p = new_promise(cx, ptr::null_mut());
        let f = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
            cx.raw_cx(),
            Some(on_fulfilled),
            1,
            0,
            c"onFulfilled".as_ptr(),
        ));
        let none: *mut JSObject = ptr::null_mut();
        assert!(jsapi::AddPromiseReactions(
            cx.raw_cx(),
            handle(&p),
            handle(&f),
            handle(&none)
        ));
        assert!(jsapi::GetPromiseIsHandled(handle(&p)));
        let before = queued(cx);
        assert!(jsapi::ResolvePromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(9))
        ));
        assert_eq!(queued(cx), before + 1);
        assert!(FULFILLED.with(|f| f.borrow().is_empty()));
        jsapi::RunJobs(cx.raw_cx());
        assert_eq!(FULFILLED.with(|f| f.borrow().clone()), vec![9]);

        // Jobs belong to the realm of their handler: Servo's entry realm for them.
        rooted!(&in(cx) let other = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let other_val = ObjectValue(other.get());
        assert!(jsapi::JS_DefineProperty(
            cx.raw_cx(),
            handle(&global.get()),
            c"other".as_ptr(),
            handle(&other_val),
            0
        ));
        for (source, realm) in [
            (
                "Promise.resolve(1).then(other.Function('return 1'))",
                other.get(),
            ),
            (
                "Promise.resolve({ then: other.Function('resolve', 'resolve(1)') })",
                other.get(),
            ),
            ("Promise.resolve(1).then(() => 1)", global.get()),
        ] {
            eval(cx, global, source).unwrap();
            assert_eq!(queued(cx), 1);
            let task = jsapi::PeekNextMicroTask(cx.raw_cx());
            let task = jsapi::ToMaybeWrappedJSMicroTask(&task);
            assert_eq!(
                jsapi::GetExecutionGlobalFromJSMicroTask(task),
                realm,
                "{source}"
            );
            jsapi::RunJobs(cx.raw_cx());
        }

        // A reaction's incumbent is the one when then() or await created it (HostMakeJobCallback,
        // as in SpiderMonkey 153); a thenable job's is the one when it was enqueued.
        let p = new_promise(cx, ptr::null_mut());
        set_global(cx, global, c"pending", ObjectValue(p));
        INCUMBENT.with(|i| i.set(other.get()));
        eval(
            cx,
            global,
            "pending.then(() => {}); (async () => { await pending; })();",
        )
        .unwrap();
        INCUMBENT.with(|i| i.set(global.get()));
        let thenable = eval(cx, global, "({ then(resolve) { resolve(); } })").unwrap();
        INCUMBENTS.with(|i| i.borrow_mut().clear());
        assert!(jsapi::ResolvePromise(
            cx.raw_cx(),
            handle(&p),
            handle(&thenable)
        ));
        jsapi::RunJobs(cx.raw_cx());
        let incumbents = INCUMBENTS.with(|i| i.borrow().clone());
        assert_eq!(incumbents[0], global.get(), "thenable job");
        assert_eq!(&incumbents[1..3], [other.get(), other.get()], "reactions");
        INCUMBENT.with(|i| i.set(ptr::null_mut()));

        // The embedder's own tasks share the queue, in order.
        let own = mozjs::jsval::PrivateValue(7 as *const c_void);
        eval(cx, global, "Promise.resolve().then(() => {})").unwrap();
        assert!(jsapi::EnqueueMicroTask(cx.raw_cx(), &own));
        assert_eq!(queued(cx), 2);
        let mut task = UndefinedValue();
        mozjs::glue::JS_DequeueNextMicroTask(
            cx.raw_cx(),
            MutableHandleValue::from_marked_location(&mut task),
        );
        assert!(jsapi::IsJSMicroTask(&task));
        mozjs::glue::JS_DequeueNextMicroTask(
            cx.raw_cx(),
            MutableHandleValue::from_marked_location(&mut task),
        );
        assert!(!jsapi::IsJSMicroTask(&task));
        assert_eq!(task.to_private(), 7 as *const c_void);

        // Jobs that were never run are released with the runtime.
        eval(cx, global, "Promise.resolve().then(() => {})").unwrap();
        assert_eq!(queued(cx), 1);
        jsapi::SetJobQueue(cx.raw_cx(), ptr::null_mut());
        DeleteJobQueue(job_queue);

        // Without a job queue, jobs use QuickJS's internal queue.
        assert!(eval_bool(
            cx,
            global,
            "var done = false; Promise.resolve().then(() => done = true); !done"
        ));
        jsapi::RunJobs(cx.raw_cx());
        assert!(eval_bool(cx, global, "done"));
    }
}

thread_local!(static MUTED: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) });
thread_local!(static TRACKER_GLOBALS: RefCell<Vec<*mut JSObject>> = const { RefCell::new(Vec::new()) });

unsafe extern "C" fn track_rejection(
    cx: *mut RawJSContext,
    muted_errors: bool,
    promise: HandleObject,
    state: PromiseRejectionHandlingState,
    data: *mut c_void,
) {
    MUTED.with(|m| m.borrow_mut().push(muted_errors));
    TRACKER_GLOBALS.with(|g| g.borrow_mut().push(jsapi::CurrentGlobalOrNull(cx)));
    assert!(jsapi::IsPromiseObject(promise));
    let log = &*(data as *const RefCell<Vec<PromiseRejectionHandlingState>>);
    log.borrow_mut().push(state);
}

fn rejection_tracker(cx: &mut JSContext, global: SafeHandleObject) {
    let log: RefCell<Vec<PromiseRejectionHandlingState>> = Default::default();
    unsafe {
        jsapi::SetPromiseRejectionTrackerCallback(
            cx.raw_cx(),
            Some(track_rejection),
            &log as *const _ as *mut c_void,
        );
        let p = eval(cx, global, "var p = Promise.reject(1); p")
            .unwrap()
            .to_object();
        assert_eq!(*log.borrow(), [PromiseRejectionHandlingState::Unhandled]);
        assert!(!jsapi::GetPromiseIsHandled(handle(&p)));
        eval(cx, global, "p.catch(() => {})").unwrap();
        assert_eq!(
            *log.borrow(),
            [
                PromiseRejectionHandlingState::Unhandled,
                PromiseRejectionHandlingState::Handled
            ]
        );
        assert!(jsapi::GetPromiseIsHandled(handle(&p)));

        log.borrow_mut().clear();
        let p = new_promise(cx, ptr::null_mut());
        assert!(jsapi::RejectPromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(2))
        ));
        assert_eq!(*log.borrow(), [PromiseRejectionHandlingState::Unhandled]);
        assert!(jsapi::SetAnyPromiseIsHandled(cx.raw_cx(), handle(&p)));
        assert!(jsapi::GetPromiseIsHandled(handle(&p)));
        assert_eq!(
            *log.borrow(),
            [
                PromiseRejectionHandlingState::Unhandled,
                PromiseRejectionHandlingState::Handled
            ]
        );

        // Handled before rejection: no report.
        log.borrow_mut().clear();
        let p = new_promise(cx, ptr::null_mut());
        assert!(jsapi::SetAnyPromiseIsHandled(cx.raw_cx(), handle(&p)));
        assert!(jsapi::RejectPromise(
            cx.raw_cx(),
            handle(&p),
            handle(&Int32Value(3))
        ));
        assert!(log.borrow().is_empty());

        // Rejections in a script with muted errors, and in eval code it runs, are muted.
        MUTED.with(|m| m.borrow_mut().clear());
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let options = CompileOptionsWrapper::new(cx, c"cross-origin.js".to_owned(), 1);
        (*options.ptr)._base.mutedErrors_ = true;
        evaluate_script(
            cx,
            global,
            "Promise.reject(4); eval('Promise.reject(5)')",
            rval.handle_mut(),
            options,
        )
        .unwrap();
        eval(cx, global, "Promise.reject(6)").unwrap();
        assert_eq!(MUTED.with(|m| m.borrow().clone()), [true, true, false]);

        // The resolving functions of a promise run in the realm that created them.
        rooted!(&in(cx) let other = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let other_val = ObjectValue(other.get());
        assert!(jsapi::JS_DefineProperty(
            cx.raw_cx(),
            handle(&global.get()),
            c"other".as_ptr(),
            handle(&other_val),
            0
        ));
        TRACKER_GLOBALS.with(|g| g.borrow_mut().clear());
        eval(cx, global, "new other.Promise((_, reject) => reject(7))").unwrap();
        eval(cx, global, "Promise.reject(8)").unwrap();
        assert_eq!(
            TRACKER_GLOBALS.with(|g| g.borrow().clone()),
            [other.get(), global.get()]
        );
        jsapi::SetPromiseRejectionTrackerCallback(cx.raw_cx(), None, ptr::null_mut());
    }
}

static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
static KEEP_RUNNING: AtomicBool = AtomicBool::new(true);

unsafe extern "C" fn interrupt_callback(_: *mut RawJSContext) -> bool {
    INTERRUPTS.fetch_add(1, Ordering::SeqCst);
    KEEP_RUNNING.load(Ordering::SeqCst)
}

unsafe extern "C" fn do_nothing(_: *mut RawJSContext, _: u32, _: *mut Value) -> bool {
    true
}

fn interrupts(thread_safe: ThreadSafeJSContext, cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        assert!(jsapi::JS_AddInterruptCallback(
            cx.raw_cx(),
            Some(interrupt_callback)
        ));

        // A callback that returns true lets the script continue.
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        let v = eval(cx, global, "var i = 0; for (; i < 100000; i++) {} i").unwrap();
        assert_eq!(v.to_int32(), 100000);
        assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 1);

        // One that returns false terminates it, uncatchably.
        KEEP_RUNNING.store(false, Ordering::SeqCst);
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        assert!(eval(
            cx,
            global,
            "var caught = false; try { for (;;) {} } catch (e) { caught = true; }"
        )
        .is_err());
        assert!(!jsapi::JS_IsExceptionPending(cx.raw_cx()));
        assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 2);
        assert!(!eval_bool(cx, global, "caught"));

        // Requests from another thread.
        let requester = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            thread_safe.request_interrupt_callback();
        });
        assert!(
            eval(cx, global, "for (let i = 0; i < 2e8; i++) {}").is_err(),
            "the loop was not interrupted"
        );
        requester.join().unwrap();
        assert!(!jsapi::JS_IsExceptionPending(cx.raw_cx()));
        assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 3);

        // As in SpiderMonkey, a pending interrupt is only handled in script: creating promises
        // and adding reactions, which run only built-in and native functions here, succeeds and
        // leaves it pending (Q36).
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        let executor = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
            cx.raw_cx(),
            Some(do_nothing),
            2,
            0,
            c"executor".as_ptr(),
        ));
        let none: *mut JSObject = ptr::null_mut();
        let value = UndefinedValue();
        for _ in 0..20_000 {
            let p = new_promise(cx, executor);
            assert!(!jsapi::CallOriginalPromiseResolve(cx.raw_cx(), handle(&value)).is_null());
            assert!(!jsapi::CallOriginalPromiseReject(cx.raw_cx(), handle(&value)).is_null());
            assert!(jsapi::AddPromiseReactions(
                cx.raw_cx(),
                handle(&p),
                handle(&executor),
                handle(&none)
            ));
        }
        assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 3);
        assert!(eval(cx, global, "for (;;) {}").is_err());
        assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 4);
        KEEP_RUNNING.store(true, Ordering::SeqCst);
        assert_eq!(eval(cx, global, "1 + 1").unwrap().to_int32(), 2);
    }
}

/// A native that creates a promise, as Servo's DOM methods do.
unsafe extern "C" fn make_promise(cx: *mut RawJSContext, _: u32, _: *mut Value) -> bool {
    let executor = jsapi::JS_GetFunctionObject(jsapi::JS_NewFunction(
        cx,
        Some(do_nothing),
        2,
        0,
        c"executor".as_ptr(),
    ));
    !jsapi::NewPromiseObject(cx, handle(&executor)).is_null()
}

/// Where the promise operations leave a pending interrupt pending, it is still handled in script:
/// script they run themselves, and the script that runs after them (Q36).
fn deferred_interrupts(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        assert!(jsapi::JS_AddInterruptCallback(
            cx.raw_cx(),
            Some(interrupt_callback)
        ));
        KEEP_RUNNING.store(false, Ordering::SeqCst);
        let count = || INTERRUPTS.load(Ordering::SeqCst);

        // A `then` getter and an executor are script. QuickJS's resolving function then rejects the
        // promise with the termination, where SpiderMonkey returns null, so the result isn't
        // checked.
        let thenable = eval(
            cx,
            global,
            "({ get then() { for (let i = 0; i < 1e6; i++) {} } })",
        )
        .unwrap();
        let before = count();
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        jsapi::CallOriginalPromiseResolve(cx.raw_cx(), handle(&thenable));
        assert_eq!(count(), before + 1, "the then getter was not interrupted");
        let executor = eval(
            cx,
            global,
            "(function () { for (let i = 0; i < 1e6; i++) {} })",
        )
        .unwrap()
        .to_object();
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        assert!(jsapi::NewPromiseObject(cx.raw_cx(), handle(&executor)).is_null());
        assert_eq!(count(), before + 2, "the executor was not interrupted");

        // Settling with a non-thenable runs no script.
        let promises: Vec<_> = (0..20_000)
            .map(|_| new_promise(cx, ptr::null_mut()))
            .collect();
        let value = UndefinedValue();
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        for p in promises {
            assert!(jsapi::ResolvePromise(
                cx.raw_cx(),
                handle(&p),
                handle(&value)
            ));
        }
        assert_eq!(count(), before + 2);
        assert!(eval(cx, global, "for (;;) {}").is_err());
        assert_eq!(count(), before + 3);

        // A loop that creates promises polls both in and out of them. Whatever the phase of
        // QuickJS's poll countdown, a deferred interrupt must not be deferred at every later poll.
        assert!(!jsapi::JS_DefineFunction(
            cx.raw_cx(),
            global.into(),
            c"makePromise".as_ptr(),
            Some(make_promise),
            0,
            0
        )
        .is_null());
        eval(cx, global, "function n() {}").unwrap();
        for calls in 0..7 {
            for offset in 0..10 {
                let source = format!(
                    "{} for (let i = 0; i < 5000; i++) {{ makePromise(); {} }}",
                    "n(); ".repeat(offset),
                    "n(); ".repeat(calls)
                );
                let before = count();
                jsapi::JS_RequestInterruptCallback(cx.raw_cx());
                assert!(
                    eval(cx, global, &source).is_err(),
                    "not interrupted: {source}"
                );
                assert_eq!(count(), before + 1);
            }
        }
        KEEP_RUNNING.store(true, Ordering::SeqCst);
    }
}

thread_local!(static CSP_CHECKS: RefCell<Vec<(CompilationType, u32)>> = const { RefCell::new(Vec::new()) });

unsafe extern "C" fn content_security_policy_allows(
    _: *mut RawJSContext,
    runtime_code: RuntimeCode,
    _code_string: HandleString,
    compilation_type: CompilationType,
    parameter_strings: Handle<StackGCVector<*mut JSString>>,
    body_string: HandleString,
    parameter_args: Handle<StackGCVector<JSVal>>,
    _body_arg: HandleValue,
    can_compile_strings: *mut bool,
) -> bool {
    assert_eq!(runtime_code, RuntimeCode::JS);
    assert!(!body_string.is_null());
    let parameter_strings = mozjs::rust::Handle::from_raw(parameter_strings);
    let parameter_args = mozjs::rust::Handle::from_raw(parameter_args);
    assert_eq!(parameter_strings.len(), parameter_args.len());
    CSP_CHECKS.with(|c| {
        c.borrow_mut()
            .push((compilation_type, parameter_strings.len()))
    });
    *can_compile_strings = compilation_type == CompilationType::Function;
    true
}

static SECURITY_CALLBACKS: JSSecurityCallbacks = JSSecurityCallbacks {
    contentSecurityPolicyAllows: Some(content_security_policy_allows),
    codeForEvalGets: None,
    subsumes: None,
};

thread_local!(static EVAL_BODY_ARGS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) });

/// The code of objects with a string `code` property, like Servo's TrustedScript.
unsafe extern "C" fn code_for_eval_gets(
    cx: *mut RawJSContext,
    code: HandleObject,
    out: MutableHandleString,
) -> bool {
    let mut v = UndefinedValue();
    if !jsapi::JS_GetProperty(
        cx,
        code,
        c"code".as_ptr(),
        MutableHandleValue::from_marked_location(&mut v),
    ) {
        return false;
    }
    if v.is_string() {
        out.set(v.to_string());
    }
    true
}

thread_local!(static FUNCTION_CODE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) });

unsafe extern "C" fn allow_and_record_body_arg(
    cx: *mut RawJSContext,
    _runtime_code: RuntimeCode,
    code_string: HandleString,
    compilation_type: CompilationType,
    _parameter_strings: Handle<StackGCVector<*mut JSString>>,
    _body_string: HandleString,
    _parameter_args: Handle<StackGCVector<JSVal>>,
    body_arg: HandleValue,
    can_compile_strings: *mut bool,
) -> bool {
    let kind = if body_arg.get().is_object() {
        "object"
    } else {
        "string"
    };
    EVAL_BODY_ARGS.with(|a| a.borrow_mut().push(kind));
    if compilation_type == CompilationType::Function {
        let cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
        let code = jsstr_to_string(&cx, NonNull::new(code_string.get()).unwrap());
        FUNCTION_CODE.with(|c| c.borrow_mut().push(code));
    }
    *can_compile_strings = true;
    true
}

static CODE_FOR_EVAL_CALLBACKS: JSSecurityCallbacks = JSSecurityCallbacks {
    contentSecurityPolicyAllows: Some(allow_and_record_body_arg),
    codeForEvalGets: Some(code_for_eval_gets),
    subsumes: None,
};

fn csp(cx: &mut JSContext, global: SafeHandleObject) {
    unsafe {
        jsapi::JS_SetSecurityCallbacks(cx.raw_cx(), &SECURITY_CALLBACKS);
        assert_eq!(
            jsapi::JS_GetSecurityCallbacks(cx.raw_cx()),
            &SECURITY_CALLBACKS as *const _
        );
        assert!(eval_bool(
            cx,
            global,
            "
            var errors = [];
            try { eval('1'); } catch (e) { errors.push(e instanceof EvalError); }
            try { (0, eval)('1'); } catch (e) { errors.push(e instanceof EvalError); }
            errors.join() === 'true,true'
        "
        ));
        assert_eq!(
            eval(cx, global, "Function('a', 'b', 'return a + b')(1, 2)")
                .unwrap()
                .to_int32(),
            3
        );
        // A non-string argument is returned by eval without a check.
        assert_eq!(eval(cx, global, "eval(5)").unwrap().to_int32(), 5);
        let checks = CSP_CHECKS.with(|c| c.borrow().clone());
        assert_eq!(
            checks,
            [
                (CompilationType::DirectEval, 0),
                (CompilationType::IndirectEval, 0),
                (CompilationType::Function, 2),
            ]
        );

        // Objects passed to eval are evaluated as the code they stand for, which the checks see
        // with the object as argument; others are returned without a check.
        jsapi::JS_SetSecurityCallbacks(cx.raw_cx(), &CODE_FOR_EVAL_CALLBACKS);
        assert!(eval_bool(
            cx,
            global,
            "
            var trusted = { code: '6 * 7' }, other = {};
            eval(trusted) === 42 && (0, eval)(trusted) === 42 && eval(other) === other &&
                eval('1') === 1 && Function('a', 'b', 'return a + b')(1, 2) === 3 &&
                (async function () {}).constructor('await 1') instanceof Function
        "
        ));
        assert_eq!(
            EVAL_BODY_ARGS.with(|a| a.borrow().clone()),
            ["object", "object", "string", "string", "string"]
        );
        // The source text, as in SpiderMonkey.
        assert_eq!(
            FUNCTION_CODE.with(|c| c.borrow().clone()),
            [
                "function anonymous(a,b\n) {\nreturn a + b\n}",
                "async function anonymous(\n) {\nawait 1\n}",
            ]
        );

        jsapi::JS_SetSecurityCallbacks(cx.raw_cx(), ptr::null());
        assert_eq!(eval(cx, global, "eval('6')").unwrap().to_int32(), 6);
    }
}

#[test]
fn promises() {
    let engine = JSEngine::init().unwrap();
    with_global(&engine, |_, cx, global| resolve_and_reject(cx, global));
    with_global(&engine, |_, cx, global| job_queue(cx, global));
    with_global(&engine, |_, cx, global| rejection_tracker(cx, global));
    with_global(&engine, interrupts);
    with_global(&engine, |_, cx, global| deferred_interrupts(cx, global));
    with_global(&engine, |_, cx, global| csp(cx, global));
}
