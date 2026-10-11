/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Strings, encodings, value conversions, property keys and JSON.
//!
//! A `*mut JSString` is a flat QuickJS string, Latin-1 or UTF-16, so character accessors return
//! pointers into the string itself that stay valid while it lives. Every string is linear, and
//! an atom's string is the atom itself. External strings are copied and their buffer is released
//! at once. Pinned atoms are kept alive by the pool, like every other value handed to Rust.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::{ptr, slice};

use servo_quickjs as q;

use super::{obj_val, Cx};
use crate::sys::glue;
use crate::sys::jsapi as root;
use crate::sys::jsid::{from_atom, IntId};
use crate::sys::jsval::{Int32Value, JSVal, StringValue, UndefinedValue};

extern "C" {
    // ID_Start / ID_Continue from QuickJS's libunicode.
    fn lre_is_id_start(c: u32) -> c_int;
    fn lre_is_id_continue(c: u32) -> c_int;
}

const ATOM_TAG_INT: q::JSAtom = 1 << 31;

fn str_val(s: *mut root::JSString) -> q::JSValue {
    q::JSValue::mkptr(q::JS_TAG_STRING, s as *mut c_void)
}

/// The characters of a string, borrowed from the string.
#[derive(Clone, Copy)]
pub(crate) enum Chars<'a> {
    Latin1(&'a [u8]),
    TwoByte(&'a [u16]),
}

impl<'a> Chars<'a> {
    /// The characters of the flat string `s`; valid while `s` is alive.
    pub unsafe fn of(s: *mut root::JSString) -> Chars<'a> {
        let mut data = ptr::null();
        let mut wide = 0;
        let len = q::JS_GetStringData(str_val(s), &mut data, &mut wide);
        if wide != 0 {
            Chars::TwoByte(slice::from_raw_parts(data as *const u16, len))
        } else {
            Chars::Latin1(slice::from_raw_parts(data as *const u8, len))
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Chars::Latin1(c) => c.len(),
            Chars::TwoByte(c) => c.len(),
        }
    }

    pub fn at(&self, i: usize) -> u16 {
        match self {
            Chars::Latin1(c) => c[i] as u16,
            Chars::TwoByte(c) => c[i],
        }
    }

    pub fn units(self) -> impl Iterator<Item = u16> + 'a {
        (0..self.len()).map(move |i| self.at(i))
    }

    pub fn to_utf16(self) -> Vec<u16> {
        match self {
            Chars::Latin1(c) => c.iter().map(|&b| b as u16).collect(),
            Chars::TwoByte(c) => c.to_vec(),
        }
    }

    /// UTF-8, with lone surrogates replaced by U+FFFD.
    pub fn to_utf8(self) -> String {
        match self {
            Chars::Latin1(c) => c.iter().map(|&b| b as char).collect(),
            Chars::TwoByte(c) => String::from_utf16_lossy(c),
        }
    }

    /// Each character truncated to 8 bits (SpiderMonkey's lossy Latin-1 conversion).
    pub fn to_latin1_lossy(self) -> Vec<u8> {
        match self {
            Chars::Latin1(c) => c.to_vec(),
            Chars::TwoByte(c) => c.iter().map(|&u| u as u8).collect(),
        }
    }
}

unsafe fn raw_slice<'a, T>(p: *const T, len: usize) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        slice::from_raw_parts(p, len)
    }
}

unsafe fn range_slice<'a, T>(r: &root::mozilla::Range<T>) -> &'a [T] {
    let start = r.mStart.mPtr;
    let len = (r.mEnd.mPtr as usize - start as usize) / std::mem::size_of::<T>();
    raw_slice(start, len)
}

unsafe fn u16_strlen(s: *const u16) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// Hands a new string reference (or `JS_EXCEPTION`) to Rust.
unsafe fn adopt_string(cx: &Cx, v: q::JSValue) -> *mut root::JSString {
    if v.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    cx.adopt(v).to_string()
}

pub(crate) unsafe fn new_latin1_string(cx: &Cx, chars: &[u8]) -> *mut root::JSString {
    adopt_string(
        cx,
        q::JS_NewStringLatin1(cx.qctx(), chars.as_ptr(), chars.len()),
    )
}

pub(crate) unsafe fn new_utf16_string(cx: &Cx, chars: &[u16]) -> *mut root::JSString {
    adopt_string(
        cx,
        q::JS_NewStringUTF16(cx.qctx(), chars.as_ptr(), chars.len()),
    )
}

/// A string from UTF-8; malformed input throws, like SpiderMonkey.
unsafe fn new_utf8_string(cx: &Cx, bytes: &[u8]) -> *mut root::JSString {
    if std::str::from_utf8(bytes).is_err() {
        q::JS_ThrowTypeError(cx.qctx(), c"malformed UTF-8 character sequence".as_ptr());
        return ptr::null_mut();
    }
    adopt_string(
        cx,
        q::JS_NewStringLen(cx.qctx(), bytes.as_ptr() as *const c_char, bytes.len()),
    )
}

/// A NUL-terminated copy in the runtime's heap (SpiderMonkey's `UniqueChars` and friends, freed
/// by `JS_free`).
unsafe fn malloc_z<T: Copy + Default>(cx: *mut root::JSContext, data: &[T]) -> *mut T {
    let p = q::js_malloc(
        Cx::from_raw(cx).qctx(),
        (data.len() + 1) * std::mem::size_of::<T>(),
    ) as *mut T;
    if !p.is_null() {
        ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        *p.add(data.len()) = T::default();
    }
    p
}

fn ranged_ptr<T>(p: *mut T) -> root::mozilla::RangedPtr<T> {
    root::mozilla::RangedPtr {
        _phantom_0: std::marker::PhantomData,
        mPtr: p,
    }
}

// Characters.

pub(crate) unsafe fn JS_GetStringLength(str_: *mut root::JSString) -> usize {
    Chars::of(str_).len()
}

pub(crate) unsafe fn JS_DeprecatedStringHasLatin1Chars(str_: *mut root::JSString) -> bool {
    matches!(Chars::of(str_), Chars::Latin1(_))
}

pub(crate) unsafe fn JS_GetLatin1StringCharsAndLength(
    cx: *mut root::JSContext,
    nogc: *const root::JS::AutoRequireNoGC,
    str_: *mut root::JSString,
    length: *mut usize,
) -> *const root::JS::Latin1Char {
    let _ = (cx, nogc);
    match Chars::of(str_) {
        Chars::Latin1(c) => {
            *length = c.len();
            c.as_ptr()
        }
        Chars::TwoByte(_) => panic!("JS_GetLatin1StringCharsAndLength on a two-byte string"),
    }
}

pub(crate) unsafe fn JS_GetTwoByteStringCharsAndLength(
    cx: *mut root::JSContext,
    nogc: *const root::JS::AutoRequireNoGC,
    str_: *mut root::JSString,
    length: *mut usize,
) -> *const u16 {
    let _ = (cx, nogc);
    match Chars::of(str_) {
        Chars::TwoByte(c) => {
            *length = c.len();
            c.as_ptr()
        }
        Chars::Latin1(_) => panic!("JS_GetTwoByteStringCharsAndLength on a Latin-1 string"),
    }
}

pub(crate) unsafe fn JS_GetStringCharAt(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
    index: usize,
    res: *mut u16,
) -> bool {
    let _ = cx;
    *res = Chars::of(str_).at(index);
    true
}

pub(crate) unsafe fn JS_StringIsLinear(str_: *mut root::JSString) -> bool {
    let _ = str_;
    true
}

pub(crate) unsafe fn JS_EnsureLinearString(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
) -> *mut root::JSLinearString {
    let _ = cx;
    str_ as *mut root::JSLinearString
}

pub(crate) unsafe fn StringToLinearStringSlow(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
) -> *mut root::JSLinearString {
    let _ = cx;
    str_ as *mut root::JSLinearString
}

pub(crate) unsafe fn JS_ForgetStringLinearness(
    str_: *mut root::JSLinearString,
) -> *mut root::JSString {
    str_ as *mut root::JSString
}

