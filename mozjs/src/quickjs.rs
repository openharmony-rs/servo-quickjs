/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS-specific API for Servo's `js-quickjs` integration. These functions do not exist in
//! SpiderMonkey's mozjs; Servo calls them only when built with `js-quickjs`.

use crate::jsapi::JSObject;

pub use servo_quickjs as sys;

/// Adds a reference to `obj` (a counted edge, e.g. Servo's `Dom<T>`).
///
/// # Safety
/// `obj` must be a live object of the current thread's runtime.
#[inline]
pub unsafe fn retain_object(obj: *mut JSObject) {
    crate::engine::retain_object(obj)
}

/// Drops a reference added by `retain_object`.
///
/// # Safety
/// The caller must own a reference to `obj`.
#[inline]
pub unsafe fn release_object(obj: *mut JSObject) {
    crate::engine::release_object(obj)
}

/// Whether the current thread's runtime is alive. Destroying it frees every object, after which
/// `release_object` does nothing and DOM objects must not be touched.
#[inline]
pub fn runtime_alive() -> bool {
    crate::engine::try_current().is_some()
}

/// Releases every value the engine handed to Rust during the current task. Call between tasks,
/// when no raw JavaScript values are held on the stack.
pub fn drain_autorelease_pool() {
    crate::engine::drain_current_pool()
}

/// The autorelease pool statistics of this thread's runtime as JSON (built with
/// `QJS_POOL_STATS=1`, see `engine/pool_stats.rs`).
#[cfg(qjs_pool_stats)]
pub fn pool_stats_json() -> String {
    crate::engine::pool_stats::json(false)
}

/// The number of values currently kept alive by the autorelease pool.
pub fn autorelease_pool_len() -> usize {
    crate::engine::current().pool.len()
}

/// The number of realms of the current thread's runtime that have not been freed yet.
#[doc(hidden)]
pub fn realm_count() -> usize {
    crate::engine::all_realms().len()
}
