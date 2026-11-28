#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Audit the QuickJS shim fix records against quickjs-difference-allowlist.toml.

Usage:
  fix_record.py check [--baseline REV|FILE] [--allowlist FILE] [--record FILE]
                      [--cases CASES.json]

A difference that disappeared from ``quickjs-difference-allowlist.toml`` must have a matching record in
``quickjs-shim-fix-records.md``. Records are Markdown sections headed by a contract case id. Each record must
carry the fields in ``REQUIRED_FIELDS``. Its case id must exist in the runtime case inventory
when ``--cases`` is supplied, unless the baseline shows that the difference was removed: a
removed case is deliberately absent from the inventory, and its record documents why it went
away. A selected run writes an inventory with ``complete = false``; in that case records outside
the selection are not treated as unknown. Code fences (including the template) are ignored.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib


REQUIRED_FIELDS = (
    "Date",
    "Owner",
    "Issue",
    "Previous status",
    "Previous severity",
    "Fixed in",
    "Shim change",
    "Regression",
    "Result",
    "Follow-up",
)
CASE_ID = re.compile(r"^[a-z][a-z0-9_]*\.[a-z0-9_]+$")
PLACEHOLDERS = re.compile(r"<[^>]+>")
STATUSES = ("pending", "bug", "known-bug", "accepted", "wontfix")
SEVERITIES = ("P0", "P1", "P2", "P3")


def difference_ids(path):
    with open(path, "rb") as file:
        document = tomllib.load(file)
    ids = set()
    for entry in document.get("difference", []):
        case_id = entry.get("id")
        if not isinstance(case_id, str):
            raise SystemExit(f"{path}: difference without a string id")
        ids.add(case_id)
    return ids


def suite_relative(path):
    """Express PATH relative to the repository root that holds this suite.

    `git show REV:PATH` resolves from the repository root, which is the servo-quickjs checkout,
    not `engine-contract/`. Keeping the default baseline working therefore needs the prefix.
    """
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    completed = subprocess.run(
        ["git", "-C", here, "rev-parse", "--show-toplevel"],
        text=True,
        capture_output=True,
    )
    if completed.returncode != 0:
        return path
    top = completed.stdout.strip()
    return os.path.relpath(os.path.join(here, path), top).replace(os.sep, "/")


def read_baseline(source):
    if ":" in source and not source.startswith("/") and not source.startswith("."):
        revision, path = source.split(":", 1)
        candidates = [path]
        # The allowlist was called known-differences.toml before this rename. A branch that has
        # not committed the rename must still produce a usable baseline for the A4 audit.
        if path.endswith("quickjs-difference-allowlist.toml"):
            candidates.append(
                path.replace(
                    "quickjs-difference-allowlist.toml", "known-differences.toml"
                )
            )
        for candidate in candidates:
            completed = subprocess.run(
                ["git", "show", f"{revision}:{candidate}"],
                text=True,
                capture_output=True,
            )
            if completed.returncode == 0:
                if candidate != path:
                    print(
                        f"using pre-rename baseline {revision}:{candidate}",
                        file=sys.stderr,
                    )
                return completed.stdout
        raise SystemExit(
            f"could not read baseline {source!r}: {completed.stderr.strip()}"
        )
    with open(source, encoding="utf-8") as file:
        return file.read()


def baseline_ids(source):
    document = tomllib.loads(read_baseline(source))
    return {
        entry["id"]
        for entry in document.get("difference", [])
        if isinstance(entry.get("id"), str)
    }


def strip_code_fences(text):
    lines = []
    fenced = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            fenced = not fenced
            continue
        if not fenced:
            lines.append(line)
    return "\n".join(lines)


