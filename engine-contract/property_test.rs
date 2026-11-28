/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Property definition, lookup, deletion, descriptors, prototypes and calls.

use std::ptr;

use js::jsapi::{
    CallArgs, HandleValueArray, JSContext as RawJSContext, JSPROP_ENUMERATE, JSPROP_PERMANENT,
    JSPROP_READONLY, ObjectOpResult, PropertyDescriptor, Value, jsid,
};
use js::jsval::{DoubleValue, Int32Value, ObjectValue, UndefinedValue};
use js::rooted;
use js::rust::JSEngine;
use js::rust::wrappers2::{
    Construct, GetObjectProto, GetPropertyKeys, JS_AlreadyHasOwnPropertyById, JS_CallFunctionName,
    JS_DefineElement, JS_DefineFunction, JS_DefineFunctionById, JS_DefineProperties,
    JS_DefineProperty, JS_DefineProperty1, JS_DefineProperty2, JS_DefineProperty3,
    JS_DefineProperty4, JS_DefineProperty5, JS_DefineProperty6, JS_DefineProperty7,
    JS_DefinePropertyById, JS_DefinePropertyById1, JS_DefinePropertyById2, JS_DefinePropertyById3,
    JS_DefinePropertyById4, JS_DefinePropertyById5, JS_DefinePropertyById6, JS_DefinePropertyById7,
    JS_DefinePropertyById8, JS_DefineUCProperty, JS_DefineUCProperty2, JS_DeleteProperty,
    JS_DeletePropertyById, JS_ForwardGetPropertyTo, JS_ForwardSetPropertyTo, JS_FreezeObject,
    JS_GetElement, JS_GetOwnPropertyDescriptor, JS_GetOwnPropertyDescriptorById,
    JS_GetPropertyById, JS_GetPropertyDescriptor, JS_GetPropertyDescriptorById, JS_GetPrototype,
    JS_GetUCProperty, JS_HasOwnProperty, JS_HasOwnPropertyById, JS_HasProperty, JS_HasPropertyById,
    JS_NewObject, JS_NewObjectWithGivenProto, JS_NewPlainObject, JS_SetElement,
    JS_SetImmutablePrototype, JS_SetProperty, JS_SetPropertyById, JS_SetPrototype,
    JS_SetUCProperty, JS_ValueToId, JS_ValueToObject, SameValue, SetDataPropertyDescriptor,
    SetPropertyIgnoringNamedGetter,
};

use crate::support::{
    Observation, Value as Observed, eval, eval_observe, id_for, observe, observe_property,
    record_case, set_global,
};

const ATTRS: u32 = (JSPROP_ENUMERATE | JSPROP_PERMANENT | JSPROP_READONLY) as u32;

fn property_semantics_get_object_proto(engine: &JSEngine) {
    record_case(
        engine,
        "property.get_object_proto",
        &[
            "GetObjectProto",
            "JS_NewPlainObject",
            "JS_SetPrototype",
            "JS_GetProperty",
        ],
        |cx, _global| unsafe {
            rooted!(&in(cx) let proto = JS_NewPlainObject(cx));
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let seven = Int32Value(7));
            assert!(js::rust::wrappers2::JS_SetProperty(
                cx,
                proto.handle(),
                c"marker".as_ptr(),
                seven.handle().into(),
            ));
            assert!(JS_SetPrototype(
                cx,
                object.handle().into(),
                proto.handle().into()
            ));
            rooted!(&in(cx) let mut got = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(GetObjectProto(
                cx,
                object.handle().into(),
                got.handle_mut().into()
            ));
            rooted!(&in(cx) let got = got.get());
            rooted!(&in(cx) let mut marker = UndefinedValue());
            assert!(js::rust::wrappers2::JS_GetProperty(
                cx,
                got.handle(),
                c"marker".as_ptr(),
                marker.handle_mut().into()
            ));
            assert_eq!(marker.get().to_int32(), 7);
            Observation::new(vec![
                ("same_proto", Observed::Boolean(got.get() == proto.get())),
                (
                    "proto_marker",
                    Observed::Number(marker.get().to_int32() as f64),
                ),
            ])
        },
    );
}

fn property_semantics_get_property_keys(engine: &JSEngine) {
    record_case(
        engine,
        "property.get_property_keys",
        &[
            "GetPropertyKeys",
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_DefineElement",
        ],
        |cx, _global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let one = Int32Value(1));
            assert!(js::rust::wrappers2::JS_SetProperty(
                cx,
                object.handle(),
                c"named".as_ptr(),
                one.handle().into()
            ));
            assert!(JS_DefineElement(
                cx,
                object.handle(),
                0,
                one.handle().into(),
                ATTRS
            ));
            let mut ids = js::rust::IdVector::new(cx);
            assert!(GetPropertyKeys(
                cx,
                object.handle().into(),
                js::jsapi::JSITER_OWNONLY,
                ids.handle_mut()
            ));
            assert_eq!(ids.len(), 2);
            assert!(ids[0].is_int() && ids[0].to_int() == 0);
            Observation::new(vec![
                ("key_count", Observed::Number(ids.len() as f64)),
                ("element_first", Observed::Boolean(ids[0].is_int())),
                ("named_second", Observed::Boolean(ids[1].is_string())),
            ])
        },
    );
}

fn property_semantics_get_and_set_property(engine: &JSEngine) {
    record_case(
        engine,
        "property.get_and_set_property",
        &["JS_NewPlainObject", "JS_SetProperty", "JS_GetProperty"],
        |cx, _global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let nineteen = Int32Value(19));
            assert!(js::rust::wrappers2::JS_SetProperty(
                cx,
                object.handle(),
                c"value".as_ptr(),
                nineteen.handle().into()
            ));
            rooted!(&in(cx) let mut out = UndefinedValue());
            assert!(js::rust::wrappers2::JS_GetProperty(
                cx,
                object.handle(),
                c"value".as_ptr(),
                out.handle_mut().into()
            ));
            assert_eq!(out.get().to_int32(), 19);
            Observation::new(vec![(
                "value",
                Observed::Number(out.get().to_int32() as f64),
            )])
        },
    );
}

