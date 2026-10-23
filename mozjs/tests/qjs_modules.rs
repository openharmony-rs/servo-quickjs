/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! QuickJS shim: ES modules loaded the way Servo does it (script_module.rs, module_loading.rs).
//! `LoadRequestedModules` asks the load hook for each request, which "fetches" the module from
//! an in-memory map, compiles it and finishes the load; then the host links and evaluates.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::marker::PhantomData;
use std::ptr;
use std::sync::OnceLock;

use mozjs::context::JSContext;
use mozjs::jsapi::{
    self, GCReason, Heap, JSContext as RawJSContext, JSObject, JSScript, JSString,
    ModuleErrorBehaviour, ModuleType, OnNewGlobalHookOption, Value,
};
use mozjs::jsval::{Int32Value, JSVal, PrivateValue, UndefinedValue};
use mozjs::qjs;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    CompileJsonModule1, CompileModule1, GetModuleNamespace, GetRequestedModuleType,
    JS_GetPendingException, JS_NewGlobalObject, LoadRequestedModules1, ModuleEvaluate, ModuleLink,
    ThrowOnModuleEvaluationFailure,
};
use mozjs::rust::{
    transform_str_to_source_text, CompileOptionsWrapper, JSEngine, JSEngineHandle, RealmOptions,
    Runtime, SIMPLE_GLOBAL_CLASS,
};

const BASE: &str = "https://test/";

/// Servo's module map plus the "network".
#[derive(Default)]
struct Loader {
    sources: HashMap<String, String>,
    /// Module records by URL (counted, like Servo's `ModuleObject`).
    modules: HashMap<String, Box<Heap<*mut JSObject>>>,
    /// URLs by private id - 1 (a module's private value is `PrivateValue(id)`).
    urls: Vec<String>,
    /// Static imports seen by the load hook: (referencing private id, specifier, type).
    static_imports: Vec<(usize, String, ModuleType)>,
    /// import() calls seen by the load hook: (referencing private id, specifier, type).
    dynamic_imports: Vec<(usize, String, ModuleType)>,
    /// The promises of those calls, for the host to settle.
    import_promises: Vec<Box<Heap<*mut JSObject>>>,
    /// Rejection tracker calls: (promise, handled).
    rejections: Vec<(*mut c_void, bool)>,
}

thread_local!(static LOADER: RefCell<Loader> = RefCell::new(Loader::default()));

/// Drops the module map before the runtime (its `Heap`s release references).
struct LoaderGuard;

impl Drop for LoaderGuard {
    fn drop(&mut self) {
        let loader = LOADER.with(|l| std::mem::take(&mut *l.borrow_mut()));
        drop(loader);
    }
}

fn h<T>(ptr: &T) -> jsapi::Handle<T> {
    jsapi::Handle {
        _phantom_0: PhantomData,
        ptr,
    }
}

fn mh<T>(ptr: &mut T) -> jsapi::MutableHandle<T> {
    jsapi::MutableHandle {
        _phantom_0: PhantomData,
        ptr,
    }
}

fn url(specifier: &str) -> String {
    format!("{BASE}{}", specifier.trim_start_matches("./"))
}

unsafe fn string(s: *mut JSString) -> String {
    let v = qjs::JSValue::mkptr(qjs::JS_TAG_STRING, s as *mut c_void);
    let mut data = ptr::null();
    let mut wide = 0;
    let len = qjs::JS_GetStringData(v, &mut data, &mut wide);
    if wide != 0 {
        String::from_utf16(std::slice::from_raw_parts(data as *const u16, len)).unwrap()
    } else {
        std::slice::from_raw_parts(data as *const u8, len)
            .iter()
            .map(|&b| b as char)
            .collect()
    }
}

fn private_id(private: &Value) -> usize {
    if private.is_undefined() {
        0
    } else {
        private.to_private() as usize
    }
}

unsafe fn compile(
    cx: &mut JSContext,
    url: &str,
    source: &str,
    module_type: ModuleType,
) -> *mut JSObject {
    let options = CompileOptionsWrapper::new(cx, CString::new(url).unwrap(), 1);
    let mut text = transform_str_to_source_text(source);
    let record = match module_type {
        ModuleType::JSON => CompileJsonModule1(cx, options.ptr, &mut text),
        _ => CompileModule1(cx, options.ptr, &mut text),
    };
    if record.is_null() {
        return record;
    }
    LOADER.with(|l| {
        let mut l = l.borrow_mut();
        if module_type != ModuleType::JSON {
            l.urls.push(url.to_owned());
            let private = PrivateValue(l.urls.len() as *const c_void);
            jsapi::SetModulePrivate(record, &private);
        }
        l.modules.insert(url.to_owned(), Heap::boxed(record));
    });
    record
}

fn record(url: &str) -> *mut JSObject {
    LOADER.with(|l| {
        l.borrow()
            .modules
            .get(url)
            .map(|m| m.get())
            .unwrap_or(ptr::null_mut())
    })
}

/// The static imports the load hook saw from the module at `url`: specifiers and types.
fn imports_of(url: &str) -> Vec<(String, ModuleType)> {
    LOADER.with(|l| {
        let l = l.borrow();
        let id = l.urls.iter().position(|u| u == url).unwrap() + 1;
        l.static_imports
            .iter()
            .filter(|i| i.0 == id)
            .map(|i| (i.1.clone(), i.2))
            .collect()
    })
}

/// Servo's LoadRequestedModules for `root`: the promise it settles.
unsafe fn load_requested(cx: &mut JSContext, root: *mut JSObject) -> *mut JSObject {
    rooted!(&in(cx) let root = root);
    rooted!(&in(cx) let host_defined = UndefinedValue());
    rooted!(&in(cx) let mut promise = ptr::null_mut::<JSObject>());
    assert!(LoadRequestedModules1(
        cx,
        root.handle(),
        host_defined.handle(),
        promise.handle_mut()
    ));
    promise.get()
}

/// Compiles the module at `root_url` and loads its graph.
unsafe fn load_graph(cx: &mut JSContext, root_url: &str) -> *mut JSObject {
    let source = LOADER.with(|l| l.borrow().sources[root_url].clone());
    let root = compile(cx, root_url, &source, ModuleType::JavaScript);
    assert!(!root.is_null(), "{root_url} failed to compile");
    let promise = load_requested(cx, root);
    assert_eq!(promise_state(promise), qjs::JS_PROMISE_FULFILLED);
    root
}

