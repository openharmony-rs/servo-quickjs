/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! GC, memory reporting and engine options; mostly no-ops on QuickJS.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::{c_char, c_void};
use std::ptr;

use servo_quickjs as q;

use super::Cx;
use crate::sys::jsapi as root;

/// Per-thread engine state that SpiderMonkey keeps on its `JSContext`.
struct Misc {
    context_private: *mut c_void,
    options: Box<root::JS::ContextOptions>,
    /// `AddRawValueRoot` locations and the value each one holds a reference to.
    raw_roots: HashMap<usize, q::JSValue>,
}

thread_local! {
    static MISC: RefCell<Misc> = RefCell::new(Misc {
        context_private: ptr::null_mut(),
        options: Box::new(unsafe { std::mem::zeroed() }),
        raw_roots: HashMap::new(),
    });
}

// Context state.

pub(crate) unsafe fn JS_SetContextPrivate(cx: *mut root::JSContext, data: *mut c_void) {
    let _ = cx;
    MISC.with(|m| m.borrow_mut().context_private = data);
}

pub(crate) unsafe fn JS_GetContextPrivate(cx: *mut root::JSContext) -> *mut c_void {
    let _ = cx;
    MISC.with(|m| m.borrow().context_private)
}

pub(crate) unsafe fn ContextOptionsRef(cx: *mut root::JSContext) -> *mut root::JS::ContextOptions {
    let _ = cx;
    MISC.with(|m| &mut *m.borrow_mut().options as *mut _)
}

pub(crate) unsafe fn JS_GetParentRuntime(cx: *mut root::JSContext) -> *mut root::JSRuntime {
    Cx::from_raw(cx).rt as *mut root::JSRuntime
}