fn property_semantics_define_properties(engine: &JSEngine) {
    record_case(
        engine,
        "property.define_properties",
        &[
            "JS_DefineElement",
            "JS_DefinePropertyById",
            "JS_DefinePropertyById2",
            "JS_DefinePropertyById5",
            "JS_DefineUCProperty2",
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));

            rooted!(&in(cx) let one = Int32Value(1));
            assert!(JS_DefineElement(
                cx,
                object.handle(),
                0,
                one.handle().into(),
                ATTRS
            ));

            let getter = eval(cx, global, "(function(){ return 2; })").to_object();
            let _ = getter;
            let mut desc = PropertyDescriptor::default();
            desc.set_hasValue_(true);
            desc.value_ = Int32Value(3);
            desc.set_hasWritable_(true);
            desc.set_writable_(true);
            desc.set_hasEnumerable_(true);
            desc.set_enumerable_(true);
            desc.set_hasConfigurable_(true);
            desc.set_configurable_(true);
            rooted!(&in(cx) let mut desc = desc);
            let id = id_for(cx, global, "'by_id'");
            rooted!(&in(cx) let id = id);
            let mut result = ObjectOpResult::default();
            assert!(JS_DefinePropertyById(
                cx,
                object.handle(),
                id.handle().into(),
                desc.handle().into(),
                &mut result
            ));
            assert!(result.ok());

            rooted!(&in(cx) let five = Int32Value(5));
            assert!(JS_DefinePropertyById2(
                cx,
                object.handle(),
                id.handle().into(),
                five.handle().into(),
                ATTRS
            ));

            let id2 = id_for(cx, global, "'owned'");
            rooted!(&in(cx) let id2 = id2);
            rooted!(&in(cx) let owned = JS_NewPlainObject(cx));
            assert!(JS_DefinePropertyById5(
                cx,
                object.handle(),
                id2.handle().into(),
                owned.handle(),
                ATTRS
            ));

            let uc: Vec<u16> = "unicodé".encode_utf16().collect();
            rooted!(&in(cx) let uc_value = Int32Value(9));
            assert!(JS_DefineUCProperty2(
                cx,
                object.handle(),
                uc.as_ptr(),
                uc.len(),
                uc_value.handle().into(),
                ATTRS
            ));

            set_global(cx, global, c"defined", ObjectValue(object.get()));
            set_global(cx, global, c"defined_owned", ObjectValue(owned.get()));
            assert!(eval(cx, global, "defined[0] === 1").to_boolean());
            assert!(eval(cx, global, "defined.by_id === 5").to_boolean());
            assert!(eval(cx, global, "defined.owned === defined_owned").to_boolean());
            assert!(eval(cx, global, "defined['unicodé'] === 9").to_boolean());
            Observation::new(vec![
                ("element", Observed::Number(1.0)),
                ("by_id", Observed::Number(5.0)),
                ("owned", Observed::Object),
                ("unicode", Observed::Number(9.0)),
            ])
        },
    );
}

fn property_semantics_delete_property_by_id(engine: &JSEngine) {
    record_case(
        engine,
        "property.delete_property_by_id",
        &[
            "JS_DeletePropertyById",
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(33));
            assert!(JS_SetProperty(
                cx,
                object.handle(),
                c"gone".as_ptr(),
                value.handle().into()
            ));
            let id = id_for(cx, global, "'gone'");
            rooted!(&in(cx) let id = id);
            let mut result = ObjectOpResult::default();
            assert!(JS_DeletePropertyById(
                cx,
                object.handle(),
                id.handle().into(),
                &mut result
            ));
            assert!(result.ok());
            let gone = observe_property(cx, ObjectValue(object.get()), c"gone");
            assert_eq!(gone, Observed::Undefined);
            Observation::new(vec![
                ("deleted", Observed::Boolean(result.ok())),
                ("gone", Observed::Undefined),
            ])
        },
    );
}

fn property_semantics_elements(engine: &JSEngine) {
    record_case(
        engine,
        "property.elements",
        &[
            "JS_DefineElement",
            "JS_GetElement",
            "JS_NewPlainObject",
            "JS_SetElement",
        ],
        |cx, _global| unsafe {
            rooted!(&in(cx) let defined = JS_NewPlainObject(cx));
            rooted!(&in(cx) let three = Int32Value(3));
            assert!(JS_DefineElement(
                cx,
                defined.handle(),
                1,
                three.handle().into(),
                ATTRS
            ));
            rooted!(&in(cx) let mut defined_out = UndefinedValue());
            assert!(JS_GetElement(
                cx,
                defined.handle(),
                1,
                defined_out.handle_mut().into()
            ));
            assert_eq!(defined_out.get().to_int32(), 3);

            rooted!(&in(cx) let set = JS_NewPlainObject(cx));
            rooted!(&in(cx) let four = Int32Value(4));
            assert!(JS_SetElement(cx, set.handle(), 2, four.handle().into()));
            rooted!(&in(cx) let mut set_out = UndefinedValue());
            assert!(JS_GetElement(
                cx,
                set.handle(),
                2,
                set_out.handle_mut().into()
            ));
            assert_eq!(set_out.get().to_int32(), 4);
            Observation::new(vec![
                (
                    "defined_element",
                    Observed::Number(defined_out.get().to_int32() as f64),
                ),
                (
                    "set_element",
                    Observed::Number(set_out.get().to_int32() as f64),
                ),
            ])
        },
    );
}

fn property_semantics_property_by_id(engine: &JSEngine) {
    record_case(
        engine,
        "property.property_by_id",
        &[
            "JS_GetOwnPropertyDescriptorById",
            "JS_GetPropertyById",
            "JS_GetPropertyDescriptorById",
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(20));
            assert!(JS_SetProperty(
                cx,
                object.handle(),
                c"id_prop".as_ptr(),
                value.handle().into()
            ));
            let id = id_for(cx, global, "'id_prop'");
            rooted!(&in(cx) let id = id);
            rooted!(&in(cx) let mut got = UndefinedValue());
            assert!(JS_GetPropertyById(
                cx,
                object.handle(),
                id.handle().into(),
                got.handle_mut().into()
            ));
            assert_eq!(got.get().to_int32(), 20);

            rooted!(&in(cx) let mut own = PropertyDescriptor::default());
            let mut is_none = true;
            assert!(JS_GetOwnPropertyDescriptorById(
                cx,
                object.handle(),
                id.handle().into(),
                own.handle_mut().into(),
                &mut is_none
            ));
            assert!(!is_none);
            let own_value = own.value_.to_int32();
            assert_eq!(own_value, 20);

            rooted!(&in(cx) let mut inherited = PropertyDescriptor::default());
            rooted!(&in(cx) let mut holder = ptr::null_mut::<js::jsapi::JSObject>());
            let mut is_none = true;
            assert!(JS_GetPropertyDescriptorById(
                cx,
                object.handle(),
                id.handle().into(),
                inherited.handle_mut().into(),
                holder.handle_mut().into(),
                &mut is_none
            ));
            assert!(!is_none);
            assert_eq!(holder.get(), object.get());
            Observation::new(vec![
                ("by_id_value", Observed::Number(got.get().to_int32() as f64)),
                ("own_descriptor", Observed::Number(own_value as f64)),
                (
                    "holder_same",
                    Observed::Boolean(holder.get() == object.get()),
                ),
            ])
        },
    );
}

