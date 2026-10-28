/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Tracing verifier, enabled with `QJS_VERIFY_TRACING=1`.
//!
//! QuickJS's cycle collector subtracts every mark from the marked object's reference count, so a
//! Rust trace hook that marks more often than Rust holds counted references (`Heap<T>`, Servo's
//! `Dom<T>`) makes live objects look like garbage, and they are freed while still in use. The
//! verifier tracks the counted references Rust holds per object and aborts when one collection
//! pass marks an object more often from Rust trace hooks. Walks of the heap outside those passes
//! (`QJS_GC_STATS=paths:<class>`) change no counts and are not checked. Shared `Rc`s, which count
//! their owners by visits, abort when a pass traces one handle twice or sees the strong count
//! change (`trace_shared_rc`).
//!
//! It also aborts when an object is freed while the autorelease pool holds it, which means some
//! code released a reference it did not own. Pool references are never marked, so the cycle
//! collector cannot free such an object.
//!
//! References to realms (QuickJS contexts) held by the shim's objects are checked the same way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

use servo_quickjs as q;

use crate::sys::jsapi as root;

#[derive(Default)]
struct Verifier {
    counts: HashMap<usize, i64>,
    marks: HashMap<usize, i64>,
    pool: HashMap<usize, i64>,
    /// The mark function of the current pass; a different one starts a new pass.
    pass: usize,
}

thread_local! {
    static VERIFIER: RefCell<Verifier> = RefCell::default();
}

/// 0: not read yet, 1: off, 2: on. Process-wide, as it is checked on every reference count change.
static ENABLED: AtomicU8 = AtomicU8::new(0);

#[inline]
pub(crate) fn enabled() -> bool {
    match ENABLED.load(Ordering::Relaxed) {
        0 => {
            let enabled = std::env::var_os("QJS_VERIFY_TRACING").is_some_and(|v| v != "0");
            ENABLED.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
            enabled
        }
        state => state == 2,
    }
}

fn gc_pointer(v: q::JSValue) -> Option<usize> {
    match v.tag() {
        q::JS_TAG_OBJECT | q::JS_TAG_FUNCTION_BYTECODE | q::JS_TAG_MODULE => Some(v.ptr() as usize),
        _ => None,
    }
}

/// Records that Rust gained (`delta > 0`) or dropped a counted reference to `v`.
#[inline]
pub(crate) fn count(v: q::JSValue, delta: i64) {
    if !enabled() {
        return;
    }
    if let Some(p) = gc_pointer(v) {
        count_pointer(p, delta);
    }
}

/// Records that Rust gained (`delta > 0`) or dropped a reference to the QuickJS context `ctx`.
#[inline]
pub(crate) fn count_context(ctx: *mut q::JSContext, delta: i64) {
    if enabled() {
        count_pointer(ctx as usize, delta);
    }
}

fn count_pointer(p: usize, delta: i64) {
    VERIFIER.with(|verifier| {
        let mut verifier = verifier.borrow_mut();
        let count = verifier.counts.entry(p).or_insert(0);
        *count += delta;
        if *count == 0 {
            verifier.counts.remove(&p);
        }
    });
}

/// Records that the autorelease pool gained (`delta > 0`) or dropped a reference to `v`.
#[inline]
pub(crate) fn pooled(v: q::JSValue, delta: i64) {
    if !enabled() {
        return;
    }
    let Some(p) = gc_pointer(v) else {
        return;
    };
    VERIFIER.with(|verifier| {
        let mut verifier = verifier.borrow_mut();
        let count = verifier.pool.entry(p).or_insert(0);
        *count += delta;
        if *count == 0 {
            verifier.pool.remove(&p);
        }
    });
}

/// Installs the free hook on a new runtime.
pub(crate) unsafe fn install(rt: *mut q::JSRuntime) {
    if enabled() {
        q::JS_SetFreeObjectHook(rt, Some(object_freed));
    }
}

unsafe fn describe(v: q::JSValue) -> String {
    if v.tag() != q::JS_TAG_OBJECT {
        return format!("tag {}", v.tag());
    }
    let id = q::JS_GetClassID(v);
    let clasp = super::class::object_class(v.ptr() as *mut root::JSObject);
    let name = std::ffi::CStr::from_ptr((*clasp).name).to_string_lossy();
    format!("{name}, QuickJS class {id}")
}

unsafe extern "C" fn object_freed(_rt: *mut q::JSRuntime, obj: q::JSValue) {
    let p = obj.ptr() as usize;
    let pooled = VERIFIER.with(|verifier| verifier.borrow().pool.get(&p).copied().unwrap_or(0));
    if pooled > 0 {
        eprintln!(
            "QuickJS tracing verifier: {p:#x} ({}) freed while the autorelease pool holds \
             {pooled} references to it; a reference was released twice\n{}",
            describe(obj),
            std::backtrace::Backtrace::force_capture()
        );
        std::process::abort();
    }
}

/// Whether `rt`'s cycle collector is in a pass that counts marks (patch Q41).
unsafe fn counting(rt: *mut q::JSRuntime) -> bool {
    q::JS_GetGCPass(rt, std::ptr::null_mut()) as u32 != q::JS_GC_PASS_NONE
}

/// Checks a mark of `v` from a Rust trace hook during the pass that uses `mark_func`.
pub(crate) unsafe fn mark(rt: *mut q::JSRuntime, v: q::JSValue, mark_func: q::JS_MarkFunc) {
    if !enabled() || !counting(rt) {
        return;
    }
    if let Some(p) = gc_pointer(v) {
        mark_pointer(p, mark_func, || describe(v));
    }
}

/// Checks a mark of the QuickJS context `ctx` from a `gc_mark` hook of the shim.
pub(crate) unsafe fn mark_context(
    rt: *mut q::JSRuntime,
    ctx: *mut q::JSContext,
    mark_func: q::JS_MarkFunc,
) {
    if enabled() && counting(rt) {
        mark_pointer(ctx as usize, mark_func, || "QuickJS context".to_owned());
    }
}

unsafe fn mark_pointer(p: usize, mark_func: q::JS_MarkFunc, describe: impl FnOnce() -> String) {
    let pass = mark_func.map_or(0, |f| f as usize);
    let violation = VERIFIER.with(|verifier| {
        let mut verifier = verifier.borrow_mut();
        if verifier.pass != pass {
            verifier.pass = pass;
            verifier.marks.clear();
        }
        let marks = verifier.marks.entry(p).or_insert(0);
        *marks += 1;
        let marks = *marks;
        let count = verifier.counts.get(&p).copied().unwrap_or(0);
        (marks > count).then_some((marks, count))
    });
    if let Some((marks, count)) = violation {
        let class = describe();
        eprintln!(
            "QuickJS tracing verifier: {p:#x} ({class}) marked {marks} times from Rust trace hooks \
             in one GC pass, but Rust holds {count} counted references to it\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        std::process::abort();
    }
}

/// Forgets all counts (the runtime is being destroyed).
pub(crate) fn reset() {
    if enabled() {
        VERIFIER.with(|verifier| *verifier.borrow_mut() = Verifier::default());
    }
}
