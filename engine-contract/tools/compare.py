#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Compare the contract results of the SpiderMonkey and QuickJS runs.

Usage: compare.py SPIDERMONKEY-RESULTS.jsonl QUICKJS-RESULTS.jsonl \
           [quickjs-difference-allowlist.toml] [--cases CASES.json] \
           [--sm-cases SPIDERMONKEY-CASES.json] [--qjs-cases QUICKJS-CASES.json]

Both inputs are JSON Lines produced by `engine_contract_tests`. Cases are matched by `id`; the
`engine` field must say `spidermonkey` in the first file and `quickjs` in the second. That check
is what catches a mis-applied `[patch.crates-io]`, where the "QuickJS" run actually built
SpiderMonkey and would otherwise compare SpiderMonkey with itself and pass. SpiderMonkey is the
reference: a difference listed in
`quickjs-difference-allowlist.toml` with status `accepted` or `wontfix` is reported as a known QuickJS
defect, and status `known-bug` as a tracked shim defect; neither fails the run. A listed
difference with status `pending` or `bug` still fails, as does an unlisted difference. When
``CASES.json`` is supplied, both engine runs must contain exactly those case ids; a truncated or
stale run therefore fails instead of comparing a subset.
An entry releases only what it names: top-level keys (`apis`, `exception`) and single
observation fields (`fields.<name>`); every other field present in both runs is still compared,
and an `exit` key, which marks a case process that failed after recording, always fails.
Fields that QuickJS did not record are accepted only when its panic matches the entry's `panic`
fingerprint, and the steps that failed must be exactly the entry's `steps`.
An entry whose listed case participated in both runs but no longer differs is stale and fails the
run, and so is a listed field, step or panic that no longer applies; the report says what to
remove. When both runtime inventories are marked complete, an entry for a case that no longer
exists is stale as well. Entries are never silent skips.

