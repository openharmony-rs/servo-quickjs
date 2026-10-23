/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! ES modules: compile, load, link, evaluate and the host hooks of SpiderMonkey's module API.
//!
//! As in SpiderMonkey 153, `LoadRequestedModules` walks the module graph (ECMA-262's
//! GraphLoadingState) and asks the embedder's load hook for every request that is not loaded
//! yet; the embedder answers with `FinishLoadingImportedModule`, which records the result in the
//! requesting module's [[LoadedModules]]. QuickJS modules are compiled without loading their
//! requests (`JS_EVAL_FLAG_NO_RESOLVE`), and `JS_LinkModule` resolves each request from
//! [[LoadedModules]] (patch Q7).
//!
//! A module record is an object of the `ModuleObject` class holding one reference to its
//! `JSModuleDef`; QuickJS's context also keeps every module alive until the realm dies. The
//! private value of a JavaScript module lives in the `JSModuleDef`, where the import.meta hook
//! and `JS_GetScriptPrivate` find it. Module requests are objects of the `ModuleRequest` class.
//! The referrer of a request is a `JSScript`: the code of the module (patch Q31) or of the
//! script calling import().

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::marker::PhantomData;
use std::ptr;
use std::sync::OnceLock;

use servo_quickjs as q;

use super::eval::{options, padded, source_buffer, utf16_source};
use super::{adopt_object, obj_val, val_obj, Cx, RealmData};
use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;

#[derive(Clone, Copy, PartialEq)]
enum ModuleKind {
    JavaScript,
    /// A synthetic module whose only export is `default` (JSON, CSS and text modules); the value
    /// is kept in the `JSModuleDef`'s private value until evaluation.
    Synthetic,
}

/// Opaque data of `ModuleObject` objects.
struct ModuleData {
    /// One reference (a `JS_TAG_MODULE` value).
    module: *mut q::JSModuleDef,
    kind: ModuleKind,
    /// The `SetModulePrivate` value of synthetic modules (counted).
    private: Cell<q::JSValue>,
    /// [[LoadedModules]]: the module record each request resolved to (counted), by index.
    loaded: Box<[Cell<q::JSValue>]>,
    /// The request objects of the module's requests (counted), by index, made on first use so
    /// that the load hook sees the same request each time.
    requests: Box<[Cell<q::JSValue>]>,
    /// [[Status]] is no longer "new": a graph load that visited the module completed.
    loaded_graph: Cell<bool>,
}

/// Opaque data of `ModuleRequest` objects.
struct RequestData {
    /// A counted string.
    specifier: q::JSValue,
    module_type: root::JS::ModuleType,
    /// The module record that has this request (counted), and the request's index there; none
    /// for import().
    referrer: q::JSValue,
    index: usize,
}

/// Opaque data of `GraphLoadingState` objects (ECMA-262's GraphLoadingState Record).
struct LoadState {
    /// The promise of `LoadRequestedModules` (counted), or `JS_UNDEFINED` with callbacks.
    promise: q::JSValue,
    resolved: root::JS::LoadModuleResolvedCallback,
    rejected: root::JS::LoadModuleRejectedCallback,
    is_loading: Cell<bool>,
    pending: Cell<usize>,
    /// Counted module records. Only borrowed to add or read entries, never across a call.
    visited: RefCell<Vec<q::JSValue>>,
    /// The records in `visited`.
    visited_set: RefCell<HashSet<*mut root::JSObject>>,
    /// Counted.
    host_defined: q::JSValue,
}

#[derive(Clone, Copy)]
struct ClassIds {
    module: q::JSClassID,
    request: q::JSClassID,
    state: q::JSClassID,
}

static CLASSES: OnceLock<ClassIds> = OnceLock::new();

thread_local! {
    /// `JSModuleDef` -> its module record, for QuickJS's hooks (linking, import.meta).
    static RECORDS: RefCell<HashMap<usize, *mut root::JSObject>> = RefCell::new(HashMap::new());
}

/// The module record of `m`, if it has one.
fn record_of(m: *mut q::JSModuleDef) -> Option<*mut root::JSObject> {
    RECORDS.with(|r| r.borrow().get(&(m as usize)).copied())
}

/// The module classes, registered in `rt` on first use.
unsafe fn classes(rt: *mut q::JSRuntime) -> ClassIds {
    let ids = *CLASSES.get_or_init(|| {
        let (mut module, mut request, mut state) = (0, 0, 0);
        q::JS_NewClassID(&mut module);
        q::JS_NewClassID(&mut request);
        q::JS_NewClassID(&mut state);
        ClassIds {
            module,
            request,
            state,
        }
    });
    if q::JS_IsRegisteredClass(rt, ids.module) == 0 {
        let defs = [
            (
                ids.module,
                c"ModuleObject",
                module_finalize as _,
                module_gc_mark as _,
            ),
            (
                ids.request,
                c"ModuleRequest",
                request_finalize as _,
                request_gc_mark as _,
            ),
            (
                ids.state,
                c"GraphLoadingState",
                state_finalize as _,
                state_gc_mark as _,
            ),
        ];
        for (id, name, finalizer, gc_mark) in defs {
            let def = q::JSClassDef {
                class_name: name.as_ptr(),
                finalizer: Some(finalizer),
                gc_mark: Some(gc_mark),
                call: None,
                exotic: ptr::null_mut(),
            };
            assert_eq!(q::JS_NewClass(rt, id, &def), 0);
        }
    }
    ids
}

fn module_val(m: *mut q::JSModuleDef) -> q::JSValue {
    q::JSValue::mkptr(q::JS_TAG_MODULE, m as *mut c_void)
}

/// The data of `v` if it is of `class`; only finalizers make it mutable, to free it.
unsafe fn opaque_of<T>(v: q::JSValue, class: fn(ClassIds) -> q::JSClassID) -> *const T {
    match CLASSES.get() {
        Some(ids) => q::JS_GetOpaque(v, class(*ids)) as *const T,
        None => ptr::null(),
    }
}

unsafe fn module_data<'a>(obj: *mut root::JSObject) -> Option<&'a ModuleData> {
    opaque_of::<ModuleData>(obj_val(obj), |ids| ids.module).as_ref()
}

