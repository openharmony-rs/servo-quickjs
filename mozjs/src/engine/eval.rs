/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Compile options and classic script evaluation.

use std::ffi::{c_char, c_uint, c_void, CString};
use std::ptr;

use servo_quickjs as q;

use super::{cstring_lossy, Cx};
use crate::sys::jsapi as root;
use crate::sys::jsval::JSVal;

/// Backs a `JS::ReadOnlyCompileOptions*`: mozjs writes the SpiderMonkey fields directly, so the
/// real struct comes first.
#[repr(C)]
pub(crate) struct CompileOptions {
    pub base: root::JS::ReadOnlyCompileOptions,
    pub filename: CString,
}

pub(crate) unsafe fn options<'a>(
    options: *const root::JS::ReadOnlyCompileOptions,
) -> &'a CompileOptions {
    &*(options as *const CompileOptions)
}

pub(crate) unsafe fn NewCompileOptions(
    aCx: *mut root::JSContext,
    aFile: *const c_char,
    aLine: c_uint,
) -> *mut root::JS::ReadOnlyCompileOptions {
    let _ = aCx;
    let mut base: root::JS::ReadOnlyCompileOptions = std::mem::zeroed();
    base.lineno = aLine;
    let options = Box::new(CompileOptions {
        base,
        filename: cstring_lossy(aFile),
    });
    Box::into_raw(options) as *mut root::JS::ReadOnlyCompileOptions
}

pub(crate) unsafe fn DeleteCompileOptions(aOpts: *mut root::JS::ReadOnlyCompileOptions) {
    drop(Box::from_raw(aOpts as *mut CompileOptions));
}

/// NUL-terminated source (QuickJS requires it), padded with newlines so that QuickJS's line
/// numbers match the starting line SpiderMonkey would have used.
pub(crate) unsafe fn source_buffer(lineno: u32, units: *const u8, len: usize) -> Vec<u8> {
    let pad = lineno.saturating_sub(1) as usize;
    let mut buf = Vec::with_capacity(pad + len + 1);
    buf.resize(pad, b'\n');
    if len > 0 {
        buf.extend_from_slice(std::slice::from_raw_parts(units, len));
    }
    buf.push(0);
    buf
}

unsafe fn eval(cx: &Cx, options: &CompileOptions, src: &[u8], mut flags: i32) -> q::JSValue {
    if options.base._base.mutedErrors_ {
        flags |= q::JS_EVAL_FLAG_MUTED_ERRORS as i32;
    }
    q::JS_Eval(
        cx.qctx(),
        src.as_ptr() as *const c_char,
        src.len() - 1,
        options.filename.as_ptr(),
        flags,
    )
}

pub(crate) unsafe fn Evaluate2(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<root::mozilla::Utf8Unit>,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = source_buffer(
        options.base.lineno,
        (*srcBuf).units_ as *const u8,
        (*srcBuf).length_ as usize,
    );
    let result = eval(cx, options, &src, q::JS_EVAL_TYPE_GLOBAL as i32);
    if result.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *rval.ptr = cx.adopt(result);
    true
}