fn property_semantics_object_creation_and_prototypes(engine: &JSEngine) {
    record_case(
        engine,
        "property.object_creation_and_prototypes",
        &[
            "JS_GetPrototype",
            "JS_NewObject",
            "JS_NewObjectWithGivenProto",
            "JS_NewPlainObject",
            "JS_SetImmutablePrototype",
            "JS_SetProperty",
            "JS_SetPrototype",
        ],
        |cx, _global| unsafe {
            rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
            let plain_is_object = !plain.get().is_null();
            assert!(plain_is_object);

            rooted!(&in(cx) let created = JS_NewObject(cx, ptr::null()));
            assert!(!created.get().is_null());
            rooted!(&in(cx) let created_value = Int32Value(27));
            assert!(JS_SetProperty(
                cx,
                created.handle(),
                c"created".as_ptr(),
                created_value.handle().into()
            ));
            assert_eq!(
                observe_property(cx, ObjectValue(created.get()), c"created"),
                Observed::Number(27.0)
            );

            rooted!(&in(cx) let proto = JS_NewPlainObject(cx));
            rooted!(&in(cx) let given =
                JS_NewObjectWithGivenProto(cx, ptr::null(), proto.handle().into()));
            assert!(!given.get().is_null());
            rooted!(&in(cx) let mut got = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_GetPrototype(
                cx,
                given.handle().into(),
                got.handle_mut().into()
            ));
            assert_eq!(got.get(), proto.get());

            assert!(JS_SetPrototype(
                cx,
                created.handle().into(),
                proto.handle().into()
            ));
            assert!(JS_GetPrototype(
                cx,
                created.handle().into(),
                got.handle_mut().into()
            ));
            let replaced = got.get() == proto.get();
            assert!(replaced);

            let mut immutable = false;
            assert!(JS_SetImmutablePrototype(
                cx,
                plain.handle().into(),
                &mut immutable
            ));
            assert!(immutable);
            Observation::new(vec![
                ("plain", Observed::Boolean(plain_is_object)),
                (
                    "created",
                    observe_property(cx, ObjectValue(created.get()), c"created"),
                ),
                ("given_proto", Observed::Boolean(!given.get().is_null())),
                ("prototype_replaced", Observed::Boolean(replaced)),
                ("immutable", Observed::Boolean(immutable)),
            ])
        },
    );
}

fn property_semantics_define_property_variants(engine: &JSEngine) {
    record_case(
        engine,
        "property.define_property_variants",
        &[
            "JS_DefineProperty3",
            "JS_DefineProperties",
            "JS_DefineProperty4",
            "JS_DefineProperty5",
            "JS_DefinePropertyById",
            "JS_DefinePropertyById2",
            "JS_DefinePropertyById5",
            "JS_DefineUCProperty2",
            "JS_NewObject",
            "JS_NewPlainObject",
            "JS_NewStringCopyN",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));

            rooted!(&in(cx) let three_value = JS_NewObject(cx, ptr::null()));
            assert!(JS_DefineProperty3(
                cx,
                object.handle(),
                c"object_value".as_ptr(),
                three_value.handle(),
                ATTRS
            ));
            set_global(cx, global, c"object_value", ObjectValue(three_value.get()));

            rooted!(&in(cx) let string =
                js::rust::wrappers2::JS_NewStringCopyN(cx, b"four".as_ptr().cast(), 4));
            assert!(JS_DefineProperty4(
                cx,
                object.handle(),
                c"string_value".as_ptr(),
                string.handle().into(),
                ATTRS
            ));
            assert!(JS_DefineProperty5(
                cx,
                object.handle(),
                c"int_value".as_ptr(),
                -7,
                ATTRS
            ));

            let by_id = id_for(cx, global, "'by_id'");
            rooted!(&in(cx) let by_id = by_id);
            rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
            rooted!(&in(cx) let by_id_value = Int32Value(12));
            SetDataPropertyDescriptor(desc.handle_mut(), by_id_value.handle(), ATTRS);
            let mut result = ObjectOpResult::default();
            assert!(JS_DefinePropertyById(
                cx,
                object.handle(),
                by_id.handle().into(),
                desc.handle().into(),
                &mut result
            ));
            assert!(result.ok());

            let by_id2 = id_for(cx, global, "'by_id2'");
            rooted!(&in(cx) let by_id2 = by_id2);
            rooted!(&in(cx) let by_id2_value = Int32Value(13));
            assert!(JS_DefinePropertyById2(
                cx,
                object.handle(),
                by_id2.handle().into(),
                by_id2_value.handle().into(),
                ATTRS
            ));

            let by_id5 = id_for(cx, global, "'by_id5'");
            rooted!(&in(cx) let by_id5 = by_id5);
            rooted!(&in(cx) let by_id5_value = JS_NewPlainObject(cx));
            assert!(JS_DefinePropertyById5(
                cx,
                object.handle(),
                by_id5.handle().into(),
                by_id5_value.handle(),
                ATTRS
            ));

            let uc: Vec<u16> = "unicodé".encode_utf16().collect();
            rooted!(&in(cx) let uc_value = Int32Value(14));
            assert!(JS_DefineUCProperty2(
                cx,
                object.handle(),
                uc.as_ptr(),
                uc.len(),
                uc_value.handle().into(),
                ATTRS
            ));

            unsafe extern "C" fn seven(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
                CallArgs::from_vp(vp, argc).rval().set(Int32Value(7));
                true
            }
            let accessor = |op| js::jsapi::JSPropertySpec_Accessor {
                native: js::jsapi::JSNativeWrapper {
                    op,
                    info: ptr::null(),
                },
            };
            let specs = [
                js::jsapi::JSPropertySpec {
                    name: js::jsapi::JSPropertySpec_Name {
                        string_: c"answer".as_ptr(),
                    },
                    attributes_: JSPROP_ENUMERATE as u8,
                    kind_: js::jsapi::JSPropertySpec_Kind::NativeAccessor,
                    u: js::jsapi::JSPropertySpec_AccessorsOrValue {
                        accessors: js::jsapi::JSPropertySpec_AccessorsOrValue_Accessors {
                            getter: accessor(Some(seven)),
                            setter: accessor(Some(seven)),
                        },
                    },
                },
                js::jsapi::JSPropertySpec::ZERO,
            ];
            assert!(JS_DefineProperties(cx, object.handle(), specs.as_ptr()));

            set_global(cx, global, c"variants", ObjectValue(object.get()));
            set_global(cx, global, c"by_id5", ObjectValue(by_id5_value.get()));
            assert!(eval(cx, global, "variants.object_value === object_value").to_boolean());
            assert!(eval(cx, global, "variants.string_value === 'four'").to_boolean());
            assert!(eval(cx, global, "variants.int_value === -7").to_boolean());
            assert!(eval(cx, global, "variants.by_id === 12").to_boolean());
            assert!(eval(cx, global, "variants.by_id2 === 13").to_boolean());
            assert!(eval(cx, global, "variants.by_id5 === by_id5").to_boolean());
            assert!(eval(cx, global, "variants['unicodé'] === 14").to_boolean());
            assert!(eval(cx, global, "variants.answer === 7").to_boolean());
            Observation::new(vec![
                ("string_value", Observed::String("four".into())),
                ("int_value", Observed::Number(-7.0)),
                ("by_id", Observed::Number(12.0)),
                ("by_id2", Observed::Number(13.0)),
                ("by_id5", Observed::Object),
                ("unicode", Observed::Number(14.0)),
                ("answer", Observed::Number(7.0)),
            ])
        },
    );
}

fn property_semantics_value_to_object(engine: &JSEngine) {
    record_case(
        engine,
        "property.value_to_object",
        &["JS_ValueToObject"],
        |cx, global| unsafe {
            rooted!(&in(cx) let number = Int32Value(3));
            rooted!(&in(cx) let mut boxed = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_ValueToObject(
                cx,
                number.handle().into(),
                boxed.handle_mut().into()
            ));
            assert!(!boxed.get().is_null());
            set_global(cx, global, c"boxed", ObjectValue(boxed.get()));
            assert!(eval(cx, global, "boxed.valueOf() === 3").to_boolean());

            rooted!(&in(cx) let string = eval(cx, global, "'abc'"));
            rooted!(&in(cx) let mut boxed_string = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_ValueToObject(
                cx,
                string.handle().into(),
                boxed_string.handle_mut().into()
            ));
            set_global(cx, global, c"boxed_string", ObjectValue(boxed_string.get()));
            assert!(eval(cx, global, "boxed_string.length === 3").to_boolean());

            Observation::new(vec![
                ("boxed_number", eval_observe(cx, global, "boxed.valueOf()")),
                (
                    "boxed_string_length",
                    eval_observe(cx, global, "boxed_string.length"),
                ),
            ])
        },
    );
}

