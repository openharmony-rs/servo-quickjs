/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Promise values, reactions, rejection tracking and the microtask job queue.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use js::jsapi::{
    self, CallArgs, JSSecurityCallbacks, OnNewGlobalHookOption, PromiseRejectionHandlingState,
    PromiseState, PromiseUserInputEventHandlingState, Value,
};
use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    AddPromiseReactions, CallOriginalPromiseReject, CallOriginalPromiseResolve,
    DequeueNextRegularMicroTask, EnqueueMicroTask, GetJobQueue, GetPromiseIsHandled,
    GetPromiseState, GetRegularMicroTaskCount, HasAnyMicroTasks, HasRegularMicroTasks,
    IsPromiseObject, JS_AddInterruptCallback, JS_DequeueNextMicroTask, JS_GetSecurityCallbacks,
    JS_NewGlobalObject, JS_NewPlainObject, JS_RequestInterruptCallback,
    JS_RequestInterruptCallbackCanWait, JS_SetSecurityCallbacks,
    MaybeGetHostDefinedDataFromJSMicroTask, NewPromiseObject, PeekNextMicroTask, PrependMicroTask,
    RejectPromise, ResolvePromise, RunJSMicroTask, SetAnyPromiseIsHandled, SetJobQueue,
    SetPromiseRejectionTrackerCallback, SetPromiseUserInputEventHandlingState,
    UseInternalJobQueues,
};
use js::rust::{JSEngine, RealmOptions, SIMPLE_GLOBAL_CLASS};

use crate::support::{
    Observation, Value as Observed, eval, observe, record, record_case, set_global,
};

thread_local! {
    static FULFILLED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
    static REJECTIONS: RefCell<Vec<PromiseRejectionHandlingState>> = const { RefCell::new(Vec::new()) };
}

static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);
static KEEP_RUNNING: AtomicBool = AtomicBool::new(true);
static SECURITY_CALLBACKS: JSSecurityCallbacks = JSSecurityCallbacks {
    contentSecurityPolicyAllows: None,
    codeForEvalGets: None,
    subsumes: None,
};

unsafe extern "C" fn interrupt_callback(_cx: *mut jsapi::JSContext) -> bool {
    INTERRUPTS.fetch_add(1, Ordering::SeqCst);
    KEEP_RUNNING.load(Ordering::SeqCst)
}

unsafe extern "C" fn on_fulfilled(_cx: *mut jsapi::JSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    FULFILLED.with(|f| f.borrow_mut().push(args.get(0).to_int32()));
    args.rval().set(UndefinedValue());
    true
}

unsafe extern "C" fn on_rejected(_cx: *mut jsapi::JSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    FULFILLED.with(|f| f.borrow_mut().push(-args.get(0).to_int32()));
    args.rval().set(UndefinedValue());
    true
}

unsafe extern "C" fn executor(_cx: *mut jsapi::JSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval().set(UndefinedValue());
    true
}

unsafe extern "C" fn get_host_defined_data(
    _cx: *mut jsapi::JSContext,
    incumbent: jsapi::MutableHandleObject,
    data: jsapi::MutableHandleObject,
) -> bool {
    incumbent.set(ptr::null_mut());
    data.set(ptr::null_mut());
    true
}

unsafe extern "C" fn get_host_defined_global(
    _cx: *mut jsapi::JSContext,
    _incumbent: jsapi::MutableHandleObject,
) -> bool {
    true
}

unsafe extern "C" fn track_rejection(
    _cx: *mut jsapi::JSContext,
    _muted_errors: bool,
    _promise: js::jsapi::HandleObject,
    state: PromiseRejectionHandlingState,
    _data: *mut c_void,
) {
    REJECTIONS.with(|r| r.borrow_mut().push(state));
}

unsafe fn new_function(
    cx: &mut js::context::JSContext,
    native: jsapi::JSNative,
    name: &std::ffi::CStr,
) -> *mut jsapi::JSFunction {
    let function = jsapi::JS_NewFunction(cx.raw_cx(), native, 1, 0, name.as_ptr());
    assert!(!function.is_null());
    function
}