/// The private id of a referrer.
unsafe fn referrer_id(referrer: jsapi::Handle<*mut JSScript>) -> usize {
    if (*referrer.ptr).is_null() {
        return 0;
    }
    let mut private = UndefinedValue();
    mozjs::glue::JS_GetScriptPrivate(*referrer.ptr, mh(&mut private));
    private_id(&private)
}

/// Servo's HostLoadImportedModule. Static imports load synchronously from the module map or the
/// sources; import() calls are recorded for the test to continue, and those of specifiers
/// starting with "throw" fail.
unsafe extern "C" fn load_hook(
    cx: *mut RawJSContext,
    referrer: jsapi::Handle<*mut JSScript>,
    request: jsapi::Handle<*mut JSObject>,
    _host_defined: jsapi::HandleValue,
    payload: jsapi::HandleValue,
    _line: u32,
    _column: jsapi::ColumnNumberOneOrigin,
) -> bool {
    let specifier = string(jsapi::GetModuleRequestSpecifier(cx, request));
    let module_type = jsapi::GetModuleRequestType(cx, request);
    let id = referrer_id(referrer);
    let payload_obj = (*payload.ptr).to_object();
    if jsapi::IsPromiseObject(h(&payload_obj)) {
        LOADER.with(|l| {
            let mut l = l.borrow_mut();
            l.dynamic_imports.push((id, specifier.clone(), module_type));
            l.import_promises.push(Heap::boxed(payload_obj));
        });
        if specifier.starts_with("throw") {
            let error = Int32Value(42);
            jsapi::JS_SetPendingException(cx, h(&error), jsapi::ExceptionStackBehavior::Capture);
            return false;
        }
        return true;
    }
    LOADER.with(|l| {
        l.borrow_mut()
            .static_imports
            .push((id, specifier.clone(), module_type))
    });
    let url = url(&specifier);
    let mut found = record(&url);
    if found.is_null() {
        let source = LOADER.with(|l| l.borrow().sources.get(&url).cloned());
        let Some(source) = source else {
            let error = UndefinedValue();
            return jsapi::FinishLoadingImportedModuleFailed(cx, payload, h(&error));
        };
        let mut context = JSContext::from_ptr(std::ptr::NonNull::new(cx).unwrap());
        found = compile(&mut context, &url, &source, module_type);
        if found.is_null() {
            return jsapi::FinishLoadingImportedModuleFailedWithPendingException(cx, payload);
        }
    }
    assert_eq!(
        jsapi::IsCyclicModule(found),
        module_type == ModuleType::JavaScript
    );
    jsapi::FinishLoadingImportedModule(cx, referrer, request, payload, h(&found), true)
}

/// Servo's HostPopulateImportMeta: `import.meta.url`, and `import.meta.id`, the module's
/// private id.
unsafe extern "C" fn metadata_hook(
    cx: *mut RawJSContext,
    record: jsapi::Handle<*mut JSObject>,
    meta: jsapi::Handle<*mut JSObject>,
) -> bool {
    let mut private = UndefinedValue();
    mozjs::glue::JS_GetModulePrivate(*record.ptr, mh(&mut private));
    let id = private_id(&private);
    let url = LOADER.with(|l| l.borrow().urls[id - 1].clone());
    let url = jsapi::JS_NewStringCopyN(cx, url.as_ptr() as *const _, url.len());
    let id = Int32Value(id as i32);
    jsapi::JS_DefineProperty4(
        cx,
        meta,
        c"url".as_ptr(),
        h(&url),
        jsapi::JSPROP_ENUMERATE as u32,
    ) && jsapi::JS_DefineProperty(
        cx,
        meta,
        c"id".as_ptr(),
        h(&id),
        jsapi::JSPROP_ENUMERATE as u32,
    )
}

/// Returns the private id of the innermost script or module.
unsafe extern "C" fn caller_id(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = jsapi::CallArgs::from_vp(vp, argc);
    let mut private = UndefinedValue();
    mozjs::glue::JS_GetScriptedCallerPrivate(cx, mh(&mut private));
    args.rval().set(Int32Value(private_id(&private) as i32));
    true
}

unsafe extern "C" fn rejection_tracker(
    _ctx: *mut qjs::JSContext,
    promise: qjs::JSValue,
    _reason: qjs::JSValue,
    is_handled: std::ffi::c_int,
    _opaque: *mut c_void,
) {
    LOADER.with(|l| {
        l.borrow_mut()
            .rejections
            .push((promise.ptr(), is_handled != 0))
    });
}

unsafe fn install_hooks(cx: &mut JSContext) {
    let rt = jsapi::JS_GetRuntime(cx.raw_cx());
    jsapi::SetModuleLoadHook(rt, Some(load_hook));
    jsapi::SetModuleMetadataHook(rt, Some(metadata_hook));
    assert!(jsapi::GetModuleLoadHook(rt).is_some());
    let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
    assert!(!jsapi::JS_DefineFunction(
        cx.raw_cx(),
        h(&global),
        c"callerId".as_ptr(),
        Some(caller_id),
        0,
        0
    )
    .is_null());
    qjs::JS_SetHostPromiseRejectionTracker(
        rt as *mut qjs::JSRuntime,
        Some(rejection_tracker),
        ptr::null_mut(),
    );
}

fn add_sources(sources: &[(&str, &str)]) {
    LOADER.with(|l| {
        let mut l = l.borrow_mut();
        for (name, source) in sources {
            l.sources.insert(url(name), source.to_string());
        }
    });
}

unsafe fn get(cx: &mut JSContext, obj: *mut JSObject, name: &std::ffi::CStr) -> JSVal {
    let mut v = UndefinedValue();
    assert!(jsapi::JS_GetProperty(
        cx.raw_cx(),
        h(&obj),
        name.as_ptr(),
        mh(&mut v)
    ));
    v
}

unsafe fn namespace(cx: &mut JSContext, url: &str) -> *mut JSObject {
    rooted!(&in(cx) let record = record(url));
    let ns = GetModuleNamespace(cx, record.handle());
    assert!(!ns.is_null());
    ns
}

fn promise_state(promise: *mut JSObject) -> qjs::JSPromiseStateEnum {
    // QuickJS reads the state without the context.
    unsafe {
        qjs::JS_PromiseState(
            ptr::null_mut(),
            qjs::JSValue::mkptr(qjs::JS_TAG_OBJECT, promise as *mut c_void),
        )
    }
}

