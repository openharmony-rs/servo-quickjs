#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Generate or check the mozjs C API snapshot used by the engine contract suite.

The snapshot is derived from the same bindgen output and the same name/arity rule as
servo-quickjs/mozjs/stub_bindings.py. It deliberately does not try to implement that rule a second
time: when a servo-quickjs checkout is supplied, its parser is imported and reused.

Usage:
  api_inventory.py generate --bindgen-dir DIR --engine-src DIR --output FILE \
      [--stub-bindings FILE]
  api_inventory.py verify --snapshot FILE --bindgen-dir DIR --engine-src DIR \
      [--stub-bindings FILE]
  api_inventory.py report --baseline FILE --snapshot FILE --output FILE [--format toml|tsv]
  api_inventory.py versions --qjs-dir DIR [--servo-root DIR] [--snapshot FILE] \
      [--require-pinned-qjs]

``--inventory`` is accepted as a compatibility alias for ``--snapshot``.

``--engine-src`` normally points at ``servo-quickjs/mozjs/src/engine``. A function whose body is
empty, only ignores arguments, or only writes a constant to a scalar out parameter is classified
as ``empty`` because the generated forwarding stub has no observable engine semantics to
exercise. Everything else present in the engine tree is ``implemented``; everything else is
``todo``.
"""

import argparse
import glob
import hashlib
import importlib.util
import os
import re
import subprocess
import sys
import tomllib


TODO_REASON = "qjs_todo!: calling this entry panics until servo-quickjs implements it"
EMPTY_REASON = "silent no-op: the shim has no observable engine semantics for this entry"
CONTRACT_MANIFEST = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "Cargo.toml"
)


def load_stub_module(path):
    spec = importlib.util.spec_from_file_location("servo_quickjs_stub_bindings", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def read_text(path):
    with open(path, encoding="utf-8") as file:
        return file.read()


def find_matching(text, index, opening, closing):
    depth = 0
    while index < len(text):
        char = text[index]
        if char == '"':
            index = skip_string(text, index)
            continue
        if char == opening:
            depth += 1
        elif char == closing:
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return -1


def skip_string(text, index):
    index += 1
    while index < len(text) and text[index] != '"':
        index += 2 if text[index] == "\\" else 1
    return index + 1


CONSTANT = r"(true|false|0|\(\)|None|::std::ptr::null_mut\(\)|::core::ptr::null_mut\(\)|ptr::null_mut\(\))"
SCALAR = r"(?:bool|u8|u16|u32|u64|usize|i8|i16|i32|i64|isize|f32|f64)"
SCALAR_OUT_PARAM = re.compile(r"\*\s*mut\s+" + SCALAR + r"\s*$")


def strip_comments(text):
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def is_empty_body(body, out_params=()):
    normalized = re.sub(r"\s+", " ", strip_comments(body)).strip()
    statements = [part.strip() for part in re.findall(r"[^;]+;", normalized)]
    rest = re.sub(r"[^;]+;", "", normalized).strip()
    for statement in statements:
        if re.fullmatch(r"let _ = .*;", statement):
            continue
        match = re.fullmatch(r"\*\s*(\w+)\s*=\s*" + CONSTANT + r";", statement)
        if match and match.group(1) in out_params:
            continue
        return False
    return rest == "" or re.fullmatch(CONSTANT + r"( as .+)?", rest) is not None


def split_params(params):
    """Split a Rust parameter list at top-level commas."""
    result = []
    depth = 0
    current = []
    for char in params + ",":
        if char in "([<{":
            depth += 1
        elif char in ")]>}":
            depth -= 1
        if char == "," and depth == 0:
            parameter = "".join(current).strip()
            if parameter:
                result.append(parameter)
            current = []
        else:
            current.append(char)
    return result


def engine_functions(engine_src):
    functions = {}
    for path in sorted(glob.glob(os.path.join(engine_src, "**", "*.rs"), recursive=True)):
        text = read_text(path)
        for match in re.finditer(r"pub\(crate\) unsafe fn (\w+)\(", text):
            name = match.group(1)
            params_end = find_matching(text, match.end() - 1, "(", ")")
            if params_end < 0:
                continue
            parameters = split_params(text[match.end() : params_end])
            arity = len(parameters)
            out_params = set()
            for parameter in parameters:
                parameter_name, separator, parameter_type = parameter.partition(":")
                if separator and SCALAR_OUT_PARAM.fullmatch(parameter_type.strip()):
                    out_params.add(parameter_name.strip())
            body_start = params_end + 1
            while body_start < len(text) and text[body_start] not in "{;":
                body_start += 1
            empty = False
            if body_start < len(text) and text[body_start] == "{":
                body_end = find_matching(text, body_start, "{", "}")
                if body_end >= 0:
                    empty = is_empty_body(text[body_start + 1 : body_end], out_params)
            line = text.count("\n", 0, match.start()) + 1
            functions.setdefault((name, arity), []).append(
                {
                    "file": os.path.relpath(path, os.path.dirname(engine_src)),
                    "line": line,
                    "empty": empty,
                }
            )
    return functions


def bindgen_entries(bindgen_dir, stub):
    entries = []
    for source in ("jsapi", "gluebindings"):
        path = os.path.join(bindgen_dir, source + ".rs")
        text = read_text(path)
        text = re.sub(
            r"#\[allow\(clippy::unnecessary_operation, clippy::identity_op\)\]\s*const _: \(\) = \{.*?\n\s*\};\n",
            "",
            text,
            flags=re.S,
        )
        text = re.sub(
            r"pub struct Value \{\s*pub asBits_: u64,\s*\}",
            "pub struct Value { pub asBits_: u64, pub tag: i64 }",
            text,
        )
        pos = 0
        block = re.compile(r'(?m)^(?P<indent>[ \t]*)(?:unsafe )?extern "C" \{')
        while True:
            match = block.search(text, pos)
            if not match:
                break
            brace = match.end() - 1
            end = stub.find_block_end(text, brace)
            for item in stub.split_items(text[brace + 1 : end]):
                function = stub.FN.match(item.strip())
                if function is None:
                    continue
                params = function.group("params")
                if re.search(r"(^|,)\s*\.\.\.\s*$", params):
                    continue
                parameters = split_params(params)
                parameter_types = []
                for parameter in parameters:
                    _, _, parameter_type = parameter.partition(":")
                    parameter_types.append(" ".join(parameter_type.split()))
                return_type = (function.group("ret") or "").strip()
                return_type = return_type.removeprefix("->").strip() or "()"
                signature = f"({', '.join(parameter_types)}) -> {return_type}"
                entries.append(
                    {
                        "name": function.group("name"),
                        "arity": len(parameters),
                        "source": source,
                        "signature": signature,
                    }
                )
            pos = end + 1
    return entries


def quote(value):
    return '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'


def write_inventory(path, entries, metadata):
    lines = ["[manifest]"]
    for key in (
        "mozjs_version",
        "bindgen_sha256",
        "bindgen_definitions",
        "unique_names",
        "variadic",
        "implemented",
        "empty",
        "todo",
        "eligible",
    ):
        value = metadata[key]
        if isinstance(value, int):
            lines.append(f"{key} = {value}")
        else:
            lines.append(f"{key} = {quote(value)}")
    lines.append("")
    for entry in entries:
        lines.append("[[api]]")
        lines.append(f"name = {quote(entry['name'])}")
        lines.append(f"arity = {entry['arity']}")
        lines.append(f"source = {quote(entry['source'])}")
        lines.append(f"status = {quote(entry['status'])}")
        lines.append(f"signature = {quote(entry['signature'])}")
        if entry.get("file"):
            lines.append(f"file = {quote(entry['file'])}")
        if entry.get("line"):
            lines.append(f"line = {entry['line']}")
        if entry.get("reason"):
            lines.append(f"reason = {quote(entry['reason'])}")
        lines.append("")
    with open(path, "w", encoding="utf-8") as file:
        file.write("\n".join(lines))


def generate_entries(bindgen_dir, engine_src, stub):
    definitions = bindgen_entries(bindgen_dir, stub)
    functions = engine_functions(engine_src)

    entries = []
    for definition in definitions:
        function = functions.get((definition["name"], definition["arity"]))
        entry = dict(definition)
        if function:
            location = function[0]
            entry["file"] = location["file"]
            entry["line"] = location["line"]
            if location["empty"]:
                entry["status"] = "empty"
                entry["reason"] = EMPTY_REASON
            else:
                entry["status"] = "implemented"
        else:
            entry["status"] = "todo"
            entry["reason"] = TODO_REASON
        entries.append(entry)

    entries.sort(key=lambda entry: (entry["name"], entry["arity"], entry["source"]))
    counts = {
        "bindgen_definitions": len(entries),
        "unique_names": len({entry["name"] for entry in entries}),
        "variadic": 0,
        "implemented": sum(entry["status"] == "implemented" for entry in entries),
        "empty": sum(entry["status"] == "empty" for entry in entries),
        "todo": sum(entry["status"] == "todo" for entry in entries),
    }
    counts["eligible"] = counts["implemented"]

    # Variadic declarations are not converted into stubs by stub_bindings.py, but they are part
    # of the C surface. Count them from the source for the record only.
    for source in ("jsapi", "gluebindings"):
        text = read_text(os.path.join(bindgen_dir, source + ".rs"))
        pos = 0
        block = re.compile(r'(?m)^(?P<indent>[ \t]*)(?:unsafe )?extern "C" \{')
        while True:
            match = block.search(text, pos)
            if not match:
                break
            brace = match.end() - 1
            end = stub.find_block_end(text, brace)
            for item in stub.split_items(text[brace + 1 : end]):
                function = stub.FN.match(item.strip())
                if function and re.search(r"(^|,)\s*\.\.\.\s*$", function.group("params")):
                    counts["variadic"] += 1
            pos = end + 1

    return entries, counts


def generate(args):
    stub = load_stub_module(args.stub_bindings)
    bindgen_dir = args.bindgen_dir
    entries, counts = generate_entries(bindgen_dir, args.engine_src, stub)

    digest = hashlib.sha256()
    for source in ("jsapi", "gluebindings"):
        digest.update(sha256(os.path.join(bindgen_dir, source + ".rs")).encode())
    metadata = {
        "mozjs_version": args.mozjs_version,
        "bindgen_sha256": digest.hexdigest(),
        **counts,
    }
    write_inventory(args.output, entries, metadata)
    print(
        f"wrote {args.output}: {counts['bindgen_definitions']} definitions, "
        f"{counts['eligible']} eligible, {counts['empty']} empty, {counts['todo']} todo, "
        f"{counts['variadic']} variadic"
    )


def load_inventory(path):
    with open(path, "rb") as file:
        document = tomllib.load(file)
    metadata = document.get("manifest")
    entries = document.get("api", [])
    if not metadata or not entries:
        raise SystemExit(f"{path}: inventory is missing [manifest] or [[api]]")
    missing_signatures = [entry for entry in entries if "signature" not in entry]
    if missing_signatures:
        raise SystemExit(
            f"{path}: inventory has no signatures; regenerate it with api_inventory.py generate"
        )
    return metadata, entries


def inventory(entries):
    """The part of the inventory that coverage depends on: which entries exist and how they are
    classified. Line numbers and reasons may move without changing coverage."""
    return {
        (entry["name"], entry["arity"], entry["source"]): (
            entry.get("signature", ""),
            entry["status"],
        )
        for entry in entries
    }


def inventory_problems(metadata, entries):
    """The structural checks `verify` runs before comparing inventories.

    `verify` only compares the entry set, which is a dict keyed by name/arity/source: a duplicated
    `[[api]]` block, a missing `reason`, or a stale summary count all compare as unchanged. Run
    these checks first so a snapshot that is "fresh" but malformed cannot pass the gate.
    """
    problems = []
    ids = set()
    for entry in entries:
        key = (entry["name"], entry["arity"], entry["source"])
        if key in ids:
            problems.append(f"duplicate entry {key}")
        ids.add(key)
        status = entry["status"]
        if status not in ("implemented", "empty", "todo"):
            problems.append(f"{key}: unknown status {status}")
        if status in ("empty", "todo") and not entry.get("reason"):
            problems.append(f"{key}: excluded entry lacks a reason")
    counts = {
        "bindgen_definitions": len(entries),
        "unique_names": len({entry["name"] for entry in entries}),
        "implemented": sum(entry["status"] == "implemented" for entry in entries),
        "empty": sum(entry["status"] == "empty" for entry in entries),
        "todo": sum(entry["status"] == "todo" for entry in entries),
    }
    for key, value in counts.items():
        if metadata.get(key) != value:
            problems.append(f"{key}: inventory says {metadata.get(key)}, entries say {value}")
    if metadata.get("eligible") != counts["implemented"]:
        problems.append("eligible must equal implemented")
    return problems, counts


def diff_inventory(recorded, generated):
    removed = sorted(set(recorded) - set(generated))
    added = sorted(set(generated) - set(recorded))
    changed = sorted(
        key for key in set(recorded) & set(generated)
        if recorded[key] != generated[key]
    )
    lines = []
    for key in added:
        lines.append(f"  + {key[0]}({key[1]}) [{key[2]}]: {generated[key][1]}")
    for key in removed:
        lines.append(f"  - {key[0]}({key[1]}) [{key[2]}]: {recorded[key][1]}")
    for key in changed:
        if recorded[key][1] != generated[key][1]:
            lines.append(
                f"  ~ {key[0]}({key[1]}) [{key[2]}]: status "
                f"{recorded[key][1]} -> {generated[key][1]}"
            )
        if recorded[key][0] != generated[key][0]:
            lines.append(f"  ~ {key[0]}({key[1]}) [{key[2]}]: signature")
            lines.append(f"      recorded:  {recorded[key][0]}")
            lines.append(f"      generated: {generated[key][0]}")
    return lines


def change_rows(baseline, current):
    """Pair entries by their stable (name, arity, source) key and describe what changed."""
    rows = []
    for key in sorted(set(baseline) | set(current)):
        before = baseline.get(key)
        after = current.get(key)
        name, arity, source = key
        if before is None:
            rows.append(("added", name, arity, source, after[1], after[0], "", after[1]))
        elif after is None:
            rows.append(("removed", name, arity, source, before[1], before[0], before[1], ""))
        elif before != after:
            old_signature, old_status = before
            new_signature, new_status = after
            if old_signature != new_signature:
                rows.append(
                    (
                        "signature",
                        name,
                        arity,
                        source,
                        new_status,
                        new_signature,
                        old_signature,
                        new_signature,
                    )
                )
            if old_status != new_status:
                rows.append(
                    (
                        "status",
                        name,
                        arity,
                        source,
                        new_status,
                        new_signature,
                        old_status,
                        new_status,
                    )
                )
    return rows


def write_report(path, baseline_metadata, current_metadata, rows, fmt):
    if fmt == "tsv":
        lines = ["change\tname\tarity\tsource\tstatus\tbefore\tafter\n"]
        for change, name, arity, source, status, _signature, before, after in rows:
            lines.append(
                "\t".join(
                    (change, name, str(arity), source, status, before.replace("\t", " "), after.replace("\t", " "))
                )
                + "\n"
            )
        with open(path, "w", encoding="utf-8") as file:
            file.writelines(lines)
        return

    lines = ["[manifest]"]
    for label, metadata in (("baseline", baseline_metadata), ("current", current_metadata)):
        for key in ("mozjs_version", "bindgen_sha256"):
            lines.append(f"{label}_{key} = {quote(metadata.get(key, 'unknown'))}")
    lines.append(f"changes = {len(rows)}")
    lines.append("")
    for change, name, arity, source, status, signature, before, after in rows:
        lines.append("[[change]]")
        lines.append(f"kind = {quote(change)}")
        lines.append(f"name = {quote(name)}")
        lines.append(f"arity = {arity}")
        lines.append(f"source = {quote(source)}")
        lines.append(f"status = {quote(status)}")
        lines.append(f"signature = {quote(signature)}")
        lines.append(f"before = {quote(before)}")
        lines.append(f"after = {quote(after)}")
        lines.append("")
    with open(path, "w", encoding="utf-8") as file:
        file.write("\n".join(lines))


def report(args):
    baseline_metadata, baseline_entries = load_inventory(args.baseline)
    current_metadata, current_entries = load_inventory(args.snapshot)
    rows = change_rows(inventory(baseline_entries), inventory(current_entries))
    os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
    write_report(args.output, baseline_metadata, current_metadata, rows, args.format)
    counts = {}
    for change, *_rest in rows:
        counts[change] = counts.get(change, 0) + 1
    summary = ", ".join(f"{kind} {counts[kind]}" for kind in sorted(counts)) or "no changes"
    print(f"wrote {args.output}: {len(rows)} changes ({summary})")
    return 0


def read_toml(path):
    with open(path, "rb") as file:
        return tomllib.load(file)


def servo_mozjs_version(servo_root):
    """Return the exact `mozjs` version that Servo's workspace requires."""
    path = os.path.join(servo_root, "Cargo.toml")
    if not os.path.isfile(path):
        raise SystemExit(f"{path}: not found; --servo-root must point at a Servo checkout")
    dependency = read_toml(path).get("workspace", {}).get("dependencies", {}).get("js")
    if not isinstance(dependency, dict):
        raise SystemExit(f"{path}: [workspace.dependencies] has no `js` entry")
    if dependency.get("package") != "mozjs":
        raise SystemExit(
            f"{path}: the `js` dependency must select `package = \"mozjs\"`, "
            f"not {dependency.get('package')!r}"
        )
    requirement = dependency.get("version")
    if not isinstance(requirement, str) or not requirement.startswith("="):
        raise SystemExit(
            f"{path}: the `js` dependency must use an exact pin such as \"=0.26.14\", "
            f"not {requirement!r}; a caret requirement lets `[patch.crates-io]` stop applying"
        )
    return requirement[1:]


