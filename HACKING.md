# Implementing SpiderMonkey API functions on QuickJS

Design: `docs/design.md`. Read it first.

## How a function gets implemented

- `mozjs/upstream-bindgen/*.rs` is SpiderMonkey's bindgen output for mozjs_sys 153.3.0-3 (x86_64 Linux, from a Servo debug build's `mozjs_sys` `OUT_DIR`). Don't edit it.
- `mozjs/build.rs` runs `mozjs/stub_bindings.py`. It turns every SpiderMonkey function into a Rust function:
  - If `mozjs/src/engine/**/*.rs` contains `pub(crate) unsafe fn NAME(`, the function forwards to it.
  - Otherwise its body panics with "`NAME` is not implemented".
- To implement `NAME`:
  1. Run `python3 tools/sig.py NAME`. It prints the exact signature.
  2. Paste it into the engine module for its area. Engine modules alias the jsapi module as `root`; types that exist only in the glue bindings (e.g. `GetSize`, `ProxyTraps`) are in `crate::sys::glue`.
  3. Write the body. Rebuilding picks it up.
- Helpers that are not SpiderMonkey functions should be `pub(crate) fn` or `pub(crate) unsafe fn` with a name that is not a SpiderMonkey function name.
- The Rust layers copied from mozjs (`mozjs/src/*.rs`, `mozjs/src/sys/*.rs`) may be changed where they depend on SpiderMonkey internals. Mark each such change with a `QuickJS:` comment.

## Rules

- **Context and realm.** `Cx::from_raw(cx)` gives the runtime state; `cx.qctx()` is the QuickJS context of the current realm. Functions without a `cx` parameter use `super::current()`.
- **Re-entrancy.** Any call into QuickJS or the embedder can run natives, hooks and finalizers that reach the same runtime state (`current()`) and object data (`JS_GetOpaque`) again. Allocating a QuickJS object counts too: it can start a GC, whose `gc_mark` hooks read object data. If that code touches data behind a `&mut` that is used after the call, or that is an argument of the function making it, the behaviour is undefined, and the compiler optimizes on it.
  - `Cx` is only borrowed shared (`&Cx`), and only pointed to by `*const Cx`. State that changes goes in a cell: a `Cell`, or a `Stack`/`Queue` from `engine/reentrant.rs`, which hand out copies only. Never add a method that returns a reference into one.
  - For object data, copy out what you need and end the `&mut` borrow before the call. Free replaced values after the borrow ends, not while it is live. New accessors return `&T`, with cells for the fields that change. Such a reference is only valid while the object is alive, so keep the object retained (rooted, pooled or counted) while you use it. A `RefCell` in object data is only borrowed around Rust code that calls nothing in QuickJS: `gc_mark` hooks borrow it during any GC, and a conflicting borrow aborts.
  - What is checked:
    - `src/engine` denies clippy's `mut_from_ref` (`just lint`, in CI). It rejects a `&mut` returned from a shared reference, such as a `&mut` into a cell or an accessor taking `cx: &Cx`.
    - `tests/qjs_aliasing_rules.rs` rejects `&mut Cx` in a type, a `&mut self` method of `Cx`, and a `*mut Cx` or `NonNull<Cx>` outside `JS_DestroyContext`.
    - The same test lists the functions that hand out a `&mut` from a raw pointer with a lifetime no argument bounds. Only structured clone's writer, reader and data buffer are left: they pass the caller's own data through the embedder's callbacks, and each borrow ends before the caller's is used again.
    - None of these sees whether a `&mut` stays in use across a call into QuickJS. Neither the compiler nor Miri (which can't run the C code) checks that; review does.
- **Values** (`JSVal`) are QuickJS `JSValue`s (`.to_qjs()`, `JSVal::from_qjs()`). `*mut JSObject`, `*mut JSString` and `*mut JS::Symbol` are the QuickJS pointers. Use `obj_val(obj)` / `val_obj(v)`.
- **Ownership.**
  - Anything returned to Rust or written to an out-parameter must stay alive for the rest of the task:
    - `cx.adopt(v)` takes over a reference you own (e.g. a QuickJS API result);
    - `cx.retain(v)` adds one.
  - Never return a borrowed QuickJS value to Rust without one of these.
  - Values passed *into* QuickJS functions that consume a reference (`JS_SetProperty`, `JS_DefinePropertyValue`, `JS_Throw`, …) need a `JS_DupValueRT` first.
  - Free every temporary you own.
  - Atoms work the same way: `cx.adopt_atom`, `JS_FreeAtomRT`.
- **Counted storage** (`Heap<T>`, reserved slots, anything kept beyond the task) holds exactly one reference. Only such storage may be marked during `gc_mark`; marking anything else causes use-after-free.
- **Strings.**
  - Rust never sees a rope: `cx.adopt`/`cx.retain` flatten them.
  - Create strings with `JS_NewStringLatin1` / `JS_NewStringUTF16` (patch Q3) or `JS_NewStringLen` (UTF-8).
  - Read them with `JS_GetStringData`.
- **Exceptions.** On failure, return `false` / null with a QuickJS exception pending (`JS_Throw*`). A `false` without an exception means uncatchable termination.
- **Property keys** (`jsid`) map to QuickJS atoms: `id.to_atom()` (borrowed), `crate::sys::jsid::from_atom`.
- **QuickJS patches** go in `servo-quickjs/quickjs`:
  - Mark them with a `/* Servo: ... */` comment.
  - Add the declaration to `quickjs.h` and run `just gen-bindings`.
  - Commit them separately with a `quickjs:` prefix.
  - List them in the design doc's patch table.

## Testing

- `just test` (`QJS_TARGET_DIR` sets the target dir), and `just test-verify` with the tracing verifier.
- The mozjs 0.26.7 tests are in `mozjs/tests/` (see `mozjs/upstream-tests/README.md` for the local changes).
- Add focused tests for your area as `mozjs/tests/qjs_<area>.rs`.
- `qjs_intl` expects ICU 74's data, on Linux the system ICU's (Ubuntu 24.04's).

## Commits

- Sign off every commit (`git commit -s`, [Developer Certificate of Origin](https://developercertificate.org/)).
- Short subject and a short body. The prefix names the part:
  - `quickjs: … (Qn)` for one QuickJS patch: the C change, `quickjs.h`, the bindings and its row in the patch table;
  - `mozjs: …` for the shim;
  - `quickjs, mozjs: … (Qn)` for a small patch together with its use in the shim, and always when a patch changes a QuickJS API the shim already uses, so neither builds without the other.
- Every commit builds (`cargo check -p mozjs --tests`) and is formatted.
- Commits written with an AI agent carry an `AI-assisted:` trailer before the sign-off, e.g. `AI-assisted: Yes (agent session; written and tested by the agent)`.

## Branches and versions

- `main` is linear. A topic branch is rebased onto `main` and lands by fast-forward once CI passed on it (a pull request, or a manual run of the workflow).
- `main` follows the `mozjs` version of servo-ohos's `ohos-main`. A version update is one `mozjs: port to mozjs <version>` commit, after the QuickJS patches it needs. Before it lands, tag the last commit for the old version `mozjs-<old version>`.
- Fixes for an older version go on a branch `mozjs-<version>` made from its tag, only when someone needs them.
