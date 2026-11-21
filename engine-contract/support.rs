/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{BTreeSet, HashSet};
use std::ffi::CStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, Once, OnceLock};

use js::context::JSContext;
use js::conversions::jsstr_to_string;
use js::jsapi::{ModuleType, OnNewGlobalHookOption, PromiseState};
use js::jsval::{JSVal, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    HasAnyMicroTasks, JS_ClearPendingException, JS_DequeueNextMicroTask, JS_GetPendingException,
    JS_IsExceptionPending, JS_NewGlobalObject, JS_SetProperty, RunJSMicroTask, SetJobQueue,
};
use js::rust::{
    CompileOptionsWrapper, HandleObject, JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
    evaluate_script,
};

/// Collects the observations of a `Steps`-based case, so a step can record the values it checked
/// without changing its signature or moving the assertions.
///
/// `Steps::finish` used to record only a `value: true` sentinel. That made the case fail on the
/// reference engine but left the cross-engine comparison with a single boolean to diff, so a
/// QuickJS defect that returned a wrong-but-truthy value compared equal. Steps now call
/// [`observe_field`] with the value they already asserted, and [`Steps::finish`] records those
/// fields instead of the sentinel.
mod observed_fields {
    use std::cell::RefCell;

    use super::Value;

    thread_local! {
        static FIELDS: RefCell<Vec<(&'static str, Value)>> = const { RefCell::new(Vec::new()) };
    }

    pub fn push(name: &'static str, value: Value) {
        FIELDS.with(|fields| fields.borrow_mut().push((name, value)));
    }

    pub fn take() -> Vec<(&'static str, Value)> {
        FIELDS.with(|fields| std::mem::take(&mut *fields.borrow_mut()))
    }

    pub fn len() -> usize {
        FIELDS.with(|fields| fields.borrow().len())
    }

    pub fn truncate(len: usize) {
        FIELDS.with(|fields| fields.borrow_mut().truncate(len));
    }

    pub fn clear() {
        FIELDS.with(|fields| fields.borrow_mut().clear());
    }
}

/// Records one observation of the running `Steps` case.
///
/// A repeated name keeps both entries; `Observation::to_json` maps fields into an object, so
/// duplicate names would silently collapse. Steps therefore use distinct, step-prefixed names.
pub fn observe_field(name: &'static str, value: Value) {
    observed_fields::push(name, value);
}

/// Discards any observations left over from a case that ran earlier in this process.
pub fn clear_observed_fields() {
    observed_fields::clear();
}

/// The normalized, engine-independent observation of one contract case.
#[derive(Debug, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(String),
    Object,
    Array(Vec<Value>),
}

