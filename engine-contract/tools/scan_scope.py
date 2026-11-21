#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Find mozjs entry points that Servo production code actually calls.

The scanner is intentionally conservative: an entry is reported only when a real call site
resolves to a `js::` / `mozjs::` import. Comments, string literals, macro definitions that are
not expanded, local functions with the same name, and paths through Servo's own wrappers are not
reported. Re-exports inside Servo are followed, so `crate::import::module::Call` resolves to
`js::rust::wrappers2::Call` when `import.rs` re-exports it.

Usage:
  scan_scope.py scan --servo-dir DIR --manifest FILE [--format jsonl|tsv] [--output FILE]

`scan` prints one record per API and provenance pair. Each record contains the API name, its
manifest status, and every file/line where the API is called, together with the call text. That
is the human review surface: a scope entry is only trustworthy when its provenance can be read
back as an actual call.

This module is the scanner behind `scope_manifest.py`, which owns `servo-mozjs-api-usage.toml`:
it merges the reviewed usage list, classifies `direct`/`indirect`, and checks the file for
staleness. Do not generate the scope file from this module; its output would omit the reviewed
entries and use a different schema.
"""

import argparse
import json
import os
import re
import sys
import tomllib


# Modules whose public items are the mozjs C surface. This is the allow-list that keeps the
# scanner from counting unrelated Rust identifiers that happen to share a name with a C entry.
JS_ROOTS = (
    "js::jsapi",
    "js::glue",
    "js::rust::wrappers2",
    "js::rust",
    "js::gc",
    "js::typedarray",
    "mozjs::jsapi",
    "mozjs::glue",
    "mozjs::rust::wrappers2",
    "mozjs::rust",
    "mozjs::gc",
    "mozjs::typedarray",
)

IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
PATH = rf"{IDENT}(?:::{IDENT})*"


def blank(text, start, end):
    """Replace a range with spaces, preserving newlines and offsets."""
    chunk = text[start:end]
    return text[:start] + re.sub(r"[^\n]", " ", chunk) + text[end:]


def strip_comments_and_strings(text):
    """Blank comments and string/char literals so brace and call matching is not confused."""
    out = text
    i = 0
    length = len(out)
    while i < length:
        char = out[i]
        if char == "/" and i + 1 < length and out[i + 1] == "/":
            end = out.find("\n", i)
            end = length if end < 0 else end
            out = blank(out, i, end)
            i = end
        elif char == "/" and i + 1 < length and out[i + 1] == "*":
            end = out.find("*/", i + 2)
            end = length if end < 0 else end + 2
            out = blank(out, i, end)
            i = end
        elif char == "r" and i + 1 < length and out[i + 1] in '#"':
            j = i + 1
            hashes = 0
            while j < length and out[j] == "#":
                hashes += 1
                j += 1
            if j < length and out[j] == '"':
                closing = '"' + "#" * hashes
                end = out.find(closing, j + 1)
                end = length if end < 0 else end + len(closing)
                out = blank(out, i, end)
                i = end
            else:
                i += 1
        elif char == "b" and i + 1 < length and out[i + 1] in '#"':
            j = i + 1
            hashes = 0
            while j < length and out[j] == "#":
                hashes += 1
                j += 1
            if j < length and out[j] == '"':
                closing = '"' + "#" * hashes
                end = out.find(closing, j + 1)
                end = length if end < 0 else end + len(closing)
                out = blank(out, i, end)
                i = end
            else:
                i += 1
        elif char == "c" and i + 1 < length and out[i + 1] in 'r#"':
            # C string literal (`c"..."` or `cr"..."`).
            j = i + 1
            if j < length and out[j] == "r":
                j += 1
            hashes = 0
            while j < length and out[j] == "#":
                hashes += 1
                j += 1
            if j < length and out[j] == '"':
                closing = '"' + "#" * hashes
                end = out.find(closing, j + 1)
                end = length if end < 0 else end + len(closing)
                out = blank(out, i, end)
                i = end
            else:
                i += 1
        elif char == '"':
            j = i + 1
            while j < length:
                if out[j] == "\\":
                    j += 2
                    continue
                if out[j] == '"':
                    j += 1
                    break
                j += 1
            out = blank(out, i, j)
            i = j
        elif char == "'":
            # Lifetimes (`'a`) are not literals; char literals close on the same line.
            if i + 2 < length and (out[i + 2] == "'" or (out[i + 1] == "\\" and i + 3 < length and out[i + 3] == "'")):
                end = out.find("'", i + 2)
                end = length if end < 0 else end + 1
                out = blank(out, i, end)
                i = end
            else:
                i += 1
        else:
            i += 1
    return out


def rust_files(root, relative_to):
    for base, dirs, files in os.walk(root):
        dirs[:] = sorted(
            directory
            for directory in dirs
            if directory not in {"target", "node_modules", ".git", "__pycache__"}
        )
        for name in sorted(files):
            if name.endswith(".rs"):
                path = os.path.join(base, name)
                yield path, os.path.relpath(path, relative_to)


def module_path_for(relative_path, crate_root):
    """Map a file to its Rust module path, assuming `lib.rs` / `main.rs` are crate roots."""
    relative = os.path.relpath(relative_path, crate_root)
    parts = relative[:-3].split(os.sep)
    if parts[-1] in ("lib", "main", "mod"):
        parts = parts[:-1]
    return "crate" + "".join("::" + part for part in parts)


def module_scopes(text, file_module):
    """Return `(start, end, module_path)` intervals for every `mod NAME { ... }` block."""
    scopes = [(0, len(text), file_module)]
    pattern = re.compile(r"\bmod\s+(" + IDENT + r")\s*\{")
    for match in pattern.finditer(text):
        brace = text.find("{", match.start())
        depth = 0
        end = len(text)
        for index in range(brace, len(text)):
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
                if depth == 0:
                    end = index + 1
                    break
        parent = file_module
        for start, other_end, path in sorted(scopes, key=lambda item: item[1] - item[0]):
            if start <= match.start() < other_end and len(path) > len(parent):
                parent = path
        scopes.append((match.start(), end, parent + "::" + match.group(1)))
    return scopes


def module_at(scopes, position):
    containing = [scope for scope in scopes if scope[0] <= position < scope[1]]
    return max(containing, key=lambda scope: len(scope[2]))[2]


def module_chain(module):
    parts = module.split("::")
    return ["::".join(parts[: index + 1]) for index in range(len(parts))]


def normalize_target(target, current_module):
    """Resolve `crate::`, `self::`, and `super::` prefixes against the current module."""
    if target.startswith("crate::"):
        return target
    if target.startswith("self::"):
        return current_module + target[len("self") :]
    if target.startswith("super::"):
        parts = current_module.split("::")
        while target.startswith("super::"):
            if len(parts) > 1:
                parts.pop()
            target = target[len("super::") :]
        return "::".join(parts + ([target] if target else []))
    return current_module + "::" + target


def crate_roots(servo_dir):
    roots = {}
    for base, dirs, _ in os.walk(os.path.join(servo_dir, "components")):
        dirs[:] = sorted(d for d in dirs if d not in {"target", "benches", "tests"})
        if "Cargo.toml" in os.listdir(base):
            for root in ("lib.rs", "main.rs"):
                if os.path.exists(os.path.join(base, root)):
                    roots[base] = root
                    break
    return roots


def cfg_test_ranges(text):
    """Return ranges covered by `#[cfg(test)] mod ... { ... }` blocks."""
    ranges = []
    for match in re.finditer(r"#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+" + IDENT + r"\s*\{", text):
        start = match.start()
        brace = text.find("{", match.start())
        depth = 0
        for index in range(brace, len(text)):
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
                if depth == 0:
                    ranges.append((start, index + 1))
                    break
    return ranges


def in_ranges(position, ranges):
    return any(start <= position < end for start, end in ranges)


def enclosing_modules(text, position):
    """Best-effort module stack at `position`, ignoring function and other blocks."""
    stack = []
    index = 0
    pending_mod = None
    while index < position:
        char = text[index]
        if char == "{":
            stack.append(pending_mod)
            pending_mod = None
        elif char == "}":
            if stack:
                stack.pop()
        else:
            match = re.match(r"\bmod\s+(" + IDENT + r")\s*\{", text[index:])
            if match:
                pending_mod = match.group(1)
            elif text.startswith("mod ", index):
                pending_mod = None
        index += 1
    return [name for name in stack if name]


def split_top_level(tree):
    """Split a `use` tree on commas that are not inside `{...}`."""
    parts = []
    depth = 0
    start = 0
    for index, char in enumerate(tree):
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(tree[start:index].strip())
            start = index + 1
    parts.append(tree[start:].strip())
    return parts


def parse_use_tree(tree):
    """Yield (target, symbol, alias, glob) for one `use` tree."""
    tree = tree.strip()
    if not tree:
        return
    for part in split_top_level(tree):
        if not part:
            continue
        brace = part.find("{")
        if brace >= 0 and part.endswith("}"):
            prefix = part[:brace].rstrip(":")
            inner = part[brace + 1 : -1]
            for nested in split_top_level(inner):
                if nested == "self":
                    # `use foo::bar::{self}` is the same import as `use foo::bar;`: bind the
                    # module under its last segment so `bar::Item(..)` resolves.
                    yield (prefix, "self", prefix.split("::")[-1], False)
                    continue
                for target, symbol, alias, glob in parse_use_tree(nested):
                    yield (prefix + "::" + target if target else prefix, symbol, alias, glob)
            continue
        if part == "*":
            # `use foo::*;` imports every public item of `foo`.
            yield ("", "*", "*", True)
            continue
        if part.endswith("::*"):
            # `use foo::bar::*;` is the qualified form of the same glob import.
            base = part[:-3].rstrip(":")
            if re.fullmatch(PATH, base):
                yield (base, "*", "*", True)
                continue
        match = re.fullmatch(r"(" + PATH + r")(?:\s+as\s+(" + IDENT + r"))?", part)
        if not match:
            continue
        path, alias = match.group(1), match.group(2)
        segments = path.split("::")
        if segments[-1] == "self":
            if len(segments) == 1:
                continue
            target = "::".join(segments[:-1])
            yield (target, "self", alias or segments[-2], False)
            continue
        target = "::".join(segments[:-1])
        symbol = segments[-1]
        # `use js::jsapi as api;` renames the module itself, so later `api::Item(..)` calls
        # resolve through it. Aliasing a single item (`use js::jsapi::Foo as Bar;`) does not.
        if alias and path in JS_ROOTS:
            yield (path, "self", alias, False)
            continue
        yield (target, symbol, alias or symbol, False)


def parse_uses(text):
    uses = []
    for match in re.finditer(r"(?m)^\s*(?:pub(?:\s*\([^)]*\))?\s+)?use\s+([^;]+);", text):
        tree = match.group(1)
        inner = parse_use_tree(tree)
        for target, symbol, alias, glob in inner:
            uses.append((match.start(), target, symbol, alias, glob, match.group(0).strip()))
    return uses


def is_js_module(path):
    return any(path == root or path.startswith(root + "::") for root in JS_ROOTS)


def is_js_module_itself(path):
    """True when `path` names one of the JS modules, not an item inside it."""
    return path in JS_ROOTS


def js_api_name(path):
    """Return the mozjs entry name for a full `js::...::Name` path, or None."""
    if not is_js_module(path):
        return None
    return path.split("::")[-1]


def call_sites(text, env_for, glob_js_for, is_known_api=None):
    """Find calls that resolve through `env` (alias -> API) or direct `js::` paths."""
    sites = []
    for match in re.finditer(r"(?<![A-Za-z0-9_])(" + IDENT + r")\s*(?:::<[^;{}()]*>)?\s*\(", text):
        name = match.group(1)
        start = match.start(1)
        before = text[max(0, start - 40) : start]
        if re.search(r"\bfn\s+$", before):
            continue
        if start > 0 and text[start - 1] == ".":
            continue
        # Walk back over a `path::` prefix.
        prefix_start = start
        while prefix_start >= 2 and text[prefix_start - 2 : prefix_start] == "::":
            ident_end = prefix_start - 2
            ident_start = ident_end
            while ident_start > 0 and (text[ident_start - 1].isalnum() or text[ident_start - 1] == "_"):
                ident_start -= 1
            if ident_start == ident_end:
                break
            prefix_start = ident_start
        path = text[prefix_start:start] + name
        path = re.sub(r"\s+", "", path).lstrip("::")
        env = env_for(match.start())
        glob_js = glob_js_for(match.start())
        if "::" in path:
            api = js_api_name(path)
            if api is None and "::" not in path.split("::")[-2:]:
                pass
            if api is not None:
                sites.append((api, start, match.group(0).strip(), path))
                continue
            # Alias to a js module, for example `use js::jsapi as jsapi; jsapi::Foo(..)`.
            head = path.split("::")[0]
            target = env.get(head, {}).get("module")
            if target and is_js_module(target) and "::" in path:
                sites.append((path.split("::")[-1], start, match.group(0).strip(), path))
                continue
            continue
        entry = env.get(name)
        if entry and "api" in entry:
            sites.append((entry["api"], start, match.group(0).strip(), name))
        elif entry and "module" in entry and glob_js:
            sites.append((name, start, match.group(0).strip(), name))
        elif glob_js and is_known_api is not None and is_known_api(name):
            # A glob import brings the module's public names into scope unqualified, so a bare
            # `Item(..)` may be a JS entry. Requiring the name to exist in the manifest keeps
            # unrelated identifiers from the same module scope out of the scope file.
            sites.append((name, start, match.group(0).strip(), name))
    return sites


def load_manifest(path):
    with open(path, "rb") as file:
        document = tomllib.load(file)
    by_name = {}
    for entry in document.get("api", []):
        by_name.setdefault(entry["name"], []).append(entry)
    return by_name


def scan(servo_dir, manifest):
    roots = crate_roots(servo_dir)
    files = []
    exports = {}

    # Pass 1: read files and collect module-local direct JS imports plus re-exports.
    records = []
    for crate_root in sorted(roots):
        for path, relative in rust_files(crate_root, servo_dir):
            with open(path, encoding="utf-8") as file:
                source = file.read()
            text = strip_comments_and_strings(source)
            file_module = module_path_for(path, crate_root)
            scopes = module_scopes(text, file_module)
            tests = cfg_test_ranges(text)
            uses = parse_uses(text)
            records.append((path, relative, file_module, scopes, text, uses, tests))
            for position, target, symbol, alias, glob, _ in uses:
                if not target:
                    continue
                module = module_at(scopes, position)
                if glob:
                    # `use js::jsapi::*;` imports every public item of the module.
                    if is_js_module_itself(target):
                        exports.setdefault(module, {}).setdefault(
                            target.split("::")[-1], {"module": target}
                        )
                    continue
                if symbol == "self":
                    if is_js_module_itself(target):
                        # `use js::jsapi::{self};` binds the module under its last segment.
                        exports.setdefault(module, {})[alias] = {"module": target}
                    continue
                joined = target + "::" + symbol
                if is_js_module_itself(joined):
                    # `use js::jsapi;` / `use js::jsapi as jsapi;` import the module, not an item.
                    exports.setdefault(module, {})[alias] = {"module": joined}
                    continue
                api = js_api_name(joined)
                if api:
                    exports.setdefault(module, {})[alias] = {"api": api}

    # Resolve internal re-exports to a fixpoint. Module paths are approximate; a use is attached
    # to the enclosing `mod` block when the source has one.
    for _ in range(8):
        changed = False
        for path, relative, file_module, scopes, text, uses, tests in records:
            for position, target, symbol, alias, glob, _ in uses:
                if not target or is_js_module(target):
                    continue
                module = module_at(scopes, position)
                target_module = normalize_target(target, module).rstrip(":")
                if glob:
                    for key, value in exports.get(target_module, {}).items():
                        if exports.setdefault(module, {}).setdefault(key, value) is not value:
                            changed = True
                    continue
                if symbol == "self":
                    continue
                value = exports.get(target_module, {}).get(symbol)
                if value and exports.setdefault(module, {}).get(alias) != value:
                    exports[module][alias] = value
                    changed = True
        if not changed:
            break

    results = {}
    for path, relative, file_module, scopes, text, uses, tests in records:
        # Re-exports are keyed by the module that contains the `use`; the fixpoint above may
        # have populated those modules through nested `mod` blocks.
        glob_modules = set()
        for position, target, symbol, alias, glob, _ in uses:
            # `use js::jsapi::*;` parses as a glob with the module target; a bare `use foo::*;`
            # keeps an empty target. Both import every item of a module.
            if glob and (not target or is_js_module(target)):
                glob_modules.add(module_at(scopes, position))

        def env_for(position, scopes=scopes):
            env = {}
            module = module_at(scopes, position)
            for parent in module_chain(module):
                env.update(exports.get(parent, {}))
            return env

        def glob_js_for(position, scopes=scopes, glob_modules=glob_modules):
            return any(
                module_at(scopes, position) == module or module_at(scopes, position).startswith(module + "::")
                for module in glob_modules
            )

        for api, position, call, resolved in call_sites(
            text, env_for, glob_js_for, lambda name: name in manifest
        ):
            if in_ranges(position, tests):
                continue
            if api not in manifest:
                continue
            line = text.count("\n", 0, position) + 1
            results.setdefault(api, []).append(
                {
                    "path": relative,
                    "line": line,
                    "call": call,
                    "resolved": resolved,
                }
            )

    report = []
    for name in sorted(results):
        entries = manifest.get(name, [])
        implemented = any(entry["status"] == "implemented" for entry in entries)
        statuses = sorted({entry["status"] for entry in entries}) or ["absent"]
        provenance = sorted(results[name], key=lambda item: (item["path"], item["line"]))
        report.append(
            {
                "api": name,
                "status": "implemented" if implemented else statuses[0],
                "statuses": statuses,
                "provenance": provenance,
            }
        )
    return report


def render_jsonl(report, output):
    with open(output, "w", encoding="utf-8") as file:
        for entry in report:
            file.write(json.dumps(entry, ensure_ascii=False, sort_keys=True) + "\n")


def render_tsv(report, output):
    with open(output, "w", encoding="utf-8") as file:
        file.write("api\tstatus\tpath\tline\tcall\tresolved\n")
        for entry in report:
            for site in entry["provenance"]:
                file.write(
                    "\t".join(
                        [
                            entry["api"],
                            entry["status"],
                            site["path"],
                            str(site["line"]),
                            site["call"].replace("\t", " "),
                            site["resolved"],
                        ]
                    )
                    + "\n"
                )


def investigate(report):
    counts = {}
    for entry in report:
        counts[entry["status"]] = counts.get(entry["status"], 0) + 1
    print(
        f"scan: {len(report)} APIs, "
        + ", ".join(f"{status} {counts[status]}" for status in sorted(counts))
    )
    for entry in report:
        print(f"{entry['api']}\t{entry['status']}\t{len(entry['provenance'])} sites")


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    scan_parser = subparsers.add_parser("scan")
    scan_parser.add_argument("--servo-dir", required=True)
    scan_parser.add_argument("--manifest", required=True)
    scan_parser.add_argument("--format", choices=("jsonl", "tsv"), default="jsonl")
    scan_parser.add_argument("--output", default="-")
    args = parser.parse_args(argv)

    report = scan(args.servo_dir, load_manifest(args.manifest))
    if args.output == "-":
        for entry in report:
            print(json.dumps(entry, ensure_ascii=False, sort_keys=True))
    elif args.format == "jsonl":
        render_jsonl(report, args.output)
        investigate(report)
    else:
        render_tsv(report, args.output)
        investigate(report)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
