/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Arrays, id vectors and enumeration, Map/Set/WeakMap, Date, RegExp, functions with reserved
//! slots, iteration and the remaining property overloads.
//!
//! A `JS::StackGCVector<T>` is a Rust `Vec<T>`: a `Handle<StackGCVector<T>>` points to a `Vec<T>`,
//! and the `PersistentRooted*Vector`s are boxed `Vec`s. Entries hold no reference count; the GC
//! things they refer to are retained by the pool, like every other value handed to Rust.
//!
//! Builtins without a QuickJS C API (Map, Set, WeakMap, Date, RegExp) call the methods of the
//! current realm's intrinsic prototypes.

use std::collections::HashSet;
use std::ffi::{c_int, CStr};
use std::ptr;

use servo_quickjs as q;

use super::class::{
    adopt_object, function_data, object_data, QJS_CLASS_BYTECODE_FUNCTION, QJS_CLASS_OBJECT,
};
use super::intrinsics::{self, Intrinsic};
use super::property::{data_flags, well_known_symbol_atom, Atom};
use super::{obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;

// QuickJS's internal class ids (`enum { JS_CLASS_OBJECT = 1, ... }` in quickjs.c).
const CLASS_ARRAY: q::JSClassID = 2;
const CLASS_ERROR: q::JSClassID = 3;
const CLASS_NUMBER: q::JSClassID = 4;
const CLASS_STRING: q::JSClassID = 5;
const CLASS_BOOLEAN: q::JSClassID = 6;
const CLASS_ARGUMENTS: q::JSClassID = 8;
const CLASS_MAPPED_ARGUMENTS: q::JSClassID = 9;
const CLASS_DATE: q::JSClassID = 10;
const CLASS_REGEXP: q::JSClassID = 18;
const CLASS_ARRAY_BUFFER: q::JSClassID = 19;
const CLASS_SHARED_ARRAY_BUFFER: q::JSClassID = 20;
const CLASS_BIG_INT: q::JSClassID = 34;
const CLASS_MAP: q::JSClassID = 35;
const CLASS_SET: q::JSClassID = 36;
const CLASS_WEAKMAP: q::JSClassID = 37;
const CLASS_ITERATOR: q::JSClassID = 39;
const CLASS_MAP_ITERATOR: q::JSClassID = 43;
const CLASS_SET_ITERATOR: q::JSClassID = 44;
const CLASS_PROXY: q::JSClassID = 51;
const CLASS_PROMISE: q::JSClassID = 52;

fn ok(status: c_int) -> bool {
    status >= 0
}

fn is_exception(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_EXCEPTION
}

/// Stores a QuickJS result (a new reference) in `out`; false if it is an exception.
unsafe fn set_result(cx: &Cx, v: q::JSValue, out: root::JS::MutableHandleValue) -> bool {
    if is_exception(v) {
        return false;
    }
    *out.ptr = cx.adopt(v);
    true
}

/// Maps a QuickJS status to an `ObjectOpResult`: `< 0` exception, `0` failure with `failure`.
unsafe fn op_result(
    status: c_int,
    result: *mut root::JS::ObjectOpResult,
    failure: root::JSErrNum,
) -> bool {
    if status < 0 {
        return false;
    }
    if status > 0 {
        (*result).succeed();
    } else {
        (*result).fail(failure);
    }
    true
}

unsafe fn class_of(obj: *mut root::JSObject) -> q::JSClassID {
    q::JS_GetClassID(obj_val(obj))
}

unsafe fn free_desc(cx: &Cx, qd: &q::JSPropertyDescriptor) {
    for v in [qd.value, qd.getter, qd.setter] {
        q::JS_FreeValueRT(cx.rt, v);
    }
}

/// Calls `f` with the own property keys of `obj` (`JS_GPN_*` flags) and their enumerability.
unsafe fn for_each_own_key(
    cx: &Cx,
    obj: q::JSValue,
    flags: u32,
    mut f: impl FnMut(&Cx, q::JSAtom, bool),
) -> bool {
    let qctx = cx.qctx();
    let mut tab: *mut q::JSPropertyEnum = ptr::null_mut();
    let mut len = 0u32;
    if q::JS_GetOwnPropertyNames(qctx, &mut tab, &mut len, obj, flags as c_int) < 0 {
        return false;
    }
    if len > 0 {
        for e in std::slice::from_raw_parts(tab, len as usize) {
            f(cx, e.atom, e.is_enumerable != 0);
        }
    }
    q::JS_FreePropertyEnum(qctx, tab, len);
    true
}

/// The id of `atom`; a string or symbol atom is kept alive by the pool.
unsafe fn retained_id(cx: &Cx, atom: q::JSAtom) -> root::jsid {
    let id = crate::sys::jsid::from_atom(atom);
    if id.is_string() || id.is_symbol() {
        cx.adopt_atom(q::JS_DupAtom(cx.base_ctx, atom));
    }
    id
}

unsafe fn class_proto(cx: *mut root::JSContext, class: q::JSClassID) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let proto = q::JS_GetClassProto(cx.qctx(), class);
    adopt_object(cx, proto)
}

// Arrays.

pub(crate) unsafe fn NewArrayObject(
    cx: *mut root::JSContext,
    contents: *const root::JS::HandleValueArray,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let arr = q::JS_NewArray(qctx);
    if is_exception(arr) {
        return ptr::null_mut();
    }
    if !contents.is_null() && (*contents).length_ > 0 {
        let values = std::slice::from_raw_parts((*contents).elements_, (*contents).length_);
        for (i, v) in values.iter().enumerate() {
            let v = q::JS_DupValueRT(cx.rt, v.to_qjs());
            if q::JS_DefinePropertyValueUint32(qctx, arr, i as u32, v, q::JS_PROP_C_W_E as c_int)
                < 0
            {
                q::JS_FreeValueRT(cx.rt, arr);
                return ptr::null_mut();
            }
        }
    }
    adopt_object(cx, arr)
}

pub(crate) unsafe fn NewArrayObject1(
    cx: *mut root::JSContext,
    length: usize,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let arr = q::JS_NewArray(qctx);
    if is_exception(arr) {
        return ptr::null_mut();
    }
    if length > 0
        && q::JS_SetPropertyStr(
            qctx,
            arr,
            c"length".as_ptr(),
            q::JS_NewInt64(qctx, length as i64),
        ) < 0
    {
        q::JS_FreeValueRT(cx.rt, arr);
        return ptr::null_mut();
    }
    adopt_object(cx, arr)
}

/// Whether `v` is an Array, by its builtin class as in SpiderMonkey's `IsArrayObject`: unlike
/// ECMAScript's IsArray, a JS Proxy of an array isn't one (e.g. not an IndexedDB array key).
pub(crate) unsafe fn is_array_object(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_OBJECT && class_of(val_obj(v)) == CLASS_ARRAY
}

pub(crate) unsafe fn IsArrayObject(
    cx: *mut root::JSContext,
    value: root::JS::Handle<root::JS::Value>,
    isArray: *mut bool,
) -> bool {
    let _ = cx;
    *isArray = is_array_object((*value.ptr).to_qjs());
    true
}

// Id and object vectors.

pub(crate) unsafe fn CreateRootedIdVector(
    cx: *mut root::JSContext,
) -> *mut root::JS::PersistentRootedIdVector {
    let _ = cx;
    Box::into_raw(Box::new(Vec::<root::jsid>::new())) as *mut root::JS::PersistentRootedIdVector
}

pub(crate) unsafe fn DestroyRootedIdVector(v: *mut root::JS::PersistentRootedIdVector) {
    drop(Box::from_raw(v as *mut Vec<root::jsid>));
}

