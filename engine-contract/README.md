# engine contract tests

Shared contract tests for the `mozjs` API. SpiderMonkey is the reference implementation: every
case asserts SpiderMonkey behavior, and the same source also runs against the QuickJS shim to
expose divergences. The engine is selected at build time by a `[patch.crates-io]`, so the test
source never names an engine and never uses `mozjs::quickjs` or `qjs::`.

The suite lives in servo-quickjs, next to the shim, so a shim change and the cases that catch its
regressions land in one repository. It is a separate Cargo workspace on purpose: a path dependency
from inside servo-quickjs' workspace could resolve to the QuickJS shim by accident, and the
SpiderMonkey reference run would silently compare QuickJS against itself.

## Prerequisites

The gate scripts under `tools/` need Bash and Python 3.11 or newer: every tool imports
`tomllib`, which only exists from 3.11 on.

`check-rustfmt` also needs the rustfmt component for the active toolchain:
`rustup component add rustfmt`.

`run-spidermonkey-contract` builds the crates.io `mozjs` with its `jit` feature, which is what
makes `mozjs_sys` link the prebuilt SpiderMonkey archive from the `servo/mozjs` release for the
matching `mozjs_sys` version. Only when that archive is unavailable does `mozjs_sys` fall back to
compiling SpiderMonkey from source, which needs Clang 19.0 or newer as `CC`/`CXX`
(`toolchain.configure` rejects anything older) and takes a long time. Pointing `MOZJS_ARCHIVE` at a
local archive of the same version avoids the download; later builds reuse the Cargo target
directory. The QuickJS run needs neither and needs a servo-quickjs checkout, which defaults to this
suite's parent directory.

