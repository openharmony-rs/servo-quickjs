/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Stencils and off-thread compilation of global scripts.
//!
//! A stencil is a global script compiled to QuickJS bytecode, which any runtime can instantiate
//! (patch Q50). A frontend context compiles with a QuickJS runtime of its own, created and freed
//! by each compilation, so that it can compile on any thread. When compiling fails, the frontend
//! context keeps the source, and `ConvertFrontendErrorsToRuntimeErrors` compiles it again in the
//! caller's runtime, which throws the error a compilation there would have thrown.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{fence, AtomicUsize, Ordering};

use servo_quickjs as q;

use super::eval::{eval, options, source_buffer, units, CompileOptions};
use super::exception::throw_error;
use super::Cx;
use crate::sys::jsapi as root;

/// What a `JS::Stencil*` points to. Immutable apart from its count, so any thread can hold it.
struct Stencil {
    refs: AtomicUsize,
    bytecode: Box<[u8]>,
    muted_errors: bool,
    /// What debuggers are told about the script when it is instantiated.
    source: ScriptSource,
}

struct ScriptSource {
    url: CString,
    lineno: u32,
    text: Box<[u8]>,
    introduction_type: Option<&'static CStr>,
}

/// What a `JS::FrontendContext*` points to.
#[derive(Default)]
struct FrontendContext {
    /// The stack a compilation may use, from `SetNativeStackQuota`, measured from where the
    /// compilation starts; QuickJS's default if 0.
    stack_quota: usize,
    /// The script that failed to compile last.
    failed: Option<FailedCompilation>,
}

struct FailedCompilation {
    /// The source as `eval` takes it.
    source: Vec<u8>,
    /// The type and message of what the compilation threw, for when compiling again succeeds, as
    /// on a larger stack.
    error: (root::JSExnType, String),
}

pub(crate) unsafe fn NewFrontendContext() -> *mut root::JS::FrontendContext {
    Box::into_raw(Box::<FrontendContext>::default()) as *mut root::JS::FrontendContext
}

pub(crate) unsafe fn DestroyFrontendContext(fc: *mut root::JS::FrontendContext) {
    drop(Box::from_raw(fc as *mut FrontendContext));
}

pub(crate) unsafe fn SetNativeStackQuota(
    fc: *mut root::JS::FrontendContext,
    stackSize: root::JS::NativeStackSize,
) {
    (*(fc as *mut FrontendContext)).stack_quota = stackSize;
}

/// Nine tenths of the thread's stack: QuickJS measures the quota from where the compilation
/// starts, which is near the top of the stack for the threads of `mozjs::offthread`.
pub(crate) unsafe fn ThreadStackQuotaForSize(stackSize: usize) -> root::JS::NativeStackSize {
    stackSize - stackSize / 10
}

pub(crate) unsafe fn HadFrontendErrors(fc: *mut root::JS::FrontendContext) -> bool {
    (*(fc as *const FrontendContext)).failed.is_some()
}

/// Copies the options, for a compilation that may outlive them.
pub(crate) unsafe fn OwningCompileOptions_for_fc(
    fc: *mut root::JS::FrontendContext,
    rhs: *const root::JS::ReadOnlyCompileOptions,
) -> *mut root::JS::OwningCompileOptions {
    let _ = fc;
    let rhs = options(rhs);
    // A `JS::OwningCompileOptions` starts with its `ReadOnlyCompileOptions`, which is all that
    // mozjs reads of it, so a `CompileOptions` can stand for one.
    let copy = Box::new(CompileOptions {
        base: ptr::read(&rhs.base),
        filename: rhs.filename.clone(),
    });
    Box::into_raw(copy) as *mut root::JS::OwningCompileOptions
}

pub(crate) unsafe fn DeleteOwningCompileOptions(opts: *mut root::JS::OwningCompileOptions) {
    drop(Box::from_raw(opts as *mut CompileOptions));
}

