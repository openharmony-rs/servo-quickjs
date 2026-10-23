/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Terminating a script, by an interrupt callback that returns false: as in SpiderMonkey and
//! HTML's "abort a running script", no more JS runs for it. QuickJS's built-ins that catch
//! exceptions (promise executors and resolving functions, promise jobs, async functions and
//! generators, iterator closing) leave the termination pending instead of turning it into a
//! rejection or a caught exception (Q37). Each route is also run with an ordinary exception, which
//! must behave as before.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use mozjs::context::{JSContext, RawJSContext};
use mozjs::conversions::jsstr_to_string;
use mozjs::jsapi::{self, JSObject, OnNewGlobalHookOption, Value};
use mozjs::jsval::{JSVal, UndefinedValue};
use mozjs::qjs;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::JS_NewGlobalObject;
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine,
    JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};

thread_local!(static INTERRUPTS: Cell<usize> = const { Cell::new(0) });

unsafe extern "C" fn terminate(_: *mut RawJSContext) -> bool {
    INTERRUPTS.with(|i| i.set(i.get() + 1));
    false
}

/// `requestTermination()`: requests an interrupt, which the next poll handles.
unsafe extern "C" fn request_termination(cx: *mut RawJSContext, _: u32, _: *mut Value) -> bool {
    jsapi::JS_RequestInterruptCallback(cx);
    true
}

/// `stop()` terminates the script inside its loop; `fail()` throws an ordinary exception.
const PRELUDE: &str = "
var log = [];
function stop() { requestTermination(); for (let i = 0; i < 1e5; i++) {} log.push('not terminated'); }
function fail() { throw new Error('fail'); }
function iter(onReturn) {
  return { [Symbol.iterator]() {
    return { next() { return { value: 1, done: false }; },
             return() { log.push('return'); onReturn(); return {}; } };
  } };
}";

/// A route: its source with `STEP` for `stop()` or `fail()`; whether the evaluation fails when it
/// is terminated (before any job runs); and `log` after the jobs ran, when terminated and when it
/// threw an ordinary exception.
struct Route {
    name: &'static str,
    source: &'static str,
    terminated_eval_fails: bool,
    terminated_log: &'static str,
    ordinary_log: &'static str,
}

const ROUTES: &[Route] = &[
    Route {
        name: "promise executor",
        source: "new Promise(() => { STEP; }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "then getter (Promise.resolve)",
        source: "Promise.resolve({ get then() { STEP; } }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "then getter (resolve function)",
        source: "new Promise(r => r({ get then() { STEP; } })).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "thenable job",
        source: "Promise.resolve({ then() { STEP; } }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: false,
        terminated_log: "after",
        ordinary_log: "after,catch",
    },
    Route {
        name: "reaction job",
        source: "Promise.resolve().then(() => { STEP; }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: false,
        terminated_log: "after",
        ordinary_log: "after,catch",
    },
    Route {
        name: "Promise.try",
        source: "Promise.try(() => { STEP; }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "Promise.all",
        source: "Promise.all({ [Symbol.iterator]() { STEP; } }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "Promise.race",
        source: "Promise.race({ [Symbol.iterator]() { STEP; } }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "async function",
        source: "(async () => { STEP; })().catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "async function after await",
        source: "(async () => { await null; STEP; log.push('resumed'); })().catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: false,
        terminated_log: "after",
        ordinary_log: "after,catch",
    },
    Route {
        name: "async function with finally",
        source: "(async () => { try { STEP; } finally { log.push('finally'); } })().catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "finally,after,catch",
    },
    Route {
        name: "async generator",
        source: "(async function* () { STEP; })().next().catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "async generator after await",
        source: "(async function* () { await null; STEP; yield 1; })().next().catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: false,
        terminated_log: "after",
        ordinary_log: "after,catch",
    },
    Route {
        name: "for await of a sync iterator",
        source: "(async () => { for await (const x of { [Symbol.iterator]() { return { next() { STEP; } }; } }) {} })().catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
    Route {
        name: "Array.fromAsync",
        source: "Array.fromAsync([1], () => { STEP; }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: false,
        terminated_log: "after",
        ordinary_log: "after,catch",
    },
    Route {
        name: "Array.from closing its iterator",
        source: "try { Array.from(iter(() => {}), () => { STEP; }); } catch (e) { log.push('catch'); } log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "return,catch,after",
    },
    Route {
        name: "return() of a for-of loop left by a throw",
        source: "try { for (const x of iter(() => { STEP; })) { throw 0; } } catch (e) { log.push('catch'); } log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "return",
        ordinary_log: "return,catch,after",
    },
    Route {
        name: "import() specifier",
        source: "import({ toString() { STEP; } }).catch(() => log.push('catch')); log.push('after');",
        terminated_eval_fails: true,
        terminated_log: "",
        ordinary_log: "after,catch",
    },
];

/// The engine can be initialized once per process; the tests share it and never shut it down.
fn engine() -> JSEngineHandle {
    static ENGINE: OnceLock<JSEngineHandle> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let engine = JSEngine::init().unwrap();
            let handle = engine.handle();
            std::mem::forget(engine);
            handle
        })
        .clone()
}

