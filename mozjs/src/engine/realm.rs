/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Realms (one QuickJS context each) and globals. A `JS::Compartment*` is the same pointer as the
//! `JS::Realm*`: the shim has one compartment per realm and no wrappers.
//!
//! The global object is an instance of its SpiderMonkey class (patch Q1), so its reserved slots,
//! trace hook and finalizer work as for other objects of that class; resolve and enumerate hooks
//! run from exotic methods (`proxy.rs`). The global `this` is the WindowProxy (Q2,
//! `SetWindowProxy`).
//!
//! A realm lives as long as its context: the context owns the `RealmData`, which its finalizer
//! frees (patch Q15). Everything that caches a realm pointer holds a counted reference to the
//! context (`hold_realm`): the global, objects of SpiderMonkey classes, native functions and
//! proxies created in the realm, and the entries of the realm stack. The context in turn holds its
//! global and WindowProxy. Objects mark their reference from `gc_mark`, so the cycle collector
//! frees a realm together with its global once nothing else refers to either.

use std::collections::HashMap;
use std::ffi::CStr;
use std::ptr;

use servo_quickjs as q;

use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;

thread_local! {
    /// Global object -> its realm, until the realm's context is freed.
    static GLOBALS: std::cell::RefCell<HashMap<usize, *mut RealmData>> = Default::default();
}

/// Adds a counted reference to the context of `realm` (if not null), keeping `realm` alive.
#[inline]
pub(crate) unsafe fn hold_realm(realm: *mut RealmData) -> *mut RealmData {
    if !realm.is_null() {
        q::JS_DupContext((*realm).qctx);
        super::verify::count_context((*realm).qctx, 1);
    }
    realm
}

/// Drops a reference added by `hold_realm`, which may free the realm.
#[inline]
pub(crate) unsafe fn release_realm(realm: *mut RealmData) {
    if !realm.is_null() {
        let qctx = (*realm).qctx;
        super::verify::count_context(qctx, -1);
        q::JS_FreeContext(qctx);
    }
}

/// Marks a reference added by `hold_realm` from a `gc_mark` hook.
#[inline]
pub(crate) unsafe fn mark_realm(
    rt: *mut q::JSRuntime,
    realm: *mut RealmData,
    mark: q::JS_MarkFunc,
) {
    if !realm.is_null() {
        super::verify::mark_context((*realm).qctx, mark);
        q::JS_MarkContextRef(rt, (*realm).qctx, mark);
    }
}

/// Patch Q15's context finalizer: frees the realm of a context being freed. Every reference to
/// the context has been dropped, so nothing caches the realm any more.
pub(crate) unsafe extern "C" fn context_finalized(qctx: *mut q::JSContext) {
    let realm = q::JS_GetContextOpaque(qctx) as *mut RealmData;
    if realm.is_null() {
        return;
    }
    let global = (*realm).global as usize;
    let _ = GLOBALS.try_with(|g| {
        let mut g = g.borrow_mut();
        if g.get(&global) == Some(&realm) {
            g.remove(&global);
        }
    });
    let realm = Box::from_raw(realm);
    if !realm.principals.is_null() {
        super::JS_DropPrincipals(ptr::null_mut(), realm.principals);
    }
}

pub(crate) fn realm_of_global(global: *mut root::JSObject) -> *mut RealmData {
    GLOBALS.with(|g| {
        g.borrow()
            .get(&(global as usize))
            .copied()
            .unwrap_or(ptr::null_mut())
    })
}

/// The realm of `obj`: exact for globals, otherwise derived from the function or prototype chain
/// (QuickJS objects do not record their realm), falling back to the current realm.
pub(crate) fn realm_of_object(cx: &Cx, obj: *mut root::JSObject) -> *mut RealmData {
    let realm = realm_of_global(obj);
    if !realm.is_null() {
        return realm;
    }
    let realm = unsafe { realm_of_plain_object(cx, obj) };
    if realm.is_null() {
        cx.current_realm()
    } else {
        realm
    }
}

pub(crate) fn realm_of_qctx(qctx: *mut q::JSContext) -> *mut RealmData {
    unsafe { q::JS_GetContextOpaque(qctx) as *mut RealmData }
}

