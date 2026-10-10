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

/// Releases, when it is dropped, what the autorelease pool took while it was alive: the values
/// the engine handed to Rust and the last references Rust dropped. Otherwise they stay until the
/// task ends. Servo opens one for each microtask, so that a long chain of microtasks in one task
/// doesn't keep every finished job alive.
pub struct AutoreleasePoolScope {
    mark: (usize, usize),
    cx: *const crate::engine::Cx,
}

impl AutoreleasePoolScope {
    /// # Safety
    /// Nothing obtained while the scope is alive may be used after it is dropped, unless it holds
    /// a reference of its own (`Heap`, `Dom<T>`, `retain_object`). That includes values written
    /// to roots declared before the scope (roots hold no references, the pool keeps their values
    /// alive), and objects obtained before the scope whose last reference Rust drops while it is
    /// alive (`release_object` moves that reference to the pool).
    ///
    /// The scope must be dropped on the runtime it was created on, before the native call or task
    /// that created it ends, and in reverse order of creation with other scopes: those drains
    /// release everything above their own mark, so a scope outliving one of them would release
    /// entries that are not its own.
    ///
    /// # Panics
    /// If the thread has no runtime.
    pub unsafe fn new() -> Self {
        let cx = crate::engine::current();
        Self {
            mark: cx.pool_mark(),
            cx,
        }
    }
}

impl Drop for AutoreleasePoolScope {
    fn drop(&mut self) {
        if let Some(cx) = crate::engine::try_current() {
            debug_assert!(
                std::ptr::eq(&*cx, self.cx),
                "AutoreleasePoolScope dropped on another runtime"
            );
            let len = cx.pool_mark();
            debug_assert!(
                self.mark.0 <= len.0 && self.mark.1 <= len.1,
                "AutoreleasePoolScope outlived a drain below its mark"
            );
            cx.drain_pool(self.mark);
        }
    }
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