fn eval(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> Result<JSVal, ()> {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"termination.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options)?;
    Ok(rval.get())
}

/// QuickJS's pending exception, which `JS_IsExceptionPending` clears if it is a termination: `Some`
/// with whether it is a termination. `function` is a function of the realm, which QuickJS maps to
/// its context.
unsafe fn qjs_pending(function: *mut JSObject) -> Option<bool> {
    let realm = qjs::JS_GetObjectRealm(qjs::JSValue::mkptr(
        qjs::JS_TAG_OBJECT,
        function as *mut c_void,
    ));
    assert!(!realm.is_null());
    (qjs::JS_HasException(realm) != 0).then(|| qjs::JS_HasUncatchableException(realm) != 0)
}

unsafe fn log(cx: &mut JSContext, global: SafeHandleObject) -> String {
    let v = eval(cx, global, "log.join()").unwrap();
    jsstr_to_string(cx, NonNull::new(v.to_string()).unwrap())
}

/// Runs `source` in a new runtime, then `followup`; returns whether the evaluation of `source`
/// succeeded and `log` after the jobs of both ran.
fn run(source: &str, followup: &str) -> (bool, String) {
    let mut runtime = Runtime::new(engine());
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
        let (global, cx) = realm.global_and_reborrow();
        assert!(jsapi::JS_AddInterruptCallback(cx.raw_cx(), Some(terminate)));
        assert!(!jsapi::JS_DefineFunction(
            cx.raw_cx(),
            global.into(),
            c"requestTermination".as_ptr(),
            Some(request_termination),
            0,
            0
        )
        .is_null());
        eval(cx, global, PRELUDE).unwrap();
        let function = eval(cx, global, "Object").unwrap().to_object();
        let ok = eval(cx, global, source).is_ok();
        // A terminated evaluation fails with the termination still pending in QuickJS, uncaught,
        // which the shim reports as no exception, as SpiderMonkey does.
        assert_eq!(qjs_pending(function), (!ok).then_some(true));
        assert!(!jsapi::JS_IsExceptionPending(cx.raw_cx()));
        jsapi::RunJobs(cx.raw_cx());
        eval(cx, global, followup).unwrap();
        jsapi::RunJobs(cx.raw_cx());
        (ok, log(cx, global))
    }
}

/// After the route, script still runs in the runtime: exceptions are caught and jobs run.
const FOLLOWUP: &str =
    "try { fail(); } catch (e) { log.push('caught'); } Promise.resolve().then(() => log.push('job'));";

fn with_followup(log: &str) -> String {
    if log.is_empty() {
        "caught,job".to_owned()
    } else {
        format!("{log},caught,job")
    }
}

#[test]
fn termination_routes() {
    let mut failures = Vec::new();
    for route in ROUTES {
        INTERRUPTS.with(|i| i.set(0));
        let terminated = run(&route.source.replace("STEP", "stop()"), FOLLOWUP);
        let interrupts = INTERRUPTS.with(Cell::get);
        let expected = (
            !route.terminated_eval_fails,
            with_followup(route.terminated_log),
        );
        if interrupts != 1 || terminated != expected {
            failures.push(format!(
                "{} (terminated): {interrupts} interrupts, (evaluated, log) {terminated:?}, expected {expected:?}",
                route.name
            ));
        }
        let ordinary = run(&route.source.replace("STEP", "fail()"), FOLLOWUP);
        let expected = (true, with_followup(route.ordinary_log));
        if ordinary != expected {
            failures.push(format!(
                "{} (ordinary exception): {ordinary:?}, expected {expected:?}",
                route.name
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// As in SpiderMonkey, an async generator whose script was terminated settles no more requests:
/// neither the terminated one nor later ones, also when they were queued before the termination,
/// and its body does not resume.
#[test]
fn terminated_async_generator() {
    let source = "
        var it = (async function* () { await null; STEP; yield 1; })();
        it.next().then(() => log.push('next'), () => log.push('rejected'));
        it.next().then(v => log.push('queued ' + v.done), () => log.push('queued rejected'));";
    let followup = "
        it.next().then(v => log.push('later ' + v.done));
        it.return(2).then(v => log.push('return ' + v.value));";
    let terminated = run(&source.replace("STEP", "stop()"), followup);
    assert_eq!(terminated, (true, String::new()));
    let ordinary = run(&source.replace("STEP", "fail()"), followup);
    assert_eq!(
        ordinary,
        (true, "rejected,queued true,later true,return 2".to_owned())
    );

    // Also when settling a request runs script: a `then` getter of the iterator result.
    let source = "
        var it = (async function* () { yield 1; log.push('resumed'); yield 2; })();
        Object.defineProperty(Object.prototype, 'then', {
          get() { delete Object.prototype.then; STEP; }, configurable: true });
        it.next().then(() => log.push('next'), () => log.push('rejected'));";
    let followup = "it.next().then(() => log.push('later'));";
    let terminated = run(&source.replace("STEP", "stop()"), followup);
    assert_eq!(terminated.1, "");
    let ordinary = run(&source.replace("STEP", "fail()"), followup);
    assert_eq!(ordinary, (true, "rejected,resumed,later".to_owned()));
}
