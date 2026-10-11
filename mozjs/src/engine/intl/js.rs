/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! The JavaScript side of the Intl subset: QuickJS native functions and option processing.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::ptr;
use std::rc::Rc;
use std::slice;
use std::sync::OnceLock;

use servo_quickjs as q;

use super::dtpg::{self, Generator, LocaleData};
use super::{icu, patterns};

/// `Err` means a JavaScript exception is pending.
type Result<T> = std::result::Result<T, ()>;

const COLLATOR: usize = 0;
const NUMBER_FORMAT: usize = 1;
const DATE_TIME_FORMAT: usize = 2;
const NAMES: [&CStr; 3] = [c"Collator", c"NumberFormat", c"DateTimeFormat"];
const TAGS: [&CStr; 3] = [
    c"Intl.Collator",
    c"Intl.NumberFormat",
    c"Intl.DateTimeFormat",
];

const CLASS_NUMBER: q::JSClassID = 4;
const CLASS_STRING: q::JSClassID = 5;
const CLASS_DATE: q::JSClassID = 10;

const METHOD: c_int = (q::JS_PROP_WRITABLE | q::JS_PROP_CONFIGURABLE) as c_int;
const DATA: c_int =
    (q::JS_PROP_WRITABLE | q::JS_PROP_CONFIGURABLE | q::JS_PROP_ENUMERABLE) as c_int;

/// The opaque data of Intl objects.
struct IntlObject {
    state: State,
    /// The bound `format` or `compare` function, once read; a counted reference.
    bound: Cell<q::JSValue>,
}

enum State {
    Collator(Rc<CollatorState>),
    Number(Rc<NumberState>),
    Date(Rc<DateState>),
}

static CLASS_IDS: OnceLock<[q::JSClassID; 3]> = OnceLock::new();

unsafe fn class_ids(rt: *mut q::JSRuntime) -> [q::JSClassID; 3] {
    let ids =
        *CLASS_IDS.get_or_init(|| std::array::from_fn(|_| super::super::class::new_class_id()));
    for (id, name) in ids.iter().zip(NAMES) {
        if q::JS_IsRegisteredClass(rt, *id) == 0 {
            let def = q::JSClassDef {
                class_name: name.as_ptr(),
                finalizer: Some(finalize),
                gc_mark: Some(gc_mark),
                call: None,
                exotic: ptr::null_mut(),
            };
            assert_eq!(q::JS_NewClass(rt, *id, &def), 0);
        }
    }
    ids
}

unsafe fn intl_object<'a>(v: q::JSValue, kind: usize) -> Option<&'a IntlObject> {
    let id = CLASS_IDS.get()?[kind];
    (q::JS_GetOpaque(v, id) as *const IntlObject).as_ref()
}

unsafe extern "C" fn finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let p = q::JS_GetOpaque(val, q::JS_GetClassID(val)) as *mut IntlObject;
    if !p.is_null() {
        let obj = Box::from_raw(p);
        q::JS_FreeValueRT(rt, obj.bound.get());
    }
}

unsafe extern "C" fn gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark_func: q::JS_MarkFunc) {
    let p = q::JS_GetOpaque(val, q::JS_GetClassID(val)) as *mut IntlObject;
    if let Some(obj) = p.as_ref() {
        q::JS_MarkValue(rt, obj.bound.get(), mark_func);
    }
}

// Values and errors.

fn is_undefined(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_UNDEFINED
}

fn is_string(v: q::JSValue) -> bool {
    v.tag() == q::JS_TAG_STRING || v.tag() == q::JS_TAG_STRING_ROPE
}

unsafe fn arg(argc: c_int, argv: *mut q::JSValue, i: usize) -> q::JSValue {
    if (i as c_int) < argc {
        *argv.add(i)
    } else {
        q::JS_UNDEFINED
    }
}

unsafe fn throw_range<T>(ctx: *mut q::JSContext, msg: &str) -> Result<T> {
    let msg = CString::new(msg).unwrap_or_default();
    q::JS_ThrowRangeError(ctx, c"%s".as_ptr(), msg.as_ptr());
    Err(())
}

unsafe fn throw_type<T>(ctx: *mut q::JSContext, msg: &str) -> Result<T> {
    let msg = CString::new(msg).unwrap_or_default();
    q::JS_ThrowTypeError(ctx, c"%s".as_ptr(), msg.as_ptr());
    Err(())
}

fn ret(r: Result<q::JSValue>) -> q::JSValue {
    r.unwrap_or(q::JS_EXCEPTION)
}

unsafe fn get(ctx: *mut q::JSContext, obj: q::JSValue, name: &CStr) -> Result<q::JSValue> {
    let v = q::JS_GetPropertyStr(ctx, obj, name.as_ptr());
    if v.tag() == q::JS_TAG_EXCEPTION {
        Err(())
    } else {
        Ok(v)
    }
}

/// ToString, as UTF-8.
unsafe fn to_string(ctx: *mut q::JSContext, v: q::JSValue) -> Result<String> {
    let mut len = 0;
    let p = q::JS_ToCStringLen2(ctx, &mut len, v, 0);
    if p.is_null() {
        return Err(());
    }
    let s = String::from_utf8_lossy(slice::from_raw_parts(p as *const u8, len)).into_owned();
    q::JS_FreeCString(ctx, p);
    Ok(s)
}

/// Calls `f` with the UTF-16 code units of ToString(`v`).
unsafe fn with_units<R>(
    ctx: *mut q::JSContext,
    v: q::JSValue,
    f: impl FnOnce(&[u16]) -> R,
) -> Result<R> {
    let s = q::JS_ToString(ctx, v);
    if s.tag() == q::JS_TAG_EXCEPTION {
        return Err(());
    }
    let mut data = ptr::null();
    let mut wide = 0;
    let len = q::JS_GetStringData(s, &mut data, &mut wide);
    let r = if wide != 0 {
        f(slice::from_raw_parts(data as *const u16, len))
    } else {
        let bytes = slice::from_raw_parts(data as *const u8, len);
        let mut stack = [0u16; 128];
        if len <= stack.len() {
            for (d, &b) in stack.iter_mut().zip(bytes) {
                *d = b as u16;
            }
            f(&stack[..len])
        } else {
            f(&bytes.iter().map(|&b| b as u16).collect::<Vec<_>>())
        }
    };
    q::JS_FreeValue(ctx, s);
    Ok(r)
}

unsafe fn new_string(ctx: *mut q::JSContext, units: &[u16]) -> q::JSValue {
    q::JS_NewStringUTF16(ctx, units.as_ptr(), units.len())
}

unsafe fn new_str(ctx: *mut q::JSContext, s: &str) -> q::JSValue {
    q::JS_NewStringLen(ctx, s.as_ptr() as *const c_char, s.len())
}

unsafe fn formatted(ctx: *mut q::JSContext, units: Option<Vec<u16>>) -> Result<q::JSValue> {
    match units {
        Some(units) => Ok(new_string(ctx, &units)),
        None => throw_range(ctx, "formatting failed"),
    }
}

/// Builds `resolvedOptions()` results and parts.
struct ObjectBuilder {
    ctx: *mut q::JSContext,
    obj: q::JSValue,
}

impl ObjectBuilder {
    unsafe fn new(ctx: *mut q::JSContext) -> ObjectBuilder {
        ObjectBuilder {
            ctx,
            obj: q::JS_NewObject(ctx),
        }
    }

    unsafe fn value(&self, name: &CStr, v: q::JSValue) -> &Self {
        q::JS_DefinePropertyValueStr(self.ctx, self.obj, name.as_ptr(), v, DATA);
        self
    }

    unsafe fn str(&self, name: &CStr, s: &str) -> &Self {
        self.value(name, new_str(self.ctx, s))
    }

    unsafe fn int(&self, name: &CStr, i: i32) -> &Self {
        self.value(name, q::JS_NewInt32(self.ctx, i))
    }

    unsafe fn bool(&self, name: &CStr, b: bool) -> &Self {
        self.value(name, q::JS_NewBool(self.ctx, b as c_int))
    }
}

unsafe fn new_array(
    ctx: *mut q::JSContext,
    items: impl IntoIterator<Item = q::JSValue>,
) -> q::JSValue {
    let array = q::JS_NewArray(ctx);
    for (i, v) in items.into_iter().enumerate() {
        q::JS_DefinePropertyValueUint32(ctx, array, i as u32, v, DATA);
    }
    array
}

/// `[{type, value}, ...]` from UTF-16 text and part types per code unit.
unsafe fn parts_array(ctx: *mut q::JSContext, text: &[u16], types: &[&str]) -> q::JSValue {
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 1..=text.len() {
        if i == text.len() || types[i] != types[start] {
            let part = ObjectBuilder::new(ctx);
            part.str(c"type", types[start])
                .value(c"value", new_string(ctx, &text[start..i]));
            parts.push(part.obj);
            start = i;
        }
    }
    new_array(ctx, parts)
}

// Options (ECMA-402 GetOption and friends).

/// CoerceOptionsToObject, keeping undefined as "no options".
unsafe fn options_object(ctx: *mut q::JSContext, options: q::JSValue) -> Result<q::JSValue> {
    if options.tag() == q::JS_TAG_NULL {
        return throw_type(ctx, "options must be an object");
    }
    Ok(options)
}

unsafe fn option_value(
    ctx: *mut q::JSContext,
    options: q::JSValue,
    name: &CStr,
) -> Result<Option<q::JSValue>> {
    if is_undefined(options) {
        return Ok(None);
    }
    let v = get(ctx, options, name)?;
    Ok(if is_undefined(v) { None } else { Some(v) })
}

