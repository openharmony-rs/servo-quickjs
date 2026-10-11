/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Property access and definition, function and property specs, and calls.

use std::ffi::{c_char, c_int, c_uint};
use std::ptr;

use servo_quickjs as q;

use super::class::new_function;
use super::{obj_val, Cx};
use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;

/// SpiderMonkey `JSPROP_*` attributes as QuickJS data-property flags.
pub(crate) fn data_flags(attrs: c_uint) -> c_int {
    let mut flags = 0;
    if attrs & root::JSPROP_PERMANENT as c_uint == 0 {
        flags |= q::JS_PROP_CONFIGURABLE;
    }
    if attrs & root::JSPROP_READONLY as c_uint == 0 {
        flags |= q::JS_PROP_WRITABLE;
    }
    if attrs & root::JSPROP_ENUMERATE as c_uint != 0 {
        flags |= q::JS_PROP_ENUMERABLE;
    }
    flags as c_int | q::JS_PROP_THROW as c_int
}

fn accessor_flags(attrs: c_uint) -> c_int {
    data_flags(attrs) & !(q::JS_PROP_WRITABLE as c_int)
}

/// An owned QuickJS atom, freed on drop.
pub(crate) struct Atom {
    pub atom: q::JSAtom,
    rt: *mut q::JSRuntime,
}

impl Drop for Atom {
    fn drop(&mut self) {
        if self.atom != 0 {
            unsafe { q::JS_FreeAtomRT(self.rt, self.atom) };
        }
    }
}

impl Atom {
    pub unsafe fn from_cstr(cx: &Cx, name: *const c_char) -> Atom {
        Atom {
            atom: q::JS_NewAtom(cx.qctx(), name),
            rt: cx.rt,
        }
    }

    pub unsafe fn from_utf16(cx: &Cx, name: *const u16, len: usize) -> Atom {
        let s = q::JS_NewStringUTF16(cx.qctx(), name, len);
        let atom = q::JS_ValueToAtom(cx.qctx(), s);
        q::JS_FreeValueRT(cx.rt, s);
        Atom { atom, rt: cx.rt }
    }

    pub unsafe fn from_spec_name(cx: &Cx, name: root::JSPropertySpec_Name) -> Atom {
        let raw = name.symbol_;
        if raw != 0 && raw <= root::JS::SymbolCode::Limit as usize {
            let atom = well_known_symbol_atom(cx, raw - 1);
            return Atom {
                atom: q::JS_DupAtom(cx.base_ctx, atom),
                rt: cx.rt,
            };
        }
        Atom::from_cstr(cx, name.string_)
    }
}

const WELL_KNOWN_SYMBOLS: [&std::ffi::CStr; 15] = [
    c"isConcatSpreadable",
    c"iterator",
    c"match",
    c"replace",
    c"search",
    c"species",
    c"hasInstance",
    c"split",
    c"toPrimitive",
    c"toStringTag",
    c"unscopables",
    c"asyncIterator",
    c"matchAll",
    c"dispose",
    c"asyncDispose",
];

thread_local! {
    /// Well-known symbols are predefined QuickJS atoms, so their indices are the same in every
    /// runtime and can be cached.
    static SYMBOL_ATOMS: std::cell::Cell<[q::JSAtom; 15]> = const { std::cell::Cell::new([0; 15]) };
}

/// The atom of `Symbol[name]` for SpiderMonkey's `SymbolCode` `code` (atoms are permanent).
pub(crate) unsafe fn well_known_symbol_atom(cx: &Cx, code: usize) -> q::JSAtom {
    let cached = SYMBOL_ATOMS.with(|c| c.get()[code]);
    if cached != 0 {
        return cached;
    }
    let atom = lookup_well_known_symbol_atom(cx, code);
    SYMBOL_ATOMS.with(|c| {
        let mut atoms = c.get();
        atoms[code] = atom;
        c.set(atoms);
    });
    atom
}

unsafe fn lookup_well_known_symbol_atom(cx: &Cx, code: usize) -> q::JSAtom {
    let qctx = cx.base_ctx;
    let global = q::JS_GetGlobalObject(qctx);
    let symbol_ctor = q::JS_GetPropertyStr(qctx, global, c"Symbol".as_ptr());
    let symbol = q::JS_GetPropertyStr(qctx, symbol_ctor, WELL_KNOWN_SYMBOLS[code].as_ptr());
    // QuickJS has no `Symbol.dispose` and `Symbol.asyncDispose`: their atom is `JS_ATOM_NULL`.
    let atom = if symbol.tag() == q::JS_TAG_SYMBOL {
        q::JS_ValueToAtom(qctx, symbol)
    } else {
        q::JS_ATOM_NULL
    };
    for v in [symbol, symbol_ctor, global] {
        q::JS_FreeValue(qctx, v);
    }
    // Well-known symbols live as long as the runtime; drop our extra count.
    q::JS_FreeAtomRT(cx.rt, atom);
    atom
}