pub(crate) unsafe fn GetIdVectorAddress(
    v: *mut root::JS::PersistentRootedIdVector,
) -> *mut ::std::os::raw::c_void {
    v as *mut _
}

pub(crate) unsafe fn SliceRootedIdVector(
    v: *const root::JS::PersistentRootedIdVector,
    length: *mut usize,
) -> *const root::jsid {
    let v = &*(v as *const Vec<root::jsid>);
    *length = v.len();
    v.as_ptr()
}

pub(crate) unsafe fn AppendToIdVector(
    v: root::JS::MutableHandleIdVector,
    id: root::JS::HandleId,
) -> bool {
    let id = *id.ptr;
    if id.is_string() || id.is_symbol() {
        let cx = super::current();
        cx.adopt_atom(q::JS_DupAtom(cx.base_ctx, id.to_atom()));
    }
    (*(v.ptr as *mut Vec<root::jsid>)).push(id);
    true
}

pub(crate) unsafe fn GetPropertyKeys(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    flags: ::std::os::raw::c_uint,
    props: root::JS::MutableHandleIdVector,
) -> bool {
    let cx = Cx::from_raw(cx);
    let out = &mut *(props.ptr as *mut Vec<root::jsid>);
    let hidden = flags & root::JSITER_HIDDEN != 0;
    let mut kinds = 0;
    if flags & root::JSITER_SYMBOLSONLY == 0 {
        kinds |= q::JS_GPN_STRING_MASK;
    }
    if flags & (root::JSITER_SYMBOLS | root::JSITER_SYMBOLSONLY) != 0 {
        kinds |= q::JS_GPN_SYMBOL_MASK;
    }
    if flags & root::JSITER_OWNONLY != 0 {
        let enum_only = if hidden { 0 } else { q::JS_GPN_ENUM_ONLY };
        return for_each_own_key(cx, obj_val(*obj.ptr), kinds | enum_only, |cx, atom, _| {
            out.push(retained_id(cx, atom))
        });
    }
    // for-in order: own keys first, then each prototype's keys that are not shadowed.
    let mut seen = HashSet::new();
    let mut current = q::JS_DupValueRT(cx.rt, obj_val(*obj.ptr));
    loop {
        let listed = for_each_own_key(
            cx,
            current,
            kinds | q::JS_GPN_SET_ENUM,
            |cx, atom, enumerable| {
                if seen.insert(atom) && (hidden || enumerable) {
                    out.push(retained_id(cx, atom));
                }
            },
        );
        let proto = if listed {
            q::JS_GetPrototype(cx.qctx(), current)
        } else {
            q::JS_EXCEPTION
        };
        q::JS_FreeValueRT(cx.rt, current);
        if is_exception(proto) {
            return false;
        }
        if proto.tag() != q::JS_TAG_OBJECT {
            return true;
        }
        current = proto;
    }
}

pub(crate) unsafe fn CreateRootedObjectVector(
    aCx: *mut root::JSContext,
) -> *mut root::JS::PersistentRootedObjectVector {
    let _ = aCx;
    Box::into_raw(Box::new(Vec::<*mut root::JSObject>::new()))
        as *mut root::JS::PersistentRootedObjectVector
}

pub(crate) unsafe fn DeleteRootedObjectVector(v: *mut root::JS::PersistentRootedObjectVector) {
    drop(Box::from_raw(v as *mut Vec<*mut root::JSObject>));
}

pub(crate) unsafe fn AppendToRootedObjectVector(
    v: *mut root::JS::PersistentRootedObjectVector,
    obj: *mut root::JSObject,
) -> bool {
    if !obj.is_null() {
        super::current().retain(JSVal::from_qjs(obj_val(obj)));
    }
    (*(v as *mut Vec<*mut root::JSObject>)).push(obj);
    true
}

pub(crate) unsafe fn GetObjectVectorAddress(
    v: *mut root::JS::PersistentRootedObjectVector,
) -> *mut ::std::os::raw::c_void {
    v as *mut _
}

/// The `Vec<T>` behind a `Handle<StackGCVector<T>>`.
pub(crate) unsafe fn stack_gc_vector<'a, T>(
    vec: root::JS::Handle<root::JS::StackGCVector<T, root::js::TempAllocPolicy>>,
) -> &'a Vec<T> {
    &*(vec.ptr as *const Vec<T>)
}

pub(crate) unsafe fn StackGCVectorValueLength(
    vec: root::JS::Handle<root::JS::StackGCVector<root::JS::Value, root::js::TempAllocPolicy>>,
) -> u32 {
    stack_gc_vector(vec).len() as u32
}

pub(crate) unsafe fn StackGCVectorStringLength(
    vec: root::JS::Handle<root::JS::StackGCVector<*mut root::JSString, root::js::TempAllocPolicy>>,
) -> u32 {
    stack_gc_vector(vec).len() as u32
}

pub(crate) unsafe fn StackGCVectorValueAtIndex(
    vec: root::JS::Handle<root::JS::StackGCVector<root::JS::Value, root::js::TempAllocPolicy>>,
    index: u32,
) -> *const root::JS::Value {
    &stack_gc_vector(vec)[index as usize]
}

pub(crate) unsafe fn StackGCVectorStringAtIndex(
    vec: root::JS::Handle<root::JS::StackGCVector<*mut root::JSString, root::js::TempAllocPolicy>>,
    index: u32,
) -> *const *mut root::JSString {
    &stack_gc_vector(vec)[index as usize]
}

// Remaining property overloads.

pub(crate) unsafe fn JS_DefineUCProperty2(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const u16,
    namelen: usize,
    value: root::JS::Handle<root::JS::Value>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_utf16(cx, name, namelen);
    let v = q::JS_DupValueRT(cx.rt, (*value.ptr).to_qjs());
    ok(q::JS_DefinePropertyValue(
        cx.qctx(),
        obj_val(*obj.ptr),
        atom.atom,
        v,
        data_flags(attrs),
    ))
}

pub(crate) unsafe fn JS_ForwardGetPropertyTo(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    receiver: root::JS::Handle<root::JS::Value>,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_GetPropertyInternal(
        cx.qctx(),
        obj_val(*obj.ptr),
        (*id.ptr).to_atom(),
        (*receiver.ptr).to_qjs(),
        0,
    );
    set_result(cx, v, vp)
}

pub(crate) unsafe fn JS_ForwardSetPropertyTo(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    v: root::JS::Handle<root::JS::Value>,
    receiver: root::JS::Handle<root::JS::Value>,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_DupValueRT(cx.rt, (*v.ptr).to_qjs());
    let r = q::JS_SetPropertyInternal(
        cx.qctx(),
        obj_val(*obj.ptr),
        (*id.ptr).to_atom(),
        v,
        (*receiver.ptr).to_qjs(),
        0,
    );
    op_result(r, result, root::JSErrNum::JSMSG_READ_ONLY)
}

