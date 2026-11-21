/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Structured clone buffers, the callback API and the convenience clone entry point.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use js::context::JSContext;
use js::glue::{
    CopyJSStructuredCloneData, DeleteJSAutoStructuredCloneBuffer, GetLengthOfJSStructuredCloneData,
    JS_GetReservedSlot, NewJSAutoStructuredCloneBuffer, WriteBytesToJSStructuredCloneData,
};
use js::jsapi::{
    CloneDataPolicy, Handle, HandleObject, JS_STRUCTURED_CLONE_VERSION,
    JSCLASS_RESERVED_SLOTS_MASK, JSCLASS_RESERVED_SLOTS_SHIFT, JSClass, JSObject, JSString,
    JSStructuredCloneCallbacks, JSStructuredCloneData, JSStructuredCloneReader,
    JSStructuredCloneWriter, MutableHandle, OnNewGlobalHookOption, StructuredCloneScope,
};
use js::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    JS_NewGlobalObject, JS_NewUint8Array, JS_ReadStructuredClone, JS_WriteStructuredClone,
};
use js::rust::{
    CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS, evaluate_script,
};

use crate::support::{Observation, Value as Observed, record};

const SCOPE: StructuredCloneScope = StructuredCloneScope::DifferentProcess;

const POLICY: CloneDataPolicy = CloneDataPolicy {
    allowIntraClusterClonableSharedObjects_: false,
    allowSharedMemoryObjects_: false,
};

const HOST_TAG: u32 = 0xFFFF_8001;
const HOST_SLOT: u32 = 0;

static HOST_CLASS: JSClass = JSClass {
    name: c"ContractHost".as_ptr(),
    flags: (1 & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

thread_local! {
    static WRITES: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static READS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static SCOPES: RefCell<Vec<StructuredCloneScope>> = const { RefCell::new(Vec::new()) };
    static CLONE_BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static CLONE_PAIR: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

fn with_global(engine: &JSEngine, f: impl FnOnce(&mut JSContext, SafeHandleObject)) {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(!global.get().is_null(), "JS_NewGlobalObject returned null");
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let (global, cx) = realm.global_and_reborrow();
        f(cx, global);
    }
}

fn eval(cx: &mut JSContext, global: SafeHandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"clone.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options)
        .unwrap_or_else(|_| panic!("evaluating {source:?} failed"));
    rval.get()
}

fn raw_handle<T>(ptr: &T) -> Handle<T> {
    unsafe { Handle::from_marked_location(ptr) }
}

fn raw_mutable<T>(ptr: &mut T) -> MutableHandle<T> {
    unsafe { MutableHandle::from_marked_location(ptr) }
}

fn safe_handle<T>(ptr: &T) -> js::rust::Handle<'_, T> {
    unsafe { js::rust::Handle::from_marked_location(ptr) }
}

fn safe_mutable<T>(ptr: &mut T) -> js::rust::MutableHandle<'_, T> {
    unsafe { js::rust::MutableHandle::from_marked_location(ptr) }
}

unsafe fn new_host(cx: &mut JSContext, id: i32) -> *mut JSObject {
    let object = js::rust::wrappers2::JS_NewObject(cx, &HOST_CLASS);
    assert!(!object.is_null());
    js::jsapi::JS_SetReservedSlot(object, HOST_SLOT, &Int32Value(id));
    object
}

unsafe fn host_id(object: *mut JSObject) -> Option<i32> {
    let mut value = UndefinedValue();
    JS_GetReservedSlot(object, HOST_SLOT, &mut value);
    value.is_int32().then(|| value.to_int32())
}