fn promise_values(engine: &JSEngine) {
    record_case(
        engine,
        "promise.values_and_reactions",
        &[
            "AddPromiseReactions",
            "JS_AddInterruptCallback",
            "JS_GetSecurityCallbacks",
            "JS_RequestInterruptCallback",
            "JS_RequestInterruptCallbackCanWait",
            "JS_SetSecurityCallbacks",
            "CallOriginalPromiseReject",
            "CallOriginalPromiseResolve",
            "GetPromiseIsHandled",
            "GetPromiseState",
            "GetPromiseUserInputEventHandlingState",
            "JS_GetPromiseResult",
            "IsPromiseObject",
            "NewPromiseObject",
            "RejectPromise",
            "ResolvePromise",
            "SetAnyPromiseIsHandled",
            "SetPromiseUserInputEventHandlingState",
            "SetPromiseRejectionTrackerCallback",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let executor = new_function(cx, Some(executor), c"executor"));
            rooted!(&in(cx) let executor_object = jsapi::JS_GetFunctionObject(executor.get()));

            rooted!(&in(cx) let promise = NewPromiseObject(cx, executor_object.handle()));
            assert!(IsPromiseObject(promise.handle()));
            rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
            let plain_is_not_promise = !IsPromiseObject(plain.handle());
            assert!(
                plain_is_not_promise,
                "IsPromiseObject must reject a plain object"
            );
            assert_eq!(GetPromiseState(promise.handle()), PromiseState::Pending);
            rooted!(&in(cx) let value = Int32Value(42));
            assert!(ResolvePromise(cx, promise.handle(), value.handle()));
            assert_eq!(GetPromiseState(promise.handle()), PromiseState::Fulfilled);
            rooted!(&in(cx) let mut result = UndefinedValue());
            js::glue::JS_GetPromiseResult(promise.handle().into(), result.handle_mut().into());
            assert_eq!(result.get().to_int32(), 42);
            assert!(RejectPromise(cx, promise.handle(), value.handle()));
            assert_eq!(GetPromiseState(promise.handle()), PromiseState::Fulfilled);

            rooted!(&in(cx) let rejected = CallOriginalPromiseReject(cx, value.handle()));
            let rejected_state = GetPromiseState(rejected.handle());
            let rejected_unhandled_before = !GetPromiseIsHandled(rejected.handle());
            assert_eq!(rejected_state, PromiseState::Rejected);
            assert!(rejected_unhandled_before);
            assert!(SetAnyPromiseIsHandled(cx, rejected.handle()));
            let rejected_handled_after = GetPromiseIsHandled(rejected.handle());
            assert!(rejected_handled_after);

            rooted!(&in(cx) let resolved = CallOriginalPromiseResolve(cx, value.handle()));
            let resolved_state = GetPromiseState(resolved.handle());
            assert_eq!(resolved_state, PromiseState::Fulfilled);
            rooted!(&in(cx) let resolved_value = ObjectValue(resolved.get()));
            assert_eq!(
                CallOriginalPromiseResolve(cx, resolved_value.handle()),
                resolved.get()
            );

            assert!(SetPromiseUserInputEventHandlingState(
                resolved.handle(),
                PromiseUserInputEventHandlingState::HadUserInteractionAtCreation
            ));
            assert_eq!(
                jsapi::GetPromiseUserInputEventHandlingState(resolved.get()),
                PromiseUserInputEventHandlingState::HadUserInteractionAtCreation
            );

            let queue = js::glue::CreateJobQueue(&QUEUE_TRAPS);
            assert!(!queue.is_null());
            SetJobQueue(cx, queue as *mut jsapi::JobQueue);
            assert_eq!(
                GetJobQueue(cx),
                queue as *mut jsapi::JobQueue,
                "GetJobQueue must return the installed queue"
            );
            FULFILLED.with(|f| f.borrow_mut().clear());
            rooted!(&in(cx) let on_fulfilled_fn = new_function(cx, Some(on_fulfilled), c"onFulfilled"));
            rooted!(&in(cx) let on_fulfilled = jsapi::JS_GetFunctionObject(on_fulfilled_fn.get()));
            rooted!(&in(cx) let on_rejected_fn = new_function(cx, Some(on_rejected), c"onRejected"));
            rooted!(&in(cx) let on_rejected = jsapi::JS_GetFunctionObject(on_rejected_fn.get()));
            assert!(AddPromiseReactions(
                cx,
                resolved.handle(),
                on_fulfilled.handle(),
                on_rejected.handle()
            ));

            jsapi::RunJobs(cx.raw_cx());
            assert_eq!(FULFILLED.with(|f| f.borrow().clone()), [42]);
            REJECTIONS.with(|r| r.borrow_mut().clear());
            SetPromiseRejectionTrackerCallback(cx, Some(track_rejection), ptr::null_mut());
            rooted!(&in(cx) let tracked = CallOriginalPromiseReject(cx, value.handle()));
            assert_eq!(
                REJECTIONS.with(|r| r.borrow().clone()),
                [PromiseRejectionHandlingState::Unhandled]
            );
            assert!(SetAnyPromiseIsHandled(cx, tracked.handle()));
            assert_eq!(
                REJECTIONS.with(|r| r.borrow().clone()),
                [
                    PromiseRejectionHandlingState::Unhandled,
                    PromiseRejectionHandlingState::Handled
                ]
            );
            SetPromiseRejectionTrackerCallback(cx, None, ptr::null_mut());

            INTERRUPTS.store(0, Ordering::SeqCst);
            KEEP_RUNNING.store(true, Ordering::SeqCst);
            assert!(JS_AddInterruptCallback(cx, Some(interrupt_callback)));
            JS_RequestInterruptCallback(cx);
            rooted!(&in(cx) let counted = eval(cx, global, "for (var i = 0; i < 10000; i++) {} i"));
            assert_eq!(counted.get().to_int32(), 10000);
            assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 1);
            JS_RequestInterruptCallbackCanWait(cx);
            rooted!(&in(cx) let counted = eval(cx, global, "for (var i = 0; i < 10000; i++) {} i"));
            assert_eq!(counted.get().to_int32(), 10000);
            assert_eq!(INTERRUPTS.load(Ordering::SeqCst), 2);

            JS_SetSecurityCallbacks(cx, &SECURITY_CALLBACKS);
            assert_eq!(JS_GetSecurityCallbacks(cx), &SECURITY_CALLBACKS as *const _);
            JS_SetSecurityCallbacks(cx, ptr::null());
            assert!(JS_GetSecurityCallbacks(cx).is_null());

            SetJobQueue(cx, ptr::null_mut());
            js::glue::DeleteJobQueue(queue);

            set_global(cx, global, c"p", ObjectValue(resolved.get()));
            rooted!(&in(cx) let check = eval(cx, global, "p instanceof Promise && p.then !== undefined"));
            let observed = observe(cx, check.get());
            Observation::new(vec![
                ("initial_pending", Observed::Boolean(true)),
                (
                    "plain_is_not_promise",
                    Observed::Boolean(plain_is_not_promise),
                ),
                ("resolved_value", Observed::Number(42.0)),
                ("settled_after_reject", Observed::Boolean(true)),
                ("resolved_state", Observed::String("fulfilled".to_owned())),
                ("rejected_state", Observed::String("rejected".to_owned())),
                (
                    "rejected_unhandled_before",
                    Observed::Boolean(rejected_unhandled_before),
                ),
                (
                    "rejected_handled_after",
                    Observed::Boolean(rejected_handled_after),
                ),
                ("resolve_identity", Observed::Boolean(true)),
                (
                    "user_input_after",
                    Observed::String(format!(
                        "{:?}",
                        jsapi::GetPromiseUserInputEventHandlingState(resolved.get())
                    )),
                ),
                (
                    "fulfilled_reactions",
                    Observed::String(format!("{:?}", FULFILLED.with(|f| f.borrow().clone()))),
                ),
                (
                    "rejections",
                    Observed::String(format!("{:?}", REJECTIONS.with(|r| r.borrow().clone()))),
                ),
                (
                    "interrupts",
                    Observed::Number(INTERRUPTS.load(Ordering::SeqCst) as f64),
                ),
                ("promise_instance", observed),
            ])
        },
    );
}

