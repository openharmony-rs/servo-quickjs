/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Reserved-slot classes, class tracing and heap write barriers.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;

use js::glue::{
    CallBigIntTracer, CallFunctionTracer, CallObjectTracer, CallScriptTracer, CallStringTracer,
    CallValueTracer, JS_GetReservedSlot,
};
use js::jsapi::{
    self, GCReason, Heap, JSCLASS_RESERVED_SLOTS_MASK, JSCLASS_RESERVED_SLOTS_SHIFT, JSClass,
    JSClassOps, JSObject, JSTracer, jsid,
};
use js::jsval::{NullValue, ObjectValue, PrivateValue, UndefinedValue};
use js::rooted;
use js::rust::wrappers2::{JS_GC, JS_NewObject, JS_NewStringCopyN};
use js::rust::{CompileOptionsWrapper, JSEngine, transform_str_to_source_text};

use crate::support::{Observation, Value, eval, record, with_global};

const SLOT: u32 = 0;

thread_local! {
    static TRACED: Cell<u32> = const { Cell::new(0) };
}

struct Held {
    object: Heap<*mut JSObject>,
    function: Heap<*mut jsapi::JSFunction>,
    string: Heap<*mut jsapi::JSString>,
    script: Heap<*mut jsapi::JSScript>,
    bigint: Heap<*mut jsapi::BigInt>,
    value: Heap<jsapi::Value>,
}

impl Held {
    fn new() -> Box<Self> {
        Box::new(Held {
            object: Heap::default(),
            function: Heap::default(),
            string: Heap::default(),
            script: Heap::default(),
            bigint: Heap::default(),
            value: Heap::default(),
        })
    }
}

unsafe extern "C" fn trace_object(trc: *mut JSTracer, obj: *mut JSObject) {
    let mut slot = UndefinedValue();
    JS_GetReservedSlot(obj, SLOT, &mut slot);
    let held = slot.to_private() as *mut Held;
    if held.is_null() {
        return;
    }
    let held = &*held;
    CallObjectTracer(trc, &held.object as *const _ as *mut _, c"object".as_ptr());
    CallFunctionTracer(
        trc,
        &held.function as *const _ as *mut _,
        c"function".as_ptr(),
    );
    CallStringTracer(trc, &held.string as *const _ as *mut _, c"string".as_ptr());
    CallScriptTracer(trc, &held.script as *const _ as *mut _, c"script".as_ptr());
    CallBigIntTracer(trc, &held.bigint as *const _ as *mut _, c"bigint".as_ptr());
    CallValueTracer(trc, &held.value as *const _ as *mut _, c"value".as_ptr());
    TRACED.with(|count| count.set(count.get() + 1));
}

static CLASS_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: None,
    call: None,
    construct: None,
    trace: Some(trace_object),
};

