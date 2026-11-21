/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! ES modules: compilation, linking, evaluation, loading hooks and module private values.

use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::ptr::{self, NonNull};

use js::context::{JSContext, RawJSContext};
use js::conversions::jsstr_to_string;
use js::glue::{
    JS_GetModulePrivate, RegisterScriptEnvironmentPreparer, RunScriptEnvironmentPreparerClosure,
};
use js::jsapi::{
    self, ColumnNumberOneOrigin, Handle, HandleObject, HandleValue, Heap, JSObject,
    JSPROP_ENUMERATE, JSScript, JSString, ModuleErrorBehaviour, ModuleType, MutableHandleValue,
    PromiseState, Value,
};
use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
use js::rooted;
use js::rust::wrappers2::{
    Compile1, CompileJsonModule, CompileJsonModule1, CompileModule1,
    CreateDefaultExportSyntheticModule, GetModuleNamespace, GetModuleScript, GetPromiseState,
    GetRequestedModuleType, JS_ExecuteScript, LoadRequestedModules, LoadRequestedModules1,
    ModuleEvaluate, ModuleLink, ResolvePromise, RunJobs, ThrowOnModuleEvaluationFailure,
};
use js::rust::{
    CompileOptionsWrapper, JSEngine, Runtime, transform_str_to_source_text,
    transform_u16_to_source_text,
};

use crate::support::{
    Observation, Value as Observed, install_job_queue, module_type_name, observe,
    promise_state_name, record_case, remove_job_queue,
};

thread_local! {
    static STATIC_SOURCE: RefCell<String> = const { RefCell::new(String::new()) };
    static STATIC_MODULE: RefCell<Option<Box<Heap<*mut JSObject>>>> = const { RefCell::new(None) };
    static CAPTURED: RefCell<Vec<(String, ModuleType)>> = const { RefCell::new(Vec::new()) };
    static DYNAMIC_PROMISE: RefCell<Option<Box<Heap<*mut JSObject>>>> = const { RefCell::new(None) };
    static DYNAMIC_MODULE: RefCell<Option<Box<Heap<*mut JSObject>>>> = const { RefCell::new(None) };
    static RESOLVED: Cell<bool> = const { Cell::new(false) };
    static REJECTED: Cell<bool> = const { Cell::new(false) };
    static ADD_REFS: Cell<u32> = const { Cell::new(0) };
    static RELEASE_REFS: Cell<u32> = const { Cell::new(0) };
    static PREPARED: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
    static STATIC_SPECIFIER: RefCell<String> = const { RefCell::new(String::new()) };
    static STATIC_FINISHED: Cell<bool> = const { Cell::new(false) };
    static DYNAMIC_FINISHED: Cell<bool> = const { Cell::new(false) };
}

fn string_of(cx: &JSContext, string: *mut JSString) -> String {
    unsafe {
        jsstr_to_string(
            cx,
            NonNull::new(string).expect("non-null module request specifier"),
        )
    }
}

unsafe extern "C" fn add_ref_private(value: *const Value) {
    if !(*value).is_undefined() {
        ADD_REFS.with(|count| count.set(count.get() + 1));
    }
}

unsafe extern "C" fn release_private(value: *const Value) {
    if !(*value).is_undefined() {
        RELEASE_REFS.with(|count| count.set(count.get() + 1));
    }
}

unsafe extern "C" fn define_meta(
    cx: *mut RawJSContext,
    _module: HandleObject,
    meta: HandleObject,
) -> bool {
    let marker = Int32Value(5);
    let marker = Handle::from_marked_location(&marker);
    jsapi::JS_DefineProperty(
        cx,
        meta,
        c"marker".as_ptr(),
        marker,
        JSPROP_ENUMERATE as u32,
    )
}

unsafe fn compile_dependency(cx: *mut RawJSContext, source: &str) -> *mut JSObject {
    let mut local_cx = JSContext::from_ptr(NonNull::new(cx).unwrap());
    let options = CompileOptionsWrapper::new(&local_cx, c"contract-dep.js".to_owned(), 1);
    CompileModule1(
        &mut local_cx,
        options.ptr,
        &mut transform_str_to_source_text(source),
    )
}

