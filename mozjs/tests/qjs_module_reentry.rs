/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Graph loading where the load hook finishes the loads of `s_` specifiers before it returns and
//! queues the others, with a GC and a pool scope in every hook call: the loads re-enter
//! InnerModuleLoading while it holds the load state and the module's data, GC marks read both,
//! and the scopes drain the pool above the loop's entries.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_void, CString};
use std::marker::PhantomData;
use std::ptr;
use std::sync::OnceLock;

use mozjs::context::JSContext;
use mozjs::jsapi::{
    self, GCReason, Heap, JSContext as RawJSContext, JSObject, JSScript, JSString,
    OnNewGlobalHookOption, Value,
};
use mozjs::jsval::UndefinedValue;
use mozjs::qjs;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    CompileModule1, JS_NewGlobalObject, LoadRequestedModules1, ModuleEvaluate, ModuleLink, JS_GC,
};
use mozjs::rust::{
    transform_str_to_source_text, CompileOptionsWrapper, JSEngine, JSEngineHandle, RealmOptions,
    Runtime, SIMPLE_GLOBAL_CLASS,
};

const BASE: &str = "https://test/";

struct Pending {
    referrer: Box<Heap<*mut JSScript>>,
    request: Box<Heap<*mut JSObject>>,
    payload: Box<Heap<Value>>,
    url: String,
}

#[derive(Default)]
struct Loader {
    sources: HashMap<String, String>,
    modules: HashMap<String, Box<Heap<*mut JSObject>>>,
    pending: VecDeque<Pending>,
    requested: Vec<String>,
}

thread_local!(static LOADER: RefCell<Loader> = RefCell::new(Loader::default()));

struct LoaderGuard;

impl Drop for LoaderGuard {
    fn drop(&mut self) {
        // Dropped outside the borrow: dropping the `Heap`s releases objects.
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

fn url(specifier: &str) -> String {
    format!("{BASE}{}", specifier.trim_start_matches("./"))
}

unsafe fn string(s: *mut JSString) -> String {
    let v = qjs::JSValue::mkptr(qjs::JS_TAG_STRING, s as *mut c_void);
    let mut data = ptr::null();
    let mut wide = 0;
    let len = qjs::JS_GetStringData(v, &mut data, &mut wide);
    assert_eq!(wide, 0);
    std::slice::from_raw_parts(data as *const u8, len)
        .iter()
        .map(|&b| b as char)
        .collect()
}

unsafe fn context(cx: *mut RawJSContext) -> JSContext {
    JSContext::from_ptr(ptr::NonNull::new(cx).unwrap())
}

unsafe fn compile(cx: *mut RawJSContext, url: &str, source: &str) -> *mut JSObject {
    let mut context = context(cx);
    let options = CompileOptionsWrapper::new(&mut context, CString::new(url).unwrap(), 1);
    let mut text = transform_str_to_source_text(source);
    let record = CompileModule1(&mut context, options.ptr, &mut text);
    if !record.is_null() {
        LOADER.with(|l| {
            l.borrow_mut()
                .modules
                .insert(url.to_owned(), Heap::boxed(record))
        });
    }
    record
}

unsafe fn finish(
    cx: *mut RawJSContext,
    referrer: *mut JSScript,
    request: *mut JSObject,
    payload: Value,
    url: &str,
) -> bool {
    let mut found = LOADER.with(|l| l.borrow().modules.get(url).map(|m| m.get()));
    if found.is_none() {
        let source = LOADER.with(|l| l.borrow().sources.get(url).cloned());
        let Some(source) = source else {
            let error = UndefinedValue();
            return jsapi::FinishLoadingImportedModuleFailed(cx, h(&payload), h(&error));
        };
        let record = compile(cx, url, &source);
        assert!(!record.is_null(), "{url} failed to compile");
        found = Some(record);
    }
    let found = found.unwrap();
    jsapi::FinishLoadingImportedModule(cx, h(&referrer), h(&request), h(&payload), h(&found), true)
}

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
    let url = url(&specifier);
    LOADER.with(|l| l.borrow_mut().requested.push(url.clone()));
    let _scope = mozjs::quickjs::AutoreleasePoolScope::new();
    JS_GC(&mut context(cx), GCReason::API);
    if specifier.trim_start_matches("./").starts_with("s_") {
        let ok = finish(cx, *referrer.ptr, *request.ptr, *payload.ptr, &url);
        JS_GC(&mut context(cx), GCReason::API);
        return ok;
    }
    let pending = Pending {
        referrer: Heap::boxed(*referrer.ptr),
        request: Heap::boxed(*request.ptr),
        payload: Heap::boxed(*payload.ptr),
        url,
    };
    LOADER.with(|l| l.borrow_mut().pending.push_back(pending));
    true
}

/// Finishes the queued loads one per "task", draining the pool after each like Servo.
unsafe fn pump(cx: &mut JSContext) {
    while let Some(p) = LOADER.with(|l| l.borrow_mut().pending.pop_front()) {
        let (referrer, request, payload) = (p.referrer.get(), p.request.get(), p.payload.get());
        assert!(finish(cx.raw_cx(), referrer, request, payload, &p.url));
        drop(p);
        JS_GC(cx, GCReason::API);
    }
}

unsafe fn load_requested(cx: &mut JSContext, root: *mut JSObject) -> Box<Heap<*mut JSObject>> {
    rooted!(&in(cx) let root = root);
    rooted!(&in(cx) let host_defined = UndefinedValue());
    rooted!(&in(cx) let mut promise = ptr::null_mut::<JSObject>());
    assert!(LoadRequestedModules1(
        cx,
        root.handle(),
        host_defined.handle(),
        promise.handle_mut()
    ));
    Heap::boxed(promise.get())
}

fn promise_state(promise: *mut JSObject) -> qjs::JSPromiseStateEnum {
    unsafe {
        qjs::JS_PromiseState(
            ptr::null_mut(),
            qjs::JSValue::mkptr(qjs::JS_TAG_OBJECT, promise as *mut c_void),
        )
    }
}

fn add_sources(sources: &[(&str, &str)]) {
    LOADER.with(|l| {
        let mut l = l.borrow_mut();
        for (name, source) in sources {
            l.sources.insert(url(name), (*source).to_owned());
        }
    });
}

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

fn with_realm(f: impl FnOnce(&mut JSContext)) {
    let mut runtime = Runtime::new(engine());
    let _loader = LoaderGuard;
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = mozjs::realm::AutoRealm::new_from_handle(cx, global.handle());
        let cx = &mut *realm;
        let rt = jsapi::JS_GetRuntime(cx.raw_cx());
        jsapi::SetModuleLoadHook(rt, Some(load_hook));
        f(cx);
    }
}

