/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The ICU4C C API, and owned wrappers of the ICU objects the Intl subset uses. On OpenHarmony
//! it is the NDK's `libicu.so` (ICU 72 headers, unversioned symbols); on other Linux, the system
//! ICU that `build.rs` links, whose symbols end in its major version (`ucol_open_74`).

#![allow(non_camel_case_types)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

pub type UErrorCode = i32;
pub type UChar = u16;
pub type UBool = i8;

pub enum UCollator {}
pub enum UNumberFormat {}
pub enum UDateFormat {}
pub enum UCalendar {}
pub enum UEnumeration {}
pub enum UFieldPositionIterator {}

const U_BUFFER_OVERFLOW_ERROR: UErrorCode = 15;
const ULOC_AVAILABLE_WITH_LEGACY_ALIASES: i32 = 2;

pub const UNUM_DECIMAL: i32 = 1;
pub const UNUM_CURRENCY: i32 = 2;
pub const UNUM_PERCENT: i32 = 3;
pub const UNUM_SCIENTIFIC: i32 = 4;
pub const UNUM_CURRENCY_ISO: i32 = 10;
pub const UNUM_CURRENCY_PLURAL: i32 = 11;
pub const UNUM_CURRENCY_ACCOUNTING: i32 = 12;
pub const UNUM_DECIMAL_COMPACT_SHORT: i32 = 14;
pub const UNUM_DECIMAL_COMPACT_LONG: i32 = 15;

pub const UNUM_GROUPING_USED: i32 = 1;
pub const UNUM_MAX_INTEGER_DIGITS: i32 = 3;
pub const UNUM_MIN_INTEGER_DIGITS: i32 = 4;
pub const UNUM_MAX_FRACTION_DIGITS: i32 = 6;
pub const UNUM_MIN_FRACTION_DIGITS: i32 = 7;
pub const UNUM_ROUNDING_MODE: i32 = 11;
pub const UNUM_SIGNIFICANT_DIGITS_USED: i32 = 16;
pub const UNUM_MIN_SIGNIFICANT_DIGITS: i32 = 17;
pub const UNUM_MAX_SIGNIFICANT_DIGITS: i32 = 18;
pub const UNUM_MINIMUM_GROUPING_DIGITS: i32 = 22;
pub const UNUM_SIGN_ALWAYS_SHOWN: i32 = 0x1004;
pub const UNUM_ROUND_HALFUP: i32 = 6;
const UNUM_CURRENCY_CODE: i32 = 5;
const UNUM_DECIMAL_SEPARATOR_SYMBOL: i32 = 0;

pub const UCOL_ALTERNATE_HANDLING: i32 = 1;
pub const UCOL_CASE_FIRST: i32 = 2;
pub const UCOL_CASE_LEVEL: i32 = 3;
pub const UCOL_STRENGTH: i32 = 5;
pub const UCOL_NUMERIC_COLLATION: i32 = 7;
pub const UCOL_PRIMARY: i32 = 0;
pub const UCOL_SECONDARY: i32 = 1;
pub const UCOL_TERTIARY: i32 = 2;
pub const UCOL_OFF: i32 = 16;
pub const UCOL_ON: i32 = 17;
pub const UCOL_SHIFTED: i32 = 20;
pub const UCOL_NON_IGNORABLE: i32 = 21;
pub const UCOL_LOWER_FIRST: i32 = 24;
pub const UCOL_UPPER_FIRST: i32 = 25;

pub const UDAT_NONE: i32 = -1;
const UDAT_PATTERN: i32 = -2;

