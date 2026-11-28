/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Structured clone on QuickJS, used the way Servo uses it: the bytes are copied out of the
//! writer's buffer and copied into a new buffer for reading.

use std::cell::RefCell;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::glue::{
    CopyJSStructuredCloneData, GetLengthOfJSStructuredCloneData, JS_GetReservedSlot,
    WriteBytesToJSStructuredCloneData,
};
use mozjs::jsapi::{
    CloneDataPolicy, Handle, HandleObject as RawHandleObject, JSClass, JSContext as RawJSContext,
    JSObject, JSString, JSStructuredCloneCallbacks, JSStructuredCloneReader,
    JSStructuredCloneWriter, JS_GetStringLength, JS_NewObject, JS_NewStringCopyZ, JS_NewUint8Array,
    JS_ReadBytes, JS_ReadDouble, JS_ReadString, JS_ReadTypedArray, JS_SetReservedSlot,
    JS_WriteBytes, JS_WriteDouble, JS_WriteString, JS_WriteTypedArray, JS_WriteUint32PairUnchecked,
    MutableHandle, MutableHandleObject, OnNewGlobalHookOption, StructuredCloneScope,
    TransferableOwnership, JSCLASS_RESERVED_SLOTS_MASK, JSCLASS_RESERVED_SLOTS_SHIFT,
    JS_SCERR_DUP_TRANSFERABLE, JS_SCERR_NOT_CLONABLE, JS_SCERR_SHMEM_TRANSFERABLE,
    JS_SCERR_TRANSFERABLE, JS_SCERR_TRANSFERABLE_TWICE, JS_SCERR_TYPED_ARRAY_DETACHED,
    JS_SCERR_UNSUPPORTED_TYPE, JS_STRUCTURED_CLONE_VERSION,
};
use mozjs::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    JS_ClearPendingException, JS_IsExceptionPending, JS_NewGlobalObject, JS_ReadStructuredClone,
    JS_SetProperty, JS_StructuredClone, JS_WriteStructuredClone,
};
use mozjs::rust::{
    evaluate_script, get_object_class, CompileOptionsWrapper, HandleObject,
    JSAutoStructuredCloneBufferWrapper, JSEngine, JSEngineHandle, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS,
};

/// The engine can only be initialized once per process; it is shared by the tests.
static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

fn with_global(f: impl FnOnce(&mut JSContext, HandleObject)) {
    let mut runtime = Runtime::new(ENGINE.clone());
    let context = runtime.cx();
    let mut options = RealmOptions::default();
    options.creationOptions_.sharedMemoryAndAtomics_ = true;
    options.creationOptions_.defineSharedArrayBufferConstructor_ = true;
    unsafe {
        rooted!(&in(context) let global = JS_NewGlobalObject(
            context,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*options,
        ));
        let mut realm = AutoRealm::new_from_handle(context, global.handle());
        f(&mut realm, global.handle());
    }
}

/// Evaluates `src` in a block, so that its `let` declarations don't outlive it.
fn eval(cx: &mut JSContext, global: HandleObject, src: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_clone".to_owned(), 1);
    let block = format!("{{\n{src}\n}}");
    assert!(
        evaluate_script(cx, global, &block, rval.handle_mut(), options).is_ok(),
        "{src}"
    );
    rval.get()
}

fn check(cx: &mut JSContext, global: HandleObject, src: &str) {
    let v = eval(cx, global, src);
    assert!(v.is_boolean() && v.to_boolean(), "{src}");
}

fn set_global(cx: &mut JSContext, global: HandleObject, name: &CStr, v: JSVal) {
    rooted!(&in(cx) let v = v);
    assert!(unsafe { JS_SetProperty(cx, global, name.as_ptr(), v.handle()) });
}

const POLICY: CloneDataPolicy = CloneDataPolicy {
    allowIntraClusterClonableSharedObjects_: false,
    allowSharedMemoryObjects_: false,
};

const SCOPE: StructuredCloneScope = StructuredCloneScope::DifferentProcess;