unsafe fn request_data<'a>(obj: *mut root::JSObject) -> Option<&'a RequestData> {
    opaque_of::<RequestData>(obj_val(obj), |ids| ids.request).as_ref()
}

/// The state of a graph load, if `v` is a `GraphLoadingState` object.
unsafe fn state_of<'a>(v: q::JSValue) -> Option<&'a LoadState> {
    if v.tag() != q::JS_TAG_OBJECT {
        return None;
    }
    opaque_of::<LoadState>(v, |ids| ids.state).as_ref()
}

unsafe fn module_of<'a>(obj: *mut root::JSObject) -> &'a ModuleData {
    module_data(obj).expect("not a module record")
}

unsafe fn mark_all(
    rt: *mut q::JSRuntime,
    values: impl IntoIterator<Item = q::JSValue>,
    mark: q::JS_MarkFunc,
) {
    for v in values {
        if v.has_ref_count() {
            q::JS_MarkValue(rt, v, mark);
        }
    }
}

unsafe fn free_all(rt: *mut q::JSRuntime, values: impl IntoIterator<Item = q::JSValue>) {
    for v in values {
        q::JS_FreeValueRT(rt, v);
    }
}

unsafe extern "C" fn module_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = opaque_of::<ModuleData>(val, |ids| ids.module);
    if !data.is_null() {
        let data = Box::from_raw(data as *mut ModuleData);
        RECORDS.with(|r| r.borrow_mut().remove(&(data.module as usize)));
        q::JS_FreeValueRT(rt, module_val(data.module));
        let private = data.private.get();
        if private.tag() != q::JS_TAG_UNDEFINED {
            release_script_private(rt, private);
        }
        q::JS_FreeValueRT(rt, private);
        free_all(rt, data.loaded.iter().chain(&*data.requests).map(Cell::get));
    }
}

unsafe extern "C" fn module_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    let data = opaque_of::<ModuleData>(val, |ids| ids.module);
    if let Some(data) = data.as_ref() {
        q::JS_MarkValue(rt, module_val(data.module), mark);
        mark_all(rt, [data.private.get()], mark);
        let records = data.loaded.iter().chain(&*data.requests);
        mark_all(rt, records.map(Cell::get), mark);
    }
}

unsafe extern "C" fn request_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = opaque_of::<RequestData>(val, |ids| ids.request);
    if !data.is_null() {
        let data = Box::from_raw(data as *mut RequestData);
        free_all(rt, [data.specifier, data.referrer]);
    }
}

unsafe extern "C" fn request_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    let data = opaque_of::<RequestData>(val, |ids| ids.request);
    if let Some(data) = data.as_ref() {
        mark_all(rt, [data.referrer], mark);
    }
}

unsafe extern "C" fn state_finalize(rt: *mut q::JSRuntime, val: q::JSValue) {
    let data = opaque_of::<LoadState>(val, |ids| ids.state);
    if !data.is_null() {
        let data = Box::from_raw(data as *mut LoadState);
        free_all(rt, [data.promise, data.host_defined]);
        free_all(rt, data.visited.take());
    }
}

unsafe extern "C" fn state_gc_mark(rt: *mut q::JSRuntime, val: q::JSValue, mark: q::JS_MarkFunc) {
    let data = opaque_of::<LoadState>(val, |ids| ids.state);
    if let Some(data) = data.as_ref() {
        mark_all(rt, [data.promise, data.host_defined], mark);
        mark_all(rt, data.visited.borrow().iter().copied(), mark);
    }
}

/// A module record for `module` (a reference the caller owns), retained by the pool.
unsafe fn new_module_object(cx: &Cx, module: q::JSValue, kind: ModuleKind) -> *mut root::JSObject {
    let class = classes(cx.rt).module;
    let obj = q::JS_NewObjectProtoClass(cx.qctx(), q::JS_NULL, class);
    if obj.tag() == q::JS_TAG_EXCEPTION {
        q::JS_FreeValueRT(cx.rt, module);
        return ptr::null_mut();
    }
    let m = module.ptr() as *mut q::JSModuleDef;
    let count = if kind == ModuleKind::JavaScript {
        q::JS_GetModuleRequestCount(m) as usize
    } else {
        0
    };
    let data = Box::into_raw(Box::new(ModuleData {
        module: m,
        kind,
        private: Cell::new(q::JS_UNDEFINED),
        loaded: vec![Cell::new(q::JS_UNDEFINED); count].into_boxed_slice(),
        requests: vec![Cell::new(q::JS_UNDEFINED); count].into_boxed_slice(),
        loaded_graph: Cell::new(false),
    }));
    RECORDS.with(|r| r.borrow_mut().insert(m as usize, val_obj(obj)));
    q::JS_SetOpaque(obj, data as *mut c_void);
    adopt_object(cx, obj)
}

/// A new module request object (a new reference) taking over `specifier` and `referrer`.
unsafe fn new_module_request(
    cx: &Cx,
    specifier: q::JSValue,
    module_type: root::JS::ModuleType,
    referrer: q::JSValue,
    index: usize,
) -> q::JSValue {
    let class = classes(cx.rt).request;
    let obj = q::JS_NewObjectProtoClass(cx.qctx(), q::JS_NULL, class);
    if obj.tag() == q::JS_TAG_EXCEPTION {
        q::JS_FreeValueRT(cx.rt, specifier);
        q::JS_FreeValueRT(cx.rt, referrer);
        return obj;
    }
    let data = Box::new(RequestData {
        specifier,
        module_type,
        referrer,
        index,
    });
    q::JS_SetOpaque(obj, Box::into_raw(data) as *mut c_void);
    obj
}

fn handle<T>(ptr: &T) -> root::JS::Handle<T> {
    root::JS::Handle {
        _phantom_0: PhantomData,
        ptr,
    }
}

/// Runs `f` in the realm of the QuickJS context `ctx` (hooks called by QuickJS).
unsafe fn in_realm_of<R>(
    cx: &Cx,
    ctx: *mut q::JSContext,
    f: impl FnOnce(*mut root::JSContext) -> R,
) -> R {
    let realm = q::JS_GetContextOpaque(ctx) as *mut RealmData;
    if !realm.is_null() {
        cx.enter_realm(realm);
    }
    let result = f(cx.as_raw());
    if !realm.is_null() {
        cx.leave_realm();
    }
    result
}