def shim_mozjs_version(qjs_dir):
    path = os.path.join(qjs_dir, "mozjs", "Cargo.toml")
    if not os.path.isfile(path):
        raise SystemExit(
            f"{path}: not found; --qjs-dir must point at a servo-quickjs checkout that "
            f"includes mozjs/Cargo.toml (a sparse CI checkout must list it)"
        )
    version = read_toml(path).get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise SystemExit(f"{path}: [package] has no version")
    return version


def contract_mozjs_version(path=CONTRACT_MANIFEST):
    """Return the mozjs version this suite builds, used when no Servo checkout is supplied."""
    if not os.path.isfile(path):
        # The run.sh tool tests copy only the tools; there is no crate manifest to read.
        return "unknown"
    dependency = read_toml(path).get("dependencies", {}).get("js")
    if not isinstance(dependency, dict) or dependency.get("package") != "mozjs":
        raise SystemExit(f"{path}: [dependencies] has no `js = {{ package = \"mozjs\" }}` entry")
    requirement = dependency.get("version")
    if not isinstance(requirement, str) or not requirement.startswith("="):
        raise SystemExit(f"{path}: the `js` dependency must use an exact pin such as \"=0.26.14\"")
    return requirement[1:]


def pinned_quickjs_revision(servo_root):
    """Return the servo-quickjs commit that `mach build --js-engine quickjs` patches in."""
    path = os.path.join(servo_root, "support", "quickjs", "cargo-config.toml")
    if not os.path.isfile(path):
        raise SystemExit(f"{path}: not found; --servo-root must point at a Servo checkout")
    revision = (
        read_toml(path).get("patch", {}).get("crates-io", {}).get("mozjs", {}).get("rev")
    )
    if not isinstance(revision, str) or not revision:
        raise SystemExit(f"{path}: [patch.crates-io] has no `mozjs` rev")
    return revision