impl Value {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Undefined => serde_json::json!({ "type": "undefined" }),
            Value::Null => serde_json::Value::Null,
            Value::Boolean(value) => serde_json::Value::Bool(*value),
            Value::Number(value) if value.is_finite() && value.fract() == 0.0 => {
                serde_json::Number::from(*value as i64).into()
            },
            Value::Number(value) => serde_json::json!(value),
            Value::String(value) => serde_json::Value::String(value.clone()),
            Value::Object => serde_json::json!({ "type": "object" }),
            Value::Array(values) => values.iter().map(Value::to_json).collect(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Observation {
    pub fields: Vec<(&'static str, Value)>,
    pub exception: Option<String>,
    /// Failures from independent sub-scenarios, in execution order. `fields["panic"]` keeps the
    /// first failure as the comparison fingerprint, while this list makes every later failure
    /// visible instead of silently reducing the case to its first bad assertion.
    pub step_failures: Vec<(String, String)>,
}

impl Observation {
    pub fn new(fields: Vec<(&'static str, Value)>) -> Self {
        Observation {
            fields,
            exception: None,
            step_failures: Vec::new(),
        }
    }

    pub fn ok(value: Value) -> Self {
        Observation {
            fields: vec![("value", value)],
            exception: None,
            step_failures: Vec::new(),
        }
    }

    pub fn exception(name: &str) -> Self {
        Observation {
            fields: Vec::new(),
            exception: Some(name.to_owned()),
            step_failures: Vec::new(),
        }
    }

    pub fn failure(message: &str) -> Self {
        Observation {
            fields: vec![("panic", Value::String(message.to_owned()))],
            exception: None,
            step_failures: Vec::new(),
        }
    }

    pub fn with_step_failures(mut self, step_failures: Vec<(String, String)>) -> Self {
        self.step_failures = step_failures;
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut fields = self
            .fields
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.to_json()))
            .collect::<serde_json::Map<_, _>>();
        if !self.step_failures.is_empty() {
            fields.insert(
                "step_failures".to_owned(),
                serde_json::Value::Array(
                    self.step_failures
                        .iter()
                        .map(
                            |(name, message)| serde_json::json!({ "step": name, "panic": message }),
                        )
                        .collect(),
                ),
            );
        }
        serde_json::json!({
            "fields": fields,
            "exception": self.exception,
        })
    }
}

/// Runs `f` in a fresh runtime and global object, like a document realm.
pub fn with_global<R>(engine: &JSEngine, f: impl FnOnce(&mut JSContext, HandleObject) -> R) -> R {
    with_realm_options(engine, RealmOptions::default(), f)
}

/// Runs `f` in a fresh runtime with caller-selected realm options.
pub fn with_realm_options<R>(
    engine: &JSEngine,
    options: RealmOptions,
    f: impl FnOnce(&mut JSContext, HandleObject) -> R,
) -> R {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*options,
        ));
        assert!(!global.get().is_null(), "JS_NewGlobalObject returned null");
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        f(&mut realm, global.handle())
    }
}

/// Evaluates `source` in the realm and returns its completion value.
pub fn eval(cx: &mut JSContext, global: HandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"engine_contract.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options)
        .unwrap_or_else(|_| panic!("evaluating {source:?} failed"));
    rval.get()
}

/// Sets `globalThis[name] = value` so later snippets can observe a native object.
pub fn set_global(cx: &mut JSContext, global: HandleObject, name: &CStr, value: JSVal) {
    unsafe {
        rooted!(&in(cx) let value = value);
        assert!(
            JS_SetProperty(cx, global, name.as_ptr(), value.handle()),
            "setting global {name:?}"
        );
    }
}

/// Normalizes a JS value without depending on engine-specific types.
pub fn observe(cx: &mut JSContext, value: JSVal) -> Value {
    if value.is_undefined() {
        Value::Undefined
    } else if value.is_null() {
        Value::Null
    } else if value.is_boolean() {
        Value::Boolean(value.to_boolean())
    } else if value.is_number() {
        Value::Number(value.to_number())
    } else if value.is_string() {
        let string = value.to_string();
        let string = std::ptr::NonNull::new(string).expect("non-null JS string");
        Value::String(unsafe { jsstr_to_string(cx, string) })
    } else {
        Value::Object
    }
}

/// Evaluates `source` and normalizes its completion value.
pub fn eval_observe(cx: &mut JSContext, global: HandleObject, source: &str) -> Value {
    let value = eval(cx, global, source);
    observe(cx, value)
}

/// Returns a stable name for a promise state.
pub fn promise_state_name(state: PromiseState) -> &'static str {
    match state {
        PromiseState::Pending => "pending",
        PromiseState::Fulfilled => "fulfilled",
        PromiseState::Rejected => "rejected",
    }
}

/// Returns a stable name for a module request type.
pub fn module_type_name(module_type: ModuleType) -> &'static str {
    match module_type {
        ModuleType::Unknown => "unknown",
        ModuleType::JavaScript => "javascript",
        ModuleType::JSON => "json",
        ModuleType::CSS => "css",
        ModuleType::Bytes => "bytes",
        ModuleType::Text => "text",
    }
}