fn job_queue(engine: &JSEngine) {
    record_case(
        engine,
        "promise.job_queue",
        &[
            "CreateJobQueue",
            "DeleteJobQueue",
            "DequeueNextRegularMicroTask",
            "EnqueueMicroTask",
            "GetExecutionGlobalFromJSMicroTask",
            "GetJobQueue",
            "GetRegularMicroTaskCount",
            "HasAnyMicroTasks",
            "HasRegularMicroTasks",
            "IsJSMicroTask",
            "JS_DequeueNextMicroTask",
            "MaybeGetHostDefinedDataFromJSMicroTask",
            "MaybeGetPromiseFromJSMicroTask",
            "PeekNextMicroTask",
            "PrependMicroTask",
            "RunJSMicroTask",
            "SetJobQueue",
            "ToMaybeWrappedJSMicroTask",
        ],
        |cx, global| unsafe {
            let queue = js::glue::CreateJobQueue(&QUEUE_TRAPS);
            assert!(!queue.is_null());
            SetJobQueue(cx, queue as *mut jsapi::JobQueue);
            assert_eq!(GetJobQueue(cx), queue as *mut jsapi::JobQueue);
            assert!(
                eval(
                    cx,
                    global,
                    "var log = [];
                 Promise.resolve(1).then(() => log.push('a'));
                 Promise.resolve(2).then(() => log.push('b'));
                 log.join() === ''"
                )
                .to_boolean()
            );
            assert!(HasAnyMicroTasks(cx));
            assert!(HasRegularMicroTasks(cx));
            assert_eq!(GetRegularMicroTaskCount(cx), 2);
            rooted!(&in(cx) let peeked = PeekNextMicroTask(cx));
            assert!(!peeked.is_null());
            assert!(jsapi::IsJSMicroTask(peeked.as_ptr()));

            rooted!(&in(cx) let a = DequeueNextRegularMicroTask(cx));
            rooted!(&in(cx) let b = DequeueNextRegularMicroTask(cx));
            assert!(jsapi::IsJSMicroTask(a.as_ptr()));
            assert!(jsapi::IsJSMicroTask(b.as_ptr()));
            assert_eq!(GetRegularMicroTaskCount(cx), 0);
            let micro = jsapi::ToMaybeWrappedJSMicroTask(a.as_ptr());
            assert!(!micro.is_null());
            assert!(!jsapi::GetExecutionGlobalFromJSMicroTask(micro).is_null());
            let promise = jsapi::MaybeGetPromiseFromJSMicroTask(micro);
            assert!(!promise.is_null());
            rooted!(&in(cx) let mut incumbent = ptr::null_mut::<jsapi::JSObject>());
            rooted!(&in(cx) let mut data = ptr::null_mut::<jsapi::JSObject>());
            assert!(MaybeGetHostDefinedDataFromJSMicroTask(
                micro,
                incumbent.handle_mut(),
                data.handle_mut()
            ));
            assert!(PrependMicroTask(cx, b.as_ptr()));
            assert!(EnqueueMicroTask(cx, a.as_ptr()));
            assert_eq!(GetRegularMicroTaskCount(cx), 2);

            rooted!(&in(cx) let mut next = UndefinedValue());
            JS_DequeueNextMicroTask(cx, next.handle_mut());
            assert!(jsapi::IsJSMicroTask(next.as_ptr()));
            let next_micro = jsapi::ToMaybeWrappedJSMicroTask(next.as_ptr());
            rooted!(&in(cx) let next_micro = next_micro);
            assert!(RunJSMicroTask(cx, next_micro.handle()));

            JS_DequeueNextMicroTask(cx, next.handle_mut());
            assert!(jsapi::IsJSMicroTask(next.as_ptr()));
            let next_micro = jsapi::ToMaybeWrappedJSMicroTask(next.as_ptr());
            rooted!(&in(cx) let next_micro = next_micro);
            assert!(RunJSMicroTask(cx, next_micro.handle()));
            assert!(!HasAnyMicroTasks(cx));
            rooted!(&in(cx) let log = eval(cx, global, "log.join()"));
            assert_eq!(observe(cx, log.get()), Observed::String("b,a".to_owned()));

            let log = match observe(cx, log.get()) {
                Observed::String(log) => log,
                other => panic!("unexpected microtask log: {other:?}"),
            };
            assert_eq!(log, "b,a");
            SetJobQueue(cx, ptr::null_mut());
            js::glue::DeleteJobQueue(queue);
            Observation::new(vec![
                ("queue_installed", Observed::Boolean(true)),
                ("microtasks_enqueued", Observed::Number(2.0)),
                ("microtasks_after_drain", Observed::Number(0.0)),
                ("execution_global_non_null", Observed::Boolean(true)),
                ("promise_non_null", Observed::Boolean(true)),
                ("prepend_before_append", Observed::Boolean(true)),
                ("log", Observed::String(log)),
            ])
        },
    );
}