/// QuickJS: functions and proxies know their realm (patch Q13); other objects get the realm of the
/// first prototype whose realm is known, or whose chain ends at a realm's `Object.prototype`.
unsafe fn realm_of_plain_object(cx: &Cx, obj: *mut root::JSObject) -> *mut RealmData {
    let qctx = q::JS_GetObjectRealm(obj_val(obj));
    if !qctx.is_null() {
        let realm = realm_of_qctx(qctx);
        if !realm.is_null() {
            return realm;
        }
    }
    let mut current = obj;
    loop {
        let proto = q::JS_GetStaticPrototype(cx.base_ctx, obj_val(current));
        if proto.tag() != q::JS_TAG_OBJECT {
            break;
        }
        let proto = val_obj(proto);
        q::JS_FreeValueRT(cx.rt, obj_val(proto));
        let realm = super::class::known_object_realm(proto);
        if !realm.is_null() {
            return realm;
        }
        current = proto;
    }
    if current == obj {
        return ptr::null_mut();
    }
    all_realms()
        .into_iter()
        .find(|realm| {
            let object_proto = q::JS_GetClassProto((**realm).qctx, 1);
            q::JS_FreeValueRT(cx.rt, object_proto);
            object_proto.tag() == q::JS_TAG_OBJECT && val_obj(object_proto) == current
        })
        .unwrap_or(ptr::null_mut())
}

pub(crate) fn all_realms() -> Vec<*mut RealmData> {
    GLOBALS.with(|g| g.borrow().values().copied().collect())
}

/// Calls `f` for every realm until it returns false. The realms are held meanwhile, as `f` may
/// run a collection.
pub(crate) unsafe fn for_each_realm(mut f: impl FnMut(*mut RealmData) -> bool) {
    let realms = all_realms();
    for realm in &realms {
        hold_realm(*realm);
    }
    for realm in &realms {
        if !f(*realm) {
            break;
        }
    }
    for realm in realms {
        release_realm(realm);
    }
}

/// Frees the realms whose contexts outlived their runtime (only with `dump-leaks`).
pub(crate) unsafe fn forget_leaked_realms() {
    let realms: Vec<_> = GLOBALS.with(|g| g.borrow_mut().drain().map(|(_, realm)| realm).collect());
    for realm in realms {
        let realm = Box::from_raw(realm);
        if !realm.principals.is_null() {
            super::JS_DropPrincipals(ptr::null_mut(), realm.principals);
        }
    }
}

pub(crate) unsafe fn JS_NewRealmOptions() -> *mut root::JS::RealmOptions {
    Box::into_raw(Box::new(std::mem::zeroed()))
}

pub(crate) unsafe fn DeleteRealmOptions(options: *mut root::JS::RealmOptions) {
    drop(Box::from_raw(options));
}

/// The number of live realms of this thread.
pub(crate) fn realm_count() -> usize {
    GLOBALS.with(|g| g.borrow().len())
}

pub(crate) unsafe fn JS_NewGlobalObject(
    cx: *mut root::JSContext,
    clasp: *const root::JSClass,
    principals: *mut root::JSPrincipals,
    hookOption: root::JS::OnNewGlobalHookOption,
    options: *const root::JS::RealmOptions,
) -> *mut root::JSObject {
    let _ = hookOption;
    let cx = Cx::from_raw(cx);
    let qctx = q::JS_NewContext(cx.rt);
    if qctx.is_null() {
        return ptr::null_mut();
    }
    super::intl::install_intl(qctx);
    let class_id = super::class::class_id(cx.rt, clasp);
    assert_eq!(
        q::JS_SetGlobalObjectClass(qctx, class_id),
        0,
        "global classes cannot be callable"
    );
    let global_val = q::JS_GetGlobalObject(qctx);
    let global = val_obj(global_val);
    // The context keeps its global alive.
    q::JS_FreeValue(qctx, global_val);
    let (creation_options, behaviors) = if options.is_null() {
        (std::mem::zeroed(), std::mem::zeroed())
    } else {
        (
            ptr::read(&(*options).creationOptions_),
            ptr::read(&(*options).behaviors_),
        )
    };
    if !creation_options.sharedMemoryAndAtomics_ {
        delete_global(qctx, global, c"Atomics");
        delete_global(qctx, global, c"SharedArrayBuffer");
    } else if !creation_options.defineSharedArrayBufferConstructor_ {
        delete_global(qctx, global, c"SharedArrayBuffer");
    }
    if !principals.is_null() {
        super::JS_HoldPrincipals(principals);
    }
    let realm = Box::into_raw(Box::new(RealmData {
        qctx,
        global,
        private: ptr::null_mut(),
        principals,
        creation_options,
        behaviors,
        window_proxy: ptr::null_mut(),
    }));
    q::JS_SetContextOpaque(qctx, realm as *mut _);
    GLOBALS.with(|g| g.borrow_mut().insert(global as usize, realm));
    // The global's data holds the realm instead of the creation reference: global and context
    // keep each other alive until the cycle collector frees both.
    let data = super::class::ObjectData::new(clasp, realm);
    // Before any script of the realm runs.
    data.slots
        .last()
        .unwrap()
        .set(super::intrinsics::new_key_object(qctx));
    q::JS_SetOpaque(obj_val(global), Box::into_raw(data) as *mut _);
    q::JS_FreeContext(qctx);
    cx.retain(JSVal::from_qjs(obj_val(global)));
    global
}