pub(crate) unsafe fn AtomToLinearString(atom: *mut root::JSAtom) -> *mut root::JSLinearString {
    atom as *mut root::JSLinearString
}

pub(crate) unsafe fn GetLinearStringLength(s: *mut root::JSLinearString) -> usize {
    Chars::of(s as *mut root::JSString).len()
}

pub(crate) unsafe fn GetLinearStringCharAt(s: *mut root::JSLinearString, idx: usize) -> u16 {
    Chars::of(s as *mut root::JSString).at(idx)
}

pub(crate) unsafe fn JS_CopyStringChars(
    cx: *mut root::JSContext,
    dest: *const root::mozilla::Range<u16>,
    str_: *mut root::JSString,
) -> bool {
    let _ = cx;
    let chars = Chars::of(str_);
    let dest = (*dest).mStart.mPtr;
    for (i, c) in chars.units().enumerate() {
        *dest.add(i) = c;
    }
    true
}

// Comparisons.

pub(crate) unsafe fn JS_CompareStrings(
    cx: *mut root::JSContext,
    str1: *mut root::JSString,
    str2: *mut root::JSString,
    result: *mut i32,
) -> bool {
    let _ = cx;
    *result = Chars::of(str1).units().cmp(Chars::of(str2).units()) as i32;
    true
}

fn equals_ascii(chars: Chars, ascii: &[u8]) -> bool {
    chars.len() == ascii.len() && chars.units().zip(ascii).all(|(c, &b)| c == b as u16)
}

pub(crate) unsafe fn JS_StringEqualsAscii(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
    asciiBytes: *const ::std::os::raw::c_char,
    match_: *mut bool,
) -> bool {
    let _ = cx;
    *match_ = equals_ascii(Chars::of(str_), CStr::from_ptr(asciiBytes).to_bytes());
    true
}

pub(crate) unsafe fn JS_StringEqualsAscii1(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
    asciiBytes: *const ::std::os::raw::c_char,
    length: usize,
    match_: *mut bool,
) -> bool {
    let _ = cx;
    *match_ = equals_ascii(Chars::of(str_), raw_slice(asciiBytes as *const u8, length));
    true
}

pub(crate) unsafe fn JS_LinearStringEqualsAscii(
    str_: *mut root::JSLinearString,
    asciiBytes: *const ::std::os::raw::c_char,
) -> bool {
    equals_ascii(
        Chars::of(str_ as *mut root::JSString),
        CStr::from_ptr(asciiBytes).to_bytes(),
    )
}

pub(crate) unsafe fn JS_LinearStringEqualsAscii1(
    str_: *mut root::JSLinearString,
    asciiBytes: *const ::std::os::raw::c_char,
    length: usize,
) -> bool {
    equals_ascii(
        Chars::of(str_ as *mut root::JSString),
        raw_slice(asciiBytes as *const u8, length),
    )
}

// Creation.

pub(crate) unsafe fn JS_NewStringCopyN(
    cx: *mut root::JSContext,
    s: *const ::std::os::raw::c_char,
    n: usize,
) -> *mut root::JSString {
    new_latin1_string(Cx::from_raw(cx), raw_slice(s as *const u8, n))
}

pub(crate) unsafe fn JS_NewStringCopyZ(
    cx: *mut root::JSContext,
    s: *const ::std::os::raw::c_char,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    if s.is_null() {
        return new_latin1_string(cx, &[]);
    }
    new_latin1_string(cx, CStr::from_ptr(s).to_bytes())
}

pub(crate) unsafe fn JS_NewUCStringCopyN(
    cx: *mut root::JSContext,
    s: *const u16,
    n: usize,
) -> *mut root::JSString {
    new_utf16_string(Cx::from_raw(cx), raw_slice(s, n))
}

pub(crate) unsafe fn JS_NewUCStringCopyZ(
    cx: *mut root::JSContext,
    s: *const u16,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    if s.is_null() {
        return new_utf16_string(cx, &[]);
    }
    new_utf16_string(cx, raw_slice(s, u16_strlen(s)))
}

pub(crate) unsafe fn JS_NewStringCopyUTF8N(
    cx: *mut root::JSContext,
    s: *const root::JS::UTF8Chars,
) -> *mut root::JSString {
    new_utf8_string(Cx::from_raw(cx), range_slice(&(*s)._base))
}

pub(crate) unsafe fn JS_NewStringCopyUTF8Z(
    cx: *mut root::JSContext,
    s: root::JS::ConstUTF8CharsZ,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    if s.data_.is_null() {
        return new_latin1_string(cx, &[]);
    }
    new_utf8_string(cx, CStr::from_ptr(s.data_).to_bytes())
}

pub(crate) unsafe fn JS_NewDependentString(
    cx: *mut root::JSContext,
    str_: root::JS::Handle<*mut root::JSString>,
    start: usize,
    length: usize,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    let range = start..start + length;
    match Chars::of(*str_.ptr) {
        Chars::Latin1(c) => new_latin1_string(cx, &c[range]),
        Chars::TwoByte(c) => new_utf16_string(cx, &c[range]),
    }
}

pub(crate) unsafe fn JS_ConcatStrings(
    cx: *mut root::JSContext,
    left: root::JS::Handle<*mut root::JSString>,
    right: root::JS::Handle<*mut root::JSString>,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    match (Chars::of(*left.ptr), Chars::of(*right.ptr)) {
        (Chars::Latin1(l), Chars::Latin1(r)) => new_latin1_string(cx, &[l, r].concat()),
        (l, r) => {
            let mut chars = l.to_utf16();
            chars.extend(r.units());
            new_utf16_string(cx, &chars)
        }
    }
}

pub(crate) unsafe fn JS_GetEmptyString(cx: *mut root::JSContext) -> *mut root::JSString {
    new_latin1_string(Cx::from_raw(cx), &[])
}

pub(crate) unsafe fn JS_GetEmptyStringValue(cx: *mut root::JSContext, dest: *mut root::JS::Value) {
    *dest = StringValue(&*JS_GetEmptyString(cx));
}

// External strings: QuickJS has none, so the characters are copied and released at once.

#[repr(C)]
struct ExternalStringCallbacks {
    /// Where SpiderMonkey's `JSExternalStringCallbacks` has its vtable; unused.
    vtable: *const c_void,
    traps: glue::JSExternalStringCallbacksTraps,
    private: *mut c_void,
}

pub(crate) unsafe fn CreateJSExternalStringCallbacks(
    aTraps: *const glue::JSExternalStringCallbacksTraps,
    privateData: *mut ::std::os::raw::c_void,
) -> *mut root::JSExternalStringCallbacks {
    let callbacks = Box::new(ExternalStringCallbacks {
        vtable: ptr::null(),
        traps: *aTraps,
        private: privateData,
    });
    Box::into_raw(callbacks) as *mut root::JSExternalStringCallbacks
}

pub(crate) unsafe fn DeleteJSExternalStringCallbacks(
    callbacks: *mut root::JSExternalStringCallbacks,
) {
    drop(Box::from_raw(callbacks as *mut ExternalStringCallbacks));
}

pub(crate) unsafe fn JS_NewExternalStringLatin1(
    cx: *mut root::JSContext,
    chars: *const root::JS::Latin1Char,
    length: usize,
    callbacks: *const root::JSExternalStringCallbacks,
) -> *mut root::JSString {
    let s = new_latin1_string(Cx::from_raw(cx), raw_slice(chars, length));
    if !s.is_null() {
        let callbacks = &*(callbacks as *const ExternalStringCallbacks);
        if let Some(finalize) = callbacks.traps.latin1Finalize {
            finalize(callbacks.private, chars as *mut _);
        }
    }
    s
}

