/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Memory the embedder reports with `AddAssociatedMemory` has a GC trigger of its own (patch
//! Q43), so garbage cycles that own much Rust memory and few JS objects are collected, and live
//! reported memory does not hold back the collections that QuickJS's own heap triggers.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::gc::Traceable;
use mozjs::jsapi::{
    self, AddAssociatedMemory, GCContext, Heap, JSClass, JSClassOps, JSObject, JSTracer, MemoryUse,
    OnNewGlobalHookOption, RemoveAssociatedMemory, JSPROP_ENUMERATE,
};
use mozjs::jsval::{JSVal, ObjectValue};
use mozjs::quickjs::drain_autorelease_pool;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    JS_DefineProperty, JS_NewGlobalObject, JS_NewObject, JS_NewPlainObject,
};
use mozjs::rust::{JSEngine, JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

/// The Rust data of an `OWNER_CLASS` object.
struct Owner {
    /// What it refers to.
    heap: Option<Box<Heap<JSVal>>>,
    /// The memory it reports.
    bytes: usize,
}

thread_local! {
    static DATA: RefCell<HashMap<usize, Owner>> = RefCell::new(HashMap::new());
    static FINALIZED: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn trace(trc: *mut JSTracer, obj: *mut JSObject) {
    DATA.with(|data| {
        if let Some(Owner {
            heap: Some(heap), ..
        }) = data.borrow().get(&(obj as usize))
        {
            heap.trace(trc);
        }
    });
}

unsafe extern "C" fn finalize(_: *mut GCContext, obj: *mut JSObject) {
    let owner = DATA.with(|data| data.borrow_mut().remove(&(obj as usize)));
    if let Some(owner) = &owner {
        RemoveAssociatedMemory(obj, owner.bytes, MemoryUse::DOMBinding);
    }
    FINALIZED.with(|f| f.set(f.get() + 1));
    drop(owner);
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

unsafe fn new_global(cx: &mut JSContext) -> *mut JSObject {
    JS_NewGlobalObject(
        cx,
        &SIMPLE_GLOBAL_CLASS,
        ptr::null_mut(),
        OnNewGlobalHookOption::FireOnNewGlobalHook,
        &*RealmOptions::default(),
    )
}

/// A new owner that reports `bytes` and refers to `target`, if any.
unsafe fn new_owner(cx: &mut JSContext, bytes: usize, target: *mut JSObject) -> *mut JSObject {
    let owner = JS_NewObject(cx, &OWNER_CLASS);
    AddAssociatedMemory(owner, bytes, MemoryUse::DOMBinding);
    let heap = (!target.is_null()).then(|| Heap::boxed(ObjectValue(target)));
    DATA.with(|data| {
        data.borrow_mut()
            .insert(owner as usize, Owner { heap, bytes })
    });
    owner
}

/// An owner reporting `bytes` that refers to a plain object that refers back: a garbage cycle
/// as soon as nothing else refers to it.
unsafe fn garbage_cycle(cx: &mut JSContext, bytes: usize) {
    let target = JS_NewPlainObject(cx);
    let owner = new_owner(cx, bytes, target);
    rooted!(&in(cx) let target = target);
    rooted!(&in(cx) let owner = ObjectValue(owner));
    assert!(JS_DefineProperty(
        cx,
        target.handle(),
        c"owner".as_ptr(),
        owner.handle(),
        JSPROP_ENUMERATE as u32
    ));
}

/// Runs `f` in a new runtime and returns how many owners it finalized while it ran.
fn finalized_by(f: impl FnOnce(&mut JSContext)) -> usize {
    let mut runtime = Runtime::new(ENGINE.clone());
    let cx = runtime.cx();
    FINALIZED.with(|f| f.set(0));
    unsafe {
        rooted!(&in(cx) let global = new_global(cx));
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        f(&mut realm);
    }
    let finalized = FINALIZED.with(Cell::get);
    // Dropping the data can finalize owners, which remove their own entries.
    let data = DATA.with(|data| std::mem::take(&mut *data.borrow_mut()));
    drop(data);
    finalized
}

#[test]
fn associated_memory_triggers_collections() {
    // 1000 cycles with 4 MiB each of embedder memory and almost nothing on the JS heap; no
    // explicit collection.
    let finalized = finalized_by(|cx| unsafe {
        for _ in 0..1000 {
            garbage_cycle(cx, 4 << 20);
            drain_autorelease_pool();
        }
    });
    let alive = 1000 - finalized;
    // Without associated memory in a trigger, several hundred of them stay alive; with it, at
    // most the trigger's 114 MiB (38 MiB times 3, collections being frequent).
    assert!(alive < 100, "{alive} of 1000 cycles alive");
}

#[test]
fn live_associated_memory_does_not_delay_collections() {
    const CYCLES: usize = 20_000;
    // Garbage cycles without associated memory, which QuickJS's own heap trigger collects.
    let cycles_alive = |live: usize| {
        let mut alive = 0;
        finalized_by(|cx| unsafe {
            let kept = Heap::boxed(ObjectValue(new_owner(cx, live, ptr::null_mut())));
            for _ in 0..CYCLES {
                garbage_cycle(cx, 0);
                drain_autorelease_pool();
            }
            alive = CYCLES - FINALIZED.with(Cell::get);
            drop(kept);
        });
        alive
    };
    let without = cycles_alive(0);
    // A live owner that reports 1 GiB, such as the DOM of a big page.
    let with = cycles_alive(1 << 30);
    assert!(without < CYCLES / 2, "{without} of {CYCLES} cycles alive");
    assert!(
        with <= without + CYCLES / 20,
        "{with} of {CYCLES} cycles alive next to 1 GiB of live associated memory, {without} without"
    );
}