unsafe extern "C" fn write_callback(
    cx: *mut js::context::RawJSContext,
    writer: *mut JSStructuredCloneWriter,
    object: HandleObject,
    _same_process: *mut bool,
    _closure: *mut c_void,
) -> bool {
    let Some(id) = host_id(*object.ptr) else {
        return false;
    };
    WRITES.with(|writes| writes.borrow_mut().push(id as u32));
    SCOPES.with(|scopes| {
        scopes
            .borrow_mut()
            .push(js::jsapi::JS_GetStructuredCloneScope(writer))
    });
    let cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let cx = &mut { cx };

    // The callback writes a host record without using startWrite for the object itself, so the
    // object must not be kept in the writer's memory as a written object.
    assert!(js::jsapi::JS_ObjectNotWritten(writer, object));

    let text = js::rust::wrappers2::JS_NewStringCopyZ(cx, c"host text".as_ptr());
    assert!(!text.is_null());
    let typed = ObjectValue(JS_NewUint8Array(cx, 4));
    assert!(typed.is_object());

    js::jsapi::JS_WriteUint32PairUnchecked(writer, HOST_TAG, id as u32) &&
        js::jsapi::JS_WriteBytes(writer, b"payload!".as_ptr().cast(), 8) &&
        js::jsapi::JS_WriteString(writer, raw_handle(&text)) &&
        js::jsapi::JS_WriteDouble(writer, 2.5) &&
        js::jsapi::JS_WriteTypedArray(writer, raw_handle(&typed)) &&
        js::jsapi::JS_WriteUint32PairUnchecked(writer, 11, 22)
}

unsafe extern "C" fn read_callback(
    cx: *mut js::context::RawJSContext,
    reader: *mut JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    data: u32,
    _closure: *mut c_void,
) -> *mut JSObject {
    assert_eq!(tag, HOST_TAG);
    let mut bytes = [0u8; 8];
    assert!(js::jsapi::JS_ReadBytes(
        reader,
        bytes.as_mut_ptr().cast(),
        bytes.len()
    ));
    assert_eq!(&bytes, b"payload!");
    CLONE_BYTES.with(|captured| *captured.borrow_mut() = bytes.to_vec());

    let mut text: *mut JSString = ptr::null_mut();
    assert!(js::jsapi::JS_ReadString(reader, raw_mutable(&mut text)));
    assert!(!text.is_null());
    assert_eq!(js::jsapi::JS_GetStringLength(text), 9);

    let mut double = 0.0;
    assert!(js::jsapi::JS_ReadDouble(reader, &mut double));
    assert_eq!(double, 2.5);

    let mut typed = UndefinedValue();
    assert!(js::jsapi::JS_ReadTypedArray(
        reader,
        raw_mutable(&mut typed)
    ));
    assert!(typed.is_object());

    let mut p1 = 0;
    let mut p2 = 0;
    assert!(js::jsapi::JS_ReadUint32Pair(reader, &mut p1, &mut p2));
    assert_eq!((p1, p2), (11, 22));
    CLONE_PAIR.with(|pair| pair.set((p1, p2)));

    READS.with(|reads| reads.borrow_mut().push(data));
    let cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    new_host(&mut { cx }, data as i32 + 1000)
}

unsafe extern "C" fn report_error(
    _cx: *mut js::context::RawJSContext,
    _error: u32,
    _closure: *mut c_void,
    _message: *const std::os::raw::c_char,
) {
}

static CALLBACKS: JSStructuredCloneCallbacks = JSStructuredCloneCallbacks {
    read: Some(read_callback),
    write: Some(write_callback),
    reportError: Some(report_error),
    readTransfer: None,
    writeTransfer: None,
    freeTransfer: None,
    canTransfer: None,
    sabCloned: None,
};

unsafe fn write_value(
    cx: &mut JSContext,
    value: JSVal,
    transfer: JSVal,
    data: *mut JSStructuredCloneData,
) -> bool {
    JS_WriteStructuredClone(
        cx,
        safe_handle(&value),
        data,
        SCOPE,
        &POLICY,
        &CALLBACKS,
        ptr::null_mut(),
        safe_handle(&transfer),
    )
}

