/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! ArrayBuffers, typed arrays and DataViews.

#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use js::context::JSContext;
use js::glue::{
    GetFloat32ArrayLengthAndData, GetFloat64ArrayLengthAndData, GetInt8ArrayLengthAndData,
    GetInt16ArrayLengthAndData, GetInt32ArrayLengthAndData, GetUint8ArrayLengthAndData,
    GetUint8ClampedArrayLengthAndData, GetUint16ArrayLengthAndData, GetUint32ArrayLengthAndData,
};
use js::jsapi::{
    ArrayBufferHasData, GetArrayBufferByteLength, GetArrayBufferData, GetArrayBufferLengthAndData,
    GetArrayBufferViewLengthAndData, GetObjectAsArrayBuffer, IsArrayBufferObject,
    IsDetachedArrayBufferObject, JS_GetArrayBufferViewByteLength, JS_GetArrayBufferViewByteOffset,
    JS_GetArrayBufferViewType, JS_GetBigInt64ArrayData, JS_GetBigUint64ArrayData,
    JS_GetFloat16ArrayData, JS_GetFloat32ArrayData, JS_GetFloat64ArrayData, JS_GetInt8ArrayData,
    JS_GetInt16ArrayData, JS_GetInt32ArrayData, JS_GetObjectAsArrayBufferView,
    JS_GetObjectAsBigInt64Array, JS_GetObjectAsBigUint64Array, JS_GetObjectAsFloat16Array,
    JS_GetObjectAsFloat32Array, JS_GetObjectAsFloat64Array, JS_GetObjectAsInt8Array,
    JS_GetObjectAsInt16Array, JS_GetObjectAsInt32Array, JS_GetObjectAsUint8Array,
    JS_GetObjectAsUint8ClampedArray, JS_GetObjectAsUint16Array, JS_GetObjectAsUint32Array,
    JS_GetTypedArrayByteLength, JS_GetTypedArrayLength, JS_GetTypedArraySharedness,
    JS_GetUint8ArrayData, JS_GetUint8ClampedArrayData, JS_GetUint16ArrayData,
    JS_GetUint32ArrayData, JS_IsArrayBufferViewObject, JS_IsTypedArrayObject, JSObject,
    OnNewGlobalHookOption, Type, UnwrapArrayBuffer, UnwrapArrayBufferView, UnwrapBigInt64Array,
    UnwrapBigUint64Array, UnwrapFloat16Array, UnwrapFloat32Array, UnwrapFloat64Array,
    UnwrapInt8Array, UnwrapInt16Array, UnwrapInt32Array, UnwrapUint8Array, UnwrapUint8ClampedArray,
    UnwrapUint16Array, UnwrapUint32Array,
};
use js::jsval::{JSVal, ObjectValue, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    ArrayBufferClone, ArrayBufferCopyData, CopyArrayBuffer, DetachArrayBuffer,
    HasDefinedArrayBufferDetachKey, JS_NewBigInt64Array, JS_NewBigInt64ArrayFromArray,
    JS_NewBigInt64ArrayWithBuffer, JS_NewBigUint64Array, JS_NewBigUint64ArrayFromArray,
    JS_NewBigUint64ArrayWithBuffer, JS_NewDataView, JS_NewFloat16Array,
    JS_NewFloat16ArrayFromArray, JS_NewFloat16ArrayWithBuffer, JS_NewFloat32Array,
    JS_NewFloat32ArrayFromArray, JS_NewFloat32ArrayWithBuffer, JS_NewFloat64Array,
    JS_NewFloat64ArrayFromArray, JS_NewFloat64ArrayWithBuffer, JS_NewGlobalObject, JS_NewInt8Array,
    JS_NewInt8ArrayFromArray, JS_NewInt8ArrayWithBuffer, JS_NewInt16Array,
    JS_NewInt16ArrayFromArray, JS_NewInt16ArrayWithBuffer, JS_NewInt32Array,
    JS_NewInt32ArrayFromArray, JS_NewInt32ArrayWithBuffer, JS_NewUint8Array,
    JS_NewUint8ArrayFromArray, JS_NewUint8ArrayWithBuffer, JS_NewUint8ClampedArray,
    JS_NewUint8ClampedArrayFromArray, JS_NewUint8ClampedArrayWithBuffer, JS_NewUint16Array,
    JS_NewUint16ArrayFromArray, JS_NewUint16ArrayWithBuffer, JS_NewUint32Array,
    JS_NewUint32ArrayFromArray, JS_NewUint32ArrayWithBuffer, JS_SetProperty, NewArrayBuffer,
    NewArrayBufferWithContents, NewArrayBufferWithUserOwnedContents, NewExternalArrayBuffer,
    StealArrayBufferContents,
};
use js::rust::{
    CompileOptionsWrapper, HandleObject, JSEngine, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
    evaluate_script,
};
use js::typedarray::Uint8Array;

/// Runs `f` in a new runtime and global; the runtime is destroyed (finalizing everything) after.
fn with_global<R>(engine: &JSEngine, f: impl FnOnce(&mut JSContext, HandleObject) -> R) -> R {
    let mut runtime = Runtime::new(engine.handle());
    let context = runtime.cx();
    rooted!(&in(context) let global = unsafe {
        JS_NewGlobalObject(
            context,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        )
    });
    assert!(!global.get().is_null(), "JS_NewGlobalObject returned null");
    let mut realm = AutoRealm::new_from_handle(context, global.handle());
    f(&mut realm, global.handle())
}

/// Evaluates `src`; the result is kept alive by the shim's pool until the runtime is destroyed.
fn eval(cx: &mut JSContext, global: HandleObject, src: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_buffers".to_owned(), 1);
    assert!(
        evaluate_script(cx, global, src, rval.handle_mut(), options).is_ok(),
        "{src}"
    );
    rval.get()
}

fn eval_obj(cx: &mut JSContext, global: HandleObject, src: &str) -> *mut JSObject {
    let v = eval(cx, global, src);
    assert!(v.is_object(), "{src}");
    v.to_object()
}

#[track_caller]
fn assert_js(cx: &mut JSContext, global: HandleObject, src: &str) {
    let v = eval(cx, global, src);
    assert!(v.is_boolean() && v.to_boolean(), "{src}");
}

fn set_global(cx: &mut JSContext, global: HandleObject, name: &CStr, obj: *mut JSObject) {
    rooted!(&in(cx) let v = ObjectValue(obj));
    assert!(unsafe { JS_SetProperty(cx, global, name.as_ptr(), v.handle()) });
}

macro_rules! check_type {
    ($cx:expr, $global:expr, $new:ident, $from_array:ident, $with_buffer:ident, $unwrap:ident,
     $ty:expr, $ctor:literal, $size:expr, $init:literal) => {{
        let (cx, global): (&mut JSContext, HandleObject) = (&mut *$cx, $global);
        rooted!(&in(cx) let a = unsafe { $new(cx, 4) });
        assert!(!a.get().is_null());
        unsafe {
            assert!(JS_IsTypedArrayObject(a.get()));
            assert!(JS_IsArrayBufferViewObject(a.get()));
            assert!(!IsArrayBufferObject(a.get()));
            assert_eq!(JS_GetArrayBufferViewType(a.get()), $ty);
            assert_eq!(JS_GetTypedArrayLength(a.get()), 4);
            assert_eq!(JS_GetTypedArrayByteLength(a.get()), 4 * $size);
            assert!(!JS_GetTypedArraySharedness(a.get()));
            assert_eq!($unwrap(a.get()), a.get());
            assert_eq!(UnwrapArrayBufferView(a.get()), a.get());
            if $ty != Type::Int8 {
                assert!(UnwrapInt8Array(a.get()).is_null());
            }
        }
        set_global(cx, global, c"a", a.get());
        assert_js(cx, global, concat!("a instanceof ", $ctor, " && a.length === 4"));

        rooted!(&in(cx) let src = eval_obj(cx, global, $init));
        rooted!(&in(cx) let b = unsafe { $from_array(cx, src.handle()) });
        assert!(!b.get().is_null());
        assert_eq!(unsafe { JS_GetTypedArrayLength(b.get()) }, 3);
        set_global(cx, global, c"b", b.get());
        assert_js(cx, global, concat!("b instanceof ", $ctor, " && b[2] == 3"));

        rooted!(&in(cx) let buf = unsafe { NewArrayBuffer(cx, 8 * $size) });
        rooted!(&in(cx) let c = unsafe { $with_buffer(cx, buf.handle(), 2 * $size, -1) });
        assert!(!c.get().is_null());
        unsafe {
            assert_eq!(JS_GetTypedArrayLength(c.get()), 6);
            assert_eq!(JS_GetArrayBufferViewByteOffset(c.get()), 2 * $size);
        }
        rooted!(&in(cx) let d = unsafe { $with_buffer(cx, buf.handle(), 0, 3) });
        assert_eq!(unsafe { JS_GetTypedArrayLength(d.get()) }, 3);
    }};
}

