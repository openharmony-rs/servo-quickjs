#!/usr/bin/env python3
"""Generate `mozjs/src/engine/intl/patterns.rs`, the date pattern data of the Intl subset, from
an ICU data package: the OS's, on OpenHarmony 7.0 `/system/usr/icu/icudt74l.dat`, which apps map.

    hdc file recv /system/usr/icu/icudt74l.dat .
    python3 tools/gen_intl_patterns.py icudt74l.dat > mozjs/src/engine/intl/patterns.rs

Needs `icupkg` and `derb` (Debian: icu-devtools). To add a locale, add its ICU locale ID and its
likely-subtags form to LOCALES and regenerate.
"""
import os
import re
import subprocess
import sys
import tempfile

# (ICU locale ID, maximized locale ID as uloc_addLikelySubtags returns it)
LOCALES = [
    ("en_US", "en_Latn_US"),
    ("en_GB", "en_Latn_GB"),
    ("zh_Hans_CN", "zh_Hans_CN"),
    ("zh_Hant_TW", "zh_Hant_TW"),
    ("zh_Hant_HK", "zh_Hant_HK"),
    ("zh_Hant_MO", "zh_Hant_MO"),
    ("zh_Hans_SG", "zh_Hans_SG"),
    ("zh_Hans_HK", "zh_Hans_HK"),
    ("zh_Hans_MO", "zh_Hans_MO"),
    ("ja_JP", "ja_Jpan_JP"),
    ("ko_KR", "ko_Kore_KR"),
    ("de_DE", "de_Latn_DE"),
    ("fr_FR", "fr_Latn_FR"),
    ("es_ES", "es_Latn_ES"),
    ("ru_RU", "ru_Cyrl_RU"),
    ("en_CA", "en_Latn_CA"),
    ("sv_SE", "sv_Latn_SE"),
]

# UDateTimePatternField order (dtptngen.cpp CLDR_FIELD_APPEND / CLDR_FIELD_NAME)
APPEND_KEYS = ["Era", "Year", "Quarter", "Month", "Week", "*", "Day-Of-Week", "*", "*", "Day", "*",
               "Hour", "Minute", "Second", "*", "Timezone"]
NAME_KEYS = ["era", "year", "quarter", "month", "week", "weekOfMonth", "weekday", "dayOfYear",
             "weekdayOfMonth", "day", "dayperiod", "hour", "minute", "second", "*", "zone"]

SEP = "\x01"

# Pattern characters of fields no ECMA-402 option asks for: availableFormats entries with them can
# never be the best match, as each costs a whole extra field.
UNUSED_FIELDS = set("QqwWYuUrDFg")

TOKEN = re.compile(r'\s*(?:(//[^\n]*)|(/\*.*?\*/)|("(?:[^"\\]|\\.)*")|([{}])|(,)|([^\s{}",]+))', re.S)


def tokens(s):
    pos = 0
    while pos < len(s):
        m = TOKEN.match(s, pos)
        if not m or m.end() == pos:
            if s[pos:].strip():
                raise ValueError("cannot parse %r" % s[pos:pos + 40])
            return
        pos = m.end()
        if m.group(3):
            yield ("str", re.sub(r'\\u([0-9a-fA-F]{4})|\\(.)',
                                 lambda e: chr(int(e.group(1), 16)) if e.group(1) else e.group(2),
                                 m.group(3)[1:-1]))
        elif m.group(4):
            yield (m.group(4), None)
        elif m.group(6):
            yield ("word", m.group(6))


def parse(text):
    """Parses derb's text output: tables become dicts, arrays lists, strings str."""
    toks = list(tokens(text.lstrip("﻿")))
    i = 0

    def value():
        nonlocal i
        assert toks[i][0] == "{", toks[i]
        i += 1
        if toks[i][0] == "}":
            i += 1
            return {}
        if (toks[i][0] == "str" and toks[i + 1][0] != "{") or toks[i][0] == "{" or \
                (toks[i][0] == "word" and toks[i + 1][0] != "{"):
            items = []
            while toks[i][0] != "}":
                if toks[i][0] in ("str", "word"):
                    items.append(toks[i][1])
                    i += 1
                else:
                    items.append(value())
            i += 1
            return items[0] if len(items) == 1 and isinstance(items[0], str) else items
        table = {}
        while toks[i][0] != "}":
            key = toks[i][1]
            i += 1
            table[key.split(":")[0]] = value()
        i += 1
        return table

    i = 1
    return value()


def get(tree, path):
    for part in path.split("/"):
        if not isinstance(tree, dict) or part not in tree:
            return None
        tree = tree[part]
    return tree


