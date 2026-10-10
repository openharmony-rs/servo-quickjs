#!/usr/bin/env python3
"""Turn bindgen output for SpiderMonkey into a link-free Rust module.

Every `extern "C"` function becomes a Rust function whose body is `qjs_todo!(...)`, so that the
mozjs API type-checks without SpiderMonkey. Functions are later replaced by QuickJS
implementations, see `IMPLEMENTED`. Variadic functions stay `extern` declarations, statics get a
zeroed value.

Usage: stub_bindings.py <bindgen.rs> <out.rs> [<engine-src-dir>]
"""

import os
import re
import sys


def skip_string(text, i):
    """Return the index just past the string literal starting at text[i] == '"'."""
    i += 1
    while text[i] != '"':
        i += 2 if text[i] == "\\" else 1
    return i + 1


def find_block_end(text, start):
    """Index of the '}' closing the '{' at text[start]."""
    depth = 0
    i = start
    while True:
        c = text[i]
        if c == '"':
            i = skip_string(text, i)
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1


def split_items(body):
    """Split an extern block body into items ending with ';' at nesting depth 0."""
    items, depth, cur, i = [], 0, [], 0
    while i < len(body):
        c = body[i]
        if c == '"':
            j = skip_string(body, i)
            cur.append(body[i:j])
            i = j
            continue
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        cur.append(c)
        if c == ";" and depth == 0:
            items.append("".join(cur))
            cur = []
        i += 1
    rest = "".join(cur).strip()
    assert not rest, rest[:200]
    return items


def param_names(params):
    names, depth, cur = [], 0, ""
    for c in params + ",":
        if c in "([<{":
            depth += 1
        elif c in ")]>}":
            depth -= 1
        if c == "," and depth == 0:
            if cur.strip():
                names.append(cur.split(":", 1)[0].strip())
            cur = ""
        else:
            cur += c
    return names


FN = re.compile(r"(?s)^(?P<attrs>.*?)(?P<vis>pub )fn (?P<name>\w+)\s*\((?P<params>.*)\)(?P<ret>\s*->\s*.*?)?;\s*$")
STATIC = re.compile(r"(?s)^(?P<attrs>.*?)pub static (?P<mut>mut )?(?P<name>\w+):\s*(?P<ty>.*?);\s*$")
def handle_value(tag, bits):
    return (
        "root::JS::Handle { _phantom_0: ::std::marker::PhantomData, "
        f"ptr: &root::JS::Value {{ asBits_: {bits}, tag: {tag} }} as *const root::JS::Value }}"
    )


# Statics whose zeroed value would be wrong; values use QuickJS tags (null 2, undefined 3, bool 1).
STATIC_INIT = {
    "NullHandleValue": handle_value(2, 0),
    "UndefinedHandleValue": handle_value(3, 0),
    "TrueHandleValue": handle_value(1, 1),
    "FalseHandleValue": handle_value(1, 0),
}

# Built with `QJS_POOL_STATS=1`: each forwarding stub records which API function is running, and
# whether it returns a raw GC pointer (see `src/engine/pool_stats.rs`).
POOL_STATS = os.environ.get("QJS_POOL_STATS", "") not in ("", "0")
RAW_GC_POINTER = re.compile(r"->\s*\*mut\s+root::(JSObject|JSString|JSLinearString|JSFunction|JSScript|JS::BigInt|JS::Symbol)\b")
LINK_NAME = re.compile(r'#\[link_name = "(?:[^"\\]|\\.)*"\]\s*')