pub(crate) unsafe fn JS_AbortIfWrongThread(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn GetWarningReporter(cx: *mut root::JSContext) -> root::JS::WarningReporter {
    Cx::from_raw(cx).warning_reporter.get()
}

// Permanent roots (Servo's `PermanentRoot`): one reference per rooted location.

pub(crate) unsafe fn AddRawValueRoot(
    cx: *mut root::JSContext,
    vp: *mut root::JS::Value,
    name: *const c_char,
) -> bool {
    let _ = name;
    let cx = Cx::from_raw(cx);
    let v = q::JS_DupValueRT(cx.rt, (*vp).to_qjs());
    super::runtime::note_root_origin(v);
    let old = MISC.with(|m| m.borrow_mut().raw_roots.insert(vp as usize, v));
    if let Some(old) = old {
        q::JS_FreeValueRT(cx.rt, old);
    }
    true
}

pub(crate) unsafe fn RemoveRawValueRoot(cx: *mut root::JSContext, vp: *mut root::JS::Value) {
    let cx = Cx::from_raw(cx);
    if let Some(v) = MISC.with(|m| m.borrow_mut().raw_roots.remove(&(vp as usize))) {
        super::runtime::forget_root_origin(v);
        q::JS_FreeValueRT(cx.rt, v);
    }
}

// GC: QuickJS frees by reference counting and runs its cycle collector on allocation; only an
// explicit full collection is meaningful.

pub(crate) unsafe fn NonIncrementalGC(
    cx: *mut root::JSContext,
    options: root::JS::GCOptions,
    reason: root::JS::GCReason,
) {
    let _ = (options, reason);
    q::JS_RunGC(Cx::from_raw(cx).rt);
}

pub(crate) unsafe fn JS_MaybeGC(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn PrepareForFullGC(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn PrepareForIncrementalGC(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn StartIncrementalGC(
    cx: *mut root::JSContext,
    options: root::JS::GCOptions,
    reason: root::JS::GCReason,
    budget: *const root::JS::SliceBudget,
) {
    let _ = (options, reason, budget);
    q::JS_RunGC(Cx::from_raw(cx).rt);
}

pub(crate) unsafe fn IncrementalGCSlice(
    cx: *mut root::JSContext,
    reason: root::JS::GCReason,
    budget: *const root::JS::SliceBudget,
) {
    let _ = (cx, reason, budget);
}

pub(crate) unsafe fn FinishIncrementalGC(cx: *mut root::JSContext, reason: root::JS::GCReason) {
    let _ = (cx, reason);
}

pub(crate) unsafe fn AbortIncrementalGC(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn IsGCScheduled(cx: *mut root::JSContext) -> bool {
    let _ = cx;
    false
}

pub(crate) unsafe fn IsIncrementalGCEnabled(cx: *mut root::JSContext) -> bool {
    let _ = cx;
    false
}

pub(crate) unsafe fn IsIncrementalGCInProgress(cx: *mut root::JSContext) -> bool {
    let _ = cx;
    false
}

pub(crate) unsafe fn RuntimeHeapState() -> root::JS::HeapState {
    // Freeing a garbage cycle corresponds to SpiderMonkey's finalization during GC. Freeing an
    // object whose count dropped to zero doesn't: the objects it reaches are still alive.
    match super::try_current() {
        Some(cx) if q::JS_IsCollectingCycles(cx.rt) != 0 => root::JS::HeapState::MajorCollecting,
        _ => root::JS::HeapState::Idle,
    }
}

pub(crate) unsafe fn JS_TracerIsTracing(cx: *mut root::JSContext) -> bool {
    let _ = cx;
    false
}

pub(crate) unsafe fn JS_GetGCParameter(cx: *mut root::JSContext, key: root::JSGCParamKey) -> u32 {
    let _ = (cx, key);
    0
}

pub(crate) unsafe fn JS_ResetGCParameter(cx: *mut root::JSContext, key: root::JSGCParamKey) {
    let _ = (cx, key);
}

pub(crate) unsafe fn JS_SetGCParametersBasedOnAvailableMemory(
    cx: *mut root::JSContext,
    availMemMB: u32,
) {
    let _ = (cx, availMemMB);
}

pub(crate) unsafe fn JS_SetGCCallback(
    cx: *mut root::JSContext,
    cb: root::JSGCCallback,
    data: *mut c_void,
) {
    let _ = (cx, cb, data);
}

pub(crate) unsafe fn SetGCSliceCallback(
    cx: *mut root::JSContext,
    callback: root::JS::GCSliceCallback,
) -> root::JS::GCSliceCallback {
    let _ = (cx, callback);
    None
}

pub(crate) unsafe fn SetCreateGCSliceBudgetCallback(
    cx: *mut root::JSContext,
    cb: root::JS::CreateSliceBudgetCallback,
) {
    let _ = (cx, cb);
}

pub(crate) unsafe fn AddGCNurseryCollectionCallback(
    cx: *mut root::JSContext,
    callback: root::JS::GCNurseryCollectionCallback,
    data: *mut c_void,
) -> bool {
    let _ = (cx, callback, data);
    true
}

pub(crate) unsafe fn RemoveGCNurseryCollectionCallback(
    cx: *mut root::JSContext,
    callback: root::JS::GCNurseryCollectionCallback,
    data: *mut c_void,
) {
    let _ = (cx, callback, data);
}

pub(crate) unsafe fn JS_AddFinalizeCallback(
    cx: *mut root::JSContext,
    cb: root::JSFinalizeCallback,
    data: *mut c_void,
) -> bool {
    let _ = (cx, cb, data);
    true
}

pub(crate) unsafe fn JS_RemoveFinalizeCallback(
    cx: *mut root::JSContext,
    cb: root::JSFinalizeCallback,
) {
    let _ = (cx, cb);
}

pub(crate) unsafe fn JS_AddWeakPointerZonesCallback(
    cx: *mut root::JSContext,
    cb: root::JSWeakPointerZonesCallback,
    data: *mut c_void,
) -> bool {
    let _ = (cx, cb, data);
    true
}

pub(crate) unsafe fn JS_AddWeakPointerCompartmentCallback(
    cx: *mut root::JSContext,
    cb: root::JSWeakPointerCompartmentCallback,
    data: *mut c_void,
) -> bool {
    let _ = (cx, cb, data);
    true
}

pub(crate) unsafe fn JS_RemoveExtraGCRootsTracer(
    cx: *mut root::JSContext,
    traceOp: root::JSTraceDataOp,
    data: *mut c_void,
) {
    let _ = (cx, traceOp, data);
}

pub(crate) unsafe fn JS_SetSizeOfIncludingThisCompartmentCallback(
    cx: *mut root::JSContext,
    callback: root::JSSizeOfIncludingThisCompartmentCallback,
) {
    let _ = (cx, callback);
}

pub(crate) unsafe fn JS_SetDestroyZoneCallback(
    cx: *mut root::JSContext,
    callback: root::JSDestroyZoneCallback,
) {
    let _ = (cx, callback);
}

pub(crate) unsafe fn SetLowMemoryState(cx: *mut root::JSContext, newState: bool) {
    if newState {
        q::JS_RunGC(Cx::from_raw(cx).rt);
    }
}

pub(crate) unsafe fn SetOutOfMemoryCallback(
    cx: *mut root::JSContext,
    cb: root::JS::OutOfMemoryCallback,
    data: *mut c_void,
) {
    let _ = (cx, cb, data);
}

pub(crate) unsafe fn ClearKeptObjects(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn SetHostCleanupFinalizationRegistryCallback(
    cx: *mut root::JSContext,
    cb: root::JSHostCleanupFinalizationRegistryCallback,
    data: *mut c_void,
) {
    let _ = (cx, cb, data);
}

pub(crate) unsafe fn SetHostEnsureCanAddPrivateElementHook(
    cx: *mut root::JSContext,
    op: root::JS::EnsureCanAddPrivateElementOp,
) {
    let _ = (cx, op);
}

pub(crate) unsafe fn JS_SetAllNonReservedSlotsToUndefined(obj: root::JS::HandleObject) {
    let _ = obj;
}

// Memory reporting.

unsafe fn malloc_size(cx: &Cx) -> usize {
    let mut usage: q::JSMemoryUsage = std::mem::zeroed();
    q::JS_ComputeMemoryUsage(cx.rt, &mut usage);
    usage.malloc_size.max(0) as usize
}

pub(crate) unsafe fn GetGCHeapUsage(cx: *mut root::JSContext) -> u64 {
    malloc_size(Cx::from_raw(cx)) as u64
}

pub(crate) unsafe fn CollectServoSizes(
    cx: *mut root::JSContext,
    sizes: *mut root::JS::ServoSizes,
    gs: crate::sys::glue::GetSize,
) -> bool {
    let _ = gs;
    (*sizes).gcHeapUsed += malloc_size(Cx::from_raw(cx));
    true
}

pub(crate) unsafe fn AddServoSizeOf(
    cx: *mut root::JSContext,
    mallocSizeOf: root::mozilla::MallocSizeOf,
    opv: *mut root::JS::ObjectPrivateVisitor,
    sizes: *mut root::JS::ServoSizes,
) -> bool {
    let _ = (mallocSizeOf, opv);
    (*sizes).gcHeapUsed += malloc_size(Cx::from_raw(cx));
    true
}

pub(crate) unsafe fn InitializeMemoryReporter(wtm: crate::sys::glue::WantToMeasure) {
    let _ = wtm;
}

pub(crate) unsafe fn AddAssociatedMemory(
    obj: *mut root::JSObject,
    nbytes: usize,
    use_: root::JS::MemoryUse,
) {
    let _ = (obj, nbytes, use_);
}

pub(crate) unsafe fn RemoveAssociatedMemory(
    obj: *mut root::JSObject,
    nbytes: usize,
    use_: root::JS::MemoryUse,
) {
    let _ = (obj, nbytes, use_);
}

// Engine options without a QuickJS counterpart (no JIT, no WebAssembly, no helper threads).

pub(crate) unsafe fn DisableJitBackend() {}

pub(crate) unsafe fn JS_SetGlobalJitCompilerOption(
    cx: *mut root::JSContext,
    opt: root::JSJitCompilerOption,
    value: u32,
) {
    let _ = (cx, opt, value);
}

pub(crate) unsafe fn JS_GetGlobalJitCompilerOption(
    cx: *mut root::JSContext,
    opt: root::JSJitCompilerOption,
    valueOut: *mut u32,
) -> bool {
    let _ = (cx, opt);
    *valueOut = 0;
    true
}

pub(crate) unsafe fn JS_SetOffthreadIonCompilationEnabled(cx: *mut root::JSContext, enabled: bool) {
    let _ = (cx, enabled);
}

pub(crate) unsafe fn JS_SetOffthreadBaselineCompilationEnabled(
    cx: *mut root::JSContext,
    enabled: bool,
) {
    let _ = (cx, enabled);
}

pub(crate) unsafe fn JS_SetFutexCanWait(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn SetProcessBuildIdOp(buildIdOp: root::JS::BuildIdOp) {
    let _ = buildIdOp;
}

pub(crate) unsafe fn SetBuildId(
    buildId: *mut root::JS::BuildIdCharVector,
    chars: *const c_char,
    len: usize,
) -> bool {
    let _ = (buildId, chars, len);
    true
}

pub(crate) unsafe fn InitConsumeStreamCallback(
    cx: *mut root::JSContext,
    consume: root::JS::ConsumeStreamCallback,
    report: root::JS::ReportStreamErrorCallback,
) {
    let _ = (cx, consume, report);
}

pub(crate) unsafe fn SetUpEventLoopDispatch(
    cx: *mut root::JSContext,
    callback: crate::sys::glue::RustDispatchToEventLoopCallback,
    startedCallback: root::JS::AsyncTaskStartedCallback,
    finishedCallback: root::JS::AsyncTaskFinishedCallback,
    closure: *mut c_void,
) {
    let _ = (cx, callback, startedCallback, finishedCallback, closure);
}

pub(crate) unsafe fn ShutdownAsyncTasks(cx: *mut root::JSContext) {
    let _ = cx;
}

pub(crate) unsafe fn EnableContextProfilingStack(cx: *mut root::JSContext, enabled: bool) {
    let _ = (cx, enabled);
}

pub(crate) unsafe fn SetContextProfilingStack(
    cx: *mut root::JSContext,
    profilingStack: *mut root::ProfilingStack,
) {
    let _ = (cx, profilingStack);
}

pub(crate) unsafe fn SetScriptEnvironmentPreparer(
    cx: *mut root::JSContext,
    preparer: *mut root::js::ScriptEnvironmentPreparer,
) {
    let _ = (cx, preparer);
}

pub(crate) unsafe fn GCTraceKindToAscii(kind: root::JS::TraceKind) -> *const c_char {
    use root::JS::TraceKind;
    let name: &'static std::ffi::CStr = match kind {
        TraceKind::Object => c"Object",
        TraceKind::BigInt => c"BigInt",
        TraceKind::String => c"String",
        TraceKind::Symbol => c"Symbol",
        TraceKind::Script => c"Script",
        _ => c"Internal",
    };
    name.as_ptr()
}
