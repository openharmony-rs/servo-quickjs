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

/// `slice` of a typed array whose buffer shrank to nothing while `slice` converted its start: a
/// zeroed result of the requested length, without a copy (Q39).
#[test]
fn slice_of_a_source_shrunk_to_nothing() {
    let result = eval_string(
        "var results = [];
         for (var start = 0; start <= 4; start++) {
           var buffer = new ArrayBuffer(16, { maxByteLength: 16 });
           var source = new Int32Array(buffer).fill(1);
           var sliced = source.slice({ valueOf() { buffer.resize(0); return start; } });
           results.push(sliced.length + ':' + sliced.join(''));
         }
         results.join()",
    );
    assert_eq!(result, "4:0000,3:000,2:00,1:0,0:");
}

/// ArrayBuffer converts `maxByteLength` with ToIndex: a value that is not an index throws a
/// RangeError, where it wrapped modulo 2^64. SharedArrayBuffer, not exposed here, shares the code
/// (Q40).
#[test]
fn max_byte_length_is_an_index() {
    let result = eval_string(
        "[2 ** 64 + 4096, Infinity, -Infinity, -1, -(2 ** 63), 2 ** 53, 4096, NaN, -0.5].map(value => {
           try { return String(new ArrayBuffer(0, { maxByteLength: value }).maxByteLength); }
           catch (e) { return e.name; }
         }).join()",
    );
    assert_eq!(
        result,
        "RangeError,RangeError,RangeError,RangeError,RangeError,RangeError,4096,0,0"
    );
}