pub(crate) unsafe fn GetWellKnownSymbol(
    cx: *mut root::JSContext,
    which: root::JS::SymbolCode,
) -> *mut root::JS::Symbol {
    let cx = Cx::from_raw(cx);
    let atom = well_known_symbol_atom(cx, which as usize);
    super::atom_to_symbol_ptr(atom)
}

/// Converts a QuickJS status (`< 0` exception) to SpiderMonkey's `bool`.
fn ok(status: c_int) -> bool {
    status >= 0
}

unsafe fn get(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let v = q::JS_GetProperty(cx.qctx(), obj_val(obj), atom);
    if v.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *vp.ptr = cx.adopt(v);
    true
}

/// Like SpiderMonkey's `JS_SetProperty`, a failed assignment (e.g. to a read-only property) is
/// ignored, as in sloppy mode; only exceptions fail.
unsafe fn set(cx: &Cx, obj: *mut root::JSObject, atom: q::JSAtom, v: JSVal) -> bool {
    let v = q::JS_DupValueRT(cx.rt, v.to_qjs());
    let obj = obj_val(obj);
    ok(q::JS_SetPropertyInternal(cx.qctx(), obj, atom, v, obj, 0))
}

unsafe fn define_value(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    v: q::JSValue,
    attrs: c_uint,
) -> bool {
    // JS_DefinePropertyValue consumes `v`.
    ok(q::JS_DefinePropertyValue(
        cx.qctx(),
        obj_val(obj),
        atom,
        v,
        data_flags(attrs),
    ))
}

unsafe fn define_accessor(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    getter: q::JSValue,
    setter: q::JSValue,
    attrs: c_uint,
) -> bool {
    // JS_DefinePropertyGetSet consumes `getter` and `setter`.
    ok(q::JS_DefinePropertyGetSet(
        cx.qctx(),
        obj_val(obj),
        atom,
        getter,
        setter,
        accessor_flags(attrs),
    ))
}

/// SetFunctionName with a prefix: `"get name"`, or `"get [description]"` for a symbol key. The
/// caller owns the returned atom.
unsafe fn accessor_name(cx: &Cx, prefix: &str, name: q::JSAtom) -> q::JSAtom {
    let qctx = cx.qctx();
    let is_symbol = crate::sys::jsid::from_atom(name).is_symbol();
    let key = if is_symbol {
        q::JS_AtomToValue(qctx, name)
    } else {
        q::JS_AtomToString(qctx, name)
    };
    let text = if is_symbol {
        let desc = symbol_description(cx, key);
        let text = string_lossy(qctx, desc);
        q::JS_FreeValueRT(cx.rt, desc);
        format!("{prefix} [{text}]")
    } else {
        format!("{prefix} {}", string_lossy(qctx, key))
    };
    q::JS_FreeValueRT(cx.rt, key);
    q::JS_NewAtomLen(qctx, text.as_ptr() as *const c_char, text.len())
}

/// `sym.description` through the realm's original getter (a new reference or `JS_EXCEPTION`).
pub(crate) unsafe fn symbol_description(cx: &Cx, sym: q::JSValue) -> q::JSValue {
    let getter = super::intrinsics::get(
        cx,
        super::intrinsics::Intrinsic::Getter(super::intrinsics::CLASS_SYMBOL, c"description"),
    );
    let desc = q::JS_Call(cx.qctx(), getter, sym, 0, ptr::null_mut());
    q::JS_FreeValueRT(cx.rt, getter);
    desc
}

unsafe fn string_lossy(qctx: *mut q::JSContext, v: q::JSValue) -> String {
    let s = q::JS_ToCString(qctx, v);
    if s.is_null() {
        q::JS_FreeValue(qctx, q::JS_GetException(qctx));
        return String::new();
    }
    let text = std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned();
    q::JS_FreeCString(qctx, s);
    text
}