fn each_type(
    cx: &mut JSContext,
    global: HandleObject,
) -> Vec<(&'static str, crate::support::Value)> {
    check_type!(
        cx,
        global,
        JS_NewInt8Array,
        JS_NewInt8ArrayFromArray,
        JS_NewInt8ArrayWithBuffer,
        UnwrapInt8Array,
        Type::Int8,
        "Int8Array",
        1,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewUint8Array,
        JS_NewUint8ArrayFromArray,
        JS_NewUint8ArrayWithBuffer,
        UnwrapUint8Array,
        Type::Uint8,
        "Uint8Array",
        1,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewUint8ClampedArray,
        JS_NewUint8ClampedArrayFromArray,
        JS_NewUint8ClampedArrayWithBuffer,
        UnwrapUint8ClampedArray,
        Type::Uint8Clamped,
        "Uint8ClampedArray",
        1,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewInt16Array,
        JS_NewInt16ArrayFromArray,
        JS_NewInt16ArrayWithBuffer,
        UnwrapInt16Array,
        Type::Int16,
        "Int16Array",
        2,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewUint16Array,
        JS_NewUint16ArrayFromArray,
        JS_NewUint16ArrayWithBuffer,
        UnwrapUint16Array,
        Type::Uint16,
        "Uint16Array",
        2,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewInt32Array,
        JS_NewInt32ArrayFromArray,
        JS_NewInt32ArrayWithBuffer,
        UnwrapInt32Array,
        Type::Int32,
        "Int32Array",
        4,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewUint32Array,
        JS_NewUint32ArrayFromArray,
        JS_NewUint32ArrayWithBuffer,
        UnwrapUint32Array,
        Type::Uint32,
        "Uint32Array",
        4,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewFloat16Array,
        JS_NewFloat16ArrayFromArray,
        JS_NewFloat16ArrayWithBuffer,
        UnwrapFloat16Array,
        Type::Float16,
        "Float16Array",
        2,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewFloat32Array,
        JS_NewFloat32ArrayFromArray,
        JS_NewFloat32ArrayWithBuffer,
        UnwrapFloat32Array,
        Type::Float32,
        "Float32Array",
        4,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewFloat64Array,
        JS_NewFloat64ArrayFromArray,
        JS_NewFloat64ArrayWithBuffer,
        UnwrapFloat64Array,
        Type::Float64,
        "Float64Array",
        8,
        "[1, 2, 3]"
    );
    check_type!(
        cx,
        global,
        JS_NewBigInt64Array,
        JS_NewBigInt64ArrayFromArray,
        JS_NewBigInt64ArrayWithBuffer,
        UnwrapBigInt64Array,
        Type::BigInt64,
        "BigInt64Array",
        8,
        "[1n, 2n, 3n]"
    );
    check_type!(
        cx,
        global,
        JS_NewBigUint64Array,
        JS_NewBigUint64ArrayFromArray,
        JS_NewBigUint64ArrayWithBuffer,
        UnwrapBigUint64Array,
        Type::BigUint64,
        "BigUint64Array",
        8,
        "[1n, 2n, 3n]"
    );

    // From another typed array, and from an iterable.
    rooted!(&in(cx) let src = eval_obj(cx, global, "new Uint8Array([7, 8])"));
    rooted!(&in(cx) let a = unsafe { JS_NewInt32ArrayFromArray(cx, src.handle()) });
    set_global(cx, global, c"a", a.get());
    assert_js(cx, global, "a instanceof Int32Array && a.join() === '7,8'");
    rooted!(&in(cx) let src = eval_obj(cx, global, "new Set([4, 5, 6])"));
    rooted!(&in(cx) let a = unsafe { JS_NewUint16ArrayFromArray(cx, src.handle()) });
    set_global(cx, global, c"a", a.get());
    assert_js(cx, global, "a.join() === '4,5,6'");

    // Plain objects and buffers are neither typed arrays nor views.
    rooted!(&in(cx) let o = eval_obj(cx, global, "({ length: 2 })"));
    rooted!(&in(cx) let buf = unsafe { NewArrayBuffer(cx, 4) });
    unsafe {
        assert!(!JS_IsArrayBufferViewObject(o.get()));
        assert!(!JS_IsTypedArrayObject(buf.get()));
        assert!(UnwrapArrayBufferView(buf.get()).is_null());
        assert!(UnwrapArrayBuffer(o.get()).is_null());
        assert_eq!(UnwrapArrayBuffer(buf.get()), buf.get());
        // JS_GetArrayBufferViewType has a checked contract: its input must be a view.
        // Passing an ArrayBuffer asserts in SpiderMonkey, so the DataView case below covers it.
    }

    // Creating a view over something that is not a buffer, or out of bounds, throws.
    rooted!(&in(cx) let bad = unsafe { JS_NewUint8ArrayWithBuffer(cx, o.handle(), 0, -1) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "TypeError");
    rooted!(&in(cx) let bad = unsafe { JS_NewUint32ArrayWithBuffer(cx, buf.handle(), 1, -1) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "RangeError");
    rooted!(&in(cx) let bad = unsafe { JS_NewUint8ArrayWithBuffer(cx, buf.handle(), 2, 3) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "RangeError");

    vec![
        ("typed_array_kinds", crate::support::Value::Number(11.0)),
        ("plain_is_not_view", crate::support::Value::Boolean(true)),
        (
            "buffer_is_not_typed_array",
            crate::support::Value::Boolean(true),
        ),
        (
            "invalid_view_rejected",
            crate::support::Value::Boolean(true),
        ),
    ]
}

fn views_with_offsets(
    cx: &mut JSContext,
    global: HandleObject,
) -> Vec<(&'static str, crate::support::Value)> {
    rooted!(&in(cx) let buf = eval_obj(cx, global, "var buf = new ArrayBuffer(16); new Uint8Array(buf).forEach((_, i, a) => a[i] = i); buf"));
    let mut shared = false;

    rooted!(&in(cx) let v = eval_obj(cx, global, "new Uint16Array(buf, 4, 3)"));
    unsafe {
        assert_eq!(JS_GetArrayBufferViewByteOffset(v.get()), 4);
        assert_eq!(JS_GetArrayBufferViewByteLength(v.get()), 6);
        assert_eq!(JS_GetTypedArrayLength(v.get()), 3);
    }
    rooted!(&in(cx) let vbuf = unsafe { js::rust::wrappers2::JS_GetArrayBufferViewBuffer(cx, v.handle(), &mut shared) });
    assert_eq!(vbuf.get(), buf.get());
    assert!(!shared);

    // DataViews are views but not typed arrays.
    rooted!(&in(cx) let dv = unsafe { JS_NewDataView(cx, buf.handle(), 2, 4) });
    assert!(!dv.get().is_null());
    unsafe {
        assert!(JS_IsArrayBufferViewObject(dv.get()));
        assert!(!JS_IsTypedArrayObject(dv.get()));
        assert_eq!(
            JS_GetArrayBufferViewType(dv.get()),
            Type::MaxTypedArrayViewType
        );
        assert_eq!(JS_GetArrayBufferViewByteOffset(dv.get()), 2);
        assert_eq!(JS_GetArrayBufferViewByteLength(dv.get()), 4);
    }
    set_global(cx, global, c"dv", dv.get());
    assert_js(cx, global, "dv instanceof DataView && dv.getUint8(0) === 2");

    rooted!(&in(cx) let dv2 = eval_obj(cx, global, "new DataView(buf, 12)"));
    unsafe {
        assert_eq!(JS_GetArrayBufferViewByteOffset(dv2.get()), 12);
        assert_eq!(JS_GetArrayBufferViewByteLength(dv2.get()), 4);
    }
    rooted!(&in(cx) let bad = unsafe { JS_NewDataView(cx, buf.handle(), 14, 4) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "RangeError");

    vec![
        ("view_byte_offset", crate::support::Value::Number(4.0)),
        ("view_byte_length", crate::support::Value::Number(6.0)),
        ("view_length", crate::support::Value::Number(3.0)),
        (
            "view_buffer_same",
            crate::support::Value::Boolean(vbuf.get() == buf.get()),
        ),
        ("view_buffer_shared", crate::support::Value::Boolean(shared)),
        ("data_view_offset", crate::support::Value::Number(12.0)),
        ("data_view_tail_length", crate::support::Value::Number(4.0)),
        (
            "invalid_data_view_rejected",
            crate::support::Value::Boolean(bad.get().is_null()),
        ),
    ]
}

fn detach(cx: &mut JSContext, global: HandleObject) -> Vec<(&'static str, crate::support::Value)> {
    rooted!(&in(cx) let d = eval_obj(cx, global, "var d = new ArrayBuffer(8); var dv = new Uint8Array(d, 2); d"));
    rooted!(&in(cx) let view = eval_obj(cx, global, "dv"));
    let mut defined = true;
    assert!(unsafe { HasDefinedArrayBufferDetachKey(cx, d.handle(), &mut defined) });
    assert!(!defined);
    assert!(unsafe { ArrayBufferHasData(d.get()) });

    assert!(unsafe { DetachArrayBuffer(cx, d.handle()) });
    unsafe {
        assert!(IsDetachedArrayBufferObject(d.get()));
        assert!(!ArrayBufferHasData(d.get()));
        assert_eq!(GetArrayBufferByteLength(d.get()), 0);
        let (mut len, mut shared, mut data) = (1, true, ptr::null_mut());
        GetArrayBufferLengthAndData(d.get(), &mut len, &mut shared, &mut data);
        assert_eq!((len, shared, data), (0, false, ptr::null_mut()));
        assert_eq!(JS_GetTypedArrayLength(view.get()), 0);
        assert_eq!(JS_GetArrayBufferViewByteOffset(view.get()), 0);
        GetArrayBufferViewLengthAndData(view.get(), &mut len, &mut shared, &mut data);
        assert_eq!(len, 0);
    }
    assert_js(
        cx,
        global,
        "d.byteLength === 0 && d.detached && dv.length === 0",
    );
    assert_eq!(Uint8Array::from(view.get()).unwrap().to_vec(), None);

    // Detaching again is a no-op; detaching a view throws.
    assert!(unsafe { DetachArrayBuffer(cx, d.handle()) });
    assert!(!unsafe { DetachArrayBuffer(cx, view.handle()) });
    crate::support::take_exception_named(cx, "TypeError");
    let view_detach_key_rejected =
        !unsafe { HasDefinedArrayBufferDetachKey(cx, view.handle(), &mut defined) };
    assert!(view_detach_key_rejected);
    crate::support::take_exception_named(cx, "TypeError");
    vec![
        ("detach_key_undefined", crate::support::Value::Boolean(true)),
        ("detached_flag", crate::support::Value::Boolean(true)),
        ("detached_byte_length", crate::support::Value::Number(0.0)),
        (
            "view_length_after_detach",
            crate::support::Value::Number(0.0),
        ),
        (
            "view_offset_after_detach",
            crate::support::Value::Number(0.0),
        ),
        ("double_detach_ok", crate::support::Value::Boolean(true)),
        (
            "view_detach_rejected",
            crate::support::Value::Boolean(view_detach_key_rejected),
        ),
    ]
}

fn clone_and_copy(
    cx: &mut JSContext,
    global: HandleObject,
) -> Vec<(&'static str, crate::support::Value)> {
    rooted!(&in(cx) let buf = eval_obj(cx, global, "var c = new Uint8Array([0, 1, 2, 3, 4, 5]); c.buffer"));
    rooted!(&in(cx) let clone = unsafe { ArrayBufferClone(cx, buf.handle(), 1, 3) });
    set_global(cx, global, c"cl", clone.get());
    assert_js(
        cx,
        global,
        "cl !== c.buffer && new Uint8Array(cl).join() === '1,2,3'",
    );
    rooted!(&in(cx) let bad = unsafe { ArrayBufferClone(cx, buf.handle(), 4, 3) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "RangeError");

    // Overlapping copy within one buffer.
    assert!(unsafe { ArrayBufferCopyData(cx, buf.handle(), 2, buf.handle(), 0, 4) });
    assert_js(cx, global, "c.join() === '0,1,0,1,2,3'");
    assert!(unsafe { ArrayBufferCopyData(cx, clone.handle(), 0, buf.handle(), 5, 1) });
    assert_js(cx, global, "new Uint8Array(cl).join() === '3,2,3'");
    assert!(!unsafe { ArrayBufferCopyData(cx, clone.handle(), 1, buf.handle(), 0, 3) });
    crate::support::take_exception_named(cx, "RangeError");

    rooted!(&in(cx) let copy = unsafe { CopyArrayBuffer(cx, buf.handle()) });
    set_global(cx, global, c"cp", copy.get());
    assert_js(
        cx,
        global,
        "cp !== c.buffer && new Uint8Array(cp).join() === c.join()",
    );
    assert!(unsafe { DetachArrayBuffer(cx, buf.handle()) });
    rooted!(&in(cx) let bad = unsafe { CopyArrayBuffer(cx, buf.handle()) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "TypeError");
    rooted!(&in(cx) let bad = unsafe { ArrayBufferClone(cx, buf.handle(), 0, 0) });
    assert!(bad.get().is_null());
    crate::support::take_exception_named(cx, "TypeError");
    vec![
        (
            "clone_values",
            crate::support::Value::String("1,2,3".to_owned()),
        ),
        (
            "overlap_copy_values",
            crate::support::Value::String("0,1,0,1,2,3".to_owned()),
        ),
        (
            "cross_copy_values",
            crate::support::Value::String("3,2,3".to_owned()),
        ),
        ("copy_distinct", crate::support::Value::Boolean(true)),
        (
            "detached_copy_rejected",
            crate::support::Value::Boolean(true),
        ),
    ]
}

/// Counts calls of an external buffer's free function.
struct Freed {
    calls: AtomicUsize,
    contents: AtomicPtr<c_void>,
}

impl Freed {
    fn new() -> Freed {
        Freed {
            calls: AtomicUsize::new(0),
            contents: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

unsafe extern "C" fn count_free(contents: *mut c_void, user_data: *mut c_void) {
    let freed = &*(user_data as *const Freed);
    freed.calls.fetch_add(1, Ordering::SeqCst);
    freed.contents.store(contents, Ordering::SeqCst);
}

fn new_external(cx: &mut JSContext, data: &mut [u8], freed: &Freed) -> *mut JSObject {
    let obj = unsafe {
        NewExternalArrayBuffer(
            cx,
            data.len(),
            data.as_mut_ptr() as *mut c_void,
            Some(count_free),
            freed as *const Freed as *mut c_void,
        )
    };
    assert!(!obj.is_null());
    obj
}

fn external_buffers(
    cx: &mut JSContext,
    global: HandleObject,
) -> Vec<(&'static str, crate::support::Value)> {
    // Freed once when the runtime is destroyed.
    let mut data = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let contents = data.as_mut_ptr() as *mut c_void;
    let freed = Freed::new();
    rooted!(&in(cx) let ext = new_external(cx, &mut data, &freed));
    let mut shared = true;
    assert_eq!(
        unsafe { GetArrayBufferData(ext.get(), &mut shared, ptr::null()) },
        contents as *mut u8
    );
    assert!(!shared);
    set_global(cx, global, c"ext", ext.get());
    assert_js(cx, global, "new Uint8Array(ext)[7] === 8");
    eval(cx, global, "new Uint8Array(ext)[0] = 42");
    assert_eq!(freed.calls(), 0);

    rooted!(&in(cx) let detached = new_external(cx, &mut data, &freed));
    assert!(unsafe { DetachArrayBuffer(cx, detached.handle()) });

    rooted!(&in(cx) let stolen_from = new_external(cx, &mut data, &freed));
    let stolen = unsafe { StealArrayBufferContents(cx, stolen_from.handle()) };
    assert!(!stolen.is_null() && stolen != contents);
    rooted!(&in(cx) let n = unsafe { NewArrayBufferWithContents(cx, 8, stolen) });
    set_global(cx, global, c"n", n.get());
    assert_js(
        cx,
        global,
        "new Uint8Array(n).join() === '42,2,3,4,5,6,7,8'",
    );

    rooted!(&in(cx) let user = unsafe { NewArrayBufferWithUserOwnedContents(cx, 8, data.as_mut_ptr() as *mut c_void) });
    set_global(cx, global, c"user", user.get());
    assert_js(cx, global, "new Uint8Array(user)[1] === 2");
    let user_second = crate::support::eval_observe(cx, global, "new Uint8Array(user)[1]");
    assert!(unsafe { DetachArrayBuffer(cx, user.handle()) });
    vec![
        (
            "external_first_and_last",
            crate::support::Value::String("42,8".to_owned()),
        ),
        ("external_shared", crate::support::Value::Boolean(shared)),
        (
            "stolen_non_null",
            crate::support::Value::Boolean(!stolen.is_null()),
        ),
        (
            "stolen_distinct",
            crate::support::Value::Boolean(stolen != contents),
        ),
        ("user_owned_second", user_second),
    ]
}

mod typed_array_contract {
    /* This Source Code Form is subject to the terms of the Mozilla Public
     * License, v. 2.0. If a copy of the MPL was not distributed with this
     * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

    #![allow(unsafe_op_in_unsafe_fn)]

    //! Additional edge-case scenarios for the engine contract.
    //!
    //! These cases do not replace the per-entry smoke cases. They add independent observations for
    //! values, error paths, ordering and object lifetime so a shim that merely returns "success"
    //! cannot pass by matching a boolean.

    use std::{ptr, slice};

    use js::glue::{
        GetFloat32ArrayLengthAndData, GetFloat64ArrayLengthAndData,
        GetUint8ClampedArrayLengthAndData,
    };
    use js::jsapi::{
        GetArrayBufferByteLength, GetArrayBufferData, GetArrayBufferViewLengthAndData,
        IsDetachedArrayBufferObject, JS_GetArrayBufferViewByteLength,
        JS_GetArrayBufferViewByteOffset, JS_GetTypedArrayByteLength, JS_GetTypedArrayLength,
        JS_GetTypedArraySharedness, JS_IsArrayBufferViewObject, JS_IsTypedArrayObject,
        UnwrapArrayBufferView, UnwrapFloat32Array, UnwrapFloat64Array, UnwrapUint8ClampedArray,
    };
    use js::jsval::ObjectValue;
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{
        DetachArrayBuffer, JS_GetArrayBufferViewBuffer, JS_NewFloat32ArrayWithBuffer,
        JS_NewFloat64Array, JS_NewFloat64ArrayWithBuffer, JS_NewUint8ArrayWithBuffer,
        JS_NewUint8ClampedArray, NewArrayBuffer, NewSharedArrayBuffer,
    };

    use crate::support::{Observation, Value, eval, record_case, set_global, take_exception_named};

    fn typed_arrays_zero_and_clamped(engine: &JSEngine) {
        record_case(
            engine,
            "edge.typed_arrays_zero_and_clamped",
            &[
                "JS_NewUint8ClampedArray",
                "UnwrapUint8ClampedArray",
                "GetUint8ClampedArrayLengthAndData",
                "JS_IsTypedArrayObject",
                "JS_IsArrayBufferViewObject",
                "JS_GetTypedArrayLength",
                "JS_GetTypedArrayByteLength",
                "UnwrapArrayBufferView",
                "JS_NewFloat64Array",
                "UnwrapFloat64Array",
                "GetFloat64ArrayLengthAndData",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let empty = JS_NewUint8ClampedArray(cx, 0));
                let mut empty_len = 1;
                let mut empty_shared = true;
                let mut empty_data = ptr::null_mut();
                GetUint8ClampedArrayLengthAndData(
                    empty.get(),
                    &mut empty_len,
                    &mut empty_shared,
                    &mut empty_data,
                );
                let empty_unwrap = UnwrapUint8ClampedArray(empty.get()) == empty.get();
                let empty_view_unwrap = UnwrapArrayBufferView(empty.get()) == empty.get();

                rooted!(&in(cx) let clamped = JS_NewUint8ClampedArray(cx, 4));
                set_global(cx, global, c"clamped", ObjectValue(clamped.get()));
                eval(
                    cx,
                    global,
                    "clamped[0] = 0.5; clamped[1] = 1.5; clamped[2] = 2.5; clamped[3] = -0.5;",
                );
                let mut clamped_len = 0;
                let mut clamped_shared = true;
                let mut clamped_data = ptr::null_mut();
                GetUint8ClampedArrayLengthAndData(
                    clamped.get(),
                    &mut clamped_len,
                    &mut clamped_shared,
                    &mut clamped_data,
                );
                let rounded = if clamped_data.is_null() {
                    Vec::new()
                } else {
                    slice::from_raw_parts(clamped_data, clamped_len).to_vec()
                };

                rooted!(&in(cx) let float64 = JS_NewFloat64Array(cx, 2));
                let float64_unwrap = UnwrapFloat64Array(float64.get()) == float64.get();
                let wrong_unwrap_f64 = UnwrapUint8ClampedArray(float64.get()).is_null();
                let wrong_unwrap_clamped = UnwrapFloat64Array(clamped.get()).is_null();
                let mut f64_len = 0;
                let mut f64_shared = true;
                let mut f64_data = ptr::null_mut();
                GetFloat64ArrayLengthAndData(
                    float64.get(),
                    &mut f64_len,
                    &mut f64_shared,
                    &mut f64_data,
                );

                assert_eq!(empty_len, 0);
                assert!(!empty_shared);
                assert!(!empty_data.is_null());
                assert!(empty_unwrap);
                assert!(empty_view_unwrap);
                assert_eq!(clamped_len, 4);
                assert_eq!(rounded, [0, 2, 2, 0]);
                assert!(float64_unwrap);
                assert!(wrong_unwrap_f64);
                assert!(wrong_unwrap_clamped);
                assert_eq!(f64_len, 2);
                assert!(!f64_data.is_null());
                assert!(JS_IsTypedArrayObject(clamped.get()));
                assert!(JS_IsArrayBufferViewObject(clamped.get()));
                assert_eq!(JS_GetTypedArrayLength(clamped.get()), 4);
                assert_eq!(JS_GetTypedArrayByteLength(clamped.get()), 4);

                Observation::new(vec![
                    ("empty_len", Value::Number(empty_len as f64)),
                    ("empty_shared", Value::Boolean(empty_shared)),
                    ("empty_data_null", Value::Boolean(empty_data.is_null())),
                    ("empty_unwrap", Value::Boolean(empty_unwrap)),
                    ("empty_view_unwrap", Value::Boolean(empty_view_unwrap)),
                    ("clamped_len", Value::Number(clamped_len as f64)),
                    (
                        "clamped_rounded",
                        Value::Array(
                            rounded
                                .into_iter()
                                .map(|v| Value::Number(v as f64))
                                .collect(),
                        ),
                    ),
                    ("float64_unwrap", Value::Boolean(float64_unwrap)),
                    ("wrong_unwrap_f64", Value::Boolean(wrong_unwrap_f64)),
                    ("wrong_unwrap_clamped", Value::Boolean(wrong_unwrap_clamped)),
                    ("f64_len", Value::Number(f64_len as f64)),
                    ("f64_data_null", Value::Boolean(f64_data.is_null())),
                    (
                        "typed",
                        Value::Boolean(JS_IsTypedArrayObject(clamped.get())),
                    ),
                    (
                        "view",
                        Value::Boolean(JS_IsArrayBufferViewObject(clamped.get())),
                    ),
                    (
                        "typed_len",
                        Value::Number(JS_GetTypedArrayLength(clamped.get()) as f64),
                    ),
                    (
                        "typed_byte_len",
                        Value::Number(JS_GetTypedArrayByteLength(clamped.get()) as f64),
                    ),
                ])
            },
        );
    }

    fn typed_arrays_buffer(engine: &JSEngine) {
        record_case(
            engine,
            "edge.typed_arrays_buffer",
            &[
                "NewArrayBuffer",
                "GetArrayBufferData",
                "JS_NewFloat32ArrayWithBuffer",
                "JS_NewFloat64ArrayWithBuffer",
                "GetFloat32ArrayLengthAndData",
                "GetFloat64ArrayLengthAndData",
                "GetArrayBufferViewLengthAndData",
                "JS_GetArrayBufferViewBuffer",
                "JS_GetArrayBufferViewByteOffset",
                "JS_GetArrayBufferViewByteLength",
                "JS_GetTypedArrayLength",
                "JS_GetTypedArraySharedness",
                "UnwrapFloat32Array",
                "UnwrapFloat64Array",
                "UnwrapArrayBufferView",
                "JS_IsExceptionPending",
                "JS_ClearPendingException",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let buffer = NewArrayBuffer(cx, 32));
                let mut shared = true;
                let base = GetArrayBufferData(buffer.get(), &mut shared, ptr::null());
                assert!(!base.is_null());

                rooted!(&in(cx) let f64 = JS_NewFloat64ArrayWithBuffer(
                    cx,
                    buffer.handle(),
                    8,
                    2,
                ));
                let mut f64_len = 0;
                let mut f64_shared = true;
                let mut f64_data = ptr::null_mut();
                GetFloat64ArrayLengthAndData(
                    f64.get(),
                    &mut f64_len,
                    &mut f64_shared,
                    &mut f64_data,
                );
                let f64_offset = JS_GetArrayBufferViewByteOffset(f64.get());
                let f64_byte_len = JS_GetArrayBufferViewByteLength(f64.get());
                let mut view_shared = true;
                let view_buffer = JS_GetArrayBufferViewBuffer(cx, f64.handle(), &mut view_shared);
                let view_buffer_identity = view_buffer == buffer.get();

                rooted!(&in(cx) let f32 = JS_NewFloat32ArrayWithBuffer(
                    cx,
                    buffer.handle(),
                    16,
                    3,
                ));
                set_global(cx, global, c"f32", ObjectValue(f32.get()));
                eval(cx, global, "f32[0] = 1.5; f32[1] = -2.25; f32[2] = 0.5;");
                let mut f32_len = 0;
                let mut f32_shared = true;
                let mut f32_data = ptr::null_mut();
                GetFloat32ArrayLengthAndData(
                    f32.get(),
                    &mut f32_len,
                    &mut f32_shared,
                    &mut f32_data,
                );
                let f32_bits = if f32_data.is_null() {
                    Vec::new()
                } else {
                    slice::from_raw_parts(f32_data, f32_len)
                        .iter()
                        .map(|v| v.to_bits() as f64)
                        .collect()
                };
                let f32_expected = [1.5f32, -2.25f32, 0.5f32];

                rooted!(&in(cx) let exact_end = JS_NewFloat64ArrayWithBuffer(
                    cx,
                    buffer.handle(),
                    32,
                    0,
                ));
                let exact_end_len = JS_GetTypedArrayLength(exact_end.get());

                rooted!(&in(cx) let oob = JS_NewFloat64ArrayWithBuffer(
                    cx,
                    buffer.handle(),
                    0,
                    5,
                ));
                let oob_null = oob.get().is_null();
                take_exception_named(cx, "RangeError");
                let oob_pending = true;

                rooted!(&in(cx) let unaligned = JS_NewFloat64ArrayWithBuffer(
                    cx,
                    buffer.handle(),
                    4,
                    1,
                ));
                let unaligned_null = unaligned.get().is_null();
                take_exception_named(cx, "RangeError");
                let unaligned_pending = true;

                let mut generic_byte_len = 0;
                let mut generic_shared = true;
                let mut generic_data = ptr::null_mut();
                GetArrayBufferViewLengthAndData(
                    f64.get(),
                    &mut generic_byte_len,
                    &mut generic_shared,
                    &mut generic_data,
                );
                let generic_offset = if base.is_null() || generic_data.is_null() {
                    usize::MAX
                } else {
                    generic_data.offset_from(base) as usize
                };
                let f64_alias = UnwrapFloat64Array(f64.get()) == f64.get();
                let f32_alias = UnwrapFloat32Array(f32.get()) == f32.get();
                let view_alias = UnwrapArrayBufferView(f64.get()) == f64.get();
                let f64_sharedness = JS_GetTypedArraySharedness(f64.get());
                assert_eq!(
                    f64_len, 2,
                    "GetFloat64ArrayLengthAndData must report the element count"
                );
                assert!(!f64_shared);
                assert!(!f64_data.is_null());
                assert_eq!(
                    f64_offset, 8,
                    "JS_GetArrayBufferViewByteOffset must expose the offset"
                );
                assert_eq!(
                    f64_byte_len, 16,
                    "JS_GetArrayBufferViewByteLength must expose the byte length"
                );
                assert!(
                    view_buffer_identity,
                    "JS_GetArrayBufferViewBuffer must return the source buffer"
                );
                assert_eq!(
                    f32_len, 3,
                    "GetFloat32ArrayLengthAndData must report the element count"
                );
                assert!(!f32_shared);
                assert_eq!(
                    f32_bits,
                    f32_expected
                        .iter()
                        .map(|v| v.to_bits() as f64)
                        .collect::<Vec<_>>(),
                    "GetFloat32ArrayLengthAndData must expose the written values"
                );
                assert_eq!(
                    exact_end_len, 0,
                    "a zero-length view at the buffer end is valid"
                );
                assert!(oob_null, "a view past the end must fail");
                assert!(
                    oob_pending,
                    "a rejected view must leave a pending exception"
                );
                assert!(unaligned_null, "an unaligned Float64Array must fail");
                assert!(
                    unaligned_pending,
                    "an unaligned view must leave a pending exception"
                );
                assert_eq!(
                    generic_byte_len, 16,
                    "GetArrayBufferViewLengthAndData must report the view byte length"
                );
                assert!(!generic_shared);
                assert_eq!(
                    generic_offset, 8,
                    "generic view data must start at the f64 offset"
                );
                assert!(f64_alias && f32_alias && view_alias);
                assert!(
                    !f64_sharedness,
                    "an ArrayBuffer-backed view must report non-shared"
                );

                Observation::new(vec![
                    ("f64_len", Value::Number(f64_len as f64)),
                    ("f64_shared", Value::Boolean(f64_shared)),
                    ("f64_offset", Value::Number(f64_offset as f64)),
                    ("f64_byte_len", Value::Number(f64_byte_len as f64)),
                    ("view_buffer_identity", Value::Boolean(view_buffer_identity)),
                    ("f32_len", Value::Number(f32_len as f64)),
                    ("f32_shared", Value::Boolean(f32_shared)),
                    (
                        "f32_bits",
                        Value::Array(f32_bits.iter().map(|v| Value::Number(*v)).collect()),
                    ),
                    (
                        "f32_expected",
                        Value::Array(
                            f32_expected
                                .iter()
                                .map(|v| Value::Number(v.to_bits() as f64))
                                .collect(),
                        ),
                    ),
                    ("exact_end_len", Value::Number(exact_end_len as f64)),
                    ("oob_null", Value::Boolean(oob_null)),
                    ("oob_pending", Value::Boolean(oob_pending)),
                    ("unaligned_null", Value::Boolean(unaligned_null)),
                    ("unaligned_pending", Value::Boolean(unaligned_pending)),
                    ("generic_byte_len", Value::Number(generic_byte_len as f64)),
                    ("generic_shared", Value::Boolean(generic_shared)),
                    ("generic_offset", Value::Number(generic_offset as f64)),
                    ("f64_alias", Value::Boolean(f64_alias)),
                    ("f32_alias", Value::Boolean(f32_alias)),
                    ("view_alias", Value::Boolean(view_alias)),
                    ("f64_sharedness", Value::Boolean(f64_sharedness)),
                ])
            },
        );
    }

    fn typed_arrays_shared_detach(engine: &JSEngine) {
        record_case(
            engine,
            "edge.typed_arrays_shared_detach",
            &[
                "NewArrayBuffer",
                "JS_NewUint8ArrayWithBuffer",
                "JS_GetTypedArraySharedness",
                "GetArrayBufferViewLengthAndData",
                "DetachArrayBuffer",
                "IsDetachedArrayBufferObject",
                "GetArrayBufferByteLength",
                "JS_GetArrayBufferViewByteLength",
                "JS_GetTypedArrayLength",
                "GetFloat64ArrayLengthAndData",
                "JS_NewFloat64ArrayWithBuffer",
                "UnwrapArrayBufferView",
            ],
            |cx, _global| unsafe {
                rooted!(&in(cx) let shared_buffer = NewSharedArrayBuffer(cx, 16));
                let shared_available = !shared_buffer.get().is_null();
                let (sharedness, shared_len, is_shared, shared_data_null, shared_alias) =
                    if shared_available {
                        rooted!(&in(cx) let shared_view =
                        JS_NewUint8ArrayWithBuffer(cx, shared_buffer.handle(), 0, 4));
                        let mut shared_len = 0;
                        let mut is_shared = false;
                        let mut shared_data = ptr::null_mut();
                        GetArrayBufferViewLengthAndData(
                            shared_view.get(),
                            &mut shared_len,
                            &mut is_shared,
                            &mut shared_data,
                        );
                        (
                            JS_GetTypedArraySharedness(shared_view.get()),
                            shared_len,
                            is_shared,
                            shared_data.is_null(),
                            UnwrapArrayBufferView(shared_view.get()) == shared_view.get(),
                        )
                    } else {
                        (false, 0, false, true, false)
                    };

                rooted!(&in(cx) let buffer = NewArrayBuffer(cx, 16));
                rooted!(&in(cx) let view =
                JS_NewFloat64ArrayWithBuffer(cx, buffer.handle(), 0, 2));
                let detached_before = IsDetachedArrayBufferObject(buffer.get());
                assert!(DetachArrayBuffer(cx, buffer.handle()));
                let detached_after = IsDetachedArrayBufferObject(buffer.get());
                let byte_length_after = GetArrayBufferByteLength(buffer.get());
                let view_length_after = JS_GetTypedArrayLength(view.get());
                let view_byte_length_after = JS_GetArrayBufferViewByteLength(view.get());
                assert!(!detached_before);
                assert!(detached_after);
                assert_eq!(byte_length_after, 0);
                assert_eq!(view_length_after, 0);
                assert_eq!(view_byte_length_after, 0);
                let mut detached_len = 1;
                let mut detached_shared = true;
                let mut detached_data = ptr::null_mut();
                GetFloat64ArrayLengthAndData(
                    view.get(),
                    &mut detached_len,
                    &mut detached_shared,
                    &mut detached_data,
                );
                let detached_alias = UnwrapArrayBufferView(view.get()) == view.get();
                assert_eq!(detached_len, 0);
                assert!(detached_data.is_null());
                assert!(detached_alias);

                Observation::new(vec![
                    ("shared_available", Value::Boolean(shared_available)),
                    ("sharedness", Value::Boolean(sharedness)),
                    ("shared_len", Value::Number(shared_len as f64)),
                    ("shared_flag", Value::Boolean(is_shared)),
                    ("shared_data_null", Value::Boolean(shared_data_null)),
                    ("shared_alias", Value::Boolean(shared_alias)),
                    ("detached_before", Value::Boolean(detached_before)),
                    ("detached_after", Value::Boolean(detached_after)),
                    ("byte_length_after", Value::Number(byte_length_after as f64)),
                    ("view_length_after", Value::Number(view_length_after as f64)),
                    (
                        "view_byte_length_after",
                        Value::Number(view_byte_length_after as f64),
                    ),
                    ("detached_len", Value::Number(detached_len as f64)),
                    ("detached_shared", Value::Boolean(detached_shared)),
                    (
                        "detached_data_null",
                        Value::Boolean(detached_data.is_null()),
                    ),
                    ("detached_alias", Value::Boolean(detached_alias)),
                ])
            },
        );
    }

    pub fn run(engine: &JSEngine) {
        typed_arrays_zero_and_clamped(engine);
        typed_arrays_buffer(engine);
        typed_arrays_shared_detach(engine);
    }
}

mod edge_buffer_more_contract {
    use std::ptr;

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::glue::GetUint8ArrayLengthAndData;
    use js::jsapi::mozilla::Range;
    use js::jsapi::{
        GetArrayBufferByteLength, GetArrayBufferData, GetArrayBufferLengthAndData,
        IsArrayBufferObject, IsDetachedArrayBufferObject, JS_GetArrayBufferViewByteLength,
        JS_GetArrayBufferViewByteOffset, JS_GetArrayBufferViewType, JS_GetTypedArrayByteLength,
        JS_GetTypedArrayLength, JS_IsArrayBufferViewObject, JS_IsTypedArrayObject,
        UnwrapArrayBuffer, UnwrapInt32Array, UnwrapUint8Array, UnwrapUint32Array,
    };
    use js::jsval::{JSVal, ObjectValue};
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{
        ArrayBufferClone, ArrayBufferCopyData, CopyArrayBuffer, DetachArrayBuffer,
        HasDefinedArrayBufferDetachKey, JS_GetArrayBufferViewBuffer, JS_NewDataView,
        JS_NewFloat32ArrayWithBuffer, JS_NewFloat64ArrayWithBuffer, JS_NewInt8ArrayWithBuffer,
        JS_NewInt16ArrayWithBuffer, JS_NewInt32Array, JS_NewInt32ArrayWithBuffer, JS_NewUint8Array,
        JS_NewUint8ArrayWithBuffer, JS_NewUint8ClampedArrayWithBuffer, JS_NewUint16ArrayWithBuffer,
        JS_NewUint32Array, JS_NewUint32ArrayWithBuffer, NewArrayBuffer, NewArrayBufferWithContents,
        StealArrayBufferContents,
    };

    use crate::support::{Observation, Value, eval, record_case, set_global, take_exception_named};

    fn typed_array_matrix(engine: &JSEngine) {
        record_case(
            engine,
            "edge.typed_array_matrix",
            &[
                "NewArrayBuffer",
                "GetArrayBufferLengthAndData",
                "GetArrayBufferData",
                "IsArrayBufferObject",
                "JS_NewInt8ArrayWithBuffer",
                "JS_NewUint8ArrayWithBuffer",
                "JS_NewUint8ClampedArrayWithBuffer",
                "JS_NewInt16ArrayWithBuffer",
                "JS_NewUint16ArrayWithBuffer",
                "JS_NewInt32ArrayWithBuffer",
                "JS_NewUint32ArrayWithBuffer",
                "JS_NewFloat32ArrayWithBuffer",
                "JS_NewFloat64ArrayWithBuffer",
                "JS_NewInt32Array",
                "JS_NewUint32Array",
                "JS_NewUint8Array",
                "UnwrapInt32Array",
                "UnwrapUint32Array",
                "UnwrapUint8Array",
                "UnwrapArrayBuffer",
                "JS_GetArrayBufferViewType",
                "JS_GetTypedArrayLength",
                "JS_GetTypedArrayByteLength",
                "JS_GetArrayBufferViewByteOffset",
                "JS_GetArrayBufferViewByteLength",
                "JS_IsArrayBufferViewObject",
                "JS_IsTypedArrayObject",
                "JS_GetArrayBufferViewBuffer",
                "GetUint8ArrayLengthAndData",
                "JS_NewDataView",
                "HasDefinedArrayBufferDetachKey",
            ],
            |cx, _global| unsafe {
                rooted!(&in(cx) let buffer = NewArrayBuffer(cx, 64));
                let mut byte_len = 0;
                let mut shared = true;
                let mut data = ptr::null_mut();
                GetArrayBufferLengthAndData(buffer.get(), &mut byte_len, &mut shared, &mut data);
                let is_buffer = IsArrayBufferObject(buffer.get());
                let mut detach_key = true;
                let has_key =
                    HasDefinedArrayBufferDetachKey(cx, buffer.handle().into(), &mut detach_key);

                let mut details = Vec::new();
                macro_rules! detail {
                ($new:ident, $offset:expr, $len:expr, $typename:literal) => {{
                    let view = $new(cx, buffer.handle(), $offset, $len);
                    rooted!(&in(cx) let view = view);
                    let mut back_shared = true;
                    let back = JS_GetArrayBufferViewBuffer(cx, view.handle(), &mut back_shared);
                    details.push(Value::String(format!(
                        "{}:{}:{}:{}:{}:{}:{}:{}",
                        $typename,
                        JS_GetArrayBufferViewType(view.get()) as i32,
                        JS_GetTypedArrayLength(view.get()),
                        JS_GetTypedArrayByteLength(view.get()),
                        JS_GetArrayBufferViewByteOffset(view.get()),
                        JS_GetArrayBufferViewByteLength(view.get()),
                        back == buffer.get() && !back_shared,
                        JS_IsTypedArrayObject(view.get()) && JS_IsArrayBufferViewObject(view.get()),
                    )));
                }};
            }
                detail!(JS_NewInt8ArrayWithBuffer, 1, 3, "i8");
                detail!(JS_NewUint8ArrayWithBuffer, 2, 3, "u8");
                detail!(JS_NewUint8ClampedArrayWithBuffer, 3, 3, "u8c");
                detail!(JS_NewInt16ArrayWithBuffer, 4, 3, "i16");
                detail!(JS_NewUint16ArrayWithBuffer, 8, 3, "u16");
                detail!(JS_NewInt32ArrayWithBuffer, 12, 3, "i32");
                detail!(JS_NewUint32ArrayWithBuffer, 16, 3, "u32");
                detail!(JS_NewFloat32ArrayWithBuffer, 20, 3, "f32");
                detail!(JS_NewFloat64ArrayWithBuffer, 32, 3, "f64");

                rooted!(&in(cx) let int32 = JS_NewInt32Array(cx, 2));
                rooted!(&in(cx) let uint32 = JS_NewUint32Array(cx, 2));
                rooted!(&in(cx) let uint8 = JS_NewUint8Array(cx, 2));
                let unwraps = [
                    UnwrapInt32Array(int32.get()) == int32.get(),
                    UnwrapUint32Array(uint32.get()) == uint32.get(),
                    UnwrapUint8Array(uint8.get()) == uint8.get(),
                    UnwrapUint8Array(int32.get()).is_null(),
                    UnwrapArrayBuffer(buffer.get()) == buffer.get(),
                    UnwrapArrayBuffer(int32.get()).is_null(),
                ];
                let mut u8_len = 0;
                let mut u8_shared = true;
                let mut u8_data = ptr::null_mut();
                GetUint8ArrayLengthAndData(uint8.get(), &mut u8_len, &mut u8_shared, &mut u8_data);

                rooted!(&in(cx) let dataview = JS_NewDataView(cx, buffer.handle().into(), 4, 8));
                let dataview_is_typed = JS_IsTypedArrayObject(dataview.get());
                let dataview_is_view = JS_IsArrayBufferViewObject(dataview.get());

                assert_eq!(byte_len, 64);
                assert!(!shared);
                assert!(!data.is_null());
                assert!(is_buffer);
                assert!(has_key);
                assert!(!detach_key);
                assert_eq!(
                    details,
                    [
                        "i8:0:3:3:1:3:true:true",
                        "u8:1:3:3:2:3:true:true",
                        "u8c:8:3:3:3:3:true:true",
                        "i16:2:3:6:4:6:true:true",
                        "u16:3:3:6:8:6:true:true",
                        "i32:4:3:12:12:12:true:true",
                        "u32:5:3:12:16:12:true:true",
                        "f32:6:3:12:20:12:true:true",
                        "f64:7:3:24:32:24:true:true",
                    ]
                    .into_iter()
                    .map(|detail| Value::String(detail.to_owned()))
                    .collect::<Vec<_>>()
                );
                assert_eq!(unwraps, [true, true, true, true, true, true]);
                assert_eq!(u8_len, 2);
                assert!(!u8_shared);
                assert!(!u8_data.is_null());
                assert_eq!(JS_GetArrayBufferViewType(dataview.get()) as i32, 12);
                assert!(!dataview_is_typed);
                assert!(dataview_is_view);

                Observation::new(vec![
                    ("byte_len", Value::Number(byte_len as f64)),
                    ("shared", Value::Boolean(shared)),
                    ("data_null", Value::Boolean(data.is_null())),
                    ("is_buffer", Value::Boolean(is_buffer)),
                    ("has_key", Value::Boolean(has_key)),
                    ("detach_key", Value::Boolean(detach_key)),
                    ("views", Value::Array(details)),
                    (
                        "unwraps",
                        Value::Array(unwraps.iter().map(|v| Value::Boolean(*v)).collect()),
                    ),
                    ("u8_len", Value::Number(u8_len as f64)),
                    ("u8_shared", Value::Boolean(u8_shared)),
                    ("u8_data_null", Value::Boolean(u8_data.is_null())),
                    (
                        "dataview_type",
                        Value::Number(JS_GetArrayBufferViewType(dataview.get()) as i32 as f64),
                    ),
                    ("dataview_is_typed", Value::Boolean(dataview_is_typed)),
                    ("dataview_is_view", Value::Boolean(dataview_is_view)),
                ])
            },
        );
    }

    fn buffer_roundtrip(engine: &JSEngine) {
        record_case(
            engine,
            "edge.buffer_copy_and_external",
            &[
                "NewArrayBuffer",
                "GetArrayBufferData",
                "ArrayBufferClone",
                "ArrayBufferCopyData",
                "CopyArrayBuffer",
                "NewArrayBufferWithContents",
                "StealArrayBufferContents",
                "DetachArrayBuffer",
                "GetArrayBufferByteLength",
                "IsDetachedArrayBufferObject",
                "JS_NewUint8ArrayWithBuffer",
                "JS_IsExceptionPending",
                "JS_ClearPendingException",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let source = NewArrayBuffer(cx, 8));
                let mut shared = true;
                let base = GetArrayBufferData(source.get(), &mut shared, ptr::null());
                assert!(!base.is_null());
                for i in 0..8 {
                    *base.add(i) = i as u8;
                }
                rooted!(&in(cx) let source_view = JS_NewUint8ArrayWithBuffer(cx, source.handle(), 0, 8));
                rooted!(&in(cx) let clone = ArrayBufferClone(cx, source.handle(), 1, 4));
                set_global(cx, global, c"clone", ObjectValue(clone.get()));
                let clone_value = eval(cx, global, "Array.from(new Uint8Array(clone)).join(',')");
                let clone_text = string_of(cx, clone_value);
                assert_eq!(
                    clone_text, "1,2,3,4",
                    "ArrayBufferClone must copy the requested range"
                );

                let copy_in_place =
                    ArrayBufferCopyData(cx, source.handle(), 2, source.handle(), 0, 4);
                assert!(
                    copy_in_place,
                    "ArrayBufferCopyData must copy within one buffer"
                );
                set_global(cx, global, c"sourceView", ObjectValue(source_view.get()));
                let in_place_value = eval(cx, global, "Array.from(sourceView).join(',')");
                let in_place = string_of(cx, in_place_value);
                assert_eq!(
                    in_place, "0,1,0,1,2,3,6,7",
                    "in-place copy must be observable"
                );

                let cross_copy = ArrayBufferCopyData(cx, clone.handle(), 0, source.handle(), 4, 4);
                assert!(cross_copy, "ArrayBufferCopyData must copy across buffers");
                set_global(cx, global, c"cloneAfter", ObjectValue(clone.get()));
                let clone_after_value = eval(
                    cx,
                    global,
                    "Array.from(new Uint8Array(cloneAfter)).join(',')",
                );
                let clone_after = string_of(cx, clone_after_value);
                assert_eq!(
                    clone_after, "2,3,6,7",
                    "cross-buffer copy must update the destination"
                );

                let bad_copy = ArrayBufferCopyData(cx, clone.handle(), 4, source.handle(), 0, 8);
                assert!(
                    !bad_copy,
                    "ArrayBufferCopyData must reject an out-of-range copy"
                );
                take_exception_named(cx, "RangeError");
                let bad_pending = true;

                rooted!(&in(cx) let copy = CopyArrayBuffer(cx, source.handle()));
                set_global(cx, global, c"copy", ObjectValue(copy.get()));
                let copy_value = eval(cx, global, "Array.from(new Uint8Array(copy)).join(',')");
                let copy_text = string_of(cx, copy_value);
                let copy_distinct = copy.get() != source.get();
                assert_eq!(
                    copy_text, "0,1,0,1,2,3,6,7",
                    "CopyArrayBuffer must copy the current bytes"
                );
                assert!(
                    copy_distinct,
                    "CopyArrayBuffer must return a distinct object"
                );

                let detached = DetachArrayBuffer(cx, source.handle());
                assert!(detached, "DetachArrayBuffer must succeed");
                let detached_flag = IsDetachedArrayBufferObject(source.get());
                let detached_len = GetArrayBufferByteLength(source.get());
                assert!(detached_flag, "a detached buffer must report detached");
                assert_eq!(
                    detached_len, 0,
                    "a detached buffer must have zero byte length"
                );
                let bad_clone = ArrayBufferClone(cx, source.handle(), 0, 1);
                let bad_clone_null = bad_clone.is_null();
                assert!(
                    bad_clone_null,
                    "ArrayBufferClone must fail for a detached source"
                );
                take_exception_named(cx, "TypeError");
                let bad_clone_pending = true;

                let detached_copy = CopyArrayBuffer(cx, source.handle());
                assert!(
                    detached_copy.is_null(),
                    "CopyArrayBuffer must reject a detached source"
                );
                take_exception_named(cx, "TypeError");

                rooted!(&in(cx) let steal_source = NewArrayBuffer(cx, 4));
                let mut steal_shared = true;
                let steal_base =
                    GetArrayBufferData(steal_source.get(), &mut steal_shared, ptr::null());
                for (offset, byte) in [9u8, 8, 7, 6].iter().enumerate() {
                    *steal_base.add(offset) = *byte;
                }
                let stolen = StealArrayBufferContents(cx, steal_source.handle());
                let stolen_non_null = !stolen.is_null();
                assert!(
                    stolen_non_null,
                    "StealArrayBufferContents must return the data pointer"
                );
                let stolen_buffer = if stolen_non_null {
                    NewArrayBufferWithContents(cx, 4, stolen)
                } else {
                    ptr::null_mut()
                };
                rooted!(&in(cx) let stolen_buffer = stolen_buffer);
                set_global(cx, global, c"stolen", ObjectValue(stolen_buffer.get()));
                let stolen_text = if stolen_buffer.get().is_null() {
                    String::new()
                } else {
                    let stolen_value =
                        eval(cx, global, "Array.from(new Uint8Array(stolen)).join(',')");
                    string_of(cx, stolen_value)
                };

                Observation::new(vec![
                    ("base_non_null", Value::Boolean(!base.is_null())),
                    ("shared", Value::Boolean(shared)),
                    ("clone", Value::String(clone_text)),
                    ("copy_in_place", Value::Boolean(copy_in_place)),
                    ("in_place", Value::String(in_place)),
                    ("cross_copy", Value::Boolean(cross_copy)),
                    ("clone_after", Value::String(clone_after)),
                    ("bad_copy", Value::Boolean(bad_copy)),
                    ("bad_pending", Value::Boolean(bad_pending)),
                    ("copy_distinct", Value::Boolean(copy_distinct)),
                    ("copy", Value::String(copy_text)),
                    ("detached", Value::Boolean(detached)),
                    ("detached_flag", Value::Boolean(detached_flag)),
                    ("detached_len", Value::Number(detached_len as f64)),
                    ("bad_clone_null", Value::Boolean(bad_clone_null)),
                    ("bad_clone_pending", Value::Boolean(bad_clone_pending)),
                    ("steal_shared", Value::Boolean(steal_shared)),
                    ("stolen_non_null", Value::Boolean(stolen_non_null)),
                    (
                        "stolen_buffer_null",
                        Value::Boolean(stolen_buffer.get().is_null()),
                    ),
                    ("stolen", Value::String(stolen_text)),
                ])
            },
        );
    }
    fn range<T>(ptr: *mut T, len: usize) -> Range<T> {
        unsafe {
            let mut range: Range<T> = std::mem::zeroed();
            range.mStart.mPtr = ptr;
            range.mEnd.mPtr = ptr.add(len);
            range
        }
    }

    fn string_of(cx: &mut JSContext, value: JSVal) -> String {
        assert!(value.is_string());
        unsafe {
            jsstr_to_string(
                cx,
                std::ptr::NonNull::new(value.to_string()).expect("non-null string"),
            )
        }
    }

    pub fn run(engine: &JSEngine) {
        typed_array_matrix(engine);
        buffer_roundtrip(engine);
    }
}

mod edge_buffer_deep_contract {
    fn string_of(cx: &mut JSContext, value: JSVal) -> String {
        assert!(value.is_string(), "expected a string value");
        unsafe {
            jsstr_to_string(
                cx,
                std::ptr::NonNull::new(value.to_string()).expect("non-null string"),
            )
        }
    }

    fn object_of(cx: &mut JSContext, global: HandleObject, source: &str) -> *mut JSObject {
        eval(cx, global, source).to_object()
    }

    fn buffer_extra_arrays(engine: &JSEngine) {
        record_case(
            engine,
            "edge.buffer.extra_arrays",
            &[
                "GetInt32ArrayLengthAndData",
                "GetUint32ArrayLengthAndData",
                "JS_GetArrayBufferViewType",
                "JS_GetTypedArrayByteLength",
                "JS_GetTypedArrayLength",
                "JS_NewBigInt64ArrayWithBuffer",
                "JS_NewBigUint64ArrayWithBuffer",
                "JS_NewFloat16ArrayFromArray",
                "JS_NewFloat16ArrayWithBuffer",
                "JS_NewFloat32Array",
                "JS_NewInt32ArrayWithBuffer",
                "JS_NewUint32ArrayWithBuffer",
                "NewExternalArrayBuffer",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let buffer = js::rust::wrappers2::NewArrayBuffer(cx, 64));
                assert!(!buffer.get().is_null());

                rooted!(&in(cx) let f16 = JS_NewFloat16ArrayWithBuffer(cx, buffer.handle(), 0, 4));
                assert!(!f16.get().is_null());
                set_global(cx, global, c"deepF16", ObjectValue(f16.get()));
                eval(
                    cx,
                    global,
                    "deepF16[0] = 1.5; deepF16[1] = -2; deepF16[2] = 0.5; deepF16[3] = 4;",
                );
                let f16_value = eval(cx, global, "Array.from(deepF16).join(',')");
                let f16_values = string_of(cx, f16_value);
                let f16_length = JS_GetTypedArrayLength(f16.get());
                let f16_bytes = JS_GetTypedArrayByteLength(f16.get());
                let f16_type = JS_GetArrayBufferViewType(f16.get());
                assert_eq!(
                    f16_values, "1.5,-2,0.5,4",
                    "Float16Array values must round-trip"
                );
                assert_eq!(
                    f16_length, 4,
                    "JS_NewFloat16ArrayWithBuffer must honor its length"
                );
                assert_eq!(f16_bytes, 8, "Float16Array element size must be two bytes");
                assert_eq!(
                    f16_type as i32, 11,
                    "Float16Array must report its view type"
                );

                let array_source = object_of(cx, global, "[1.5, -2, 0.5, 4]");
                rooted!(&in(cx) let array_source = array_source);
                rooted!(&in(cx) let f16_from_array = JS_NewFloat16ArrayFromArray(cx, array_source.handle()));
                set_global(
                    cx,
                    global,
                    c"deepF16FromArray",
                    ObjectValue(f16_from_array.get()),
                );
                let f16_array_value = eval(cx, global, "Array.from(deepF16FromArray).join(',')");
                let f16_array_values = string_of(cx, f16_array_value);
                assert_eq!(
                    f16_array_values, "1.5,-2,0.5,4",
                    "JS_NewFloat16ArrayFromArray must copy every element"
                );

                rooted!(&in(cx) let f32 = JS_NewFloat32Array(cx, 3));
                assert!(!f32.get().is_null());
                set_global(cx, global, c"deepF32", ObjectValue(f32.get()));
                eval(
                    cx,
                    global,
                    "deepF32[0] = 1.25; deepF32[1] = -3.5; deepF32[2] = 9;",
                );
                let f32_value = eval(cx, global, "Array.from(deepF32).join(',')");
                let f32_values = string_of(cx, f32_value);
                let f32_type = JS_GetArrayBufferViewType(f32.get());
                assert_eq!(
                    f32_values, "1.25,-3.5,9",
                    "JS_NewFloat32Array values must round-trip"
                );
                assert_eq!(f32_type as i32, 6, "Float32Array must report its view type");

                rooted!(&in(cx) let big = JS_NewBigInt64ArrayWithBuffer(cx, buffer.handle(), 16, 2));
                rooted!(&in(cx) let ubig = JS_NewBigUint64ArrayWithBuffer(cx, buffer.handle(), 32, 2));
                assert!(!big.get().is_null() && !ubig.get().is_null());
                set_global(cx, global, c"deepBig", ObjectValue(big.get()));
                set_global(cx, global, c"deepUbig", ObjectValue(ubig.get()));
                eval(
                    cx,
                    global,
                    "deepBig[0] = 5n; deepBig[1] = -6n; deepUbig[0] = 7n; deepUbig[1] = 8n;",
                );
                let big_value = eval(cx, global, "Array.from(deepBig).join(',')");
                let big_values = string_of(cx, big_value);
                let ubig_value = eval(cx, global, "Array.from(deepUbig).join(',')");
                let ubig_values = string_of(cx, ubig_value);
                let big_length = JS_GetTypedArrayLength(big.get());
                let ubig_length = JS_GetTypedArrayLength(ubig.get());
                let big_type = JS_GetArrayBufferViewType(big.get());
                let ubig_type = JS_GetArrayBufferViewType(ubig.get());
                assert_eq!(
                    big_values, "5,-6",
                    "BigInt64Array must retain signed values"
                );
                assert_eq!(
                    ubig_values, "7,8",
                    "BigUint64Array must retain unsigned values"
                );
                assert_eq!(big_length, 2, "BigInt64Array must honor its element count");
                assert_eq!(
                    ubig_length, 2,
                    "BigUint64Array must honor its element count"
                );
                assert_eq!(
                    big_type as i32, 9,
                    "BigInt64Array must report its view type"
                );
                assert_eq!(
                    ubig_type as i32, 10,
                    "BigUint64Array must report its view type"
                );

                rooted!(&in(cx) let int32 = JS_NewInt32ArrayWithBuffer(cx, buffer.handle(), 0, 4));
                rooted!(&in(cx) let uint32 = JS_NewUint32ArrayWithBuffer(cx, buffer.handle(), 16, 4));
                assert!(!int32.get().is_null() && !uint32.get().is_null());
                set_global(cx, global, c"deepInt32", ObjectValue(int32.get()));
                set_global(cx, global, c"deepUint32", ObjectValue(uint32.get()));
                eval(
                    cx,
                    global,
                    "deepInt32[0] = 1; deepInt32[1] = -2; deepInt32[2] = 3; deepInt32[3] = -4; deepUint32[0] = 4000000000; deepUint32[1] = 5; deepUint32[2] = 6; deepUint32[3] = 7;",
                );

                let (mut int_len, mut int_shared, mut int_data) =
                    (0usize, true, ptr::null_mut::<i32>());
                js::glue::GetInt32ArrayLengthAndData(
                    int32.get(),
                    &mut int_len,
                    &mut int_shared,
                    &mut int_data,
                );
                let int_values = std::slice::from_raw_parts(int_data, int_len).to_vec();
                let (mut uint_len, mut uint_shared, mut uint_data) =
                    (0usize, true, ptr::null_mut::<u32>());
                js::glue::GetUint32ArrayLengthAndData(
                    uint32.get(),
                    &mut uint_len,
                    &mut uint_shared,
                    &mut uint_data,
                );
                let uint_values = std::slice::from_raw_parts(uint_data, uint_len).to_vec();
                assert_eq!(
                    int_len, 4,
                    "GetInt32ArrayLengthAndData must report the view length"
                );
                assert_eq!(int_values, [1, -2, 3, -4]);
                assert!(!int_shared);
                assert!(!int_data.is_null());
                assert_eq!(
                    uint_len, 4,
                    "GetUint32ArrayLengthAndData must report the view length"
                );
                assert_eq!(uint_values, [4_000_000_000, 5, 6, 7]);
                assert!(!uint_shared);
                assert!(!uint_data.is_null());

                let mut external = Box::new([1u8, 2, 3, 4, 5, 6, 7, 8]);
                let external_ptr = external.as_mut_ptr();
                rooted!(&in(cx) let external_buffer = NewExternalArrayBuffer(
                    cx,
                    external.len(),
                    external_ptr.cast(),
                    Some(ignore_external_free),
                    ptr::null_mut(),
                ));
                assert!(!external_buffer.get().is_null());
                set_global(
                    cx,
                    global,
                    c"deepExternal",
                    ObjectValue(external_buffer.get()),
                );
                let external_before_value = eval(
                    cx,
                    global,
                    "Array.from(new Uint8Array(deepExternal)).join(',')",
                );
                let external_before = string_of(cx, external_before_value);
                eval(cx, global, "new Uint8Array(deepExternal)[0] = 42;");
                let external_after_value = eval(
                    cx,
                    global,
                    "Array.from(new Uint8Array(deepExternal)).join(',')",
                );
                let external_after = string_of(cx, external_after_value);
                let external_shared = external_before.len() == external_after.len();
                let external_data_matches = external[0] == 42;
                assert_eq!(external_before, "1,2,3,4,5,6,7,8");
                assert_eq!(external_after, "42,2,3,4,5,6,7,8");
                assert!(
                    external_data_matches,
                    "external buffer must share native storage"
                );

                Observation::new(vec![
                    ("f16_values", Value::String(f16_values)),
                    ("f16_length", Value::Number(f16_length as f64)),
                    ("f16_bytes", Value::Number(f16_bytes as f64)),
                    ("f16_type", Value::Number(f16_type as i32 as f64)),
                    ("f16_array_values", Value::String(f16_array_values)),
                    ("f32_values", Value::String(f32_values)),
                    ("f32_type", Value::Number(f32_type as i32 as f64)),
                    ("big_values", Value::String(big_values)),
                    ("ubig_values", Value::String(ubig_values)),
                    ("big_length", Value::Number(big_length as f64)),
                    ("ubig_length", Value::Number(ubig_length as f64)),
                    ("big_type", Value::Number(big_type as i32 as f64)),
                    ("ubig_type", Value::Number(ubig_type as i32 as f64)),
                    ("int32_length", Value::Number(int_len as f64)),
                    (
                        "int32_values",
                        Value::String(
                            int_values
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(","),
                        ),
                    ),
                    ("uint32_length", Value::Number(uint_len as f64)),
                    (
                        "uint32_values",
                        Value::String(
                            uint_values
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(","),
                        ),
                    ),
                    ("external_before", Value::String(external_before)),
                    ("external_after", Value::String(external_after)),
                    ("external_shared", Value::Boolean(external_shared)),
                    (
                        "external_data_matches",
                        Value::Boolean(external_data_matches),
                    ),
                ])
            },
        );
    }

    unsafe extern "C" fn ignore_external_free(_contents: *mut c_void, _user_data: *mut c_void) {}

    use std::ffi::c_void;
    use std::ptr;

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::jsapi::{
        JS_GetArrayBufferViewType, JS_GetTypedArrayByteLength, JS_GetTypedArrayLength, JSObject,
    };
    use js::jsval::{JSVal, ObjectValue};
    use js::rooted;
    use js::rust::wrappers2::{
        JS_NewBigInt64ArrayWithBuffer, JS_NewBigUint64ArrayWithBuffer, JS_NewFloat16ArrayFromArray,
        JS_NewFloat16ArrayWithBuffer, JS_NewFloat32Array, JS_NewInt32ArrayWithBuffer,
        JS_NewUint32ArrayWithBuffer, NewExternalArrayBuffer,
    };
    use js::rust::{HandleObject, JSEngine};

    use crate::support::{Observation, Value, eval, record_case, set_global};

    pub fn run(engine: &JSEngine) {
        buffer_extra_arrays(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_buffer_deep_contract::run(engine);
    edge_buffer_more_contract::run(engine);
    typed_array_contract::run(engine);
    run_group(engine, "buffer.types", each_type);
    run_group(engine, "buffer.views_with_offsets", views_with_offsets);
    run_group(engine, "buffer.detach", detach);
    run_group(engine, "buffer.clone_and_copy", clone_and_copy);
    run_group(engine, "buffer.external", external_buffers);
    run_group(engine, "buffer.get_object_as", get_object_as_family);
}

fn run_group(
    engine: &JSEngine,
    case: &str,
    f: impl FnOnce(&mut JSContext, HandleObject) -> Vec<(&'static str, crate::support::Value)>,
) {
    let apis = apis_for_case(case);
    crate::support::run_case(case, apis, || {
        let fields = with_global(engine, f);
        crate::support::Observation::new(fields)
    });
}

fn get_object_as_family(
    cx: &mut JSContext,
    _global: HandleObject,
) -> Vec<(&'static str, crate::support::Value)> {
    let buffer = unsafe { NewArrayBuffer(cx, 64) };
    rooted!(&in(cx) let buffer = buffer);
    let mut shared = false;
    let base = unsafe { GetArrayBufferData(buffer.get(), &mut shared, ptr::null()) };
    assert!(!base.is_null());
    assert!(!shared);

    macro_rules! check_array {
        ($new:ident, $object_as:ident, $data:ident, $length_and_data:ident, $ty:ty, $count:expr) => {{
            let array = unsafe { $new(cx, buffer.handle(), 0, $count) };
            rooted!(&in(cx) let array = array);
            assert!(!array.get().is_null());
            let mut length = 0;
            let (mut data, mut is_shared) = (ptr::null_mut::<$ty>(), false);
            let object = unsafe { $object_as(array.get(), &mut length, &mut is_shared, &mut data) };
            assert_eq!(object, array.get());
            assert_eq!(length, $count);
            assert!(!is_shared);
            assert_eq!(data as *mut u8, base);
            assert_eq!(unsafe { $data(array.get(), &mut is_shared, ptr::null()) }, data);
            assert!(!is_shared);
            let (mut glen, mut gshared, mut gdata) = (0, true, ptr::null_mut());
            unsafe { $length_and_data(array.get(), &mut glen, &mut gshared, &mut gdata) };
            assert_eq!(glen, $count);
            assert!(!gshared);
            assert_eq!(gdata.cast::<u8>(), (data as *mut u8).cast::<u8>());
        }};
    }

    check_array!(
        JS_NewInt8ArrayWithBuffer,
        JS_GetObjectAsInt8Array,
        JS_GetInt8ArrayData,
        GetInt8ArrayLengthAndData,
        i8,
        4
    );
    check_array!(
        JS_NewUint8ArrayWithBuffer,
        JS_GetObjectAsUint8Array,
        JS_GetUint8ArrayData,
        GetUint8ArrayLengthAndData,
        u8,
        4
    );
    check_array!(
        JS_NewUint8ClampedArrayWithBuffer,
        JS_GetObjectAsUint8ClampedArray,
        JS_GetUint8ClampedArrayData,
        GetUint8ClampedArrayLengthAndData,
        u8,
        4
    );
    check_array!(
        JS_NewInt16ArrayWithBuffer,
        JS_GetObjectAsInt16Array,
        JS_GetInt16ArrayData,
        GetInt16ArrayLengthAndData,
        i16,
        2
    );
    check_array!(
        JS_NewUint16ArrayWithBuffer,
        JS_GetObjectAsUint16Array,
        JS_GetUint16ArrayData,
        GetUint16ArrayLengthAndData,
        u16,
        2
    );
    check_array!(
        JS_NewInt32ArrayWithBuffer,
        JS_GetObjectAsInt32Array,
        JS_GetInt32ArrayData,
        GetInt32ArrayLengthAndData,
        i32,
        2
    );
    check_array!(
        JS_NewUint32ArrayWithBuffer,
        JS_GetObjectAsUint32Array,
        JS_GetUint32ArrayData,
        GetUint32ArrayLengthAndData,
        u32,
        2
    );
    check_array!(
        JS_NewFloat32ArrayWithBuffer,
        JS_GetObjectAsFloat32Array,
        JS_GetFloat32ArrayData,
        GetFloat32ArrayLengthAndData,
        f32,
        2
    );
    check_array!(
        JS_NewFloat64ArrayWithBuffer,
        JS_GetObjectAsFloat64Array,
        JS_GetFloat64ArrayData,
        GetFloat64ArrayLengthAndData,
        f64,
        1
    );

    // Float16 and the BigInt arrays have no `Get*ArrayLengthAndData` glue helper, so check the
    // view-level byte length rather than borrowing the accessor of a different element type.
    macro_rules! check_view {
        ($new:ident, $object_as:ident, $data:ident, $ty:ty, $count:expr) => {{
            let array = unsafe { $new(cx, buffer.handle(), 0, $count) };
            rooted!(&in(cx) let array = array);
            assert!(!array.get().is_null());
            let mut length = 0;
            let (mut data, mut is_shared) = (ptr::null_mut::<$ty>(), false);
            let object = unsafe { $object_as(array.get(), &mut length, &mut is_shared, &mut data) };
            assert_eq!(object, array.get());
            assert_eq!(length, $count);
            assert!(!is_shared);
            assert_eq!(data as *mut u8, base);
            assert_eq!(unsafe { $data(array.get(), &mut is_shared, ptr::null()) }, data);
            assert!(!is_shared);
            let (mut glen, mut gshared, mut gdata) = (0usize, true, ptr::null_mut());
            unsafe { GetArrayBufferViewLengthAndData(array.get(), &mut glen, &mut gshared, &mut gdata) };
            assert_eq!(glen, $count * std::mem::size_of::<$ty>());
            assert!(!gshared);
            assert_eq!(gdata, data as *mut u8);
        }};
    }

    check_view!(
        JS_NewFloat16ArrayWithBuffer,
        JS_GetObjectAsFloat16Array,
        JS_GetFloat16ArrayData,
        u16,
        2
    );
    check_view!(
        JS_NewBigInt64ArrayWithBuffer,
        JS_GetObjectAsBigInt64Array,
        JS_GetBigInt64ArrayData,
        i64,
        1
    );
    check_view!(
        JS_NewBigUint64ArrayWithBuffer,
        JS_GetObjectAsBigUint64Array,
        JS_GetBigUint64ArrayData,
        u64,
        1
    );

    let view = unsafe { JS_NewUint8ArrayWithBuffer(cx, buffer.handle(), 8, 4) };
    rooted!(&in(cx) let view = view);
    let (mut length, mut is_shared, mut data) = (0, true, ptr::null_mut());
    assert_eq!(
        unsafe {
            JS_GetObjectAsArrayBufferView(view.get(), &mut length, &mut is_shared, &mut data)
        },
        view.get()
    );
    assert_eq!((length, is_shared), (4, false));
    assert_eq!(data, unsafe { base.add(8) });

    let mut length = 0;
    let mut data = ptr::null_mut();
    assert_eq!(
        unsafe { GetObjectAsArrayBuffer(buffer.get(), &mut length, &mut data) },
        buffer.get()
    );
    assert_eq!((length, data), (64, base));
    vec![
        (
            "buffer_byte_length",
            crate::support::Value::Number(length as f64),
        ),
        (
            "data_matches_base",
            crate::support::Value::Boolean(data == base),
        ),
        ("view_byte_offset", crate::support::Value::Number(8.0)),
        ("view_byte_length", crate::support::Value::Number(4.0)),
        (
            "view_data_matches_base_plus_offset",
            crate::support::Value::Boolean(true),
        ),
    ]
}

fn apis_for_case(case: &str) -> &'static [&'static str] {
    match case {
        "buffer.types" => &[
            "JS_NewInt8Array",
            "JS_NewInt8ArrayFromArray",
            "JS_NewInt8ArrayWithBuffer",
            "UnwrapInt8Array",
            "JS_NewUint8Array",
            "JS_NewUint8ArrayFromArray",
            "JS_NewUint8ArrayWithBuffer",
            "UnwrapUint8Array",
            "JS_NewUint8ClampedArray",
            "JS_NewUint8ClampedArrayFromArray",
            "JS_NewUint8ClampedArrayWithBuffer",
            "UnwrapUint8ClampedArray",
            "JS_NewInt16Array",
            "JS_NewInt16ArrayFromArray",
            "JS_NewInt16ArrayWithBuffer",
            "UnwrapInt16Array",
            "JS_NewUint16Array",
            "JS_NewUint16ArrayFromArray",
            "JS_NewUint16ArrayWithBuffer",
            "UnwrapUint16Array",
            "JS_NewInt32Array",
            "JS_NewInt32ArrayFromArray",
            "JS_NewInt32ArrayWithBuffer",
            "UnwrapInt32Array",
            "JS_NewUint32Array",
            "JS_NewUint32ArrayFromArray",
            "JS_NewUint32ArrayWithBuffer",
            "UnwrapUint32Array",
            "JS_NewFloat16Array",
            "JS_NewFloat16ArrayFromArray",
            "JS_NewFloat16ArrayWithBuffer",
            "UnwrapFloat16Array",
            "JS_NewFloat32Array",
            "JS_NewFloat32ArrayFromArray",
            "JS_NewFloat32ArrayWithBuffer",
            "UnwrapFloat32Array",
            "JS_NewFloat64Array",
            "JS_NewFloat64ArrayFromArray",
            "JS_NewFloat64ArrayWithBuffer",
            "UnwrapFloat64Array",
            "JS_NewBigInt64Array",
            "JS_NewBigInt64ArrayFromArray",
            "JS_NewBigInt64ArrayWithBuffer",
            "UnwrapBigInt64Array",
            "JS_NewBigUint64Array",
            "JS_NewBigUint64ArrayFromArray",
            "JS_NewBigUint64ArrayWithBuffer",
            "UnwrapBigUint64Array",
            "JS_IsTypedArrayObject",
            "JS_IsArrayBufferViewObject",
            "IsArrayBufferObject",
            "JS_GetArrayBufferViewType",
            "JS_GetTypedArrayLength",
            "JS_GetTypedArrayByteLength",
            "JS_GetTypedArraySharedness",
            "UnwrapArrayBufferView",
            "NewArrayBuffer",
            "JS_GetArrayBufferViewByteOffset",
            "UnwrapArrayBuffer",
        ],
        "buffer.views_with_offsets" => &[
            "JS_GetArrayBufferViewByteOffset",
            "JS_GetArrayBufferViewByteLength",
            "JS_GetTypedArrayLength",
            "JS_GetArrayBufferViewBuffer",
            "JS_NewDataView",
            "JS_IsArrayBufferViewObject",
            "JS_IsTypedArrayObject",
            "JS_GetArrayBufferViewType",
        ],
        "buffer.detach" => &[
            "HasDefinedArrayBufferDetachKey",
            "ArrayBufferHasData",
            "DetachArrayBuffer",
            "IsDetachedArrayBufferObject",
            "GetArrayBufferByteLength",
            "GetArrayBufferLengthAndData",
            "JS_GetTypedArrayLength",
            "JS_GetArrayBufferViewByteOffset",
            "GetArrayBufferViewLengthAndData",
        ],
        "buffer.clone_and_copy" => &[
            "ArrayBufferClone",
            "ArrayBufferCopyData",
            "CopyArrayBuffer",
            "DetachArrayBuffer",
        ],
        "buffer.external" => &[
            "NewExternalArrayBuffer",
            "GetArrayBufferData",
            "StealArrayBufferContents",
            "NewArrayBufferWithContents",
            "NewArrayBufferWithUserOwnedContents",
            "DetachArrayBuffer",
        ],
        "buffer.get_object_as" => &[
            "NewArrayBuffer",
            "GetArrayBufferData",
            "JS_NewInt8ArrayWithBuffer",
            "JS_GetObjectAsInt8Array",
            "JS_GetInt8ArrayData",
            "GetInt8ArrayLengthAndData",
            "JS_NewUint8ArrayWithBuffer",
            "JS_GetObjectAsUint8Array",
            "JS_GetUint8ArrayData",
            "GetUint8ArrayLengthAndData",
            "JS_NewUint8ClampedArrayWithBuffer",
            "JS_GetObjectAsUint8ClampedArray",
            "JS_GetUint8ClampedArrayData",
            "GetUint8ClampedArrayLengthAndData",
            "JS_NewInt16ArrayWithBuffer",
            "JS_GetObjectAsInt16Array",
            "JS_GetInt16ArrayData",
            "GetInt16ArrayLengthAndData",
            "JS_NewUint16ArrayWithBuffer",
            "JS_GetObjectAsUint16Array",
            "JS_GetUint16ArrayData",
            "GetUint16ArrayLengthAndData",
            "JS_NewInt32ArrayWithBuffer",
            "JS_GetObjectAsInt32Array",
            "JS_GetInt32ArrayData",
            "GetInt32ArrayLengthAndData",
            "JS_NewUint32ArrayWithBuffer",
            "JS_GetObjectAsUint32Array",
            "JS_GetUint32ArrayData",
            "GetUint32ArrayLengthAndData",
            "JS_NewFloat32ArrayWithBuffer",
            "JS_GetObjectAsFloat32Array",
            "JS_GetFloat32ArrayData",
            "GetFloat32ArrayLengthAndData",
            "JS_NewFloat64ArrayWithBuffer",
            "JS_GetObjectAsFloat64Array",
            "JS_GetFloat64ArrayData",
            "GetFloat64ArrayLengthAndData",
            "JS_NewFloat16ArrayWithBuffer",
            "JS_GetObjectAsFloat16Array",
            "JS_GetFloat16ArrayData",
            "JS_NewBigInt64ArrayWithBuffer",
            "JS_GetObjectAsBigInt64Array",
            "JS_GetBigInt64ArrayData",
            "JS_NewBigUint64ArrayWithBuffer",
            "JS_GetObjectAsBigUint64Array",
            "JS_GetBigUint64ArrayData",
            "GetArrayBufferViewLengthAndData",
            "JS_GetObjectAsArrayBufferView",
            "GetObjectAsArrayBuffer",
        ],
        _ => panic!("unknown buffer case {case}"),
    }
}