def convert_item(item, indent, implemented, stats):
    text = item.strip()
    m = FN.match(text)
    if m:
        params = m.group("params")
        if re.search(r"(^|,)\s*\.\.\.\s*$", params):
            stats["variadic"] += 1
            return None
        attrs = LINK_NAME.sub("", m.group("attrs"))
        ret = m.group("ret") or ""
        name = m.group("name")
        stats["fn"] += 1
        # A name in both jsapi and glue (NewProxyObject) forwards only where the arity matches.
        if name in implemented and len(param_names(params)) in implemented[name]:
            args = ", ".join(param_names(params))
            body = f"crate::engine::{name}({args})"
            if POOL_STATS:
                raw = "true" if RAW_GC_POINTER.search(ret) else "false"
                body = f"let _api = crate::engine::pool_stats::enter(\"{name}\", {raw}); {body}"
            return f"{indent}{attrs.strip()}\n{indent}pub unsafe extern \"C\" fn {name}({params}){ret} {{ {body} }}\n"
        return f"{indent}{attrs.strip()}\n{indent}pub unsafe extern \"C\" fn {name}({params}){ret} {{ qjs_todo!(\"{name}\") }}\n"
    m = STATIC.match(text)
    if m:
        stats["static"] += 1
        attrs = LINK_NAME.sub("", m.group("attrs"))
        # Raw pointers are never `Sync`; everything else gets an `unsafe impl Sync` in `sys`.
        mut = "mut " if m.group("mut") or m.group("ty").lstrip().startswith("*") else ""
        name, ty = m.group("name"), m.group("ty")
        init = STATIC_INIT.get(name, "unsafe { ::core::mem::zeroed() }")
        if mut:
            return f"{indent}{attrs.strip()}\n{indent}pub static {mut}{name}: {ty} = {init};\n"
        # Exported under a private symbol and declared `extern` again, so that reading it needs
        # `unsafe` as with SpiderMonkey.
        symbol = f"mozjs_qjs_{stats['static']}_{name}"
        return (
            f"{indent}#[unsafe(export_name = \"{symbol}\")]\n"
            f"{indent}static __qjs_{name}: {ty} = {init};\n"
            f"{indent}unsafe extern \"C\" {{\n"
            f"{indent}    {attrs.strip()}\n"
            f"{indent}    #[link_name = \"{symbol}\"]\n"
            f"{indent}    pub static {name}: {ty};\n"
            f"{indent}}}\n"
        )
    raise ValueError("unrecognised extern item: " + text[:300])


def convert(text, implemented):
    stats = {"fn": 0, "static": 0, "variadic": 0}
    out = []
    pos = 0
    pat = re.compile(r'(?m)^(?P<indent>[ \t]*)(?:unsafe )?extern "C" \{')
    while True:
        m = pat.search(text, pos)
        if not m:
            out.append(text[pos:])
            break
        out.append(text[pos : m.start()])
        brace = m.end() - 1
        end = find_block_end(text, brace)
        body = text[brace + 1 : end]
        indent = m.group("indent")
        kept_extern = []
        for item in split_items(body):
            converted = convert_item(item, indent, implemented, stats)
            if converted is None:
                kept_extern.append(item)
            else:
                out.append(converted)
        if kept_extern:
            out.append(f'{indent}unsafe extern "C" {{' + "".join(kept_extern) + f"\n{indent}}}")
        pos = end + 1
    return "".join(out), stats


def main():
    src, dst = sys.argv[1], sys.argv[2]
    implemented = {}
    if len(sys.argv) > 3:
        # Functions implemented in the shim's engine module: `pub(crate) unsafe fn NAME(`, with
        # their arities.
        import glob
        for path in glob.glob(sys.argv[3] + "/**/*.rs", recursive=True):
            with open(path) as f:
                text = f.read()
            for m in re.finditer(r"pub\(crate\) unsafe fn (\w+)\(", text):
                depth, i = 1, m.end()
                while depth:
                    depth += {"(": 1, ")": -1}.get(text[i], 0)
                    i += 1
                arity = len(param_names(text[m.end() : i - 1]))
                implemented.setdefault(m.group(1), set()).add(arity)
    with open(src) as f:
        text = f.read()
    # Layout assertions encode SpiderMonkey's sizes, which the shim changes (e.g. JS::Value).
    text = re.sub(
        r"#\[allow\(clippy::unnecessary_operation, clippy::identity_op\)\]\s*const _: \(\) = \{.*?\n\s*\};\n",
        "",
        text,
        flags=re.S,
    )
    # JS::Value uses QuickJS's 64-bit struct representation: payload bits plus a tag.
    text, n = re.subn(
        r"pub struct Value \{\s*pub asBits_: u64,\s*\}",
        "pub struct Value {\n            pub asBits_: u64,\n            pub tag: i64,\n        }",
        text,
    )
    assert n <= 1
    result, stats = convert(text, implemented)
    # mozjs_sys modules live under `crate::sys` in the shim.
    result = re.sub(r"\bcrate::(jsgc|jsval|jsid|glue|trace|jsapi)\b", r"crate::sys::\1", result)
    with open(dst, "w") as f:
        f.write(result)
    print(f"{dst}: {stats}", file=sys.stderr)


if __name__ == "__main__":
    main()
