/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Structured clone: SpiderMonkey's JS_WriteStructuredClone/JS_ReadStructuredClone API with a
//! shim-specific format, following HTML's StructuredSerializeInternal/StructuredDeserialize.
//!
//! Format (little-endian): a header (magic, format version, transfer state, transfer count),
//! the transfer map, then the value. A value is a tag byte and a payload. Objects get
//! consecutive indices in the order their tags are written, transferred objects first; later
//! occurrences are back-references. A host object is `T_HOST` followed by whatever the
//! embedder's write callback writes; its first `JS_WriteUint32PairUnchecked` is the (tag, data)
//! handed to the read callback. Transferred ArrayBuffers are copied into the transfer map and
//! the source is detached, so the data is self-contained for every scope. Like SpiderMonkey,
//! nested containers are handled with an explicit stack instead of recursion.
//!
//! A `JSStructuredCloneData` keeps a `Box<Vec<u8>>` in its first word (SpiderMonkey's buffer
//! list); its scope, callbacks, closure and transferable ownership fields are used as in
//! SpiderMonkey.

use std::collections::HashMap;
use std::ffi::{c_int, c_void, CStr};
use std::marker::PhantomData;
use std::ptr;

use servo_quickjs as q;

use super::{obj_val, object_realm, val_obj, Cx};
use crate::sys::jsapi as root;

const MAGIC: u32 = u32::from_le_bytes(*b"QJSC");
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const TRANSFER_STATE_OFFSET: usize = 8;
const TRANSFER_NONE: u32 = 0;
const TRANSFER_UNREAD: u32 = 1;
const TRANSFER_CONSUMED: u32 = 2;

const XFER_ARRAY_BUFFER: u32 = 1;
const XFER_CUSTOM: u32 = 2;

const T_UNDEFINED: u8 = 0;
const T_NULL: u8 = 1;
const T_FALSE: u8 = 2;
const T_TRUE: u8 = 3;
const T_INT32: u8 = 4;
const T_DOUBLE: u8 = 5;
const T_BIGINT: u8 = 6;
const T_STRING: u8 = 7;
/// A property key that is an array index.
const T_INDEX: u8 = 8;
const T_BACKREF: u8 = 9;
/// Ends the properties of an object or array and the entries of a Map or Set.
const T_END: u8 = 10;
const T_OBJECT: u8 = 11;
const T_ARRAY: u8 = 12;
const T_NUMBER_OBJECT: u8 = 13;
const T_STRING_OBJECT: u8 = 14;
const T_BOOLEAN_OBJECT: u8 = 15;
const T_BIGINT_OBJECT: u8 = 16;
const T_DATE: u8 = 17;
const T_REGEXP: u8 = 18;
const T_MAP: u8 = 19;
const T_SET: u8 = 20;
const T_ERROR: u8 = 21;
const T_ARRAY_BUFFER: u8 = 22;
const T_TYPED_ARRAY: u8 = 23;
/// The length of a length-tracking view.
const TRACKING_LENGTH: u64 = u64::MAX;
const T_DATA_VIEW: u8 = 24;
const T_HOST: u8 = 25;

/// QuickJS's internal class ids (`JS_CLASS_*` in quickjs.c).
const CLASS_OBJECT: q::JSClassID = 1;
const CLASS_ARRAY: q::JSClassID = 2;
const CLASS_ERROR: q::JSClassID = 3;
const CLASS_NUMBER: q::JSClassID = 4;
const CLASS_STRING: q::JSClassID = 5;
const CLASS_BOOLEAN: q::JSClassID = 6;
const CLASS_DATE: q::JSClassID = 10;
const CLASS_REGEXP: q::JSClassID = 18;
const CLASS_BIG_INT: q::JSClassID = 34;
const CLASS_MAP: q::JSClassID = 35;
const CLASS_SET: q::JSClassID = 36;

/// Error names HTML preserves (plus AggregateError, like SpiderMonkey) and their QuickJS
/// `JSErrorEnum`; -1 is plain `Error`.
const ERROR_KINDS: [(&str, i8); 8] = [
    ("Error", -1),
    ("EvalError", 0),
    ("RangeError", 1),
    ("ReferenceError", 2),
    ("SyntaxError", 3),
    ("TypeError", 4),
    ("URIError", 5),
    ("AggregateError", 7),
];
const AGGREGATE_ERROR: i8 = 7;

/// Element sizes by `JSTypedArrayEnum`.
const ELEMENT_SIZES: [usize; 12] = [1, 1, 1, 2, 2, 4, 4, 8, 8, 2, 4, 8];

/// An owned QuickJS value, freed on drop.
struct Owned {
    v: q::JSValue,
    rt: *mut q::JSRuntime,
}

impl Owned {
    fn new(cx: &Cx, v: q::JSValue) -> Owned {
        Owned { v, rt: cx.rt }
    }

    fn take(self) -> q::JSValue {
        let v = self.v;
        std::mem::forget(self);
        v
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { q::JS_FreeValueRT(self.rt, self.v) };
    }
}

fn is_exception(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_EXCEPTION
}

fn is_string(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_STRING || v.tag() == q::JS_TAG_STRING_ROPE
}

fn number(v: q::JSValue) -> f64 {
    if v.tag() == q::JS_TAG_INT {
        v.int() as f64
    } else {
        v.float64()
    }
}

fn scerr_message(id: u32) -> &'static CStr {
    match id {
        root::JS_SCERR_DUP_TRANSFERABLE => c"duplicate transferable for structured clone",
        root::JS_SCERR_TRANSFERABLE => c"invalid transferable array for structured clone",
        root::JS_SCERR_SHMEM_TRANSFERABLE => c"Shared memory objects must not be in the transfer list",
        root::JS_SCERR_TRANSFERABLE_TWICE => c"structured clone cannot transfer twice",
        root::JS_SCERR_TYPED_ARRAY_DETACHED => c"attempting to access detached ArrayBuffer",
        root::JS_SCERR_NOT_CLONABLE => c"The SharedArrayBuffer object cannot be serialized. The Cross-Origin-Opener-Policy and Cross-Origin-Embedder-Policy HTTP headers will enable this in the future.",
        _ => c"unsupported type for structured data",
    }
}

/// SpiderMonkey's `ReportDataCloneError`: the `reportError` callback if there is one (no
/// exception is set), otherwise a TypeError.
unsafe fn report_error(
    cx: &Cx,
    callbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut c_void,
    id: u32,
) {
    let message = scerr_message(id);
    match callbacks.as_ref().and_then(|c| c.reportError) {
        Some(report) => report(cx.as_raw(), id, closure, message.as_ptr()),
        None => {
            q::JS_ThrowTypeError(cx.qctx(), c"%s".as_ptr(), message.as_ptr());
        }
    }
}

unsafe fn bad_data(cx: &Cx, what: &CStr) {
    q::JS_ThrowInternalError(
        cx.qctx(),
        c"bad serialized structured data (%s)".as_ptr(),
        what.as_ptr(),
    );
}

/// A flat string for a string or rope value; `None` for anything else.
unsafe fn flat_string(cx: &Cx, v: q::JSValue) -> Option<Owned> {
    match v.tag() {
        q::JS_TAG_STRING => Some(Owned::new(cx, q::JS_DupValueRT(cx.rt, v))),
        q::JS_TAG_STRING_ROPE => {
            let s = q::JS_ToString(cx.qctx(), v);
            (!is_exception(s)).then(|| Owned::new(cx, s))
        }
        _ => None,
    }
}

/// Whether the flat string `s` equals the ASCII string `name`.
unsafe fn string_eq(s: q::JSValue, name: &str) -> bool {
    let mut data = ptr::null();
    let mut wide = 0;
    let len = q::JS_GetStringData(s, &mut data, &mut wide);
    if len != name.len() {
        return false;
    }
    if wide != 0 {
        let units = std::slice::from_raw_parts(data as *const u16, len);
        units.iter().zip(name.bytes()).all(|(&u, b)| u == b as u16)
    } else {
        std::slice::from_raw_parts(data as *const u8, len) == name.as_bytes()
    }
}

