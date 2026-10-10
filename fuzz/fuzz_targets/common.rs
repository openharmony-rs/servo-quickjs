/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared fuzzing harness: one runtime per process (so that leaks across inputs grow the RSS),
//! a fresh realm per input, the tracing verifier on, a QuickJS memory limit, and a watchdog that
//! interrupts scripts running past their deadline.

#![allow(dead_code)]

use std::cell::RefCell;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Once};
use std::time::{Duration, Instant};

use mozjs::context::JSContext;
use mozjs::glue::JS_GetReservedSlot;
use mozjs::jsapi::{
    CloneDataPolicy, GCReason, Handle, HandleObject as RawHandleObject, JSClass,
    JSContext as RawJSContext, JSObject, JSString, JSStructuredCloneCallbacks,
    JSStructuredCloneReader, JSStructuredCloneWriter, JS_NewObject, JS_NewStringCopyZ,
    JS_ReadString, JS_ReadUint32Pair, JS_SetReservedSlot, JS_WriteString,
    JS_WriteUint32PairUnchecked, OnNewGlobalHookOption, StructuredCloneScope,
    JSCLASS_RESERVED_SLOTS_MASK, JSCLASS_RESERVED_SLOTS_SHIFT, JS_STRUCTURED_CLONE_VERSION,
};
use mozjs::jsval::{JSVal, ObjectValue, StringValue, UndefinedValue};
use mozjs::qjs;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    JS_ClearPendingException, JS_NewGlobalObject, JS_ReadStructuredClone, JS_WriteStructuredClone,
    JS_GC,
};
use mozjs::rust::{
    get_object_class, HandleObject, JSAutoStructuredCloneBufferWrapper, JSEngine, JSEngineHandle,
    RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};

static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    // Read by the shim when the first runtime is created.
    unsafe { std::env::set_var("QJS_VERIFY_TRACING", "1") };
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

/// Milliseconds since `START` after which the running script is interrupted; 0 for none.
static DEADLINE: AtomicU64 = AtomicU64::new(0);
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

fn past_deadline() -> bool {
    let deadline = DEADLINE.load(Ordering::SeqCst);
    deadline != 0 && now_ms() > deadline
}

unsafe extern "C" fn interrupt(_cx: *mut RawJSContext) -> bool {
    !past_deadline()
}

thread_local! {
    static RUNTIME: RefCell<Option<&'static mut Runtime>> = const { RefCell::new(None) };
}

/// Runs `f` in a new global's realm of this thread's runtime, then collects garbage.
pub fn with_realm(f: impl FnOnce(&mut JSContext, HandleObject)) {
    RUNTIME.with(|cell| {
        let mut cell = cell.borrow_mut();
        let runtime = cell.get_or_insert_with(|| {
            let runtime = Box::leak(Box::new(Runtime::new(ENGINE.clone())));
            let cx = runtime.cx();
            unsafe {
                let rt = mozjs::jsapi::JS_GetRuntime(cx.raw_cx()) as *mut qjs::JSRuntime;
                qjs::JS_SetMemoryLimit(rt, 256 << 20);
                assert!(mozjs::jsapi::JS_AddInterruptCallback(
                    cx.raw_cx(),
                    Some(interrupt)
                ));
            }
            let thread_safe = runtime.thread_safe_js_context();
            static WATCHDOG: Once = Once::new();
            WATCHDOG.call_once(|| {
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(5));
                    if past_deadline() {
                        thread_safe.request_interrupt_callback();
                    }
                });
            });
            runtime
        });
        let cx = runtime.cx();
        let mut options = RealmOptions::default();
        options.creationOptions_.sharedMemoryAndAtomics_ = true;
        options.creationOptions_.defineSharedArrayBufferConstructor_ = true;
        unsafe {
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*options,
            ));
            {
                let mut realm = AutoRealm::new_from_handle(cx, global.handle());
                let (global, cx) = realm.global_and_reborrow();
                f(cx, global);
                JS_ClearPendingException(cx);
            }
        }
        // Like Servo between tasks: release what the engine handed to Rust, then collect.
        mozjs::quickjs::drain_autorelease_pool();
        unsafe { JS_GC(cx, GCReason::API) };
        // Nothing outside refers to this input's realms any more, unless jobs are still queued.
        let rt = unsafe { mozjs::jsapi::JS_GetRuntime(cx.raw_cx()) } as *mut qjs::JSRuntime;
        if unsafe { qjs::JS_IsJobPending(rt) } == 0 {
            let leaked = mozjs::quickjs::realm_count();
            assert_eq!(leaked, 0, "{leaked} realm(s) leaked by this input");
        }
        print_memory_usage(rt);
    });
}

unsafe extern "C" {
    fn __sanitizer_get_current_allocated_bytes() -> usize;
}