unsafe fn native_fn(
    cx: &Cx,
    native: root::JSNative,
    info: *const root::JSJitInfo,
    nargs: u32,
    name: q::JSAtom,
) -> q::JSValue {
    if native.is_none() {
        return q::JS_UNDEFINED;
    }
    new_function(cx, native, info, nargs, false, name)
}

unsafe fn handle_obj_val(cx: &Cx, h: root::JS::Handle<*mut root::JSObject>) -> q::JSValue {
    let o = *h.ptr;
    if o.is_null() {
        q::JS_UNDEFINED
    } else {
        q::JS_DupValueRT(cx.rt, obj_val(o))
    }
}

pub(crate) unsafe fn JS_GetProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    get(cx, *obj.ptr, atom.atom, vp)
}

pub(crate) unsafe fn JS_GetPropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    get(cx, *obj.ptr, (*id.ptr).to_atom(), vp)
}

pub(crate) unsafe fn JS_GetUCProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const u16,
    namelen: usize,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_utf16(cx, name, namelen);
    get(cx, *obj.ptr, atom.atom, vp)
}

pub(crate) unsafe fn JS_GetElement(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    index: u32,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_GetPropertyUint32(cx.qctx(), obj_val(*obj.ptr), index);
    if v.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *vp.ptr = cx.adopt(v);
    true
}

pub(crate) unsafe fn JS_SetProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    v: root::JS::Handle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    set(cx, *obj.ptr, atom.atom, *v.ptr)
}

pub(crate) unsafe fn JS_SetPropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    v: root::JS::Handle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    set(cx, *obj.ptr, (*id.ptr).to_atom(), *v.ptr)
}

pub(crate) unsafe fn JS_SetUCProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const u16,
    namelen: usize,
    v: root::JS::Handle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_utf16(cx, name, namelen);
    set(cx, *obj.ptr, atom.atom, *v.ptr)
}

pub(crate) unsafe fn JS_SetElement(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    index: u32,
    v: root::JS::Handle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = q::JS_NewAtomUInt32(cx.qctx(), index);
    if atom == q::JS_ATOM_NULL {
        return false;
    }
    let atom = cx.adopt_atom(atom);
    set(cx, *obj.ptr, atom, *v.ptr)
}

unsafe fn has(cx: &Cx, obj: *mut root::JSObject, atom: q::JSAtom, foundp: *mut bool) -> bool {
    let r = q::JS_HasPropertyCached(cx.qctx(), obj_val(obj), atom, 0);
    if r < 0 {
        return false;
    }
    *foundp = r > 0;
    true
}

unsafe fn has_own(cx: &Cx, obj: *mut root::JSObject, atom: q::JSAtom, foundp: *mut bool) -> bool {
    let r = q::JS_GetOwnProperty(cx.qctx(), ptr::null_mut(), obj_val(obj), atom);
    if r < 0 {
        return false;
    }
    *foundp = r > 0;
    true
}

pub(crate) unsafe fn JS_HasProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    foundp: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    has(cx, *obj.ptr, atom.atom, foundp)
}

pub(crate) unsafe fn JS_HasPropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    foundp: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    has(cx, *obj.ptr, (*id.ptr).to_atom(), foundp)
}

pub(crate) unsafe fn JS_HasOwnProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    foundp: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    has_own(cx, *obj.ptr, atom.atom, foundp)
}

pub(crate) unsafe fn JS_HasOwnPropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    foundp: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    has_own(cx, *obj.ptr, (*id.ptr).to_atom(), foundp)
}

pub(crate) unsafe fn JS_AlreadyHasOwnPropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    foundp: *mut bool,
) -> bool {
    JS_HasOwnPropertyById(cx, obj, id, foundp)
}

unsafe fn delete(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let r = q::JS_DeleteProperty(cx.qctx(), obj_val(obj), atom, 0);
    if r < 0 {
        return false;
    }
    if r > 0 {
        (*result).succeed();
    } else {
        (*result).fail(root::JSErrNum::JSMSG_CANT_DELETE);
    }
    true
}

pub(crate) unsafe fn JS_DeleteProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    delete(cx, *obj.ptr, atom.atom, result)
}

pub(crate) unsafe fn JS_DeletePropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let cx = Cx::from_raw(cx);
    delete(cx, *obj.ptr, (*id.ptr).to_atom(), result)
}

// Definitions by name.