unsafe fn array_length(cx: &Cx, arr: q::JSValue) -> Option<u32> {
    let len = q::JS_GetPropertyStr(cx.qctx(), arr, c"length".as_ptr());
    if is_exception(len) {
        return None;
    }
    let mut n = 0;
    let r = q::JS_ToUint32(cx.qctx(), &mut n, len);
    q::JS_FreeValueRT(cx.rt, len);
    (r >= 0).then_some(n)
}

/// The value of the own data property `name` (`None` if missing or an accessor).
unsafe fn own_data_property(cx: &Cx, obj: q::JSValue, name: &CStr) -> Result<Option<Owned>, ()> {
    let ctx = cx.qctx();
    let atom = q::JS_NewAtom(ctx, name.as_ptr());
    let mut desc: q::JSPropertyDescriptor = std::mem::zeroed();
    let r = q::JS_GetOwnProperty(ctx, &mut desc, obj, atom);
    q::JS_FreeAtom(ctx, atom);
    if r < 0 {
        return Err(());
    }
    if r == 0 {
        return Ok(None);
    }
    let value = Owned::new(cx, desc.value);
    drop((Owned::new(cx, desc.getter), Owned::new(cx, desc.setter)));
    Ok((desc.flags as u32 & q::JS_PROP_GETSET == 0).then_some(value))
}

fn transfer_ownership(v: u32) -> root::JS::TransferableOwnership {
    use root::JS::TransferableOwnership::*;
    match v {
        0 => SCTAG_TMO_UNFILLED,
        1 => SCTAG_TMO_UNOWNED,
        2 => SCTAG_TMO_FIRST_OWNED,
        3 => SCTAG_TMO_MAPPED_DATA,
        4 => SCTAG_TMO_CUSTOM,
        _ => SCTAG_TMO_USER_MIN,
    }
}

// Writing.

/// Owned atoms, freed on drop.
struct Atoms {
    atoms: Vec<q::JSAtom>,
    rt: *mut q::JSRuntime,
}

impl Drop for Atoms {
    fn drop(&mut self) {
        for &atom in &self.atoms {
            unsafe { q::JS_FreeAtomRT(self.rt, atom) };
        }
    }
}

/// A container whose children are being written.
enum WriteFrame {
    /// The keys of an object or array, collected when it was started.
    Properties {
        obj: q::JSValue,
        keys: Atoms,
        next: usize,
    },
    /// A snapshot of the entries of a Map or Set.
    Entries { records: Owned, len: u32, next: u32 },
    /// The cause and errors of an Error, then its stack.
    Error {
        values: [Option<Owned>; 2],
        stack: Option<Owned>,
        next: usize,
    },
}

/// What a `JSStructuredCloneWriter*` points to.
struct Writer {
    cx: *const Cx,
    out: Vec<u8>,
    /// Objects seen so far and their indices.
    memory: HashMap<usize, u32>,
    /// One reference per object in `memory`, so that their addresses stay unique.
    held: Vec<q::JSValue>,
    transferables: Vec<q::JSValue>,
    scope: root::JS::StructuredCloneScope,
    callbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut c_void,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let rt = unsafe { (*self.cx).rt };
        for v in self.held.drain(..) {
            unsafe { q::JS_FreeValueRT(rt, v) };
        }
    }
}

impl Writer {
    fn new(
        cx: &Cx,
        scope: root::JS::StructuredCloneScope,
        callbacks: *const root::JSStructuredCloneCallbacks,
        closure: *mut c_void,
    ) -> Writer {
        Writer {
            cx,
            out: Vec::new(),
            memory: HashMap::new(),
            held: Vec::new(),
            transferables: Vec::new(),
            scope,
            callbacks,
            closure,
        }
    }

    fn cx<'a>(&self) -> &'a Cx {
        unsafe { &*self.cx }
    }

    fn ctx(&self) -> *mut q::JSContext {
        self.cx().qctx()
    }

    fn raw(&mut self) -> *mut root::JSStructuredCloneWriter {
        self as *mut Writer as *mut root::JSStructuredCloneWriter
    }

    unsafe fn report(&self, id: u32) -> bool {
        report_error(self.cx(), self.callbacks, self.closure, id);
        false
    }

    fn put_u8(&mut self, v: u8) {
        self.out.push(v);
    }

    fn put_u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    fn put_f64(&mut self, v: f64) {
        self.put_u64(v.to_bits());
    }

    unsafe fn put_raw(&mut self, p: *const u8, len: usize) {
        if len > 0 {
            self.out
                .extend_from_slice(std::slice::from_raw_parts(p, len));
        }
    }

    unsafe fn register(&mut self, v: q::JSValue) {
        let index = self.memory.len() as u32;
        self.memory.insert(v.ptr() as usize, index);
        self.held.push(q::JS_DupValueRT(self.cx().rt, v));
    }

    /// SpiderMonkey's `parseTransferable`: validates the transfer list and gives its objects the
    /// first indices.
    unsafe fn parse_transferables(&mut self, list: q::JSValue) -> bool {
        if matches!(list.tag(), q::JS_TAG_UNDEFINED | q::JS_TAG_NULL) {
            return true;
        }
        if list.tag() != q::JS_TAG_OBJECT {
            return self.report(root::JS_SCERR_TRANSFERABLE);
        }
        if !super::object::is_array_object(list) {
            return self.report(root::JS_SCERR_TRANSFERABLE);
        }
        let cx = self.cx();
        let Some(len) = array_length(cx, list) else {
            return false;
        };
        for i in 0..len {
            let item = Owned::new(cx, q::JS_GetPropertyUint32(cx.qctx(), list, i));
            if is_exception(item.v) {
                return false;
            }
            if item.v.tag() != q::JS_TAG_OBJECT {
                return self.report(root::JS_SCERR_TRANSFERABLE);
            }
            let mut info = q::JSBufferInfo::default();
            match q::JS_GetBufferInfo(item.v, &mut info) {
                q::JS_BUFFER_SHARED_ARRAY_BUFFER => {
                    return self.report(root::JS_SCERR_SHMEM_TRANSFERABLE)
                }
                q::JS_BUFFER_ARRAY_BUFFER => {}
                _ => {
                    let Some(can_transfer) = self.callbacks.as_ref().and_then(|c| c.canTransfer)
                    else {
                        return self.report(root::JS_SCERR_TRANSFERABLE);
                    };
                    let obj = val_obj(item.v);
                    let realm = object_realm(cx, obj);
                    if !realm.is_null() {
                        cx.enter_realm(realm);
                    }
                    let mut same_process = false;
                    let ok =
                        can_transfer(cx.as_raw(), handle(&obj), &mut same_process, self.closure);
                    if !realm.is_null() {
                        cx.leave_realm();
                    }
                    if !ok {
                        return self.report(root::JS_SCERR_TRANSFERABLE);
                    }
                }
            }
            if self.memory.contains_key(&(item.v.ptr() as usize)) {
                return self.report(root::JS_SCERR_DUP_TRANSFERABLE);
            }
            self.register(item.v);
            self.transferables.push(item.v);
        }
        true
    }

    /// Writes `v` and everything it references without recursion, like SpiderMonkey: a
    /// container is written as a header (`start`), then its children (`next_child`).
    unsafe fn write_value(&mut self, v: q::JSValue) -> bool {
        let mut stack = Vec::new();
        if !self.start(v, &mut stack) {
            return false;
        }
        while let Some(frame) = stack.last_mut() {
            let child = match self.next_child(frame) {
                Ok(Some(child)) => child,
                Ok(None) => {
                    stack.pop();
                    continue;
                }
                Err(()) => return false,
            };
            if !self.start(child.v, &mut stack) {
                return false;
            }
        }
        true
    }