fn property_semantics_construct(engine: &JSEngine) {
    record_case(
        engine,
        "property.construct",
        &["Construct"],
        |cx, global| unsafe {
            rooted!(&in(cx) let constructor = eval(cx, global, "(function C(v) { this.v = v; })"));
            rooted!(&in(cx) let new_target = constructor.to_object());
            rooted!(&in(cx) let mut constructed = ptr::null_mut::<js::jsapi::JSObject>());
            rooted!(&in(cx) let args = vec![Int32Value(31)]);
            assert!(Construct(
                cx,
                constructor.handle().into(),
                new_target.handle(),
                &HandleValueArray::from(&args),
                constructed.handle_mut().into()
            ));
            assert!(!constructed.get().is_null());
            set_global(cx, global, c"constructed", ObjectValue(constructed.get()));
            assert!(
                eval(cx, global, "constructed instanceof constructed.constructor").to_boolean()
            );
            let value = eval_observe(cx, global, "constructed.v");
            assert_eq!(value, Observed::Number(31.0));
            Observation::new(vec![
                ("constructed_value", value),
                (
                    "instance_of_object",
                    eval_observe(cx, global, "constructed instanceof Object"),
                ),
            ])
        },
    );
}

fn property_semantics_call_function_name(engine: &JSEngine) {
    record_case(
        engine,
        "property.call_function_name",
        &[
            "JS_CallFunctionName",
            "JS_DefineFunction",
            "JS_NewPlainObject",
        ],
        |cx, _global| unsafe {
            unsafe extern "C" fn seven(_cx: *mut RawJSContext, argc: u32, vp: *mut Value) -> bool {
                CallArgs::from_vp(vp, argc).rval().set(Int32Value(7));
                true
            }
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            assert!(
                !JS_DefineFunction(
                    cx,
                    object.handle(),
                    c"native_fun".as_ptr(),
                    Some(seven),
                    0,
                    JSPROP_ENUMERATE as u32
                )
                .is_null()
            );
            rooted!(&in(cx) let mut rval = UndefinedValue());
            assert!(JS_CallFunctionName(
                cx,
                object.handle(),
                c"native_fun".as_ptr(),
                &HandleValueArray::empty(),
                rval.handle_mut().into()
            ));
            let observed = observe(cx, rval.get());
            assert_eq!(observed, Observed::Number(7.0));
            Observation::new(vec![("result", observed)])
        },
    );
}

fn property_semantics_forward_and_ignore(engine: &JSEngine) {
    record_case(
        engine,
        "property.forward_and_ignore",
        &[
            "JS_ForwardGetPropertyTo",
            "JS_ForwardSetPropertyTo",
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_ValueToId",
            "SetPropertyIgnoringNamedGetter",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let target = JS_NewPlainObject(cx));
            rooted!(&in(cx) let receiver = JS_NewPlainObject(cx));
            rooted!(&in(cx) let marker = Int32Value(15));
            assert!(JS_SetProperty(
                cx,
                receiver.handle(),
                c"marker".as_ptr(),
                marker.handle().into()
            ));
            let marker_id = id_for(cx, global, "'marker'");
            rooted!(&in(cx) let marker_id = marker_id);
            rooted!(&in(cx) let receiver_value = ObjectValue(receiver.get()));
            rooted!(&in(cx) let mut forwarded = UndefinedValue());
            assert!(JS_ForwardGetPropertyTo(
                cx,
                receiver.handle(),
                marker_id.handle().into(),
                receiver_value.handle().into(),
                forwarded.handle_mut().into()
            ));
            assert_eq!(forwarded.get().to_int32(), 15);

            rooted!(&in(cx) let sixteen = Int32Value(16));
            let mut set_result = ObjectOpResult::default();
            assert!(JS_ForwardSetPropertyTo(
                cx,
                receiver.handle(),
                marker_id.handle().into(),
                sixteen.handle().into(),
                receiver_value.handle().into(),
                &mut set_result
            ));
            assert!(set_result.ok());
            assert_eq!(
                observe_property(cx, ObjectValue(receiver.get()), c"marker"),
                Observed::Number(16.0)
            );

            rooted!(&in(cx) let readonly = JS_NewPlainObject(cx));
            rooted!(&in(cx) let five = Int32Value(5));
            assert!(JS_SetProperty(
                cx,
                readonly.handle(),
                c"readonly".as_ptr(),
                five.handle().into()
            ));
            let readonly_id = id_for(cx, global, "'readonly'");
            rooted!(&in(cx) let readonly_id = readonly_id);
            rooted!(&in(cx) let nine = Int32Value(9));
            rooted!(&in(cx) let receiver_value = ObjectValue(readonly.get()));
            let mut desc = PropertyDescriptor::default();
            desc.set_hasValue_(true);
            desc.value_ = Int32Value(5);
            desc.set_hasWritable_(true);
            desc.set_writable_(false);
            rooted!(&in(cx) let desc = desc);
            let mut ignored_result = ObjectOpResult::default();
            assert!(SetPropertyIgnoringNamedGetter(
                cx,
                readonly.handle().into(),
                readonly_id.handle().into(),
                nine.handle().into(),
                receiver_value.handle().into(),
                Some(desc.handle()),
                &mut ignored_result
            ));
            assert!(!ignored_result.ok());
            Observation::new(vec![
                ("forward_get", Observed::Number(15.0)),
                ("forward_set", Observed::Number(16.0)),
                (
                    "ignored_write_rejected",
                    Observed::Boolean(!ignored_result.ok()),
                ),
            ])
        },
    );
}

fn property_semantics_descriptors_and_prototypes(engine: &JSEngine) {
    record_case(
        engine,
        "property.descriptors_and_prototypes",
        &[
            "JS_DeletePropertyById",
            "JS_FreezeObject",
            "JS_GetOwnPropertyDescriptorById",
            "JS_GetPropertyDescriptorById",
            "JS_GetPrototype",
            "JS_NewPlainObject",
            "JS_SetImmutablePrototype",
            "JS_SetPrototype",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(21));
            assert!(js::rust::wrappers2::JS_SetProperty(
                cx,
                object.handle(),
                c"value".as_ptr(),
                value.handle().into()
            ));
            let id = id_for(cx, global, "'value'");
            rooted!(&in(cx) let id = id);

            rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
            let mut is_none = true;
            assert!(JS_GetOwnPropertyDescriptorById(
                cx,
                object.handle(),
                id.handle().into(),
                desc.handle_mut().into(),
                &mut is_none
            ));
            assert!(!is_none);
            let own_value = desc.value_.to_int32();
            assert_eq!(own_value, 21);
            let mut is_none = true;
            rooted!(&in(cx) let mut holder = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_GetPropertyDescriptorById(
                cx,
                object.handle(),
                id.handle().into(),
                desc.handle_mut().into(),
                holder.handle_mut().into(),
                &mut is_none
            ));
            assert!(!is_none);
            assert_eq!(holder.get(), object.get());

            rooted!(&in(cx) let proto = JS_NewPlainObject(cx));
            assert!(JS_SetPrototype(
                cx,
                object.handle().into(),
                proto.handle().into()
            ));
            rooted!(&in(cx) let mut got = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_GetPrototype(
                cx,
                object.handle().into(),
                got.handle_mut().into()
            ));
            assert_eq!(got.get(), proto.get());
            let mut immutable = false;
            assert!(JS_SetImmutablePrototype(
                cx,
                object.handle().into(),
                &mut immutable
            ));
            assert!(immutable);
            assert!(JS_FreezeObject(cx, object.handle()));
            assert!(eval(cx, global, "Object.isFrozen(this.object)").is_boolean());
            let mut delete_result = ObjectOpResult::default();
            assert!(JS_DeletePropertyById(
                cx,
                object.handle(),
                id.handle().into(),
                &mut delete_result
            ));
            assert!(!delete_result.ok());
            Observation::new(vec![
                ("descriptor_value", Observed::Number(own_value as f64)),
                (
                    "holder_same",
                    Observed::Boolean(holder.get() == object.get()),
                ),
                (
                    "prototype_same",
                    Observed::Boolean(got.get() == proto.get()),
                ),
                ("immutable", Observed::Boolean(immutable)),
                (
                    "frozen_delete_rejected",
                    Observed::Boolean(!delete_result.ok()),
                ),
            ])
        },
    );
}

