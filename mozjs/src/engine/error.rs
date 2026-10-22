/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Error objects, error reports, pending-exception stacks and stack capture.
//!
//! Stacks come from QuickJS's frames (patch Q8). A SpiderMonkey `SavedFrame` is an object of
//! the shim class `SAVED_FRAME_CLASS` whose reserved slots hold the frame's source, line,
//! column, function name and parent frame. Error reports read QuickJS Error objects: their
//! `message`, and their location from `fileName`/`lineNumber`/`columnNumber` (syntax errors) or
//! the first frame of `stack`.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;

use servo_quickjs as q;

use super::class::{new_object, object_class, object_data};
use super::{obj_val, Cx, RealmData};
use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;

// Stack frames.

/// A scripted frame of the current stack.
struct Frame {
    realm: *mut q::JSContext,
    module: *mut q::JSModuleDef,
    /// The host-defined value of the frame's script (borrowed), if not a module.
    script_private: q::JSValue,
    /// The function name (borrowed), or undefined for top-level code.
    name: q::JSValue,
    filename: q::JSAtom,
    line: u32,
    column: u32,
}

/// Calls `f` for the frames of the current stack, innermost first, until it returns true.
unsafe fn visit_frames(cx: &Cx, mut f: impl FnMut(&q::JSStackFrameInfo) -> bool) {
    unsafe extern "C" fn trampoline(
        opaque: *mut c_void,
        info: *const q::JSStackFrameInfo,
    ) -> c_int {
        let f = &mut *(opaque as *mut &mut dyn FnMut(&q::JSStackFrameInfo) -> bool);
        f(&*info) as c_int
    }
    let mut f: &mut dyn FnMut(&q::JSStackFrameInfo) -> bool = &mut f;
    q::JS_IterateStackFrames(cx.qctx(), Some(trampoline), &mut f as *mut _ as *mut c_void);
}

/// The scripted frames of the current stack, innermost first; at most `max`.
unsafe fn scripted_frames(cx: &Cx, max: usize) -> Vec<Frame> {
    let mut frames = Vec::new();
    visit_frames(cx, |info| {
        if frames.len() >= max {
            return true;
        }
        if info.is_native == 0 {
            frames.push(Frame {
                realm: info.realm,
                module: info.module,
                script_private: info.script_private,
                name: info.name,
                filename: info.filename,
                line: info.line_num.max(0) as u32,
                column: info.col_num.max(0) as u32,
            });
        }
        false
    });
    frames
}

/// Whether the innermost scripted frame's script has muted errors (SpiderMonkey's
/// `cx->currentScript()->mutedErrors()`).
pub(crate) unsafe fn current_script_muted(cx: &Cx) -> bool {
    let mut muted = false;
    visit_frames(cx, |info| {
        if info.is_native != 0 {
            return false;
        }
        muted = info.muted_errors != 0;
        true
    });
    muted
}

/// The realm of the innermost scripted frame, or null.
pub(crate) unsafe fn innermost_script_realm(cx: &Cx) -> *mut q::JSContext {
    scripted_frames(cx, 1)
        .first()
        .map_or(ptr::null_mut(), |f| f.realm)
}