    /// Writes a value, or the header of a container pushed on `stack`.
    unsafe fn start(&mut self, v: q::JSValue, stack: &mut Vec<WriteFrame>) -> bool {
        match v.tag() {
            q::JS_TAG_UNDEFINED => self.put_u8(T_UNDEFINED),
            q::JS_TAG_NULL => self.put_u8(T_NULL),
            q::JS_TAG_BOOL => self.put_u8(if v.int() != 0 { T_TRUE } else { T_FALSE }),
            q::JS_TAG_INT => {
                self.put_u8(T_INT32);
                self.put_u32(v.int() as u32);
            }
            q::JS_TAG_FLOAT64 => {
                self.put_u8(T_DOUBLE);
                self.put_f64(v.float64());
            }
            q::JS_TAG_STRING | q::JS_TAG_STRING_ROPE => {
                self.put_u8(T_STRING);
                return self.write_string(v);
            }
            q::JS_TAG_SHORT_BIG_INT | q::JS_TAG_BIG_INT => {
                self.put_u8(T_BIGINT);
                self.write_bigint(v);
            }
            q::JS_TAG_OBJECT => return self.start_object(v, stack),
            _ => return self.report(root::JS_SCERR_UNSUPPORTED_TYPE),
        }
        true
    }

    /// A string payload: `(length << 1) | wide` and the Latin-1 or UTF-16 code units.
    unsafe fn write_string(&mut self, v: q::JSValue) -> bool {
        let Some(s) = flat_string(self.cx(), v) else {
            return false;
        };
        let mut data = ptr::null();
        let mut wide = 0;
        let len = q::JS_GetStringData(s.v, &mut data, &mut wide);
        self.put_u32(((len as u32) << 1) | (wide != 0) as u32);
        if wide != 0 {
            let units = std::slice::from_raw_parts(data as *const u16, len);
            self.out.reserve(len * 2);
            for u in units {
                self.out.extend_from_slice(&u.to_le_bytes());
            }
        } else {
            self.put_raw(data as *const u8, len);
        }
        true
    }

    /// A BigInt payload: the count and the two's complement 32-bit words.
    unsafe fn write_bigint(&mut self, v: q::JSValue) {
        let n = q::JS_GetBigIntWords(v, ptr::null_mut(), 0);
        let mut words = vec![0u32; n];
        q::JS_GetBigIntWords(v, words.as_mut_ptr(), n);
        self.put_u32(n as u32);
        for w in words {
            self.put_u32(w);
        }
    }

    unsafe fn start_object(&mut self, v: q::JSValue, stack: &mut Vec<WriteFrame>) -> bool {
        if let Some(&index) = self.memory.get(&(v.ptr() as usize)) {
            self.put_u8(T_BACKREF);
            self.put_u32(index);
            return true;
        }
        self.register(v);
        let class = q::JS_GetClassID(v);
        let frame = match class {
            CLASS_OBJECT => {
                self.put_u8(T_OBJECT);
                self.properties(v)
            }
            CLASS_ARRAY => {
                let Some(len) = array_length(self.cx(), v) else {
                    return false;
                };
                self.put_u8(T_ARRAY);
                self.put_u32(len);
                self.properties(v)
            }
            CLASS_MAP | CLASS_SET => self.start_map(v, class == CLASS_SET),
            CLASS_ERROR => self.start_error(v),
            CLASS_NUMBER | CLASS_STRING | CLASS_BOOLEAN | CLASS_BIG_INT | CLASS_DATE => {
                return self.write_primitive_object(v, class)
            }
            CLASS_REGEXP => return self.write_regexp(v),
            _ => return self.write_other(v),
        };
        match frame {
            Some(frame) => {
                stack.push(frame);
                true
            }
            None => false,
        }
    }

    /// Writes the next key of a container and returns the child to write after it, or writes
    /// the end of the container.
    unsafe fn next_child(&mut self, frame: &mut WriteFrame) -> Result<Option<Owned>, ()> {
        let cx = self.cx();
        let ctx = cx.qctx();
        match frame {
            // Properties deleted by user code in the meantime are skipped.
            WriteFrame::Properties { obj, keys, next } => {
                while let Some(&atom) = keys.atoms.get(*next) {
                    *next += 1;
                    let r = q::JS_GetOwnProperty(ctx, ptr::null_mut(), *obj, atom);
                    if r < 0 {
                        return Err(());
                    }
                    if r == 0 {
                        continue;
                    }
                    let value = Owned::new(cx, q::JS_GetProperty(ctx, *obj, atom));
                    if is_exception(value.v) || !self.write_key(atom) {
                        return Err(());
                    }
                    return Ok(Some(value));
                }
            }
            WriteFrame::Entries { records, len, next } => {
                if *next < *len {
                    let item = Owned::new(cx, q::JS_GetPropertyUint32(ctx, records.v, *next));
                    *next += 1;
                    return if is_exception(item.v) {
                        Err(())
                    } else {
                        Ok(Some(item))
                    };
                }
            }
            WriteFrame::Error {
                values,
                stack,
                next,
            } => {
                while let Some(value) = values.get_mut(*next) {
                    *next += 1;
                    self.put_u8(value.is_some() as u8);
                    if let Some(value) = value.take() {
                        return Ok(Some(value));
                    }
                }
                self.put_u8(stack.is_some() as u8);
                if let Some(stack) = stack.take() {
                    if !self.write_string(stack.v) {
                        return Err(());
                    }
                }
                return Ok(None);
            }
        }
        self.put_u8(T_END);
        Ok(None)
    }

    unsafe fn write_primitive_object(&mut self, v: q::JSValue, class: q::JSClassID) -> bool {
        let data = Owned::new(self.cx(), q::JS_GetPrimitiveObjectData(self.ctx(), v));
        match class {
            CLASS_NUMBER => {
                self.put_u8(T_NUMBER_OBJECT);
                self.put_f64(number(data.v));
            }
            CLASS_DATE => {
                self.put_u8(T_DATE);
                self.put_f64(number(data.v));
            }
            CLASS_BOOLEAN => {
                self.put_u8(T_BOOLEAN_OBJECT);
                self.put_u8((data.v.int() != 0) as u8);
            }
            CLASS_STRING => {
                self.put_u8(T_STRING_OBJECT);
                return self.write_string(data.v);
            }
            _ => {
                self.put_u8(T_BIGINT_OBJECT);
                self.write_bigint(data.v);
            }
        }
        true
    }

    unsafe fn write_regexp(&mut self, v: q::JSValue) -> bool {
        let mut flags = q::JS_UNDEFINED;
        let source = Owned::new(
            self.cx(),
            q::JS_GetRegExpOriginal(self.ctx(), v, &mut flags),
        );
        let flags = Owned::new(self.cx(), flags);
        if is_exception(source.v) {
            return false;
        }
        self.put_u8(T_REGEXP);
        self.write_string(source.v) && self.write_string(flags.v)
    }

    unsafe fn write_other(&mut self, v: q::JSValue) -> bool {
        let mut info = q::JSBufferInfo::default();
        match q::JS_GetBufferInfo(v, &mut info) {
            // A detached buffer goes to the write callback, like in SpiderMonkey.
            q::JS_BUFFER_ARRAY_BUFFER if info.detached == 0 => self.write_array_buffer(v, &info),
            q::JS_BUFFER_SHARED_ARRAY_BUFFER => self.report(root::JS_SCERR_NOT_CLONABLE),
            kind @ (q::JS_BUFFER_TYPED_ARRAY | q::JS_BUFFER_DATA_VIEW) => {
                self.write_view(kind, &info)
            }
            _ => self.write_host(v),
        }
    }

    /// A frame for the own enumerable string-keyed properties of `obj`.
    unsafe fn properties(&mut self, obj: q::JSValue) -> Option<WriteFrame> {
        let cx = self.cx();
        let ctx = cx.qctx();
        let mut tab = ptr::null_mut();
        let mut len = 0;
        let flags = (q::JS_GPN_STRING_MASK | q::JS_GPN_ENUM_ONLY) as c_int;
        if q::JS_GetOwnPropertyNames(ctx, &mut tab, &mut len, obj, flags) < 0 {
            return None;
        }
        let mut keys = Atoms {
            atoms: Vec::with_capacity(len as usize),
            rt: cx.rt,
        };
        if len > 0 {
            keys.atoms.extend(
                std::slice::from_raw_parts(tab, len as usize)
                    .iter()
                    .map(|e| q::JS_DupAtom(ctx, e.atom)),
            );
        }
        q::JS_FreePropertyEnum(ctx, tab, len);
        Some(WriteFrame::Properties { obj, keys, next: 0 })
    }