/// A hook that fails without an exception is SpiderMonkey's uncatchable termination; QuickJS
/// needs an exception (see `call_native`).
unsafe fn ensure_exception(ctx: *mut q::JSContext) {
    if q::JS_HasException(ctx) == 0 {
        q::JS_ThrowInternalError(ctx, c"script terminated".as_ptr());
        q::JS_SetUncatchableException(ctx, 1);
    }
}

// Import attributes: HTML supports only `type`, and module types "json" and JavaScript.

/// Throws a SyntaxError (like SpiderMonkey) if `attributes` has a key other than `type`.
/// Whether all import attributes are supported (only `type` is); throws a SyntaxError for
/// static imports and a TypeError for import(), as SpiderMonkey does.
unsafe fn check_attributes(ctx: *mut q::JSContext, attributes: q::JSValue, dynamic: bool) -> bool {
    if attributes.tag() != q::JS_TAG_OBJECT {
        return true;
    }
    let mut tab: *mut q::JSPropertyEnum = ptr::null_mut();
    let mut len = 0;
    if q::JS_GetOwnPropertyNames(
        ctx,
        &mut tab,
        &mut len,
        attributes,
        q::JS_GPN_STRING_MASK as c_int,
    ) < 0
    {
        return false;
    }
    let type_atom = q::JS_NewAtom(ctx, c"type".as_ptr());
    let mut ok = true;
    for i in 0..len as usize {
        let atom = (*tab.add(i)).atom;
        if atom != type_atom {
            let key = q::JS_AtomToCString(ctx, atom);
            if dynamic {
                q::JS_ThrowTypeError(ctx, c"Unsupported import attribute: %s".as_ptr(), key);
            } else {
                q::JS_ThrowSyntaxError(ctx, c"Unsupported import attribute: %s".as_ptr(), key);
            }
            q::JS_FreeCString(ctx, key);
            ok = false;
            break;
        }
    }
    q::JS_FreeAtom(ctx, type_atom);
    q::JS_FreePropertyEnum(ctx, tab, len);
    ok
}

unsafe fn module_type_of(ctx: *mut q::JSContext, attributes: q::JSValue) -> root::JS::ModuleType {
    if attributes.tag() != q::JS_TAG_OBJECT {
        return root::JS::ModuleType::JavaScript;
    }
    let value = q::JS_GetPropertyStr(ctx, attributes, c"type".as_ptr());
    let module_type = match value.tag() {
        q::JS_TAG_UNDEFINED => root::JS::ModuleType::JavaScript,
        q::JS_TAG_STRING => {
            let s = q::JS_ToCString(ctx, value);
            let module_type = match (!s.is_null()).then(|| CStr::from_ptr(s).to_bytes()) {
                Some(b"json") => root::JS::ModuleType::JSON,
                Some(b"css") => root::JS::ModuleType::CSS,
                Some(b"text") => root::JS::ModuleType::Text,
                _ => root::JS::ModuleType::Unknown,
            };
            q::JS_FreeCString(ctx, s);
            module_type
        }
        _ => root::JS::ModuleType::Unknown,
    };
    if value.tag() == q::JS_TAG_EXCEPTION {
        let exception = q::JS_GetException(ctx);
        q::JS_FreeValue(ctx, exception);
    }
    q::JS_FreeValue(ctx, value);
    module_type
}

/// The request object for request `idx` of `module` (borrowed: the record keeps it), or null
/// with an exception.
unsafe fn module_request(cx: &Cx, module: *mut root::JSObject, idx: usize) -> *mut root::JSObject {
    let data = module_of(module);
    let request = data.requests[idx].get();
    if request.tag() == q::JS_TAG_OBJECT {
        return val_obj(request);
    }
    let ctx = cx.qctx();
    let m = data.module;
    let atom = q::JS_GetModuleRequestSpecifier(ctx, m, idx as c_int);
    if atom == q::JS_ATOM_NULL {
        q::JS_ThrowRangeError(ctx, c"invalid module request index".as_ptr());
        return ptr::null_mut();
    }
    let specifier = q::JS_AtomToString(ctx, atom);
    q::JS_FreeAtom(ctx, atom);
    if specifier.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    let attributes = q::JS_GetModuleRequestAttributes(ctx, m, idx as c_int);
    let module_type = module_type_of(ctx, attributes);
    q::JS_FreeValue(ctx, attributes);
    let referrer = q::JS_DupValueRT(cx.rt, obj_val(module));
    let request = new_module_request(cx, specifier, module_type, referrer, idx);
    if request.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    data.requests[idx].set(request);
    val_obj(request)
}

// Compilation.

unsafe fn compile_module(cx: &Cx, filename: &CStr, src: &[u8]) -> *mut root::JSObject {
    let flags = q::JS_EVAL_TYPE_MODULE | q::JS_EVAL_FLAG_COMPILE_ONLY | q::JS_EVAL_FLAG_NO_RESOLVE;
    let module = q::JS_Eval(
        cx.qctx(),
        src.as_ptr() as *const c_char,
        src.len() - 1,
        filename.as_ptr(),
        flags as c_int,
    );
    if module.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    new_module_object(cx, module, ModuleKind::JavaScript)
}

unsafe extern "C" fn synthetic_module_init(
    ctx: *mut q::JSContext,
    m: *mut q::JSModuleDef,
) -> c_int {
    let value = q::JS_GetModulePrivateValue(ctx, m);
    q::JS_SetModulePrivateValue(ctx, m, q::JS_UNDEFINED);
    q::JS_SetModuleExport(ctx, m, c"default".as_ptr(), value)
}

/// A synthetic module whose default export is `value` (consumed).
unsafe fn synthetic_module(cx: &Cx, name: &CStr, value: q::JSValue) -> *mut root::JSObject {
    let qctx = cx.qctx();
    let m = q::JS_NewCModule(qctx, name.as_ptr(), Some(synthetic_module_init));
    if m.is_null() || q::JS_AddModuleExport(qctx, m, c"default".as_ptr()) < 0 {
        q::JS_FreeValue(qctx, value);
        return ptr::null_mut();
    }
    q::JS_SetModulePrivateValue(qctx, m, value);
    let module = q::JS_DupValue(qctx, module_val(m));
    new_module_object(cx, module, ModuleKind::Synthetic)
}