/// OrdinarySetWithOwnDescriptor (ES 10.1.9.2) with `ownDesc` (null: no own property).
pub(crate) unsafe fn SetPropertyIgnoringNamedGetter(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    id: root::JS::HandleId,
    v: root::JS::HandleValue,
    receiver: root::JS::HandleValue,
    ownDesc: *const root::JS::Handle<root::JS::PropertyDescriptor>,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let atom = (*id.ptr).to_atom();
    let v = (*v.ptr).to_qjs();
    let receiver = (*receiver.ptr).to_qjs();
    let desc = if ownDesc.is_null() {
        let proto = q::JS_GetPrototype(qctx, obj_val(*obj.ptr));
        if is_exception(proto) {
            return false;
        }
        if proto.tag() == q::JS_TAG_OBJECT {
            let r = q::JS_SetPropertyInternal(
                qctx,
                proto,
                atom,
                q::JS_DupValueRT(cx.rt, v),
                receiver,
                0,
            );
            q::JS_FreeValueRT(cx.rt, proto);
            return op_result(r, result, root::JSErrNum::JSMSG_READ_ONLY);
        }
        None
    } else {
        Some(&*(*ownDesc).ptr)
    };
    let is_accessor = desc.is_some_and(|d| d.hasGetter_() || d.hasSetter_());
    if !is_accessor {
        if desc.is_some_and(|d| !d.writable_()) {
            (*result).fail(root::JSErrNum::JSMSG_READ_ONLY);
            return true;
        }
        if receiver.tag() != q::JS_TAG_OBJECT {
            (*result).fail(root::JSErrNum::JSMSG_SET_NON_OBJECT_RECEIVER);
            return true;
        }
        let mut existing: q::JSPropertyDescriptor = std::mem::zeroed();
        let r = q::JS_GetOwnProperty(qctx, &mut existing, receiver, atom);
        if r < 0 {
            return false;
        }
        if r > 0 {
            free_desc(cx, &existing);
            let flags = existing.flags as u32;
            if flags & q::JS_PROP_GETSET != 0 {
                (*result).fail(root::JSErrNum::JSMSG_OVERWRITING_ACCESSOR);
                return true;
            }
            if flags & q::JS_PROP_WRITABLE == 0 {
                (*result).fail(root::JSErrNum::JSMSG_READ_ONLY);
                return true;
            }
            let r = q::JS_DefineProperty(
                qctx,
                receiver,
                atom,
                v,
                q::JS_UNDEFINED,
                q::JS_UNDEFINED,
                q::JS_PROP_HAS_VALUE as c_int,
            );
            return op_result(r, result, root::JSErrNum::JSMSG_CANT_REDEFINE_PROP);
        }
        let r = q::JS_DefinePropertyValue(
            qctx,
            receiver,
            atom,
            q::JS_DupValueRT(cx.rt, v),
            q::JS_PROP_C_W_E as c_int,
        );
        return op_result(
            r,
            result,
            root::JSErrNum::JSMSG_CANT_DEFINE_PROP_OBJECT_NOT_EXTENSIBLE,
        );
    }
    let desc = desc.unwrap();
    if !desc.hasSetter_() || desc.setter_.is_null() {
        (*result).fail(root::JSErrNum::JSMSG_GETTER_ONLY);
        return true;
    }
    let mut argv = [v];
    let r = q::JS_Call(qctx, obj_val(desc.setter_), receiver, 1, argv.as_mut_ptr());
    if is_exception(r) {
        return false;
    }
    q::JS_FreeValueRT(cx.rt, r);
    (*result).succeed();
    true
}

/// Copies the own properties of `src` (with their attributes) to `dst`.
pub(crate) unsafe fn JS_InitializePropertiesFromCompatibleNativeObject(
    cx: *mut root::JSContext,
    dst: root::JS::HandleObject,
    src: root::JS::HandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let (dst, src) = (obj_val(*dst.ptr), obj_val(*src.ptr));
    let mut failed = false;
    let listed = for_each_own_key(
        cx,
        src,
        q::JS_GPN_STRING_MASK | q::JS_GPN_SYMBOL_MASK,
        |cx, atom, _| {
            if failed {
                return;
            }
            let qctx = cx.qctx();
            let mut qd: q::JSPropertyDescriptor = std::mem::zeroed();
            let r = q::JS_GetOwnProperty(qctx, &mut qd, src, atom);
            if r <= 0 {
                failed = r < 0;
                return;
            }
            let attrs = qd.flags as u32
                & (q::JS_PROP_CONFIGURABLE | q::JS_PROP_ENUMERABLE | q::JS_PROP_WRITABLE);
            let mut flags =
                attrs | q::JS_PROP_HAS_CONFIGURABLE | q::JS_PROP_HAS_ENUMERABLE | q::JS_PROP_THROW;
            if qd.flags as u32 & q::JS_PROP_GETSET != 0 {
                flags |= q::JS_PROP_HAS_GET | q::JS_PROP_HAS_SET;
            } else {
                flags |= q::JS_PROP_HAS_VALUE | q::JS_PROP_HAS_WRITABLE;
            }
            failed = q::JS_DefineProperty(
                qctx,
                dst,
                atom,
                qd.value,
                qd.getter,
                qd.setter,
                flags as c_int,
            ) < 0;
            free_desc(cx, &qd);
        },
    );
    listed && !failed
}

// Property descriptors.

pub(crate) unsafe fn FromPropertyDescriptor(
    cx: *mut root::JSContext,
    desc_: root::JS::Handle<root::JS::PropertyDescriptor>,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let d = &*desc_.ptr;
    let obj = q::JS_NewObject(qctx);
    if is_exception(obj) {
        return false;
    }
    let object_or_undefined = |o: *mut root::JSObject| {
        if o.is_null() {
            q::JS_UNDEFINED
        } else {
            q::JS_DupValueRT(cx.rt, obj_val(o))
        }
    };
    let mut fields = Vec::new();
    if d.hasValue_() {
        fields.push((c"value", q::JS_DupValueRT(cx.rt, d.value_.to_qjs())));
    }
    if d.hasWritable_() {
        fields.push((c"writable", q::JS_NewBool(qctx, d.writable_() as c_int)));
    }
    if d.hasGetter_() {
        fields.push((c"get", object_or_undefined(d.getter_)));
    }
    if d.hasSetter_() {
        fields.push((c"set", object_or_undefined(d.setter_)));
    }
    if d.hasEnumerable_() {
        fields.push((c"enumerable", q::JS_NewBool(qctx, d.enumerable_() as c_int)));
    }
    if d.hasConfigurable_() {
        fields.push((
            c"configurable",
            q::JS_NewBool(qctx, d.configurable_() as c_int),
        ));
    }
    let mut defined = true;
    for (name, v) in fields {
        // Consumes `v` even on failure.
        defined &= ok(q::JS_DefinePropertyValueStr(
            qctx,
            obj,
            name.as_ptr(),
            v,
            q::JS_PROP_C_W_E as c_int,
        ));
    }
    if !defined {
        q::JS_FreeValueRT(cx.rt, obj);
        return false;
    }
    *vp.ptr = cx.adopt(obj);
    true
}

pub(crate) unsafe fn SetDataPropertyDescriptor(
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    value: root::JS::HandleValue,
    attrs: u32,
) {
    let d = &mut *desc.ptr;
    *d = std::mem::zeroed();
    d.set_hasConfigurable_(true);
    d.set_configurable_(attrs & root::JSPROP_PERMANENT as u32 == 0);
    d.set_hasEnumerable_(true);
    d.set_enumerable_(attrs & root::JSPROP_ENUMERATE as u32 != 0);
    d.set_hasWritable_(true);
    d.set_writable_(attrs & root::JSPROP_READONLY as u32 == 0);
    d.set_hasValue_(true);
    d.set_resolving_(attrs & root::JSPROP_RESOLVING != 0);
    d.value_ = super::current().retain(*value.ptr);
}

pub(crate) unsafe fn CallPropertyDescriptorTracer(
    trc: *mut root::JSTracer,
    desc: *mut root::JS::PropertyDescriptor,
) {
    // Descriptor fields are pool-retained, not counted, so they must not be marked.
    let _ = (trc, desc);
}

// Symbols and BigInts are reference-counted but not cycle-collected (`JS_MarkValue` ignores
// them); the count held by the `Heap` is enough.

pub(crate) unsafe fn CallSymbolTracer(
    trc: *mut root::JSTracer,
    bip: *mut root::JS::Heap<*mut root::JS::Symbol>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = (trc, bip, name);
}