unsafe fn string_option(
    ctx: *mut q::JSContext,
    options: q::JSValue,
    name: &CStr,
    values: &[&'static str],
) -> Result<Option<&'static str>> {
    let Some(v) = option_value(ctx, options, name)? else {
        return Ok(None);
    };
    let s = to_string(ctx, v);
    q::JS_FreeValue(ctx, v);
    let s = s?;
    match values.iter().find(|&&x| x == s) {
        Some(&x) => Ok(Some(x)),
        None => throw_range(
            ctx,
            &format!("invalid value {s:?} for option {}", name.to_string_lossy()),
        ),
    }
}

unsafe fn any_string_option(
    ctx: *mut q::JSContext,
    options: q::JSValue,
    name: &CStr,
) -> Result<Option<String>> {
    let Some(v) = option_value(ctx, options, name)? else {
        return Ok(None);
    };
    let s = to_string(ctx, v);
    q::JS_FreeValue(ctx, v);
    s.map(Some)
}

unsafe fn bool_option(
    ctx: *mut q::JSContext,
    options: q::JSValue,
    name: &CStr,
) -> Result<Option<bool>> {
    let Some(v) = option_value(ctx, options, name)? else {
        return Ok(None);
    };
    let b = q::JS_ToBool(ctx, v);
    q::JS_FreeValue(ctx, v);
    if b < 0 {
        Err(())
    } else {
        Ok(Some(b != 0))
    }
}

unsafe fn number_option(
    ctx: *mut q::JSContext,
    options: q::JSValue,
    name: &CStr,
    min: i32,
    max: i32,
) -> Result<Option<i32>> {
    let Some(v) = option_value(ctx, options, name)? else {
        return Ok(None);
    };
    let mut x = 0.0;
    let r = q::JS_ToFloat64(ctx, &mut x, v);
    q::JS_FreeValue(ctx, v);
    if r < 0 {
        return Err(());
    }
    if x.is_nan() || x < min as f64 || x > max as f64 {
        return throw_range(
            ctx,
            &format!("{} value is out of range", name.to_string_lossy()),
        );
    }
    Ok(Some(x.floor() as i32))
}

/// A Unicode extension type (`-u-ca-<type>`): 3 to 8 alphanumerics per subtag.
fn is_type(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|t| (3..=8).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric()))
}

unsafe fn type_option(
    ctx: *mut q::JSContext,
    options: q::JSValue,
    name: &CStr,
) -> Result<Option<String>> {
    match any_string_option(ctx, options, name)? {
        Some(s) if !is_type(&s) => {
            throw_range(ctx, &format!("invalid {}: {s:?}", name.to_string_lossy()))
        }
        s => Ok(s.map(|s| s.to_ascii_lowercase())),
    }
}

// Locales.

/// CanonicalizeLocaleList.
unsafe fn locale_list(ctx: *mut q::JSContext, locales: q::JSValue) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if is_undefined(locales) {
        return Ok(out);
    }
    if is_string(locales) {
        push_tag(ctx, &mut out, &to_string(ctx, locales)?)?;
        return Ok(out);
    }
    if locales.tag() == q::JS_TAG_NULL {
        return throw_type(ctx, "locales must not be null");
    }
    let length = get(ctx, locales, c"length")?;
    let mut len = 0.0;
    let r = q::JS_ToFloat64(ctx, &mut len, length);
    q::JS_FreeValue(ctx, length);
    if r < 0 {
        return Err(());
    }
    let len = if len.is_nan() || len <= 0.0 {
        0
    } else {
        len.min(u32::MAX as f64) as u32
    };
    for i in 0..len {
        let v = q::JS_GetPropertyUint32(ctx, locales, i);
        if v.tag() == q::JS_TAG_EXCEPTION {
            return Err(());
        }
        if is_undefined(v) {
            continue;
        }
        if !is_string(v) && v.tag() != q::JS_TAG_OBJECT {
            q::JS_FreeValue(ctx, v);
            return throw_type(ctx, "locales must be strings");
        }
        let tag = to_string(ctx, v);
        q::JS_FreeValue(ctx, v);
        push_tag(ctx, &mut out, &tag?)?;
    }
    Ok(out)
}

unsafe fn push_tag(ctx: *mut q::JSContext, out: &mut Vec<String>, tag: &str) -> Result<()> {
    match icu::canonicalize_tag(tag) {
        Some(tag) => {
            if !out.contains(&tag) {
                out.push(tag);
            }
            Ok(())
        }
        None => throw_range(ctx, &format!("invalid language tag: {tag}")),
    }
}

/// Splits a tag into its part before any extension, and the keys and types of its -u- extension.
fn split_extension(tag: &str) -> (String, Vec<(String, String)>) {
    let subtags: Vec<&str> = tag.split('-').collect();
    let base_len = subtags
        .iter()
        .position(|s| s.len() == 1)
        .unwrap_or(subtags.len());
    let mut keywords = Vec::new();
    let mut i = base_len;
    while i < subtags.len() {
        let singleton = subtags[i];
        i += 1;
        if singleton == "x" {
            break;
        }
        if singleton != "u" {
            while i < subtags.len() && subtags[i].len() != 1 {
                i += 1;
            }
            continue;
        }
        while i < subtags.len() && subtags[i].len() > 2 {
            i += 1;
        }
        while i < subtags.len() && subtags[i].len() == 2 {
            let key = subtags[i];
            i += 1;
            let start = i;
            while i < subtags.len() && subtags[i].len() > 2 {
                i += 1;
            }
            keywords.push((key.to_owned(), subtags[start..i].join("-")));
        }
    }
    (subtags[..base_len].join("-"), keywords)
}

/// BestAvailableLocale.
fn best_available(tag: &str) -> Option<String> {
    let mut candidate = tag.to_owned();
    loop {
        if icu::is_available(&candidate) {
            return Some(candidate);
        }
        let mut pos = candidate.rfind('-')?;
        if pos >= 2 && candidate.as_bytes()[pos - 2] == b'-' {
            pos -= 2;
        }
        candidate.truncate(pos);
    }
}

thread_local! {
    static DEFAULT_LOCALE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn default_locale() -> String {
    DEFAULT_LOCALE.with(|d| {
        d.borrow_mut()
            .get_or_insert_with(|| {
                best_available(&split_extension(&icu::default_tag()).0)
                    .unwrap_or_else(|| "en-US".to_owned())
            })
            .clone()
    })
}

/// LookupMatcher: the available locale and the requested -u- keywords.
fn lookup(requested: &[String]) -> (String, Vec<(String, String)>) {
    for tag in requested {
        let (base, keywords) = split_extension(tag);
        if let Some(found) = best_available(&base) {
            return (found, keywords);
        }
    }
    (default_locale(), Vec::new())
}

fn keyword<'a>(keywords: &'a [(String, String)], key: &str) -> Option<&'a str> {
    keywords
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// A resolved locale.
struct Locale {
    /// The BCP 47 tag `resolvedOptions().locale` reports.
    tag: String,
    /// The ICU locale ID, with the keywords that apply.
    id: CString,
    /// `uloc_addLikelySubtags` of the locale, e.g. `zh_Hans_CN`.
    maximized: String,
}

impl Locale {
    fn new(base: &str, keywords: &[(&str, &str)]) -> Rc<Locale> {
        let mut tag = base.to_owned();
        if !keywords.is_empty() {
            tag.push_str("-u");
            for (key, value) in keywords {
                tag.push('-');
                tag.push_str(key);
                if !value.is_empty() && *value != "true" {
                    tag.push('-');
                    tag.push_str(value);
                }
            }
        }
        let key = format!("L|{tag}");
        if let Some(locale) = cache_get(&key) {
            return locale;
        }
        let id = icu::locale_id(&tag)
            .or_else(|| icu::locale_id(base))
            .unwrap_or_default();
        let maximized = icu::maximize(&id);
        let locale = Rc::new(Locale {
            tag,
            id: CString::new(id).unwrap_or_default(),
            maximized,
        });
        cache_put(key, locale.clone());
        locale
    }

    fn with_keyword(&self, key: &str, value: &str) -> CString {
        let (base, mut keywords) = split_extension(&self.tag);
        keywords.retain(|(k, _)| k != key);
        keywords.push((key.to_owned(), value.to_owned()));
        let keywords: Vec<(&str, &str)> = keywords
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        Locale::new(&base, &keywords).id.clone()
    }
}

thread_local! {
    /// ICU objects, pattern generators, and the states of `toLocaleString`-like calls, by a key
    /// that starts with their kind.
    static CACHE: RefCell<HashMap<String, Rc<dyn Any>>> = RefCell::default();
}

const CACHE_LIMIT: usize = 256;

fn cache_get<T: 'static>(key: &str) -> Option<Rc<T>> {
    CACHE
        .with(|c| c.borrow().get(key).cloned())
        .and_then(|v| v.downcast().ok())
}

fn cache_put(key: String, v: Rc<dyn Any>) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_LIMIT {
            c.clear();
        }
        c.insert(key, v);
    });
}

fn cached<T: 'static>(key: String, make: impl FnOnce() -> Option<T>) -> Option<Rc<T>> {
    if let Some(v) = cache_get(&key) {
        return Some(v);
    }
    let v = Rc::new(make()?);
    cache_put(key, v.clone());
    Some(v)
}

