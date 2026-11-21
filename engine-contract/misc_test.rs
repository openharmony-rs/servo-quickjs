/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Runtime-wide GC, memory accounting and engine options.

use std::ffi::c_void;
use std::ptr;

use js::jsapi::{
    self, ContextOptions, GCOptions, GCProgress, GCReason, HeapState, JSClass, JSClassOps,
    JSGCParamKey, JSGCStatus, JSJitCompilerOption, JSTracer, MemoryUse, ObjectPrivateVisitor,
    ServoSizes, SliceBudget, TraceKind, jsid,
};
use js::jsval::Int32Value;
use js::rooted;
use js::rust::JSEngine;
use js::rust::wrappers2::{
    JS_GC, JS_GetProperty, JS_GetPropertyById, JS_IdToValue, JS_NewEnumerateStandardClasses,
    JS_NewObject, JS_NewPlainObject, JS_ResolveStandardClass, RUST_INTERNED_STRING_TO_JSID,
};

use crate::support::{Observation, Value, eval, observe, record, with_global};

unsafe extern "C" fn size_of(_: *const c_void) -> usize {
    7
}

unsafe extern "C" fn object_size(_: *mut jsapi::JSObject) -> usize {
    11
}

unsafe extern "C" fn has_private(
    obj: *mut jsapi::JSObject,
    iface: *mut *mut jsapi::nsISupports,
) -> bool {
    let _ = obj;
    *iface = ptr::null_mut();
    false
}

unsafe extern "C" fn want_to_measure(_: *mut jsapi::JSObject) -> bool {
    false
}

thread_local! {
    static ROOTED_FINALIZED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

unsafe extern "C" fn finalize_rooted(_cx: *mut jsapi::GCContext, _obj: *mut jsapi::JSObject) {
    ROOTED_FINALIZED.with(|count| count.set(count.get() + 1));
}

static ROOTED_CLASS_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize_rooted),
    call: None,
    construct: None,
    trace: None,
};