    unsafe fn write_key(&mut self, atom: q::JSAtom) -> bool {
        if atom & (1 << 31) != 0 {
            self.put_u8(T_INDEX);
            self.put_u32(atom & 0x7fff_ffff);
            return true;
        }
        let s = Owned::new(self.cx(), q::JS_AtomToString(self.ctx(), atom));
        if is_exception(s.v) {
            return false;
        }
        self.put_u8(T_STRING);
        self.write_string(s.v)
    }

    /// A frame for a snapshot of the entries of a Map (key, value, ...) or Set.
    unsafe fn start_map(&mut self, obj: q::JSValue, is_set: bool) -> Option<WriteFrame> {
        let cx = self.cx();
        let records = Owned::new(cx, q::JS_GetMapRecords(cx.qctx(), obj));
        if is_exception(records.v) {
            return None;
        }
        let len = array_length(cx, records.v)?;
        self.put_u8(if is_set { T_SET } else { T_MAP });
        Some(WriteFrame::Entries {
            records,
            len,
            next: 0,
        })
    }

    /// HTML's name (from the allowed list) and message, plus SpiderMonkey's extensions: cause,
    /// the errors of an AggregateError, and the stack string (in a frame).
    unsafe fn start_error(&mut self, obj: q::JSValue) -> Option<WriteFrame> {
        let cx = self.cx();
        let ctx = cx.qctx();
        let name = Owned::new(cx, q::JS_GetPropertyStr(ctx, obj, c"name".as_ptr()));
        if is_exception(name.v) {
            return None;
        }
        let kind = match flat_string(cx, name.v) {
            Some(s) => ERROR_KINDS
                .iter()
                .find(|(n, _)| string_eq(s.v, n))
                .map_or(-1, |&(_, k)| k),
            None if is_string(name.v) => return None,
            None => -1,
        };
        let message = match own_data_property(cx, obj, c"message").ok()? {
            Some(m) => {
                let s = Owned::new(cx, q::JS_ToString(ctx, m.v));
                if is_exception(s.v) {
                    return None;
                }
                Some(s)
            }
            None => None,
        };
        let cause = own_data_property(cx, obj, c"cause").ok()?;
        let errors = if kind == AGGREGATE_ERROR {
            let errors = Owned::new(cx, q::JS_GetPropertyStr(ctx, obj, c"errors".as_ptr()));
            if is_exception(errors.v) {
                return None;
            }
            Some(errors)
        } else {
            None
        };
        let stack = own_data_property(cx, obj, c"stack")
            .ok()?
            .filter(|s| is_string(s.v));
        self.put_u8(T_ERROR);
        self.put_u8(kind as u8);
        self.put_u8(message.is_some() as u8);
        if let Some(m) = &message {
            if !self.write_string(m.v) {
                return None;
            }
        }
        Some(WriteFrame::Error {
            values: [cause, errors],
            stack,
            next: 0,
        })
    }

    /// Length, maximum length (`u64::MAX` if not resizable) and contents.
    unsafe fn put_array_buffer(&mut self, obj: q::JSValue, info: &q::JSBufferInfo) {
        self.put_u64(info.byte_length as u64);
        self.put_u64(q::JS_GetArrayBufferMaxByteLength(obj) as u64);
        self.put_raw(info.data, info.byte_length);
    }

    unsafe fn write_array_buffer(&mut self, obj: q::JSValue, info: &q::JSBufferInfo) -> bool {
        self.put_u8(T_ARRAY_BUFFER);
        self.put_array_buffer(obj, info);
        true
    }

    /// A typed array (type, length) or DataView (byte length), its buffer, and the byte offset.
    /// Length-tracking views have the length `TRACKING_LENGTH`.
    unsafe fn write_view(&mut self, kind: q::JSBufferKind, info: &q::JSBufferInfo) -> bool {
        if info.detached != 0 || info.data.is_null() {
            return self.report(root::JS_SCERR_TYPED_ARRAY_DETACHED);
        }
        let tracking = info.length_tracking != 0;
        if kind == q::JS_BUFFER_TYPED_ARRAY {
            let ty = info.type_ as usize;
            self.put_u8(T_TYPED_ARRAY);
            self.put_u8(ty as u8);
            self.put_u64(if tracking {
                TRACKING_LENGTH
            } else {
                (info.byte_length / ELEMENT_SIZES[ty]) as u64
            });
        } else {
            self.put_u8(T_DATA_VIEW);
            self.put_u64(if tracking {
                TRACKING_LENGTH
            } else {
                info.byte_length as u64
            });
        }
        if !self.write_value(info.buffer) {
            return false;
        }
        self.put_u64(info.byte_offset as u64);
        true
    }

    unsafe fn write_host(&mut self, v: q::JSValue) -> bool {
        let Some(write) = self.callbacks.as_ref().and_then(|c| c.write) else {
            return self.report(root::JS_SCERR_UNSUPPORTED_TYPE);
        };
        self.put_u8(T_HOST);
        let obj = val_obj(v);
        let mut same_process = false;
        let cx = self.cx().as_raw();
        let closure = self.closure;
        write(cx, self.raw(), handle(&obj), &mut same_process, closure)
    }

    /// Transfers the transfer list (HTML StructuredSerializeWithTransfer step 5) into a
    /// transfer map. On failure, the custom transferables already transferred are freed.
    unsafe fn transfer(&mut self) -> Option<Vec<u8>> {
        let mut map = Vec::new();
        let mut custom = Vec::new();
        let ok = self.transfer_into(&mut map, &mut custom);
        if !ok {
            let free = self.callbacks.as_ref().and_then(|c| c.freeTransfer);
            for &(tag, ownership, content, extra) in &custom {
                if let Some(free) = free {
                    if ownership >= root::JS::TransferableOwnership::SCTAG_TMO_FIRST_OWNED as u32 {
                        free(
                            tag,
                            transfer_ownership(ownership),
                            content as *mut c_void,
                            extra,
                            self.closure,
                        );
                    }
                }
            }
            return None;
        }
        Some(map)
    }

    unsafe fn transfer_into(
        &mut self,
        map: &mut Vec<u8>,
        custom: &mut Vec<(u32, u32, usize, u64)>,
    ) -> bool {
        let out = std::mem::take(&mut self.out);
        let mut ok = true;
        for t in self.transferables.clone() {
            let mut info = q::JSBufferInfo::default();
            if q::JS_GetBufferInfo(t, &mut info) == q::JS_BUFFER_ARRAY_BUFFER {
                if info.detached != 0 {
                    ok = self.report(root::JS_SCERR_TYPED_ARRAY_DETACHED);
                    break;
                }
                self.put_u32(XFER_ARRAY_BUFFER);
                self.put_array_buffer(t, &info);
                q::JS_DetachArrayBuffer(self.ctx(), t);
                continue;
            }
            let Some(write_transfer) = self.callbacks.as_ref().and_then(|c| c.writeTransfer) else {
                ok = self.report(root::JS_SCERR_TRANSFERABLE);
                break;
            };
            let obj = val_obj(t);
            let mut tag = 0;
            let mut ownership = root::JS::TransferableOwnership::SCTAG_TMO_UNFILLED;
            let mut content = ptr::null_mut();
            let mut extra = 0;
            if !write_transfer(
                self.cx().as_raw(),
                handle(&obj),
                self.closure,
                &mut tag,
                &mut ownership,
                &mut content,
                &mut extra,
            ) {
                ok = false;
                break;
            }
            custom.push((tag, ownership as u32, content as usize, extra));
            self.put_u32(XFER_CUSTOM);
            self.put_u32(tag);
            self.put_u32(ownership as u32);
            self.put_u64(content as usize as u64);
            self.put_u64(extra);
        }
        *map = std::mem::replace(&mut self.out, out);
        ok
    }

