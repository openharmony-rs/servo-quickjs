#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Generate the Servo mozjs API usage used by the engine contract coverage gate.

The usage file contains the mozjs entries Servo production code calls directly or reaches through
the mozjs wrapper crates. It is the standard for contract coverage; the full API inventory stays
in place to detect bindgen and shim changes.

Usage:
  scope_manifest.py --servo-dir DIR --inventory mozjs-api-snapshot.toml \
      --usage servo-mozjs-api-usage.toml \
      --mozjs-root DIR --mozjs-sys-root DIR
  scope_manifest.py ... --check

The scanner is intentionally conservative. A direct call must resolve through a `js::` or
`mozjs::` import in Servo production code; indirect calls are followed through the Rust call
graph inside the `mozjs` and `mozjs_sys` crates.

Calls that reach mozjs through wrapper methods, generic `TypedArray` helpers, generated bindings
or callback registrations are not visible to that scan. They are reviewed by hand and recorded
in `servo-mozjs-api-usage-reviewed.toml`; the generated usage merges that list so a reviewed use cannot silently
drop out of coverage.
"""

import argparse
import collections
import csv
import glob
import os
import re
import subprocess
import sys
import tempfile
import tomllib


CALL = re.compile(r"\b([A-Za-z_]\w*)\s*\(")
FUNCTION_ARGUMENT = re.compile(r"(?<=[(,])\s*([A-Za-z_]\w*)\s*(?=[,)])")
IMPLICIT_DISPATCH = {"deref", "deref_mut", "drop", "trace", "post_barrier"}
# Accessors of the shim's rooted/stack-vector wrappers. Servo calls these through `Handle<..>`,
# whose concrete owner is not recoverable from the call site; every definition of the name
# forwards to the entry of the same family, so following the name stays sound.
WRAPPER_ACCESSORS = {"len", "at"}

REVIEWED_USAGE = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "servo-mozjs-api-usage-reviewed.toml"
)


def block_end(text, start):
    depth = 0
    for index in range(start, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    return len(text) - 1


def load_reviewed_usage(path=REVIEWED_USAGE):
    with open(path, "rb") as handle:
        document = tomllib.load(handle)
    reviewed = {}
    for entry in document.get("entry", []):
        name = entry["name"]
        if name in reviewed:
            raise SystemExit(f"{path}: duplicate reviewed usage {name!r}")
        if entry.get("mode") not in ("direct", "indirect"):
            raise SystemExit(f"{path}: reviewed usage {name!r} has an invalid mode")
        evidence = entry.get("evidence")
        if not evidence:
            raise SystemExit(f"{path}: reviewed usage {name!r} has no evidence")
        reviewed[name] = {"mode": entry["mode"], "evidence": f"reviewed: {evidence}"}
    return reviewed


def load_manifest(path):
    with open(path, "rb") as handle:
        document = tomllib.load(handle)
    by_name = collections.defaultdict(list)
    for entry in document["api"]:
        by_name[entry["name"]].append(entry)
    return by_name


def status_of(entries):
    statuses = {entry["status"] for entry in entries}
    return "implemented" if "implemented" in statuses else sorted(statuses)[0]


def call_path_before(text, position):
    """Return the `a::b::Name` path segments immediately before a call name."""
    start = position
    while start >= 2 and text[start - 2 : start] == "::":
        ident_end = start - 2
        ident_start = ident_end
        while ident_start > 0 and (
            text[ident_start - 1].isalnum() or text[ident_start - 1] == "_"
        ):
            ident_start -= 1
        if ident_start == ident_end:
            break
        start = ident_start
    return [part.strip() for part in text[start:position].split("::") if part.strip()]


QUICKJS_ALIASES = ("q",)


def is_quickjs_ffi(segments):
    """Whether a call goes to the raw QuickJS C API rather than through a mozjs entry.

    The shim's `engine` modules alias `servo_quickjs` as `q` and call `q::JS_DeleteProperty`,
    `q::JS_IsConstructor`, ... Those are the QuickJS natives that the mozjs entry of the same name
    wraps; they are not that entry, so following them would claim entries Servo never reaches.
    """
    return bool(segments) and segments[0] in QUICKJS_ALIASES


def function_body_start(text, position):
    """Return where a function's body starts, or None for a bodyless declaration.

    Scanning from after the name to the first `;` would misread a signature such as
    `fn new(length: [u8; 4])` as a declaration, so skip the parameter list first. The return type
    may itself contain parentheses (`-> Result<i32, ()>`), so the body brace is looked for only
    after the matching `)`.
    """
    paren = text.find("(", position)
    if paren < 0:
        return None
    depth = 0
    for index in range(paren, len(text)):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                brace = text.find("{", index + 1)
                semicolon = text.find(";", index + 1)
                if brace < 0 or (semicolon >= 0 and semicolon < brace):
                    return None
                return brace
    return None


def impl_ranges(text):
    """Return `(start, end, type name)` for every `impl` block in `text`."""
    ranges = []
    for match in re.finditer(r"\bimpl\b([^{;]*)\{", text):
        head = match.group(1)
        if " for " in head:
            head = head.rsplit(" for ", 1)[1]
        head = re.sub(r"<.*", "", head).strip()
        brace = match.end() - 1
        ranges.append((brace, block_end(text, brace), head.split("::")[-1].strip()))
    return ranges


def scan_mozjs(roots, all_entries, strip):
    """Return the call graph of the mozjs shim and the manifest entries each item reaches.

    Nodes stay keyed by function name, because many entries are reached through a wrapper that
    shares the name of a sibling helper (`Runtime::new` -> `create`). Receiver types are not
    modelled. Instead, a method call whose name is defined on more than one shim type is only
    followed for `self.method()` and for the trait methods Rust dispatches implicitly (`deref`,
    `drop`, `trace`, `post_barrier`). That keeps real edges such as
    `IdVector::deref -> SliceRootedIdVector` while refusing `OpenOptions::append(true)` ->
    `RootedObjectVectorWrapper::append` -> `AppendToRootedObjectVector`.

    Besides plain calls, two more forms forward. Functions passed as values are not calls but still
    forward, which is how `convert_int_from_jsval` obtains `ToInt32Slow`, and an entry reached
    through the raw QuickJS API (`q::Name`) is not the mozjs entry `Name`.
    """
    bodies = []
    for root in roots:
        for path in glob.glob(root + "/**/*.rs", recursive=True):
            text = strip(open(path, encoding="utf-8", errors="ignore").read())
            impls = impl_ranges(text)
            for match in re.finditer(r"\bfn\s+([A-Za-z_]\w*)", text):
                start = function_body_start(text, match.end())
                # `wrap!(jsapi: pub fn Name(...) -> T);` declares a forwarding stub, and a bare
                # `fn Name(...) -> T;` is an extern declaration: neither has a body, and treating
                # the following code as one would invent calls that never happen.
                if start is None:
                    continue
                owner = next((typ for begin, end, typ in impls if begin < start < end), None)
                bodies.append((match.group(1), owner, text[start : block_end(text, start) + 1]))
    method_owners = collections.defaultdict(set)
    for name, owner, _body in bodies:
        if owner:
            method_owners[name].add(owner)
    free_functions = {name for name, owner, _body in bodies if owner is None}

    calls = collections.defaultdict(set)
    entry_deps = collections.defaultdict(set)
    for name, owner, body in bodies:
        found = set()
        for call in CALL.finditer(body):
            position = call.start(1)
            target = call.group(1)
            if is_quickjs_ffi(call_path_before(body, position)):
                continue
            if position > 0 and body[position - 1] == ".":
                receiver = body[max(0, position - 24) : position]
                if re.search(r"\bself\s*$", receiver):
                    found.add(target)
                elif (
                    target in IMPLICIT_DISPATCH
                    or target in WRAPPER_ACCESSORS
                    or len(method_owners.get(target, ())) == 1
                ):
                    found.add(target)
                continue
            found.add(target)
        for argument in FUNCTION_ARGUMENT.finditer(body):
            # A function used as a value, e.g. `convert_from_int32::<i32>(cx, v, ToInt32Slow)`.
            # Requiring an argument position keeps struct fields (`append: [...]`) out, and
            # restricting it to free functions keeps struct literals (`append,`) out as well.
            target = argument.group(1)
            if target in free_functions or len(method_owners.get(target, ())) == 1:
                found.add(target)
        calls[name] |= found
        entry_deps[name] |= found & all_entries
    return calls, entry_deps


def rust_files(directory):
    for path in glob.glob(directory + "/**/*.rs", recursive=True):
        if "/target/" in path:
            continue
        relative = path.split("/components/", 1)[-1].split("/ports/", 1)[-1]
        if "/tests/" in relative or relative.startswith("tests/"):
            continue
        yield path


def collect_wrapper_roots(servo_dir, scan_scope, mozjs_functions, all_entries):
    records = []
    paths = list(rust_files(os.path.join(servo_dir, "components"))) + list(
        rust_files(os.path.join(servo_dir, "ports"))
    )
    for path in paths:
        records.append(
            (
                path,
                scan_scope.strip_comments_and_strings(
                    open(path, encoding="utf-8", errors="ignore").read()
                ),
            )
        )

    imported = collections.defaultdict(set)
    for path, text in records:
        for _position, target, symbol, alias, glob, _raw in scan_scope.parse_uses(text):
            if target.startswith(("js::", "mozjs::")) and not glob and symbol not in ("*", "self"):
                imported[alias].add(path)

    roots = collections.defaultdict(set)
    for path, text in records:
        relative = os.path.relpath(path, servo_dir)

        def add(name, position):
            # A manifest entry called directly from Servo is already handled by direct_scan().
            # Treating it as a wrapper root would add it to the indirect set under the same name
            # and make the direct/indirect counts overlap.
            if name in mozjs_functions and name not in all_entries:
                line = text.count("\n", 0, position) + 1
                roots[name].add(f"{relative}:{line}")

        for match in re.finditer(r"\b(?:js|mozjs)::((?:[A-Za-z_]\w*::)+)([A-Za-z_]\w*)\s*\(", text):
            add(match.group(2), match.start())
        for match in re.finditer(r"(?<![A-Za-z0-9_])([A-Za-z_]\w*)\s*\(", text):
            name = match.group(1)
            before = text[max(0, match.start(1) - 40) : match.start(1)]
            if re.search(r"\bfn\s+$", before):
                continue
            if match.start(1) > 0 and text[match.start(1) - 1] == ".":
                continue
            if name not in imported or path not in imported[name]:
                continue
            add(name, match.start(1))
        for match in re.finditer(r"\b([A-Za-z_]\w*)\s*::\s*([A-Za-z_]\w*)\s*\(", text):
            if match.group(1) in imported:
                add(match.group(2), match.start())
    return roots


def direct_scan(servo_dir, manifest_path):
    tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scan_scope.py")
    with tempfile.NamedTemporaryFile(suffix=".tsv", delete=False) as handle:
        output = handle.name
    try:
        subprocess.run(
            [
                sys.executable,
                tool,
                "scan",
                "--servo-dir",
                servo_dir,
                "--manifest",
                manifest_path,
                "--format",
                "tsv",
                "--output",
                output,
            ],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        rows = list(csv.DictReader(open(output, encoding="utf-8"), delimiter="\t"))
    finally:
        os.unlink(output)
    direct = collections.defaultdict(list)
    for row in rows:
        direct[row["api"]].append(f"{row['path']}:{row['line']}")
    return direct


def merge_reviewed_usage(direct, reached, reviewed, all_entries):
    unknown = sorted(set(reviewed) - all_entries)
    if unknown:
        raise SystemExit(f"reviewed usage not in the mozjs API inventory: {', '.join(unknown)}")
    for name, entry in reviewed.items():
        if entry["mode"] == "direct":
            direct.setdefault(name, []).append(entry["evidence"])
        else:
            reached[name].add(entry["evidence"])


def summarize(servo_dir, manifest_path, mozjs_root, mozjs_sys_root, reviewed_path=REVIEWED_USAGE):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import scan_scope

    by_name = load_manifest(manifest_path)
    all_entries = set(by_name)
    calls, entry_deps = scan_mozjs(
        [mozjs_root, mozjs_sys_root], all_entries, scan_scope.strip_comments_and_strings
    )
    mozjs_functions = set(calls) | set(entry_deps)
    roots = collect_wrapper_roots(servo_dir, scan_scope, mozjs_functions, all_entries)
    direct = direct_scan(servo_dir, manifest_path)

    # Both kinds of entry can forward to a sibling entry point, so both seed the walk. A wrapper
    # helper is called by Servo but is not itself a manifest entry; a direct entry is a manifest
    # entry Servo calls, and it can forward too (`Construct1` calls `Construct`, `NewWindowProxy`
    # calls `WrapperNew`, `ToInt32` calls `ToInt32Slow`). Seeding from the wrapper helpers alone
    # left those siblings outside the scope, which exempted them from contract cases even though
    # Servo runs them.
    seen = set()
    reached = collections.defaultdict(set)
    queue = list(roots) + list(direct)
    while queue:
        name = queue.pop()
        if name in seen:
            continue
        seen.add(name)
        for child in entry_deps.get(name, ()):
            reached[child].add(name)
        for child in calls.get(name, ()):
            if child in mozjs_functions and child not in seen:
                queue.append(child)

    reviewed = load_reviewed_usage(reviewed_path)
    merge_reviewed_usage(direct, reached, reviewed, all_entries)
    return {
        "manifest": by_name,
        "direct": direct,
        "roots": roots,
        "via": reached,
        "used": set(direct) | set(reached),
        "reviewed": reviewed,
    }
def quote(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def render_scope(result):
    by_name = result["manifest"]
    direct = result["direct"]
    reached = result["via"]
    lines = [
        "# The mozjs surface Servo actually uses.\n",
        "#\n",
        "# This file is the coverage standard for the engine contract suite. Entries outside it are\n",
        "# not required to have a contract case. Regenerate with:\n",
        "#   SERVO_DIR=/path/to/servo engine-contract/tools/run.sh scan-mozjs-surface\n",
        "\n",
        "[scope]\n",
        'owner = "servo"\n',
        "description = \"mozjs entries Servo calls directly or reaches through wrappers\"\n",
        f"entries = {len(result['used'])}\n",
    ]
    for name in sorted(result["used"]):
        entries = by_name[name]
        status = status_of(entries)
        reviewed = result["reviewed"].get(name)
        mode = reviewed["mode"] if reviewed else ("direct" if name in direct else "indirect")
        evidence = []
        if name in direct:
            evidence.extend(direct[name])
        if name in reached:
            evidence.extend(sorted(reached[name]))
        lines.append("\n[[entry]]\n")
        lines.append(f"name = {quote(name)}\n")
        lines.append(f"status = {quote(status)}\n")
        lines.append(f"mode = {quote(mode)}\n")
        lines.append("provenance = [\n")
        for item in sorted(set(evidence)):
            lines.append(f"  {quote(item)},\n")
        lines.append("]\n")
    return "".join(lines)


def load_scope_entries(path):
    with open(path, "rb") as handle:
        document = tomllib.load(handle)
    entries = {}
    for entry in document.get("entry", []):
        entries[entry["name"]] = (entry.get("status"), entry.get("mode"))
    declared = document.get("scope", {}).get("entries")
    if declared is not None and declared != len(entries):
        raise SystemExit(
            f"{path}: scope.entries says {declared}, but there are {len(entries)} entries"
        )
    return entries


def check_scope(path, result):
    if not os.path.exists(path):
        print(
            f"error: missing usage file {path}; run 'run.sh scan-mozjs-surface'",
            file=sys.stderr,
        )
        return 1
    recorded = load_scope_entries(path)
    generated = {
        name: (
            status_of(result["manifest"][name]),
            "direct" if name in result["direct"] else "indirect",
        )
        for name in result["used"]
    }
    for name, entry in result["reviewed"].items():
        expected = (status_of(result["manifest"][name]), entry["mode"])
        if name in generated and generated[name] != expected:
            generated[name] = expected
    removed = sorted(set(recorded) - set(generated))
    added = sorted(set(generated) - set(recorded))
    changed = sorted(
        name for name in set(recorded) & set(generated) if recorded[name] != generated[name]
    )
    if removed or added or changed:
        print(f"error: {path} is stale for Servo's current mozjs usage", file=sys.stderr)
        for name in added:
            print(f"  + {name}: {generated[name][0]} [{generated[name][1]}]", file=sys.stderr)
        for name in removed:
            print(f"  - {name}: {recorded[name][0]} [{recorded[name][1]}]", file=sys.stderr)
        for name in changed:
            print(
                f"  ~ {name}: {recorded[name][0]} [{recorded[name][1]}] -> "
                f"{generated[name][0]} [{generated[name][1]}]",
                file=sys.stderr,
            )
        print(
            "Regenerate with "
            "'tools/run.sh scan-mozjs-surface'.",
            file=sys.stderr,
        )
        return 1
    print(f"servo mozjs API usage ok: {len(recorded)} entries match Servo's current usage")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--servo-dir", default=".")
    parser.add_argument("--inventory", required=True)
    parser.add_argument("--usage", required=True)
    parser.add_argument("--mozjs-root", required=True)
    parser.add_argument("--mozjs-sys-root", required=True)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args(argv)

    servo_dir = os.path.abspath(args.servo_dir)
    result = summarize(
        servo_dir,
        os.path.abspath(args.inventory),
        os.path.abspath(args.mozjs_root),
        os.path.abspath(args.mozjs_sys_root),
    )
    scope_path = os.path.abspath(args.usage)
    if args.check:
        return check_scope(scope_path, result)
    with open(scope_path, "w", encoding="utf-8") as handle:
        handle.write(render_scope(result))
    rendered = load_scope_entries(scope_path)
    direct = sum(mode == "direct" for _status, mode in rendered.values())
    indirect = sum(mode == "indirect" for _status, mode in rendered.values())
    print(
        f"wrote {args.usage}: {len(result['used'])} entries "
        f"({direct} direct, {indirect} indirect)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