/// The cache key of a `toLocaleString`-like call without options and with no or one locale.
unsafe fn simple_call_key(
    ctx: *mut q::JSContext,
    locales: q::JSValue,
    options: q::JSValue,
    kind: &str,
) -> Result<Option<String>> {
    if !is_undefined(options) {
        return Ok(None);
    }
    if is_undefined(locales) {
        return Ok(Some(kind.to_owned()));
    }
    if is_string(locales) {
        return Ok(Some(format!("{kind}|{}", to_string(ctx, locales)?)));
    }
    Ok(None)
}

// Intl.Collator.

struct CollatorState {
    locale: String,
    usage: &'static str,
    sensitivity: &'static str,
    ignore_punctuation: bool,
    collation: String,
    numeric: bool,
    case_first: &'static str,
    collator: Rc<icu::Collator>,
}

unsafe fn collator_state(
    ctx: *mut q::JSContext,
    locales: q::JSValue,
    options: q::JSValue,
) -> Result<Rc<CollatorState>> {
    let key = simple_call_key(ctx, locales, options, "c")?;
    if let Some(state) = key.as_deref().and_then(cache_get) {
        return Ok(state);
    }
    let requested = locale_list(ctx, locales)?;
    let options = options_object(ctx, options)?;
    let usage = string_option(ctx, options, c"usage", &["sort", "search"])?.unwrap_or("sort");
    string_option(ctx, options, c"localeMatcher", &["lookup", "best fit"])?;
    let collation = type_option(ctx, options, c"collation")?;
    let numeric = bool_option(ctx, options, c"numeric")?;
    let case_first = string_option(ctx, options, c"caseFirst", &["upper", "lower", "false"])?;
    let sensitivity = string_option(
        ctx,
        options,
        c"sensitivity",
        &["base", "accent", "case", "variant"],
    )?
    .unwrap_or("variant");
    let ignore_punctuation = bool_option(ctx, options, c"ignorePunctuation")?;

    let (base, requested_keywords) = lookup(&requested);
    let mut keywords: Vec<(&str, &str)> = Vec::new();
    let co = keyword(&requested_keywords, "co").filter(|co| !matches!(*co, "standard" | "search"));
    let collation = match (collation.as_deref(), co) {
        (Some(c), _) => c.to_owned(),
        (None, Some(co)) => {
            keywords.push(("co", co));
            co.to_owned()
        }
        (None, None) => "default".to_owned(),
    };
    let kn = keyword(&requested_keywords, "kn").filter(|v| ["", "true", "false"].contains(v));
    if let (None, Some(kn)) = (numeric, kn) {
        keywords.push(("kn", kn));
    }
    let kf = keyword(&requested_keywords, "kf").filter(|v| ["upper", "lower", "false"].contains(v));
    if let (None, Some(kf)) = (case_first, kf) {
        keywords.push(("kf", kf));
    }
    let locale = Locale::new(&base, &keywords);
    let id = if collation != "default" || usage == "search" {
        locale.with_keyword(
            "co",
            if usage == "search" {
                "search"
            } else {
                &collation
            },
        )
    } else {
        locale.id.clone()
    };
    // Options override the locale's defaults (and its keywords); the rest is read back from ICU.
    let mut attributes = vec![
        (
            icu::UCOL_STRENGTH,
            match sensitivity {
                "base" | "case" => icu::UCOL_PRIMARY,
                "accent" => icu::UCOL_SECONDARY,
                _ => icu::UCOL_TERTIARY,
            },
        ),
        (
            icu::UCOL_CASE_LEVEL,
            if sensitivity == "case" {
                icu::UCOL_ON
            } else {
                icu::UCOL_OFF
            },
        ),
    ];
    if let Some(numeric) = numeric {
        attributes.push((
            icu::UCOL_NUMERIC_COLLATION,
            if numeric { icu::UCOL_ON } else { icu::UCOL_OFF },
        ));
    }
    if let Some(case_first) = case_first {
        attributes.push((
            icu::UCOL_CASE_FIRST,
            match case_first {
                "upper" => icu::UCOL_UPPER_FIRST,
                "lower" => icu::UCOL_LOWER_FIRST,
                _ => icu::UCOL_OFF,
            },
        ));
    }
    if let Some(ignore) = ignore_punctuation {
        attributes.push((
            icu::UCOL_ALTERNATE_HANDLING,
            if ignore {
                icu::UCOL_SHIFTED
            } else {
                icu::UCOL_NON_IGNORABLE
            },
        ));
    }
    let cache_key = format!("C|{}|{attributes:?}", id.to_string_lossy());
    let Some(collator) = cached(cache_key, || icu::Collator::open(&id, &attributes)) else {
        return throw_range(ctx, "cannot create a collator");
    };
    let numeric = collator.attribute(icu::UCOL_NUMERIC_COLLATION) == icu::UCOL_ON;
    let case_first = match collator.attribute(icu::UCOL_CASE_FIRST) {
        icu::UCOL_UPPER_FIRST => "upper",
        icu::UCOL_LOWER_FIRST => "lower",
        _ => "false",
    };
    let ignore_punctuation = collator.attribute(icu::UCOL_ALTERNATE_HANDLING) == icu::UCOL_SHIFTED;
    let state = Rc::new(CollatorState {
        locale: locale.tag.clone(),
        usage,
        sensitivity,
        ignore_punctuation,
        collation,
        numeric,
        case_first,
        collator,
    });
    if let Some(key) = key {
        cache_put(key, state.clone());
    }
    Ok(state)
}

unsafe fn compare(
    ctx: *mut q::JSContext,
    state: &CollatorState,
    x: q::JSValue,
    y: q::JSValue,
) -> Result<q::JSValue> {
    let r = with_units(ctx, x, |a| {
        with_units(ctx, y, |b| state.collator.compare(a, b))
    })??;
    Ok(q::JS_NewInt32(ctx, r))
}

unsafe fn collator_resolved_options(ctx: *mut q::JSContext, s: &CollatorState) -> q::JSValue {
    let o = ObjectBuilder::new(ctx);
    o.str(c"locale", &s.locale)
        .str(c"usage", s.usage)
        .str(c"sensitivity", s.sensitivity)
        .bool(c"ignorePunctuation", s.ignore_punctuation)
        .str(c"collation", &s.collation)
        .bool(c"numeric", s.numeric)
        .str(c"caseFirst", s.case_first);
    o.obj
}

// Intl.NumberFormat.

#[derive(Clone, Copy, PartialEq, Debug)]
enum Grouping {
    Auto,
    Always,
    Min2,
    Off,
}

struct NumberState {
    locale: String,
    numbering_system: String,
    style: &'static str,
    currency: Option<String>,
    currency_display: &'static str,
    currency_sign: &'static str,
    min_integer: i32,
    fraction: (i32, i32),
    significant: Option<(i32, i32)>,
    grouping: Grouping,
    notation: &'static str,
    compact_display: &'static str,
    sign_display: &'static str,
    format: Rc<icu::NumberFormat>,
}

impl NumberState {
    /// The number to format and whether to show its sign, per `signDisplay`.
    fn signed(&self, x: f64) -> (f64, bool) {
        let zero = x == 0.0 || x.is_nan();
        match self.sign_display {
            "always" => (x, true),
            "exceptZero" => (if zero { x.abs() } else { x }, !zero),
            "never" => (x.abs(), false),
            "negative" => (if zero { x.abs() } else { x }, false),
            _ => (x, false),
        }
    }

    fn format(&self, x: f64) -> Option<Vec<u16>> {
        let (x, sign) = self.signed(x);
        if self.sign_display != "auto" {
            self.format.set(icu::UNUM_SIGN_ALWAYS_SHOWN, sign as i32);
        }
        self.format.format(x)
    }

    fn format_decimal(&self, digits: &str) -> Option<Vec<u16>> {
        let negative = digits.starts_with('-');
        let zero = digits.trim_start_matches('-').bytes().all(|b| b == b'0');
        let digits = match self.sign_display {
            "never" => digits.trim_start_matches('-'),
            _ => digits,
        };
        if self.sign_display != "auto" {
            let sign = matches!(self.sign_display, "always")
                || (self.sign_display == "exceptZero" && !zero);
            self.format
                .set(icu::UNUM_SIGN_ALWAYS_SHOWN, (sign && !negative) as i32);
        }
        self.format.format_decimal(digits)
    }
}

/// The ISO 4217 code of a well-formed currency option.
fn currency_code(s: &str) -> Option<String> {
    (s.len() == 3 && s.bytes().all(|b| b.is_ascii_alphabetic())).then(|| s.to_ascii_uppercase())
}