/// Declares ICU functions under their symbol names, which end in `ICU_SUFFIX` (from `build.rs`).
macro_rules! icu {
    ($(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        #[cfg_attr(target_env = "ohos", link(name = "icu"))]
        unsafe extern "C" {
            $(
                #[link_name = concat!(stringify!($name), env!("ICU_SUFFIX"))]
                fn $name($($arg: $ty),*) $(-> $ret)?;
            )*
        }
    };
}

icu! {
    fn uloc_getDefault() -> *const c_char;
    fn uloc_forLanguageTag(
        langtag: *const c_char,
        locale_id: *mut c_char,
        capacity: i32,
        parsed_length: *mut i32,
        err: *mut UErrorCode,
    ) -> i32;
    fn uloc_toLanguageTag(
        locale_id: *const c_char,
        langtag: *mut c_char,
        capacity: i32,
        strict: UBool,
        err: *mut UErrorCode,
    ) -> i32;
    fn uloc_addLikelySubtags(
        locale_id: *const c_char,
        maximized: *mut c_char,
        capacity: i32,
        err: *mut UErrorCode,
    ) -> i32;
    fn uloc_openAvailableByType(kind: i32, err: *mut UErrorCode) -> *mut UEnumeration;
    fn uenum_next(en: *mut UEnumeration, length: *mut i32, err: *mut UErrorCode) -> *const c_char;
    fn uenum_close(en: *mut UEnumeration);

    fn ucol_open(locale: *const c_char, err: *mut UErrorCode) -> *mut UCollator;
    fn ucol_close(coll: *mut UCollator);
    fn ucol_setAttribute(coll: *mut UCollator, attr: i32, value: i32, err: *mut UErrorCode);
    fn ucol_getAttribute(coll: *const UCollator, attr: i32, err: *mut UErrorCode) -> i32;
    fn ucol_strcoll(
        coll: *const UCollator,
        source: *const UChar,
        source_length: i32,
        target: *const UChar,
        target_length: i32,
    ) -> i32;

    fn unum_open(
        style: i32,
        pattern: *const UChar,
        pattern_length: i32,
        locale: *const c_char,
        parse_err: *mut c_void,
        err: *mut UErrorCode,
    ) -> *mut UNumberFormat;
    fn unum_close(fmt: *mut UNumberFormat);
    fn unum_setAttribute(fmt: *mut UNumberFormat, attr: i32, value: i32);
    fn unum_getAttribute(fmt: *const UNumberFormat, attr: i32) -> i32;
    fn unum_setTextAttribute(
        fmt: *mut UNumberFormat,
        tag: i32,
        value: *const UChar,
        length: i32,
        err: *mut UErrorCode,
    );
    fn unum_getSymbol(
        fmt: *const UNumberFormat,
        symbol: i32,
        buffer: *mut UChar,
        size: i32,
        err: *mut UErrorCode,
    ) -> i32;
    fn unum_formatDouble(
        fmt: *const UNumberFormat,
        number: f64,
        result: *mut UChar,
        capacity: i32,
        pos: *mut c_void,
        err: *mut UErrorCode,
    ) -> i32;
    fn unum_formatDecimal(
        fmt: *const UNumberFormat,
        number: *const c_char,
        length: i32,
        result: *mut UChar,
        capacity: i32,
        pos: *mut c_void,
        err: *mut UErrorCode,
    ) -> i32;
    fn unum_formatDoubleForFields(
        fmt: *const UNumberFormat,
        number: f64,
        result: *mut UChar,
        capacity: i32,
        iter: *mut UFieldPositionIterator,
        err: *mut UErrorCode,
    ) -> i32;

    fn udat_open(
        time_style: i32,
        date_style: i32,
        locale: *const c_char,
        tz: *const UChar,
        tz_length: i32,
        pattern: *const UChar,
        pattern_length: i32,
        err: *mut UErrorCode,
    ) -> *mut UDateFormat;
    fn udat_close(fmt: *mut UDateFormat);
    fn udat_format(
        fmt: *const UDateFormat,
        date: f64,
        result: *mut UChar,
        capacity: i32,
        pos: *mut c_void,
        err: *mut UErrorCode,
    ) -> i32;
    fn udat_formatForFields(
        fmt: *const UDateFormat,
        date: f64,
        result: *mut UChar,
        capacity: i32,
        iter: *mut UFieldPositionIterator,
        err: *mut UErrorCode,
    ) -> i32;
    fn udat_toPattern(
        fmt: *const UDateFormat,
        localized: UBool,
        result: *mut UChar,
        capacity: i32,
        err: *mut UErrorCode,
    ) -> i32;
    fn udat_getCalendar(fmt: *const UDateFormat) -> *const UCalendar;
    fn udat_setCalendar(fmt: *mut UDateFormat, cal: *const UCalendar);
    fn ucal_clone(cal: *const UCalendar, err: *mut UErrorCode) -> *mut UCalendar;
    fn ucal_close(cal: *mut UCalendar);
    fn ucal_getType(cal: *const UCalendar, err: *mut UErrorCode) -> *const c_char;
    fn ucal_setGregorianChange(cal: *mut UCalendar, date: f64, err: *mut UErrorCode);
    fn ucal_getDefaultTimeZone(result: *mut UChar, capacity: i32, err: *mut UErrorCode) -> i32;
    fn ucal_getCanonicalTimeZoneID(
        id: *const UChar,
        length: i32,
        result: *mut UChar,
        capacity: i32,
        is_system_id: *mut UBool,
        err: *mut UErrorCode,
    ) -> i32;

    fn ufieldpositer_open(err: *mut UErrorCode) -> *mut UFieldPositionIterator;
    fn ufieldpositer_close(iter: *mut UFieldPositionIterator);
    fn ufieldpositer_next(iter: *mut UFieldPositionIterator, begin: *mut i32, end: *mut i32)
        -> i32;

    fn u_strToUpper(
        dest: *mut UChar,
        capacity: i32,
        src: *const UChar,
        length: i32,
        locale: *const c_char,
        err: *mut UErrorCode,
    ) -> i32;
    fn u_strToLower(
        dest: *mut UChar,
        capacity: i32,
        src: *const UChar,
        length: i32,
        locale: *const c_char,
        err: *mut UErrorCode,
    ) -> i32;
}

#[cfg(target_env = "ohos")]
#[link(name = "time_service_ndk")]
unsafe extern "C" {
    fn OH_TimeService_GetTimeZone(time_zone: *mut c_char, len: u32) -> i32;
}

fn failed(err: UErrorCode) -> bool {
    err > 0
}

/// Calls an ICU function that writes into a buffer, growing it once on overflow.
fn with_buffer<T: Copy + Default>(
    mut f: impl FnMut(*mut T, i32, &mut UErrorCode) -> i32,
) -> Option<Vec<T>> {
    let mut buf = vec![T::default(); 64];
    let mut err = 0;
    let mut len = f(buf.as_mut_ptr(), buf.len() as i32, &mut err);
    if err == U_BUFFER_OVERFLOW_ERROR {
        buf = vec![T::default(); len as usize + 1];
        err = 0;
        len = f(buf.as_mut_ptr(), buf.len() as i32, &mut err);
    }
    if failed(err) || len < 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(buf)
}

fn with_char_buffer(f: impl FnMut(*mut c_char, i32, &mut UErrorCode) -> i32) -> Option<String> {
    with_buffer::<c_char>(f)
        .and_then(|v| String::from_utf8(v.into_iter().map(|c| c as u8).collect()).ok())
}

/// The ICU locale ID of a BCP 47 tag, if the whole tag is well-formed.
pub fn locale_id(tag: &str) -> Option<String> {
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return None;
    }
    let ctag = CString::new(tag).ok()?;
    let mut parsed = 0;
    let id = with_char_buffer(|buf, cap, err| unsafe {
        uloc_forLanguageTag(ctag.as_ptr(), buf, cap, &mut parsed, err)
    })?;
    (parsed as usize == tag.len()).then_some(id)
}

