/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! An `Rc` that several traced objects share holds one reference count per reference however
//! many owners it has; the cycle collector sees through it once every owner is traced, and
//! treats its contents as referenced from outside while one is not (patch Q41). Servo's custom
//! element definitions, shared by the registry and every element, kept their window alive before.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::process::Command;
use std::ptr;
use std::rc::Rc;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::gc::Traceable;
use mozjs::jsapi::{
    self, GCContext, GCReason, Heap, JSClass, JSClassOps, JSObject, JSTracer,
    OnNewGlobalHookOption, JSPROP_ENUMERATE,
};
use mozjs::jsval::{JSVal, ObjectValue, UndefinedValue};
use mozjs::quickjs::drain_autorelease_pool;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    JS_DefineProperty, JS_GetProperty, JS_NewGlobalObject, JS_NewObject, JS_NewPlainObject, JS_GC,
};
use mozjs::rust::{JSEngine, JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

thread_local! {
    /// The Rust data of each `OWNER_CLASS` object: an `Rc` that may be shared with other owners.
    static DATA: RefCell<HashMap<usize, Rc<Heap<JSVal>>>> = RefCell::new(HashMap::new());
    /// The Rust data of each `NESTED_OWNER_CLASS` object.
    static NESTED: RefCell<HashMap<usize, Rc<Nested>>> = RefCell::new(HashMap::new());
    /// The Rust data of each `TWIN_OWNER_CLASS` object: two clones of one `Rc`.
    static TWINS: RefCell<HashMap<usize, Twins>> = RefCell::new(HashMap::new());
    static FINALIZED: Cell<usize> = const { Cell::new(0) };
}

/// Rust data with a counted reference and a shared `Rc` of its own.
struct Nested {
    heap: Heap<JSVal>,
    inner: Rc<Heap<JSVal>>,
}

unsafe impl Traceable for Nested {
    unsafe fn trace(&self, trc: *mut JSTracer) {
        self.heap.trace(trc);
        self.inner.trace(trc);
    }
}

type Twins = (Rc<Heap<JSVal>>, Rc<Heap<JSVal>>);

unsafe extern "C" fn trace(trc: *mut JSTracer, obj: *mut JSObject) {
    DATA.with(|data| {
        if let Some(rc) = data.borrow().get(&(obj as usize)) {
            rc.trace(trc);
        }
    });
}

unsafe extern "C" fn finalize(_: *mut GCContext, obj: *mut JSObject) {
    let rc = DATA.with(|data| data.borrow_mut().remove(&(obj as usize)));
    FINALIZED.with(|f| f.set(f.get() + 1));
    drop(rc);
}

/// The bug `QJS_VERIFY_TRACING` catches: the same `Rc` traced twice.
unsafe extern "C" fn trace_twice(trc: *mut JSTracer, obj: *mut JSObject) {
    DATA.with(|data| {
        if let Some(rc) = data.borrow().get(&(obj as usize)) {
            rc.trace(trc);
            rc.trace(trc);
        }
    });
}

unsafe extern "C" fn trace_twins(trc: *mut JSTracer, obj: *mut JSObject) {
    TWINS.with(|twins| {
        if let Some((first, second)) = twins.borrow().get(&(obj as usize)) {
            first.trace(trc);
            second.trace(trc);
        }
    });
}

unsafe extern "C" fn finalize_twins(_: *mut GCContext, obj: *mut JSObject) {
    let twins = TWINS.with(|twins| twins.borrow_mut().remove(&(obj as usize)));
    FINALIZED.with(|f| f.set(f.get() + 1));
    drop(twins);
}

unsafe extern "C" fn trace_nested(trc: *mut JSTracer, obj: *mut JSObject) {
    NESTED.with(|nested| {
        if let Some(rc) = nested.borrow().get(&(obj as usize)) {
            rc.trace(trc);
        }
    });
}

unsafe extern "C" fn finalize_nested(_: *mut GCContext, obj: *mut JSObject) {
    let rc = NESTED.with(|nested| nested.borrow_mut().remove(&(obj as usize)));
    FINALIZED.with(|f| f.set(f.get() + 1));
    drop(rc);
}

static OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize),
    call: None,
    construct: None,
    trace: Some(trace),
};

