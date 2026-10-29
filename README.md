# servo-quickjs

This is an experimental, AI agent-driven experiment of using quickjs instead of spidermonkey in servo.
The main goals are to reduce the binary size and memory usage of servo.
Assume basically everything in this repo, besides these first sentences are written by AI.

--------------------------

[QuickJS](https://bellard.org/quickjs/) as an alternative JavaScript engine for [Servo](https://servo.org).

The `mozjs` crate in this repository implements the API of Servo's `mozjs` crate, the SpiderMonkey bindings, on QuickJS. Servo switches engines with a Cargo `[patch]`, without changes to its `Cargo.toml`.

Status: experimental. Servo runs on it with its `js-quickjs` feature, which is not in upstream Servo. On the WPT directories `dom`, `WebIDL`, `html/webappapis`, `html/semantics/scripting-1` and `encoding`, the results equal SpiderMonkey's, apart from 7 subtests that pass only on QuickJS.

## Crates

- `servo-quickjs`: Fabrice Bellard's QuickJS (upstream commit in `servo-quickjs/quickjs/UPSTREAM`), vendored with local patches, and pregenerated bindings.
- `mozjs`: the `mozjs` 0.26.9 API on QuickJS. Its version must equal the `mozjs` version Servo pins.
- `gen-bindings`: regenerates the bindings of `servo-quickjs` (`just gen-bindings`).
- `fuzz`: cargo-fuzz targets.

[docs/design.md](docs/design.md) describes how SpiderMonkey's API maps onto QuickJS, and lists the QuickJS patches.

## Building Servo with it

```sh
cargo build -p servoshell --features js-quickjs \
    --config 'patch.crates-io.mozjs.git="https://github.com/openharmony-rs/servo-quickjs"'
```

`main` implements the `mozjs` version that [servo-ohos](https://github.com/openharmony-rs/servo-ohos)'s `ohos-main` pins. The last commit for an older version is tagged `mozjs-<version>` (e.g. `mozjs-0.23.1`); add `--config 'patch.crates-io.mozjs.tag="mozjs-0.23.1"'` to build a Servo that pins it.

Use `patch.crates-io.mozjs.path="/path/to/servo-quickjs/mozjs"` for a local checkout. `mach` cannot pass `--config`; put the `[patch.crates-io]` entry in Servo's `.cargo/config.toml` instead.

## Differences from SpiderMonkey

- No JIT, WebAssembly, devtools debugger or `SharedArrayBuffer`.
- `Intl` is a subset implemented over the OS's ICU on OpenHarmony and glibc Linux, and absent elsewhere (see [docs/design.md](docs/design.md#intl)). On Linux it needs the system ICU, 65 or later (`libicu-dev`), unless the `intl` feature is disabled.
- QuickJS frees objects by reference counting and collects cycles. Servo's tracing must report every reference exactly once; `QJS_VERIFY_TRACING=1` checks that at run time.

## Development

[HACKING.md](HACKING.md) explains how SpiderMonkey functions are implemented. The [justfile](justfile) has the common tasks:

- `just test`, and `just test-verify` with the tracing verifier;
- `just test-asan`: the tests under ASan and LSan (nightly);
- `just fuzz <target>` and `just fuzz-smoke`;
- `just check-bindings`: fails if the bindings are out of date;
- `just servo`, `just run <url>` and `just wpt <log> <tests>` with a Servo checkout in `SERVO_DIR`.

`QJS_GC_STATS` prints live objects, roots and retaining paths after each GC (see `mozjs/src/engine/runtime.rs`).

A build with `QJS_POOL_STATS=1` records which API functions put values in the autorelease pool, and whether a root took them before it drained; `tools/pool_stats.py` summarizes the output (see `mozjs/src/engine/pool_stats.rs`, `just test-pool-stats`).

## License

- `mozjs`, `gen-bindings`, `fuzz` and `tools`: [MPL-2.0](LICENSE-MPL). Much of `mozjs` is copied from [servo/mozjs](https://github.com/servo/mozjs), under the same license.
- `servo-quickjs`: [MIT](servo-quickjs/LICENSE), as QuickJS itself ([servo-quickjs/quickjs/LICENSE](servo-quickjs/quickjs/LICENSE)).
- `mozjs/src/engine/intl/dtpg.rs` (a port of ICU's date pattern generator) and `mozjs/src/engine/intl/patterns.rs` (CLDR data): [Unicode-3.0](LICENSE-UNICODE).
