# A Servo checkout with the `js-quickjs` feature, and the cargo target dirs of Servo and of this repo.
servo := absolute_path(env_var_or_default("SERVO_DIR", justfile_directory() + "/../servo"))
servo_target := absolute_path(env_var_or_default("SERVO_TARGET_DIR", servo + "/target"))
target := absolute_path(env_var_or_default("QJS_TARGET_DIR", justfile_directory() + "/target"))
libclang := env_var_or_default("LIBCLANG_PATH", "/usr/lib/llvm-18/lib")
patch := 'patch.crates-io.mozjs.path="' + justfile_directory() + '/mozjs"'

# Regenerate servo-quickjs bindings from the vendored QuickJS header.
gen-bindings:
    LIBCLANG_PATH={{libclang}} CARGO_TARGET_DIR={{target}} cargo run -p gen-bindings

# The mozjs tests that cannot pass on QuickJS: SpiderMonkey's stack positions, and root tracers (no-ops by design).
by_design := "--skip capture_stack --skip virtual_trace_called --skip custom_auto_rooter_macro"
clang := env_var_or_default("CLANG", "clang-20")
clangxx := replace(clang, "clang", "clang++")
# UBSan reports abort, so they fail a test or are kept as fuzzing crashes.
ubsan := "-fno-sanitize-recover=undefined"

test *args:
    CARGO_TARGET_DIR={{target}} cargo test -p mozjs {{args}} -- {{by_design}}

# Run the tests with the tracing verifier.
test-verify *args:
    QJS_VERIFY_TRACING=1 CARGO_TARGET_DIR={{target}} cargo test -p mozjs {{args}} -- {{by_design}}

# Run the tests with autorelease pool statistics (`engine/pool_stats.rs`), in a separate target dir.
test-pool-stats *args:
    QJS_POOL_STATS=1 CARGO_TARGET_DIR={{target}}/pool-stats cargo test -p mozjs {{args}} -- {{by_design}}

# Run the tests under ASan, LSan and UBSan, QuickJS's C code included; `tools/lsan.supp` lists the leaks that are by design.
test-asan *args:
    CC={{clang}} CFLAGS="-fsanitize=address,undefined {{ubsan}}" RUSTFLAGS="-Zsanitizer=address -C force-frame-pointers=yes" LSAN_OPTIONS=suppressions={{justfile_directory()}}/tools/lsan.supp CARGO_TARGET_DIR={{target}}/asan cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu -p mozjs {{args}} -- {{by_design}}

# Fail if the QuickJS bindings are out of date.
check-bindings: gen-bindings
    git diff --exit-code servo-quickjs/src/bindings.rs

# Run the clippy lints the source denies, e.g. `mut_from_ref` in `src/engine` (HACKING.md, "Re-entrancy").
lint:
    CARGO_TARGET_DIR={{target}} cargo clippy -p mozjs -- -A clippy::all

# Build Servo's servoshell (in `SERVO_DIR`) against the shim.
servo cmd="build":
    cd {{servo}} && LIBCLANG_PATH={{libclang}} CARGO_TARGET_DIR={{servo_target}} cargo {{cmd}} -p servoshell --config '{{patch}}'

# Run the shim-backed servoshell headless on a page (`just servo "build --features js-quickjs"` first).
run page *args:
    {{servo_target}}/debug/servoshell --headless --exit {{args}} {{page}}

# Run WPT tests (e.g. `dom/nodes`) with the shim-backed servoshell and the tracing verifier.
wpt log *tests:
    cd {{servo}} && QJS_VERIFY_TRACING=1 ./mach test-wpt --bin {{servo_target}}/debug/servoshell --log-raw {{log}} {{tests}}

fuzz_target := absolute_path(env_var_or_default("FUZZ_TARGET_DIR", target + "/fuzz"))

# Fuzz a target (clone_read, eval, strings) under ASan and UBSan, QuickJS's C code included; extra args go to libFuzzer.
# The host target is passed explicitly: a prebuilt cargo-fuzz (a musl binary) defaults to its own target.
fuzz target *args:
    CC={{clang}} CXX={{clangxx}} CFLAGS="-fsanitize=address,undefined,fuzzer-no-link {{ubsan}}" LIBCLANG_PATH={{libclang}} CARGO_TARGET_DIR={{fuzz_target}} cargo +nightly fuzz run --target "$(rustc +nightly -vV | sed -n 's/^host: //p')" {{target}} fuzz/corpus/{{target}} -- {{args}}

# Run each fuzz target for `seconds` from its corpus.
fuzz-smoke seconds="60":
    for t in clone_read eval strings; do just fuzz $t -max_total_time={{seconds}} -timeout=10 -rss_limit_mb=4096 || exit 1; done