unsafe fn number_state(
    ctx: *mut q::JSContext,
    locales: q::JSValue,
    options: q::JSValue,
) -> Result<Rc<NumberState>> {
    let key = simple_call_key(ctx, locales, options, "n")?;
    if let Some(state) = key.as_deref().and_then(cache_get) {
        return Ok(state);
    }
    let requested = locale_list(ctx, locales)?;
    let options = options_object(ctx, options)?;
    string_option(ctx, options, c"localeMatcher", &["lookup", "best fit"])?;
    let numbering_system = type_option(ctx, options, c"numberingSystem")?;
    let (base, requested_keywords) = lookup(&requested);

    let style = string_option(
        ctx,
        options,
        c"style",
        &["decimal", "percent", "currency", "unit"],
    )?
    .unwrap_or("decimal");
    let currency = match any_string_option(ctx, options, c"currency")? {
        Some(c) => match currency_code(&c) {
            Some(c) => Some(c),
            None => return throw_range(ctx, &format!("invalid currency code: {c:?}")),
        },
        None => None,
    };
    let currency_display = string_option(
        ctx,
        options,
        c"currencyDisplay",
        &["code", "symbol", "narrowSymbol", "name"],
    )?
    .unwrap_or("symbol");
    let currency_sign = string_option(ctx, options, c"currencySign", &["standard", "accounting"])?
        .unwrap_or("standard");
    let unit = any_string_option(ctx, options, c"unit")?;
    string_option(ctx, options, c"unitDisplay", &["short", "narrow", "long"])?;
    if style == "currency" && currency.is_none() {
        return throw_type(ctx, "currency style requires a currency");
    }
    if style == "unit" {
        return throw_range(ctx, &format!("unit style is not supported (unit {unit:?})"));
    }
    let notation = string_option(
        ctx,
        options,
        c"notation",
        &["standard", "scientific", "engineering", "compact"],
    )?
    .unwrap_or("standard");

    let min_integer = number_option(ctx, options, c"minimumIntegerDigits", 1, 21)?.unwrap_or(1);
    let mnfd = number_option(ctx, options, c"minimumFractionDigits", 0, 100)?;
    let mxfd = number_option(ctx, options, c"maximumFractionDigits", 0, 100)?;
    let mnsd = number_option(ctx, options, c"minimumSignificantDigits", 1, 21)?;
    let mxsd = number_option(ctx, options, c"maximumSignificantDigits", 1, 21)?;
    let compact_display =
        string_option(ctx, options, c"compactDisplay", &["short", "long"])?.unwrap_or("short");
    let grouping = match option_value(ctx, options, c"useGrouping")? {
        None => {
            if notation == "compact" {
                Grouping::Min2
            } else {
                Grouping::Auto
            }
        }
        Some(v) if v.tag() == q::JS_TAG_BOOL => {
            if v.int() != 0 {
                Grouping::Always
            } else {
                Grouping::Off
            }
        }
        Some(v) => {
            let s = to_string(ctx, v);
            q::JS_FreeValue(ctx, v);
            match s?.as_str() {
                "always" => Grouping::Always,
                "auto" | "true" | "false" => Grouping::Auto,
                "min2" => Grouping::Min2,
                "" => Grouping::Off,
                s => {
                    return throw_range(ctx, &format!("invalid value {s:?} for option useGrouping"))
                }
            }
        }
    };
    let sign_display = string_option(
        ctx,
        options,
        c"signDisplay",
        &["auto", "never", "always", "exceptZero", "negative"],
    )?
    .unwrap_or("auto");

    let mut keywords: Vec<(&str, &str)> = Vec::new();
    let nu = numbering_system
        .as_deref()
        .or_else(|| keyword(&requested_keywords, "nu"));
    if let Some(nu) = nu {
        keywords.push(("nu", nu));
    }
    let locale = Locale::new(&base, &keywords);
    let icu_style = match (
        style,
        notation,
        currency_display,
        currency_sign,
        compact_display,
    ) {
        ("percent", ..) => icu::UNUM_PERCENT,
        ("currency", _, "code", ..) => icu::UNUM_CURRENCY_ISO,
        ("currency", _, "name", ..) => icu::UNUM_CURRENCY_PLURAL,
        ("currency", _, _, "accounting", _) => icu::UNUM_CURRENCY_ACCOUNTING,
        ("currency", ..) => icu::UNUM_CURRENCY,
        (_, "scientific" | "engineering", ..) => icu::UNUM_SCIENTIFIC,
        (_, "compact", _, _, "long") => icu::UNUM_DECIMAL_COMPACT_LONG,
        (_, "compact", ..) => icu::UNUM_DECIMAL_COMPACT_SHORT,
        _ => icu::UNUM_DECIMAL,
    };

    // SetNumberFormatDigitOptions; the currency's digits come from ICU.
    let currency_digits = match &currency {
        Some(c) if style == "currency" => {
            let probe_key = format!("N|{}|digits|{c}", locale.id.to_string_lossy());
            cached(probe_key, || {
                let f = icu::NumberFormat::open(icu::UNUM_CURRENCY, &locale.id)?;
                f.set_currency(c).then_some(f)
            })
            .map(|f| f.get(icu::UNUM_MAX_FRACTION_DIGITS))
            .unwrap_or(2)
        }
        _ => 0,
    };
    let mnfd_default = if style == "currency" && notation == "standard" {
        currency_digits
    } else {
        0
    };
    let mxfd_default = match style {
        "currency" if notation == "standard" => mnfd_default.max(currency_digits),
        "percent" => mnfd_default,
        _ => mnfd_default.max(3),
    };
    let has_sd = mnsd.is_some() || mxsd.is_some();
    let has_fd = mnfd.is_some() || mxfd.is_some();
    let significant = if has_sd {
        let (min, max) = (mnsd.unwrap_or(1), mxsd.unwrap_or(21));
        if min > max {
            return throw_range(
                ctx,
                "minimumSignificantDigits is greater than maximumSignificantDigits",
            );
        }
        Some((min, max))
    } else {
        None
    };
    let compact_rounding = !has_sd && !has_fd && notation == "compact";
    let fraction = match (mnfd, mxfd) {
        (None, None) if compact_rounding => (0, 0),
        (None, None) => (mnfd_default, mxfd_default),
        (Some(min), None) => (min, mxfd_default.max(min)),
        (None, Some(max)) => (mnfd_default.min(max), max),
        (Some(min), Some(max)) if min > max => {
            return throw_range(
                ctx,
                "minimumFractionDigits is greater than maximumFractionDigits",
            )
        }
        (Some(min), Some(max)) => (min, max),
    };

    let cache_key = format!(
        "N|{}|{icu_style}|{currency:?}|{min_integer}|{fraction:?}|{significant:?}|{has_sd}|{compact_rounding}|{grouping:?}|{notation}|{sign_display}",
        locale.id.to_string_lossy()
    );
    let format = cached(cache_key, || {
        let f = icu::NumberFormat::open(icu_style, &locale.id)?;
        if let Some(c) = &currency {
            if style == "currency" && !f.set_currency(c) {
                return None;
            }
        }
        f.set(icu::UNUM_ROUNDING_MODE, icu::UNUM_ROUND_HALFUP);
        f.set(icu::UNUM_MIN_INTEGER_DIGITS, min_integer);
        if notation == "engineering" {
            f.set(icu::UNUM_MAX_INTEGER_DIGITS, 3);
        }
        if let Some((min, max)) = significant {
            f.set(icu::UNUM_SIGNIFICANT_DIGITS_USED, 1);
            f.set(icu::UNUM_MAX_SIGNIFICANT_DIGITS, max);
            f.set(icu::UNUM_MIN_SIGNIFICANT_DIGITS, min);
        } else if !compact_rounding {
            f.set(icu::UNUM_MAX_FRACTION_DIGITS, fraction.1);
            f.set(icu::UNUM_MIN_FRACTION_DIGITS, fraction.0);
        }
        match grouping {
            Grouping::Auto => {}
            Grouping::Always => {
                f.set(icu::UNUM_GROUPING_USED, 1);
                f.set(icu::UNUM_MINIMUM_GROUPING_DIGITS, 1);
            }
            Grouping::Min2 => {
                f.set(icu::UNUM_GROUPING_USED, 1);
                f.set(icu::UNUM_MINIMUM_GROUPING_DIGITS, 2);
            }
            Grouping::Off => f.set(icu::UNUM_GROUPING_USED, 0),
        }
        if sign_display == "always" {
            f.set(icu::UNUM_SIGN_ALWAYS_SHOWN, 1);
        }
        Some(f)
    });
    let Some(format) = format else {
        return throw_range(ctx, "cannot create a number format");
    };
    let state = Rc::new(NumberState {
        locale: locale.tag.clone(),
        numbering_system: nu.unwrap_or("latn").to_owned(),
        style,
        currency: currency.filter(|_| style == "currency"),
        currency_display,
        currency_sign,
        min_integer,
        fraction,
        significant: if compact_rounding {
            Some((1, 2))
        } else {
            significant
        },
        grouping,
        notation,
        compact_display,
        sign_display,
        format,
    });
    if let Some(key) = key {
        cache_put(key, state.clone());
    }
    Ok(state)
}

unsafe fn is_bigint(ctx: *mut q::JSContext, v: q::JSValue) -> bool {
    q::JS_IsBigInt(ctx, v) != 0
}

unsafe fn format_number(
    ctx: *mut q::JSContext,
    state: &NumberState,
    v: q::JSValue,
) -> Result<q::JSValue> {
    if is_bigint(ctx, v) {
        let digits = to_string(ctx, v)?;
        return formatted(ctx, state.format_decimal(&digits));
    }
    let mut x = 0.0;
    if q::JS_ToFloat64(ctx, &mut x, v) < 0 {
        return Err(());
    }
    formatted(ctx, state.format(x))
}

