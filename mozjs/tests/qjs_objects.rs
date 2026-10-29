/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Object-level shim functions: arrays, key enumeration, Map/Set/WeakMap, Date, RegExp, native
//! functions with reserved slots and jit info, iteration and instanceof.

use std::ops::ControlFlow;
use std::ptr;
use std::sync::LazyLock;

use mozjs::context::JSContext;
use mozjs::jsapi::{
    CallArgs, ClippedTime, ESClass, GCReason, GetFunctionNativeReserved, HandleValueArray,
    IsCallable, IsConstructor, JSContext as RawJSContext, JSFunctionSpec, JSJitInfo,
    JSJitMethodCallArgs, JSNativeWrapper, JSObject, JSPropertySpec_Name, NewArrayObject1,
    OnNewGlobalHookOption, RegExpFlags, SetFunctionNativeReserved, Value, JSITER_HIDDEN,
    JSITER_OWNONLY, JSITER_SYMBOLS, JSITER_SYMBOLSONLY, JSPROP_ENUMERATE,
};
use mozjs::jsval::{Int32Value, JSVal, ObjectValue, StringValue, UndefinedValue};
use mozjs::realm::AutoRealm;
use mozjs::rooted;
use mozjs::rust::wrappers2::{
    CheckRegExpSyntax, DateGetMsecSinceEpoch, ExecuteRegExpNoStatics, GetArrayLength,
    GetBuiltinClass, GetPropertyKeys, GetRealmObjectPrototype, GetWeakMapEntry, IsArrayObject,
    JS_ClearPendingException, JS_DefineFunctions, JS_DefineProperty, JS_FreezeObject,
    JS_GetElement, JS_GetFunctionId, JS_GetPropertyById, JS_HasInstance, JS_IsExceptionPending,
    JS_NewGlobalObject, JS_NewPlainObject, JS_SetElement, JS_SetProperty, MapEntries, MapGet,
    MapSet, MapSize, NewArrayObject, NewDateObject, NewFunctionWithReserved, NewMapObject,
    NewSetObject, NewUCRegExpObject, NewWeakMapObject, ObjectIsDate, SetAdd, SetArrayLength,
    SetWeakMapEntry, JS_GC,
};
use mozjs::rust::{
    evaluate_script, for_of, CompileOptionsWrapper, HandleObject, IdVector, JSEngine,
    JSEngineHandle, RealmOptions, Runtime, SIMPLE_GLOBAL_CLASS,
};

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

/// The engine can only be initialized once per process; it is shared by the tests.
static ENGINE: LazyLock<JSEngineHandle> = LazyLock::new(|| {
    let engine = JSEngine::init().unwrap();
    let handle = engine.handle();
    std::mem::forget(engine);
    handle
});

/// Runs `f` in the realm of a new global.
fn with_global(f: impl FnOnce(&mut JSContext, HandleObject)) {
    let mut runtime = Runtime::new(ENGINE.clone());
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
        f(&mut realm, global.handle());
    }
}

#[test]
fn arrays() {
    with_global(|cx, global| unsafe {
        rooted!(&in(cx) let values = vec![Int32Value(1), Int32Value(2), Int32Value(3)]);
        rooted!(&in(cx) let array = NewArrayObject(cx, &HandleValueArray::from(&values)));
        let mut length = 0;
        assert!(GetArrayLength(cx, array.handle(), &mut length));
        assert_eq!(length, 3);
        rooted!(&in(cx) let mut element = UndefinedValue());
        assert!(JS_GetElement(cx, array.handle(), 2, element.handle_mut()));
        assert_eq!(element.get().to_int32(), 3);

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
    });
}

#[test]
fn property_keys() {
    with_global(|cx, global| unsafe {
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
        assert_eq!(keys(cx, JSITER_OWNONLY | JSITER_SYMBOLSONLY), (vec![3], 1));
        assert_eq!(keys(cx, 0), (vec![1, 2, 10], 0));

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
    });
}