fn property_semantics_forwarding(engine: &JSEngine) {
    record_case(
        engine,
        "property.forwarding",
        &[
            "JS_ForwardGetPropertyTo",
            "JS_ForwardSetPropertyTo",
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_ValueToId",
            "SetDataPropertyDescriptor",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let receiver = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(15));
            assert!(js::rust::wrappers2::JS_SetProperty(
                cx,
                receiver.handle(),
                c"marker".as_ptr(),
                value.handle().into()
            ));
            let id = id_for(cx, global, "'marker'");
            rooted!(&in(cx) let id = id);
            rooted!(&in(cx) let mut forward_get = UndefinedValue());
            rooted!(&in(cx) let receiver_value = ObjectValue(receiver.get()));
            assert!(JS_ForwardGetPropertyTo(
                cx,
                receiver.handle(),
                id.handle().into(),
                receiver_value.handle().into(),
                forward_get.handle_mut().into()
            ));
            assert_eq!(forward_get.get().to_int32(), 15);

            let mut result = ObjectOpResult::default();
            rooted!(&in(cx) let sixteen = Int32Value(16));
            assert!(JS_ForwardSetPropertyTo(
                cx,
                receiver.handle(),
                id.handle().into(),
                sixteen.handle().into(),
                receiver_value.handle().into(),
                &mut result
            ));
            assert!(result.ok());
            assert_eq!(
                observe_property(cx, ObjectValue(receiver.get()), c"marker"),
                Observed::Number(16.0)
            );

            rooted!(&in(cx) let mut desc = PropertyDescriptor::default());
            rooted!(&in(cx) let descriptor_value = Int32Value(17));
            SetDataPropertyDescriptor(desc.handle_mut(), descriptor_value.handle(), ATTRS);
            Observation::new(vec![
                ("forward_get", Observed::Number(15.0)),
                ("forward_set", Observed::Number(16.0)),
            ])
        },
    );
}

fn property_semantics_has_properties(engine: &JSEngine) {
    record_case(
        engine,
        "property.has_properties",
        &[
            "JS_NewPlainObject",
            "JS_SetProperty",
            "JS_HasProperty",
            "JS_HasPropertyById",
            "JS_HasOwnProperty",
            "JS_HasOwnPropertyById",
            "JS_AlreadyHasOwnPropertyById",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(23));
            assert!(js::rust::wrappers2::JS_SetProperty(
                cx,
                object.handle(),
                c"own".as_ptr(),
                value.handle().into()
            ));
            let mut has = false;
            assert!(JS_HasProperty(
                cx,
                object.handle(),
                c"own".as_ptr(),
                &mut has
            ));
            assert!(has);
            let mut has_own = false;
            assert!(JS_HasOwnProperty(
                cx,
                object.handle(),
                c"own".as_ptr(),
                &mut has_own
            ));
            assert!(has_own);
            rooted!(&in(cx) let key = eval(cx, global, "'own'"));
            rooted!(&in(cx) let mut key_id = jsid::default());
            assert!(js::rust::wrappers2::JS_ValueToId(
                cx,
                key.handle().into(),
                key_id.handle_mut().into()
            ));
            let mut has_by_id = false;
            assert!(JS_HasPropertyById(
                cx,
                object.handle(),
                key_id.handle().into(),
                &mut has_by_id
            ));
            let mut own_by_id = false;
            assert!(JS_HasOwnPropertyById(
                cx,
                object.handle(),
                key_id.handle().into(),
                &mut own_by_id
            ));
            let mut already = false;
            assert!(JS_AlreadyHasOwnPropertyById(
                cx,
                object.handle().into(),
                key_id.handle().into(),
                &mut already
            ));
            let mut missing = true;
            assert!(JS_HasProperty(
                cx,
                object.handle(),
                c"missing".as_ptr(),
                &mut missing
            ));
            assert!(!missing);
            Observation::new(vec![
                ("has", Observed::Boolean(has)),
                ("has_own", Observed::Boolean(has_own)),
                ("has_by_id", Observed::Boolean(has_by_id)),
                ("own_by_id", Observed::Boolean(own_by_id)),
                ("already", Observed::Boolean(already)),
                ("missing", Observed::Boolean(missing)),
            ])
        },
    );
}

