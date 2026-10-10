#!/usr/bin/env python3
"""Summarize autorelease pool statistics (`mozjs/src/engine/pool_stats.rs`) as Markdown.

    QJS_POOL_STATS=1 just servo "build --features js-quickjs"   # an instrumented servoshell
    QJS_POOL_STATS_OUT=stats.jsonl servoshell …                 # one JSON line per runtime
    python3 tools/pool_stats.py stats.jsonl [more.jsonl[.gz] …]

Values in the pool are classified by what became of them before it drained: rooted (a `Rooted`
or handle write took them), kept (something else held them: a `Heap`, a `Dom<T>`, a JS object)
or dropped (the pool held their last count).
"""
import collections
import gzip
import json
import re
import sys

KINDS = ["result", "borrowed", "root", "deferred"]


def load(paths):
    """The last line of each runtime: lines are cumulative snapshots, then a final one."""
    runtimes = {}
    for i, path in enumerate(paths):
        with (gzip.open if path.endswith(".gz") else open)(path, "rt") as f:
            for n, line in enumerate(f):
                if line.strip():
                    r = json.loads(line)
                    runtimes[(i, r["pid"], r.get("runtime", f"line {n}"))] = r
    return list(runtimes.values())


def short(frame):
    """`symbol (path:line:col)` with the path from the repository root and generics elided."""
    frame = re.sub(r"\(/[^)]*?/((components|ports|third_party|mozjs)/[^)]*)\)", r"(\1)", frame)
    frame = re.sub(r"\(/rustc/[^)]*\)", "", frame)
    while True:
        elided = re.sub(r"<[^<>]*>", "", frame)
        if elided == frame:
            return frame.strip()
        frame = elided


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(out)


def pct(part, whole):
    return f"{100 * part / whole:.0f}%" if whole else "-"


def main(paths):
    runtimes = load(paths)
    # Script#3 → Script; worker threads are named after their script (WW:url, SWW:url).
    threads = collections.Counter(
        re.sub(r"#\d+$", "", r["thread"].split(":")[0]) or "?" for r in runtimes
    )
    pushes = collections.Counter()
    tasks = sum(r["tasks"] for r in runtimes)
    sum_len = sum(r["sum_len"] for r in runtimes)
    max_len = max((r["max_len"] for r in runtimes), default=0)
    for r in runtimes:
        pushes.update(r["pushes"])

    apis = collections.defaultdict(collections.Counter)
    for r in runtimes:
        for a in r["apis"]:
            name = a["api"] or f"(outside API functions) {a['site']}"
            apis[(name, a["raw"], a["kind"])].update(
                {k: a[k] for k in ("total", "fresh", "rooted", "kept", "dropped")}
            )

    print("# Autorelease pool statistics\n")
    print(f"Runtimes: {len(runtimes)} ({', '.join(f'{t} ×{n}' for t, n in threads.most_common())}).")
    print(f"Tasks that drained the pool: {tasks}; values in the pool at the drain: "
          f"{sum_len / tasks if tasks else 0:.0f} on average, {max_len} at most.\n")

    total = sum(pushes.values())
    print("## How values entered the pool\n")
    print(table(["kind", "values", "share"],
                [(k, pushes[k], pct(pushes[k], total)) for k in KINDS] + [("all", total, "")]))

    def rows(select, limit=None):
        selected = sorted(
            ((key, c) for key, c in apis.items() if select(*key)),
            key=lambda kc: -(kc[1]["kept"] + kc[1]["dropped"]),
        )
        return [
            (f"`{name}`", c["total"], c["fresh"], c["rooted"], c["kept"], c["dropped"],
             pct(c["kept"] + c["dropped"], c["total"]))
            for (name, _, _), c in selected[:limit]
        ]

    header = ["function", "values", "new", "rooted", "kept", "dropped", "unrooted"]
    print("\n## Results of API functions that return raw GC pointers\n")
    print("Unrooted results (kept or dropped) are what a pool-free design must give an owner.\n")
    print(table(header, rows(lambda n, raw, kind: raw and kind == "result")))
    print("\n## Results of other API functions (out-parameters, values)\n")
    print("These are written to the caller's handle, which is not tracked: \"rooted\" is not "
          "observable here.\n")
    print(table(header, rows(lambda n, raw, kind: not raw and kind == "result", 25)))
    print("\n## Extra counts taken on values that already had an owner\n")
    print(table(header, rows(lambda n, raw, kind: kind == "borrowed", 25)))

    callers = collections.Counter()
    every = max((r["sample_every"] for r in runtimes), default=0)
    for r in runtimes:
        for s in r["samples"]:
            frames = [short(f) for f in s["frames"][:2]] or ["?"]
            callers[(s["api"], s["fate"], " ← ".join(frames))] += s.get("count", 1)
    print(f"\n## Callers of raw-pointer functions that leave the result unrooted "
          f"(1 in {every} sampled)\n")
    print(table(["function", "fate", "caller ← its caller", "samples"],
                [(f"`{api}`", fate, f"`{frames}`", n)
                 for (api, fate, frames), n in callers.most_common(40)]))


if __name__ == "__main__":
    main(sys.argv[1:])