/// Runs QuickJS's own job queue (Servo's job queue hook is not installed here).
unsafe fn run_jobs(cx: &mut JSContext) {
    let rt = jsapi::JS_GetRuntime(cx.raw_cx()) as *mut qjs::JSRuntime;
    let mut ctx = ptr::null_mut();
    while qjs::JS_ExecutePendingJob(rt, &mut ctx) > 0 {}
}

unsafe fn pending_exception(cx: &mut JSContext) -> JSVal {
    rooted!(&in(cx) let mut exception = UndefinedValue());
    assert!(JS_GetPendingException(cx, exception.handle_mut()));
    jsapi::JS_ClearPendingException(cx.raw_cx());
    exception.get()
}

unsafe fn message(cx: &mut JSContext, error: JSVal) -> String {
    string(get(cx, error.to_object(), c"message").to_string())
}

unsafe fn error_name(cx: &mut JSContext, error: JSVal) -> String {
    string(get(cx, error.to_object(), c"name").to_string())
}

/// Compiles and runs a classic script with private id `id`, like Servo's `evaluate_script`.
unsafe fn run_classic(cx: &mut JSContext, url: &str, source: &str, id: usize) {
    assert!(execute_classic(cx, url, source, id));
}

/// Runs a classic script; returns whether it completed.
unsafe fn execute_classic(cx: &mut JSContext, url: &str, source: &str, id: usize) -> bool {
    let options = CompileOptionsWrapper::new(cx, CString::new(url).unwrap(), 1);
    let script = mozjs::rust::wrappers2::Compile1(
        cx,
        options.ptr,
        &mut transform_str_to_source_text(source),
    );
    assert!(!script.is_null());
    let private = PrivateValue(id as *const c_void);
    jsapi::SetScriptPrivate(script, &private);
    rooted!(&in(cx) let script: *mut JSScript = script);
    rooted!(&in(cx) let mut rval = UndefinedValue());
    mozjs::rust::wrappers2::JS_ExecuteScript(cx, script.handle(), rval.handle_mut())
}

/// The engine can be initialized once per process; the tests share it and never shut it down.
fn engine() -> JSEngineHandle {
    static ENGINE: OnceLock<JSEngineHandle> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let engine = JSEngine::init().unwrap();
            let handle = engine.handle();
            std::mem::forget(engine);
            handle
        })
        .clone()
}

/// Runs `f` in a new global's realm (and runtime) with the module hooks installed.
fn with_realm(f: impl FnOnce(&mut JSContext)) {
    let mut runtime = Runtime::new(engine());
    let _loader = LoaderGuard;
    let cx = runtime.cx();
    let options = RealmOptions::default();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*options,
        ));
        let mut realm = mozjs::realm::AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        install_hooks(cx);
        f(cx);
    }
}

#[test]
fn module_graph() {
    add_sources(&[
        ("root.js", "import { a, c } from './a.js';\nimport { b } from './b.js';\nexport const sum = a + b + c;\nexport const id = import.meta.id;\nexport const sameMeta = import.meta === import.meta;"),
        ("a.js", "export { c } from './c.js';\nexport const a = 1;"),
        ("b.js", "export const b = 2;"),
        ("c.js", "export const c = 4;\nexport const metaId = import.meta.id;\nexport const metaUrl = import.meta.url;\nexport const keys = Object.keys(import.meta).join();\nexport const caller = callerId();"),
    ]);
    with_realm(|cx| unsafe {
        let root_url = url("root.js");
        {
            let root = load_graph(cx, &root_url);
            assert_eq!(
                imports_of(&root_url),
                vec![
                    ("./a.js".to_owned(), ModuleType::JavaScript),
                    ("./b.js".to_owned(), ModuleType::JavaScript)
                ]
            );
            assert_eq!(imports_of(&url("a.js")).len(), 1);
            assert!(imports_of(&url("b.js")).is_empty());
            // Loading again finds every module loaded.
            let loads = LOADER.with(|l| l.borrow().static_imports.len());
            let promise = load_requested(cx, root);
            assert_eq!(promise_state(promise), qjs::JS_PROMISE_FULFILLED);
            assert_eq!(LOADER.with(|l| l.borrow().static_imports.len()), loads);

            rooted!(&in(cx) let root = root);
            assert!(ModuleLink(cx, root.handle()));
            rooted!(&in(cx) let mut promise = UndefinedValue());
            assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
            assert!(promise.get().is_object());
            assert_eq!(
                promise_state(promise.get().to_object()),
                qjs::JS_PROMISE_FULFILLED
            );
            rooted!(&in(cx) let promise = promise.get().to_object());
            assert!(ThrowOnModuleEvaluationFailure(
                cx,
                promise.handle(),
                ModuleErrorBehaviour::ThrowModuleErrorsSync
            ));
        }

        // Only the module map keeps the records alive now.
        mozjs::quickjs::drain_autorelease_pool();
        jsapi::JS_GC(cx.raw_cx(), GCReason::API);

        let ns = namespace(cx, &root_url);
        assert_eq!(get(cx, ns, c"sum").to_int32(), 7);
        assert_eq!(get(cx, ns, c"id").to_int32(), 1);
        assert!(get(cx, ns, c"sameMeta").to_boolean());
        let c_id = LOADER.with(|l| {
            l.borrow()
                .urls
                .iter()
                .position(|u| *u == url("c.js"))
                .unwrap()
                + 1
        });
        let ns = namespace(cx, &url("c.js"));
        assert_eq!(get(cx, ns, c"metaId").to_int32(), c_id as i32);
        assert_eq!(string(get(cx, ns, c"keys").to_string()), "url,id");
        assert_eq!(string(get(cx, ns, c"metaUrl").to_string()), url("c.js"));
        assert_eq!(get(cx, ns, c"caller").to_int32(), c_id as i32);

        let mut private = UndefinedValue();
        mozjs::glue::JS_GetModulePrivate(record(&root_url), mh(&mut private));
        assert_eq!(private_id(&private), 1);
    });
}