pub fn language_tag(id: &str) -> Option<String> {
    let cid = CString::new(id).ok()?;
    with_char_buffer(|buf, cap, err| unsafe { uloc_toLanguageTag(cid.as_ptr(), buf, cap, 1, err) })
}

/// The canonical form of a well-formed BCP 47 tag.
pub fn canonicalize_tag(tag: &str) -> Option<String> {
    language_tag(&locale_id(tag)?)
}

/// `uloc_addLikelySubtags` of a locale ID, without keywords.
pub fn maximize(id: &str) -> String {
    let base = id.split('@').next().unwrap_or("");
    let Ok(cid) = CString::new(base) else {
        return String::new();
    };
    with_char_buffer(|buf, cap, err| unsafe { uloc_addLikelySubtags(cid.as_ptr(), buf, cap, err) })
        .unwrap_or_else(|| base.to_owned())
}

/// The system's default locale as a BCP 47 tag, without a script subtag its region implies.
pub fn default_tag() -> String {
    let id = unsafe { CStr::from_ptr(uloc_getDefault()) }
        .to_string_lossy()
        .into_owned();
    // OpenHarmony's uloc_getDefault returns the system locale parameter, a BCP 47 tag.
    let id = if id.contains('-') {
        locale_id(&id).unwrap_or_default()
    } else {
        id
    };
    let base = id.split('@').next().unwrap_or("").to_owned();
    let parts: Vec<&str> = base.split('_').collect();
    let mut chosen = base.clone();
    if parts.len() == 3 && parts[1].len() == 4 {
        let short = format!("{}_{}", parts[0], parts[2]);
        if maximize(&short) == maximize(&base) {
            chosen = short;
        }
    }
    language_tag(&chosen)
        .filter(|t| t != "und")
        .unwrap_or_else(|| "en-US".to_owned())
}