pub(crate) unsafe fn JS_DefineProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    value: root::JS::Handle<root::JS::Value>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    let v = q::JS_DupValueRT(cx.rt, (*value.ptr).to_qjs());
    define_value(cx, *obj.ptr, atom.atom, v, attrs)
}

pub(crate) unsafe fn JS_DefineProperty1(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    getter: root::JSNative,
    setter: root::JSNative,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    let g = native_fn(cx, getter, ptr::null(), 0, atom.atom);
    let s = native_fn(cx, setter, ptr::null(), 1, atom.atom);
    define_accessor(cx, *obj.ptr, atom.atom, g, s, attrs)
}

pub(crate) unsafe fn JS_DefineProperty2(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    getter: root::JS::Handle<*mut root::JSObject>,
    setter: root::JS::Handle<*mut root::JSObject>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    let g = handle_obj_val(cx, getter);
    let s = handle_obj_val(cx, setter);
    define_accessor(cx, *obj.ptr, atom.atom, g, s, attrs)
}

pub(crate) unsafe fn JS_DefineProperty3(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    value: root::JS::Handle<*mut root::JSObject>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    let v = handle_obj_val(cx, value);
    define_value(cx, *obj.ptr, atom.atom, v, attrs)
}

pub(crate) unsafe fn JS_DefineProperty4(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    value: root::JS::Handle<*mut root::JSString>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    let v = q::JS_DupValueRT(
        cx.rt,
        q::JSValue::mkptr(q::JS_TAG_STRING, *value.ptr as *mut _),
    );
    define_value(cx, *obj.ptr, atom.atom, v, attrs)
}

pub(crate) unsafe fn JS_DefineProperty5(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    value: i32,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    define_value(
        cx,
        *obj.ptr,
        atom.atom,
        q::JS_NewInt32(cx.qctx(), value),
        attrs,
    )
}

pub(crate) unsafe fn JS_DefineProperty6(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    value: u32,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    define_value(
        cx,
        *obj.ptr,
        atom.atom,
        q::JS_NewUint32(cx.qctx(), value),
        attrs,
    )
}

pub(crate) unsafe fn JS_DefineProperty7(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    value: f64,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    define_value(
        cx,
        *obj.ptr,
        atom.atom,
        q::JS_NewFloat64(cx.qctx(), value),
        attrs,
    )
}

// Definitions by id.

pub(crate) unsafe fn JS_DefinePropertyById2(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    value: root::JS::Handle<root::JS::Value>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_DupValueRT(cx.rt, (*value.ptr).to_qjs());
    define_value(cx, *obj.ptr, (*id.ptr).to_atom(), v, attrs)
}

pub(crate) unsafe fn JS_DefinePropertyById3(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    getter: root::JSNative,
    setter: root::JSNative,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = (*id.ptr).to_atom();
    let g = native_fn(cx, getter, ptr::null(), 0, atom);
    let s = native_fn(cx, setter, ptr::null(), 1, atom);
    define_accessor(cx, *obj.ptr, atom, g, s, attrs)
}

pub(crate) unsafe fn JS_DefinePropertyById4(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    getter: root::JS::Handle<*mut root::JSObject>,
    setter: root::JS::Handle<*mut root::JSObject>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let g = handle_obj_val(cx, getter);
    let s = handle_obj_val(cx, setter);
    define_accessor(cx, *obj.ptr, (*id.ptr).to_atom(), g, s, attrs)
}

pub(crate) unsafe fn JS_DefinePropertyById5(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    value: root::JS::Handle<*mut root::JSObject>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = handle_obj_val(cx, value);
    define_value(cx, *obj.ptr, (*id.ptr).to_atom(), v, attrs)
}

pub(crate) unsafe fn JS_DefinePropertyById6(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    value: root::JS::Handle<*mut root::JSString>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_DupValueRT(
        cx.rt,
        q::JSValue::mkptr(q::JS_TAG_STRING, *value.ptr as *mut _),
    );
    define_value(cx, *obj.ptr, (*id.ptr).to_atom(), v, attrs)
}

pub(crate) unsafe fn JS_DefinePropertyById7(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    value: i32,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_NewInt32(cx.qctx(), value);
    define_value(cx, *obj.ptr, (*id.ptr).to_atom(), v, attrs)
}

pub(crate) unsafe fn JS_DefinePropertyById8(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    value: u32,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_NewUint32(cx.qctx(), value);
    define_value(cx, *obj.ptr, (*id.ptr).to_atom(), v, attrs)
}