/// A synthetic module whose default export is the JSON value of `src` (NUL-terminated).
unsafe fn compile_json_module(cx: &Cx, filename: &CStr, src: &[u8]) -> *mut root::JSObject {
    let value = q::JS_ParseJSON(
        cx.qctx(),
        src.as_ptr() as *const c_char,
        src.len() - 1,
        filename.as_ptr(),
    );
    if value.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    synthetic_module(cx, filename, value)
}

pub(crate) unsafe fn CreateDefaultExportSyntheticModule(
    cx: *mut root::JSContext,
    defaultExport: root::JS::Handle<root::JS::Value>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let value = q::JS_DupValueRT(cx.rt, (*defaultExport.ptr).to_qjs());
    synthetic_module(cx, c"<synthetic>", value)
}

pub(crate) unsafe fn CompileModule1(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<root::mozilla::Utf8Unit>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = source_buffer(
        options.base.lineno,
        (*srcBuf).units_ as *const u8,
        (*srcBuf).length_ as usize,
    );
    compile_module(cx, &options.filename, &src)
}

pub(crate) unsafe fn CompileModule(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<u16>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = padded(options.base.lineno, &utf16_source(srcBuf));
    compile_module(cx, &options.filename, &src)
}

pub(crate) unsafe fn CompileJsonModule1(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<root::mozilla::Utf8Unit>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = source_buffer(1, (*srcBuf).units_ as *const u8, (*srcBuf).length_ as usize);
    compile_json_module(cx, &options.filename, &src)
}

pub(crate) unsafe fn CompileJsonModule(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<u16>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = padded(1, &utf16_source(srcBuf));
    compile_json_module(cx, &options.filename, &src)
}

// Module records.

pub(crate) unsafe fn IsCyclicModule(module: *mut root::JSObject) -> bool {
    module_of(module).kind == ModuleKind::JavaScript
}

/// Like SpiderMonkey's `ScriptSourceObject::setPrivate`, the host's release hook runs for the old
/// value and its add-ref hook for the new one; the release hook runs again when the module is
/// freed (Q34 for JavaScript modules).
pub(crate) unsafe fn SetModulePrivate(module: *mut root::JSObject, value: *const root::JS::Value) {
    let cx = super::current();
    let data = module_of(module);
    let value = (*value).to_qjs();
    match data.kind {
        ModuleKind::JavaScript => {
            q::JS_SetModulePrivateValue(cx.base_ctx, data.module, q::JS_DupValueRT(cx.rt, value));
        }
        ModuleKind::Synthetic => {
            let old = data.private.replace(q::JS_DupValueRT(cx.rt, value));
            if old.tag() != q::JS_TAG_UNDEFINED {
                release_script_private(cx.rt, old);
            }
            q::JS_FreeValueRT(cx.rt, old);
        }
    }
    if value.tag() != q::JS_TAG_UNDEFINED {
        add_ref_script_private(cx.rt, value);
    }
}

pub(crate) unsafe fn ClearModulePrivate(module: *mut root::JSObject) {
    SetModulePrivate(module, &JSVal::from_qjs(q::JS_UNDEFINED));
}

pub(crate) unsafe fn JS_GetModulePrivate(
    module: *mut root::JSObject,
    dest: root::JS::MutableHandleValue,
) {
    let cx = super::current();
    let data = module_of(module);
    let value = match data.kind {
        ModuleKind::JavaScript => q::JS_GetModulePrivateValue(cx.base_ctx, data.module),
        ModuleKind::Synthetic => q::JS_DupValueRT(cx.rt, data.private.get()),
    };
    *dest.ptr = cx.adopt(value);
}

pub(crate) unsafe fn GetModuleScript(
    moduleRecord: root::JS::Handle<*mut root::JSObject>,
) -> *mut root::JSScript {
    let cx = super::current();
    let data = module_of(*moduleRecord.ptr);
    if data.kind != ModuleKind::JavaScript {
        return ptr::null_mut();
    }
    let script = q::JS_GetModuleScript(cx.base_ctx, data.module);
    if script.tag() != q::JS_TAG_FUNCTION_BYTECODE {
        return ptr::null_mut();
    }
    cx.adopt(script);
    script.ptr() as *mut root::JSScript
}

pub(crate) unsafe fn GetRequestedModuleType(
    cx: *mut root::JSContext,
    moduleRecord: root::JS::Handle<*mut root::JSObject>,
    index: u32,
) -> root::JS::ModuleType {
    let qctx = Cx::from_raw(cx).qctx();
    let attributes =
        q::JS_GetModuleRequestAttributes(qctx, module_of(*moduleRecord.ptr).module, index as c_int);
    let module_type = module_type_of(qctx, attributes);
    q::JS_FreeValue(qctx, attributes);
    module_type
}

// Module requests.

pub(crate) unsafe fn GetModuleRequestSpecifier(
    cx: *mut root::JSContext,
    moduleRequestArg: root::JS::Handle<*mut root::JSObject>,
) -> *mut root::JSString {
    let cx = Cx::from_raw(cx);
    let data = request_data(*moduleRequestArg.ptr).expect("not a module request");
    cx.retain(JSVal::from_qjs(data.specifier)).to_string()
}

pub(crate) unsafe fn GetModuleRequestType(
    cx: *mut root::JSContext,
    moduleRequestArg: root::JS::Handle<*mut root::JSObject>,
) -> root::JS::ModuleType {
    let _ = cx;
    request_data(*moduleRequestArg.ptr)
        .expect("not a module request")
        .module_type
}

pub(crate) unsafe fn ModuleRequestIsSourcePhase(
    cx: *mut root::JSContext,
    moduleRequestArg: root::JS::Handle<*mut root::JSObject>,
) -> bool {
    let _ = (cx, moduleRequestArg);
    false
}

// Loading (ECMA-262 LoadRequestedModules and FinishLoadingImportedModule).