unsafe fn read_value(cx: &mut JSContext, data: *const JSStructuredCloneData) -> Option<JSVal> {
    let mut value = UndefinedValue();
    let ok = JS_ReadStructuredClone(
        cx,
        data,
        JS_STRUCTURED_CLONE_VERSION,
        SCOPE,
        safe_mutable(&mut value),
        &POLICY,
        &CALLBACKS,
        ptr::null_mut(),
    );
    ok.then_some(value)
}

fn buffer_lifecycle(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        let value = eval(cx, global, "({ answer: 42, list: [1, 2, 3] })");
        let empty = UndefinedValue();

        let source = NewJSAutoStructuredCloneBuffer(SCOPE, &CALLBACKS);
        assert!(!source.is_null());
        assert!(write_value(cx, value, empty, &mut (*source).data_));
        let length = GetLengthOfJSStructuredCloneData(&mut (*source).data_);
        assert!(length > 0);
        let source_length = length;

        let mut bytes = vec![0u8; length];
        CopyJSStructuredCloneData(&mut (*source).data_, bytes.as_mut_ptr());
        DeleteJSAutoStructuredCloneBuffer(source);

        let destination = NewJSAutoStructuredCloneBuffer(SCOPE, ptr::null());
        assert!(!destination.is_null());
        assert!(WriteBytesToJSStructuredCloneData(
            bytes.as_ptr(),
            bytes.len(),
            &mut (*destination).data_
        ));
        let copied_length = GetLengthOfJSStructuredCloneData(&mut (*destination).data_);
        assert_eq!(
            copied_length, length,
            "copied bytes must preserve the clone length"
        );
        let output = read_value(cx, &(*destination).data_).expect("reading copied bytes");
        assert!(output.is_object());
        let answer = crate::support::get_property(cx, output, c"answer");
        assert_eq!(
            crate::support::observe(cx, answer),
            crate::support::Value::Number(42.0)
        );
        let answer = 42.0;
        rooted!(&in(cx) let list = crate::support::get_property(cx, output, c"list"));
        let list = list.to_object();
        let list_len = if !list.is_null() {
            let mut len = 0;
            assert!(js::rust::wrappers2::GetArrayLength(
                cx,
                safe_handle(&list),
                &mut len
            ));
            len
        } else {
            0
        };
        assert_eq!(list_len, 3, "serialized array must retain all elements");
        DeleteJSAutoStructuredCloneBuffer(destination);
        record(
            "clone.buffer_lifecycle",
            &Observation::new(vec![
                (
                    "source_length_positive",
                    Observed::Boolean(source_length > 0),
                ),
                (
                    "copied_length_matches",
                    Observed::Boolean(copied_length == source_length),
                ),
                ("output_is_object", Observed::Boolean(true)),
                ("answer", Observed::Number(answer)),
                ("list_length", Observed::Number(list_len as f64)),
            ]),
            &[
                "CopyJSStructuredCloneData",
                "DeleteJSAutoStructuredCloneBuffer",
                "GetLengthOfJSStructuredCloneData",
                "JS_NewGlobalObject",
                "JS_ReadStructuredClone",
                "JS_WriteStructuredClone",
                "NewJSAutoStructuredCloneBuffer",
                "WriteBytesToJSStructuredCloneData",
            ],
        );
    });
}

