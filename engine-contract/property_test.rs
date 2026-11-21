/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Property definition, lookup, deletion, descriptors, prototypes and calls.

use std::ptr;

use js::context::JSContext;
use js::jsapi::{
    CallArgs, HandleValueArray, JSContext as RawJSContext, JSPROP_ENUMERATE, JSPROP_PERMANENT,
    JSPROP_READONLY, ObjectOpResult, PropertyDescriptor, Value, jsid,
};
use js::jsval::{Int32Value, ObjectValue, UndefinedValue};
use js::rooted;
use js::rust::wrappers2::{
    Construct, JS_AlreadyHasOwnPropertyById, JS_CallFunctionName, JS_DefineElement,
    JS_DefineFunction, JS_DefineFunctionById, JS_DefineProperty1, JS_DefineProperty2,
    JS_DefineProperty3, JS_DefineProperty4, JS_DefineProperty5, JS_DefineProperty6,
    JS_DefineProperty7, JS_DefinePropertyById, JS_DefinePropertyById1, JS_DefinePropertyById3,
    JS_DefinePropertyById4, JS_DefinePropertyById5, JS_DefinePropertyById6, JS_DefinePropertyById7,
    JS_DefinePropertyById8, JS_DefineUCProperty, JS_DeleteProperty, JS_DeletePropertyById,
    JS_GetOwnPropertyDescriptor, JS_GetOwnPropertyDescriptorById, JS_GetPropertyDescriptor,
    JS_GetPropertyDescriptorById, JS_GetPrototype, JS_GetUCProperty, JS_HasOwnProperty,
    JS_HasOwnPropertyById, JS_HasProperty, JS_HasPropertyById, JS_NewPlainObject, JS_SetProperty,
    JS_SetPropertyById, JS_SetPrototype, JS_SetUCProperty, JS_ValueToId, JS_ValueToObject,
};
use js::rust::{HandleObject, JSEngine};

use crate::support::{
    Observation, Value as Observed, eval, eval_observe, observe, observe_property, record_case,
    set_global,
};

const ATTRS: u32 = (JSPROP_ENUMERATE | JSPROP_PERMANENT | JSPROP_READONLY) as u32;