/// Reads `name` from an arbitrary object.
pub fn get_property(cx: &mut JSContext, object: JSVal, name: &CStr) -> JSVal {
    use js::rust::wrappers2::JS_GetProperty;

    unsafe {
        rooted!(&in(cx) let object = object.to_object());
        rooted!(&in(cx) let mut value = UndefinedValue());
        assert!(
            JS_GetProperty(cx, object.handle(), name.as_ptr(), value.handle_mut()),
            "getting property {name:?}"
        );
        value.get()
    }
}

/// Reads and normalizes `name` from an arbitrary object.
pub fn observe_property(cx: &mut JSContext, object: JSVal, name: &CStr) -> Value {
    let value = get_property(cx, object, name);
    observe(cx, value)
}

/// Takes the pending exception and asserts its `name`, so a failure path cannot pass merely by
/// leaving some unrelated exception pending.
#[track_caller]
pub fn take_exception_named(cx: &mut JSContext, expected: &str) -> String {
    assert!(
        unsafe { JS_IsExceptionPending(cx) },
        "expected a pending {expected}"
    );
    rooted!(&in(cx) let mut exception = UndefinedValue());
    assert!(
        unsafe { JS_GetPendingException(cx, exception.handle_mut()) },
        "reading the pending exception"
    );
    let name = observe_property(cx, exception.get(), c"name");
    assert_eq!(
        name,
        Value::String(expected.to_owned()),
        "unexpected pending exception"
    );
    unsafe { JS_ClearPendingException(cx) };
    assert!(!unsafe { JS_IsExceptionPending(cx) });
    expected.to_owned()
}

/// Asserts the pending exception is an `Error` object named `expected` and clears it.
#[track_caller]
pub unsafe fn assert_exception_named(cx: &mut JSContext, expected: &str) {
    assert!(JS_IsExceptionPending(cx), "expected a pending {expected}");
    rooted!(&in(cx) let mut exception = UndefinedValue());
    assert!(
        JS_GetPendingException(cx, exception.handle_mut()),
        "reading the pending exception"
    );
    let value = exception.get();
    assert!(value.is_object(), "expected an {expected} object");
    assert_eq!(
        observe_property(cx, value, c"name"),
        Value::String(expected.to_owned()),
        "unexpected pending exception"
    );
    unsafe { js::jsapi::JS_ClearPendingException(cx.raw_cx()) };
    assert!(!unsafe { JS_IsExceptionPending(cx) });
}

static JOB_QUEUE_TRAPS: js::glue::JobQueueTraps = js::glue::JobQueueTraps {
    getHostDefinedData: Some(job_queue_host_defined_data),
    getHostDefinedGlobal: Some(job_queue_host_defined_global),
    runJobs: Some(run_queued_jobs),
    traceNonGCThingMicroTask: None,
};

unsafe extern "C" fn job_queue_host_defined_data(
    _cx: *mut js::jsapi::JSContext,
    incumbent: js::jsapi::MutableHandleObject,
    data: js::jsapi::MutableHandleObject,
) -> bool {
    incumbent.set(ptr::null_mut());
    data.set(ptr::null_mut());
    true
}

unsafe extern "C" fn job_queue_host_defined_global(
    _cx: *mut js::jsapi::JSContext,
    _incumbent: js::jsapi::MutableHandleObject,
) -> bool {
    true
}

unsafe extern "C" fn run_queued_jobs(cx: *mut js::jsapi::JSContext) {
    let cx = std::ptr::NonNull::new(cx).expect("non-null JSContext in the job queue trap");
    let mut cx = JSContext::from_ptr(cx);
    while HasAnyMicroTasks(&cx) {
        rooted!(&in(cx) let mut task = UndefinedValue());
        JS_DequeueNextMicroTask(&cx, task.handle_mut());
        assert!(js::jsapi::IsJSMicroTask(task.as_ptr()));
        let task = js::jsapi::ToMaybeWrappedJSMicroTask(task.as_ptr());
        rooted!(&in(cx) let task = task);
        assert!(RunJSMicroTask(&mut cx, task.handle()));
    }
}