unsafe fn prepare_module(cx: &mut JSContext, module: js::rust::Handle<*mut JSObject>) {
    rooted!(&in(cx) let host_defined = UndefinedValue());
    rooted!(&in(cx) let mut promise = ptr::null_mut::<JSObject>());
    assert!(LoadRequestedModules1(
        cx,
        module,
        host_defined.handle(),
        promise.handle_mut(),
    ));
    assert!(!promise.get().is_null());
    assert_eq!(GetPromiseState(promise.handle()), PromiseState::Fulfilled);
}

unsafe extern "C" fn finish_static_load(
    cx: *mut RawJSContext,
    referrer: Handle<*mut JSScript>,
    request: HandleObject,
    _host_defined: HandleValue,
    payload: HandleValue,
    _line: u32,
    _column: ColumnNumberOneOrigin,
) -> bool {
    let specifier = {
        let context = JSContext::from_ptr(NonNull::new(cx).unwrap());
        string_of(&context, jsapi::GetModuleRequestSpecifier(cx, request))
    };
    STATIC_SPECIFIER.with(|captured| *captured.borrow_mut() = specifier);
    let source = STATIC_SOURCE.with(|source| source.borrow().clone());
    let module = compile_dependency(cx, &source);
    if module.is_null() {
        return jsapi::FinishLoadingImportedModuleFailedWithPendingException(cx, payload);
    }
    STATIC_MODULE.with(|slot| *slot.borrow_mut() = Some(Heap::boxed(module)));
    let result = Handle::from_marked_location(&module);
    let finished = jsapi::FinishLoadingImportedModule(cx, referrer, request, payload, result, true);
    STATIC_FINISHED.with(|done| done.set(finished));
    finished
}

unsafe extern "C" fn capture_and_fail(
    cx: *mut RawJSContext,
    _referrer: Handle<*mut JSScript>,
    request: HandleObject,
    _host_defined: HandleValue,
    payload: HandleValue,
    _line: u32,
    _column: ColumnNumberOneOrigin,
) -> bool {
    let specifier = {
        let context = JSContext::from_ptr(NonNull::new(cx).unwrap());
        string_of(&context, jsapi::GetModuleRequestSpecifier(cx, request))
    };
    let module_type = jsapi::GetModuleRequestType(cx, request);
    CAPTURED.with(|captured| captured.borrow_mut().push((specifier, module_type)));
    let error = UndefinedValue();
    let error = Handle::from_marked_location(&error);
    let failed = jsapi::FinishLoadingImportedModuleFailed(cx, payload, error);
    STATIC_FINISHED.with(|done| done.set(failed));
    failed
}

unsafe extern "C" fn fail_with_pending(
    cx: *mut RawJSContext,
    _referrer: Handle<*mut JSScript>,
    _request: HandleObject,
    _host_defined: HandleValue,
    payload: HandleValue,
    _line: u32,
    _column: ColumnNumberOneOrigin,
) -> bool {
    let error = Int32Value(42);
    let error = Handle::from_marked_location(&error);
    jsapi::JS_SetPendingException(cx, error, jsapi::ExceptionStackBehavior::Capture);
    let failed = jsapi::FinishLoadingImportedModuleFailedWithPendingException(cx, payload);
    STATIC_FINISHED.with(|done| done.set(failed));
    failed
}

unsafe extern "C" fn finish_dynamic_load(
    cx: *mut RawJSContext,
    referrer: Handle<*mut JSScript>,
    request: HandleObject,
    _host_defined: HandleValue,
    payload: HandleValue,
    _line: u32,
    _column: ColumnNumberOneOrigin,
) -> bool {
    let specifier = {
        let context = JSContext::from_ptr(NonNull::new(cx).unwrap());
        string_of(&context, jsapi::GetModuleRequestSpecifier(cx, request))
    };
    let module_type = jsapi::GetModuleRequestType(cx, request);
    CAPTURED.with(|captured| captured.borrow_mut().push((specifier, module_type)));

    let payload_value = *payload.ptr;
    if !payload_value.is_object() {
        return false;
    }
    let promise = payload_value.to_object();
    let promise_handle = Handle::from_marked_location(&promise);
    if !jsapi::IsPromiseObject(promise_handle) {
        return false;
    }

    let module = compile_dependency(cx, "export const value = 21;");
    if module.is_null() {
        return jsapi::FinishLoadingImportedModuleFailedWithPendingException(cx, payload);
    }
    DYNAMIC_PROMISE.with(|slot| *slot.borrow_mut() = Some(Heap::boxed(promise)));
    DYNAMIC_MODULE.with(|slot| *slot.borrow_mut() = Some(Heap::boxed(module)));
    let result = Handle::from_marked_location(&module);
    let finished =
        jsapi::FinishLoadingDynamicImportedModule(cx, referrer, request, payload, result);
    DYNAMIC_FINISHED.with(|done| done.set(finished));
    finished
}