The coverage scope belongs to Servo. Commands that scan it (`scan-mozjs-surface`,
`check-servo-mozjs-api-usage`, `prepare-mozjs-upgrade`) read a Servo checkout from `SERVO_DIR`,
which defaults to a `servo-ohos` checkout beside servo-quickjs (`../../servo-ohos` from this
directory). A `servo-ohos` directory inside servo-quickjs is never used. `SERVO_DIR` may also be a
clone URL (`https://`, `ssh://`, `git@`, or `file://`):
the suite sparse-clones it into `SERVO_CACHE` (default `target/contract/servo`) at `SERVO_REF`
(default: the remote's default branch), and only reads it. This lets a run use an unmodified
servo-ohos baseline without changing that repository. The engine runs and
`compare-engine-results` do not need one: without a Servo checkout, `check_engine_versions`
compares the shim with this suite's own mozjs dependency and only warns that Servo's pin was not
checked. `run-contract-gate` still needs `SERVO_DIR`, because its coverage check requires Servo's
usage surface.

```bash
SERVO_DIR=https://github.com/openharmony-rs/servo-ohos.git \
SERVO_REF=ohos-main \
engine-contract/tools/run.sh run-contract-gate
```

## Naming

There are three names to read in a command or path:

- **Commands** use `<verb>-<object>`: the verb is what happens, the object is the input. Examples:
  `run-spidermonkey-contract` runs the contract cases on SpiderMonkey;
  `scan-mozjs-surface` scans both the mozjs API and Servo usage surfaces.
- **Checked-in files** use `<object>-<role>`: the object is what it describes, the role is how it is
  used. Examples: `mozjs-api-snapshot.toml` is the mozjs API snapshot;
  `quickjs-difference-allowlist.toml` is the allowlist of accepted QuickJS differences.
- **Run outputs** use `<producer>-<role>`: `spidermonkey-contract-results.jsonl` is the
  SpiderMonkey contract result; `quickjs-contract-cases.json` is the QuickJS case set that
  produced it.

The main commands and their files:

| Command | Reads | Writes |
| --- | --- | --- |
| `scan-mozjs-surface` | bindgen + shim, then Servo source | `mozjs-api-snapshot.toml`, `mozjs-api-changes.toml`, `servo-mozjs-api-usage.toml` |
| `prepare-mozjs-upgrade` | the shim checkout, both surfaces | the three files above, both mozjs pins, and a case list |
| `check-servo-mozjs-api-usage` | Servo source + `servo-mozjs-api-usage.toml` | nothing (check only) |
| `run-spidermonkey-contract` | contract cases | `spidermonkey-contract-results.jsonl` |
| `run-quickjs-contract` | contract cases | `quickjs-contract-results.jsonl` |
| `run-quickjs-against-golden` | contract cases + checked-in SpiderMonkey results | QuickJS results, comparison |
| `compare-engine-results` | both result files + `quickjs-difference-allowlist.toml` | terminal report |
| `check-quickjs-shim-fixes` | allowlist + `quickjs-shim-fix-records.md` | nothing (check only) |
| `check-contract-case-coverage` | inventory + usage + SpiderMonkey results | nothing (check only) |
| `run-contract-gate` | all of the above | results, comparison, coverage |

Internal commands, only for troubleshooting one stage:

| Command | Purpose |
| --- | --- |
| `scan-mozjs-api` | only refresh the mozjs snapshot and change report |
| `scan-servo-mozjs-api-usage` | only refresh the Servo usage file |
| `generate-mozjs-api-snapshot` | only regenerate `mozjs-api-snapshot.toml` |
| `check-mozjs-api-snapshot` | only verify the snapshot is fresh |
| `report-mozjs-api-changes` | only compare two snapshots and write the changes |

## Coverage scope

Coverage is measured against `servo-mozjs-api-usage.toml`: the mozjs entries Servo production
code actually calls directly or reaches through the mozjs wrappers. `check-contract-case-coverage`
checks that every scoped entry has a case and prints the exact count, so this document does not
restate a number that drifts with every scan.

Every case records the `apis` it calls. The coverage gate compares those names with
`mozjs-api-snapshot.toml` for status and with `servo-mozjs-api-usage.toml` for the required set.
A case is evidence for an entry only because it declares that entry and drives it. There is no
WPT-replay ledger and no tier index in this repository: Servo's WPT tree is a source of scenarios
for new cases, not a checked-in expectation, because every case asserts the native behaviour
directly.

Every scoped entry has a contract case: `implemented` entries
are checked as normal coverage, and `empty`/`todo` entries are required cases whose shim behavior
is reported separately. Such a case still asserts SpiderMonkey behavior; on QuickJS the silent
no-op or `qjs_todo!` panic is recorded and must be settled by an entry in
`quickjs-difference-allowlist.toml`, so a missing capability can never silently drop its case.

Cases may exercise entries outside the scope; those entries are ignored for coverage, but every
case must exercise at least one scoped entry. `implemented` entries outside the scope do not need
a case.

Scoped `empty` and `todo` entries must also have a case. They are counted separately, and a case
may declare them even though the manifest does not classify them as `implemented`; outside the
scope the same entries are rejected. This is what keeps an unimplemented
entry visible: dropping the case would fail `coverage`, while keeping it records today's behavior
for the diff.

`servo-mozjs-api-usage.toml` is generated, not hand maintained. Entries reached only through
mozjs wrapper methods, generic `TypedArray` helpers, generated bindings or callback registrations
are not visible to the conservative call scan; each one is listed in
`tools/servo-mozjs-api-usage-reviewed.toml` with its review evidence, and the generator merges
that list into the scope. The call scan keys the shim's functions by name and does not model
receiver types, so a method name defined on more than one shim type is only followed for
`self.method()`, for the trait methods Rust dispatches implicitly (`deref`, `drop`, `trace`,
`post_barrier`) and for the rooted/stack-vector accessors (`len`, `at`). That is what keeps
`OpenOptions::append(true)` from claiming `AppendToRootedObjectVector`, while `IdVector::deref`
still reaches `SliceRootedIdVector`.

It is the usage standard: a mozjs upgrade that adds entries does not fail coverage unless Servo
starts using them. If Servo gains a new call site, `check-servo-mozjs-api-usage` fails until the
usage file and any newly required case are updated.

`check-contract-case-coverage` first checks the version places: the exact `mozjs` pin in Servo's
`Cargo.toml`, the version in `mozjs/Cargo.toml`, the `rev` in Servo's
`support/quickjs/cargo-config.toml`, and `mozjs_version` in the snapshot. When the shim version
drifts, Cargo stops applying `[patch.crates-io]` without an error and the "QuickJS" run silently
builds SpiderMonkey, so a version mismatch is a hard failure. A checkout that is merely not the
pinned commit is a warning, because testing a local shim is normal; `CHECK_PINNED_QJS=1` makes it
fatal. The `js-quickjs` test build also fails to compile when the patch did not apply.

The snapshot deliberately does not record the shim commit. Regenerating it moves `HEAD`, so a
committed revision would be stale the moment its own snapshot is committed and every gate run
would fail. The snapshot identifies its input by content instead: `bindgen_sha256` covers the
bindgen output, and the `verify` step compares the recorded entries with the current shim's engine
tree. The Servo pin is still checked against `support/quickjs/cargo-config.toml`.

The check reads `QJS_DIR/mozjs/Cargo.toml`, so a sparse checkout must include it along with
`mozjs/upstream-bindgen`, the whole `mozjs/src` tree and `mozjs/stub_bindings.py`. Taking only
`mozjs/src/engine` is not enough: the usage scan follows calls through the wrapper bodies in
`mozjs/src/{rust,conversions,...}.rs`, so a checkout that stops at `engine` reports every
indirect entry as removed.

Running the quickjs leg also needs the `servo-quickjs` directory (the crate that builds vendored
QuickJS).

It then checks that `mozjs-api-snapshot.toml` still matches the bindgen and
QuickJS shim, then checks that `servo-mozjs-api-usage.toml` still matches Servo's usage, and finally
checks cases against the scope. A bindgen addition, removal, status change, or signature change
requires regenerating the inventory; it does not require a case by itself. The signature records the parameter and return
types, so changing a type, adding or removing a parameter, or reordering same-typed parameters
fails; renaming a parameter does not. Line-number and file movements in the shim do not fail this
check.

The public scan command refreshes both sides in dependency order. Use it after a mozjs upgrade or
after adding or removing a mozjs call in Servo:

```bash
engine-contract/tools/run.sh scan-mozjs-surface
engine-contract/tools/run.sh check-servo-mozjs-api-usage
```

`mozjs-api-snapshot.toml` is generated, not edited by hand. When a mozjs upgrade or a shim change alters
the entry points, regenerate it, then regenerate the scope and add cases for every newly used
scoped entry, including `empty`/`todo` entries that are not implemented yet:

```bash
engine-contract/tools/run.sh scan-mozjs-surface
engine-contract/tools/run.sh run-spidermonkey-contract
engine-contract/tools/run.sh check-contract-case-coverage
```

`scan-mozjs-surface` is the only public scan entry: it runs the mozjs scan and then the Servo usage
scan, so the usage file is always classified against the snapshot that was just generated. It
regenerates `mozjs-api-snapshot.toml`, verifies it against the current bindgen and shim, and writes the changes
relative to `HEAD` to `target/contract/mozjs-api-changes.toml`. It reads the bindgen files, engine
sources, mozjs version and shim commit from `QJS_DIR`, so `QJS_DIR` must point at the same checkout
that the build uses. Review the manifest diff, then update the scope and cases; do not edit the
TOML files by hand. Use `--baseline REV:PATH`, `--output FILE`, and `--format tsv` to select an
older baseline, a different output path, or a spreadsheet-friendly table.

`generate-mozjs-api-snapshot`, `check-mozjs-api-snapshot`, and `report-mozjs-api-changes` remain available
for debugging the individual stages.

`tools/scan_scope.py` is the scanner behind `scope_manifest.py`; it only exposes the `scan`
subcommand. `scope_manifest.py` owns `servo-mozjs-api-usage.toml`, because it merges the reviewed
usage list and classifies `direct`/`indirect`. Do not generate the scope file from `scan_scope.py`.

## Upgrading mozjs

A developer upgrading the `mozjs` version needs four steps. The first one is a single command that
does the mechanical work; only the affected cases are left to edit.

1. **Confirm the shim supports the new version.** The `servo-quickjs` checkout in `QJS_DIR` must
   have a commit whose `mozjs/Cargo.toml` says exactly the new `X.Y.Z`. Without it the
   `[patch.crates-io]` cannot apply and the "QuickJS" run silently builds SpiderMonkey.

2. **Prepare the upgrade.**

   ```bash
   export QJS_DIR=/path/to/servo-quickjs
   engine-contract/tools/run.sh prepare-mozjs-upgrade
   ```

   It validates the shim checkout, runs `scan-mozjs-surface`, updates the `mozjs` pins in the Servo
   checkout (`$SERVO_DIR/Cargo.toml` and `$SERVO_DIR/support/quickjs/cargo-config.toml`), rescans the
   usage surface, and prints `engine versions ok` together with the cases to `ADD` or `UPDATE`. The
   pins live in Servo, not in this repository. Pass `--baseline REV:PATH` to compare against another
   snapshot.

   It updates Servo's `Cargo.lock` with `cargo update -p mozjs` so the new version is actually
   selected. Pass `--skip-cargo-update` when you want to update the lockfile yourself; that flag is
   the only mode that leaves `Cargo.lock` alone. If `cargo update` fails, the script restores both
   pins and `Cargo.lock`, so a failed upgrade leaves the checkout unchanged.

3. **Edit only the listed cases.** Open the file printed next to each `UPDATE` or `ADD` line, keep
   the case id, declare the entry in `apis`, and assert what the entry actually does. If the output
   says `no changes; no cases are affected`, skip this step. `empty` and `todo` entries still need a
   case; the rules above apply.

4. **Run the gate, format, and commit.**

   ```bash
   engine-contract/tools/run.sh run-contract-gate
   engine-contract/tools/run.sh check-rustfmt
   git status --short
   ```

   `run-contract-gate` starts with `run-tool-tests`, which runs the manifest tool tests in
   `tools/tests`. Those tests decide which entries are required and which differences are
   accepted, so a regression there can weaken the gate without failing a contract case.

   `check-rustfmt` runs rustfmt with Servo's tidy options (`binop_separator=Back`,
   `imports_granularity=Module`, `group_imports=StdExternalCrate`), so a formatting-only change is
   what Servo's `test-tidy` also expects. Do not run plain `cargo fmt --check` as the gate; the
   stable rustfmt defaults differ from the checked-in format.

   The pins it updates live in Servo, so the upgrade produces commits in two checkouts. Commit the
   Servo pin and its lockfile in the Servo checkout, and the regenerated snapshot/usage files plus
   the touched cases here (add `engine-contract/Cargo.lock` only if a version bump changed it). If
   `check-rustfmt` also reformatted unrelated files, commit that separately:

   ```bash
   # in the Servo checkout
   git -C "$SERVO_DIR" add Cargo.toml support/quickjs/cargo-config.toml Cargo.lock
   git -C "$SERVO_DIR" commit -m "deps: Bump mozjs to <X.Y.Z>"

   # in servo-quickjs
   git add engine-contract/mozjs-api-snapshot.toml \
     engine-contract/servo-mozjs-api-usage.toml \
     engine-contract/<touched>_test.rs
   git commit -m "deps: Bump mozjs to <X.Y.Z>"

   git add engine-contract/*.rs
   git commit -m "chore: run check-rustfmt on the engine contract tests"
   ```

   Add the allowlist or fix records only when the gate required them.