pub(crate) unsafe fn CallBigIntTracer(
    trc: *mut root::JSTracer,
    bip: *mut root::JS::Heap<*mut root::JS::BigInt>,
    name: *const ::std::os::raw::c_char,
) {
    let _ = (trc, bip, name);
}

// Objects and prototypes.

pub(crate) unsafe fn JS_NewObjectWithoutMetadata(
    cx: *mut root::JSContext,
    clasp: *const root::JSClass,
    proto: root::JS::Handle<*mut root::JSObject>,
) -> *mut root::JSObject {
    super::JS_NewObjectWithGivenProto(cx, clasp, proto)
}

/// SetIntegrityLevel(obj, frozen).
pub(crate) unsafe fn JS_FreezeObject(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let o = obj_val(*obj.ptr);
    let r = q::JS_PreventExtensions(cx.qctx(), o);
    if r < 0 {
        return false;
    }
    if r == 0 {
        q::JS_ThrowTypeError(cx.qctx(), c"can't freeze object".as_ptr());
        return false;
    }
    let mut failed = false;
    let listed = for_each_own_key(
        cx,
        o,
        q::JS_GPN_STRING_MASK | q::JS_GPN_SYMBOL_MASK,
        |cx, atom, _| {
            if failed {
                return;
            }
            let qctx = cx.qctx();
            let mut qd: q::JSPropertyDescriptor = std::mem::zeroed();
            let r = q::JS_GetOwnProperty(qctx, &mut qd, o, atom);
            if r <= 0 {
                failed = r < 0;
                return;
            }
            free_desc(cx, &qd);
            let mut flags = q::JS_PROP_HAS_CONFIGURABLE | q::JS_PROP_THROW;
            if qd.flags as u32 & q::JS_PROP_GETSET == 0 {
                flags |= q::JS_PROP_HAS_WRITABLE;
            }
            failed = q::JS_DefineProperty(
                qctx,
                o,
                atom,
                q::JS_UNDEFINED,
                q::JS_UNDEFINED,
                q::JS_UNDEFINED,
                flags as c_int,
            ) < 0;
        },
    );
    listed && !failed
}

pub(crate) unsafe fn JS_SetImmutablePrototype(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    succeeded: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    q::JS_SetImmutablePrototype(cx.qctx(), obj_val(*obj.ptr));
    *succeeded = true;
    true
}

pub(crate) unsafe fn GetObjectProto(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    proto: root::JS::MutableHandleObject,
) -> bool {
    super::JS_GetPrototype(cx, obj, proto)
}

/// The prototype in the object's shape, without exotic `getPrototypeOf` hooks.
pub(crate) unsafe fn GetStaticPrototype(obj: *mut root::JSObject) -> *mut root::JSObject {
    let cx = super::current();
    let proto = q::JS_GetStaticPrototype(cx.base_ctx, obj_val(obj));
    let proto = cx.adopt(proto);
    if proto.is_object() {
        proto.to_object()
    } else {
        ptr::null_mut()
    }
}

pub(crate) unsafe fn JS_IsGlobalObject(obj: *mut root::JSObject) -> bool {
    !super::realm::realm_of_global(obj).is_null()
}

pub(crate) unsafe fn JS_LinkConstructorAndPrototype(
    cx: *mut root::JSContext,
    ctor: root::JS::Handle<*mut root::JSObject>,
    proto: root::JS::Handle<*mut root::JSObject>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let (ctor, proto) = (obj_val(*ctor.ptr), obj_val(*proto.ptr));
    let throw = q::JS_PROP_THROW as c_int;
    let ctor_proto_flags = throw;
    let proto_ctor_flags = (q::JS_PROP_CONFIGURABLE | q::JS_PROP_WRITABLE) as c_int | throw;
    ok(q::JS_DefinePropertyValueStr(
        qctx,
        ctor,
        c"prototype".as_ptr(),
        q::JS_DupValueRT(cx.rt, proto),
        ctor_proto_flags,
    )) && ok(q::JS_DefinePropertyValueStr(
        qctx,
        proto,
        c"constructor".as_ptr(),
        q::JS_DupValueRT(cx.rt, ctor),
        proto_ctor_flags,
    ))
}

pub(crate) unsafe fn JS_HasInstance(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    v: root::JS::Handle<root::JS::Value>,
    bp: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = q::JS_IsInstanceOf(cx.qctx(), (*v.ptr).to_qjs(), obj_val(*obj.ptr));
    *bp = r > 0;
    ok(r)
}

pub(crate) unsafe fn GetBuiltinClass(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    cls: *mut root::js::ESClass,
) -> bool {
    use root::js::ESClass;
    let cx = Cx::from_raw(cx);
    let obj = *obj.ptr;
    let class = class_of(obj);
    *cls = match class {
        QJS_CLASS_OBJECT => ESClass::Object,
        CLASS_ARRAY => ESClass::Array,
        CLASS_NUMBER => ESClass::Number,
        CLASS_STRING => ESClass::String,
        CLASS_BOOLEAN => ESClass::Boolean,
        CLASS_REGEXP => ESClass::RegExp,
        CLASS_ARRAY_BUFFER => ESClass::ArrayBuffer,
        CLASS_SHARED_ARRAY_BUFFER => ESClass::SharedArrayBuffer,
        CLASS_DATE => ESClass::Date,
        CLASS_SET => ESClass::Set,
        CLASS_MAP => ESClass::Map,
        CLASS_PROMISE => ESClass::Promise,
        CLASS_MAP_ITERATOR => ESClass::MapIterator,
        CLASS_SET_ITERATOR => ESClass::SetIterator,
        CLASS_ARGUMENTS | CLASS_MAPPED_ARGUMENTS => ESClass::Arguments,
        CLASS_ERROR => ESClass::Error,
        CLASS_BIG_INT => ESClass::BigInt,
        CLASS_PROXY => ESClass::Other,
        // Native functions and QuickJS functions are SpiderMonkey `JSFunction`s; callable objects
        // of SpiderMonkey classes are not.
        _ if object_data(obj).is_none() && q::JS_IsFunction(cx.qctx(), obj_val(obj)) != 0 => {
            ESClass::Function
        }
        _ => ESClass::Other,
    };
    true
}

pub(crate) unsafe fn IsArgumentsObject(obj: root::JS::HandleObject) -> bool {
    matches!(class_of(*obj.ptr), CLASS_ARGUMENTS | CLASS_MAPPED_ARGUMENTS)
}

pub(crate) unsafe fn GetRealmObjectPrototype(cx: *mut root::JSContext) -> *mut root::JSObject {
    class_proto(cx, QJS_CLASS_OBJECT)
}

pub(crate) unsafe fn GetRealmFunctionPrototype(cx: *mut root::JSContext) -> *mut root::JSObject {
    class_proto(cx, QJS_CLASS_BYTECODE_FUNCTION)
}

pub(crate) unsafe fn GetRealmErrorPrototype(cx: *mut root::JSContext) -> *mut root::JSObject {
    class_proto(cx, CLASS_ERROR)
}

pub(crate) unsafe fn GetRealmIteratorPrototype(cx: *mut root::JSContext) -> *mut root::JSObject {
    class_proto(cx, CLASS_ITERATOR)
}

// Standard classes: QuickJS contexts define all of them eagerly, so global resolve and
// enumerate hooks have nothing to add.

pub(crate) unsafe fn JS_ResolveStandardClass(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    id: root::JS::HandleId,
    resolved: *mut bool,
) -> bool {
    let _ = (cx, obj, id);
    *resolved = false;
    true
}