pub(crate) unsafe fn JS_DefineElement(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    index: u32,
    value: root::JS::Handle<root::JS::Value>,
    attrs: ::std::os::raw::c_uint,
) -> bool {
    let cx = Cx::from_raw(cx);
    let v = q::JS_DupValueRT(cx.rt, (*value.ptr).to_qjs());
    ok(q::JS_DefinePropertyValueUint32(
        cx.qctx(),
        obj_val(*obj.ptr),
        index,
        v,
        data_flags(attrs),
    ))
}

// Property descriptors.

/// Defines a property from a SpiderMonkey descriptor (`[[DefineOwnProperty]]`).
unsafe fn define_desc(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    desc: &root::JS::PropertyDescriptor,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let mut flags = 0;
    if desc.hasConfigurable_() {
        flags |= q::JS_PROP_HAS_CONFIGURABLE;
        if desc.configurable_() {
            flags |= q::JS_PROP_CONFIGURABLE;
        }
    }
    if desc.hasEnumerable_() {
        flags |= q::JS_PROP_HAS_ENUMERABLE;
        if desc.enumerable_() {
            flags |= q::JS_PROP_ENUMERABLE;
        }
    }
    if desc.hasWritable_() {
        flags |= q::JS_PROP_HAS_WRITABLE;
        if desc.writable_() {
            flags |= q::JS_PROP_WRITABLE;
        }
    }
    let mut value = q::JS_UNDEFINED;
    let mut getter = q::JS_UNDEFINED;
    let mut setter = q::JS_UNDEFINED;
    if desc.hasValue_() {
        flags |= q::JS_PROP_HAS_VALUE;
        value = desc.value_.to_qjs();
    }
    if desc.hasGetter_() {
        flags |= q::JS_PROP_HAS_GET;
        if !desc.getter_.is_null() {
            getter = obj_val(desc.getter_);
        }
    }
    if desc.hasSetter_() {
        flags |= q::JS_PROP_HAS_SET;
        if !desc.setter_.is_null() {
            setter = obj_val(desc.setter_);
        }
    }
    // Without JS_PROP_THROW a rejected definition returns 0, which maps to ObjectOpResult.
    let r = q::JS_DefineProperty(
        cx.qctx(),
        obj_val(obj),
        atom,
        value,
        getter,
        setter,
        flags as c_int,
    );
    if r < 0 {
        return false;
    }
    if r > 0 {
        (*result).succeed();
    } else {
        (*result).fail(root::JSErrNum::JSMSG_CANT_DEFINE_PROP_OBJECT_NOT_EXTENSIBLE);
    }
    true
}

pub(crate) unsafe fn JS_DefinePropertyById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    desc: root::JS::Handle<root::JS::PropertyDescriptor>,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let cx = Cx::from_raw(cx);
    define_desc(cx, *obj.ptr, (*id.ptr).to_atom(), &*desc.ptr, result)
}

pub(crate) unsafe fn JS_DefinePropertyById1(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    desc: root::JS::Handle<root::JS::PropertyDescriptor>,
) -> bool {
    let mut result = root::JS::ObjectOpResult { code_: 0 };
    let cx = Cx::from_raw(cx);
    if !define_desc(cx, *obj.ptr, (*id.ptr).to_atom(), &*desc.ptr, &mut result) {
        return false;
    }
    if !result.ok() {
        q::JS_ThrowTypeError(cx.qctx(), c"cannot define property".as_ptr());
        return false;
    }
    true
}

pub(crate) unsafe fn JS_DefineUCProperty(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const u16,
    namelen: usize,
    desc: root::JS::Handle<root::JS::PropertyDescriptor>,
    result: *mut root::JS::ObjectOpResult,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_utf16(cx, name, namelen);
    define_desc(cx, *obj.ptr, atom.atom, &*desc.ptr, result)
}