unsafe fn new_load_state(
    cx: &Cx,
    promise: q::JSValue,
    resolved: root::JS::LoadModuleResolvedCallback,
    rejected: root::JS::LoadModuleRejectedCallback,
    host_defined: root::JS::Handle<root::JS::Value>,
) -> q::JSValue {
    let class = classes(cx.rt).state;
    let obj = q::JS_NewObjectProtoClass(cx.qctx(), q::JS_NULL, class);
    if obj.tag() == q::JS_TAG_EXCEPTION {
        q::JS_FreeValueRT(cx.rt, promise);
        return obj;
    }
    let data = Box::new(LoadState {
        promise,
        resolved,
        rejected,
        is_loading: Cell::new(true),
        pending: Cell::new(1),
        visited: Default::default(),
        visited_set: Default::default(),
        host_defined: q::JS_DupValueRT(cx.rt, (*host_defined.ptr).to_qjs()),
    });
    q::JS_SetOpaque(obj, Box::into_raw(data) as *mut c_void);
    obj
}

/// Settles the load of `state` (borrowed).
unsafe fn settle_load(cx: &Cx, state: q::JSValue, error: Option<q::JSValue>) -> bool {
    let data = state_of(state).expect("not a graph loading state");
    if data.promise.tag() == q::JS_TAG_OBJECT {
        let value = error.unwrap_or(q::JS_UNDEFINED);
        return q::JS_SettlePromise(cx.qctx(), data.promise, value, error.is_some() as c_int) >= 0;
    }
    let host_defined = JSVal::from_qjs(data.host_defined);
    match error {
        None => data.resolved.unwrap()(cx.as_raw(), handle(&host_defined)),
        Some(error) => {
            let error = JSVal::from_qjs(error);
            data.rejected.unwrap()(cx.as_raw(), handle(&host_defined), handle(&error))
        }
    }
}

/// HostLoadImportedModule: asks the embedder's load hook for `request`; the load fails with the
/// hook's exception if it returns false.
unsafe fn host_load_imported_module(
    cx: &Cx,
    referrer: *mut root::JSScript,
    request: *mut root::JSObject,
    host_defined: JSVal,
    payload: JSVal,
) -> bool {
    let ok = match hooks(cx.rt).load {
        Some(hook) => hook(
            cx.as_raw(),
            handle(&referrer),
            handle(&request),
            handle(&host_defined),
            handle(&payload),
            0,
            root::JS::ColumnNumberOneOrigin { _base: 1 },
        ),
        None => {
            q::JS_ThrowTypeError(cx.qctx(), c"Module load hook not set".as_ptr());
            false
        }
    };
    if ok {
        return true;
    }
    let raw = cx.as_raw();
    // As in SpiderMonkey, a termination is no pending exception: the load fails with undefined
    // (Q37), not with QuickJS's uncatchable error.
    if super::exception::JS_IsExceptionPending(raw) {
        FinishLoadingImportedModuleFailedWithPendingException(raw, handle(&payload))
    } else {
        let undefined = JSVal::from_qjs(q::JS_UNDEFINED);
        FinishLoadingImportedModuleFailed(raw, handle(&payload), handle(&undefined))
    }
}

/// Fails the load of `state` with the pending exception.
unsafe fn fail_with_pending_exception(cx: &Cx, state: q::JSValue) -> bool {
    let error = cx.adopt(q::JS_GetException(cx.qctx()));
    continue_module_loading(cx, state, ptr::null_mut(), Some(error.to_qjs()))
}

/// InnerModuleLoading(state, module): `state` and `module` are borrowed.
unsafe fn inner_module_loading(cx: &Cx, state: q::JSValue, module: *mut root::JSObject) -> bool {
    // It recurses over loaded requests: a long chain loaded again would overflow the native
    // stack. SpiderMonkey checks its recursion limit here too.
    if q::JS_CheckStackOverflow(cx.qctx()) != 0 {
        return fail_with_pending_exception(cx, state);
    }
    // The load hook may finish loads before it returns (FinishLoadingImportedModule): that fills
    // `data.loaded` and continues this load, which changes `load`'s flags, pending count and
    // visited records, and may set `loaded_graph`. All of them are cells, read only through them.
    let load = state_of(state).expect("not a graph loading state");
    let data = module_of(module);
    let visited = load.visited_set.borrow().contains(&module);
    if data.kind == ModuleKind::JavaScript && !data.loaded_graph.get() && !visited {
        load.visited_set.borrow_mut().insert(module);
        let record = q::JS_DupValueRT(cx.rt, obj_val(module));
        load.visited.borrow_mut().push(record);
        let count = data.requests.len();
        load.pending.set(load.pending.get() + count);
        let referrer = GetModuleScript(handle(&module));
        for idx in 0..count {
            let qctx = cx.qctx();
            let attributes = q::JS_GetModuleRequestAttributes(qctx, data.module, idx as c_int);
            let supported = check_attributes(qctx, attributes, false);
            q::JS_FreeValue(qctx, attributes);
            let loaded = data.loaded[idx].get();
            let ok = if !supported {
                fail_with_pending_exception(cx, state)
            } else if loaded.tag() == q::JS_TAG_OBJECT {
                inner_module_loading(cx, state, val_obj(loaded))
            } else {
                let request = module_request(cx, module, idx);
                if request.is_null() {
                    fail_with_pending_exception(cx, state)
                } else {
                    let host_defined = JSVal::from_qjs(load.host_defined);
                    host_load_imported_module(
                        cx,
                        referrer,
                        request,
                        host_defined,
                        JSVal::from_qjs(state),
                    )
                }
            };
            if !ok {
                return false;
            }
            if !load.is_loading.get() {
                return true;
            }
        }
    }
    debug_assert!(load.pending.get() >= 1);
    load.pending.set(load.pending.get() - 1);
    if load.pending.get() == 0 {
        load.is_loading.set(false);
        for v in load.visited.borrow().iter() {
            module_of(val_obj(*v)).loaded_graph.set(true);
        }
        return settle_load(cx, state, None);
    }
    true
}