unsafe fn format_number_to_parts(
    ctx: *mut q::JSContext,
    state: &NumberState,
    v: q::JSValue,
) -> Result<q::JSValue> {
    let mut x = 0.0;
    if q::JS_ToFloat64(ctx, &mut x, v) < 0 {
        return Err(());
    }
    let (x, sign) = state.signed(x);
    if state.sign_display != "auto" {
        state.format.set(icu::UNUM_SIGN_ALWAYS_SHOWN, sign as i32);
    }
    let Some((text, mut spans)) = state.format.format_to_parts(x) else {
        return throw_range(ctx, "formatting failed");
    };
    // Outer spans first, so that nested ones (group separators in integers) win.
    spans.sort_unstable_by_key(|&(_, begin, end)| (begin, std::cmp::Reverse(end)));
    let mut types = vec!["literal"; text.len()];
    for (field, begin, end) in spans {
        let kind = match field {
            0 if x.is_nan() => "nan",
            0 if x.is_infinite() => "infinity",
            0 => "integer",
            1 => "fraction",
            2 => "decimal",
            3 => "exponentSeparator",
            4 => "exponentMinusSign",
            5 => "exponentInteger",
            6 => "group",
            7 => "currency",
            8 => "percentSign",
            10 if text.get(begin) == Some(&(b'+' as u16)) => "plusSign",
            10 => "minusSign",
            11 => "unit",
            12 => "compact",
            _ => continue,
        };
        for t in &mut types[begin.min(text.len())..end.min(text.len())] {
            *t = kind;
        }
    }
    Ok(parts_array(ctx, &text, &types))
}

unsafe fn number_resolved_options(ctx: *mut q::JSContext, s: &NumberState) -> q::JSValue {
    let o = ObjectBuilder::new(ctx);
    o.str(c"locale", &s.locale)
        .str(c"numberingSystem", &s.numbering_system)
        .str(c"style", s.style);
    if let Some(c) = &s.currency {
        o.str(c"currency", c)
            .str(c"currencyDisplay", s.currency_display)
            .str(c"currencySign", s.currency_sign);
    }
    o.int(c"minimumIntegerDigits", s.min_integer);
    if s.significant.is_none() || s.notation == "compact" {
        o.int(c"minimumFractionDigits", s.fraction.0)
            .int(c"maximumFractionDigits", s.fraction.1);
    }
    if let Some((min, max)) = s.significant {
        o.int(c"minimumSignificantDigits", min)
            .int(c"maximumSignificantDigits", max);
    }
    match s.grouping {
        Grouping::Off => {
            o.bool(c"useGrouping", false);
        }
        Grouping::Auto => {
            o.str(c"useGrouping", "auto");
        }
        Grouping::Always => {
            o.str(c"useGrouping", "always");
        }
        Grouping::Min2 => {
            o.str(c"useGrouping", "min2");
        }
    }
    o.str(c"notation", s.notation);
    if s.notation == "compact" {
        o.str(c"compactDisplay", s.compact_display);
    }
    o.str(c"signDisplay", s.sign_display)
        .int(c"roundingIncrement", 1)
        .str(c"roundingMode", "halfExpand")
        .str(c"roundingPriority", "auto")
        .str(c"trailingZeroDisplay", "auto");
    o.obj
}

// Intl.DateTimeFormat.

/// ToDateTimeOptions's `required` and `defaults`.
#[derive(Clone, Copy, PartialEq)]
enum Fields {
    Date,
    Time,
    Any,
}

const STYLES: [&str; 4] = ["full", "long", "medium", "short"];

struct DateState {
    locale: String,
    calendar: String,
    numbering_system: String,
    time_zone: String,
    date_style: Option<&'static str>,
    time_style: Option<&'static str>,
    pattern: String,
    format: Rc<icu::DateFormat>,
}

/// CLDR's preferred and allowed hour characters for a maximized locale ID.
fn hour_cycles(maximized: &str) -> &'static str {
    let mut parts = maximized.split('_');
    let language = parts.next().unwrap_or("");
    let region = parts
        .filter(|p| p.len() == 2 || p.len() == 3 && p.bytes().all(|b| b.is_ascii_digit()))
        .next_back()
        .unwrap_or("001");
    let find = |key: &str| {
        patterns::HOUR_CYCLES.split(',').find_map(|entry| {
            entry
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
        })
    };
    find(&format!("{language}_{region}"))
        .or_else(|| find(region))
        .or_else(|| find("001"))
        .unwrap_or("H")
}

/// The pattern generator of a locale: from the table if listed, else ICU's style patterns and
/// root's availableFormats.
fn generator(locale: &Locale) -> Option<Rc<Generator>> {
    cached(format!("G|{}", locale.id.to_string_lossy()), || {
        let default_hour = hour_cycles(&locale.maximized).chars().next().unwrap_or('H');
        let decimal = icu::NumberFormat::open(icu::UNUM_DECIMAL, &locale.id)
            .map(|f| String::from_utf16_lossy(&f.decimal_separator()))
            .unwrap_or_else(|| ".".to_owned());
        if let Some((_, data)) = patterns::LOCALES
            .iter()
            .find(|(id, _)| *id == locale.maximized)
        {
            return Some(Generator::new(
                &LocaleData::parse(data),
                true,
                default_hour,
                &decimal,
            ));
        }
        let style = |date: i32, time: i32| {
            icu::DateFormat::open(&locale.id, None, None, (date, time))
                .and_then(|f| f.pattern())
                .map(|p| String::from_utf16_lossy(&p))
                .unwrap_or_default()
        };
        let styles: Vec<String> = (0..4)
            .map(|i| style(icu::UDAT_NONE, i))
            .chain((0..4).map(|i| style(i, icu::UDAT_NONE)))
            .collect();
        let both = style(3, 3);
        let glue = match (both.find(&styles[7]), styles[3].is_empty()) {
            (Some(pos), false) => {
                let glue = format!("{}{{1}}{}", &both[..pos], &both[pos + styles[7].len()..]);
                glue.replacen(&styles[3], "{0}", 1)
            }
            _ => "{1} {0}".to_owned(),
        };
        let glue = if glue.contains("{0}") && glue.contains("{1}") {
            glue
        } else {
            "{1} {0}".to_owned()
        };
        let mut data = LocaleData::parse(patterns::ROOT);
        data.styles = std::array::from_fn(|i| styles[i].as_str());
        data.glue = [glue.as_str(); 4];
        Some(Generator::new(&data, false, default_hour, &decimal))
    })
}

fn best_pattern(locale: &Locale, generator: &Generator, skeleton: &str) -> String {
    let key = format!("P|{}|{skeleton}", locale.id.to_string_lossy());
    cached(key, || Some(generator.best_pattern(skeleton)))
        .map(|p| String::clone(&p))
        .unwrap_or_default()
}

/// Replaces the hour fields of a pattern with `hc`, like V8's ReplaceHourCycleInPattern.
fn replace_hour_cycle(pattern: &str, hc: char) -> String {
    let mut quoted = false;
    pattern
        .chars()
        .map(|c| {
            if c == '\'' {
                quoted = !quoted;
            }
            if !quoted && matches!(c, 'h' | 'H' | 'k' | 'K') {
                hc
            } else {
                c
            }
        })
        .collect()
}

fn is_12_hour(c: char) -> bool {
    c == 'h' || c == 'K'
}

/// The canonical name and ICU ID of a `timeZone` option.
unsafe fn time_zone(
    ctx: *mut q::JSContext,
    options: q::JSValue,
) -> Result<(String, Option<Vec<u16>>)> {
    let utc = |name: &str| {
        matches!(
            name.to_ascii_uppercase().as_str(),
            "UTC"
                | "ETC/UTC"
                | "ETC/GMT"
                | "GMT"
                | "ETC/UCT"
                | "UCT"
                | "ETC/ZULU"
                | "ZULU"
                | "ETC/UNIVERSAL"
                | "UNIVERSAL"
                | "ETC/GMT0"
                | "GMT0"
                | "ETC/GREENWICH"
                | "GREENWICH"
        )
    };
    match any_string_option(ctx, options, c"timeZone")? {
        None => {
            let name = icu::system_time_zone();
            let units = name.encode_utf16().collect();
            Ok((
                if utc(&name) { "UTC".to_owned() } else { name },
                Some(units),
            ))
        }
        Some(name) if utc(&name) => Ok(("UTC".to_owned(), Some("UTC".encode_utf16().collect()))),
        Some(name) => match icu::canonical_time_zone(&name) {
            Some(id) => {
                let units = id.encode_utf16().collect();
                Ok((if utc(&id) { "UTC".to_owned() } else { id }, Some(units)))
            }
            None => throw_range(ctx, &format!("invalid time zone: {name}")),
        },
    }
}