/// Fills a SpiderMonkey descriptor from a QuickJS one (taking over its references via the pool).
unsafe fn to_sm_desc(
    cx: &Cx,
    qd: &q::JSPropertyDescriptor,
    out: &mut root::JS::PropertyDescriptor,
) {
    *out = std::mem::zeroed();
    let flags = qd.flags as u32;
    out.set_hasConfigurable_(true);
    out.set_configurable_(flags & q::JS_PROP_CONFIGURABLE != 0);
    out.set_hasEnumerable_(true);
    out.set_enumerable_(flags & q::JS_PROP_ENUMERABLE != 0);
    if flags & q::JS_PROP_GETSET != 0 {
        out.set_hasGetter_(true);
        out.set_hasSetter_(true);
        let g = cx.adopt(qd.getter);
        let s = cx.adopt(qd.setter);
        out.getter_ = if g.is_object() {
            g.to_object()
        } else {
            ptr::null_mut()
        };
        out.setter_ = if s.is_object() {
            s.to_object()
        } else {
            ptr::null_mut()
        };
        q::JS_FreeValueRT(cx.rt, qd.value);
    } else {
        out.set_hasWritable_(true);
        out.set_writable_(flags & q::JS_PROP_WRITABLE != 0);
        out.set_hasValue_(true);
        out.value_ = cx.adopt(qd.value);
        q::JS_FreeValueRT(cx.rt, qd.getter);
        q::JS_FreeValueRT(cx.rt, qd.setter);
    }
}

unsafe fn own_desc(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    isNone: *mut bool,
) -> bool {
    let mut qd: q::JSPropertyDescriptor = std::mem::zeroed();
    let r = q::JS_GetOwnProperty(cx.qctx(), &mut qd, obj_val(obj), atom);
    if r < 0 {
        return false;
    }
    *isNone = r == 0;
    if r > 0 {
        to_sm_desc(cx, &qd, &mut *desc.ptr);
    }
    true
}

unsafe fn desc_on_chain(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    holder: root::JS::MutableHandle<*mut root::JSObject>,
    isNone: *mut bool,
) -> bool {
    let mut current = obj;
    loop {
        if !own_desc(cx, current, atom, desc, isNone) {
            return false;
        }
        if !*isNone {
            *holder.ptr = current;
            return true;
        }
        let proto = q::JS_GetPrototype(cx.qctx(), obj_val(current));
        if proto.tag() == q::JS_TAG_EXCEPTION {
            return false;
        }
        let proto = cx.adopt(proto);
        if !proto.is_object() {
            *holder.ptr = ptr::null_mut();
            return true;
        }
        current = proto.to_object();
    }
}

pub(crate) unsafe fn JS_GetOwnPropertyDescriptorById(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    id: root::JS::HandleId,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    isNone: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    own_desc(cx, *obj.ptr, (*id.ptr).to_atom(), desc, isNone)
}

pub(crate) unsafe fn JS_GetOwnPropertyDescriptor(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    name: *const ::std::os::raw::c_char,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    isNone: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    own_desc(cx, *obj.ptr, atom.atom, desc, isNone)
}

pub(crate) unsafe fn JS_GetPropertyDescriptorById(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    id: root::JS::HandleId,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    holder: root::JS::MutableHandleObject,
    isNone: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    desc_on_chain(cx, *obj.ptr, (*id.ptr).to_atom(), desc, holder, isNone)
}

pub(crate) unsafe fn JS_GetPropertyDescriptor(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    desc: root::JS::MutableHandle<root::JS::PropertyDescriptor>,
    holder: root::JS::MutableHandle<*mut root::JSObject>,
    isNone: *mut bool,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    desc_on_chain(cx, *obj.ptr, atom.atom, desc, holder, isNone)
}

// Prototypes.

pub(crate) unsafe fn JS_GetPrototype(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    result: root::JS::MutableHandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let proto = q::JS_GetPrototype(cx.qctx(), obj_val(*obj.ptr));
    if proto.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let proto = cx.adopt(proto);
    *result.ptr = if proto.is_object() {
        proto.to_object()
    } else {
        ptr::null_mut()
    };
    true
}

pub(crate) unsafe fn JS_SetPrototype(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    proto: root::JS::HandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let p = if (*proto.ptr).is_null() {
        q::JS_NULL
    } else {
        obj_val(*proto.ptr)
    };
    ok(q::JS_SetPrototype(cx.qctx(), obj_val(*obj.ptr), p))
}

// Functions and specs.

pub(crate) unsafe fn JS_DefineFunction(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    call: root::JSNative,
    nargs: ::std::os::raw::c_uint,
    attrs: ::std::os::raw::c_uint,
) -> *mut root::JSFunction {
    let cx = Cx::from_raw(cx);
    let atom = Atom::from_cstr(cx, name);
    define_function(cx, *obj.ptr, atom.atom, call, ptr::null(), nargs, attrs)
}