fn internal_job_queue(engine: &JSEngine) {
    let observed = unsafe {
        let cx = jsapi::JS_NewContext(1 << 20, ptr::null_mut());
        assert!(!cx.is_null(), "JS_NewContext");
        let cx = NonNull::new(cx).unwrap();
        let mut cx = js::context::JSContext::from_ptr(cx);

        // SpiderMonkey requires this before self-hosting is initialized.
        assert!(UseInternalJobQueues(&cx));

        let cache = jsapi::__BindgenOpaqueArray::<u64, 2>::default();
        assert!(jsapi::InitSelfHostedCode(cx.raw_cx(), cache, None));
        jsapi::JS_SetNativeStackQuota(cx.raw_cx(), 256 * 1024, 192 * 1024, 128 * 1024);

        {
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                &mut cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*RealmOptions::default(),
            ));
            let mut realm = AutoRealm::new_from_handle(&mut cx, global.handle());
            let (global, cx) = realm.global_and_reborrow();
            let queued = eval(
                cx,
                global,
                "globalThis.log = [];
                 Promise.resolve(1).then(value => log.push(value));
                 log.join()",
            );
            assert_eq!(observe(cx, queued), Observed::String(String::new()));
            assert!(HasAnyMicroTasks(cx));
            jsapi::RunJobs(cx.raw_cx());
            let drained = eval(cx, global, "log.join()");
            assert_eq!(observe(cx, drained), Observed::String("1".to_owned()));
            assert!(!HasAnyMicroTasks(cx));
        }

        jsapi::JS_DestroyContext(cx.raw_cx());
        let _ = engine;
        Observation::ok(Observed::String("1".to_owned()))
    };
    record(
        "promise.internal_job_queue",
        &observed,
        &[
            "HasAnyMicroTasks",
            "JS_NewContext",
            "JS_NewGlobalObject",
            "JS_SetNativeStackQuota",
            "RunJobs",
            "UseInternalJobQueues",
        ],
    );
}

static QUEUE_TRAPS: js::glue::JobQueueTraps = js::glue::JobQueueTraps {
    getHostDefinedData: Some(get_host_defined_data),
    getHostDefinedGlobal: Some(get_host_defined_global),
    runJobs: Some(run_jobs),
    traceNonGCThingMicroTask: None,
};

unsafe extern "C" fn run_jobs(cx: *mut jsapi::JSContext) {
    let cx = NonNull::new(cx).expect("non-null JSContext in the job queue trap");
    let mut cx = js::context::JSContext::from_ptr(cx);
    while HasAnyMicroTasks(&cx) {
        rooted!(&in(cx) let mut task = UndefinedValue());
        JS_DequeueNextMicroTask(&cx, task.handle_mut());
        assert!(jsapi::IsJSMicroTask(task.as_ptr()));
        let task = jsapi::ToMaybeWrappedJSMicroTask(task.as_ptr());
        rooted!(&in(cx) let task = task);
        assert!(RunJSMicroTask(&mut cx, task.handle()));
    }
}

mod edge_promises_more_contract {
    use std::ptr;

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::jsapi::mozilla::Range;
    use js::jsval::{JSVal, UndefinedValue};
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{
        DequeueNextRegularMicroTask, EnqueueMicroTask, GetRegularMicroTaskCount, HasAnyMicroTasks,
        HasRegularMicroTasks, JS_DequeueNextMicroTask, PeekNextMicroTask, PrependMicroTask,
        RunJSMicroTask,
    };