pub(crate) unsafe fn JS_MayResolveStandardClass(
    names: *const root::JSAtomState,
    id: root::jsid,
    maybeObj: *mut root::JSObject,
) -> bool {
    let _ = (names, id, maybeObj);
    false
}

pub(crate) unsafe fn JS_EnumerateStandardClasses(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
) -> bool {
    let _ = (cx, obj);
    true
}

pub(crate) unsafe fn JS_NewEnumerateStandardClasses(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    properties: root::JS::MutableHandleIdVector,
    enumerableOnly: bool,
) -> bool {
    let _ = (cx, obj, properties, enumerableOnly);
    true
}

// Functions.

pub(crate) unsafe fn IsCallable(obj: *mut root::JSObject) -> bool {
    q::JS_IsFunction(super::current().base_ctx, obj_val(obj)) != 0
}

pub(crate) unsafe fn IsConstructor(obj: *mut root::JSObject) -> bool {
    q::JS_IsConstructor(super::current().base_ctx, obj_val(obj)) != 0
}

pub(crate) unsafe fn JS_ValueToFunction(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
) -> *mut root::JSFunction {
    let cx = Cx::from_raw(cx);
    let v = (*v.ptr).to_qjs();
    if v.tag() == q::JS_TAG_OBJECT && q::JS_IsFunction(cx.qctx(), v) != 0 {
        return val_obj(v) as *mut root::JSFunction;
    }
    q::JS_ThrowTypeError(cx.qctx(), c"value is not a function".as_ptr());
    ptr::null_mut()
}

/// The value of the own data property `name` of `obj` (a new reference), `JS_UNDEFINED` if there
/// is none, or `JS_EXCEPTION`.
unsafe fn own_data_property(cx: &Cx, obj: *mut root::JSObject, name: &CStr) -> q::JSValue {
    let atom = Atom::from_cstr(cx, name.as_ptr());
    let mut qd: q::JSPropertyDescriptor = std::mem::zeroed();
    let r = q::JS_GetOwnProperty(cx.qctx(), &mut qd, obj_val(obj), atom.atom);
    if r < 0 {
        return q::JS_EXCEPTION;
    }
    if r == 0 {
        return q::JS_UNDEFINED;
    }
    q::JS_FreeValueRT(cx.rt, qd.getter);
    q::JS_FreeValueRT(cx.rt, qd.setter);
    qd.value
}

/// The length of a `JS_TAG_STRING` value (not a rope).
unsafe fn flat_string_len(s: q::JSValue) -> usize {
    let mut data = ptr::null();
    let mut wide = 0;
    q::JS_GetStringData(s, &mut data, &mut wide)
}

/// The function's `name` (null when anonymous). QuickJS keeps no separate explicit or display
/// name, so both ids read the own `name` property.
unsafe fn function_name(
    cx: *mut root::JSContext,
    fun: root::JS::Handle<*mut root::JSFunction>,
    name: root::JS::MutableHandle<*mut root::JSString>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = own_data_property(cx, *fun.ptr as *mut root::JSObject, c"name");
    if is_exception(v) {
        return false;
    }
    let v = cx.adopt(v);
    *name.ptr = if v.is_string() && flat_string_len(v.to_qjs()) > 0 {
        v.to_string()
    } else {
        ptr::null_mut()
    };
    true
}

pub(crate) unsafe fn JS_GetFunctionId(
    cx: *mut root::JSContext,
    fun: root::JS::Handle<*mut root::JSFunction>,
    name: root::JS::MutableHandle<*mut root::JSString>,
) -> bool {
    function_name(cx, fun, name)
}

pub(crate) unsafe fn JS_GetFunctionDisplayId(
    cx: *mut root::JSContext,
    fun: root::JS::Handle<*mut root::JSFunction>,
    name: root::JS::MutableHandle<*mut root::JSString>,
) -> bool {
    function_name(cx, fun, name)
}

pub(crate) unsafe fn JS_GetFunctionArity(fun: *mut root::JSFunction) -> u16 {
    let cx = super::current();
    let v = own_data_property(cx, fun as *mut root::JSObject, c"length");
    if is_exception(v) {
        q::JS_FreeValueRT(cx.rt, q::JS_GetException(cx.qctx()));
        return 0;
    }
    let mut n = 0u32;
    q::JS_ToUint32(cx.qctx(), &mut n, v);
    q::JS_FreeValueRT(cx.rt, v);
    n.min(u16::MAX as u32) as u16
}

pub(crate) unsafe fn GetFunctionRealm(
    cx: *mut root::JSContext,
    objArg: root::JS::HandleObject,
) -> *mut root::JS::Realm {
    let cx = Cx::from_raw(cx);
    let obj = *objArg.ptr;
    if let Some(data) = function_data(obj) {
        if !data.realm.is_null() {
            return data.realm as *mut root::JS::Realm;
        }
    }
    if let Some(data) = object_data(obj) {
        if !data.realm.is_null() {
            return data.realm as *mut root::JS::Realm;
        }
    }
    let qrealm = q::JS_GetFunctionRealm(cx.qctx(), obj_val(obj));
    if qrealm.is_null() {
        return ptr::null_mut();
    }
    let realm = q::JS_GetContextOpaque(qrealm) as *mut RealmData;
    if realm.is_null() {
        cx.current_realm() as *mut root::JS::Realm
    } else {
        realm as *mut root::JS::Realm
    }
}

pub(crate) unsafe fn NewFunctionWithReserved(
    cx: *mut root::JSContext,
    call: root::JSNative,
    nargs: ::std::os::raw::c_uint,
    flags: ::std::os::raw::c_uint,
    name: *const ::std::os::raw::c_char,
) -> *mut root::JSFunction {
    // Every native function has the two extended slots.
    super::JS_NewFunction(cx, call, nargs, flags, name)
}

pub(crate) unsafe fn DefineFunctionWithReserved(
    cx: *mut root::JSContext,
    obj: *mut root::JSObject,
    name: *const ::std::os::raw::c_char,
    call: root::JSNative,
    nargs: ::std::os::raw::c_uint,
    attrs: ::std::os::raw::c_uint,
) -> *mut root::JSFunction {
    super::JS_DefineFunction(
        cx,
        root::JS::Handle::from_marked_location(&obj),
        name,
        call,
        nargs,
        attrs,
    )
}

pub(crate) unsafe fn GetFunctionNativeReserved(
    fun: *mut root::JSObject,
    which: usize,
) -> *const root::JS::Value {
    let data = function_data(fun).expect("GetFunctionNativeReserved on a non-native function");
    &data.slots[which] as *const q::JSValue as *const root::JS::Value
}

pub(crate) unsafe fn SetFunctionNativeReserved(
    fun: *mut root::JSObject,
    which: usize,
    val: *const root::JS::Value,
) {
    let cx = super::current();
    let data = function_data(fun).expect("SetFunctionNativeReserved on a non-native function");
    let new = q::JS_DupValueRT(cx.rt, (*val).to_qjs());
    let old = std::mem::replace(&mut data.slots[which], new);
    q::JS_FreeValueRT(cx.rt, old);
}

pub(crate) unsafe fn RUST_FUNCTION_VALUE_TO_JITINFO(v: root::JS::Value) -> *const root::JSJitInfo {
    if !v.is_object() {
        return ptr::null();
    }
    function_data(v.to_object()).map_or(ptr::null(), |data| data.info)
}

pub(crate) unsafe fn CallJitGetterOp(
    info: *const root::JSJitInfo,
    cx: *mut root::JSContext,
    thisObj: root::JS::HandleObject,
    specializedThis: *mut ::std::os::raw::c_void,
    argc: ::std::os::raw::c_uint,
    vp: *mut root::JS::Value,
) -> bool {
    let _ = argc;
    let getter = (*info)
        .__bindgen_anon_1
        .getter
        .expect("JSJitInfo without a getter");
    let args = root::JSJitGetterCallArgs {
        _base: root::JS::MutableHandle::from_marked_location(vp),
    };
    getter(cx, thisObj, specializedThis, args)
}