pub(crate) unsafe fn JS_DefineFunctionById(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    id: root::JS::Handle<root::jsid>,
    call: root::JSNative,
    nargs: ::std::os::raw::c_uint,
    attrs: ::std::os::raw::c_uint,
) -> *mut root::JSFunction {
    let cx = Cx::from_raw(cx);
    define_function(
        cx,
        *obj.ptr,
        (*id.ptr).to_atom(),
        call,
        ptr::null(),
        nargs,
        attrs,
    )
}

unsafe fn define_function(
    cx: &Cx,
    obj: *mut root::JSObject,
    atom: q::JSAtom,
    call: root::JSNative,
    info: *const root::JSJitInfo,
    nargs: c_uint,
    attrs: c_uint,
) -> *mut root::JSFunction {
    let f = new_function(
        cx,
        call,
        info,
        nargs,
        attrs & root::JSFUN_CONSTRUCTOR != 0,
        atom,
    );
    if f.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    let f = cx.adopt(f);
    let v = q::JS_DupValueRT(cx.rt, f.to_qjs());
    if !define_value(cx, obj, atom, v, attrs & !root::JSFUN_CONSTRUCTOR) {
        return ptr::null_mut();
    }
    f.to_object() as *mut root::JSFunction
}

/// The function for a self-hosted name in a spec. Only the `Array.prototype` methods that
/// iterable DOM interfaces use are supported.
unsafe fn self_hosted(cx: &Cx, name: *const c_char) -> q::JSValue {
    let name = std::ffi::CStr::from_ptr(name);
    let method = match name.to_bytes() {
        b"$ArrayValues" => c"values",
        b"ArrayKeys" => c"keys",
        b"ArrayEntries" => c"entries",
        b"ArrayForEach" => c"forEach",
        _ => panic!("unsupported self-hosted function {name:?}"),
    };
    super::intrinsics::get(
        cx,
        super::intrinsics::Intrinsic::Method(super::intrinsics::CLASS_ARRAY, method),
    )
}

pub(crate) unsafe fn JS_DefineFunctions(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    fs: *const root::JSFunctionSpec,
) -> bool {
    let cx = Cx::from_raw(cx);
    let mut spec = fs;
    while !(*spec).name.string_.is_null() {
        let s = &*spec;
        let atom = Atom::from_spec_name(cx, s.name);
        let attrs = s.flags as c_uint;
        if !s.selfHostedName.is_null() {
            let f = self_hosted(cx, s.selfHostedName);
            if !define_value(cx, *obj.ptr, atom.atom, f, attrs) {
                return false;
            }
        } else if define_function(
            cx,
            *obj.ptr,
            atom.atom,
            s.call.op,
            s.call.info,
            s.nargs as c_uint,
            attrs,
        )
        .is_null()
        {
            return false;
        }
        spec = spec.add(1);
    }
    true
}

pub(crate) unsafe fn JS_DefineProperties(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    ps: *const root::JSPropertySpec,
) -> bool {
    let cx = Cx::from_raw(cx);
    let mut spec = ps;
    while !(*spec).name.string_.is_null() {
        let s = &*spec;
        let atom = Atom::from_spec_name(cx, s.name);
        let attrs = s.attributes_ as c_uint;
        let ok = match s.kind_ {
            root::JSPropertySpec_Kind::NativeAccessor => {
                let getter = s.u.accessors.getter.native;
                let setter = s.u.accessors.setter.native;
                let get_name = accessor_name(cx, "get", atom.atom);
                let set_name = accessor_name(cx, "set", atom.atom);
                let g = native_fn(cx, getter.op, getter.info, 0, get_name);
                let st = native_fn(cx, setter.op, setter.info, 1, set_name);
                q::JS_FreeAtomRT(cx.rt, get_name);
                q::JS_FreeAtomRT(cx.rt, set_name);
                define_accessor(cx, *obj.ptr, atom.atom, g, st, attrs)
            }
            root::JSPropertySpec_Kind::Value => {
                let w = &s.u.value;
                let v = match w.type_ {
                    root::JSPropertySpec_ValueWrapper_Type::String => {
                        q::JS_NewString(cx.qctx(), w.__bindgen_anon_1.string)
                    }
                    root::JSPropertySpec_ValueWrapper_Type::Int32 => {
                        q::JS_NewInt32(cx.qctx(), w.__bindgen_anon_1.int32)
                    }
                    root::JSPropertySpec_ValueWrapper_Type::Double => {
                        q::JS_NewFloat64(cx.qctx(), w.__bindgen_anon_1.double_)
                    }
                };
                define_value(cx, *obj.ptr, atom.atom, v, attrs)
            }
            root::JSPropertySpec_Kind::SelfHostedAccessor => {
                panic!("self-hosted accessors are not supported")
            }
        };
        if !ok {
            return false;
        }
        spec = spec.add(1);
    }
    true
}

