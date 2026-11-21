/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Off-thread compilation, frontend errors and stencil ownership.

use std::mem::MaybeUninit;
use std::ptr;
use std::sync::Arc;

use js::glue::CompileGlobalScriptToStencil_C;
use js::jsapi::{self, HadFrontendErrors, OnNewGlobalHookOption, Value};
use js::jsval::UndefinedValue;
use js::offthread::{CompilationResult, FrontendContext, compile_to_stencil_offthread};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    ConvertFrontendErrorsToRuntimeErrors, InstantiateGlobalStencil, JS_ExecuteScript,
    JS_NewGlobalObject,
};
use js::rust::{
    CompileOptionsWrapper, HandleObject as SafeHandleObject, JSEngine, OwningCompileOptionsWrapper,
    RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS, transform_str_to_source_text,
};

use crate::support::{Value as Observed, observe_field};

fn with_global(engine: &JSEngine, f: impl FnOnce(&mut js::context::JSContext, SafeHandleObject)) {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(!global.get().is_null(), "JS_NewGlobalObject returned null");
        let mut realm = AutoRealm::new_from_handle(cx, global.handle());
        let (global, cx) = realm.global_and_reborrow();
        f(cx, global);
    }
}

unsafe fn execute(
    cx: &mut js::context::JSContext,
    script: *mut jsapi::JSScript,
) -> Result<Value, ()> {
    rooted!(&in(cx) let script = script);
    rooted!(&in(cx) let mut rval = UndefinedValue());
    if JS_ExecuteScript(cx, script.handle(), rval.handle_mut()) {
        Ok(rval.get())
    } else {
        Err(())
    }
}

fn offthread_success(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        let options = CompileOptionsWrapper::new(cx, c"stencil.js".to_owned(), 1);
        let result = compile_to_stencil_offthread(options.ptr, Arc::new("6 * 7".to_owned()), Some)
            .finish()
            .expect("off-thread compilation failed");
        let CompilationResult {
            stencil,
            mut storage,
            fc,
            compile_options,
        } = result;
        assert!(!HadFrontendErrors(*fc));
        let stencil = stencil.expect("no stencil");
        let instantiate_options = (&compile_options).into();
        let script =
            InstantiateGlobalStencil(cx, &instantiate_options, *stencil, storage.as_mut_ptr());
        assert!(!script.is_null());
        let instantiated_result = execute(cx, script).unwrap().to_int32();
        assert_eq!(instantiated_result, 42);
        observe_field(
            "offthread_result",
            Observed::Number(instantiated_result as f64),
        );

        rooted!(&in(cx) let script = script);
        rooted!(&in(cx) let mut rval = UndefinedValue());
        assert!(JS_ExecuteScript(cx, script.handle(), rval.handle_mut()));
        let executed_result = rval.get().to_int32();
        assert_eq!(executed_result, 42);
        observe_field(
            "offthread_executed",
            Observed::Number(executed_result as f64),
        );

        jsapi::StencilAddRef(*stencil);
        jsapi::StencilRelease(*stencil);
    });
}

fn offthread_failure(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        let options = CompileOptionsWrapper::new(cx, c"bad-stencil.js".to_owned(), 1);
        let result =
            compile_to_stencil_offthread(options.ptr, Arc::new("var = ;".to_owned()), Some)
                .finish()
                .expect("off-thread compilation failed");
        let CompilationResult {
            stencil,
            fc,
            compile_options,
            ..
        } = result;
        assert!(stencil.is_none());
        observe_field(
            "failed_stencil_absent",
            Observed::Boolean(stencil.is_none()),
        );
        assert!(HadFrontendErrors(*fc));
        observe_field(
            "failed_had_errors",
            Observed::Boolean(HadFrontendErrors(*fc)),
        );
        assert!(ConvertFrontendErrorsToRuntimeErrors(
            cx,
            *fc,
            compile_options.read_only()
        ));
        crate::support::assert_exception_named(cx, "SyntaxError");
        observe_field("failed_has_exception", Observed::Boolean(true));
    });
}

fn explicit_stencil(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        let options = CompileOptionsWrapper::new(cx, c"stencil-raw.js".to_owned(), 1);
        let fc = FrontendContext::new();
        fc.set_stack_quota(jsapi::ThreadStackQuotaForSize(2 * 1024 * 1024));
        let owning = OwningCompileOptionsWrapper::new_for_fc(&fc, options.ptr);
        let mut stencil = MaybeUninit::uninit();
        let mut source = transform_str_to_source_text("21 + 21");
        CompileGlobalScriptToStencil_C(*fc, owning.read_only(), &mut source, stencil.as_mut_ptr());
        let stencil = stencil.assume_init();
        assert!(!HadFrontendErrors(*fc));
        observe_field(
            "explicit_had_errors",
            Observed::Boolean(HadFrontendErrors(*fc)),
        );
        assert!(!stencil.mRawPtr.is_null());
        observe_field(
            "explicit_stencil_non_null",
            Observed::Boolean(!stencil.mRawPtr.is_null()),
        );
        jsapi::StencilAddRef(stencil.mRawPtr);
        jsapi::StencilRelease(stencil.mRawPtr);
        jsapi::StencilRelease(stencil.mRawPtr);
        observe_field("explicit_refcount_balanced", Observed::Boolean(true));
    });
}

pub fn run(engine: &JSEngine) {
    let mut steps = crate::support::Steps::new(
        "stencil.offthread_compile_instantiate",
        &[
            "CompileGlobalScriptToStencil_C",
            "ConvertFrontendErrorsToRuntimeErrors",
            "DeleteOwningCompileOptions",
            "DestroyFrontendContext",
            "HadFrontendErrors",
            "InstantiateGlobalStencil",
            "NewFrontendContext",
            "OwningCompileOptions_for_fc",
            "PrepareForInstantiate",
            "SetNativeStackQuota",
            "StencilAddRef",
            "StencilRelease",
            "ThreadStackQuotaForSize",
        ],
    );
    steps.step("offthread_success", || offthread_success(engine));
    steps.step("offthread_failure", || offthread_failure(engine));
    steps.step("explicit_stencil", || explicit_stencil(engine));
    steps.finish();
}