thread_local! {
    /// `HideScriptedCaller` marks: the innermost frame (0 for none) at the time of each call.
    /// That frame and older ones are hidden from scripted-caller queries.
    static HIDDEN_CALLERS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// The innermost scripted frame unless it is hidden by `HideScriptedCaller`.
unsafe fn scripted_caller(cx: &Cx) -> Option<Frame> {
    let mark = HIDDEN_CALLERS.with(|h| h.borrow().last().copied());
    let mut caller = None;
    visit_frames(cx, |info| {
        if mark.is_some_and(|mark| mark == info.frame as usize) {
            return true;
        }
        if info.is_native != 0 {
            return false;
        }
        caller = Some(Frame {
            realm: info.realm,
            module: info.module,
            script_private: info.script_private,
            name: info.name,
            filename: info.filename,
            line: info.line_num.max(0) as u32,
            column: info.col_num.max(0) as u32,
        });
        true
    });
    caller
}

unsafe fn atom_to_string(cx: &Cx, atom: q::JSAtom) -> String {
    if atom == 0 {
        return String::new();
    }
    let s = q::JS_AtomToCString(cx.qctx(), atom);
    if s.is_null() {
        q::JS_FreeValueRT(cx.rt, q::JS_GetException(cx.qctx()));
        return String::new();
    }
    let result = CStr::from_ptr(s).to_string_lossy().into_owned();
    q::JS_FreeCString(cx.qctx(), s);
    result
}

/// The UTF-8 contents of a string value (other values are converted with `ToString`).
unsafe fn value_to_string(cx: &Cx, v: q::JSValue) -> Option<String> {
    let mut len = 0;
    let s = q::JS_ToCStringLen2(cx.qctx(), &mut len, v, 0);
    if s.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(s as *const u8, len);
    let result = String::from_utf8_lossy(bytes).into_owned();
    q::JS_FreeCString(cx.qctx(), s);
    Some(result)
}

/// The name of a frame's function, or `None` for top-level code and anonymous functions.
unsafe fn frame_name(cx: &Cx, frame: &Frame) -> Option<String> {
    if frame.name.tag() != q::JS_TAG_STRING {
        return None;
    }
    value_to_string(cx, frame.name).filter(|name| !name.is_empty())
}

pub(crate) unsafe fn DescribeScriptedCaller(
    cx: *mut root::JSContext,
    buffer: *mut ::std::os::raw::c_char,
    buflen: usize,
    line: *mut u32,
    col: *mut u32,
) -> bool {
    let cx = Cx::from_raw(cx);
    if buflen > 0 {
        *buffer = 0;
    }
    *line = 0;
    *col = 0;
    let Some(caller) = scripted_caller(cx) else {
        return false;
    };
    let filename = atom_to_string(cx, caller.filename);
    if buflen > 0 {
        let n = filename.len().min(buflen - 1);
        ptr::copy_nonoverlapping(filename.as_ptr() as *const c_char, buffer, n);
        *buffer.add(n) = 0;
    }
    *line = caller.line;
    // mozjs's glue returns a zero-based column.
    *col = caller.column.saturating_sub(1);
    true
}

pub(crate) unsafe fn GetScriptedCallerGlobal(cx: *mut root::JSContext) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let Some(caller) = scripted_caller(cx) else {
        return ptr::null_mut();
    };
    let realm = q::JS_GetContextOpaque(caller.realm) as *mut RealmData;
    if realm.is_null() {
        ptr::null_mut()
    } else {
        (*realm).global
    }
}

/// The innermost stack frame, or 0 for none.
pub(crate) unsafe fn innermost_frame(cx: &Cx) -> usize {
    let mut innermost = 0;
    visit_frames(cx, |info| {
        innermost = info.frame as usize;
        true
    });
    innermost
}

/// Whether a scripted frame is newer than `mark`, a frame from `innermost_frame` that is still on
/// the stack.
pub(crate) unsafe fn script_since(cx: &Cx, mark: usize) -> bool {
    let mut script = false;
    visit_frames(cx, |info| {
        if info.frame as usize == mark {
            return true;
        }
        script = info.is_native == 0;
        script
    });
    script
}

pub(crate) unsafe fn HideScriptedCaller(cx: *mut root::JSContext) {
    let innermost = innermost_frame(Cx::from_raw(cx));
    HIDDEN_CALLERS.with(|h| h.borrow_mut().push(innermost));
}

pub(crate) unsafe fn UnhideScriptedCaller(cx: *mut root::JSContext) {
    let _ = cx;
    HIDDEN_CALLERS.with(|h| {
        h.borrow_mut()
            .pop()
            .expect("UnhideScriptedCaller without HideScriptedCaller")
    });
}

