/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Which mozjs entry points the shim actually forwards, built with `QJS_CALL_LOG=1`.
//!
//! Every forwarding stub in `sys` calls [`enter`] before its body, which records the name once.
//! When a runtime is destroyed the set is appended to `QJS_CALL_LOG_OUT` (default
//! `qjs-call-log-<pid>.jsonl` in the temporary directory), one JSON line per entry point. This
//! answers whether an `implemented` entry is ever reached by a workload such as WPT, which the
//! contract suite cannot show because it only runs its own cases.

#[cfg(qjs_call_log)]
use std::cell::RefCell;
#[cfg(qjs_call_log)]
use std::collections::BTreeSet;
#[cfg(qjs_call_log)]
use std::io::Write as _;
#[cfg(qjs_call_log)]
use std::path::PathBuf;
#[cfg(qjs_call_log)]
use std::sync::Once;
#[cfg(qjs_call_log)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(qjs_call_log)]
thread_local! {
    static CALLED: RefCell<BTreeSet<&'static str>> = const { RefCell::new(BTreeSet::new()) };
}

#[cfg(qjs_call_log)]
static ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(qjs_call_log)]
static INIT: Once = Once::new();

#[cfg(qjs_call_log)]
fn enabled() -> bool {
    INIT.call_once(|| {
        let on = std::env::var("QJS_CALL_LOG").is_ok_and(|v| v != "0" && !v.is_empty());
        ENABLED.store(on, Ordering::Relaxed);
    });
    ENABLED.load(Ordering::Relaxed)
}

/// Records that `name` is about to be forwarded.
#[cfg(qjs_call_log)]
pub(crate) fn enter(name: &'static str) {
    if enabled() {
        CALLED.with(|called| {
            called.borrow_mut().insert(name);
        });
    }
}

/// Records `name` for a call from one `engine` function to another.
///
/// The generated FFI stubs record their own names, but forwarding functions here call each other
/// directly (e.g. `Construct1` calls `Construct`), which skips those stubs. Callers that wrap a
/// sibling entry point must record it explicitly so the trace stays complete.
#[cfg(qjs_call_log)]
pub(crate) fn enter_inner(name: &'static str) {
    enter(name);
}

#[cfg(not(qjs_call_log))]
pub(crate) fn enter_inner(_name: &'static str) {}

#[cfg(qjs_call_log)]
fn out_path() -> PathBuf {
    std::env::var_os("QJS_CALL_LOG_OUT").map_or_else(
        || {
            let mut path = std::env::temp_dir();
            path.push(format!("qjs-call-log-{}.jsonl", std::process::id()));
            path
        },
        PathBuf::from,
    )
}

/// Appends one JSON line per distinct entry point seen on this thread.
#[cfg(qjs_call_log)]
pub(crate) fn flush() {
    if !enabled() {
        return;
    }
    let names = CALLED.with(|called| called.borrow().iter().copied().collect::<Vec<_>>());
    if names.is_empty() {
        return;
    }
    let path = out_path();
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    // One write for the whole batch: contexts in different processes share the file when the
    // caller points `QJS_CALL_LOG_OUT` at one path, and a `writeln!` per name can interleave.
    let mut batch = String::new();
    for name in names {
        batch.push_str("{\"api\":\"");
        batch.push_str(name);
        batch.push_str("\"}\n");
    }
    let _ = file.write_all(batch.as_bytes());
}
