/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![allow(unsafe_op_in_unsafe_fn)]

//! Object-level entry points: arrays, key enumeration, Map/Set/WeakMap, Date, RegExp,
//! native functions with reserved slots and jit info, iteration and instanceof.

use std::ops::ControlFlow;
use std::ptr;

use js::context::JSContext;
use js::jsapi::{
    CallArgs, ClippedTime, ESClass, GCReason, GetFunctionNativeReserved, HandleValueArray,
    IsCallable, IsConstructor, JSContext as RawJSContext, JSFunctionSpec, JSITER_HIDDEN,
    JSITER_OWNONLY, JSITER_SYMBOLS, JSITER_SYMBOLSONLY, JSJitInfo, JSJitMethodCallArgs,
    JSNativeWrapper, JSObject, JSPROP_ENUMERATE, JSPropertySpec_Name, NewArrayObject1,
    OnNewGlobalHookOption, RegExpFlags, SetFunctionNativeReserved, Value,
};
use js::jsval::{Int32Value, JSVal, ObjectValue, StringValue, UndefinedValue};
use js::realm::AutoRealm;
use js::rooted;
use js::rust::wrappers2::{
    CheckRegExpSyntax, DateGetMsecSinceEpoch, ExecuteRegExpNoStatics, GetArrayLength,
    GetBuiltinClass, GetPropertyKeys, GetRealmObjectPrototype, GetWeakMapEntry, IsArrayObject,
    JS_ClearPendingException, JS_DefineFunctions, JS_DefineProperty, JS_FreezeObject, JS_GC,
    JS_GetElement, JS_GetFunctionId, JS_GetPropertyById, JS_HasInstance, JS_IsExceptionPending,
    JS_NewGlobalObject, JS_NewObjectWithGivenProto, JS_NewPlainObject, JS_SetElement,
    JS_SetProperty, MapEntries, MapGet, MapSet, MapSize, NewArrayObject, NewDateObject,
    NewFunctionWithReserved, NewMapObject, NewSetObject, NewUCRegExpObject, NewWeakMapObject,
    ObjectIsDate, SetAdd, SetArrayLength, SetWeakMapEntry,
};
use js::rust::{
    CompileOptionsWrapper, HandleObject, IdVector, JSEngine, RealmOptions, Runtime,
    SIMPLE_GLOBAL_CLASS, evaluate_script, for_of,
};

use crate::support::{Value as Observed, observe_field};

unsafe fn eval(cx: &mut JSContext, global: HandleObject, src: &str) -> JSVal {
    rooted!(&in(cx) let mut rval = UndefinedValue());
    let options = CompileOptionsWrapper::new(cx, c"qjs_objects".to_owned(), 1);
    assert!(
        evaluate_script(cx, global, src, rval.handle_mut(), options).is_ok(),
        "{src}"
    );
    rval.get()
}

unsafe fn set_global(cx: &mut JSContext, global: HandleObject, name: &std::ffi::CStr, v: JSVal) {
    rooted!(&in(cx) let v = v);
    assert!(JS_DefineProperty(
        cx,
        global,
        name.as_ptr(),
        v.handle(),
        JSPROP_ENUMERATE as u32
    ));
}

unsafe fn builtin_class(cx: &mut JSContext, obj: *mut JSObject) -> ESClass {
    rooted!(&in(cx) let obj = obj);
    let mut cls = ESClass::Other;
    assert!(GetBuiltinClass(cx, obj.handle(), &mut cls));
    cls
}

fn function_id_text(cx: &mut JSContext, string: *mut js::jsapi::JSString) -> String {
    unsafe {
        js::conversions::jsstr_to_string(
            cx,
            std::ptr::NonNull::new(string).expect("non-null string"),
        )
    }
}

/// Runs `f` in the realm of a new global.
fn with_global(engine: &JSEngine, f: impl FnOnce(&mut JSContext, HandleObject)) {
    let mut runtime = Runtime::new(engine.handle());
    let context = runtime.cx();
    unsafe {
        rooted!(&in(context) let global = JS_NewGlobalObject(
            context,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(!global.get().is_null(), "JS_NewGlobalObject returned null");
        let mut realm = AutoRealm::new_from_handle(context, global.handle());
        f(&mut realm, global.handle());
    }
}

fn arrays(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        rooted!(&in(cx) let values = vec![Int32Value(1), Int32Value(2), Int32Value(3)]);
        rooted!(&in(cx) let array = NewArrayObject(cx, &HandleValueArray::from(&values)));
        let mut length = 0;
        assert!(GetArrayLength(cx, array.handle(), &mut length));
        assert_eq!(length, 3);
        observe_field("arrays_length", Observed::Number(length as f64));
        rooted!(&in(cx) let mut element = UndefinedValue());
        assert!(JS_GetElement(cx, array.handle(), 2, element.handle_mut()));
        assert_eq!(element.get().to_int32(), 3);
        observe_field(
            "arrays_element_2",
            Observed::Number(element.get().to_int32() as f64),
        );

        rooted!(&in(cx) let sized = NewArrayObject1(cx.raw_cx(), 5));
        assert!(GetArrayLength(cx, sized.handle(), &mut length));
        assert_eq!(length, 5);

        // SetArrayLength truncates and extends, and throws where `length` can't be set.
        rooted!(&in(cx) let resized = NewArrayObject(cx, &HandleValueArray::from(&values)));
        assert!(SetArrayLength(cx, resized.handle(), 1));
        assert!(GetArrayLength(cx, resized.handle(), &mut length));
        assert_eq!(length, 1);
        assert!(JS_GetElement(cx, resized.handle(), 1, element.handle_mut()));
        assert!(element.get().is_undefined());
        assert!(SetArrayLength(cx, resized.handle(), 4));
        assert!(GetArrayLength(cx, resized.handle(), &mut length));
        assert_eq!(length, 4);
        assert!(JS_FreezeObject(cx, sized.handle()));
        assert!(!SetArrayLength(cx, sized.handle(), 2));
        assert!(JS_IsExceptionPending(cx));
        JS_ClearPendingException(cx);
        assert!(GetArrayLength(cx, sized.handle(), &mut length));
        assert_eq!(length, 5);

        let mut is_array = false;
        rooted!(&in(cx) let v = ObjectValue(array.get()));
        assert!(IsArrayObject(cx, v.handle(), &mut is_array));
        assert!(is_array);
        observe_field("arrays_is_array", Observed::Boolean(is_array));
        rooted!(&in(cx) let v = Int32Value(1));
        assert!(IsArrayObject(cx, v.handle(), &mut is_array));
        assert!(!is_array);
        // As in SpiderMonkey, by the builtin class: a Proxy of an array isn't one.
        rooted!(&in(cx) let v = eval(cx, global, "new Proxy([1], {})"));
        assert!(IsArrayObject(cx, v.handle(), &mut is_array));
        assert!(!is_array);
        assert_eq!(builtin_class(cx, array.get()), ESClass::Array);

        assert!(JS_FreezeObject(cx, array.handle()));
        set_global(cx, global, c"frozen", ObjectValue(array.get()));
        assert!(eval(cx, global, "Object.isFrozen(frozen) && frozen[1] === 2").to_boolean());

        // As in SpiderMonkey, failed assignments are ignored, but exceptions are not.
        rooted!(&in(cx) let v = Int32Value(7));
        assert!(JS_SetElement(cx, array.handle(), 1, v.handle()));
        assert!(JS_SetProperty(
            cx,
            array.handle(),
            c"x".as_ptr(),
            v.handle()
        ));
        assert!(eval(cx, global, "frozen[1] === 2 && !('x' in frozen)").to_boolean());
        rooted!(&in(cx) let thrower = eval(cx, global, "({ set x(v) { throw 1; } })").to_object());
        assert!(!JS_SetProperty(
            cx,
            thrower.handle(),
            c"x".as_ptr(),
            v.handle()
        ));
        assert!(JS_IsExceptionPending(cx));
        JS_ClearPendingException(cx);

        rooted!(&in(cx) let ctor = eval(cx, global, "Array").to_object());
        rooted!(&in(cx) let v = ObjectValue(array.get()));
        let mut is_instance = false;
        assert!(JS_HasInstance(
            cx,
            ctor.handle(),
            v.handle(),
            &mut is_instance
        ));
        assert!(is_instance);
        rooted!(&in(cx) let v = ObjectValue(GetRealmObjectPrototype(cx)));
        assert!(JS_HasInstance(
            cx,
            ctor.handle(),
            v.handle(),
            &mut is_instance
        ));
        assert!(!is_instance);

        // A new object given an explicit prototype must inherit exactly that prototype, so both
        // the inherited-property lookup and the prototype identity depend on the call.
        rooted!(&in(cx) let proto = eval(cx, global, "({ inherited: 41 })").to_object());
        rooted!(&in(cx) let given = JS_NewObjectWithGivenProto(cx, ptr::null(), proto.handle()));
        assert!(!given.get().is_null());
        set_global(cx, global, c"given_proto", ObjectValue(proto.get()));
        set_global(cx, global, c"given", ObjectValue(given.get()));
        assert!(
            eval(
                cx,
                global,
                "Object.getPrototypeOf(given) === given_proto && given.inherited === 41 \
                 && !Object.hasOwn(given, 'inherited')",
            )
            .to_boolean()
        );
        observe_field("arrays_given_prototype", Observed::Boolean(true));
    });
}

fn property_keys(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        rooted!(&in(cx) let obj = eval(cx, global, "
            var proto = { inherited: 10 };
            var obj = Object.create(proto);
            obj.a = 1;
            obj.b = 2;
            obj[Symbol.iterator] = 3;
            Object.defineProperty(obj, 'hidden', { value: 4, enumerable: false });
            obj;
        ").to_object());

        // Returns the values of the keys, which identify them.
        let keys = |cx: &mut JSContext, flags: u32| -> (Vec<i32>, usize) {
            let mut ids = IdVector::new(cx);
            assert!(GetPropertyKeys(cx, obj.handle(), flags, ids.handle_mut()));
            let mut values = vec![];
            let mut symbols = 0;
            for id in ids.iter() {
                rooted!(&in(cx) let id = *id);
                rooted!(&in(cx) let mut v = UndefinedValue());
                assert!(JS_GetPropertyById(
                    cx,
                    obj.handle(),
                    id.handle(),
                    v.handle_mut()
                ));
                values.push(v.get().to_int32());
                symbols += id.get().is_symbol() as usize;
            }
            (values, symbols)
        };

        assert_eq!(keys(cx, JSITER_OWNONLY), (vec![1, 2], 0));
        assert_eq!(keys(cx, JSITER_OWNONLY | JSITER_HIDDEN), (vec![1, 2, 4], 0));
        assert_eq!(
            keys(cx, JSITER_OWNONLY | JSITER_HIDDEN | JSITER_SYMBOLS),
            (vec![1, 2, 4, 3], 1)
        );
        // SpiderMonkey's JSITER_SYMBOLSONLY only affects proxies; ordinary objects return no keys.
        assert_eq!(keys(cx, JSITER_OWNONLY | JSITER_SYMBOLSONLY), (vec![], 0));
        assert_eq!(keys(cx, 0), (vec![1, 2, 10], 0));
        observe_field(
            "keys_own",
            Observed::String(format!("{:?}", keys(cx, JSITER_OWNONLY))),
        );
        observe_field(
            "keys_own_hidden_symbols",
            Observed::String(format!(
                "{:?}",
                keys(cx, JSITER_OWNONLY | JSITER_HIDDEN | JSITER_SYMBOLS)
            )),
        );

        rooted!(&in(cx) let indexed = eval(cx, global, "({ 1: 5, x: 6 })").to_object());
        let mut ids = IdVector::new(cx);
        assert!(GetPropertyKeys(
            cx,
            indexed.handle(),
            JSITER_OWNONLY,
            ids.handle_mut()
        ));
        assert_eq!(ids.len(), 2);
        assert!(ids[0].is_int() && ids[0].to_int() == 1);
        assert!(ids[1].is_string());
        observe_field("keys_indexed_count", Observed::Number(ids.len() as f64));
        observe_field(
            "keys_indexed_first_is_int",
            Observed::Boolean(ids[0].is_int()),
        );
    });
}

/// `MapSize` must track inserts and deletes.
fn map_size(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let map = NewMapObject(cx));
        rooted!(&in(cx) let key = Int32Value(1));
        rooted!(&in(cx) let value = Int32Value(42));
        assert_eq!(MapSize(cx, map.handle()), 0);
        assert!(MapSet(cx, map.handle(), key.handle(), value.handle()));
        assert_eq!(MapSize(cx, map.handle()), 1);
        observe_field("map_size_after_set", Observed::Number(1.0));
        let mut deleted = false;
        assert!(js::jsapi::MapDelete(
            cx.raw_cx(),
            map.handle().into(),
            key.handle().into(),
            &mut deleted
        ));
        assert!(deleted);
        assert_eq!(MapSize(cx, map.handle()), 0);
        observe_field("map_size_after_delete", Observed::Number(0.0));
        observe_field("map_delete_removed", Observed::Boolean(deleted));
    });
}

fn map_get_and_has(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let map = NewMapObject(cx));
        rooted!(&in(cx) let key = Int32Value(1));
        rooted!(&in(cx) let value = Int32Value(42));
        rooted!(&in(cx) let mut out = UndefinedValue());
        let mut has = true;
        assert!(MapGet(cx, map.handle(), key.handle(), out.handle_mut()));
        assert!(out.get().is_undefined());
        assert!(js::jsapi::MapHas(
            cx.raw_cx(),
            map.handle().into(),
            key.handle().into(),
            &mut has
        ));
        assert!(!has);

        assert!(MapSet(cx, map.handle(), key.handle(), value.handle()));
        assert!(MapGet(cx, map.handle(), key.handle(), out.handle_mut()));
        assert_eq!(out.get().to_int32(), 42);
        observe_field(
            "map_get_value",
            Observed::Number(out.get().to_int32() as f64),
        );
        assert!(js::jsapi::MapHas(
            cx.raw_cx(),
            map.handle().into(),
            key.handle().into(),
            &mut has
        ));
        assert!(has);
        observe_field("map_has_after_set", Observed::Boolean(has));
        observe_field("map_has_empty", Observed::Boolean(false));
    });
}

/// `MapEntries` must return a fresh iterator over exactly the live key/value pairs.
fn map_entries(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let map = NewMapObject(cx));
        rooted!(&in(cx) let first_key = Int32Value(1));
        rooted!(&in(cx) let first_value = Int32Value(42));
        rooted!(&in(cx) let second_key = Int32Value(2));
        rooted!(&in(cx) let second_value = Int32Value(43));
        assert!(MapSet(
            cx,
            map.handle(),
            first_key.handle(),
            first_value.handle()
        ));
        assert!(MapSet(
            cx,
            map.handle(),
            second_key.handle(),
            second_value.handle()
        ));

        rooted!(&in(cx) let mut out = UndefinedValue());
        assert!(MapEntries(cx, map.handle(), out.handle_mut()));
        assert!(out.get().is_object());
        let mut entries = Vec::new();
        assert!(
            for_of::<_, _, ()>(cx, out.handle(), |inner_cx, entry| {
                assert!(entry.get().is_object());
                rooted!(&in(inner_cx) let pair = entry.get().to_object());
                rooted!(&in(inner_cx) let mut key = UndefinedValue());
                assert!(JS_GetElement(inner_cx, pair.handle(), 0, key.handle_mut()));
                rooted!(&in(inner_cx) let mut value = UndefinedValue());
                assert!(JS_GetElement(
                    inner_cx,
                    pair.handle(),
                    1,
                    value.handle_mut()
                ));
                entries.push((key.get().to_int32(), value.get().to_int32()));
                Ok(ControlFlow::Continue(()))
            })
            .is_ok()
        );
        assert_eq!(entries, [(1, 42), (2, 43)]);
        observe_field("map_entries", Observed::String(format!("{entries:?}")));
    });
}

fn set_objects(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let key = Int32Value(1));
        rooted!(&in(cx) let value = Int32Value(42));
        rooted!(&in(cx) let set = NewSetObject(cx));
        assert!(SetAdd(cx, set.handle(), key.handle()));
        assert!(SetAdd(cx, set.handle(), value.handle()));
        assert!(SetAdd(cx, set.handle(), key.handle()));
        assert_eq!(js::jsapi::SetSize(cx.raw_cx(), set.handle().into()), 2);
        assert_eq!(builtin_class(cx, set.get()), ESClass::Set);
        rooted!(&in(cx) let set_value = ObjectValue(set.get()));
        let mut sum = 0;
        assert!(
            for_of::<_, _, ()>(cx, set_value.handle(), |_, v| {
                sum += v.get().to_int32();
                Ok(ControlFlow::Continue(()))
            })
            .is_ok()
        );
        assert_eq!(sum, 43);
        observe_field("set_sum_before_clear", Observed::Number(sum as f64));
        observe_field("set_size_after_add", Observed::Number(2.0));
        assert!(js::jsapi::SetClear(cx.raw_cx(), set.handle().into()));
        assert_eq!(js::jsapi::SetSize(cx.raw_cx(), set.handle().into()), 0);
        observe_field("set_size_after_clear", Observed::Number(0.0));
    });
}

fn get_realm_key_object(engine: &JSEngine) {
    let mut runtime = Runtime::new(engine.handle());
    let cx = runtime.cx();
    unsafe {
        rooted!(&in(cx) let first_global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(
            !first_global.get().is_null(),
            "JS_NewGlobalObject returned null"
        );
        rooted!(&in(cx) let second_global = JS_NewGlobalObject(
            cx,
            &SIMPLE_GLOBAL_CLASS,
            ptr::null_mut(),
            OnNewGlobalHookOption::FireOnNewGlobalHook,
            &*RealmOptions::default(),
        ));
        assert!(
            !second_global.get().is_null(),
            "JS_NewGlobalObject returned null"
        );

        let mut realm = AutoRealm::new_from_handle(cx, first_global.handle());
        let key_a = js::rust::wrappers2::GetRealmKeyObject(&mut realm);
        let key_b = js::rust::wrappers2::GetRealmKeyObject(&mut realm);
        assert!(!key_a.is_null() && !key_b.is_null());
        assert_eq!(key_a, key_b, "same realm returned two key objects");

        let mut realm = AutoRealm::new_from_handle(&mut realm, second_global.handle());
        let other_key = js::rust::wrappers2::GetRealmKeyObject(&mut realm);
        assert!(!other_key.is_null());
        assert_ne!(key_a, other_key, "different realms shared a key object");

        observe_field(
            "realm_key_identity",
            Observed::String(format!(
                "same_realm={},different_realm={}",
                key_a == key_b,
                key_a != other_key
            )),
        );
    }
}

fn new_weak_map_object(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let weak = NewWeakMapObject(cx));
        assert!(!weak.get().is_null());
        assert!(js::jsapi::IsWeakMapObject(weak.get()));
        observe_field("weak_map_is_weak_map", Observed::Boolean(true));
        rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
        assert!(!js::jsapi::IsWeakMapObject(plain.get()));
        observe_field("weak_map_plain_is_not_weak_map", Observed::Boolean(true));
    });
}