pub(crate) unsafe fn CallJitSetterOp(
    info: *const root::JSJitInfo,
    cx: *mut root::JSContext,
    thisObj: root::JS::HandleObject,
    specializedThis: *mut ::std::os::raw::c_void,
    argc: ::std::os::raw::c_uint,
    vp: *mut root::JS::Value,
) -> bool {
    let setter = (*info)
        .__bindgen_anon_1
        .setter
        .expect("JSJitInfo without a setter");
    // `vp` has no argument slot when the setter is called without arguments.
    let mut undefined = JSVal::from_qjs(q::JS_UNDEFINED);
    let arg = if argc > 0 {
        vp.add(2)
    } else {
        &mut undefined as *mut JSVal
    };
    let args = root::JSJitSetterCallArgs {
        _base: root::JS::MutableHandle::from_marked_location(arg),
    };
    setter(cx, thisObj, specializedThis, args)
}

pub(crate) unsafe fn CallJitMethodOp(
    info: *const root::JSJitInfo,
    cx: *mut root::JSContext,
    thisObj: root::JS::HandleObject,
    specializedThis: *mut ::std::os::raw::c_void,
    argc: u32,
    vp: *mut root::JS::Value,
) -> bool {
    let method = (*info)
        .__bindgen_anon_1
        .method
        .expect("JSJitInfo without a method");
    let mut args: root::JSJitMethodCallArgs = std::mem::zeroed();
    args.argv_ = vp.add(2);
    args.argc_ = argc;
    args.set_constructing_((*vp.add(1)).is_magic());
    method(cx, thisObj, specializedThis, &args)
}

pub(crate) unsafe fn Construct1(
    cx: *mut root::JSContext,
    fun: root::JS::Handle<root::JS::Value>,
    args: *const root::JS::HandleValueArray,
    objp: root::JS::MutableHandle<*mut root::JSObject>,
) -> bool {
    let no_new_target: *mut root::JSObject = ptr::null_mut();
    super::Construct(
        cx,
        fun,
        root::JS::Handle::from_marked_location(&no_new_target),
        args,
        objp,
    )
}

// Intrinsics of the current realm.

/// `%Class.prototype%[name]` of the current realm as it was when the realm was created (a new
/// reference); see `intrinsics.rs`.
unsafe fn intrinsic(cx: &Cx, class: q::JSClassID, name: &'static CStr) -> q::JSValue {
    intrinsics::get(cx, Intrinsic::Method(class, name))
}

/// Calls `%Class.prototype%[name]` on `this` (a new reference or `JS_EXCEPTION`).
unsafe fn call_method(
    cx: &Cx,
    class: q::JSClassID,
    name: &'static CStr,
    this: q::JSValue,
    args: &[q::JSValue],
) -> q::JSValue {
    let f = intrinsic(cx, class, name);
    if is_exception(f) {
        return f;
    }
    let mut argv = args.to_vec();
    let r = q::JS_Call(cx.qctx(), f, this, argv.len() as c_int, argv.as_mut_ptr());
    q::JS_FreeValueRT(cx.rt, f);
    r
}

/// Calls the getter `%Class.prototype%[name]` with receiver `this`.
unsafe fn call_getter(
    cx: &Cx,
    class: q::JSClassID,
    name: &'static CStr,
    this: q::JSValue,
) -> q::JSValue {
    let f = intrinsics::get(cx, Intrinsic::Getter(class, name));
    let r = q::JS_Call(cx.qctx(), f, this, 0, ptr::null_mut());
    q::JS_FreeValueRT(cx.rt, f);
    r
}

/// `new %Class%(...args)` (a new reference or `JS_EXCEPTION`).
unsafe fn construct_intrinsic(cx: &Cx, class: q::JSClassID, args: &[q::JSValue]) -> q::JSValue {
    let ctor = intrinsic(cx, class, c"constructor");
    if is_exception(ctor) {
        return ctor;
    }
    let mut argv = args.to_vec();
    let r = q::JS_CallConstructor(cx.qctx(), ctor, argv.len() as c_int, argv.as_mut_ptr());
    q::JS_FreeValueRT(cx.rt, ctor);
    r
}

unsafe fn method_value(
    cx: *mut root::JSContext,
    class: q::JSClassID,
    name: &'static CStr,
    obj: root::JS::HandleObject,
    args: &[q::JSValue],
    rval: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = call_method(cx, class, name, obj_val(*obj.ptr), args);
    set_result(cx, r, rval)
}

unsafe fn method_bool(
    cx: *mut root::JSContext,
    class: q::JSClassID,
    name: &'static CStr,
    obj: root::JS::HandleObject,
    args: &[q::JSValue],
    rval: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = call_method(cx, class, name, obj_val(*obj.ptr), args);
    if is_exception(r) {
        return false;
    }
    *rval = q::JS_ToBool(cx.qctx(), r) > 0;
    q::JS_FreeValueRT(cx.rt, r);
    true
}

unsafe fn method_void(
    cx: *mut root::JSContext,
    class: q::JSClassID,
    name: &'static CStr,
    obj: root::JS::HandleObject,
    args: &[q::JSValue],
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = call_method(cx, class, name, obj_val(*obj.ptr), args);
    if is_exception(r) {
        return false;
    }
    q::JS_FreeValueRT(cx.rt, r);
    true
}

unsafe fn size(cx: *mut root::JSContext, class: q::JSClassID, obj: root::JS::HandleObject) -> u32 {
    let cx = Cx::from_raw(cx);
    let r = call_getter(cx, class, c"size", obj_val(*obj.ptr));
    if is_exception(r) {
        return 0;
    }
    let mut n = 0u32;
    q::JS_ToUint32(cx.qctx(), &mut n, r);
    q::JS_FreeValueRT(cx.rt, r);
    n
}

unsafe fn new_intrinsic(cx: *mut root::JSContext, class: q::JSClassID) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let obj = construct_intrinsic(cx, class, &[]);
    adopt_object(cx, obj)
}

// Map, Set and WeakMap.

pub(crate) unsafe fn NewMapObject(cx: *mut root::JSContext) -> *mut root::JSObject {
    new_intrinsic(cx, CLASS_MAP)
}

pub(crate) unsafe fn IsMapObject(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    isMap: *mut bool,
) -> bool {
    let _ = cx;
    *isMap = class_of(*obj.ptr) == CLASS_MAP;
    true
}

pub(crate) unsafe fn MapSize(cx: *mut root::JSContext, obj: root::JS::HandleObject) -> u32 {
    size(cx, CLASS_MAP, obj)
}

pub(crate) unsafe fn MapGet(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_MAP, c"get", obj, &[(*key.ptr).to_qjs()], rval)
}

pub(crate) unsafe fn MapHas(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    rval: *mut bool,
) -> bool {
    method_bool(cx, CLASS_MAP, c"has", obj, &[(*key.ptr).to_qjs()], rval)
}

pub(crate) unsafe fn MapSet(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    val: root::JS::HandleValue,
) -> bool {
    method_void(
        cx,
        CLASS_MAP,
        c"set",
        obj,
        &[(*key.ptr).to_qjs(), (*val.ptr).to_qjs()],
    )
}

pub(crate) unsafe fn MapDelete(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    rval: *mut bool,
) -> bool {
    method_bool(cx, CLASS_MAP, c"delete", obj, &[(*key.ptr).to_qjs()], rval)
}