/// Hand-written native cases derived from the upstream test262 `Object.hasOwn` cases through the native own-property entry points.
///
/// `hasown.js` checks an own built-in method on `Object`; `hasown_inherited_exists.js` checks that
/// an inherited property is not an own property; `hasown_nonexistent.js` checks a missing key.
/// Each source drives the name, id, and already-has-by-id variants and records what JavaScript
/// observes, so a shim returning a constant cannot pass.
fn record_has_own_wpt_cases(engine: &JSEngine) {
    record_case(
        engine,
        "property.wpt_object_has_own",
        &[
            "JS_HasOwnProperty",
            "JS_HasOwnPropertyById",
            "JS_AlreadyHasOwnPropertyById",
            "JS_ValueToObject",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object_value = eval(cx, global, "Object"));
            rooted!(&in(cx) let mut object = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_ValueToObject(
                cx,
                object_value.handle().into(),
                object.handle_mut().into(),
            ));

            let mut own_name = false;
            assert!(JS_HasOwnProperty(
                cx,
                object.handle(),
                c"hasOwn".as_ptr(),
                &mut own_name,
            ));
            let has_own_id = id_for(cx, global, "'hasOwn'");
            rooted!(&in(cx) let has_own_id = has_own_id);
            let mut own_by_id = false;
            assert!(JS_HasOwnPropertyById(
                cx,
                object.handle(),
                has_own_id.handle().into(),
                &mut own_by_id,
            ));
            let mut own_already = false;
            assert!(JS_AlreadyHasOwnPropertyById(
                cx,
                object.handle().into(),
                has_own_id.handle().into(),
                &mut own_already,
            ));
            assert!(own_name && own_by_id && own_already);

            rooted!(&in(cx) let inherited_object =
                eval(cx, global, "Object.create({foo: 42})").to_object());
            let mut inherited_name = true;
            assert!(JS_HasOwnProperty(
                cx,
                inherited_object.handle(),
                c"foo".as_ptr(),
                &mut inherited_name,
            ));
            let foo_id = id_for(cx, global, "'foo'");
            rooted!(&in(cx) let foo_id = foo_id);
            let mut inherited_by_id = true;
            assert!(JS_HasOwnPropertyById(
                cx,
                inherited_object.handle(),
                foo_id.handle().into(),
                &mut inherited_by_id,
            ));
            let mut inherited_already = true;
            assert!(JS_AlreadyHasOwnPropertyById(
                cx,
                inherited_object.handle().into(),
                foo_id.handle().into(),
                &mut inherited_already,
            ));
            assert!(!inherited_name && !inherited_by_id && !inherited_already);

            rooted!(&in(cx) let missing_object = JS_NewPlainObject(cx));
            let mut missing_name = true;
            assert!(JS_HasOwnProperty(
                cx,
                missing_object.handle(),
                c"foo".as_ptr(),
                &mut missing_name,
            ));
            let missing_id = id_for(cx, global, "'foo'");
            rooted!(&in(cx) let missing_id = missing_id);
            let mut missing_by_id = true;
            assert!(JS_HasOwnPropertyById(
                cx,
                missing_object.handle(),
                missing_id.handle().into(),
                &mut missing_by_id,
            ));
            let mut missing_already = true;
            assert!(JS_AlreadyHasOwnPropertyById(
                cx,
                missing_object.handle().into(),
                missing_id.handle().into(),
                &mut missing_already,
            ));
            assert!(!missing_name && !missing_by_id && !missing_already);

            Observation::new(vec![
                ("own_name", Observed::Boolean(own_name)),
                ("own_by_id", Observed::Boolean(own_by_id)),
                ("own_already", Observed::Boolean(own_already)),
                ("inherited_name", Observed::Boolean(inherited_name)),
                ("inherited_by_id", Observed::Boolean(inherited_by_id)),
                ("inherited_already", Observed::Boolean(inherited_already)),
                ("missing_name", Observed::Boolean(missing_name)),
                ("missing_by_id", Observed::Boolean(missing_by_id)),
                ("missing_already", Observed::Boolean(missing_already)),
            ])
        },
    );
}

/// Hand-written native cases derived from the upstream test262 property cases through the native entry points.
///
/// `Object/defineProperty/15.2.3.6-1-1.js` defines data properties, `15.2.3.6-2-37.js`
/// applies a Unicode property name, `15.2.3.6-2-6.js` defines element `0`, `15.2.3.6-4-2.js`
/// stores a small integer value, `8.12.9-9-c-i_2.js` stores a string value, `15.2.3.6-4-95.js`
/// stores an object value, `Reflect/defineProperty/define-properties.js` defines
/// both data and accessor properties, `Reflect/get/return-value-from-receiver.js` reads through
/// a receiver, `Object/getOwnPropertyDescriptor/15.2.3.3-4-1.js` inspects a data descriptor,
/// `Object/create/15.2.3.5-4-1.js` sets the prototype,
/// `Object/is/same-value-x-y-number.js` distinguishes `-0` from `+0`, and
/// `Object/prototype/__proto__/set-immutable.js` rejects a prototype change on an immutable
/// prototype exotic object.
fn record_property_wpt_cases(engine: &JSEngine) {
    record_case(
        engine,
        "property.wpt_define_and_create",
        &[
            "JS_DefineElement",
            "JS_DefineProperty3",
            "JS_DefineProperty4",
            "JS_DefineProperty5",
            "JS_DefinePropertyById2",
            "JS_DefinePropertyById5",
            "JS_DefineUCProperty2",
            "JS_ForwardGetPropertyTo",
            "JS_GetElement",
            "JS_GetPropertyDescriptorById",
            "JS_NewObject",
            "JS_NewObjectWithGivenProto",
            "JS_NewPlainObject",
            "JS_SetImmutablePrototype",
            "SameValue",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(42));
            assert!(JS_DefineProperty(
                cx,
                object.handle(),
                c"p1".as_ptr(),
                value.handle(),
                ATTRS,
            ));

            rooted!(&in(cx) let string_value =
                js::rust::wrappers2::JS_NewStringCopyN(cx, b"str".as_ptr().cast(), 3));
            assert!(JS_DefineProperty4(
                cx,
                object.handle(),
                c"p2".as_ptr(),
                string_value.handle().into(),
                ATTRS,
            ));
            assert!(JS_DefineProperty5(
                cx,
                object.handle(),
                c"p3".as_ptr(),
                7,
                ATTRS,
            ));

            let by_id = id_for(cx, global, "'p4'");
            rooted!(&in(cx) let by_id = by_id);
            rooted!(&in(cx) let by_id_value = Int32Value(9));
            assert!(JS_DefinePropertyById2(
                cx,
                object.handle(),
                by_id.handle().into(),
                by_id_value.handle().into(),
                ATTRS,
            ));

            let by_id5 = id_for(cx, global, "'p5'");
            rooted!(&in(cx) let by_id5 = by_id5);
            rooted!(&in(cx) let owned = JS_NewObject(cx, ptr::null()));
            assert!(JS_DefinePropertyById5(
                cx,
                object.handle(),
                by_id5.handle().into(),
                owned.handle(),
                ATTRS,
            ));
            rooted!(&in(cx) let object_value = JS_NewObject(cx, ptr::null()));
            assert!(JS_DefineProperty3(
                cx,
                object.handle(),
                c"p6".as_ptr(),
                object_value.handle(),
                ATTRS,
            ));

            let uc: Vec<u16> = "123αβπcd".encode_utf16().collect();
            rooted!(&in(cx) let uc_value = Int32Value(11));
            assert!(JS_DefineUCProperty2(
                cx,
                object.handle(),
                uc.as_ptr(),
                uc.len(),
                uc_value.handle().into(),
                ATTRS,
            ));

            rooted!(&in(cx) let one = Int32Value(1));
            assert!(JS_DefineElement(
                cx,
                object.handle(),
                0,
                one.handle().into(),
                ATTRS,
            ));
            rooted!(&in(cx) let mut element = UndefinedValue());
            assert!(JS_GetElement(cx, object.handle(), 0, element.handle_mut()));
            assert_eq!(element.get().to_int32(), 1);

            let receiver = JS_NewObjectWithGivenProto(cx, ptr::null(), object.handle());
            rooted!(&in(cx) let receiver = receiver);
            assert!(!receiver.get().is_null());
            rooted!(&in(cx) let receiver_value = ObjectValue(receiver.get()));
            let by_id_receiver = id_for(cx, global, "'p4'");
            rooted!(&in(cx) let by_id_receiver = by_id_receiver);
            rooted!(&in(cx) let mut forwarded = UndefinedValue());
            assert!(JS_ForwardGetPropertyTo(
                cx,
                object.handle(),
                by_id_receiver.handle().into(),
                receiver_value.handle(),
                forwarded.handle_mut(),
            ));
            assert_eq!(forwarded.get().to_int32(), 9);

            let desc_id = id_for(cx, global, "'p1'");
            rooted!(&in(cx) let desc_id = desc_id);
            rooted!(&in(cx) let mut descriptor = PropertyDescriptor::default());
            rooted!(&in(cx) let mut holder = ptr::null_mut::<js::jsapi::JSObject>());
            let mut is_none = true;
            assert!(JS_GetPropertyDescriptorById(
                cx,
                object.handle(),
                desc_id.handle().into(),
                descriptor.handle_mut().into(),
                holder.handle_mut().into(),
                &mut is_none,
            ));
            assert!(!is_none);
            assert_eq!(descriptor.value_.to_int32(), 42);

            rooted!(&in(cx) let immutable_object = JS_NewPlainObject(cx));
            let mut succeeded = false;
            assert!(JS_SetImmutablePrototype(
                cx,
                immutable_object.handle(),
                &mut succeeded,
            ));
            assert!(succeeded);

            set_global(cx, global, c"wptProperty", ObjectValue(object.get()));
            set_global(cx, global, c"wptOwned", ObjectValue(owned.get()));
            set_global(
                cx,
                global,
                c"wptObjectValue",
                ObjectValue(object_value.get()),
            );
            set_global(cx, global, c"wptPropertyReceiver", receiver_value.get());
            set_global(
                cx,
                global,
                c"wptImmutable",
                ObjectValue(immutable_object.get()),
            );
            let p1 = eval_observe(cx, global, "wptProperty.p1");
            let p2 = eval_observe(cx, global, "wptProperty.p2");
            let p3 = eval_observe(cx, global, "wptProperty.p3");
            let p4 = eval_observe(cx, global, "wptPropertyReceiver.p4");
            let p5 = eval_observe(cx, global, "wptProperty.p5");
            let p5_is_owned = eval_observe(cx, global, "wptProperty.p5 === wptOwned");
            let p6_is_owned = eval_observe(cx, global, "wptProperty.p6 === wptObjectValue");
            let unicode_value = eval_observe(cx, global, "wptProperty['123αβπcd']");
            let immutable_rejected = eval_observe(
                cx,
                global,
                "Reflect.setPrototypeOf(wptImmutable, null) === false && Object.getPrototypeOf(wptImmutable) !== null",
            );
            assert_eq!(p1, Observed::Number(42.0));
            assert_eq!(p2, Observed::String("str".to_owned()));
            assert_eq!(p3, Observed::Number(7.0));
            assert_eq!(p4, Observed::Number(9.0));
            assert_eq!(p5, Observed::Object);
            assert_eq!(p5_is_owned, Observed::Boolean(true));
            assert_eq!(p6_is_owned, Observed::Boolean(true));
            assert_eq!(unicode_value, Observed::Number(11.0));
            assert_eq!(immutable_rejected, Observed::Boolean(true));

            rooted!(&in(cx) let nan_a = DoubleValue(f64::NAN));
            rooted!(&in(cx) let nan_b = DoubleValue(f64::NAN));
            let mut same_nan = false;
            assert!(SameValue(cx, nan_a.handle(), nan_b.handle(), &mut same_nan));
            assert!(same_nan);

            Observation::new(vec![
                ("p1", p1),
                ("p2", p2),
                ("p3", p3),
                ("p4", p4),
                ("p5", p5),
                ("p5_is_owned", p5_is_owned),
                ("p6_is_owned", p6_is_owned),
                ("unicode", unicode_value),
                ("element", Observed::Number(element.get().to_int32() as f64)),
                (
                    "forwarded",
                    Observed::Number(forwarded.get().to_int32() as f64),
                ),
                (
                    "descriptor",
                    Observed::Number(descriptor.value_.to_int32() as f64),
                ),
                ("immutable_rejected", immutable_rejected),
                ("same_nan", Observed::Boolean(same_nan)),
            ])
        },
    );
}

