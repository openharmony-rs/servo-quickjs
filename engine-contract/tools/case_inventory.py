#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""List the contract case ids declared by the engine contract test sources.

Usage:
  case_inventory.py scan --tests-dir DIR --output FILE [--case ID]... [--cases ID[,ID...]]...

The output is JSON: ``{"version": 1, "cases": ["module.name", ...]}``. ``compare.py`` uses it to
verify that both engine runs produced exactly the cases the source declares, rather than silently
comparing whatever subset happened to be present in the two JSONL files.
"""

import argparse
import json
import os
import re
import sys


CASE_ID = re.compile(r"^[a-z][a-z0-9_]*\.[a-z0-9_]+$")
CALL = re.compile(
    r"\b(?:record_case(?:_with_options)?|isolate_case|run_case|run_group|record)\s*\("
)


def matching_paren(text, start):
    depth = 0
    index = start
    in_string = False
    escaped = False
    while index < len(text):
        char = text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        else:
            if char == '"':
                in_string = True
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    return index
        index += 1
    return -1


def scan_file(path):
    with open(path, encoding="utf-8") as file:
        text = file.read()
    cases = set()
    for match in CALL.finditer(text):
        end = matching_paren(text, match.end() - 1)
        if end < 0:
            continue
        arguments = text[match.end() : end]
        for value in re.findall(r'"((?:\\.|[^"\\])*)"', arguments):
            if CASE_ID.fullmatch(value):
                cases.add(value)
                break
    return cases


def scan_directory(tests_dir):
    cases = set()
    for name in sorted(os.listdir(tests_dir)):
        if not name.endswith("_test.rs"):
            continue
        cases.update(scan_file(os.path.join(tests_dir, name)))
    return cases


def split_cases(values):
    cases = []
    for value in values:
        cases.extend(part for part in re.split(r"[, \t\n]+", value) if part)
    return cases


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    scan_parser = subparsers.add_parser("scan")
    scan_parser.add_argument("--tests-dir", required=True)
    scan_parser.add_argument("--output", required=True)
    scan_parser.add_argument("--case", action="append", default=[])
    scan_parser.add_argument("--cases", action="append", default=[])
    args = parser.parse_args(argv)

    cases = scan_directory(args.tests_dir)
    if not cases:
        print(f"error: no contract cases found under {args.tests_dir}", file=sys.stderr)
        return 1

    selected = split_cases(args.case + args.cases)
    if selected:
        missing = sorted(set(selected) - cases)
        if missing:
            print("error: selected contract case(s) not found:", file=sys.stderr)
            for case in missing:
                print(f"  {case}", file=sys.stderr)
            return 1
        cases = set(selected)

    output_dir = os.path.dirname(os.path.abspath(args.output))
    os.makedirs(output_dir, exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as file:
        json.dump({"version": 1, "cases": sorted(cases)}, file, indent=2)
        file.write("\n")
    print(f"wrote {args.output}: {len(cases)} cases")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