/// ContinueModuleLoading(state, moduleCompletion): `module` is the loaded module, or null for
/// the failure `error`.
unsafe fn continue_module_loading(
    cx: &Cx,
    state: q::JSValue,
    module: *mut root::JSObject,
    error: Option<q::JSValue>,
) -> bool {
    let load = state_of(state).expect("not a graph loading state");
    if !load.is_loading.get() {
        return true;
    }
    if !module.is_null() {
        return inner_module_loading(cx, state, module);
    }
    load.is_loading.set(false);
    settle_load(cx, state, Some(error.unwrap_or(q::JS_UNDEFINED)))
}

/// LoadRequestedModules with the state's promise or callbacks set by the caller.
unsafe fn load_requested_modules(cx: &Cx, module: *mut root::JSObject, state: q::JSValue) -> bool {
    if state.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    let state = cx.adopt(state).to_qjs();
    let load = state_of(state).expect("not a graph loading state");
    if module_of(module).kind != ModuleKind::JavaScript {
        load.is_loading.set(false);
        return settle_load(cx, state, None);
    }
    if !inner_module_loading(cx, state, module) {
        load.is_loading.set(false);
        return false;
    }
    true
}

pub(crate) unsafe fn LoadRequestedModules(
    cx: *mut root::JSContext,
    module: root::JS::Handle<*mut root::JSObject>,
    hostDefined: root::JS::Handle<root::JS::Value>,
    resolved: root::JS::LoadModuleResolvedCallback,
    rejected: root::JS::LoadModuleRejectedCallback,
) -> bool {
    let cx = Cx::from_raw(cx);
    let state = new_load_state(cx, q::JS_UNDEFINED, resolved, rejected, hostDefined);
    load_requested_modules(cx, *module.ptr, state)
}

pub(crate) unsafe fn LoadRequestedModules1(
    cx: *mut root::JSContext,
    module: root::JS::Handle<*mut root::JSObject>,
    hostDefined: root::JS::Handle<root::JS::Value>,
    promiseOut: root::JS::MutableHandle<*mut root::JSObject>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let mut funcs = [q::JS_UNDEFINED; 2];
    let promise = q::JS_NewPromiseCapability(cx.qctx(), funcs.as_mut_ptr());
    free_all(cx.rt, funcs);
    if promise.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *promiseOut.ptr = cx.retain(JSVal::from_qjs(promise)).to_object();
    let state = new_load_state(cx, promise, None, None, hostDefined);
    load_requested_modules(cx, *module.ptr, state)
}

/// Records `result` in the referrer's [[LoadedModules]] (requests of import() have none).
unsafe fn record_loaded_module(cx: &Cx, request: *mut root::JSObject, result: *mut root::JSObject) {
    let request = request_data(request).expect("not a module request");
    if request.referrer.tag() != q::JS_TAG_OBJECT {
        return;
    }
    let loaded = &module_of(val_obj(request.referrer)).loaded[request.index];
    if loaded.get().tag() == q::JS_TAG_OBJECT {
        debug_assert!(val_obj(loaded.get()) == result);
        return;
    }
    loaded.set(q::JS_DupValueRT(cx.rt, obj_val(result)));
}

pub(crate) unsafe fn FinishLoadingImportedModule(
    cx: *mut root::JSContext,
    referrer: root::JS::Handle<*mut root::JSScript>,
    moduleRequest: root::JS::Handle<*mut root::JSObject>,
    payload: root::JS::Handle<root::JS::Value>,
    result: root::JS::Handle<*mut root::JSObject>,
    usePromise: bool,
) -> bool {
    let _ = (referrer, usePromise);
    let cx = Cx::from_raw(cx);
    record_loaded_module(cx, *moduleRequest.ptr, *result.ptr);
    let payload = (*payload.ptr).to_qjs();
    if state_of(payload).is_none() {
        // ContinueDynamicImport: Servo uses FinishLoadingDynamicImportedModule instead.
        q::JS_ThrowInternalError(
            cx.qctx(),
            c"FinishLoadingImportedModule for import() is not implemented".as_ptr(),
        );
        return false;
    }
    continue_module_loading(cx, payload, *result.ptr, None)
}

pub(crate) unsafe fn FinishLoadingDynamicImportedModule(
    cx: *mut root::JSContext,
    referrer: root::JS::Handle<*mut root::JSScript>,
    moduleRequest: root::JS::Handle<*mut root::JSObject>,
    payload: root::JS::Handle<root::JS::Value>,
    result: root::JS::Handle<*mut root::JSObject>,
) -> bool {
    let _ = (referrer, payload);
    record_loaded_module(Cx::from_raw(cx), *moduleRequest.ptr, *result.ptr);
    true
}

pub(crate) unsafe fn FinishLoadingImportedModuleFailed(
    cx: *mut root::JSContext,
    payloadArg: root::JS::Handle<root::JS::Value>,
    error: root::JS::Handle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let payload = (*payloadArg.ptr).to_qjs();
    let error = (*error.ptr).to_qjs();
    if state_of(payload).is_some() {
        return continue_module_loading(cx, payload, ptr::null_mut(), Some(error));
    }
    // ContinueDynamicImport: rejects the promise of import().
    q::JS_SettlePromise(cx.qctx(), payload, error, 1) >= 0
}

pub(crate) unsafe fn FinishLoadingImportedModuleFailedWithPendingException(
    cx: *mut root::JSContext,
    payload: root::JS::Handle<root::JS::Value>,
) -> bool {
    let error = {
        let cx = Cx::from_raw(cx);
        cx.adopt(q::JS_GetException(cx.qctx()))
    };
    FinishLoadingImportedModuleFailed(cx, payload, handle(&error))
}

// Link and evaluate.

/// `JSModuleResolveFunc` for `JS_LinkModule`: the module that request `idx` of `m` loaded.
unsafe extern "C" fn resolve_imported_module(
    ctx: *mut q::JSContext,
    m: *mut q::JSModuleDef,
    idx: c_int,
    opaque: *mut c_void,
) -> *mut q::JSModuleDef {
    let _ = opaque;
    let loaded =
        record_of(m).and_then(|record| module_of(record).loaded.get(idx as usize).map(Cell::get));
    match loaded {
        Some(loaded) if loaded.tag() == q::JS_TAG_OBJECT => module_of(val_obj(loaded)).module,
        _ => {
            q::JS_ThrowTypeError(ctx, c"module request was not loaded".as_ptr());
            ptr::null_mut()
        }
    }
}

