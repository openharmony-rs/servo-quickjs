#!/usr/bin/env bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Run the engine contract suite against SpiderMonkey (the reference) or QuickJS, then compare.
#
# Naming:
#   Commands: `<verb>-<object>` -- the verb says what happens, the object names the input.
#   Checked-in files: `<object>-<role>` -- the object says what it describes, the role says how
#   it is used (snapshot, usage, allowlist, records, results, cases).
#   Run outputs: `<producer>-<role>` -- the producer is an engine or scan, the role is `results`
#   or `changes`.
#
# Usage:
#   tools/run.sh run-spidermonkey-contract [--case ID]... [--cases ID[,ID...]]... [cargo args...]
#                               -> spidermonkey-contract-results.jsonl
#   tools/run.sh run-quickjs-contract      [--case ID]... [--cases ID[,ID...]]... [cargo args...]
#                               -> quickjs-contract-results.jsonl
#   tools/run.sh compare-engine-results
#                               compare both results against quickjs-difference-allowlist.toml
#   tools/run.sh run-quickjs-against-golden
#                               run only the QuickJS leg and compare it with the checked-in
#                               SpiderMonkey golden records (no SpiderMonkey build in PR CI)
#   tools/run.sh scan-mozjs-surface [--baseline REV:PATH|FILE] [--output FILE] [--format toml|tsv]
#                               one public scan entry: writes mozjs-api-snapshot.toml,
#                               mozjs-api-changes.{toml,tsv}, and servo-mozjs-api-usage.toml
#   tools/run.sh prepare-mozjs-upgrade [--version X.Y.Z] [--skip-cargo-update]
#                               scan, update both pins and Cargo.lock, then list the cases to edit
#   tools/run.sh check-contract-case-coverage
#                               check the API inventory and Servo usage surface
#   tools/run.sh check-quickjs-shim-fixes
#                               verify removed allowlist entries have fix records
#   tools/run.sh run-tool-tests
#                               run the manifest tool tests under tools/tests
#   tools/run.sh check-rustfmt
#                               check the cases with Servo's tidy rustfmt configuration
#   tools/run.sh run-contract-gate
#                               run tool tests, both engines, compare, check fixes and coverage
#
# Internal commands (kept for troubleshooting and compatibility):
#   scan-mozjs-api, scan-servo-mozjs-api-usage, generate-mozjs-api-snapshot,
#   check-mozjs-api-snapshot, report-mozjs-api-changes
#
#
# Environment:
#   SERVO_DIR       Servo checkout the coverage scope is scanned from, or a clone URL for one
#                   (default: a sibling servo-ohos checkout; there is deliberately no fallback
#                   inside servo-quickjs, so a missing checkout fails instead of scanning the
#                   shim). URLs are fetched into
#                   SERVO_CACHE at SERVO_REF (default: the remote's default branch); the checkout
#                   is read-only input.
#                   Required by scan-*, check-*-usage and prepare-mozjs-upgrade; when it is
#                   present, the version checks also compare Servo's QuickJS pin.
#   QJS_DIR         servo-quickjs checkout (default: this suite's parent directory)
#   OUT_DIR         output directory (default: engine-contract/target/contract)
#   CONTRACT_CASES  comma/space separated case ids; normally set by --case/--cases
#   CONTRACT_CASES_OUT  runtime case inventory written by the test harness
#   CONTRACT_ISOLATE_CASES  run each case in its own process (default 1); 0 runs them all in one
#                       process, where a crash hides every case after it
#   CONTRACT_SM_GOLDEN  checked-in SpiderMonkey results to compare against instead of running the
#                       reference leg; `run-quickjs-against-golden` defaults it to this suite's
#                       `spidermonkey-contract-results.jsonl`
#   CONTRACT_SM_GOLDEN_CASES  the golden records' runtime case inventory
#   SERVO_CACHE     where a URL in SERVO_DIR is cloned (default: OUT_DIR/servo)
#   SERVO_REF       branch, tag or commit to check out for a URL in SERVO_DIR
#                   (default: the remote's default branch)
#   FIX_CHECK_BASELINE  baseline quickjs-difference-allowlist.toml for fix-record checks
#                       (default: HEAD)

set -o errexit
set -o nounset
set -o pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The suite is its own Cargo workspace, so the servo-quickjs `mozjs` member can never be selected
# implicitly: every engine run names the engine it builds. `contract_dir` is where the plain
# `cargo` commands run; `qjs_dir` only feeds the scan tools and the `[patch.crates-io]` paths.
contract_dir="$(cd "${script_dir}/.." && pwd)"
qjs_dir="${QJS_DIR:-$(cd "${contract_dir}/.." && pwd)}"
# A URL in `SERVO_DIR` is input, not a second checkout to edit: clone it once into the cache and
# scan that copy. `git -C <dir>` ignores a `file://` or `https://` SERVO_DIR that this did not
# materialize, so resolve it before any scan command.
is_servo_url() {
    case "$1" in
        http://*|https://*|ssh://*|git://*|file://*|git@*|*://*) return 0 ;;
        *) return 1 ;;
    esac
}

servo_url() {
    printf '%s\n' "$1"
}

# Resolve a caller-supplied relative directory against the invoking directory. The scan commands
# run from the suite and from the Servo checkout, so a path that was relative to the caller would
# stop resolving after the first `cd`.
absolute_path() {
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s\n' "$(pwd)/$1" ;;
    esac
}