## Tool tests

Run the manifest tool tests on their own with either command:

```bash
engine-contract/tools/run.sh run-tool-tests
python3 -m unittest discover -s engine-contract/tools/tests
```

`run-contract-gate` runs them first, so a broken manifest tool fails the gate before either engine
run produces a result.

## Engine policy

- This suite is Servo's source of engine contract cases, and every case must pass on
  SpiderMonkey. SpiderMonkey is the reference implementation.
- The QuickJS gate is `run.sh run-quickjs-contract` followed by `run.sh compare-engine-results`: an unlisted difference fails,
  while a difference recorded in `quickjs-difference-allowlist.toml` with status `accepted` or `wontfix`
  is reported but allowed. `pending` and `bug` entries still fail.
- Do not weaken an assertion to make a shim green. Accepted QuickJS behavior changes are recorded
  in `quickjs-difference-allowlist.toml` after review, per field, and the entry is removed once fixed.

When a difference is fixed, add a record to `quickjs-shim-fix-records.md` before deleting its
`quickjs-difference-allowlist.toml` entry. The record names the case, the shim commit, the change, and the
regression command/result, so the A4 fix-to-regression step leaves an auditable trace.

## Running

```bash
engine-contract/tools/run.sh run-spidermonkey-contract # SpiderMonkey reference
engine-contract/tools/run.sh run-quickjs-contract      # local servo-quickjs checkout
engine-contract/tools/run.sh compare-engine-results
engine-contract/tools/run.sh scan-mozjs-surface # public: snapshot, changes, and Servo usage
engine-contract/tools/run.sh generate-mozjs-api-snapshot # internal: regenerate only the snapshot
engine-contract/tools/run.sh report-mozjs-api-changes # internal: compare two snapshots
engine-contract/tools/run.sh check-mozjs-api-snapshot # internal: snapshot matches the shim and bindgen
engine-contract/tools/run.sh scan-mozjs-api # internal: snapshot and changes only
engine-contract/tools/run.sh scan-servo-mozjs-api-usage # internal: usage only
engine-contract/tools/run.sh check-servo-mozjs-api-usage # servo-mozjs-api-usage.toml matches Servo usage
engine-contract/tools/run.sh check-contract-case-coverage # inventory, usage, and case coverage
engine-contract/tools/run.sh check-quickjs-shim-fixes # removed differences have fix records
engine-contract/tools/run.sh run-tool-tests # manifest tool tests in tools/tests
engine-contract/tools/run.sh check-rustfmt # Servo's tidy rustfmt configuration
engine-contract/tools/run.sh run-contract-gate # tool tests, both engines, compare, fixes, coverage
```