fn set_weak_map_entry(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let weak = NewWeakMapObject(cx));
        rooted!(&in(cx) let key_value = ObjectValue(js::rust::wrappers2::GetRealmKeyObject(cx)));
        rooted!(&in(cx) let value = Int32Value(42));
        rooted!(&in(cx) let mut out = UndefinedValue());
        assert!(GetWeakMapEntry(
            cx,
            weak.handle(),
            key_value.handle(),
            out.handle_mut()
        ));
        assert!(out.get().is_undefined());
        assert!(SetWeakMapEntry(
            cx,
            weak.handle(),
            key_value.handle(),
            value.handle()
        ));
        assert!(GetWeakMapEntry(
            cx,
            weak.handle(),
            key_value.handle(),
            out.handle_mut()
        ));
        assert_eq!(out.get().to_int32(), 42);
        observe_field(
            "weak_map_set_value",
            Observed::Number(out.get().to_int32() as f64),
        );
    });
}

fn get_weak_map_entry(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let weak_one = NewWeakMapObject(cx));
        rooted!(&in(cx) let weak_two = NewWeakMapObject(cx));
        rooted!(&in(cx) let key_value = ObjectValue(js::rust::wrappers2::GetRealmKeyObject(cx)));
        rooted!(&in(cx) let value_one = Int32Value(42));
        rooted!(&in(cx) let value_two = Int32Value(43));
        rooted!(&in(cx) let mut out = UndefinedValue());
        assert!(SetWeakMapEntry(
            cx,
            weak_one.handle(),
            key_value.handle(),
            value_one.handle()
        ));
        assert!(SetWeakMapEntry(
            cx,
            weak_two.handle(),
            key_value.handle(),
            value_two.handle()
        ));
        assert!(GetWeakMapEntry(
            cx,
            weak_one.handle(),
            key_value.handle(),
            out.handle_mut()
        ));
        assert_eq!(out.get().to_int32(), 42);
        assert!(GetWeakMapEntry(
            cx,
            weak_two.handle(),
            key_value.handle(),
            out.handle_mut()
        ));
        assert_eq!(out.get().to_int32(), 43);
        observe_field(
            "weak_map_two_values",
            Observed::String(format!("{},{}", 42, out.get().to_int32())),
        );

        rooted!(&in(cx) let missing_key = ObjectValue(JS_NewPlainObject(cx)));
        let missing_ok = GetWeakMapEntry(
            cx,
            weak_one.handle(),
            missing_key.handle(),
            out.handle_mut(),
        );
        assert!(missing_ok, "GetWeakMapEntry failed for a missing key");
        assert!(
            out.get().is_undefined(),
            "missing key did not read back undefined"
        );
        observe_field(
            "weak_map_missing_entry",
            Observed::String(format!("{missing_ok}:undefined")),
        );
    });
}

fn new_date_object(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let date = NewDateObject(cx, ClippedTime { t: 1.5e12 }));
        assert!(!date.get().is_null());
        assert_eq!(builtin_class(cx, date.get()), ESClass::Date);
        let mut ms = 0.0;
        assert!(DateGetMsecSinceEpoch(cx, date.handle(), &mut ms));
        assert_eq!(ms, 1.5e12);
        observe_field("date_constructed_ms", Observed::Number(ms));
        observe_field(
            "date_class",
            Observed::String(format!("{:?}", builtin_class(cx, date.get()))),
        );
    });
}

fn date_get_msec_since_epoch(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let date = NewDateObject(cx, ClippedTime { t: 1_234_567_890_123.0 }));
        let mut ms = f64::NAN;
        assert!(DateGetMsecSinceEpoch(cx, date.handle(), &mut ms));
        assert_eq!(ms, 1_234_567_890_123.0);
        observe_field("date_ms", Observed::Number(ms));
    });
}

fn object_is_date(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        rooted!(&in(cx) let date = NewDateObject(cx, ClippedTime { t: 0.0 }));
        let mut is_date = false;
        assert!(ObjectIsDate(cx, date.handle(), &mut is_date));
        assert!(is_date);
        observe_field("date_is_date", Observed::Boolean(is_date));
        rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
        assert!(ObjectIsDate(cx, plain.handle(), &mut is_date));
        assert!(!is_date);
        observe_field("date_plain_is_not_date", Observed::Boolean(!is_date));
    });
}

fn new_uc_regexp_object(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        let flags = RegExpFlags {
            flags_: js::jsapi::RegExpFlag_UnicodeSets,
        };
        let pattern: Vec<u16> = "^(?:[a-c]+)$".encode_utf16().collect();
        rooted!(&in(cx) let re = NewUCRegExpObject(cx, pattern.as_ptr(), pattern.len(), flags));
        assert!(!re.get().is_null());
        assert_eq!(builtin_class(cx, re.get()), ESClass::RegExp);
        set_global(cx, global, c"re", ObjectValue(re.get()));
        assert!(eval(cx, global, "re.flags === 'v' && re.lastIndex === 0").to_boolean());
        let source = {
            let string = eval(cx, global, "re.source").to_string();
            js::conversions::jsstr_to_string(
                cx,
                std::ptr::NonNull::new(string).expect("non-null regexp source"),
            )
        };
        assert_eq!(source, "^(?:[a-c]+)$");
        assert_eq!(
            crate::support::eval_observe(cx, global, "re.source"),
            Observed::String("^(?:[a-c]+)$".to_owned())
        );
        observe_field(
            "regexp_class",
            Observed::String(format!("{:?}", builtin_class(cx, re.get()))),
        );
        observe_field("regexp_source", Observed::String(source));
    });
}

fn object_is_regexp(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        let flags = RegExpFlags {
            flags_: js::jsapi::RegExpFlag_UnicodeSets,
        };
        let pattern: Vec<u16> = "^[a-c]+$".encode_utf16().collect();
        rooted!(&in(cx) let re = NewUCRegExpObject(cx, pattern.as_ptr(), pattern.len(), flags));
        let mut is_regexp = false;
        assert!(js::jsapi::ObjectIsRegExp(
            cx.raw_cx(),
            re.handle().into(),
            &mut is_regexp
        ));
        assert!(is_regexp);
        observe_field("regexp_is_regexp", Observed::Boolean(is_regexp));
        rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
        assert!(js::jsapi::ObjectIsRegExp(
            cx.raw_cx(),
            plain.handle().into(),
            &mut is_regexp
        ));
        assert!(!is_regexp);
        observe_field("regexp_plain_is_not_regexp", Observed::Boolean(!is_regexp));
    });
}

fn execute_regexp_no_statics(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        let flags = RegExpFlags {
            flags_: js::jsapi::RegExpFlag_UnicodeSets,
        };
        let pattern: Vec<u16> = "^(?:[a-c]+)$".encode_utf16().collect();
        rooted!(&in(cx) let re = NewUCRegExpObject(cx, pattern.as_ptr(), pattern.len(), flags));
        rooted!(&in(cx) let mut rval = UndefinedValue());
        for (input, matches) in [("abc", true), ("abd", false)] {
            let input: Vec<u16> = input.encode_utf16().collect();
            let mut index = 0;
            assert!(ExecuteRegExpNoStatics(
                cx,
                re.handle(),
                input.as_ptr(),
                input.len(),
                &mut index,
                true,
                rval.handle_mut()
            ));
            assert_eq!(!rval.get().is_null(), matches);
            assert_eq!(index, if matches { 3 } else { 0 });
            observe_field(
                if matches {
                    "regexp_exec_match_index"
                } else {
                    "regexp_exec_miss_index"
                },
                Observed::Number(index as f64),
            );
        }
    });
}

fn check_regexp_syntax(engine: &JSEngine) {
    with_global(engine, |cx, _global| unsafe {
        let flags = RegExpFlags {
            flags_: js::jsapi::RegExpFlag_UnicodeSets,
        };
        let check = |cx: &mut JSContext, pattern: &str| {
            let pattern: Vec<u16> = pattern.encode_utf16().collect();
            rooted!(&in(cx) let mut error = UndefinedValue());
            assert!(CheckRegExpSyntax(
                cx,
                pattern.as_ptr(),
                pattern.len(),
                flags,
                error.handle_mut()
            ));
            assert!(!js::jsapi::JS_IsExceptionPending(cx.raw_cx()));
            error.get()
        };
        let valid = check(cx, "[a-z]+");
        assert!(valid.is_undefined());
        let invalid = check(cx, "(");
        assert!(invalid.is_object());
        set_global(cx, _global, c"regexpError", invalid);
        let error_text = eval(cx, _global, "regexpError.name + ':' + regexpError.message");
        let error_text = js::conversions::jsstr_to_string(
            cx,
            std::ptr::NonNull::new(error_text.to_string()).expect("regexp error text"),
        );
        assert!(
            eval(
                cx,
                _global,
                "regexpError instanceof SyntaxError && regexpError.message.length > 0"
            )
            .to_boolean()
        );
        observe_field("regexp_syntax_valid_is_undefined", Observed::Boolean(true));
        observe_field("regexp_syntax_invalid_is_object", Observed::Boolean(true));
        observe_field("regexp_syntax_error", Observed::String(error_text));
    });
}

const SLOT_HOLDER: usize = 0;

unsafe extern "C" fn read_reserved(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval()
        .set(*GetFunctionNativeReserved(args.callee(), SLOT_HOLDER));
    true
}

fn reserved_slots(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        rooted!(&in(cx) let fun = NewFunctionWithReserved(cx, Some(read_reserved), 0, 0, c"readReserved".as_ptr()));
        rooted!(&in(cx) let fun_obj = fun.get() as *mut JSObject);
        set_global(cx, global, c"readReserved", ObjectValue(fun_obj.get()));

        // A cycle through the reserved slot: holder.fun -> function -> slot -> holder.
        rooted!(&in(cx) let holder = eval(cx, global, "({ value: 7, fun: readReserved })").to_object());
        SetFunctionNativeReserved(fun_obj.get(), SLOT_HOLDER, &ObjectValue(holder.get()));
        SetFunctionNativeReserved(fun_obj.get(), 1, &Int32Value(5));
        JS_GC(cx, GCReason::API);
        assert_eq!((*GetFunctionNativeReserved(fun_obj.get(), 1)).to_int32(), 5);
        observe_field(
            "reserved_slot_int",
            Observed::Number((*GetFunctionNativeReserved(fun_obj.get(), 1)).to_int32() as f64),
        );
        assert_eq!(eval(cx, global, "readReserved().value").to_int32(), 7);
        observe_field("reserved_slot_holder_value", Observed::Number(7.0));
        JS_GC(cx, GCReason::API);
        assert_eq!(
            eval(cx, global, "readReserved().fun === readReserved").to_boolean(),
            true
        );
        observe_field("reserved_slot_cycle_survives_gc", Observed::Boolean(true));

        assert_eq!(builtin_class(cx, fun_obj.get()), ESClass::Function);
        observe_field(
            "reserved_function_class",
            Observed::String("Function".into()),
        );
    });
}

fn is_callable_and_constructor(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        rooted!(&in(cx) let fun = NewFunctionWithReserved(
            cx,
            Some(read_reserved),
            0,
            0,
            c"isCallable".as_ptr()
        ));
        rooted!(&in(cx) let fun_obj = fun.get() as *mut JSObject);
        assert!(IsCallable(fun_obj.get()));
        assert!(!IsConstructor(fun_obj.get()));
        observe_field("callable_native_callable", Observed::Boolean(true));
        observe_field("callable_native_not_constructor", Observed::Boolean(true));

        rooted!(&in(cx) let ctor = eval(cx, global, "Array").to_object());
        assert!(IsCallable(ctor.get()));
        assert!(IsConstructor(ctor.get()));
        observe_field("callable_array_constructor", Observed::Boolean(true));

        rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
        assert!(!IsCallable(plain.get()));
        assert!(!IsConstructor(plain.get()));
        observe_field("callable_plain_neither", Observed::Boolean(true));
    });
}

fn new_function_with_reserved(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        rooted!(&in(cx) let fun = NewFunctionWithReserved(
            cx,
            Some(read_reserved),
            0,
            0,
            c"reservedName".as_ptr()
        ));
        assert!(!fun.get().is_null());
        let fun_obj = fun.get() as *mut JSObject;
        SetFunctionNativeReserved(fun_obj, SLOT_HOLDER, &Int32Value(17));
        assert_eq!(
            (*GetFunctionNativeReserved(fun_obj, SLOT_HOLDER)).to_int32(),
            17
        );
        observe_field(
            "function_reserved_value",
            Observed::Number((*GetFunctionNativeReserved(fun_obj, SLOT_HOLDER)).to_int32() as f64),
        );
        set_global(cx, global, c"reservedName", ObjectValue(fun_obj));
        assert_eq!(eval(cx, global, "reservedName()").to_int32(), 17);
        observe_field("function_reserved_call", Observed::Number(17.0));
    });
}

fn js_get_function_id(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        rooted!(&in(cx) let named = NewFunctionWithReserved(
            cx,
            Some(read_reserved),
            0,
            0,
            c"namedFunction".as_ptr()
        ));
        rooted!(&in(cx) let mut name = ptr::null_mut::<js::jsapi::JSString>());
        assert!(JS_GetFunctionId(cx, named.handle(), name.handle_mut()));
        assert!(!name.get().is_null());
        observe_field(
            "function_id_named",
            Observed::String(function_id_text(cx, name.get())),
        );
        set_global(cx, global, c"funName", StringValue(&*name.get()));
        assert!(eval(cx, global, "funName === 'namedFunction'").to_boolean());

        rooted!(&in(cx) let other = NewFunctionWithReserved(
            cx,
            Some(read_reserved),
            0,
            0,
            c"otherFunction".as_ptr()
        ));
        rooted!(&in(cx) let mut other_name = ptr::null_mut::<js::jsapi::JSString>());
        assert!(JS_GetFunctionId(
            cx,
            other.handle(),
            other_name.handle_mut()
        ));
        assert!(!other_name.get().is_null());
        observe_field(
            "function_id_other",
            Observed::String(function_id_text(cx, other_name.get())),
        );
        set_global(cx, global, c"otherFunName", StringValue(&*other_name.get()));
        assert!(eval(cx, global, "otherFunName === 'otherFunction'").to_boolean());
        assert!(eval(cx, global, "funName !== otherFunName").to_boolean());
    });
}

fn js_define_functions(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        let specs: &'static [JSFunctionSpec] = Box::leak(Box::new([
            JSFunctionSpec {
                name: JSPropertySpec_Name {
                    string_: c"seven".as_ptr(),
                },
                call: JSNativeWrapper {
                    op: Some(seven),
                    info: ptr::null(),
                },
                nargs: 0,
                flags: JSPROP_ENUMERATE as u16,
                selfHostedName: ptr::null(),
            },
            JSFunctionSpec::ZERO,
        ]));
        rooted!(&in(cx) let obj = JS_NewPlainObject(cx));
        assert!(JS_DefineFunctions(cx, obj.handle(), specs.as_ptr()));
        set_global(cx, global, c"defined", ObjectValue(obj.get()));
        assert_eq!(eval(cx, global, "defined.seven()").to_int32(), 7);
        assert!(eval(cx, global, "Object.keys(defined).join() === 'seven'").to_boolean());
        observe_field("defined_function_result", Observed::Number(7.0));
        observe_field(
            "defined_function_keys",
            crate::support::eval_observe(cx, global, "Object.keys(defined).join()"),
        );
    });
}

unsafe extern "C" fn double_first_arg(
    _cx: *mut RawJSContext,
    _obj: js::jsapi::HandleObject,
    this: *mut std::ffi::c_void,
    args: *const JSJitMethodCallArgs,
) -> bool {
    assert_eq!(this as usize, 0x1234);
    let v = (*args).get(0).to_int32();
    (*args).rval().set(Int32Value(v * 2));
    true
}

/// A generic method in the style of Servo's bindings: finds its jit info through the callee.
unsafe extern "C" fn generic_method(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    let info = js::glue::RUST_FUNCTION_VALUE_TO_JITINFO(*args.calleev().ptr);
    assert!(!info.is_null());
    rooted!(in(cx) let this = args.thisv().to_object());
    js::glue::CallJitMethodOp(info, cx, this.handle().into(), 0x1234 as *mut _, argc, vp)
}

fn rust_function_value_to_jitinfo(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        let mut info: JSJitInfo = std::mem::zeroed();
        info.__bindgen_anon_1.method = Some(double_first_arg);
        let info: &'static JSJitInfo = Box::leak(Box::new(info));
        let specs: &'static [JSFunctionSpec] = Box::leak(Box::new([
            JSFunctionSpec {
                name: JSPropertySpec_Name {
                    string_: c"jitDouble".as_ptr(),
                },
                call: JSNativeWrapper {
                    op: Some(generic_method),
                    info,
                },
                nargs: 1,
                flags: JSPROP_ENUMERATE as u16,
                selfHostedName: ptr::null(),
            },
            JSFunctionSpec::ZERO,
        ]));
        rooted!(&in(cx) let obj = JS_NewPlainObject(cx));
        assert!(JS_DefineFunctions(cx, obj.handle(), specs.as_ptr()));
        set_global(cx, global, c"jitObj", ObjectValue(obj.get()));

        rooted!(&in(cx) let fun = eval(cx, global, "jitObj.jitDouble"));
        assert_eq!(
            js::glue::RUST_FUNCTION_VALUE_TO_JITINFO(fun.get()),
            info as *const _
        );
    });
}

fn call_jit_method_op(engine: &JSEngine) {
    with_global(engine, |cx, global| unsafe {
        let mut info: JSJitInfo = std::mem::zeroed();
        info.__bindgen_anon_1.method = Some(double_first_arg);
        let info: &'static JSJitInfo = Box::leak(Box::new(info));
        let specs: &'static [JSFunctionSpec] = Box::leak(Box::new([
            JSFunctionSpec {
                name: JSPropertySpec_Name {
                    string_: c"callDouble".as_ptr(),
                },
                call: JSNativeWrapper {
                    op: Some(generic_method),
                    info,
                },
                nargs: 1,
                flags: JSPROP_ENUMERATE as u16,
                selfHostedName: ptr::null(),
            },
            JSFunctionSpec::ZERO,
        ]));
        rooted!(&in(cx) let obj = JS_NewPlainObject(cx));
        assert!(JS_DefineFunctions(cx, obj.handle(), specs.as_ptr()));
        set_global(cx, global, c"callObj", ObjectValue(obj.get()));
        assert_eq!(eval(cx, global, "callObj.callDouble(21)").to_int32(), 42);
        observe_field("jit_method_result", Observed::Number(42.0));
    });
}