materialize_servo_url() {
    local url="$1"
    local cache="${SERVO_CACHE:-${out_dir}/servo}"
    local ref="${SERVO_REF:-}"
    mkdir -p "$(dirname "${cache}")"
    if [[ ! -d "${cache}/.git" ]]; then
        echo "cloning Servo from $(servo_url "${url}") into ${cache}" >&2
        # The scope scan reads only these paths. `ports` holds the OHOS-specific mozjs call
        # sites. `--no-checkout` avoids copying a local `file://` source's worktree, which may
        # contain build output (`target-sm` can run to hundreds of gigabytes). A network clone
        # also skips blobs until sparse-checkout asks for them.
        local -a clone_args=(--quiet --no-checkout)
        if [[ "${url}" != file://* ]]; then
            clone_args+=(--filter=blob:none)
        fi
        git clone "${clone_args[@]}" "${url}" "${cache}" || {
            echo "could not clone Servo from ${url}; pass a local SERVO_DIR or set SERVO_CACHE to a writable directory" >&2
            return 1
        }
        git -C "${cache}" sparse-checkout set --no-cone \
            Cargo.toml components ports support/quickjs/cargo-config.toml || return 1
    fi
    local remote
    remote=$(git -C "${cache}" remote get-url origin 2>/dev/null || true)
    if [[ -n "${remote}" && "${remote}" != "${url}" ]]; then
        echo "SERVO_DIR is ${url}, but ${cache} is a clone of ${remote}; set SERVO_CACHE to a new directory" >&2
        return 1
    fi
    if [[ -n "${ref}" ]]; then
        git -C "${cache}" fetch --quiet --depth 1 origin "${ref}" || {
            echo "could not fetch Servo ref ${ref} from ${url}" >&2
            return 1
        }
        git -C "${cache}" checkout --quiet --detach FETCH_HEAD || return 1
    else
        git -C "${cache}" checkout --quiet || {
            echo "${cache} has no checked-out Servo tree; set SERVO_REF or clone by hand" >&2
            return 1
        }
    fi
    echo "${cache}"
}

# `SERVO_DIR` wins. Otherwise try the layouts a developer is likely to have: the Servo checkout
# beside servo-quickjs, and the one servo-quickjs itself sits in. A suite run without one stays
# self-contained: version checks compare the shim with the suite's own mozjs dependency.
looks_like_servo() {
    [[ -f "$1/Cargo.toml" && -d "$1/components" ]]
}
servo_source=""
if [[ -n "${SERVO_DIR:-}" ]]; then
    if is_servo_url "${SERVO_DIR}"; then
        servo_source="${SERVO_DIR}"
        servo_dir=""
    else
        servo_dir="${SERVO_DIR}"
    fi
else
    servo_dir=""
    # Only the sibling checkout counts: a candidate under this repository would be servo-quickjs
    # itself, which is not a Servo checkout.
    for candidate in "${contract_dir}/../../servo-ohos"; do
        if looks_like_servo "${candidate}"; then
            servo_dir="$(cd "${candidate}" && pwd)"
            break
        fi
    done
    [[ -n "${servo_dir}" ]] || servo_dir="${contract_dir}/../../servo-ohos"
fi
out_dir="${OUT_DIR:-${contract_dir}/target/contract}"

if [[ -n "${servo_source}" ]]; then
    out_dir=$(absolute_path "${out_dir}")
    servo_dir=$(materialize_servo_url "${servo_source}") || exit 1
fi
servo_dir=$(absolute_path "${servo_dir}")

# `git show REV:PATH` resolves PATH from the repository root, which is the servo-quickjs checkout
# rather than this suite's directory.
# Git reports the prefix itself. Computing it with `os.path.relpath` compared the caller's
# spelling of the path with git's, so a case-insensitive file system (or a symlinked checkout)
# produced a `..`-laden prefix that `git show REV:PATH` rejected.
suite_rel=$(git -C "${contract_dir}" rev-parse --show-prefix 2>/dev/null || true)
suite_rel="${suite_rel%/}"
suite_path() {
    if [[ -n "${suite_rel}" ]]; then
        echo "${suite_rel}/$1"
    else
        echo "$1"
    fi
}
# The same file as an absolute path, for commands that run from another directory.
suite_file() {
    echo "${contract_dir}/$1"
}

lockfile="${contract_dir}/Cargo.lock"
lockfile_backup=""

require_servo_dir() {
    if [[ ! -f "${servo_dir}/Cargo.toml" || ! -d "${servo_dir}/components" ]]; then
        echo "missing Servo checkout at ${servo_dir}; set SERVO_DIR to the servo-ohos checkout" >&2
        return 1
    fi
}

has_servo_dir() {
    [[ -f "${servo_dir}/Cargo.toml" && -d "${servo_dir}/components" ]]
}

# Restore through the lockfile itself rather than renaming the backup into place: `mktemp` creates
# the backup with mode 0600, and `mv` would leave Cargo.lock unreadable to other users for good.
restore_lockfile() {
    if [[ -n "${lockfile_backup}" && -f "${lockfile_backup}" ]]; then
        cat "${lockfile_backup}" > "${lockfile}"
        rm -f "${lockfile_backup}"
        lockfile_backup=""
    fi
}

trap restore_lockfile EXIT

# Cargo rewrites Cargo.lock when a local path patch replaces mozjs. Keep that build side effect
# out of the working tree, but never overwrite a caller's own lockfile change.
protect_lockfile() {
    [[ -f "${lockfile}" ]] || return 0
    # An untracked Cargo.lock is this suite's own (a fresh checkout has not committed it yet);
    # only a modification to a tracked lockfile is somebody else's work in progress.
    if git -C "${contract_dir}" status --porcelain -- Cargo.lock 2>/dev/null \
        | grep -qv '^??'; then
        echo "Cargo.lock has uncommitted changes; commit or restore it before running the QuickJS suite" >&2
        return 1
    fi
    lockfile_backup=$(mktemp "${TMPDIR:-/tmp}/engine-contract-Cargo.lock.XXXXXX")
    cp "${lockfile}" "${lockfile_backup}"
}

# Both engine runs and the comparison must agree on OUT_DIR, and QJS_DIR feeds commands that `cd`.
out_dir=$(absolute_path "${out_dir}")
qjs_dir=$(absolute_path "${qjs_dir}")

parse_contract_args() {
    local -a caller_args=("$@")
    local -a parsed_cases=()
    local -a parsed_cargo=()
    PARSED_CONTRACT_CASES=()
    while [[ ${#caller_args[@]} -gt 0 ]]; do
        case "${caller_args[0]}" in
            --case)
                [[ ${#caller_args[@]} -ge 2 && -n "${caller_args[1]}" ]] || {
                    echo "--case requires a case id" >&2
                    return 2
                }
                parsed_cases+=("${caller_args[1]}")
                caller_args=("${caller_args[@]:2}")
                ;;
            --case=*)
                [[ -n "${caller_args[0]#--case=}" ]] || {
                    echo "--case requires a case id" >&2
                    return 2
                }
                parsed_cases+=("${caller_args[0]#--case=}")
                caller_args=("${caller_args[@]:1}")
                ;;
            --cases)
                [[ ${#caller_args[@]} -ge 2 && -n "${caller_args[1]}" ]] || {
                    echo "--cases requires one or more case ids" >&2
                    return 2
                }
                parsed_cases+=("${caller_args[1]}")
                caller_args=("${caller_args[@]:2}")
                ;;
            --cases=*)
                [[ -n "${caller_args[0]#--cases=}" ]] || {
                    echo "--cases requires one or more case ids" >&2
                    return 2
                }
                parsed_cases+=("${caller_args[0]#--cases=}")
                caller_args=("${caller_args[@]:1}")
                ;;
            --)
                caller_args=("${caller_args[@]:1}")
                parsed_cargo+=("${caller_args[@]}")
                break
                ;;
            *)
                parsed_cargo+=("${caller_args[0]}")
                caller_args=("${caller_args[@]:1}")
                ;;
        esac
    done

    if [[ ${#parsed_cases[@]} -gt 0 ]]; then
        local joined
        joined=$(IFS=,; echo "${parsed_cases[*]}")
        CASE_SELECTION="${joined//[[:space:]]/,}"
        while [[ "${CASE_SELECTION}" == *,,* ]]; do
            CASE_SELECTION="${CASE_SELECTION//,,/,}"
        done
        CASE_SELECTION="${CASE_SELECTION#,}"
        CASE_SELECTION="${CASE_SELECTION%,}"
        [[ -n "${CASE_SELECTION}" ]] || {
            echo "--case/--cases did not name any case" >&2
            return 2
        }
    else
        CASE_SELECTION="${CONTRACT_CASES:-}"
    fi

    PARSED_CARGO_ARGS=("${parsed_cargo[@]}")
}

spidermonkey_results_path() {
    echo "${out_dir}/spidermonkey-contract-results.jsonl"
}

quickjs_results_path() {
    echo "${out_dir}/quickjs-contract-results.jsonl"
}

spidermonkey_case_inventory_path() {
    echo "${out_dir}/spidermonkey-contract-cases.json"
}

quickjs_case_inventory_path() {
    echo "${out_dir}/quickjs-contract-cases.json"
}

# The checked-in SpiderMonkey observations. A PR run builds only QuickJS and compares it with
# these records, so CI does not need a SpiderMonkey build on every push. Regenerate them with
# `run-spidermonkey-contract` whenever a case or the mozjs pin changes.
spidermonkey_golden_path() {
    echo "${CONTRACT_SM_GOLDEN:-$(suite_file spidermonkey-contract-results.jsonl)}"
}

spidermonkey_golden_case_inventory_path() {
    echo "${CONTRACT_SM_GOLDEN_CASES:-$(suite_file spidermonkey-contract-cases.json)}"
}

spidermonkey_comparison_results_path() {
    if [[ -n "${CONTRACT_SM_GOLDEN:-}" ]]; then
        spidermonkey_golden_path
    else
        spidermonkey_results_path
    fi
}

spidermonkey_comparison_case_inventory_path() {
    if [[ -n "${CONTRACT_SM_GOLDEN:-}" ]]; then
        spidermonkey_golden_case_inventory_path
    else
        spidermonkey_case_inventory_path
    fi
}

with_case_selection() {
    local selection="$1"
    shift
    if [[ -n "${selection}" ]]; then
        CONTRACT_CASES="${selection}" "$@"
    else
        env -u CONTRACT_CASES "$@"
    fi
}

run_spidermonkey_contract() {
    local -a cargo_args=()
    parse_contract_args "$@" || return
    cargo_args=("${PARSED_CARGO_ARGS[@]}")
    local selection="${CASE_SELECTION}"
    mkdir -p "${out_dir}"
    cd "${contract_dir}"
    rm -f "$(spidermonkey_results_path)" "$(spidermonkey_case_inventory_path)"
    with_case_selection "${selection}" env \
        CONTRACT_ENGINE=spidermonkey \
        CONTRACT_OUT="$(spidermonkey_results_path)" \
        CONTRACT_CASES_OUT="$(spidermonkey_case_inventory_path)" \
        CONTRACT_ISOLATE_CASES="${CONTRACT_ISOLATE_CASES:-1}" \
        cargo test --locked --offline --release -p engine_contract_tests "${cargo_args[@]}"
}

run_quickjs_contract() {
    local -a cargo_args=()
    parse_contract_args "$@" || return
    cargo_args=("${PARSED_CARGO_ARGS[@]}")
    local selection="${CASE_SELECTION}"
    protect_lockfile || return
    check_engine_versions || {
        restore_lockfile
        return 1
    }
    mkdir -p "${out_dir}"
    cd "${contract_dir}"
    rm -f "$(quickjs_results_path)" "$(quickjs_case_inventory_path)"
    with_case_selection "${selection}" env \
        CONTRACT_ENGINE=quickjs \
        CONTRACT_OUT="$(quickjs_results_path)" \
        CONTRACT_CASES_OUT="$(quickjs_case_inventory_path)" \
        CONTRACT_REFERENCE_OUT="$(spidermonkey_comparison_results_path)" \
        CONTRACT_ISOLATE_CASES="${CONTRACT_ISOLATE_CASES:-1}" \
        cargo test --offline --release -p engine_contract_tests --features js-quickjs \
        --config "patch.crates-io.mozjs.path=\"${qjs_dir}/mozjs\"" \
        --config 'env.SERVO_QUICKJS_DEBUGGER="1"' "${cargo_args[@]}"
    local status=$?
    restore_lockfile
    return "${status}"
}

run_compare_engine_results() {
    local spidermonkey_results quickjs_results
    spidermonkey_results=$(spidermonkey_comparison_results_path)
    quickjs_results=$(quickjs_results_path)
    local spidermonkey_cases quickjs_cases
    spidermonkey_cases=$(spidermonkey_comparison_case_inventory_path)
    quickjs_cases=$(quickjs_case_inventory_path)


    [[ -f "${spidermonkey_results}" && -f "${quickjs_results}" ]] || {
        echo "missing engine contract results: run '$0 run-spidermonkey-contract' and '$0 run-quickjs-contract' first" >&2
        return 1
    }
    [[ -f "${spidermonkey_cases}" && -f "${quickjs_cases}" ]] || {
        echo "missing runtime case inventory files: ${spidermonkey_cases}, ${quickjs_cases}" >&2
        return 1
    }
    echo "using Spidermonkey results: ${spidermonkey_results}"
    echo "using QuickJS results:      ${quickjs_results}"
    echo "using runtime case inventories: ${spidermonkey_cases}, ${quickjs_cases}"
    cd "${contract_dir}"
    python3 "${contract_dir}/tools/compare.py" \
        "${spidermonkey_results}" "${quickjs_results}" \
        "$(suite_file quickjs-difference-allowlist.toml)" \
        --sm-cases "${spidermonkey_cases}" \
        --qjs-cases "${quickjs_cases}"
}

check_quickjs_shim_fixes_after_diff() {
    local inventory
    inventory=$(spidermonkey_case_inventory_path)
    if [[ -f "${inventory}" ]]; then
        check_quickjs_shim_fixes --cases "${inventory}" ${FIX_CHECK_BASELINE:+--baseline "${FIX_CHECK_BASELINE}"}
    else
        check_quickjs_shim_fixes ${FIX_CHECK_BASELINE:+--baseline "${FIX_CHECK_BASELINE}"}
    fi
}

# The manifest tools decide which entries are required and which differences are accepted, so a
# regression there can silently weaken the gate. Run their tests before trusting any engine result.
run_tool_tests() {
    local tests_dir="${contract_dir}/tools/tests"
    [[ -d "${tests_dir}" ]] || return 0
    cd "${contract_dir}"
    python3 -m unittest discover -s "${contract_dir}/tools/tests"
}

check_rustfmt() {
    cd "${contract_dir}"
    cargo fmt --all -- --check \
        --config unstable_features=true \
        --config binop_separator=Back \
        --config imports_granularity=Module \
        --config group_imports=StdExternalCrate
}

# A PR run builds only QuickJS: SpiderMonkey is the reference, and its observations change only
# when a case or the mozjs pin changes. Comparing against the checked-in golden records keeps the
# protection without paying for a SpiderMonkey build on every push.
run_quickjs_against_golden() {
    local golden golden_cases
    golden=$(spidermonkey_golden_path)
    golden_cases=$(spidermonkey_golden_case_inventory_path)
    [[ -f "${golden}" ]] || {
        echo "missing SpiderMonkey golden records at ${golden}; run '$0 run-spidermonkey-contract' and commit its output, or unset CONTRACT_SM_GOLDEN" >&2
        return 1
    }
    [[ -f "${golden_cases}" ]] || {
        echo "missing SpiderMonkey golden case inventory at ${golden_cases}" >&2
        return 1
    }
    parse_contract_args "$@" || return
    if [[ -n "${CASE_SELECTION}" ]]; then
        mkdir -p "${out_dir}"
        local selected_golden="${out_dir}/spidermonkey-golden-selected.jsonl"
        local selected_cases="${out_dir}/spidermonkey-golden-selected-cases.json"
        python3 - "${golden}" "${golden_cases}" "${CASE_SELECTION}" \
            "${selected_golden}" "${selected_cases}" <<'PY'
import json
import sys

results, cases, selection, out_results, out_cases = sys.argv[1:6]
selected = {part for part in selection.replace(",", " ").split() if part}
with open(results, encoding="utf-8") as source, open(
    out_results, "w", encoding="utf-8"
) as destination:
    for line in source:
        line = line.strip()
        if line and json.loads(line).get("id") in selected:
            destination.write(line + "\n")
with open(cases, encoding="utf-8") as source:
    document = json.load(source)
document["cases"] = [case for case in document["cases"] if case in selected]
# The run compares only the selected subset, so the allowlist must not treat the rest as stale.
document["complete"] = False
with open(out_cases, "w", encoding="utf-8") as destination:
    json.dump(document, destination)
    destination.write("\n")
PY
        golden="${selected_golden}"
        golden_cases="${selected_cases}"
    fi
    export CONTRACT_SM_GOLDEN="${golden}"
    export CONTRACT_SM_GOLDEN_CASES="${golden_cases}"
    local quickjs_status=0
    run_quickjs_contract "$@" || quickjs_status=$?
    local compare_status=0
    run_compare_engine_results || compare_status=$?
    if [[ "${compare_status}" -ne 0 ]]; then
        return "${compare_status}"
    fi
    if [[ "${quickjs_status}" -ne 0 ]]; then
        echo "note: run-quickjs-contract exited non-zero; every difference is settled by compare-engine-results, which passed" >&2
    fi
    return 0
}

run_all() {
    parse_contract_args "$@" || return
    local selection="${CASE_SELECTION}"
    local -a step_args=()
    if [[ -n "${selection}" ]]; then
        step_args=(--cases "${selection}")
    fi
    local -a failures=()
    local quickjs_failed=0
    local compare_failed=0
    local step
    for step in run-tool-tests run-spidermonkey-contract run-quickjs-contract compare-engine-results \
        check-quickjs-shim-fixes check-contract-case-coverage; do
        if [[ "${step}" == "run-tool-tests" ]] \
            && [[ ! -d "${contract_dir}/tools/tests" ]]; then
            echo "==> run-tool-tests (skipped: no tools/tests)"
            continue
        fi
        if [[ "${step}" == "check-contract-case-coverage" && -n "${selection}" ]]; then
            echo "==> check-contract-case-coverage (skipped: coverage requires the full case set)"
            continue
        fi
        if [[ "${step}" == "check-quickjs-shim-fixes" \
            && ! -f "$(spidermonkey_case_inventory_path)" ]]; then
            echo "==> check-quickjs-shim-fixes (no runtime inventory; checking records only)"
        fi
        if [[ "${step}" != "check-quickjs-shim-fixes" ]]; then
            echo "==> ${step}"
        fi
        if [[ "${step}" == "check-quickjs-shim-fixes" ]]; then
            if ! check_quickjs_shim_fixes_after_diff; then
                failures+=("${step}")
            fi
            continue
        fi
        local runner
        case "${step}" in
            run-tool-tests) runner=run_tool_tests ;;
            run-spidermonkey-contract) runner=run_spidermonkey_contract ;;
            run-quickjs-contract) runner=run_quickjs_contract ;;
            compare-engine-results) runner=run_compare_engine_results ;;
            check-contract-case-coverage) runner=check_contract_case_coverage ;;
        esac
        local -a current_args=("${step_args[@]}")
        if ! "${runner}" "${current_args[@]}"; then
            case "${step}" in
                run-quickjs-contract)
                    # A QuickJS assertion failure is the expected way to record a difference; only
                    # "compare-engine-results" can decide whether that difference is accepted.
                    quickjs_failed=1
                    ;;
                compare-engine-results)
                    compare_failed=1
                    failures+=("${step}")
                    ;;
                *)
                    failures+=("${step}")
                    ;;
            esac
        fi
    done
    if [[ "${quickjs_failed}" -eq 1 ]]; then
        if [[ "${compare_failed}" -eq 1 ]]; then
            failures+=(run-quickjs-contract)
        else
            echo "note: run-quickjs-contract exited non-zero; every difference is settled by" >&2
            echo "      compare-engine-results, which passed" >&2
        fi
    fi
    if [[ ${#failures[@]} -gt 0 ]]; then
        echo "engine contract steps failed: ${failures[*]}" >&2
        return 1
    fi
}

check_mozjs_api_snapshot() {
    [[ -f "${qjs_dir}/mozjs/upstream-bindgen/jsapi.rs" ]] || {
        echo "missing ${qjs_dir}/mozjs/upstream-bindgen; set QJS_DIR to a servo-quickjs checkout" >&2
        return 1
    }
    cd "${contract_dir}"
    python3 "${contract_dir}/tools/api_inventory.py" verify \
        --inventory "$(suite_file mozjs-api-snapshot.toml)" \
        --bindgen-dir "${qjs_dir}/mozjs/upstream-bindgen" \
        --engine-src "${qjs_dir}/mozjs/src/engine" \
        --stub-bindings "${qjs_dir}/mozjs/stub_bindings.py"
}

generate_mozjs_api_snapshot() {
    [[ -f "${qjs_dir}/mozjs/upstream-bindgen/jsapi.rs" ]] || {
        echo "missing ${qjs_dir}/mozjs/upstream-bindgen; set QJS_DIR to a servo-quickjs checkout" >&2
        return 1
    }
    local mozjs_version
    mozjs_version=$(sed -nE 's/^version = "([^"]+)".*/\1/p' "${qjs_dir}/mozjs/Cargo.toml" | head -n 1)
    cd "${contract_dir}"
    python3 "${contract_dir}/tools/api_inventory.py" generate \
        --bindgen-dir "${qjs_dir}/mozjs/upstream-bindgen" \
        --engine-src "${qjs_dir}/mozjs/src/engine" \
        --stub-bindings "${qjs_dir}/mozjs/stub_bindings.py" \
        --output "$(suite_file mozjs-api-snapshot.toml)" \
        --mozjs-version "${mozjs_version}"
}

check_contract_case_coverage() {
    # "set -e" does not abort inside a function that the caller invokes in a condition, which is
    # how every command here is reached. Chain the checks explicitly so a version drift or a stale
    # snapshot cannot be overwritten by a later, passing coverage check.
    check_engine_versions || return 1
    check_mozjs_api_snapshot || return 1
    check_servo_mozjs_api_usage || return 1
    # Prefer the checked-in records: a gate run without a SpiderMonkey build still has the
    # reference observations, and the committed set is what a PR is held to.
    local results
    results=$(spidermonkey_comparison_results_path)
    if [[ ! -f "${results}" ]]; then
        results=$(spidermonkey_golden_path)
    fi
    [[ -f "${results}" ]] || {
        echo "missing SpiderMonkey contract results; run '$0 run-spidermonkey-contract' first" >&2
        return 1
    }
    cd "${contract_dir}"
    python3 "${contract_dir}/tools/check_api_coverage.py" \
        "${results}" "$(suite_file mozjs-api-snapshot.toml)" \
        "$(suite_file servo-mozjs-api-usage.toml)"
}

# The version numbers are hand-edited in four places. When they drift, Cargo silently stops
# applying `[patch.crates-io]`, and a "QuickJS" run builds SpiderMonkey instead: every case passes
# and the comparison is meaningless. Check the versions before trusting either result file.
check_engine_versions() {
    cd "${contract_dir}"
    local -a version_args=()
    if [[ "${CHECK_PINNED_QJS:-0}" == "1" ]]; then
        version_args=(--require-pinned-qjs)
    fi
    if [[ -f "$(suite_file mozjs-api-snapshot.toml)" ]]; then
        version_args+=(--snapshot "$(suite_file mozjs-api-snapshot.toml)")
    fi
    local -a servo_args=()
    if has_servo_dir; then
        servo_args=(--servo-root "${servo_dir}")
    fi
    python3 "${contract_dir}/tools/api_inventory.py" versions \
        "${servo_args[@]}" --qjs-dir "${qjs_dir}" "${version_args[@]}"
}

locate_mozjs() {
    cd "${contract_dir}"
    # The usage baseline is generated against the servo-quickjs shim: its wrapper bodies expose
    # forwarding edges that the registry mozjs crate's generated "wrap!" declarations do not.
    # Prefer the build's QJS_DIR so a registry package selected by cargo metadata cannot silently
    # drop those edges from the scope manifest.
    if [[ -f "${qjs_dir}/mozjs/Cargo.toml" ]]; then
        mozjs_manifest="${qjs_dir}/mozjs/Cargo.toml"
        mozjs_sys_manifest="${qjs_dir}/mozjs/Cargo.toml"
        return 0
    fi
    local registries
    registries=$(cargo metadata --locked --offline --format-version 1 | python3 -c '
import json, sys
for package in json.load(sys.stdin)["packages"]:
    if package["name"] in ("mozjs", "mozjs_sys"):
        print(package["name"], package["manifest_path"])
')
    mozjs_manifest=$(sed -n 's/^mozjs //p' <<<"${registries}")
    mozjs_sys_manifest=$(sed -n 's/^mozjs_sys //p' <<<"${registries}")
    [[ -n "${mozjs_manifest}" && -n "${mozjs_sys_manifest}" ]] || {
        echo "could not locate the mozjs and mozjs_sys crate sources" >&2
        return 1
    }
}

scan_servo_mozjs_api_usage() {
    require_servo_dir || return 1
    locate_mozjs || return 1
    python3 "${contract_dir}/tools/scope_manifest.py" \
        --servo-dir "${servo_dir}" \
        --inventory "$(suite_file mozjs-api-snapshot.toml)" \
        --usage "$(suite_file servo-mozjs-api-usage.toml)" \
        --mozjs-root "$(dirname "${mozjs_manifest}")/src" \
        --mozjs-sys-root "$(dirname "${mozjs_sys_manifest}")/src"
}

check_servo_mozjs_api_usage() {
    require_servo_dir || {
        echo "the coverage check needs SERVO_DIR (Servo's mozjs usage surface)" >&2
        return 1
    }
    locate_mozjs || return 1
    python3 "${contract_dir}/tools/scope_manifest.py" \
        --servo-dir "${servo_dir}" \
        --inventory "$(suite_file mozjs-api-snapshot.toml)" \
        --usage "$(suite_file servo-mozjs-api-usage.toml)" \
        --mozjs-root "$(dirname "${mozjs_manifest}")/src" \
        --mozjs-sys-root "$(dirname "${mozjs_sys_manifest}")/src" \
        --check
}

report_mozjs_api_changes() {
    local baseline="HEAD:$(suite_path mozjs-api-snapshot.toml)"
    local manifest="$(suite_file mozjs-api-snapshot.toml)"
    local output=""
    local format="toml"
    local invoke_dir="${PWD}"
    local baseline_path temporary_baseline=""
    local status

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --baseline)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--baseline requires REV:PATH or a file" >&2
                    return 2
                }
                baseline="$2"
                shift 2
                ;;
            --baseline=*)
                [[ -n "${1#--baseline=}" ]] || {
                    echo "--baseline requires REV:PATH or a file" >&2
                    return 2
                }
                baseline="${1#--baseline=}"
                shift
                ;;
            --inventory)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--inventory requires a file" >&2
                    return 2
                }
                manifest="$2"
                shift 2
                ;;
            --inventory=*)
                [[ -n "${1#--inventory=}" ]] || {
                    echo "--inventory requires a file" >&2
                    return 2
                }
                manifest="${1#--inventory=}"
                shift
                ;;
            --output)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--output requires a file" >&2
                    return 2
                }
                output="$2"
                shift 2
                ;;
            --output=*)
                [[ -n "${1#--output=}" ]] || {
                    echo "--output requires a file" >&2
                    return 2
                }
                output="${1#--output=}"
                shift
                ;;
            --format)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--format requires toml or tsv" >&2
                    return 2
                }
                format="$2"
                shift 2
                ;;
            --format=*)
                [[ -n "${1#--format=}" ]] || {
                    echo "--format requires toml or tsv" >&2
                    return 2
                }
                format="${1#--format=}"
                shift
                ;;
            *)
                echo "report-mozjs-api-changes: unknown argument: $1" >&2
                return 2
                ;;
        esac
    done

    case "${format}" in
        toml|tsv) ;;
        *)
            echo "--format must be toml or tsv" >&2
            return 2
            ;;
    esac

    if [[ -z "${output}" ]]; then
        output="${out_dir}/mozjs-api-changes.${format}"
    fi

    cd "${contract_dir}"
    if [[ "${baseline}" == *:* ]]; then
        temporary_baseline=$(mktemp "${TMPDIR:-/tmp}/engine-contract-baseline.XXXXXX")
        if git show "${baseline}" >"${temporary_baseline}" 2>/dev/null; then
            :
        elif [[ "${baseline}" == "HEAD:$(suite_path mozjs-api-snapshot.toml)" ]] \
            && git show "HEAD:$(suite_path mozjs-api-inventory.toml)" >"${temporary_baseline}" 2>/dev/null; then
            echo "using pre-rename baseline HEAD:$(suite_path mozjs-api-inventory.toml)" >&2
        elif [[ "${baseline}" == "HEAD:$(suite_path mozjs-api-snapshot.toml)" ]] \
            && git show "HEAD:$(suite_path api-manifest.toml)" >"${temporary_baseline}" 2>/dev/null; then
            echo "using pre-rename baseline HEAD:$(suite_path api-manifest.toml)" >&2
        else
            echo "could not read baseline inventory '${baseline}'" >&2
            rm -f "${temporary_baseline}"
            return 1
        fi
        baseline_path="${temporary_baseline}"
    else
        baseline_path="${baseline}"
        [[ "${baseline_path}" == /* ]] || baseline_path="${invoke_dir}/${baseline_path}"
        [[ -f "${baseline_path}" ]] || {
            echo "missing baseline mozjs API inventory: ${baseline_path}" >&2
            return 1
        }
    fi

    [[ "${manifest}" == /* ]] || manifest="${contract_dir}/${manifest}"
    [[ "${output}" == /* ]] || output="${invoke_dir}/${output}"
    mkdir -p "$(dirname "${output}")"
    if python3 "${contract_dir}/tools/api_inventory.py" report \
        --baseline "${baseline_path}" \
        --inventory "${manifest}" \
        --output "${output}" \
        --format "${format}"; then
        status=0
    else
        status=$?
    fi
    rm -f "${temporary_baseline}"
    return "${status}"
}

# The internal mozjs stage of the public surface scan: refresh the full inventory snapshot, verify it
# against the current bindgen/shim, and emit the changes relative to a baseline inventory. The
# underlying generate/check/report commands stay available for debugging individual stages.
scan_mozjs_api() {
    local argument
    for argument in "$@"; do
        case "${argument}" in
            --inventory|--inventory=*)
                echo "scan-mozjs-api always regenerates $(suite_path mozjs-api-snapshot.toml); use report-mozjs-api-changes separately to compare another snapshot" >&2
                return 2
                ;;
        esac
    done
    local -a report_args=("$@")
    generate_mozjs_api_snapshot || return
    check_mozjs_api_snapshot || return
    report_mozjs_api_changes "${report_args[@]}"
}

# The only public scan entry. Scan both sides of the interface surface in dependency order: the
# mozjs snapshot first, then the Servo usage that is classified against it.
scan_mozjs_surface() {
    scan_mozjs_api "$@" || return
    scan_servo_mozjs_api_usage
}

prepare_mozjs_upgrade() {
    cd "${contract_dir}"
    local version=""
    local skip_cargo_update=0
    local -a scan_args=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --version)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--version requires X.Y.Z" >&2
                    return 2
                }
                version="$2"
                shift 2
                ;;
            --version=*)
                version="${1#--version=}"
                shift
                ;;
            --skip-cargo-update)
                skip_cargo_update=1
                shift
                ;;
            --baseline|--output|--format)
                [[ $# -ge 2 ]] || {
                    echo "$1 requires a value" >&2
                    return 2
                }
                scan_args+=("$1" "$2")
                shift 2
                ;;
            --baseline=*|--output=*|--format=*)
                scan_args+=("$1")
                shift
                ;;
            *)
                echo "prepare-mozjs-upgrade: unknown argument: $1" >&2
                return 2
                ;;
        esac
    done
    require_servo_dir || return 1
    if [[ -z "${version}" ]]; then
        version=$(python3 "${contract_dir}/tools/mozjs_upgrade.py" \
            shim-version --qjs-dir "${qjs_dir}") || return
    fi
    python3 "${contract_dir}/tools/mozjs_upgrade.py" check \
        --qjs-dir "${qjs_dir}" --version "${version}" || return
    scan_mozjs_surface "${scan_args[@]}" || return
    python3 "${contract_dir}/tools/mozjs_upgrade.py" apply \
        --servo-root "${servo_dir}" --qjs-dir "${qjs_dir}" --version "${version}" \
        ${skip_cargo_update:+--skip-cargo-update} || return
    check_engine_versions || return
    # The usage scan follows calls through the mozjs wrapper crate, so rerun it against the
    # mozjs sources that the refreshed Cargo.lock selects. The interface snapshot is unchanged.
    if [[ "${skip_cargo_update}" == "0" ]]; then
        scan_servo_mozjs_api_usage || return
    fi
    python3 "${contract_dir}/tools/mozjs_upgrade.py" report \
        --servo-root "${servo_dir}" --qjs-dir "${qjs_dir}" --out-dir "${out_dir}"
}

check_quickjs_shim_fixes() {
    local baseline="HEAD:$(suite_path quickjs-difference-allowlist.toml)"
    local cases=""
    local invoke_dir="${PWD}"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --baseline)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--baseline requires REV:PATH or a file" >&2
                    return 2
                }
                baseline="$2"
                shift 2
                ;;
            --baseline=*)
                [[ -n "${1#--baseline=}" ]] || {
                    echo "--baseline requires REV:PATH or a file" >&2
                    return 2
                }
                baseline="${1#--baseline=}"
                shift
                ;;
            --cases)
                [[ $# -ge 2 && -n "$2" ]] || {
                    echo "--cases requires a file" >&2
                    return 2
                }
                cases="$2"
                shift 2
                ;;
            --cases=*)
                [[ -n "${1#--cases=}" ]] || {
                    echo "--cases requires a file" >&2
                    return 2
                }
                cases="${1#--cases=}"
                shift
                ;;
            *)
                echo "check-quickjs-shim-fixes: unknown argument: $1" >&2
                return 2
                ;;
        esac
    done
    local -a case_args=()
    if [[ -n "${cases}" ]]; then
        [[ "${cases}" == /* ]] || cases="${invoke_dir}/${cases}"
        case_args=(--cases "${cases}")
    elif [[ -f "$(spidermonkey_case_inventory_path)" ]]; then
        case_args=(--cases "$(spidermonkey_case_inventory_path)")
    fi
    cd "${contract_dir}"
    python3 "${contract_dir}/tools/fix_record.py" check \
        --baseline "${baseline}" \
        --allowlist "$(suite_file quickjs-difference-allowlist.toml)" \
        --record "$(suite_file quickjs-shim-fix-records.md)" \
        "${case_args[@]}"
}

command="${1:-run-contract-gate}"
if [[ $# -gt 0 ]]; then
    shift
fi

case "${command}" in
    run-spidermonkey-contract) run_spidermonkey_contract "$@" ;;
    run-quickjs-contract) run_quickjs_contract "$@" ;;
    run-quickjs-against-golden) run_quickjs_against_golden "$@" ;;
    compare-engine-results) run_compare_engine_results ;;
    scan-mozjs-api) scan_mozjs_api "$@" ;;
    scan-mozjs-surface) scan_mozjs_surface "$@" ;;
    prepare-mozjs-upgrade) prepare_mozjs_upgrade "$@" ;;
    generate-mozjs-api-snapshot) generate_mozjs_api_snapshot ;;
    check-mozjs-api-snapshot) check_mozjs_api_snapshot ;;
    report-mozjs-api-changes) report_mozjs_api_changes "$@" ;;
    scan-servo-mozjs-api-usage) scan_servo_mozjs_api_usage ;;
    check-servo-mozjs-api-usage) check_servo_mozjs_api_usage ;;
    check-contract-case-coverage) check_contract_case_coverage ;;
    check-quickjs-shim-fixes) check_quickjs_shim_fixes "$@" ;;
    run-tool-tests) run_tool_tests ;;
    check-rustfmt) check_rustfmt ;;
    run-contract-gate) run_all "$@" ;;
    *)
        echo "usage: $0 {run-spidermonkey-contract|run-quickjs-contract|run-quickjs-against-golden|compare-engine-results|scan-mozjs-surface|prepare-mozjs-upgrade|check-servo-mozjs-api-usage|check-contract-case-coverage|check-quickjs-shim-fixes|run-tool-tests|check-rustfmt|run-contract-gate} [--case ID|--cases ID[,ID...]] [cargo test args...]" >&2
        echo "internal commands: scan-mozjs-api scan-servo-mozjs-api-usage generate-mozjs-api-snapshot check-mozjs-api-snapshot report-mozjs-api-changes" >&2
        exit 2
        ;;
esac