thread_local! {
    static AVAILABLE: RefCell<Option<HashSet<String>>> = const { RefCell::new(None) };
}

/// Whether ICU has data for the BCP 47 tag (including legacy aliases such as zh-CN).
pub fn is_available(tag: &str) -> bool {
    AVAILABLE.with(|available| {
        available
            .borrow_mut()
            .get_or_insert_with(|| {
                let mut set = HashSet::new();
                let mut err = 0;
                let en = unsafe {
                    uloc_openAvailableByType(ULOC_AVAILABLE_WITH_LEGACY_ALIASES, &mut err)
                };
                if en.is_null() {
                    return set;
                }
                loop {
                    let mut len = 0;
                    let id = unsafe { uenum_next(en, &mut len, &mut err) };
                    if id.is_null() || failed(err) {
                        break;
                    }
                    if let Some(tag) =
                        language_tag(&unsafe { CStr::from_ptr(id) }.to_string_lossy())
                    {
                        set.insert(tag.to_ascii_lowercase());
                    }
                }
                unsafe { uenum_close(en) };
                set
            })
            .contains(&tag.to_ascii_lowercase())
    })
}

pub struct Collator(*mut UCollator);

impl Collator {
    pub fn open(locale: &CStr, attributes: &[(i32, i32)]) -> Option<Collator> {
        let mut err = 0;
        let coll = unsafe { ucol_open(locale.as_ptr(), &mut err) };
        if coll.is_null() || failed(err) {
            return None;
        }
        let coll = Collator(coll);
        for &(attr, value) in attributes {
            unsafe { ucol_setAttribute(coll.0, attr, value, &mut err) };
        }
        (!failed(err)).then_some(coll)
    }

    pub fn attribute(&self, attr: i32) -> i32 {
        let mut err = 0;
        unsafe { ucol_getAttribute(self.0, attr, &mut err) }
    }

    pub fn compare(&self, a: &[u16], b: &[u16]) -> i32 {
        unsafe {
            ucol_strcoll(
                self.0,
                a.as_ptr(),
                a.len() as i32,
                b.as_ptr(),
                b.len() as i32,
            )
        }
    }
}

impl Drop for Collator {
    fn drop(&mut self) {
        unsafe { ucol_close(self.0) };
    }
}

pub struct NumberFormat(*mut UNumberFormat);

impl NumberFormat {
    pub fn open(style: i32, locale: &CStr) -> Option<NumberFormat> {
        let mut err = 0;
        let fmt = unsafe {
            unum_open(
                style,
                ptr::null(),
                0,
                locale.as_ptr(),
                ptr::null_mut(),
                &mut err,
            )
        };
        if fmt.is_null() {
            return None;
        }
        let fmt = NumberFormat(fmt);
        (!failed(err)).then_some(fmt)
    }

    pub fn set(&self, attr: i32, value: i32) {
        unsafe { unum_setAttribute(self.0, attr, value) };
    }

    pub fn get(&self, attr: i32) -> i32 {
        unsafe { unum_getAttribute(self.0, attr) }
    }

    pub fn set_currency(&self, code: &str) -> bool {
        let code: Vec<u16> = code.encode_utf16().collect();
        let mut err = 0;
        unsafe {
            unum_setTextAttribute(
                self.0,
                UNUM_CURRENCY_CODE,
                code.as_ptr(),
                code.len() as i32,
                &mut err,
            )
        };
        !failed(err)
    }