/// Serializes `v` like Servo's `structuredclone::write`: the bytes are copied out of the buffer.
fn write(cx: &mut JSContext, v: JSVal, transfer: JSVal) -> Option<Vec<u8>> {
    rooted!(&in(cx) let v = v);
    rooted!(&in(cx) let transfer = transfer);
    unsafe {
        let buf = JSAutoStructuredCloneBufferWrapper::new(SCOPE, &CALLBACKS);
        let data = &mut (*buf.as_raw_ptr()).data_;
        let ok = JS_WriteStructuredClone(
            cx,
            v.handle(),
            data,
            SCOPE,
            &POLICY,
            &CALLBACKS,
            ptr::null_mut(),
            transfer.handle(),
        );
        if !ok {
            return None;
        }
        let len = GetLengthOfJSStructuredCloneData(data);
        let mut bytes = Vec::with_capacity(len);
        CopyJSStructuredCloneData(data, bytes.as_mut_ptr());
        bytes.set_len(len);
        Some(bytes)
    }
}

/// Deserializes like Servo's `structuredclone::read`: the bytes are copied into a new buffer.
fn read(cx: &mut JSContext, bytes: &[u8]) -> Option<JSVal> {
    unsafe {
        let buf = JSAutoStructuredCloneBufferWrapper::new(SCOPE, &CALLBACKS);
        let data = &mut (*buf.as_raw_ptr()).data_;
        assert!(WriteBytesToJSStructuredCloneData(
            bytes.as_ptr(),
            bytes.len(),
            data
        ));
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

/// Evaluates `src` into the global `input`, and its clone into the global `out`.
fn roundtrip(cx: &mut JSContext, global: HandleObject, src: &str) {
    let input = eval(cx, global, src);
    set_global(cx, global, c"input", input);
    let bytes =
        write(cx, input, UndefinedValue()).unwrap_or_else(|| panic!("writing {src} failed"));
    let out = read(cx, &bytes).unwrap_or_else(|| panic!("reading {src} failed"));
    set_global(cx, global, c"out", out);
}

/// Expects writing `src` to fail without an exception, after reporting `error`.
fn expect_reported(
    cx: &mut JSContext,
    global: HandleObject,
    src: &str,
    transfer: &str,
    error: u32,
) {
    let v = eval(cx, global, src);
    let t = eval(cx, global, transfer);
    take_log();
    assert!(write(cx, v, t).is_none(), "{src}");
    assert!(!unsafe { JS_IsExceptionPending(cx) }, "{src}");
    let log = take_log();
    assert!(log.contains(&format!("error {error}")), "{src}: {log:?}");
}

// Embedder callbacks, with a host class whose objects carry an id in a reserved slot.

static HOST_CLASS: JSClass = JSClass {
    name: c"Host".as_ptr(),
    flags: (1 & JSCLASS_RESERVED_SLOTS_MASK) << JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

const HOST_TAG: u32 = 0xFFFF8001;
const HOST_TRANSFER_TAG: u32 = 0xFFFF8002;

thread_local!(static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) });

fn log(entry: String) {
    LOG.with(|l| l.borrow_mut().push(entry));
}

fn take_log() -> Vec<String> {
    LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

unsafe fn new_host(cx: *mut RawJSContext, id: i32) -> *mut JSObject {
    let obj = JS_NewObject(cx, &HOST_CLASS);
    JS_SetReservedSlot(obj, 0, &Int32Value(id));
    obj
}

unsafe fn host_id(obj: *mut JSObject) -> Option<i32> {
    if !ptr::eq(get_object_class(obj), &HOST_CLASS) {
        return None;
    }
    let mut v = UndefinedValue();
    JS_GetReservedSlot(obj, 0, &mut v);
    Some(v.to_int32())
}

unsafe extern "C" fn write_callback(
    cx: *mut RawJSContext,
    w: *mut JSStructuredCloneWriter,
    obj: RawHandleObject,
    _same_process_scope_required: *mut bool,
    _closure: *mut c_void,
) -> bool {
    let Some(id) = host_id(*obj.ptr) else {
        log("write rejected".into());
        return false;
    };
    log(format!("write {id}"));
    let s = JS_NewStringCopyZ(cx, c"host string".as_ptr());
    let typed = ObjectValue(JS_NewUint8Array(cx, 3));
    JS_WriteUint32PairUnchecked(w, HOST_TAG, id as u32)
        && JS_WriteBytes(w, b"payload!".as_ptr() as *const c_void, 8)
        && JS_WriteString(w, Handle::from_marked_location(&s))
        && JS_WriteDouble(w, 2.5)
        && JS_WriteTypedArray(w, Handle::from_marked_location(&typed))
}

unsafe extern "C" fn read_callback(
    cx: *mut RawJSContext,
    r: *mut JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    data: u32,
    _closure: *mut c_void,
) -> *mut JSObject {
    assert_eq!(tag, HOST_TAG);
    let mut bytes = [0u8; 8];
    assert!(JS_ReadBytes(r, bytes.as_mut_ptr() as *mut c_void, 8));
    assert_eq!(&bytes, b"payload!");
    let mut s: *mut JSString = ptr::null_mut();
    assert!(JS_ReadString(
        r,
        MutableHandle::from_marked_location(&mut s)
    ));
    assert_eq!(JS_GetStringLength(s), "host string".len());
    let mut d = 0.0;
    assert!(JS_ReadDouble(r, &mut d));
    assert_eq!(d, 2.5);
    let mut typed = UndefinedValue();
    assert!(JS_ReadTypedArray(
        r,
        MutableHandle::from_marked_location(&mut typed)
    ));
    assert!(typed.is_object());
    log(format!("read {data}"));
    new_host(cx, data as i32 + 1000)
}

unsafe extern "C" fn report_error_callback(
    _cx: *mut RawJSContext,
    errorid: u32,
    _closure: *mut c_void,
    message: *const std::os::raw::c_char,
) {
    assert!(!CStr::from_ptr(message).to_bytes().is_empty());
    log(format!("error {errorid}"));
}

unsafe extern "C" fn can_transfer_callback(
    _cx: *mut RawJSContext,
    obj: RawHandleObject,
    _same_process_scope_required: *mut bool,
    _closure: *mut c_void,
) -> bool {
    host_id(*obj.ptr).is_some()
}

unsafe extern "C" fn write_transfer_callback(
    _cx: *mut RawJSContext,
    obj: RawHandleObject,
    _closure: *mut c_void,
    tag: *mut u32,
    ownership: *mut TransferableOwnership,
    content: *mut *mut c_void,
    extra_data: *mut u64,
) -> bool {
    let id = host_id(*obj.ptr).unwrap();
    if id < 0 {
        log("transfer rejected".into());
        return false;
    }
    log(format!("transfer {id}"));
    *tag = HOST_TRANSFER_TAG;
    *ownership = TransferableOwnership::SCTAG_TMO_CUSTOM;
    *content = ptr::null_mut();
    *extra_data = id as u64;
    true
}

unsafe extern "C" fn read_transfer_callback(
    cx: *mut RawJSContext,
    _r: *mut JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    _content: *mut c_void,
    extra_data: u64,
    _closure: *mut c_void,
    return_object: MutableHandleObject,
) -> bool {
    assert_eq!(tag, HOST_TRANSFER_TAG);
    log(format!("receive {extra_data}"));
    *return_object.ptr = new_host(cx, extra_data as i32 + 2000);
    true
}

unsafe extern "C" fn free_transfer_callback(
    tag: u32,
    ownership: TransferableOwnership,
    _content: *mut c_void,
    extra_data: u64,
    _closure: *mut c_void,
) {
    assert_eq!(tag, HOST_TRANSFER_TAG);
    assert_eq!(ownership, TransferableOwnership::SCTAG_TMO_CUSTOM);
    log(format!("free {extra_data}"));
}

static CALLBACKS: JSStructuredCloneCallbacks = JSStructuredCloneCallbacks {
    read: Some(read_callback),
    write: Some(write_callback),
    reportError: Some(report_error_callback),
    readTransfer: Some(read_transfer_callback),
    writeTransfer: Some(write_transfer_callback),
    freeTransfer: Some(free_transfer_callback),
    canTransfer: Some(can_transfer_callback),
    sabCloned: None,
};

#[test]
fn primitives() {
    with_global(|cx, global| {
        let cases = [
            "undefined",
            "null",
            "true",
            "false",
            "42",
            "-0",
            "NaN",
            "-1.5e300",
            "Infinity",
            "''",
            "'latin1 \\xe9'",
            "'wide \\u{1F600} lone \\uD800 end'",
            "'x'.repeat(100000)",
            "'a' + 'b'.repeat(1000)",
            "0n",
            "-1n",
            "2n ** 64n - 1n",
            "-(2n ** 200n) + 12345n",
            "-(2n ** 63n)",
        ];
        for case in cases {
            roundtrip(cx, global, case);
            check(cx, global, "Object.is(input, out)");
        }
    });
}

#[test]
fn wrappers_dates_and_regexps() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "[new Number(-0), new String('s\\u{1F600}'), new Boolean(false), Object(-(2n ** 70n)),
              new Date(1234567890123), new Date(NaN), /a+(?<n>b)\\//dgimsy, /[x]/v]",
        );
        check(
            cx,
            global,
            "out[0] instanceof Number && Object.is(out[0].valueOf(), -0)",
        );
        check(
            cx,
            global,
            "out[1] instanceof String && out[1].valueOf() === input[1].valueOf() && out[1].length === 3",
        );
        check(
            cx,
            global,
            "out[2] instanceof Boolean && out[2].valueOf() === false",
        );
        check(
            cx,
            global,
            "typeof out[3] === 'object' && out[3].valueOf() === -(2n ** 70n)",
        );
        check(
            cx,
            global,
            "out[4] instanceof Date && out[4].getTime() === 1234567890123",
        );
        check(
            cx,
            global,
            "out[5] instanceof Date && isNaN(out[5].getTime())",
        );
        check(
            cx,
            global,
            "out[6] instanceof RegExp && out[6].source === input[6].source && out[6].flags === 'dgimsy'",
        );
        check(
            cx,
            global,
            "out[6].lastIndex === 0 && out[6].exec('aab/').groups.n === 'b'",
        );
        check(cx, global, "out[7].flags === 'v' && out[7].test('x')");
        // Internal slots are read without calling user code.
        roundtrip(
            cx,
            global,
            "Number.prototype.valueOf = () => { throw 1 }; Date.prototype.getTime = () => { throw 2 };
             Object.defineProperty(RegExp.prototype, 'flags', { get() { throw 3 } });
             [new Number(5), new Date(5), /x/g]",
        );
        check(
            cx,
            global,
            "Number.prototype.toFixed.call(out[0], 0) === '5' && Date.prototype.toISOString.call(out[1]).endsWith('.005Z') && out[2].global",
        );
    });
}