/// InitializeDateTimeFormat, with ToDateTimeOptions(`required`, `defaults`).
unsafe fn date_state(
    ctx: *mut q::JSContext,
    locales: q::JSValue,
    options: q::JSValue,
    required: Fields,
    defaults: Fields,
) -> Result<Rc<DateState>> {
    let kind = format!("d{}{}", required as u8, defaults as u8);
    let key = simple_call_key(ctx, locales, options, &kind)?
        .map(|key| key + "|" + &icu::system_time_zone());
    if let Some(state) = key.as_deref().and_then(cache_get) {
        return Ok(state);
    }
    let requested = locale_list(ctx, locales)?;
    let options = options_object(ctx, options)?;
    string_option(ctx, options, c"localeMatcher", &["lookup", "best fit"])?;
    let calendar = type_option(ctx, options, c"calendar")?;
    let numbering_system = type_option(ctx, options, c"numberingSystem")?;
    let hour12 = bool_option(ctx, options, c"hour12")?;
    let hour_cycle = string_option(ctx, options, c"hourCycle", &["h11", "h12", "h23", "h24"])?;
    let (base, requested_keywords) = lookup(&requested);
    let (tz_name, tz_id) = time_zone(ctx, options)?;

    let narrow_short_long = &["narrow", "short", "long"];
    let numeric = &["2-digit", "numeric"];
    let weekday = string_option(ctx, options, c"weekday", narrow_short_long)?;
    let era = string_option(ctx, options, c"era", narrow_short_long)?;
    let mut year = string_option(ctx, options, c"year", numeric)?;
    let mut month = string_option(
        ctx,
        options,
        c"month",
        &["2-digit", "numeric", "narrow", "short", "long"],
    )?;
    let mut day = string_option(ctx, options, c"day", numeric)?;
    let day_period = string_option(ctx, options, c"dayPeriod", narrow_short_long)?;
    let mut hour = string_option(ctx, options, c"hour", numeric)?;
    let mut minute = string_option(ctx, options, c"minute", numeric)?;
    let mut second = string_option(ctx, options, c"second", numeric)?;
    let fractional = number_option(ctx, options, c"fractionalSecondDigits", 1, 3)?;
    let time_zone_name = string_option(
        ctx,
        options,
        c"timeZoneName",
        &[
            "short",
            "long",
            "shortOffset",
            "longOffset",
            "shortGeneric",
            "longGeneric",
        ],
    )?;
    string_option(ctx, options, c"formatMatcher", &["basic", "best fit"])?;
    let date_style = string_option(ctx, options, c"dateStyle", &STYLES)?;
    let time_style = string_option(ctx, options, c"timeStyle", &STYLES)?;

    let has_date = weekday.is_some() || year.is_some() || month.is_some() || day.is_some();
    let has_time = day_period.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || fractional.is_some();
    if date_style.is_some() || time_style.is_some() {
        if has_date || has_time || era.is_some() || time_zone_name.is_some() {
            return throw_type(
                ctx,
                "dateStyle and timeStyle cannot be combined with other date and time options",
            );
        }
        if required == Fields::Date && time_style.is_some() {
            return throw_type(ctx, "timeStyle is not allowed here");
        }
        if required == Fields::Time && date_style.is_some() {
            return throw_type(ctx, "dateStyle is not allowed here");
        }
    } else {
        let need_defaults = !(matches!(required, Fields::Date | Fields::Any) && has_date)
            && !(matches!(required, Fields::Time | Fields::Any) && has_time);
        if need_defaults && matches!(defaults, Fields::Date | Fields::Any) {
            (year, month, day) = (Some("numeric"), Some("numeric"), Some("numeric"));
        }
        if need_defaults && matches!(defaults, Fields::Time | Fields::Any) {
            (hour, minute, second) = (Some("numeric"), Some("numeric"), Some("numeric"));
        }
    }

    let mut keywords: Vec<(&str, &str)> = Vec::new();
    let ca = calendar
        .as_deref()
        .or_else(|| keyword(&requested_keywords, "ca"));
    if let Some(ca) = ca {
        keywords.push(("ca", ca));
    }
    let nu = numbering_system
        .as_deref()
        .or_else(|| keyword(&requested_keywords, "nu"));
    if let Some(nu) = nu {
        keywords.push(("nu", nu));
    }
    let hc_keyword =
        keyword(&requested_keywords, "hc").filter(|hc| ["h11", "h12", "h23", "h24"].contains(hc));
    if hour12.is_none() && hour_cycle.is_none() {
        if let Some(hc) = hc_keyword {
            keywords.push(("hc", hc));
        }
    }
    let locale = Locale::new(&base, &keywords);
    let cycles: Vec<char> = hour_cycles(&locale.maximized).chars().collect();
    let default_hour = cycles.first().copied().unwrap_or('H');
    let hc_char = |hc: &str| match hc {
        "h11" => 'K',
        "h12" => 'h',
        "h24" => 'k',
        _ => 'H',
    };
    let explicit_hour = match (hour12, hour_cycle.or(hc_keyword)) {
        (Some(true), _) => Some(
            cycles
                .iter()
                .copied()
                .find(|&c| is_12_hour(c))
                .unwrap_or('h'),
        ),
        (Some(false), _) => Some(
            cycles
                .iter()
                .copied()
                .find(|&c| !is_12_hour(c))
                .unwrap_or('H'),
        ),
        (None, Some(hc)) => Some(hc_char(hc)),
        (None, None) => None,
    };
    let Some(generator) = generator(&locale) else {
        return throw_range(ctx, "cannot create a date format");
    };

    let pattern = if date_style.is_some() || time_style.is_some() {
        let index = |s: Option<&str>| {
            s.and_then(|s| STYLES.iter().position(|&x| x == s))
                .map_or(icu::UDAT_NONE, |i| i as i32)
        };
        let style_pattern = |date: i32, time: i32| {
            icu::DateFormat::open(&locale.id, None, None, (date, time))
                .and_then(|f| f.pattern())
                .map(|p| String::from_utf16_lossy(&p))
        };
        let (ds, ts) = (index(date_style), index(time_style));
        let Some(mut pattern) = style_pattern(ds, ts) else {
            return throw_range(ctx, "cannot create a date format");
        };
        if let (Some(hc), true) = (explicit_hour, ts != icu::UDAT_NONE) {
            let time = style_pattern(icu::UDAT_NONE, ts).unwrap_or_default();
            let time_hour = time.chars().find(|c| matches!(c, 'h' | 'H' | 'k' | 'K'));
            if time_hour.is_some_and(|c| is_12_hour(c) != is_12_hour(hc)) {
                let skeleton: String = dtpg::pattern_skeleton(&time)
                    .fields()
                    .filter(|&(c, _)| !matches!(c, 'a' | 'b' | 'B'))
                    .flat_map(|(c, n)| {
                        std::iter::repeat_n(
                            if matches!(c, 'h' | 'H' | 'k' | 'K') {
                                hc
                            } else {
                                c
                            },
                            n,
                        )
                    })
                    .collect();
                let new_time = best_pattern(&locale, &generator, &skeleton);
                pattern = match (ds != icu::UDAT_NONE, style_pattern(ds, icu::UDAT_NONE)) {
                    (true, Some(date)) => generator
                        .glue(ds as usize)
                        .replace("{1}", &date)
                        .replace("{0}", &new_time),
                    _ => new_time,
                };
            }
            pattern = replace_hour_cycle(&pattern, hc);
        }
        pattern
    } else {
        let hour_char = explicit_hour.unwrap_or(default_hour);
        let mut skeleton = String::new();
        let mut push = |s: Option<&str>, values: &[(&str, &str)]| {
            if let Some((_, v)) = values.iter().find(|(k, _)| Some(*k) == s) {
                skeleton.push_str(v);
            }
        };
        push(
            weekday,
            &[("narrow", "EEEEE"), ("short", "E"), ("long", "EEEE")],
        );
        push(
            era,
            &[("narrow", "GGGGG"), ("short", "G"), ("long", "GGGG")],
        );
        push(year, &[("2-digit", "yy"), ("numeric", "y")]);
        push(
            month,
            &[
                ("2-digit", "MM"),
                ("numeric", "M"),
                ("narrow", "MMMMM"),
                ("short", "MMM"),
                ("long", "MMMM"),
            ],
        );
        push(day, &[("2-digit", "dd"), ("numeric", "d")]);
        if hour.is_some() && is_12_hour(hour_char) {
            push(
                day_period,
                &[("narrow", "BBBBB"), ("short", "B"), ("long", "BBBB")],
            );
        }
        let hour_fields = [hour_char.to_string(), hour_char.to_string().repeat(2)];
        push(
            hour,
            &[("numeric", &hour_fields[0]), ("2-digit", &hour_fields[1])],
        );
        push(minute, &[("2-digit", "mm"), ("numeric", "m")]);
        push(second, &[("2-digit", "ss"), ("numeric", "s")]);
        push(
            fractional.map(|n| ["S", "SS", "SSS"][n as usize - 1]),
            &[("S", "S"), ("SS", "SS"), ("SSS", "SSS")],
        );
        push(
            time_zone_name,
            &[
                ("short", "z"),
                ("long", "zzzz"),
                ("shortOffset", "O"),
                ("longOffset", "OOOO"),
                ("shortGeneric", "v"),
                ("longGeneric", "vvvv"),
            ],
        );
        let pattern = best_pattern(&locale, &generator, &skeleton);
        match explicit_hour {
            Some(hc) if hour.is_some() => replace_hour_cycle(&pattern, hc),
            _ => pattern,
        }
    };

    let units: Vec<u16> = pattern.encode_utf16().collect();
    let tz_key = tz_id
        .as_ref()
        .map(|t| String::from_utf16_lossy(t))
        .unwrap_or_default();
    let cache_key = format!("D|{}|{tz_key}|{pattern}", locale.id.to_string_lossy());
    let format = cached(cache_key, || {
        let f = icu::DateFormat::open(&locale.id, tz_id.as_deref(), Some(&units), (0, 0))?;
        f.make_proleptic();
        Some(f)
    });
    let Some(format) = format else {
        return throw_range(ctx, "cannot create a date format");
    };
    let state = Rc::new(DateState {
        locale: locale.tag.clone(),
        calendar: match ca {
            Some("gregorian") | None => "gregory".to_owned(),
            Some(ca) => ca.to_owned(),
        },
        numbering_system: nu.unwrap_or("latn").to_owned(),
        time_zone: tz_name,
        date_style,
        time_style,
        pattern,
        format,
    });
    if let Some(key) = key {
        cache_put(key, state.clone());
    }
    Ok(state)
}

