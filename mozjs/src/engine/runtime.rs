/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Engine and runtime lifetime, GC and stack configuration.

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use servo_quickjs as q;

use super::{set_current, Cx};
use crate::sys::jsapi as root;

pub(crate) unsafe fn JS_Init() -> bool {
    true
}

pub(crate) unsafe fn JS_ShutDown() {}

pub(crate) unsafe fn JS_NewContext(
    maxbytes: u32,
    parentRuntime: *mut root::JSRuntime,
) -> *mut root::JSContext {
    let _ = (maxbytes, parentRuntime);
    let rt = q::JS_NewRuntime();
    if rt.is_null() {
        return ptr::null_mut();
    }
    super::verify::install(rt);
    q::JS_SetContextFinalizer(rt, Some(super::context_finalized));
    let base_ctx = q::JS_NewContext(rt);
    let base_key_object = super::intrinsics::new_key_object(base_ctx);
    let cx = Box::into_raw(Box::new(Cx {
        rt,
        base_ctx,
        realm_stack: Default::default(),
        pool: Default::default(),
        atom_pool: Default::default(),
        warning_reporter: Cell::new(None),
        hooks: Default::default(),
        base_key_object: Cell::new(base_key_object),
        debugger: Default::default(),
    }));
    q::JS_SetRuntimeOpaque(rt, cx as *mut _);
    set_current(cx);
    cx as *mut root::JSContext
}

pub(crate) unsafe fn JS_DestroyContext(raw: *mut root::JSContext) {
    // Finalizers reach the `Cx` through `current()` until the end, so it is only borrowed shared
    // until then, and freed last.
    let cx = Cx::from_raw(raw);
    while !cx.realm_stack.is_empty() {
        cx.leave_realm();
    }
    super::free_microtasks(cx);
    super::free_debuggers(cx);
    // The last two rows of this runtime in the memory-stats CSV: before and after the pool drain.
    append_memory_stats(cx);
    cx.drain_pool((0, 0));
    append_memory_stats(cx);
    q::JS_FreeValueRT(cx.rt, cx.base_key_object.replace(q::JS_UNDEFINED));
    q::JS_FreeContext(cx.base_ctx);
    // Frees every object and realm (realms with their contexts, patch Q15).
    q::JS_FreeRuntime(cx.rt);
    super::realm::forget_leaked_realms();
    set_current(ptr::null());
    // After the finalizers that `JS_FreeRuntime` runs, which still release values into the pool.
    #[cfg(qjs_pool_stats)]
    super::pool_stats::report();
    super::verify::reset();
    super::forget_shared_rcs();
    drop(Box::from_raw(raw as *mut Cx));
}

const MEMORY_STATS_HEADER: &str =
    "t_ms,obj_size,obj_count,dom_stub_size,dom_stub_count,prop_size,prop_count,memory_used_size,malloc_size,pool_count,pool_size\n";

static MEMORY_STATS_LOCK: Mutex<()> = Mutex::new(());
static MEMORY_STATS_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static MEMORY_STATS_ANNOUNCED: OnceLock<()> = OnceLock::new();
static MEMORY_STATS_ERROR_REPORTED: OnceLock<()> = OnceLock::new();

/// The process start time, in milliseconds on the `/proc/uptime` clock.
#[cfg(target_os = "linux")]
fn process_start_ms() -> Option<u64> {
    static START_MS: OnceLock<Option<u64>> = OnceLock::new();
    *START_MS.get_or_init(|| {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Field 22 (starttime) is the 20th token after the comm field. Split on the last ')'
        // so a comm containing spaces or parentheses cannot shift fields.
        let rest = stat.rsplit_once(')')?.1;
        let start_ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
        // SAFETY: `sysconf` with a constant key is always safe to call.
        let ticks_per_s = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if ticks_per_s <= 0 {
            return None;
        }
        Some(start_ticks.saturating_mul(1000) / ticks_per_s as u64)
    })
}