#[test]
fn arrays_and_objects() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "let a = [1, , 'three', { four: 4 }]; a.length = 6; a.extra = 'yes'; a[-1] = 'neg'; a",
        );
        check(
            cx,
            global,
            "Array.isArray(out) && out.length === 6 && !(1 in out) && !(5 in out)",
        );
        check(
            cx,
            global,
            "out[0] === 1 && out[2] === 'three' && out[3].four === 4",
        );
        check(cx, global, "out.extra === 'yes' && out[-1] === 'neg'");
        check(cx, global, "Object.keys(out).join() === '0,2,3,extra,-1'");

        roundtrip(
            cx,
            global,
            "class C { constructor() { this.z = 1; } method() {} }
             let o = new C();
             o.b = 2; o[1] = 'one'; o[0] = 'zero'; o.a = undefined;
             Object.defineProperty(o, 'hidden', { value: 1, enumerable: false });
             o[Symbol('s')] = 3;
             Object.defineProperty(o, 'computed', { get() { return 'got'; }, enumerable: true });
             o",
        );
        check(
            cx,
            global,
            "Object.getPrototypeOf(out) === Object.prototype && !('method' in out)",
        );
        check(
            cx,
            global,
            "Object.keys(out).join() === '0,1,z,b,a,computed'",
        );
        check(
            cx,
            global,
            "'a' in out && out.a === undefined && out.computed === 'got'",
        );
        check(
            cx,
            global,
            "!('hidden' in out) && Object.getOwnPropertySymbols(out).length === 0",
        );
        check(
            cx,
            global,
            "Object.getOwnPropertyDescriptor(out, 'computed').value === 'got'",
        );

        // A getter deleting a later property: it is skipped.
        roundtrip(
            cx,
            global,
            "({ get first() { delete this.second; return 1; }, second: 2, third: 3 })",
        );
        check(cx, global, "Object.keys(out).join() === 'first,third'");
    });
}