`scan-mozjs-surface` passes scan options to the mozjs stage; it defaults to
`HEAD:engine-contract/mozjs-api-snapshot.toml` as the
baseline, the working-tree inventory as the current input, and
`$OUT_DIR/mozjs-api-changes.{toml,tsv}` as the output.
All three may be overridden, and `--format tsv` writes a spreadsheet-friendly table:

```bash
engine-contract/tools/run.sh scan-mozjs-surface \
  --baseline origin/main:engine-contract/mozjs-api-snapshot.toml \
  --output target/contract/mozjs-api-changes.tsv --format tsv
```

Run one case or a few cases against either engine:

```bash
engine-contract/tools/run.sh run-spidermonkey-contract --case objects.core
engine-contract/tools/run.sh run-quickjs-contract --case objects.core
engine-contract/tools/run.sh run-spidermonkey-contract --cases objects.core,strings.core
engine-contract/tools/run.sh run-spidermonkey-contract --case objects.core --case strings.core
```

`--case` and `--cases` may be repeated. Values from `--cases` may be separated by commas or
whitespace. The selected ids are passed to the harness as `CONTRACT_CASES`; unselected cases do
not execute. If a selected id does not exist, the run fails and lists every id that produced no
record. Each `run-spidermonkey-contract`/`run-quickjs-contract` run writes its runtime inventory to `$OUT_DIR/spidermonkey-contract-cases.json` or
`$OUT_DIR/quickjs-contract-cases.json` through `CONTRACT_CASES_OUT`; `compare-engine-results` checks each JSONL against its own
inventory and requires the two inventories to agree. A truncated or stale run therefore fails
instead of silently comparing a subset. The inventory is written by the harness when cases
execute, so case ids computed at runtime are included; `case_inventory.py` remains a diagnostic
source scan and is not a gate input. A selected run marks the inventory `"complete": false`, so
`compare-engine-results` does not treat differences for unselected cases as stale.