    fn finish(&mut self, map: Vec<u8>) -> Vec<u8> {
        let count = self.transferables.len() as u32;
        let mut bytes = Vec::with_capacity(HEADER_LEN + map.len() + self.out.len());
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        let state = if count > 0 {
            TRANSFER_UNREAD
        } else {
            TRANSFER_NONE
        };
        bytes.extend_from_slice(&state.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&map);
        bytes.extend_from_slice(&self.out);
        bytes
    }
}

fn handle(obj: &*mut root::JSObject) -> root::JS::Handle<*mut root::JSObject> {
    root::JS::Handle {
        ptr: obj,
        _phantom_0: PhantomData,
    }
}

unsafe fn writer<'a>(w: *mut root::JSStructuredCloneWriter) -> &'a mut Writer {
    &mut *(w as *mut Writer)
}

/// Serializes `v` with its transfer list; `None` on failure (exception pending or error
/// reported).
unsafe fn write_clone(
    cx: &Cx,
    v: q::JSValue,
    scope: root::JS::StructuredCloneScope,
    callbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut c_void,
    transferable: q::JSValue,
) -> Option<Vec<u8>> {
    let mut w = Writer::new(cx, scope, callbacks, closure);
    if !w.parse_transferables(transferable) || !w.write_value(v) {
        return None;
    }
    let map = w.transfer()?;
    Some(w.finish(map))
}

// Reading.

/// The properties an Error's frame reads, after its message.
const ERROR_FIELDS: [&CStr; 2] = [c"cause", c"errors"];
const ERROR_FIELD_FLAGS: c_int = (q::JS_PROP_WRITABLE | q::JS_PROP_CONFIGURABLE) as c_int;

/// What `Reader::start` read.
enum Started {
    Value(Owned),
    /// The header of a container, whose frame was pushed.
    Container,
}

/// A container whose children are being read.
enum ReadFrame {
    /// An object or array, and the key of the value being read.
    Properties { obj: Owned, key: Option<Atoms> },
    /// A Map or Set, and the key of the value being read.
    Entries {
        map: Owned,
        is_set: bool,
        key: Option<Owned>,
    },
    /// An Error, and how many of `ERROR_FIELDS` were read.
    Error { error: Owned, next: usize },
}

impl ReadFrame {
    fn into_object(self) -> Owned {
        match self {
            ReadFrame::Properties { obj, .. } => obj,
            ReadFrame::Entries { map, .. } => map,
            ReadFrame::Error { error, .. } => error,
        }
    }
}

/// What a `JSStructuredCloneReader*` points to.
struct Reader<'a> {
    cx: *const Cx,
    input: &'a [u8],
    pos: usize,
    /// One reference per object read, by index (`JS_UNDEFINED` while being created).
    all_objs: Vec<q::JSValue>,
    policy: *const root::JS::CloneDataPolicy,
    callbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut c_void,
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        let rt = unsafe { (*self.cx).rt };
        for v in self.all_objs.drain(..) {
            unsafe { q::JS_FreeValueRT(rt, v) };
        }
    }
}

impl<'a> Reader<'a> {
    fn cx<'b>(&self) -> &'b Cx {
        unsafe { &*self.cx }
    }

    fn ctx(&self) -> *mut q::JSContext {
        self.cx().qctx()
    }

    fn raw(&mut self) -> *mut root::JSStructuredCloneReader {
        self as *mut Reader as *mut root::JSStructuredCloneReader
    }

    unsafe fn bad(&self, what: &CStr) -> Option<q::JSValue> {
        bad_data(self.cx(), what);
        None
    }