#[test]
fn maps_and_sets() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "let key = { k: 1 };
             let m = new Map([[1, 'one'], [key, new Set([1, 'two', key])], [-0, 'zero'], [NaN, 'nan']]);
             m.delete(1); m.set(1, 'again'); m.extra = 'not copied'; m",
        );
        check(
            cx,
            global,
            "out instanceof Map && out.size === 4 && !('extra' in out)",
        );
        check(
            cx,
            global,
            "[...out.keys()].map(k => typeof k).join() === 'object,number,number,number'",
        );
        check(
            cx,
            global,
            "out.get(0) === 'zero' && out.get(NaN) === 'nan' && out.get(1) === 'again'",
        );
        check(
            cx,
            global,
            "let [k, s] = [...out][0]; k.k === 1 && s instanceof Set && s.size === 3 && s.has(k) && s.has('two')",
        );
        // Mutations by user code while serializing don't affect the snapshot.
        roundtrip(
            cx,
            global,
            "let s = new Set(); s.add({ get x() { s.add('late'); return 1; } }); s.add(2); s",
        );
        check(
            cx,
            global,
            "out.size === 2 && !out.has('late') && [...out][0].x === 1 && input.has('late')",
        );
    });
}

#[test]
fn buffers_and_views() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "let b = new ArrayBuffer(16);
             new Uint8Array(b).set([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
             ({ b, u8: new Uint8Array(b, 4, 8), dv: new DataView(b, 2, 4), i16: new Int16Array(b, 2),
                own: new Float64Array([1.5, -0]), big: new BigInt64Array([-5n]),
                all: [Int8Array, Uint8ClampedArray, Uint16Array, Int32Array, Uint32Array, Float32Array,
                      Float16Array, BigUint64Array].map(C => new C(2)),
                resizable: new ArrayBuffer(4, { maxByteLength: 64 }), empty: new ArrayBuffer(0) })",
        );
        check(
            cx,
            global,
            "out.b instanceof ArrayBuffer && out.b.byteLength === 16 && new Uint8Array(out.b)[15] === 16",
        );
        check(
            cx,
            global,
            "out.u8.buffer === out.b && out.u8.byteOffset === 4 && out.u8.length === 8 && out.u8[0] === 5",
        );
        check(
            cx,
            global,
            "out.dv.buffer === out.b && out.dv.byteOffset === 2 && out.dv.byteLength === 4 && out.dv.getUint8(0) === 3",
        );
        check(
            cx,
            global,
            "out.i16.buffer === out.b && out.i16.length === 7",
        );
        check(
            cx,
            global,
            "out.own[0] === 1.5 && Object.is(out.own[1], -0) && out.big[0] === -5n",
        );
        check(
            cx,
            global,
            "out.all.every((a, i) => a.constructor === input.all[i].constructor && a.length === 2)",
        );
        check(
            cx,
            global,
            "out.resizable.resizable && out.resizable.maxByteLength === 64 && out.resizable.byteLength === 4",
        );
        check(cx, global, "out.empty.byteLength === 0 && !out.b.resizable");
        // The source is unchanged: buffers are copied, not shared.
        check(
            cx,
            global,
            "out.u8[0] = 99, new Uint8Array(input.b)[4] === 5",
        );
    });
}