pub(crate) unsafe fn JS_GetScriptedCallerPrivate(
    cx: *mut root::JSContext,
    dest: root::JS::MutableHandleValue,
) {
    let cx = Cx::from_raw(cx);
    *dest.ptr = match scripted_caller(cx) {
        Some(caller) => super::module::script_or_module_private(
            cx,
            caller.realm,
            caller.module,
            caller.script_private,
        ),
        None => JSVal::from_qjs(q::JS_UNDEFINED),
    };
}

pub(crate) unsafe fn DumpJSStack(
    cx: *mut root::JSContext,
    showArgs: bool,
    showLocals: bool,
    showThisProps: bool,
) {
    let _ = (showArgs, showLocals, showThisProps);
    let cx = Cx::from_raw(cx);
    for (i, frame) in scripted_frames(cx, usize::MAX).iter().enumerate() {
        let name = frame_name(cx, frame).unwrap_or_default();
        let file = atom_to_string(cx, frame.filename);
        println!("{i} {name}() [\"{file}\":{}:{}]", frame.line, frame.column);
    }
}

// Saved frames.

const SAVED_FRAME_SOURCE: usize = 0;
const SAVED_FRAME_LINE: usize = 1;
const SAVED_FRAME_COLUMN: usize = 2;
const SAVED_FRAME_NAME: usize = 3;
const SAVED_FRAME_PARENT: usize = 4;
const SAVED_FRAME_SLOTS: u32 = 5;