/// Milliseconds since this process started, on the same monotonic `/proc/uptime` clock that
/// `.vscode/pss-sampler.sh` uses. `None` when `/proc` is unavailable.
#[cfg(target_os = "linux")]
fn process_ms() -> Option<u64> {
    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_s: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let uptime_ms = (uptime_s * 1000.0) as u64;
    Some(uptime_ms.saturating_sub(process_start_ms()?))
}

#[cfg(not(target_os = "linux"))]
fn process_ms() -> Option<u64> {
    None
}

/// `$SERVO_MEMORY_STATS_PATH`, or `None` when it is not set. The memory-stats CSV is opt-in:
/// without the environment variable nothing is collected and no file is created.
fn memory_stats_path() -> Option<&'static Path> {
    MEMORY_STATS_PATH
        .get_or_init(|| std::env::var_os("SERVO_MEMORY_STATS_PATH").map(PathBuf::from))
        .as_deref()
}

/// Appends one CSV row. Writes the header when the file is still empty, serializes writers
/// within the process, and reports an I/O error only once.
fn write_memory_stats_row(
    path: &Path,
    usage: super::MemoryUsage,
    t_ms: Option<u64>,
    pool_count: usize,
    pool_size: usize,
) {
    let t = t_ms.map(|v| v.to_string()).unwrap_or_default();
    let row = format!(
        "{t},{obj_size},{obj_count},{dom_stub_size},{dom_stub_count},{prop_size},{prop_count},{memory_used_size},{malloc_size},{pool_count},{pool_size}\n",
        obj_size = usage.obj_size,
        obj_count = usage.obj_count,
        dom_stub_size = usage.dom_stub_size,
        dom_stub_count = usage.dom_stub_count,
        prop_size = usage.prop_size,
        prop_count = usage.prop_count,
        memory_used_size = usage.memory_used_size,
        malloc_size = usage.malloc_size,
    );
    if MEMORY_STATS_ANNOUNCED.set(()).is_ok() {
        eprintln!("memory-stats: writing {}", path.display());
    }
    let _guard = MEMORY_STATS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(MEMORY_STATS_HEADER.as_bytes())?;
        }
        file.write_all(row.as_bytes())
    })();
    if let Err(error) = result {
        if MEMORY_STATS_ERROR_REPORTED.set(()).is_ok() {
            eprintln!("memory-stats: cannot write {}: {error}", path.display());
        }
    }
}

/// Appends one row with `cx`'s current memory usage to `$SERVO_MEMORY_STATS_PATH`: the
/// JSObject, Property and DOM stub sizes/counts, `memory_used_size`, `malloc_size`, and the
/// number and size of values in the Rust-side autorelease pool. When the environment variable
/// is not set, nothing is collected, no file is created and this function returns immediately.
///
/// `t_ms` is milliseconds since process start on the same monotonic `/proc/uptime` clock as
/// `pss-sampler.sh`; it is written as an empty column when `/proc` is unavailable.
///
/// Every call runs one fresh `JS_ComputeMemoryUsage`, an O(live objects) traversal. The call
/// creates no QuickJS heap objects and does not trigger a GC. `JS_DestroyContext` calls it
/// before and after draining the pool (the last two rows of a runtime; the pool is empty in the
/// second), and `runtime.memory_stats()` exposes it to pages.
pub(crate) fn append_memory_stats(cx: &Cx) {
    // Opt-in: skip the heap walk entirely unless the embedder asked for the CSV.
    let Some(path) = memory_stats_path() else {
        return;
    };
    // Capture the time and pool size first, then walk the heap: the row is timestamped at the
    // sample point, not after the (potentially long) `JS_ComputeMemoryUsage`.
    let t_ms = process_ms();
    let pool_count = cx.pool.len();
    let pool_size = pool_count * std::mem::size_of::<q::JSValue>();
    // SAFETY: `cx` is the live runtime of this thread.
    let usage = unsafe { super::memory_usage(cx) };
    write_memory_stats_row(path, usage, t_ms, pool_count, pool_size);
}

pub(crate) unsafe fn JS_GetRuntime(cx: *mut root::JSContext) -> *mut root::JSRuntime {
    Cx::from_raw(cx).rt as *mut root::JSRuntime
}