#[test]
fn errors() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "let custom = new Error('custom'); custom.name = 'Custom';
             let plain = new RangeError(); plain.extra = 1;
             [new TypeError('msg', { cause: { why: 1 } }), plain, custom, new EvalError(42),
              new URIError('u'), new SyntaxError('s'), new ReferenceError('r'),
              new AggregateError([1, new Error('inner')], 'agg'), new Error('e')]",
        );
        check(
            cx,
            global,
            "Object.getPrototypeOf(out[0]) === TypeError.prototype && out[0].message === 'msg'",
        );
        check(
            cx,
            global,
            "out[0].cause.why === 1 && !Object.getOwnPropertyDescriptor(out[0], 'message').enumerable",
        );
        check(
            cx,
            global,
            "out[1] instanceof RangeError && !out[1].hasOwnProperty('message') && !('extra' in out[1])",
        );
        check(
            cx,
            global,
            "Object.getPrototypeOf(out[2]) === Error.prototype && out[2].message === 'custom' && out[2].name === 'Error'",
        );
        check(
            cx,
            global,
            "out[3] instanceof EvalError && out[3].message === '42'",
        );
        check(
            cx,
            global,
            "out[4] instanceof URIError && out[5] instanceof SyntaxError && out[6] instanceof ReferenceError",
        );
        check(
            cx,
            global,
            "out[7] instanceof AggregateError && out[7].errors[0] === 1 && out[7].errors[1].message === 'inner'",
        );
        check(
            cx,
            global,
            "typeof out[8].stack === 'string' && out[8].stack === input[8].stack",
        );
        check(
            cx,
            global,
            "out[0].hasOwnProperty('cause') && !out[8].hasOwnProperty('cause')",
        );
    });
}