pub(crate) unsafe fn MapClear(cx: *mut root::JSContext, obj: root::JS::HandleObject) -> bool {
    method_void(cx, CLASS_MAP, c"clear", obj, &[])
}

pub(crate) unsafe fn MapKeys(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_MAP, c"keys", obj, &[], rval)
}

pub(crate) unsafe fn MapValues(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_MAP, c"values", obj, &[], rval)
}

pub(crate) unsafe fn MapEntries(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_MAP, c"entries", obj, &[], rval)
}

pub(crate) unsafe fn MapForEach(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    callbackFn: root::JS::HandleValue,
    thisVal: root::JS::HandleValue,
) -> bool {
    method_void(
        cx,
        CLASS_MAP,
        c"forEach",
        obj,
        &[(*callbackFn.ptr).to_qjs(), (*thisVal.ptr).to_qjs()],
    )
}

pub(crate) unsafe fn NewSetObject(cx: *mut root::JSContext) -> *mut root::JSObject {
    new_intrinsic(cx, CLASS_SET)
}

pub(crate) unsafe fn IsSetObject(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    isSet: *mut bool,
) -> bool {
    let _ = cx;
    *isSet = class_of(*obj.ptr) == CLASS_SET;
    true
}

pub(crate) unsafe fn SetSize(cx: *mut root::JSContext, obj: root::JS::HandleObject) -> u32 {
    size(cx, CLASS_SET, obj)
}

pub(crate) unsafe fn SetHas(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    rval: *mut bool,
) -> bool {
    method_bool(cx, CLASS_SET, c"has", obj, &[(*key.ptr).to_qjs()], rval)
}

pub(crate) unsafe fn SetAdd(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
) -> bool {
    method_void(cx, CLASS_SET, c"add", obj, &[(*key.ptr).to_qjs()])
}

pub(crate) unsafe fn SetDelete(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    rval: *mut bool,
) -> bool {
    method_bool(cx, CLASS_SET, c"delete", obj, &[(*key.ptr).to_qjs()], rval)
}

pub(crate) unsafe fn SetClear(cx: *mut root::JSContext, obj: root::JS::HandleObject) -> bool {
    method_void(cx, CLASS_SET, c"clear", obj, &[])
}

pub(crate) unsafe fn SetKeys(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_SET, c"keys", obj, &[], rval)
}

pub(crate) unsafe fn SetValues(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_SET, c"values", obj, &[], rval)
}

pub(crate) unsafe fn SetEntries(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    rval: root::JS::MutableHandleValue,
) -> bool {
    method_value(cx, CLASS_SET, c"entries", obj, &[], rval)
}

pub(crate) unsafe fn SetForEach(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    callbackFn: root::JS::HandleValue,
    thisVal: root::JS::HandleValue,
) -> bool {
    method_void(
        cx,
        CLASS_SET,
        c"forEach",
        obj,
        &[(*callbackFn.ptr).to_qjs(), (*thisVal.ptr).to_qjs()],
    )
}

// Servo uses these only for the cross-origin property holder cache of a WindowProxy or Location
// (`ensure_cross_origin_property_holder`): the map in the object's reserved slot, the key the
// accessing realm's key object, the value a holder made in the accessing realm. A QuickJS WeakMap
// keeps its values alive as long as the map, so the holder's realm would live as long as the
// object, and its methods are script-replaceable. Instead the map is an id, and the holder is
// kept by the accessing realm's key object: no script runs, and nothing the object keeps alive
// refers to the accessing realm. A holder map's entries are freed with the accessing realms.

pub(crate) unsafe fn NewWeakMapObject(cx: *mut root::JSContext) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let map = intrinsics::new_holder_map(cx);
    adopt_object(cx, map)
}

pub(crate) unsafe fn IsWeakMapObject(obj: *mut root::JSObject) -> bool {
    q::JS_GetClassID(obj_val(obj)) == CLASS_WEAKMAP || intrinsics::is_holder_map(obj_val(obj))
}

/// The current realm's key object: private to the shim, kept alive by the realm's global.
pub(crate) unsafe fn GetRealmKeyObject(cx: *mut root::JSContext) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let key = intrinsics::key_object(cx);
    adopt_object(cx, q::JS_DupValueRT(cx.rt, key))
}

pub(crate) unsafe fn GetWeakMapEntry(
    cx: *mut root::JSContext,
    mapObj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    val: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let (map, key) = (obj_val(*mapObj.ptr), (*key.ptr).to_qjs());
    let v = match intrinsics::holder(cx.rt, map, key) {
        Some(v) => v,
        None => call_method(cx, CLASS_WEAKMAP, c"get", map, &[key]),
    };
    set_result(cx, v, val)
}

pub(crate) unsafe fn SetWeakMapEntry(
    cx: *mut root::JSContext,
    mapObj: root::JS::HandleObject,
    key: root::JS::HandleValue,
    val: root::JS::HandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let (map, key, val) = (
        obj_val(*mapObj.ptr),
        (*key.ptr).to_qjs(),
        (*val.ptr).to_qjs(),
    );
    if intrinsics::set_holder(cx.rt, map, key, val) {
        return true;
    }
    let r = call_method(cx, CLASS_WEAKMAP, c"set", map, &[key, val]);
    if is_exception(r) {
        return false;
    }
    q::JS_FreeValueRT(cx.rt, r);
    true
}

// Date.

pub(crate) unsafe fn NewDateObject(
    cx: *mut root::JSContext,
    time: root::JS::ClippedTime,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let date = q::JS_NewDate(cx.qctx(), time.t);
    adopt_object(cx, date)
}

pub(crate) unsafe fn ObjectIsDate(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    isDate: *mut bool,
) -> bool {
    let _ = cx;
    *isDate = class_of(*obj.ptr) == CLASS_DATE;
    true
}

pub(crate) unsafe fn DateGetMsecSinceEpoch(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    msecSinceEpoch: *mut f64,
) -> bool {
    let cx = Cx::from_raw(cx);
    let t = call_method(cx, CLASS_DATE, c"getTime", obj_val(*obj.ptr), &[]);
    if is_exception(t) {
        return false;
    }
    let r = q::JS_ToFloat64(cx.qctx(), msecSinceEpoch, t);
    q::JS_FreeValueRT(cx.rt, t);
    ok(r)
}

// RegExp.

/// The flags string of SpiderMonkey `RegExpFlags`, in `RegExp.prototype.flags` order.
fn regexp_flags(flags: root::JS::RegExpFlags) -> Vec<u8> {
    const FLAGS: [(u8, u8); 8] = [
        (64, b'd'),
        (2, b'g'),
        (1, b'i'),
        (4, b'm'),
        (32, b's'),
        (16, b'u'),
        (128, b'v'),
        (8, b'y'),
    ];
    FLAGS
        .iter()
        .filter(|(bit, _)| flags.flags_ & bit != 0)
        .map(|(_, c)| *c)
        .collect()
}

/// `new RegExp(source, flags)` (a new reference or `JS_EXCEPTION`).
unsafe fn new_regexp(
    cx: &Cx,
    chars: *const u16,
    length: usize,
    flags: root::JS::RegExpFlags,
) -> q::JSValue {
    let qctx = cx.qctx();
    let source = q::JS_NewStringUTF16(qctx, chars, length);
    if is_exception(source) {
        return source;
    }
    let flags = regexp_flags(flags);
    let flags = q::JS_NewStringLen(qctx, flags.as_ptr() as *const _, flags.len());
    let re = construct_intrinsic(cx, CLASS_REGEXP, &[source, flags]);
    q::JS_FreeValueRT(cx.rt, source);
    q::JS_FreeValueRT(cx.rt, flags);
    re
}

