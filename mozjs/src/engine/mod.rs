/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS implementations of SpiderMonkey API functions.
//!
//! Every `pub(crate) unsafe fn NAME` in this module tree replaces the stub of the SpiderMonkey
//! function `NAME` (see `tools/stub_bindings.py`), so signatures must match the bindgen ones
//! (`tools/sig.py NAME` prints them). Design: `docs/design.md`.

#![allow(non_snake_case)]
// A `&mut` handed out from a shared reference (`&Cx`) aliases: see HACKING.md, "Re-entrancy".
#![deny(clippy::mut_from_ref)]

use std::cell::Cell;
use std::ffi::CString;
use std::ptr;

use servo_quickjs as q;

use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;
use reentrant::Stack;

mod atom;
mod buffer;
mod class;
mod clone;
mod error;
mod eval;
mod exception;
mod intl;
mod intrinsics;
mod misc;
mod module;
mod object;
#[cfg(qjs_pool_stats)]
pub(crate) mod pool_stats;
mod promise;
mod property;
mod proxy;
mod realm;
mod reentrant;
mod runtime;
mod string;
pub(crate) mod verify;

pub(crate) use atom::*;
pub(crate) use buffer::*;
pub(crate) use class::*;
pub(crate) use clone::*;
pub(crate) use error::*;
pub(crate) use eval::*;
pub(crate) use exception::*;
pub(crate) use misc::*;
pub(crate) use module::*;
pub(crate) use object::*;
pub(crate) use promise::*;
pub(crate) use property::*;
pub(crate) use proxy::*;
pub(crate) use realm::*;
pub(crate) use runtime::*;
pub(crate) use string::*;

/// What a SpiderMonkey `JSContext*` points to: one per QuickJS runtime (one per thread).
///
/// Only ever reached through shared references: natives, hooks and finalizers that run during a
/// call into QuickJS reach it again through `current()`, so the state they change is in cells
/// (see `reentrant.rs`).
pub(crate) struct Cx {
    pub rt: *mut q::JSRuntime,
    /// A context without a global of ours, for operations outside any realm.
    pub base_ctx: *mut q::JSContext,
    /// Entered realms, innermost last; each holds a reference (see `enter_realm`).
    pub realm_stack: Stack<*mut RealmData>,
    /// Values (and atoms) handed to Rust during the current task; see `retain`.
    pub pool: Stack<q::JSValue>,
    pub atom_pool: Stack<q::JSAtom>,
    pub warning_reporter: Cell<root::JS::WarningReporter>,
    /// Job queue, promise rejection tracking, interrupts and security callbacks.
    pub hooks: promise::RuntimeHooks,
    /// The key object of `base_ctx` (counted), for intrinsics outside any realm.
    pub base_key_object: Cell<q::JSValue>,
}

/// What a SpiderMonkey `JS::Realm*` (and `JS::Compartment*`) points to. Owned by its QuickJS
/// context, and freed with it (see `realm.rs`).
pub(crate) struct RealmData {
    pub qctx: *mut q::JSContext,
    pub global: *mut root::JSObject,
    pub private: *mut std::os::raw::c_void,
    pub principals: *mut root::JSPrincipals,
    /// Copied from the `RealmOptions`; `traceGlobal_` is run by `JS_GlobalObjectTraceHook`.
    pub creation_options: root::JS::RealmCreationOptions,
    pub behaviors: root::JS::RealmBehaviors,
    /// The WindowProxy of a Window global (`SetWindowProxy`), kept alive as its global `this`.
    pub window_proxy: *mut root::JSObject,
}

thread_local!(static CURRENT: Cell<*const Cx> = const { Cell::new(ptr::null()) });

impl Cx {
    pub fn from_raw<'a>(cx: *mut root::JSContext) -> &'a Cx {
        assert!(!cx.is_null());
        unsafe { &*(cx as *const Cx) }
    }

    pub fn as_raw(&self) -> *mut root::JSContext {
        self as *const Cx as *mut root::JSContext
    }

    /// The QuickJS context of the current realm, or the base context outside any realm.
    pub fn qctx(&self) -> *mut q::JSContext {
        match self.realm_stack.last() {
            Some(realm) => unsafe { (*realm).qctx },
            None => self.base_ctx,
        }
    }

    pub fn current_realm(&self) -> *mut RealmData {
        self.realm_stack.last().unwrap_or(ptr::null_mut())
    }