pub(crate) unsafe fn CompileGlobalScriptToStencil_C(
    fc: *mut root::JS::FrontendContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<root::mozilla::Utf8Unit>,
    out_stencil: *mut root::already_AddRefed<root::JS::Stencil>,
) {
    let fc = &mut *(fc as *mut FrontendContext);
    let options = self::options(options);
    let source = source_buffer(
        options.base.lineno,
        (*srcBuf).units_ as *const u8,
        (*srcBuf).length_ as usize,
    );
    fc.failed = None;
    let stencil = match compile(fc.stack_quota, options, &source) {
        Ok(bytecode) => Box::into_raw(Box::new(Stencil {
            refs: AtomicUsize::new(1),
            bytecode,
            muted_errors: options.base._base.mutedErrors_,
            source: ScriptSource {
                url: options.filename.clone(),
                lineno: options.base.lineno,
                text: units(srcBuf).into(),
                // Introduction types are static strings.
                introduction_type: (!options.base._base.introductionType.is_null())
                    .then(|| CStr::from_ptr(options.base._base.introductionType)),
            },
        })) as *mut root::JS::Stencil,
        Err(error) => {
            fc.failed = Some(FailedCompilation { source, error });
            ptr::null_mut()
        }
    };
    *out_stencil = root::already_AddRefed {
        _phantom_0: std::marker::PhantomData,
        mRawPtr: stencil,
    };
}