static SAVED_FRAME_CLASS: root::JSClass = root::JSClass {
    name: c"SavedFrame".as_ptr(),
    flags: (SAVED_FRAME_SLOTS & root::JSCLASS_RESERVED_SLOTS_MASK)
        << root::JSCLASS_RESERVED_SLOTS_SHIFT,
    cOps: ptr::null(),
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

/// `JS::StackCapture` (a `mozilla::Variant`) as written by the `JS_StackCapture_*` glue:
/// `[kind, max frames]`.
const CAPTURE_ALL: u64 = 0;
const CAPTURE_MAX: u64 = 1;
const CAPTURE_FIRST_SUBSUMED: u64 = 2;

pub(crate) unsafe fn JS_StackCapture_AllFrames(capture: *mut root::JS::StackCapture) {
    (*capture).0 = [CAPTURE_ALL, 0, 0, 0];
}

pub(crate) unsafe fn JS_StackCapture_MaxFrames(max: u32, capture: *mut root::JS::StackCapture) {
    (*capture).0 = [CAPTURE_MAX, max as u64, 0, 0];
}

pub(crate) unsafe fn JS_StackCapture_FirstSubsumedFrame(
    cx: *mut root::JSContext,
    ignoreSelfHostedFrames: bool,
    capture: *mut root::JS::StackCapture,
) {
    let _ = (cx, ignoreSelfHostedFrames);
    (*capture).0 = [CAPTURE_FIRST_SUBSUMED, 1, 0, 0];
}

pub(crate) unsafe fn CaptureCurrentStack(
    cx: *mut root::JSContext,
    stackp: root::JS::MutableHandleObject,
    capture: *mut root::JS::StackCapture,
    startAfter: root::JS::HandleObject,
) -> bool {
    let _ = startAfter;
    let cx = Cx::from_raw(cx);
    let [kind, max, ..] = (*capture).0;
    let max = match kind {
        CAPTURE_MAX | CAPTURE_FIRST_SUBSUMED if max > 0 => max as usize,
        _ => usize::MAX,
    };
    let frames = scripted_frames(cx, max);
    // Outermost first, so that each frame can take its parent.
    let mut parent = q::JS_NULL;
    for frame in frames.iter().rev() {
        let obj = new_object(cx, &SAVED_FRAME_CLASS, q::JS_NULL);
        if obj.tag() == q::JS_TAG_EXCEPTION {
            q::JS_FreeValueRT(cx.rt, parent);
            return false;
        }
        let qctx = cx.qctx();
        let source = if frame.filename == 0 {
            q::JS_NewStringLen(qctx, c"".as_ptr(), 0)
        } else {
            q::JS_AtomToString(qctx, frame.filename)
        };
        let name = match frame_name(cx, frame) {
            Some(_) => q::JS_DupValueRT(cx.rt, frame.name),
            None => q::JS_NULL,
        };
        let data = object_data(super::val_obj(obj)).unwrap();
        data.slots[SAVED_FRAME_SOURCE].set(source);
        data.slots[SAVED_FRAME_LINE].set(q::JS_NewUint32(qctx, frame.line));
        data.slots[SAVED_FRAME_COLUMN].set(q::JS_NewUint32(qctx, frame.column));
        data.slots[SAVED_FRAME_NAME].set(name);
        data.slots[SAVED_FRAME_PARENT].set(parent);
        parent = obj;
    }
    *stackp.ptr = if parent.tag() == q::JS_TAG_OBJECT {
        cx.adopt(parent).to_object()
    } else {
        ptr::null_mut()
    };
    true
}

/// The slots of a saved frame, or `None` if `frame` is not one.
unsafe fn saved_frame<'a>(frame: *mut root::JSObject) -> Option<&'a [Cell<q::JSValue>]> {
    if frame.is_null() || !ptr::eq(object_class(frame), &SAVED_FRAME_CLASS) {
        return None;
    }
    object_data(frame).map(|data| &*data.slots)
}

fn uint_slot(v: q::JSValue) -> u32 {
    match v.tag() {
        q::JS_TAG_INT => v.int() as u32,
        q::JS_TAG_FLOAT64 => v.float64() as u32,
        _ => 0,
    }
}

pub(crate) unsafe fn GetSavedFrameSource(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    savedFrame: root::JS::Handle<*mut root::JSObject>,
    sourcep: root::JS::MutableHandle<*mut root::JSString>,
    selfHosted: root::JS::SavedFrameSelfHosted,
) -> root::JS::SavedFrameResult {
    let _ = (principals, selfHosted);
    let cx = Cx::from_raw(cx);
    let Some(slots) = saved_frame(*savedFrame.ptr) else {
        *sourcep.ptr = cx
            .adopt(q::JS_NewStringLen(cx.qctx(), c"".as_ptr(), 0))
            .to_string();
        return root::JS::SavedFrameResult::AccessDenied;
    };
    *sourcep.ptr = cx
        .retain(JSVal::from_qjs(slots[SAVED_FRAME_SOURCE].get()))
        .to_string();
    root::JS::SavedFrameResult::Ok
}

pub(crate) unsafe fn GetSavedFrameLine(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    savedFrame: root::JS::Handle<*mut root::JSObject>,
    linep: *mut u32,
    selfHosted: root::JS::SavedFrameSelfHosted,
) -> root::JS::SavedFrameResult {
    let _ = (cx, principals, selfHosted);
    let Some(slots) = saved_frame(*savedFrame.ptr) else {
        *linep = 0;
        return root::JS::SavedFrameResult::AccessDenied;
    };
    *linep = uint_slot(slots[SAVED_FRAME_LINE].get());
    root::JS::SavedFrameResult::Ok
}

pub(crate) unsafe fn GetSavedFrameColumn(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    savedFrame: root::JS::Handle<*mut root::JSObject>,
    columnp: *mut root::JS::TaggedColumnNumberOneOrigin,
    selfHosted: root::JS::SavedFrameSelfHosted,
) -> root::JS::SavedFrameResult {
    let _ = (cx, principals, selfHosted);
    let Some(slots) = saved_frame(*savedFrame.ptr) else {
        (*columnp).value_ = 0;
        return root::JS::SavedFrameResult::AccessDenied;
    };
    (*columnp).value_ = uint_slot(slots[SAVED_FRAME_COLUMN].get());
    root::JS::SavedFrameResult::Ok
}

pub(crate) unsafe fn GetSavedFrameFunctionDisplayName(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    savedFrame: root::JS::Handle<*mut root::JSObject>,
    namep: root::JS::MutableHandle<*mut root::JSString>,
    selfHosted: root::JS::SavedFrameSelfHosted,
) -> root::JS::SavedFrameResult {
    let _ = (principals, selfHosted);
    let cx = Cx::from_raw(cx);
    *namep.ptr = ptr::null_mut();
    let Some(slots) = saved_frame(*savedFrame.ptr) else {
        return root::JS::SavedFrameResult::AccessDenied;
    };
    let name = slots[SAVED_FRAME_NAME].get();
    if name.tag() == q::JS_TAG_STRING {
        *namep.ptr = cx.retain(JSVal::from_qjs(name)).to_string();
    }
    root::JS::SavedFrameResult::Ok
}

pub(crate) unsafe fn GetSavedFrameParent(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    savedFrame: root::JS::Handle<*mut root::JSObject>,
    parentp: root::JS::MutableHandle<*mut root::JSObject>,
    selfHosted: root::JS::SavedFrameSelfHosted,
) -> root::JS::SavedFrameResult {
    let _ = (principals, selfHosted);
    let cx = Cx::from_raw(cx);
    *parentp.ptr = ptr::null_mut();
    let Some(slots) = saved_frame(*savedFrame.ptr) else {
        return root::JS::SavedFrameResult::AccessDenied;
    };
    let parent = slots[SAVED_FRAME_PARENT].get();
    if parent.tag() == q::JS_TAG_OBJECT {
        *parentp.ptr = cx.retain(JSVal::from_qjs(parent)).to_object();
    }
    root::JS::SavedFrameResult::Ok
}

pub(crate) unsafe fn BuildStackString(
    cx: *mut root::JSContext,
    principals: *mut root::JSPrincipals,
    stack: root::JS::HandleObject,
    stringp: root::JS::MutableHandleString,
    indent: usize,
    stackFormat: root::js::StackFormat,
) -> bool {
    let _ = principals;
    let cx = Cx::from_raw(cx);
    let mut out = String::new();
    let mut frame = *stack.ptr;
    while let Some(slots) = saved_frame(frame) {
        let source = value_to_string(cx, slots[SAVED_FRAME_SOURCE].get()).unwrap_or_default();
        let line = uint_slot(slots[SAVED_FRAME_LINE].get());
        let column = uint_slot(slots[SAVED_FRAME_COLUMN].get());
        let name = slots[SAVED_FRAME_NAME].get();
        let name = if name.tag() == q::JS_TAG_STRING {
            value_to_string(cx, name)
        } else {
            None
        };
        out += &" ".repeat(indent);
        match (stackFormat, name) {
            (root::js::StackFormat::V8, Some(name)) => {
                out += &format!("    at {name} ({source}:{line}:{column})\n")
            }
            (root::js::StackFormat::V8, None) => {
                out += &format!("    at {source}:{line}:{column}\n")
            }
            (_, name) => out += &format!("{}@{source}:{line}:{column}\n", name.unwrap_or_default()),
        }
        let parent = slots[SAVED_FRAME_PARENT].get();
        frame = if parent.tag() == q::JS_TAG_OBJECT {
            super::val_obj(parent)
        } else {
            ptr::null_mut()
        };
    }
    let s = q::JS_NewStringLen(cx.qctx(), out.as_ptr() as *const c_char, out.len());
    if s.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *stringp.ptr = cx.adopt(s).to_string();
    true
}

// Error reports.

/// Keeps `value` alive until the autorelease pool drains (end of the task), like the values the
/// shim hands to Rust.
unsafe fn pool_owned<T>(cx: &Cx, value: Box<T>) -> *mut T {
    unsafe extern "C" fn free<T>(_rt: *mut q::JSRuntime, opaque: *mut c_void, _ptr: *mut c_void) {
        drop(Box::from_raw(opaque as *mut T));
    }
    let raw = Box::into_raw(value);
    // An empty ArrayBuffer that owns `raw` and is never exposed to script.
    let holder = q::JS_NewArrayBuffer(
        cx.qctx(),
        raw as *mut u8,
        0,
        Some(free::<T>),
        raw as *mut c_void,
        0,
    );
    if holder.tag() == q::JS_TAG_EXCEPTION {
        // Out of memory: leak the value rather than fail.
        q::JS_FreeValueRT(cx.rt, q::JS_GetException(cx.qctx()));
    } else {
        cx.adopt(holder);
    }
    raw
}

/// A `JSErrorReport` with the strings it points to.
#[repr(C)]
struct OwnedReport {
    report: root::JSErrorReport,
    message: CString,
    filename: Option<CString>,
}

/// What an error report says about an exception.
#[derive(Default)]
struct ReportInfo {
    message: String,
    filename: String,
    line: u32,
    column: u32,
}

/// The value of an own data property (a new reference), without running getters.
unsafe fn own_data_property(cx: &Cx, obj: q::JSValue, name: &CStr) -> Option<q::JSValue> {
    let qctx = cx.qctx();
    let atom = q::JS_NewAtom(qctx, name.as_ptr());
    let mut desc: q::JSPropertyDescriptor = std::mem::zeroed();
    let r = q::JS_GetOwnProperty(qctx, &mut desc, obj, atom);
    q::JS_FreeAtomRT(cx.rt, atom);
    if r < 0 {
        q::JS_FreeValueRT(cx.rt, q::JS_GetException(qctx));
        return None;
    }
    if r == 0 {
        return None;
    }
    q::JS_FreeValueRT(cx.rt, desc.getter);
    q::JS_FreeValueRT(cx.rt, desc.setter);
    if desc.flags as u32 & q::JS_PROP_GETSET != 0 {
        q::JS_FreeValueRT(cx.rt, desc.value);
        return None;
    }
    Some(desc.value)
}

unsafe fn own_string_property(cx: &Cx, obj: q::JSValue, name: &CStr) -> Option<String> {
    let v = own_data_property(cx, obj, name)?;
    let s = if v.tag() == q::JS_TAG_STRING || v.tag() == q::JS_TAG_STRING_ROPE {
        value_to_string(cx, v)
    } else {
        None
    };
    q::JS_FreeValueRT(cx.rt, v);
    s
}

unsafe fn own_uint_property(cx: &Cx, obj: q::JSValue, name: &CStr) -> Option<u32> {
    let v = own_data_property(cx, obj, name)?;
    let n = match v.tag() {
        q::JS_TAG_INT => Some(v.int().max(0) as u32),
        q::JS_TAG_FLOAT64 => Some(v.float64() as u32),
        _ => None,
    };
    q::JS_FreeValueRT(cx.rt, v);
    n
}

/// The location of a QuickJS backtrace line: `    at name (file:line:col)` or `    at file:line:col`.
fn parse_backtrace_line(line: &str) -> Option<(String, u32, u32)> {
    let rest = line.trim_start().strip_prefix("at ")?;
    let location = match rest.strip_suffix(')') {
        Some(rest) => &rest[rest.rfind(" (")? + 2..],
        None => rest,
    };
    let (rest, column) = location.rsplit_once(':')?;
    let (file, line) = rest.rsplit_once(':')?;
    Some((file.to_owned(), line.parse().ok()?, column.parse().ok()?))
}

/// The report of a QuickJS Error object.
unsafe fn error_report_info(cx: &Cx, error: q::JSValue) -> ReportInfo {
    let mut info = ReportInfo {
        message: own_string_property(cx, error, c"message").unwrap_or_default(),
        ..Default::default()
    };
    if let (Some(file), Some(line)) = (
        own_string_property(cx, error, c"fileName"),
        own_uint_property(cx, error, c"lineNumber"),
    ) {
        info.filename = file;
        info.line = line;
        info.column = own_uint_property(cx, error, c"columnNumber").unwrap_or(0);
    } else if let Some(stack) = own_string_property(cx, error, c"stack") {
        if let Some((file, line, column)) = stack.lines().find_map(parse_backtrace_line) {
            info.filename = file;
            info.line = line;
            info.column = column;
        }
    }
    info
}

unsafe fn lossy_cstring(s: String) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

pub(crate) unsafe fn JS_ErrorFromException(
    cx: *mut root::JSContext,
    obj: root::JS::HandleObject,
    errorReport: *mut root::JS::BorrowedErrorReport,
) -> bool {
    let cx = Cx::from_raw(cx);
    let error = obj_val(*obj.ptr);
    if q::JS_IsError(cx.qctx(), error) == 0 {
        return false;
    }
    let info = error_report_info(cx, error);
    let mut owned = Box::new(OwnedReport {
        report: std::mem::zeroed(),
        message: lossy_cstring(info.message),
        filename: (!info.filename.is_empty()).then(|| lossy_cstring(info.filename)),
    });
    owned.report._base.message_.data_ = owned.message.as_ptr();
    owned.report._base.filename.data_ = owned.filename.as_ref().map_or(ptr::null(), |f| f.as_ptr());
    owned.report._base.lineno = info.line;
    owned.report._base.column._base = info.column;
    owned.report.exnType = root::JSExnType::JSEXN_ERR as i16;
    // SpiderMonkey keeps the report alive with the error object; the pool keeps it for the task.
    let owned = pool_owned(cx, owned);
    (*errorReport).owner_.data = *obj.ptr;
    (*errorReport).report_ = &mut (*owned).report;
    true
}

pub(crate) unsafe fn PendingExceptionStackInfo(
    cx: *mut root::JSContext,
    callback: crate::glue::StringCallback,
    message_target: *mut ::std::os::raw::c_void,
    filename_target: *mut ::std::os::raw::c_void,
    line: *mut u32,
    col: *mut u32,
    dest: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    if q::JS_HasException(qctx) == 0 {
        return false;
    }
    let uncatchable = q::JS_HasUncatchableException(qctx) != 0;
    let mut thrown_at = (0, 0, 0);
    let has_location =
        q::JS_GetExceptionLocation(qctx, &mut thrown_at.0, &mut thrown_at.1, &mut thrown_at.2) != 0;
    let exception = cx.adopt(q::JS_GetException(qctx)).to_qjs();
    if uncatchable {
        return false;
    }
    let info = if q::JS_IsError(qctx, exception) != 0 {
        error_report_info(cx, exception)
    } else {
        let text = value_to_string(cx, exception).unwrap_or_else(|| {
            q::JS_FreeValueRT(cx.rt, q::JS_GetException(qctx));
            "unknown (can't convert to string)".to_owned()
        });
        let mut info = ReportInfo {
            message: format!("uncaught exception: {text}"),
            ..Default::default()
        };
        // SpiderMonkey uses the stack captured when the value was thrown.
        if has_location {
            info.filename = atom_to_string(cx, thrown_at.0);
            info.line = thrown_at.1.max(0) as u32;
            info.column = thrown_at.2.max(0) as u32;
        } else if let Some(caller) = scripted_caller(cx) {
            info.filename = atom_to_string(cx, caller.filename);
            info.line = caller.line;
            info.column = caller.column;
        }
        info
    };
    let callback = callback.expect("PendingExceptionStackInfo without a callback");
    callback(
        info.message.as_ptr() as *const c_char,
        info.message.len(),
        message_target,
    );
    if !info.filename.is_empty() {
        callback(
            info.filename.as_ptr() as *const c_char,
            info.filename.len(),
            filename_target,
        );
    }
    *line = info.line;
    *col = info.column;
    *dest.ptr = JSVal::from_qjs(exception);
    true
}

#[cfg(test)]
mod tests {
    use super::parse_backtrace_line;

    #[test]
    fn backtrace_lines() {
        assert_eq!(
            parse_backtrace_line("    at foo (http://a:80/x.js:3:21)"),
            Some(("http://a:80/x.js".into(), 3, 21))
        );
        assert_eq!(
            parse_backtrace_line("    at <eval> (test.js:8:16)"),
            Some(("test.js".into(), 8, 16))
        );
        assert_eq!(
            parse_backtrace_line("    at test.js:2:5"),
            Some(("test.js".into(), 2, 5))
        );
        assert_eq!(parse_backtrace_line("    at parse (native)"), None);
    }
}
