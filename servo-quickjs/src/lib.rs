#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unnecessary_transmutes,
    clippy::all
)]

#[cfg(not(target_pointer_width = "64"))]
compile_error!("bindings.rs assumes the 64-bit struct JSValue representation");

include!("bindings.rs");

/// Rust equivalents of the `JS_MKVAL`/`JS_VALUE_GET_*` macros for the 64-bit representation.
impl JSValue {
    pub const fn mkval(tag: i32, val: i32) -> JSValue {
        JSValue {
            u: JSValueUnion {
                uint64: val as u32 as u64,
            },
            tag: tag as i64,
        }
    }

    pub fn mkptr(tag: i32, ptr: *mut ::std::os::raw::c_void) -> JSValue {
        JSValue {
            u: JSValueUnion { ptr },
            tag: tag as i64,
        }
    }

    pub const fn tag(&self) -> i32 {
        self.tag as i32
    }

    pub fn int(&self) -> i32 {
        unsafe { self.u.uint64 as i32 }
    }

    pub fn float64(&self) -> f64 {
        unsafe { self.u.float64 }
    }

    pub fn ptr(&self) -> *mut ::std::os::raw::c_void {
        unsafe { self.u.ptr }
    }

    pub fn has_ref_count(&self) -> bool {
        (self.tag as i32 as u32) >= (JS_TAG_FIRST as u32)
    }
}

/// `__js_rc`: the reference count is stored just before the object.
#[inline(always)]
unsafe fn ref_count(v: JSValue) -> *mut i32 {
    unsafe { (v.ptr() as *mut i32).sub(1) }
}

/// The `static inline` reference counting functions of quickjs.h, inlined into Rust callers.
#[inline(always)]
pub unsafe fn JS_DupValueRT(_rt: *mut JSRuntime, v: JSValue) -> JSValue {
    if v.has_ref_count() {
        unsafe { *ref_count(v) += 1 };
    }
    v
}

/// The batched form of `JS_DupValueRT`: add several references at once.
#[inline(always)]
pub unsafe fn JS_DupValueRTCount(_rt: *mut JSRuntime, v: JSValue, count: i32) {
    if count > 0 && v.has_ref_count() {
        unsafe { *ref_count(v) += count };
    }
}

/// The reference count of a value that has one.
#[inline(always)]
pub unsafe fn JS_GetRefCount(v: JSValue) -> i32 {
    unsafe { *ref_count(v) }
}

#[inline(always)]
pub unsafe fn JS_DupValue(_ctx: *mut JSContext, v: JSValue) -> JSValue {
    unsafe { JS_DupValueRT(std::ptr::null_mut(), v) }
}

#[inline(always)]
pub unsafe fn JS_FreeValueRT(rt: *mut JSRuntime, v: JSValue) {
    if v.has_ref_count() {
        unsafe {
            let count = ref_count(v);
            *count -= 1;
            if *count <= 0 {
                __JS_FreeValueRT(rt, v);
            }
        }
    }
}

/// The batched form of `JS_FreeValueRT`: free the value when the count reaches zero.
#[inline(always)]
pub unsafe fn JS_FreeValueRTCount(rt: *mut JSRuntime, v: JSValue, count: i32) {
    if count > 0 && v.has_ref_count() {
        unsafe {
            let ref_count = ref_count(v);
            *ref_count -= count;
            if *ref_count <= 0 {
                __JS_FreeValueRT(rt, v);
            }
        }
    }
}

#[inline(always)]
pub unsafe fn JS_FreeValue(ctx: *mut JSContext, v: JSValue) {
    if v.has_ref_count() {
        unsafe {
            let count = ref_count(v);
            *count -= 1;
            if *count <= 0 {
                __JS_FreeValue(ctx, v);
            }
        }
    }
}

unsafe extern "C" {
    #[link_name = "JS_FreeContext"]
    fn JS_FreeContext_out_of_line(ctx: *mut JSContext);
}

/// `JS_DupContext`, inlined (contexts carry the same reference count header as values): entering
/// a realm counts its context, on every native call and proxy trap.
#[inline(always)]
pub unsafe fn JS_DupContext(ctx: *mut JSContext) -> *mut JSContext {
    unsafe { *(ctx as *mut i32).sub(1) += 1 };
    ctx
}

/// `JS_FreeContext`, with the count decremented inline unless the context is freed.
#[inline(always)]
pub unsafe fn JS_FreeContext(ctx: *mut JSContext) {
    unsafe {
        let count = (ctx as *mut i32).sub(1);
        if *count > 1 {
            *count -= 1;
        } else {
            JS_FreeContext_out_of_line(ctx);
        }
    }
}

pub const JS_NULL: JSValue = JSValue::mkval(JS_TAG_NULL, 0);
pub const JS_UNDEFINED: JSValue = JSValue::mkval(JS_TAG_UNDEFINED, 0);
pub const JS_FALSE: JSValue = JSValue::mkval(JS_TAG_BOOL, 0);
pub const JS_TRUE: JSValue = JSValue::mkval(JS_TAG_BOOL, 1);
pub const JS_EXCEPTION: JSValue = JSValue::mkval(JS_TAG_EXCEPTION, 0);
pub const JS_UNINITIALIZED: JSValue = JSValue::mkval(JS_TAG_UNINITIALIZED, 0);