// Calls.

unsafe fn args_vec(args: *const root::JS::HandleValueArray) -> Vec<q::JSValue> {
    if args.is_null() || (*args).length_ == 0 {
        return Vec::new();
    }
    let elems = std::slice::from_raw_parts((*args).elements_, (*args).length_);
    elems.iter().map(|v| v.to_qjs()).collect()
}

unsafe fn call(
    cx: &Cx,
    fun: q::JSValue,
    this: q::JSValue,
    args: *const root::JS::HandleValueArray,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let mut argv = args_vec(args);
    let r = q::JS_Call(cx.qctx(), fun, this, argv.len() as c_int, argv.as_mut_ptr());
    if r.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *rval.ptr = cx.adopt(r);
    true
}

pub(crate) unsafe fn Call(
    cx: *mut root::JSContext,
    thisv: root::JS::Handle<root::JS::Value>,
    fun: root::JS::Handle<root::JS::Value>,
    args: *const root::JS::HandleValueArray,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    call(cx, (*fun.ptr).to_qjs(), (*thisv.ptr).to_qjs(), args, rval)
}

pub(crate) unsafe fn JS_CallFunctionValue(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    fval: root::JS::Handle<root::JS::Value>,
    args: *const root::JS::HandleValueArray,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let this = if (*obj.ptr).is_null() {
        q::JS_UNDEFINED
    } else {
        obj_val(*obj.ptr)
    };
    call(cx, (*fval.ptr).to_qjs(), this, args, rval)
}

pub(crate) unsafe fn JS_CallFunctionName(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    name: *const ::std::os::raw::c_char,
    args: *const root::JS::HandleValueArray,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let this = obj_val(*obj.ptr);
    let f = q::JS_GetPropertyStr(cx.qctx(), this, name);
    if f.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let f = cx.adopt(f);
    call(cx, f.to_qjs(), this, args, rval)
}

pub(crate) unsafe fn Construct(
    cx: *mut root::JSContext,
    fun: root::JS::Handle<root::JS::Value>,
    newTarget: root::JS::Handle<*mut root::JSObject>,
    args: *const root::JS::HandleValueArray,
    objp: root::JS::MutableHandle<*mut root::JSObject>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let mut argv = args_vec(args);
    let target = if (*newTarget.ptr).is_null() {
        (*fun.ptr).to_qjs()
    } else {
        obj_val(*newTarget.ptr)
    };
    let r = q::JS_CallConstructor2(
        cx.qctx(),
        (*fun.ptr).to_qjs(),
        target,
        argv.len() as c_int,
        argv.as_mut_ptr(),
    );
    if r.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *objp.ptr = cx.adopt(r).to_object();
    true
}

pub(crate) unsafe fn JS_ValueToObject(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    objp: root::JS::MutableHandleObject,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let object_ctor = super::intrinsics::get(
        cx,
        super::intrinsics::Intrinsic::Method(super::intrinsics::CLASS_OBJECT, c"constructor"),
    );
    let v = (*v.ptr).to_qjs();
    if v.tag() == q::JS_TAG_NULL || v.tag() == q::JS_TAG_UNDEFINED {
        q::JS_FreeValue(qctx, object_ctor);
        q::JS_ThrowTypeError(qctx, c"can't convert null or undefined to object".as_ptr());
        return false;
    }
    let mut argv = [v];
    let o = q::JS_Call(qctx, object_ctor, q::JS_UNDEFINED, 1, argv.as_mut_ptr());
    q::JS_FreeValue(qctx, object_ctor);
    if o.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *objp.ptr = cx.adopt(o).to_object();
    true
}

pub(crate) unsafe fn GetArrayLength(
    cx: *mut root::JSContext,
    obj: root::JS::Handle<*mut root::JSObject>,
    lengthp: *mut u32,
) -> bool {
    let cx = Cx::from_raw(cx);
    let len = q::JS_GetPropertyStr(cx.qctx(), obj_val(*obj.ptr), c"length".as_ptr());
    if len.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let mut out = 0u32;
    let r = q::JS_ToUint32(cx.qctx(), &mut out, len);
    q::JS_FreeValueRT(cx.rt, len);
    *lengthp = out;
    ok(r)
}