    /// Makes `realm` current, and keeps it alive, until the matching `leave_realm`.
    pub unsafe fn enter_realm(&self, realm: *mut RealmData) {
        self.realm_stack.push(hold_realm(realm));
    }

    pub unsafe fn leave_realm(&self) {
        let realm = self
            .realm_stack
            .pop()
            .expect("leaving a realm that was not entered");
        release_realm(realm);
    }

    /// Keeps `v` alive until the pool is drained (end of task), and returns it. Ropes are
    /// flattened, so values Rust sees never carry `JS_TAG_STRING_ROPE`.
    #[cfg_attr(qjs_pool_stats, track_caller)]
    pub fn retain(&self, v: JSVal) -> JSVal {
        self.retain_as(v, Pooled::Borrowed)
    }

    #[cfg_attr(qjs_pool_stats, track_caller)]
    fn retain_as(&self, v: JSVal, pooled: Pooled) -> JSVal {
        let qv = v.to_qjs();
        if !qv.has_ref_count() {
            return v;
        }
        let qv = unsafe { q::JS_DupValueRT(self.rt, qv) };
        self.adopt_as(qv, pooled)
    }

    /// Like `retain`, but takes over a reference the caller owns (e.g. a QuickJS API result).
    #[cfg_attr(qjs_pool_stats, track_caller)]
    pub fn adopt(&self, qv: q::JSValue) -> JSVal {
        self.adopt_as(qv, Pooled::Result)
    }

    #[cfg_attr(qjs_pool_stats, track_caller)]
    fn adopt_as(&self, qv: q::JSValue, pooled: Pooled) -> JSVal {
        if !qv.has_ref_count() {
            return JSVal::from_qjs(qv);
        }
        let qv = if qv.tag() == q::JS_TAG_STRING_ROPE {
            let flat = unsafe { q::JS_ToString(self.qctx(), qv) };
            unsafe { q::JS_FreeValueRT(self.rt, qv) };
            flat
        } else {
            qv
        };
        self.push_pool(qv, pooled);
        JSVal::from_qjs(qv)
    }

    /// Takes over a reference to `qv` until the pool is drained.
    #[cfg_attr(qjs_pool_stats, track_caller)]
    #[cfg_attr(not(qjs_pool_stats), allow(unused_variables))]
    fn push_pool(&self, qv: q::JSValue, pooled: Pooled) {
        self.pool.push(qv);
        verify::pooled(qv, 1);
        #[cfg(qjs_pool_stats)]
        pool_stats::pushed(qv, pooled, std::panic::Location::caller());
    }

    pub fn adopt_atom(&self, atom: q::JSAtom) -> q::JSAtom {
        self.atom_pool.push(atom);
        atom
    }

    /// Releases the values retained since `mark` (see `pool_mark`). The frees can run finalizers
    /// that release more values into the pool (`release_object`), which are drained too.
    pub fn drain_pool(&self, mark: (usize, usize)) {
        while self.pool.len() > mark.0 {
            let v = self.pool.pop().unwrap();
            #[cfg(qjs_pool_stats)]
            pool_stats::popped(v);
            verify::pooled(v, -1);
            unsafe { q::JS_FreeValueRT(self.rt, v) };
        }
        while self.atom_pool.len() > mark.1 {
            let a = self.atom_pool.pop().unwrap();
            unsafe { q::JS_FreeAtomRT(self.rt, a) };
        }
    }

    pub fn pool_mark(&self) -> (usize, usize) {
        (self.pool.len(), self.atom_pool.len())
    }
}

/// The runtime of the current thread.
#[inline]
pub(crate) fn current<'a>() -> &'a Cx {
    let cx = CURRENT.with(|c| c.get());
    assert!(!cx.is_null(), "no QuickJS runtime on this thread");
    unsafe { &*cx }
}

/// The runtime of the current thread, or `None` once it has been destroyed. Like SpiderMonkey's
/// shutdown GC, destroying a runtime frees every object, so later releases must do nothing.
#[inline]
pub(crate) fn try_current<'a>() -> Option<&'a Cx> {
    let cx = CURRENT.with(|c| c.get());
    if cx.is_null() {
        None
    } else {
        Some(unsafe { &*cx })
    }
}