fn callback_roundtrip(engine: &JSEngine) {
    WRITES.with(|writes| writes.borrow_mut().clear());
    READS.with(|reads| reads.borrow_mut().clear());
    SCOPES.with(|scopes| scopes.borrow_mut().clear());
    CLONE_BYTES.with(|bytes| bytes.borrow_mut().clear());
    CLONE_PAIR.with(|pair| pair.set((0, 0)));

    with_global(engine, |cx, global| unsafe {
        let host = new_host(cx, 7);
        let value = js::jsval::ObjectValue(host);
        let array = eval(cx, global, "[1, 2]");
        // Put the host object into an array twice: JS_ObjectNotWritten makes the second
        // occurrence invoke the write callback again instead of becoming a back-reference.
        rooted!(&in(cx) let array = array.to_object());
        let element = safe_handle(&value);
        assert!(js::rust::wrappers2::JS_SetElement(
            cx,
            array.handle(),
            0,
            element
        ));
        assert!(js::rust::wrappers2::JS_SetElement(
            cx,
            array.handle(),
            1,
            element
        ));

        let buffer = NewJSAutoStructuredCloneBuffer(SCOPE, &CALLBACKS);
        let empty = UndefinedValue();
        let array = ObjectValue(array.get());
        assert!(write_value(cx, array, empty, &mut (*buffer).data_));
        let output = read_value(cx, &(*buffer).data_).expect("reading callback payload");
        DeleteJSAutoStructuredCloneBuffer(buffer);
        assert!(output.is_object());
        let first = crate::support::get_property(cx, output, c"0");
        let second = crate::support::get_property(cx, output, c"1");
        assert!(first.is_object() && second.is_object());
        let first_id = host_id(first.to_object());
        let second_id = host_id(second.to_object());
        let callback_bytes = String::from_utf8(CLONE_BYTES.with(|bytes| bytes.borrow().clone()))
            .expect("callback bytes are UTF-8");
        assert_eq!(callback_bytes, "payload!");
        let callback_pair = CLONE_PAIR.with(Cell::get);
        assert_eq!(callback_pair, (11, 22));
        record(
            "clone.callback_roundtrip",
            &Observation::new(vec![
                (
                    "writes",
                    Observed::String(format!(
                        "{:?}",
                        WRITES.with(|writes| writes.borrow().clone())
                    )),
                ),
                (
                    "reads",
                    Observed::String(format!("{:?}", READS.with(|reads| reads.borrow().clone()))),
                ),
                (
                    "scopes",
                    Observed::String(format!(
                        "{:?}",
                        SCOPES.with(|scopes| scopes.borrow().clone())
                    )),
                ),
                (
                    "host_ids",
                    Observed::String(format!("{:?}", (first_id, second_id))),
                ),
                ("first_is_object", Observed::Boolean(first.is_object())),
                ("second_is_object", Observed::Boolean(second.is_object())),
                ("callback_bytes", Observed::String(callback_bytes)),
                (
                    "callback_pair",
                    Observed::String(format!("{},{}", callback_pair.0, callback_pair.1)),
                ),
            ]),
            &[
                "JS_GetStructuredCloneScope",
                "JS_NewGlobalObject",
                "JS_ObjectNotWritten",
                "JS_ReadBytes",
                "JS_ReadDouble",
                "JS_ReadString",
                "JS_ReadTypedArray",
                "JS_ReadUint32Pair",
                "JS_WriteBytes",
                "JS_WriteDouble",
                "JS_WriteString",
                "JS_WriteTypedArray",
                "JS_WriteUint32PairUnchecked",
                "NewJSAutoStructuredCloneBuffer",
                "DeleteJSAutoStructuredCloneBuffer",
            ],
        );
    });
}

mod edge_clone_more_contract {
    use std::ptr;

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::glue::{
        CopyJSStructuredCloneData, DeleteJSAutoStructuredCloneBuffer,
        GetLengthOfJSStructuredCloneData, NewJSAutoStructuredCloneBuffer,
        WriteBytesToJSStructuredCloneData,
    };
    use js::jsapi::mozilla::Range;
    use js::jsapi::{CloneDataPolicy, JS_STRUCTURED_CLONE_VERSION, StructuredCloneScope};
    use js::jsval::{JSVal, UndefinedValue};
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{JS_ReadStructuredClone, JS_WriteStructuredClone};

    use crate::support::{Observation, Value, eval, record_case, set_global};