/// Installs a host job queue for the current context and returns the queue to delete after use.
pub unsafe fn install_job_queue(cx: &mut JSContext) -> *mut std::ffi::c_void {
    let queue = js::glue::CreateJobQueue(&JOB_QUEUE_TRAPS);
    assert!(!queue.is_null());
    SetJobQueue(cx, queue as *mut js::jsapi::JobQueue);
    queue.cast()
}

pub unsafe fn remove_job_queue(cx: &mut JSContext, queue: *mut std::ffi::c_void) {
    SetJobQueue(cx, ptr::null_mut());
    js::glue::DeleteJobQueue(queue.cast());
}

/// Writes one JSON line per case so the two engine runs can be diffed.
///
/// `apis` names every mozjs entry point the case exercises. The coverage gate checks these names
/// against `servo-mozjs-api-usage.toml` and `mozjs-api-snapshot.toml`, so a case cannot silently claim capability it
/// never calls or a status that the shim does not support.
pub fn record(case: &str, observation: &Observation, apis: &[&str]) {
    static RESET: Once = Once::new();

    register_case(case);
    if !case_selected(case) || dry_run() {
        return;
    }
    mark_observed(case);

    let Ok(path) = std::env::var("CONTRACT_OUT") else {
        return;
    };
    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent).expect("creating CONTRACT_OUT parent directory");
    }
    // A per-case process must append to a file the driver owns; only an in-process run creates
    // (and therefore truncates) the file itself.
    if std::env::var_os("CONTRACT_OUT_APPEND").is_none() {
        RESET.call_once(|| {
            std::fs::write(&path, b"").expect("truncating CONTRACT_OUT");
        });
    }
    let engine = contract_engine();
    let observation = observation.to_json();
    let line = serde_json::json!({
        "id": case,
        "engine": engine,
        "fields": observation["fields"],
        "exception": observation["exception"],
        "apis": apis,
    });
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("opening CONTRACT_OUT");
    writeln!(file, "{line}").expect("writing CONTRACT_OUT");
}

static CASE_FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static SELECTED_CASES: OnceLock<Option<HashSet<String>>> = OnceLock::new();
static SELECTED_STEPS: OnceLock<Option<HashSet<String>>> = OnceLock::new();
static OBSERVED_CASES: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static REGISTERED_CASES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn selected_cases() -> &'static Option<HashSet<String>> {
    SELECTED_CASES.get_or_init(|| {
        let Ok(value) = std::env::var("CONTRACT_CASES") else {
            return None;
        };
        let cases = value
            .split([',', ' ', '\t', '\n'])
            .filter(|case| !case.is_empty())
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        assert!(
            !cases.is_empty(),
            "CONTRACT_CASES is set but does not name any case"
        );
        Some(cases)
    })
}

fn selected_steps() -> &'static Option<HashSet<String>> {
    SELECTED_STEPS.get_or_init(|| {
        let Ok(value) = std::env::var("CONTRACT_STEP") else {
            return None;
        };
        let steps = value
            .split([',', ' ', '\t', '\n'])
            .filter(|step| !step.is_empty())
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        assert!(
            !steps.is_empty(),
            "CONTRACT_STEP is set but does not name any step"
        );
        Some(steps)
    })
}

/// Returns whether `step` should run. `CONTRACT_STEP` narrows a compound case to one or more
/// sub-scenarios, which is how an interface-level run attributes a failure to a single API group.
pub fn step_selected(step: &str) -> bool {
    selected_steps()
        .as_ref()
        .is_none_or(|steps| steps.contains(step))
}