pub(crate) unsafe fn JS_NewExternalUCString(
    cx: *mut root::JSContext,
    chars: *const u16,
    length: usize,
    callbacks: *const root::JSExternalStringCallbacks,
) -> *mut root::JSString {
    let s = new_utf16_string(Cx::from_raw(cx), raw_slice(chars, length));
    if !s.is_null() {
        let callbacks = &*(callbacks as *const ExternalStringCallbacks);
        if let Some(finalize) = callbacks.traps.utf16Finalize {
            finalize(callbacks.private, chars as *mut _);
        }
    }
    s
}

pub(crate) unsafe fn JS_NewMaybeExternalStringLatin1(
    cx: *mut root::JSContext,
    chars: *const root::JS::Latin1Char,
    length: usize,
    callbacks: *const root::JSExternalStringCallbacks,
    allocatedExternal: *mut bool,
) -> *mut root::JSString {
    let _ = callbacks;
    *allocatedExternal = false;
    new_latin1_string(Cx::from_raw(cx), raw_slice(chars, length))
}

pub(crate) unsafe fn JS_NewMaybeExternalUCString(
    cx: *mut root::JSContext,
    chars: *const u16,
    length: usize,
    callbacks: *const root::JSExternalStringCallbacks,
    allocatedExternal: *mut bool,
) -> *mut root::JSString {
    let _ = callbacks;
    *allocatedExternal = false;
    new_utf16_string(Cx::from_raw(cx), raw_slice(chars, length))
}

pub(crate) unsafe fn JS_NewMaybeExternalStringUTF8(
    cx: *mut root::JSContext,
    utf8: *const root::JS::UTF8Chars,
    callbacks: *const root::JSExternalStringCallbacks,
    allocatedExternal: *mut bool,
) -> *mut root::JSString {
    let _ = callbacks;
    *allocatedExternal = false;
    JS_NewStringCopyUTF8N(cx, utf8)
}

// Atoms.

/// The atom of the new string reference `s` (consumed). Integer-like strings have no atom string
/// (QuickJS makes them integer atoms) and are returned themselves.
unsafe fn atomize(cx: &Cx, s: q::JSValue) -> *mut root::JSString {
    if s.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    let atom = q::JS_ValueToAtom(cx.qctx(), s);
    if atom & ATOM_TAG_INT != 0 {
        return adopt_string(cx, s);
    }
    q::JS_FreeValueRT(cx.rt, s);
    if atom == q::JS_ATOM_NULL {
        return ptr::null_mut();
    }
    super::atom_to_string_ptr(cx.adopt_atom(atom))
}

unsafe fn atomize_latin1(cx: *mut root::JSContext, chars: &[u8]) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    let s = q::JS_NewStringLatin1(cx.qctx(), chars.as_ptr(), chars.len());
    atomize(cx, s)
}

unsafe fn atomize_utf16(cx: *mut root::JSContext, chars: &[u16]) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    let s = q::JS_NewStringUTF16(cx.qctx(), chars.as_ptr(), chars.len());
    atomize(cx, s)
}

pub(crate) unsafe fn JS_AtomizeString(
    cx: *mut root::JSContext,
    s: *const ::std::os::raw::c_char,
) -> *mut root::JSString {
    atomize_latin1(cx, CStr::from_ptr(s).to_bytes())
}

pub(crate) unsafe fn JS_AtomizeStringN(
    cx: *mut root::JSContext,
    s: *const ::std::os::raw::c_char,
    length: usize,
) -> *mut root::JSString {
    atomize_latin1(cx, raw_slice(s as *const u8, length))
}

pub(crate) unsafe fn JS_AtomizeAndPinString(
    cx: *mut root::JSContext,
    s: *const ::std::os::raw::c_char,
) -> *mut root::JSString {
    atomize_latin1(cx, CStr::from_ptr(s).to_bytes())
}

pub(crate) unsafe fn JS_AtomizeAndPinStringN(
    cx: *mut root::JSContext,
    s: *const ::std::os::raw::c_char,
    length: usize,
) -> *mut root::JSString {
    atomize_latin1(cx, raw_slice(s as *const u8, length))
}

pub(crate) unsafe fn JS_AtomizeUCString(
    cx: *mut root::JSContext,
    s: *const u16,
) -> *mut root::JSString {
    atomize_utf16(cx, raw_slice(s, u16_strlen(s)))
}

pub(crate) unsafe fn JS_AtomizeUCStringN(
    cx: *mut root::JSContext,
    s: *const u16,
    length: usize,
) -> *mut root::JSString {
    atomize_utf16(cx, raw_slice(s, length))
}

// Encodings.

pub(crate) unsafe fn EncodeStringToUTF8(
    cx: *mut root::JSContext,
    str_: root::JS::HandleString,
    cb: glue::EncodedStringCallback,
) {
    let _ = cx;
    let mut bytes = Chars::of(*str_.ptr).to_utf8().into_bytes();
    bytes.push(0);
    cb(bytes.as_ptr() as *const c_char);
}

pub(crate) unsafe fn JS_EncodeStringToUTF8(
    cx: *mut root::JSContext,
    str_: root::JS::Handle<*mut root::JSString>,
) -> root::JS::UniqueChars {
    malloc_z(cx, Chars::of(*str_.ptr).to_utf8().as_bytes()) as root::JS::UniqueChars
}

pub(crate) unsafe fn JS_EncodeStringToLatin1(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
) -> root::JS::UniqueChars {
    malloc_z(cx, &Chars::of(str_).to_latin1_lossy()) as root::JS::UniqueChars
}

pub(crate) unsafe fn JS_EncodeStringToASCII(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
) -> root::JS::UniqueChars {
    JS_EncodeStringToLatin1(cx, str_)
}

pub(crate) unsafe fn JS_GetStringEncodingLength(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
) -> usize {
    let _ = cx;
    Chars::of(str_).len()
}

pub(crate) unsafe fn JS_EncodeStringToBuffer(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
    buffer: *mut ::std::os::raw::c_char,
    length: usize,
) -> bool {
    let _ = cx;
    let bytes = Chars::of(str_).to_latin1_lossy();
    ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, bytes.len().min(length));
    true
}

pub(crate) unsafe fn LossyTwoByteCharsToNewLatin1CharsZ(
    cx: *mut root::JSContext,
    tbchars: *const root::mozilla::Range<u16>,
) -> root::JS::Latin1CharsZ {
    let bytes: Vec<u8> = range_slice(&*tbchars).iter().map(|&c| c as u8).collect();
    root::JS::Latin1CharsZ {
        _base: ranged_ptr(malloc_z(cx, &bytes)),
    }
}

unsafe fn utf8_to_two_byte_z(
    cx: *mut root::JSContext,
    utf8: *const root::JS::UTF8Chars,
    outlen: *mut usize,
    lossy: bool,
) -> root::JS::TwoByteCharsZ {
    let bytes = range_slice(&(*utf8)._base);
    let units: Vec<u16> = match std::str::from_utf8(bytes) {
        Ok(s) => s.encode_utf16().collect(),
        Err(_) if lossy => String::from_utf8_lossy(bytes).encode_utf16().collect(),
        Err(_) => {
            q::JS_ThrowTypeError(
                Cx::from_raw(cx).qctx(),
                c"malformed UTF-8 character sequence".as_ptr(),
            );
            *outlen = 0;
            return root::JS::TwoByteCharsZ {
                _base: ranged_ptr(ptr::null_mut()),
            };
        }
    };
    *outlen = units.len();
    root::JS::TwoByteCharsZ {
        _base: ranged_ptr(malloc_z(cx, &units)),
    }
}

pub(crate) unsafe fn UTF8CharsToNewTwoByteCharsZ(
    cx: *mut root::JSContext,
    utf8: *const root::JS::UTF8Chars,
    outlen: *mut usize,
    destArenaId: root::arena_id_t,
) -> root::JS::TwoByteCharsZ {
    let _ = destArenaId;
    utf8_to_two_byte_z(cx, utf8, outlen, false)
}

pub(crate) unsafe fn LossyUTF8CharsToNewTwoByteCharsZ(
    cx: *mut root::JSContext,
    utf8: *const root::JS::UTF8Chars,
    outlen: *mut usize,
    destArenaId: root::arena_id_t,
) -> root::JS::TwoByteCharsZ {
    let _ = destArenaId;
    utf8_to_two_byte_z(cx, utf8, outlen, true)
}