unsafe extern "C" fn seven(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval().set(Int32Value(7));
    true
}

/// WebIDL attribute getters are named "get <name>".
fn accessor_function_names(engine: &JSEngine) {
    use js::jsapi::{
        JSPropertySpec, JSPropertySpec_Accessor, JSPropertySpec_AccessorsOrValue,
        JSPropertySpec_AccessorsOrValue_Accessors, JSPropertySpec_Kind,
    };
    with_global(engine, |cx, global| unsafe {
        let accessor = |op| JSPropertySpec_Accessor {
            native: JSNativeWrapper {
                op,
                info: ptr::null(),
            },
        };
        let specs = [
            JSPropertySpec {
                name: JSPropertySpec_Name {
                    string_: c"x".as_ptr(),
                },
                attributes_: JSPROP_ENUMERATE as u8,
                kind_: JSPropertySpec_Kind::NativeAccessor,
                u: JSPropertySpec_AccessorsOrValue {
                    accessors: JSPropertySpec_AccessorsOrValue_Accessors {
                        getter: accessor(Some(seven)),
                        setter: accessor(Some(seven)),
                    },
                },
            },
            JSPropertySpec::ZERO,
        ];
        let obj = JS_NewPlainObject(cx);
        rooted!(&in(cx) let obj = obj);
        assert!(js::rust::wrappers2::JS_DefineProperties(
            cx,
            obj.handle(),
            specs.as_ptr()
        ));
        set_global(cx, global, c"o", ObjectValue(obj.get()));
        let v = eval(
            cx,
            global,
            "var d = Object.getOwnPropertyDescriptor(o, 'x'); [o.x, d.get.name, d.set.name].join()",
        );
        rooted!(&in(cx) let v = v);
        let s =
            js::conversions::jsstr_to_string(cx, std::ptr::NonNull::new(v.to_string()).unwrap());
        assert_eq!(s, "7,get x,set x");
        observe_field("accessor_names", Observed::String(s));
    });
}

fn js_define_properties(engine: &JSEngine) {
    use js::jsapi::{
        JSPropertySpec, JSPropertySpec_Accessor, JSPropertySpec_AccessorsOrValue,
        JSPropertySpec_AccessorsOrValue_Accessors, JSPropertySpec_Kind,
    };
    with_global(engine, |cx, global| unsafe {
        let accessor = |op| JSPropertySpec_Accessor {
            native: JSNativeWrapper {
                op,
                info: ptr::null(),
            },
        };
        let specs = [
            JSPropertySpec {
                name: JSPropertySpec_Name {
                    string_: c"answer".as_ptr(),
                },
                attributes_: JSPROP_ENUMERATE as u8,
                kind_: JSPropertySpec_Kind::NativeAccessor,
                u: JSPropertySpec_AccessorsOrValue {
                    accessors: JSPropertySpec_AccessorsOrValue_Accessors {
                        getter: accessor(Some(seven)),
                        setter: accessor(Some(seven)),
                    },
                },
            },
            JSPropertySpec::ZERO,
        ];
        rooted!(&in(cx) let obj = JS_NewPlainObject(cx));
        assert!(js::rust::wrappers2::JS_DefineProperties(
            cx,
            obj.handle(),
            specs.as_ptr()
        ));
        set_global(cx, global, c"props", ObjectValue(obj.get()));
        assert!(eval(cx, global, "props.answer === 7").to_boolean());
        assert!(
            eval(
                cx,
                global,
                "Object.getOwnPropertyDescriptor(props, 'answer').get.name === 'get answer'"
            )
            .to_boolean()
        );
        observe_field("properties_accessor_value", Observed::Number(7.0));
        observe_field(
            "properties_accessor_name",
            Observed::String("get answer".into()),
        );
    });
}