#[test]
fn collections() {
    with_global(|cx, _global| unsafe {
        rooted!(&in(cx) let map = NewMapObject(cx));
        rooted!(&in(cx) let key = Int32Value(1));
        rooted!(&in(cx) let value = Int32Value(42));
        assert!(MapSet(cx, map.handle(), key.handle(), value.handle()));
        assert_eq!(MapSize(cx, map.handle()), 1);
        rooted!(&in(cx) let mut out = UndefinedValue());
        assert!(MapGet(cx, map.handle(), key.handle(), out.handle_mut()));
        assert_eq!(out.get().to_int32(), 42);
        assert!(MapEntries(cx, map.handle(), out.handle_mut()));
        let mut entries = 0;
        assert!(for_of::<_, _, ()>(cx, out.handle(), |_, entry| {
            assert!(entry.get().is_object());
            entries += 1;
            Ok(ControlFlow::Continue(()))
        })
        .is_ok());
        assert_eq!(entries, 1);
        assert_eq!(builtin_class(cx, map.get()), ESClass::Map);

        let mut has = false;
        assert!(mozjs::jsapi::MapHas(
            cx.raw_cx(),
            map.handle().into(),
            key.handle().into(),
            &mut has
        ));
        assert!(has);
        assert!(mozjs::jsapi::MapDelete(
            cx.raw_cx(),
            map.handle().into(),
            key.handle().into(),
            &mut has
        ));
        assert!(has);
        assert_eq!(MapSize(cx, map.handle()), 0);

        rooted!(&in(cx) let set = NewSetObject(cx));
        assert!(SetAdd(cx, set.handle(), key.handle()));
        assert!(SetAdd(cx, set.handle(), value.handle()));
        assert!(SetAdd(cx, set.handle(), key.handle()));
        assert_eq!(mozjs::jsapi::SetSize(cx.raw_cx(), set.handle().into()), 2);
        assert_eq!(builtin_class(cx, set.get()), ESClass::Set);
        rooted!(&in(cx) let set_value = ObjectValue(set.get()));
        let mut sum = 0;
        assert!(for_of::<_, _, ()>(cx, set_value.handle(), |_, v| {
            sum += v.get().to_int32();
            Ok(ControlFlow::Continue(()))
        })
        .is_ok());
        assert_eq!(sum, 43);
        assert!(mozjs::jsapi::SetClear(cx.raw_cx(), set.handle().into()));
        assert_eq!(mozjs::jsapi::SetSize(cx.raw_cx(), set.handle().into()), 0);

        // Servo's cross-origin property holder cache: realm key objects as keys.
        rooted!(&in(cx) let weak = NewWeakMapObject(cx));
        assert!(mozjs::jsapi::IsWeakMapObject(weak.get()));
        rooted!(&in(cx) let weak_key = ObjectValue(mozjs::rust::wrappers2::GetRealmKeyObject(cx)));
        assert!(SetWeakMapEntry(
            cx,
            weak.handle(),
            weak_key.handle(),
            value.handle()
        ));
        assert!(GetWeakMapEntry(
            cx,
            weak.handle(),
            weak_key.handle(),
            out.handle_mut()
        ));
        assert_eq!(out.get().to_int32(), 42);
        rooted!(&in(cx) let plain_key = ObjectValue(JS_NewPlainObject(cx)));
        assert!(!GetWeakMapEntry(
            cx,
            weak.handle(),
            plain_key.handle(),
            out.handle_mut()
        ));
        mozjs::rust::wrappers2::JS_ClearPendingException(cx);
    });
}