#[test]
fn cycles_and_shared_references() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "let o = { name: 'o' }; o.self = o; o.list = [o, 1]; let s = { shared: true }; o.x = s; o.y = s;
             let m = new Map(); m.set(m, o); o.m = m; let e = new Error('cycle', { cause: o }); o.e = e;
             let d = new Date(0); o.dates = [d, d]; o",
        );
        check(
            cx,
            global,
            "out !== input && out.self === out && out.list[0] === out && out.list[1] === 1",
        );
        check(
            cx,
            global,
            "out.x === out.y && out.x.shared && out.x !== input.x",
        );
        check(
            cx,
            global,
            "out.m.get(out.m) === out && out.e.cause === out && out.dates[0] === out.dates[1]",
        );
    });
}

#[test]
fn unsupported_values() {
    with_global(|cx, global| {
        // Symbols are reported; objects without builtin support go to the write callback.
        expect_reported(
            cx,
            global,
            "Symbol('s')",
            "undefined",
            JS_SCERR_UNSUPPORTED_TYPE,
        );
        expect_reported(
            cx,
            global,
            "({ a: [Symbol.iterator] })",
            "undefined",
            JS_SCERR_UNSUPPORTED_TYPE,
        );
        for src in [
            "(function f() {})",
            "({ f: () => 1 })",
            "new Proxy({}, {})",
            "new WeakMap()",
            "Promise.resolve()",
            "Object(Symbol())",
        ] {
            let v = eval(cx, global, src);
            take_log();
            assert!(write(cx, v, UndefinedValue()).is_none(), "{src}");
            assert!(!unsafe { JS_IsExceptionPending(cx) }, "{src}");
            assert_eq!(take_log(), ["write rejected"], "{src}");
        }
        expect_reported(
            cx,
            global,
            "new SharedArrayBuffer(4)",
            "undefined",
            JS_SCERR_NOT_CLONABLE,
        );
        expect_reported(
            cx,
            global,
            "let detached = new ArrayBuffer(4); let view = new Uint8Array(detached); detached.transfer(); view",
            "undefined",
            JS_SCERR_TYPED_ARRAY_DETACHED,
        );

        // Exceptions from user code propagate.
        let v = eval(cx, global, "({ get boom() { throw new Error('boom'); } })");
        assert!(write(cx, v, UndefinedValue()).is_none());
        assert!(unsafe { JS_IsExceptionPending(cx) });
        unsafe { JS_ClearPendingException(cx) };

        // Without callbacks, unsupported values throw a TypeError.
        let v = eval(cx, global, "(function f() {})");
        rooted!(&in(cx) let v = v);
        rooted!(&in(cx) let mut out = UndefinedValue());
        assert!(!unsafe {
            JS_StructuredClone(
                cx,
                v.handle(),
                out.handle_mut(),
                ptr::null(),
                ptr::null_mut(),
            )
        });
        assert!(unsafe { JS_IsExceptionPending(cx) });
        unsafe { JS_ClearPendingException(cx) };
    });
}

#[test]
fn host_objects() {
    with_global(|cx, global| unsafe {
        let host = new_host(cx.raw_cx(), 7);
        set_global(cx, global, c"host", ObjectValue(host));
        take_log();
        roundtrip(cx, global, "[host, { again: host }, 'after']");
        assert_eq!(take_log(), ["write 7", "read 7"]);
        check(
            cx,
            global,
            "out[0] !== host && out[1].again === out[0] && out[2] === 'after'",
        );
        let copy = eval(cx, global, "out[0]").to_object();
        assert_eq!(host_id(copy), Some(1007));
    });
}

#[test]
fn transfer_array_buffer() {
    with_global(|cx, global| {
        let input = eval(
            cx,
            global,
            "var b = new ArrayBuffer(8); new Uint8Array(b).set([9, 8, 7]);
             var r = new ArrayBuffer(2, { maxByteLength: 8 });
             ({ b, view: new Uint8Array(b, 1, 2), r, tracking: new Uint16Array(r), dv: new DataView(r, 1),
                fixed: new Uint8Array(r, 0, 2) })",
        );
        let transfer = eval(cx, global, "[b, r]");
        let bytes = write(cx, input, transfer).unwrap();
        check(cx, global, "b.detached && b.byteLength === 0 && r.detached");
        let out = read(cx, &bytes).unwrap();
        set_global(cx, global, c"out", out);
        check(
            cx,
            global,
            "out.b.byteLength === 8 && new Uint8Array(out.b)[0] === 9",
        );
        check(
            cx,
            global,
            "out.view.buffer === out.b && out.view[0] === 8 && out.view.byteOffset === 1",
        );
        check(cx, global, "out.r.resizable && out.r.maxByteLength === 8");
        // Length-tracking views stay length-tracking.
        check(
            cx,
            global,
            "out.r.resize(6); out.tracking.length === 3 && out.dv.byteLength === 5 && out.fixed.length === 2",
        );
        // Each copy of the bytes can be read once.
        assert!(read(cx, &bytes).is_some());

        // A transferred buffer that is not otherwise referenced.
        let input = eval(cx, global, "var lone = new ArrayBuffer(3); 'message'");
        let transfer = eval(cx, global, "[lone]");
        let bytes = write(cx, input, transfer).unwrap();
        check(cx, global, "lone.detached");
        assert!(read(cx, &bytes).unwrap().is_string());
    });
}