// The runtime's allocator, so memory can move between these and ArrayBuffer contents.

pub(crate) unsafe fn JS_malloc(
    cx: *mut root::JSContext,
    nbytes: usize,
) -> *mut ::std::os::raw::c_void {
    q::js_malloc(Cx::from_raw(cx).qctx(), nbytes)
}

pub(crate) unsafe fn JS_realloc(
    cx: *mut root::JSContext,
    p: *mut ::std::os::raw::c_void,
    oldBytes: usize,
    newBytes: usize,
) -> *mut ::std::os::raw::c_void {
    let _ = oldBytes;
    q::js_realloc(Cx::from_raw(cx).qctx(), p, newBytes)
}

pub(crate) unsafe fn JS_free(cx: *mut root::JSContext, p: *mut ::std::os::raw::c_void) {
    q::js_free_rt(Cx::from_raw(cx).rt, p)
}

/// Appends `chars` escaped like SpiderMonkey's `QuoteString`: printable ASCII as is, the rest as
/// `\n`-style, `\xXX` or `\uXXXX` escapes. `quote` (if not 0) surrounds the result and is escaped.
fn escape_into(out: &mut String, chars: Chars, quote: u8) {
    use std::fmt::Write;
    if quote != 0 {
        out.push(quote as char);
    }
    for c in chars.units() {
        if (0x20..0x7f).contains(&c) && c != quote as u16 && c != b'\\' as u16 {
            out.push(c as u8 as char);
            continue;
        }
        let escape = match c {
            0x08 => Some('b'),
            0x0c => Some('f'),
            0x0a => Some('n'),
            0x0d => Some('r'),
            0x09 => Some('t'),
            0x0b => Some('v'),
            0x22 => Some('"'),
            0x27 => Some('\''),
            0x5c => Some('\\'),
            _ => None,
        };
        let _ = match escape {
            Some(e) => write!(out, "\\{e}"),
            None if c < 0x100 => write!(out, "\\x{c:02X}"),
            None => write!(out, "\\u{c:04X}"),
        };
    }
    if quote != 0 {
        out.push(quote as char);
    }
}

unsafe fn put_escaped(buffer: *mut c_char, size: usize, chars: Chars, quote: c_char) -> usize {
    let mut escaped = String::new();
    escape_into(&mut escaped, chars, quote as u8);
    if !buffer.is_null() && size > 0 {
        let n = escaped.len().min(size - 1);
        ptr::copy_nonoverlapping(escaped.as_ptr(), buffer as *mut u8, n);
        *buffer.add(n) = 0;
    }
    escaped.len()
}

pub(crate) unsafe fn JS_PutEscapedString(
    cx: *mut root::JSContext,
    buffer: *mut ::std::os::raw::c_char,
    size: usize,
    str_: *mut root::JSString,
    quote: ::std::os::raw::c_char,
) -> usize {
    let _ = cx;
    put_escaped(buffer, size, Chars::of(str_), quote)
}

pub(crate) unsafe fn JS_PutEscapedLinearString(
    buffer: *mut ::std::os::raw::c_char,
    size: usize,
    str_: *mut root::JSLinearString,
    quote: ::std::os::raw::c_char,
) -> usize {
    put_escaped(buffer, size, Chars::of(str_ as *mut root::JSString), quote)
}

// Value conversions.

unsafe fn to_number(cx: &Cx, v: q::JSValue) -> Option<f64> {
    let mut d = 0.0;
    (q::JS_ToFloat64(cx.qctx(), &mut d, v) >= 0).then_some(d)
}

/// `d` modulo 2^bits, the integer conversion of ECMAScript's ToInt32/ToUint8/... and of
/// SpiderMonkey's ToInt64/ToUint64.
fn modular(d: f64, bits: u32) -> u64 {
    if !d.is_finite() {
        return 0;
    }
    let r = d.trunc() % 2f64.powi(bits as i32);
    let n = if r < 0.0 {
        ((-r) as u64).wrapping_neg()
    } else {
        r as u64
    };
    if bits == 64 {
        n
    } else {
        n & ((1 << bits) - 1)
    }
}

unsafe fn to_modular(cx: *mut root::JSContext, v: root::JS::HandleValue, bits: u32) -> Option<u64> {
    to_number(Cx::from_raw(cx), (*v.ptr).to_qjs()).map(|d| modular(d, bits))
}

pub(crate) unsafe fn ToBooleanSlow(v: root::JS::HandleValue) -> bool {
    q::JS_ToBool(super::current().qctx(), (*v.ptr).to_qjs()) > 0
}

pub(crate) unsafe fn ToNumberSlow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    dp: *mut f64,
) -> bool {
    match to_number(Cx::from_raw(cx), (*v.ptr).to_qjs()) {
        Some(d) => {
            *dp = d;
            true
        }
        None => false,
    }
}

pub(crate) unsafe fn ToInt8Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut i8,
) -> bool {
    to_modular(cx, v, 8).map(|n| *out = n as u8 as i8).is_some()
}

pub(crate) unsafe fn ToUint8Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut u8,
) -> bool {
    to_modular(cx, v, 8).map(|n| *out = n as u8).is_some()
}

pub(crate) unsafe fn ToInt16Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut i16,
) -> bool {
    to_modular(cx, v, 16)
        .map(|n| *out = n as u16 as i16)
        .is_some()
}

pub(crate) unsafe fn ToUint16Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut u16,
) -> bool {
    to_modular(cx, v, 16).map(|n| *out = n as u16).is_some()
}

pub(crate) unsafe fn ToInt32Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut i32,
) -> bool {
    to_modular(cx, v, 32)
        .map(|n| *out = n as u32 as i32)
        .is_some()
}

pub(crate) unsafe fn ToUint32Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut u32,
) -> bool {
    to_modular(cx, v, 32).map(|n| *out = n as u32).is_some()
}

pub(crate) unsafe fn ToInt64Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut i64,
) -> bool {
    to_modular(cx, v, 64).map(|n| *out = n as i64).is_some()
}

pub(crate) unsafe fn ToUint64Slow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    out: *mut u64,
) -> bool {
    to_modular(cx, v, 64).map(|n| *out = n).is_some()
}

pub(crate) unsafe fn ToStringSlow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    let s = q::JS_ToString(cx.qctx(), (*v.ptr).to_qjs());
    adopt_string(cx, s)
}

pub(crate) unsafe fn ToObjectSlow(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    reportScanStack: bool,
) -> *mut root::JSObject {
    let _ = reportScanStack;
    let mut obj = ptr::null_mut();
    let objp = root::JS::MutableHandle {
        _phantom_0: std::marker::PhantomData,
        ptr: &mut obj,
    };
    if super::JS_ValueToObject(cx, v, objp) {
        obj
    } else {
        ptr::null_mut()
    }
}

/// OrdinaryToPrimitive; a new reference or `JS_EXCEPTION`.
unsafe fn ordinary_to_primitive(cx: &Cx, obj: q::JSValue, hint: root::JSType) -> q::JSValue {
    let qctx = cx.qctx();
    let methods = if hint == root::JSType::JSTYPE_STRING {
        [c"toString", c"valueOf"]
    } else {
        [c"valueOf", c"toString"]
    };
    for name in methods {
        let method = q::JS_GetPropertyStr(qctx, obj, name.as_ptr());
        if method.tag() == q::JS_TAG_EXCEPTION {
            return method;
        }
        if q::JS_IsFunction(qctx, method) != 0 {
            let r = q::JS_Call(qctx, method, obj, 0, ptr::null_mut());
            q::JS_FreeValueRT(cx.rt, method);
            if r.tag() != q::JS_TAG_OBJECT {
                return r;
            }
            q::JS_FreeValueRT(cx.rt, r);
        } else {
            q::JS_FreeValueRT(cx.rt, method);
        }
    }
    q::JS_ThrowTypeError(qctx, c"can't convert object to primitive type".as_ptr())
}