pub(crate) fn set_current(cx: *const Cx) {
    CURRENT.with(|c| c.set(cx));
}

/// The QuickJS object pointer as a value (no reference count change).
#[inline]
pub(crate) fn obj_val(obj: *mut root::JSObject) -> q::JSValue {
    q::JSValue::mkptr(q::JS_TAG_OBJECT, obj as *mut _)
}

#[inline]
pub(crate) fn val_obj(v: q::JSValue) -> *mut root::JSObject {
    debug_assert_eq!(v.tag(), q::JS_TAG_OBJECT);
    v.ptr() as *mut root::JSObject
}

pub(crate) fn cstring_lossy(s: *const std::os::raw::c_char) -> CString {
    if s.is_null() {
        CString::default()
    } else {
        unsafe { std::ffi::CStr::from_ptr(s) }.to_owned()
    }
}

/// Retains a value written to a rooted location (`MutableHandle::set`, `RootedGuard::set`): like
/// SpiderMonkey's roots, the location keeps it alive, here until the pool is drained.
pub(crate) unsafe fn retain_stored(kind: root::JS::RootKind, data: *const std::ffi::c_void) {
    if let Some(cx) = try_current() {
        retain_rooted(cx.as_raw(), kind, data);
    }
}

/// Retains the initial value of a `JS::Rooted<T>` (see `JS::Rooted::add_to_root_stack`).
pub(crate) unsafe fn retain_rooted(
    cx: *mut root::JSContext,
    kind: root::JS::RootKind,
    data: *const std::ffi::c_void,
) {
    use root::JS::RootKind;
    let cx = Cx::from_raw(cx);
    let ptr_tag = match kind {
        RootKind::Value => {
            cx.retain_as(*(data as *const JSVal), Pooled::Root);
            return;
        }
        RootKind::Id => {
            let id = *(data as *const root::jsid);
            if id.is_string() || id.is_symbol() {
                cx.adopt_atom(q::JS_DupAtom(cx.base_ctx, id.to_atom()));
            }
            return;
        }
        RootKind::Object => q::JS_TAG_OBJECT,
        RootKind::String => q::JS_TAG_STRING,
        RootKind::Symbol => q::JS_TAG_SYMBOL,
        RootKind::BigInt => q::JS_TAG_BIG_INT,
        RootKind::Script => q::JS_TAG_FUNCTION_BYTECODE,
        _ => return,
    };
    let p = *(data as *const *mut std::ffi::c_void);
    if !p.is_null() {
        cx.retain_as(JSVal::from_qjs(q::JSValue::mkptr(ptr_tag, p)), Pooled::Root);
    }
}

/// See `crate::quickjs::retain_object`.
#[inline]
pub(crate) unsafe fn retain_object(obj: *mut root::JSObject) {
    q::JS_DupValueRT(current().rt, obj_val(obj));
    verify::count(obj_val(obj), 1);
}

/// See `crate::quickjs::release_object`.
#[inline]
pub(crate) unsafe fn release_object(obj: *mut root::JSObject) {
    if let Some(cx) = try_current() {
        let v = obj_val(obj);
        verify::count(v, -1);
        // The last reference is dropped when the pool drains, after the current native call or
        // task, so finalizers don't run inside the Rust code that released it: SpiderMonkey
        // finalizes in GC, and Servo relies on that (e.g. replacing a `Dom<T>` under a `RefCell`
        // borrow that the target's finalizer takes too). In a cycle collection, the object may
        // belong to the cycle being freed and must go with it.
        if q::JS_GetRefCount(v) == 1 && q::JS_IsCollectingCycles(cx.rt) == 0 {
            cx.push_pool(v, Pooled::Deferred);
        } else {
            q::JS_FreeValueRT(cx.rt, v);
        }
    }
}

pub(crate) fn drain_current_pool() {
    if let Some(cx) = try_current() {
        #[cfg(qjs_pool_stats)]
        pool_stats::task_drained(cx.pool.len());
        cx.drain_pool((0, 0));
    }
}

/// How a value entered the autorelease pool.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Pooled {
    /// A reference an API function handed to Rust (`Cx::adopt`).
    Result,
    /// An extra count on a value that already had an owner (`Cx::retain`).
    Borrowed,
    /// The value of a `Rooted` or of a handle write.
    Root,
    /// A last release deferred to the drain (`release_object`).
    Deferred,
}
