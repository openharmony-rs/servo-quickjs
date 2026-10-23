/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Fixes to Bellard's QuickJS built-ins that are not about the embedding. Undefined behavior that
//! UBSan reports is fixed without changing results, which these tests pin.

use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use mozjs::conversions::jsstr_to_string;
use mozjs::jsapi::OnNewGlobalHookOption;
use mozjs::jsval::UndefinedValue;
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::JS_NewGlobalObject;
use mozjs::rust::{
    evaluate_script, CompileOptionsWrapper, JSEngine, JSEngineHandle, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS,
};

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

/// Evaluates `source` in a new runtime; returns its result, a string.
fn eval_string(source: &str) -> String {
    let mut runtime = Runtime::new(engine());
    let context = runtime.cx();
    unsafe {
        rooted!(&in(context) let global = JS_NewGlobalObject(
            context,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        let mut realm = AutoRealm::new_from_handle(context, global.handle());
        let (global, cx) = realm.global_and_reborrow();
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let options = CompileOptionsWrapper::new(cx, c"builtins.js".to_owned(), 1);
        evaluate_script(cx, global, source, rval.handle_mut(), options).unwrap();
        jsstr_to_string(cx, NonNull::new(rval.get().to_string()).unwrap())
    }
}

/// Normalization grows a DynBuf, as compiling does (Q38).
#[test]
fn normalization() {
    let result = eval_string("'e\\u0301'.repeat(1000).normalize('NFC').length.toString()");
    assert_eq!(result, "1000");
}