    use crate::support::{Observation, Value, eval, record_case, set_global};

    fn microtask_queue(engine: &JSEngine) {
        record_case(
            engine,
            "edge.microtask_queue",
            &[
                "CreateJobQueue",
                "DeleteJobQueue",
                "SetJobQueue",
                "EnqueueMicroTask",
                "DequeueNextRegularMicroTask",
                "PeekNextMicroTask",
                "PrependMicroTask",
                "HasAnyMicroTasks",
                "HasRegularMicroTasks",
                "GetRegularMicroTaskCount",
                "IsJSMicroTask",
                "ToMaybeWrappedJSMicroTask",
                "GetExecutionGlobalFromJSMicroTask",
                "MaybeGetHostDefinedDataFromJSMicroTask",
                "MaybeGetPromiseFromJSMicroTask",
                "RunJSMicroTask",
                "JS_DequeueNextMicroTask",
            ],
            |cx, global| unsafe {
                let queue = crate::support::install_job_queue(cx);
                let log = eval(cx, global, "[]");
                set_global(cx, global, c"log", log);
                let queued = eval(
                    cx,
                    global,
                    "new Promise((resolve) => { log.push('sync'); resolve(1); }); \
                 Promise.resolve(2).then(() => log.push('a')); \
                 Promise.resolve(3).then(() => log.push('b')); log.join()",
                );
                let queued_text = string_of(cx, queued);
                let count_after = GetRegularMicroTaskCount(cx);
                let any = HasAnyMicroTasks(cx);
                let regular = HasRegularMicroTasks(cx);
                rooted!(&in(cx) let peeked = PeekNextMicroTask(cx));
                let peek_is_task = js::jsapi::IsJSMicroTask(peeked.as_ptr());

                rooted!(&in(cx) let first = DequeueNextRegularMicroTask(cx));
                rooted!(&in(cx) let second = DequeueNextRegularMicroTask(cx));
                let first_task = js::jsapi::IsJSMicroTask(first.as_ptr());
                let second_task = js::jsapi::IsJSMicroTask(second.as_ptr());
                let count_after_dequeue = GetRegularMicroTaskCount(cx);
                let first_micro = js::jsapi::ToMaybeWrappedJSMicroTask(first.as_ptr());
                let second_micro = js::jsapi::ToMaybeWrappedJSMicroTask(second.as_ptr());
                rooted!(&in(cx) let first_micro = first_micro);
                rooted!(&in(cx) let second_micro = second_micro);
                let execution_global =
                    js::jsapi::GetExecutionGlobalFromJSMicroTask(first_micro.get());
                let execution_global_non_null = !execution_global.is_null();
                rooted!(&in(cx) let mut incumbent = ptr::null_mut::<js::jsapi::JSObject>());
                rooted!(&in(cx) let mut host_data = ptr::null_mut::<js::jsapi::JSObject>());
                let host_ok = js::jsapi::MaybeGetHostDefinedDataFromJSMicroTask(
                    first_micro.get(),
                    incumbent.handle_mut().into(),
                    host_data.handle_mut().into(),
                );
                let promise = js::jsapi::MaybeGetPromiseFromJSMicroTask(first_micro.get());

                let prepended = PrependMicroTask(cx, second.as_ptr());
                let enqueued = EnqueueMicroTask(cx, first.as_ptr());
                let count_before_run = GetRegularMicroTaskCount(cx);
                rooted!(&in(cx) let mut next = UndefinedValue());
                JS_DequeueNextMicroTask(cx, next.handle_mut());
                let next_is_task = js::jsapi::IsJSMicroTask(next.as_ptr());
                let next_micro = js::jsapi::ToMaybeWrappedJSMicroTask(next.as_ptr());
                rooted!(&in(cx) let next_micro = next_micro);
                let ran = RunJSMicroTask(cx, next_micro.handle());
                rooted!(&in(cx) let mut next = UndefinedValue());
                JS_DequeueNextMicroTask(cx, next.handle_mut());
                let next_micro = js::jsapi::ToMaybeWrappedJSMicroTask(next.as_ptr());
                rooted!(&in(cx) let next_micro = next_micro);
                let ran_second = RunJSMicroTask(cx, next_micro.handle());
                let log_value = eval(cx, global, "log.join()");
                let log_text = string_of(cx, log_value);
                let drained = !HasAnyMicroTasks(cx);

                assert_eq!(queued_text, "sync");
                assert_eq!(count_after, 2);
                assert!(any);
                assert!(regular);
                assert!(peek_is_task);
                assert!(first_task);
                assert!(second_task);
                assert_eq!(count_after_dequeue, 0);
                assert!(execution_global_non_null);
                assert!(host_ok);
                assert!(!promise.is_null());
                assert!(prepended);
                assert!(enqueued);
                assert_eq!(count_before_run, 2);
                assert!(next_is_task);
                assert!(ran);
                assert!(ran_second);
                assert_eq!(log_text, "sync,b,a");
                assert!(drained);

                crate::support::remove_job_queue(cx, queue);

                Observation::new(vec![
                    ("queued", Value::String(queued_text)),
                    ("count_after", Value::Number(count_after as f64)),
                    ("any", Value::Boolean(any)),
                    ("regular", Value::Boolean(regular)),
                    ("peek_is_task", Value::Boolean(peek_is_task)),
                    ("first_task", Value::Boolean(first_task)),
                    ("second_task", Value::Boolean(second_task)),
                    (
                        "count_after_dequeue",
                        Value::Number(count_after_dequeue as f64),
                    ),
                    (
                        "execution_global_non_null",
                        Value::Boolean(execution_global_non_null),
                    ),
                    ("host_ok", Value::Boolean(host_ok)),
                    ("promise_null", Value::Boolean(promise.is_null())),
                    ("prepended", Value::Boolean(prepended)),
                    ("enqueued", Value::Boolean(enqueued)),
                    ("count_before_run", Value::Number(count_before_run as f64)),
                    ("next_is_task", Value::Boolean(next_is_task)),
                    ("ran", Value::Boolean(ran)),
                    ("ran_second", Value::Boolean(ran_second)),
                    ("log", Value::String(log_text)),
                    ("drained", Value::Boolean(drained)),
                ])
            },
        );
    }
    fn range<T>(ptr: *mut T, len: usize) -> Range<T> {
        unsafe {
            let mut range: Range<T> = std::mem::zeroed();
            range.mStart.mPtr = ptr;
            range.mEnd.mPtr = ptr.add(len);
            range
        }
    }

