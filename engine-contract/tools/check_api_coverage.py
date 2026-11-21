#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Check that every mozjs entry in Servo's API usage has a contract case.

Usage: check_api_coverage.py SPIDERMONKEY-RESULTS.jsonl MOZJS-API-INVENTORY.toml \
           SERVO-MOZJS-API-USAGE.toml

The JSON Lines input is one of the files written by `CONTRACT_OUT`. Each case must list the mozjs
entry points it calls in its `apis` field. Only entries in `SCOPE.toml` are required to have a
case: that is the surface Servo actually uses. Entries outside the usage file are validated against
the inventory but do not affect coverage. Every case must still exercise at least one scoped
entry: the suite only keeps cases for Servo's usage surface.

Every scoped entry must be exercised by a case. `implemented` entries count as normal coverage;
`empty` and `todo` entries are counted separately because their case records what the shim does
today (silent no-op or a `qjs_todo!` panic), and any behavioural difference is settled by
`quickjs-difference-allowlist.toml` during the QuickJS diff. A case may only claim an entry with
the status the inventory gives it, so a case cannot pass an unimplemented entry off as working.
"""

import collections
import json
import sys
import tomllib


def load_inventory(path):
    with open(path, "rb") as file:
        document = tomllib.load(file)
    return document["api"]


def load_scope(path):
    with open(path, "rb") as file:
        document = tomllib.load(file)
    scope = {}
    for entry in document.get("entry", []):
        name = entry["name"]
        if name in scope:
            raise SystemExit(f"{path}: duplicate scope entry {name!r}")
        provenance = entry.get("provenance")
        if not isinstance(provenance, list) or not provenance:
            raise SystemExit(f"{path}: scope entry {name!r} has no provenance")
        scope[name] = entry
    declared = document.get("scope", {}).get("entries")
    if declared is not None and declared != len(scope):
        raise SystemExit(
            f"{path}: scope.entries says {declared}, but there are {len(scope)} entries"
        )
    return scope


def load_coverage(path):
    covered = set()
    cases = []
    case_ids = set()
    with open(path, encoding="utf-8") as file:
        for line_number, line in enumerate(file, 1):
            line = line.strip()
            if not line:
                continue
            case = json.loads(line)
            case_id = case["id"]
            if case_id in case_ids:
                raise SystemExit(f"{path}:{line_number}: duplicate case id {case_id!r}")
            case_ids.add(case_id)
            apis = case.get("apis")
            if not isinstance(apis, list) or not apis:
                raise SystemExit(f"{path}:{line_number}: case {case_id!r} has no apis")
            unknown = [api for api in apis if not isinstance(api, str)]
            if unknown:
                raise SystemExit(f"{path}:{line_number}: case {case_id!r} has non-string apis")
            cases.append((case_id, apis))
            covered.update(apis)
    return cases, covered


def main(argv):
    if len(argv) != 4:
        raise SystemExit(__doc__)
    coverage_path, inventory_path, usage_path = argv[1], argv[2], argv[3]
    entries = load_inventory(inventory_path)
    scope = load_scope(usage_path)
    cases, covered = load_coverage(coverage_path)

    by_name = collections.defaultdict(list)
    for entry in entries:
        by_name[entry["name"]].append(entry)

    statuses = {
        name: (
            "implemented"
            if any(entry["status"] == "implemented" for entry in matches)
            else sorted({entry["status"] for entry in matches})[0]
        )
        for name, matches in by_name.items()
    }

    errors = []
    for name, entry in scope.items():
        matches = by_name.get(name)
        if not matches:
            errors.append(f"usage entry {name} is not in the mozjs API inventory")
            continue
        expected = entry.get("status")
        actual = statuses[name]
        if expected != actual:
            errors.append(f"usage entry {name} says {expected}, inventory says {actual}")

    covered_eligible = set()
    covered_unimplemented = set()
    covered_outside_scope = set()
    for case_id, apis in cases:
        for name in apis:
            matches = by_name.get(name)
            if not matches:
                errors.append(f"{case_id}: {name} is not in the mozjs API inventory")
            elif statuses[name] != "implemented":
                if name in scope:
                    # A scoped entry the shim does not implement yet still needs a case. The
                    # case documents today's behaviour; the QuickJS diff decides whether the
                    # recorded divergence is a known defect or a fixable bug.
                    covered_unimplemented.add(name)
                else:
                    errors.append(
                        f"{case_id}: {name} is classified {statuses[name]}, not implemented"
                    )
            elif name not in scope:
                covered_outside_scope.add(name)
            else:
                covered_eligible.add(name)

        if not set(apis) & set(scope):
            errors.append(f"{case_id}: case has no entry in servo-mozjs-api-usage.toml")

    eligible = {
        name for name, entry in scope.items() if entry.get("status") == "implemented"
    }
    missing = eligible - covered_eligible
    if missing:
        errors.append(f"{len(missing)} scoped mozjs entry points have no contract case")
        for name in sorted(missing):
            errors.append(f"  {name}: {scope[name]['provenance'][0]}")

    unimplemented = {
        name for name, entry in scope.items() if entry.get("status") != "implemented"
    }
    missing_unimplemented = unimplemented - covered_unimplemented
    if missing_unimplemented:
        errors.append(
            f"{len(missing_unimplemented)} scoped mozjs entry points are not exercised by any case"
        )
        for name in sorted(missing_unimplemented):
            errors.append(
                f"  {name} ({scope[name]['status']}): {scope[name]['provenance'][0]}"
            )

    if errors:
        for error in errors:
            print("error:", error, file=sys.stderr)
        return 1

    print(
        f"API coverage ok: {len(covered_eligible) + len(covered_unimplemented)}/{len(scope)} "
        f"scoped entries covered by {len(cases)} cases"
    )
    print(
        f"  implemented {len(covered_eligible)}/{len(eligible)}, "
        f"empty/todo {len(covered_unimplemented)}/{len(unimplemented)}"
    )
    print(
        f"scope: {len(scope)} entries; "
        f"{len(covered_outside_scope)} out-of-scope entries exercised and ignored"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