mod extra_contract {
    /* This Source Code Form is subject to the terms of the Mozilla Public
     * License, v. 2.0. If a copy of the MPL was not distributed with this
     * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

    #![allow(unsafe_op_in_unsafe_fn)]

    //! Object entry points that are mostly about identity, rooted vectors, iteration and the
    //! remaining property/collection helpers.

    use std::cell::Cell;
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};
    use std::sync::{LazyLock, Mutex};

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::gc::StackGCVector;
    use js::jsapi::{
        CompilationType, GetStaticPrototype, Handle, HandleString, HandleValue,
        JS_GetFunctionArity, JS_IsGlobalObject, JSContext as RawJSContext, JSFunction,
        JSJitGetterCallArgs, JSJitInfo, JSJitSetterCallArgs, JSObject, JSPROP_ENUMERATE,
        JSPROP_PERMANENT, JSPROP_READONLY, JSSecurityCallbacks, JSString, ObjectOpResult,
        PropertyDescriptor, RuntimeCode, Value, jsid,
    };
    use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
    use js::rooted;
    use js::rust::wrappers2::{
        Construct1, DefineFunctionWithReserved, FromPropertyDescriptor, GetCurrentRealmOrNull,
        GetFunctionRealm, GetObjectProto, GetRealmErrorPrototype, GetRealmFunctionPrototype,
        GetRealmIteratorPrototype, GetRealmObjectPrototype, IsArgumentsObject, IsMapObject,
        IsSetObject, JS_DefineProperty, JS_DefineProperty2, JS_DefineUCProperty2,
        JS_ForwardGetPropertyTo, JS_ForwardSetPropertyTo, JS_GetFunctionDisplayId,
        JS_InitializePropertiesFromCompatibleNativeObject, JS_LinkConstructorAndPrototype,
        JS_NewObjectWithoutMetadata, JS_NewPlainObject, JS_SetImmutablePrototype,
        JS_SetSecurityCallbacks, JS_ValueToFunction, NewMapObject, NewSetObject,
        SetDataPropertyDescriptor, SetPropertyIgnoringNamedGetter,
    };
    use js::rust::{
        CompileOptionsWrapper, HandleObject, JSEngine, RootedObjectVectorWrapper, Runtime,
        SIMPLE_GLOBAL_CLASS, evaluate_script, for_of,
    };

    use crate::support::{Observation, Value as Observed, eval, record_case, set_global};

    const ATTRS: u32 = (JSPROP_ENUMERATE | JSPROP_PERMANENT) as u32;

    fn string_ptr_of(cx: &mut JSContext, string: *mut JSString) -> String {
        unsafe { jsstr_to_string(cx, NonNull::new(string).expect("non-null JS string")) }
    }

    fn string_of(cx: &mut JSContext, value: js::jsval::JSVal) -> String {
        assert!(value.is_string(), "expected a string value");
        string_ptr_of(cx, value.to_string())
    }

    fn object_of(cx: &mut JSContext, global: HandleObject, source: &str) -> *mut JSObject {
        eval(cx, global, source).to_object()
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

    fn rooted_vectors(engine: &JSEngine) {
        record_case(
            engine,
            "objects.rooted_vectors",
            &[
                "AppendToIdVector",
                "AppendToRootedObjectVector",
                "CreateRootedIdVector",
                "CreateRootedObjectVector",
                "DeleteRootedObjectVector",
                "DestroyRootedIdVector",
                "GetIdVectorAddress",
                "GetObjectVectorAddress",
                "SliceRootedIdVector",
            ],
            |cx, global| unsafe {
                let raw_ids = js::glue::CreateRootedIdVector(cx.raw_cx());
                assert!(
                    !raw_ids.is_null(),
                    "CreateRootedIdVector must allocate a vector"
                );
                let raw_id_address = js::glue::GetIdVectorAddress(raw_ids);
                assert!(
                    !raw_id_address.is_null(),
                    "GetIdVectorAddress must expose the vector"
                );
                let mut raw_len = usize::MAX;
                let raw_slice = js::glue::SliceRootedIdVector(raw_ids, &mut raw_len);
                assert_eq!(raw_len, 0, "a fresh rooted id vector must be empty");
                assert!(!raw_slice.is_null());
                js::glue::DestroyRootedIdVector(raw_ids);

                let raw_objects = js::glue::CreateRootedObjectVector(cx.raw_cx());
                assert!(
                    !raw_objects.is_null(),
                    "CreateRootedObjectVector must allocate a vector"
                );
                assert!(
                    !js::glue::GetObjectVectorAddress(raw_objects).is_null(),
                    "GetObjectVectorAddress must expose the vector"
                );
                js::glue::DeleteRootedObjectVector(raw_objects);

                let (id_len, id_is_string, id_text) = {
                    let mut ids = js::rust::IdVector::new(cx);
                    rooted!(&in(cx) let id = id_for(cx, global, "'rooted-id'"));
                    assert!(js::rust::wrappers2::AppendToIdVector(
                        ids.handle_mut(),
                        id.handle()
                    ));
                    assert_eq!(ids.len(), 1);
                    assert!(ids[0].is_string());
                    let text = string_ptr_of(cx, ids[0].to_string());
                    assert_eq!(text, "rooted-id");
                    (ids.len(), ids[0].is_string(), text)
                };

                let (object_count, object_handle_non_null) = {
                    let objects = RootedObjectVectorWrapper::new(cx);
                    let first = object_of(cx, global, "({ first: 1 })");
                    let second = object_of(cx, global, "({ second: 2 })");
                    assert!(objects.append(first));
                    assert!(!objects.handle().ptr.is_null());
                    assert!(objects.append(second));
                    (2u32, !objects.handle().ptr.is_null())
                };

                Observation::new(vec![
                    ("id_len", Observed::Number(id_len as f64)),
                    ("id_is_string", Observed::Boolean(id_is_string)),
                    ("id_text", Observed::String(id_text)),
                    ("object_count", Observed::Number(object_count as f64)),
                    (
                        "object_handle_non_null",
                        Observed::Boolean(object_handle_non_null),
                    ),
                ])
            },
        );
    }

    fn identity_and_functions(engine: &JSEngine) {
        record_case(
            engine,
            "objects.identity_and_functions",
            &[
                "Construct1",
                "DefineFunctionWithReserved",
                "GetFunctionRealm",
                "GetObjectProto",
                "GetRealmErrorPrototype",
                "GetRealmFunctionPrototype",
                "GetRealmIteratorPrototype",
                "GetStaticPrototype",
                "IsArgumentsObject",
                "JS_GetFunctionArity",
                "JS_GetFunctionDisplayId",
                "JS_InitializePropertiesFromCompatibleNativeObject",
                "JS_IsGlobalObject",
                "JS_LinkConstructorAndPrototype",
                "JS_NewObjectWithoutMetadata",
                "JS_SetImmutablePrototype",
                "JS_ValueToFunction",
                "SetFunctionNativeReserved",
                "GetFunctionNativeReserved",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let proto = GetRealmObjectPrototype(cx));
                rooted!(&in(cx) let mut actual_proto = ptr::null_mut::<JSObject>());
                assert!(GetObjectProto(cx, global, actual_proto.handle_mut()));
                assert_eq!(actual_proto.get(), proto.get());
                rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
                assert_eq!(GetStaticPrototype(plain.get()), proto.get());
                assert!(JS_IsGlobalObject(global.get()));
                assert!(!JS_IsGlobalObject(plain.get()));

                rooted!(&in(cx) let function_proto = GetRealmFunctionPrototype(cx));
                rooted!(&in(cx) let error_proto = GetRealmErrorPrototype(cx));
                rooted!(&in(cx) let iterator_proto = GetRealmIteratorPrototype(cx));
                assert_eq!(
                    function_proto.get(),
                    object_of(cx, global, "Function.prototype")
                );
                assert_eq!(error_proto.get(), object_of(cx, global, "Error.prototype"));
                assert_eq!(
                    iterator_proto.get(),
                    object_of(
                        cx,
                        global,
                        "Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))"
                    )
                );

                let arguments = object_of(cx, global, "(function () { return arguments; })()");
                assert!(IsArgumentsObject(
                    js::rust::Handle::from_marked_location(&arguments).into()
                ));
                assert!(!IsArgumentsObject(plain.handle().into()));
                let map = NewMapObject(cx);
                let set = NewSetObject(cx);
                let mut is_map = false;
                assert!(IsMapObject(
                    cx,
                    js::rust::Handle::from_marked_location(&map).into(),
                    &mut is_map
                ));
                assert!(is_map);
                let mut is_set = false;
                assert!(IsSetObject(
                    cx,
                    js::rust::Handle::from_marked_location(&set).into(),
                    &mut is_set
                ));
                assert!(is_set);

                rooted!(&in(cx) let ctor = object_of(cx, global, "(function C(v) { this.v = v; })"));
                rooted!(&in(cx) let ctor_value = ObjectValue(ctor.get()));
                rooted!(&in(cx) let args = vec![Int32Value(31)]);
                let args = js::jsapi::HandleValueArray::from(&args);
                rooted!(&in(cx) let mut instance = ptr::null_mut::<JSObject>());
                assert!(Construct1(
                    cx,
                    ctor_value.handle(),
                    &args,
                    instance.handle_mut()
                ));
                set_global(cx, global, c"instance", ObjectValue(instance.get()));
                set_global(cx, global, c"C", ctor_value.get());
                let instance_value = eval(cx, global, "instance.v").to_int32();
                let instance_of_ctor = eval(cx, global, "instance instanceof C").to_boolean();
                assert_eq!(instance_value, 31);
                assert!(instance_of_ctor);

                assert_eq!(
                    JS_ValueToFunction(cx, ctor_value.handle().into()),
                    ctor.get().cast::<JSFunction>()
                );
                assert_eq!(JS_GetFunctionArity(ctor.get().cast::<JSFunction>()), 1);
                rooted!(&in(cx) let mut display = ptr::null_mut::<JSString>());
                rooted!(&in(cx) let constructor = ctor.get().cast::<JSFunction>());
                assert!(JS_GetFunctionDisplayId(
                    cx,
                    constructor.handle(),
                    display.handle_mut(),
                ));
                assert_eq!(string_ptr_of(cx, display.get()), "C");
                let realm = GetFunctionRealm(cx, ctor.handle().into());
                assert!(!realm.is_null());
                assert_eq!(realm, GetCurrentRealmOrNull(cx));
                assert!(JS_LinkConstructorAndPrototype(
                    cx,
                    ctor.handle(),
                    plain.handle()
                ));
                set_global(cx, global, c"linked_ctor", ObjectValue(ctor.get()));
                set_global(cx, global, c"linked_proto", ObjectValue(plain.get()));
                assert!(eval(
                cx,
                global,
                "linked_ctor.prototype === linked_proto && linked_proto.constructor === linked_ctor"
            )
            .to_boolean());

                rooted!(&in(cx) let without_metadata =
                JS_NewObjectWithoutMetadata(cx, ptr::null(), proto.handle()));
                rooted!(&in(cx) let mut copied = JS_NewObjectWithoutMetadata(cx, ptr::null(), proto.handle()));
                let uc_name: Vec<u16> = "copied".encode_utf16().collect();
                rooted!(&in(cx) let copied_value = Int32Value(55));
                assert!(JS_DefineUCProperty2(
                    cx,
                    without_metadata.handle(),
                    uc_name.as_ptr(),
                    uc_name.len(),
                    copied_value.handle(),
                    ATTRS,
                ));
                assert!(JS_InitializePropertiesFromCompatibleNativeObject(
                    cx,
                    copied.handle(),
                    without_metadata.handle(),
                ));
                set_global(cx, global, c"copied", ObjectValue(copied.get()));
                assert!(eval(cx, global, "copied.copied === 55").to_boolean());

                rooted!(&in(cx) let immutable = JS_NewPlainObject(cx));
                let mut succeeded = false;
                assert!(JS_SetImmutablePrototype(
                    cx,
                    immutable.handle(),
                    &mut succeeded
                ));
                assert!(succeeded);
                set_global(cx, global, c"immutable", ObjectValue(immutable.get()));
                assert!(eval(
                cx,
                global,
                "Reflect.setPrototypeOf(immutable, null) === false && Object.getPrototypeOf(immutable) !== null"
            )
            .to_boolean());

                let native = DefineFunctionWithReserved(
                    cx,
                    plain.get(),
                    c"reservedNative".as_ptr(),
                    Some(read_reserved),
                    2,
                    JSPROP_ENUMERATE as u32,
                );
                assert!(!native.is_null());
                js::jsapi::SetFunctionNativeReserved(native.cast(), 0, &Int32Value(73));
                set_global(cx, global, c"reservedNative", ObjectValue(native.cast()));
                let reserved_result = eval(cx, global, "reservedNative() === 73").to_boolean();
                assert!(reserved_result);

                Observation::new(vec![
                    ("global_is_global", Observed::Boolean(true)),
                    ("plain_is_not_global", Observed::Boolean(true)),
                    ("global_proto_matches", Observed::Boolean(true)),
                    ("map_is_map", Observed::Boolean(is_map)),
                    ("set_is_set", Observed::Boolean(is_set)),
                    ("instance_value", Observed::Number(instance_value as f64)),
                    ("instance_of_ctor", Observed::Boolean(instance_of_ctor)),
                    (
                        "function_arity",
                        Observed::Number(
                            JS_GetFunctionArity(ctor.get().cast::<JSFunction>()) as f64
                        ),
                    ),
                    (
                        "display_name",
                        Observed::String(string_ptr_of(cx, display.get())),
                    ),
                    ("immutable_prototype", Observed::Boolean(succeeded)),
                    ("reserved_native_result", Observed::Boolean(reserved_result)),
                    (
                        "copied_property",
                        Observed::Number(eval(cx, global, "copied.copied").to_int32() as f64),
                    ),
                ])
            },
        );
    }

    unsafe extern "C" fn read_reserved(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
        let args = js::jsapi::CallArgs::from_vp(vp, argc);
        args.rval()
            .set(*js::jsapi::GetFunctionNativeReserved(args.callee(), 0));
        true
    }

    fn descriptors_and_forwarding(engine: &JSEngine) {
        record_case(
            engine,
            "objects.descriptors_and_forwarding",
            &[
                "FromPropertyDescriptor",
                "JS_DefineProperty2",
                "JS_DefineUCProperty2",
                "JS_ForwardGetPropertyTo",
                "JS_ForwardSetPropertyTo",
                "JS_NewPlainObject",
                "JS_SetProperty",
                "SetDataPropertyDescriptor",
                "SetPropertyIgnoringNamedGetter",
                "JS_ValueToId",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
                rooted!(&in(cx) let value = Int32Value(90));
                SetDataPropertyDescriptor(desc.handle_mut(), value.handle(), ATTRS);
                assert!(desc.hasValue_() && desc.hasWritable_() && desc.hasEnumerable_());
                assert!(desc.writable_() && desc.enumerable_() && !desc.configurable_());
                rooted!(&in(cx) let mut desc_object = UndefinedValue());
                assert!(FromPropertyDescriptor(
                    cx,
                    desc.handle(),
                    desc_object.handle_mut()
                ));
                set_global(cx, global, c"desc_object", desc_object.get());
                let descriptor_matches = eval(
                cx,
                global,
                "desc_object.value === 90 && desc_object.writable === true && desc_object.enumerable === true && desc_object.configurable === false"
            )
            .to_boolean();
                assert!(descriptor_matches);

                rooted!(&in(cx) let target = JS_NewPlainObject(cx));
                rooted!(&in(cx) let receiver = JS_NewPlainObject(cx));
                let uc_name: Vec<u16> = "unicode".encode_utf16().collect();
                rooted!(&in(cx) let uc_value = Int32Value(12));
                assert!(JS_DefineUCProperty2(
                    cx,
                    target.handle(),
                    uc_name.as_ptr(),
                    uc_name.len(),
                    uc_value.handle(),
                    ATTRS,
                ));
                set_global(cx, global, c"target", ObjectValue(target.get()));
                set_global(cx, global, c"receiver", ObjectValue(receiver.get()));
                let unicode_defined = eval(cx, global, "target.unicode === 12").to_boolean();
                assert!(unicode_defined);

                rooted!(&in(cx) let getter = object_of(
                    cx,
                    global,
                    "(function () { return this.marker; })",
                ));
                rooted!(&in(cx) let setter = object_of(
                    cx,
                    global,
                    "(function (v) { this.seen = v; })",
                ));
                assert!(JS_DefineProperty2(
                    cx,
                    target.handle(),
                    c"x".as_ptr(),
                    getter.handle(),
                    setter.handle(),
                    JSPROP_ENUMERATE as u32,
                ));
                rooted!(&in(cx) let marker = Int32Value(17));
                assert!(JS_DefineProperty(
                    cx,
                    receiver.handle(),
                    c"marker".as_ptr(),
                    marker.handle(),
                    ATTRS,
                ));
                rooted!(&in(cx) let x_id = id_for(cx, global, "'x'"));
                rooted!(&in(cx) let receiver_value = ObjectValue(receiver.get()));
                rooted!(&in(cx) let mut forwarded = UndefinedValue());
                assert!(JS_ForwardGetPropertyTo(
                    cx,
                    target.handle(),
                    x_id.handle(),
                    receiver_value.handle(),
                    forwarded.handle_mut(),
                ));
                assert_eq!(forwarded.get().to_int32(), 17);
                rooted!(&in(cx) let twenty_three = Int32Value(23));
                let mut result = ObjectOpResult::default();
                assert!(JS_ForwardSetPropertyTo(
                    cx,
                    target.handle(),
                    x_id.handle(),
                    twenty_three.handle(),
                    receiver_value.handle(),
                    &mut result,
                ));
                assert!(result.ok());
                let forward_set_seen = eval(cx, global, "receiver.seen === 23").to_boolean();
                assert!(forward_set_seen);

                rooted!(&in(cx) let ignored = JS_NewPlainObject(cx));
                rooted!(&in(cx) let ignored_value = Int32Value(5));
                assert!(JS_DefineProperty(
                    cx,
                    ignored.handle(),
                    c"readonly".as_ptr(),
                    ignored_value.handle(),
                    (JSPROP_PERMANENT | JSPROP_READONLY) as u32,
                ));
                let readonly_id = id_for(cx, global, "'readonly'");
                rooted!(&in(cx) let readonly_id = readonly_id);
                rooted!(&in(cx) let nine = Int32Value(9));
                rooted!(&in(cx) let ignored_value_root = ObjectValue(ignored.get()));
                let mut own_desc = PropertyDescriptor::default();
                own_desc.set_hasValue_(true);
                own_desc.value_ = Int32Value(5);
                own_desc.set_hasWritable_(true);
                own_desc.set_writable_(false);
                rooted!(&in(cx) let own_desc = own_desc);
                let mut ignored_result = ObjectOpResult::default();
                assert!(SetPropertyIgnoringNamedGetter(
                    cx,
                    ignored.handle(),
                    readonly_id.handle(),
                    nine.handle(),
                    ignored_value_root.handle(),
                    Some(own_desc.handle()),
                    &mut ignored_result,
                ));
                assert!(!ignored_result.ok());

                Observation::new(vec![
                    ("descriptor_value", Observed::Number(90.0)),
                    ("descriptor_writable", Observed::Boolean(desc.writable_())),
                    (
                        "descriptor_enumerable",
                        Observed::Boolean(desc.enumerable_()),
                    ),
                    (
                        "descriptor_configurable",
                        Observed::Boolean(desc.configurable_()),
                    ),
                    (
                        "descriptor_roundtrip",
                        Observed::Boolean(descriptor_matches),
                    ),
                    ("unicode_defined", Observed::Boolean(unicode_defined)),
                    (
                        "forward_get_value",
                        Observed::Number(forwarded.get().to_int32() as f64),
                    ),
                    ("forward_set_ok", Observed::Boolean(result.ok())),
                    ("forward_set_seen", Observed::Boolean(forward_set_seen)),
                    ("readonly_rejected", Observed::Boolean(!ignored_result.ok())),
                ])
            },
        );
    }

    fn jit_getters_and_setters(engine: &JSEngine) {
        record_case(
            engine,
            "objects.jit_accessors",
            &["CallJitGetterOp", "CallJitSetterOp"],
            |cx, global| unsafe {
                rooted!(&in(cx) let this = JS_NewPlainObject(cx));
                let mut info: JSJitInfo = std::mem::zeroed();
                info.__bindgen_anon_1.getter = Some(jit_getter);
                let mut getter_vp = [UndefinedValue(); 3];
                assert!(js::rust::wrappers2::CallJitGetterOp(
                    &info,
                    cx,
                    this.handle(),
                    0x1234 as *mut c_void,
                    0,
                    getter_vp.as_mut_ptr(),
                ));
                assert_eq!(getter_vp[0].to_int32(), 77);

                let mut info: JSJitInfo = std::mem::zeroed();
                info.__bindgen_anon_1.setter = Some(jit_setter);
                JIT_SETTER_VALUE.with(|cell| cell.set(0));
                let mut setter_vp = [UndefinedValue(); 3];
                setter_vp[2] = Int32Value(33);
                assert!(js::rust::wrappers2::CallJitSetterOp(
                    &info,
                    cx,
                    this.handle(),
                    0x5678 as *mut c_void,
                    1,
                    setter_vp.as_mut_ptr(),
                ));
                assert_eq!(JIT_SETTER_VALUE.with(Cell::get), 33);
                let _ = global;
                Observation::new(vec![
                    (
                        "getter_value",
                        Observed::Number(getter_vp[0].to_int32() as f64),
                    ),
                    (
                        "setter_value",
                        Observed::Number(JIT_SETTER_VALUE.with(Cell::get) as f64),
                    ),
                ])
            },
        );
    }

    thread_local! {
        static JIT_SETTER_VALUE: Cell<i32> = const { Cell::new(0) };
    }

    unsafe extern "C" fn jit_getter(
        _cx: *mut RawJSContext,
        _this: Handle<*mut JSObject>,
        specialized: *mut c_void,
        args: JSJitGetterCallArgs,
    ) -> bool {
        assert_eq!(specialized as usize, 0x1234);
        args.rval().set(Int32Value(77));
        true
    }

    unsafe extern "C" fn jit_setter(
        _cx: *mut RawJSContext,
        _this: Handle<*mut JSObject>,
        specialized: *mut c_void,
        args: JSJitSetterCallArgs,
    ) -> bool {
        assert_eq!(specialized as usize, 0x5678);
        JIT_SETTER_VALUE.with(|cell| cell.set(args.get(0).to_int32()));
        true
    }

    static SECURITY_CALLBACKS: JSSecurityCallbacks = JSSecurityCallbacks {
        contentSecurityPolicyAllows: Some(content_security_policy_allows),
        codeForEvalGets: None,
        subsumes: None,
    };

    static RAN_CSP_CALLBACK: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

    unsafe extern "C" fn content_security_policy_allows(
        cx: *mut RawJSContext,
        _runtime_code: RuntimeCode,
        _code_string: HandleString,
        _compilation_type: CompilationType,
        parameter_strings: Handle<StackGCVector<*mut JSString>>,
        _body_string: HandleString,
        parameter_args: Handle<StackGCVector<js::jsval::JSVal>>,
        _body_arg: HandleValue,
        can_compile_strings: *mut bool,
    ) -> bool {
        let mut cx = unsafe { JSContext::from_ptr(NonNull::new(cx).unwrap()) };
        let cx = &mut cx;
        let parameter_strings = js::rust::Handle::from_raw(parameter_strings);
        assert_eq!(parameter_strings.len(), 2);
        assert_eq!(
            string_ptr_of(cx, parameter_strings.at(0).unwrap().get()),
            "a"
        );
        assert_eq!(
            string_ptr_of(cx, parameter_strings.at(1).unwrap().get()),
            "b"
        );
        let parameter_args = js::rust::Handle::from_raw(parameter_args);
        assert_eq!(parameter_args.len(), 2);
        assert_eq!(string_of(cx, parameter_args.at(0).unwrap().get()), "a");
        assert_eq!(string_of(cx, parameter_args.at(1).unwrap().get()), "b");
        *RAN_CSP_CALLBACK.lock().unwrap() = true;
        *can_compile_strings = true;
        true
    }

    fn stack_gc_vectors_and_for_of(engine: &JSEngine) {
        let (csp_ran, for_of_sum, for_of_rejected) =
            with_global_with_callbacks(engine, |cx, global| unsafe {
                let source = "Function(\"a\", \"b\", \"return a + b\")";
                rooted!(&in(cx) let mut result = UndefinedValue());
                let options = CompileOptionsWrapper::new(cx, c"csp.js".to_owned(), 1);
                assert!(evaluate_script(cx, global, source, result.handle_mut(), options).is_ok());
                assert!(*RAN_CSP_CALLBACK.lock().unwrap());

                let array = eval(cx, global, "[1, 2, 3]");
                rooted!(&in(cx) let array = array.to_object());
                let value = ObjectValue(array.get());
                rooted!(&in(cx) let value = value);
                let mut sum = 0;
                assert!(
                    for_of::<_, _, ()>(cx, value.handle(), |_, item| {
                        sum += item.get().to_int32();
                        Ok(std::ops::ControlFlow::Continue(()))
                    })
                    .is_ok()
                );
                assert_eq!(sum, 6);
                rooted!(&in(cx) let not_iterable = Int32Value(1));
                let for_of_rejected = for_of::<_, _, ()>(cx, not_iterable.handle(), |_, _| {
                    Ok(std::ops::ControlFlow::Continue(()))
                })
                .is_err();
                assert!(for_of_rejected);
                let _ = SetDataPropertyDescriptor;
                (*RAN_CSP_CALLBACK.lock().unwrap(), sum, for_of_rejected)
            });
        crate::support::record(
            "objects.stack_vectors_for_of",
            &Observation::new(vec![
                ("csp_callback_ran", Observed::Boolean(csp_ran)),
                ("for_of_sum", Observed::Number(for_of_sum as f64)),
                ("non_iterable_rejected", Observed::Boolean(for_of_rejected)),
            ]),
            &[
                "JS_ForOfIteratorInit",
                "JS_ForOfIteratorNext",
                "JS_ForOfIteratorValueIsIterable",
                "StackGCVectorStringAtIndex",
                "StackGCVectorStringLength",
                "StackGCVectorValueAtIndex",
                "StackGCVectorValueLength",
            ],
        );
    }

    fn with_global_with_callbacks<R>(
        engine: &JSEngine,
        f: impl FnOnce(&mut JSContext, HandleObject) -> R,
    ) -> R {
        let mut runtime = Runtime::new(engine.handle());
        let cx = runtime.cx();
        unsafe {
            JS_SetSecurityCallbacks(cx, &SECURITY_CALLBACKS);
            rooted!(&in(cx) let global = js::rust::wrappers2::JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                js::jsapi::OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*js::rust::RealmOptions::default(),
            ));
            let mut realm = js::realm::AutoRealm::new_from_handle(cx, global.handle());
            f(&mut realm, global.handle())
        }
    }

    mod collections_contract {
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
        use std::{ptr, slice};

        use js::context::JSContext;
        use js::conversions::jsstr_to_string;
        use js::jsapi::{
            ClippedTime, JS_ClearPendingException, JS_IsExceptionPending, JSObject,
            RegExpFlag_Global, RegExpFlag_UnicodeSets, RegExpFlags, jsid,
        };
        use js::jsval::{DoubleValue, Int32Value, JSVal, ObjectValue, UndefinedValue};
        use js::rooted;
        use js::rust::wrappers2::{
            CheckRegExpSyntax, DateGetMsecSinceEpoch, ExecuteRegExpNoStatics, GetPropertyKeys,
            GetWeakMapEntry, JS_GetElement, JS_GetProperty, JS_GetPropertyById, JS_IdToValue,
            JS_IndexToId, JS_NewPlainObject, JS_SetElement, JS_SetProperty, JS_Stringify,
            JS_ValueToSource, MapClear, MapDelete, MapEntries, MapForEach, MapGet, MapHas, MapKeys,
            MapSet, MapSize, MapValues, NewDateObject, NewMapObject, NewSetObject,
            NewUCRegExpObject, NewWeakMapObject, ObjectIsDate, ObjectIsRegExp, SameValue, SetAdd,
            SetClear, SetDelete, SetEntries, SetHas, SetKeys, SetSize, SetValues, SetWeakMapEntry,
            StrictlyEqual,
        };
        use js::rust::{HandleObject, HandleValue, IdVector, JSEngine};

        use crate::support::{Observation, Value, eval, record_case, set_global};

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

        unsafe fn value_source(cx: &mut JSContext, value: JSVal) -> Option<String> {
            rooted!(&in(cx) let value = value);
            let string = JS_ValueToSource(cx, value.handle());
            if string.is_null() {
                None
            } else {
                Some(jsstr_to_string(
                    cx,
                    std::ptr::NonNull::new(string).expect("non-null source string"),
                ))
            }
        }

        unsafe fn id_name(cx: &mut JSContext, id: jsid) -> Value {
            rooted!(&in(cx) let mut value = UndefinedValue());
            assert!(JS_IdToValue(cx, id, value.handle_mut()));
            if id.is_symbol() {
                Value::String("symbol".to_owned())
            } else if value.get().is_number() {
                number(value.get().to_number())
            } else if value.get().is_string() {
                let string = value.get().to_string();
                Value::String(jsstr_to_string(
                    cx,
                    std::ptr::NonNull::new(string).expect("non-null string id"),
                ))
            } else {
                Value::String("other".to_owned())
            }
        }

        unsafe fn key_names(cx: &mut JSContext, object: *mut JSObject, flags: u32) -> Value {
            rooted!(&in(cx) let object = object);
            let mut ids = IdVector::new(cx);
            assert!(GetPropertyKeys(
                cx,
                object.handle(),
                flags,
                ids.handle_mut()
            ));
            let mut names = Vec::new();
            for index in 0..ids.len() {
                names.push(id_name(cx, ids[index]));
            }
            Value::Array(names)
        }

        fn collection_map(engine: &JSEngine) {
            record_case(
                engine,
                "edge.collections_map",
                &[
                    "NewMapObject",
                    "MapSet",
                    "MapGet",
                    "MapHas",
                    "MapDelete",
                    "MapSize",
                    "MapKeys",
                    "MapValues",
                    "MapEntries",
                    "MapForEach",
                    "MapClear",
                ],
                |cx, global| unsafe {
                    rooted!(&in(cx) let map = NewMapObject(cx));
                    rooted!(&in(cx) let nan = DoubleValue(f64::NAN));
                    rooted!(&in(cx) let zero = DoubleValue(-0.0));
                    rooted!(&in(cx) let positive_zero = DoubleValue(0.0));
                    rooted!(&in(cx) let missing = Int32Value(99));
                    rooted!(&in(cx) let one = Int32Value(1));
                    rooted!(&in(cx) let two = Int32Value(2));
                    rooted!(&in(cx) let three = Int32Value(3));
                    rooted!(&in(cx) let mut out = UndefinedValue());
                    let mut has = false;
                    let mut deleted = false;

                    assert!(MapSet(cx, map.handle(), nan.handle(), one.handle()));
                    assert!(MapSet(cx, map.handle(), zero.handle(), two.handle()));
                    let size_after_insert = MapSize(cx, map.handle());

                    assert!(MapHas(cx, map.handle(), nan.handle(), &mut has));
                    let has_nan = has;
                    assert!(MapGet(cx, map.handle(), nan.handle(), out.handle_mut()));
                    let get_nan = out.get().to_int32();
                    assert!(MapHas(cx, map.handle(), positive_zero.handle(), &mut has));
                    let has_positive_zero = has;
                    assert!(MapGet(
                        cx,
                        map.handle(),
                        positive_zero.handle(),
                        out.handle_mut()
                    ));
                    let get_zero = out.get().to_int32();

                    assert!(MapDelete(cx, map.handle(), missing.handle(), &mut deleted));
                    let delete_missing = deleted;
                    let size_after_missing = MapSize(cx, map.handle());
                    assert!(MapDelete(
                        cx,
                        map.handle(),
                        positive_zero.handle(),
                        &mut deleted
                    ));
                    let delete_zero = deleted;
                    assert!(MapSet(cx, map.handle(), zero.handle(), three.handle()));
                    let size_after_reinsert = MapSize(cx, map.handle());

                    assert!(MapKeys(cx, map.handle(), out.handle_mut()));
                    set_global(cx, global, c"mapKeys", out.get());
                    let keys = eval(cx, global, "JSON.stringify(Array.from(mapKeys))").to_string();
                    let keys =
                        jsstr_to_string(cx, std::ptr::NonNull::new(keys).expect("keys string"));

                    assert!(MapValues(cx, map.handle(), out.handle_mut()));
                    set_global(cx, global, c"mapValues", out.get());
                    let values =
                        eval(cx, global, "JSON.stringify(Array.from(mapValues))").to_string();
                    let values =
                        jsstr_to_string(cx, std::ptr::NonNull::new(values).expect("values string"));

                    assert!(MapEntries(cx, map.handle(), out.handle_mut()));
                    set_global(cx, global, c"mapEntries", out.get());
                    let entries =
                        eval(cx, global, "JSON.stringify(Array.from(mapEntries))").to_string();
                    let entries = jsstr_to_string(
                        cx,
                        std::ptr::NonNull::new(entries).expect("entries string"),
                    );

                    set_global(cx, global, c"sum", Int32Value(0));
                    let callback = eval(cx, global, "(v) => { sum += v; }");
                    rooted!(&in(cx) let callback = callback);
                    assert!(MapForEach(
                        cx,
                        map.handle(),
                        callback.handle(),
                        HandleValue::null(),
                    ));
                    let foreach_sum = eval(cx, global, "sum").to_int32();
                    assert!(MapClear(&*cx, map.handle()));
                    let size_after_clear = MapSize(cx, map.handle());

                    Observation::new(vec![
                        ("size_after_insert", Value::Number(size_after_insert as f64)),
                        ("has_nan", Value::Boolean(has_nan)),
                        ("get_nan", Value::Number(get_nan as f64)),
                        ("has_positive_zero", Value::Boolean(has_positive_zero)),
                        ("get_zero", Value::Number(get_zero as f64)),
                        ("delete_missing", Value::Boolean(delete_missing)),
                        (
                            "size_after_missing",
                            Value::Number(size_after_missing as f64),
                        ),
                        ("delete_zero", Value::Boolean(delete_zero)),
                        (
                            "size_after_reinsert",
                            Value::Number(size_after_reinsert as f64),
                        ),
                        ("keys", Value::String(keys)),
                        ("values", Value::String(values)),
                        ("entries", Value::String(entries)),
                        ("foreach_sum", Value::Number(foreach_sum as f64)),
                        ("size_after_clear", Value::Number(size_after_clear as f64)),
                    ])
                },
            );
        }

        fn collection_set(engine: &JSEngine) {
            record_case(
                engine,
                "edge.collections_set",
                &[
                    "NewSetObject",
                    "SetAdd",
                    "SetHas",
                    "SetDelete",
                    "SetSize",
                    "SetKeys",
                    "SetValues",
                    "SetEntries",
                    "SetClear",
                    "MapSize",
                    "MapEntries",
                    "NewMapObject",
                ],
                |cx, global| unsafe {
                    rooted!(&in(cx) let set = NewSetObject(cx));
                    rooted!(&in(cx) let nan = DoubleValue(f64::NAN));
                    rooted!(&in(cx) let zero = DoubleValue(-0.0));
                    rooted!(&in(cx) let positive_zero = DoubleValue(0.0));
                    rooted!(&in(cx) let missing = Int32Value(5));
                    rooted!(&in(cx) let x = eval(cx, global, "'x'"));
                    let mut has = false;
                    let mut deleted = false;

                    assert!(SetAdd(cx, set.handle(), nan.handle()));
                    assert!(SetAdd(cx, set.handle(), zero.handle()));
                    let size_after_insert = SetSize(&*cx, set.handle());
                    assert!(SetHas(cx, set.handle(), nan.handle(), &mut has));
                    let has_nan = has;
                    assert!(SetHas(cx, set.handle(), positive_zero.handle(), &mut has));
                    let has_zero = has;
                    assert!(SetDelete(cx, set.handle(), missing.handle(), &mut deleted));
                    let delete_missing = deleted;
                    assert!(SetDelete(cx, set.handle(), nan.handle(), &mut deleted));
                    let delete_nan = deleted;
                    assert!(SetAdd(cx, set.handle(), x.handle()));
                    let size_after = SetSize(&*cx, set.handle());

                    rooted!(&in(cx) let mut out = UndefinedValue());
                    assert!(SetEntries(cx, set.handle(), out.handle_mut()));
                    set_global(cx, global, c"setEntries", out.get());
                    let entries =
                        eval(cx, global, "JSON.stringify(Array.from(setEntries))").to_string();
                    let entries = jsstr_to_string(
                        cx,
                        std::ptr::NonNull::new(entries).expect("set entries string"),
                    );

                    rooted!(&in(cx) let mut keys = UndefinedValue());
                    assert!(SetKeys(cx, set.handle(), keys.handle_mut()));
                    set_global(cx, global, c"setKeys", keys.get());
                    let keys_text =
                        eval(cx, global, "JSON.stringify(Array.from(setKeys))").to_string();
                    let keys_text = jsstr_to_string(
                        cx,
                        std::ptr::NonNull::new(keys_text).expect("set keys string"),
                    );

                    rooted!(&in(cx) let mut values = UndefinedValue());
                    assert!(SetValues(cx, set.handle(), values.handle_mut()));
                    set_global(cx, global, c"setValues", values.get());
                    let values_text =
                        eval(cx, global, "JSON.stringify(Array.from(setValues))").to_string();
                    let values_text = jsstr_to_string(
                        cx,
                        std::ptr::NonNull::new(values_text).expect("set values string"),
                    );

                    assert!(SetClear(&*cx, set.handle()));
                    let size_after_clear = SetSize(&*cx, set.handle());
                    assert!(SetHas(cx, set.handle(), zero.handle(), &mut has));
                    let has_after_clear = has;

                    // Keep an in-scope API in this case's declaration: MapSize and MapEntries are the
                    // collection anchors required by the coverage gate.
                    rooted!(&in(cx) let map = NewMapObject(cx));
                    rooted!(&in(cx) let key = Int32Value(1));
                    rooted!(&in(cx) let value = Int32Value(2));
                    assert!(MapSet(cx, map.handle(), key.handle(), value.handle()));
                    let map_size = MapSize(cx, map.handle());
                    rooted!(&in(cx) let mut map_entries = UndefinedValue());
                    assert!(MapEntries(cx, map.handle(), map_entries.handle_mut()));

                    Observation::new(vec![
                        ("size_after_insert", Value::Number(size_after_insert as f64)),
                        ("has_nan", Value::Boolean(has_nan)),
                        ("has_zero", Value::Boolean(has_zero)),
                        ("delete_missing", Value::Boolean(delete_missing)),
                        ("delete_nan", Value::Boolean(delete_nan)),
                        ("size_after", Value::Number(size_after as f64)),
                        ("entries", Value::String(entries)),
                        ("keys", Value::String(keys_text)),
                        ("values", Value::String(values_text)),
                        ("size_after_clear", Value::Number(size_after_clear as f64)),
                        ("has_after_clear", Value::Boolean(has_after_clear)),
                        ("map_size", Value::Number(map_size as f64)),
                    ])
                },
            );
        }

        fn weakmap(engine: &JSEngine) {
            record_case(
                engine,
                "edge.weakmap",
                &[
                    "NewWeakMapObject",
                    "SetWeakMapEntry",
                    "GetWeakMapEntry",
                    "JS_IsExceptionPending",
                    "JS_ClearPendingException",
                ],
                |cx, global| unsafe {
                    rooted!(&in(cx) let weak = NewWeakMapObject(cx));
                    rooted!(&in(cx) let key = ObjectValue(eval(cx, global, "({})").to_object()));
                    rooted!(&in(cx) let other = ObjectValue(eval(cx, global, "({})").to_object()));
                    rooted!(&in(cx) let one = Int32Value(1));
                    rooted!(&in(cx) let two = Int32Value(2));
                    rooted!(&in(cx) let mut out = UndefinedValue());

                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        out.handle_mut()
                    ));
                    let missing_ok = out.get().is_undefined();

                    assert!(SetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        one.handle()
                    ));
                    assert!(SetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        two.handle()
                    ));
                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        out.handle_mut()
                    ));
                    let overwritten = out.get().to_int32();
                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        other.handle(),
                        out.handle_mut()
                    ));
                    let other_missing = out.get().is_undefined();

                    rooted!(&in(cx) let primitive = Int32Value(7));
                    let primitive_set =
                        SetWeakMapEntry(cx, weak.handle(), primitive.handle(), one.handle());
                    assert!(!primitive_set, "a primitive WeakMap key must be rejected");
                    crate::support::assert_exception_named(cx, "TypeError");
                    let primitive_get_ok =
                        GetWeakMapEntry(cx, weak.handle(), primitive.handle(), out.handle_mut());
                    assert!(
                        primitive_get_ok,
                        "GetWeakMapEntry must accept a missing key"
                    );
                    assert!(
                        out.get().is_undefined(),
                        "a primitive WeakMap key has no entry"
                    );
                    assert!(!JS_IsExceptionPending(cx.raw_cx()));

                    Observation::new(vec![
                        ("missing_ok", Value::Boolean(missing_ok)),
                        ("overwritten", Value::Number(overwritten as f64)),
                        ("other_missing", Value::Boolean(other_missing)),
                        ("primitive_set", Value::Boolean(primitive_set)),
                        ("primitive_pending", Value::Boolean(true)),
                        ("primitive_get_ok", Value::Boolean(primitive_get_ok)),
                        ("primitive_get_pending", Value::Boolean(false)),
                    ])
                },
            );
        }

        fn weakmap_overwrite_only(engine: &JSEngine) {
            record_case(
                engine,
                "edge.weakmap_overwrite",
                &["NewWeakMapObject", "SetWeakMapEntry", "GetWeakMapEntry"],
                |cx, global| unsafe {
                    rooted!(&in(cx) let weak = NewWeakMapObject(cx));
                    rooted!(&in(cx) let key = ObjectValue(eval(cx, global, "({})").to_object()));
                    rooted!(&in(cx) let other = ObjectValue(eval(cx, global, "({})").to_object()));
                    rooted!(&in(cx) let one = Int32Value(1));
                    rooted!(&in(cx) let two = Int32Value(2));
                    rooted!(&in(cx) let mut out = UndefinedValue());

                    assert!(SetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        one.handle()
                    ));
                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        out.handle_mut()
                    ));
                    let first = out.get().to_int32();
                    assert!(SetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        two.handle()
                    ));
                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        out.handle_mut()
                    ));
                    let second = out.get().to_int32();
                    // A distinct object key must not alias the first key.
                    assert!(SetWeakMapEntry(
                        cx,
                        weak.handle(),
                        other.handle(),
                        one.handle()
                    ));
                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        key.handle(),
                        out.handle_mut()
                    ));
                    let first_after_other = out.get().to_int32();
                    assert!(GetWeakMapEntry(
                        cx,
                        weak.handle(),
                        other.handle(),
                        out.handle_mut()
                    ));
                    let other_value = out.get().to_int32();

                    Observation::new(vec![
                        ("first", Value::Number(first as f64)),
                        ("second", Value::Number(second as f64)),
                        ("first_after_other", Value::Number(first_after_other as f64)),
                        ("other", Value::Number(other_value as f64)),
                    ])
                },
            );
        }

        fn dates(engine: &JSEngine) {
            record_case(
                engine,
                "edge.dates",
                &["NewDateObject", "DateGetMsecSinceEpoch", "ObjectIsDate"],
                |cx, global| unsafe {
                    rooted!(&in(cx) let invalid = NewDateObject(cx, ClippedTime { t: f64::NAN }));
                    let mut invalid_ms = 0.0;
                    let invalid_ok = DateGetMsecSinceEpoch(cx, invalid.handle(), &mut invalid_ms);
                    let mut invalid_is_date = false;
                    assert!(ObjectIsDate(cx, invalid.handle(), &mut invalid_is_date));

                    rooted!(&in(cx) let zero = NewDateObject(cx, ClippedTime { t: 0.0 }));
                    let mut zero_ms = 1.0;
                    let zero_ok = DateGetMsecSinceEpoch(cx, zero.handle(), &mut zero_ms);

                    let boundary = eval(cx, global, "new Date(8640000000000000)").to_object();
                    rooted!(&in(cx) let boundary = boundary);
                    let mut boundary_ms = 0.0;
                    let boundary_ok =
                        DateGetMsecSinceEpoch(cx, boundary.handle(), &mut boundary_ms);

                    let beyond = eval(cx, global, "new Date(8640000000000001)").to_object();
                    rooted!(&in(cx) let beyond = beyond);
                    let mut beyond_ms = 0.0;
                    let beyond_ok = DateGetMsecSinceEpoch(cx, beyond.handle(), &mut beyond_ms);

                    Observation::new(vec![
                        ("invalid_ok", Value::Boolean(invalid_ok)),
                        ("invalid_ms", number(invalid_ms)),
                        ("invalid_is_date", Value::Boolean(invalid_is_date)),
                        ("zero_ok", Value::Boolean(zero_ok)),
                        ("zero_ms", number(zero_ms)),
                        ("boundary_ok", Value::Boolean(boundary_ok)),
                        ("boundary_ms", number(boundary_ms)),
                        ("beyond_ok", Value::Boolean(beyond_ok)),
                        ("beyond_ms", number(beyond_ms)),
                    ])
                },
            );
        }

        fn regexp_exec(engine: &JSEngine) {
            record_case(
                engine,
                "edge.regexp_exec",
                &[
                    "NewUCRegExpObject",
                    "ObjectIsRegExp",
                    "ExecuteRegExpNoStatics",
                    "JS_GetProperty",
                    "JS_SetProperty",
                    "JS_IsExceptionPending",
                    "JS_ClearPendingException",
                ],
                |cx, global| unsafe {
                    let flags = RegExpFlags {
                        flags_: RegExpFlag_Global | RegExpFlag_UnicodeSets,
                    };
                    let pattern: Vec<u16> = "(?<n>a)b".encode_utf16().collect();
                    rooted!(&in(cx) let re =
                NewUCRegExpObject(cx, pattern.as_ptr(), pattern.len(), flags));
                    assert!(!re.get().is_null());
                    let mut is_regexp = false;
                    assert!(ObjectIsRegExp(cx, re.handle(), &mut is_regexp));
                    set_global(cx, global, c"re", ObjectValue(re.get()));

                    let last_index_before =
                        eval(cx, global, "re.lastIndex = 2; re.lastIndex").to_int32();
                    let input: Vec<u16> = "zab".encode_utf16().collect();
                    rooted!(&in(cx) let mut match_value = UndefinedValue());
                    let mut index = 0;
                    let matched = ExecuteRegExpNoStatics(
                        cx,
                        re.handle(),
                        input.as_ptr(),
                        input.len(),
                        &mut index,
                        false,
                        match_value.handle_mut(),
                    );
                    let last_index_after = eval(cx, global, "re.lastIndex").to_int32();
                    set_global(cx, global, c"matchValue", match_value.get());
                    let match_text = eval(
                cx,
                global,
                "JSON.stringify({match: matchValue && matchValue[0], index: matchValue && matchValue.index, group: matchValue && matchValue.groups && matchValue.groups.n})",
            )
            .to_string();
                    let match_text = jsstr_to_string(
                        cx,
                        std::ptr::NonNull::new(match_text).expect("match string"),
                    );

                    rooted!(&in(cx) let mut test_value = UndefinedValue());
                    let mut test_index = 0;
                    let test_ok = ExecuteRegExpNoStatics(
                        cx,
                        re.handle(),
                        input.as_ptr(),
                        input.len(),
                        &mut test_index,
                        true,
                        test_value.handle_mut(),
                    );
                    let test_is_boolean = test_value.get().is_boolean();
                    let test_result = test_value.get().to_boolean();

                    let no_match_input: Vec<u16> = "zzz".encode_utf16().collect();
                    rooted!(&in(cx) let mut no_match = UndefinedValue());
                    let mut no_match_index = 0;
                    let no_match_ok = ExecuteRegExpNoStatics(
                        cx,
                        re.handle(),
                        no_match_input.as_ptr(),
                        no_match_input.len(),
                        &mut no_match_index,
                        false,
                        no_match.handle_mut(),
                    );

                    Observation::new(vec![
                        ("is_regexp", Value::Boolean(is_regexp)),
                        ("last_index_before", Value::Number(last_index_before as f64)),
                        ("matched", Value::Boolean(matched)),
                        ("match_text", Value::String(match_text)),
                        ("index", Value::Number(index as f64)),
                        ("last_index_after", Value::Number(last_index_after as f64)),
                        ("test_ok", Value::Boolean(test_ok)),
                        ("test_is_boolean", Value::Boolean(test_is_boolean)),
                        ("test_result", Value::Boolean(test_result)),
                        ("no_match_ok", Value::Boolean(no_match_ok)),
                        ("no_match_is_null", Value::Boolean(no_match.get().is_null())),
                        ("no_match_index", Value::Number(no_match_index as f64)),
                    ])
                },
            );
        }

        fn regexp_syntax(engine: &JSEngine) {
            record_case(
                engine,
                "edge.regexp_syntax",
                &[
                    "NewUCRegExpObject",
                    "CheckRegExpSyntax",
                    "JS_IsExceptionPending",
                    "JS_ClearPendingException",
                ],
                |cx, _global| unsafe {
                    let flags = RegExpFlags {
                        flags_: RegExpFlag_UnicodeSets,
                    };
                    let check = |cx: &mut JSContext, pattern: &str| {
                        let units: Vec<u16> = pattern.encode_utf16().collect();
                        rooted!(&in(cx) let mut error = UndefinedValue());
                        assert!(CheckRegExpSyntax(
                            cx,
                            units.as_ptr(),
                            units.len(),
                            flags,
                            error.handle_mut()
                        ));
                        error.get()
                    };
                    let valid = ["", "a+", "[a-z]", "(?<n>a)", "[[a-z]&&[^aeiou]]"];
                    let invalid = ["(", "[z-a]", "(?<n>a)(?<n>b)", "\\"];
                    let mut valid_count = 0;
                    let mut invalid_count = 0;
                    for pattern in valid {
                        if check(cx, pattern).is_undefined() {
                            valid_count += 1;
                        }
                        if JS_IsExceptionPending(cx.raw_cx()) {
                            JS_ClearPendingException(cx.raw_cx());
                        }
                    }
                    for pattern in invalid {
                        if check(cx, pattern).is_object() {
                            invalid_count += 1;
                        }
                        if JS_IsExceptionPending(cx.raw_cx()) {
                            JS_ClearPendingException(cx.raw_cx());
                        }
                    }

                    let bad: Vec<u16> = "(".encode_utf16().collect();
                    rooted!(&in(cx) let bad_re =
                NewUCRegExpObject(cx, bad.as_ptr(), bad.len(), flags));
                    assert!(
                        bad_re.get().is_null(),
                        "an invalid RegExp must not be constructed"
                    );
                    crate::support::assert_exception_named(cx, "SyntaxError");

                    Observation::new(vec![
                        ("valid", Value::Number(valid_count as f64)),
                        ("invalid", Value::Number(invalid_count as f64)),
                        ("bad_null", Value::Boolean(true)),
                        ("bad_pending", Value::Boolean(true)),
                    ])
                },
            );
        }

        fn keys_and_ids(engine: &JSEngine) {
            record_case(
                engine,
                "edge.keys_and_ids",
                &[
                    "JS_IndexToId",
                    "JS_IdToValue",
                    "GetPropertyKeys",
                    "JS_GetProperty",
                    "JS_SetProperty",
                    "JS_GetElement",
                    "JS_SetElement",
                    "JS_GetPropertyById",
                    "JS_NewPlainObject",
                    "JS_NewObject",
                    "JS_NewObjectWithGivenProto",
                    "JS_IsExceptionPending",
                    "JS_ClearPendingException",
                ],
                |cx, global| unsafe {
                    let mut roundtrips = Vec::new();
                    for index in [0u32, 1, 4294967294, 4294967295] {
                        rooted!(&in(cx) let mut id = jsid::default());
                        if JS_IndexToId(cx, index, id.handle_mut()) {
                            rooted!(&in(cx) let mut value = UndefinedValue());
                            assert!(JS_IdToValue(cx, id.get(), value.handle_mut()));
                            if value.get().is_number() {
                                roundtrips.push(Value::String(format!(
                                    "number:{}",
                                    value.get().to_number()
                                )));
                            } else if value.get().is_string() {
                                let text = value.get().to_string();
                                roundtrips.push(Value::String(format!(
                                    "string:{}",
                                    jsstr_to_string(
                                        cx,
                                        std::ptr::NonNull::new(text).expect("index string")
                                    )
                                )));
                            } else {
                                roundtrips.push(Value::String("other".to_owned()));
                            }
                        } else {
                            roundtrips.push(Value::String("rejected".to_owned()));
                        }
                    }

                    rooted!(&in(cx) let symbol = eval(cx, global, "Symbol.iterator"));
                    rooted!(&in(cx) let mut symbol_id = jsid::default());
                    assert!(js::rust::wrappers2::JS_ValueToId(
                        cx,
                        symbol.handle(),
                        symbol_id.handle_mut()
                    ));
                    rooted!(&in(cx) let mut symbol_back = UndefinedValue());
                    assert!(JS_IdToValue(cx, symbol_id.get(), symbol_back.handle_mut()));
                    let mut symbol_same = false;
                    assert!(SameValue(
                        cx,
                        symbol.handle(),
                        symbol_back.handle(),
                        &mut symbol_same
                    ));

                    rooted!(&in(cx) let object = eval(
                cx,
                global,
                "var o = {b:1, 2:'two', a:2, 10:'ten'}; o[1]='one'; \
                 o[Symbol.iterator]=3; Object.defineProperty(o,'h',{value:4,enumerable:false}); o",
            ).to_object());
                    let own = key_names(cx, object.get(), js::jsapi::JSITER_OWNONLY as u32);
                    let with_symbols = key_names(
                        cx,
                        object.get(),
                        (js::jsapi::JSITER_OWNONLY | js::jsapi::JSITER_SYMBOLS) as u32,
                    );
                    let with_hidden = key_names(
                        cx,
                        object.get(),
                        (js::jsapi::JSITER_OWNONLY |
                            js::jsapi::JSITER_SYMBOLS |
                            js::jsapi::JSITER_HIDDEN) as u32,
                    );

                    rooted!(&in(cx) let mut id = jsid::default());
                    assert!(JS_IndexToId(cx, 2, id.handle_mut()));
                    rooted!(&in(cx) let mut by_id = UndefinedValue());
                    assert!(JS_GetPropertyById(
                        cx,
                        object.handle(),
                        id.handle(),
                        by_id.handle_mut()
                    ));
                    let by_id_text = by_id.get().to_string();
                    let by_id_text = jsstr_to_string(
                        cx,
                        std::ptr::NonNull::new(by_id_text).expect("by-id string"),
                    );

                    rooted!(&in(cx) let native = JS_NewPlainObject(cx));
                    rooted!(&in(cx) let value = Int32Value(7));
                    assert!(JS_SetProperty(
                        cx,
                        native.handle(),
                        c"k".as_ptr(),
                        value.handle()
                    ));
                    rooted!(&in(cx) let mut got = UndefinedValue());
                    assert!(JS_GetProperty(
                        cx,
                        native.handle(),
                        c"k".as_ptr(),
                        got.handle_mut()
                    ));
                    let property = got.get().to_int32();

                    let array = eval(cx, global, "[1, 2]");
                    let array = array.to_object();
                    rooted!(&in(cx) let array = array);
                    rooted!(&in(cx) let nine = Int32Value(9));
                    assert!(JS_SetElement(cx, array.handle(), 1, nine.handle()));
                    rooted!(&in(cx) let mut element = UndefinedValue());
                    assert!(JS_GetElement(cx, array.handle(), 1, element.handle_mut()));
                    let element = element.get().to_int32();

                    rooted!(&in(cx) let proto = eval(cx, global, "({p: 1})").to_object());
                    let created = js::rust::wrappers2::JS_NewObject(cx, ptr::null());
                    rooted!(&in(cx) let created = created);
                    let given = js::rust::wrappers2::JS_NewObjectWithGivenProto(
                        cx,
                        ptr::null(),
                        proto.handle(),
                    );
                    rooted!(&in(cx) let given = given);
                    set_global(cx, global, c"created", ObjectValue(created.get()));
                    set_global(cx, global, c"given", ObjectValue(given.get()));
                    set_global(cx, global, c"proto", ObjectValue(proto.get()));
                    let prototypes = eval(
                cx,
                global,
                "Object.getPrototypeOf(created) === Object.prototype && Object.getPrototypeOf(given) === proto",
            )
            .to_boolean();

                    Observation::new(vec![
                        ("roundtrips", Value::Array(roundtrips)),
                        ("symbol_same", Value::Boolean(symbol_same)),
                        ("own", own),
                        ("with_symbols", with_symbols),
                        ("with_hidden", with_hidden),
                        ("by_id", Value::String(by_id_text)),
                        ("property", Value::Number(property as f64)),
                        ("element", Value::Number(element as f64)),
                        ("prototypes", Value::Boolean(prototypes)),
                    ])
                },
            );
        }

        fn identity_and_source(engine: &JSEngine) {
            record_case(
                engine,
                "edge.identity_and_source",
                &[
                    "SameValue",
                    "StrictlyEqual",
                    "JS_ValueToSource",
                    "JS_NewPlainObject",
                ],
                |cx, global| unsafe {
                    rooted!(&in(cx) let nan_a = eval(cx, global, "NaN"));
                    rooted!(&in(cx) let nan_b = eval(cx, global, "NaN"));
                    rooted!(&in(cx) let zero = eval(cx, global, "0"));
                    rooted!(&in(cx) let negative_zero = eval(cx, global, "-0"));
                    rooted!(&in(cx) let object = eval(cx, global, "({})"));
                    rooted!(&in(cx) let object_alias = object.get());
                    rooted!(&in(cx) let other_object = eval(cx, global, "({})"));

                    let mut same_nan = false;
                    let mut strict_nan = false;
                    assert!(SameValue(cx, nan_a.handle(), nan_b.handle(), &mut same_nan));
                    assert!(StrictlyEqual(
                        cx,
                        nan_a.handle(),
                        nan_b.handle(),
                        &mut strict_nan
                    ));
                    let mut same_zero = false;
                    let mut strict_zero = false;
                    assert!(SameValue(
                        cx,
                        zero.handle(),
                        negative_zero.handle(),
                        &mut same_zero
                    ));
                    assert!(StrictlyEqual(
                        cx,
                        zero.handle(),
                        negative_zero.handle(),
                        &mut strict_zero
                    ));
                    let mut same_object = false;
                    let mut strict_object = false;
                    assert!(SameValue(
                        cx,
                        object.handle(),
                        object_alias.handle(),
                        &mut same_object
                    ));
                    assert!(StrictlyEqual(
                        cx,
                        object.handle(),
                        object_alias.handle(),
                        &mut strict_object
                    ));
                    let mut same_other = false;
                    assert!(SameValue(
                        cx,
                        object.handle(),
                        other_object.handle(),
                        &mut same_other
                    ));

                    rooted!(&in(cx) let source_string_value = eval(cx, global, "'x'"));
                    rooted!(&in(cx) let source_number_value = eval(cx, global, "1.5"));
                    rooted!(&in(cx) let source_true_value = eval(cx, global, "true"));
                    rooted!(&in(cx) let source_null_value = eval(cx, global, "null"));
                    let source_string = value_source(cx, source_string_value.get());
                    let source_number = value_source(cx, source_number_value.get());
                    let source_true = value_source(cx, source_true_value.get());
                    let source_null = value_source(cx, source_null_value.get());

                    rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
                    let plain_non_null = !plain.get().is_null();

                    Observation::new(vec![
                        ("same_nan", Value::Boolean(same_nan)),
                        ("strict_nan", Value::Boolean(strict_nan)),
                        ("same_zero", Value::Boolean(same_zero)),
                        ("strict_zero", Value::Boolean(strict_zero)),
                        ("same_object", Value::Boolean(same_object)),
                        ("strict_object", Value::Boolean(strict_object)),
                        ("same_other", Value::Boolean(same_other)),
                        (
                            "source_string",
                            Value::String(source_string.unwrap_or_default()),
                        ),
                        (
                            "source_number",
                            Value::String(source_number.unwrap_or_default()),
                        ),
                        (
                            "source_true",
                            Value::String(source_true.unwrap_or_default()),
                        ),
                        (
                            "source_null",
                            Value::String(source_null.unwrap_or_default()),
                        ),
                        ("plain_non_null", Value::Boolean(plain_non_null)),
                    ])
                },
            );
        }

        pub fn run(engine: &JSEngine) {
            collection_map(engine);
            collection_set(engine);
            weakmap(engine);
            weakmap_overwrite_only(engine);
            dates(engine);
            regexp_exec(engine);
            regexp_syntax(engine);
            keys_and_ids(engine);
            identity_and_source(engine);
        }
    }

    pub fn run(engine: &JSEngine) {
        collections_contract::run(engine);
        crate::support::isolate_case("objects.rooted_vectors", || rooted_vectors(engine));
        crate::support::isolate_case("objects.identity_and_functions", || {
            identity_and_functions(engine)
        });
        crate::support::isolate_case("objects.descriptors_and_forwarding", || {
            descriptors_and_forwarding(engine)
        });
        crate::support::isolate_case("objects.jit_accessors", || jit_getters_and_setters(engine));
        crate::support::isolate_case("objects.stack_vectors_for_of", || {
            stack_gc_vectors_and_for_of(engine)
        });
    }
}

mod edge_objects_more_contract {
    use std::ptr;

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::jsapi::mozilla::Range;
    use js::jsapi::{
        GetFunctionNativeReserved, GetStaticPrototype, IsCallable, IsConstructor,
        JS_GetFunctionArity, JS_GetFunctionObject, JS_IsGlobalObject, JSObject,
        SetFunctionNativeReserved, jsid,
    };
    use js::jsval::{Int32Value, JSVal, ObjectValue, StringValue, UndefinedValue};
    use js::rooted;
    use js::rust::JSEngine;
    use js::rust::wrappers2::{
        Construct1, DefineFunctionWithReserved, FromPropertyDescriptor, GetArrayLength,
        GetCurrentRealmOrNull, GetFunctionRealm, GetObjectProto, GetRealmErrorPrototype,
        GetRealmFunctionPrototype, GetRealmIteratorPrototype, GetRealmObjectPrototype,
        IsArgumentsObject, IsArrayObject, JS_DefineProperty, JS_DefineProperty2,
        JS_DefineUCProperty2, JS_ForwardGetPropertyTo, JS_ForwardSetPropertyTo, JS_FreezeObject,
        JS_GetFunctionDisplayId, JS_GetFunctionId,
        JS_InitializePropertiesFromCompatibleNativeObject, JS_IsIdentifier,
        JS_LinkConstructorAndPrototype, JS_NewObjectWithoutMetadata, JS_NewPlainObject,
        JS_NewStringCopyN, JS_SetImmutablePrototype, JS_ValueToFunction, JS_ValueToId,
        NewArrayObject, NewArrayObject1, NewFunctionWithReserved, SetArrayLength,
        SetDataPropertyDescriptor, SetPropertyIgnoringNamedGetter,
    };

    use crate::support::{Observation, Value, eval, record_case, set_global};

    fn functions_and_constructors(engine: &JSEngine) {
        record_case(
            engine,
            "edge.functions_and_constructors",
            &[
                "IsCallable",
                "IsConstructor",
                "JS_NewFunction",
                "JS_GetFunctionObject",
                "JS_GetFunctionId",
                "JS_GetFunctionDisplayId",
                "JS_ValueToFunction",
                "DefineFunctionWithReserved",
                "NewFunctionWithReserved",
                "GetFunctionNativeReserved",
                "SetFunctionNativeReserved",
                "Construct1",
                "JS_LinkConstructorAndPrototype",
                "JS_GetFunctionArity",
                "GetObjectProto",
                "GetStaticPrototype",
                "GetFunctionRealm",
                "GetRealmObjectPrototype",
                "GetRealmFunctionPrototype",
                "GetRealmErrorPrototype",
                "GetRealmIteratorPrototype",
                "IsArgumentsObject",
                "JS_NewPlainObject",
                "GetCurrentRealmOrNull",
            ],
            |cx, global| unsafe {
                let callable = eval(cx, global, "(function named(a, b) { return a + b; })");
                rooted!(&in(cx) let callable_obj = callable.to_object());
                rooted!(&in(cx) let not_callable = JS_NewPlainObject(cx));
                let callable_is_callable = IsCallable(callable_obj.get());
                let object_is_callable = IsCallable(not_callable.get());
                let callable_is_ctor = IsConstructor(callable_obj.get());
                rooted!(&in(cx) let arrow = eval(cx, global, "(() => 1)").to_object());
                let arrow_is_ctor = IsConstructor(arrow.get());

                let function = callable_obj.get().cast::<js::jsapi::JSFunction>();
                let function_object = JS_GetFunctionObject(function);
                let function_object_is_same = function_object == callable_obj.get();
                let arity = JS_GetFunctionArity(function);
                rooted!(&in(cx) let function = function);
                rooted!(&in(cx) let mut id = ptr::null_mut::<js::jsapi::JSString>());
                let id_ok = JS_GetFunctionId(cx, function.handle(), id.handle_mut());
                let id_text = if id.get().is_null() {
                    String::new()
                } else {
                    jsstr_to_string(cx, std::ptr::NonNull::new(id.get()).unwrap())
                };
                rooted!(&in(cx) let mut display = ptr::null_mut::<js::jsapi::JSString>());
                let display_ok =
                    JS_GetFunctionDisplayId(cx, function.handle(), display.handle_mut());
                let display_text = if display.get().is_null() {
                    String::new()
                } else {
                    jsstr_to_string(cx, std::ptr::NonNull::new(display.get()).unwrap())
                };
                rooted!(&in(cx) let callable_value = ObjectValue(callable_obj.get()));
                let converted = JS_ValueToFunction(cx, callable_value.handle());
                let converted_same = converted == *function;

                rooted!(&in(cx) let reserved =
                NewFunctionWithReserved(cx, Some(return_reserved), 1, 0, c"reservedFn".as_ptr()));
                let reserved_object = JS_GetFunctionObject(reserved.get());
                SetFunctionNativeReserved(reserved_object, 0, &Int32Value(77));
                let stored_value = (*GetFunctionNativeReserved(reserved_object, 0)).to_int32();
                rooted!(&in(cx) let slot = JS_NewStringCopyN(cx, b"slot".as_ptr().cast(), 4));
                SetFunctionNativeReserved(reserved.get().cast(), 1, &StringValue(&*slot.get()));
                let slot_string =
                    (*GetFunctionNativeReserved(reserved.get().cast(), 1)).to_string();
                let slot_string_text =
                    jsstr_to_string(cx, std::ptr::NonNull::new(slot_string).unwrap());

                rooted!(&in(cx) let defined = DefineFunctionWithReserved(
                    cx,
                    not_callable.get(),
                    c"definedFn".as_ptr(),
                    Some(return_reserved),
                    0,
                    1,
                ));
                SetFunctionNativeReserved(defined.get().cast(), 0, &Int32Value(99));
                let defined_after =
                    (*GetFunctionNativeReserved(defined.get().cast(), 0)).to_int32();

                rooted!(&in(cx) let ctor = eval(cx, global, "(function C(v) { this.v = v; })").to_object());
                rooted!(&in(cx) let proto = JS_NewPlainObject(cx));
                let linked = JS_LinkConstructorAndPrototype(cx, ctor.handle(), proto.handle());
                rooted!(&in(cx) let args = vec![Int32Value(42)]);
                let args = js::jsapi::HandleValueArray::from(&args);
                rooted!(&in(cx) let mut instance = ptr::null_mut::<JSObject>());
                rooted!(&in(cx) let ctor_value = ObjectValue(ctor.get()));
                let constructed = Construct1(cx, ctor_value.handle(), &args, instance.handle_mut());
                set_global(cx, global, c"constructed", ObjectValue(instance.get()));
                let constructed_ok =
                    eval(cx, global, "constructed && constructed.v === 42").to_boolean();

                rooted!(&in(cx) let mut object_proto = ptr::null_mut::<JSObject>());
                let object_proto_ok = GetObjectProto(cx, ctor.handle(), object_proto.handle_mut());
                let same_proto = GetStaticPrototype(ctor.get()) == object_proto.get();
                let realm = GetFunctionRealm(cx, ctor.handle().into());
                let realm_same = realm == GetCurrentRealmOrNull(cx);
                let realm_object_proto = GetRealmObjectPrototype(cx);
                let realm_function_proto = GetRealmFunctionPrototype(cx);
                let realm_error_proto = GetRealmErrorPrototype(cx);
                let realm_iterator_proto = GetRealmIteratorPrototype(cx);

                rooted!(&in(cx) let arguments = eval(cx, global, "(function () { return arguments; })()").to_object());
                let is_arguments = IsArgumentsObject(arguments.handle().into());

                assert!(callable_is_callable);
                assert!(!object_is_callable);
                assert!(callable_is_ctor);
                assert!(!arrow_is_ctor);
                assert!(function_object_is_same);
                assert_eq!(arity, 2);
                assert!(id_ok);
                assert_eq!(id_text, "named");
                assert!(display_ok);
                assert_eq!(display_text, "named");
                assert!(converted_same);
                assert_eq!(stored_value, 77);
                assert_eq!(slot_string_text, "slot");
                assert_eq!(defined_after, 99);
                assert!(linked);
                assert!(constructed);
                assert!(constructed_ok);
                assert!(object_proto_ok);
                assert!(same_proto);
                assert!(realm_same);
                assert!(!realm_object_proto.is_null());
                assert!(!realm_function_proto.is_null());
                assert!(!realm_error_proto.is_null());
                assert!(!realm_iterator_proto.is_null());
                assert!(is_arguments);

                Observation::new(vec![
                    ("callable_is_callable", Value::Boolean(callable_is_callable)),
                    ("object_is_callable", Value::Boolean(object_is_callable)),
                    ("callable_is_ctor", Value::Boolean(callable_is_ctor)),
                    ("arrow_is_ctor", Value::Boolean(arrow_is_ctor)),
                    (
                        "function_object_is_same",
                        Value::Boolean(function_object_is_same),
                    ),
                    ("arity", Value::Number(arity as f64)),
                    ("id_ok", Value::Boolean(id_ok)),
                    ("id", Value::String(id_text)),
                    ("display_ok", Value::Boolean(display_ok)),
                    ("display", Value::String(display_text)),
                    ("converted_same", Value::Boolean(converted_same)),
                    ("stored_value", Value::Number(stored_value as f64)),
                    ("slot_string", Value::String(slot_string_text)),
                    ("defined_after", Value::Number(defined_after as f64)),
                    ("linked", Value::Boolean(linked)),
                    ("constructed", Value::Boolean(constructed)),
                    ("constructed_ok", Value::Boolean(constructed_ok)),
                    ("object_proto_ok", Value::Boolean(object_proto_ok)),
                    ("same_proto", Value::Boolean(same_proto)),
                    ("realm_same", Value::Boolean(realm_same)),
                    (
                        "realm_object_non_null",
                        Value::Boolean(!realm_object_proto.is_null()),
                    ),
                    (
                        "realm_function_non_null",
                        Value::Boolean(!realm_function_proto.is_null()),
                    ),
                    (
                        "realm_error_non_null",
                        Value::Boolean(!realm_error_proto.is_null()),
                    ),
                    (
                        "realm_iterator_non_null",
                        Value::Boolean(!realm_iterator_proto.is_null()),
                    ),
                    ("is_arguments", Value::Boolean(is_arguments)),
                ])
            },
        );
    }
    fn properties_and_forwarding(engine: &JSEngine) {
        record_case(
            engine,
            "edge.properties_and_forwarding",
            &[
                "JS_NewPlainObject",
                "JS_DefineProperty",
                "JS_DefineProperty2",
                "JS_DefineUCProperty2",
                "JS_ForwardGetPropertyTo",
                "JS_ForwardSetPropertyTo",
                "SetDataPropertyDescriptor",
                "SetPropertyIgnoringNamedGetter",
                "FromPropertyDescriptor",
                "JS_FreezeObject",
                "JS_SetImmutablePrototype",
                "JS_NewObjectWithoutMetadata",
                "JS_InitializePropertiesFromCompatibleNativeObject",
                "JS_GetProperty",
                "JS_SetProperty",
                "JS_ValueToId",
                "JS_GetElement",
                "JS_SetElement",
                "GetArrayLength",
                "IsArrayObject",
                "NewArrayObject",
                "NewArrayObject1",
                "SetArrayLength",
                "JS_IsGlobalObject",
                "JS_IsIdentifier",
            ],
            |cx, global| unsafe {
                rooted!(&in(cx) let target = JS_NewPlainObject(cx));
                rooted!(&in(cx) let receiver = JS_NewPlainObject(cx));
                rooted!(&in(cx) let value = Int32Value(12));
                let defined = JS_DefineProperty(
                    cx,
                    target.handle(),
                    c"plain".as_ptr(),
                    value.handle(),
                    js::jsapi::JSPROP_ENUMERATE as u32,
                );
                let uc: Vec<u16> = "unicode".encode_utf16().collect();
                let defined_uc = JS_DefineUCProperty2(
                    cx,
                    target.handle(),
                    uc.as_ptr(),
                    uc.len(),
                    value.handle(),
                    js::jsapi::JSPROP_ENUMERATE as u32,
                );
                rooted!(&in(cx) let getter = eval(cx, global, "(function () { return this.marker; })").to_object());
                rooted!(&in(cx) let setter = eval(cx, global, "(function (v) { this.seen = v; })").to_object());
                let defined_accessor = JS_DefineProperty2(
                    cx,
                    target.handle(),
                    c"x".as_ptr(),
                    getter.handle(),
                    setter.handle(),
                    js::jsapi::JSPROP_ENUMERATE as u32,
                );
                rooted!(&in(cx) let marker = Int32Value(17));
                assert!(JS_DefineProperty(
                    cx,
                    receiver.handle(),
                    c"marker".as_ptr(),
                    marker.handle(),
                    js::jsapi::JSPROP_ENUMERATE as u32
                ));
                rooted!(&in(cx) let id_value = eval(cx, global, "'x'"));
                rooted!(&in(cx) let mut id = jsid::default());
                assert!(JS_ValueToId(cx, id_value.handle(), id.handle_mut()));
                rooted!(&in(cx) let receiver_value = ObjectValue(receiver.get()));
                rooted!(&in(cx) let mut forwarded = UndefinedValue());
                let forwarded_ok = JS_ForwardGetPropertyTo(
                    cx,
                    target.handle(),
                    id.handle(),
                    receiver_value.handle(),
                    forwarded.handle_mut(),
                );
                let forwarded_value = forwarded.get().to_int32();
                rooted!(&in(cx) let twenty_three = Int32Value(23));
                let mut op = js::jsapi::ObjectOpResult::default();
                let forward_set_ok = JS_ForwardSetPropertyTo(
                    cx,
                    target.handle(),
                    id.handle(),
                    twenty_three.handle(),
                    receiver_value.handle(),
                    &mut op,
                );
                set_global(cx, global, c"receiver", receiver_value.get());
                let seen = eval(cx, global, "receiver.seen").to_int32();

                rooted!(&in(cx) let mut descriptor = js::jsapi::PropertyDescriptor::default());
                SetDataPropertyDescriptor(
                    descriptor.handle_mut(),
                    value.handle(),
                    js::jsapi::JSPROP_ENUMERATE as u32,
                );
                let desc_value = descriptor.value_.to_int32();
                let desc_writable = descriptor.writable_();
                let desc_enumerable = descriptor.enumerable_();
                let desc_configurable = descriptor.configurable_();
                rooted!(&in(cx) let mut roundtrip = UndefinedValue());
                let roundtrip_ok =
                    FromPropertyDescriptor(cx, descriptor.handle(), roundtrip.handle_mut());
                set_global(cx, global, c"roundtrip", roundtrip.get());
                let roundtrip_value = eval(cx, global, "roundtrip.value").to_int32();
                let roundtrip_writable = eval(cx, global, "roundtrip.writable").to_boolean();

                rooted!(&in(cx) let ignored = JS_NewPlainObject(cx));
                rooted!(&in(cx) let ignored_value = Int32Value(5));
                assert!(JS_DefineProperty(
                    cx,
                    ignored.handle(),
                    c"readonly".as_ptr(),
                    ignored_value.handle(),
                    (js::jsapi::JSPROP_PERMANENT | js::jsapi::JSPROP_READONLY) as u32
                ));
                rooted!(&in(cx) let mut readonly_id = jsid::default());
                rooted!(&in(cx) let readonly_source = eval(cx, global, "'readonly'"));
                assert!(JS_ValueToId(
                    cx,
                    readonly_source.handle(),
                    readonly_id.handle_mut()
                ));
                rooted!(&in(cx) let ignored_receiver = ObjectValue(ignored.get()));
                let mut own_desc = js::jsapi::PropertyDescriptor::default();
                own_desc.set_hasValue_(true);
                own_desc.value_ = Int32Value(5);
                own_desc.set_hasWritable_(true);
                own_desc.set_writable_(false);
                rooted!(&in(cx) let own_desc = own_desc);
                let mut ignored_result = js::jsapi::ObjectOpResult::default();
                let ignored_ok = SetPropertyIgnoringNamedGetter(
                    cx,
                    ignored.handle(),
                    readonly_id.handle(),
                    ignored_value.handle(),
                    ignored_receiver.handle(),
                    Some(own_desc.handle()),
                    &mut ignored_result,
                );

                rooted!(&in(cx) let array = NewArrayObject(cx, &js::jsapi::HandleValueArray::empty()));
                rooted!(&in(cx) let sized = NewArrayObject1(cx, 5));
                let mut length = 0;
                let get_len = GetArrayLength(cx, array.handle(), &mut length);
                let mut sized_len = 0;
                let sized_ok = GetArrayLength(cx, sized.handle(), &mut sized_len);
                let mut is_array = false;
                rooted!(&in(cx) let array_value = ObjectValue(array.get()));
                let is_array_ok = IsArrayObject(cx, array_value.handle(), &mut is_array);
                let set_len = SetArrayLength(cx, array.handle(), 3);
                let mut after_len = 0;
                let after_ok = GetArrayLength(cx, array.handle(), &mut after_len);
                let frozen = JS_FreezeObject(cx, sized.handle());
                let frozen_set = SetArrayLength(cx, sized.handle(), 9);

                rooted!(&in(cx) let created = JS_NewPlainObject(cx));
                rooted!(&in(cx) let prototype = eval(cx, global, "({p: 1})").to_object());
                let mut immutable_succeeded = false;
                let immutable =
                    JS_SetImmutablePrototype(cx, created.handle(), &mut immutable_succeeded);
                let no_meta = JS_NewObjectWithoutMetadata(cx, ptr::null(), prototype.handle());
                let compat = JS_InitializePropertiesFromCompatibleNativeObject(
                    cx,
                    created.handle(),
                    ignored.handle(),
                );
                set_global(cx, global, c"created", ObjectValue(created.get()));
                let created_ok = eval(cx, global, "created.readonly === 5").to_boolean();
                let is_global = JS_IsGlobalObject(global.get());
                rooted!(&in(cx) let identifier = JS_NewStringCopyN(cx, b"ok".as_ptr().cast(), 2));
                let mut is_identifier = false;
                let identifier_ok = JS_IsIdentifier(cx, identifier.handle(), &mut is_identifier);
                assert!(defined && defined_uc && defined_accessor);
                assert!(
                    forwarded_ok,
                    "JS_ForwardGetPropertyTo must invoke the getter"
                );
                assert_eq!(
                    forwarded_value, 17,
                    "the forwarded getter must see the receiver"
                );
                assert!(
                    forward_set_ok && op.ok(),
                    "JS_ForwardSetPropertyTo must succeed"
                );
                assert_eq!(seen, 23, "the forwarded setter must mutate the receiver");
                assert_eq!(desc_value, 12);
                assert!(desc_writable && desc_enumerable && desc_configurable);
                assert!(roundtrip_ok && roundtrip_value == 12 && roundtrip_writable);
                assert!(ignored_ok, "SetPropertyIgnoringNamedGetter must complete");
                assert!(
                    !ignored_result.ok(),
                    "a readonly own descriptor must reject the set"
                );
                assert!(get_len);
                assert!(sized_ok && sized_len == 5);
                assert!(is_array_ok && is_array);
                assert!(set_len && after_ok && after_len == 3);
                assert!(frozen && !frozen_set);
                assert!(immutable && immutable_succeeded);
                assert!(!no_meta.is_null());
                assert!(compat && created_ok);
                assert!(
                    is_global,
                    "JS_IsGlobalObject must recognize the test global"
                );
                assert!(identifier_ok && is_identifier);

                Observation::new(vec![
                    ("defined", Value::Boolean(defined)),
                    ("defined_uc", Value::Boolean(defined_uc)),
                    ("defined_accessor", Value::Boolean(defined_accessor)),
                    ("forwarded_ok", Value::Boolean(forwarded_ok)),
                    ("forwarded_value", Value::Number(forwarded_value as f64)),
                    ("forward_set_ok", Value::Boolean(forward_set_ok)),
                    ("op_ok", Value::Boolean(op.ok())),
                    ("seen", Value::Number(seen as f64)),
                    ("desc_value", Value::Number(desc_value as f64)),
                    ("desc_writable", Value::Boolean(desc_writable)),
                    ("desc_enumerable", Value::Boolean(desc_enumerable)),
                    ("desc_configurable", Value::Boolean(desc_configurable)),
                    ("roundtrip_ok", Value::Boolean(roundtrip_ok)),
                    ("roundtrip_value", Value::Number(roundtrip_value as f64)),
                    ("roundtrip_writable", Value::Boolean(roundtrip_writable)),
                    ("ignored_ok", Value::Boolean(ignored_ok)),
                    ("ignored_result", Value::Boolean(ignored_result.ok())),
                    ("get_len", Value::Boolean(get_len)),
                    ("sized_ok", Value::Boolean(sized_ok)),
                    ("sized_len", Value::Number(sized_len as f64)),
                    ("is_array_ok", Value::Boolean(is_array_ok)),
                    ("is_array", Value::Boolean(is_array)),
                    ("set_len", Value::Boolean(set_len)),
                    ("after_ok", Value::Boolean(after_ok)),
                    ("after_len", Value::Number(after_len as f64)),
                    ("frozen", Value::Boolean(frozen)),
                    ("frozen_set", Value::Boolean(frozen_set)),
                    ("immutable", Value::Boolean(immutable)),
                    ("immutable_succeeded", Value::Boolean(immutable_succeeded)),
                    ("no_meta_null", Value::Boolean(no_meta.is_null())),
                    ("compat", Value::Boolean(compat)),
                    ("created_ok", Value::Boolean(created_ok)),
                    ("is_global", Value::Boolean(is_global)),
                    ("identifier_ok", Value::Boolean(identifier_ok)),
                    ("is_identifier", Value::Boolean(is_identifier)),
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
    unsafe extern "C" fn return_reserved(
        _cx: *mut js::jsapi::JSContext,
        argc: u32,
        vp: *mut js::jsapi::Value,
    ) -> bool {
        let args = js::jsapi::CallArgs::from_vp(vp, argc);
        args.rval()
            .set(*GetFunctionNativeReserved(args.callee(), 0));
        true
    }

    pub fn run(engine: &JSEngine) {
        functions_and_constructors(engine);
        properties_and_forwarding(engine);
    }
}

mod edge_objects_deep_contract {
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::ops::ControlFlow;
    use std::ptr::{self, NonNull};

    use js::context::JSContext;
    use js::conversions::jsstr_to_string;
    use js::glue::{CallJitMethodOp, RUST_FUNCTION_VALUE_TO_JITINFO};
    use js::jsapi::{
        self, CallArgs, CompilationType, ESClass, Handle, HandleString, HandleValue,
        IsWeakMapObject, JSFunction, JSFunctionSpec, JSJitInfo, JSJitMethodCallArgs,
        JSNativeWrapper, JSObject, JSPROP_ENUMERATE, JSPropertySpec, JSPropertySpec_Accessor,
        JSPropertySpec_AccessorsOrValue, JSPropertySpec_AccessorsOrValue_Accessors,
        JSPropertySpec_Kind, JSPropertySpec_Name, JSSecurityCallbacks, JSString, RuntimeCode,
        Value as JsValue, jsid,
    };
    use js::jsval::{Int32Value, JSVal, ObjectValue, UndefinedValue};
    use js::rooted;
    use js::rust::wrappers2::{
        GetBuiltinClass, GetRealmKeyObject, JS_DefineFunctions, JS_DefineProperties,
        JS_NewGlobalObject, JS_SetSecurityCallbacks, int_to_jsid,
    };
    use js::rust::{
        CompileOptionsWrapper, HandleObject, IdVector, JSEngine, RealmOptions,
        RootedObjectVectorWrapper, Runtime, SIMPLE_GLOBAL_CLASS, evaluate_script, for_of,
    };

    use crate::support::{Observation, Value, eval, record_case, set_global};
    // ---------------------------------------------------------------------------
    // Object definitions, JIT metadata and builtin-class identity.
    // ---------------------------------------------------------------------------

    unsafe extern "C" fn triple_arg(
        _cx: *mut jsapi::JSContext,
        argc: u32,
        vp: *mut JsValue,
    ) -> bool {
        let args = CallArgs::from_vp(vp, argc);
        let value = if argc == 0 { 0 } else { args.get(0).to_int32() };
        args.rval().set(Int32Value(value * 3));
        true
    }

    unsafe extern "C" fn return_seven(
        _cx: *mut jsapi::JSContext,
        argc: u32,
        vp: *mut JsValue,
    ) -> bool {
        CallArgs::from_vp(vp, argc).rval().set(Int32Value(7));
        true
    }

    unsafe extern "C" fn double_first_arg(
        _cx: *mut jsapi::JSContext,
        _obj: Handle<*mut JSObject>,
        specialized: *mut c_void,
        args: *const JSJitMethodCallArgs,
    ) -> bool {
        assert_eq!(specialized as usize, 0x1234);
        let value = (*args).get(0).to_int32();
        (*args).rval().set(Int32Value(value * 2));
        true
    }

    unsafe extern "C" fn generic_method(
        cx: *mut jsapi::JSContext,
        argc: u32,
        vp: *mut JsValue,
    ) -> bool {
        let args = CallArgs::from_vp(vp, argc);
        let info = RUST_FUNCTION_VALUE_TO_JITINFO(*args.calleev().ptr);
        assert!(!info.is_null());
        rooted!(in(cx) let this = args.thisv().to_object());
        CallJitMethodOp(
            info,
            cx,
            this.handle().into(),
            0x1234 as *mut c_void,
            argc,
            vp,
        )
    }

    fn object_definitions_and_identity(engine: &JSEngine) {
        record_case(
            engine,
            "edge.objects.definitions_and_identity",
            &[
                "CallJitMethodOp",
                "GetBuiltinClass",
                "GetRealmKeyObject",
                "IsWeakMapObject",
                "JS_DefineFunctions",
                "JS_DefineProperties",
                "JS_GetFunctionArity",
                "JS_GetFunctionDisplayId",
                "JS_GetFunctionId",
                "RUST_FUNCTION_VALUE_TO_JITINFO",
            ],
            |cx, global| unsafe {
                let mut info: JSJitInfo = std::mem::zeroed();
                info.__bindgen_anon_1.method = Some(double_first_arg);
                let info: &'static JSJitInfo = Box::leak(Box::new(info));

                let function_specs: &'static [JSFunctionSpec] = Box::leak(Box::new([
                    JSFunctionSpec {
                        name: JSPropertySpec_Name {
                            string_: c"triple".as_ptr(),
                        },
                        call: JSNativeWrapper {
                            op: Some(triple_arg),
                            info: ptr::null(),
                        },
                        nargs: 1,
                        flags: JSPROP_ENUMERATE as u16,
                        selfHostedName: ptr::null(),
                    },
                    JSFunctionSpec {
                        name: JSPropertySpec_Name {
                            string_: c"jitDouble".as_ptr(),
                        },
                        call: JSNativeWrapper {
                            op: Some(generic_method),
                            info,
                        },
                        nargs: 1,
                        flags: JSPROP_ENUMERATE as u16,
                        selfHostedName: ptr::null(),
                    },
                    JSFunctionSpec::ZERO,
                ]));

                let accessor = |op| JSPropertySpec_Accessor {
                    native: JSNativeWrapper {
                        op,
                        info: ptr::null(),
                    },
                };
                let property_specs = [
                    JSPropertySpec {
                        name: JSPropertySpec_Name {
                            string_: c"answer".as_ptr(),
                        },
                        attributes_: JSPROP_ENUMERATE as u8,
                        kind_: JSPropertySpec_Kind::NativeAccessor,
                        u: JSPropertySpec_AccessorsOrValue {
                            accessors: JSPropertySpec_AccessorsOrValue_Accessors {
                                getter: accessor(Some(return_seven)),
                                setter: accessor(Some(return_seven)),
                            },
                        },
                    },
                    JSPropertySpec::ZERO,
                ];

                rooted!(&in(cx) let object = js::rust::wrappers2::JS_NewPlainObject(cx));
                assert!(JS_DefineFunctions(
                    cx,
                    object.handle(),
                    function_specs.as_ptr()
                ));
                assert!(JS_DefineProperties(
                    cx,
                    object.handle(),
                    property_specs.as_ptr()
                ));
                set_global(cx, global, c"deepObject", ObjectValue(object.get()));

                rooted!(&in(cx) let triple = eval(cx, global, "deepObject.triple"));
                let triple_function_ptr = triple.get().to_object().cast::<JSFunction>();
                let arity = jsapi::JS_GetFunctionArity(triple_function_ptr);
                rooted!(&in(cx) let mut minor_name = ptr::null_mut::<JSString>());
                rooted!(&in(cx) let triple_function = triple_function_ptr);
                assert!(js::rust::wrappers2::JS_GetFunctionId(
                    cx,
                    triple_function.handle(),
                    minor_name.handle_mut(),
                ));
                let function_name = string_ptr_of(cx, minor_name.get());

                let keys_value = eval(cx, global, "Object.keys(deepObject).join(',')");
                let keys = string_of(cx, keys_value);
                let triple_result = eval(cx, global, "deepObject.triple(14)").to_int32();
                let jit_result = eval(cx, global, "deepObject.jitDouble(21)").to_int32();
                let property_value = eval(cx, global, "deepObject.answer").to_int32();
                let accessor_name_value = eval(
                    cx,
                    global,
                    "Object.getOwnPropertyDescriptor(deepObject, 'answer').get.name",
                );
                let accessor_name = string_of(cx, accessor_name_value);
                let jit_value = eval(cx, global, "deepObject.jitDouble");
                let jit_pointer = RUST_FUNCTION_VALUE_TO_JITINFO(jit_value);
                let jit_pointer_matches = jit_pointer == info as *const JSJitInfo;

                let mut builtin = |source: &str| -> ESClass {
                    rooted!(&in(cx) let object = object_of(cx, global, source));
                    let mut class = ESClass::Other;
                    assert!(GetBuiltinClass(cx, object.handle(), &mut class));
                    class
                };
                let plain_class = builtin("({})");
                let map_class = builtin("new Map()");
                let weak_class = builtin("new WeakMap()");

                rooted!(&in(cx) let weak = object_of(cx, global, "new WeakMap()"));
                rooted!(&in(cx) let plain = object_of(cx, global, "({})"));
                let weak_is_weakmap = IsWeakMapObject(weak.get());
                let plain_is_weakmap = IsWeakMapObject(plain.get());

                let key_a = GetRealmKeyObject(cx);
                let key_b = GetRealmKeyObject(cx);
                let realm_key_stable = !key_a.is_null() && key_a == key_b;

                Observation::new(vec![
                    ("keys", Value::String(keys)),
                    ("arity", Value::Number(arity as f64)),
                    ("function_name", Value::String(function_name)),
                    ("triple_result", Value::Number(triple_result as f64)),
                    ("jit_result", Value::Number(jit_result as f64)),
                    ("jit_pointer_matches", Value::Boolean(jit_pointer_matches)),
                    ("property_value", Value::Number(property_value as f64)),
                    ("accessor_name", Value::String(accessor_name)),
                    ("plain_class", Value::String(format!("{plain_class:?}"))),
                    ("map_class", Value::String(format!("{map_class:?}"))),
                    ("weak_class", Value::String(format!("{weak_class:?}"))),
                    ("weak_is_weakmap", Value::Boolean(weak_is_weakmap)),
                    ("plain_is_weakmap", Value::Boolean(plain_is_weakmap)),
                    ("realm_key_stable", Value::Boolean(realm_key_stable)),
                ])
            },
        );
    }

    // ---------------------------------------------------------------------------
    // Rooted vectors: exact id kinds and ordered append behavior.
    // ---------------------------------------------------------------------------

    fn rooted_vectors_deep(engine: &JSEngine) {
        record_case(
            engine,
            "edge.objects.rooted_vectors_deep",
            &[
                "AppendToIdVector",
                "AppendToRootedObjectVector",
                "CreateRootedIdVector",
                "CreateRootedObjectVector",
                "DeleteRootedObjectVector",
                "DestroyRootedIdVector",
                "GetIdVectorAddress",
                "GetObjectVectorAddress",
                "SliceRootedIdVector",
            ],
            |cx, global| unsafe {
                let raw_ids = js::glue::CreateRootedIdVector(cx.raw_cx());
                assert!(!raw_ids.is_null());
                assert!(!js::glue::GetIdVectorAddress(raw_ids).is_null());
                let mut raw_len = 0;
                let raw_slice = js::glue::SliceRootedIdVector(raw_ids, &mut raw_len);
                assert_eq!(raw_len, 0);
                assert!(!raw_slice.is_null() || raw_len == 0);
                js::glue::DestroyRootedIdVector(raw_ids);

                let mut ids = IdVector::new(cx);
                let string_id = id_for(cx, global, "'alpha'");
                rooted!(&in(cx) let string_id = string_id);
                rooted!(&in(cx) let mut int_id_handle = jsid::default());
                int_to_jsid(9, int_id_handle.handle_mut());
                let symbol_id = id_for(cx, global, "Symbol('deep')");
                rooted!(&in(cx) let symbol_id = symbol_id);

                assert!(js::rust::wrappers2::AppendToIdVector(
                    ids.handle_mut(),
                    string_id.handle()
                ));
                assert!(js::rust::wrappers2::AppendToIdVector(
                    ids.handle_mut(),
                    int_id_handle.handle()
                ));
                assert!(js::rust::wrappers2::AppendToIdVector(
                    ids.handle_mut(),
                    symbol_id.handle()
                ));

                let id_length = ids.len();
                let id_kinds = ids
                    .iter()
                    .map(|id| {
                        if id.is_string() {
                            "string"
                        } else if id.is_int() {
                            "int"
                        } else if id.is_symbol() {
                            "symbol"
                        } else {
                            "other"
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let id_string = string_ptr_of(cx, ids[0].to_string());
                let id_int = ids[1].to_int();

                let objects = RootedObjectVectorWrapper::new(cx);
                let first = object_of(cx, global, "({ index: 1 })");
                let second = object_of(cx, global, "({ index: 2 })");
                let append_first = objects.append(first);
                let append_second = objects.append(second);
                let object_address_nonnull = !objects.handle().ptr.is_null();

                Observation::new(vec![
                    ("id_length", Value::Number(id_length as f64)),
                    ("id_kinds", Value::String(id_kinds)),
                    ("id_string", Value::String(id_string)),
                    ("id_int", Value::Number(id_int as f64)),
                    ("symbol_id_is_symbol", Value::Boolean(ids[2].is_symbol())),
                    ("append_first", Value::Boolean(append_first)),
                    ("append_second", Value::Boolean(append_second)),
                    (
                        "object_address_nonnull",
                        Value::Boolean(object_address_nonnull),
                    ),
                ])
            },
        );
    }

    // ---------------------------------------------------------------------------
    // for-of iterators and the CSP StackGCVector arguments.
    // ---------------------------------------------------------------------------

    thread_local! {
        static CSP_STRINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static CSP_VALUES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static CSP_STRING_LEN: Cell<u32> = const { Cell::new(0) };
        static CSP_VALUE_LEN: Cell<u32> = const { Cell::new(0) };
    }

    unsafe extern "C" fn csp_callback(
        cx: *mut jsapi::JSContext,
        _runtime_code: RuntimeCode,
        _code_string: HandleString,
        _compilation_type: CompilationType,
        parameter_strings: Handle<js::gc::StackGCVector<*mut JSString>>,
        _body_string: HandleString,
        parameter_args: Handle<js::gc::StackGCVector<JsValue>>,
        _body_arg: HandleValue,
        can_compile_strings: *mut bool,
    ) -> bool {
        let mut cx = JSContext::from_ptr(NonNull::new(cx).expect("non-null JSContext"));
        let strings = js::rust::Handle::from_raw(parameter_strings);
        let values = js::rust::Handle::from_raw(parameter_args);
        CSP_STRING_LEN.with(|len| len.set(strings.len() as u32));
        CSP_VALUE_LEN.with(|len| len.set(values.len() as u32));
        let mut string_values = Vec::new();
        for index in 0..strings.len() {
            let string = strings.at(index).expect("string parameter").get();
            string_values.push(string_ptr_of(&mut cx, string));
        }
        let mut value_strings = Vec::new();
        for index in 0..values.len() {
            let value = values.at(index).expect("value parameter").get();
            value_strings.push(string_of(&mut cx, value));
        }
        CSP_STRINGS.with(|out| *out.borrow_mut() = string_values);
        CSP_VALUES.with(|out| *out.borrow_mut() = value_strings);
        *can_compile_strings = true;
        true
    }

    static DEEP_SECURITY_CALLBACKS: JSSecurityCallbacks = JSSecurityCallbacks {
        contentSecurityPolicyAllows: Some(csp_callback),
        codeForEvalGets: None,
        subsumes: None,
    };

    fn for_of_and_stack_vectors(engine: &JSEngine) {
        crate::support::isolate_case("edge.objects.for_of_and_stack_vectors", || {
            for_of_and_stack_vectors_inner(engine);
        });
    }

    fn for_of_and_stack_vectors_inner(engine: &JSEngine) {
        let mut runtime = Runtime::new(engine.handle());
        let cx = runtime.cx();
        unsafe {
            JS_SetSecurityCallbacks(cx, &DEEP_SECURITY_CALLBACKS);
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                js::jsapi::OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*RealmOptions::default(),
            ));
            let mut realm = js::realm::AutoRealm::new_from_handle(cx, global.handle());
            let (global, cx) = realm.global_and_reborrow();

            let function_source = "Function('a', 'b', 'return a + b')";
            rooted!(&in(cx) let mut compiled = UndefinedValue());
            let options = CompileOptionsWrapper::new(cx, c"deep-csp.js".to_owned(), 1);
            assert!(
                evaluate_script(cx, global, function_source, compiled.handle_mut(), options)
                    .is_ok()
            );
            assert!(compiled.is_object());

            let array = eval(cx, global, "[1, 2, 3]");
            rooted!(&in(cx) let array = array);
            let mut array_values = Vec::new();
            let array_result = for_of::<_, _, ()>(cx, array.handle(), |_, item| {
                array_values.push(item.get().to_int32());
                Ok(ControlFlow::Continue(()))
            });

            let iterable = eval(
                cx,
                global,
                "({ [Symbol.iterator]() { const values = [3, 1, 4]; let i = 0; return { next() { return i < values.length ? { value: values[i++], done: false } : { value: undefined, done: true }; } }; } })",
            );
            rooted!(&in(cx) let iterable = iterable);
            let mut iterable_values = Vec::new();
            let iterable_result = for_of::<_, _, ()>(cx, iterable.handle(), |_, item| {
                iterable_values.push(item.get().to_int32());
                Ok(ControlFlow::Continue(()))
            });

            let non_iterable = eval(cx, global, "7");
            rooted!(&in(cx) let non_iterable = non_iterable);
            let non_iterable_result = for_of::<_, _, ()>(cx, non_iterable.handle(), |_, _| {
                Ok(ControlFlow::Continue(()))
            });

            let csp_strings = CSP_STRINGS.with(|values| values.borrow().join(","));
            let csp_values = CSP_VALUES.with(|values| values.borrow().join(","));
            let csp_string_len = CSP_STRING_LEN.with(Cell::get);
            let csp_value_len = CSP_VALUE_LEN.with(Cell::get);

            let array_text = array_values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let iterable_text = iterable_values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");

            assert!(array_result.is_ok(), "for-of over an array must succeed");
            assert_eq!(array_text, "1,2,3");
            assert!(
                iterable_result.is_ok(),
                "for-of over a custom iterable must succeed"
            );
            assert_eq!(iterable_text, "3,1,4");
            assert_eq!(
                format!("{:?}", non_iterable_result.err()),
                "Some(ValueIsNotIterable)"
            );
            assert_eq!(csp_strings, "a,b");
            assert_eq!(csp_values, "a,b");

            record_case_result(
                "edge.objects.for_of_and_stack_vectors",
                Observation::new(vec![
                    ("csp_strings", Value::String(csp_strings)),
                    ("csp_values", Value::String(csp_values)),
                    ("csp_string_len", Value::Number(csp_string_len as f64)),
                    ("csp_value_len", Value::Number(csp_value_len as f64)),
                    ("array_ok", Value::Boolean(array_result.is_ok())),
                    ("array_values", Value::String(array_text)),
                    ("iterable_ok", Value::Boolean(iterable_result.is_ok())),
                    ("iterable_values", Value::String(iterable_text)),
                    (
                        "non_iterable_error",
                        Value::String(format!("{:?}", non_iterable_result.err())),
                    ),
                ]),
                &[
                    "AppendToIdVector",
                    "JS_ForOfIteratorInit",
                    "JS_ForOfIteratorNext",
                    "JS_ForOfIteratorValueIsIterable",
                    "StackGCVectorStringAtIndex",
                    "StackGCVectorStringLength",
                    "StackGCVectorValueAtIndex",
                    "StackGCVectorValueLength",
                ],
            );
        }
    }

    fn record_case_result(case: &str, observation: Observation, apis: &[&str]) {
        crate::support::record(case, &observation, apis);
    }
    fn string_ptr_of(cx: &mut JSContext, string: *mut JSString) -> String {
        unsafe { jsstr_to_string(cx, NonNull::new(string).expect("non-null JS string")) }
    }

    fn string_of(cx: &mut JSContext, value: JSVal) -> String {
        assert!(value.is_string(), "expected a string value");
        string_ptr_of(cx, value.to_string())
    }

    fn object_of(cx: &mut JSContext, global: HandleObject, source: &str) -> *mut JSObject {
        eval(cx, global, source).to_object()
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

    pub fn run(engine: &JSEngine) {
        object_definitions_and_identity(engine);
        rooted_vectors_deep(engine);
        for_of_and_stack_vectors(engine);
    }
}
pub fn run(engine: &JSEngine) {
    edge_objects_deep_contract::run(engine);
    edge_objects_more_contract::run(engine);
    extra_contract::run(engine);
    let mut steps = crate::support::Steps::new(
        "objects.core",
        &[
            "CallJitMethodOp",
            "CheckRegExpSyntax",
            "DateGetMsecSinceEpoch",
            "ExecuteRegExpNoStatics",
            "GetArrayLength",
            "GetBuiltinClass",
            "GetFunctionNativeReserved",
            "GetPropertyKeys",
            "GetRealmKeyObject",
            "GetRealmObjectPrototype",
            "GetWeakMapEntry",
            "IsArrayObject",
            "IsCallable",
            "IsConstructor",
            "IsWeakMapObject",
            "JS_ClearPendingException",
            "JS_DefineFunctions",
            "JS_DefineProperties",
            "JS_DefineProperty",
            "JS_FreezeObject",
            "JS_GC",
            "JS_GetElement",
            "JS_GetFunctionId",
            "JS_GetPropertyById",
            "JS_HasInstance",
            "JS_IsExceptionPending",
            "JS_NewGlobalObject",
            "JS_NewObjectWithGivenProto",
            "JS_NewPlainObject",
            "JS_SetElement",
            "JS_SetProperty",
            "MapDelete",
            "MapEntries",
            "MapGet",
            "MapHas",
            "MapSet",
            "MapSize",
            "NewArrayObject",
            "NewArrayObject1",
            "NewDateObject",
            "NewFunctionWithReserved",
            "NewMapObject",
            "NewSetObject",
            "NewUCRegExpObject",
            "NewWeakMapObject",
            "ObjectIsDate",
            "ObjectIsRegExp",
            "RUST_FUNCTION_VALUE_TO_JITINFO",
            "SetAdd",
            "SetArrayLength",
            "SetClear",
            "SetFunctionNativeReserved",
            "SetSize",
            "SetWeakMapEntry",
        ],
    );
    steps.step("arrays", || arrays(engine));
    steps.step("property_keys", || property_keys(engine));
    steps.step("map_size", || map_size(engine));
    steps.step("map_get_and_has", || map_get_and_has(engine));
    steps.step("map_entries", || map_entries(engine));
    steps.step("set_objects", || set_objects(engine));
    steps.step("get_realm_key_object", || get_realm_key_object(engine));
    steps.step("new_weak_map_object", || new_weak_map_object(engine));
    steps.step("set_weak_map_entry", || set_weak_map_entry(engine));
    steps.step("get_weak_map_entry", || get_weak_map_entry(engine));
    steps.step("new_date_object", || new_date_object(engine));
    steps.step("date_get_msec_since_epoch", || {
        date_get_msec_since_epoch(engine)
    });
    steps.step("object_is_date", || object_is_date(engine));
    steps.step("new_uc_regexp_object", || new_uc_regexp_object(engine));
    steps.step("object_is_regexp", || object_is_regexp(engine));
    steps.step("execute_regexp_no_statics", || {
        execute_regexp_no_statics(engine)
    });
    steps.step("check_regexp_syntax", || check_regexp_syntax(engine));
    steps.step("reserved_slots", || reserved_slots(engine));
    steps.step("is_callable_and_constructor", || {
        is_callable_and_constructor(engine)
    });
    steps.step("new_function_with_reserved", || {
        new_function_with_reserved(engine)
    });
    steps.step("js_get_function_id", || js_get_function_id(engine));
    steps.step("js_define_functions", || js_define_functions(engine));
    steps.step("rust_function_value_to_jitinfo", || {
        rust_function_value_to_jitinfo(engine)
    });
    steps.step("call_jit_method_op", || call_jit_method_op(engine));
    steps.step("accessor_function_names", || {
        accessor_function_names(engine)
    });
    steps.step("js_define_properties", || js_define_properties(engine));
    steps.finish();
}