pub(crate) unsafe fn ModuleLink(
    cx: *mut root::JSContext,
    moduleRecord: root::JS::Handle<*mut root::JSObject>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let m = module_of(*moduleRecord.ptr).module;
    q::JS_LinkModule(cx.qctx(), m, Some(resolve_imported_module), ptr::null_mut()) >= 0
}

pub(crate) unsafe fn ModuleEvaluate(
    cx: *mut root::JSContext,
    moduleRecord: root::JS::Handle<*mut root::JSObject>,
    rval: root::JS::MutableHandleValue,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let m = module_of(*moduleRecord.ptr).module;
    if q::JS_IsModuleLinked(m) == 0 {
        q::JS_ThrowTypeError(qctx, c"module is not linked".as_ptr());
        return false;
    }
    // Returns the evaluation promise (rejected on evaluation errors); consumes a reference. As in
    // SpiderMonkey, only module code can be terminated: a terminated module rejects it with
    // undefined (Q37).
    let promise = super::promise::deferring_interrupts(cx.as_raw(), || {
        q::JS_EvalFunction(qctx, q::JS_DupValueRT(cx.rt, module_val(m)))
    });
    if promise.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *rval.ptr = cx.adopt(promise);
    true
}

pub(crate) unsafe fn ThrowOnModuleEvaluationFailure(
    cx: *mut root::JSContext,
    evaluationPromise: root::JS::Handle<*mut root::JSObject>,
    errorBehaviour: root::JS::ModuleErrorBehaviour,
) -> bool {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let promise = *evaluationPromise.ptr;
    if promise.is_null() {
        return false;
    }
    let promise = obj_val(promise);
    let state = q::JS_PromiseState(qctx, promise);
    // SpiderMonkey's asynchronous path, also taken for a pending (top-level await) evaluation,
    // which SpiderMonkey reports as failed with an undefined exception: a later rejection is
    // reported as an exception (HTML "run a module script").
    if errorBehaviour == root::JS::ModuleErrorBehaviour::ReportModuleErrorsAsync
        || state == q::JS_PROMISE_PENDING
    {
        let on_rejected =
            super::class::new_function(cx, Some(report_rejected_module), ptr::null(), 1, false, 0);
        if on_rejected.tag() == q::JS_TAG_EXCEPTION {
            return false;
        }
        let on_rejected = cx.adopt(on_rejected);
        return q::JS_PerformPromiseThen(qctx, promise, q::JS_UNDEFINED, on_rejected.to_qjs(), 0)
            .tag()
            != q::JS_TAG_EXCEPTION;
    }
    if state != q::JS_PROMISE_REJECTED {
        return true;
    }
    q::JS_MarkPromiseHandled(qctx, promise);
    q::JS_Throw(qctx, q::JS_PromiseResult(qctx, promise));
    false
}

pub(crate) unsafe fn GetModuleNamespace(
    cx: *mut root::JSContext,
    moduleRecord: root::JS::Handle<*mut root::JSObject>,
) -> *mut root::JSObject {
    let cx = Cx::from_raw(cx);
    let qctx = cx.qctx();
    let m = module_of(*moduleRecord.ptr).module;
    if q::JS_IsModuleLinked(m) == 0 {
        q::JS_ThrowTypeError(qctx, c"module is not linked".as_ptr());
        return ptr::null_mut();
    }
    let namespace = q::JS_GetModuleNamespace(qctx, m);
    adopt_object(cx, namespace)
}

// Host hooks, per runtime.

#[derive(Clone, Copy, Default)]
struct Hooks {
    preparer: crate::sys::glue::InvokeScriptPreparerHook,
    load: root::JS::ModuleLoadHook,
    metadata: root::JS::ModuleMetadataHook,
    add_ref: root::JS::ScriptPrivateReferenceHook,
    release: root::JS::ScriptPrivateReferenceHook,
}

thread_local! {
    static HOOKS: RefCell<HashMap<usize, Hooks>> = RefCell::new(HashMap::new());
}

fn hooks(rt: *mut q::JSRuntime) -> Hooks {
    HOOKS.with(|h| h.borrow().get(&(rt as usize)).copied().unwrap_or_default())
}

fn update_hooks(rt: *mut root::JSRuntime, f: impl FnOnce(&mut Hooks)) {
    HOOKS.with(|h| f(h.borrow_mut().entry(rt as usize).or_default()));
}

pub(crate) unsafe fn RegisterScriptEnvironmentPreparer(
    cx: *mut root::JSContext,
    hook: crate::sys::glue::InvokeScriptPreparerHook,
) {
    update_hooks(Cx::from_raw(cx).rt as *mut root::JSRuntime, |h| {
        h.preparer = hook
    });
}

/// What the shim passes as a `ScriptEnvironmentPreparer::Closure`: running it throws `error`.
#[repr(C)]
struct ReportClosure {
    error: JSVal,
}

pub(crate) unsafe fn RunScriptEnvironmentPreparerClosure(
    cx: *mut root::JSContext,
    closure: *mut root::js::ScriptEnvironmentPreparer_Closure,
) -> bool {
    let cx = Cx::from_raw(cx);
    let closure = &*(closure as *const ReportClosure);
    q::JS_Throw(cx.qctx(), q::JS_DupValueRT(cx.rt, closure.error.to_qjs()));
    false
}

/// Rejection handler of a module evaluation promise: reports the reason through the embedder's
/// script environment preparer, as SpiderMonkey's `OnRootModuleRejected`.
unsafe extern "C" fn report_rejected_module(
    cx: *mut root::JSContext,
    argc: u32,
    vp: *mut JSVal,
) -> bool {
    let cx = Cx::from_raw(cx);
    let mut closure = ReportClosure {
        error: if argc > 0 {
            *vp.add(2)
        } else {
            JSVal::from_qjs(q::JS_UNDEFINED)
        },
    };
    *vp = JSVal::from_qjs(q::JS_UNDEFINED);
    let realm = cx.current_realm();
    if let (Some(preparer), false) = (hooks(cx.rt).preparer, realm.is_null()) {
        let global = (*realm).global;
        preparer(
            handle(&global),
            &mut closure as *mut ReportClosure as *mut root::js::ScriptEnvironmentPreparer_Closure,
        );
    }
    true
}