static ROOTED_CLASS: JSClass = JSClass {
    name: c"ContractRawRoot".as_ptr(),
    flags: jsapi::JSCLASS_FOREGROUND_FINALIZE,
    cOps: &ROOTED_CLASS_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

fn gc_state(engine: &JSEngine) {
    let fields = with_global(engine, |cx, _global| unsafe {
        let parent_is_runtime =
            jsapi::JS_GetParentRuntime(cx.raw_cx()) == jsapi::JS_GetRuntime(cx.raw_cx());
        assert!(parent_is_runtime);
        let idle_before = jsapi::RuntimeHeapState() == HeapState::Idle;
        assert!(idle_before);

        let before = jsapi::GetGCHeapUsage(cx.raw_cx());
        rooted!(&in(cx) let unused = JS_NewPlainObject(cx));
        assert!(!unused.get().is_null());
        let after = jsapi::GetGCHeapUsage(cx.raw_cx());
        assert!(
            after >= before,
            "GC heap usage must not shrink on allocation"
        );

        jsapi::NonIncrementalGC(cx.raw_cx(), GCOptions::Normal, GCReason::API);
        let idle_after_full = jsapi::RuntimeHeapState() == HeapState::Idle;
        assert!(idle_after_full);

        let budget = SliceBudget {
            counter: jsapi::SliceBudget_UnlimitedCounter,
            interruptRequested: ptr::null_mut(),
            budget: Default::default(),
            interrupted: false,
            idle: false,
            extended: false,
            keepGoing: false,
        };
        jsapi::StartIncrementalGC(cx.raw_cx(), GCOptions::Normal, GCReason::API, &budget);
        let idle_after_incremental = jsapi::RuntimeHeapState() == HeapState::Idle;
        assert!(idle_after_incremental);

        jsapi::SetLowMemoryState(cx.raw_cx(), true);
        jsapi::SetLowMemoryState(cx.raw_cx(), false);
        let idle_after_low_memory = jsapi::RuntimeHeapState() == HeapState::Idle;
        assert!(idle_after_low_memory);

        let object_kind = std::ffi::CStr::from_ptr(jsapi::GCTraceKindToAscii(TraceKind::Object))
            .to_string_lossy()
            .into_owned();
        assert_eq!(object_kind, "JS Object");
        let script_kind = std::ffi::CStr::from_ptr(jsapi::GCTraceKindToAscii(TraceKind::Script))
            .to_string_lossy()
            .into_owned();
        assert_eq!(script_kind, "JS Script");
        let string_kind = std::ffi::CStr::from_ptr(jsapi::GCTraceKindToAscii(TraceKind::String))
            .to_string_lossy()
            .into_owned();
        assert_eq!(string_kind, "JS String");

        vec![
            ("parent_is_runtime", Value::Boolean(parent_is_runtime)),
            ("idle_before", Value::Boolean(idle_before)),
            ("idle_after_full", Value::Boolean(idle_after_full)),
            (
                "idle_after_incremental",
                Value::Boolean(idle_after_incremental),
            ),
            (
                "idle_after_low_memory",
                Value::Boolean(idle_after_low_memory),
            ),
            ("heap_usage_non_decreasing", Value::Boolean(after >= before)),
            ("heap_usage_nonzero", Value::Boolean(after > 0)),
            ("object_trace_kind", Value::String(object_kind)),
            ("script_trace_kind", Value::String(script_kind)),
            ("string_trace_kind", Value::String(string_kind)),
        ]
    });
    record(
        "misc.gc_state",
        &Observation::new(fields),
        &[
            "GCTraceKindToAscii",
            "GetGCHeapUsage",
            "JS_GetParentRuntime",
            "JS_GetRuntime",
            "JS_NewGlobalObject",
            "JS_NewPlainObject",
            "NonIncrementalGC",
            "RuntimeHeapState",
            "SetLowMemoryState",
            "StartIncrementalGC",
        ],
    );
}

fn roots_options_and_memory(engine: &JSEngine) {
    let fields = with_global(engine, |cx, _global| unsafe {
        let options: *mut ContextOptions = jsapi::ContextOptionsRef(cx.raw_cx());
        assert!(!options.is_null());
        let stable_options = options == jsapi::ContextOptionsRef(cx.raw_cx());
        assert!(stable_options);
        let wasm = (*options).wasm_();
        (*options).set_wasm_(!wasm);
        let wasm_flipped = (*options).wasm_() == !wasm;
        assert!(wasm_flipped);
        (*options).set_wasm_(wasm);
        let wasm_restored = (*options).wasm_() == wasm;
        assert!(wasm_restored);

        let mut raw = Int32Value(9);
        assert!(jsapi::AddRawValueRoot(
            cx.raw_cx(),
            &mut raw,
            c"contract-root".as_ptr(),
        ));
        let raw_value_preserved = raw.to_int32() == 9;
        assert!(raw_value_preserved);
        jsapi::RemoveRawValueRoot(cx.raw_cx(), &mut raw);

        // Root a real object at an unrooted location, collect, and read it back. The raw value
        // is the only owner of the object here (no RootedGuard), so an AddRawValueRoot no-op
        // leaves the object collectible and fails this check.
        ROOTED_FINALIZED.with(|count| count.set(0));
        let rooted_object = JS_NewObject(cx, &ROOTED_CLASS);
        assert!(!rooted_object.is_null());
        let marker = Int32Value(123);
        let object_handle = js::rust::Handle::from_marked_location(&rooted_object);
        let marker_handle = js::rust::Handle::from_marked_location(&marker);
        assert!(js::rust::wrappers2::JS_SetProperty(
            cx,
            object_handle,
            c"marker".as_ptr(),
            marker_handle,
        ));
        let mut raw_object = js::jsval::ObjectValue(rooted_object);
        assert!(jsapi::AddRawValueRoot(
            cx.raw_cx(),
            &mut raw_object,
            c"contract-object-root".as_ptr(),
        ));
        JS_GC(cx, GCReason::API);
        let rooted_marker = {
            let raw_rooted_object = raw_object.to_object();
            rooted!(&in(cx) let raw_rooted_object = raw_rooted_object);
            rooted!(&in(cx) let mut marker_value = Int32Value(0));
            assert!(js::rust::wrappers2::JS_GetProperty(
                cx,
                raw_rooted_object.handle(),
                c"marker".as_ptr(),
                marker_value.handle_mut(),
            ));
            marker_value.get().to_int32()
        };
        assert_eq!(rooted_marker, 123);
        crate::support::observe_field("raw_rooted_marker", Value::Number(rooted_marker as f64));
        let rooted_finalized = ROOTED_FINALIZED.with(std::cell::Cell::get);
        assert_eq!(rooted_finalized, 0);
        jsapi::RemoveRawValueRoot(cx.raw_cx(), &mut raw_object);
        JS_GC(cx, GCReason::API);
        let finalized_after_remove = ROOTED_FINALIZED.with(std::cell::Cell::get);
        assert_eq!(
            finalized_after_remove, 1,
            "RemoveRawValueRoot did not release the object"
        );

        rooted!(&in(cx) let object = JS_NewPlainObject(cx));
        assert!(!object.get().is_null());
        jsapi::AddAssociatedMemory(object.get(), 64, MemoryUse::DOMBinding);
        jsapi::RemoveAssociatedMemory(object.get(), 64, MemoryUse::DOMBinding);

        // The Servo stats glue dispatches through this process-global callback. It must be
        // installed before CollectServoSizes enumerates objects.
        js::glue::InitializeMemoryReporter(Some(want_to_measure));
        let mut sizes: ServoSizes = std::mem::zeroed();
        let add_sizes_ok =
            jsapi::AddServoSizeOf(cx.raw_cx(), Some(size_of), ptr::null_mut(), &mut sizes);
        assert!(add_sizes_ok);
        let mut collected: ServoSizes = std::mem::zeroed();
        let collect_ok =
            js::glue::CollectServoSizes(cx.raw_cx(), &mut collected, Some(object_size));
        assert!(collect_ok);
        assert!(collected.gcHeapUsed > 0);

        let mut visitor = ObjectPrivateVisitor {
            vtable_: ptr::null(),
            getISupports_: Some(has_private),
        };
        let mut with_visitor: ServoSizes = std::mem::zeroed();
        let add_with_visitor_ok =
            jsapi::AddServoSizeOf(cx.raw_cx(), Some(size_of), &mut visitor, &mut with_visitor);
        assert!(add_with_visitor_ok);
        assert!(sizes.gcHeapUsed > 0);
        assert!(with_visitor.gcHeapUsed > 0);

        vec![
            ("options_stable", Value::Boolean(stable_options)),
            ("wasm_initial", Value::Boolean(wasm)),
            ("wasm_flipped", Value::Boolean(wasm_flipped)),
            ("wasm_restored", Value::Boolean(wasm_restored)),
            ("raw_root_value", Value::Number(raw.to_int32() as f64)),
            ("raw_value_preserved", Value::Boolean(raw_value_preserved)),
            ("rooted_marker", Value::Number(rooted_marker as f64)),
            ("rooted_finalized", Value::Number(rooted_finalized as f64)),
            (
                "finalized_after_remove",
                Value::Number(finalized_after_remove as f64),
            ),
            ("add_servo_sizes", Value::Boolean(add_sizes_ok)),
            ("collect_servo_sizes", Value::Boolean(collect_ok)),
            (
                "add_servo_sizes_with_visitor",
                Value::Boolean(add_with_visitor_ok),
            ),
            (
                "collected_gc_heap_nonzero",
                Value::Boolean(collected.gcHeapUsed > 0),
            ),
            (
                "sizeof_gc_heap_nonzero",
                Value::Boolean(sizes.gcHeapUsed > 0),
            ),
            (
                "visitor_gc_heap_nonzero",
                Value::Boolean(with_visitor.gcHeapUsed > 0),
            ),
        ]
    });
    record(
        "misc.roots_options_memory",
        &Observation::new(fields),
        &[
            "AddAssociatedMemory",
            "AddRawValueRoot",
            "AddServoSizeOf",
            "CollectServoSizes",
            "ContextOptionsRef",
            "JS_NewGlobalObject",
            "JS_NewPlainObject",
            "RemoveAssociatedMemory",
            "RemoveRawValueRoot",
        ],
    );
}

// ---------------------------------------------------------------------------
// GC callbacks, GC parameters and job-queue hints.
// ---------------------------------------------------------------------------

unsafe extern "C" fn gc_callback(
    _cx: *mut jsapi::JSContext,
    _status: JSGCStatus,
    _reason: GCReason,
    data: *mut c_void,
) {
    let counter = data as *mut u32;
    if !counter.is_null() {
        *counter += 1;
    }
}

thread_local! {
    static SLICE_CALLBACKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static EXTRA_ROOT_TRACES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

unsafe extern "C" fn gc_slice_callback(
    _cx: *mut jsapi::JSContext,
    _progress: GCProgress,
    _desc: *const jsapi::GCDescription,
) {
    SLICE_CALLBACKS.with(|count| count.set(count.get() + 1));
}

unsafe extern "C" fn trace_data(_trc: *mut JSTracer, _data: *mut c_void) {
    EXTRA_ROOT_TRACES.with(|count| count.set(count.get() + 1));
}

fn gc_callbacks(engine: &JSEngine) -> Observation {
    let (gc_callback_calls, slice_callback_calls, extra_root_traces, gc_parameter) =
        with_global(engine, |cx, _global| unsafe {
            let mut count: u32 = 0;
            jsapi::JS_SetGCCallback(
                cx.raw_cx(),
                Some(gc_callback),
                &mut count as *mut u32 as *mut c_void,
            );
            JS_GC(cx, GCReason::API);
            jsapi::JS_SetGCCallback(cx.raw_cx(), None, ptr::null_mut());
            let gc_callback_calls = format!("{count}");

            SLICE_CALLBACKS.with(|value| value.set(0));
            let previous = jsapi::SetGCSliceCallback(cx.raw_cx(), Some(gc_slice_callback));
            let installed = previous.is_none();
            JS_GC(cx, GCReason::API);
            jsapi::SetGCSliceCallback(cx.raw_cx(), previous);
            // `<calls>:<installed>`: installing the callback must replace an empty slot, and the
            // collection must then report its slices to it.
            let slice_callback_calls = format!(
                "{}:{}",
                SLICE_CALLBACKS.with(std::cell::Cell::get),
                installed
            );

            EXTRA_ROOT_TRACES.with(|value| value.set(0));
            let registered =
                jsapi::JS_AddExtraGCRootsTracer(cx.raw_cx(), Some(trace_data), ptr::null_mut());
            JS_GC(cx, GCReason::API);
            let extra_root_traces = format!(
                "{}:{}",
                registered,
                EXTRA_ROOT_TRACES.with(std::cell::Cell::get)
            );

            jsapi::ClearKeptObjects(cx.raw_cx());
            jsapi::JS_MaybeGC(cx.raw_cx());

            let previous_max = jsapi::JS_GetGCParameter(cx.raw_cx(), JSGCParamKey::JSGC_MAX_BYTES);
            jsapi::JS_SetGCParameter(
                cx.raw_cx(),
                JSGCParamKey::JSGC_MAX_BYTES,
                previous_max.saturating_sub(1).max(1),
            );
            let updated = jsapi::JS_GetGCParameter(cx.raw_cx(), JSGCParamKey::JSGC_MAX_BYTES);
            jsapi::JS_SetGCParameter(cx.raw_cx(), JSGCParamKey::JSGC_MAX_BYTES, previous_max);
            // The value read back after the write; 0 means the shim stores no GC parameter.
            let gc_parameter = format!("{updated}");

            jsapi::JobQueueIsEmpty(cx.raw_cx());
            jsapi::JobQueueMayNotBeEmpty(cx.raw_cx());

            js::glue::InitializeMemoryReporter(Some(want_to_measure));

            (
                gc_callback_calls,
                slice_callback_calls,
                extra_root_traces,
                gc_parameter,
            )
        });

    // The callbacks are deliberately exercised on a reference engine too: a no-op registration
    // leaves all four counts at zero, so these assertions fail instead of recording a silent
    // "ran" fingerprint. QuickJS's known no-op behavior is covered by the allowlist.
    assert!(gc_callback_calls.parse::<u32>().unwrap_or(0) > 0);
    assert!(!slice_callback_calls.starts_with("0:"));
    assert!(extra_root_traces.starts_with("true:") && !extra_root_traces.ends_with(":0"));
    assert_ne!(gc_parameter, "0");
    Observation::new(vec![
        ("gc_callback_calls", Value::String(gc_callback_calls)),
        ("slice_callback_calls", Value::String(slice_callback_calls)),
        ("extra_root_traces", Value::String(extra_root_traces)),
        ("gc_parameter", Value::String(gc_parameter)),
    ])
}

// ---------------------------------------------------------------------------
// Lazy standard classes on the global.
// ---------------------------------------------------------------------------

thread_local! {
    static MAY_RESOLVED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

unsafe extern "C" fn may_resolve_standard_class(
    names: *const jsapi::JSAtomState,
    id: jsid,
    maybe_obj: *mut jsapi::JSObject,
) -> bool {
    MAY_RESOLVED.with(|count| count.set(count.get() + 1));
    jsapi::JS_MayResolveStandardClass(names, id, maybe_obj)
}

unsafe extern "C" fn resolve_nothing(
    _cx: *mut jsapi::JSContext,
    _obj: jsapi::HandleObject,
    _id: jsapi::HandleId,
    _resolved: *mut bool,
) -> bool {
    true
}

static LAZY_CLASS_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: Some(resolve_nothing),
    mayResolve: Some(may_resolve_standard_class),
    finalize: None,
    call: None,
    construct: None,
    trace: None,
};

static LAZY_CLASS: JSClass = JSClass {
    name: c"ContractLazyClass".as_ptr(),
    flags: 0,
    cOps: &LAZY_CLASS_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

fn standard_classes(engine: &JSEngine) {
    let apis = &[
        "JS_AtomizeStringN",
        "JS_GetProperty",
        "JS_MayResolveStandardClass",
        "JS_NewEnumerateStandardClasses",
        "JS_ResolveStandardClass",
        "RUST_INTERNED_STRING_TO_JSID",
    ];
    let mut steps = crate::support::Steps::new("misc.standard_classes", apis);
    steps.step("enumerate", || {
        with_global(engine, |cx, global| unsafe {
            let mut ids = js::rust::IdVector::new(cx);
            assert!(JS_NewEnumerateStandardClasses(
                cx,
                global,
                ids.handle_mut(),
                false
            ));
            assert!(
                !ids.is_empty(),
                "standard class enumeration returned no names"
            );
            crate::support::observe_field(
                "enumerate_count_nonzero",
                Value::Boolean(!ids.is_empty()),
            );
        })
    });
    steps.step("interned_string_to_id", || {
        with_global(engine, |cx, global| unsafe {
            let atom = jsapi::JS_AtomizeStringN(cx.raw_cx(), c"Array".as_ptr(), 5);
            assert!(!atom.is_null(), "atomizing Array");
            rooted!(&in(cx) let mut id = jsid::default());
            RUST_INTERNED_STRING_TO_JSID(cx, atom, id.handle_mut());
            assert!(id.get().is_string());

            rooted!(&in(cx) let mut value = js::jsval::UndefinedValue());
            assert!(JS_IdToValue(cx, id.get(), value.handle_mut()));
            let string = js::conversions::jsstr_to_string(
                cx,
                std::ptr::NonNull::new(value.get().to_string()).unwrap(),
            );
            assert_eq!(string, "Array");

            let property = JS_GetPropertyById(cx, global, id.handle(), value.handle_mut());
            assert!(property);
            assert!(
                !value.get().is_undefined(),
                "the interned id did not address the global Array property"
            );
            crate::support::observe_field("interned_id_text", Value::String(string));
        })
    });
    steps.step("atomize_and_resolve", || {
        with_global(engine, |cx, global| unsafe {
            let atom = jsapi::JS_AtomizeStringN(cx.raw_cx(), c"Array".as_ptr(), 5);
            assert!(!atom.is_null(), "atomizing Array");
            rooted!(&in(cx) let mut id = jsid::default());
            RUST_INTERNED_STRING_TO_JSID(cx, atom, id.handle_mut());

            let mut resolved = false;
            assert!(JS_ResolveStandardClass(
                cx,
                global,
                id.handle(),
                &mut resolved
            ));
            assert!(resolved, "JS_ResolveStandardClass did not resolve Array");
            crate::support::observe_field("resolved_array", Value::Boolean(resolved));
        })
    });
    steps.step("get_property", || {
        with_global(engine, |cx, global| unsafe {
            // The global has not resolved `Array` lazily in this fresh realm, so this checks the
            // property reader itself rather than the standard-class resolver.
            rooted!(&in(cx) let mut value = js::jsval::UndefinedValue());
            assert!(JS_GetProperty(
                cx,
                global,
                c"Array".as_ptr(),
                value.handle_mut()
            ));
            assert!(
                !value.get().is_undefined(),
                "Array stayed undefined in a fresh global"
            );
            assert!(eval(cx, global, "Array.isArray([])").to_boolean());
            crate::support::observe_field(
                "array_is_array",
                crate::support::eval_observe(cx, global, "Array.isArray([])"),
            );
        })
    });
    steps.step("may_resolve", || {
        with_global(engine, |cx, global| unsafe {
            let lazy = JS_NewObject(cx, &LAZY_CLASS);
            assert!(!lazy.is_null(), "creating the lazy object");
            rooted!(&in(cx) let lazy = lazy);
            crate::support::set_global(
                cx,
                global,
                c"lazyObject",
                js::jsval::ObjectValue(lazy.get()),
            );

            MAY_RESOLVED.with(|count| count.set(0));
            let stringified = eval(cx, global, "JSON.stringify(lazyObject)");
            assert_eq!(observe(cx, stringified), Value::String("{}".to_owned()));
            assert!(
                MAY_RESOLVED.with(std::cell::Cell::get) > 0,
                "the mayResolve hook did not run during the pure toJSON lookup"
            );
            crate::support::observe_field(
                "may_resolve_count",
                Value::Number(MAY_RESOLVED.with(std::cell::Cell::get) as f64),
            );
            crate::support::observe_field("stringified_lazy", observe(cx, stringified));
        })
    });
    steps.finish()
}

// ---------------------------------------------------------------------------
// Engine options, JIT backend and process lifecycle.
// ---------------------------------------------------------------------------

fn engine_options(engine: &JSEngine) -> Observation {
    // The setters return void, and reading a JIT option back does not help: on this SpiderMonkey
    // build `JS_GetGlobalJitCompilerOption` reports 0 for every option before and after the write,
    // including warmup triggers that should have a non-zero default. With no readable value there
    // is nothing a causal assertion could compare, so the case pins that the calls are safe on a
    // live context. Enabling or disabling a JIT switch here would also change how the rest of the
    // suite executes.
    with_global(engine, |cx, _global| unsafe {
        jsapi::JS_SetGlobalJitCompilerOption(
            cx.raw_cx(),
            JSJitCompilerOption::JSJITCOMPILER_NATIVE_REGEXP_ENABLE,
            1,
        );
        jsapi::JS_SetOffthreadIonCompilationEnabled(cx.raw_cx(), false);
        jsapi::JS_SetOffthreadIonCompilationEnabled(cx.raw_cx(), true);
    });

    Observation::new(vec![("ran", Value::Boolean(true))])
}

/// `DisableJitBackend` may only run before `JS_Init` and before any context exists. The suite's
/// engine is already live, so record the precondition rather than call it out of order.
fn disable_jit_backend(_engine: &JSEngine) -> Observation {
    Observation::new(vec![(
        "precondition",
        Value::String("called before JS_Init and before any JSContext exists".to_owned()),
    )])
}

/// `JS_Init` and `JS_ShutDown` are process-global and single-shot. The suite owns one engine for
/// the whole run, so the observable half of the contract is that a second `JSEngine::init` must
/// not call `JS_Init` again.
fn process_lifecycle(_engine: &JSEngine) -> Observation {
    match JSEngine::init() {
        Err(js::rust::JSEngineError::AlreadyInitialized) => Observation::new(vec![
            (
                "second_init",
                Value::String("already initialized".to_owned()),
            ),
            (
                "shutdown",
                Value::String("deferred until the last engine is dropped".to_owned()),
            ),
        ]),
        Err(other) => panic!("unexpected second JSEngine::init error: {other:?}"),
        Ok(_) => panic!("a second JSEngine::init succeeded; JS_Init ran twice"),
    }
}

/// `Runtime::new` initializes self-hosted code; the case confirms the result is usable.
///
/// No causal assertion is possible in this harness. All four attempts abort SpiderMonkey instead
/// of producing an observation: a bare context that skips `InitSelfHostedCode` segfaults inside
/// `JS_NewGlobalObject`, calling it a second time segfaults, and passing a `SelfHostedWriter`
/// (meant to observe the generated cache) segfaults with the same signature. The cache is passed
/// by value as an opaque array, so its contents cannot be read back either. QuickJS returns `true`
/// unconditionally and builds its standard library natively.
fn self_hosted_code(engine: &JSEngine) -> Observation {
    let result = with_global(engine, |cx, global| {
        let value = eval(cx, global, "[].map(x => x + 1).length === 0");
        assert_eq!(observe(cx, value), Value::Boolean(true));
        crate::support::eval_observe(
            cx,
            global,
            "'abc'.toUpperCase() + ':' + [1, 2, 3].map(x => x * 2).join('-')",
        )
    });

    Observation::new(vec![("self_hosted_result", result)])
}

// ---------------------------------------------------------------------------
// Build ids.
// ---------------------------------------------------------------------------

unsafe extern "C" fn build_id(vector: *mut jsapi::BuildIdCharVector) -> bool {
    let bytes = b"servo-contract";
    js::glue::SetBuildId(vector, bytes.as_ptr() as *const _, bytes.len())
}

/// Only registration is observable: `SetProcessBuildIdOp` stores the callback and SpiderMonkey
/// invokes it while encoding a stencil for the off-thread compilation cache. That encoder is not
/// exported by bindgen, `SetBuildId` appends to an opaque `BuildIdCharVector` with no length or
/// content accessor, and SpiderMonkey has no `GetBuildId` to read the result back.
fn build_ids(_engine: &JSEngine) -> Observation {
    unsafe {
        jsapi::SetProcessBuildIdOp(Some(build_id));
    }
    Observation::new(vec![("ran", Value::Boolean(true))])
}

pub fn run(engine: &JSEngine) {
    crate::support::isolate_case("misc.gc_state", || gc_state(engine));
    crate::support::isolate_case("misc.roots_options_memory", || {
        roots_options_and_memory(engine)
    });
    crate::support::run_case(
        "misc.gc_callbacks",
        &[
            "ClearKeptObjects",
            "InitializeMemoryReporter",
            "JS_AddExtraGCRootsTracer",
            "JS_GC",
            "JS_GetGCParameter",
            "JS_MaybeGC",
            "JS_SetGCCallback",
            "JS_SetGCParameter",
            "JobQueueIsEmpty",
            "JobQueueMayNotBeEmpty",
            "SetGCSliceCallback",
        ],
        || gc_callbacks(engine),
    );
    crate::support::isolate_case("misc.standard_classes", || standard_classes(engine));
    crate::support::run_case(
        "misc.engine_options",
        &[
            "JS_SetGlobalJitCompilerOption",
            "JS_SetOffthreadIonCompilationEnabled",
        ],
        || engine_options(engine),
    );
    crate::support::run_case("misc.jit_backend", &["DisableJitBackend"], || {
        disable_jit_backend(engine)
    });
    crate::support::run_case(
        "misc.process_lifecycle",
        &["JS_Init", "JS_ShutDown"],
        || process_lifecycle(engine),
    );
    crate::support::run_case("misc.self_hosted_code", &["InitSelfHostedCode"], || {
        self_hosted_code(engine)
    });
    crate::support::run_case(
        "misc.build_ids",
        &["SetBuildId", "SetProcessBuildIdOp"],
        || build_ids(engine),
    );
}
