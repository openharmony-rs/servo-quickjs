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
defect and does not fail the run. A listed difference with status `pending` or `bug` still fails,
as does an unlisted difference. When ``CASES.json`` is supplied, both engine runs must contain
exactly those case ids; a truncated or stale run therefore fails instead of comparing a subset.
An entry whose listed case participated in both runs but no longer differs is stale and fails the
run. When both runtime inventories are marked complete, an entry for a case that no longer exists
is stale as well. Entries are never silent skips.

Use ``--sm-cases`` and ``--qjs-cases`` when the two engines write their own runtime case
inventories. Each engine's JSONL is then checked against its own inventory, and the inventories
must match the selected case set exactly.
"""

import json
import sys
import tomllib


DIFFERENCE_STATUSES = ("pending", "bug", "accepted", "wontfix")
DIFFERENCE_SEVERITIES = ("P0", "P1", "P2", "P3")
REQUIRED_FIELDS = ("id", "fields", "reason", "owner", "status", "severity")
OPTIONAL_FIELDS = ("issue", "steps")
RELEASED_STATUSES = frozenset(("accepted", "wontfix"))


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
        fields = entry["fields"]
        if not isinstance(fields, list) or not fields or not all(
            isinstance(field, str) and field for field in fields
        ):
            raise SystemExit(f"{path}: difference {case_id!r} must list non-empty fields")
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
        if status in ("pending", "bug") and "issue" not in entry:
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


def matches_difference(left, right, difference):
    fields = difference["fields"]
    if not fields:
        return False
    panic = difference.get("panic")
    left_panic = panic_message(left)
    right_panic = panic_message(right)
    if panic is None:
        if right_panic is not None:
            return False
    elif right_panic != panic:
        return False
    remaining_left = {key: value for key, value in left.items() if key not in fields}
    remaining_right = {key: value for key, value in right.items() if key not in fields}
    if remaining_left != remaining_right:
        return False
    # `fields = ["fields"]` must not silently absorb a new sub-scenario failure. Every step that
    # failed on either engine has to be named in the allowlist so the divergence stays visible.
    observed_steps = set(failed_steps(left)) | set(failed_steps(right))
    return observed_steps <= set(difference.get("steps", ()))


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
    compared_cases = set(sm) & set(qjs)
    for case_id in sorted(compared_cases):
        left = sm[case_id]
        right = qjs[case_id]
        if left == right:
            continue
        difference = differences.get(case_id)
        if difference and matches_difference(left, right, difference):
            known.append((case_id, left, right, difference))
        else:
            mismatches.append((case_id, left, right))

    known_ids = {case_id for case_id, _, _, _ in known}
    mismatch_ids = {case_id for case_id, _, _ in mismatches}
    stale = []
    for case_id in sorted(differences):
        difference = differences[case_id]
        if case_id in compared_cases:
            if case_id not in known_ids and case_id not in mismatch_ids:
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
    for case_id, left, right, difference in known:
        status = difference["status"]
        if status in RELEASED_STATUSES:
            released += 1
            print(
                f"known QuickJS defect: {case_id} "
                f"(fields: {', '.join(sorted(difference['fields']))}, "
                f"status: {status}, severity: {difference['severity']}"
                + (", panic: matched" if difference.get("panic") else "")
                + ")"
            )
            if difference.get("issue"):
                print(f"  issue: {difference['issue']}")
        else:
            print(
                f"unaccepted QuickJS defect: {case_id} "
                f"(fields: {', '.join(sorted(difference['fields']))}, "
                f"status: {status}, severity: {difference['severity']}"
                + (", panic: matched" if difference.get("panic") else "")
                + ")"
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
            f"stale known QuickJS defect: {case_id} "
            f"(fields: {', '.join(sorted(difference['fields']))}, {reason})"
        )
    for case_id, left, right in mismatches:
        print(f"mismatch: {case_id}")
        print(f"  SpiderMonkey: {json.dumps(left, sort_keys=True)}")
        print(f"  QuickJS:      {json.dumps(right, sort_keys=True)}")

    unaccepted = len(known) - released
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