    pub fn decimal_separator(&self) -> Vec<u16> {
        with_buffer(|buf, cap, err| unsafe {
            unum_getSymbol(self.0, UNUM_DECIMAL_SEPARATOR_SYMBOL, buf, cap, err)
        })
        .unwrap_or_else(|| vec![b'.' as u16])
    }

    pub fn format(&self, x: f64) -> Option<Vec<u16>> {
        with_buffer(|buf, cap, err| unsafe {
            unum_formatDouble(self.0, x, buf, cap, ptr::null_mut(), err)
        })
    }

    /// Formats a decimal number string, e.g. a BigInt's digits.
    pub fn format_decimal(&self, digits: &str) -> Option<Vec<u16>> {
        with_buffer(|buf, cap, err| unsafe {
            unum_formatDecimal(
                self.0,
                digits.as_ptr() as *const c_char,
                digits.len() as i32,
                buf,
                cap,
                ptr::null_mut(),
                err,
            )
        })
    }

    /// The formatted number and its (UNumberFormatFields, begin, end) spans.
    pub fn format_to_parts(&self, x: f64) -> Option<(Vec<u16>, Vec<(i32, usize, usize)>)> {
        let iter = FieldIterator::new()?;
        let text = with_buffer(|buf, cap, err| unsafe {
            unum_formatDoubleForFields(self.0, x, buf, cap, iter.0, err)
        })?;
        Some((text, iter.spans()))
    }
}

impl Drop for NumberFormat {
    fn drop(&mut self) {
        unsafe { unum_close(self.0) };
    }
}

struct FieldIterator(*mut UFieldPositionIterator);

impl FieldIterator {
    fn new() -> Option<FieldIterator> {
        let mut err = 0;
        let iter = unsafe { ufieldpositer_open(&mut err) };
        (!iter.is_null() && !failed(err)).then_some(FieldIterator(iter))
    }

    fn spans(&self) -> Vec<(i32, usize, usize)> {
        let mut spans = Vec::new();
        loop {
            let (mut begin, mut end) = (0, 0);
            let field = unsafe { ufieldpositer_next(self.0, &mut begin, &mut end) };
            if field < 0 {
                break;
            }
            spans.push((field, begin as usize, end as usize));
        }
        spans
    }
}

impl Drop for FieldIterator {
    fn drop(&mut self) {
        unsafe { ufieldpositer_close(self.0) };
    }
}

pub struct DateFormat(*mut UDateFormat);

impl DateFormat {
    /// A formatter for `pattern`, or for the date and time styles (`UDAT_NONE` for neither) if
    /// `pattern` is `None`. `tz` is a time zone ID, `None` for the default zone.
    pub fn open(
        locale: &CStr,
        tz: Option<&[u16]>,
        pattern: Option<&[u16]>,
        styles: (i32, i32),
    ) -> Option<DateFormat> {
        let (tz_ptr, tz_len) = tz.map_or((ptr::null(), 0), |t| (t.as_ptr(), t.len() as i32));
        let (time, date, pat_ptr, pat_len) = match pattern {
            Some(p) => (UDAT_PATTERN, UDAT_PATTERN, p.as_ptr(), p.len() as i32),
            None => (styles.1, styles.0, ptr::null(), 0),
        };
        let mut err = 0;
        let fmt = unsafe {
            udat_open(
                time,
                date,
                locale.as_ptr(),
                tz_ptr,
                tz_len,
                pat_ptr,
                pat_len,
                &mut err,
            )
        };
        if fmt.is_null() {
            return None;
        }
        let fmt = DateFormat(fmt);
        if failed(err) {
            return None;
        }
        Some(fmt)
    }

    /// Makes a Gregorian calendar proleptic, as ECMAScript's.
    pub fn make_proleptic(&self) {
        unsafe {
            let mut err = 0;
            let cal = udat_getCalendar(self.0);
            let kind = ucal_getType(cal, &mut err);
            if failed(err) || kind.is_null() || CStr::from_ptr(kind).to_bytes() != b"gregorian" {
                return;
            }
            let copy = ucal_clone(cal, &mut err);
            if copy.is_null() {
                return;
            }
            ucal_setGregorianChange(copy, -8.64e15, &mut err);
            if !failed(err) {
                udat_setCalendar(self.0, copy);
            }
            ucal_close(copy);
        }
    }