    fn structured_clone(engine: &JSEngine) {
        record_case(
            engine,
            "edge.structured_clone",
            &[
                "NewJSAutoStructuredCloneBuffer",
                "DeleteJSAutoStructuredCloneBuffer",
                "CopyJSStructuredCloneData",
                "GetLengthOfJSStructuredCloneData",
                "JS_WriteStructuredClone",
                "JS_ReadStructuredClone",
                "WriteBytesToJSStructuredCloneData",
            ],
            |cx, global| unsafe {
                let scope = StructuredCloneScope::DifferentProcess;
                let policy = CloneDataPolicy {
                    allowIntraClusterClonableSharedObjects_: false,
                    allowSharedMemoryObjects_: false,
                };
                let source = NewJSAutoStructuredCloneBuffer(scope, ptr::null());
                assert!(!source.is_null());
                rooted!(&in(cx) let value = eval(cx, global, "({a:1, b:[2,3], s:'clone'})"));
                rooted!(&in(cx) let transfer = UndefinedValue());
                let write_ok = JS_WriteStructuredClone(
                    cx,
                    value.handle(),
                    &mut (*source).data_,
                    scope,
                    &policy,
                    ptr::null(),
                    ptr::null_mut(),
                    transfer.handle(),
                );
                assert!(
                    write_ok,
                    "JS_WriteStructuredClone must serialize the object"
                );
                let written_len = GetLengthOfJSStructuredCloneData(&mut (*source).data_);
                assert!(written_len > 0, "a written clone must have non-zero length");
                let mut bytes = vec![0u8; written_len];
                CopyJSStructuredCloneData(&mut (*source).data_, bytes.as_mut_ptr());
                assert_eq!(bytes.len(), written_len);

                let destination = NewJSAutoStructuredCloneBuffer(scope, ptr::null());
                assert!(!destination.is_null());
                let copy_ok = WriteBytesToJSStructuredCloneData(
                    bytes.as_ptr(),
                    bytes.len(),
                    &mut (*destination).data_,
                );
                assert!(
                    copy_ok,
                    "WriteBytesToJSStructuredCloneData must accept copied bytes"
                );
                let copied_len = GetLengthOfJSStructuredCloneData(&mut (*destination).data_);
                assert_eq!(
                    copied_len, written_len,
                    "WriteBytesToJSStructuredCloneData must preserve the serialized length"
                );
                rooted!(&in(cx) let mut read_back = UndefinedValue());
                let read_ok = JS_ReadStructuredClone(
                    cx,
                    &(*destination).data_,
                    JS_STRUCTURED_CLONE_VERSION,
                    scope,
                    read_back.handle_mut(),
                    &policy,
                    ptr::null(),
                    ptr::null_mut(),
                );
                assert!(
                    read_ok,
                    "JS_ReadStructuredClone must deserialize the copied bytes"
                );
                let roundtrip = if read_back.get().is_object() {
                    set_global(cx, global, c"roundtrip", read_back.get());
                    let text = eval(
                        cx,
                        global,
                        "`${roundtrip.a}|${roundtrip.b.join(',')}|${roundtrip.s}`",
                    );
                    let text = string_of(cx, text);
                    Value::String(text)
                } else {
                    Value::Null
                };
                assert_eq!(
                    roundtrip,
                    Value::String("1|2,3|clone".to_owned()),
                    "structured clone must preserve object fields and nested arrays"
                );

                DeleteJSAutoStructuredCloneBuffer(source);
                DeleteJSAutoStructuredCloneBuffer(destination);

                Observation::new(vec![
                    ("write_ok", Value::Boolean(write_ok)),
                    ("bytes_match", Value::Boolean(bytes.len() == written_len)),
                    ("copy_ok", Value::Boolean(copy_ok)),
                    ("copied_len_positive", Value::Boolean(copied_len > 0)),
                    ("read_ok", Value::Boolean(read_ok)),
                    ("roundtrip", roundtrip),
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
        structured_clone(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_clone_more_contract::run(engine);
    crate::support::isolate_case("clone.buffer_lifecycle", || buffer_lifecycle(engine));
    crate::support::isolate_case("clone.callback_roundtrip", || callback_roundtrip(engine));
}
