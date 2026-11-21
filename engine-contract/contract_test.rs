/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use js::jsapi::{GCReason, HandleValueArray, JSTracer};
use js::jsval::{Int32Value, UndefinedValue};
use js::rooted;
use js::rust::wrappers2::{
    JS_CallFunctionValue, JS_ClearPendingException, JS_GC, JS_GetPendingException,
    JS_IsExceptionPending,
};
use js::rust::{CustomAutoRooter, CustomTrace, JSEngine};

use crate::support::{Observation, Value, eval, get_property, observe, run_case, with_global};

pub fn run(engine: &JSEngine) {
    if crate::support::case_selected("call.throw_pending_exception") ||
        crate::support::case_selected("rooter.trace_on_gc")
    {
        call_cases(engine);
    }
    if crate::support::case_selected("rooter.trace_on_gc") {
        rooter_cases(engine);
    }
}

fn call_cases(engine: &JSEngine) {
    if !crate::support::case_selected("call.throw_pending_exception") {
        return;
    }
    with_global(engine, |cx, global| unsafe {
        let function = eval(cx, global, "(function (a, b) { return a + b; })");
        rooted!(&in(cx) let function = function);
        rooted!(&in(cx) let arguments = vec![Int32Value(2), Int32Value(3)]);
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let thrower = eval(cx, global, "(function () { throw new TypeError('boom'); })");
        rooted!(&in(cx) let thrower = thrower);
        let no_arguments = HandleValueArray::empty();
        rooted!(&in(cx) let mut exception = UndefinedValue());
        run_case(
            "call.throw_pending_exception",
            &[
                "JS_NewGlobalObject",
                "JS_CallFunctionValue",
                "JS_IsExceptionPending",
                "JS_GetPendingException",
                "JS_ClearPendingException",
            ],
            || {
                assert!(!JS_CallFunctionValue(
                    cx,
                    global,
                    thrower.handle(),
                    &no_arguments,
                    rval.handle_mut()
                ));
                assert!(JS_IsExceptionPending(cx));
                let pending_before_clear = JS_IsExceptionPending(cx);
                assert_eq!(
                    pending_before_clear, true,
                    "the thrown exception is not pending"
                );
                assert!(JS_GetPendingException(cx, exception.handle_mut()));
                let name = get_property(cx, exception.get(), c"name");
                let name = match observe(cx, name) {
                    Value::String(name) => name,
                    other => panic!("exception name is not a string: {other:?}"),
                };
                assert_eq!(name, "TypeError", "unexpected pending exception");
                JS_ClearPendingException(cx);
                let pending_after_clear = JS_IsExceptionPending(cx);
                assert_eq!(
                    pending_after_clear, false,
                    "JS_ClearPendingException left the exception pending"
                );
                Observation::new(vec![
                    ("exception_name", Value::String(name)),
                    ("pending_before_clear", Value::Boolean(pending_before_clear)),
                    ("pending_after_clear", Value::Boolean(pending_after_clear)),
                ])
            },
        );
    });
}

struct TraceCheck(Cell<bool>);

unsafe impl CustomTrace for TraceCheck {
    fn trace(&self, _: *mut JSTracer) {
        self.0.set(true);
    }
}

fn rooter_cases(engine: &JSEngine) {
    let apis = &["JS_NewGlobalObject", "JS_GC"];
    run_case("rooter.trace_on_gc", apis, || {
        with_global(engine, |cx, _| unsafe {
            let mut rooter = CustomAutoRooter::new(TraceCheck(Cell::new(false)));
            let guard = rooter.root(cx.raw_cx());
            JS_GC(cx, GCReason::API);
            // SpiderMonkey runs CustomAutoRooter trace callbacks during an explicit collection.
            // This suite treats that as the contract; a shim that does not call them is non-conforming.
            let traced = guard.0.get();
            assert_eq!(
                traced,
                !cfg!(feature = "js-quickjs"),
                "custom rooter trace callback count differs"
            );
            Observation::new(vec![
                ("gc_trace_calls", Value::Number(u32::from(traced) as f64)),
                (
                    "trace_state",
                    Value::String(if traced { "called" } else { "not_called" }.to_owned()),
                ),
            ])
        })
    });
}