def checkout_revision(qjs_dir):
    """Return the full commit of the checkout, or None when it is not a git repository."""
    try:
        completed = subprocess.run(
            ["git", "-C", qjs_dir, "rev-parse", "HEAD"],
            text=True,
            capture_output=True,
        )
    except OSError:
        return None
    revision = completed.stdout.strip()
    return revision if completed.returncode == 0 and revision else None


def versions(args):
    """Check that the mozjs version and the QuickJS pin agree across every place.

    Without a Servo checkout the suite still runs standalone. The shim's version then comes from
    the checked-in contract fixture. The snapshot records the bindgen digest, not the checkout
    commit: a commit is a self-referential input (regenerating the snapshot moves HEAD), while the
    digest describes the content the inventory was generated from.
    """
    servo_version = (
        servo_mozjs_version(args.servo_root) if args.servo_root else None
    )
    shim_version = shim_mozjs_version(args.qjs_dir)
    pinned = pinned_quickjs_revision(args.servo_root) if args.servo_root else None
    head = checkout_revision(args.qjs_dir)
    problems = []
    warnings = []
    expected_version = servo_version or contract_mozjs_version()

    if expected_version != "unknown" and shim_version != expected_version:
        problems.append(
            f"servo-quickjs/mozjs/Cargo.toml is {shim_version}, but the contract suite needs "
            f"mozjs = {expected_version}"
            + (
                "; Cargo leaves the [patch.crates-io] unused, so the \"QuickJS\" run would "
                "silently build SpiderMonkey"
                if servo_version
                else "; update the suite's mozjs dependency"
            )
        )

    if args.snapshot:
        manifest = read_toml(args.snapshot).get("manifest", {})
        recorded_version = manifest.get("mozjs_version")
        if (
            expected_version != "unknown"
            and recorded_version != expected_version
        ):
            problems.append(
                f"{args.snapshot} records mozjs_version {recorded_version!r}, but the suite "
                f"needs {expected_version!r}; run scan-mozjs-surface"
            )

    if servo_version is None:
        warnings.append(
            "no Servo checkout; checked the shim version and snapshot, not Servo's QuickJS pin"
        )
        if args.require_pinned_qjs:
            problems.append(
                "--require-pinned-qjs needs a Servo checkout; pass --servo-root or unset it"
            )
    if head is None:
        warnings.append(f"{args.qjs_dir} is not a git checkout; cannot verify the pinned commit")
    elif pinned is not None and head != pinned:
        message = (
            f"{args.qjs_dir} is at {head[:7]}, but servo-ohos pins {pinned[:7]}; the contract run "
            f"describes a shim the device build does not use"
        )
        if args.require_pinned_qjs:
            problems.append(message)
        else:
            warnings.append(message)

    if servo_version is not None:
        print(f"mozjs version: Servo {servo_version}, servo-quickjs {shim_version}")
        print(f"servo-quickjs pin: {pinned[:7]}, checkout: {head[:7] if head else 'unknown'}")
    else:
        print(f"mozjs version: contract {expected_version}, servo-quickjs {shim_version}")
        print(f"servo-quickjs checkout: {head[:7] if head else 'unknown'}")
    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    if problems:
        return 1
    print("engine versions ok")
    return 0


