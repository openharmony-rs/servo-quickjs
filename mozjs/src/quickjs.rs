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

/// Adds `count` references to `obj` at once.
///
/// # Safety
/// `obj` must be a live object of the current thread's runtime.
#[inline]
pub unsafe fn retain_object_count(obj: *mut JSObject, count: u32) {
    crate::engine::retain_object_count(obj, count)
}

/// Drops a reference added by `retain_object`.
///
/// # Safety
/// The caller must own a reference to `obj`.
#[inline]
pub unsafe fn release_object(obj: *mut JSObject) {
    crate::engine::release_object(obj)
}

/// Drops `count` references from a `JSDomStub`.
///
/// # Safety
/// `obj` must be a live `JSDomStub` of the current thread's runtime, and the caller must own
/// `count` references to it.
#[inline]
pub unsafe fn release_dom_stub_count(obj: *mut JSObject, count: u32) {
    crate::engine::release_dom_stub_count(obj, count)
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

/// Releases every value the pool holds, then runs a full GC as one operation.
pub fn drain_pool_and_gc() {
    crate::engine::drain_pool_and_gc()
}

/// Appends one row with the current thread's runtime memory usage to the CSV selected by
/// `SERVO_MEMORY_STATS_PATH`. Does nothing when the environment variable is not set (the CSV is
/// opt-in) or when there is no live runtime. See `engine::append_memory_stats` for the columns
/// and the `t_ms` definition; `JS_DestroyContext` also calls it, before and after it drains the
/// pool.
pub fn append_memory_stats() {
    if let Some(cx) = crate::engine::try_current() {
        crate::engine::append_memory_stats(cx);
    }
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

/// Bytes occupied by the values kept alive by the autorelease pool:
/// `autorelease_pool_len() * size_of::<JSValue>()` (16 bytes on 64-bit).
pub fn autorelease_pool_size() -> usize {
    autorelease_pool_len() * std::mem::size_of::<sys::JSValue>()
}

/// The number of realms of the current thread's runtime that have not been freed yet.
#[doc(hidden)]
pub fn realm_count() -> usize {
    crate::engine::all_realms().len()
}

/// The number of live DOM stubs and the bytes they occupy (each counted as `sizeof(JSDomStub)`;
/// allocation headers are not included), or `(0, 0)` when the current thread has no live
/// runtime. A stub is a lazy-binding placeholder before it is materialized, so this complements
/// `GetGCHeapUsage` for measuring the lazy-binding overhead.
#[doc(hidden)]
pub fn dom_stub_usage() -> (usize, usize) {
    match crate::engine::try_current() {
        // SAFETY: `cx` is the live runtime of this thread.
        Some(cx) => unsafe { crate::engine::dom_stub_usage(cx) },
        None => (0, 0),
    }
}

/// The number of live JSObjects and the bytes they occupy (each counted as `sizeof(JSObject)`;
/// property storage, shapes and strings are not included), or `(0, 0)` when the current thread
/// has no live runtime. JSDomStubs are not JSObjects; use `dom_stub_usage` for those.
#[doc(hidden)]
pub fn obj_usage() -> (usize, usize) {
    match crate::engine::try_current() {
        // SAFETY: `cx` is the live runtime of this thread.
        Some(cx) => unsafe { crate::engine::obj_usage(cx) },
        None => (0, 0),
    }
}

/// The live JSObject and DOM stub counts and byte sizes, in the order
/// `(obj_count, obj_size, dom_stub_count, dom_stub_size)`, or all zeroes when the current
/// thread has no live runtime.
///
/// Every value comes from one `JS_ComputeMemoryUsage` walk, so calling this once is cheaper
/// than `obj_usage()` plus `dom_stub_usage()`, each of which walks the whole GC object list.
/// Sizes follow the same rules as those functions: one `sizeof(JSObject)` per object and one
/// `sizeof(JSDomStub)` per stub, with no allocation headers, property storage, shapes or
/// strings.
#[doc(hidden)]
pub fn memory_usage() -> (usize, usize, usize, usize) {
    match crate::engine::try_current() {
        // SAFETY: `cx` is the live runtime of this thread.
        Some(cx) => {
            let usage = unsafe { crate::engine::memory_usage(cx) };
            (
                usage.obj_count as usize,
                usage.obj_size as usize,
                usage.dom_stub_count as usize,
                usage.dom_stub_size as usize,
            )
        }
        None => (0, 0, 0, 0),
    }
}