#[test]
fn dates_and_regexps() {
    with_global(|cx, global| unsafe {
        rooted!(&in(cx) let date = NewDateObject(cx, ClippedTime { t: 1.5e12 }));
        let mut is_date = false;
        assert!(ObjectIsDate(cx, date.handle(), &mut is_date));
        assert!(is_date);
        let mut ms = 0.0;
        assert!(DateGetMsecSinceEpoch(cx, date.handle(), &mut ms));
        assert_eq!(ms, 1.5e12);
        assert_eq!(builtin_class(cx, date.get()), ESClass::Date);
        rooted!(&in(cx) let not_date = JS_NewPlainObject(cx));
        assert!(ObjectIsDate(cx, not_date.handle(), &mut is_date));
        assert!(!is_date);
        assert_eq!(builtin_class(cx, not_date.get()), ESClass::Object);

        let flags = RegExpFlags {
            flags_: mozjs::jsapi::RegExpFlag_UnicodeSets,
        };
        let pattern: Vec<u16> = "^(?:[a-c]+)$".encode_utf16().collect();
        rooted!(&in(cx) let re = NewUCRegExpObject(cx, pattern.as_ptr(), pattern.len(), flags));
        assert!(!re.get().is_null());
        assert_eq!(builtin_class(cx, re.get()), ESClass::RegExp);
        let mut is_regexp = false;
        assert!(mozjs::jsapi::ObjectIsRegExp(
            cx.raw_cx(),
            re.handle().into(),
            &mut is_regexp
        ));
        assert!(is_regexp);
        set_global(cx, global, c"re", ObjectValue(re.get()));
        assert!(eval(cx, global, "re.flags === 'v' && re.lastIndex === 0").to_boolean());

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
        }

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
            assert!(!mozjs::jsapi::JS_IsExceptionPending(cx.raw_cx()));
            error.get()
        };
        assert!(check(cx, "[a-z]+").is_undefined());
        assert!(check(cx, "(").is_object());
    });
}

const SLOT_HOLDER: usize = 0;

unsafe extern "C" fn read_reserved(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval()
        .set(*GetFunctionNativeReserved(args.callee(), SLOT_HOLDER));
    true
}

#[test]
fn reserved_slots() {
    with_global(|cx, global| unsafe {
        rooted!(&in(cx) let fun = NewFunctionWithReserved(cx, Some(read_reserved), 0, 0, c"readReserved".as_ptr()));
        rooted!(&in(cx) let fun_obj = fun.get() as *mut JSObject);
        assert!(IsCallable(fun_obj.get()));
        assert!(!IsConstructor(fun_obj.get()));
        set_global(cx, global, c"readReserved", ObjectValue(fun_obj.get()));

        // A cycle through the reserved slot: holder.fun -> function -> slot -> holder.
        rooted!(&in(cx) let holder = eval(cx, global, "({ value: 7, fun: readReserved })").to_object());
        SetFunctionNativeReserved(fun_obj.get(), SLOT_HOLDER, &ObjectValue(holder.get()));
        SetFunctionNativeReserved(fun_obj.get(), 1, &Int32Value(5));
        JS_GC(cx, GCReason::API);
        assert_eq!((*GetFunctionNativeReserved(fun_obj.get(), 1)).to_int32(), 5);
        assert_eq!(eval(cx, global, "readReserved().value").to_int32(), 7);
        JS_GC(cx, GCReason::API);
        assert_eq!(
            eval(cx, global, "readReserved().fun === readReserved").to_boolean(),
            true
        );

        rooted!(&in(cx) let mut name = ptr::null_mut::<mozjs::jsapi::JSString>());
        assert!(JS_GetFunctionId(cx, fun.handle(), name.handle_mut()));
        assert!(!name.get().is_null());
        set_global(cx, global, c"funName", StringValue(&*name.get()));
        assert!(eval(cx, global, "funName === 'readReserved'").to_boolean());
        assert_eq!(builtin_class(cx, fun_obj.get()), ESClass::Function);
    });
}