static OWNER_CLASS: JSClass = JSClass {
    name: c"Owner".as_ptr(),
    flags: jsapi::JSCLASS_FOREGROUND_FINALIZE,
    cOps: &OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static NESTED_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize_nested),
    call: None,
    construct: None,
    trace: Some(trace_nested),
};

static NESTED_OWNER_CLASS: JSClass = JSClass {
    name: c"NestedOwner".as_ptr(),
    flags: jsapi::JSCLASS_FOREGROUND_FINALIZE,
    cOps: &NESTED_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static TWIN_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize_twins),
    call: None,
    construct: None,
    trace: Some(trace_twins),
};

static TWIN_OWNER_CLASS: JSClass = JSClass {
    name: c"TwinOwner".as_ptr(),
    flags: jsapi::JSCLASS_FOREGROUND_FINALIZE,
    cOps: &TWIN_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

static TRACE_TWICE_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize),
    call: None,
    construct: None,
    trace: Some(trace_twice),
};

static TRACE_TWICE_CLASS: JSClass = JSClass {
    name: c"TraceTwice".as_ptr(),
    flags: jsapi::JSCLASS_FOREGROUND_FINALIZE,
    cOps: &TRACE_TWICE_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

unsafe fn new_global(cx: &mut JSContext) -> *mut JSObject {
    JS_NewGlobalObject(
        cx,
        &SIMPLE_GLOBAL_CLASS,
        ptr::null_mut(),
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*RealmOptions::default(),
    )
}

/// A new `OWNER_CLASS` object whose Rust data is `rc`.
unsafe fn new_owner(cx: &mut JSContext, rc: &Rc<Heap<JSVal>>) -> *mut JSObject {
    let owner = JS_NewObject(cx, &OWNER_CLASS);
    DATA.with(|data| data.borrow_mut().insert(owner as usize, rc.clone()));
    owner
}

unsafe fn define(cx: &mut JSContext, obj: *mut JSObject, name: &std::ffi::CStr, v: *mut JSObject) {
    rooted!(&in(cx) let obj = obj);
    rooted!(&in(cx) let v = ObjectValue(v));
    assert!(JS_DefineProperty(
        cx,
        obj.handle(),
        name.as_ptr(),
        v.handle(),
        JSPROP_ENUMERATE as u32
    ));
}

/// The value of property `name` of the object `v`.
unsafe fn get(cx: &mut JSContext, v: JSVal, name: &std::ffi::CStr) -> JSVal {
    rooted!(&in(cx) let obj = v.to_object());
    rooted!(&in(cx) let mut v = UndefinedValue());
    assert!(JS_GetProperty(
        cx,
        obj.handle(),
        name.as_ptr(),
        v.handle_mut()
    ));
    v.get()
}

/// Two owners share an `Rc` that refers to `target`, and `target` refers to both owners:
/// `owner_a -> Rc -> target -> owner_a`, the same for `owner_b`. Returns the owners and the `Rc`.
unsafe fn shared_cycle(cx: &mut JSContext) -> (*mut JSObject, *mut JSObject, Rc<Heap<JSVal>>) {
    let rc = Rc::new(Heap::default());
    let target = JS_NewPlainObject(cx);
    rc.set(ObjectValue(target));
    let a = new_owner(cx, &rc);
    let b = new_owner(cx, &rc);
    define(cx, target, c"a", a);
    define(cx, target, c"b", b);
    (a, b, rc)
}

/// Owners `a` and `b` share an `Rc<Nested>`; its `heap` refers to `outer_target`, which refers to
/// `a` and `b`; its `inner` is shared with owner `c` and refers to `inner_target`, which refers to
/// `c`. Every reference between them is counted, so all of them are one garbage cycle once
/// nothing else refers to them. Returns `a` and `inner`.
unsafe fn nested_cycle(cx: &mut JSContext) -> (*mut JSObject, Rc<Heap<JSVal>>) {
    let inner = Rc::new(Heap::default());
    let inner_target = JS_NewPlainObject(cx);
    inner.set(ObjectValue(inner_target));
    let c = new_owner(cx, &inner);
    define(cx, inner_target, c"c", c);

    let outer = Rc::new(Nested {
        heap: Heap::default(),
        inner: inner.clone(),
    });
    let outer_target = JS_NewPlainObject(cx);
    outer.heap.set(ObjectValue(outer_target));
    let mut owners = [ptr::null_mut(); 2];
    for owner in &mut owners {
        *owner = JS_NewObject(cx, &NESTED_OWNER_CLASS);
        NESTED.with(|nested| nested.borrow_mut().insert(*owner as usize, outer.clone()));
    }
    define(cx, outer_target, c"a", owners[0]);
    define(cx, outer_target, c"b", owners[1]);
    (owners[0], inner)
}

/// One owner holds two clones of an `Rc` that refers to `target`, which refers to the owner.
/// Returns the owner and the `Rc`.
unsafe fn twin_cycle(cx: &mut JSContext) -> (*mut JSObject, Rc<Heap<JSVal>>) {
    let rc = Rc::new(Heap::default());
    let target = JS_NewPlainObject(cx);
    rc.set(ObjectValue(target));
    let owner = JS_NewObject(cx, &TWIN_OWNER_CLASS);
    TWINS.with(|twins| {
        twins
            .borrow_mut()
            .insert(owner as usize, (rc.clone(), rc.clone()))
    });
    define(cx, target, c"owner", owner);
    (owner, rc)
}

fn with_global(f: impl FnOnce(&mut JSContext)) {
    let mut runtime = Runtime::new(ENGINE.clone());
    let cx = runtime.cx();
    FINALIZED.with(|c| c.set(0));
    unsafe {
        rooted!(&in(cx) let global = new_global(cx));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        f(&mut realm);
    }
    // Dropping the data can finalize owners, which remove their own entries.
    let data = DATA.with(|data| std::mem::take(&mut *data.borrow_mut()));
    let nested = NESTED.with(|nested| std::mem::take(&mut *nested.borrow_mut()));
    let twins = TWINS.with(|twins| std::mem::take(&mut *twins.borrow_mut()));
    drop((data, nested, twins));
}

fn finalized() -> usize {
    FINALIZED.with(Cell::get)
}

#[test]
fn cycle_through_a_shared_rc_is_collected() {
    with_global(|cx| unsafe {
        let (a, b, rc) = shared_cycle(cx);
        let _ = (a, b);
        drop(rc);
        drain_autorelease_pool();
        assert_eq!(finalized(), 0);
        JS_GC(cx, GCReason::API);
        assert_eq!(finalized(), 2);
    });
}

/// Keeps owner `a` or `b` of a `shared_cycle` alive and checks that it keeps the rest.
fn live_owner_keeps_shared_contents_alive_with(keep_b: bool) {
    with_global(|cx| unsafe {
        let (a, b, rc) = shared_cycle(cx);
        // A counted reference keeps the owner alive, like a `Dom<T>` in a live object. Roots
        // don't: they rely on the autorelease pool, which the drain below empties.
        let kept = Heap::boxed(ObjectValue(if keep_b { b } else { a }));
        let target = rc.get();
        drop(rc);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        // The live owner keeps the target, and through it the other owner.
        assert_eq!(finalized(), 0);
        let name = if keep_b { c"b" } else { c"a" };
        assert_eq!(get(cx, target, name).to_object(), kept.get().to_object());
        assert!(get(cx, target, c"a").is_object());
        assert!(get(cx, target, c"b").is_object());
        drop(kept);
    });
}

#[test]
fn live_owner_keeps_shared_contents_alive() {
    live_owner_keeps_shared_contents_alive_with(false);
}

/// The collector visits owners in the order they were created; the live one comes last here.
#[test]
fn live_later_owner_keeps_shared_contents_alive() {
    live_owner_keeps_shared_contents_alive_with(true);
}

#[test]
fn cycle_through_two_clones_in_one_owner_is_collected() {
    with_global(|cx| unsafe {
        let (owner, rc) = twin_cycle(cx);
        let _ = owner;
        drop(rc);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(finalized(), 1);
    });
}

#[test]
fn live_owner_of_two_clones_keeps_the_contents_alive() {
    with_global(|cx| unsafe {
        let (owner, rc) = twin_cycle(cx);
        let kept = Heap::boxed(ObjectValue(owner));
        let target = rc.get();
        drop(rc);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(finalized(), 0);
        assert_eq!(
            get(cx, target, c"owner").to_object(),
            kept.get().to_object()
        );
        drop(kept);
    });
}

#[test]
fn owner_outside_the_heap_keeps_shared_contents_alive() {
    with_global(|cx| unsafe {
        let (a, b, rc) = shared_cycle(cx);
        let _ = (a, b);
        // `rc` stays a third owner that no trace hook reaches, like a Rust local or a task.
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(finalized(), 0);
        assert!(get(cx, rc.get(), c"a").is_object());
        assert!(get(cx, rc.get(), c"b").is_object());
        drop(rc);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(finalized(), 2);
    });
}

#[test]
fn cycle_through_nested_shared_rcs_is_collected() {
    with_global(|cx| unsafe {
        let (a, inner) = nested_cycle(cx);
        let _ = a;
        drop(inner);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(finalized(), 3);
    });
}

#[test]
fn live_owner_keeps_nested_shared_contents_alive() {
    with_global(|cx| unsafe {
        let (a, inner) = nested_cycle(cx);
        let kept = Heap::boxed(ObjectValue(a));
        let inner_target = inner.get();
        drop(inner);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        // `a` keeps the outer `Rc`, which keeps the inner one, which keeps `inner_target`.
        assert_eq!(finalized(), 0);
        assert!(get(cx, inner_target, c"c").is_object());
        drop(kept);
    });
}

/// `QJS_GC_STATS=paths:<class>` walks the heap from the roots between the collector's passes,
/// through the same trace hooks; it must not change what the passes count.
#[test]
fn debugging_walks_leave_the_counts_alone() {
    for test in [
        "cycle_through_a_shared_rc_is_collected",
        "live_owner_keeps_shared_contents_alive",
        "live_later_owner_keeps_shared_contents_alive",
        "owner_outside_the_heap_keeps_shared_contents_alive",
        "cycle_through_two_clones_in_one_owner_is_collected",
        "live_owner_of_two_clones_keeps_the_contents_alive",
        "cycle_through_nested_shared_rcs_is_collected",
        "live_owner_keeps_nested_shared_contents_alive",
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("QJS_GC_STATS", "paths:Owner")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{test}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Run by `verifier_catches_a_handle_traced_twice`: an owner whose trace hook traces its `Rc`
/// twice counts as two owners, so with the other owner outside the heap the collector would free
/// the contents under it.
#[test]
#[ignore = "run by verifier_catches_a_handle_traced_twice, with the verifier"]
fn handle_traced_twice() {
    if std::env::var_os("QJS_VERIFY_TRACING").is_none() {
        return;
    }
    with_global(|cx| unsafe {
        let rc = Rc::new(Heap::default());
        let target = JS_NewPlainObject(cx);
        rc.set(ObjectValue(target));
        let owner = JS_NewObject(cx, &TRACE_TWICE_CLASS);
        DATA.with(|data| data.borrow_mut().insert(owner as usize, rc.clone()));
        define(cx, target, c"owner", owner);
        drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        drop(rc);
    });
}

#[test]
fn verifier_catches_a_handle_traced_twice() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "handle_traced_twice", "--ignored", "--nocapture"])
        .env("QJS_VERIFY_TRACING", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("traced through the handle at"),
        "{stderr}"
    );
}