/// Compiles `source` in a runtime of its own, on the calling thread, to the bytecode of a global
/// script, or to the type and message of the error compiling threw.
unsafe fn compile(
    stack_quota: usize,
    options: &CompileOptions,
    source: &[u8],
) -> Result<Box<[u8]>, (root::JSExnType, String)> {
    let out_of_memory = || {
        (
            root::JSExnType::JSEXN_INTERNALERR,
            "out of memory".to_owned(),
        )
    };
    let rt = q::JS_NewRuntime();
    if rt.is_null() {
        return Err(out_of_memory());
    }
    let ctx = q::JS_NewContext(rt);
    let result = if ctx.is_null() {
        Err(out_of_memory())
    } else {
        // The quota is the compilation's, not the context's.
        if stack_quota > 0 {
            q::JS_SetMaxStackSize(rt, stack_quota);
        }
        let flags = (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
        let fun = q::JS_Eval(
            ctx,
            source.as_ptr() as *const c_char,
            source.len() - 1,
            options.filename.as_ptr(),
            flags,
        );
        let result = if q::JS_IsException(fun) != 0 {
            Err(take_exception(ctx))
        } else {
            let mut len = 0;
            let buf = q::JS_WriteObject(ctx, &mut len, fun, q::JS_WRITE_OBJ_BYTECODE as i32);
            q::JS_FreeValue(ctx, fun);
            if buf.is_null() {
                Err(take_exception(ctx))
            } else {
                let bytecode = std::slice::from_raw_parts(buf, len).into();
                q::js_free(ctx, buf as *mut c_void);
                Ok(bytecode)
            }
        };
        q::JS_FreeContext(ctx);
        result
    };
    q::JS_FreeRuntime(rt);
    result
}

/// Takes the pending exception of `ctx`: the type and message of an error, or an internal error
/// with the value as a string.
unsafe fn take_exception(ctx: *mut q::JSContext) -> (root::JSExnType, String) {
    let exception = q::JS_GetException(ctx);
    let (kind, value) = if q::JS_IsError(ctx, exception) != 0 {
        let name = to_string(ctx, q::JS_GetPropertyStr(ctx, exception, c"name".as_ptr()));
        let kind = match name.as_deref() {
            Some("SyntaxError") => root::JSExnType::JSEXN_SYNTAXERR,
            Some("TypeError") => root::JSExnType::JSEXN_TYPEERR,
            Some("RangeError") => root::JSExnType::JSEXN_RANGEERR,
            Some("ReferenceError") => root::JSExnType::JSEXN_REFERENCEERR,
            _ => root::JSExnType::JSEXN_INTERNALERR,
        };
        let message = q::JS_GetPropertyStr(ctx, exception, c"message".as_ptr());
        (kind, message)
    } else {
        let value = q::JS_DupValue(ctx, exception);
        (root::JSExnType::JSEXN_INTERNALERR, value)
    };
    q::JS_FreeValue(ctx, exception);
    let message = to_string(ctx, value).unwrap_or_else(|| "compilation failed".to_owned());
    (kind, message)
}

/// Converts `value`, which it frees, to a string.
unsafe fn to_string(ctx: *mut q::JSContext, value: q::JSValue) -> Option<String> {
    let text = q::JS_ToCStringLen2(ctx, ptr::null_mut(), value, 0);
    q::JS_FreeValue(ctx, value);
    if text.is_null() {
        q::JS_FreeValue(ctx, q::JS_GetException(ctx));
        return None;
    }
    let string = CStr::from_ptr(text).to_string_lossy().into_owned();
    q::JS_FreeCString(ctx, text);
    Some(string)
}

pub(crate) unsafe fn PrepareForInstantiate(
    fc: *mut root::JS::FrontendContext,
    stencil: *mut root::JS::Stencil,
    storage: *mut root::JS::InstantiationStorage,
) -> bool {
    let _ = (fc, stencil, storage);
    true
}

pub(crate) unsafe fn InstantiateGlobalStencil(
    cx: *mut root::JSContext,
    options: *const root::JS::InstantiateOptions,
    stencil: *mut root::JS::Stencil,
    storage: *mut root::JS::InstantiationStorage,
) -> *mut root::JSScript {
    let _ = (options, storage);
    let cx = Cx::from_raw(cx);
    let stencil = &*(stencil as *const Stencil);
    let flags = if stencil.muted_errors {
        q::JS_EVAL_FLAG_MUTED_ERRORS as i32
    } else {
        0
    };
    let bytecode = &stencil.bytecode;
    let source = &stencil.source;
    let mut script = q::JS_UNDEFINED;
    if cx.debugger.is_active() {
        // The parser records statement positions and sourceURL directives for debuggers only
        // while they are active, so the script is compiled again here.
        let buf = source_buffer(source.lineno, source.text.as_ptr(), source.text.len());
        script = q::JS_Eval(
            cx.qctx(),
            buf.as_ptr() as *const c_char,
            buf.len() - 1,
            source.url.as_ptr(),
            flags | (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as i32,
        );
        // Compiling here can fail where the compiling thread succeeded, with less stack left: the
        // bytecode then, with QuickJS's positions.
        if q::JS_IsException(script) != 0 {
            q::JS_FreeValue(cx.qctx(), q::JS_GetException(cx.qctx()));
            script = q::JS_UNDEFINED;
        }
    }
    if script.tag() == q::JS_TAG_UNDEFINED {
        script = q::JS_ReadScript(cx.qctx(), bytecode.as_ptr(), bytecode.len(), flags);
    }
    if q::JS_IsException(script) != 0 {
        return ptr::null_mut();
    }
    // A termination of a hook requests the interrupt again, which terminates the script when it
    // runs.
    if super::new_script(
        cx,
        script,
        &source.url,
        &source.text,
        source.introduction_type.map_or(ptr::null(), CStr::as_ptr),
        source.lineno,
    ) < 0
    {
        super::reinterrupt(cx, cx.qctx());
    }
    cx.adopt(script).asBits_ as usize as *mut root::JSScript
}

/// Throws the error of the frontend context's failed compilation: compiles its source again in
/// `cx`'s runtime, which throws what a compilation there throws, or throws the first
/// compilation's error if this one succeeds. Returns true: an error is pending either way.
pub(crate) unsafe fn ConvertFrontendErrorsToRuntimeErrors(
    cx: *mut root::JSContext,
    fc: *mut root::JS::FrontendContext,
    options: *const root::JS::ReadOnlyCompileOptions,
) -> bool {
    let Some(failed) = &(*(fc as *const FrontendContext)).failed else {
        return true;
    };
    let raw_cx = cx;
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let flags = (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    let script = eval(cx, options, &failed.source, flags);
    if q::JS_IsException(script) == 0 {
        q::JS_FreeValue(cx.qctx(), script);
        let (kind, message) = &failed.error;
        let message = CString::new(message.replace('\0', "")).unwrap_or_default();
        throw_error(raw_cx, *kind, &message);
    }
    true
}

pub(crate) unsafe fn StencilAddRef(stencil: *mut root::JS::Stencil) {
    (*(stencil as *const Stencil))
        .refs
        .fetch_add(1, Ordering::Relaxed);
}

pub(crate) unsafe fn StencilRelease(stencil: *mut root::JS::Stencil) {
    let s = stencil as *mut Stencil;
    if (*s).refs.fetch_sub(1, Ordering::Release) == 1 {
        fence(Ordering::Acquire);
        drop(Box::from_raw(s));
    }
}