unsafe extern "C" fn double_first_arg(
    _cx: *mut RawJSContext,
    _obj: mozjs::jsapi::HandleObject,
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
    let info = mozjs::glue::RUST_FUNCTION_VALUE_TO_JITINFO(*args.calleev().ptr);
    assert!(!info.is_null());
    rooted!(in(cx) let this = args.thisv().to_object());
    mozjs::glue::CallJitMethodOp(info, cx, this.handle().into(), 0x1234 as *mut _, argc, vp)
}

#[test]
fn jit_info() {
    with_global(|cx, global| unsafe {
        let mut info: JSJitInfo = std::mem::zeroed();
        info.__bindgen_anon_1.method = Some(double_first_arg);
        let info: &'static JSJitInfo = Box::leak(Box::new(info));
        let specs: &'static [JSFunctionSpec] = Box::leak(Box::new([
            JSFunctionSpec {
                name: JSPropertySpec_Name {
                    string_: c"double".as_ptr(),
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
        set_global(cx, global, c"obj", ObjectValue(obj.get()));

        rooted!(&in(cx) let fun = eval(cx, global, "obj.double"));
        assert_eq!(
            mozjs::glue::RUST_FUNCTION_VALUE_TO_JITINFO(fun.get()),
            info as *const _
        );
        assert_eq!(eval(cx, global, "obj.double(21)").to_int32(), 42);
        let plain = eval(cx, global, "(function() {})");
        assert!(mozjs::glue::RUST_FUNCTION_VALUE_TO_JITINFO(plain).is_null());
    });
}

unsafe extern "C" fn construct_hook(cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    let obj = mozjs::jsapi::JS_NewPlainObject(cx);
    let arg = if argc > 0 {
        *args.get(0).ptr
    } else {
        UndefinedValue()
    };
    rooted!(in(cx) let obj = obj);
    rooted!(in(cx) let arg = arg);
    assert!(mozjs::jsapi::JS_DefineProperty(
        cx,
        obj.handle().into(),
        c"arg".as_ptr(),
        arg.handle().into(),
        JSPROP_ENUMERATE as u32
    ));
    args.rval().set(ObjectValue(obj.get()));
    true
}

static CONSTRUCTIBLE_OPS: mozjs::jsapi::JSClassOps = mozjs::jsapi::JSClassOps {
    addProperty: None,
    delProperty: None,
    enumerate: None,
    newEnumerate: None,
    resolve: None,
    mayResolve: None,
    finalize: None,
    call: None,
    construct: Some(construct_hook),
    trace: None,
};

static CONSTRUCTIBLE_CLASS: mozjs::jsapi::JSClass = mozjs::jsapi::JSClass {
    name: c"Constructible".as_ptr(),
    flags: 0,
    cOps: &CONSTRUCTIBLE_OPS,
    spec: ptr::null(),
    ext: ptr::null(),
    oOps: ptr::null(),
};

/// Like Servo's interface objects: an object whose class has a `construct` hook works with `new`.
#[test]
fn class_construct_hook() {
    with_global(|cx, global| unsafe {
        rooted!(&in(cx) let proto = ptr::null_mut::<JSObject>());
        let ctor = mozjs::rust::wrappers2::JS_NewObjectWithGivenProto(
            cx,
            &CONSTRUCTIBLE_CLASS,
            proto.handle(),
        );
        assert!(!ctor.is_null());
        rooted!(&in(cx) let ctor = ctor);
        assert!(IsConstructor(ctor.get()));
        set_global(cx, global, c"C", ObjectValue(ctor.get()));
        let v = eval(cx, global, "new C(42).arg");
        assert_eq!(v.to_int32(), 42);
    });
}

unsafe extern "C" fn seven(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval().set(Int32Value(7));
    true
}

/// WebIDL attribute getters are named "get <name>".
#[test]
fn accessor_function_names() {
    use mozjs::jsapi::{
        JSPropertySpec, JSPropertySpec_Accessor, JSPropertySpec_AccessorsOrValue,
        JSPropertySpec_AccessorsOrValue_Accessors, JSPropertySpec_Kind,
    };
    with_global(|cx, global| unsafe {
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
        assert!(mozjs::rust::wrappers2::JS_DefineProperties(
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
            mozjs::conversions::jsstr_to_string(cx, std::ptr::NonNull::new(v.to_string()).unwrap());
        assert_eq!(s, "7,get x,set x");
    });
}

/// QuickJS behaviour changed to match SpiderMonkey and the specification.
#[test]
fn builtin_conformance() {
    with_global(|cx, global| unsafe {
        // Q25: Iterator.prototype.constructor has a getter and a setter of its own.
        assert!(eval(
            cx,
            global,
            "
            var d = Object.getOwnPropertyDescriptor(Iterator.prototype, 'constructor');
            var o = Object.create(Iterator.prototype);
            o.constructor = 5;
            var threw = false;
            try { d.set.call(Iterator.prototype, 1); } catch (e) { threw = e instanceof TypeError; }
            d.get !== d.set && d.set.length === 1 && Iterator.prototype.constructor === Iterator &&
                Object.getOwnPropertyDescriptor(o, 'constructor').value === 5 && threw
            "
        )
        .to_boolean());
        // Q28: Array.fromAsync.
        eval(
            cx,
            global,
            "
            var results = [];
            async function* gen() { yield 1; yield Promise.resolve(2); }
            Array.fromAsync(gen()).then(a => results.push('async:' + a.join()));
            Array.fromAsync([Promise.resolve(3), 4], (x, i) => x * 2 + i)
                .then(a => results.push('sync:' + a.join()));
            Array.fromAsync({ length: 2, 0: 'a', 1: Promise.resolve('b') })
                .then(a => results.push('arraylike:' + a.join()));
            Array.fromAsync([1], 5).catch(e => results.push('mapfn:' + (e instanceof TypeError)));
            var closed = false;
            var rejecting = { [Symbol.iterator]() { return {
                next() { return { value: Promise.reject('r'), done: false }; },
                return() { closed = true; return {}; } }; } };
            Array.fromAsync(rejecting).catch(e => results.push('rejected:' + e + ':' + closed));
            class MyArray extends Array {}
            MyArray.fromAsync([1]).then(a => results.push('subclass:' + (a instanceof MyArray)));
            ",
        );
        mozjs::jsapi::RunJobs(cx.raw_cx());
        assert!(eval(
            cx,
            global,
            "Array.fromAsync.length === 1 && Array.fromAsync.name === 'fromAsync' && \
             results.sort().join(' ') === \
             'arraylike:a,b async:1,2 mapfn:true rejected:r:true subclass:true sync:6,9'"
        )
        .to_boolean());
        // Q29: Error.prototype.stack accessor; errors keep their own stack.
        assert!(eval(
            cx,
            global,
            "
            var d = Object.getOwnPropertyDescriptor(Error.prototype, 'stack');
            var e = new Error('x');
            var o = Object.create(Error.prototype);
            var notError = false;
            try { d.get.call({}); } catch (x) { notError = x instanceof TypeError; }
            d.set.call(o, 's');
            var own = Object.getOwnPropertyDescriptor(o, 'stack');
            typeof d.get === 'function' && typeof d.set === 'function' && !d.enumerable &&
                d.configurable && typeof e.stack === 'string' && e.hasOwnProperty('stack') &&
                d.get.call(e) === e.stack && Object.create(Error.prototype).stack === '' &&
                notError && own.value === 's' && own.enumerable && own.writable
            "
        )
        .to_boolean());
        // Q26: localeCompare returns -1, 0 or 1.
        assert!(eval(
            cx,
            global,
            "['a'.localeCompare('z'), 'z'.localeCompare('a'), 'a'.localeCompare('a')].join() === '-1,1,0'"
        )
        .to_boolean());
    });
}