/// Returns whether `case` should run in this invocation.
///
/// Without `CONTRACT_CASES`, every case runs. When it is set, only the listed cases run, so a
/// single-case QJS investigation does not pay for the full suite.
pub fn case_selected(case: &str) -> bool {
    selected_cases()
        .as_ref()
        .is_none_or(|cases| cases.contains(case))
}

fn register_case(case: &str) {
    REGISTERED_CASES
        .lock()
        .expect("registered case list")
        .insert(case.to_owned());
}

/// Writes the cases that actually reached the harness. This is the gate's source of truth: unlike
/// a source scan it also sees case ids computed at runtime. With `CONTRACT_CASES`, write the
/// selected set so a single-case run remains diffable; a selected case that never runs is reported
/// separately by `assert_no_case_failures`.
fn flush_case_inventory() {
    let Ok(path) = std::env::var("CONTRACT_CASES_OUT") else {
        return;
    };
    let cases = match selected_cases() {
        Some(selected) => selected.iter().cloned().collect::<BTreeSet<_>>(),
        None => REGISTERED_CASES
            .lock()
            .expect("registered case list")
            .clone(),
    };
    let document = serde_json::json!({
        "version": 1,
        "cases": cases.into_iter().collect::<Vec<_>>(),
        "complete": selected_cases().is_none(),
    });
    if let Some(parent) = Path::new(&path).parent() {
        std::fs::create_dir_all(parent).expect("creating CONTRACT_CASES_OUT parent directory");
    }
    std::fs::write(path, format!("{document}\n").as_bytes()).expect("writing CONTRACT_CASES_OUT");
}

fn mark_observed(case: &str) {
    if selected_cases().is_some() {
        OBSERVED_CASES
            .lock()
            .expect("observed case list")
            .get_or_insert_with(HashSet::new)
            .insert(case.to_owned());
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "case panicked with a non-string payload".to_owned()
    }
}

/// True while the harness is only collecting the case ids an isolated run should execute.
fn dry_run() -> bool {
    std::env::var_os("CONTRACT_DRY_RUN").is_some()
}

/// Runs one case in isolation. A panic is recorded as that case's observation and collected for
/// the final test failure, so every later case still runs on the same engine.
pub fn run_case(case: &str, apis: &[&str], f: impl FnOnce() -> Observation) {
    register_case(case);
    if !case_selected(case) || dry_run() {
        return;
    }
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(observation) => record(case, &observation, apis),
        Err(payload) => {
            let message = panic_message(payload);
            eprintln!("case {case} failed: {message}");
            record(case, &Observation::failure(&message), apis);
            CASE_FAILURES
                .lock()
                .expect("case failure list")
                .push(format!("{case}: {message}"));
        },
    }
}

/// Isolates a case that records itself on success. On panic, a failure observation is written
/// under the same case id; the failed run is therefore never silently compared as a match, and
/// later cases still execute.
///
/// The case cannot record after it panics, so its `apis` list is copied from the reference
/// engine's result. That keeps a failed case attributable at the interface level instead of
/// dropping its coverage evidence.
pub fn isolate_case(case: &str, f: impl FnOnce()) {
    register_case(case);
    if !case_selected(case) || dry_run() {
        return;
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
        let message = panic_message(payload);
        eprintln!("case {case} failed: {message}");
        let reference = reference_apis(case);
        let apis = reference.iter().map(String::as_str).collect::<Vec<_>>();
        record(case, &Observation::failure(&message), &apis);
        CASE_FAILURES
            .lock()
            .expect("case failure list")
            .push(format!("{case}: {message}"));
    }
}

/// Runs the independent scenarios of one case so a panic in one cannot hide the rest.
///
/// The first panic is recorded as the case observation, so its message stays the comparison
/// fingerprint, while every remaining step still runs. Calling the steps one after another would
/// instead let the first failing assertion end the case and silently drop all later coverage.
pub struct Steps<'a> {
    case: &'a str,
    apis: &'a [&'a str],
    failures: Vec<(String, String)>,
}

