/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! `CustomAutoRooter` lifetimes and tracing.
//!
//! QuickJS keeps Rust roots alive through the autorelease pool, so `CustomAutoRooter` values do
//! not need to be marked for correctness. SpiderMonkey still runs their trace callbacks during a
//! collection, and Servo's `CustomTraceable` implementations rely on that observable contract.

use std::ffi::c_void;

use super::{class::Tracer, Cx};
use crate::sys::jsapi as root;

pub(crate) unsafe fn register_custom_rooter(
    cx: *mut root::JSContext,
    this: *mut c_void,
    trace: super::RooterTrace,
) {
    Cx::from_raw(cx).add_rooter(this as usize, trace);
}

pub(crate) unsafe fn unregister_custom_rooter(cx: *mut root::JSContext, this: *mut c_void) {
    Cx::from_raw(cx).remove_rooter(this as usize);
}

/// Runs the trace callback of every live custom rooter before an explicit collection.
pub(crate) unsafe fn trace_custom_rooters(cx: &Cx) {
    let rooters = cx.rooters();
    if rooters.is_empty() {
        return;
    }
    let mut tracer = Tracer {
        rt: cx.rt,
        mark: None,
    };
    for (this, trace) in rooters {
        trace(
            this as *mut c_void,
            &mut tracer as *mut Tracer as *mut root::JSTracer,
        );
    }
}

pub(crate) unsafe fn run_gc(raw: *mut root::JSContext) {
    let cx = Cx::from_raw(raw);
    trace_custom_rooters(cx);
    servo_quickjs::JS_RunGC(cx.rt);
}
