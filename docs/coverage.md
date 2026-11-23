# Coverage

How much of the shim do the tests cover, and how much of that is checked against SpiderMonkey?
The `coverage-*` recipes measure both. They compare the result with what Servo's WPT run reaches.
The same recipes measure SpiderMonkey's API layer, the reference (see the section of that name).

The report has three groups:

- **all**: every `mozjs` test, run on QuickJS.
- **runs on SM**: tests whose file also compiles against the crates.io `mozjs` of the same version,
  that is, on SpiderMonkey.
- **agrees**: tests that pass on both engines. Only these check the shim against a reference; the
  others check it against what their author expected of QuickJS.

Coverage is LLVM source-based coverage of `mozjs/src` and `servo-quickjs/src`, and it counts two
things:

- **Code regions.** A region is a span of code that runs as a unit: a function body, a block, a `match`
  arm, the right operand of `&&`. A region is covered when it ran at least once.
- **Condition outcomes** (`-Zcoverage-options=condition`). Each condition has two outcomes, true and
  false. Conditions are the expression of an `if`, `while` or `let … else`, and each operand of
  `&&`/`||`. An outcome is covered when the condition evaluated to it at least once.

A condition that WPT drives both ways but the tests only one way is an outcome gap, even when every
region around it is covered. `match` arms and `?` exits have regions but no outcomes. Combinations of
operands (MC/DC) are not measured.

## Recipes

```sh
just coverage            # coverage-units, coverage-sm and coverage-report
just coverage-units      # each mozjs test alone in an instrumented build, one profile per test
just coverage-sm         # mozjs/tests against crates.io mozjs, each test alone
just coverage-servo      # servoshell from SERVO_DIR against this checkout, instrumented
just coverage-wpt [WPT args]   # WPT with that servoshell; exports what it reaches
just coverage-report     # the groups, against the WPT reach set if there is one

# SpiderMonkey's side (see "SpiderMonkey's API layer" below)
just coverage-spidermonkey     # mozjs/tests on SpiderMonkey built from source, both instrumented
just coverage-servo-sm         # servoshell on that SpiderMonkey, instrumented
just coverage-wpt-sm [WPT args]
just coverage-report --engine spidermonkey
```

Outputs go to `$QJS_TARGET_DIR/coverage`, which defaults to `target/coverage`:

- `units/results.tsv` and `sm/results.tsv`: one line per test, with its id and status (`pass`,
  `fail`, `timeout`, `crash`, `ignored`, or `compile-error` for a whole SpiderMonkey test file);
- `wpt/reach.json`: the WPT export, with `wpt/meta.json` taken from the binary's build record;
- `report/quickjs/summary.json` and `report/spidermonkey/summary.json`: the numbers of the last report.

### WPT

`coverage-servo` builds servoshell with `--features js-quickjs`, so `SERVO_DIR` must be a Servo
checkout with that feature (servo-ohos). The build uses its own target dir,
`SERVO_COVERAGE_TARGET_DIR` (default `$SERVO_TARGET_DIR-coverage`), and the `medium` profile
(`SERVO_COVERAGE_PROFILE`).

`tools/coverage-rustc-wrapper.sh` instruments the shim, every crate that links it, and servoshell.
The shim's inlined and generic functions are compiled into the crates that use them, such as
`script` and `script_bindings`, so instrumenting only `mozjs` misses that code. Cargo does not see the
wrapper's flags, so never build into that target dir without the wrapper.

`coverage-wpt` passes its arguments to `./mach test-wpt`; without test paths it runs the whole suite.
It sets `--timeout-multiplier 2`, because the instrumented build is slower. Unexpected results are
normal on QuickJS and do not stop the export. A full run takes hours: use `--processes N`, and expect a
serial tail at the end. The profiles use continuous mode (`%c` in `LLVM_PROFILE_FILE`), so browsers
that wptrunner kills still count.

## Reading the report

- **Absolute numbers** come from the unit builds alone. They are stable from commit to commit and can
  be compared over time.
- **The WPT columns** say how much of what WPT reaches each group covers. The gap of `agrees` is the
  to-do list, in the per-file table at the end.
- **The WPT reach set belongs to one version of the shim's sources.** Regions are keyed by file and
  source span, so any edit shifts the keys below it.
  - `coverage-servo` writes `servoshell.coverage.json` next to the binary. It holds a digest of the
    sources it was built from, uncommitted changes included, and its rustc.
  - `coverage-wpt` refuses a binary without that record, or one rebuilt after it, and the export
    carries the record over.
  - The report compares the reach set only when the units used the same sources; otherwise it says
    so and leaves the WPT columns out. Rerun `coverage-servo` and `coverage-wpt` after changing
    `mozjs/src` or `servo-quickjs/src`.
- **Agreement needs the same tests on both engines.** Results join by test name, so the report refuses
  to compare runs whose `mozjs/tests` or mozjs versions differ. Rerun the older of `coverage-units` and
  `coverage-sm` (or `coverage-spidermonkey`).
- **The unit builds use Servo's pinned toolchain** (`SERVO_DIR/rust-toolchain.toml`, or
  `COVERAGE_TOOLCHAIN`), so region spans match the WPT build. Both need the `llvm-tools` component.
  Condition coverage is unstable, so the instrumented crates are built with `RUSTC_BOOTSTRAP=1`.

## Is a feature covered?

"Relevant" here means code that Servo runs: the WPT reach set. Code that WPT never runs is left out
of every WPT number. It may be dead for Servo, or used by pages WPT doesn't cover, so the reach set is
only as good as the run behind it. Use the whole suite; a subset only describes itself.