Set `QJS_DIR` if `servo-quickjs` is not next to this checkout. Set `OUT_DIR` to change where the
JSONL files are written. The script always uses absolute paths for `CONTRACT_OUT`.

When `CONTRACT_OUT` is unset the tests still assert their expectations, but no JSONL is written.
The file is truncated at the start of each `run-spidermonkey-contract`/`run-quickjs-contract` run and appended per case. A selected run
therefore cannot leave an old full-suite JSONL behind.

Cases are isolated from each other at two levels.

At the process level, `run.sh` sets `CONTRACT_ISOLATE_CASES=1` by default. The test entry then
re-executes the binary once per case and merges each child's JSONL record into one file, so a
segfault, abort or `panic = "abort"` in one case cannot hide the cases after it. A child that dies
without recording is written as a failure observation with its exit status, and a child that exits
non-zero makes the parent exit non-zero even when it did record an observation. A child that
recorded an observation and then failed for any reason other than its recorded panic, for example
a crash while the runtime is torn down, gets an `exit` key in its record, so the comparison fails
instead of treating the observation as a pass. Set
`CONTRACT_ISOLATE_CASES=0` to run everything in one process for debugging; a crash then truncates
the run.

At the scenario level, compound cases use `support::Steps`, which catches each sub-scenario's
panic, runs a fresh realm/global for it, and continues with the remaining sub-scenarios. The first
panic becomes `fields.panic` (the existing comparison fingerprint) and all of them are listed in
`fields.step_failures`:

```json
"step_failures": [{"step": "weak_map_objects", "panic": "assertion failed: ..."}]
```

`CONTRACT_STEP` narrows a case to named sub-scenarios, which keeps one API group's observation
separate from the others in a compound case.

A case that fails before it can record (for example a panic outside any `Steps` block) records
`{"fields": {"panic": "<message>"}, "apis": []}` and the run continues with the next case. The
test still fails after every case has run, and the failure summary lists each failed case, so one
engine defect cannot hide the cases that follow it.

Cases for `empty`/`todo` entries record the shim precondition or divergence; the QuickJS diff must
already have the corresponding entry in `quickjs-difference-allowlist.toml`.