    fn string_of(cx: &mut JSContext, value: JSVal) -> String {
        assert!(value.is_string());
        unsafe {
            jsstr_to_string(
                cx,
                std::ptr::NonNull::new(value.to_string()).expect("non-null string"),
            )
        }
    }

    pub fn run(engine: &JSEngine) {
        microtask_queue(engine);
    }
}

mod edge_promises_deep_contract {

    // ---------------------------------------------------------------------------
    // Promise values, rejection tracking and the host job queue.
    // ---------------------------------------------------------------------------

    thread_local! {
        static PROMISE_REACTIONS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static PROMISE_REJECTIONS: RefCell<Vec<PromiseRejectionHandlingState>> = const { RefCell::new(Vec::new()) };
    }

    static PROMISE_INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn promise_executor(
        _cx: *mut jsapi::JSContext,
        argc: u32,
        vp: *mut JsValue,
    ) -> bool {
        CallArgs::from_vp(vp, argc).rval().set(UndefinedValue());
        true
    }

    unsafe extern "C" fn on_fulfilled(
        _cx: *mut jsapi::JSContext,
        argc: u32,
        vp: *mut JsValue,
    ) -> bool {
        let args = CallArgs::from_vp(vp, argc);
        PROMISE_REACTIONS.with(|out| {
            out.borrow_mut()
                .push(format!("ok:{}", args.get(0).to_int32()))
        });
        args.rval().set(UndefinedValue());
        true
    }

    unsafe extern "C" fn on_rejected(
        _cx: *mut jsapi::JSContext,
        argc: u32,
        vp: *mut JsValue,
    ) -> bool {
        let args = CallArgs::from_vp(vp, argc);
        PROMISE_REACTIONS.with(|out| {
            out.borrow_mut()
                .push(format!("err:{}", args.get(0).to_int32()))
        });
        args.rval().set(UndefinedValue());
        true
    }

    unsafe extern "C" fn track_rejection(
        _cx: *mut jsapi::JSContext,
        _muted_errors: bool,
        _promise: js::jsapi::HandleObject,
        state: PromiseRejectionHandlingState,
        _data: *mut c_void,
    ) {
        PROMISE_REJECTIONS.with(|out| out.borrow_mut().push(state));
    }

    unsafe extern "C" fn count_interrupt(_cx: *mut jsapi::JSContext) -> bool {
        PROMISE_INTERRUPTS.fetch_add(1, Ordering::SeqCst);
        true
    }

    static PROMISE_SECURITY_CALLBACKS: JSSecurityCallbacks = JSSecurityCallbacks {
        contentSecurityPolicyAllows: None,
        codeForEvalGets: None,
        subsumes: None,
    };