    pub fn pattern(&self) -> Option<Vec<u16>> {
        with_buffer(|buf, cap, err| unsafe { udat_toPattern(self.0, 0, buf, cap, err) })
    }

    pub fn format(&self, t: f64) -> Option<Vec<u16>> {
        with_buffer(|buf, cap, err| unsafe {
            udat_format(self.0, t, buf, cap, ptr::null_mut(), err)
        })
        .map(replace_special_spaces)
    }

    /// The formatted date and its (UDateFormatField, begin, end) spans.
    pub fn format_to_parts(&self, t: f64) -> Option<(Vec<u16>, Vec<(i32, usize, usize)>)> {
        let iter = FieldIterator::new()?;
        let text = with_buffer(|buf, cap, err| unsafe {
            udat_formatForFields(self.0, t, buf, cap, iter.0, err)
        })?;
        Some((replace_special_spaces(text), iter.spans()))
    }
}

/// Replaces NARROW NO-BREAK SPACE and THIN SPACE, which CLDR 42 put into time formats, with a
/// space, as SpiderMonkey does: pages expect "3:05 PM" (Firefox bug 1806042).
fn replace_special_spaces(mut text: Vec<u16>) -> Vec<u16> {
    for c in &mut text {
        if *c == 0x202F || *c == 0x2009 {
            *c = 0x20;
        }
    }
    text
}

impl Drop for DateFormat {
    fn drop(&mut self) {
        unsafe { udat_close(self.0) };
    }
}

thread_local! {
    /// The last system time zone name and its canonical ID.
    static SYSTEM_TIME_ZONE: RefCell<(String, String)> = RefCell::default();
}

/// The time zone `Date` uses: `TZ`, else the system's. On OpenHarmony, ICU's default can differ
/// (outside apps it is a guess from the UTC offset); on Linux, ICU reads the system's.
pub fn system_time_zone() -> String {
    let tz = std::env::var("TZ")
        .ok()
        .map(|tz| tz.trim_start_matches(':').to_owned())
        .filter(|tz| !tz.is_empty());
    #[cfg(target_env = "ohos")]
    let tz = tz.or_else(|| {
        let mut buf = [0 as c_char; 64];
        let r = unsafe { OH_TimeService_GetTimeZone(buf.as_mut_ptr(), buf.len() as u32) };
        let tz = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        (r == 0 && !tz.is_empty()).then_some(tz)
    });
    if let Some(tz) = tz {
        let cached = SYSTEM_TIME_ZONE.with(|c| {
            let c = c.borrow();
            (c.0 == tz).then(|| c.1.clone())
        });
        if let Some(canonical) = cached.or_else(|| canonical_time_zone(&tz)) {
            SYSTEM_TIME_ZONE.with(|c| *c.borrow_mut() = (tz, canonical.clone()));
            return canonical;
        }
    }
    with_buffer(|buf, cap, err| unsafe { ucal_getDefaultTimeZone(buf, cap, err) })
        .map(|v| String::from_utf16_lossy(&v))
        .unwrap_or_else(|| "UTC".to_owned())
}

/// The canonical ID of an IANA time zone name, if ICU knows it.
pub fn canonical_time_zone(id: &str) -> Option<String> {
    let id: Vec<u16> = id.encode_utf16().collect();
    let mut system = 0;
    let canonical = with_buffer(|buf, cap, err| unsafe {
        ucal_getCanonicalTimeZoneID(id.as_ptr(), id.len() as i32, buf, cap, &mut system, err)
    })?;
    (system != 0).then(|| String::from_utf16_lossy(&canonical))
}

pub fn to_upper(s: &[u16], language: &CStr) -> Option<Vec<u16>> {
    with_buffer(|buf, cap, err| unsafe {
        u_strToUpper(buf, cap, s.as_ptr(), s.len() as i32, language.as_ptr(), err)
    })
}

pub fn to_lower(s: &[u16], language: &CStr) -> Option<Vec<u16>> {
    with_buffer(|buf, cap, err| unsafe {
        u_strToLower(buf, cap, s.as_ptr(), s.len() as i32, language.as_ptr(), err)
    })
}