pub(crate) unsafe fn SetModuleLoadHook(rt: *mut root::JSRuntime, func: root::JS::ModuleLoadHook) {
    update_hooks(rt, |h| h.load = func);
    q::JS_SetDynamicImportHook(
        rt as *mut q::JSRuntime,
        func.map(|_| dynamic_import_hook as _),
        ptr::null_mut(),
    );
}

pub(crate) unsafe fn GetModuleLoadHook(rt: *mut root::JSRuntime) -> root::JS::ModuleLoadHook {
    hooks(rt as *mut q::JSRuntime).load
}

pub(crate) unsafe fn SetModuleMetadataHook(
    rt: *mut root::JSRuntime,
    func: root::JS::ModuleMetadataHook,
) {
    update_hooks(rt, |h| h.metadata = func);
    q::JS_SetImportMetaHook(
        rt as *mut q::JSRuntime,
        func.map(|_| import_meta_hook as _),
        ptr::null_mut(),
    );
}

pub(crate) unsafe fn GetModuleMetadataHook(
    rt: *mut root::JSRuntime,
) -> root::JS::ModuleMetadataHook {
    hooks(rt as *mut q::JSRuntime).metadata
}

pub(crate) unsafe fn SetScriptPrivateReferenceHooks(
    rt: *mut root::JSRuntime,
    addRefHook: root::JS::ScriptPrivateReferenceHook,
    releaseHook: root::JS::ScriptPrivateReferenceHook,
) {
    update_hooks(rt, |h| {
        h.add_ref = addRefHook;
        h.release = releaseHook;
    });
    q::JS_SetScriptPrivateFinalizer(rt as *mut q::JSRuntime, Some(release_script_private));
}

/// Takes a reference for a script private value stored by QuickJS, like SpiderMonkey's
/// `ScriptSourceObject::setPrivate`.
pub(crate) unsafe fn add_ref_script_private(rt: *mut q::JSRuntime, value: q::JSValue) {
    if let Some(add_ref) = hooks(rt).add_ref {
        add_ref(&JSVal::from_qjs(value));
    }
}

/// `JSScriptPrivateFinalizer`: releases the reference taken by `add_ref_script_private`.
unsafe extern "C" fn release_script_private(rt: *mut q::JSRuntime, value: q::JSValue) {
    let release = HOOKS.try_with(|h| h.borrow().get(&(rt as usize)).and_then(|h| h.release));
    if let Ok(Some(release)) = release {
        release(&JSVal::from_qjs(value));
    }
}

/// `JSImportMetaHook`: Servo's HostPopulateImportMeta, called when import.meta is first used.
unsafe extern "C" fn import_meta_hook(
    ctx: *mut q::JSContext,
    m: *mut q::JSModuleDef,
    meta: q::JSValue,
    opaque: *mut c_void,
) -> c_int {
    let _ = opaque;
    let cx = super::current();
    let Some(hook) = hooks(cx.rt).metadata else {
        return 0;
    };
    // The record may be gone while code of the module still runs: make another one.
    let record = match record_of(m) {
        Some(record) => record,
        None => new_module_object(
            cx,
            q::JS_DupValue(ctx, module_val(m)),
            ModuleKind::JavaScript,
        ),
    };
    if record.is_null() {
        return -1;
    }
    let meta = val_obj(meta);
    if in_realm_of(cx, ctx, |raw| hook(raw, handle(&record), handle(&meta))) {
        0
    } else {
        ensure_exception(ctx);
        -1
    }
}

/// The private value of a module (a new reference); C modules (JSON) have none.
unsafe fn module_private(ctx: *mut q::JSContext, m: *mut q::JSModuleDef) -> q::JSValue {
    if q::JS_IsCModule(m) != 0 {
        q::JS_UNDEFINED
    } else {
        q::JS_GetModulePrivateValue(ctx, m)
    }
}

/// The private value of a frame's module `m`, or else of its script (`script_private`,
/// borrowed). Retained by the pool.
pub(crate) unsafe fn script_or_module_private(
    cx: &Cx,
    ctx: *mut q::JSContext,
    m: *mut q::JSModuleDef,
    script_private: q::JSValue,
) -> JSVal {
    if m.is_null() {
        return cx.retain(JSVal::from_qjs(script_private));
    }
    let ctx = if ctx.is_null() { cx.qctx() } else { ctx };
    cx.adopt(module_private(ctx, m))
}

/// `JSDynamicImportHook`: import() loads through the load hook (HostLoadImportedModule) with the
/// import() promise as payload; the embedder settles it.
unsafe extern "C" fn dynamic_import_hook(
    ctx: *mut q::JSContext,
    m: *mut q::JSModuleDef,
    script_private: q::JSValue,
    specifier: q::JSValue,
    attributes: q::JSValue,
    promise: q::JSValue,
    opaque: *mut c_void,
) -> c_int {
    let _ = (m, script_private, opaque);
    let cx = super::current();
    if !check_attributes(ctx, attributes, true) {
        return -1;
    }
    let module_type = module_type_of(ctx, attributes);
    let specifier = q::JS_DupValue(ctx, specifier);
    let request = new_module_request(cx, specifier, module_type, q::JS_UNDEFINED, 0);
    if request.tag() == q::JS_TAG_EXCEPTION {
        return -1;
    }
    let request = cx.adopt(request).to_object();
    let script = cx.adopt(q::JS_GetActiveScript(ctx));
    let script = if script.to_qjs().tag() == q::JS_TAG_FUNCTION_BYTECODE {
        script.to_qjs().ptr() as *mut root::JSScript
    } else {
        ptr::null_mut()
    };
    let undefined = JSVal::from_qjs(q::JS_UNDEFINED);
    let payload = JSVal::from_qjs(promise);
    let ok = in_realm_of(cx, ctx, |raw| {
        host_load_imported_module(Cx::from_raw(raw), script, request, undefined, payload)
    });
    if ok {
        0
    } else {
        ensure_exception(ctx);
        -1
    }
}