def parse_records(path):
    with open(path, encoding="utf-8") as file:
        text = strip_code_fences(file.read())
    try:
        records_text = text.split("## Records", 1)[1]
    except IndexError:
        raise SystemExit(f"{path}: missing '## Records' section")
    records = {}
    current = None
    current_field = None
    for line in records_text.splitlines():
        heading = re.match(r"^##\s+(\S+)\s*$", line)
        if heading:
            case_id = heading.group(1)
            current = {"case_id": case_id, "fields": {}}
            records[case_id] = current
            current_field = None
            continue
        if current is None:
            continue
        field = re.match(r"^-\s+([^:]+):\s*(.*)$", line)
        if field:
            same_line = field.group(2).strip()
            current_field = field.group(1).strip()
            current["fields"][current_field] = [same_line] if same_line else []
            continue
        if current_field and (line.startswith("  ") or not line.strip()):
            if line.strip():
                current["fields"][current_field].append(line.strip())
            continue
        current_field = None
    for record in records.values():
        record["fields"] = {
            name: " ".join(values).strip()
            for name, values in record["fields"].items()
        }
    return records


def load_cases(path):
    with open(path, encoding="utf-8") as file:
        document = json.load(file)
    if document.get("version") != 1:
        raise SystemExit(f"{path}: unsupported case inventory version {document.get('version')!r}")
    cases = document.get("cases")
    if not isinstance(cases, list) or not all(isinstance(case, str) and case for case in cases):
        raise SystemExit(f"{path}: expected a 'cases' string list")
    complete = document.get("complete", True)
    if not isinstance(complete, bool):
        raise SystemExit(f"{path}: expected 'complete' to be a boolean")
    return set(cases), complete


def is_placeholder(value):
    return bool(PLACEHOLDERS.fullmatch(value.strip()))


def validate_record(record, path):
    problems = []
    case_id = record["case_id"]
    if not CASE_ID.fullmatch(case_id):
        problems.append(f"{path}: record heading {case_id!r} is not a contract case id")
    for field in REQUIRED_FIELDS:
        value = record["fields"].get(field)
        if value is None:
            problems.append(f"{path}: record {case_id!r} is missing {field}")
        elif not value.strip():
            problems.append(f"{path}: record {case_id!r} has an empty {field}")
        elif is_placeholder(value):
            problems.append(f"{path}: record {case_id!r} still has the template placeholder in {field}")
    status = record["fields"].get("Previous status")
    if status is not None and status not in STATUSES:
        problems.append(f"{path}: record {case_id!r} has unknown previous status {status!r}")
    severity = record["fields"].get("Previous severity")
    if severity is not None and severity not in SEVERITIES:
        problems.append(f"{path}: record {case_id!r} has unknown previous severity {severity!r}")
    return problems


def check(args):
    current = difference_ids(args.allowlist)
    previous = baseline_ids(args.baseline)
    removed = previous - current
    records = parse_records(args.record)
    problems = []
    for case_id in sorted(removed):
        if case_id not in records:
            problems.append(
                f"{args.record}: removed difference {case_id!r} has no fix record"
            )
    for record in records.values():
        problems.extend(validate_record(record, args.record))

    if args.cases:
        cases, complete = load_cases(args.cases)
        if complete:
            for case_id in sorted(set(records) - cases):
                if case_id in removed:
                    continue
                problems.append(
                    f"{args.record}: record {case_id!r} is not in the runtime case inventory"
                )

    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        return 1
    print(
        f"fix record: {len(removed)} removed difference(s), "
        f"{len(records)} record(s), 0 problems"
    )
    return 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    check_parser = subparsers.add_parser("check")
    check_parser.add_argument(
        "--baseline",
        default="HEAD:" + suite_relative("quickjs-difference-allowlist.toml"),
    )
    check_parser.add_argument(
        "--allowlist",
        default=os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "quickjs-difference-allowlist.toml",
        ),
    )
    check_parser.add_argument(
        "--record",
        default=os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "quickjs-shim-fix-records.md",
        ),
    )
    check_parser.add_argument("--cases")
    args = parser.parse_args(argv)
    if args.command == "check":
        return check(args)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
