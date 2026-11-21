/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Strings, conversions, property keys and JSON.

#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::slice;

use js::context::JSContext;
use js::conversions::{ToJSValConvertible, jsstr_to_string};
use js::jsapi::{
    JS_ClearPendingException, JS_DeprecatedStringHasLatin1Chars, JS_IsExceptionPending, JSString,
    JSType, SymbolCode, jsid,
};
use js::jsval::{DoubleValue, Int32Value, JSVal, StringValue, UndefinedValue};
use js::rooted;
use js::rust::wrappers2::{
    EncodeStringToUTF8, GetWellKnownSymbol, JS_AtomizeAndPinString, JS_CompareStrings,
    JS_ConcatStrings, JS_DefinePropertyById2, JS_GetLatin1StringCharsAndLength, JS_GetStringCharAt,
    JS_GetTwoByteStringCharsAndLength, JS_IdToValue, JS_IndexToId, JS_IsIdentifier,
    JS_NewDependentString, JS_NewStringCopyN, JS_NewUCStringCopyN, JS_ParseJSON,
    JS_ParseJSONWithReviver, JS_SetProperty, JS_Stringify, JS_TypeOfValue, JS_ValueToId,
    JS_ValueToSource, OrdinaryToPrimitive, RUST_INTERNED_STRING_TO_JSID, RUST_SYMBOL_TO_JSID,
    SameValue, StrictlyEqual, ToInt8Slow, ToInt16Slow, ToJSON, ToPrimitive, ToUint8Slow,
};
use js::rust::{
    CompileOptionsWrapper, HandleObject, HandleValue, JSEngine, ToBoolean, ToInt32, ToInt64,
    ToNumber, ToString, ToUint16, ToUint32, ToUint64, evaluate_script,
};

use crate::support::{Value as Observed, observe_field};