class Data:
    def __init__(self, dat, tmp):
        self.dat = dat
        self.tmp = tmp
        self.bundles = {}
        # Locale bundles share their keys through the pool bundle.
        subprocess.run(["icupkg", "-x", "pool.res", dat, "-d", tmp], check=True)

    def load(self, name):
        if name not in self.bundles:
            res = name + ".res"
            if subprocess.run(["icupkg", "-x", res, self.dat, "-d", self.tmp],
                              capture_output=True).returncode != 0 or \
                    not os.path.exists(os.path.join(self.tmp, res)):
                self.bundles[name] = None
            else:
                subprocess.run(["derb", "-d", ".", "./" + res], cwd=self.tmp, capture_output=True)
                with open(os.path.join(self.tmp, name + ".txt"), encoding="utf-8") as f:
                    self.bundles[name] = parse(f.read())
        return self.bundles[name]

    def chain(self, locale):
        """The bundles ICU falls back through for `locale`, root last."""
        result = []
        while locale != "root":
            bundle = self.load(locale)
            if bundle is not None:
                result.append(bundle)
                parent = bundle.get("%%Parent")
            else:
                parent = None
            if parent is None:
                parent = locale.rsplit("_", 1)[0] if "_" in locale else "root"
            locale = parent
        result.append(self.load("root"))
        return result


def first(chain, path):
    for bundle in chain:
        v = get(bundle, path)
        if v is not None:
            return v
    raise KeyError(path)


def merged(chain, path):
    """First value per key along the chain, in ICU's iteration order (bundle by bundle)."""
    out = {}
    for bundle in chain:
        for key, v in (get(bundle, path) or {}).items():
            if key not in out:
                out[key] = v
    return out


def locale_data(chain):
    greg = "calendar/gregorian/"
    patterns = first(chain, greg + "DateTimePatterns")
    styles = [p if isinstance(p, str) else p[0] for p in patterns[:8]]
    # ICU 74's generator prefers the %atTime variant (full, long, medium, short).
    try:
        glue = first(chain, greg + "DateTimePatterns%atTime")
    except KeyError:
        glue = patterns[9:13]
    glue = [p if isinstance(p, str) else p[0] for p in glue]
    append = merged(chain, greg + "appendItems")
    fields = [""] * 16
    for i, key in enumerate(NAME_KEYS):
        dn = None
        for bundle in chain:
            dn = get(bundle, "fields/%s/dn" % key)
            if dn is not None:
                break
        fields[i] = dn or ""
    out = styles + glue + [append.get(k, "") if k != "*" else "" for k in APPEND_KEYS] + fields
    for key, pattern in merged(chain, greg + "availableFormats").items():
        if isinstance(pattern, str) and not set(key) & UNUSED_FIELDS:
            out += [key, pattern]
    for s in out:
        assert SEP not in s
    return SEP.join(out)


def rust_str(s):
    out = []
    for c in s:
        if c == SEP:
            out.append("\\x01")
        elif c in '"\\':
            out.append("\\" + c)
        else:
            out.append(c)
    return '"' + "".join(out) + '"'


def main():
    with tempfile.TemporaryDirectory() as tmp:
        generate(Data(sys.argv[1], tmp))


def generate(data):
    supplemental = data.load("supplementalData")
    hours = []
    for key, v in get(supplemental, "timeData").items():
        preferred = v.get("preferred") if isinstance(v, dict) else None
        if preferred:
            allowed = v.get("allowed", [])
            allowed = [allowed] if isinstance(allowed, str) else allowed
            hours.append("%s:%s" % (key, preferred[0] + "".join(a[0] for a in allowed)))
    print("/* This Source Code Form is subject to the terms of the Mozilla Public")
    print(" * License, v. 2.0. If a copy of the MPL was not distributed with this")
    print(" * file, You can obtain one at http://mozilla.org/MPL/2.0/. */")
    print()
    print("// CLDR data © Unicode, Inc., under the Unicode License v3 (LICENSE-UNICODE).")
    print()
    print("//! Generated by `tools/gen_intl_patterns.py` from the ICU data of OpenHarmony 7.0 (ICU 74, CLDR 44.1).")
    print("//! Do not edit.")
    print("//!")
    print("//! Per locale, separated by `\\x01`: the 8 date and time style patterns, the 4 date-time")
    print("//! combining patterns, the 16 appendItems and 16 field names (UDateTimePatternField order),")
    print("//! then skeleton and pattern of each gregorian availableFormats entry, root's last.")
    print()
    print("/// Region (or language_region): its preferred, then its allowed hour characters (CLDR timeData).")
    print("pub(super) static HOUR_CYCLES: &str = %s;" % rust_str(",".join(hours)))
    print()
    print("pub(super) static ROOT: &str = %s;" % rust_str(locale_data([data.load("root")])))
    print()
    print("/// Maximized locale ID (`uloc_addLikelySubtags`) and its data.")
    print("pub(super) static LOCALES: &[(&str, &str)] = &[")
    for locale, maximized in LOCALES:
        print("    (%s, %s)," % (rust_str(maximized), rust_str(locale_data(data.chain(locale)))))
    print("];")


if __name__ == "__main__":
    main()