/// ToPrimitive of an object (`hint` `JSTYPE_UNDEFINED` is "default"); a new reference or
/// `JS_EXCEPTION`.
unsafe fn to_primitive(cx: &Cx, obj: q::JSValue, hint: root::JSType) -> q::JSValue {
    let qctx = cx.qctx();
    let atom = super::well_known_symbol_atom(cx, root::JS::SymbolCode::toPrimitive as usize);
    let exotic = q::JS_GetProperty(qctx, obj, atom);
    match exotic.tag() {
        q::JS_TAG_EXCEPTION => return exotic,
        q::JS_TAG_UNDEFINED | q::JS_TAG_NULL => {
            let hint = if hint == root::JSType::JSTYPE_STRING {
                hint
            } else {
                root::JSType::JSTYPE_NUMBER
            };
            return ordinary_to_primitive(cx, obj, hint);
        }
        _ => {}
    }
    if q::JS_IsFunction(qctx, exotic) == 0 {
        q::JS_FreeValueRT(cx.rt, exotic);
        return q::JS_ThrowTypeError(qctx, c"Symbol.toPrimitive is not a function".as_ptr());
    }
    let hint_name = match hint {
        root::JSType::JSTYPE_STRING => c"string",
        root::JSType::JSTYPE_NUMBER => c"number",
        _ => c"default",
    };
    let mut arg = [q::JS_NewString(qctx, hint_name.as_ptr())];
    let r = q::JS_Call(qctx, exotic, obj, 1, arg.as_mut_ptr());
    q::JS_FreeValueRT(cx.rt, arg[0]);
    q::JS_FreeValueRT(cx.rt, exotic);
    if r.tag() == q::JS_TAG_OBJECT {
        q::JS_FreeValueRT(cx.rt, r);
        return q::JS_ThrowTypeError(qctx, c"can't convert object to primitive type".as_ptr());
    }
    r
}

pub(crate) unsafe fn ToPrimitive(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    hint: root::JSType,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = to_primitive(cx, obj_val(*obj.ptr), hint);
    if r.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *vp.ptr = cx.adopt(r);
    true
}

pub(crate) unsafe fn OrdinaryToPrimitive(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    type_: root::JSType,
    vp: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let r = ordinary_to_primitive(cx, obj_val(*obj.ptr), type_);
    if r.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *vp.ptr = cx.adopt(r);
    true
}

pub(crate) unsafe fn JS_TypeOfValue(
    cx: *mut root::JSContext,
    v: root::JS::Handle<root::JS::Value>,
) -> root::JSType {
    use root::JSType::*;
    let v = (*v.ptr).to_qjs();
    match v.tag() {
        q::JS_TAG_UNDEFINED => JSTYPE_UNDEFINED,
        q::JS_TAG_BOOL => JSTYPE_BOOLEAN,
        q::JS_TAG_INT | q::JS_TAG_FLOAT64 => JSTYPE_NUMBER,
        q::JS_TAG_STRING | q::JS_TAG_STRING_ROPE => JSTYPE_STRING,
        q::JS_TAG_SYMBOL => JSTYPE_SYMBOL,
        q::JS_TAG_BIG_INT | q::JS_TAG_SHORT_BIG_INT => JSTYPE_BIGINT,
        q::JS_TAG_OBJECT if q::JS_IsFunction(Cx::from_raw(cx).qctx(), v) != 0 => JSTYPE_FUNCTION,
        _ => JSTYPE_OBJECT,
    }
}

pub(crate) unsafe fn SameValue(
    cx: *mut root::JSContext,
    v1: root::JS::Handle<root::JS::Value>,
    v2: root::JS::Handle<root::JS::Value>,
    same: *mut bool,
) -> bool {
    *same = q::JS_SameValue(
        Cx::from_raw(cx).qctx(),
        (*v1.ptr).to_qjs(),
        (*v2.ptr).to_qjs(),
    ) != 0;
    true
}

pub(crate) unsafe fn StrictlyEqual(
    cx: *mut root::JSContext,
    v1: root::JS::Handle<root::JS::Value>,
    v2: root::JS::Handle<root::JS::Value>,
    equal: *mut bool,
) -> bool {
    *equal = q::JS_StrictEq(
        Cx::from_raw(cx).qctx(),
        (*v1.ptr).to_qjs(),
        (*v2.ptr).to_qjs(),
    ) != 0;
    true
}

// Property keys.

/// The id of a new atom reference, which the pool takes over.
unsafe fn atom_id(cx: &Cx, atom: q::JSAtom) -> root::jsid {
    from_atom(cx.adopt_atom(atom))
}

unsafe fn value_to_id(cx: &Cx, v: q::JSValue, idp: *mut root::jsid) -> bool {
    let atom = q::JS_ValueToAtom(cx.qctx(), v);
    if atom == q::JS_ATOM_NULL {
        return false;
    }
    *idp = atom_id(cx, atom);
    true
}

pub(crate) unsafe fn JS_StringToId(
    cx: *mut root::JSContext,
    s: root::JS::HandleString,
    idp: root::JS::MutableHandleId,
) -> bool {
    value_to_id(Cx::from_raw(cx), str_val(*s.ptr), idp.ptr)
}

pub(crate) unsafe fn JS_ValueToId(
    cx: *mut root::JSContext,
    v: root::JS::HandleValue,
    idp: root::JS::MutableHandleId,
) -> bool {
    value_to_id(Cx::from_raw(cx), (*v.ptr).to_qjs(), idp.ptr)
}

pub(crate) unsafe fn JS_CharsToId(
    cx: *mut root::JSContext,
    chars: root::JS::TwoByteChars,
    arg1: root::JS::MutableHandleId,
) -> bool {
    let cx = Cx::from_raw(cx);
    let chars = range_slice(&chars._base);
    let s = q::JS_NewStringUTF16(cx.qctx(), chars.as_ptr(), chars.len());
    if s.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let ok = value_to_id(cx, s, arg1.ptr);
    q::JS_FreeValueRT(cx.rt, s);
    ok
}

pub(crate) unsafe fn JS_IndexToId(
    cx: *mut root::JSContext,
    index: u32,
    arg1: root::JS::MutableHandleId,
) -> bool {
    let cx = Cx::from_raw(cx);
    if index <= i32::MAX as u32 {
        *arg1.ptr = IntId(index as i32);
        return true;
    }
    let atom = q::JS_NewAtomUInt32(cx.qctx(), index);
    if atom == q::JS_ATOM_NULL {
        return false;
    }
    *arg1.ptr = atom_id(cx, atom);
    true
}

pub(crate) unsafe fn JS_IdToValue(
    cx: *mut root::JSContext,
    id: root::jsid,
    vp: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    *vp.ptr = if id.is_int() {
        Int32Value(id.to_int())
    } else if id.is_void() {
        UndefinedValue()
    } else {
        cx.adopt(q::JS_AtomToValue(cx.qctx(), id.to_atom()))
    };
    true
}

/// The well-known symbol code of a `JSPropertySpec` name, if it names a symbol.
fn spec_symbol_code(name: root::JSPropertySpec_Name) -> Option<usize> {
    let raw = unsafe { name.symbol_ };
    (raw != 0 && raw <= root::JS::SymbolCode::Limit as usize).then(|| raw - 1)
}

pub(crate) unsafe fn PropertySpecNameEqualsId(
    name: root::JSPropertySpec_Name,
    id: root::JS::HandleId,
) -> bool {
    let id = *id.ptr;
    match spec_symbol_code(name) {
        Some(code) => {
            id.is_symbol() && id.to_atom() == super::well_known_symbol_atom(super::current(), code)
        }
        None => {
            id.is_string()
                && equals_ascii(
                    Chars::of(id.to_string()),
                    CStr::from_ptr(name.string_).to_bytes(),
                )
        }
    }
}