#[test]
fn transfer_host_objects() {
    with_global(|cx, global| unsafe {
        let port = new_host(cx.raw_cx(), 3);
        set_global(cx, global, c"port", ObjectValue(port));
        let bad = new_host(cx.raw_cx(), -1);
        set_global(cx, global, c"bad", ObjectValue(bad));
        let input = eval(cx, global, "({ port, ports: [port] })");
        let transfer = eval(cx, global, "[port]");
        take_log();
        let bytes = write(cx, input, transfer).unwrap();
        // The writer's buffer is deleted with the transferable unread: it is freed.
        assert_eq!(take_log(), ["transfer 3", "free 3"]);
        let out = read(cx, &bytes).unwrap();
        assert_eq!(take_log(), ["receive 3"]);
        set_global(cx, global, c"out", out);
        check(cx, global, "out.port === out.ports[0] && out.port !== port");
        assert_eq!(
            host_id(eval(cx, global, "out.port").to_object()),
            Some(2003)
        );

        // A failing writeTransfer frees the transferables already transferred.
        let input = eval(cx, global, "1");
        let transfer = eval(cx, global, "[port, bad]");
        assert!(write(cx, input, transfer).is_none());
        assert_eq!(take_log(), ["transfer 3", "transfer rejected", "free 3"]);

        // Reading the same buffer twice.
        let input = eval(cx, global, "port");
        let transfer = eval(cx, global, "[port]");
        rooted!(&in(cx) let input = input);
        rooted!(&in(cx) let transfer = transfer);
        let buf = JSAutoStructuredCloneBufferWrapper::new(SCOPE, &CALLBACKS);
        let data = &mut (*buf.as_raw_ptr()).data_;
        assert!(JS_WriteStructuredClone(
            cx,
            input.handle(),
            data,
            SCOPE,
            &POLICY,
            &CALLBACKS,
            ptr::null_mut(),
            transfer.handle()
        ));
        rooted!(&in(cx) let mut rval = UndefinedValue());
        assert!(JS_ReadStructuredClone(
            cx,
            data,
            JS_STRUCTURED_CLONE_VERSION,
            SCOPE,
            rval.handle_mut(),
            &POLICY,
            &CALLBACKS,
            ptr::null_mut()
        ));
        take_log();
        assert!(!JS_ReadStructuredClone(
            cx,
            data,
            JS_STRUCTURED_CLONE_VERSION,
            SCOPE,
            rval.handle_mut(),
            &POLICY,
            &CALLBACKS,
            ptr::null_mut()
        ));
        assert_eq!(take_log(), [format!("error {JS_SCERR_TRANSFERABLE_TWICE}")]);
        drop(buf);
        // Read transferables are not freed.
        assert!(take_log().is_empty());
    });
}

#[test]
fn transfer_errors() {
    with_global(|cx, global| {
        eval(
            cx,
            global,
            "var ab = new ArrayBuffer(1); var gone = new ArrayBuffer(1); gone.transfer()",
        );
        expect_reported(cx, global, "1", "[ab, ab]", JS_SCERR_DUP_TRANSFERABLE);
        expect_reported(cx, global, "1", "ab", JS_SCERR_TRANSFERABLE);
        expect_reported(cx, global, "1", "[1]", JS_SCERR_TRANSFERABLE);
        expect_reported(cx, global, "1", "[{}]", JS_SCERR_TRANSFERABLE);
        expect_reported(
            cx,
            global,
            "1",
            "[new SharedArrayBuffer(1)]",
            JS_SCERR_SHMEM_TRANSFERABLE,
        );
        expect_reported(cx, global, "1", "[gone]", JS_SCERR_TYPED_ARRAY_DETACHED);
        // Detached by user code while serializing (HTML checks after serialization).
        expect_reported(
            cx,
            global,
            "({ get x() { ab.transfer(); return 1; } })",
            "[ab]",
            JS_SCERR_TYPED_ARRAY_DETACHED,
        );
    });
}