pub(crate) unsafe fn JS_SetGCParameter(
    cx: *mut root::JSContext,
    key: root::JSGCParamKey,
    value: u32,
) {
    let _ = (cx, key, value);
}

pub(crate) unsafe fn JS_AddExtraGCRootsTracer(
    cx: *mut root::JSContext,
    traceOp: root::JSTraceDataOp,
    data: *mut ::std::os::raw::c_void,
) -> bool {
    // Rust-side roots hold reference counts instead of being traced (see the design doc).
    let _ = (cx, traceOp, data);
    true
}

pub(crate) unsafe fn JS_SetNativeStackQuota(
    cx: *mut root::JSContext,
    systemCodeStackSize: root::JS::NativeStackSize,
    trustedScriptStackSize: root::JS::NativeStackSize,
    untrustedScriptStackSize: root::JS::NativeStackSize,
) {
    let _ = (systemCodeStackSize, trustedScriptStackSize);
    let cx = Cx::from_raw(cx);
    q::JS_SetMaxStackSize(cx.rt, untrustedScriptStackSize as usize);
    q::JS_UpdateStackTop(cx.rt);
}

pub(crate) unsafe fn InitSelfHostedCode(
    cx: *mut root::JSContext,
    cache: root::JS::SelfHostedCache,
    writer: root::JS::SelfHostedWriter,
) -> bool {
    let _ = (cx, cache, writer);
    true
}

pub(crate) unsafe fn SetWarningReporter(
    cx: *mut root::JSContext,
    reporter: root::JS::WarningReporter,
) -> root::JS::WarningReporter {
    Cx::from_raw(cx).warning_reporter.replace(reporter)
}

pub(crate) unsafe fn JS_GC(cx: *mut root::JSContext, reason: root::JS::GCReason) {
    let _ = reason;
    let cx = Cx::from_raw(cx);
    let rt = cx.rt;
    let roots = gc_stats_mode().is_some_and(|mode| mode == "roots" || mode.starts_with("paths:"));
    if roots {
        q::JS_SetGCRootsHook(rt, Some(record_root), ptr::null_mut());
    }
    q::JS_RunGC(rt);
    if roots {
        q::JS_SetGCRootsHook(rt, None, ptr::null_mut());
        print_roots(cx);
    }
    print_gc_stats(rt);
}

fn gc_stats_mode() -> Option<&'static str> {
    static MODE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    MODE.get_or_init(|| std::env::var("QJS_GC_STATS").ok().filter(|v| v != "0"))
        .as_deref()
}

/// With `QJS_GC_STATS=roots`, where Rust rooted an object permanently (`AddRawValueRoot`):
/// the frames of its backtrace in Servo's script crates.
pub(crate) fn note_root_origin(v: q::JSValue) {
    if v.tag() != q::JS_TAG_OBJECT
        || !gc_stats_mode().is_some_and(|mode| mode == "roots" || mode.starts_with("paths:"))
    {
        return;
    }
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    let frames: Vec<&str> = trace
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("at "))
        .filter_map(|l| l.split_once(": ").map(|(_, f)| f))
        .filter(|f| {
            ![
                "mozjs::", "std::", "core::", "alloc::", "<alloc", "<core", "<std", "__rust",
                "rust_",
            ]
            .iter()
            .any(|p| f.starts_with(p))
        })
        .take(10)
        .collect();
    ROOT_ORIGINS.with(|o| o.borrow_mut().insert(v.ptr() as usize, frames.join(" <- ")));
}

pub(crate) fn forget_root_origin(v: q::JSValue) {
    if v.tag() == q::JS_TAG_OBJECT && gc_stats_mode().is_some() {
        ROOT_ORIGINS.with(|o| o.borrow_mut().remove(&(v.ptr() as usize)));
    }
}