impl<'a> Steps<'a> {
    pub fn new(case: &'a str, apis: &'a [&'a str]) -> Self {
        register_case(case);
        observed_fields::clear();
        Steps {
            case,
            apis,
            failures: Vec::new(),
        }
    }

    /// Runs one scenario. A panic is reported with the scenario name and remembered; the
    /// remaining scenarios still run.
    pub fn step(&mut self, name: &str, f: impl FnOnce()) {
        if !case_selected(self.case) || !step_selected(name) || dry_run() {
            return;
        }
        // A step that panics halfway may already have recorded some of its observations. Drop
        // them so a failed step cannot leave a partial payload that looks like a passing one;
        // the failure itself is reported through `step_failures`.
        let mark = observed_fields::len();
        if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
            let message = panic_message(payload);
            eprintln!("case {} step {name} failed: {message}", self.case);
            observed_fields::truncate(mark);
            self.failures.push((name.to_owned(), message));
        }
    }

    /// Turns the run into one observation, without recording it. The caller records it through
    /// [`run_case`], which is how a case keeps control of its own observation shape.
    pub fn into_observation(self) -> Observation {
        let Some((_, first)) = self.failures.first() else {
            return Observation::ok(Value::Boolean(true));
        };
        eprintln!("case {} failed: {first}", self.case);
        Observation::failure(first).with_step_failures(self.failures)
    }

    /// Records the case after every scenario has run, using the observations its steps recorded
    /// through [`observe_field`].
    pub fn finish(self) {
        let fields = observed_fields::take();
        self.finish_with_fields(fields);
    }