mod extra_contract {
    /* This Source Code Form is subject to the terms of the Mozilla Public
     * License, v. 2.0. If a copy of the MPL was not distributed with this
     * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

    //! String, atom, id, JSON and memory entry points.

    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::NonNull;
    use std::slice;

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::glue::{
        CreateJSExternalStringCallbacks, DeleteJSExternalStringCallbacks,
        JS_ForgetStringLinearness, JSExternalStringCallbacksTraps,
    };
    use js::jsapi::js::{StringIsArrayIndex, StringIsArrayIndex1};
    use js::jsapi::mozilla::{MallocSizeOf, Range};
    use js::jsapi::{
        AtomToLinearString, GetLinearStringCharAt, GetLinearStringLength, JS_GetStringLength,
        JS_IsIdentifier1, JS_LinearStringEqualsAscii, JS_LinearStringEqualsAscii1,
        JS_PutEscapedLinearString, JSAtom, JSExternalStringCallbacks, JSPropertySpec_Name, jsid,
    };
    use js::jsval::{BooleanValue, Int32Value, NullValue, StringValue, UndefinedValue};
    use js::rooted;
    use js::rust::wrappers2::{
        JS_AtomizeAndPinStringN, JS_AtomizeString, JS_AtomizeStringN, JS_AtomizeUCString,
        JS_AtomizeUCStringN, JS_CopyStringChars, JS_EncodeStringToBuffer, JS_EnsureLinearString,
        JS_GetEmptyString, JS_GetEmptyStringValue, JS_GetStringEncodingLength, JS_IsIdentifier,
        JS_NewExternalStringLatin1, JS_NewExternalUCString, JS_NewMaybeExternalStringLatin1,
        JS_NewMaybeExternalStringUTF8, JS_NewMaybeExternalUCString, JS_NewPlainObject,
        JS_NewStringCopyUTF8N, JS_NewStringCopyUTF8Z, JS_NewStringCopyZ, JS_NewUCStringCopyN,
        JS_NewUCStringCopyZ, JS_ParseJSON1, JS_ParseJSON2, JS_ParseJSONWithReviver1,
        JS_PutEscapedString, JS_StringEqualsAscii, JS_StringEqualsAscii1, JS_StringToId,
        JS_StringifyWithLengthHint, JS_free, JS_malloc, JS_realloc, PropertySpecNameEqualsId,
        PropertySpecNameToPermanentId, RUST_JSID_IS_INT, RUST_JSID_IS_STRING, RUST_JSID_IS_VOID,
        RUST_JSID_TO_INT, RUST_JSID_TO_STRING, StringToLinearStringSlow, ToBooleanSlow,
        ToInt32Slow, ToInt64Slow, ToJSONMaybeSafely, ToNumberSlow, ToObjectSlow, ToStringSlow,
        ToUint16Slow, ToUint32Slow, ToUint64Slow, int_to_jsid,
    };
    use js::rust::{HandleObject, HandleValue, JSEngine, MutableHandle};

    use crate::support::{Observation, Value, eval, record_case, set_global};

    #[cfg(all(not(feature = "js-quickjs"), not(target_env = "msvc")))]
    mod encode_abi {
        use std::ffi::{c_char, c_void};
        use std::ptr;

        use js::context::JSContext;
        use js::jsapi::mozilla::Range;
        use js::jsapi::{
            Handle, JSString, Latin1CharsZ, MutableHandle as RawMutableHandle, TwoByteChars,
            TwoByteCharsZ, UTF8Chars, jsid,
        };

        unsafe extern "C" {
            // These C++ functions return a non-trivial `UniqueChars`, so the platform ABI passes a
            // hidden result pointer. Declaring that explicitly is required; the bindgen signature
            // returns a plain integer and crashes when called this way.
            #[link_name = "_Z22JS_EncodeStringToASCIIP9JSContextP8JSString"]
            fn js_encode_string_to_ascii(
                out: *mut *mut c_char,
                cx: *mut c_void,
                string: *mut JSString,
            );
            #[link_name = "_Z23JS_EncodeStringToLatin1P9JSContextP8JSString"]
            fn js_encode_string_to_latin1(
                out: *mut *mut c_char,
                cx: *mut c_void,
                string: *mut JSString,
            );
            #[link_name = "_Z21JS_EncodeStringToUTF8P9JSContextN2JS6HandleIP8JSStringEE"]
            fn js_encode_string_to_utf8(
                out: *mut *mut c_char,
                cx: *mut c_void,
                string: Handle<*mut JSString>,
            );
            #[link_name = "_Z12JS_CharsToIdP9JSContextN2JS12TwoByteCharsENS1_13MutableHandleINS1_11PropertyKeyEEE"]
            fn js_chars_to_id(
                cx: *mut c_void,
                chars: *const TwoByteChars,
                id: RawMutableHandle<jsid>,
            ) -> bool;
            #[link_name = "_ZN2JS34LossyTwoByteCharsToNewLatin1CharsZEP9JSContextRKN7mozilla5RangeIKDsEE"]
            fn js_lossy_two_byte_to_latin1(
                out: *mut Latin1CharsZ,
                cx: *mut c_void,
                chars: *const Range<u16>,
            );
            #[link_name = "_ZN2JS27UTF8CharsToNewTwoByteCharsZEP9JSContextRKNS_9UTF8CharsEPmm"]
            fn js_utf8_to_two_byte(
                out: *mut TwoByteCharsZ,
                cx: *mut c_void,
                chars: *const UTF8Chars,
                out_len: *mut usize,
                arena: u32,
            );
            #[link_name = "_ZN2JS32LossyUTF8CharsToNewTwoByteCharsZEP9JSContextRKNS_9UTF8CharsEPmm"]
            fn js_lossy_utf8_to_two_byte(
                out: *mut TwoByteCharsZ,
                cx: *mut c_void,
                chars: *const UTF8Chars,
                out_len: *mut usize,
                arena: u32,
            );
        }

        pub unsafe fn encode_string_to_ascii(
            cx: &mut JSContext,
            string: *mut JSString,
        ) -> *mut c_char {
            let mut out = ptr::null_mut();
            js_encode_string_to_ascii(&mut out, cx.raw_cx().cast(), string);
            out
        }

        pub unsafe fn encode_string_to_latin1(
            cx: &mut JSContext,
            string: *mut JSString,
        ) -> *mut c_char {
            let mut out = ptr::null_mut();
            js_encode_string_to_latin1(&mut out, cx.raw_cx().cast(), string);
            out
        }

        pub unsafe fn encode_string_to_utf8(
            cx: &mut JSContext,
            string: Handle<*mut JSString>,
        ) -> *mut c_char {
            let mut out = ptr::null_mut();
            js_encode_string_to_utf8(&mut out, cx.raw_cx().cast(), string);
            out
        }

        pub unsafe fn chars_to_id(
            cx: &mut JSContext,
            chars: TwoByteChars,
            id: RawMutableHandle<jsid>,
        ) -> bool {
            js_chars_to_id(cx.raw_cx().cast(), &chars, id)
        }

        pub unsafe fn lossy_two_byte_to_latin1(
            cx: &mut JSContext,
            chars: &Range<u16>,
        ) -> Latin1CharsZ {
            let mut out = std::mem::MaybeUninit::uninit();
            js_lossy_two_byte_to_latin1(out.as_mut_ptr(), cx.raw_cx().cast(), chars);
            out.assume_init()
        }

        pub unsafe fn utf8_to_two_byte(
            cx: &mut JSContext,
            chars: &UTF8Chars,
            out_len: &mut usize,
        ) -> TwoByteCharsZ {
            let mut out = std::mem::MaybeUninit::uninit();
            js_utf8_to_two_byte(out.as_mut_ptr(), cx.raw_cx().cast(), chars, out_len, 0);
            out.assume_init()
        }

        pub unsafe fn lossy_utf8_to_two_byte(
            cx: &mut JSContext,
            chars: &UTF8Chars,
            out_len: &mut usize,
        ) -> TwoByteCharsZ {
            let mut out = std::mem::MaybeUninit::uninit();
            js_lossy_utf8_to_two_byte(out.as_mut_ptr(), cx.raw_cx().cast(), chars, out_len, 0);
            out.assume_init()
        }
    }

    #[cfg(feature = "js-quickjs")]
    mod encode_abi {
        use std::ffi::c_char;

        use js::context::JSContext;
        use js::jsapi::mozilla::Range;
        use js::jsapi::{
            Handle, JSString, Latin1CharsZ, MutableHandle as RawMutableHandle, TwoByteChars,
            TwoByteCharsZ, UTF8Chars, jsid,
        };
        use js::rust::wrappers2::{
            JS_CharsToId, JS_EncodeStringToASCII, JS_EncodeStringToLatin1, JS_EncodeStringToUTF8,
            LossyTwoByteCharsToNewLatin1CharsZ, LossyUTF8CharsToNewTwoByteCharsZ,
            UTF8CharsToNewTwoByteCharsZ,
        };
        use js::rust::{Handle as SafeHandle, MutableHandle as SafeMutableHandle};

        pub unsafe fn encode_string_to_ascii(
            cx: &mut JSContext,
            string: *mut JSString,
        ) -> *mut c_char {
            JS_EncodeStringToASCII(cx, string) as *mut c_char
        }

        pub unsafe fn encode_string_to_latin1(
            cx: &mut JSContext,
            string: *mut JSString,
        ) -> *mut c_char {
            JS_EncodeStringToLatin1(cx, string) as *mut c_char
        }

        pub unsafe fn encode_string_to_utf8(
            cx: &mut JSContext,
            string: Handle<*mut JSString>,
        ) -> *mut c_char {
            let string = SafeHandle::from_marked_location(string.ptr);
            JS_EncodeStringToUTF8(cx, string) as *mut c_char
        }

        pub unsafe fn chars_to_id(
            cx: &mut JSContext,
            chars: TwoByteChars,
            id: RawMutableHandle<jsid>,
        ) -> bool {
            let id = SafeMutableHandle::from_marked_location(id.ptr);
            JS_CharsToId(cx, chars, id)
        }

        pub unsafe fn lossy_two_byte_to_latin1(
            cx: &mut JSContext,
            chars: &Range<u16>,
        ) -> Latin1CharsZ {
            LossyTwoByteCharsToNewLatin1CharsZ(cx, chars)
        }

        pub unsafe fn utf8_to_two_byte(
            cx: &mut JSContext,
            chars: &UTF8Chars,
            out_len: &mut usize,
        ) -> TwoByteCharsZ {
            UTF8CharsToNewTwoByteCharsZ(cx, chars, out_len, 0)
        }

        pub unsafe fn lossy_utf8_to_two_byte(
            cx: &mut JSContext,
            chars: &UTF8Chars,
            out_len: &mut usize,
        ) -> TwoByteCharsZ {
            LossyUTF8CharsToNewTwoByteCharsZ(cx, chars, out_len, 0)
        }
    }

    /// MSVC uses a different C++ ABI for the hidden `UniqueChars` return value, and the generated
    /// bindings declare it as a scalar. Keep the module present so the rest of the file type-checks;
    /// the two cases that use it record an unsupported-platform observation before calling it.
    #[cfg(all(not(feature = "js-quickjs"), target_env = "msvc"))]
    mod encode_abi {
        use std::ffi::c_char;

        use js::context::JSContext;
        use js::jsapi::mozilla::Range;
        use js::jsapi::{
            Handle, JSString, Latin1CharsZ, MutableHandle as RawMutableHandle, TwoByteChars,
            TwoByteCharsZ, UTF8Chars, jsid,
        };

        fn unsupported() -> ! {
            unreachable!("string encoding ABI is not available on MSVC")
        }

        pub unsafe fn encode_string_to_ascii(
            cx: &mut JSContext,
            string: *mut JSString,
        ) -> *mut c_char {
            let _ = (cx, string);
            unsupported()
        }

        pub unsafe fn encode_string_to_latin1(
            cx: &mut JSContext,
            string: *mut JSString,
        ) -> *mut c_char {
            let _ = (cx, string);
            unsupported()
        }

        pub unsafe fn encode_string_to_utf8(
            cx: &mut JSContext,
            string: Handle<*mut JSString>,
        ) -> *mut c_char {
            let _ = (cx, string);
            unsupported()
        }

        pub unsafe fn chars_to_id(
            cx: &mut JSContext,
            chars: TwoByteChars,
            id: RawMutableHandle<jsid>,
        ) -> bool {
            let _ = (cx, chars, id);
            unsupported()
        }

        pub unsafe fn lossy_two_byte_to_latin1(
            cx: &mut JSContext,
            chars: &Range<u16>,
        ) -> Latin1CharsZ {
            let _ = (cx, chars);
            unsupported()
        }

        pub unsafe fn utf8_to_two_byte(
            cx: &mut JSContext,
            chars: &UTF8Chars,
            out_len: &mut usize,
        ) -> TwoByteCharsZ {
            let _ = (cx, chars, out_len);
            unsupported()
        }

        pub unsafe fn lossy_utf8_to_two_byte(
            cx: &mut JSContext,
            chars: &UTF8Chars,
            out_len: &mut usize,
        ) -> TwoByteCharsZ {
            let _ = (cx, chars, out_len);
            unsupported()
        }
    }

    const APIS: &[&str] = &[
        "AtomToLinearString",
        "CreateJSExternalStringCallbacks",
        "DeleteJSExternalStringCallbacks",
        "GetLinearStringCharAt",
        "GetLinearStringLength",
        "JS_AtomizeAndPinStringN",
        "JS_AtomizeString",
        "JS_AtomizeStringN",
        "JS_AtomizeUCString",
        "JS_AtomizeUCStringN",
        "JS_CharsToId",
        "JS_CopyStringChars",
        "JS_EncodeStringToASCII",
        "JS_EncodeStringToBuffer",
        "JS_EncodeStringToLatin1",
        "JS_EncodeStringToUTF8",
        "JS_EnsureLinearString",
        "JS_ForgetStringLinearness",
        "JS_GetEmptyString",
        "JS_GetEmptyStringValue",
        "JS_GetStringEncodingLength",
        "JS_GetStringLength",
        "JS_IsIdentifier",
        "JS_IsIdentifier1",
        "JS_LinearStringEqualsAscii",
        "JS_LinearStringEqualsAscii1",
        "JS_NewExternalStringLatin1",
        "JS_NewExternalUCString",
        "JS_NewMaybeExternalStringLatin1",
        "JS_NewMaybeExternalStringUTF8",
        "JS_NewMaybeExternalUCString",
        "JS_NewStringCopyUTF8N",
        "JS_NewStringCopyUTF8Z",
        "JS_NewStringCopyZ",
        "JS_NewUCStringCopyZ",
        "JS_ParseJSON1",
        "JS_ParseJSON2",
        "JS_ParseJSONWithReviver1",
        "JS_PutEscapedLinearString",
        "JS_PutEscapedString",
        "JS_StringEqualsAscii",
        "JS_StringEqualsAscii1",
        "JS_StringToId",
        "JS_StringifyWithLengthHint",
        "JS_free",
        "JS_malloc",
        "JS_realloc",
        "LossyTwoByteCharsToNewLatin1CharsZ",
        "LossyUTF8CharsToNewTwoByteCharsZ",
        "PropertySpecNameEqualsId",
        "PropertySpecNameToPermanentId",
        "RUST_JSID_IS_INT",
        "RUST_JSID_IS_STRING",
        "RUST_JSID_IS_VOID",
        "RUST_JSID_TO_INT",
        "RUST_JSID_TO_STRING",
        "StringIsArrayIndex",
        "StringIsArrayIndex1",
        "StringToLinearStringSlow",
        "ToBooleanSlow",
        "ToInt32Slow",
        "ToInt64Slow",
        "ToJSONMaybeSafely",
        "ToNumberSlow",
        "ToObjectSlow",
        "ToStringSlow",
        "ToUint16Slow",
        "ToUint32Slow",
        "ToUint64Slow",
        "UTF8CharsToNewTwoByteCharsZ",
        "int_to_jsid",
    ];

    mod json_contract {
        /* This Source Code Form is subject to the terms of the Mozilla Public
         * License, v. 2.0. If a copy of the MPL was not distributed with this
         * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

        #![allow(unsafe_op_in_unsafe_fn)]

        //! Additional edge-case scenarios for the engine contract.
        //!
        //! These cases do not replace the per-entry smoke cases. They add independent observations for
        //! values, error paths, ordering and object lifetime so a shim that merely returns "success"
        //! cannot pass by matching a boolean.

        use std::ffi::c_void;
        use std::slice;

        use js::context::JSContext;
        use js::jsapi::{JS_ClearPendingException, JS_IsExceptionPending};
        use js::jsval::{JSVal, UndefinedValue};
        use js::rooted;
        use js::rust::wrappers2::{JS_GetPendingException, JS_ParseJSON, JS_Stringify};
        use js::rust::{HandleObject, HandleValue, JSEngine};

        use crate::support::{Observation, Value, eval, observe_property, record_case, set_global};

        /// Reads and clears the pending exception without requiring it to be an `Error` object, then
        /// returns its `name` when it has one. Primitive throws are legal in JavaScript and have no name.
        unsafe fn exception_name(cx: &mut JSContext) -> Option<String> {
            assert!(JS_IsExceptionPending(cx.raw_cx()));
            rooted!(&in(cx) let mut exception = UndefinedValue());
            assert!(JS_GetPendingException(cx, exception.handle_mut()));
            JS_ClearPendingException(cx.raw_cx());
            let value = exception.get();
            if value.is_object() {
                match observe_property(cx, value, c"name") {
                    Value::String(name) => Some(name),
                    _ => None,
                }
            } else {
                None
            }
        }

        #[track_caller]
        unsafe fn assert_exception_named(cx: &mut JSContext, expected: &str, context: &str) {
            assert_eq!(
                exception_name(cx).as_deref(),
                Some(expected),
                "{context} must throw {expected}"
            );
        }

        unsafe extern "C" fn append_json(buf: *const u16, len: u32, data: *mut c_void) -> bool {
            let out = &mut *(data as *mut Option<String>);
            out.get_or_insert_with(String::new)
                .push_str(&String::from_utf16_lossy(slice::from_raw_parts(
                    buf,
                    len as usize,
                )));
            true
        }

        fn number(value: f64) -> Value {
            if value.is_nan() {
                Value::String("NaN".to_owned())
            } else if value == f64::INFINITY {
                Value::String("Infinity".to_owned())
            } else if value == f64::NEG_INFINITY {
                Value::String("-Infinity".to_owned())
            } else if value == 0.0 && value.is_sign_negative() {
                Value::String("-0".to_owned())
            } else {
                Value::Number(value)
            }
        }

        unsafe fn json_text(cx: &mut JSContext, value: JSVal) -> Option<String> {
            let mut out: Option<String> = None;
            rooted!(&in(cx) let mut value = value);
            let ok = js::rust::wrappers2::ToJSON(
                cx,
                value.handle(),
                HandleObject::null(),
                HandleValue::null(),
                Some(append_json),
                &mut out as *mut _ as *mut c_void,
            );
            if ok { out } else { None }
        }

        unsafe fn parse_json(cx: &mut JSContext, text: &str) -> Option<JSVal> {
            let units: Vec<u16> = text.encode_utf16().collect();
            rooted!(&in(cx) let mut value = UndefinedValue());
            if JS_ParseJSON(cx, units.as_ptr(), units.len() as u32, value.handle_mut()) {
                Some(value.get())
            } else {
                None
            }
        }

        unsafe fn stringify(
            cx: &mut JSContext,
            value: JSVal,
            replacer: HandleObject,
            space: HandleValue,
        ) -> Result<String, ()> {
            let mut out: Option<String> = None;
            rooted!(&in(cx) let mut value = value);
            if JS_Stringify(
                cx,
                value.handle_mut(),
                replacer,
                space,
                Some(append_json),
                &mut out as *mut _ as *mut c_void,
            ) {
                Ok(out.unwrap_or_default())
            } else {
                Err(())
            }
        }

        fn json_parse_values(engine: &JSEngine) {
            record_case(
                engine,
                "edge.json_parse_values",
                &["JS_ParseJSON", "ToJSON"],
                |cx, _global| unsafe {
                    let duplicate = parse_json(cx, r#"{"a":1,"a":2}"#).expect("duplicate key JSON");
                    let duplicate = json_text(cx, duplicate).unwrap();

                    let proto =
                        parse_json(cx, r#"{"__proto__":{"x":1},"a":2}"#).expect("__proto__ JSON");
                    let proto_text = json_text(cx, proto).unwrap();
                    set_global(cx, _global, c"protoValue", proto);
                    let proto_own = eval(
                        cx,
                        _global,
                        "Object.hasOwn(protoValue, '__proto__') && protoValue.__proto__.x === 1",
                    )
                    .to_boolean();
                    let proto_unchanged = eval(
                        cx,
                        _global,
                        "Object.getPrototypeOf(protoValue) === Object.prototype",
                    )
                    .to_boolean();

                    let numbers = parse_json(cx, "[1e400,-0,9007199254740993,1.0,1e-400]")
                        .expect("number edge JSON");
                    let numbers_text = json_text(cx, numbers).unwrap();

                    let deep_text = format!("{}0{}", "[".repeat(128), "]".repeat(128));
                    let deep = parse_json(cx, &deep_text).expect("deep JSON");
                    let deep_roundtrip = json_text(cx, deep).unwrap() == deep_text;

                    let long_text = format!("\"{}\"", "a".repeat(4096));
                    let long = parse_json(cx, &long_text).expect("long JSON string");
                    set_global(cx, _global, c"longValue", long);
                    let long_length = eval(cx, _global, "longValue.length").to_int32();

                    let whitespace = parse_json(cx, " \t\r\n[1] ").is_some();

                    Observation::new(vec![
                        ("duplicate", Value::String(duplicate)),
                        ("proto_text", Value::String(proto_text)),
                        ("proto_own", Value::Boolean(proto_own)),
                        ("proto_unchanged", Value::Boolean(proto_unchanged)),
                        ("numbers_text", Value::String(numbers_text)),
                        ("deep_roundtrip", Value::Boolean(deep_roundtrip)),
                        ("long_length", Value::Number(long_length as f64)),
                        ("whitespace", Value::Boolean(whitespace)),
                    ])
                },
            );
        }

        fn json_parse_errors(engine: &JSEngine) {
            #[track_caller]
            unsafe fn assert_syntax_error(cx: &mut JSContext, text: &str) {
                assert!(parse_json(cx, text).is_none(), "JSON must reject {text:?}");
                assert_eq!(
                    exception_name(cx).as_deref(),
                    Some("SyntaxError"),
                    "invalid JSON must throw SyntaxError: {text:?}"
                );
            }

            record_case(
                engine,
                "edge.json_parse_errors",
                &[
                    "JS_ParseJSON",
                    "JS_IsExceptionPending",
                    "JS_ClearPendingException",
                ],
                |cx, _global| unsafe {
                    let invalid = [
                        "{\"a\":1,}",
                        "[1,]",
                        "{a:1}",
                        "'x'",
                        "NaN",
                        "01",
                        "\"a\nb\"",
                        "{\"a\": }",
                        "",
                    ];
                    let mut rejected = 0;
                    let mut pending = 0;
                    for text in invalid {
                        assert_syntax_error(cx, text);
                        rejected += 1;
                        pending += 1;
                    }

                    // A non-breaking space is not JSON whitespace.
                    assert_syntax_error(cx, "\u{00a0}[1]");
                    let nbsp = true;

                    // A lone surrogate inside a JSON string is data, not a syntax error.
                    let lone = parse_json(cx, "\"\\ud800x\"").expect("lone surrogate JSON");
                    let lone_text = json_text(cx, lone).unwrap();

                    Observation::new(vec![
                        ("rejected", Value::Number(rejected as f64)),
                        ("pending", Value::Number(pending as f64)),
                        ("nbsp_rejected", Value::Boolean(nbsp)),
                        ("lone", Value::String(lone_text)),
                    ])
                },
            );
        }

        fn json_stringify_values(engine: &JSEngine) {
            record_case(
                engine,
                "edge.json_stringify_values",
                &["JS_Stringify", "ToJSON"],
                |cx, global| unsafe {
                    let mixed = eval(
                        cx,
                        global,
                        "({u: undefined, n: NaN, i: Infinity, z: -0, a: [undefined, NaN]})",
                    );
                    let mixed_text =
                        stringify(cx, mixed, HandleObject::null(), HandleValue::null())
                            .expect("stringify mixed values");

                    let top = eval(cx, global, "undefined");
                    let top_text = stringify(cx, top, HandleObject::null(), HandleValue::null())
                        .expect("stringify undefined");

                    let lone = eval(cx, global, "\"\\ud800\"");
                    let lone_text = stringify(cx, lone, HandleObject::null(), HandleValue::null())
                        .expect("stringify lone surrogate");

                    let object = eval(cx, global, "({a:[1]})");
                    let space_string =
                        js::rust::wrappers2::JS_NewStringCopyN(cx, "--".as_ptr().cast(), 2);
                    rooted!(&in(cx) let space = js::jsval::StringValue(&*space_string));
                    let space_text = stringify(cx, object, HandleObject::null(), space.handle())
                        .expect("stringify space");

                    Observation::new(vec![
                        ("mixed", Value::String(mixed_text)),
                        ("top", Value::String(top_text)),
                        ("lone", Value::String(lone_text)),
                        ("space", Value::String(space_text)),
                    ])
                },
            );
        }

        fn json_stringify_errors(engine: &JSEngine) {
            record_case(
                engine,
                "edge.json_stringify_errors",
                &[
                    "JS_Stringify",
                    "JS_IsExceptionPending",
                    "JS_ClearPendingException",
                ],
                |cx, global| unsafe {
                    let cases = [
                        ("bigint", "1n"),
                        ("cycle", "(() => { const o = {}; o.self = o; return o; })()"),
                        ("tojson", "({ toJSON() { throw new Error('toJSON'); } })"),
                        ("getter", "({ get x() { throw new Error('getter'); } })"),
                    ];
                    let mut rejected = 0;
                    let mut pending = 0;
                    for (_, source) in cases {
                        let value = eval(cx, global, source);
                        assert!(
                            stringify(cx, value, HandleObject::null(), HandleValue::null())
                                .is_err(),
                            "JSON.stringify must reject {source:?}"
                        );
                        rejected += 1;
                        let expected = match source {
                            "1n" | "(() => { const o = {}; o.self = o; return o; })()" => {
                                "TypeError"
                            },
                            _ => "Error",
                        };
                        assert_exception_named(
                            cx,
                            expected,
                            &format!("JSON.stringify({source:?})"),
                        );
                        pending += 1;
                    }
                    assert_eq!(rejected, 4);
                    assert_eq!(pending, 4);
                    Observation::new(vec![
                        ("rejected", Value::Number(rejected as f64)),
                        ("pending", Value::Number(pending as f64)),
                    ])
                },
            );
        }

        fn json_stringify_replacer(engine: &JSEngine) {
            record_case(
                engine,
                "edge.json_stringify_replacer",
                &["JS_Stringify", "JS_GetProperty", "ToJSON"],
                |cx, global| unsafe {
                    let value = eval(cx, global, "({a:1,b:2,c:3})");
                    let replacer = eval(cx, global, "['b','a']").to_object();
                    rooted!(&in(cx) let replacer = replacer);
                    let filtered = stringify(cx, value, replacer.handle(), HandleValue::null())
                        .expect("stringify replacer");

                    let ordered = eval(
                        cx,
                        global,
                        "({2:'two', 10:'ten', b:1, a:2, [Symbol.toStringTag]:'tag'})",
                    );
                    let ordered_text =
                        stringify(cx, ordered, HandleObject::null(), HandleValue::null())
                            .expect("stringify property order");

                    Observation::new(vec![
                        ("filtered", Value::String(filtered)),
                        ("ordered", Value::String(ordered_text)),
                    ])
                },
            );
        }

        pub fn run(engine: &JSEngine) {
            json_parse_values(engine);
            json_parse_errors(engine);
            json_stringify_values(engine);
            json_stringify_errors(engine);
            json_stringify_replacer(engine);
        }
    }

    pub fn run(engine: &JSEngine) {
        json_contract::run(engine);
        record_case(
            engine,
            "strings.atoms_ids_json_and_memory",
            APIS,
            |cx, global| unsafe {
                #[cfg(target_env = "msvc")]
                {
                    let _ = (cx, global);
                    return Observation::new(vec![(
                        "unsupported_platform",
                        Observed::String("msvc_cxx_abi".to_owned()),
                    )]);
                }

                let mut utf8 = *b"a\xe2\x82\xac";
                let utf8_chars = js::jsapi::JS::UTF8Chars {
                    _base: range(utf8.as_mut_ptr(), utf8.len()),
                };

                rooted!(&in(cx) let ascii = JS_NewStringCopyZ(cx, c"abc".as_ptr()));
                rooted!(&in(cx) let wide = JS_NewUCStringCopyZ(cx, [0x20ac, 0].as_ptr()));
                rooted!(&in(cx) let utf8z = JS_NewStringCopyUTF8Z(
                    cx,
                    js::jsapi::JS::ConstUTF8CharsZ {
                        data_: b"caf\xc3\xa9\0".as_ptr().cast(),
                    },
                ));
                rooted!(&in(cx) let utf8n = JS_NewStringCopyUTF8N(cx, &utf8_chars));
                assert_eq!(to_rust(cx, ascii.get()), "abc");
                assert_eq!(to_rust(cx, wide.get()), "€");
                assert_eq!(to_rust(cx, utf8z.get()), "café");
                assert_eq!(to_rust(cx, utf8n.get()), "a€");
                assert_eq!(JS_GetStringLength(ascii.get()), 3);
                assert_eq!(JS_GetStringEncodingLength(cx, ascii.get()), 3);
                let encoded = encode_abi::encode_string_to_ascii(cx, ascii.get());
                assert_eq!(encode(cx, encoded), b"abc".to_vec());
                let encoded = encode_abi::encode_string_to_latin1(cx, wide.get());
                assert_eq!(encode(cx, encoded), [0xac]);
                let encoded = encode_abi::encode_string_to_utf8(cx, ascii.handle().into());
                assert_eq!(encode(cx, encoded), b"abc".to_vec());
                let mut buffer = [0i8; 4];
                assert!(JS_EncodeStringToBuffer(
                    cx,
                    ascii.get(),
                    buffer.as_mut_ptr(),
                    buffer.len()
                ));
                assert_eq!(&buffer[..3], &[b'a' as i8, b'b' as i8, b'c' as i8]);

                let mut escaped = [0i8; 8];
                let n = JS_PutEscapedString(
                    cx,
                    escaped.as_mut_ptr(),
                    escaped.len(),
                    wide.get(),
                    b'"' as c_char,
                );
                assert_eq!(CStr::from_ptr(escaped.as_ptr()).to_bytes(), br#""\u20AC"#);
                assert_eq!(n, 8);
                let linear = StringToLinearStringSlow(cx, wide.get());
                let n = JS_PutEscapedLinearString(
                    escaped.as_mut_ptr(),
                    escaped.len(),
                    linear,
                    b'"' as c_char,
                );
                assert_eq!(n, 8);

                let empty = JS_GetEmptyString(cx);
                assert_eq!(JS_GetStringLength(empty), 0);
                let mut empty_value = UndefinedValue();
                JS_GetEmptyStringValue(cx, &mut empty_value);
                assert_eq!(empty_value.to_string(), empty);

                let ext1 = external_callbacks(3);
                let p = Box::into_raw(b"abc".to_vec().into_boxed_slice());
                let latin1 = JS_NewExternalStringLatin1(cx, p.cast(), 3, ext1);
                let ext2 = external_callbacks(1);
                let p = Box::into_raw(Box::new(0x20acu16));
                let utf16 = JS_NewExternalUCString(cx, p, 1, ext2);
                rooted!(&in(cx) let latin1 = latin1);
                rooted!(&in(cx) let utf16 = utf16);
                assert_eq!(to_rust(cx, latin1.get()), "abc");
                assert_eq!(to_rust(cx, utf16.get()), "€");

                let ext3 = external_callbacks(3);
                let p = Box::into_raw(b"xyz".to_vec().into_boxed_slice());
                let mut allocated = false;
                rooted!(&in(cx) let maybe_latin1 =
            JS_NewMaybeExternalStringLatin1(cx, p.cast(), 3, ext3, &mut allocated));
                let ext4 = external_callbacks(1);
                let p = Box::into_raw(Box::new(0x2603u16));
                let mut allocated_uc = false;
                rooted!(&in(cx) let maybe_utf16 =
            JS_NewMaybeExternalUCString(cx, p, 1, ext4, &mut allocated_uc));
                let ext5 = external_callbacks(4);
                let mut utf8_bytes = *b"rust";
                let utf8_chars = js::jsapi::JS::UTF8Chars {
                    _base: range(utf8_bytes.as_mut_ptr(), utf8_bytes.len()),
                };
                let mut allocated_utf8 = false;
                rooted!(&in(cx) let maybe_utf8 =
            JS_NewMaybeExternalStringUTF8(cx, &utf8_chars, ext5, &mut allocated_utf8));
                assert_eq!(to_rust(cx, maybe_latin1.get()), "xyz");
                assert_eq!(to_rust(cx, maybe_utf16.get()), "☃");
                assert_eq!(to_rust(cx, maybe_utf8.get()), "rust");
                DeleteJSExternalStringCallbacks(external_callbacks(0));
                let _ = (allocated, allocated_uc, allocated_utf8);

                let atom_ids = atoms_and_ids(cx, global, ascii.get());
                let json_values = json(cx, global);
                let converted = conversions(cx);
                let allocated = allocator(cx);
                set_global(cx, global, c"encoded", StringValue(&*utf8z.get()));
                let encoded_matches = eval(cx, global, "encoded === 'café'").to_boolean();
                assert!(encoded_matches);
                Observation::new(vec![
                    ("ascii", Value::String(to_rust(cx, ascii.get()))),
                    ("wide", Value::String(to_rust(cx, wide.get()))),
                    ("utf8z", Value::String(to_rust(cx, utf8z.get()))),
                    ("utf8n", Value::String(to_rust(cx, utf8n.get()))),
                    (
                        "escaped",
                        Value::String(
                            String::from_utf8_lossy(CStr::from_ptr(escaped.as_ptr()).to_bytes())
                                .into_owned(),
                        ),
                    ),
                    ("encoded_matches", Value::Boolean(encoded_matches)),
                    ("atom_ids", Value::String(atom_ids)),
                    ("json", Value::String(json_values)),
                    ("converted", Value::String(converted)),
                    ("allocated", Value::Boolean(allocated)),
                ])
            },
        );
    }

    unsafe fn to_rust(cx: &mut JSContext, s: *mut js::jsapi::JSString) -> String {
        jsstr_to_string(cx, NonNull::new(s).unwrap())
    }

    unsafe fn encode(cx: &mut JSContext, p: *mut c_char) -> Vec<u8> {
        let bytes = CStr::from_ptr(p).to_bytes().to_vec();
        JS_free(cx, p as *mut c_void);
        bytes
    }

    static TRAPS: JSExternalStringCallbacksTraps = JSExternalStringCallbacksTraps {
        latin1Finalize: Some(latin1_finalize),
        utf16Finalize: Some(utf16_finalize),
        latin1SizeOfBuffer: Some(latin1_size),
        utf16SizeOfBuffer: Some(utf16_size),
    };

    unsafe extern "C" fn latin1_finalize(private: *const c_void, chars: *mut u8) {
        let _ = (private, chars);
    }

    unsafe extern "C" fn utf16_finalize(private: *const c_void, chars: *mut u16) {
        let _ = (private, chars);
    }

    unsafe extern "C" fn latin1_size(
        private: *const c_void,
        _: *const u8,
        _: MallocSizeOf,
    ) -> usize {
        private as usize
    }

    unsafe extern "C" fn utf16_size(
        private: *const c_void,
        _: *const u16,
        _: MallocSizeOf,
    ) -> usize {
        private as usize
    }

    unsafe fn external_callbacks(len: usize) -> *mut JSExternalStringCallbacks {
        CreateJSExternalStringCallbacks(&TRAPS, len as *mut c_void)
    }

    unsafe fn range<T>(ptr: *mut T, len: usize) -> Range<T> {
        let mut range: Range<T> = std::mem::zeroed();
        range.mStart.mPtr = ptr;
        range.mEnd.mPtr = ptr.add(len);
        range
    }

    unsafe fn atoms_and_ids(
        cx: &mut JSContext,
        global: HandleObject,
        ascii: *mut js::jsapi::JSString,
    ) -> String {
        let a = JS_AtomizeString(cx, c"name".as_ptr());
        let b = JS_AtomizeStringN(cx, c"name".as_ptr(), 4);
        let c = JS_AtomizeAndPinStringN(cx, c"pinned".as_ptr(), 6);
        let d = JS_AtomizeUCString(cx, [0x20ac, 0].as_ptr());
        let e = JS_AtomizeUCStringN(cx, [0x20ac].as_ptr(), 1);
        assert_eq!(to_rust(cx, a), "name");
        assert_eq!(to_rust(cx, b), "name");
        assert_eq!(to_rust(cx, c), "pinned");
        assert_eq!(to_rust(cx, d), "€");
        assert_eq!(to_rust(cx, e), "€");

        let linear = AtomToLinearString(a as *mut JSAtom);
        assert_eq!(GetLinearStringLength(linear), 4);
        assert_eq!(GetLinearStringCharAt(linear, 1), b'a' as u16);
        let ensured = JS_EnsureLinearString(cx, b);
        assert_eq!(GetLinearStringLength(ensured), 4);
        assert!(JS_LinearStringEqualsAscii(ensured, c"name".as_ptr()));
        assert!(JS_LinearStringEqualsAscii1(ensured, c"name!".as_ptr(), 4));
        assert!(!JS_LinearStringEqualsAscii1(ensured, c"name".as_ptr(), 3));
        let forgotten = JS_ForgetStringLinearness(ensured);
        assert_eq!(to_rust(cx, forgotten), "name");

        let mut matched = false;
        assert!(JS_StringEqualsAscii(cx, a, c"name".as_ptr(), &mut matched));
        assert!(matched);
        assert!(JS_StringEqualsAscii1(
            cx,
            a,
            c"name".as_ptr(),
            4,
            &mut matched
        ));
        assert!(matched);

        rooted!(&in(cx) let a = a);
        rooted!(&in(cx) let mut string_id = jsid::default());
        assert!(JS_StringToId(cx, a.handle(), string_id.handle_mut()));
        assert!(RUST_JSID_IS_STRING(string_id.handle()));
        assert!(!RUST_JSID_IS_INT(string_id.handle()));
        assert!(!RUST_JSID_IS_VOID(string_id.handle()));
        let back = RUST_JSID_TO_STRING(string_id.handle());
        assert_eq!(to_rust(cx, back), "name");

        rooted!(&in(cx) let mut int_id = jsid::default());
        int_to_jsid(17, int_id.handle_mut());
        assert!(RUST_JSID_IS_INT(int_id.handle()));
        assert_eq!(RUST_JSID_TO_INT(int_id.handle()), 17);

        let mut chars = [b'n' as u16, b'a' as u16, b'm' as u16, b'e' as u16, 0];
        let chars = js::jsapi::JS::TwoByteChars {
            _base: range(chars.as_mut_ptr(), 4),
        };
        rooted!(&in(cx) let mut chars_id = jsid::default());
        assert!(!js::jsapi::JS_IsExceptionPending(cx.raw_cx()));
        let ok = encode_abi::chars_to_id(cx, chars, chars_id.handle_mut().into());
        let pending = js::jsapi::JS_IsExceptionPending(cx.raw_cx());
        rooted!(&in(cx) let mut exception = UndefinedValue());
        if pending {
            js::rust::wrappers2::JS_GetPendingException(cx, exception.handle_mut());
            let _ = exception;
        }
        js::jsapi::JS_ClearPendingException(cx.raw_cx());
        assert!(ok, "JS_CharsToId failed; pending={pending}");
        assert!(RUST_JSID_IS_STRING(chars_id.handle()));

        let spec_name = JSPropertySpec_Name {
            string_: c"name".as_ptr(),
        };
        assert!(PropertySpecNameEqualsId(spec_name, string_id.handle()));
        rooted!(&in(cx) let mut permanent = jsid::default());
        assert!(PropertySpecNameToPermanentId(
            cx,
            spec_name,
            permanent.as_ptr()
        ));
        assert!(RUST_JSID_IS_STRING(permanent.handle()));
        assert_eq!(to_rust(cx, RUST_JSID_TO_STRING(permanent.handle())), "name");

        let mut ident = false;
        rooted!(&in(cx) let ident_string = JS_AtomizeString(cx, c"identifier".as_ptr()));
        assert!(JS_IsIdentifier(cx, ident_string.handle(), &mut ident));
        assert!(ident);
        let ident_chars: Vec<u16> = "identifier".encode_utf16().collect();
        assert!(JS_IsIdentifier1(ident_chars.as_ptr(), ident_chars.len()));
        let not_ident: Vec<u16> = "1a".encode_utf16().collect();
        assert!(!JS_IsIdentifier1(not_ident.as_ptr(), not_ident.len()));

        let mut index = 0;
        let index_chars: Vec<u16> = "123".encode_utf16().collect();
        rooted!(&in(cx) let index_string = JS_NewUCStringCopyN(
            cx,
            index_chars.as_ptr(),
            index_chars.len()
        ));
        let index_linear = JS_EnsureLinearString(cx, index_string.get());
        assert!(StringIsArrayIndex(index_linear, &mut index));
        assert_eq!(index, 123);
        assert!(StringIsArrayIndex1(index_chars.as_ptr(), 3, &mut index));
        assert_eq!(index, 123);

        let _ = ascii;
        set_global(cx, global, c"atom", StringValue(&*ident_string.get()));
        let atom_global = eval(cx, global, "atom === 'identifier'").to_boolean();
        assert!(atom_global);
        format!(
            "string_id={}:int_id={}:chars_id={}:permanent={}:ident={}:not_ident={}:index={}:atom_global={}",
            to_rust(cx, back),
            RUST_JSID_TO_INT(int_id.handle()),
            to_rust(cx, RUST_JSID_TO_STRING(chars_id.handle())),
            to_rust(cx, RUST_JSID_TO_STRING(permanent.handle())),
            ident,
            !JS_IsIdentifier1(not_ident.as_ptr(), not_ident.len()),
            StringIsArrayIndex(index_linear, &mut index),
            atom_global,
        )
    }

    fn mutable<T>(value: &mut T) -> MutableHandle<'_, T> {
        unsafe { MutableHandle::from_marked_location(value) }
    }

    unsafe extern "C" fn json_write(buf: *const u16, len: u32, data: *mut c_void) -> bool {
        let out = &mut *(data as *mut String);
        out.push_str(&String::from_utf16_lossy(slice::from_raw_parts(
            buf,
            len as usize,
        )));
        true
    }

    unsafe fn json(cx: &mut JSContext, global: HandleObject) -> String {
        rooted!(&in(cx) let source = JS_NewStringCopyZ(cx, c"{\"a\":[1,2]}".as_ptr()));
        rooted!(&in(cx) let mut parsed = UndefinedValue());
        assert!(JS_ParseJSON1(cx, source.handle(), parsed.handle_mut()));
        assert!(parsed.get().is_object());

        let latin1 = b"{\"b\":3}";
        let mut parsed2 = UndefinedValue();
        assert!(JS_ParseJSON2(
            cx,
            latin1.as_ptr(),
            latin1.len() as u32,
            mutable(&mut parsed2)
        ));
        assert!(parsed2.is_object());

        rooted!(&in(cx) let reviver = eval(
            cx,
            global,
            "(key, value) => typeof value === 'number' ? value * 2 : value"
        ));
        let mut revived = UndefinedValue();
        assert!(JS_ParseJSONWithReviver1(
            cx,
            source.handle(),
            reviver.handle(),
            mutable(&mut revived)
        ));
        set_global(cx, global, c"revived", revived);
        assert!(eval(cx, global, "revived.a[1] === 4").to_boolean());

        let mut value = parsed.get();
        let mut out = String::new();
        assert!(JS_StringifyWithLengthHint(
            cx,
            mutable(&mut value),
            HandleObject::null(),
            HandleValue::null(),
            Some(json_write),
            &mut out as *mut _ as *mut c_void,
            16,
        ));
        assert_eq!(out, "{\"a\":[1,2]}");

        rooted!(&in(cx) let object = JS_NewPlainObject(cx));
        let mut safe_out = String::new();
        assert!(ToJSONMaybeSafely(
            cx,
            object.handle(),
            Some(json_write),
            &mut safe_out as *mut _ as *mut c_void,
        ));
        assert_eq!(safe_out, "{}");
        format!(
            "parsed={};revived={};stringify={};safe={}",
            parsed2.is_object(),
            eval(cx, global, "revived.a[1]").to_int32(),
            out,
            safe_out
        )
    }

    unsafe fn conversions(cx: &mut JSContext) -> String {
        rooted!(&in(cx) let empty = JS_GetEmptyString(cx));
        rooted!(&in(cx) let nonempty = JS_NewStringCopyZ(cx, c"x".as_ptr()));
        rooted!(&in(cx) let empty_value = StringValue(&*empty.get()));
        rooted!(&in(cx) let nonempty_value = StringValue(&*nonempty.get()));
        assert!(!ToBooleanSlow(empty_value.handle()));
        assert!(ToBooleanSlow(nonempty_value.handle()));

        rooted!(&in(cx) let slow_value = BooleanValue(true));
        let mut number = 0.0;
        assert!(ToNumberSlow(cx, slow_value.handle(), &mut number));
        assert_eq!(number, 1.0);
        let mut i32_ = 0;
        let mut u32_ = 0;
        let mut u16_ = 0;
        let mut i64_ = 0;
        let mut u64_ = 0;
        assert!(ToInt32Slow(cx, slow_value.handle(), &mut i32_));
        assert!(ToUint32Slow(cx, slow_value.handle(), &mut u32_));
        assert!(ToUint16Slow(cx, slow_value.handle(), &mut u16_));
        assert!(ToInt64Slow(cx, slow_value.handle(), &mut i64_));
        assert!(ToUint64Slow(cx, slow_value.handle(), &mut u64_));
        assert_eq!((i32_, u32_, u16_, i64_, u64_), (1, 1, 1, 1, 1));

        rooted!(&in(cx) let mut value = Int32Value(42));
        let string = ToStringSlow(cx, value.handle());
        assert_eq!(to_rust(cx, string), "42");
        let object = ToObjectSlow(cx, value.handle(), false);
        assert!(!object.is_null());
        value.set(NullValue());
        let null_object = ToObjectSlow(cx, value.handle(), false);
        assert!(null_object.is_null());
        let pending = js::jsapi::JS_IsExceptionPending(cx.raw_cx());
        js::jsapi::JS_ClearPendingException(cx.raw_cx());
        assert!(pending);

        let mut tb = vec![0x00e9u16, 0x20ac, 0];
        let two_byte = range(tb.as_mut_ptr(), 2);
        let latin1 = encode_abi::lossy_two_byte_to_latin1(cx, &two_byte);
        assert_eq!(*latin1._base.mPtr, 0xe9);
        assert_eq!(*latin1._base.mPtr.add(1), 0xac);
        JS_free(cx, latin1._base.mPtr as *mut c_void);

        let mut utf8 = vec![b'c', b'a', b'f', 0xc3, 0xa9, 0];
        let chars = js::jsapi::JS::UTF8Chars {
            _base: range(utf8.as_mut_ptr(), 5),
        };
        let mut out_len = 0;
        let units = encode_abi::utf8_to_two_byte(cx, &chars, &mut out_len);
        assert_eq!(out_len, 4);
        assert_eq!(*units._base.mPtr, b'c' as u16);
        assert_eq!(*units._base.mPtr.add(3), 0xe9);
        JS_free(cx, units._base.mPtr as *mut c_void);

        let mut bad = vec![b'a', 0xff, 0];
        let chars = js::jsapi::JS::UTF8Chars {
            _base: range(bad.as_mut_ptr(), 2),
        };
        let mut out_len = 0;
        let lost = encode_abi::lossy_utf8_to_two_byte(cx, &chars, &mut out_len);
        assert_eq!(out_len, 2);
        JS_free(cx, lost._base.mPtr as *mut c_void);
        js::jsapi::JS_ClearPendingException(cx.raw_cx());
        format!(
            "bool_empty={}:bool_nonempty={}:numbers={},{},{},{},{},{}:string={}:object_nonnull={}:null_pending={}:utf8_len={}:bad_utf8_len={}",
            ToBooleanSlow(empty_value.handle()),
            ToBooleanSlow(nonempty_value.handle()),
            number,
            i32_,
            u32_,
            u16_,
            i64_,
            u64_,
            to_rust(cx, string),
            !object.is_null(),
            pending,
            out_len,
            out_len,
        )
    }

    unsafe fn allocator(cx: &mut JSContext) -> bool {
        let p = JS_malloc(cx, 8);
        assert!(!p.is_null());
        *(p as *mut u64) = 7;
        let p = JS_realloc(cx, p, 8, 16);
        assert!(!p.is_null());
        assert_eq!(*(p as *mut u64), 7);
        JS_free(cx, p);

        let src = [b'a' as u16, b'b' as u16, b'c' as u16];
        let mut dest = [0u16; 4];
        let dest_range = range(dest.as_mut_ptr(), 3);
        let string = JS_NewUCStringCopyN(cx, src.as_ptr(), src.len());
        assert!(JS_CopyStringChars(cx, &dest_range, string));
        assert_eq!(&dest[..3], &src);
        true
    }
}

mod edge_strings_more_contract {
    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::jsapi::mozilla::Range;
    use js::jsapi::{
        AtomToLinearString, GetLinearStringCharAt, GetLinearStringLength,
        JS_DeprecatedStringHasLatin1Chars, JS_GetStringLength, jsid,
    };
    use js::jsval::{JSVal, StringValue};
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{
        JS_AtomizeAndPinString, JS_AtomizeStringN, JS_CompareStrings, JS_ConcatStrings,
        JS_GetEmptyString, JS_GetLatin1StringCharsAndLength, JS_GetStringCharAt,
        JS_GetTwoByteStringCharsAndLength, JS_NewDependentString, JS_NewStringCopyN,
        JS_NewStringCopyUTF8N, JS_NewUCStringCopyN, JS_TypeOfValue, JS_ValueToSource,
        RUST_JSID_IS_VOID, SameValue, ToBooleanSlow, ToInt32Slow, ToInt64Slow, ToNumberSlow,
        ToStringSlow, ToUint16Slow, ToUint32Slow, ToUint64Slow,
    };

    use crate::support::{Observation, Value, eval, record_case};

    fn string_flattening_and_props(engine: &JSEngine) {
        record_case(
            engine,
            "edge.strings_rope_and_props",
            &[
                "JS_NewStringCopyN",
                "JS_NewUCStringCopyN",
                "JS_ConcatStrings",
                "JS_NewDependentString",
                "JS_CompareStrings",
                "JS_DeprecatedStringHasLatin1Chars",
                "JS_GetLatin1StringCharsAndLength",
                "JS_GetTwoByteStringCharsAndLength",
                "JS_GetStringCharAt",
                "GetLinearStringLength",
                "GetLinearStringCharAt",
                "AtomToLinearString",
                "JS_GetStringLength",
                "JS_GetEmptyString",
                "JS_NewStringCopyUTF8N",
                "JS_AtomizeAndPinString",
                "JS_AtomizeStringN",
                "SameValue",
                "JS_ValueToSource",
            ],
            |cx, _global| unsafe {
                let latin = JS_NewStringCopyN(cx, b"cafe".as_ptr().cast(), 4);
                let wide_chars = [0x20acu16, 0x2603];
                let wide = JS_NewUCStringCopyN(cx, wide_chars.as_ptr(), wide_chars.len());
                rooted!(&in(cx) let latin = latin);
                rooted!(&in(cx) let wide = wide);
                rooted!(&in(cx) let concat = JS_ConcatStrings(cx, latin.handle(), wide.handle()));
                let concat_len = JS_GetStringLength(concat.get());
                let dependent = JS_NewDependentString(cx, concat.handle(), 2, 2);
                rooted!(&in(cx) let dependent = dependent);
                let mut order = 0;
                let cmp = JS_CompareStrings(cx, latin.get(), concat.get(), &mut order);
                let cmp_order = order;

                let latin1 = JS_DeprecatedStringHasLatin1Chars(latin.get());
                let wide_latin1 = JS_DeprecatedStringHasLatin1Chars(wide.get());
                let mut latin_len = 0;
                let mut wide_len = 0;
                let latin_ok = !latin1 ||
                    !JS_GetLatin1StringCharsAndLength(cx, latin.get(), &mut latin_len).is_null();
                let wide_ok = wide_latin1 ||
                    !JS_GetTwoByteStringCharsAndLength(cx, wide.get(), &mut wide_len).is_null();
                let mut first = 0;
                let first_ok = JS_GetStringCharAt(cx, concat.get(), 0, &mut first);
                let atom = AtomToLinearString(concat.get().cast());
                let atom_len = if atom.is_null() {
                    -1
                } else {
                    GetLinearStringLength(atom) as i32
                };
                let atom_first = if atom.is_null() {
                    0
                } else {
                    GetLinearStringCharAt(atom, 1)
                };

                let mut utf8 = *b"caf\xc3\xa9";
                let utf8 = JS_NewStringCopyUTF8N(
                    cx,
                    &js::jsapi::JS::UTF8Chars {
                        _base: range(utf8.as_mut_ptr(), utf8.len()),
                    },
                );
                rooted!(&in(cx) let utf8 = utf8);
                let empty = JS_GetEmptyString(cx);
                let empty_len = JS_GetStringLength(empty);
                let atomized = JS_AtomizeAndPinString(cx, c"name".as_ptr());
                let atomized_n = JS_AtomizeStringN(cx, c"name".as_ptr(), 4);
                rooted!(&in(cx) let latin_value = StringValue(&*latin.get()));
                let source = JS_ValueToSource(cx, latin_value.handle());
                let source_text = if source.is_null() {
                    String::new()
                } else {
                    jsstr_to_string(cx, std::ptr::NonNull::new(source).unwrap())
                };

                let mut atom_same = false;
                rooted!(&in(cx) let atomized_value = js::jsval::StringValue(&*atomized));
                rooted!(&in(cx) let atomized_n_value = js::jsval::StringValue(&*atomized_n));
                assert!(SameValue(
                    cx,
                    atomized_value.handle(),
                    atomized_n_value.handle(),
                    &mut atom_same
                ));
                let dependent_text = string_of(cx, StringValue(&*dependent.get()));
                let utf8_text = string_of(cx, StringValue(&*utf8.get()));
                assert_eq!(
                    concat_len, 6,
                    "JS_ConcatStrings must concatenate code units"
                );
                assert_eq!(
                    dependent_text, "fe",
                    "JS_NewDependentString must select the requested slice"
                );
                assert!(cmp, "JS_CompareStrings must report unequal strings");
                assert!(
                    cmp_order < 0,
                    "the latin prefix must compare before the concatenation"
                );
                assert!(latin1 && !wide_latin1);
                assert!(latin_ok && latin_len == 4);
                assert!(wide_ok && wide_len == 2);
                assert!(first_ok && first == 0x63);
                assert_eq!(
                    atom_len, 6,
                    "AtomToLinearString must expose the concatenated length"
                );
                assert_eq!(
                    atom_first, b'a' as u16,
                    "GetLinearStringCharAt must return code unit 1"
                );
                assert_eq!(utf8_text, "café", "JS_NewStringCopyUTF8N must decode UTF-8");
                assert_eq!(
                    empty_len, 0,
                    "JS_GetEmptyString must return the empty string"
                );
                assert!(
                    atom_same,
                    "both atomization entry points must return the same atom"
                );
                assert_eq!(source_text, "\"cafe\"");

                Observation::new(vec![
                    ("concat_len", Value::Number(concat_len as f64)),
                    (
                        "dependent",
                        Value::String(string_of(cx, StringValue(&*dependent.get()))),
                    ),
                    ("cmp", Value::Boolean(cmp)),
                    ("cmp_negative", Value::Boolean(cmp_order < 0)),
                    ("latin1", Value::Boolean(latin1)),
                    ("wide_latin1", Value::Boolean(wide_latin1)),
                    ("latin_ok", Value::Boolean(latin_ok)),
                    ("latin_len", Value::Number(latin_len as f64)),
                    ("wide_ok", Value::Boolean(wide_ok)),
                    ("wide_len", Value::Number(wide_len as f64)),
                    ("first_ok", Value::Boolean(first_ok)),
                    ("first", Value::Number(first as f64)),
                    ("atom_len", Value::Number(atom_len as f64)),
                    ("atom_first", Value::Number(atom_first as f64)),
                    (
                        "utf8",
                        Value::String(string_of(cx, StringValue(&*utf8.get()))),
                    ),
                    ("empty_len", Value::Number(empty_len as f64)),
                    ("atom_same", Value::Boolean(atom_same)),
                    ("source", Value::String(source_text)),
                ])
            },
        );
    }

    fn value_conversions(engine: &JSEngine) {
        record_case(
            engine,
            "edge.value_conversions",
            &[
                "ToBooleanSlow",
                "ToInt32Slow",
                "ToInt64Slow",
                "ToNumberSlow",
                "ToStringSlow",
                "ToUint16Slow",
                "ToUint32Slow",
                "ToUint64Slow",
                "JS_TypeOfValue",
                "StringIsArrayIndex",
                "RUST_JSID_IS_VOID",
            ],
            |cx, global| unsafe {
                let mut numbers = Vec::new();
                for source in [
                    "' 42 '",
                    "'0x10'",
                    "''",
                    "null",
                    "true",
                    "({ valueOf() { return 7; } })",
                    "[5]",
                    "'4a'",
                    "undefined",
                ] {
                    rooted!(&in(cx) let value = eval(cx, global, source));
                    let mut out = f64::NAN;
                    let ok = ToNumberSlow(cx, value.handle(), &mut out);
                    let text = if out.is_nan() {
                        "NaN".to_owned()
                    } else {
                        format!("{out:?}")
                    };
                    numbers.push(Value::String(format!("{source}:{ok}:{text}")));
                }

                let mut ints = Vec::new();
                for source in ["4294967301", "-1.5", "-3.7", "2147483648", "65537.9"] {
                    rooted!(&in(cx) let value = eval(cx, global, source));
                    let (mut i32_out, mut u32_out, mut u16_out, mut i64_out, mut u64_out) =
                        (0i32, 0u32, 0u16, 0i64, 0u64);
                    let ok = ToInt32Slow(cx, value.handle(), &mut i32_out) &&
                        ToUint32Slow(cx, value.handle(), &mut u32_out) &&
                        ToUint16Slow(cx, value.handle(), &mut u16_out) &&
                        ToInt64Slow(cx, value.handle(), &mut i64_out) &&
                        ToUint64Slow(cx, value.handle(), &mut u64_out);
                    ints.push(Value::String(format!(
                        "{source}:{ok}:{i32_out}:{u32_out}:{u16_out}:{i64_out}:{u64_out}"
                    )));
                }

                rooted!(&in(cx) let obj = eval(cx, global, "({ toString() { return 'custom'; } })"));
                let string_out = ToStringSlow(cx, obj.handle());
                let string_text = if string_out.is_null() {
                    String::new()
                } else {
                    jsstr_to_string(cx, std::ptr::NonNull::new(string_out).unwrap())
                };
                let bool_out = ToBooleanSlow(obj.handle());

                let mut typeofs = Vec::new();
                for source in [
                    "undefined",
                    "null",
                    "({})",
                    "(function () {})",
                    "'s'",
                    "1.5",
                    "false",
                    "Symbol()",
                    "1n",
                ] {
                    rooted!(&in(cx) let value = eval(cx, global, source));
                    typeofs.push(Value::String(format!(
                        "{:?}",
                        JS_TypeOfValue(cx, value.handle())
                    )));
                }

                let mut indexes = Vec::new();
                for text in ["0", "123", "0123", "4294967294", "4294967295", "12a"] {
                    let units: Vec<u16> = text.encode_utf16().collect();
                    let mut index = 0u32;
                    let ok = js::jsapi::js::StringIsArrayIndex1(
                        units.as_ptr(),
                        units.len() as u32,
                        &mut index,
                    );
                    indexes.push(Value::String(format!("{text}:{ok}:{index}")));
                }

                rooted!(&in(cx) let void = jsid::default());
                let void_is_void = RUST_JSID_IS_VOID(void.handle());

                assert_eq!(
                    numbers,
                    [
                        "' 42 ':true:42.0",
                        "'0x10':true:16.0",
                        "'':true:0.0",
                        "null:true:0.0",
                        "true:true:1.0",
                        "({ valueOf() { return 7; } }):true:7.0",
                        "[5]:true:5.0",
                        "'4a':true:NaN",
                        "undefined:true:NaN",
                    ]
                    .into_iter()
                    .map(|text| Value::String(text.to_owned()))
                    .collect::<Vec<_>>()
                );
                assert_eq!(
                    ints,
                    [
                        "4294967301:true:5:5:5:4294967301:4294967301",
                        "-1.5:true:-1:4294967295:65535:-1:18446744073709551615",
                        "-3.7:true:-3:4294967293:65533:-3:18446744073709551613",
                        "2147483648:true:-2147483648:2147483648:0:2147483648:2147483648",
                        "65537.9:true:65537:65537:1:65537:65537",
                    ]
                    .into_iter()
                    .map(|text| Value::String(text.to_owned()))
                    .collect::<Vec<_>>()
                );
                assert_eq!(string_text, "custom");
                assert!(bool_out);
                assert_eq!(
                    typeofs,
                    [
                        "JSTYPE_UNDEFINED",
                        "JSTYPE_OBJECT",
                        "JSTYPE_OBJECT",
                        "JSTYPE_FUNCTION",
                        "JSTYPE_STRING",
                        "JSTYPE_NUMBER",
                        "JSTYPE_BOOLEAN",
                        "JSTYPE_SYMBOL",
                        "JSTYPE_BIGINT",
                    ]
                    .into_iter()
                    .map(|text| Value::String(text.to_owned()))
                    .collect::<Vec<_>>()
                );
                assert_eq!(
                    indexes,
                    [
                        "0:true:0",
                        "123:true:123",
                        "0123:false:0",
                        "4294967294:true:4294967294",
                        "4294967295:false:0",
                        "12a:false:0",
                    ]
                    .into_iter()
                    .map(|text| Value::String(text.to_owned()))
                    .collect::<Vec<_>>()
                );
                assert!(void_is_void);

                Observation::new(vec![
                    ("numbers", Value::Array(numbers)),
                    ("ints", Value::Array(ints)),
                    ("string", Value::String(string_text)),
                    ("bool", Value::Boolean(bool_out)),
                    ("typeofs", Value::Array(typeofs)),
                    ("indexes", Value::Array(indexes)),
                    ("void_is_void", Value::Boolean(void_is_void)),
                ])
            },
        );
    }
    fn range<T>(ptr: *mut T, len: usize) -> Range<T> {
        unsafe {
            let mut range: Range<T> = std::mem::zeroed();
            range.mStart.mPtr = ptr;
            range.mEnd.mPtr = ptr.add(len);
            range
        }
    }

    fn string_of(cx: &mut JSContext, value: JSVal) -> String {
        assert!(value.is_string());
        unsafe {
            jsstr_to_string(
                cx,
                std::ptr::NonNull::new(value.to_string()).expect("non-null string"),
            )
        }
    }

    pub fn run(engine: &JSEngine) {
        string_flattening_and_props(engine);
        value_conversions(engine);
    }
}

mod edge_strings_deep_contract {
    fn string_ptr_of(cx: &mut JSContext, string: *mut JSString) -> String {
        unsafe { jsstr_to_string(cx, NonNull::new(string).expect("non-null JS string")) }
    }

    fn string_of(cx: &mut JSContext, value: JSVal) -> String {
        assert!(value.is_string(), "expected a string value");
        string_ptr_of(cx, value.to_string())
    }

    unsafe fn id_for(cx: &mut JSContext, global: HandleObject, source: &str) -> jsid {
        rooted!(&in(cx) let value = eval(cx, global, source));
        rooted!(&in(cx) let mut id = jsid::default());
        assert!(js::rust::wrappers2::JS_ValueToId(
            cx,
            value.handle(),
            id.handle_mut()
        ));
        id.get()
    }

    fn strings_symbols_ids_and_encoding(engine: &JSEngine) {
        record_case(
            engine,
            "edge.strings.symbols_ids_and_encoding",
            &[
                "GetWellKnownSymbol",
                "JS_DefinePropertyById2",
                "JS_EncodeStringToLatin1",
                "JS_ValueToId",
                "RUST_SYMBOL_TO_JSID",
                "int_to_jsid",
            ],
            |cx, global| unsafe {
                #[cfg(target_env = "msvc")]
                {
                    let _ = (cx, global);
                    return Observation::new(vec![(
                        "unsupported_platform",
                        Value::String("msvc_cxx_abi".to_owned()),
                    )]);
                }

                let iterator = GetWellKnownSymbol(cx, jsapi::SymbolCode::iterator);
                assert!(!iterator.is_null());
                let async_iterator = GetWellKnownSymbol(cx, jsapi::SymbolCode::asyncIterator);
                assert!(!async_iterator.is_null());
                rooted!(&in(cx) let mut iterator_id = jsid::default());
                rooted!(&in(cx) let mut async_id = jsid::default());
                RUST_SYMBOL_TO_JSID(iterator, iterator_id.handle_mut());
                RUST_SYMBOL_TO_JSID(async_iterator, async_id.handle_mut());

                let script_iterator = id_for(cx, global, "Symbol.iterator");
                let script_async = id_for(cx, global, "Symbol.asyncIterator");
                let iterator_matches_script = iterator_id.get().asBits_ == script_iterator.asBits_;
                let async_matches_script = async_id.get().asBits_ == script_async.asBits_;
                let symbols_distinct = iterator_id.get().asBits_ != async_id.get().asBits_;

                rooted!(&in(cx) let object = js::rust::wrappers2::JS_NewPlainObject(cx));
                rooted!(&in(cx) let value = Int32Value(9));
                assert!(JS_DefinePropertyById2(
                    cx,
                    object.handle(),
                    iterator_id.handle(),
                    value.handle(),
                    1
                ));
                assert!(JS_DefinePropertyById2(
                    cx,
                    object.handle(),
                    async_id.handle(),
                    value.handle(),
                    2
                ));
                rooted!(&in(cx) let mut int_id = jsid::default());
                int_to_jsid(17, int_id.handle_mut());
                let int_value = int_id.get().to_int();
                assert!(JS_DefinePropertyById2(
                    cx,
                    object.handle(),
                    int_id.handle(),
                    value.handle(),
                    1
                ));
                set_global(cx, global, c"deepKeyed", ObjectValue(object.get()));

                let iterator_property = eval(cx, global, "deepKeyed[Symbol.iterator]").to_int32();
                let async_property = eval(cx, global, "deepKeyed[Symbol.asyncIterator]").to_int32();
                let int_property = eval(cx, global, "deepKeyed[17]").to_int32();
                let iterator_name_value = eval(cx, global, "Symbol.iterator.toString()");
                let iterator_name = string_of(cx, iterator_name_value);

                let latin_source =
                    js::rust::wrappers2::JS_NewStringCopyN(cx, b"caf\xc3\xa9".as_ptr().cast(), 5);
                assert!(!latin_source.is_null());
                let mut encoded = ptr::null_mut::<c_char>();
                js_encode_latin1(&mut encoded, cx.raw_cx().cast(), latin_source);
                let latin_bytes = if encoded.is_null() {
                    "null".to_owned()
                } else {
                    let bytes = CStr::from_ptr(encoded).to_bytes().to_vec();
                    js::rust::wrappers2::JS_free(cx, encoded.cast());
                    bytes
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(",")
                };
                assert_eq!(
                    latin_bytes, "63,61,66,c3,a9",
                    "JS_EncodeStringToLatin1 must expose the original UTF-8 bytes for this string"
                );
                assert_eq!(int_value, 17, "int_to_jsid must preserve the integer value");

                Observation::new(vec![
                    (
                        "iterator_is_symbol",
                        Value::Boolean(iterator_id.get().is_symbol()),
                    ),
                    (
                        "async_is_symbol",
                        Value::Boolean(async_id.get().is_symbol()),
                    ),
                    (
                        "iterator_matches_script",
                        Value::Boolean(iterator_matches_script),
                    ),
                    ("async_matches_script", Value::Boolean(async_matches_script)),
                    ("symbols_distinct", Value::Boolean(symbols_distinct)),
                    ("iterator_property", Value::Number(iterator_property as f64)),
                    ("async_property", Value::Number(async_property as f64)),
                    ("int_property", Value::Number(int_property as f64)),
                    ("int_id_value", Value::Number(int_value as f64)),
                    ("iterator_name", Value::String(iterator_name)),
                    ("latin1_bytes", Value::String(latin_bytes)),
                ])
            },
        );
    }

    #[cfg(not(feature = "js-quickjs"))]
    unsafe fn js_encode_latin1(out: *mut *mut c_char, cx: *mut c_void, string: *mut JSString) {
        #[cfg(not(target_env = "msvc"))]
        {
            unsafe extern "C" {
                #[link_name = "_Z23JS_EncodeStringToLatin1P9JSContextP8JSString"]
                fn encode(out: *mut *mut c_char, cx: *mut c_void, string: *mut JSString);
            }
            encode(out, cx, string);
        }
        #[cfg(target_env = "msvc")]
        {
            let _ = (out, cx, string);
        }
    }

    #[cfg(feature = "js-quickjs")]
    unsafe fn js_encode_latin1(out: *mut *mut c_char, cx: *mut c_void, string: *mut JSString) {
        let cx = JSContext::from_ptr(NonNull::new(cx.cast()).expect("non-null JSContext"));
        *out = js::rust::wrappers2::JS_EncodeStringToLatin1(&cx, string) as *mut c_char;
    }

    // ---------------------------------------------------------------------------
    // Extra typed arrays and external buffer storage.
    // ---------------------------------------------------------------------------

    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::{self, NonNull};

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::jsapi::{self, JSString, jsid};
    use js::jsval::{Int32Value, JSVal, ObjectValue};
    use js::rooted;
    use js::rust::wrappers2::{
        GetWellKnownSymbol, JS_DefinePropertyById2, RUST_SYMBOL_TO_JSID, int_to_jsid,
    };
    use js::rust::{HandleObject, JSEngine};

    use crate::support::{Observation, Value, eval, record_case, set_global};

    pub fn run(engine: &JSEngine) {
        strings_symbols_ids_and_encoding(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_strings_deep_contract::run(engine);
    edge_strings_more_contract::run(engine);
    extra_contract::run(engine);
    // Each scenario gets its own runtime and global, so a panic or pending exception in one of
    // them cannot truncate or contaminate the ones after it.
    let mut steps = crate::support::Steps::new("strings.core", STRING_APIS);
    steps.step("latin1_and_two_byte", || {
        crate::support::with_global(engine, |cx, global| unsafe {
            latin1_and_two_byte(cx, global)
        })
    });
    steps.step("lone_surrogates", || {
        crate::support::with_global(engine, |cx, global| unsafe { lone_surrogates(cx, global) })
    });
    steps.step("number_conversions", || {
        crate::support::with_global(engine, |cx, global| unsafe {
            number_conversions(cx, global)
        })
    });
    steps.step("string_conversions", || {
        crate::support::with_global(engine, |cx, global| unsafe {
            string_conversions(cx, global)
        })
    });
    steps.step("primitives_and_equality", || {
        crate::support::with_global(engine, |cx, global| unsafe {
            primitives_and_equality(cx, global)
        })
    });
    steps.step("value_to_source", || {
        crate::support::with_global(engine, |cx, global| unsafe { value_to_source(cx, global) })
    });
    for (name, step) in [
        (
            "js_index_to_id",
            js_index_to_id as unsafe fn(&mut JSContext, HandleObject),
        ),
        ("js_value_to_id", js_value_to_id),
        ("js_atomize_and_pin_string", js_atomize_and_pin_string),
        ("get_well_known_symbol", get_well_known_symbol),
        ("rust_symbol_to_jsid", rust_symbol_to_jsid),
        ("js_define_property_by_id2", js_define_property_by_id2),
        ("js_id_to_value", js_id_to_value),
        ("js_is_identifier", js_is_identifier),
    ] {
        steps.step(name, || {
            crate::support::with_global(engine, |cx, global| unsafe { step(cx, global) })
        });
    }
    steps.step("js_parse_json", || {
        crate::support::with_global(engine, |cx, global| unsafe { js_parse_json(cx, global) })
    });
    steps.step("to_json", || {
        crate::support::with_global(engine, |cx, global| unsafe { to_json_step(cx, global) })
    });
    steps.step("js_stringify", || {
        crate::support::with_global(engine, |cx, global| unsafe { js_stringify(cx, global) })
    });
    steps.finish();
}

const STRING_APIS: &[&str] = &[
    "JS_NewStringCopyN",
    "JS_DeprecatedStringHasLatin1Chars",
    "JS_GetLatin1StringCharsAndLength",
    "JS_GetStringCharAt",
    "JS_GetTwoByteStringCharsAndLength",
    "JS_NewUCStringCopyN",
    "JS_ConcatStrings",
    "JS_NewDependentString",
    "JS_CompareStrings",
    "EncodeStringToUTF8",
    "ToInt8Slow",
    "ToUint8Slow",
    "ToInt16Slow",
    "ToPrimitive",
    "OrdinaryToPrimitive",
    "JS_TypeOfValue",
    "SameValue",
    "StrictlyEqual",
    "JS_ValueToSource",
    "JS_IndexToId",
    "JS_IdToValue",
    "JS_ValueToId",
    "JS_AtomizeAndPinString",
    "RUST_INTERNED_STRING_TO_JSID",
    "RUST_SYMBOL_TO_JSID",
    "GetWellKnownSymbol",
    "JS_DefinePropertyById2",
    "JS_ParseJSON",
    "JS_ParseJSONWithReviver",
    "JS_Stringify",
    "ToJSON",
];

unsafe fn eval(cx: &mut JSContext, global: HandleObject, source: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_strings.js".to_owned(), 1);
    evaluate_script(cx, global, source, rval.handle_mut(), options)
        .unwrap_or_else(|_| panic!("evaluating {source:?} failed"));
    rval.get()
}

unsafe fn to_rust(cx: &mut JSContext, s: *mut JSString) -> String {
    jsstr_to_string(cx, NonNull::new(s).unwrap())
}

unsafe fn units(cx: &mut JSContext, s: *mut JSString) -> Vec<u16> {
    let mut len = 0;
    if JS_DeprecatedStringHasLatin1Chars(s) {
        let p = JS_GetLatin1StringCharsAndLength(cx, s, &mut len);
        slice::from_raw_parts(p, len)
            .iter()
            .map(|&b| b as u16)
            .collect()
    } else {
        let p = JS_GetTwoByteStringCharsAndLength(cx, s, &mut len);
        slice::from_raw_parts(p, len).to_vec()
    }
}

/// Sets `globalThis[name] = v`.
unsafe fn set_global(cx: &mut JSContext, global: HandleObject, name: &CStr, v: JSVal) {
    rooted!(&in(cx) let v = v);
    assert!(JS_SetProperty(cx, global, name.as_ptr(), v.handle()));
}

fn take_exception(cx: &mut JSContext) -> bool {
    unsafe {
        let pending = JS_IsExceptionPending(cx.raw_cx());
        JS_ClearPendingException(cx.raw_cx());
        pending
    }
}

unsafe fn latin1_and_two_byte(cx: &mut JSContext, global: HandleObject) {
    let latin1 = JS_NewStringCopyN(cx, b"caf\xe9".as_ptr() as *const c_char, 4);
    rooted!(&in(cx) let latin1 = latin1);
    assert!(JS_DeprecatedStringHasLatin1Chars(latin1.get()));
    let mut len = 0;
    let chars = JS_GetLatin1StringCharsAndLength(cx, latin1.get(), &mut len);
    assert_eq!(slice::from_raw_parts(chars, len), b"caf\xe9");
    assert_eq!(to_rust(cx, latin1.get()), "café");
    observe_field("latin1_len", Observed::Number(len as f64));
    observe_field("latin1_text", Observed::String(to_rust(cx, latin1.get())));
    let mut c = 0;
    assert!(JS_GetStringCharAt(cx, latin1.get(), 3, &mut c));
    assert_eq!(c, 0xe9);
    observe_field("latin1_char_at_3", Observed::Number(c as f64));

    // Rust strings are created from UTF-8; non-Latin-1 text becomes two-byte.
    rooted!(&in(cx) let mut v = UndefinedValue());
    "1 € ☃".to_jsval(cx, v.handle_mut());
    let wide = v.get().to_string();
    assert!(!JS_DeprecatedStringHasLatin1Chars(wide));
    assert_eq!(units(cx, wide), "1 € ☃".encode_utf16().collect::<Vec<_>>());
    assert_eq!(to_rust(cx, wide), "1 € ☃");
    observe_field("two_byte_text", Observed::String(to_rust(cx, wide)));
    observe_field(
        "two_byte_units",
        Observed::String(format!("{:?}", units(cx, wide))),
    );

    "café".to_jsval(cx, v.handle_mut());
    assert!(JS_DeprecatedStringHasLatin1Chars(v.get().to_string()));
    assert_eq!(to_rust(cx, v.get().to_string()), "café");

    // UTF-16 input that fits Latin-1 is stored as Latin-1.
    let narrow: Vec<u16> = "naïve".encode_utf16().collect();
    let s = JS_NewUCStringCopyN(cx, narrow.as_ptr(), narrow.len());
    assert!(JS_DeprecatedStringHasLatin1Chars(s));
    assert_eq!(to_rust(cx, s), "naïve");
    observe_field("uc_latin1_text", Observed::String(to_rust(cx, s)));

    // Strings made by script are readable in place, both kinds.
    let s = eval(cx, global, "'abc'.repeat(3) + 'd'").to_string();
    assert_eq!(to_rust(cx, s), "abcabcabcd");
    let s = eval(cx, global, "'x'.repeat(40) + '\\u20ac'").to_string();
    assert!(!JS_DeprecatedStringHasLatin1Chars(s));
    assert_eq!(to_rust(cx, s), format!("{}€", "x".repeat(40)));

    rooted!(&in(cx) let left = latin1.get());
    rooted!(&in(cx) let right = wide);
    let cat = JS_ConcatStrings(cx, left.handle(), right.handle());
    assert_eq!(to_rust(cx, cat), "café1 € ☃");
    observe_field("concat_text", Observed::String(to_rust(cx, cat)));
    rooted!(&in(cx) let cat = cat);
    let sub = JS_NewDependentString(cx, cat.handle(), 3, 3);
    assert_eq!(to_rust(cx, sub), "é1 ");
    observe_field("substring_text", Observed::String(to_rust(cx, sub)));
    let mut order = 0;
    assert!(JS_CompareStrings(cx, latin1.get(), cat.get(), &mut order));
    assert!(order < 0);
    assert!(JS_CompareStrings(cx, cat.get(), latin1.get(), &mut order));
    assert!(order > 0);
    assert!(JS_CompareStrings(
        cx,
        latin1.get(),
        latin1.get(),
        &mut order
    ));
    assert_eq!(order, 0);
    observe_field("compare_equal_order", Observed::Number(order as f64));

    thread_local!(static ENCODED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) });
    unsafe extern "C" fn record(s: *const c_char) {
        ENCODED.with(|e| *e.borrow_mut() = CStr::from_ptr(s).to_bytes().to_vec());
    }
    EncodeStringToUTF8(cx, latin1.handle(), record);
    assert_eq!(ENCODED.with(|e| e.borrow().clone()), "café".as_bytes());
    observe_field(
        "utf8_encoded",
        Observed::String(String::from_utf8_lossy(&ENCODED.with(|e| e.borrow().clone())).into()),
    );
}

unsafe fn lone_surrogates(cx: &mut JSContext, global: HandleObject) {
    let chars = [0x61, 0xd800, 0x62, 0xdc00];
    let s = JS_NewUCStringCopyN(cx, chars.as_ptr(), chars.len());
    assert!(!JS_DeprecatedStringHasLatin1Chars(s));
    assert_eq!(units(cx, s), chars);
    assert_eq!(to_rust(cx, s), "a\u{fffd}b\u{fffd}");
    observe_field("lone_surrogate_rendered", Observed::String(to_rust(cx, s)));

    // Through script and back.
    set_global(cx, global, c"lone", StringValue(&*s));
    let v = eval(
        cx,
        global,
        "lone.length === 4 && lone.charCodeAt(1) === 0xd800 && lone.charCodeAt(3) === 0xdc00",
    );
    assert!(v.to_boolean());
    let s = eval(cx, global, "lone.slice(1, 2) + '\\udbff\\udfff'").to_string();
    assert_eq!(units(cx, s), [0xd800, 0xdbff, 0xdfff]);
    assert_eq!(to_rust(cx, s), "\u{fffd}\u{10ffff}");

    rooted!(&in(cx) let s = s);
    thread_local!(static ENCODED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) });
    unsafe extern "C" fn record(s: *const c_char) {
        ENCODED.with(|e| *e.borrow_mut() = CStr::from_ptr(s).to_bytes().to_vec());
    }
    EncodeStringToUTF8(cx, s.handle(), record);
    assert_eq!(
        ENCODED.with(|e| e.borrow().clone()),
        "\u{fffd}\u{10ffff}".as_bytes()
    );
}

unsafe fn number_conversions(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut v = UndefinedValue());
    let mut number = |cx: &mut JSContext, source: &str| {
        v.set(eval(cx, global, source));
        ToNumber(cx, v.handle())
    };
    assert_eq!(number(cx, "' 42 '"), Ok(42.0));
    assert_eq!(number(cx, "'0x10'"), Ok(16.0));
    assert_eq!(number(cx, "''"), Ok(0.0));
    assert!(number(cx, "'4a'").unwrap().is_nan());
    assert!(number(cx, "undefined").unwrap().is_nan());
    assert_eq!(number(cx, "null"), Ok(0.0));
    assert_eq!(number(cx, "true"), Ok(1.0));
    assert_eq!(number(cx, "({ valueOf() { return 7; } })"), Ok(7.0));
    assert_eq!(number(cx, "[5]"), Ok(5.0));
    observe_field("number_trimmed", Observed::Number(42.0));
    observe_field("number_hex", Observed::Number(16.0));
    observe_field("number_value_of", Observed::Number(7.0));
    observe_field("number_single_element_array", Observed::Number(5.0));
    assert_eq!(number(cx, "Symbol()"), Err(()));
    crate::support::assert_exception_named(cx, "TypeError");
    assert_eq!(number(cx, "1n"), Err(()));
    crate::support::assert_exception_named(cx, "TypeError");
    assert_eq!(number(cx, "({ valueOf() { throw 1; } })"), Err(()));
    assert!(take_exception(cx));

    rooted!(&in(cx) let mut v = UndefinedValue());
    let cases: [(f64, i32, u32, u16, i64, u64); 10] = [
        (4294967301.0, 5, 5, 5, 4294967301, 4294967301),
        (-1.0, -1, u32::MAX, u16::MAX, -1, u64::MAX),
        (-3.7, -3, u32::MAX - 2, u16::MAX - 2, -3, u64::MAX - 2),
        (2147483648.0, i32::MIN, 1 << 31, 0, 1 << 31, 1 << 31),
        (65537.9, 65537, 65537, 1, 65537, 65537),
        (-0.0, 0, 0, 0, 0, 0),
        (f64::NAN, 0, 0, 0, 0, 0),
        (f64::INFINITY, 0, 0, 0, 0, 0),
        (18446744073709555712.0, 4096, 4096, 4096, 4096, 4096),
        (
            -9007199254740993.0,
            0,
            0,
            0,
            -9007199254740992,
            18437736874454810624,
        ),
    ];
    for (d, i32_, u32_, u16_, i64_, u64_) in cases {
        v.set(DoubleValue(d));
        assert_eq!(ToInt32(cx, v.handle()), Ok(i32_), "ToInt32({d})");
        assert_eq!(ToUint32(cx, v.handle()), Ok(u32_), "ToUint32({d})");
        assert_eq!(ToUint16(cx, v.handle()), Ok(u16_), "ToUint16({d})");
        assert_eq!(ToInt64(cx, v.handle()), Ok(i64_), "ToInt64({d})");
        assert_eq!(ToUint64(cx, v.handle()), Ok(u64_), "ToUint64({d})");
        if d == -3.7 {
            observe_field("number_to_int32_neg", Observed::Number(i32_ as f64));
            observe_field("number_to_uint32_neg", Observed::Number(u32_ as f64));
            observe_field("number_to_uint16_neg", Observed::Number(u16_ as f64));
            observe_field("number_to_int64_neg", Observed::Number(i64_ as f64));
            observe_field("number_to_uint64_neg", Observed::String(u64_.to_string()));
        }
    }

    v.set(eval(cx, global, "'-129'"));
    let (mut i8_, mut u8_, mut i16_) = (0i8, 0u8, 0i16);
    assert!(ToInt8Slow(cx, v.handle(), &mut i8_));
    assert!(ToUint8Slow(cx, v.handle(), &mut u8_));
    assert!(ToInt16Slow(cx, v.handle(), &mut i16_));
    assert_eq!((i8_, u8_, i16_), (127, 127, -129));
    observe_field("number_to_int8", Observed::Number(i8_ as f64));
    observe_field("number_to_uint8", Observed::Number(u8_ as f64));
    observe_field("number_to_int16", Observed::Number(i16_ as f64));
    v.set(eval(cx, global, "'0x1ff'"));
    assert!(ToUint8Slow(cx, v.handle(), &mut u8_));
    assert_eq!(u8_, 0xff);
    observe_field("number_hex_to_uint8", Observed::Number(u8_ as f64));

    for (source, expected) in [
        ("''", false),
        ("'0'", true),
        ("0n", false),
        ("5n", true),
        ("({})", true),
        ("NaN", false),
    ] {
        v.set(eval(cx, global, source));
        assert_eq!(ToBoolean(v.handle()), expected, "ToBoolean({source})");
    }
}

unsafe fn string_conversions(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut v = UndefinedValue());
    let cases = [
        (DoubleValue(1.5), "1.5"),
        (DoubleValue(-0.0), "0"),
        (DoubleValue(1e21), "1e+21"),
        (DoubleValue(0.1 + 0.2), "0.30000000000000004"),
        (DoubleValue(f64::NEG_INFINITY), "-Infinity"),
        (Int32Value(-42), "-42"),
        (UndefinedValue(), "undefined"),
    ];
    for (value, expected) in cases {
        v.set(value);
        let s = ToString(cx, v.handle());
        assert_eq!(to_rust(cx, s), expected);
    }
    observe_field("string_from_number_1_5", Observed::String("1.5".into()));
    observe_field(
        "string_from_number_0_1_plus_0_2",
        Observed::String("0.30000000000000004".into()),
    );
    for (source, expected) in [
        ("({})", "[object Object]"),
        ("[1, [2, 3], null]", "1,2,3,"),
        ("({ toString() { return 'custom'; } })", "custom"),
        (
            "({ [Symbol.toPrimitive](hint) { return hint; } })",
            "string",
        ),
        ("12345678901234567890n", "12345678901234567890"),
        ("'a' + 'b'.repeat(3)", "abbb"),
    ] {
        v.set(eval(cx, global, source));
        let s = ToString(cx, v.handle());
        assert_eq!(to_rust(cx, s), expected, "ToString({source})");
        match source {
            "({})" => observe_field("string_object", Observed::String(expected.into())),
            "[1, [2, 3], null]" => observe_field("string_array", Observed::String(expected.into())),
            "({ toString() { return 'custom'; } })" => {
                observe_field("string_custom_to_string", Observed::String(expected.into()))
            },
            "({ [Symbol.toPrimitive](hint) { return hint; } })" => observe_field(
                "string_to_primitive_hint",
                Observed::String(expected.into()),
            ),
            "12345678901234567890n" => {
                observe_field("string_bigint", Observed::String(expected.into()))
            },
            _ => {},
        }
    }
    v.set(eval(cx, global, "Symbol('s')"));
    assert!(ToString(cx, v.handle()).is_null());
    crate::support::assert_exception_named(cx, "TypeError");
}

unsafe fn primitives_and_equality(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let obj = eval(cx, global, "({ [Symbol.toPrimitive](hint) { return 'hint:' + hint; } })").to_object());
    rooted!(&in(cx) let mut v = UndefinedValue());
    for (hint, expected) in [
        (JSType::JSTYPE_STRING, "hint:string"),
        (JSType::JSTYPE_NUMBER, "hint:number"),
        (JSType::JSTYPE_UNDEFINED, "hint:default"),
    ] {
        assert!(ToPrimitive(cx, obj.handle(), hint, v.handle_mut()));
        assert_eq!(to_rust(cx, v.get().to_string()), expected);
        if hint == JSType::JSTYPE_STRING {
            observe_field("primitive_string_hint", Observed::String(expected.into()));
        }
    }

    rooted!(&in(cx) let obj = eval(cx, global, "({ valueOf() { return 1; }, toString() { return 's'; } })").to_object());
    assert!(ToPrimitive(
        cx,
        obj.handle(),
        JSType::JSTYPE_UNDEFINED,
        v.handle_mut()
    ));
    assert_eq!(v.get().to_int32(), 1);
    observe_field("primitive_default_value", Observed::Number(1.0));
    assert!(OrdinaryToPrimitive(
        cx,
        obj.handle(),
        JSType::JSTYPE_STRING,
        v.handle_mut()
    ));
    assert_eq!(to_rust(cx, v.get().to_string()), "s");
    observe_field("primitive_string_value", Observed::String("s".into()));

    rooted!(&in(cx) let obj = eval(cx, global, "({ valueOf() { return {}; }, toString() { return {}; } })").to_object());
    assert!(!ToPrimitive(
        cx,
        obj.handle(),
        JSType::JSTYPE_NUMBER,
        v.handle_mut()
    ));
    crate::support::assert_exception_named(cx, "TypeError");

    for (source, expected) in [
        ("undefined", JSType::JSTYPE_UNDEFINED),
        ("null", JSType::JSTYPE_OBJECT),
        ("({})", JSType::JSTYPE_OBJECT),
        ("(function () {})", JSType::JSTYPE_FUNCTION),
        ("(class A {})", JSType::JSTYPE_FUNCTION),
        ("'s'", JSType::JSTYPE_STRING),
        ("1.5", JSType::JSTYPE_NUMBER),
        ("false", JSType::JSTYPE_BOOLEAN),
        ("Symbol()", JSType::JSTYPE_SYMBOL),
        ("1n", JSType::JSTYPE_BIGINT),
    ] {
        v.set(eval(cx, global, source));
        assert_eq!(JS_TypeOfValue(cx, v.handle()), expected, "typeof {source}");
        if source == "null" || source == "1n" {
            observe_field(
                if source == "null" {
                    "typeof_null"
                } else {
                    "typeof_bigint"
                },
                Observed::String(
                    match expected {
                        JSType::JSTYPE_OBJECT => "object",
                        JSType::JSTYPE_BIGINT => "bigint",
                        _ => unreachable!(),
                    }
                    .into(),
                ),
            );
        }
    }

    rooted!(&in(cx) let mut a = UndefinedValue());
    rooted!(&in(cx) let mut b = UndefinedValue());
    let mut compare = |cx: &mut JSContext, x: JSVal, y: JSVal| {
        a.set(x);
        b.set(y);
        let (mut same, mut strict) = (false, false);
        assert!(SameValue(cx, a.handle(), b.handle(), &mut same));
        assert!(StrictlyEqual(cx, a.handle(), b.handle(), &mut strict));
        (same, strict)
    };
    assert_eq!(
        compare(cx, DoubleValue(f64::NAN), DoubleValue(f64::NAN)),
        (true, false)
    );
    observe_field("same_value_nan", Observed::Boolean(true));
    observe_field("strictly_equal_nan", Observed::Boolean(false));
    assert_eq!(
        compare(cx, DoubleValue(0.0), DoubleValue(-0.0)),
        (false, true)
    );
    observe_field("same_value_zero", Observed::Boolean(false));
    observe_field("strictly_equal_zero", Observed::Boolean(true));
    assert_eq!(compare(cx, Int32Value(3), DoubleValue(3.0)), (true, true));
    let s1 = eval(cx, global, "'ab' + 'c'");
    let s2 = eval(cx, global, "'a' + 'bc'");
    assert_eq!(compare(cx, s1, s2), (true, true));
    let o1 = eval(cx, global, "({})");
    let o2 = eval(cx, global, "({})");
    assert_eq!(compare(cx, o1, o2), (false, false));
}

unsafe fn source_of(cx: &mut JSContext, v: HandleValue) -> String {
    let s = JS_ValueToSource(cx, v);
    to_rust(cx, s)
}

unsafe fn value_to_source(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut v = UndefinedValue());
    for (source, expected) in [
        ("undefined", "(void 0)"),
        ("null", "null"),
        ("true", "true"),
        ("-0", "-0"),
        ("1.25", "1.25"),
        ("10n", "10n"),
        (r#"'a"b\né€'"#, r#""a\"b\n\xE9\u20AC""#),
        ("Symbol.iterator", "Symbol.iterator"),
        ("Symbol('d')", r#"Symbol("d")"#),
        ("Symbol()", "Symbol()"),
        ("Symbol.for('k')", r#"Symbol.for("k")"#),
        ("[1, 'x', [null]]", r#"[1, "x", [null]]"#),
        (
            r#"({a: 1, 'b c': [2], 3: undefined, [Symbol.iterator]: 0})"#,
            r#"({3:(void 0), a:1, 'b c':[2], [Symbol.iterator]:0})"#,
        ),
        ("({})", "({})"),
        (
            "(() => { const o = {}; o.self = o; return o; })()",
            "({self:{}})",
        ),
        ("new Number(5)", "new Number(5)"),
        ("new String('s')", r#"new String("s")"#),
        ("new Boolean(false)", "new Boolean(false)"),
        ("new Date(0)", "new Date(0)"),
        ("/a+/g", "/a+/g"),
        (
            "(function f(a) { return a; })",
            "(function f(a) { return a; })",
        ),
        (
            "({ toSource() { return 'mine'; } })",
            "({toSource() { return 'mine'; }})",
        ),
        (
            "new TypeError('boom')",
            r#"(new TypeError("boom", "qjs_strings.js", 1))"#,
        ),
    ] {
        v.set(eval(cx, global, source));
        assert_eq!(
            source_of(cx, v.handle()),
            expected,
            "ValueToSource({source})"
        );
        if source == r#"'a"b\né€'"# {
            observe_field("source_escaped_string", Observed::String(expected.into()));
        } else if source == "[1, 'x', [null]]" {
            observe_field("source_array", Observed::String(expected.into()));
        } else if source == "Symbol.iterator" {
            observe_field(
                "source_well_known_symbol",
                Observed::String(expected.into()),
            );
        } else if source == "new Date(0)" {
            observe_field("source_date", Observed::String(expected.into()));
        }
    }
}

unsafe fn js_index_to_id(cx: &mut JSContext, _global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());

    assert!(JS_IndexToId(cx, 5, id.handle_mut()));
    assert!(id.get().is_int());
    assert_eq!(id.get().to_int(), 5);
    observe_field(
        "index_to_id_int",
        Observed::Number(id.get().to_int() as f64),
    );
    assert!(JS_IdToValue(cx, id.get(), v.handle_mut()));
    assert_eq!(v.get().to_int32(), 5);
    observe_field(
        "id_to_value_int",
        Observed::Number(v.get().to_int32() as f64),
    );

    assert!(JS_IndexToId(cx, u32::MAX - 1, id.handle_mut()));
    assert!(id.get().is_string());
    assert_eq!(to_rust(cx, id.get().to_string()), "4294967294");
    observe_field(
        "index_to_id_string",
        Observed::String(to_rust(cx, id.get().to_string())),
    );
    assert!(JS_IdToValue(cx, id.get(), v.handle_mut()));
    assert_eq!(to_rust(cx, v.get().to_string()), "4294967294");
    observe_field(
        "id_to_value_string",
        Observed::String(to_rust(cx, v.get().to_string())),
    );
}

unsafe fn js_value_to_id(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());
    for (source, int) in [
        ("'17'", Some(17)),
        ("17", Some(17)),
        ("17.0", Some(17)),
        ("'017'", None),
        ("'-1'", None),
        ("-1", None),
        ("2147483648", None),
    ] {
        v.set(eval(cx, global, source));
        assert!(JS_ValueToId(cx, v.handle(), id.handle_mut()));
        match int {
            Some(i) => assert_eq!(
                (id.get().is_int(), id.get().to_int()),
                (true, i),
                "id of {source}"
            ),
            None => assert!(id.get().is_string(), "id of {source}"),
        }
        if source == "17" {
            observe_field(
                "value_to_id_number",
                Observed::Number(id.get().to_int() as f64),
            );
        } else if source == "'017'" {
            observe_field(
                "value_to_id_leading_zero",
                Observed::String(to_rust(cx, id.get().to_string())),
            );
        }
    }
}

unsafe fn js_atomize_and_pin_string(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());
    v.set(eval(cx, global, "'na' + 'me'"));
    assert!(JS_ValueToId(cx, v.handle(), id.handle_mut()));
    let name_id = id.get();
    assert!(name_id.is_string());
    let pinned = JS_AtomizeAndPinString(cx, c"name".as_ptr());
    assert!(!pinned.is_null());
    let mut order = 0;
    assert!(JS_CompareStrings(
        cx,
        pinned,
        name_id.to_string(),
        &mut order
    ));
    assert_eq!(order, 0);
    rooted!(&in(cx) let mut interned = jsid::default());
    RUST_INTERNED_STRING_TO_JSID(cx, pinned, interned.handle_mut());
    assert_eq!(interned.get().asBits_, name_id.asBits_);
    assert_eq!(to_rust(cx, name_id.to_string()), "name");
    observe_field("atomize_compare_order", Observed::Number(order as f64));
    observe_field(
        "atomize_pinned_text",
        Observed::String(to_rust(cx, name_id.to_string())),
    );
    assert!(JS_IdToValue(cx, name_id, v.handle_mut()));
    assert_eq!(to_rust(cx, v.get().to_string()), "name");
    observe_field(
        "atomize_id_to_value",
        Observed::String(to_rust(cx, v.get().to_string())),
    );
}

unsafe fn get_well_known_symbol(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());
    v.set(eval(cx, global, "Symbol.iterator"));
    assert!(JS_ValueToId(cx, v.handle(), id.handle_mut()));
    assert!(id.get().is_symbol());
    let symbol = GetWellKnownSymbol(cx, SymbolCode::iterator);
    assert!(!symbol.is_null());
    rooted!(&in(cx) let mut well_known = jsid::default());
    RUST_SYMBOL_TO_JSID(symbol, well_known.handle_mut());
    assert!(well_known.get().is_symbol());
    assert_eq!(well_known.get().asBits_, id.get().asBits_);
    rooted!(&in(cx) let mut back = UndefinedValue());
    assert!(JS_IdToValue(cx, id.get(), back.handle_mut()));
    let mut same = false;
    assert!(SameValue(cx, v.handle(), back.handle(), &mut same));
    assert!(same);
    observe_field("well_known_symbol_roundtrip", Observed::Boolean(same));
}

unsafe fn rust_symbol_to_jsid(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());
    v.set(eval(cx, global, "Symbol.iterator"));
    assert!(JS_ValueToId(cx, v.handle(), id.handle_mut()));
    let symbol = GetWellKnownSymbol(cx, SymbolCode::iterator);
    rooted!(&in(cx) let mut converted = jsid::default());
    RUST_SYMBOL_TO_JSID(symbol, converted.handle_mut());
    assert!(converted.get().is_symbol());
    assert_eq!(converted.get().asBits_, id.get().asBits_);

    let other = GetWellKnownSymbol(cx, SymbolCode::asyncIterator);
    rooted!(&in(cx) let mut other_id = jsid::default());
    RUST_SYMBOL_TO_JSID(other, other_id.handle_mut());
    assert_ne!(other_id.get().asBits_, converted.get().asBits_);
    observe_field(
        "symbol_ids_distinct",
        Observed::Boolean(other_id.get().asBits_ != converted.get().asBits_),
    );
}

unsafe fn js_define_property_by_id2(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());
    v.set(eval(cx, global, "'na' + 'me'"));
    assert!(JS_ValueToId(cx, v.handle(), id.handle_mut()));
    let pinned = JS_AtomizeAndPinString(cx, c"name".as_ptr());
    rooted!(&in(cx) let mut interned = jsid::default());
    RUST_INTERNED_STRING_TO_JSID(cx, pinned, interned.handle_mut());
    let symbol = GetWellKnownSymbol(cx, SymbolCode::iterator);
    rooted!(&in(cx) let mut well_known = jsid::default());
    RUST_SYMBOL_TO_JSID(symbol, well_known.handle_mut());

    rooted!(&in(cx) let obj = eval(cx, global, "globalThis.keyed = {}; keyed").to_object());
    rooted!(&in(cx) let value = Int32Value(9));
    assert!(JS_DefinePropertyById2(
        cx,
        obj.handle(),
        interned.handle(),
        value.handle(),
        1
    ));
    assert!(JS_DefinePropertyById2(
        cx,
        obj.handle(),
        well_known.handle(),
        value.handle(),
        1
    ));
    assert!(JS_IndexToId(cx, 3, id.handle_mut()));
    assert!(JS_DefinePropertyById2(
        cx,
        obj.handle(),
        id.handle(),
        value.handle(),
        1
    ));
    let keyed_name = eval(cx, global, "keyed.name").to_int32();
    let keyed_iterator = eval(cx, global, "keyed[Symbol.iterator]").to_int32();
    let keyed_index = eval(cx, global, "keyed[3]").to_int32();
    assert_eq!(
        keyed_name, 9,
        "JS_DefinePropertyById2 did not define `name`"
    );
    assert_eq!(
        keyed_iterator, 9,
        "JS_DefinePropertyById2 did not define Symbol.iterator"
    );
    assert_eq!(
        keyed_index, 9,
        "JS_DefinePropertyById2 did not define element 3"
    );
    observe_field(
        "define_by_id2_values",
        Observed::String(format!("{keyed_name},{keyed_iterator},{keyed_index}")),
    );
}

unsafe fn js_id_to_value(cx: &mut JSContext, _global: HandleObject) {
    rooted!(&in(cx) let mut id = jsid::default());
    rooted!(&in(cx) let mut v = UndefinedValue());
    assert!(JS_IndexToId(cx, 7, id.handle_mut()));
    assert!(JS_IdToValue(cx, id.get(), v.handle_mut()));
    assert_eq!(v.get().to_int32(), 7);
    observe_field(
        "id_to_value_index",
        Observed::Number(v.get().to_int32() as f64),
    );

    let pinned = JS_AtomizeAndPinString(cx, c"roundTrip".as_ptr());
    assert!(!pinned.is_null());
    rooted!(&in(cx) let mut string_id = jsid::default());
    RUST_INTERNED_STRING_TO_JSID(cx, pinned, string_id.handle_mut());
    assert!(JS_IdToValue(cx, string_id.get(), v.handle_mut()));
    assert_eq!(to_rust(cx, v.get().to_string()), "roundTrip");
    observe_field(
        "id_to_value_string_id",
        Observed::String(to_rust(cx, v.get().to_string())),
    );
}

unsafe fn js_is_identifier(cx: &mut JSContext, _global: HandleObject) {
    for (name, expected) in [
        ("foo", true),
        ("$a1", true),
        ("_", true),
        ("class", true),
        ("café", true),
        ("a\u{200c}b", true),
        ("\u{1d4b3}x", true),
        ("1a", false),
        ("", false),
        ("a-b", false),
        ("a b", false),
    ] {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        rooted!(&in(cx) let s = JS_NewUCStringCopyN(cx, utf16.as_ptr(), utf16.len()));
        let mut is_identifier = false;
        assert!(JS_IsIdentifier(cx, s.handle(), &mut is_identifier));
        assert_eq!(is_identifier, expected, "JS_IsIdentifier({name:?})");
        if name == "café" || name == "1a" {
            observe_field(
                if name == "café" {
                    "identifier_unicode"
                } else {
                    "identifier_invalid"
                },
                Observed::Boolean(is_identifier),
            );
        }
    }
    let lone = [0x61, 0xd800];
    rooted!(&in(cx) let s = JS_NewUCStringCopyN(cx, lone.as_ptr(), lone.len()));
    let mut is_identifier = true;
    assert!(JS_IsIdentifier(cx, s.handle(), &mut is_identifier));
    assert!(!is_identifier);
    observe_field(
        "identifier_lone_surrogate",
        Observed::Boolean(is_identifier),
    );

    for (s, expected) in [
        ("0", Some(0)),
        ("123", Some(123)),
        ("0123", None),
        ("4294967294", Some(4294967294)),
        ("4294967295", None),
        ("", None),
        ("12a", None),
    ] {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let mut index = 0;
        let is_index =
            js::jsapi::js::StringIsArrayIndex1(utf16.as_ptr(), utf16.len() as u32, &mut index);
        assert_eq!(
            is_index.then_some(index),
            expected,
            "StringIsArrayIndex({s:?})"
        );
    }
}

unsafe extern "C" fn append_json(buf: *const u16, len: u32, data: *mut c_void) -> bool {
    let out = &mut *(data as *mut Option<String>);
    out.get_or_insert_with(String::new)
        .push_str(&String::from_utf16_lossy(slice::from_raw_parts(
            buf,
            len as usize,
        )));
    true
}

unsafe fn to_json(cx: &mut JSContext, v: HandleValue) -> Option<String> {
    let mut out: Option<String> = None;
    assert!(ToJSON(
        cx,
        v,
        HandleObject::null(),
        HandleValue::null(),
        Some(append_json),
        &mut out as *mut _ as *mut c_void
    ));
    out
}

unsafe fn js_parse_json(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut v = UndefinedValue());
    let text: Vec<u16> = r#"{"a": [1, 2.5, {"b": "é€"}], "c": null, "d": true}"#
        .encode_utf16()
        .collect();
    assert!(JS_ParseJSON(
        cx,
        text.as_ptr(),
        text.len() as u32,
        v.handle_mut()
    ));
    assert!(v.get().is_object());
    assert_eq!(
        to_json(cx, v.handle()).unwrap(),
        r#"{"a":[1,2.5,{"b":"é€"}],"c":null,"d":true}"#
    );
    observe_field(
        "parse_json_text",
        Observed::String(to_json(cx, v.handle()).expect("parsed JSON should stringify")),
    );
    set_global(cx, global, c"parsed", v.get());
    assert!(
        eval(
            cx,
            global,
            "parsed.a[2].b === '\\u00e9\\u20ac' && parsed.a[1] === 2.5"
        )
        .to_boolean()
    );
    observe_field("parse_json_unicode", Observed::Boolean(true));

    let bad: Vec<u16> = "{a: 1}".encode_utf16().collect();
    assert!(!JS_ParseJSON(
        cx,
        bad.as_ptr(),
        bad.len() as u32,
        v.handle_mut()
    ));
    crate::support::assert_exception_named(cx, "SyntaxError");

    // A raw lone surrogate inside a JSON string survives parsing.
    let lone = [0x22, 0xd800, 0x78, 0x22];
    assert!(JS_ParseJSON(
        cx,
        lone.as_ptr(),
        lone.len() as u32,
        v.handle_mut()
    ));
    assert_eq!(units(cx, v.get().to_string()), [0xd800, 0x78]);
    assert_eq!(to_json(cx, v.handle()).unwrap(), r#""\ud800x""#);
    observe_field(
        "parse_json_lone_surrogate",
        Observed::String(to_json(cx, v.handle()).unwrap()),
    );

    rooted!(&in(cx) let reviver = eval(cx, global, "(k, v) => typeof v === 'number' ? v * 2 : v"));
    let text: Vec<u16> = "[1, [2]]".encode_utf16().collect();
    assert!(JS_ParseJSONWithReviver(
        cx,
        text.as_ptr(),
        text.len() as u32,
        reviver.handle(),
        v.handle_mut()
    ));
    assert_eq!(to_json(cx, v.handle()).unwrap(), "[2,[4]]");
    observe_field(
        "parse_json_reviver",
        Observed::String(to_json(cx, v.handle()).unwrap()),
    );
}

unsafe fn to_json_step(cx: &mut JSContext, _global: HandleObject) {
    rooted!(&in(cx) let mut v = UndefinedValue());
    v.set(UndefinedValue());
    assert_eq!(to_json(cx, v.handle()), None);
    observe_field("to_json_undefined", Observed::String("none".into()));

    v.set(eval(cx, _global, "({a: 1, b: [2]})"));
    assert_eq!(
        to_json(cx, v.handle()).as_deref(),
        Some(r#"{"a":1,"b":[2]}"#)
    );
    observe_field(
        "to_json_object",
        Observed::String(to_json(cx, v.handle()).unwrap()),
    );

    v.set(eval(
        cx,
        _global,
        "({ get x() { throw new Error('no'); } })",
    ));
    let mut out: Option<String> = None;
    assert!(!ToJSON(
        cx,
        v.handle(),
        HandleObject::null(),
        HandleValue::null(),
        Some(append_json),
        &mut out as *mut _ as *mut c_void
    ));
    crate::support::assert_exception_named(cx, "Error");
    v.set(eval(cx, _global, "1n"));
    assert!(!ToJSON(
        cx,
        v.handle(),
        HandleObject::null(),
        HandleValue::null(),
        Some(append_json),
        &mut out as *mut _ as *mut c_void
    ));
    crate::support::assert_exception_named(cx, "TypeError");
}

unsafe fn js_stringify(cx: &mut JSContext, global: HandleObject) {
    rooted!(&in(cx) let mut v = UndefinedValue());
    v.set(UndefinedValue());
    let mut out: Option<String> = None;
    assert!(JS_Stringify(
        cx,
        v.handle_mut(),
        HandleObject::null(),
        HandleValue::null(),
        Some(append_json),
        &mut out as *mut _ as *mut c_void
    ));
    assert_eq!(out.as_deref(), Some("null"));
    observe_field("stringify_undefined", Observed::String("null".into()));

    v.set(eval(
        cx,
        global,
        "({a: [1], toJSON() { return {b: this.a}; }})",
    ));
    rooted!(&in(cx) let space = Int32Value(2));
    let mut out: Option<String> = None;
    assert!(JS_Stringify(
        cx,
        v.handle_mut(),
        HandleObject::null(),
        space.handle(),
        Some(append_json),
        &mut out as *mut _ as *mut c_void
    ));
    assert_eq!(out.as_deref(), Some("{\n  \"b\": [\n    1\n  ]\n}"));
    observe_field(
        "stringify_indented",
        Observed::String(out.clone().unwrap_or_default()),
    );
}