pub(crate) unsafe fn PropertySpecNameToPermanentId(
    cx: *mut root::JSContext,
    name: root::JSPropertySpec_Name,
    idp: *mut root::jsid,
) -> bool {
    let cx = Cx::from_raw(cx);
    let atom = match spec_symbol_code(name) {
        Some(code) => super::well_known_symbol_atom(cx, code),
        // Permanent: this reference is never released.
        None => q::JS_NewAtom(cx.qctx(), name.string_),
    };
    if atom == q::JS_ATOM_NULL {
        return false;
    }
    *idp = from_atom(atom);
    true
}

pub(crate) unsafe fn RUST_JSID_IS_INT(id: root::JS::HandleId) -> bool {
    (*id.ptr).is_int()
}

pub(crate) unsafe fn RUST_JSID_TO_INT(id: root::JS::HandleId) -> i32 {
    (*id.ptr).to_int()
}

pub(crate) unsafe fn RUST_JSID_IS_STRING(id: root::JS::HandleId) -> bool {
    (*id.ptr).is_string()
}

pub(crate) unsafe fn RUST_JSID_TO_STRING(id: root::JS::HandleId) -> *mut root::JSString {
    (*id.ptr).to_string()
}

pub(crate) unsafe fn RUST_JSID_IS_VOID(id: root::JS::HandleId) -> bool {
    (*id.ptr).is_void()
}

pub(crate) unsafe fn RUST_INTERNED_STRING_TO_JSID(
    cx: *mut root::JSContext,
    str_: *mut root::JSString,
    id: root::JS::MutableHandleId,
) {
    let _ = cx;
    *id.ptr = crate::sys::jsid::StringId(str_);
}

pub(crate) unsafe fn RUST_SYMBOL_TO_JSID(
    sym: *mut root::JS::Symbol,
    id: root::JS::MutableHandleId,
) {
    *id.ptr = crate::sys::jsid::SymbolId(sym);
}

pub(crate) unsafe fn int_to_jsid(i: i32, id: root::JS::MutableHandleId) {
    *id.ptr = IntId(i);
}

/// The array index (a canonical decimal below 2^32 - 1) spelled by `units`, if any.
fn array_index(len: usize, units: impl Iterator<Item = u16>) -> Option<u32> {
    if len == 0 || len > 10 {
        return None;
    }
    let mut n: u64 = 0;
    for (i, c) in units.enumerate() {
        let d = c.wrapping_sub(b'0' as u16);
        if d > 9 || (i == 0 && d == 0 && len > 1) {
            return None;
        }
        n = n * 10 + d as u64;
    }
    (n < u32::MAX as u64).then_some(n as u32)
}

pub(crate) unsafe fn StringIsArrayIndex(
    str_: *const root::JSLinearString,
    indexp: *mut u32,
) -> bool {
    let chars = Chars::of(str_ as *mut root::JSString);
    array_index(chars.len(), chars.units())
        .map(|i| *indexp = i)
        .is_some()
}

pub(crate) unsafe fn StringIsArrayIndex1(str_: *const u16, length: u32, indexp: *mut u32) -> bool {
    let chars = raw_slice(str_, length as usize);
    array_index(chars.len(), chars.iter().copied())
        .map(|i| *indexp = i)
        .is_some()
}

/// Whether `units` form an ECMAScript IdentifierName (reserved words included).
fn is_identifier(units: impl Iterator<Item = u16>) -> bool {
    let mut empty = true;
    for c in char::decode_utf16(units) {
        let Ok(c) = c else {
            return false;
        };
        let c = c as u32;
        let ok = if c < 0x80 {
            let b = c as u8;
            b.is_ascii_alphabetic() || b == b'$' || b == b'_' || (!empty && b.is_ascii_digit())
        } else if empty {
            unsafe { lre_is_id_start(c) != 0 }
        } else {
            c == 0x200c || c == 0x200d || unsafe { lre_is_id_continue(c) != 0 }
        };
        if !ok {
            return false;
        }
        empty = false;
    }
    !empty
}

pub(crate) unsafe fn JS_IsIdentifier(
    cx: *mut root::JSContext,
    str_: root::JS::HandleString,
    isIdentifier: *mut bool,
) -> bool {
    let _ = cx;
    *isIdentifier = is_identifier(Chars::of(*str_.ptr).units());
    true
}

pub(crate) unsafe fn JS_IsIdentifier1(chars: *const u16, length: usize) -> bool {
    is_identifier(raw_slice(chars, length).iter().copied())
}

// JSON.

/// `JSON.parse(text, reviver)`; consumes `text`.
unsafe fn parse_json(
    cx: &Cx,
    text: q::JSValue,
    reviver: q::JSValue,
    vp: root::JS::MutableHandleValue,
) -> bool {
    if text.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let qctx = cx.qctx();
    let v = if q::JS_IsFunction(qctx, reviver) != 0 {
        let parse = super::intrinsics::get(
            cx,
            super::intrinsics::Intrinsic::Global(c"JSON", Some(c"parse")),
        );
        let mut args = [text, reviver];
        let v = q::JS_Call(qctx, parse, q::JS_UNDEFINED, 2, args.as_mut_ptr());
        q::JS_FreeValueRT(cx.rt, parse);
        v
    } else {
        let mut len = 0;
        let buf = q::JS_ToCStringLen2(qctx, &mut len, text, 0);
        if buf.is_null() {
            q::JS_EXCEPTION
        } else {
            let v = q::JS_ParseJSON(qctx, buf, len, c"<input>".as_ptr());
            q::JS_FreeCString(qctx, buf);
            v
        }
    };
    q::JS_FreeValueRT(cx.rt, text);
    if v.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *vp.ptr = cx.adopt(v);
    true
}

pub(crate) unsafe fn JS_ParseJSON(
    cx: *mut root::JSContext,
    chars: *const u16,
    len: u32,
    vp: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let chars = raw_slice(chars, len as usize);
    let text = q::JS_NewStringUTF16(cx.qctx(), chars.as_ptr(), chars.len());
    parse_json(cx, text, q::JS_UNDEFINED, vp)
}

pub(crate) unsafe fn JS_ParseJSON1(
    cx: *mut root::JSContext,
    str_: root::JS::Handle<*mut root::JSString>,
    vp: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let text = q::JS_DupValueRT(cx.rt, str_val(*str_.ptr));
    parse_json(cx, text, q::JS_UNDEFINED, vp)
}

pub(crate) unsafe fn JS_ParseJSON2(
    cx: *mut root::JSContext,
    chars: *const root::JS::Latin1Char,
    len: u32,
    vp: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let chars = raw_slice(chars, len as usize);
    let text = q::JS_NewStringLatin1(cx.qctx(), chars.as_ptr(), chars.len());
    parse_json(cx, text, q::JS_UNDEFINED, vp)
}

pub(crate) unsafe fn JS_ParseJSONWithReviver(
    cx: *mut root::JSContext,
    chars: *const u16,
    len: u32,
    reviver: root::JS::Handle<root::JS::Value>,
    vp: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let chars = raw_slice(chars, len as usize);
    let text = q::JS_NewStringUTF16(cx.qctx(), chars.as_ptr(), chars.len());
    parse_json(cx, text, (*reviver.ptr).to_qjs(), vp)
}

pub(crate) unsafe fn JS_ParseJSONWithReviver1(
    cx: *mut root::JSContext,
    str_: root::JS::Handle<*mut root::JSString>,
    reviver: root::JS::Handle<root::JS::Value>,
    vp: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let text = q::JS_DupValueRT(cx.rt, str_val(*str_.ptr));
    parse_json(cx, text, (*reviver.ptr).to_qjs(), vp)
}