    unsafe fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.input.len() - self.pos < len {
            bad_data(self.cx(), c"truncated");
            return None;
        }
        let bytes = &self.input[self.pos..self.pos + len];
        self.pos += len;
        Some(bytes)
    }

    unsafe fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    unsafe fn peek_u8(&mut self) -> Option<u8> {
        let b = self.u8()?;
        self.pos -= 1;
        Some(b)
    }

    unsafe fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    unsafe fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    unsafe fn f64(&mut self) -> Option<f64> {
        self.u64().map(f64::from_bits)
    }

    unsafe fn usize(&mut self) -> Option<usize> {
        let v = self.u64()?;
        match usize::try_from(v) {
            Ok(v) => Some(v),
            Err(_) => {
                bad_data(self.cx(), c"length");
                None
            }
        }
    }

    /// Takes over a new QuickJS reference; `None` if it is an exception.
    fn owned(&self, v: q::JSValue) -> Option<Owned> {
        (!is_exception(v)).then(|| Owned::new(self.cx(), v))
    }

    fn reserve(&mut self) -> usize {
        self.all_objs.push(q::JS_UNDEFINED);
        self.all_objs.len() - 1
    }

    unsafe fn fill(&mut self, index: usize, v: &Owned) {
        self.all_objs[index] = q::JS_DupValueRT(self.cx().rt, v.v);
    }

    /// Gives a new object the next index.
    unsafe fn new_object(&mut self, v: q::JSValue) -> Option<Owned> {
        let v = self.owned(v)?;
        let index = self.reserve();
        self.fill(index, &v);
        Some(v)
    }

    unsafe fn read_string(&mut self) -> Option<Owned> {
        let header = self.u32()?;
        let len = (header >> 1) as usize;
        let ctx = self.ctx();
        let s = if header & 1 != 0 {
            let bytes = self.take(len * 2)?;
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            q::JS_NewStringUTF16(ctx, units.as_ptr(), len)
        } else {
            let bytes = self.take(len)?;
            q::JS_NewStringLatin1(ctx, bytes.as_ptr(), len)
        };
        self.owned(s)
    }

    unsafe fn read_bigint(&mut self) -> Option<Owned> {
        let n = self.u32()? as usize;
        let bytes = self.take(n * 4)?;
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        self.owned(q::JS_NewBigIntWords(self.ctx(), words.as_ptr(), n))
    }

    unsafe fn read_array_buffer(&mut self) -> Option<Owned> {
        let len = self.usize()?;
        let max = self.u64()?;
        let data = self.take(len)?;
        let ctx = self.ctx();
        self.owned(if max == u64::MAX {
            q::JS_NewArrayBufferCopy(ctx, data.as_ptr(), len)
        } else {
            q::JS_NewResizableArrayBufferCopy(ctx, data.as_ptr(), len, max as usize)
        })
    }

    unsafe fn read_transfer_entry(&mut self) -> bool {
        let Some(kind) = self.u32() else { return false };
        match kind {
            XFER_ARRAY_BUFFER => {
                let Some(buffer) = self.read_array_buffer() else {
                    return false;
                };
                self.all_objs.push(buffer.take());
                true
            }
            XFER_CUSTOM => {
                let (Some(tag), Some(_ownership), Some(content), Some(extra)) =
                    (self.u32(), self.u32(), self.u64(), self.u64())
                else {
                    return false;
                };
                let cx = self.cx();
                let Some(read_transfer) = self.callbacks.as_ref().and_then(|c| c.readTransfer)
                else {
                    report_error(
                        cx,
                        self.callbacks,
                        self.closure,
                        root::JS_SCERR_TRANSFERABLE,
                    );
                    return false;
                };
                let mut obj = ptr::null_mut();
                let out = root::JS::MutableHandle {
                    ptr: &mut obj,
                    _phantom_0: PhantomData,
                };
                let (policy, closure) = (self.policy, self.closure);
                if !read_transfer(
                    cx.as_raw(),
                    self.raw(),
                    policy,
                    tag,
                    content as usize as *mut c_void,
                    extra,
                    closure,
                    out,
                ) {
                    if q::JS_HasException(cx.qctx()) == 0 {
                        report_error(
                            cx,
                            self.callbacks,
                            self.closure,
                            root::JS_SCERR_TRANSFERABLE,
                        );
                    }
                    return false;
                }
                if obj.is_null() {
                    return false;
                }
                self.all_objs.push(q::JS_DupValueRT(cx.rt, obj_val(obj)));
                true
            }
            _ => {
                bad_data(self.cx(), c"transfer map");
                false
            }
        }
    }

    /// Reads a value and everything it references without recursion (see
    /// `Writer::write_value`).
    unsafe fn read_value(&mut self) -> Option<q::JSValue> {
        let mut stack = Vec::new();
        let mut done = match self.start(&mut stack)? {
            Started::Value(v) => Some(v),
            Started::Container => None,
        };
        loop {
            if let Some(value) = done.take() {
                let Some(frame) = stack.last_mut() else {
                    return Some(value.take());
                };
                self.accept(frame, value)?;
            }
            let frame = stack.last_mut().expect("a container being read");
            if !self.next_child(frame)? {
                done = stack.pop().map(ReadFrame::into_object);
                continue;
            }
            if let Started::Value(v) = self.start(&mut stack)? {
                done = Some(v);
            }
        }
    }

    /// Reads a value, or the header of a container pushed on `stack`.
    unsafe fn start(&mut self, stack: &mut Vec<ReadFrame>) -> Option<Started> {
        let ctx = self.ctx();
        let tag = self.u8()?;
        let value = match tag {
            T_UNDEFINED => q::JS_UNDEFINED,
            T_NULL => q::JS_NULL,
            T_FALSE => q::JS_FALSE,
            T_TRUE => q::JS_TRUE,
            T_INT32 => q::JS_NewInt32(ctx, self.u32()? as i32),
            T_DOUBLE => q::JS_NewFloat64(ctx, self.f64()?),
            T_BIGINT => self.read_bigint()?.take(),
            T_STRING => self.read_string()?.take(),
            T_BACKREF => {
                let index = self.u32()? as usize;
                match self.all_objs.get(index) {
                    Some(v) if v.tag() == q::JS_TAG_OBJECT => q::JS_DupValueRT(self.cx().rt, *v),
                    _ => {
                        self.bad(c"invalid back reference");
                        return None;
                    }
                }
            }
            T_OBJECT => {
                let obj = self.new_object(q::JS_NewObject(ctx))?;
                stack.push(ReadFrame::Properties { obj, key: None });
                return Some(Started::Container);
            }
            T_ARRAY => {
                let len = self.u32()?;
                let obj = self.new_object(q::JS_NewArray(ctx))?;
                if q::JS_SetPropertyStr(ctx, obj.v, c"length".as_ptr(), q::JS_NewUint32(ctx, len))
                    < 0
                {
                    return None;
                }
                stack.push(ReadFrame::Properties { obj, key: None });
                return Some(Started::Container);
            }
            T_MAP | T_SET => {
                let map = self.new_object(q::JS_NewMapOrSet(ctx, (tag == T_SET) as c_int))?;
                stack.push(ReadFrame::Entries {
                    map,
                    is_set: tag == T_SET,
                    key: None,
                });
                return Some(Started::Container);
            }
            T_ERROR => {
                let kind = self.u8()? as i8;
                let error = self.new_object(q::JS_NewNativeErrorObject(ctx, kind as c_int))?;
                if self.u8()? != 0 {
                    let message = self.read_string()?;
                    if q::JS_DefinePropertyValueStr(
                        ctx,
                        error.v,
                        c"message".as_ptr(),
                        message.take(),
                        ERROR_FIELD_FLAGS,
                    ) < 0
                    {
                        return None;
                    }
                }
                stack.push(ReadFrame::Error { error, next: 0 });
                return Some(Started::Container);
            }
            _ => return self.read_object(tag).map(Started::Value),
        };
        Some(Started::Value(Owned::new(self.cx(), value)))
    }

    /// Reads the next key of a container; false at its end.
    unsafe fn next_child(&mut self, frame: &mut ReadFrame) -> Option<bool> {
        let ctx = self.ctx();
        match frame {
            ReadFrame::Properties { key, .. } => {
                let atom = match self.u8()? {
                    T_END => return Some(false),
                    T_INDEX => q::JS_NewAtomUInt32(ctx, self.u32()?),
                    T_STRING => {
                        let s = self.read_string()?;
                        q::JS_ValueToAtom(ctx, s.v)
                    }
                    _ => {
                        self.bad(c"property key");
                        return None;
                    }
                };
                if atom == 0 {
                    return None;
                }
                *key = Some(Atoms {
                    atoms: vec![atom],
                    rt: self.cx().rt,
                });
                Some(true)
            }
            ReadFrame::Entries { key, .. } => {
                if key.is_none() && self.peek_u8()? == T_END {
                    self.pos += 1;
                    return Some(false);
                }
                Some(true)
            }
            ReadFrame::Error { error, next } => {
                while *next < ERROR_FIELDS.len() {
                    *next += 1;
                    if self.u8()? != 0 {
                        return Some(true);
                    }
                }
                if self.u8()? != 0 {
                    let stack = self.read_string()?;
                    if q::JS_DefinePropertyValueStr(
                        ctx,
                        error.v,
                        c"stack".as_ptr(),
                        stack.take(),
                        ERROR_FIELD_FLAGS,
                    ) < 0
                    {
                        return None;
                    }
                }
                Some(false)
            }
        }
    }

    /// Adds a child read after `next_child` to its container.
    unsafe fn accept(&mut self, frame: &mut ReadFrame, value: Owned) -> Option<()> {
        let ctx = self.ctx();
        let ok = match frame {
            ReadFrame::Properties { obj, key } => {
                let key = key.take().expect("a property key");
                q::JS_DefinePropertyValue(
                    ctx,
                    obj.v,
                    key.atoms[0],
                    value.take(),
                    q::JS_PROP_C_W_E as c_int,
                ) >= 0
            }
            ReadFrame::Entries {
                map, is_set: true, ..
            } => q::JS_AddMapRecord(ctx, map.v, value.v, q::JS_UNDEFINED) >= 0,
            ReadFrame::Entries { map, key, .. } => match key.take() {
                Some(k) => q::JS_AddMapRecord(ctx, map.v, k.v, value.v) >= 0,
                None => {
                    *key = Some(value);
                    true
                }
            },
            ReadFrame::Error { error, next } => {
                let name = ERROR_FIELDS[*next - 1];
                q::JS_DefinePropertyValueStr(
                    ctx,
                    error.v,
                    name.as_ptr(),
                    value.take(),
                    ERROR_FIELD_FLAGS,
                ) >= 0
            }
        };
        ok.then_some(())
    }

    /// The values without nested containers.
    unsafe fn read_object(&mut self, tag: u8) -> Option<Owned> {
        let ctx = self.ctx();
        let value = match tag {
            T_DATE => {
                let time = self.f64()?;
                return self.new_object(q::JS_NewDate(ctx, time));
            }
            T_REGEXP => {
                let source = self.read_string()?;
                let flags = self.read_string()?;
                return self.new_object(q::JS_NewRegExpOriginal(ctx, source.v, flags.v));
            }
            T_ARRAY_BUFFER => {
                let buffer = self.read_array_buffer()?;
                let index = self.reserve();
                self.fill(index, &buffer);
                return Some(buffer);
            }
            T_TYPED_ARRAY | T_DATA_VIEW => return self.read_view(tag),
            T_HOST => return self.read_host(),
            T_NUMBER_OBJECT => Owned::new(self.cx(), q::JS_NewFloat64(ctx, self.f64()?)),
            T_BOOLEAN_OBJECT => Owned::new(self.cx(), q::JS_NewBool(ctx, self.u8()? as c_int)),
            T_STRING_OBJECT => self.read_string()?,
            T_BIGINT_OBJECT => self.read_bigint()?,
            _ => {
                self.bad(c"unknown tag");
                return None;
            }
        };
        self.new_object(q::JS_NewPrimitiveObject(ctx, value.v))
    }

    unsafe fn read_view(&mut self, tag: u8) -> Option<Owned> {
        let ctx = self.ctx();
        let ty = if tag == T_TYPED_ARRAY { self.u8()? } else { 0 };
        if ty as usize >= ELEMENT_SIZES.len() {
            self.bad(c"typed array type");
            return None;
        }
        let length = self.u64()?;
        let index = self.reserve();
        let buffer = Owned::new(self.cx(), self.read_value()?);
        let offset = self.u64()?;
        let mut info = q::JSBufferInfo::default();
        if q::JS_GetBufferInfo(buffer.v, &mut info) != q::JS_BUFFER_ARRAY_BUFFER {
            self.bad(c"view without an ArrayBuffer");
            return None;
        }
        let tracking = length == TRACKING_LENGTH;
        if tracking && info.resizable == 0 {
            self.bad(c"length-tracking view of a fixed-length buffer");
            return None;
        }
        let view = if tag == T_TYPED_ARRAY {
            let length = if tracking {
                q::JS_UNDEFINED
            } else {
                q::JS_NewInt64(ctx, length as i64)
            };
            let mut args = [buffer.v, q::JS_NewInt64(ctx, offset as i64), length];
            q::JS_NewTypedArray(ctx, 3, args.as_mut_ptr(), ty as q::JSTypedArrayEnum)
        } else {
            q::JS_NewDataView(
                ctx,
                buffer.v,
                offset as usize,
                if tracking {
                    usize::MAX
                } else {
                    length as usize
                },
            )
        };
        let view = self.owned(view)?;
        self.fill(index, &view);
        Some(view)
    }

    unsafe fn read_host(&mut self) -> Option<Owned> {
        let index = self.reserve();
        let tag = self.u32()?;
        let data = self.u32()?;
        let Some(read) = self.callbacks.as_ref().and_then(|c| c.read) else {
            self.bad(c"unsupported type");
            return None;
        };
        let cx = self.cx();
        let (policy, closure) = (self.policy, self.closure);
        let obj = read(cx.as_raw(), self.raw(), policy, tag, data, closure);
        if obj.is_null() {
            return None;
        }
        let obj = Owned::new(cx, q::JS_DupValueRT(cx.rt, obj_val(obj)));
        self.fill(index, &obj);
        Some(obj)
    }
}