static CLASS: JSClass = JSClass {
    name: c"ContractReservedSlot".as_ptr(),
    flags: (1 & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: &CLASS_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

fn reserved_slots_and_tracing(engine: &JSEngine) {
    let mut slot_pointer_after_gc = false;
    let mut slot_cleared = false;
    let mut trace_count = 0;
    with_global(engine, |cx, global| unsafe {
        let object = JS_NewObject(cx, &CLASS);
        assert!(!object.is_null());
        rooted!(&in(cx) let object = object);

        let held = Held::new();
        let inner = JS_NewObject(cx, ptr::null());
        assert!(!inner.is_null());
        held.object.set(inner);
        held.value.set(ObjectValue(inner));
        held.function.set(
            eval(cx, global, "(function traced() { return 41; })")
                .to_object()
                .cast(),
        );
        held.string.set(JS_NewStringCopyN(cx, c"slot".as_ptr(), 4));
        held.bigint
            .set(eval(cx, global, "123456789012345678901234567890n").to_bigint());
        let options = CompileOptionsWrapper::new(cx, c"trace-script.js".to_owned(), 1);
        held.script.set(jsapi::Compile1(
            cx.raw_cx(),
            options.ptr,
            &mut transform_str_to_source_text("1 + 1"),
        ));
        assert!(!held.string.get().is_null());
        assert!(!held.bigint.get().is_null());
        assert!(!held.script.get().is_null());

        let raw = Box::into_raw(held);
        jsapi::JS_SetReservedSlot(object.get(), SLOT, &PrivateValue(raw.cast()));
        let mut slot = UndefinedValue();
        JS_GetReservedSlot(object.get(), SLOT, &mut slot);
        assert_eq!(slot.to_private(), raw.cast::<c_void>());

        // The rooted object keeps the class live; its trace hook must visit every heap edge.
        TRACED.with(|count| count.set(0));
        JS_GC(cx, GCReason::API);
        trace_count = TRACED.with(Cell::get);
        assert!(trace_count > 0, "class trace hook did not run during JS_GC");
        JS_GetReservedSlot(object.get(), SLOT, &mut slot);
        slot_pointer_after_gc = slot.to_private() == raw.cast::<c_void>();
        assert!(slot_pointer_after_gc);

        jsapi::JS_SetReservedSlot(object.get(), SLOT, &NullValue());
        JS_GetReservedSlot(object.get(), SLOT, &mut slot);
        slot_cleared = slot.is_null();
        assert!(slot_cleared);
        drop(Box::from_raw(raw));
    });

    record(
        "class.reserved_slots_trace",
        &Observation::new(vec![
            (
                "slot_pointer_after_gc",
                Value::Boolean(slot_pointer_after_gc),
            ),
            ("slot_cleared", Value::Boolean(slot_cleared)),
            ("trace_count", Value::Number(trace_count as f64)),
        ]),
        &[
            "CallFunctionTracer",
            "CallObjectTracer",
            "CallScriptTracer",
            "CallStringTracer",
            "CallValueTracer",
            "Compile1",
            "JS_GC",
            "JS_GetReservedSlot",
            "JS_NewObject",
            "JS_SetReservedSlot",
        ],
    );
}

fn write_barriers(engine: &JSEngine) {
    let mut slot_kinds_live = 0u32;
    let mut barrier_calls_stable = 0u32;
    with_global(engine, |cx, global| unsafe {
        let object = JS_NewObject(cx, ptr::null());
        assert!(!object.is_null());
        let function = eval(cx, global, "(function () {})").to_object() as *mut jsapi::JSFunction;
        let string = JS_NewStringCopyN(cx, c"barrier".as_ptr(), 7);
        let bigint = eval(cx, global, "123456789012345678901234567890n").to_bigint();
        let options = CompileOptionsWrapper::new(cx, c"barrier-script.js".to_owned(), 1);
        let script = jsapi::Compile1(
            cx.raw_cx(),
            options.ptr,
            &mut transform_str_to_source_text("7"),
        );
        assert!(!string.is_null() && !bigint.is_null() && !script.is_null());

        // Barriers must see a stable slot: a stack temporary is invalidated when the frame ends
        // and the store buffer's edge outlives it. `Heap::boxed` keeps each slot at a fixed
        // address, and `set` runs the matching barrier after writing it.
        let object_slot = Heap::boxed(object);
        object_slot.set(object);
        assert_eq!(object_slot.get(), object);
        let function_slot = Heap::boxed(function);
        function_slot.set(function);
        assert_eq!(function_slot.get(), function);
        let string_slot = Heap::boxed(string);
        string_slot.set(string);
        assert_eq!(string_slot.get(), string);
        let bigint_slot = Heap::boxed(bigint);
        bigint_slot.set(bigint);
        assert_eq!(bigint_slot.get(), bigint);
        let script_slot = Heap::boxed(script);
        script_slot.set(script);
        assert_eq!(script_slot.get(), script);

        let value_slot = Heap::boxed(ObjectValue(object));
        value_slot.set(ObjectValue(object));
        assert_eq!(value_slot.get(), ObjectValue(object));

        // Call the public combined pre/post-write barriers directly. They return void and do not
        // write the slot themselves, so the strongest observable contract is that each call
        // leaves the stable slot unchanged; a no-op remains indistinguishable here.
        let null_object: *mut JSObject = ptr::null_mut();
        jsapi::HeapObjectWriteBarriers(object_slot.get_unsafe(), null_object, object);
        barrier_calls_stable += (object_slot.get() == object) as u32;

        let function_as_object_slot = function_slot.get_unsafe().cast::<*mut JSObject>();
        jsapi::HeapObjectWriteBarriers(function_as_object_slot, null_object, function.cast());
        barrier_calls_stable += (function_slot.get() == function) as u32;

        jsapi::HeapStringWriteBarriers(string_slot.get_unsafe(), ptr::null_mut(), string);
        barrier_calls_stable += (string_slot.get() == string) as u32;

        jsapi::HeapBigIntWriteBarriers(bigint_slot.get_unsafe(), ptr::null_mut(), bigint);
        barrier_calls_stable += (bigint_slot.get() == bigint) as u32;

        jsapi::HeapScriptWriteBarriers(script_slot.get_unsafe(), ptr::null_mut(), script);
        barrier_calls_stable += (script_slot.get() == script) as u32;

        let prev_value = UndefinedValue();
        let next_value = ObjectValue(object);
        jsapi::HeapValueWriteBarriers(value_slot.get_unsafe(), &prev_value, &next_value);
        barrier_calls_stable += (value_slot.get() == ObjectValue(object)) as u32;
        assert_eq!(
            barrier_calls_stable, 6,
            "write barriers must leave their slots unchanged"
        );

        // Keep the slots rooted across a GC so the recorded edges are traced while alive. GC may
        // move the referents, so only check that each slot still holds a live value.
        jsapi::JS_GC(cx.raw_cx(), GCReason::API);
        assert!(!object_slot.get().is_null());
        assert!(!function_slot.get().is_null());
        assert!(!string_slot.get().is_null());
        assert!(!bigint_slot.get().is_null());
        assert!(!script_slot.get().is_null());
        assert!(!value_slot.get().is_undefined());
        slot_kinds_live = [
            !object_slot.get().is_null(),
            !function_slot.get().is_null(),
            !string_slot.get().is_null(),
            !bigint_slot.get().is_null(),
            !script_slot.get().is_null(),
            !value_slot.get().is_undefined(),
        ]
        .into_iter()
        .filter(|live| *live)
        .count() as u32;
    });

    record(
        "class.heap_write_barriers",
        &Observation::new(vec![
            (
                "slot_kinds_live_after_gc",
                Value::Number(slot_kinds_live as f64),
            ),
            (
                "barrier_calls_stable",
                Value::Number(barrier_calls_stable as f64),
            ),
        ]),
        &[
            "Compile1",
            "HeapBigIntWriteBarriers",
            "HeapObjectWriteBarriers",
            "HeapScriptWriteBarriers",
            "HeapStringWriteBarriers",
            "HeapValueWriteBarriers",
        ],
    );
}

// ---------------------------------------------------------------------------
// Tracing every GC edge kind through a class trace hook.
// ---------------------------------------------------------------------------

thread_local! {
    static TRACER_COUNT: Cell<u32> = const { Cell::new(0) };
    /// Objects that only the trace hook above can keep alive.
    static TRACED_CHILD_FINALIZED: Cell<u32> = const { Cell::new(0) };
    /// Whether the trace hook follows its raw slots; the negative control turns this off.
    static TRACE_RAW_SLOTS: Cell<bool> = const { Cell::new(true) };
}

unsafe extern "C" fn finalize_traced_child(_cx: *mut jsapi::GCContext, _obj: *mut JSObject) {
    TRACED_CHILD_FINALIZED.with(|count| count.set(count.get() + 1));
}

static TRACED_CHILD_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: Some(finalize_traced_child),
    call: None,
    construct: None,
    trace: None,
};

static TRACED_CHILD_CLASS: JSClass = JSClass {
    name: c"ContractTracedChild".as_ptr(),
    flags: 0,
    cOps: &TRACED_CHILD_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

/// Raw, uncounted slots. Nothing but the class trace hook refers to the objects they hold, so a
/// slot the hook does not follow is only stored here and the object is collected.
struct RawEdges {
    object: *mut JSObject,
    value: jsapi::Value,
    values: [jsapi::Value; 1],
    descriptor: jsapi::PropertyDescriptor,
}

static TRACER_CLASS_OPS: JSClassOps = JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: None,
    call: None,
    construct: None,
    trace: Some(trace_edge_kinds),
};

static TRACER_CLASS: JSClass = JSClass {
    name: c"ContractTracer".as_ptr(),
    flags: (1 & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: &TRACER_CLASS_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

const RAW_EDGES_SLOT: u32 = 0;

unsafe extern "C" fn trace_edge_kinds(trc: *mut JSTracer, obj: *mut JSObject) {
    TRACER_COUNT.with(|count| count.set(count.get() + 1));

    let mut edges = UndefinedValue();
    JS_GetReservedSlot(obj, RAW_EDGES_SLOT, &mut edges);
    let edges = edges.to_private() as *mut RawEdges;
    if !edges.is_null() && TRACE_RAW_SLOTS.with(Cell::get) {
        let edges = &mut *edges;
        js::glue::CallObjectRootTracer(trc, &mut edges.object, c"object".as_ptr());
        js::glue::CallValueRootTracer(trc, &mut edges.value, c"value".as_ptr());
        jsapi::TraceValueArray(trc, edges.values.len(), edges.values.as_mut_ptr());
        js::glue::CallPropertyDescriptorTracer(trc, &mut edges.descriptor);
    }

    // `CallBigIntTracer` and `CallSymbolTracer` trace cell kinds that carry no object finalizer, so
    // there is no counter to read survival from; proving they were marked needs the equivalent of
    // `IsAboutToBeFinalized`, which bindgen does not export. `CallIdTracer` traces atoms, which are
    // reference-counted rather than collected, so again no finalizer observes them. All three are
    // therefore exercised for arity and calling convention only.
    let mut bigint: Heap<*mut jsapi::BigInt> = Heap::default();
    js::glue::CallBigIntTracer(trc, &mut bigint, c"bigint".as_ptr());

    let mut id = Heap {
        ptr: std::cell::UnsafeCell::new(jsid::default()),
    };
    js::glue::CallIdTracer(trc, &mut id, c"id".as_ptr());

    let mut symbol: Heap<*mut jsapi::Symbol> = Heap::default();
    js::glue::CallSymbolTracer(trc, &mut symbol, c"symbol".as_ptr());
}

fn raw_edges(cx: &mut js::context::JSContext) -> Box<RawEdges> {
    unsafe {
        let object = JS_NewObject(cx, &TRACED_CHILD_CLASS);
        let value_object = JS_NewObject(cx, &TRACED_CHILD_CLASS);
        let array_object = JS_NewObject(cx, &TRACED_CHILD_CLASS);
        let getter_object = JS_NewObject(cx, &TRACED_CHILD_CLASS);
        assert!(
            !object.is_null() &&
                !value_object.is_null() &&
                !array_object.is_null() &&
                !getter_object.is_null(),
            "creating traced children"
        );
        let mut descriptor: jsapi::PropertyDescriptor = std::mem::zeroed();
        descriptor.getter_ = getter_object;
        Box::new(RawEdges {
            object,
            value: ObjectValue(value_object),
            values: [ObjectValue(array_object)],
            descriptor,
        })
    }
}

/// The raw-slot tracers must keep their referents alive. The children (an object, a value, a value
/// array element and a property descriptor's getter) are reachable only through the boxed
/// `RawEdges`, so the trace hook is the only thing that can mark them. The second pass turns the
/// tracing off and shows that all four are then collected, which is the control that makes the
/// first number meaningful rather than "nothing was collected anyway".
fn edge_tracers(engine: &JSEngine) -> Observation {
    let (traced_finalized, untraced_finalized) = with_global(engine, |cx, _global| unsafe {
        let object = JS_NewObject(cx, &TRACER_CLASS);
        assert!(!object.is_null(), "creating the traced object");
        rooted!(&in(cx) let object = object);

        let edges = Box::into_raw(raw_edges(cx));
        jsapi::JS_SetReservedSlot(
            object.get(),
            RAW_EDGES_SLOT,
            &PrivateValue(edges.cast::<c_void>()),
        );

        TRACER_COUNT.with(|count| count.set(0));
        TRACED_CHILD_FINALIZED.with(|count| count.set(0));
        TRACE_RAW_SLOTS.with(|trace| trace.set(true));
        JS_GC(cx, GCReason::API);
        assert!(
            TRACER_COUNT.with(Cell::get) > 0,
            "the class trace hook did not run"
        );
        let traced_finalized = TRACED_CHILD_FINALIZED.with(Cell::get);

        TRACE_RAW_SLOTS.with(|trace| trace.set(false));
        JS_GC(cx, GCReason::API);
        let untraced_finalized = TRACED_CHILD_FINALIZED.with(Cell::get);
        TRACE_RAW_SLOTS.with(|trace| trace.set(true));
        jsapi::JS_SetReservedSlot(object.get(), RAW_EDGES_SLOT, &NullValue());

        drop(Box::from_raw(edges));
        (traced_finalized, untraced_finalized)
    });

    Observation::new(vec![
        (
            "traced_children_finalized",
            Value::Number(traced_finalized as f64),
        ),
        (
            "untraced_children_finalized",
            Value::Number(untraced_finalized as f64),
        ),
    ])
}

pub fn run(engine: &JSEngine) {
    crate::support::isolate_case("class.reserved_slots_trace", || {
        reserved_slots_and_tracing(engine)
    });
    crate::support::isolate_case("class.heap_write_barriers", || write_barriers(engine));
    crate::support::run_case(
        "class.edge_tracers",
        &[
            "CallBigIntTracer",
            "CallIdTracer",
            "CallObjectRootTracer",
            "CallPropertyDescriptorTracer",
            "CallSymbolTracer",
            "CallValueRootTracer",
            "JS_GC",
            "JS_NewObject",
            "TraceValueArray",
        ],
        || edge_tracers(engine),
    );
}
