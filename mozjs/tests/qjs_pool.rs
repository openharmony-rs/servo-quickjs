/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The autorelease pool: what a drain frees.

use std::cell::{Cell, RefCell};
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::jsapi::{GCContext, JSClass, JSClassOps, JSObject, OnNewGlobalHookOption};
use mozjs::quickjs::{autorelease_pool_len, drain_autorelease_pool, release_object, retain_object};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{JS_NewGlobalObject, JS_NewObject};
use mozjs::rust::{JSEngine, JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

fn with_global(f: impl FnOnce(&mut JSContext)) {
    let mut runtime = Runtime::new(ENGINE.clone());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        drain_autorelease_pool();
        f(&mut realm);
    }
}

thread_local! {
    /// Objects `Owner`s hold counted references to; each owner's finalizer releases one.
    static OWNED: RefCell<Vec<*mut JSObject>> = const { RefCell::new(Vec::new()) };
    static OWNED_FREED: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn finalize_owner(_: *mut GCContext, _: *mut JSObject) {
    if let Some(owned) = OWNED.with(|o| o.borrow_mut().pop()) {
        release_object(owned);
    }
}

unsafe extern "C" fn finalize_owned(_: *mut GCContext, _: *mut JSObject) {
    OWNED_FREED.with(|f| f.set(f.get() + 1));
}

const fn ops(finalize: unsafe extern "C" fn(*mut GCContext, *mut JSObject)) -> JSClassOps {
    JSClassOps {
        addProperty: None,
        delProperty: None,
        enumerate: None,
        newEnumerate: None,
        resolve: None,
        mayResolve: None,
        finalize: Some(finalize),
        call: None,
        construct: None,
        trace: None,
    }
}

static OWNER_OPS: JSClassOps = ops(finalize_owner);
static OWNED_OPS: JSClassOps = ops(finalize_owned);

static OWNER_CLASS: JSClass = JSClass {
    name: c"Owner".as_ptr(),
    flags: 0,
    cOps: &OWNER_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static OWNED_CLASS: JSClass = JSClass {
    name: c"Owned".as_ptr(),
    flags: 0,
    cOps: &OWNED_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

/// The drain frees the owned objects' entries first, then the owners, whose finalizers drop the
/// owned objects' last references, which go to the pool in the middle of the drain. Optimized
/// builds lost those entries when the drain went through `&mut Cx`; CI runs this test in an
/// optimized ASan build too.
#[test]
fn drains_free_what_finalizers_release() {
    with_global(|cx| unsafe {
        const N: usize = 1000;
        for _ in 0..N {
            assert!(!JS_NewObject(cx, &OWNER_CLASS).is_null());
        }
        for _ in 0..N {
            let owned = JS_NewObject(cx, &OWNED_CLASS);
            assert!(!owned.is_null());
            retain_object(owned);
            OWNED.with(|o| o.borrow_mut().push(owned));
        }
        drain_autorelease_pool();
        assert_eq!(OWNED_FREED.with(Cell::get), N);
        assert_eq!(autorelease_pool_len(), 0);
    });
}