#[test]
fn import_meta_after_record_freed() {
    with_realm(|cx| unsafe {
        let url = url("late.js");
        let source = "export function id() { return import.meta.id; }";
        rooted!(&in(cx) let mut module = compile(cx, &url, source, ModuleType::JavaScript));
        assert!(ModuleLink(cx, module.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, module.handle(), promise.handle_mut()));
        let ns = GetModuleNamespace(cx, module.handle());
        rooted!(&in(cx) let id = get(cx, ns, c"id"));

        // import.meta is made on first use, after the module record is gone.
        LOADER.with(|l| l.borrow_mut().modules.clear());
        module.set(ptr::null_mut());
        mozjs::quickjs::drain_autorelease_pool();
        jsapi::JS_GC(cx.raw_cx(), GCReason::API);

        rooted!(&in(cx) let mut rval = UndefinedValue());
        rooted!(&in(cx) let this = UndefinedValue());
        let args = jsapi::HandleValueArray {
            length_: 0,
            elements_: ptr::null(),
        };
        assert!(mozjs::rust::wrappers2::Call(
            cx,
            this.handle(),
            id.handle(),
            &args,
            rval.handle_mut()
        ));
        assert_eq!(rval.get().to_int32(), 1);
    });
}

#[test]
fn cyclic_imports() {
    add_sources(&[
        ("main.js", "import { getB } from './a.js';\nexport const result = getB();"),
        ("a.js", "import { b } from './b.js';\nexport const a = 1;\nexport function getB() { return b; }"),
        ("b.js", "import { a } from './a.js';\nexport const b = 2;\nexport function getA() { return a; }"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(jsapi::IsCyclicModule(root.get()));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        assert_eq!(
            promise_state(promise.get().to_object()),
            qjs::JS_PROMISE_FULFILLED
        );
        let ns = namespace(cx, &url("main.js"));
        assert_eq!(get(cx, ns, c"result").to_int32(), 2);
        let ns_b = namespace(cx, &url("b.js"));
        rooted!(&in(cx) let get_a = get(cx, ns_b, c"getA"));
        rooted!(&in(cx) let mut rval = UndefinedValue());
        rooted!(&in(cx) let this = UndefinedValue());
        let args = jsapi::HandleValueArray {
            length_: 0,
            elements_: ptr::null(),
        };
        assert!(mozjs::rust::wrappers2::Call(
            cx,
            this.handle(),
            get_a.handle(),
            &args,
            rval.handle_mut()
        ));
        assert_eq!(rval.get().to_int32(), 1);
    });
}

#[test]
fn evaluation_error() {
    add_sources(&[
        (
            "main.js",
            "import './bad.js';\nexport const unreachable = 1;",
        ),
        ("bad.js", "throw new Error('boom');"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        let promise_obj = promise.get().to_object();
        assert_eq!(promise_state(promise_obj), qjs::JS_PROMISE_REJECTED);
        rooted!(&in(cx) let promise = promise_obj);
        assert!(!ThrowOnModuleEvaluationFailure(
            cx,
            promise.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        let error = pending_exception(cx);
        assert_eq!(message(cx, error), "boom");

        // The rejection was reported as unhandled, then as handled; so were those of QuickJS's
        // internal promises.
        let rejections = LOADER.with(|l| l.borrow().rejections.clone());
        let ours: Vec<bool> = rejections
            .iter()
            .filter(|r| r.0 == promise_obj as *mut c_void)
            .map(|r| r.1)
            .collect();
        assert_eq!(ours, vec![false, true]);
        assert_eq!(
            rejections.iter().filter(|r| r.1).count(),
            rejections.iter().filter(|r| !r.1).count()
        );

        // Evaluating again gives the same rejection.
        rooted!(&in(cx) let mut again = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), again.handle_mut()));
        rooted!(&in(cx) let again = again.get().to_object());
        assert!(!ThrowOnModuleEvaluationFailure(
            cx,
            again.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        let error = pending_exception(cx);
        assert_eq!(message(cx, error), "boom");
    });
}

#[test]
fn top_level_await() {
    add_sources(&[
        (
            "main.js",
            "import { later } from './tla.js';\nexport const seen = later;",
        ),
        ("tla.js", "export let later = 1;\nawait null;\nlater = 2;"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        let promise_obj = promise.get().to_object();
        assert_eq!(promise_state(promise_obj), qjs::JS_PROMISE_PENDING);
        rooted!(&in(cx) let promise = promise_obj);
        assert!(ThrowOnModuleEvaluationFailure(
            cx,
            promise.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        assert!(!jsapi::JS_IsExceptionPending(cx.raw_cx()));

        run_jobs(cx);
        assert_eq!(promise_state(promise_obj), qjs::JS_PROMISE_FULFILLED);
        let ns = namespace(cx, &url("main.js"));
        assert_eq!(get(cx, ns, c"seen").to_int32(), 2);
        assert!(REPORTED.with(|r| r.borrow().is_empty()));
    });
}

thread_local!(static REPORTED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) });

/// Servo's script environment preparer: runs the closure and reports its exception.
unsafe extern "C" fn prepare_and_report(
    _global: jsapi::HandleObject,
    closure: *mut jsapi::ScriptEnvironmentPreparer_Closure,
) {
    let mut context = mozjs::rust::Runtime::get()
        .map(|cx| JSContext::from_ptr(cx))
        .unwrap();
    let cx = &mut context;
    if !mozjs::glue::RunScriptEnvironmentPreparerClosure(cx.raw_cx(), closure) {
        let error = pending_exception(cx);
        REPORTED.with(|r| r.borrow_mut().push(error.to_int32()));
    }
}

#[test]
fn top_level_await_rejection_is_reported() {
    add_sources(&[
        ("main.js", "import './tla.js';"),
        ("tla.js", "await null;\nthrow 5;"),
    ]);
    with_realm(|cx| unsafe {
        mozjs::glue::RegisterScriptEnvironmentPreparer(cx.raw_cx(), Some(prepare_and_report));
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        rooted!(&in(cx) let promise = promise.get().to_object());
        assert!(ThrowOnModuleEvaluationFailure(
            cx,
            promise.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        run_jobs(cx);
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_REJECTED);
        assert_eq!(REPORTED.with(|r| r.borrow().clone()), [5]);
        // Reported, not an unhandled rejection.
        assert!(LOADER.with(|l| l
            .borrow()
            .rejections
            .iter()
            .all(|&(p, handled)| p != promise.get() as *mut c_void || handled)));
    });
}

unsafe extern "C" fn terminate(_: *mut RawJSContext) -> bool {
    false
}

/// `requestTermination()`: requests an interrupt, which the next poll handles.
unsafe extern "C" fn request_termination(cx: *mut RawJSContext, _: u32, _: *mut Value) -> bool {
    jsapi::JS_RequestInterruptCallback(cx);
    true
}

/// Defines `requestTermination()` and an interrupt callback that terminates: `stop()` terminates
/// the module that calls it, inside its loop.
unsafe fn add_termination(cx: &mut JSContext, global: *mut JSObject) {
    assert!(jsapi::JS_AddInterruptCallback(cx.raw_cx(), Some(terminate)));
    assert!(!jsapi::JS_DefineFunction(
        cx.raw_cx(),
        h(&global),
        c"requestTermination".as_ptr(),
        Some(request_termination),
        0,
        0
    )
    .is_null());
    run_classic(
        cx,
        "stop.js",
        "function stop() { requestTermination(); for (let i = 0; i < 1e5; i++) {} }",
        0,
    );
}

/// QuickJS's pending exception, which `JS_IsExceptionPending` clears if it is a termination: `Some`
/// with whether it is a termination. `function` is a function of the realm, which QuickJS maps to
/// its context.
unsafe fn qjs_pending(function: *mut JSObject) -> Option<bool> {
    let realm = qjs::JS_GetObjectRealm(qjs::JSValue::mkptr(
        qjs::JS_TAG_OBJECT,
        function as *mut c_void,
    ));
    assert!(!realm.is_null());
    (qjs::JS_HasException(realm) != 0).then(|| qjs::JS_HasUncatchableException(realm) != 0)
}

/// A module evaluation that is terminated, by an interrupt callback that returns false, rejects
/// its promise with undefined and leaves no exception pending, as in SpiderMonkey; no more of the
/// graph runs (Q37).
#[test]
fn terminated_evaluation() {
    add_sources(&[
        (
            "main.js",
            "import './stopping.js';\nglobalThis.mainRan = true;",
        ),
        ("stopping.js", "stop();\nglobalThis.stoppingRan = true;"),
        ("tla.js", "await null;\nstop();\nglobalThis.tlaRan = true;"),
        (
            "tla-before-await.js",
            "stop();\nawait null;\nglobalThis.tlaRan = true;",
        ),
    ]);
    with_realm(|cx| unsafe {
        let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        add_termination(cx, global);
        let function = get(cx, global, c"Object").to_object();

        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        assert_eq!(qjs_pending(function), None);
        rooted!(&in(cx) let evaluation = promise.get().to_object());
        assert_eq!(promise_state(evaluation.get()), qjs::JS_PROMISE_REJECTED);
        assert!(!ThrowOnModuleEvaluationFailure(
            cx,
            evaluation.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        assert!(pending_exception(cx).is_undefined());
        run_jobs(cx);
        assert!(get(cx, global, c"stoppingRan").is_undefined());
        assert!(get(cx, global, c"mainRan").is_undefined());

        // A later evaluation gets the same rejected promise, and script still runs.
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        assert_eq!(promise.get().to_object(), evaluation.get());
        run_classic(cx, "after.js", "globalThis.afterRan = true;", 0);
        assert!(get(cx, global, c"afterRan").to_boolean());

        // Before top-level await, as without it.
        rooted!(&in(cx) let tla = load_graph(cx, &url("tla-before-await.js")));
        assert!(ModuleLink(cx, tla.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, tla.handle(), promise.handle_mut()));
        assert_eq!(qjs_pending(function), None);
        rooted!(&in(cx) let evaluation = promise.get().to_object());
        assert_eq!(promise_state(evaluation.get()), qjs::JS_PROMISE_REJECTED);
        assert!(!ThrowOnModuleEvaluationFailure(
            cx,
            evaluation.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        assert!(pending_exception(cx).is_undefined());

        // After top-level await, the termination leaves the job, uncaught, and the evaluation
        // pending, as SpiderMonkey leaves a terminated async function.
        rooted!(&in(cx) let tla = load_graph(cx, &url("tla.js")));
        assert!(ModuleLink(cx, tla.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, tla.handle(), promise.handle_mut()));
        run_jobs(cx);
        assert_eq!(qjs_pending(function), Some(true));
        assert!(!jsapi::JS_IsExceptionPending(cx.raw_cx()));
        assert_eq!(
            promise_state(promise.get().to_object()),
            qjs::JS_PROMISE_PENDING
        );
        assert!(get(cx, global, c"tlaRan").is_undefined());
    });
}

/// A module that runs after an asynchronous dependency, and is terminated, rejects with undefined,
/// as do the modules that import it; the other modules waiting for that dependency still run, as
/// in SpiderMonkey (Q37).
#[test]
fn terminated_async_evaluation() {
    add_sources(&[
        ("dependency.js", "await null;"),
        (
            "stopping.js",
            "import './dependency.js';\nstop();\nglobalThis.stoppingRan = true;",
        ),
        (
            "other.js",
            "import './dependency.js';\nglobalThis.otherRan = true;",
        ),
        (
            "main.js",
            "import './stopping.js';\nimport './other.js';\nglobalThis.mainRan = true;",
        ),
        ("tla-dependency.js", "await null;"),
        (
            "tla-stopping.js",
            "import './tla-dependency.js';\nstop();\nawait null;\nglobalThis.stoppingRan = true;",
        ),
        (
            "tla-other.js",
            "import './tla-dependency.js';\nglobalThis.otherRan = true;",
        ),
        (
            "tla-main.js",
            "import './tla-stopping.js';\nimport './tla-other.js';\nglobalThis.mainRan = true;",
        ),
    ]);
    with_realm(|cx| unsafe {
        let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        add_termination(cx, global);
        let function = get(cx, global, c"Object").to_object();
        for main in ["main.js", "tla-main.js"] {
            run_classic(cx, "reset.js", "globalThis.otherRan = false;", 0);
            rooted!(&in(cx) let root = load_graph(cx, &url(main)));
            assert!(ModuleLink(cx, root.handle()));
            rooted!(&in(cx) let mut promise = UndefinedValue());
            assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
            rooted!(&in(cx) let evaluation = promise.get().to_object());
            assert_eq!(
                promise_state(evaluation.get()),
                qjs::JS_PROMISE_PENDING,
                "{main}"
            );
            run_jobs(cx);
            assert_eq!(qjs_pending(function), None, "{main}");
            assert_eq!(
                promise_state(evaluation.get()),
                qjs::JS_PROMISE_REJECTED,
                "{main}"
            );
            assert!(!ThrowOnModuleEvaluationFailure(
                cx,
                evaluation.handle(),
                ModuleErrorBehaviour::ThrowModuleErrorsSync
            ));
            assert!(pending_exception(cx).is_undefined(), "{main}");
            assert!(get(cx, global, c"stoppingRan").is_undefined(), "{main}");
            assert!(get(cx, global, c"mainRan").is_undefined(), "{main}");
            assert!(get(cx, global, c"otherRan").to_boolean(), "{main}");
        }
    });
}

/// While only built-ins run, `ModuleEvaluate` leaves a requested interrupt pending, as SpiderMonkey
/// checks for interrupts only in script and Servo expects the evaluation to succeed (Q36, Q37):
/// only module code is terminated.
#[test]
fn module_evaluation_defers_interrupts() {
    add_sources(&[("plain.js", "globalThis.plainRan = true;")]);
    with_realm(|cx| unsafe {
        let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        add_termination(cx, global);
        rooted!(&in(cx) let root = load_graph(cx, &url("plain.js")));
        assert!(ModuleLink(cx, root.handle()));

        // Promise creation defers the interrupt too, at QuickJS's next poll after 10,000 calls
        // at most, and then QuickJS polls at every call: at the first one of the evaluation.
        jsapi::JS_RequestInterruptCallback(cx.raw_cx());
        let executor = ptr::null_mut::<JSObject>();
        for _ in 0..20_000 {
            assert!(!jsapi::NewPromiseObject(cx.raw_cx(), h(&executor)).is_null());
        }
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        rooted!(&in(cx) let evaluation = promise.get().to_object());
        assert_eq!(promise_state(evaluation.get()), qjs::JS_PROMISE_REJECTED);
        assert!(!ThrowOnModuleEvaluationFailure(
            cx,
            evaluation.handle(),
            ModuleErrorBehaviour::ThrowModuleErrorsSync
        ));
        assert!(pending_exception(cx).is_undefined());
        assert!(get(cx, global, c"plainRan").is_undefined());
    });
}

/// An asynchronous module subscribes to its body's promise with PerformPromiseThen, as in the
/// spec: without looking up the promise's `constructor`, a getter that could run script after a
/// termination (Q37).
#[test]
fn async_module_does_not_look_up_constructor() {
    add_sources(&[
        ("awaiting.js", "await null;\nglobalThis.awaited = true;"),
        (
            "after-awaiting.js",
            "import './awaiting.js';\nglobalThis.afterRan = true;",
        ),
    ]);
    with_realm(|cx| unsafe {
        let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        run_classic(
            cx,
            "constructor.js",
            "globalThis.lookups = 0;
             Object.defineProperty(Promise.prototype, 'constructor', {
               get() { globalThis.lookups++; return Promise; }, configurable: true });",
            0,
        );
        rooted!(&in(cx) let root = load_graph(cx, &url("after-awaiting.js")));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        run_jobs(cx);
        assert_eq!(
            promise_state(promise.get().to_object()),
            qjs::JS_PROMISE_FULFILLED
        );
        assert!(get(cx, global, c"afterRan").to_boolean());
        assert_eq!(get(cx, global, c"lookups").to_int32(), 0);
    });
}

#[test]
fn json_modules() {
    add_sources(&[
        ("main.js", "import data from './data.json' with { type: 'json' };\nexport const second = data.list[1];\nexport { default as raw } from './data.json' with { type: 'json' };"),
        ("data.json", "{ \"list\": [10, 20, 30] }"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert_eq!(
            imports_of(&url("main.js")),
            vec![
                ("./data.json".to_owned(), ModuleType::JSON),
                ("./data.json".to_owned(), ModuleType::JSON)
            ]
        );
        let json = record(&url("data.json"));
        assert!(!jsapi::IsCyclicModule(json));
        // A synthetic module has nothing to load.
        let promise = load_requested(cx, json);
        assert_eq!(promise_state(promise), qjs::JS_PROMISE_FULFILLED);

        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        assert_eq!(
            promise_state(promise.get().to_object()),
            qjs::JS_PROMISE_FULFILLED
        );
        let ns = namespace(cx, &url("main.js"));
        assert_eq!(get(cx, ns, c"second").to_int32(), 20);
        assert!(get(cx, ns, c"raw").is_object());
        let json_ns = namespace(cx, &url("data.json"));
        assert_eq!(
            get(cx, json_ns, c"default").to_object(),
            get(cx, ns, c"raw").to_object()
        );

        // A JSON module evaluated on its own (as by a dynamic import).
        let options = CompileOptionsWrapper::new(cx, c"https://test/solo.json".to_owned(), 1);
        rooted!(&in(cx) let solo = CompileJsonModule1(cx, options.ptr, &mut transform_str_to_source_text("[1, 2]")));
        assert!(!solo.get().is_null());
        let private = PrivateValue(99 as *const c_void);
        jsapi::SetModulePrivate(solo.get(), &private);
        let mut read = UndefinedValue();
        mozjs::glue::JS_GetModulePrivate(solo.get(), mh(&mut read));
        assert_eq!(private_id(&read), 99);
        assert!(ModuleLink(cx, solo.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, solo.handle(), promise.handle_mut()));
        let ns = GetModuleNamespace(cx, solo.handle());
        let list = get(cx, ns, c"default").to_object();
        assert_eq!(get(cx, list, c"length").to_int32(), 2);

        // Parse errors are reported at compile time.
        let options = CompileOptionsWrapper::new(cx, c"https://test/bad.json".to_owned(), 1);
        let bad = CompileJsonModule1(cx, options.ptr, &mut transform_str_to_source_text("{ nope"));
        assert!(bad.is_null());
        let error = pending_exception(cx);
        assert!(error.is_object());

        // CSS and text modules are synthetic modules with a default export.
        rooted!(&in(cx) let text = mozjs::jsval::ObjectValue(list));
        rooted!(&in(cx) let synthetic = mozjs::rust::wrappers2::CreateDefaultExportSyntheticModule(cx, text.handle()));
        assert!(!jsapi::IsCyclicModule(synthetic.get()));
        assert!(ModuleLink(cx, synthetic.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, synthetic.handle(), promise.handle_mut()));
        let ns = GetModuleNamespace(cx, synthetic.handle());
        assert_eq!(get(cx, ns, c"default").to_object(), list);
    });
}

#[test]
fn compile_and_link_errors() {
    add_sources(&[
        (
            "main.js",
            "import { missing } from './dep.js';\nexport { missing };",
        ),
        ("dep.js", "export const present = 1;"),
    ]);
    with_realm(|cx| unsafe {
        // Syntax error.
        let options = CompileOptionsWrapper::new(cx, c"https://test/syntax.js".to_owned(), 1);
        let bad = CompileModule1(
            cx,
            options.ptr,
            &mut transform_str_to_source_text("export let = ;"),
        );
        assert!(bad.is_null());
        assert!(pending_exception(cx).is_object());

        // Missing export.
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(!ModuleLink(cx, root.handle()));
        let error = pending_exception(cx);
        assert!(
            message(cx, error).contains("missing"),
            "{}",
            message(cx, error)
        );
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(!ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        pending_exception(cx);
    });
}

#[test]
fn import_attributes() {
    add_sources(&[
        (
            "main.js",
            "import a from './a.js' with { type: 'css' };\nimport c from './c.js' with { type: 'json' };\nimport t from './t.txt' with { type: 'text' };\nimport x from './x.js' with { type: 'x' };",
        ),
        (
            "bad.js",
            "import b from './b.js' with { foo: 'bar' };",
        ),
    ]);
    with_realm(|cx| unsafe {
        let options = CompileOptionsWrapper::new(cx, c"https://test/main.js".to_owned(), 1);
        let source = LOADER.with(|l| l.borrow().sources[&url("main.js")].clone());
        rooted!(&in(cx) let root = CompileModule1(cx, options.ptr, &mut transform_str_to_source_text(&source)));
        assert!(!root.get().is_null());
        for (i, module_type) in [
            ModuleType::CSS,
            ModuleType::JSON,
            ModuleType::Text,
            ModuleType::Unknown,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                GetRequestedModuleType(cx, root.handle(), i as u32),
                module_type
            );
        }

        // Like SpiderMonkey: an unsupported key fails the load with a SyntaxError.
        let options = CompileOptionsWrapper::new(cx, c"https://test/bad.js".to_owned(), 1);
        let source = LOADER.with(|l| l.borrow().sources[&url("bad.js")].clone());
        rooted!(&in(cx) let bad = CompileModule1(cx, options.ptr, &mut transform_str_to_source_text(&source)));
        rooted!(&in(cx) let promise = load_requested(cx, bad.get()));
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_REJECTED);
        rooted!(&in(cx) let mut reason = UndefinedValue());
        mozjs::glue::JS_GetPromiseResult(promise.handle().into(), reason.handle_mut().into());
        assert_eq!(error_name(cx, reason.get()), "SyntaxError");
        assert!(message(cx, reason.get()).contains("foo"));
        assert!(LOADER.with(|l| l.borrow().static_imports.is_empty()));

        // A failed load rejects with the host's error.
        add_sources(&[("missing.js", "import './nowhere.js';")]);
        let source = LOADER.with(|l| l.borrow().sources[&url("missing.js")].clone());
        let missing = compile(cx, &url("missing.js"), &source, ModuleType::JavaScript);
        rooted!(&in(cx) let promise = load_requested(cx, missing));
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_REJECTED);
        mozjs::glue::JS_GetPromiseResult(promise.handle().into(), reason.handle_mut().into());
        assert!(reason.get().is_undefined());
    });
}

#[test]
fn dynamic_import() {
    add_sources(&[
        (
            "main.js",
            "export const ok = import('./dyn.js').then(ns => ns.value * 2);\nexport const failed = import('throw.js');\nexport const unsupported = import('./x.js', { with: { foo: 'bar' } });\nexport const json = import('./x.json', { with: { type: 'json' } });",
        ),
        ("dyn.js", "export const value = 21;"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = load_graph(cx, &url("main.js")));
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut promise = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), promise.handle_mut()));
        assert_eq!(
            promise_state(promise.get().to_object()),
            qjs::JS_PROMISE_FULFILLED
        );

        let ns = namespace(cx, &url("main.js"));
        assert_eq!(
            promise_state(get(cx, ns, c"failed").to_object()),
            qjs::JS_PROMISE_REJECTED
        );
        rooted!(&in(cx) let unsupported = get(cx, ns, c"unsupported").to_object());
        assert_eq!(promise_state(unsupported.get()), qjs::JS_PROMISE_REJECTED);
        rooted!(&in(cx) let mut reason = UndefinedValue());
        mozjs::glue::JS_GetPromiseResult(unsupported.handle().into(), reason.handle_mut().into());
        assert_eq!(error_name(cx, reason.get()), "TypeError");
        let calls = LOADER.with(|l| l.borrow().dynamic_imports.clone());
        assert_eq!(
            calls,
            vec![
                (1, "./dyn.js".to_owned(), ModuleType::JavaScript),
                (1, "throw.js".to_owned(), ModuleType::JavaScript),
                (1, "./x.json".to_owned(), ModuleType::JSON),
            ]
        );

        // Servo's ContinueDynamicImport: load, link and evaluate, then resolve with the namespace.
        rooted!(&in(cx) let dyn_root = load_graph(cx, &url("dyn.js")));
        assert!(ModuleLink(cx, dyn_root.handle()));
        rooted!(&in(cx) let mut evaluated = UndefinedValue());
        assert!(ModuleEvaluate(
            cx,
            dyn_root.handle(),
            evaluated.handle_mut()
        ));
        rooted!(&in(cx) let dyn_ns = mozjs::jsval::ObjectValue(GetModuleNamespace(cx, dyn_root.handle())));
        rooted!(&in(cx) let import_promise = LOADER.with(|l| l.borrow().import_promises[0].get()));
        assert!(mozjs::rust::wrappers2::ResolvePromise(
            cx,
            import_promise.handle(),
            dyn_ns.handle()
        ));
        run_jobs(cx);
        rooted!(&in(cx) let ok = get(cx, ns, c"ok").to_object());
        assert_eq!(promise_state(ok.get()), qjs::JS_PROMISE_FULFILLED);
        rooted!(&in(cx) let mut result = UndefinedValue());
        mozjs::glue::JS_GetPromiseResult(ok.handle().into(), result.handle_mut().into());
        assert_eq!(result.get().to_int32(), 42);

        // From a classic script, with its script private.
        let options = CompileOptionsWrapper::new(cx, c"https://test/classic.js".to_owned(), 1);
        let script = mozjs::rust::wrappers2::Compile1(cx, options.ptr, &mut transform_str_to_source_text("globalThis.p = import('./from-classic.js');\nglobalThis.classicCaller = callerId();"));
        assert!(!script.is_null());
        let private = PrivateValue(77 as *const c_void);
        jsapi::SetScriptPrivate(script, &private);
        rooted!(&in(cx) let script: *mut JSScript = script);
        rooted!(&in(cx) let mut rval = UndefinedValue());
        assert!(mozjs::rust::wrappers2::JS_ExecuteScript(
            cx,
            script.handle(),
            rval.handle_mut()
        ));
        let calls = LOADER.with(|l| l.borrow().dynamic_imports.clone());
        assert_eq!(
            calls.last().unwrap(),
            &(77, "./from-classic.js".to_owned(), ModuleType::JavaScript)
        );
        let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        assert_eq!(get(cx, global, c"classicCaller").to_int32(), 77);
    });
}

thread_local!(static SCRIPT_PRIVATE_REFS: RefCell<HashMap<usize, isize>> = RefCell::new(HashMap::new()));

unsafe extern "C" fn add_ref_private(private: *const Value) {
    SCRIPT_PRIVATE_REFS.with(|r| *r.borrow_mut().entry(private_id(&*private)).or_default() += 1);
}

unsafe extern "C" fn release_private(private: *const Value) {
    SCRIPT_PRIVATE_REFS.with(|r| *r.borrow_mut().entry(private_id(&*private)).or_default() -= 1);
}

#[test]
fn script_privates() {
    with_realm(|cx| unsafe {
        let rt = jsapi::JS_GetRuntime(cx.raw_cx());
        jsapi::SetScriptPrivateReferenceHooks(rt, Some(add_ref_private), Some(release_private));
        // Two inline scripts of one page share its URL but not their privates. Eval and Function
        // code, and functions they create, use the private of the script that created them.
        run_classic(
            cx,
            "https://test/page.html",
            "globalThis.fromA = () => import('./a1.js');\n\
             (0, eval)(\"import('./a2.js')\");\n\
             eval(\"import('./a3.js')\");\n\
             Function(\"return import('./a4.js')\")();\n\
             globalThis.evalFnA = (0, eval)(\"(() => import('./a5.js'))\");\n\
             globalThis.callersA = [callerId(), (0, eval)('callerId()'), Function('return callerId()')()];",
            5,
        );
        run_classic(
            cx,
            "https://test/page.html",
            "fromA(); evalFnA(); import('./b1.js');",
            6,
        );
        let calls: Vec<_> = LOADER.with(|l| {
            l.borrow()
                .dynamic_imports
                .iter()
                .map(|c| (c.0, c.1.clone()))
                .collect()
        });
        let expected = [
            (5, "./a2.js"),
            (5, "./a3.js"),
            (5, "./a4.js"),
            (5, "./a1.js"),
            (5, "./a5.js"),
            (6, "./b1.js"),
        ];
        assert_eq!(calls, expected.map(|(id, s)| (id, s.to_owned())));
        let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        rooted!(&in(cx) let callers = get(cx, global, c"callersA").to_object());
        for i in 0..3 {
            let mut v = UndefinedValue();
            assert!(jsapi::JS_GetElement(
                cx.raw_cx(),
                h(&callers.get()),
                i,
                mh(&mut v)
            ));
            assert_eq!(v.to_int32(), 5);
        }

        // Two inline modules of one page: import.meta of each.
        for (i, url) in ["https://test/page.html", "https://test/page.html"]
            .iter()
            .enumerate()
        {
            rooted!(&in(cx) let module = compile(cx, url, "export const id = import.meta.id;", ModuleType::JavaScript));
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut promise = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), promise.handle_mut()));
            let ns = GetModuleNamespace(cx, module.handle());
            assert_eq!(
                get(cx, ns, c"id").to_int32() as usize,
                LOADER.with(|l| l.borrow().urls.len()),
                "module {i}"
            );
        }
        // The two classic scripts and, as with SpiderMonkey, the two modules (Q34).
        SCRIPT_PRIVATE_REFS
            .with(|r| assert_eq!(*r.borrow(), HashMap::from([(5, 1), (6, 1), (1, 1), (2, 1)])));
    });
    // Freeing the scripts and modules releases their privates.
    SCRIPT_PRIVATE_REFS
        .with(|r| assert_eq!(*r.borrow(), HashMap::from([(5, 0), (6, 0), (1, 0), (2, 0)])));
}

/// The global of a referrer is that of the realm its code was compiled in, not the current one
/// (Q35): Servo's HostLoadImportedModule takes the settings object from it.
#[test]
fn global_from_script() {
    with_realm(|cx| unsafe {
        let global_a = jsapi::CurrentGlobalOrNull(cx.raw_cx());
        let options = CompileOptionsWrapper::new(cx, CString::new(url("classic-a.js")).unwrap(), 1);
        let script = mozjs::rust::wrappers2::Compile1(
            cx,
            options.ptr,
            &mut transform_str_to_source_text("1"),
        );
        rooted!(&in(cx) let script_a: *mut JSScript = script);
        let module = compile(
            cx,
            &url("module-a.js"),
            "export const a = 1;",
            ModuleType::JavaScript,
        );
        rooted!(&in(cx) let module_script_a: *mut JSScript = jsapi::GetModuleScript(h(&module)));
        assert!(!module_script_a.get().is_null());

        let options = RealmOptions::default();
        rooted!(&in(cx) let global_b = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*options,
        ));
        let mut realm = mozjs::realm::AutoRealm::new_from_handle(cx, global_b.handle());
        let cx = &mut *realm;
        let options = CompileOptionsWrapper::new(cx, CString::new(url("classic-b.js")).unwrap(), 1);
        let script = mozjs::rust::wrappers2::Compile1(
            cx,
            options.ptr,
            &mut transform_str_to_source_text("2"),
        );
        rooted!(&in(cx) let script_b: *mut JSScript = script);

        assert_eq!(jsapi::JS_GetGlobalFromScript(script_a.get()), global_a);
        assert_eq!(
            jsapi::JS_GetGlobalFromScript(module_script_a.get()),
            global_a
        );
        assert_eq!(
            jsapi::JS_GetGlobalFromScript(script_b.get()),
            global_b.get()
        );
    });
}