unsafe fn reader<'a>(r: *mut root::JSStructuredCloneReader) -> &'a mut Reader<'a> {
    &mut *(r as *mut Reader)
}

/// Deserializes the bytes in `storage` (a new reference), marking its transfer map consumed.
unsafe fn read_clone(
    cx: &Cx,
    storage: *mut Vec<u8>,
    policy: *const root::JS::CloneDataPolicy,
    callbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut c_void,
) -> Option<q::JSValue> {
    let Some(bytes) = storage.as_mut().filter(|b| b.len() >= HEADER_LEN) else {
        bad_data(cx, c"truncated");
        return None;
    };
    let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    if word(0) != MAGIC || word(4) != FORMAT_VERSION {
        bad_data(cx, c"unsupported format");
        return None;
    }
    let count = word(12);
    if count > 0 {
        if word(TRANSFER_STATE_OFFSET) == TRANSFER_CONSUMED {
            report_error(cx, callbacks, closure, root::JS_SCERR_TRANSFERABLE_TWICE);
            return None;
        }
        bytes[TRANSFER_STATE_OFFSET..TRANSFER_STATE_OFFSET + 4]
            .copy_from_slice(&TRANSFER_CONSUMED.to_le_bytes());
    }
    let mut r = Reader {
        cx,
        input: &bytes[HEADER_LEN..],
        pos: 0,
        all_objs: Vec::new(),
        policy,
        callbacks,
        closure,
    };
    for _ in 0..count {
        if !r.read_transfer_entry() {
            return None;
        }
    }
    let v = Owned::new(cx, r.read_value()?);
    if r.pos != r.input.len() {
        bad_data(cx, c"extra data after end");
        return None;
    }
    Some(v.take())
}