pub fn run(engine: &JSEngine) {
    record_case(
        engine,
        "property.families",
        &[
            "JS_NewPlainObject",
            "JS_DefineProperty1",
            "JS_DefineProperty2",
            "JS_DefineProperty3",
            "JS_DefineProperty4",
            "JS_DefineProperty5",
            "JS_DefineProperty6",
            "JS_DefineProperty7",
            "JS_DefineUCProperty",
            "JS_DefineElement",
            "JS_DefineFunction",
            "JS_DefineFunctionById",
            "JS_DefinePropertyById",
            "JS_DefinePropertyById1",
            "JS_DefinePropertyById3",
            "JS_DefinePropertyById4",
            "JS_DefinePropertyById5",
            "JS_DefinePropertyById6",
            "JS_DefinePropertyById7",
            "JS_DefinePropertyById8",
            "JS_HasProperty",
            "JS_HasPropertyById",
            "JS_HasOwnProperty",
            "JS_HasOwnPropertyById",
            "JS_AlreadyHasOwnPropertyById",
            "JS_SetPropertyById",
            "JS_SetUCProperty",
            "JS_GetUCProperty",
            "JS_DeleteProperty",
            "JS_DeletePropertyById",
            "JS_GetOwnPropertyDescriptor",
            "JS_GetOwnPropertyDescriptorById",
            "JS_GetPropertyDescriptor",
            "JS_GetPropertyDescriptorById",
            "JS_GetPrototype",
            "JS_SetPrototype",
            "JS_ValueToObject",
            "Construct",
            "JS_CallFunctionName",
            "JS_SetProperty",
            "JS_GetProperty",
        ],
        |cx, _global| unsafe {
            let object = JS_NewPlainObject(cx);
            rooted!(&in(cx) let object = object);
            let proto = JS_NewPlainObject(cx);
            rooted!(&in(cx) let proto = proto);
            set_global(cx, _global, c"o", ObjectValue(object.get()));

            assert!(JS_DefineProperty1(
                cx,
                object.handle(),
                c"native_accessor".as_ptr(),
                Some(native_getter),
                Some(native_setter),
                JSPROP_ENUMERATE as u32,
            ));

            let getter = eval(cx, _global, "(function () { return 11; })").to_object();
            rooted!(&in(cx) let getter = getter);
            let setter = eval(cx, _global, "(function (v) { globalThis.seen = v; })").to_object();
            rooted!(&in(cx) let setter = setter);
            assert!(JS_DefineProperty2(
                cx,
                object.handle(),
                c"object_accessor".as_ptr(),
                getter.handle(),
                setter.handle(),
                JSPROP_ENUMERATE as u32,
            ));
            let object_accessor = eval_observe(cx, _global, "o.object_accessor");
            rooted!(&in(cx) let seven = Int32Value(7));
            assert!(JS_SetProperty(
                cx,
                object.handle(),
                c"object_accessor".as_ptr(),
                seven.handle(),
            ));
            let seen_after_set = eval_observe(cx, _global, "globalThis.seen");

            let object_value = eval(cx, _global, "({ answer: 41 })").to_object();
            rooted!(&in(cx) let object_value = object_value);
            assert!(JS_DefineProperty3(
                cx,
                object.handle(),
                c"object_value".as_ptr(),
                object_value.handle(),
                ATTRS,
            ));
            let string = eval(cx, _global, "'native string'");
            rooted!(&in(cx) let string = string);
            rooted!(&in(cx) let string_ptr = string.to_string());
            assert!(JS_DefineProperty4(
                cx,
                object.handle(),
                c"string_value".as_ptr(),
                string_ptr.handle(),
                ATTRS,
            ));
            assert!(JS_DefineProperty5(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                -7,
                ATTRS,
            ));
            assert!(JS_DefineProperty6(
                cx,
                object.handle(),
                c"uint_value".as_ptr(),
                u32::MAX,
                ATTRS,
            ));
            assert!(JS_DefineProperty7(
                cx,
                object.handle(),
                c"double_value".as_ptr(),
                1.5,
                ATTRS,
            ));

            let uc_name: Vec<u16> = "ünicode".encode_utf16().collect();
            rooted!(&in(cx) let uc_value = Int32Value(12));
            let mut uc_desc = Box::new(PropertyDescriptor::default());
            uc_desc.set_hasValue_(true);
            uc_desc.value_ = uc_value.get();
            uc_desc.set_hasEnumerable_(true);
            uc_desc.set_enumerable_(true);
            uc_desc.set_hasWritable_(true);
            uc_desc.set_writable_(true);
            uc_desc.set_hasConfigurable_(true);
            uc_desc.set_configurable_(true);
            let mut op_result = ObjectOpResult::default();
            assert!(JS_DefineUCProperty(
                cx,
                object.handle(),
                uc_name.as_ptr(),
                uc_name.len(),
                js::rust::Handle::from_marked_location(&*uc_desc),
                &mut op_result,
            ));
            assert!(op_result.ok());
            rooted!(&in(cx) let five = Int32Value(5));
            assert!(JS_DefineElement(
                cx,
                object.handle(),
                0,
                five.handle(),
                ATTRS,
            ));

            let callable = eval(cx, _global, "(function named(a) { return a + 1; })");
            rooted!(&in(cx) let callable = callable);
            let callable_object = callable.to_object();
            rooted!(&in(cx) let callable_object = callable_object);
            assert!(
                !JS_DefineFunction(
                    cx,
                    object.handle(),
                    c"native_fun".as_ptr(),
                    Some(native_seven),
                    0,
                    JSPROP_ENUMERATE as u32,
                )
                .is_null()
            );

            rooted!(&in(cx) let function_id = id_for(cx, _global, "'by_id_fun'"));
            assert!(
                !JS_DefineFunctionById(
                    cx,
                    object.handle(),
                    function_id.handle().into(),
                    Some(native_seven),
                    0,
                    JSPROP_ENUMERATE as u32,
                )
                .is_null()
            );

            // The descriptor forms exercise both the checked and unchecked variants.
            rooted!(&in(cx) let desc_id = id_for(cx, _global, "'desc_id'"));
            let mut desc = Box::new(PropertyDescriptor::default());
            desc.set_hasValue_(true);
            desc.value_ = Int32Value(99);
            desc.set_hasWritable_(true);
            desc.set_writable_(true);
            let mut op_result = ObjectOpResult::default();
            assert!(JS_DefinePropertyById(
                cx,
                object.handle(),
                desc_id.handle().into(),
                js::rust::Handle::from_marked_location(&*desc).into(),
                &mut op_result,
            ));
            assert!(op_result.ok());
            rooted!(&in(cx) let desc1_id = id_for(cx, _global, "'desc_id_1'"));
            assert!(JS_DefinePropertyById1(
                cx,
                object.handle(),
                desc1_id.handle().into(),
                js::rust::Handle::from_marked_location(&*desc).into(),
            ));

            rooted!(&in(cx) let id3 = id_for(cx, _global, "'id_accessor'"));
            assert!(JS_DefinePropertyById3(
                cx,
                object.handle(),
                id3.handle().into(),
                Some(native_getter),
                Some(native_setter),
                JSPROP_ENUMERATE as u32,
            ));
            let id_accessor = eval_observe(cx, _global, "o.id_accessor");
            rooted!(&in(cx) let id4 = id_for(cx, _global, "'id_object_accessor'"));
            assert!(JS_DefinePropertyById4(
                cx,
                object.handle(),
                id4.handle().into(),
                getter.handle().into(),
                setter.handle().into(),
                JSPROP_ENUMERATE as u32,
            ));
            rooted!(&in(cx) let id5 = id_for(cx, _global, "'id_object_value'"));
            assert!(JS_DefinePropertyById5(
                cx,
                object.handle(),
                id5.handle().into(),
                object_value.handle().into(),
                ATTRS,
            ));
            let id_object_answer = eval_observe(cx, _global, "o.id_object_value.answer");
            assert_eq!(id_object_answer, Observed::Number(41.0));
            rooted!(&in(cx) let id6 = id_for(cx, _global, "'id_string_value'"));
            assert!(JS_DefinePropertyById6(
                cx,
                object.handle(),
                id6.handle().into(),
                string_ptr.handle(),
                ATTRS,
            ));
            rooted!(&in(cx) let id7 = id_for(cx, _global, "'id_int_value'"));
            assert!(JS_DefinePropertyById7(
                cx,
                object.handle(),
                id7.handle().into(),
                -9,
                ATTRS,
            ));
            rooted!(&in(cx) let id8 = id_for(cx, _global, "'id_uint_value'"));
            assert!(JS_DefinePropertyById8(
                cx,
                object.handle(),
                id8.handle().into(),
                u32::MAX,
                ATTRS,
            ));

            rooted!(&in(cx) let has = id_for(cx, _global, "'int_value'"));
            let mut found = false;
            assert!(JS_HasProperty(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                &mut found,
            ));
            assert!(found);

            // The positive lookups above cannot distinguish a real lookup from one that always
            // answers true, so each family also checks an absent name.
            let mut missing = true;
            assert!(JS_HasProperty(
                cx,
                object.handle(),
                c"missing_value".as_ptr(),
                &mut missing,
            ));
            assert!(!missing);
            rooted!(&in(cx) let missing_name_value = eval(cx, _global, "'missing_value'"));
            rooted!(&in(cx) let mut missing_id = jsid::default());
            assert!(JS_ValueToId(
                cx,
                missing_name_value.handle().into(),
                missing_id.handle_mut().into()
            ));
            assert!(JS_HasPropertyById(
                cx,
                object.handle(),
                missing_id.handle().into(),
                &mut missing,
            ));
            assert!(!missing);
            assert!(JS_HasOwnProperty(
                cx,
                object.handle(),
                c"missing_value".as_ptr(),
                &mut missing,
            ));
            assert!(!missing);
            assert!(JS_HasOwnPropertyById(
                cx,
                object.handle(),
                missing_id.handle().into(),
                &mut missing,
            ));
            assert!(!missing);
            assert!(JS_AlreadyHasOwnPropertyById(
                cx,
                object.handle(),
                missing_id.handle().into(),
                &mut missing,
            ));
            assert!(!missing);
            assert!(JS_HasPropertyById(
                cx,
                object.handle(),
                has.handle().into(),
                &mut found,
            ));
            assert!(found);
            assert!(JS_HasOwnProperty(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                &mut found,
            ));
            assert!(found);
            assert!(JS_HasOwnPropertyById(
                cx,
                object.handle(),
                has.handle().into(),
                &mut found,
            ));
            assert!(found);
            assert!(JS_AlreadyHasOwnPropertyById(
                cx,
                object.handle(),
                has.handle().into(),
                &mut found,
            ));
            assert!(found);
            rooted!(&in(cx) let set_id = id_for(cx, _global, "'by_id_set'"));
            rooted!(&in(cx) let twenty_one = Int32Value(21));
            assert!(JS_SetPropertyById(
                cx,
                object.handle(),
                set_id.handle().into(),
                twenty_one.handle().into(),
            ));
            let set_uc: Vec<u16> = "uc_set".encode_utf16().collect();
            rooted!(&in(cx) let twenty_two = Int32Value(22));
            assert!(JS_SetUCProperty(
                cx,
                object.handle(),
                set_uc.as_ptr(),
                set_uc.len(),
                twenty_two.handle().into(),
            ));
            rooted!(&in(cx) let mut got = UndefinedValue());
            assert!(JS_GetUCProperty(
                cx,
                object.handle(),
                set_uc.as_ptr(),
                set_uc.len(),
                got.handle_mut().into(),
            ));
            let uc_get = observe(cx, got.get());

            let mut op_result = ObjectOpResult::default();
            assert!(JS_DeleteProperty(
                cx,
                object.handle(),
                c"by_id_fun".as_ptr(),
                &mut op_result,
            ));
            assert!(op_result.ok());
            rooted!(&in(cx) let delete_id = id_for(cx, _global, "'by_id_set'"));
            let mut op_result = ObjectOpResult::default();
            assert!(JS_DeletePropertyById(
                cx,
                object.handle(),
                delete_id.handle().into(),
                &mut op_result,
            ));
            assert!(op_result.ok());

            rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
            let mut is_none = true;
            assert!(JS_GetOwnPropertyDescriptor(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                desc.handle_mut().into(),
                &mut is_none,
            ));
            assert!(!is_none);
            let int_descriptor_value = observe(cx, desc.get().value_);
            let int_descriptor_writable = desc.get().writable_();
            let int_descriptor_enumerable = desc.get().enumerable_();
            let int_descriptor_configurable = desc.get().configurable_();
            assert!(
                !int_descriptor_writable,
                "JSPROP_READONLY must clear writable"
            );
            assert!(
                int_descriptor_enumerable,
                "JSPROP_ENUMERATE must set enumerable"
            );
            assert!(
                !int_descriptor_configurable,
                "JSPROP_PERMANENT must clear configurable"
            );

            // A failed assignment to a readonly property is ignored in sloppy mode; the value
            // must not change even though the API reports success.
            rooted!(&in(cx) let replacement = Int32Value(99));
            let write_attempt = JS_SetProperty(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                replacement.handle(),
            );
            assert!(write_attempt, "a sloppy readonly assignment must not throw");
            let int_value_after_failed_write =
                observe_property(cx, ObjectValue(object.get()), c"int_value");
            assert_eq!(
                int_value_after_failed_write,
                Observed::Number(-7.0),
                "a readonly property must ignore assignment"
            );

            // A non-configurable property cannot be deleted; the operation reports failure and
            // the property remains visible.
            let mut delete_result = ObjectOpResult::default();
            let delete_attempt = JS_DeleteProperty(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                &mut delete_result,
            );
            assert!(delete_attempt, "a failed delete must not throw");
            assert!(
                !delete_result.ok(),
                "deleting a non-configurable property must fail"
            );
            let int_value_after_failed_delete =
                observe_property(cx, ObjectValue(object.get()), c"int_value");
            assert_eq!(
                int_value_after_failed_delete,
                Observed::Number(-7.0),
                "a non-configurable property must survive deletion"
            );
            rooted!(&in(cx) let own_id = id_for(cx, _global, "'uint_value'"));
            let mut is_none = true;
            assert!(JS_GetOwnPropertyDescriptorById(
                cx,
                object.handle(),
                own_id.handle().into(),
                desc.handle_mut().into(),
                &mut is_none,
            ));
            assert!(!is_none);
            let uint_descriptor_value = observe(cx, desc.get().value_);

            rooted!(&in(cx) let mut holder_obj: *mut js::jsapi::JSObject = ptr::null_mut());
            let mut is_none = true;
            assert!(JS_GetPropertyDescriptor(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                desc.handle_mut().into(),
                holder_obj.handle_mut().into(),
                &mut is_none,
            ));
            assert!(!is_none);
            let holder_is_object = holder_obj.get() == object.get();
            assert!(holder_is_object);
            rooted!(&in(cx) let prop_id = id_for(cx, _global, "'double_value'"));
            let mut is_none = true;
            assert!(JS_GetPropertyDescriptorById(
                cx,
                object.handle(),
                prop_id.handle().into(),
                desc.handle_mut().into(),
                holder_obj.handle_mut().into(),
                &mut is_none,
            ));
            assert!(!is_none);
            let double_descriptor_value = observe(cx, desc.get().value_);

            let missing_name = c"missing_value".as_ptr();
            let mut missing_desc = true;
            assert!(JS_GetOwnPropertyDescriptor(
                cx,
                object.handle(),
                missing_name,
                desc.handle_mut().into(),
                &mut missing_desc,
            ));
            assert!(missing_desc);
            rooted!(&in(cx) let missing_name_value = eval(cx, _global, "'missing_value'"));
            rooted!(&in(cx) let mut missing_id = jsid::default());
            assert!(JS_ValueToId(
                cx,
                missing_name_value.handle().into(),
                missing_id.handle_mut().into()
            ));
            let mut missing_desc = true;
            assert!(JS_GetOwnPropertyDescriptorById(
                cx,
                object.handle(),
                missing_id.handle().into(),
                desc.handle_mut().into(),
                &mut missing_desc,
            ));
            assert!(missing_desc);
            let mut missing_desc = true;
            assert!(JS_GetPropertyDescriptor(
                cx,
                object.handle(),
                missing_name,
                desc.handle_mut().into(),
                holder_obj.handle_mut().into(),
                &mut missing_desc,
            ));
            assert!(missing_desc);
            let mut missing_desc = true;
            assert!(JS_GetPropertyDescriptorById(
                cx,
                object.handle(),
                missing_id.handle().into(),
                desc.handle_mut().into(),
                holder_obj.handle_mut().into(),
                &mut missing_desc,
            ));
            assert!(missing_desc);

            rooted!(&in(cx) let mut prototype: *mut js::jsapi::JSObject = ptr::null_mut());
            assert!(JS_GetPrototype(
                cx,
                object.handle().into(),
                prototype.handle_mut().into()
            ));
            assert!(!prototype.get().is_null());
            assert_ne!(prototype.get(), proto.get());
            let prototype_before_set = prototype.get();
            rooted!(&in(cx) let plain_for_proto = JS_NewPlainObject(cx));
            assert!(JS_GetPrototype(
                cx,
                plain_for_proto.handle().into(),
                prototype.handle_mut().into()
            ));
            assert_eq!(prototype.get(), prototype_before_set);
            assert!(JS_SetPrototype(
                cx,
                object.handle().into(),
                proto.handle().into()
            ));
            assert!(JS_GetPrototype(
                cx,
                object.handle().into(),
                prototype.handle_mut().into()
            ));
            let prototype_replaced = prototype.get() == proto.get();
            assert!(prototype_replaced);

            rooted!(&in(cx) let number = Int32Value(3));
            rooted!(&in(cx) let mut boxed: *mut js::jsapi::JSObject = ptr::null_mut());
            assert!(JS_ValueToObject(
                cx,
                number.handle().into(),
                boxed.handle_mut().into(),
            ));
            let boxed_number_is_object = !boxed.get().is_null();
            assert!(boxed_number_is_object);
            set_global(cx, _global, c"boxed", ObjectValue(boxed.get()));
            let boxed_value = eval_observe(cx, _global, "globalThis.boxed.valueOf()");
            assert_eq!(boxed_value, Observed::Number(3.0));

            let constructor = eval(cx, _global, "(function C(v) { this.v = v; })");
            rooted!(&in(cx) let constructor = constructor);
            rooted!(&in(cx) let new_target = constructor.to_object());
            rooted!(&in(cx) let mut constructed: *mut js::jsapi::JSObject = ptr::null_mut());
            rooted!(&in(cx) let args = vec![Int32Value(31)]);
            assert!(Construct(
                cx,
                constructor.handle().into(),
                new_target.handle(),
                &HandleValueArray::from(&args),
                constructed.handle_mut().into(),
            ));
            assert!(!constructed.get().is_null());
            set_global(cx, _global, c"constructed", ObjectValue(constructed.get()));
            let constructed_instanceof_object =
                eval_observe(cx, _global, "globalThis.constructed instanceof Object");
            let constructed_value = eval_observe(cx, _global, "globalThis.constructed.v");
            assert_eq!(constructed_value, Observed::Number(31.0));

            rooted!(&in(cx) let mut rval = UndefinedValue());
            assert!(JS_CallFunctionName(
                cx,
                object.handle(),
                c"native_fun".as_ptr(),
                &HandleValueArray::empty(),
                rval.handle_mut().into(),
            ));
            let native_fun_result = observe(cx, rval.get());
            assert_eq!(native_fun_result, Observed::Number(7.0));

            let object_value = ObjectValue(object.get());
            let object_value_answer = eval_observe(cx, _global, "o.object_value.answer");
            assert_eq!(object_value_answer, Observed::Number(41.0));
            let string_value = observe_property(cx, object_value, c"string_value");
            assert_eq!(string_value, Observed::String("native string".to_owned()));
            let int_value = observe_property(cx, object_value, c"int_value");
            assert_eq!(int_value, Observed::Number(-7.0));
            let uint_value = observe_property(cx, object_value, c"uint_value");
            assert_eq!(uint_value, Observed::Number(u32::MAX as f64));
            let double_value = observe_property(cx, object_value, c"double_value");
            assert_eq!(double_value, Observed::Number(1.5));
            let unicode = eval_observe(cx, _global, "o.ünicode");
            assert_eq!(unicode, Observed::Number(12.0));
            let desc_id = observe_property(cx, object_value, c"desc_id");
            assert_eq!(desc_id, Observed::Number(99.0));
            let id_string_value = observe_property(cx, object_value, c"id_string_value");
            assert_eq!(
                id_string_value,
                Observed::String("native string".to_owned())
            );
            let by_id_set_after_delete = observe_property(cx, object_value, c"by_id_set");
            assert_eq!(by_id_set_after_delete, Observed::Undefined);
            let by_id_fun_after_delete = observe_property(cx, object_value, c"by_id_fun");
            assert_eq!(by_id_fun_after_delete, Observed::Undefined);
            let element_0 = observe_property(cx, object_value, c"0");
            assert_eq!(element_0, Observed::Number(5.0));
            let uc_property_set = observe_property(cx, object_value, c"uc_set");
            assert_eq!(uc_property_set, Observed::Number(22.0));
            Observation::new(vec![
                ("object_accessor", object_accessor),
                ("seen_after_set", seen_after_set),
                ("id_object_answer", id_object_answer),
                ("id_accessor", id_accessor),
                ("uc_property_get", uc_get),
                ("object_value_answer", object_value_answer),
                ("string_value", string_value),
                ("int_value", int_value),
                ("uint_value", uint_value),
                ("double_value", double_value),
                ("unicode", unicode),
                ("desc_id", desc_id),
                ("id_string_value", id_string_value),
                ("by_id_set_after_delete", by_id_set_after_delete),
                ("native_fun_result", native_fun_result),
                ("by_id_fun_after_delete", by_id_fun_after_delete),
                ("element_0", element_0),
                ("uc_property_set", uc_property_set),
                (
                    "constructed_instanceof_object",
                    constructed_instanceof_object,
                ),
                ("constructed_value", constructed_value),
                ("boxed_value", boxed_value),
                ("int_descriptor_value", int_descriptor_value),
                ("int_value_after_failed_write", int_value_after_failed_write),
                (
                    "int_value_after_failed_delete",
                    int_value_after_failed_delete,
                ),
                (
                    "int_descriptor_writable",
                    Observed::Boolean(int_descriptor_writable),
                ),
                (
                    "int_descriptor_enumerable",
                    Observed::Boolean(int_descriptor_enumerable),
                ),
                (
                    "int_descriptor_configurable",
                    Observed::Boolean(int_descriptor_configurable),
                ),
                ("uint_descriptor_value", uint_descriptor_value),
                ("prototype_replaced", Observed::Boolean(prototype_replaced)),
                (
                    "boxed_number_is_object",
                    Observed::Boolean(boxed_number_is_object),
                ),
                ("holder_is_object", Observed::Boolean(holder_is_object)),
                ("double_descriptor_value", double_descriptor_value),
            ])
        },
    );
}

unsafe fn id_for(cx: &mut JSContext, global: HandleObject, source: &str) -> jsid {
    rooted!(&in(cx) let value = eval(cx, global, source));
    rooted!(&in(cx) let mut id = jsid::default());
    assert!(JS_ValueToId(
        cx,
        value.handle().into(),
        id.handle_mut().into()
    ));
    id.get()
}

unsafe extern "C" fn native_getter(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval().set(Int32Value(11));
    true
}

unsafe extern "C" fn native_setter(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval().set(UndefinedValue());
    true
}

unsafe extern "C" fn native_seven(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
    let args = CallArgs::from_vp(vp, argc);
    args.rval().set(Int32Value(7));
    true
}