/// Removes a standard global that the realm's creation options disable.
unsafe fn delete_global(qctx: *mut q::JSContext, global: *mut root::JSObject, name: &CStr) {
    let atom = q::JS_NewAtom(qctx, name.as_ptr());
    q::JS_DeleteProperty(qctx, obj_val(global), atom, 0);
    q::JS_FreeAtom(qctx, atom);
}

pub(crate) unsafe fn JS_FireOnNewGlobalObject(
    cx: *mut root::JSContext,
    global: root::JS::HandleObject,
) {
    let _ = (cx, global);
}

pub(crate) unsafe fn EnterRealm(
    cx: *mut root::JSContext,
    target: *mut root::JSObject,
) -> *mut root::JS::Realm {
    let cx = Cx::from_raw(cx);
    let old = cx.current_realm();
    let realm = super::class::object_realm(cx, target);
    assert!(
        !realm.is_null(),
        "EnterRealm on an object without a known realm"
    );
    cx.enter_realm(realm);
    old as *mut root::JS::Realm
}

pub(crate) unsafe fn LeaveRealm(cx: *mut root::JSContext, oldRealm: *mut root::JS::Realm) {
    let cx = Cx::from_raw(cx);
    cx.leave_realm();
    debug_assert_eq!(cx.current_realm() as *mut root::JS::Realm, oldRealm);
}

pub(crate) unsafe fn GetCurrentRealmOrNull(cx: *mut root::JSContext) -> *mut root::JS::Realm {
    Cx::from_raw(cx).current_realm() as *mut root::JS::Realm
}

pub(crate) unsafe fn CurrentGlobalOrNull(cx: *mut root::JSContext) -> *mut root::JSObject {
    let realm = Cx::from_raw(cx).current_realm();
    if realm.is_null() {
        ptr::null_mut()
    } else {
        (*realm).global
    }
}

pub(crate) unsafe fn GetRealmGlobalOrNull(realm: *mut root::JS::Realm) -> *mut root::JSObject {
    if realm.is_null() {
        ptr::null_mut()
    } else {
        (*(realm as *mut RealmData)).global
    }
}

pub(crate) unsafe fn GetObjectRealmOrNull(obj: *mut root::JSObject) -> *mut root::JS::Realm {
    super::class::object_realm(super::current(), obj) as *mut root::JS::Realm
}

pub(crate) unsafe fn GetNonCCWObjectGlobal(obj: *mut root::JSObject) -> *mut root::JSObject {
    let realm = super::class::object_realm(super::current(), obj);
    if realm.is_null() {
        ptr::null_mut()
    } else {
        (*realm).global
    }
}

pub(crate) unsafe fn GetRealmPrivate(realm: *mut root::JS::Realm) -> *mut ::std::os::raw::c_void {
    (*(realm as *mut RealmData)).private
}

pub(crate) unsafe fn SetRealmPrivate(
    realm: *mut root::JS::Realm,
    data: *mut ::std::os::raw::c_void,
) {
    (*(realm as *mut RealmData)).private = data;
}

pub(crate) unsafe fn JS_GlobalObjectTraceHook(
    trc: *mut root::JSTracer,
    global: *mut root::JSObject,
) {
    let realm = realm_of_global(global);
    if realm.is_null() {
        return;
    }
    if let Some(trace) = (*realm).creation_options.traceGlobal_ {
        trace(trc, global);
    }
}