pub(crate) unsafe fn Compile1(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<root::mozilla::Utf8Unit>,
) -> *mut root::JSScript {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = source_buffer(
        options.base.lineno,
        (*srcBuf).units_ as *const u8,
        (*srcBuf).length_ as usize,
    );
    let flags = (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    let result = eval(cx, options, &src, flags);
    if result.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    cx.adopt(result).asBits_ as usize as *mut root::JSScript
}

pub(crate) unsafe fn JS_ExecuteScript(
    cx: *mut root::JSContext,
    script: root::JS::Handle<*mut root::JSScript>,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let fun = q::JSValue::mkptr(q::JS_TAG_FUNCTION_BYTECODE, *script.ptr as *mut _);
    // JS_EvalFunction consumes a reference; the script stays retained by the pool.
    let result = q::JS_EvalFunction(cx.qctx(), q::JS_DupValueRT(cx.rt, fun));
    if result.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *rval.ptr = cx.adopt(result);
    true
}

pub(crate) unsafe fn utf16_source(srcBuf: *mut root::JS::SourceText<u16>) -> String {
    let len = (*srcBuf).length_ as usize;
    if len == 0 {
        return String::new();
    }
    String::from_utf16_lossy(std::slice::from_raw_parts((*srcBuf).units_, len))
}

/// UTF-8 source as a NUL-terminated buffer padded to start at `lineno`.
pub(crate) fn padded(lineno: u32, src: &str) -> Vec<u8> {
    source_buffer_from(lineno, src.as_bytes())
}

fn source_buffer_from(lineno: u32, src: &[u8]) -> Vec<u8> {
    unsafe { source_buffer(lineno, src.as_ptr(), src.len()) }
}

pub(crate) unsafe fn Evaluate(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<u16>,
    rval: root::JS::MutableHandle<root::JS::Value>,
) -> bool {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = padded(options.base.lineno, &utf16_source(srcBuf));
    let result = eval(cx, options, &src, q::JS_EVAL_TYPE_GLOBAL as i32);
    if result.tag() == q::JS_TAG_EXCEPTION {
        return false;
    }
    *rval.ptr = cx.adopt(result);
    true
}

pub(crate) unsafe fn Compile(
    cx: *mut root::JSContext,
    options: *const root::JS::ReadOnlyCompileOptions,
    srcBuf: *mut root::JS::SourceText<u16>,
) -> *mut root::JSScript {
    let cx = Cx::from_raw(cx);
    let options = self::options(options);
    let src = padded(options.base.lineno, &utf16_source(srcBuf));
    let flags = (q::JS_EVAL_TYPE_GLOBAL | q::JS_EVAL_FLAG_COMPILE_ONLY) as i32;
    let result = eval(cx, options, &src, flags);
    if result.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    cx.adopt(result).asBits_ as usize as *mut root::JSScript
}

pub(crate) unsafe fn JS_ExecuteScript1(
    cx: *mut root::JSContext,
    script: root::JS::Handle<*mut root::JSScript>,
) -> bool {
    let mut rval = JSVal::default();
    let handle = root::JS::MutableHandle {
        ptr: &mut rval,
        _phantom_0: std::marker::PhantomData,
    };
    JS_ExecuteScript(cx, script, handle)
}

// Environment chains: the objects of `with`-like scopes, outermost first (event handlers).

pub(crate) unsafe fn NewEnvironmentChain(
    cx: *mut root::JSContext,
    supportUnscopables: root::JS::SupportUnscopables,
) -> *mut root::JS::EnvironmentChain {
    let _ = (cx, supportUnscopables);
    Box::into_raw(Box::new(Vec::<q::JSValue>::new())) as *mut root::JS::EnvironmentChain
}

pub(crate) unsafe fn AppendToEnvironmentChain(
    chain: *mut root::JS::EnvironmentChain,
    obj: *mut root::JSObject,
) -> bool {
    let chain = &mut *(chain as *mut Vec<q::JSValue>);
    chain.push(q::JS_DupValueRT(super::current().rt, super::obj_val(obj)));
    true
}

pub(crate) unsafe fn DeleteEnvironmentChain(chain: *mut root::JS::EnvironmentChain) {
    let chain = Box::from_raw(chain as *mut Vec<q::JSValue>);
    let rt = super::current().rt;
    for v in chain.iter() {
        q::JS_FreeValueRT(rt, *v);
    }
}

/// Compiles `body` as a function `name(args)` whose free variables resolve through the chain's
/// objects (innermost last), like SpiderMonkey's non-syntactic environments: the function is
/// created inside nested `with` statements over the chain objects.
unsafe fn compile_function(
    cx: &Cx,
    chain: *const root::JS::EnvironmentChain,
    options: &CompileOptions,
    name: *const c_char,
    nargs: c_uint,
    argnames: *const *const c_char,
    body: &str,
) -> *mut root::JSFunction {
    let scopes: &[q::JSValue] = if chain.is_null() {
        &[]
    } else {
        &*(chain as *const Vec<q::JSValue>)
    };
    let name = if name.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned()
    };
    let args: Vec<String> = (0..nargs as usize)
        .map(|i| {
            std::ffi::CStr::from_ptr(*argnames.add(i))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let scope_params: Vec<String> = (0..scopes.len())
        .map(|i| format!("__servo_scope{i}"))
        .collect();
    let withs: String = scope_params
        .iter()
        .map(|p| format!("with ({p}) "))
        .collect();
    // The header goes on the line before the body so that line numbers match.
    let src = format!(
        "(function({}) {{ {}return function {}({}) {{\n{}\n}}; }})",
        scope_params.join(", "),
        withs,
        name,
        args.join(", "),
        body
    );
    let buf = padded(options.base.lineno.saturating_sub(1).max(1), &src);
    let outer = eval(cx, options, &buf, q::JS_EVAL_TYPE_GLOBAL as i32);
    if outer.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    let mut argv: Vec<q::JSValue> = scopes.to_vec();
    let f = q::JS_Call(
        cx.qctx(),
        outer,
        q::JS_UNDEFINED,
        argv.len() as i32,
        argv.as_mut_ptr(),
    );
    q::JS_FreeValueRT(cx.rt, outer);
    if f.tag() == q::JS_TAG_EXCEPTION {
        return ptr::null_mut();
    }
    cx.adopt(f).to_object() as *mut root::JSFunction
}

pub(crate) unsafe fn CompileFunction(
    cx: *mut root::JSContext,
    envChain: *const root::JS::EnvironmentChain,
    options: *const root::JS::ReadOnlyCompileOptions,
    name: *const ::std::os::raw::c_char,
    nargs: ::std::os::raw::c_uint,
    argnames: *const *const ::std::os::raw::c_char,
    srcBuf: *mut root::JS::SourceText<u16>,
) -> *mut root::JSFunction {
    let cx = Cx::from_raw(cx);
    let body = utf16_source(srcBuf);
    compile_function(
        cx,
        envChain,
        self::options(options),
        name,
        nargs,
        argnames,
        &body,
    )
}

pub(crate) unsafe fn CompileFunctionUtf8(
    cx: *mut root::JSContext,
    envChain: *const root::JS::EnvironmentChain,
    options: *const root::JS::ReadOnlyCompileOptions,
    name: *const ::std::os::raw::c_char,
    nargs: ::std::os::raw::c_uint,
    argnames: *const *const ::std::os::raw::c_char,
    utf8: *const ::std::os::raw::c_char,
    length: usize,
) -> *mut root::JSFunction {
    let cx = Cx::from_raw(cx);
    let body =
        String::from_utf8_lossy(std::slice::from_raw_parts(utf8 as *const u8, length)).into_owned();
    compile_function(
        cx,
        envChain,
        self::options(options),
        name,
        nargs,
        argnames,
        &body,
    )
}

// Script privates (fetch options of classic scripts), stored by QuickJS (patch Q16) and shared
// with the eval and Function code the script creates.

pub(crate) unsafe fn SetScriptPrivate(script: *mut root::JSScript, value: *const root::JS::Value) {
    let cx = super::current();
    let v = (*value).to_qjs();
    let script = q::JSValue::mkptr(q::JS_TAG_FUNCTION_BYTECODE, script as *mut c_void);
    if q::JS_SetScriptPrivateValue(cx.base_ctx, script, q::JS_DupValueRT(cx.rt, v)) == 0
        && v.tag() != q::JS_TAG_UNDEFINED
    {
        super::module::add_ref_script_private(cx.rt, v);
    }
}

pub(crate) unsafe fn JS_GetScriptPrivate(
    script: *mut root::JSScript,
    dest: root::JS::MutableHandleValue,
) {
    let cx = super::current();
    let script = q::JSValue::mkptr(q::JS_TAG_FUNCTION_BYTECODE, script as *mut c_void);
    // As in SpiderMonkey, a module's code has the module's private value (patch Q31).
    let module = q::JS_GetScriptModule(script);
    let private = if module.is_null() {
        q::JS_GetScriptPrivateValue(cx.base_ctx, script)
    } else {
        q::JS_GetModulePrivateValue(cx.base_ctx, module)
    };
    *dest.ptr = cx.adopt(private);
}

/// The global of the realm a script's code was compiled in (patch Q35), which is the settings
/// object of a module's or classic script's referrer.
pub(crate) unsafe fn JS_GetGlobalFromScript(script: *mut root::JSScript) -> *mut root::JSObject {
    let script = q::JSValue::mkptr(q::JS_TAG_FUNCTION_BYTECODE, script as *mut c_void);
    let qctx = q::JS_GetScriptRealm(script);
    if qctx.is_null() {
        return ptr::null_mut();
    }
    let realm = super::realm::realm_of_qctx(qctx);
    if realm.is_null() {
        ptr::null_mut()
    } else {
        (*realm).global
    }
}