/// Servo moves the bytes to another thread, with its own runtime.
#[test]
fn other_runtime() {
    let mut bytes = None;
    with_global(|cx, global| {
        let input = eval(
            cx,
            global,
            "var buffer = new Uint16Array([1, 2, 3]).buffer;
             let o = { text: 'héllo \\u{1F600}', buffer, view: new Uint16Array(buffer, 2), date: new Date(5),
                       map: new Map([['k', 10n ** 30n]]), re: /re/u };
             o.self = o; o",
        );
        let transfer = eval(cx, global, "[buffer]");
        bytes = write(cx, input, transfer);
    });
    let bytes = bytes.unwrap();
    std::thread::spawn(move || {
        with_global(|cx, global| {
            let out = read(cx, &bytes).unwrap();
            set_global(cx, global, c"out", out);
            check(cx, global, "out.text === 'héllo \\u{1F600}' && out.self === out && out.date.getTime() === 5");
            check(cx, global, "out.view.buffer === out.buffer && out.view[0] === 2 && out.view.length === 2");
            check(cx, global, "out.map.get('k') === 10n ** 30n && out.re.unicode && out.re.source === 're'");
            check(cx, global, "out.buffer instanceof ArrayBuffer && Object.getPrototypeOf(out) === Object.prototype");
        });
    })
    .join()
    .unwrap();
}

#[test]
fn structured_clone_api() {
    with_global(|cx, global| unsafe {
        let v = eval(
            cx,
            global,
            "var src = { a: [1, 2], d: new Date(3) }; src.a.push(src); src",
        );
        rooted!(&in(cx) let v = v);
        rooted!(&in(cx) let mut out = UndefinedValue());
        assert!(JS_StructuredClone(
            cx,
            v.handle(),
            out.handle_mut(),
            &CALLBACKS,
            ptr::null_mut()
        ));
        set_global(cx, global, c"out", out.get());
        check(
            cx,
            global,
            "out !== src && out.a[2] === out && out.d.getTime() === 3",
        );
        rooted!(&in(cx) let s = eval(cx, global, "'str'"));
        assert!(JS_StructuredClone(
            cx,
            s.handle(),
            out.handle_mut(),
            ptr::null(),
            ptr::null_mut()
        ));
        assert!(out.get().is_string());
    });
}

/// Like SpiderMonkey, depth is not limited by the native stack.
#[test]
fn deep_nesting() {
    with_global(|cx, global| {
        roundtrip(
            cx,
            global,
            "let a = []; let m = new Map(); let e = new Error('e');
             for (let i = 0; i < 100000; i++) {
                 a = [a, { o: a }];
                 m = new Map([[m, new Set([m])]]);
                 e = new Error('e', { cause: e });
             }
             ({ a, m, e })",
        );
        check(
            cx,
            global,
            "let d = 0; for (let x = out.a; x.length; x = x[0]) { if (x[1].o !== x[0]) throw 1; d++; } d === 100000",
        );
        check(
            cx,
            global,
            "let d = 0; for (let x = out.m; x.size; x = [...x.keys()][0]) { if (!x.get([...x.keys()][0]).has([...x.keys()][0])) throw 1; d++; } d === 100000",
        );
        check(
            cx,
            global,
            "let d = 0; for (let x = out.e; x.cause; x = x.cause) d++; d === 100000",
        );
    });
}

#[test]
fn corrupted_data() {
    with_global(|cx, global| {
        let v = eval(
            cx,
            global,
            "({ a: [1, 'two', new Map([[3, 4]])], b: new Uint8Array(4) })",
        );
        let bytes = write(cx, v, UndefinedValue()).unwrap();
        for len in 0..bytes.len() {
            assert!(read(cx, &bytes[..len]).is_none(), "{len}");
            assert!(unsafe { JS_IsExceptionPending(cx) });
            unsafe { JS_ClearPendingException(cx) };
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(read(cx, &extra).is_none());
        unsafe { JS_ClearPendingException(cx) };
        assert!(read(cx, &bytes).is_some());
    });
}