CI runs `run-spidermonkey-contract` and fails when it produces a diff against the checked-in golden
records, so a case edited without regenerating them cannot land. It then runs
`run-quickjs-against-golden`, which compares the QuickJS leg with those records. The unit-test
step's JSONL is not reused because that build uses the registry `mozjs`, not the pinned shim. The
`jit` feature makes the reference leg a cheap archive link, so it runs on every push.
Regenerate the records and commit them in the same change whenever a case is added or edited, when
a `mozjs` feature changes engine behaviour, or when the `mozjs` pin moves:

```sh
engine-contract/tools/run.sh run-spidermonkey-contract
cp engine-contract/target/contract/spidermonkey-contract-results.jsonl \
   engine-contract/target/contract/spidermonkey-contract-cases.json \
   engine-contract/
git add engine-contract/spidermonkey-contract-results.jsonl \
        engine-contract/spidermonkey-contract-cases.json
```

`run-spidermonkey-contract` writes under `target/contract/`, so the `cp` is not optional: adding
the committed paths without it stages the previous records and leaves the run's output untracked.

Treat a regeneration as a review artifact. Diff the new records against the previous ones and
explain every changed field in the commit message or the PR: a mozjs upgrade is exactly that
diff, and an unexplained change is either a case regression or a behaviour change that needs a
`quickjs-difference-allowlist.toml` entry. The records carry the `spidermonkey` engine marker, so a
QuickJS run whose `[patch.crates-io]` did not apply cannot pass itself off as the reference.

The full gate (`run-contract-gate`) keeps running both engines for maintainers and release runs.

`run.sh run-contract-gate` runs `run-spidermonkey-contract → run-quickjs-contract → compare-engine-results → check-quickjs-shim-fixes → check-contract-case-coverage` without stopping at the first failure. It reports
every failed step at the end and exits non-zero if any step failed, so a known QuickJS failure
still produces `quickjs-contract-results.jsonl`, the diff, and the coverage result. A non-zero
`run-quickjs-contract` is deliberately not a gate failure on its own: an assertion failure is how a
QuickJS difference is recorded, and `compare-engine-results` decides whether every one is accepted.
The gate only fails on the QuickJS run when the comparison also fails. When `--case`/`--cases` is used,
`run-contract-gate` skips coverage because a single case cannot cover the full scope; the skip is
reported explicitly.

## Comparing

Each case records its normalized value, the pending exception name, and the `apis` it calls. A
case that panics records a `panic` field instead.
`compare.py` checks that each file carries its engine's `engine` marker and then drops it.
`quickjs-difference-allowlist.toml` is the only difference list for
the SpiderMonkey/QuickJS comparison: an entry names one case, what may differ, its
`status`, `severity`, `reason` and `owner`. `fields` lists the record keys that may differ
(`apis`, `exception`) and single observation fields as `fields.<name>`; every other field
recorded by both engines is still compared, and an `exit` key always fails. `accepted` and `wontfix` release an accepted
behaviour difference, while `known-bug` releases a QuickJS shim defect and requires an `issue`;
all three stay in the report. `pending` and `bug` fail the run. `severity` is `P0`-`P3`, and a
`P0`/`P1` entry cannot be released as accepted, wontfix or known-bug. An unlisted or malformed
entry fails the run. An optional
`issue` may point at the tracking issue for a difference that is still present; `pending`,
`bug` and `known-bug` entries must have one, while accepted/wontfix entries may omit it. One issue
covers one defect: a `known-bug` entry that points at a shared umbrella issue hides which defect is
reported; `known-bug-issues.md` maps each entry to its issue. `panic` is an optional
exact fingerprint for a panicking case: when present, the QuickJS `panic` text must match it
character for character, and a QuickJS panic on an entry without one is a mismatch. A panicking
case stops recording and a failed step drops its own observations, so fields that only
SpiderMonkey recorded are accepted only behind a matching `panic`. `steps` lists the
sub-scenarios that fail on QuickJS, needs `panic` (the first failing step's), and must equal the
steps in `fields.step_failures`: a newly failing step fails the comparison. A listed field, step or
`panic` that no longer applies is reported as a stale entry with what to remove from it. A fixed
difference is removed from this file and recorded in `quickjs-shim-fix-records.md`; `check-quickjs-shim-fixes` rejects a removed
entry without a complete record, and rejects a record whose case id is not in the runtime
inventory. An entry for a case that participated in both runs but no longer differs is stale and
fails `diff`; when both runtime inventories are complete, an entry for a case that no longer
exists is stale too. A selected run only reports the first kind for its selected cases.
