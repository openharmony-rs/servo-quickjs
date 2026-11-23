#!/bin/sh
# RUSTC_WRAPPER for `just coverage-servo` (docs/coverage.md). It instruments the shim, every crate
# that links it, and servoshell. The linking crates matter because the shim's inlined and generic
# functions are compiled into them, so instrumenting only the shim misses that code. servoshell
# matters because the final binary must link the profiling runtime. Cargo does not see these flags,
# so never build into the same target dir without this wrapper.

rustc="$1"
shift
instrument=
prev=
for arg in "$@"; do
    case "$prev:$arg" in
        --crate-name:mozjs | --crate-name:servo_quickjs | --crate-name:servoshell) instrument=1 ;;
        --extern:js=* | --extern:mozjs=*) instrument=1 ;;
    esac
    prev="$arg"
done
if [ -n "$instrument" ]; then
    # `-Zcoverage-options=condition` records both outcomes of each condition; it is unstable.
    RUSTC_BOOTSTRAP=1 exec "$rustc" "$@" -C instrument-coverage -Zcoverage-options=condition \
        -C llvm-args=-runtime-counter-relocation
fi
exec "$rustc" "$@"