Use ``--sm-cases`` and ``--qjs-cases`` when the two engines write their own runtime case
inventories. Each engine's JSONL is then checked against its own inventory, and the inventories
must match the selected case set exactly.
"""

import json
import sys
import tomllib


DIFFERENCE_STATUSES = ("pending", "bug", "known-bug", "accepted", "wontfix")
DIFFERENCE_SEVERITIES = ("P0", "P1", "P2", "P3")
REQUIRED_FIELDS = ("id", "reason", "owner", "status", "severity")
OPTIONAL_FIELDS = ("fields", "issue", "panic", "steps")
RELEASED_STATUSES = frozenset(("accepted", "wontfix", "known-bug"))
# Record keys besides `id` and `fields` that an entry may list. `exit`, which the isolated runner
# adds when a case process failed after recording, is deliberately not listable.
TOP_LEVEL_KEYS = ("apis", "exception")
# Observation fields that the `panic` fingerprint and `steps` settle instead of a field name.
FAILURE_FIELDS = ("panic", "step_failures")


def load(path, expected_engine):
    cases = {}
    engines = set()
    with open(path, encoding="utf-8") as file:
        for line_number, line in enumerate(file, 1):
            line = line.strip()
            if not line:
                continue
            case = json.loads(line)
            engines.add(case.pop("engine", None))
            case_id = case["id"]
            if case_id in cases:
                raise SystemExit(f"{path}:{line_number}: duplicate case id {case_id!r}")
            cases[case_id] = case
    if not cases:
        raise SystemExit(f"{path}: no cases recorded")
    if len(engines) > 1:
        raise SystemExit(f"{path}: mixed engine markers {sorted(map(str, engines))}")
    actual = next(iter(engines))
    if actual != expected_engine:
        raise SystemExit(
            f"{path}: expected {expected_engine!r} results, found {actual!r}. "
            "A missing [patch.crates-io] makes the QuickJS run build SpiderMonkey, so this "
            "comparison would be SpiderMonkey against itself."
        )
    return cases


def load_expected_cases(path, allow_empty=True):
    with open(path, encoding="utf-8") as file:
        document = json.load(file)
    cases = document.get("cases")
    if document.get("version") != 1:
        raise SystemExit(f"{path}: unsupported case inventory version {document.get('version')!r}")
    if not isinstance(cases, list) or not all(isinstance(case, str) and case for case in cases):
        raise SystemExit(f"{path}: expected a 'cases' string list")
    if not allow_empty and not cases:
        raise SystemExit(f"{path}: expected a non-empty 'cases' string list")
    if len(cases) != len(set(cases)):
        raise SystemExit(f"{path}: duplicate case id in case inventory")
    complete = document.get("complete", True)
    if not isinstance(complete, bool):
        raise SystemExit(f"{path}: expected 'complete' to be a boolean")
    return set(cases), complete


def load_differences(path):
    if not path:
        return {}
    with open(path, "rb") as file:
        document = tomllib.load(file)
    differences = {}
    for entry in document.get("difference", []):
        missing = [field for field in REQUIRED_FIELDS if field not in entry]
        if missing:
            raise SystemExit(
                f"{path}: difference {entry.get('id', '<no id>')!r} is missing "
                f"{', '.join(missing)}"
            )
        case_id = entry["id"]
        unknown = sorted(set(entry) - set(REQUIRED_FIELDS) - set(OPTIONAL_FIELDS))
        if unknown:
            raise SystemExit(
                f"{path}: difference {case_id!r} has unknown key(s) {', '.join(unknown)}; "
                f"expected {', '.join(REQUIRED_FIELDS + OPTIONAL_FIELDS)}"
            )
        fields = entry.setdefault("fields", [])
        if not isinstance(fields, list) or not all(
            isinstance(field, str) and field for field in fields
        ):
            raise SystemExit(f"{path}: difference {case_id!r} must list non-empty fields")
        if len(fields) != len(set(fields)):
            raise SystemExit(f"{path}: difference {case_id!r} repeats a field")
        for field in fields:
            if field == "fields":
                raise SystemExit(
                    f"{path}: difference {case_id!r} lists \"fields\", which would release the "
                    "whole observation; list each differing field as \"fields.<name>\""
                )
            name = field.removeprefix("fields.")
            if field.startswith("fields.") and name and name not in FAILURE_FIELDS:
                continue
            if field not in TOP_LEVEL_KEYS:
                raise SystemExit(
                    f"{path}: difference {case_id!r} lists unknown field {field!r}; expected "
                    f"{', '.join(TOP_LEVEL_KEYS)} or \"fields.<name>\" (use `panic` and `steps` "
                    "for failures)"
                )
        if not fields and "panic" not in entry:
            raise SystemExit(
                f"{path}: difference {case_id!r} releases nothing; list fields or a panic"
            )
        status = entry["status"]
        if status not in DIFFERENCE_STATUSES:
            raise SystemExit(
                f"{path}: difference {case_id!r} has unknown status {status!r}; "
                f"expected one of {', '.join(DIFFERENCE_STATUSES)}"
            )
        severity = entry["severity"]
        if severity not in DIFFERENCE_SEVERITIES:
            raise SystemExit(
                f"{path}: difference {case_id!r} has unknown severity {severity!r}; "
                f"expected one of {', '.join(DIFFERENCE_SEVERITIES)}"
            )
        if status in RELEASED_STATUSES and severity in ("P0", "P1"):
            raise SystemExit(
                f"{path}: difference {case_id!r} cannot release a {severity} defect; "
                "fix it or keep its status pending/bug"
            )
        if "issue" in entry and (
            not isinstance(entry["issue"], str) or not entry["issue"].strip()
        ):
            raise SystemExit(
                f"{path}: difference {case_id!r} has an empty issue; omit it or fill it in"
            )
        if "steps" in entry:
            steps = entry["steps"]
            if not isinstance(steps, list) or not steps or not all(
                isinstance(step, str) and step for step in steps
            ):
                raise SystemExit(
                    f"{path}: difference {case_id!r} must list non-empty step names"
                )
            if len(steps) != len(set(steps)):
                raise SystemExit(f"{path}: difference {case_id!r} repeats a step name")
        if "panic" in entry and (not isinstance(entry["panic"], str) or not entry["panic"]):
            raise SystemExit(
                f"{path}: difference {case_id!r} has an empty panic fingerprint"
            )
        if "steps" in entry and "panic" not in entry:
            raise SystemExit(
                f"{path}: difference {case_id!r} lists steps without a panic; a failing step "
                "records its panic as fields.panic, so add the first failing step's panic as `panic`"
            )
        if status in ("pending", "bug", "known-bug") and "issue" not in entry:
            raise SystemExit(
                f"{path}: difference {case_id!r} is {status} but has no tracking issue"
            )
        if case_id in differences:
            raise SystemExit(f"{path}: duplicate difference id {case_id!r}")
        differences[case_id] = entry
    return differences


def failed_steps(case):
    """The sub-scenarios that panicked in `case`, in execution order."""
    fields = case.get("fields")
    if not isinstance(fields, dict):
        return []
    failures = fields.get("step_failures")
    if not isinstance(failures, list):
        return []
    steps = []
    for failure in failures:
        if isinstance(failure, dict) and isinstance(failure.get("step"), str):
            steps.append(failure["step"])
    return steps


def panic_message(case):
    fields = case.get("fields")
    if not isinstance(fields, dict):
        return None
    panic = fields.get("panic")
    return panic if isinstance(panic, str) else None


def observation_fields(case):
    fields = case.get("fields")
    return fields if isinstance(fields, dict) else {}


def difference_problems(left, right, difference):
    """Compares `right` (QuickJS) with `left` (SpiderMonkey) under one allowlist entry.

    Returns `(uncovered, stale)`: the differences the entry does not release, and the parts of the
    entry that no longer apply. Both empty means the entry releases the difference exactly.
    """
    listed = difference["fields"]
    listed_keys = {field for field in listed if not field.startswith("fields.")}
    listed_fields = {field.removeprefix("fields.") for field in listed if field.startswith("fields.")}
    panic = difference.get("panic")
    left_panic = panic_message(left)
    right_panic = panic_message(right)
    # SpiderMonkey is the reference. A crash there is a regression in the reference engine, not a
    # QuickJS difference, so no allowlist entry may absorb it: `panic` fingerprints describe the
    # QuickJS side only, and an entry without one licenses fields, not a crash on either side.
    if left_panic is not None:
        return [f"SpiderMonkey panicked: {left_panic!r}"], []
    uncovered = []
    stale = []
    if right_panic is not None and right_panic != panic:
        uncovered.append(
            f"QuickJS panicked: {right_panic!r}"
            + ("" if panic is None else f", the entry's fingerprint is {panic!r}")
        )
    elif right_panic is None and panic is not None:
        stale.append("QuickJS no longer panics; remove `panic`")

    for key in sorted((set(left) | set(right)) - {"id", "fields"}):
        if key in listed_keys:
            if left.get(key) == right.get(key):
                stale.append(f"{key} no longer differs; remove it from `fields`")
        elif left.get(key) != right.get(key):
            uncovered.append(f"{key} differs")

    # A case that panics stops recording, and a failed step drops its own observations, so the
    # fields QuickJS lacks are only accounted for by a panic the entry fingerprints. This relies on
    # a case recording the same fields on both engines up to that panic, which holds because the
    # cases record unconditionally or branch only on their own data.
    left_fields = observation_fields(left)
    right_fields = observation_fields(right)
    lost = 0
    for name in sorted((set(left_fields) | set(right_fields)) - set(FAILURE_FIELDS)):
        in_left = name in left_fields
        in_right = name in right_fields
        if name in listed_fields:
            if not in_right:
                stale.append(f"fields.{name} is not recorded by QuickJS; remove it from `fields`")
            elif in_left and left_fields[name] == right_fields[name]:
                stale.append(f"fields.{name} no longer differs; remove it from `fields`")
        elif in_left and in_right:
            if left_fields[name] != right_fields[name]:
                uncovered.append(f"fields.{name} differs")
        elif in_right:
            uncovered.append(f"fields.{name} is recorded only by QuickJS")
        elif right_panic is None:
            uncovered.append(f"fields.{name} is missing on QuickJS")
        elif right_panic != panic:
            lost += 1
    if lost:
        uncovered.append(f"{lost} SpiderMonkey fields are missing after the QuickJS panic")
    for name in sorted(listed_fields - set(left_fields) - set(right_fields)):
        stale.append(f"fields.{name} is recorded by neither engine; remove it from `fields`")

    failed = set(failed_steps(right))
    steps = set(difference.get("steps", ()))
    for step in sorted(failed - steps):
        uncovered.append(f"step {step} failed but is not in `steps`")
    for step in sorted(steps - failed):
        stale.append(f"step {step} no longer fails; remove it from `steps`")
    return uncovered, stale


def summary(difference, *details, matched=True):
    """The parenthesised description of an entry in the report."""
    parts = []
    if difference["fields"]:
        parts.append(f"fields: {', '.join(sorted(difference['fields']))}")
    if difference.get("steps"):
        parts.append(f"steps: {', '.join(difference['steps'])}")
    parts.extend(details)
    if matched and difference.get("panic"):
        parts.append("panic: matched")
    return ", ".join(parts)


def main(argv):
    arguments = list(argv[1:])
    cases_path = None
    sm_cases_path = None
    qjs_cases_path = None
    while any(argument in arguments for argument in ("--cases", "--sm-cases", "--qjs-cases")):
        for option in ("--cases", "--sm-cases", "--qjs-cases"):
            if option not in arguments:
                continue
            index = arguments.index(option)
            if index + 1 >= len(arguments):
                raise SystemExit(__doc__)
            value = arguments[index + 1]
            del arguments[index : index + 2]
            if option == "--cases":
                cases_path = value
            elif option == "--sm-cases":
                sm_cases_path = value
            else:
                qjs_cases_path = value
    if (sm_cases_path is None) != (qjs_cases_path is None):
        raise SystemExit(__doc__)
    if cases_path is not None and sm_cases_path is not None:
        raise SystemExit("use either --cases or --sm-cases/--qjs-cases, not both")
    if len(arguments) not in (2, 3):
        raise SystemExit(__doc__)
    sm = load(arguments[0], "spidermonkey")
    qjs = load(arguments[1], "quickjs")
    differences = load_differences(arguments[2] if len(arguments) == 3 else None)
    expected = load_expected_cases(cases_path) if cases_path else None

    expected_sm = load_expected_cases(sm_cases_path) if sm_cases_path else expected
    expected_qjs = load_expected_cases(qjs_cases_path) if qjs_cases_path else expected
    expected_sm_cases, expected_sm_complete = expected_sm if expected_sm else (None, False)
    expected_qjs_cases, expected_qjs_complete = expected_qjs if expected_qjs else (None, False)

    missing_in_qjs = sorted(set(sm) - set(qjs))
    missing_in_sm = sorted(set(qjs) - set(sm))
    missing_from_sm = sorted(expected_sm_cases - set(sm)) if expected_sm_cases is not None else []
    missing_from_qjs = sorted(expected_qjs_cases - set(qjs)) if expected_qjs_cases is not None else []
    unexpected_in_sm = sorted(set(sm) - expected_sm_cases) if expected_sm_cases is not None else []
    unexpected_in_qjs = sorted(set(qjs) - expected_qjs_cases) if expected_qjs_cases is not None else []
    inventory_mismatches = []
    if expected_sm_cases is not None and expected_qjs_cases is not None and expected_sm_cases != expected_qjs_cases:
        inventory_mismatches = sorted(expected_sm_cases ^ expected_qjs_cases)
    mismatches = []
    known = []
    stale_parts = {}
    compared_cases = set(sm) & set(qjs)
    for case_id in sorted(compared_cases):
        left = sm[case_id]
        right = qjs[case_id]
        if left == right:
            continue
        difference = differences.get(case_id)
        if difference is None:
            mismatches.append((case_id, left, right, ["no allowlist entry for this case"], []))
            continue
        uncovered, stale_entry = difference_problems(left, right, difference)
        if uncovered:
            mismatches.append((case_id, left, right, uncovered, stale_entry))
        elif stale_entry:
            stale_parts[case_id] = stale_entry
        else:
            known.append((case_id, left, right, difference))

    known_ids = {case_id for case_id, _, _, _ in known}
    mismatch_ids = {case_id for case_id, _, _, _, _ in mismatches}
    stale = []
    for case_id in sorted(differences):
        difference = differences[case_id]
        if case_id in compared_cases:
            if case_id in stale_parts:
                stale.append((case_id, difference, "; ".join(stale_parts[case_id])))
            elif case_id not in known_ids and case_id not in mismatch_ids:
                stale.append((case_id, difference, "listed case no longer differs"))
        elif (
            expected_sm_cases is not None
            and expected_qjs_cases is not None
            and expected_sm_complete
            and expected_qjs_complete
            and case_id not in expected_sm_cases
            and case_id not in expected_qjs_cases
        ):
            stale.append((case_id, difference, "listed case no longer exists"))

    released = 0
    bugs = 0
    for case_id, left, right, difference in known:
        status = difference["status"]
        severity = f"severity: {difference['severity']}"
        if status == "known-bug":
            bugs += 1
            print(f"known QuickJS bug: {case_id} ({summary(difference, severity)})")
            print(f"  issue: {difference['issue']}")
        elif status in RELEASED_STATUSES:
            released += 1
            print(
                f"known QuickJS defect: {case_id} "
                f"({summary(difference, f'status: {status}', severity)})"
            )
            if difference.get("issue"):
                print(f"  issue: {difference['issue']}")
        else:
            print(
                f"unaccepted QuickJS defect: {case_id} "
                f"({summary(difference, f'status: {status}', severity)})"
            )
            if difference.get("issue"):
                print(f"  issue: {difference['issue']}")
        print(f"  SpiderMonkey: {json.dumps(left, sort_keys=True)}")
        print(f"  QuickJS:      {json.dumps(right, sort_keys=True)}")
    for case_id in missing_in_qjs:
        print(f"missing in QuickJS: {case_id}")
    for case_id in missing_in_sm:
        print(f"missing in SpiderMonkey: {case_id}")
    for case_id in missing_from_sm:
        print(f"missing from SpiderMonkey case inventory: {case_id}")
    for case_id in missing_from_qjs:
        print(f"missing from QuickJS case inventory: {case_id}")
    for case_id in unexpected_in_sm:
        print(f"unexpected in SpiderMonkey case inventory: {case_id}")
    for case_id in unexpected_in_qjs:
        print(f"unexpected in QuickJS case inventory: {case_id}")
    for case_id in inventory_mismatches:
        print(f"case inventory mismatch between engines: {case_id}")
    for case_id, difference, reason in stale:
        print(
            f"stale known QuickJS defect: {case_id} ({summary(difference, reason, matched=False)})"
        )
    for case_id, left, right, uncovered, stale_entry in mismatches:
        print(f"mismatch: {case_id}")
        for problem in uncovered:
            print(f"  not covered: {problem}")
        for problem in stale_entry:
            print(f"  also stale: {problem}")
        print(f"  SpiderMonkey: {json.dumps(left, sort_keys=True)}")
        print(f"  QuickJS:      {json.dumps(right, sort_keys=True)}")

    unaccepted = len(known) - released - bugs
    failed = bool(
        missing_in_qjs
        or missing_in_sm
        or missing_from_sm
        or missing_from_qjs
        or unexpected_in_sm
        or unexpected_in_qjs
        or inventory_mismatches
        or stale
        or mismatches
        or unaccepted
    )
    print(
        f"compared {len(set(sm) & set(qjs))} cases: "
        f"{len(mismatches)} mismatches, "
        f"{released} known QuickJS defects, "
        f"{bugs} known QuickJS bugs, "
        f"{unaccepted} unaccepted QuickJS defects, "
        f"{len(stale)} stale known QuickJS defects, "
        f"{len(missing_in_qjs)} missing in QuickJS, "
        f"{len(missing_in_sm)} missing in SpiderMonkey, "
        f"{len(missing_from_sm)} missing from SpiderMonkey case inventory, "
        f"{len(missing_from_qjs)} missing from QuickJS case inventory, "
        f"{len(unexpected_in_sm)} unexpected in SpiderMonkey case inventory, "
        f"{len(unexpected_in_qjs)} unexpected in QuickJS case inventory, "
        f"{len(inventory_mismatches)} case inventory mismatch(es) between engines"
    )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