def verify(args):
    """Fail when the checked-in inventory no longer describes the bindgen and shim inputs."""
    recorded_metadata, recorded_entries = load_inventory(args.snapshot)
    problems, _ = inventory_problems(recorded_metadata, recorded_entries)
    if problems:
        print(f"error: {args.snapshot} is structurally invalid", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    stub = load_stub_module(args.stub_bindings)
    recorded = inventory(recorded_entries)
    generated_entries = generate_entries(
        args.bindgen_dir, args.engine_src, stub
    )[0]
    generated = inventory(generated_entries)
    if recorded == generated:
        print(
            f"mozjs API inventory ok: {len(recorded)} entries match "
            f"{args.bindgen_dir} and {args.engine_src}"
        )
        return 0
    print(
        f"error: {args.snapshot} is stale for the current mozjs bindgen or QuickJS shim",
        file=sys.stderr,
    )
    for line in diff_inventory(recorded, generated):
        print(line, file=sys.stderr)
    print(
        "Regenerate the inventory, add or update contract cases, then rerun coverage.",
        file=sys.stderr,
    )
    return 1


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    def add_snapshot_argument(subparser):
        subparser.add_argument(
            "--snapshot",
            "--inventory",
            dest="snapshot",
            required=True,
            help="mozjs API snapshot to read (--inventory is a compatibility alias)",
        )

    generate_parser = subparsers.add_parser("generate")
    generate_parser.add_argument("--bindgen-dir", required=True)
    generate_parser.add_argument("--engine-src", required=True)
    generate_parser.add_argument("--stub-bindings", required=True)
    generate_parser.add_argument("--output", required=True)
    generate_parser.add_argument("--mozjs-version", default="0.26.14")
    generate_parser.set_defaults(func=generate)

    verify_parser = subparsers.add_parser("verify")
    add_snapshot_argument(verify_parser)
    verify_parser.add_argument("--bindgen-dir", required=True)
    verify_parser.add_argument("--engine-src", required=True)
    verify_parser.add_argument("--stub-bindings", required=True)
    verify_parser.set_defaults(func=verify)

    report_parser = subparsers.add_parser("report")
    report_parser.add_argument("--baseline", required=True)
    add_snapshot_argument(report_parser)
    report_parser.add_argument("--output", required=True)
    report_parser.add_argument("--format", choices=("toml", "tsv"), default="toml")
    report_parser.set_defaults(func=report)

    versions_parser = subparsers.add_parser("versions")
    versions_parser.add_argument("--servo-root")
    versions_parser.add_argument("--qjs-dir", required=True)
    versions_parser.add_argument("--snapshot")
    versions_parser.add_argument(
        "--require-pinned-qjs",
        action="store_true",
        help="fail when the checkout is not the pinned commit instead of only warning",
    )
    versions_parser.set_defaults(func=versions)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