    /// Records the case with the observations collected by its successful scenarios.
    ///
    /// A scenario that panics still contributes its failure through `step_failures`; the
    /// surviving observations remain in the record so one bad sub-scenario does not erase the
    /// evidence gathered by the others.
    pub fn finish_with_fields(self, mut fields: Vec<(&'static str, Value)>) {
        if !case_selected(self.case) || dry_run() {
            return;
        }
        // `Steps::new` starts a case; earlier cases in this process may have left observations
        // behind when they were filtered out or panicked before `finish`.
        fields.extend(observed_fields::take());
        let case = self.case;
        let apis = self.apis;
        let failures = self.failures.clone();
        let mut observation = self.into_observation();
        observation.fields.extend(fields);
        record(case, &observation, apis);
        if !failures.is_empty() {
            let summary = failures
                .iter()
                .map(|(step, message)| format!("{step}: {message}"))
                .collect::<Vec<_>>()
                .join("\n");
            CASE_FAILURES
                .lock()
                .expect("case failure list")
                .push(format!("{case}: {summary}"));
        }
    }
}

/// Re-executes the current test binary once per case so that a crash cannot hide later cases.
///
/// `catch_unwind` in [`run_case`] covers Rust panics, but not SIGSEGV, SIGABRT, or a panic that
/// aborts: any of those kills the process and would silently drop every case after the crashing
/// one. When `CONTRACT_ISOLATE_CASES=1` is set, the test entry calls this instead of running the
/// suite in-process. Each case gets its own process, and a case that dies without writing a record
/// is reported with the exit status as its observation, so the run stays attributable.
///
/// Returns `false` when isolation is not requested, so the caller runs the suite normally.
pub fn run_isolated() -> bool {
    if std::env::var("CONTRACT_ISOLATE_CASES").as_deref() != Ok("1") {
        return false;
    }
    let Ok(results) = std::env::var("CONTRACT_OUT") else {
        eprintln!("CONTRACT_ISOLATE_CASES=1 needs CONTRACT_OUT");
        std::process::exit(2);
    };
    let engine = contract_engine();
    let selection = std::env::var("CONTRACT_CASES").unwrap_or_default();

    let cases = collect_cases(&selection);
    if cases.is_empty() {
        eprintln!("no contract cases selected for an isolated run");
        std::process::exit(2);
    }

    let mut crashed = Vec::new();
    let mut failed = Vec::new();
    let mut records = Vec::new();
    for case in &cases {
        let record = run_one_isolated_case(case, &engine, &results);
        if record.crashed {
            crashed.push(case.clone());
        }
        if record.failed {
            failed.push(case.clone());
        }
        records.push(record.line);
    }

    if let Some(parent) = Path::new(&results).parent() {
        std::fs::create_dir_all(parent).expect("creating CONTRACT_OUT parent directory");
    }
    let body = records.join("\n");
    std::fs::write(&results, format!("{body}\n")).expect("writing merged CONTRACT_OUT");
    flush_case_inventory_for(&cases);

    for case in &crashed {
        eprintln!("case {case} crashed without recording a result");
    }
    for case in &failed {
        eprintln!("case {case} exited non-zero; its recorded observation is still compared");
    }
    eprintln!(
        "recorded {} cases, {} failed, {} crashed",
        cases.len(),
        failed.len(),
        crashed.len()
    );
    // Exit here: the parent process must not also run the suite in-process, which would double
    // every observation and re-run cases that already failed or crashed. A failed child is still
    // an error even when it wrote an observation: the assertion failure must not look like a
    // successful runner.
    std::process::exit(if failed.is_empty() { 0 } else { 1 });
}

/// The engine marker written into every result line.
///
/// It is required whenever results are recorded: an absent or misspelled value used to be written
/// as `"unknown"`, which only failed later in `compare.py`. Rejecting it here keeps the failure at
/// the recording site and makes the JSONL self-describing.
fn contract_engine() -> String {
    let engine = std::env::var("CONTRACT_ENGINE")
        .expect("CONTRACT_ENGINE must be set when CONTRACT_OUT is written");
    assert!(
        engine == "spidermonkey" || engine == "quickjs",
        "CONTRACT_ENGINE must be spidermonkey or quickjs, got {engine:?}"
    );
    engine
}

/// Runs the suite once without executing any case to learn which ids the selection covers.
fn collect_cases(selection: &str) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("engine-contract-cases-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let inventory = dir.join("cases.json");
    let _ = std::fs::remove_file(&inventory);

    let exe = std::env::current_exe().expect("locating the test binary");
    let mut command = std::process::Command::new(exe);
    command
        .args(["tests::engine_contract", "--exact"])
        .env("CONTRACT_DRY_RUN", "1")
        .env("CONTRACT_CASES_OUT", &inventory)
        .env_remove("CONTRACT_ISOLATE_CASES")
        .env_remove("CONTRACT_OUT")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if !selection.is_empty() {
        command.env("CONTRACT_CASES", selection);
    }
    let _ = command.status();

    let cases = std::fs::read_to_string(&inventory)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|document| {
            document
                .get("cases")
                .and_then(|value| value.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
        })
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    cases
}

fn flush_case_inventory_for(cases: &[String]) {
    let Ok(path) = std::env::var("CONTRACT_CASES_OUT") else {
        return;
    };
    let document = serde_json::json!({
        "version": 1,
        "cases": cases,
        "complete": std::env::var_os("CONTRACT_CASES").is_none(),
    });
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{document}\n"));
}

struct IsolatedRecord {
    line: String,
    crashed: bool,
    failed: bool,
}

fn run_one_isolated_case(case: &str, engine: &str, results: &str) -> IsolatedRecord {
    let dir = Path::new(results)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("isolated-cases");
    std::fs::create_dir_all(&dir).expect("creating the isolated case directory");
    let output = dir.join(format!("{case}.jsonl"));
    let log = dir.join(format!("{case}.log"));
    let _ = std::fs::remove_file(&output);

    let exe = std::env::current_exe().expect("locating the test binary");
    let log_file = std::fs::File::create(&log).expect("creating the isolated case log");
    let log_err = log_file.try_clone().expect("cloning the isolated case log");
    let status = std::process::Command::new(exe)
        .args(["tests::engine_contract", "--exact", "--nocapture"])
        .env("CONTRACT_ENGINE", engine)
        .env("CONTRACT_CASES", case)
        .env("CONTRACT_OUT", &output)
        .env_remove("CONTRACT_CASES_OUT")
        .env_remove("CONTRACT_ISOLATE_CASES")
        .env("CONTRACT_OUT_APPEND", "1")
        .stdout(log_file)
        .stderr(log_err)
        .status()
        .expect("running an isolated contract case");

    if let Ok(text) = std::fs::read_to_string(&output) {
        if let Some(line) = text.lines().rev().find(|line| !line.trim().is_empty()) {
            return IsolatedRecord {
                line: line.trim().to_owned(),
                crashed: false,
                failed: !status.success(),
            };
        }
    }

    let description = match status.code() {
        Some(code) => format!("exit {code}"),
        None => "terminated by a signal".to_owned(),
    };
    let reference_apis = reference_apis(case);
    let line = serde_json::json!({
        "id": case,
        "engine": engine,
        "fields": {
            "panic": format!("case process exited without recording a result ({description})"),
        },
        "exception": serde_json::Value::Null,
        "apis": reference_apis,
    })
    .to_string();
    IsolatedRecord {
        line,
        crashed: true,
        failed: true,
    }
}

/// Copies `apis` from the other engine's results so a crashed case stays comparable.
fn reference_apis(case: &str) -> Vec<String> {
    let Ok(path) = std::env::var("CONTRACT_REFERENCE_OUT") else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<serde_json::Value>(line) {
            if record.get("id").and_then(|value| value.as_str()) == Some(case) {
                return record
                    .get("apis")
                    .and_then(|value| value.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }
    }
    Vec::new()
}

/// Fails the enclosing test after every case has had a chance to run.
pub fn assert_no_case_failures() {
    flush_case_inventory();

    if let Some(selected) = selected_cases() {
        let observed = OBSERVED_CASES
            .lock()
            .expect("observed case list")
            .clone()
            .unwrap_or_default();
        let mut missing = selected.difference(&observed).cloned().collect::<Vec<_>>();
        missing.sort();
        if !missing.is_empty() {
            panic!(
                "{} selected engine contract case(s) did not run:\n{}",
                missing.len(),
                missing.join("\n")
            );
        }
    }

    let failures = CASE_FAILURES.lock().expect("case failure list");
    if !failures.is_empty() {
        panic!(
            "{} engine contract case(s) failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

/// Runs a module after its cases have been isolated. A panic outside a case still stops that
/// module, but it is reported with the other failures instead of hiding later modules.
pub fn run_module(name: &str, f: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
        let message = panic_message(payload);
        eprintln!("module {name} failed: {message}");
        CASE_FAILURES
            .lock()
            .expect("case failure list")
            .push(format!("{name}: {message}"));
    }
}

/// Runs a case in a fresh runtime and records its observation together with the entries it
/// exercises. The `apis` list is the coverage evidence: every named entry must be classified as
/// implemented in `mozjs-api-snapshot.toml`, or as `empty`/`todo` in both the inventory and
/// `servo-mozjs-api-usage.toml`. Every scoped entry must appear in at least one case.
pub fn record_case(
    engine: &JSEngine,
    case: &str,
    apis: &[&str],
    f: impl FnOnce(&mut JSContext, HandleObject) -> Observation,
) {
    run_case(case, apis, || {
        with_global(engine, |cx, global| f(cx, global))
    });
}

/// Like [`record_case`], but constructs the realm with the supplied options.
pub fn record_case_with_options(
    engine: &JSEngine,
    case: &str,
    apis: &[&str],
    options: RealmOptions,
    f: impl FnOnce(&mut JSContext, HandleObject) -> Observation,
) {
    run_case(case, apis, || {
        with_realm_options(engine, options, |cx, global| f(cx, global))
    });
}