1. Measure the reach set for the shim's current sources: `just coverage-servo`, then
   `just coverage-wpt --processes N`.
2. Restrict the report to the feature. `--match` is a regex that matches the file
   (`mozjs/src/engine/proxy.rs`) or the function path (`mozjs::engine::proxy::proxy_get`):

   ```sh
   just coverage-report --match 'engine/proxy\.rs'          # one file
   just coverage-report --match 'engine/intl/'              # a directory
   just coverage-report --match '::JS_DefineProperty'       # functions by name
   ```

3. Read the `agrees` row of the WPT table. The feature is covered when both of its gaps are 0: every
   region and every condition outcome that WPT runs in it also ran in a test that passes on
   SpiderMonkey and on QuickJS.
4. Find what is missing:
   - `--gaps N` lists the N functions with the hottest uncovered code: their line ranges with the
     largest WPT count, and each condition outcome that WPT takes and no agreeing test does.
   - `--lcov FILE` writes the same as an lcov file for an editor's coverage view, with paths relative
     to this repository. WPT-reached lines that the group covers show as hit, the gap as missed. Lines
     WPT doesn't run are not marked at all, so whatever an editor shows is relevant code.
   - `--group 'runs on SM'` or `--group all` shows the other groups' gaps instead.

What "covered" does not mean:

- **It is not checked.** A region counts as covered when an agreeing test ran it, not when the test
  observed its result. A test may run a branch and assert something else. Mutation testing would
  measure that; these recipes don't.
- **It is not every input.** A region covered with a plain object may still differ for a DOM object,
  a proxy, a cross-realm object, a two-byte string or a lone surrogate.
- **Agreement is per test.** One assertion that differs on SpiderMonkey drops the whole test from
  `agrees`, with everything else it covers. Splitting a large test keeps the rest.
- **It is not missing code.** A case that the shim doesn't handle has no region to be uncovered. The
  SpiderMonkey side shows these.

## SpiderMonkey's API layer

The shim's coverage cannot show a case that the shim lacks. SpiderMonkey, the reference, has code for
it, so its coverage can. If Servo on SpiderMonkey takes a branch of SpiderMonkey's API layer that no
agreeing test takes, that case is untested, however the shim handles it, or whether it handles it at
all. A test that takes the branch, run on both engines, tells which.

- **Sources:** the files of `js/src` that define a `JS_PUBLIC_API` or `JS_FRIEND_API` function, the
  `mozjs_sys` glue (`src/`) and the `mozjs` crate's Rust (`src/`). `jit/`, `frontend/`, `irregexp/`,
  `wasm/` and `gc/` are left out: their branches are SpiderMonkey's implementation choices, not API
  cases, and test262 covers QuickJS's parser and regexp engine. Paths in the report are relative to the
  crates: `js/src/jsapi.cpp`, `mozjs_sys/src/jsglue.cpp`, `mozjs/src/rust.rs`.
- **Build:** `MOZJS_FROM_SOURCE=1`, with SpiderMonkey's C++ instrumented by the clang of rustc's LLVM
  version (`clang-23` for rustc 1.99.1; `COVERAGE_CLANG` overrides). The Rust side is instrumented as
  for the shim, and both share rustc's profiling runtime, which needs the same LLVM version. bindgen
  uses that clang's libclang and include paths (`LIBCLANG_PATH`, `CLANG_PATH`); with another version's
  builtin headers, `mmintrin.h` doesn't parse. The C++ uses atomic counter updates, because
  SpiderMonkey's helper threads otherwise make llvm-cov's derived counts wrap around.
- **Groups:** `all` are the tests that run on SpiderMonkey, `runs on QuickJS` those that also run on
  the shim, and `agrees` those that pass on both. `coverage-units` provides the QuickJS side.
- **Relevance:** `coverage-servo-sm` and `coverage-wpt-sm` build servoshell on the instrumented
  SpiderMonkey (`SERVO_COVERAGE_SM_TARGET_DIR`, default `$SERVO_TARGET_DIR-coverage-sm`) and record
  what WPT reaches there. That is what Servo does on the engine it ships with, independent of the
  shim.

`--match`, `--gaps` and `--lcov` work the same; demangled C++ names such as `JS::GetCurrentRealmOrNull`
match too. Expect internal functions in these files as well: a branch on an object's shape or a
string's representation is not always a case the API distinguishes.

## Costs

- **`coverage-units`:** one instrumented build of the shim's tests (a few GB), then every test once.
- **`coverage-sm`:** builds against the prebuilt SpiderMonkey archive of `mozjs_sys`. 13 of the test
  files use the shim's own API (`mozjs::quickjs`, raw QuickJS) and do not compile there.
- **`coverage-servo`:** a full `medium` Servo build.
- **`coverage-wpt`:** the WPT run plus one merge of all its profiles.
- **`coverage-spidermonkey` and `coverage-servo-sm`:** SpiderMonkey from source, once each, with all
  C and C++ in the build instrumented. With atomic counters, WPT on SpiderMonkey runs about as fast as
  on the shim.

## Not covered yet

- The engine-contract cases, once they land in this repository: their agreement comes from comparing
  observations, not from pass/fail.
- gcov for `quickjs.c`. The unit tests plus test262 cover 99.6% of the `quickjs.c` lines WPT executes,
  so this matters less.
- Call shapes, i.e. argument kinds and object classes per API call.