/// `JSON.stringify(value, replacer, space)`, handed to `callback` in one piece. An undefined
/// result is written as "null" if `undefined_as_null`, and not written otherwise.
unsafe fn stringify(
    cx: &Cx,
    value: JSVal,
    replacer: *mut root::JSObject,
    space: JSVal,
    callback: root::JSONWriteCallback,
    data: *mut c_void,
    undefined_as_null: bool,
) -> bool {
    let replacer = if replacer.is_null() {
        q::JS_UNDEFINED
    } else {
        obj_val(replacer)
    };
    let s = q::JS_JSONStringify(cx.qctx(), value.to_qjs(), replacer, space.to_qjs());
    let units = match s.tag() {
        q::JS_TAG_EXCEPTION => return false,
        q::JS_TAG_UNDEFINED if !undefined_as_null => return true,
        q::JS_TAG_UNDEFINED => "null".encode_utf16().collect(),
        _ => Chars::of(cx.adopt(s).to_string()).to_utf16(),
    };
    match callback {
        Some(callback) => callback(units.as_ptr(), units.len() as u32, data),
        None => true,
    }
}

unsafe fn handle_obj(h: root::JS::Handle<*mut root::JSObject>) -> *mut root::JSObject {
    if h.ptr.is_null() {
        ptr::null_mut()
    } else {
        *h.ptr
    }
}

unsafe fn handle_val(h: root::JS::Handle<root::JS::Value>) -> JSVal {
    if h.ptr.is_null() {
        UndefinedValue()
    } else {
        *h.ptr
    }
}

pub(crate) unsafe fn JS_Stringify(
    cx: *mut root::JSContext,
    value: root::JS::MutableHandle<root::JS::Value>,
    replacer: root::JS::Handle<*mut root::JSObject>,
    space: root::JS::Handle<root::JS::Value>,
    callback: root::JSONWriteCallback,
    data: *mut ::std::os::raw::c_void,
) -> bool {
    stringify(
        Cx::from_raw(cx),
        *value.ptr,
        handle_obj(replacer),
        handle_val(space),
        callback,
        data,
        true,
    )
}

pub(crate) unsafe fn JS_StringifyWithLengthHint(
    cx: *mut root::JSContext,
    value: root::JS::MutableHandle<root::JS::Value>,
    replacer: root::JS::Handle<*mut root::JSObject>,
    space: root::JS::Handle<root::JS::Value>,
    callback: root::JSONWriteCallback,
    data: *mut ::std::os::raw::c_void,
    lengthHint: usize,
) -> bool {
    let _ = lengthHint;
    stringify(
        Cx::from_raw(cx),
        *value.ptr,
        handle_obj(replacer),
        handle_val(space),
        callback,
        data,
        true,
    )
}

pub(crate) unsafe fn ToJSON(
    cx: *mut root::JSContext,
    value: root::JS::Handle<root::JS::Value>,
    replacer: root::JS::Handle<*mut root::JSObject>,
    space: root::JS::Handle<root::JS::Value>,
    callback: root::JSONWriteCallback,
    data: *mut ::std::os::raw::c_void,
) -> bool {
    stringify(
        Cx::from_raw(cx),
        handle_val(value),
        handle_obj(replacer),
        handle_val(space),
        callback,
        data,
        false,
    )
}

pub(crate) unsafe fn ToJSONMaybeSafely(
    cx: *mut root::JSContext,
    input: root::JS::Handle<*mut root::JSObject>,
    callback: root::JSONWriteCallback,
    data: *mut ::std::os::raw::c_void,
) -> bool {
    let value = JSVal::from_qjs(obj_val(*input.ptr));
    stringify(
        Cx::from_raw(cx),
        value,
        ptr::null_mut(),
        UndefinedValue(),
        callback,
        data,
        false,
    )
}

// ValueToSource, as SpiderMonkey's `uneval`.

const QJS_CLASS_ARRAY: q::JSClassID = 2;
const QJS_CLASS_ERROR: q::JSClassID = 3;
const QJS_CLASS_NUMBER: q::JSClassID = 4;
const QJS_CLASS_STRING: q::JSClassID = 5;
const QJS_CLASS_BOOLEAN: q::JSClassID = 6;
const QJS_CLASS_DATE: q::JSClassID = 10;
const QJS_CLASS_REGEXP: q::JSClassID = 18;

const MAX_SOURCE_DEPTH: usize = 1000;

fn push_str(out: &mut Vec<u16>, s: &str) {
    out.extend(s.encode_utf16());
}

fn push_quoted(out: &mut Vec<u16>, chars: Chars) {
    let mut s = String::new();
    escape_into(&mut s, chars, b'"');
    push_str(out, &s);
}

/// Calls `f` with the characters of `ToString(v)`.
unsafe fn with_string_chars(cx: &Cx, v: q::JSValue, f: impl FnOnce(Chars)) -> bool {
    let s = q::JS_ToString(cx.qctx(), v);
    if s.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    f(Chars::of(s.ptr() as *mut root::JSString));
    q::JS_FreeValueRT(cx.rt, s);
    true
}

unsafe fn append_to_string(cx: &Cx, out: &mut Vec<u16>, v: q::JSValue) -> bool {
    with_string_chars(cx, v, |c| out.extend(c.units()))
}

unsafe fn value_to_source(
    cx: &Cx,
    v: q::JSValue,
    seen: &mut Vec<*mut c_void>,
    out: &mut Vec<u16>,
) -> bool {
    match v.tag() {
        q::JS_TAG_UNDEFINED => push_str(out, "(void 0)"),
        q::JS_TAG_STRING | q::JS_TAG_STRING_ROPE => {
            return with_string_chars(cx, v, |c| push_quoted(out, c))
        }
        q::JS_TAG_FLOAT64 if v.float64() == 0.0 && v.float64().is_sign_negative() => {
            push_str(out, "-0")
        }
        q::JS_TAG_SYMBOL => return symbol_to_source(cx, v, out),
        q::JS_TAG_BIG_INT | q::JS_TAG_SHORT_BIG_INT => {
            if !append_to_string(cx, out, v) {
                return false;
            }
            out.push(b'n' as u16);
        }
        q::JS_TAG_OBJECT => return object_to_source(cx, v, seen, out),
        _ => return append_to_string(cx, out, v),
    }
    true
}

/// `Symbol.iterator`, `Symbol.for("key")` or `Symbol("description")`.
unsafe fn symbol_to_source(cx: &Cx, sym: q::JSValue, out: &mut Vec<u16>) -> bool {
    let qctx = cx.qctx();
    let atom = q::JS_ValueToAtom(qctx, sym);
    let well_known = (0..root::JS::SymbolCode::Limit as usize)
        .any(|code| super::well_known_symbol_atom(cx, code) == atom);
    q::JS_FreeAtomRT(cx.rt, atom);
    let desc = super::property::symbol_description(cx, sym);
    if desc.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let mut ok = true;
    if well_known {
        ok = append_to_string(cx, out, desc);
    } else {
        let key_for = super::intrinsics::get(
            cx,
            super::intrinsics::Intrinsic::Global(c"Symbol", Some(c"keyFor")),
        );
        let mut args = [sym];
        let key = q::JS_Call(qctx, key_for, q::JS_UNDEFINED, 1, args.as_mut_ptr());
        q::JS_FreeValueRT(cx.rt, key_for);
        if key.tag() == q::JS_TAG_EXCEPTION {
            ok = false;
        } else {
            push_str(
                out,
                if key.tag() == q::JS_TAG_UNDEFINED {
                    "Symbol("
                } else {
                    "Symbol.for("
                },
            );
            if desc.tag() != q::JS_TAG_UNDEFINED {
                ok = with_string_chars(cx, desc, |c| push_quoted(out, c));
            }
            out.push(b')' as u16);
            q::JS_FreeValueRT(cx.rt, key);
        }
    }
    q::JS_FreeValueRT(cx.rt, desc);
    ok
}