    fn promise_states_and_queue(engine: &JSEngine) {
        record_case(
            engine,
            "edge.promise.states_and_queue",
            &[
                "AddPromiseReactions",
                "CallOriginalPromiseReject",
                "CallOriginalPromiseResolve",
                "GetJobQueue",
                "GetPromiseIsHandled",
                "GetPromiseState",
                "GetPromiseUserInputEventHandlingState",
                "JS_AddInterruptCallback",
                "JS_GetPromiseResult",
                "JS_GetSecurityCallbacks",
                "JS_NewFunction",
                "JS_RequestInterruptCallback",
                "JS_SetSecurityCallbacks",
                "NewPromiseObject",
                "RejectPromise",
                "RunJobs",
                "SetAnyPromiseIsHandled",
                "SetPromiseRejectionTrackerCallback",
                "SetPromiseUserInputEventHandlingState",
            ],
            |cx, global| unsafe {
                PROMISE_REACTIONS.with(|out| out.borrow_mut().clear());
                PROMISE_REJECTIONS.with(|out| out.borrow_mut().clear());
                PROMISE_INTERRUPTS.store(0, Ordering::SeqCst);

                let queue = crate::support::install_job_queue(cx);
                let queue_installed = GetJobQueue(cx) as *mut c_void == queue;
                assert!(
                    queue_installed,
                    "GetJobQueue must return the installed queue"
                );
                JS_SetSecurityCallbacks(cx, &PROMISE_SECURITY_CALLBACKS);
                let callbacks_installed =
                    !js::rust::wrappers2::JS_GetSecurityCallbacks(cx).is_null();
                assert!(
                    callbacks_installed,
                    "JS_SetSecurityCallbacks must install the callbacks"
                );

                let executor = jsapi::JS_NewFunction(
                    cx.raw_cx(),
                    Some(promise_executor),
                    0,
                    0,
                    c"executor".as_ptr(),
                );
                assert!(!executor.is_null());
                rooted!(&in(cx) let executor = jsapi::JS_GetFunctionObject(executor));
                rooted!(&in(cx) let promise = NewPromiseObject(cx, executor.handle()));
                let initial_state = GetPromiseState(promise.handle());
                assert_eq!(initial_state, PromiseState::Pending);
                assert!(IsPromiseObject(promise.handle()));
                rooted!(&in(cx) let plain = js::rust::wrappers2::JS_NewPlainObject(cx));
                let plain_is_not_promise = !IsPromiseObject(plain.handle());
                assert!(
                    plain_is_not_promise,
                    "IsPromiseObject must reject a plain object"
                );
                rooted!(&in(cx) let forty_two = Int32Value(42));
                assert!(ResolvePromise(cx, promise.handle(), forty_two.handle()));
                let fulfilled_state = GetPromiseState(promise.handle());
                assert_eq!(fulfilled_state, PromiseState::Fulfilled);
                rooted!(&in(cx) let mut result = UndefinedValue());
                js::glue::JS_GetPromiseResult(promise.handle().into(), result.handle_mut().into());
                let fulfilled_result = result.get().to_int32();
                assert_eq!(fulfilled_result, 42);
                let reject_after_resolve = RejectPromise(cx, promise.handle(), forty_two.handle());
                assert!(reject_after_resolve);
                let state_after_late_reject = GetPromiseState(promise.handle());
                assert_eq!(state_after_late_reject, PromiseState::Fulfilled);

                rooted!(&in(cx) let seven = Int32Value(7));
                rooted!(&in(cx) let rejected = CallOriginalPromiseReject(cx, seven.handle()));
                let rejected_state = GetPromiseState(rejected.handle());
                let rejected_handled_before = GetPromiseIsHandled(rejected.handle());
                assert_eq!(rejected_state, PromiseState::Rejected);
                assert!(!rejected_handled_before);
                assert!(IsPromiseObject(rejected.handle()));

                rooted!(&in(cx) let resolved_value = CallOriginalPromiseResolve(cx, forty_two.handle()));
                let resolved_state = GetPromiseState(resolved_value.handle());
                assert_eq!(resolved_state, PromiseState::Fulfilled);
                assert!(IsPromiseObject(resolved_value.handle()));
                rooted!(&in(cx) let resolved_object = ObjectValue(resolved_value.get()));
                rooted!(&in(cx) let same_promise = CallOriginalPromiseResolve(cx, resolved_object.handle()));
                let resolve_identity = same_promise.get() == resolved_value.get();
                assert!(
                    resolve_identity,
                    "resolving a promise must return the same promise"
                );

                assert!(SetPromiseUserInputEventHandlingState(
                    resolved_value.handle(),
                    PromiseUserInputEventHandlingState::HadUserInteractionAtCreation
                ));
                let user_input_after =
                    jsapi::GetPromiseUserInputEventHandlingState(resolved_value.get());
                let user_input_before =
                    jsapi::GetPromiseUserInputEventHandlingState(rejected.get());
                assert_eq!(
                    user_input_after,
                    PromiseUserInputEventHandlingState::HadUserInteractionAtCreation
                );
                assert_eq!(
                    user_input_before,
                    PromiseUserInputEventHandlingState::DontCare
                );

                let on_fulfilled_fn = jsapi::JS_NewFunction(
                    cx.raw_cx(),
                    Some(on_fulfilled),
                    1,
                    0,
                    c"onFulfilled".as_ptr(),
                );
                let on_rejected_fn = jsapi::JS_NewFunction(
                    cx.raw_cx(),
                    Some(on_rejected),
                    1,
                    0,
                    c"onRejected".as_ptr(),
                );
                assert!(!on_fulfilled_fn.is_null() && !on_rejected_fn.is_null());
                rooted!(&in(cx) let on_fulfilled = jsapi::JS_GetFunctionObject(on_fulfilled_fn));
                rooted!(&in(cx) let on_rejected = jsapi::JS_GetFunctionObject(on_rejected_fn));
                SetPromiseRejectionTrackerCallback(cx, Some(track_rejection), ptr::null_mut());
                rooted!(&in(cx) let tracked = CallOriginalPromiseReject(cx, seven.handle()));
                assert!(SetAnyPromiseIsHandled(cx, tracked.handle()));
                assert!(GetPromiseIsHandled(tracked.handle()));

                assert!(AddPromiseReactions(
                    cx,
                    resolved_value.handle(),
                    on_fulfilled.handle(),
                    on_rejected.handle()
                ));
                assert!(AddPromiseReactions(
                    cx,
                    rejected.handle(),
                    on_fulfilled.handle(),
                    on_rejected.handle()
                ));
                jsapi::RunJobs(cx.raw_cx());

                assert!(JS_AddInterruptCallback(cx, Some(count_interrupt)));
                JS_RequestInterruptCallback(cx);
                let interrupt_value = eval(cx, global, "for (var i = 0; i < 1000; i++) {} i");
                let interrupt_result = interrupt_value.to_int32();
                assert_eq!(interrupt_result, 1000);
                let interrupts = PROMISE_INTERRUPTS.load(Ordering::SeqCst);
                #[cfg(not(feature = "js-quickjs"))]
                assert_eq!(
                    interrupts, 1,
                    "JS_RequestInterruptCallback must schedule the callback once"
                );
                #[cfg(feature = "js-quickjs")]
                assert_eq!(
                    interrupts, 0,
                    "QuickJS does not run an interrupt callback requested by JS_RequestInterruptCallback"
                );

                let reaction_log = PROMISE_REACTIONS.with(|out| out.borrow().join(","));
                let rejection_log = PROMISE_REJECTIONS.with(|out| {
                    out.borrow()
                        .iter()
                        .map(|state| format!("{state:?}"))
                        .collect::<Vec<_>>()
                        .join(",")
                });
                assert_eq!(reaction_log, "ok:42,err:7");
                assert_eq!(rejection_log, "Unhandled,Handled,Handled");
                SetPromiseRejectionTrackerCallback(cx, None, ptr::null_mut());

                let observation = Observation::new(vec![
                    (
                        "initial_state",
                        Value::String(promise_state_name(initial_state).to_owned()),
                    ),
                    ("plain_is_not_promise", Value::Boolean(plain_is_not_promise)),
                    (
                        "fulfilled_state",
                        Value::String(promise_state_name(fulfilled_state).to_owned()),
                    ),
                    ("fulfilled_result", Value::Number(fulfilled_result as f64)),
                    ("reject_after_resolve", Value::Boolean(reject_after_resolve)),
                    (
                        "state_after_late_reject",
                        Value::String(promise_state_name(state_after_late_reject).to_owned()),
                    ),
                    (
                        "rejected_state",
                        Value::String(promise_state_name(rejected_state).to_owned()),
                    ),
                    (
                        "rejected_handled_before",
                        Value::Boolean(rejected_handled_before),
                    ),
                    (
                        "resolved_state",
                        Value::String(promise_state_name(resolved_state).to_owned()),
                    ),
                    ("resolve_identity", Value::Boolean(resolve_identity)),
                    (
                        "user_input_before",
                        Value::String(format!("{user_input_before:?}")),
                    ),
                    (
                        "user_input_after",
                        Value::String(format!("{user_input_after:?}")),
                    ),
                    ("reactions", Value::String(reaction_log)),
                    ("rejections", Value::String(rejection_log)),
                    ("interrupt_result", Value::Number(interrupt_result as f64)),
                    ("interrupts", Value::Number(interrupts as f64)),
                    ("queue_installed", Value::Boolean(queue_installed)),
                    (
                        "security_callbacks_installed",
                        Value::Boolean(callbacks_installed),
                    ),
                ]);
                crate::support::remove_job_queue(cx, queue);
                observation
            },
        );
    }