unsafe extern "C" fn load_resolved(_cx: *mut RawJSContext, host_defined: HandleValue) -> bool {
    RESOLVED.with(|resolved| resolved.set(true));
    assert_eq!((*host_defined.ptr).to_int32(), 99);
    true
}

unsafe extern "C" fn load_rejected(
    _cx: *mut RawJSContext,
    _host_defined: HandleValue,
    _error: HandleValue,
) -> bool {
    REJECTED.with(|rejected| rejected.set(true));
    true
}

unsafe extern "C" fn prepare_and_run(
    _global: HandleObject,
    closure: *mut jsapi::ScriptEnvironmentPreparer_Closure,
) {
    let cx = Runtime::get().expect("a runtime is current");
    let mut cx = JSContext::from_ptr(cx);
    if !RunScriptEnvironmentPreparerClosure(cx.raw_cx(), closure) {
        let mut error = UndefinedValue();
        let destination = MutableHandleValue::from_marked_location(&mut error);
        assert!(jsapi::JS_GetPendingException(cx.raw_cx(), destination));
        PREPARED.with(|prepared| prepared.borrow_mut().push(error.to_int32()));
        jsapi::JS_ClearPendingException(cx.raw_cx());
    }
}

fn json_and_synthetic(engine: &JSEngine) {
    record_case(
        engine,
        "module.json_and_synthetic",
        &[
            "CompileJsonModule",
            "CompileJsonModule1",
            "CreateDefaultExportSyntheticModule",
            "GetModuleNamespace",
            "GetModuleScript",
            "IsCyclicModule",
            "ModuleEvaluate",
            "ModuleLink",
        ],
        |cx, _global| unsafe {
            let options = CompileOptionsWrapper::new(cx, c"contract-json.js".to_owned(), 1);
            let module = CompileJsonModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("{\"list\": [10, 20]}"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            assert!(!jsapi::IsCyclicModule(module.get()));
            assert!(GetModuleScript(module.handle()).is_null());
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            let namespace = GetModuleNamespace(cx, module.handle());
            assert!(!namespace.is_null());
            let default = crate::support::get_property(cx, ObjectValue(namespace), c"default");
            assert!(default.is_object());
            let list = crate::support::get_property(cx, default, c"list");
            let length = crate::support::get_property(cx, list, c"length");
            let list_length = observe(cx, length);
            assert_eq!(list_length, crate::support::Value::Number(2.0));

            let options = CompileOptionsWrapper::new(cx, c"contract-json-utf16.js".to_owned(), 1);
            let source: Vec<u16> = "{\"ok\": true}".encode_utf16().collect();
            let module =
                CompileJsonModule(cx, options.ptr, &mut transform_u16_to_source_text(&source));
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            let namespace = GetModuleNamespace(cx, module.handle());
            assert!(!namespace.is_null());
            let default = crate::support::get_property(cx, ObjectValue(namespace), c"default");
            let ok = crate::support::get_property(cx, default, c"ok");
            let ok = observe(cx, ok);
            assert_eq!(ok, crate::support::Value::Boolean(true));

            let options = CompileOptionsWrapper::new(cx, c"contract-bad-json.js".to_owned(), 1);
            let bad =
                CompileJsonModule1(cx, options.ptr, &mut transform_str_to_source_text("{ nope"));
            let bad_json_compile_failed = bad.is_null();
            assert!(
                bad_json_compile_failed,
                "CompileJsonModule1 must reject invalid JSON"
            );
            crate::support::assert_exception_named(cx, "SyntaxError");

            rooted!(&in(cx) let default = Int32Value(7));
            let synthetic = CreateDefaultExportSyntheticModule(cx, default.handle());
            assert!(!synthetic.is_null());
            rooted!(&in(cx) let synthetic = synthetic);
            assert!(!jsapi::IsCyclicModule(synthetic.get()));
            assert!(GetModuleScript(synthetic.handle()).is_null());
            prepare_module(cx, synthetic.handle());
            assert!(ModuleLink(cx, synthetic.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(
                cx,
                synthetic.handle(),
                evaluation.handle_mut()
            ));
            let namespace = GetModuleNamespace(cx, synthetic.handle());
            assert!(!namespace.is_null());
            let value = crate::support::get_property(cx, ObjectValue(namespace), c"default");
            let synthetic_default = observe(cx, value);
            assert_eq!(synthetic_default, crate::support::Value::Number(7.0));
            Observation::new(vec![
                ("json.list_length", list_length),
                ("json.ok", ok),
                (
                    "bad_json_compile_failed",
                    Observed::Boolean(bad_json_compile_failed),
                ),
                ("bad_json_has_pending_exception", Observed::Boolean(true)),
                ("synthetic_default", synthetic_default),
            ])
        },
    );
}

fn privates_hooks_and_requests(engine: &JSEngine) {
    record_case(
        engine,
        "module.privates_hooks_and_requests",
        &[
            "ClearModulePrivate",
            "CompileModule1",
            "FinishLoadingImportedModuleFailed",
            "GetModuleLoadHook",
            "GetModuleMetadataHook",
            "GetModuleNamespace",
            "GetModuleRequestSpecifier",
            "GetModuleRequestType",
            "GetRequestedModuleType",
            "JS_GetModulePrivate",
            "JS_GetRuntime",
            "LoadRequestedModules1",
            "ModuleEvaluate",
            "ModuleLink",
            "SetModuleLoadHook",
            "SetModuleMetadataHook",
            "SetModulePrivate",
            "SetScriptPrivateReferenceHooks",
        ],
        |cx, _global| unsafe {
            let rt = jsapi::JS_GetRuntime(cx.raw_cx());
            ADD_REFS.with(|count| count.set(0));
            RELEASE_REFS.with(|count| count.set(0));
            jsapi::SetScriptPrivateReferenceHooks(rt, Some(add_ref_private), Some(release_private));

            let options = CompileOptionsWrapper::new(cx, c"contract-private.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("export const n = 1;"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);

            let seven = Int32Value(7);
            jsapi::SetModulePrivate(module.get(), &seven);
            let mut private = UndefinedValue();
            let destination = MutableHandleValue::from_marked_location(&mut private);
            JS_GetModulePrivate(module.get(), destination);
            let private_after_set = observe(cx, private);

            let eight = Int32Value(8);
            jsapi::SetModulePrivate(module.get(), &eight);
            let mut private = UndefinedValue();
            let destination = MutableHandleValue::from_marked_location(&mut private);
            JS_GetModulePrivate(module.get(), destination);
            let private_after_replace = observe(cx, private);

            jsapi::ClearModulePrivate(module.get());
            let mut private = UndefinedValue();
            let destination = MutableHandleValue::from_marked_location(&mut private);
            JS_GetModulePrivate(module.get(), destination);
            let private_after_clear = observe(cx, private);
            let private_hooks_called =
                ADD_REFS.with(Cell::get) > 0 && RELEASE_REFS.with(Cell::get) > 0;

            assert!(jsapi::GetModuleLoadHook(rt).is_none());
            jsapi::SetModuleLoadHook(rt, Some(capture_and_fail));
            let load_hook_installed = jsapi::GetModuleLoadHook(rt).is_some();
            assert!(load_hook_installed);
            assert!(jsapi::GetModuleMetadataHook(rt).is_none());
            jsapi::SetModuleMetadataHook(rt, Some(define_meta));
            let metadata_hook_installed = jsapi::GetModuleMetadataHook(rt).is_some();
            assert!(metadata_hook_installed);

            let options = CompileOptionsWrapper::new(cx, c"contract-meta.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("export const marker = import.meta.marker;"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            let namespace = GetModuleNamespace(cx, module.handle());
            let marker = crate::support::get_property(cx, ObjectValue(namespace), c"marker");
            let import_meta_marker = observe(cx, marker);

            let cases = [
                ("style.css", "css"),
                ("data.json", "json"),
                ("note.txt", "text"),
            ];
            let mut requests = Vec::new();
            STATIC_FINISHED.with(|done| done.set(false));
            for (specifier, attribute) in cases {
                let source = format!(
                    "import value from './{specifier}' with {{ type: '{attribute}' }}; export {{ value }};"
                );
                let options = CompileOptionsWrapper::new(
                    cx,
                    CString::new(format!("contract-{specifier}.js")).unwrap(),
                    1,
                );
                let module =
                    CompileModule1(cx, options.ptr, &mut transform_str_to_source_text(&source));
                assert!(!module.is_null());
                rooted!(&in(cx) let module = module);
                let requested_type = GetRequestedModuleType(cx, module.handle(), 0);

                CAPTURED.with(|captured| captured.borrow_mut().clear());
                rooted!(&in(cx) let host_defined = UndefinedValue());
                rooted!(&in(cx) let mut promise = ptr::null_mut::<JSObject>());
                assert!(LoadRequestedModules1(
                    cx,
                    module.handle(),
                    host_defined.handle(),
                    promise.handle_mut(),
                ));
                assert!(!promise.get().is_null());
                let load_state = GetPromiseState(promise.handle());
                let captured = CAPTURED.with(|captured| captured.borrow().clone());
                assert_eq!(captured.len(), 1);
                requests.push(Observed::Array(vec![
                    Observed::String(captured[0].0.clone()),
                    Observed::String(module_type_name(captured[0].1).to_owned()),
                    Observed::String(module_type_name(requested_type).to_owned()),
                    Observed::String(promise_state_name(load_state).to_owned()),
                ]));
            }
            assert!(
                STATIC_FINISHED.with(Cell::get),
                "FinishLoadingImportedModuleFailed must report failure"
            );
            jsapi::SetModuleLoadHook(rt, None);
            Observation::new(vec![
                ("private_after_set", private_after_set),
                ("private_after_replace", private_after_replace),
                ("private_after_clear", private_after_clear),
                (
                    "private_hooks_called",
                    Observed::Boolean(private_hooks_called),
                ),
                (
                    "load_hook_installed",
                    Observed::Boolean(load_hook_installed),
                ),
                (
                    "metadata_hook_installed",
                    Observed::Boolean(metadata_hook_installed),
                ),
                ("import_meta_marker", import_meta_marker),
                ("request_captures", Observed::Array(requests)),
                (
                    "import_failure_reported",
                    Observed::Boolean(STATIC_FINISHED.with(Cell::get)),
                ),
            ])
        },
    );
}

fn load_requested(engine: &JSEngine) {
    record_case(
        engine,
        "module.load_requested",
        &[
            "CompileModule1",
            "FinishLoadingImportedModule",
            "GetModuleNamespace",
            "GetPromiseState",
            "JS_GetRuntime",
            "LoadRequestedModules",
            "LoadRequestedModules1",
            "ModuleEvaluate",
            "ModuleLink",
            "SetModuleLoadHook",
        ],
        |cx, _global| unsafe {
            STATIC_SOURCE
                .with(|source| *source.borrow_mut() = "export const value = 41;".to_owned());
            STATIC_MODULE.with(|slot| *slot.borrow_mut() = None);
            STATIC_SPECIFIER.with(|specifier| specifier.borrow_mut().clear());
            STATIC_FINISHED.with(|done| done.set(false));
            RESOLVED.with(|resolved| resolved.set(false));
            REJECTED.with(|rejected| rejected.set(false));
            let rt = jsapi::JS_GetRuntime(cx.raw_cx());
            jsapi::SetModuleLoadHook(rt, Some(finish_static_load));

            let options = CompileOptionsWrapper::new(cx, c"contract-loader.js".to_owned(), 1);
            let source = "import { value } from './dep.js'; export const total = value + 1;";
            let module = CompileModule1(cx, options.ptr, &mut transform_str_to_source_text(source));
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            rooted!(&in(cx) let host_defined = UndefinedValue());
            rooted!(&in(cx) let mut promise = ptr::null_mut::<JSObject>());
            assert!(LoadRequestedModules1(
                cx,
                module.handle(),
                host_defined.handle(),
                promise.handle_mut(),
            ));
            assert!(!promise.get().is_null());
            let load_state = GetPromiseState(promise.handle());

            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            let namespace = GetModuleNamespace(cx, module.handle());
            let total = crate::support::get_property(cx, ObjectValue(namespace), c"total");
            let total = observe(cx, total);

            rooted!(&in(cx) let callback_host_defined = Int32Value(99));
            assert!(LoadRequestedModules(
                cx,
                module.handle(),
                callback_host_defined.handle(),
                Some(load_resolved),
                Some(load_rejected),
            ));
            let callback_resolved = RESOLVED.with(Cell::get);
            let callback_rejected = REJECTED.with(Cell::get);
            let imported_specifier = STATIC_SPECIFIER.with(|specifier| specifier.borrow().clone());
            assert_eq!(imported_specifier, "./dep.js");
            assert!(
                STATIC_FINISHED.with(Cell::get),
                "FinishLoadingImportedModule must report success"
            );

            STATIC_MODULE.with(|slot| *slot.borrow_mut() = None);
            jsapi::SetModuleLoadHook(rt, None);
            Observation::new(vec![
                (
                    "load_promise_state",
                    Observed::String(promise_state_name(load_state).to_owned()),
                ),
                ("total", total),
                ("callback_resolved", Observed::Boolean(callback_resolved)),
                ("callback_rejected", Observed::Boolean(callback_rejected)),
                ("imported_specifier", Observed::String(imported_specifier)),
                (
                    "imported_module_finished",
                    Observed::Boolean(STATIC_FINISHED.with(Cell::get)),
                ),
            ])
        },
    );
}

fn dynamic_import(engine: &JSEngine) {
    record_case(
        engine,
        "module.dynamic_import",
        &[
            "Compile1",
            "CompileModule1",
            "FinishLoadingDynamicImportedModule",
            "GetModuleNamespace",
            "GetModuleRequestSpecifier",
            "GetModuleRequestType",
            "GetPromiseState",
            "IsPromiseObject",
            "JS_ExecuteScript",
            "JS_GetPromiseResult",
            "JS_GetRuntime",
            "ModuleEvaluate",
            "ModuleLink",
            "ResolvePromise",
            "RunJobs",
            "SetModuleLoadHook",
        ],
        |cx, _global| unsafe {
            DYNAMIC_PROMISE.with(|slot| *slot.borrow_mut() = None);
            DYNAMIC_MODULE.with(|slot| *slot.borrow_mut() = None);
            DYNAMIC_FINISHED.with(|done| done.set(false));
            CAPTURED.with(|captured| captured.borrow_mut().clear());
            let rt = jsapi::JS_GetRuntime(cx.raw_cx());
            jsapi::SetModuleLoadHook(rt, Some(finish_dynamic_load));
            let queue = install_job_queue(cx);

            let options = CompileOptionsWrapper::new(cx, c"contract-dynamic.js".to_owned(), 1);
            let script = Compile1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("globalThis.imported = import('./dynamic.js');"),
            );
            assert!(!script.is_null());
            rooted!(&in(cx) let script = script);
            rooted!(&in(cx) let mut rval = UndefinedValue());
            assert!(JS_ExecuteScript(cx, script.handle(), rval.handle_mut()));

            let captured = CAPTURED.with(|captured| captured.borrow().clone());
            assert_eq!(captured.len(), 1);
            assert!(
                DYNAMIC_FINISHED.with(Cell::get),
                "FinishLoadingDynamicImportedModule must report success"
            );

            let import_promise = DYNAMIC_PROMISE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .expect("dynamic import payload")
                    .get()
            });
            let module = DYNAMIC_MODULE
                .with(|slot| slot.borrow().as_ref().expect("dynamic module record").get());
            rooted!(&in(cx) let module = module);
            assert!(!GetModuleScript(module.handle()).is_null());
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            let namespace = GetModuleNamespace(cx, module.handle());
            assert!(!namespace.is_null());
            let value = crate::support::get_property(cx, ObjectValue(namespace), c"value");
            let module_value = observe(cx, value);

            rooted!(&in(cx) let import_promise = import_promise);
            rooted!(&in(cx) let namespace = namespace);
            rooted!(&in(cx) let namespace_value = ObjectValue(namespace.get()));
            assert!(ResolvePromise(
                cx,
                import_promise.handle(),
                namespace_value.handle()
            ));
            RunJobs(cx);

            let global = jsapi::CurrentGlobalOrNull(cx.raw_cx());
            let imported = crate::support::get_property(cx, ObjectValue(global), c"imported");
            assert!(imported.is_object());
            rooted!(&in(cx) let imported = imported.to_object());
            let imported_promise_state = GetPromiseState(imported.handle());
            let mut resolved = UndefinedValue();
            let destination = MutableHandleValue::from_marked_location(&mut resolved);
            js::glue::JS_GetPromiseResult(
                Handle::from_marked_location(&imported.get()),
                destination,
            );
            let resolved_is_namespace = resolved.to_object() == namespace.get();

            DYNAMIC_PROMISE.with(|slot| *slot.borrow_mut() = None);
            DYNAMIC_MODULE.with(|slot| *slot.borrow_mut() = None);
            jsapi::SetModuleLoadHook(rt, None);
            remove_job_queue(cx, queue);
            Observation::new(vec![
                (
                    "captured_specifier",
                    Observed::String(captured[0].0.clone()),
                ),
                (
                    "captured_type",
                    Observed::String(module_type_name(captured[0].1).to_owned()),
                ),
                ("module_value", module_value),
                (
                    "imported_promise_state",
                    Observed::String(promise_state_name(imported_promise_state).to_owned()),
                ),
                (
                    "resolved_is_namespace",
                    Observed::Boolean(resolved_is_namespace),
                ),
                (
                    "dynamic_module_finished",
                    Observed::Boolean(DYNAMIC_FINISHED.with(Cell::get)),
                ),
            ])
        },
    );
}

fn failures_and_evaluation(engine: &JSEngine) {
    record_case(
        engine,
        "module.failures_and_evaluation",
        &[
            "CompileModule1",
            "FinishLoadingImportedModuleFailedWithPendingException",
            "GetPromiseState",
            "JS_ClearPendingException",
            "JS_GetPendingException",
            "JS_GetPromiseResult",
            "JS_GetRuntime",
            "JS_IsExceptionPending",
            "JS_SetPendingException",
            "LoadRequestedModules1",
            "ModuleEvaluate",
            "ModuleLink",
            "RegisterScriptEnvironmentPreparer",
            "RunScriptEnvironmentPreparerClosure",
            "SetModuleLoadHook",
            "ThrowOnModuleEvaluationFailure",
        ],
        |cx, _global| unsafe {
            let rt = jsapi::JS_GetRuntime(cx.raw_cx());
            let queue = install_job_queue(cx);

            let options = CompileOptionsWrapper::new(cx, c"contract-ok.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("export const ok = 1;"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            assert!(evaluation.get().is_object());
            rooted!(&in(cx) let evaluation = evaluation.get().to_object());
            let ok_sync_throw_result = ThrowOnModuleEvaluationFailure(
                cx,
                evaluation.handle(),
                ModuleErrorBehaviour::ThrowModuleErrorsSync,
            );
            let ok_sync_has_pending = jsapi::JS_IsExceptionPending(cx.raw_cx());

            let options = CompileOptionsWrapper::new(cx, c"contract-throw.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("throw 9;"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            assert!(evaluation.get().is_object());
            rooted!(&in(cx) let evaluation = evaluation.get().to_object());
            let throw_promise_state = GetPromiseState(evaluation.handle());
            let throw_sync_result = ThrowOnModuleEvaluationFailure(
                cx,
                evaluation.handle(),
                ModuleErrorBehaviour::ThrowModuleErrorsSync,
            );
            let throw_sync_has_pending = jsapi::JS_IsExceptionPending(cx.raw_cx());
            let mut thrown = UndefinedValue();
            let destination = MutableHandleValue::from_marked_location(&mut thrown);
            assert!(jsapi::JS_GetPendingException(cx.raw_cx(), destination));
            let thrown_value = observe(cx, thrown);
            jsapi::JS_ClearPendingException(cx.raw_cx());

            jsapi::SetModuleLoadHook(rt, Some(fail_with_pending));
            STATIC_FINISHED.with(|done| done.set(false));
            let options = CompileOptionsWrapper::new(cx, c"contract-missing.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("import './missing.js';"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            rooted!(&in(cx) let host_defined = UndefinedValue());
            rooted!(&in(cx) let mut promise = ptr::null_mut::<JSObject>());
            assert!(LoadRequestedModules1(
                cx,
                module.handle(),
                host_defined.handle(),
                promise.handle_mut(),
            ));
            assert!(!promise.get().is_null());
            let missing_load_state = GetPromiseState(promise.handle());
            let mut reason = UndefinedValue();
            let destination = MutableHandleValue::from_marked_location(&mut reason);
            js::glue::JS_GetPromiseResult(
                Handle::from_marked_location(&promise.get()),
                destination,
            );
            let missing_reason = observe(cx, reason);
            assert!(
                STATIC_FINISHED.with(Cell::get),
                "FinishLoadingImportedModuleFailedWithPendingException must report failure"
            );
            jsapi::SetModuleLoadHook(rt, None);

            RegisterScriptEnvironmentPreparer(cx.raw_cx(), Some(prepare_and_run));
            PREPARED.with(|prepared| prepared.borrow_mut().clear());
            let options = CompileOptionsWrapper::new(cx, c"contract-tla.js".to_owned(), 1);
            let module = CompileModule1(
                cx,
                options.ptr,
                &mut transform_str_to_source_text("await null; throw 5;"),
            );
            assert!(!module.is_null());
            rooted!(&in(cx) let module = module);
            prepare_module(cx, module.handle());
            assert!(ModuleLink(cx, module.handle()));
            rooted!(&in(cx) let mut evaluation = UndefinedValue());
            assert!(ModuleEvaluate(cx, module.handle(), evaluation.handle_mut()));
            assert!(evaluation.get().is_object());
            rooted!(&in(cx) let evaluation = evaluation.get().to_object());
            let tla_async_result = ThrowOnModuleEvaluationFailure(
                cx,
                evaluation.handle(),
                ModuleErrorBehaviour::ReportModuleErrorsAsync,
            );
            RunJobs(cx);
            let tla_promise_state = GetPromiseState(evaluation.handle());
            let tla_prepared = PREPARED.with(|prepared| prepared.borrow().clone());
            let tla_has_pending_after_jobs = jsapi::JS_IsExceptionPending(cx.raw_cx());
            remove_job_queue(cx, queue);
            Observation::new(vec![
                (
                    "ok_sync_throw_result",
                    Observed::Boolean(ok_sync_throw_result),
                ),
                (
                    "ok_sync_has_pending",
                    Observed::Boolean(ok_sync_has_pending),
                ),
                (
                    "throw_promise_state",
                    Observed::String(promise_state_name(throw_promise_state).to_owned()),
                ),
                ("throw_sync_result", Observed::Boolean(throw_sync_result)),
                (
                    "throw_sync_has_pending",
                    Observed::Boolean(throw_sync_has_pending),
                ),
                ("thrown_value", thrown_value),
                (
                    "missing_load_promise_state",
                    Observed::String(promise_state_name(missing_load_state).to_owned()),
                ),
                ("missing_reason", missing_reason),
                (
                    "missing_failure_reported",
                    Observed::Boolean(STATIC_FINISHED.with(Cell::get)),
                ),
                ("tla_async_result", Observed::Boolean(tla_async_result)),
                (
                    "tla_promise_state",
                    Observed::String(promise_state_name(tla_promise_state).to_owned()),
                ),
                (
                    "tla_prepared",
                    Observed::Array(
                        tla_prepared
                            .into_iter()
                            .map(|value| Observed::Number(value as f64))
                            .collect(),
                    ),
                ),
                (
                    "tla_has_pending_after_jobs",
                    Observed::Boolean(tla_has_pending_after_jobs),
                ),
            ])
        },
    );
}

pub fn run(engine: &JSEngine) {
    json_and_synthetic(engine);
    privates_hooks_and_requests(engine);
    load_requested(engine);
    dynamic_import(engine);
    failures_and_evaluation(engine);
}