unsafe fn root_record(cx: &mut JSContext, name: &str) -> *mut JSObject {
    let source = LOADER.with(|l| l.borrow().sources[&url(name)].clone());
    compile(cx.raw_cx(), &url(name), &source)
}

/// Cycles through synchronously finished loads (including a self-import) mixed with queued ones.
#[test]
fn mixed_sync_and_queued_graph() {
    add_sources(&[
        ("root.js", "import './s_a.js'; import './b.js'; import './s_c.js'; export const r = 1;"),
        ("s_a.js", "import './s_a.js'; import './b.js'; import './s_c.js'; import './root.js'; export const a = 1;"),
        ("b.js", "import './s_d.js'; export const b = 1;"),
        ("s_c.js", "import './s_d.js'; import './s_a.js'; export const c = 1;"),
        ("s_d.js", "import './root.js'; import './b.js'; export const d = 1;"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = root_record(cx, "root.js"));
        let promise = load_requested(cx, root.get());
        mozjs::quickjs::drain_autorelease_pool();
        JS_GC(cx, GCReason::API);
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_PENDING);
        pump(cx);
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_FULFILLED);
        assert!(ModuleLink(cx, root.handle()));
        rooted!(&in(cx) let mut rval = UndefinedValue());
        assert!(ModuleEvaluate(cx, root.handle(), rval.handle_mut()));
        assert_eq!(
            promise_state(rval.get().to_object()),
            qjs::JS_PROMISE_FULFILLED
        );
        let again = load_requested(cx, root.get());
        assert_eq!(promise_state(again.get()), qjs::JS_PROMISE_FULFILLED);
        assert!(LOADER.with(|l| l.borrow().pending.is_empty()));
        let requested = LOADER.with(|l| l.borrow().requested.clone());
        assert!(requested.iter().filter(|u| u.contains("/s_")).count() >= 4);
        assert!(requested.iter().any(|u| u.ends_with("/b.js")));
    });
}

/// A load that fails inside the hook ends the referrer's loop; queued siblings finish later
/// against the failed state.
#[test]
fn sync_failure_with_queued_siblings() {
    add_sources(&[
        (
            "r2.js",
            "import './q1.js'; import './s_e.js'; import './s_missing.js'; import './q2.js';",
        ),
        ("q1.js", "export const q1 = 1;"),
        ("s_e.js", "import './q3.js'; export const e = 1;"),
        ("q3.js", "export const q3 = 1;"),
        ("q2.js", "export const q2 = 1;"),
    ]);
    with_realm(|cx| unsafe {
        rooted!(&in(cx) let root = root_record(cx, "r2.js"));
        let promise = load_requested(cx, root.get());
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_REJECTED);
        let requested = LOADER.with(|l| l.borrow().requested.clone());
        assert!(!requested.contains(&url("q2.js")), "{requested:?}");
        assert_eq!(LOADER.with(|l| l.borrow().pending.len()), 2);
        mozjs::quickjs::drain_autorelease_pool();
        pump(cx);
        assert_eq!(promise_state(promise.get()), qjs::JS_PROMISE_REJECTED);
    });
}