    fn promise_state_name(state: PromiseState) -> &'static str {
        match state {
            PromiseState::Pending => "pending",
            PromiseState::Fulfilled => "fulfilled",
            PromiseState::Rejected => "rejected",
        }
    }

    // ---------------------------------------------------------------------------
    // Realm entry, identity and private storage.
    // ---------------------------------------------------------------------------

    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use js::jsapi::{
        self, CallArgs, JSSecurityCallbacks, PromiseRejectionHandlingState, PromiseState,
        PromiseUserInputEventHandlingState, Value as JsValue,
    };
    use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{
        AddPromiseReactions, CallOriginalPromiseReject, CallOriginalPromiseResolve, GetJobQueue,
        GetPromiseIsHandled, GetPromiseState, IsPromiseObject, JS_AddInterruptCallback,
        JS_RequestInterruptCallback, JS_SetSecurityCallbacks, NewPromiseObject, RejectPromise,
        ResolvePromise, SetAnyPromiseIsHandled, SetPromiseRejectionTrackerCallback,
        SetPromiseUserInputEventHandlingState,
    };

    use crate::support::{Observation, Value, eval, record_case};

    pub fn run(engine: &JSEngine) {
        promise_states_and_queue(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_promises_deep_contract::run(engine);
    edge_promises_more_contract::run(engine);
    crate::support::isolate_case("promise.values_and_reactions", || promise_values(engine));
    crate::support::isolate_case("promise.job_queue", || job_queue(engine));
    crate::support::isolate_case("promise.internal_job_queue", || internal_job_queue(engine));
}