pub(crate) unsafe fn NewUCRegExpObject(
    cx: *mut root::JSContext,
    chars: *const u16,
    length: usize,
    flags: root::JS::RegExpFlags,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let re = new_regexp(cx, chars, length, flags);
    adopt_object(cx, re)
}

pub(crate) unsafe fn ObjectIsRegExp(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    isRegExp: *mut bool,
) -> bool {
    let _ = cx;
    *isRegExp = class_of(*obj.ptr) == CLASS_REGEXP;
    true
}

/// Stores the syntax error of an invalid pattern in `error` (undefined if it is valid).
pub(crate) unsafe fn CheckRegExpSyntax(
    cx: *mut root::JSContext,
    chars: *const u16,
    length: usize,
    flags: root::JS::RegExpFlags,
    error: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let re = new_regexp(cx, chars, length, flags);
    if is_exception(re) {
        let exception = q::JS_GetException(cx.qctx());
        *error.ptr = cx.adopt(exception);
    } else {
        q::JS_FreeValueRT(cx.rt, re);
        *error.ptr = JSVal::from_qjs(q::JS_UNDEFINED);
    }
    true
}

/// `ToNumber(obj[name])`, or `None` with an exception pending.
unsafe fn number_property(cx: &Cx, obj: q::JSValue, name: &CStr) -> Option<f64> {
    let v = q::JS_GetPropertyStr(cx.qctx(), obj, name.as_ptr());
    if is_exception(v) {
        return None;
    }
    let mut n = 0f64;
    let r = q::JS_ToFloat64(cx.qctx(), &mut n, v);
    q::JS_FreeValueRT(cx.rt, v);
    if r < 0 {
        None
    } else {
        Some(n)
    }
}

/// The length of the matched substring of a match array.
unsafe fn match_length(cx: &Cx, result: q::JSValue) -> Option<f64> {
    let matched = q::JS_GetPropertyUint32(cx.qctx(), result, 0);
    if is_exception(matched) {
        return None;
    }
    let len = number_property(cx, matched, c"length");
    q::JS_FreeValueRT(cx.rt, matched);
    len
}

/// Matches `reobj` against `chars` from `*indexp` with `RegExp.prototype.exec`, leaving its
/// `lastIndex` unchanged. On a match `*indexp` is the end of the match and `rval` is `true` (if
/// `test`) or the match array; otherwise `rval` is null.
pub(crate) unsafe fn ExecuteRegExpNoStatics(
    cx: *mut root::JSContext,
    reobj: root::JS::Handle<*mut root::JSObject>,
    chars: *const u16,
    length: usize,
    indexp: *mut usize,
    test: bool,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let re = obj_val(*reobj.ptr);
    let input = q::JS_NewStringUTF16(qctx, chars, length);
    if is_exception(input) {
        return false;
    }
    let last_index = Atom::from_cstr(cx, c"lastIndex".as_ptr());
    let saved = q::JS_GetProperty(qctx, re, last_index.atom);
    if is_exception(saved)
        || q::JS_SetProperty(
            qctx,
            re,
            last_index.atom,
            q::JS_NewInt64(qctx, *indexp as i64),
        ) < 0
    {
        q::JS_FreeValueRT(cx.rt, saved);
        q::JS_FreeValueRT(cx.rt, input);
        return false;
    }
    let result = call_method(cx, CLASS_REGEXP, c"exec", re, &[input]);
    q::JS_FreeValueRT(cx.rt, input);
    let restored = q::JS_SetProperty(qctx, re, last_index.atom, saved) >= 0;
    if is_exception(result) || !restored {
        q::JS_FreeValueRT(cx.rt, result);
        return false;
    }
    if result.tag() != q::JS_TAG_OBJECT {
        q::JS_FreeValueRT(cx.rt, result);
        *rval.ptr = JSVal::from_qjs(q::JS_NULL);
        return true;
    }
    let (Some(start), Some(len)) = (
        number_property(cx, result, c"index"),
        match_length(cx, result),
    ) else {
        q::JS_FreeValueRT(cx.rt, result);
        return false;
    };
    *indexp = (start + len) as usize;
    if test {
        q::JS_FreeValueRT(cx.rt, result);
        *rval.ptr = JSVal::from_qjs(q::JS_TRUE);
        return true;
    }
    *rval.ptr = cx.adopt(result);
    true
}

// for-of iteration.

pub(crate) unsafe fn JS_ForOfIteratorInit(
    iterator: *mut root::JS::ForOfIterator,
    iterable: root::JS::HandleValue,
    nonIterableBehavior: root::JS::ForOfIterator_NonIterableBehavior,
) -> bool {
    let it = &mut *iterator;
    let cx = Cx::from_raw(it.cx_);
    let qctx = cx.qctx();
    let iterable = (*iterable.ptr).to_qjs();
    let atom = well_known_symbol_atom(cx, root::JS::SymbolCode::iterator as usize);
    let method = q::JS_GetProperty(qctx, iterable, atom);
    if is_exception(method) {
        return false;
    }
    if method.tag() == q::JS_TAG_UNDEFINED
        && nonIterableBehavior == root::JS::ForOfIterator_NonIterableBehavior::AllowNonIterable
    {
        return true;
    }
    if q::JS_IsFunction(qctx, method) == 0 {
        q::JS_FreeValueRT(cx.rt, method);
        q::JS_ThrowTypeError(qctx, c"value is not iterable".as_ptr());
        return false;
    }
    let iter = q::JS_Call(qctx, method, iterable, 0, ptr::null_mut());
    q::JS_FreeValueRT(cx.rt, method);
    if is_exception(iter) {
        return false;
    }
    if iter.tag() != q::JS_TAG_OBJECT {
        q::JS_FreeValueRT(cx.rt, iter);
        q::JS_ThrowTypeError(qctx, c"iterator is not an object".as_ptr());
        return false;
    }
    let next = q::JS_GetPropertyStr(qctx, iter, c"next".as_ptr());
    let iter = cx.adopt(iter);
    if is_exception(next) {
        return false;
    }
    it.iterator.data = iter.to_object();
    it.nextMethod.data = cx.adopt(next);
    true
}

pub(crate) unsafe fn JS_ForOfIteratorNext(
    iterator: *mut root::JS::ForOfIterator,
    val: root::JS::MutableHandleValue,
    done: *mut bool,
) -> bool {
    let it = &mut *iterator;
    let cx = Cx::from_raw(it.cx_);
    let qctx = cx.qctx();
    let result = q::JS_Call(
        qctx,
        it.nextMethod.data.to_qjs(),
        obj_val(it.iterator.data),
        0,
        ptr::null_mut(),
    );
    if is_exception(result) {
        return false;
    }
    if result.tag() != q::JS_TAG_OBJECT {
        q::JS_FreeValueRT(cx.rt, result);
        q::JS_ThrowTypeError(qctx, c"iterator result is not an object".as_ptr());
        return false;
    }
    let is_done = q::JS_GetPropertyStr(qctx, result, c"done".as_ptr());
    if is_exception(is_done) {
        q::JS_FreeValueRT(cx.rt, result);
        return false;
    }
    *done = q::JS_ToBool(qctx, is_done) > 0;
    q::JS_FreeValueRT(cx.rt, is_done);
    if *done {
        q::JS_FreeValueRT(cx.rt, result);
        *val.ptr = JSVal::from_qjs(q::JS_UNDEFINED);
        return true;
    }
    let value = q::JS_GetPropertyStr(qctx, result, c"value".as_ptr());
    q::JS_FreeValueRT(cx.rt, result);
    set_result(cx, value, val)
}

pub(crate) unsafe fn JS_ForOfIteratorValueIsIterable(
    iterator: *const root::JS::ForOfIterator,
) -> bool {
    !(*iterator).iterator.data.is_null()
}