pub fn run(engine: &JSEngine) {
    record_object_wpt_cases(engine);
    record_property_wpt_cases(engine);
    record_reflect_wpt_cases(engine);
    record_has_own_wpt_cases(engine);
    property_semantics_get_object_proto(engine);
    property_semantics_get_property_keys(engine);
    property_semantics_get_and_set_property(engine);
    property_semantics_has_properties(engine);
    property_semantics_define_properties(engine);
    property_semantics_descriptors_and_prototypes(engine);
    property_semantics_forwarding(engine);
    property_semantics_delete_property_by_id(engine);
    property_semantics_elements(engine);
    property_semantics_property_by_id(engine);
    property_semantics_object_creation_and_prototypes(engine);
    property_semantics_define_property_variants(engine);
    property_semantics_forward_and_ignore(engine);
    property_semantics_value_to_object(engine);
    property_semantics_construct(engine);
    property_semantics_call_function_name(engine);
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

/// Hand-written native cases derived from the upstream test262 `Reflect.get` and
/// `Reflect.setPrototypeOf` files.
///
/// The scenarios follow `return-value.js` (data, accessor and inherited properties) and
/// `return-value-from-receiver.js` (the receiver argument), but the suite does not read the WPT
/// tree: the native calls below drive `JS_GetPropertyById` directly and assert the values this
/// test creates.
///
/// `return-true-if-new-prototype-is-set.js` replaces and clears prototypes, so it drives
/// `JS_SetPrototype` rather than only asserting that `Reflect.setPrototypeOf` exists.
fn record_reflect_wpt_cases(engine: &JSEngine) {
    record_case(
        engine,
        "property.wpt_reflect_get_and_set_prototype",
        &[
            "JS_GetPropertyById",
            "JS_GetPrototype",
            "JS_NewPlainObject",
            "JS_SetPrototype",
            "JS_ValueToId",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            set_global(cx, global, c"wptReflectObject", ObjectValue(object.get()));
            eval(
                cx,
                global,
                "wptReflectObject.p1 = 'value 1'; \
                 Object.defineProperty(wptReflectObject, 'p2', {get: undefined}); \
                 Object.defineProperty(wptReflectObject, 'p3', {get: function() { return 'foo'; }});",
            );
            rooted!(&in(cx) let mut data = UndefinedValue());
            let data_id = id_for(cx, global, "'p1'");
            rooted!(&in(cx) let data_id = data_id);
            assert!(JS_GetPropertyById(
                cx,
                object.handle(),
                data_id.handle().into(),
                data.handle_mut().into(),
            ));
            assert_eq!(
                observe(cx, data.get()),
                Observed::String("value 1".to_owned())
            );

            rooted!(&in(cx) let mut no_getter = UndefinedValue());
            let no_getter_id = id_for(cx, global, "'p2'");
            rooted!(&in(cx) let no_getter_id = no_getter_id);
            assert!(JS_GetPropertyById(
                cx,
                object.handle(),
                no_getter_id.handle().into(),
                no_getter.handle_mut().into(),
            ));
            assert!(no_getter.get().is_undefined());

            rooted!(&in(cx) let mut accessor = UndefinedValue());
            let accessor_id = id_for(cx, global, "'p3'");
            rooted!(&in(cx) let accessor_id = accessor_id);
            assert!(JS_GetPropertyById(
                cx,
                object.handle(),
                accessor_id.handle().into(),
                accessor.handle_mut().into(),
            ));
            assert_eq!(
                observe(cx, accessor.get()),
                Observed::String("foo".to_owned())
            );

            let inherited = eval(cx, global, "Object.create({p: 42})");
            rooted!(&in(cx) let inherited = inherited.to_object());
            set_global(
                cx,
                global,
                c"wptInheritedObject",
                ObjectValue(inherited.get()),
            );
            rooted!(&in(cx) let mut inherited_value = UndefinedValue());
            let inherited_id = id_for(cx, global, "'p'");
            rooted!(&in(cx) let inherited_id = inherited_id);
            assert!(JS_GetPropertyById(
                cx,
                inherited.handle(),
                inherited_id.handle().into(),
                inherited_value.handle_mut().into(),
            ));
            assert_eq!(inherited_value.get().to_int32(), 42);

            let receiver_source = eval(
                cx,
                global,
                "var wptReceiver = {y: 42}; \
                 var wptReceiverTarget = {}; \
                 Object.defineProperty(wptReceiverTarget, 'x', {get: function() { return this.y; }}); \
                 Reflect.get(wptReceiverTarget, 'x', wptReceiver);",
            );
            assert_eq!(receiver_source.to_int32(), 42);

            rooted!(&in(cx) let plain = JS_NewPlainObject(cx));
            rooted!(&in(cx) let proto = JS_NewPlainObject(cx));
            assert!(JS_SetPrototype(
                cx,
                plain.handle().into(),
                proto.handle().into()
            ));
            rooted!(&in(cx) let mut got = ptr::null_mut::<js::jsapi::JSObject>());
            assert!(JS_GetPrototype(
                cx,
                plain.handle().into(),
                got.handle_mut().into()
            ));
            assert_eq!(got.get(), proto.get());
            assert!(JS_SetPrototype(
                cx,
                plain.handle().into(),
                js::rust::HandleObject::null()
            ));
            assert!(JS_GetPrototype(
                cx,
                plain.handle().into(),
                got.handle_mut().into()
            ));
            assert!(got.get().is_null());

            Observation::new(vec![
                ("data_value", observe(cx, data.get())),
                (
                    "undefined_getter",
                    Observed::Boolean(no_getter.get().is_undefined()),
                ),
                ("accessor_value", observe(cx, accessor.get())),
                (
                    "inherited_value",
                    Observed::Number(inherited_value.get().to_int32() as f64),
                ),
                (
                    "receiver_value",
                    Observed::Number(receiver_source.to_int32() as f64),
                ),
                ("prototype_replaced", Observed::Boolean(got.get().is_null())),
            ])
        },
    );
    record_case(
        engine,
        "property.wpt_reflect_set_receiver",
        &["JS_ForwardSetPropertyTo", "JS_ValueToId"],
        |cx, global| unsafe {
            rooted!(&in(cx) let target = eval(
                cx,
                global,
                "({set p(value) { this.seen = value; }})",
            )
            .to_object());
            rooted!(&in(cx) let receiver = eval(cx, global, "({})").to_object());
            set_global(cx, global, c"wptSetReceiver", ObjectValue(receiver.get()));
            rooted!(&in(cx) let receiver_value = ObjectValue(receiver.get()));
            let property_id = id_for(cx, global, "'p'");
            rooted!(&in(cx) let property_id = property_id);
            rooted!(&in(cx) let value = Int32Value(42));
            let mut result = ObjectOpResult::default();
            assert!(JS_ForwardSetPropertyTo(
                cx,
                target.handle(),
                property_id.handle().into(),
                value.handle().into(),
                receiver_value.handle().into(),
                &mut result,
            ));
            let accepted = result.ok();
            let seen = eval(cx, global, "wptSetReceiver.seen").to_int32();
            assert!(accepted);
            assert_eq!(seen, 42);
            Observation::new(vec![
                ("set_accepted", Observed::Boolean(accepted)),
                ("receiver_seen", Observed::Number(seen as f64)),
            ])
        },
    );
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

/// Hand-written native cases derived from the upstream test262 `Object`/`Reflect` cases that drive the native property entry points.
///
/// `15.2.3.6-1-1.js` defines a data property, `15.2.3.9-0-1.js` freezes an object, and
/// `15.2.3.14-0-1.js` enumerates keys. The observation is what JavaScript sees, so a divergence in
/// the native call cannot pass by asserting only that the call returned `true`.
fn record_object_wpt_cases(engine: &JSEngine) {
    record_case(
        engine,
        "property.object_wpt_define_and_freeze",
        &[
            "JS_NewPlainObject",
            "JS_DefineProperty",
            "JS_DefinePropertyById",
            "JS_FreezeObject",
        ],
        |cx, global| unsafe {
            rooted!(&in(cx) let object = JS_NewPlainObject(cx));
            rooted!(&in(cx) let value = Int32Value(42));
            assert!(JS_DefineProperty(
                cx,
                object.handle(),
                c"answer".as_ptr(),
                value.handle(),
                JSPROP_ENUMERATE as u32,
            ));
            set_global(cx, global, c"wptObject", ObjectValue(object.get()));
            assert_eq!(
                eval_observe(cx, global, "wptObject.answer"),
                Observed::Number(42.0)
            );

            let id = id_for(cx, global, "'second'");
            rooted!(&in(cx) let id = id);
            rooted!(&in(cx) let second = Int32Value(7));
            // `JS_DefinePropertyById` takes a descriptor; the data property matches what
            // `Object.defineProperty(o, "second", { value: 7, ... })` passes.
            let mut define_desc = PropertyDescriptor::default();
            define_desc.set_hasValue_(true);
            define_desc.value_ = second.get();
            define_desc.set_hasWritable_(true);
            define_desc.set_writable_(true);
            define_desc.set_hasEnumerable_(true);
            define_desc.set_enumerable_(true);
            define_desc.set_hasConfigurable_(true);
            define_desc.set_configurable_(true);
            rooted!(&in(cx) let mut define_desc = define_desc);
            let mut result = ObjectOpResult::default();
            assert!(JS_DefinePropertyById(
                cx,
                object.handle(),
                id.handle().into(),
                define_desc.handle().into(),
                &mut result,
            ));
            assert!(JS_FreezeObject(cx, object.handle()));

            // The frozen object must reject a write and still report both keys.
            assert_eq!(
                eval_observe(cx, global, "wptObject.second = 9; wptObject.second"),
                Observed::Number(7.0)
            );
            let key_count = eval_observe(cx, global, "Object.keys(wptObject).length");
            assert_eq!(key_count, Observed::Number(2.0));
            assert_eq!(
                eval_observe(cx, global, "Object.isFrozen(wptObject)"),
                Observed::Boolean(true)
            );

            let second_value = eval_observe(
                cx,
                global,
                "Object.getOwnPropertyDescriptor(wptObject, 'second').value",
            );
            let frozen_writable = eval_observe(
                cx,
                global,
                "Object.getOwnPropertyDescriptor(wptObject, 'second').writable",
            );
            let frozen_configurable = eval_observe(
                cx,
                global,
                "Object.getOwnPropertyDescriptor(wptObject, 'second').configurable",
            );
            let prototype_matches = eval_observe(
                cx,
                global,
                "Object.getPrototypeOf(wptObject) === Object.prototype",
            );
            Observation::new(vec![
                ("key_count", key_count),
                ("second_value", second_value),
                ("frozen_writable", frozen_writable),
                ("frozen_configurable", frozen_configurable),
                ("prototype_is_object_prototype", prototype_matches),
            ])
        },
    );
}
