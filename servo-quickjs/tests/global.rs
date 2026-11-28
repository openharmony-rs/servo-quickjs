//! Servo patches: a global object of an embedder class, a separate global `this`, and lazy
//! global properties defined during enumeration.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicUsize, Ordering};

use servo_quickjs::*;

static FINALIZED: AtomicUsize = AtomicUsize::new(0);
static MARKED: AtomicUsize = AtomicUsize::new(0);
static RESOLVED: AtomicUsize = AtomicUsize::new(0);

unsafe fn eval(ctx: *mut JSContext, src: &str) -> JSValue {
    let src = CString::new(src).unwrap();
    let flags = JS_EVAL_TYPE_GLOBAL as c_int;
    unsafe {
        JS_Eval(
            ctx,
            src.as_ptr(),
            src.as_bytes().len(),
            c"<global>".as_ptr(),
            flags,
        )
    }
}

unsafe fn eval_bool(ctx: *mut JSContext, src: &str) -> bool {
    let v = unsafe { eval(ctx, src) };
    assert_eq!(v.tag(), JS_TAG_BOOL, "{src}");
    v.int() != 0
}

unsafe extern "C" fn finalizer(_rt: *mut JSRuntime, val: JSValue) {
    let opaque = unsafe { JS_GetAnyOpaque(val, &mut 0) };
    assert_eq!(opaque as usize, 0x5678);
    FINALIZED.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn gc_mark(_rt: *mut JSRuntime, _val: JSValue, _mark: JS_MarkFunc) {
    MARKED.fetch_add(1, Ordering::SeqCst);
}

/// Resolves `lazy` by defining it on the global, like a SpiderMonkey resolve hook.
unsafe extern "C" fn get_own_property(
    ctx: *mut JSContext,
    desc: *mut JSPropertyDescriptor,
    obj: JSValue,
    prop: JSAtom,
) -> c_int {
    let name = unsafe { JS_AtomToCString(ctx, prop) };
    let is_lazy = unsafe { CStr::from_ptr(name) } == c"lazy";
    unsafe { JS_FreeCString(ctx, name as *const c_char) };
    if !is_lazy {
        return 0;
    }
    RESOLVED.fetch_add(1, Ordering::SeqCst);
    let flags = (JS_PROP_CONFIGURABLE | JS_PROP_WRITABLE) as c_int;
    unsafe {
        JS_DefinePropertyValue(ctx, obj, prop, JS_NewInt32(ctx, 7), flags);
        JS_GetOwnProperty(ctx, desc, obj, prop)
    }
}

/// Resolves everything during enumeration and reports no exotic keys.
unsafe extern "C" fn get_own_property_names(
    ctx: *mut JSContext,
    ptab: *mut *mut JSPropertyEnum,
    plen: *mut u32,
    obj: JSValue,
) -> c_int {
    unsafe {
        let atom = JS_NewAtom(ctx, c"lazy".as_ptr());
        let r = JS_GetOwnProperty(ctx, std::ptr::null_mut(), obj, atom);
        JS_FreeAtom(ctx, atom);
        assert_eq!(r, 1);
        *ptab = std::ptr::null_mut();
        *plen = 0;
    }
    0
}

#[test]
fn global_class_and_global_this() {
    unsafe {
        let rt = JS_NewRuntime();
        let mut class_id: JSClassID = 0;
        JS_NewClassID(&mut class_id);
        let exotic = Box::leak(Box::new(JSClassExoticMethods {
            get_own_property: Some(get_own_property),
            get_own_property_names: Some(get_own_property_names),
            ..Default::default()
        }));
        let def = JSClassDef {
            class_name: c"Window".as_ptr(),
            finalizer: Some(finalizer),
            gc_mark: Some(gc_mark),
            exotic,
            ..Default::default()
        };
        assert_eq!(JS_NewClass(rt, class_id, &def), 0);

        let ctx = JS_NewContext(rt);
        assert_eq!(JS_SetGlobalObjectClass(ctx, class_id), 0);
        let global = JS_GetGlobalObject(ctx);
        assert_eq!(JS_GetClassID(global), class_id);
        JS_SetOpaque(global, 0x5678 as *mut c_void);
        assert_eq!(JS_GetOpaque(global, class_id) as usize, 0x5678);

        // Global variables keep working on the embedder-class global.
        assert!(eval_bool(
            ctx,
            "var a = 1; let b = 2; function f() { return a + b; } f() === 3"
        ));
        assert!(eval_bool(
            ctx,
            "a = 5; globalThis.a === 5 && Object.getOwnPropertyDescriptor(globalThis, 'a').value === 5"
        ));
        assert!(eval_bool(
            ctx,
            "delete globalThis.c; c = 3; c === 3 && 'c' in globalThis"
        ));

        // Lazy properties, for direct and global variable access, and during enumeration.
        assert!(eval_bool(ctx, "lazy === 7 && typeof lazy === 'number'"));
        assert!(eval_bool(
            ctx,
            "delete globalThis.lazy; Object.getOwnPropertyNames(globalThis).includes('lazy')"
        ));
        assert!(eval_bool(
            ctx,
            "delete globalThis.lazy; (function() { return lazy; })() === 7"
        ));
        assert!(RESOLVED.load(Ordering::SeqCst) >= 3);

        // A separate global `this`.
        let window_proxy = JS_NewObject(ctx);
        JS_SetPropertyStr(
            ctx,
            global,
            c"proxy".as_ptr(),
            JS_DupValue(ctx, window_proxy),
        );
        JS_SetGlobalThis(ctx, window_proxy);
        assert!(eval_bool(ctx, "this === proxy && globalThis === proxy"));
        assert!(eval_bool(ctx, "(function() { return this; })() === proxy"));
        assert!(eval_bool(
            ctx,
            "(function() { 'use strict'; return this; })() === undefined"
        ));
        assert!(eval_bool(
            ctx,
            "(0, eval)('this') === proxy && (() => this)() === proxy"
        ));
        let this = JS_GetGlobalThis(ctx);
        assert_eq!(this.ptr(), window_proxy.ptr());
        JS_FreeValue(ctx, this);
        assert_eq!(JS_IsStrictMode(ctx), 0);

        JS_RunGC(rt);
        assert!(MARKED.load(Ordering::SeqCst) >= 1);
        JS_FreeValue(ctx, global);
        JS_FreeContext(ctx);
        JS_RunGC(rt);
        assert_eq!(FINALIZED.load(Ordering::SeqCst), 1);
        JS_FreeRuntime(rt);
    }
}