thread_local! {
    static ROOT_ORIGINS: std::cell::RefCell<std::collections::HashMap<usize, String>> = std::cell::RefCell::new(Default::default());
    /// The roots of the last `JS_GC` with `QJS_GC_STATS=roots`: (GC thing type, pointer, count).
    static ROOTS: std::cell::RefCell<Vec<(i32, usize, i32)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Only records: during the collection, taking or dropping a reference could free an object.
unsafe extern "C" fn record_root(
    rt: *mut q::JSRuntime,
    gc_obj_type: c_int,
    ptr: *mut c_void,
    ref_count: c_int,
    _opaque: *mut c_void,
) {
    if gc_obj_type >= 0 {
        ROOTS.with(|r| r.borrow_mut().push((gc_obj_type, ptr as usize, ref_count)));
    } else if let Some(class) = gc_stats_mode().and_then(|mode| mode.strip_prefix("paths:")) {
        print_paths(rt, class);
    }
}

unsafe fn gc_thing_name(rt: *mut q::JSRuntime, gc_obj_type: i32, ptr: usize) -> String {
    match gc_obj_type {
        0 => {
            let mut buf = [0 as c_char; 64];
            let id = q::JS_GetClassID(super::obj_val(ptr as *mut root::JSObject));
            let name = q::JS_GetClassNameRT(rt, id, buf.as_mut_ptr(), buf.len() as c_int);
            let name = std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned();
            if name != "Proxy" {
                return name;
            }
            // DOM proxies, such as documents, by their prototype's class.
            let proto = q::JS_GetObjectShapeProto(ptr as *mut c_void);
            if proto.is_null() {
                return name;
            }
            let id = q::JS_GetClassID(super::obj_val(proto as *mut root::JSObject));
            let proto = q::JS_GetClassNameRT(rt, id, buf.as_mut_ptr(), buf.len() as c_int);
            let proto = std::ffi::CStr::from_ptr(proto).to_string_lossy();
            format!(
                "Proxy<{}>",
                proto.strip_suffix("Prototype").unwrap_or(&proto)
            )
        }
        1 => "(bytecode)".to_owned(),
        2 => "(shape)".to_owned(),
        3 => "(var_ref)".to_owned(),
        4 => "(async)".to_owned(),
        5 => "(realm)".to_owned(),
        6 => "(module)".to_owned(),
        _ => "?".to_owned(),
    }
}

struct PathSearch {
    /// GC thing → (its type, the GC thing it was first reached from, or 0 for a root).
    reached: std::collections::HashMap<usize, (i32, usize)>,
    queue: std::collections::VecDeque<usize>,
    from: usize,
}

unsafe extern "C" fn visit_child(
    _rt: *mut q::JSRuntime,
    gc_obj_type: c_int,
    ptr: *mut c_void,
    opaque: *mut c_void,
) {
    let search = &mut *(opaque as *mut PathSearch);
    if let std::collections::hash_map::Entry::Vacant(e) = search.reached.entry(ptr as usize) {
        e.insert((gc_obj_type, search.from));
        search.queue.push_back(ptr as usize);
    }
}

/// With `QJS_GC_STATS=paths:<class>`, the shortest path from a root to each live object of the
/// class, grouped by the classes along the path.
unsafe fn print_paths(rt: *mut q::JSRuntime, class: &str) {
    let mut search = PathSearch {
        reached: Default::default(),
        queue: Default::default(),
        from: 0,
    };
    ROOTS.with(|r| {
        for &(kind, ptr, _) in r.borrow().iter() {
            search.reached.insert(ptr, (kind, 0));
            search.queue.push_back(ptr);
        }
    });
    while let Some(ptr) = search.queue.pop_front() {
        search.from = ptr;
        q::JS_VisitGCChildren(
            rt,
            ptr as *mut c_void,
            Some(visit_child),
            &mut search as *mut PathSearch as *mut c_void,
        );
    }
    let mut paths: std::collections::BTreeMap<String, usize> = Default::default();
    for (&ptr, &(kind, _)) in &search.reached {
        let name = gc_thing_name(rt, kind, ptr);
        if kind != 0 || (name != class && name != format!("Proxy<{class}>")) {
            continue;
        }
        let mut steps = Vec::new();
        let mut at = ptr;
        while at != 0 {
            let (kind, from) = search.reached[&at];
            let mut name = gc_thing_name(rt, kind, at);
            if from == 0 {
                if let Some(origin) = ROOT_ORIGINS.with(|o| o.borrow().get(&at).cloned()) {
                    name = format!("{name}[{origin}]");
                }
                name = format!("ROOT {name}");
            }
            steps.push(name);
            at = from;
        }
        steps.reverse();
        *paths.entry(steps.join(" -> ")).or_default() += 1;
    }
    for (path, n) in paths {
        eprintln!("qjs-paths: {n}x {path}");
    }
}

/// The roots of the last collection by realm and class; they are all still alive.
unsafe fn print_roots(cx: &Cx) {
    use std::collections::BTreeMap;
    let roots = ROOTS.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let mut by_realm: BTreeMap<usize, BTreeMap<String, (usize, i64)>> = BTreeMap::new();
    let mut buf = [0 as c_char; 64];
    for (kind, ptr, count) in roots {
        let (realm, class) = match kind {
            0 => {
                let obj = ptr as *mut root::JSObject;
                let id = q::JS_GetClassID(super::obj_val(obj));
                let name = q::JS_GetClassNameRT(cx.rt, id, buf.as_mut_ptr(), buf.len() as c_int);
                let mut class = std::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned();
                if let Some(origin) = ROOT_ORIGINS.with(|o| o.borrow().get(&ptr).cloned()) {
                    class = format!("{class}[{origin}]");
                }
                (super::class::object_realm(cx, obj), class)
            }
            5 => (
                super::realm::realm_of_qctx(ptr as *mut q::JSContext),
                "(realm)".to_owned(),
            ),
            _ => continue,
        };
        let entry = by_realm
            .entry(realm as usize)
            .or_default()
            .entry(class)
            .or_default();
        entry.0 += 1;
        entry.1 += count as i64;
    }
    for (realm, classes) in by_realm {
        let total: usize = classes.values().map(|c| c.0).sum();
        let mut classes: Vec<_> = classes.into_iter().collect();
        classes.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        let list: Vec<String> = classes
            .iter()
            .take(12)
            .map(|(name, (n, refs))| format!("{name}x{n}({refs})"))
            .collect();
        eprintln!(
            "qjs-roots: realm {realm:#x}: {total} roots: {}",
            list.join(" ")
        );
    }
}

/// With `QJS_GC_STATS=1`, a line of live realms, objects and memory after each `JS_GC`; with
/// `QJS_GC_STATS=classes`, also the classes with the most live objects; with `roots`, also what
/// the collection found referenced from outside, by realm and class; with `paths:<class>`, also
/// the paths from those roots to the live objects of the class. For leak hunting.
unsafe fn print_gc_stats(rt: *mut q::JSRuntime) {
    let Some(mode) = gc_stats_mode() else {
        return;
    };
    let mut usage: q::JSMemoryUsage = std::mem::zeroed();
    q::JS_ComputeMemoryUsage(rt, &mut usage);
    eprintln!(
        "qjs-gc: [{}] realms={} objects={} strings={} functions={} shapes={} malloc={}KiB",
        std::thread::current().name().unwrap_or("?"),
        super::realm::realm_count(),
        usage.obj_count,
        usage.str_count,
        usage.js_func_count,
        usage.shape_count,
        usage.malloc_size / 1024,
    );
    if mode == "1" {
        return;
    }
    let mut counts = vec![0u32; 8192];
    let len = (q::JS_CountObjectsByClass(rt, counts.as_mut_ptr(), counts.len() as u32) as usize)
        .min(counts.len());
    let mut classes: Vec<(u32, usize)> = counts[..len]
        .iter()
        .enumerate()
        .filter(|(_, &n)| n > 0)
        .map(|(id, &n)| (n, id))
        .collect();
    classes.sort_unstable_by(|a, b| b.cmp(a));
    let mut buf = [0 as std::ffi::c_char; 64];
    for (n, id) in classes.into_iter().take(25) {
        let name = q::JS_GetClassNameRT(rt, id as q::JSClassID, buf.as_mut_ptr(), buf.len() as i32);
        eprintln!(
            "qjs-gc:   {n:7} {}",
            std::ffi::CStr::from_ptr(name).to_string_lossy()
        );
    }
}