unsafe fn object_to_source(
    cx: &Cx,
    obj: q::JSValue,
    seen: &mut Vec<*mut c_void>,
    out: &mut Vec<u16>,
) -> bool {
    let qctx = cx.qctx();
    if seen.contains(&obj.ptr()) {
        push_str(out, "{}");
        return true;
    }
    if seen.len() >= MAX_SOURCE_DEPTH {
        q::JS_ThrowInternalError(qctx, c"too much recursion".as_ptr());
        return false;
    }
    let to_source = q::JS_GetPropertyStr(qctx, obj, c"toSource".as_ptr());
    if to_source.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    if q::JS_IsFunction(qctx, to_source) != 0 {
        let r = q::JS_Call(qctx, to_source, obj, 0, ptr::null_mut());
        q::JS_FreeValueRT(cx.rt, to_source);
        if r.tag() == q::JS_TAG_EXCEPTION {
            return false;
        }
        let ok = append_to_string(cx, out, r);
        q::JS_FreeValueRT(cx.rt, r);
        return ok;
    }
    q::JS_FreeValueRT(cx.rt, to_source);
    seen.push(obj.ptr());
    let ok = builtin_object_to_source(cx, obj, seen, out);
    seen.pop();
    ok
}

unsafe fn builtin_object_to_source(
    cx: &Cx,
    obj: q::JSValue,
    seen: &mut Vec<*mut c_void>,
    out: &mut Vec<u16>,
) -> bool {
    let qctx = cx.qctx();
    if q::JS_IsFunction(qctx, obj) != 0 {
        return append_to_string(cx, out, obj);
    }
    let class = q::JS_GetClassID(obj);
    match class {
        QJS_CLASS_ARRAY => array_to_source(cx, obj, seen, out),
        QJS_CLASS_ERROR => error_to_source(cx, obj, out),
        QJS_CLASS_REGEXP => append_to_string(cx, out, obj),
        QJS_CLASS_NUMBER | QJS_CLASS_STRING | QJS_CLASS_BOOLEAN | QJS_CLASS_DATE => {
            let name = match class {
                QJS_CLASS_NUMBER => "Number",
                QJS_CLASS_STRING => "String",
                QJS_CLASS_BOOLEAN => "Boolean",
                _ => "Date",
            };
            let proto = q::JS_GetClassProto(qctx, class);
            let value_of = q::JS_GetPropertyStr(qctx, proto, c"valueOf".as_ptr());
            q::JS_FreeValueRT(cx.rt, proto);
            let prim = q::JS_Call(qctx, value_of, obj, 0, ptr::null_mut());
            q::JS_FreeValueRT(cx.rt, value_of);
            if prim.tag() == q::JS_TAG_EXCEPTION {
                return false;
            }
            push_str(out, &format!("(new {name}("));
            let ok = value_to_source(cx, prim, seen, out);
            q::JS_FreeValueRT(cx.rt, prim);
            push_str(out, "))");
            ok
        }
        _ => plain_object_to_source(cx, obj, seen, out),
    }
}

unsafe fn array_to_source(
    cx: &Cx,
    obj: q::JSValue,
    seen: &mut Vec<*mut c_void>,
    out: &mut Vec<u16>,
) -> bool {
    let qctx = cx.qctx();
    let len = q::JS_GetPropertyStr(qctx, obj, c"length".as_ptr());
    let mut n = 0u32;
    let r = q::JS_ToUint32(qctx, &mut n, len);
    q::JS_FreeValueRT(cx.rt, len);
    if r < 0 {
        return false;
    }
    out.push(b'[' as u16);
    for i in 0..n {
        if i > 0 {
            push_str(out, ", ");
        }
        let e = q::JS_GetPropertyUint32(qctx, obj, i);
        if e.tag() == q::JS_TAG_EXCEPTION {
            return false;
        }
        let ok = value_to_source(cx, e, seen, out);
        q::JS_FreeValueRT(cx.rt, e);
        if !ok {
            return false;
        }
    }
    out.push(b']' as u16);
    true
}

/// `(new TypeError("message", "file", line))`.
unsafe fn error_to_source(cx: &Cx, obj: q::JSValue, out: &mut Vec<u16>) -> bool {
    let qctx = cx.qctx();
    let mut props = [q::JS_UNDEFINED; 4];
    for (p, name) in props
        .iter_mut()
        .zip([c"name", c"message", c"fileName", c"lineNumber"])
    {
        *p = q::JS_GetPropertyStr(qctx, obj, name.as_ptr());
    }
    let [name, message, file, line] = props;
    let mut ok = props.iter().all(|p| p.tag() != q::JS_TAG_EXCEPTION);
    if ok {
        push_str(out, "(new ");
        ok = append_to_string(cx, out, name) && {
            out.push(b'(' as u16);
            message.tag() == q::JS_TAG_UNDEFINED
                || with_string_chars(cx, message, |c| push_quoted(out, c))
        };
        if ok
            && file.tag() == q::JS_TAG_STRING
            && Chars::of(file.ptr() as *mut root::JSString).len() > 0
        {
            push_str(out, ", ");
            push_quoted(out, Chars::of(file.ptr() as *mut root::JSString));
            if line.tag() == q::JS_TAG_INT && line.int() > 0 {
                push_str(out, &format!(", {}", line.int()));
            }
        }
        push_str(out, "))");
    }
    for p in props {
        q::JS_FreeValueRT(cx.rt, p);
    }
    ok
}

unsafe fn key_to_source(cx: &Cx, atom: q::JSAtom, out: &mut Vec<u16>) -> bool {
    if atom & ATOM_TAG_INT != 0 {
        push_str(out, &(atom & !ATOM_TAG_INT).to_string());
        return true;
    }
    let key = q::JS_AtomToValue(cx.qctx(), atom);
    let ok = if key.tag() == q::JS_TAG_SYMBOL {
        out.push(b'[' as u16);
        let ok = symbol_to_source(cx, key, out);
        out.push(b']' as u16);
        ok
    } else {
        with_string_chars(cx, key, |c| {
            if is_identifier(c.units()) {
                out.extend(c.units())
            } else {
                push_quoted(out, c)
            }
        })
    };
    q::JS_FreeValueRT(cx.rt, key);
    ok
}

/// `({key:value, get accessor() {...}})` over the own enumerable properties.
unsafe fn plain_object_to_source(
    cx: &Cx,
    obj: q::JSValue,
    seen: &mut Vec<*mut c_void>,
    out: &mut Vec<u16>,
) -> bool {
    let qctx = cx.qctx();
    let mut tab = ptr::null_mut();
    let mut len = 0;
    let flags = q::JS_GPN_STRING_MASK | q::JS_GPN_SYMBOL_MASK | q::JS_GPN_ENUM_ONLY;
    if q::JS_GetOwnPropertyNames(qctx, &mut tab, &mut len, obj, flags as c_int) < 0 {
        return false;
    }
    push_str(out, "({");
    let mut ok = true;
    let mut first = true;
    for prop in raw_slice(tab, len as usize) {
        let mut desc: q::JSPropertyDescriptor = std::mem::zeroed();
        let r = q::JS_GetOwnProperty(qctx, &mut desc, obj, prop.atom);
        if r < 0 {
            ok = false;
            break;
        }
        if r == 0 {
            continue;
        }
        if desc.flags as u32 & q::JS_PROP_GETSET != 0 {
            for f in [desc.getter, desc.setter] {
                if ok && q::JS_IsFunction(qctx, f) != 0 {
                    if !first {
                        push_str(out, ", ");
                    }
                    first = false;
                    ok = append_to_string(cx, out, f);
                }
            }
        } else {
            if !first {
                push_str(out, ", ");
            }
            first = false;
            ok = key_to_source(cx, prop.atom, out) && {
                out.push(b':' as u16);
                value_to_source(cx, desc.value, seen, out)
            };
        }
        for v in [desc.value, desc.getter, desc.setter] {
            q::JS_FreeValueRT(cx.rt, v);
        }
        if !ok {
            break;
        }
    }
    q::JS_FreePropertyEnum(qctx, tab, len);
    push_str(out, "})");
    ok
}

pub(crate) unsafe fn JS_ValueToSource(
    cx: *mut root::JSContext,
    v: root::JS::Handle<root::JS::Value>,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    let mut out = Vec::new();
    if !value_to_source(cx, (*v.ptr).to_qjs(), &mut Vec::new(), &mut out) {
        return ptr::null_mut();
    }
    new_utf16_string(cx, &out)
}