/// Calls `freeTransfer` for the custom transferables of an unread transfer map owned by `data`
/// (SpiderMonkey's `discardTransferables`).
unsafe fn discard_transferables(data: *mut root::JSStructuredCloneData) {
    if (*data).ownTransferables_ != root::OwnTransferablePolicy::OwnsTransferablesIfAny {
        return;
    }
    let Some(free) = (*data).callbacks_.as_ref().and_then(|c| c.freeTransfer) else {
        return;
    };
    let bytes = data_bytes(data);
    if bytes.len() < HEADER_LEN {
        return;
    }
    let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    if word(0) != MAGIC || word(TRANSFER_STATE_OFFSET) != TRANSFER_UNREAD {
        return;
    }
    let mut pos = HEADER_LEN;
    let u32_at = |p: usize| {
        bytes
            .get(p..p + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let u64_at = |p: usize| {
        bytes
            .get(p..p + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    };
    for _ in 0..word(12) {
        match u32_at(pos) {
            Some(XFER_ARRAY_BUFFER) => {
                let Some(len) = u64_at(pos + 4) else { return };
                pos += 4 + 16 + len as usize;
            }
            Some(XFER_CUSTOM) => {
                let (Some(tag), Some(ownership), Some(content), Some(extra)) = (
                    u32_at(pos + 4),
                    u32_at(pos + 8),
                    u64_at(pos + 12),
                    u64_at(pos + 20),
                ) else {
                    return;
                };
                if ownership >= root::JS::TransferableOwnership::SCTAG_TMO_FIRST_OWNED as u32 {
                    free(
                        tag,
                        transfer_ownership(ownership),
                        content as usize as *mut c_void,
                        extra,
                        (*data).closure_,
                    );
                }
                pos += 28;
            }
            _ => return,
        }
    }
}

// JSStructuredCloneData storage.

unsafe fn storage(data: *const root::JSStructuredCloneData) -> *mut Vec<u8> {
    *(data as *const *mut Vec<u8>)
}

unsafe fn data_bytes<'a>(data: *const root::JSStructuredCloneData) -> &'a [u8] {
    match storage(data).as_ref() {
        Some(v) => v,
        None => &[],
    }
}

unsafe fn data_vec<'a>(data: *mut root::JSStructuredCloneData) -> &'a mut Vec<u8> {
    let slot = data as *mut *mut Vec<u8>;
    if (*slot).is_null() {
        *slot = Box::into_raw(Box::default());
    }
    &mut **slot
}

/// Releases unread transferables the data owns and frees its bytes.
unsafe fn clear_data(data: *mut root::JSStructuredCloneData) {
    discard_transferables(data);
    let slot = data as *mut *mut Vec<u8>;
    if !(*slot).is_null() {
        drop(Box::from_raw(*slot));
        *slot = ptr::null_mut();
    }
    (*data).ownTransferables_ = root::OwnTransferablePolicy::NoTransferables;
}

pub(crate) unsafe fn NewJSAutoStructuredCloneBuffer(
    scope: root::JS::StructuredCloneScope,
    callbacks: *const root::JSStructuredCloneCallbacks,
) -> *mut root::JSAutoStructuredCloneBuffer {
    // The bindgen struct has enum fields without a zero value, so it is filled in place.
    let layout = std::alloc::Layout::new::<root::JSAutoStructuredCloneBuffer>();
    let buf = std::alloc::alloc_zeroed(layout) as *mut root::JSAutoStructuredCloneBuffer;
    if buf.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    let data = ptr::addr_of_mut!((*buf).data_);
    ptr::addr_of_mut!((*data).scope_).write(scope);
    ptr::addr_of_mut!((*data).callbacks_).write(callbacks);
    ptr::addr_of_mut!((*data).closure_).write(ptr::null_mut());
    ptr::addr_of_mut!((*data).ownTransferables_)
        .write(root::OwnTransferablePolicy::NoTransferables);
    ptr::addr_of_mut!((*buf).version_).write(root::JS_STRUCTURED_CLONE_VERSION);
    buf
}

pub(crate) unsafe fn DeleteJSAutoStructuredCloneBuffer(
    buf: *mut root::JSAutoStructuredCloneBuffer,
) {
    clear_data(ptr::addr_of_mut!((*buf).data_));
    std::alloc::dealloc(
        buf as *mut u8,
        std::alloc::Layout::new::<root::JSAutoStructuredCloneBuffer>(),
    );
}

pub(crate) unsafe fn GetLengthOfJSStructuredCloneData(
    data: *mut root::JSStructuredCloneData,
) -> usize {
    data_bytes(data).len()
}

pub(crate) unsafe fn CopyJSStructuredCloneData(
    src: *mut root::JSStructuredCloneData,
    dest: *mut u8,
) {
    let bytes = data_bytes(src);
    ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
}

pub(crate) unsafe fn WriteBytesToJSStructuredCloneData(
    src: *const u8,
    len: usize,
    dest: *mut root::JSStructuredCloneData,
) -> bool {
    if len > 0 {
        data_vec(dest).extend_from_slice(std::slice::from_raw_parts(src, len));
    }
    true
}

pub(crate) unsafe fn JS_StructuredCloneHasTransferables(
    data: *mut root::JSStructuredCloneData,
    hasTransferable: *mut bool,
) -> bool {
    let bytes = data_bytes(data);
    *hasTransferable = bytes.len() >= HEADER_LEN
        && bytes[12..16] != [0; 4]
        && u32::from_le_bytes(bytes[8..12].try_into().unwrap()) == TRANSFER_UNREAD;
    true
}

// Reading and writing.

pub(crate) unsafe fn JS_WriteStructuredClone(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    data: *mut root::JSStructuredCloneData,
    scope: root::JS::StructuredCloneScope,
    cloneDataPolicy: *const root::JS::CloneDataPolicy,
    optionalCallbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut ::std::os::raw::c_void,
    transferable: root::JS::HandleValue,
) -> bool {
    // Shared memory is never cloned, so the policy does not matter.
    let _ = cloneDataPolicy;
    let cx = Cx::from_raw(cx);
    let Some(bytes) = write_clone(
        cx,
        (*v.ptr).to_qjs(),
        scope,
        optionalCallbacks,
        closure,
        (*transferable.ptr).to_qjs(),
    ) else {
        return false;
    };
    clear_data(data);
    *data_vec(data) = bytes;
    (*data).scope_ = scope;
    (*data).callbacks_ = optionalCallbacks;
    (*data).closure_ = closure;
    (*data).ownTransferables_ = root::OwnTransferablePolicy::OwnsTransferablesIfAny;
    true
}

pub(crate) unsafe fn JS_ReadStructuredClone(
    cx: *mut root::JSContext,
    data: *const root::JSStructuredCloneData,
    version: u32,
    scope: root::JS::StructuredCloneScope,
    vp: root::JS::MutableHandleValue,
    cloneDataPolicy: *const root::JS::CloneDataPolicy,
    optionalCallbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut ::std::os::raw::c_void,
) -> bool {
    // The format has its own version, and is self-contained for every scope.
    let _ = (version, scope);
    let cx = Cx::from_raw(cx);
    let Some(v) = read_clone(
        cx,
        storage(data),
        cloneDataPolicy,
        optionalCallbacks,
        closure,
    ) else {
        return false;
    };
    *vp.ptr = cx.adopt(v);
    true
}

pub(crate) unsafe fn JS_StructuredClone(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    vp: root::JS::MutableHandleValue,
    optionalCallbacks: *const root::JSStructuredCloneCallbacks,
    closure: *mut ::std::os::raw::c_void,
) -> bool {
    let cx = Cx::from_raw(cx);
    let value = (*v.ptr).to_qjs();
    if is_string(value) {
        *vp.ptr = cx.retain(*v.ptr);
        return true;
    }
    let scope = root::JS::StructuredCloneScope::SameProcess;
    let Some(mut bytes) = write_clone(
        cx,
        value,
        scope,
        optionalCallbacks,
        closure,
        q::JS_UNDEFINED,
    ) else {
        return false;
    };
    let policy = root::JS::CloneDataPolicy {
        allowIntraClusterClonableSharedObjects_: false,
        allowSharedMemoryObjects_: false,
    };
    let Some(result) = read_clone(cx, &mut bytes, &policy, optionalCallbacks, closure) else {
        return false;
    };
    *vp.ptr = cx.adopt(result);
    true
}

// Callback API: writing.

pub(crate) unsafe fn JS_WriteUint32PairUnchecked(
    w: *mut root::JSStructuredCloneWriter,
    tag: u32,
    data: u32,
) -> bool {
    let w = writer(w);
    w.put_u32(tag);
    w.put_u32(data);
    true
}

pub(crate) unsafe fn JS_WriteBytes(
    w: *mut root::JSStructuredCloneWriter,
    p: *const ::std::os::raw::c_void,
    len: usize,
) -> bool {
    writer(w).put_raw(p as *const u8, len);
    true
}

pub(crate) unsafe fn JS_WriteString(
    w: *mut root::JSStructuredCloneWriter,
    str_: root::JS::HandleString,
) -> bool {
    let w = writer(w);
    w.put_u8(T_STRING);
    w.write_string(q::JSValue::mkptr(
        q::JS_TAG_STRING,
        *str_.ptr as *mut c_void,
    ))
}

pub(crate) unsafe fn JS_WriteDouble(w: *mut root::JSStructuredCloneWriter, v: f64) -> bool {
    writer(w).put_f64(v);
    true
}

pub(crate) unsafe fn JS_WriteTypedArray(
    w: *mut root::JSStructuredCloneWriter,
    v: root::JS::HandleValue,
) -> bool {
    let w = writer(w);
    let v = (*v.ptr).to_qjs();
    let mut info = q::JSBufferInfo::default();
    if v.tag() != q::JS_TAG_OBJECT || q::JS_GetBufferInfo(v, &mut info) != q::JS_BUFFER_TYPED_ARRAY
    {
        q::JS_ThrowTypeError(w.ctx(), c"not a typed array".as_ptr());
        return false;
    }
    w.write_value(v)
}

pub(crate) unsafe fn JS_ObjectNotWritten(
    w: *mut root::JSStructuredCloneWriter,
    obj: root::JS::HandleObject,
) -> bool {
    writer(w).memory.remove(&(*obj.ptr as usize));
    true
}

pub(crate) unsafe fn JS_GetStructuredCloneScope(
    w: *mut root::JSStructuredCloneWriter,
) -> root::JS::StructuredCloneScope {
    writer(w).scope
}

// Callback API: reading.

pub(crate) unsafe fn JS_ReadUint32Pair(
    r: *mut root::JSStructuredCloneReader,
    p1: *mut u32,
    p2: *mut u32,
) -> bool {
    let r = reader(r);
    let (Some(a), Some(b)) = (r.u32(), r.u32()) else {
        return false;
    };
    *p1 = a;
    *p2 = b;
    true
}

pub(crate) unsafe fn JS_ReadBytes(
    r: *mut root::JSStructuredCloneReader,
    p: *mut ::std::os::raw::c_void,
    len: usize,
) -> bool {
    let Some(bytes) = reader(r).take(len) else {
        return false;
    };
    ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, len);
    true
}

pub(crate) unsafe fn JS_ReadString(
    r: *mut root::JSStructuredCloneReader,
    str_: root::JS::MutableHandleString,
) -> bool {
    let r = reader(r);
    match r.u8() {
        Some(T_STRING) => {}
        Some(_) => {
            r.bad(c"expected string");
            return false;
        }
        None => return false,
    }
    let Some(s) = r.read_string() else {
        return false;
    };
    *str_.ptr = r.cx().adopt(s.take()).to_string();
    true
}

pub(crate) unsafe fn JS_ReadDouble(r: *mut root::JSStructuredCloneReader, v: *mut f64) -> bool {
    let Some(d) = reader(r).f64() else {
        return false;
    };
    *v = d;
    true
}

pub(crate) unsafe fn JS_ReadTypedArray(
    r: *mut root::JSStructuredCloneReader,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let r = reader(r);
    match r.peek_u8() {
        Some(T_TYPED_ARRAY | T_BACKREF) => {}
        Some(_) => {
            r.bad(c"expected typed array");
            return false;
        }
        None => return false,
    }
    let Some(v) = r.read_value() else {
        return false;
    };
    let v = Owned::new(r.cx(), v);
    let mut info = q::JSBufferInfo::default();
    if q::JS_GetBufferInfo(v.v, &mut info) != q::JS_BUFFER_TYPED_ARRAY {
        r.bad(c"expected typed array");
        return false;
    }
    *vp.ptr = r.cx().adopt(v.take());
    true
}