/// TimeClip of a `format` argument; `undefined` is now.
unsafe fn time_value(ctx: *mut q::JSContext, v: q::JSValue) -> Result<f64> {
    let t = if is_undefined(v) {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    } else {
        let mut t = 0.0;
        if q::JS_ToFloat64(ctx, &mut t, v) < 0 {
            return Err(());
        }
        t
    };
    if !t.is_finite() || t.abs() > 8.64e15 {
        return throw_range(ctx, "invalid time value");
    }
    Ok(t.trunc() + 0.0)
}

unsafe fn format_date_to_parts(
    ctx: *mut q::JSContext,
    state: &DateState,
    t: f64,
) -> Result<q::JSValue> {
    let Some((text, spans)) = state.format.format_to_parts(t) else {
        return throw_range(ctx, "formatting failed");
    };
    let mut types = vec!["literal"; text.len()];
    for (field, begin, end) in spans {
        let kind = match field {
            0 => "era",
            1 | 18 | 20 | 30 => "year",
            2 | 26 => "month",
            3 => "day",
            4 | 5 | 15 | 16 => "hour",
            6 => "minute",
            7 => "second",
            8 => "fractionalSecond",
            9 | 19 | 25 => "weekday",
            14 | 35 | 36 => "dayPeriod",
            17 | 23 | 24 | 29 | 31 | 32 | 33 => "timeZoneName",
            34 => "relatedYear",
            _ => continue,
        };
        for t in &mut types[begin.min(text.len())..end.min(text.len())] {
            *t = kind;
        }
    }
    Ok(parts_array(ctx, &text, &types))
}

unsafe fn date_resolved_options(ctx: *mut q::JSContext, s: &DateState) -> q::JSValue {
    let o = ObjectBuilder::new(ctx);
    o.str(c"locale", &s.locale)
        .str(c"calendar", &s.calendar)
        .str(c"numberingSystem", &s.numbering_system)
        .str(c"timeZone", &s.time_zone);
    let skeleton = dtpg::pattern_skeleton(&s.pattern);
    let fields: Vec<(char, usize)> = skeleton.fields().collect();
    if let Some(&(c, _)) = fields
        .iter()
        .find(|(c, _)| matches!(c, 'h' | 'H' | 'k' | 'K'))
    {
        let hc = match c {
            'K' => "h11",
            'h' => "h12",
            'k' => "h24",
            _ => "h23",
        };
        o.str(c"hourCycle", hc).bool(c"hour12", is_12_hour(c));
    }
    if s.date_style.is_some() || s.time_style.is_some() {
        if let Some(ds) = s.date_style {
            o.str(c"dateStyle", ds);
        }
        if let Some(ts) = s.time_style {
            o.str(c"timeStyle", ts);
        }
        return o.obj;
    }
    let text = |n: usize| match n {
        4 => "long",
        5 => "narrow",
        _ => "short",
    };
    let digits = |n: usize| if n == 2 { "2-digit" } else { "numeric" };
    for (c, n) in fields {
        match c {
            'E' | 'c' | 'e' => {
                o.str(c"weekday", text(n));
            }
            'G' => {
                o.str(c"era", text(n));
            }
            'y' | 'Y' | 'u' | 'r' | 'U' => {
                o.str(c"year", digits(n));
            }
            'M' | 'L' => {
                o.str(c"month", if n <= 2 { digits(n) } else { text(n) });
            }
            'd' => {
                o.str(c"day", digits(n));
            }
            'B' | 'b' => {
                o.str(c"dayPeriod", text(n));
            }
            'h' | 'H' | 'k' | 'K' => {
                o.str(c"hour", digits(n));
            }
            'm' => {
                o.str(c"minute", digits(n));
            }
            's' => {
                o.str(c"second", digits(n));
            }
            'S' => {
                o.int(c"fractionalSecondDigits", n as i32);
            }
            'z' => {
                o.str(c"timeZoneName", if n >= 4 { "long" } else { "short" });
            }
            'O' => {
                o.str(
                    c"timeZoneName",
                    if n >= 4 { "longOffset" } else { "shortOffset" },
                );
            }
            'v' | 'V' => {
                o.str(
                    c"timeZoneName",
                    if n >= 4 {
                        "longGeneric"
                    } else {
                        "shortGeneric"
                    },
                );
            }
            _ => {}
        }
    }
    o.obj
}

// Native functions.

unsafe extern "C" fn construct(
    ctx: *mut q::JSContext,
    new_target: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    magic: c_int,
) -> q::JSValue {
    let kind = magic as usize;
    let (locales, options) = (arg(argc, argv, 0), arg(argc, argv, 1));
    let state = match kind {
        COLLATOR => collator_state(ctx, locales, options).map(State::Collator),
        NUMBER_FORMAT => number_state(ctx, locales, options).map(State::Number),
        _ => date_state(ctx, locales, options, Fields::Any, Fields::Date).map(State::Date),
    };
    let Ok(state) = state else {
        return q::JS_EXCEPTION;
    };
    let id = CLASS_IDS.get().unwrap()[kind];
    let mut proto = q::JS_UNDEFINED;
    if new_target.tag() == q::JS_TAG_OBJECT {
        proto = q::JS_GetPropertyStr(ctx, new_target, c"prototype".as_ptr());
        if proto.tag() == q::JS_TAG_EXCEPTION {
            return q::JS_EXCEPTION;
        }
    }
    if proto.tag() != q::JS_TAG_OBJECT {
        q::JS_FreeValue(ctx, proto);
        proto = q::JS_GetClassProto(ctx, id);
    }
    let obj = q::JS_NewObjectProtoClass(ctx, proto, id);
    q::JS_FreeValue(ctx, proto);
    if obj.tag() == q::JS_TAG_EXCEPTION {
        return obj;
    }
    q::JS_SetOpaque(
        obj,
        Box::into_raw(Box::new(IntlObject {
            state,
            bound: Cell::new(q::JS_UNDEFINED),
        })) as *mut _,
    );
    obj
}

unsafe fn this_object<'a>(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    kind: usize,
) -> Result<&'a IntlObject> {
    match intl_object(this, kind) {
        Some(obj) => Ok(obj),
        None => throw_type(
            ctx,
            &format!("not an Intl.{} object", NAMES[kind].to_string_lossy()),
        ),
    }
}

unsafe extern "C" fn resolved_options(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    _argc: c_int,
    _argv: *mut q::JSValue,
    magic: c_int,
) -> q::JSValue {
    ret(
        this_object(ctx, this, magic as usize).map(|obj| match &obj.state {
            State::Collator(s) => collator_resolved_options(ctx, s),
            State::Number(s) => number_resolved_options(ctx, s),
            State::Date(s) => date_resolved_options(ctx, s),
        }),
    )
}

/// The `format` and `compare` getters: a function bound to the object, created once.
unsafe extern "C" fn get_bound(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    _argc: c_int,
    _argv: *mut q::JSValue,
    magic: c_int,
) -> q::JSValue {
    let obj = match this_object(ctx, this, magic as usize) {
        Ok(obj) => obj,
        Err(()) => return q::JS_EXCEPTION,
    };
    if is_undefined(obj.bound.get()) {
        let length = if magic as usize == COLLATOR { 2 } else { 1 };
        let mut data = this;
        let f = q::JS_NewCFunctionData(ctx, Some(call_bound), length, magic, 1, &mut data);
        if f.tag() == q::JS_TAG_EXCEPTION {
            return f;
        }
        obj.bound.set(f);
    }
    q::JS_DupValue(ctx, obj.bound.get())
}

unsafe extern "C" fn call_bound(
    ctx: *mut q::JSContext,
    _this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    magic: c_int,
    data: *mut q::JSValue,
) -> q::JSValue {
    let Some(obj) = intl_object(*data, magic as usize) else {
        return q::JS_EXCEPTION;
    };
    ret(match &obj.state {
        State::Collator(s) => compare(ctx, &s.clone(), arg(argc, argv, 0), arg(argc, argv, 1)),
        State::Number(s) => format_number(ctx, &s.clone(), arg(argc, argv, 0)),
        State::Date(s) => {
            let s = s.clone();
            time_value(ctx, arg(argc, argv, 0)).and_then(|t| formatted(ctx, s.format.format(t)))
        }
    })
}

unsafe extern "C" fn format_to_parts(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    magic: c_int,
) -> q::JSValue {
    let state = match this_object(ctx, this, magic as usize) {
        Ok(obj) => match &obj.state {
            State::Number(s) => Ok(State::Number(s.clone())),
            State::Date(s) => Ok(State::Date(s.clone())),
            State::Collator(_) => Err(()),
        },
        Err(()) => return q::JS_EXCEPTION,
    };
    ret(match state {
        Ok(State::Number(s)) => format_number_to_parts(ctx, &s, arg(argc, argv, 0)),
        Ok(State::Date(s)) => {
            time_value(ctx, arg(argc, argv, 0)).and_then(|t| format_date_to_parts(ctx, &s, t))
        }
        _ => Err(()),
    })
}

unsafe extern "C" fn supported_locales_of(
    ctx: *mut q::JSContext,
    _this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    _magic: c_int,
) -> q::JSValue {
    let r = (|| {
        let requested = locale_list(ctx, arg(argc, argv, 0))?;
        let options = options_object(ctx, arg(argc, argv, 1))?;
        string_option(ctx, options, c"localeMatcher", &["lookup", "best fit"])?;
        let supported: Vec<String> = requested
            .into_iter()
            .filter(|tag| best_available(&split_extension(tag).0).is_some())
            .collect();
        Ok(new_array(
            ctx,
            supported.iter().map(|tag| new_str(ctx, tag)),
        ))
    })();
    ret(r)
}

