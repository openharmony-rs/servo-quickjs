#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Mechanical steps of a mozjs upgrade.

`prepare-mozjs-upgrade` in run.sh calls this after scanning the interface surface. It validates the
shim checkout, updates the two hand-edited pins, refreshes Cargo.lock, and reports the changed
entries that Servo uses together with the case files that mention them.

Usage:
  mozjs_upgrade.py shim-version --qjs-dir DIR
  mozjs_upgrade.py check  --qjs-dir DIR --version X.Y.Z
  mozjs_upgrade.py apply  --servo-root DIR --qjs-dir DIR --version X.Y.Z [--skip-cargo-update]
  mozjs_upgrade.py report --servo-root DIR --qjs-dir DIR --out-dir DIR
"""

import argparse
import glob
import os
import re
import subprocess
import sys
import tomllib


CONTRACT_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


# The usage scan follows forwarding edges through the shim's wrapper bodies, which live beside
# `mozjs/src/engine` rather than inside it. A sparse checkout that stops at `engine` silently drops
# those edges, so require the files that prove the whole `mozjs/src` tree is present.
REQUIRED_SHIM_FILES = (
    "mozjs/upstream-bindgen/jsapi.rs",
    "mozjs/src/engine/mod.rs",
    "mozjs/src/rust.rs",
    "mozjs/src/conversions.rs",
    "mozjs/stub_bindings.py",
)


def read_toml(path):
    with open(path, "rb") as file:
        return tomllib.load(file)


def shim_version(qjs_dir):
    path = os.path.join(qjs_dir, "mozjs", "Cargo.toml")
    if not os.path.isfile(path):
        raise SystemExit(f"{path}: not found; --qjs-dir is not a servo-quickjs checkout")
    version = read_toml(path).get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit(f"{path}: [package] has no version")
    return version


def shim_head(qjs_dir):
    completed = subprocess.run(
        ["git", "-C", qjs_dir, "rev-parse", "HEAD"], text=True, capture_output=True
    )
    head = completed.stdout.strip()
    if completed.returncode != 0 or not head:
        raise SystemExit(f"{qjs_dir}: not a git checkout; cannot record the shim commit")
    return head


def check(args):
    actual = shim_version(args.qjs_dir)
    if actual != args.version:
        raise SystemExit(
            f"servo-quickjs/mozjs/Cargo.toml is {actual}, but --version is {args.version}; "
            "land the shim for the new release first"
        )
    missing = [
        name for name in REQUIRED_SHIM_FILES if not os.path.isfile(os.path.join(args.qjs_dir, name))
    ]
    if missing:
        raise SystemExit(f"{args.qjs_dir}: missing {', '.join(missing)}")
    head = shim_head(args.qjs_dir)
    status = subprocess.run(
        ["git", "-C", args.qjs_dir, "status", "--porcelain"], text=True, capture_output=True
    ).stdout.splitlines()
    status = [
        line
        for line in status
        if line.strip() and "__pycache__" not in line and not line.endswith(".pyc")
    ]
    if status:
        print(
            "warning: the shim checkout has uncommitted changes; record the commit you tested",
            file=sys.stderr,
        )
    print(f"shim version: {actual}; commit: {head[:7]}")
    return 0


def shim_version_command(args):
    print(shim_version(args.qjs_dir))
    return 0


def replace_once(path, pattern, replacement, description):
    with open(path, encoding="utf-8") as file:
        text = file.read()
    updated, count = re.subn(pattern, replacement, text)
    if count != 1:
        raise SystemExit(f"{path}: expected one {description}, found {count}")
    if updated != text:
        with open(path, "w", encoding="utf-8") as file:
            file.write(updated)


def update_pins(servo_root, qjs_dir, version):
    cargo_toml = os.path.join(servo_root, "Cargo.toml")
    replace_once(
        cargo_toml,
        re.compile(
            r'(?m)(\bjs\s*=\s*\{[^\n}]*package = "mozjs"[^\n}]*version = ")[^"]*(")'
        ),
        lambda match: match.group(1) + "=" + version + match.group(2),
        '`js = { package = "mozjs", version = "..." }` entry',
    )
    cargo_config = os.path.join(servo_root, "support", "quickjs", "cargo-config.toml")
    replace_once(
        cargo_config,
        re.compile(r'(?m)^(mozjs = \{[^\n}]*\brev = ")[^"]*(")'),
        lambda match: match.group(1) + shim_head(qjs_dir) + match.group(2),
        "`mozjs` shim revision",
    )
    print(f"updated {cargo_toml} and {cargo_config}")


def update_lockfile(servo_root):
    completed = subprocess.run(["cargo", "update", "-p", "mozjs"], cwd=servo_root)
    if completed.returncode != 0:
        return False
    return True


def apply(args):
    cargo_toml = os.path.join(args.servo_root, "Cargo.toml")
    cargo_config = os.path.join(args.servo_root, "support", "quickjs", "cargo-config.toml")
    lockfile = os.path.join(args.servo_root, "Cargo.lock")
    backups = {}
    existed = {}
    for path in (cargo_toml, cargo_config, lockfile):
        existed[path] = os.path.isfile(path)
        if os.path.isfile(path):
            with open(path, "rb") as file:
                backups[path] = file.read()

    def restore():
        for path, contents in backups.items():
            with open(path, "wb") as file:
                file.write(contents)
        for path, present in existed.items():
            if not present and os.path.isfile(path):
                os.unlink(path)

    try:
        update_pins(args.servo_root, args.qjs_dir, args.version)
    except BaseException:
        restore()
        raise
    if args.skip_cargo_update:
        print("skipped cargo update -p mozjs; Cargo.lock was not changed")
        return 0
    if update_lockfile(args.servo_root):
        return 0
    restore()
    raise SystemExit(
        "cargo update -p mozjs failed; restored Cargo.toml, "
        "support/quickjs/cargo-config.toml and Cargo.lock"
    )


def load_table(path, section):
    if not os.path.isfile(path):
        raise SystemExit(f"{path}: not found; run scan-mozjs-surface first")
    return read_toml(path).get(section, [])


def case_files(contract_dir, name):
    matches = []
    for path in glob.glob(os.path.join(contract_dir, "*_test.rs")):
        with open(path, encoding="utf-8") as file:
            if f'"{name}"' in file.read():
                matches.append(os.path.relpath(path, contract_dir))
    return matches


def report(args):
    changes = load_table(
        os.path.join(args.out_dir, "mozjs-api-changes.toml"), "change"
    )
    usage = {
        entry["name"]: entry
        for entry in load_table(os.path.join(CONTRACT_DIR, "servo-mozjs-api-usage.toml"), "entry")
    }

    used = [change for change in changes if change["name"] in usage]
    unused = [change for change in changes if change["name"] not in usage]
    print()
    if not changes:
        print("interface surface: no changes; no cases are affected")
    elif not used:
        print(f"interface surface: {len(unused)} changed entries, none used by Servo; no cases needed")
    else:
        print(f"interface surface: {len(used)} changed entries used by Servo")
        for change in used:
            name = change["name"]
            status = change["status"]
            paths = case_files(CONTRACT_DIR, name)
            if paths:
                action = "UPDATE"
                detail = ", ".join(paths)
            elif change["kind"] == "removed":
                action = "REMOVE"
                detail = "no case mentions it; delete any assertion that still does"
            else:
                action = "ADD"
                detail = "no case mentions it"
            print(f"  {action:6} {name} [{status}] {detail}")
        if unused:
            print(f"  {len(unused)} unused changed entries need no case")
    if not used:
        return 0
    print()
    print("next: edit the cases above, then run:")
    print("  engine-contract/tools/run.sh run-contract-gate")
    return 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    version_parser = subparsers.add_parser("shim-version")
    version_parser.add_argument("--qjs-dir", required=True)
    version_parser.set_defaults(func=shim_version_command)

    check_parser = subparsers.add_parser("check")
    check_parser.add_argument("--qjs-dir", required=True)
    check_parser.add_argument("--version", required=True)
    check_parser.set_defaults(func=check)

    apply_parser = subparsers.add_parser("apply")
    apply_parser.add_argument("--servo-root", required=True)
    apply_parser.add_argument("--qjs-dir", required=True)
    apply_parser.add_argument("--version", required=True)
    apply_parser.add_argument("--skip-cargo-update", action="store_true")

    apply_parser.set_defaults(func=apply)

    report_parser = subparsers.add_parser("report")
    report_parser.add_argument("--servo-root", required=True)
    report_parser.add_argument("--qjs-dir", required=True)
    report_parser.add_argument("--out-dir", required=True)
    report_parser.set_defaults(func=report)

    args = parser.parse_args(argv[1:])
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