/// With `FUZZ_MEMORY_STATS=<n>`, the runtime's memory and the process's live heap every n inputs:
/// growth across inputs that leave nothing behind is a leak.
fn print_memory_usage(rt: *mut qjs::JSRuntime) {
    static INPUTS: AtomicU64 = AtomicU64::new(0);
    static EVERY: LazyLock<u64> = LazyLock::new(|| {
        std::env::var("FUZZ_MEMORY_STATS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    });
    let inputs = INPUTS.fetch_add(1, Ordering::Relaxed) + 1;
    if *EVERY == 0 || inputs % *EVERY != 0 {
        return;
    }
    let mut usage: qjs::JSMemoryUsage = unsafe { std::mem::zeroed() };
    unsafe { qjs::JS_ComputeMemoryUsage(rt, &mut usage) };
    eprintln!(
        "memory: inputs={inputs} heap={}KiB malloc={}KiB objects={} atoms={} strings={} shapes={} functions={}",
        unsafe { __sanitizer_get_current_allocated_bytes() } / 1024,
        usage.malloc_size / 1024,
        usage.obj_count,
        usage.atom_count,
        usage.str_count,
        usage.shape_count,
        usage.js_func_count,
    );
}

/// Runs `f` with the watchdog armed for `budget`.
pub fn with_deadline<R>(budget: Duration, f: impl FnOnce() -> R) -> R {
    DEADLINE.store(now_ms() + budget.as_millis() as u64, Ordering::SeqCst);
    let result = f();
    DEADLINE.store(0, Ordering::SeqCst);
    result
}

// Structured clone with an embedder class, as Servo's DOM objects are serialized.

pub static HOST_CLASS: JSClass = JSClass {
    name: c"Host".as_ptr(),
    flags: (1 & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

pub const HOST_TAG: u32 = 0xFFFF8001;

/// A host object holding `v` in its reserved slot (counted storage, traced by the shim).
pub unsafe fn new_host(cx: *mut RawJSContext, v: JSVal) -> *mut JSObject {
    let obj = JS_NewObject(cx, &HOST_CLASS);
    if !obj.is_null() {
        JS_SetReservedSlot(obj, 0, &v);
    }
    obj
}

unsafe extern "C" fn write_host(
    cx: *mut RawJSContext,
    w: *mut JSStructuredCloneWriter,
    obj: RawHandleObject,
    _same_process_scope_required: *mut bool,
    _closure: *mut c_void,
) -> bool {
    if !ptr::eq(get_object_class(*obj.ptr), &HOST_CLASS) {
        return false;
    }
    let mut v = UndefinedValue();
    JS_GetReservedSlot(*obj.ptr, 0, &mut v);
    let s = if v.is_string() {
        v.to_string()
    } else {
        JS_NewStringCopyZ(cx, c"host".as_ptr())
    };
    !s.is_null()
        && JS_WriteUint32PairUnchecked(w, HOST_TAG, 7)
        && JS_WriteString(w, Handle::from_marked_location(&s))
}

unsafe extern "C" fn read_host(
    cx: *mut RawJSContext,
    r: *mut JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    _data: u32,
    _closure: *mut c_void,
) -> *mut JSObject {
    if tag != HOST_TAG {
        return ptr::null_mut();
    }
    let (mut a, mut b) = (0, 0);
    let _ = JS_ReadUint32Pair(r, &mut a, &mut b);
    rooted!(in(cx) let mut s = ptr::null_mut::<JSString>());
    if !JS_ReadString(r, s.handle_mut().into()) || s.is_null() {
        return ptr::null_mut();
    }
    new_host(cx, StringValue(&*s.get()))
}

pub static CALLBACKS: JSStructuredCloneCallbacks = JSStructuredCloneCallbacks {
    read: Some(read_host),
    write: Some(write_host),
    reportError: None,
    readTransfer: None,
    writeTransfer: None,
    freeTransfer: None,
    canTransfer: None,
    sabCloned: None,
};

pub const POLICY: CloneDataPolicy = CloneDataPolicy {
    allowIntraClusterClonableSharedObjects_: false,
    allowSharedMemoryObjects_: true,
};

pub const SCOPE: StructuredCloneScope = StructuredCloneScope::DifferentProcess;

/// Serializes `v` with `transfer` like Servo's `structuredclone::write`.
pub fn write(cx: &mut JSContext, v: JSVal, transfer: JSVal) -> Option<Vec<u8>> {
    rooted!(&in(cx) let v = v);
    rooted!(&in(cx) let transfer = transfer);
    unsafe {
        let buf = JSAutoStructuredCloneBufferWrapper::new(SCOPE, &CALLBACKS);
        let data = &mut (*buf.as_raw_ptr()).data_;
        if !JS_WriteStructuredClone(
            cx,
            v.handle(),
            data,
            SCOPE,
            &POLICY,
            &CALLBACKS,
            ptr::null_mut(),
            transfer.handle(),
        ) {
            return None;
        }
        let len = mozjs::glue::GetLengthOfJSStructuredCloneData(data);
        let mut bytes = Vec::with_capacity(len);
        mozjs::glue::CopyJSStructuredCloneData(data, bytes.as_mut_ptr());
        bytes.set_len(len);
        Some(bytes)
    }
}

/// Deserializes like Servo's `structuredclone::read`.
pub fn read(cx: &mut JSContext, bytes: &[u8]) -> Option<JSVal> {
    unsafe {
        let buf = JSAutoStructuredCloneBufferWrapper::new(SCOPE, &CALLBACKS);
        let data = &mut (*buf.as_raw_ptr()).data_;
        if !mozjs::glue::WriteBytesToJSStructuredCloneData(bytes.as_ptr(), bytes.len(), data) {
            return None;
        }
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let ok = JS_ReadStructuredClone(
            cx,
            data,
            JS_STRUCTURED_CLONE_VERSION,
            SCOPE,
            rval.handle_mut(),
            &POLICY,
            &CALLBACKS,
            ptr::null_mut(),
        );
        ok.then(|| rval.get())
    }
}

pub fn object_value(obj: *mut JSObject) -> JSVal {
    if obj.is_null() {
        UndefinedValue()
    } else {
        ObjectValue(obj)
    }
}

pub fn c(name: &'static CStr) -> *const std::ffi::c_char {
    name.as_ptr()
}