unsafe extern "C" fn get_canonical_locales(
    ctx: *mut q::JSContext,
    _this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    ret(locale_list(ctx, arg(argc, argv, 0))
        .map(|tags| new_array(ctx, tags.iter().map(|tag| new_str(ctx, tag)))))
}

unsafe extern "C" fn number_to_locale_string(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let value = match this.tag() {
        q::JS_TAG_INT | q::JS_TAG_FLOAT64 => this,
        q::JS_TAG_OBJECT if q::JS_GetClassID(this) == CLASS_NUMBER => {
            q::JS_GetPrimitiveObjectData(ctx, this)
        }
        _ => {
            return ret(throw_type(
                ctx,
                "Number.prototype.toLocaleString called on an incompatible receiver",
            ))
        }
    };
    ret(number_state(ctx, arg(argc, argv, 0), arg(argc, argv, 1))
        .and_then(|s| format_number(ctx, &s, value)))
}

unsafe extern "C" fn date_to_locale_string(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    magic: c_int,
) -> q::JSValue {
    if this.tag() != q::JS_TAG_OBJECT || q::JS_GetClassID(this) != CLASS_DATE {
        return ret(throw_type(ctx, "not a Date object"));
    }
    let v = q::JS_GetPrimitiveObjectData(ctx, this);
    let mut t = f64::NAN;
    q::JS_ToFloat64(ctx, &mut t, v);
    let (required, defaults) = match magic {
        1 => (Fields::Date, Fields::Date),
        2 => (Fields::Time, Fields::Time),
        _ => (Fields::Any, Fields::Any),
    };
    let state = date_state(
        ctx,
        arg(argc, argv, 0),
        arg(argc, argv, 1),
        required,
        defaults,
    );
    ret(state.and_then(|s| {
        if t.is_nan() {
            Ok(new_str(ctx, "Invalid Date"))
        } else {
            formatted(ctx, s.format.format(t))
        }
    }))
}

unsafe fn require_object_coercible(ctx: *mut q::JSContext, this: q::JSValue) -> Result<()> {
    if this.tag() == q::JS_TAG_NULL || is_undefined(this) {
        return throw_type(ctx, "String.prototype method called on null or undefined");
    }
    Ok(())
}

unsafe extern "C" fn string_locale_compare(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
) -> q::JSValue {
    let r = (|| {
        require_object_coercible(ctx, this)?;
        let s = q::JS_ToString(ctx, this);
        if s.tag() == q::JS_TAG_EXCEPTION {
            return Err(());
        }
        let that = q::JS_ToString(ctx, arg(argc, argv, 0));
        if that.tag() == q::JS_TAG_EXCEPTION {
            q::JS_FreeValue(ctx, s);
            return Err(());
        }
        let r = collator_state(ctx, arg(argc, argv, 1), arg(argc, argv, 2))
            .and_then(|state| compare(ctx, &state, s, that));
        q::JS_FreeValue(ctx, s);
        q::JS_FreeValue(ctx, that);
        r
    })();
    ret(r)
}

unsafe extern "C" fn string_to_locale_case(
    ctx: *mut q::JSContext,
    this: q::JSValue,
    argc: c_int,
    argv: *mut q::JSValue,
    magic: c_int,
) -> q::JSValue {
    let r = (|| {
        require_object_coercible(ctx, this)?;
        let requested = locale_list(ctx, arg(argc, argv, 0))?;
        let tag = requested.into_iter().next().unwrap_or_else(default_locale);
        let language = CString::new(
            split_extension(&tag)
                .0
                .split('-')
                .next()
                .unwrap_or("")
                .to_owned(),
        )
        .unwrap_or_default();
        let units = with_units(ctx, this, |s| {
            if magic == 0 {
                icu::to_upper(s, &language)
            } else {
                icu::to_lower(s, &language)
            }
        })?;
        formatted(ctx, units)
    })();
    ret(r)
}

// Installation.

unsafe fn define_function(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    name: &CStr,
    f: q::JSCFunction,
    length: c_int,
) {
    let f = q::JS_NewCFunction2(ctx, f, name.as_ptr(), length, q::JS_CFUNC_generic, 0);
    q::JS_DefinePropertyValueStr(ctx, obj, name.as_ptr(), f, METHOD);
}

unsafe fn define_magic_function(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    name: &CStr,
    f: q::JSCFunctionMagic,
    length: c_int,
    magic: usize,
) {
    let f = q::JS_NewCFunctionMagic(
        ctx,
        f,
        name.as_ptr(),
        length,
        q::JS_CFUNC_generic_magic,
        magic as c_int,
    );
    q::JS_DefinePropertyValueStr(ctx, obj, name.as_ptr(), f, METHOD);
}

unsafe fn define_to_string_tag(
    ctx: *mut q::JSContext,
    obj: q::JSValue,
    atom: q::JSAtom,
    tag: &CStr,
) {
    q::JS_DefinePropertyValue(
        ctx,
        obj,
        atom,
        q::JS_NewString(ctx, tag.as_ptr()),
        q::JS_PROP_CONFIGURABLE as c_int,
    );
}

/// Defines `Intl` on the global object of `ctx` and replaces the locale-sensitive methods of
/// `Number`, `String` and `Date`.
pub(super) unsafe fn install(ctx: *mut q::JSContext) {
    let ids = class_ids(q::JS_GetRuntime(ctx));
    let global = q::JS_GetGlobalObject(ctx);
    let symbol = q::JS_GetPropertyStr(ctx, global, c"Symbol".as_ptr());
    let tag = q::JS_GetPropertyStr(ctx, symbol, c"toStringTag".as_ptr());
    let to_string_tag = q::JS_ValueToAtom(ctx, tag);
    q::JS_FreeValue(ctx, tag);
    q::JS_FreeValue(ctx, symbol);

    let intl = q::JS_NewObject(ctx);
    define_to_string_tag(ctx, intl, to_string_tag, c"Intl");
    define_function(
        ctx,
        intl,
        c"getCanonicalLocales",
        Some(get_canonical_locales),
        1,
    );
    for kind in [COLLATOR, NUMBER_FORMAT, DATE_TIME_FORMAT] {
        let proto = q::JS_NewObject(ctx);
        let ctor = q::JS_NewCFunctionMagic(
            ctx,
            Some(construct),
            NAMES[kind].as_ptr(),
            0,
            q::JS_CFUNC_constructor_or_func_magic,
            kind as c_int,
        );
        q::JS_SetConstructor(ctx, ctor, proto);
        define_magic_function(
            ctx,
            ctor,
            c"supportedLocalesOf",
            Some(supported_locales_of),
            1,
            kind,
        );
        define_magic_function(
            ctx,
            proto,
            c"resolvedOptions",
            Some(resolved_options),
            0,
            kind,
        );
        let (name, getter_name) = if kind == COLLATOR {
            (c"compare", c"get compare")
        } else {
            (c"format", c"get format")
        };
        let getter = q::JS_NewCFunctionMagic(
            ctx,
            Some(get_bound),
            getter_name.as_ptr(),
            0,
            q::JS_CFUNC_generic_magic,
            kind as c_int,
        );
        let atom = q::JS_NewAtom(ctx, name.as_ptr());
        q::JS_DefinePropertyGetSet(
            ctx,
            proto,
            atom,
            getter,
            q::JS_UNDEFINED,
            q::JS_PROP_CONFIGURABLE as c_int,
        );
        q::JS_FreeAtom(ctx, atom);
        if kind != COLLATOR {
            define_magic_function(ctx, proto, c"formatToParts", Some(format_to_parts), 1, kind);
        }
        define_to_string_tag(ctx, proto, to_string_tag, TAGS[kind]);
        q::JS_SetClassProto(ctx, ids[kind], proto);
        q::JS_DefinePropertyValueStr(ctx, intl, NAMES[kind].as_ptr(), ctor, METHOD);
    }
    q::JS_FreeAtom(ctx, to_string_tag);
    q::JS_DefinePropertyValueStr(ctx, global, c"Intl".as_ptr(), intl, METHOD);
    q::JS_FreeValue(ctx, global);

    let number = q::JS_GetClassProto(ctx, CLASS_NUMBER);
    define_function(
        ctx,
        number,
        c"toLocaleString",
        Some(number_to_locale_string),
        0,
    );
    q::JS_FreeValue(ctx, number);
    let string = q::JS_GetClassProto(ctx, CLASS_STRING);
    define_function(
        ctx,
        string,
        c"localeCompare",
        Some(string_locale_compare),
        1,
    );
    define_magic_function(
        ctx,
        string,
        c"toLocaleUpperCase",
        Some(string_to_locale_case),
        0,
        0,
    );
    define_magic_function(
        ctx,
        string,
        c"toLocaleLowerCase",
        Some(string_to_locale_case),
        0,
        1,
    );
    q::JS_FreeValue(ctx, string);
    let date = q::JS_GetClassProto(ctx, CLASS_DATE);
    define_magic_function(
        ctx,
        date,
        c"toLocaleString",
        Some(date_to_locale_string),
        0,
        0,
    );
    define_magic_function(
        ctx,
        date,
        c"toLocaleDateString",
        Some(date_to_locale_string),
        0,
        1,
    );
    define_magic_function(
        ctx,
        date,
        c"toLocaleTimeString",
        Some(date_to_locale_string),
        0,
        2,
    );
    q::JS_FreeValue(ctx, date);
}
