#!/usr/bin/env python3
"""Coverage of the shim, and of SpiderMonkey, by the tests and by WPT (docs/coverage.md).

    coverage.py units  OUT --target DIR --servo-dir DIR [--skip NAME]...  instrumented mozjs tests on QuickJS
    coverage.py sm     OUT --target DIR --servo-dir DIR [--instrument]    the same test files on SpiderMonkey
    coverage.py servo  ENGINE --target DIR --servo-dir DIR --profile P    instrumented servoshell from Servo
    coverage.py wpt    ENGINE OUT --binary BIN --servo-dir DIR -- ARGS    WPT with it, then `export`
    coverage.py export OUT --binary BIN [--engine E]                      merge OUT/prof into OUT/reach.json
    coverage.py report DIR [--engine E]                                   groups, coverage, and the WPT gap

`units` runs each test alone with its own profile. `sm` builds `mozjs/tests` against the crates.io mozjs
of the same version and runs each test alone; with `--instrument` it builds SpiderMonkey from source
with its C++ instrumented too, using the clang of rustc's LLVM version, so both share one profiling
runtime. `report` groups one engine's profiles into all tests, tests that also run on the other engine,
and tests that pass on both, and compares each group with what that engine's WPT run reaches.
"""
import argparse
import collections
import concurrent.futures
import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCES = [ROOT / "mozjs" / "src", ROOT / "servo-quickjs" / "src"]
RUSTFLAGS = "-C instrument-coverage -Zcoverage-options=condition -C llvm-args=-runtime-counter-relocation"
# Atomic updates: SpiderMonkey's helper threads otherwise lose increments, and llvm-cov's derived counts
# (a region's count minus its children's) wrap around.
CLANG_FLAGS = ("-fprofile-instr-generate -fcoverage-mapping -mllvm=-runtime-counter-relocation "
               "-fprofile-update=atomic")
# SpiderMonkey's internals without a counterpart in the shim; their branches are no API cases.
SPIDERMONKEY_INTERNALS = {"jit", "frontend", "irregexp", "wasm", "gc", "jsapi-tests"}
TIMEOUT = 300
# Per engine: the directory of its unit results, of the other engine's, and of its WPT reach set.
ENGINES = {
    "quickjs": {"units": "units", "other": "sm", "other_label": "SM", "wpt": "wpt"},
    "spidermonkey": {"units": "spidermonkey", "other": "units", "other_label": "QuickJS", "wpt": "wpt-spidermonkey"},
}
# What `units` and `sm` write into their output dir, and `wpt` into its own; a rerun removes only these.
RUN_OUTPUTS = ("prof", "list-prof", "build-prof", "logs", "build.log", "results.tsv", "objects.txt", "meta.json")
WPT_OUTPUTS = ("prof", "wpt.log", "reach.json", "reach.list", "meta.json")
# llvm-cov export's JSON (llvm/tools/llvm-cov/CoverageExporterJson.cpp) has positional arrays:
# regions [line, column, end line, end column, count, file, expanded file, kind] and
# branches [line, column, end line, end column, true count, false count, file, expanded file, kind].
EXPORT_MAJOR = "3"
REGION_COUNT, REGION_FILE, REGION_KIND = 4, 5, 7
BRANCH_TRUE, BRANCH_FALSE, BRANCH_FILE = 4, 5, 6
CODE_REGION = 0
Span = collections.namedtuple("Span", "line column end_line end_column")
# The keys of coverage data: a code region, and one outcome (True or False) of a condition.
Region = collections.namedtuple("Region", "file span")
Outcome = collections.namedtuple("Outcome", "file span value")


def toolchain(servo_dir):
    """Servo's pinned toolchain, so unit and WPT builds produce the same region spans."""
    if os.environ.get("COVERAGE_TOOLCHAIN"):
        return os.environ["COVERAGE_TOOLCHAIN"]
    path = Path(servo_dir) / "rust-toolchain.toml"
    if path.is_file():
        return tomllib.loads(path.read_text())["toolchain"]["channel"]
    return "stable"


def rustc_info(tc):
    out = subprocess.run(["rustc", f"+{tc}", "-vV"], check=True, capture_output=True, text=True).stdout
    return dict(line.split(": ", 1) for line in out.splitlines() if ": " in line)


def llvm_bin(tc):
    sysroot = subprocess.run(["rustc", f"+{tc}", "--print", "sysroot"], check=True, capture_output=True,
                             text=True).stdout.strip()
    path = Path(sysroot) / "lib" / "rustlib" / rustc_info(tc)["host"] / "bin"
    if not (path / "llvm-profdata").exists():
        sys.exit(f"llvm-tools missing for {tc}: rustup component add llvm-tools --toolchain {tc}")
    return path


def spidermonkey_env(tc, profiles):
    """Builds SpiderMonkey from source with its C++ instrumented by the clang of rustc's LLVM version.

    The test binaries and servoshell link rustc's profiling runtime; C++ instrumented by another LLVM
    version would need a second, incompatible one.
    """
    clang = os.environ.get("COVERAGE_CLANG") or f"clang-{rustc_info(tc)['LLVM version'].split('.')[0]}"
    if not shutil.which(clang):
        sys.exit(f"{clang} not found: instrumenting SpiderMonkey needs the clang of rustc +{tc}'s LLVM version "
                 "(or set COVERAGE_CLANG)")
    # bindgen's libclang from the same LLVM, so headers, compiler and bindings agree. bindgen takes the
    # include paths from the `clang` on PATH unless CLANG_PATH names one, and another version's builtin
    # headers don't parse.
    resource_dir = subprocess.run([clang, "-print-resource-dir"], capture_output=True, text=True).stdout.strip()
    libclang = Path(resource_dir).parents[1] if resource_dir else None
    env = {"CLANG_PATH": shutil.which(clang)}
    if libclang and any(libclang.glob("libclang*.so*")):
        env["LIBCLANG_PATH"] = str(libclang)
    return {**env, "MOZJS_FROM_SOURCE": "1", "CC": clang, "CXX": clang.replace("clang", "clang++", 1),
            "CFLAGS": CLANG_FLAGS, "CXXFLAGS": CLANG_FLAGS, "LDFLAGS": "-fprofile-instr-generate",
            # Tools that SpiderMonkey's build compiles and runs are instrumented too.
            "LLVM_PROFILE_FILE": str(profiles / "%p.profraw")}


def spidermonkey_crates(cwd):
    """The crates.io `mozjs` and `mozjs_sys` that a workspace resolves: {name: (directory, version)}."""
    metadata = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1"], cwd=cwd, check=True,
                                         capture_output=True, text=True).stdout)
    return {p["name"]: (Path(p["manifest_path"]).parent, p["version"]) for p in metadata["packages"]
            if p["name"] in ("mozjs", "mozjs_sys") and (p["source"] or "").startswith("registry+")}


def spidermonkey_identity(crates):
    """SpiderMonkey's region keys follow the crates.io versions, which never change their sources."""
    return ", ".join(f"{name} {version}" for name, (_, version) in sorted(crates.items()))


def spidermonkey_sources(crates):
    """SpiderMonkey's API layer: the files that define public or friend API functions, the glue, and mozjs."""
    mozjs_sys, mozjs = crates["mozjs_sys"][0], crates["mozjs"][0]
    js_src = mozjs_sys / "mozjs" / "js" / "src"
    definition = re.compile(r"^JS_(?:PUBLIC|FRIEND)_API", re.M)
    files = [p for p in sorted(js_src.rglob("*.cpp")) if not SPIDERMONKEY_INTERNALS & set(p.relative_to(js_src).parts)
             and definition.search(p.read_text(errors="replace"))]
    return [str(p) for p in files] + [str(mozjs_sys / "src"), str(mozjs / "src")]


def fingerprint(paths):
    """A digest of the files under `paths` in this checkout, uncommitted changes included."""
    digest = hashlib.sha256()
    for root in paths:
        for path in sorted(Path(root).rglob("*")):
            if path.is_file() and "__pycache__" not in path.parts:
                digest.update(str(path.relative_to(ROOT)).encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()[:12]


def identity():
    """What runs must share to be compared: region keys follow the shim's sources, results the tests."""
    return {"sources": "shim " + fingerprint(SOURCES), "tests": fingerprint([ROOT / "mozjs" / "tests"]),
            "mozjs": tomllib.loads((ROOT / "mozjs" / "Cargo.toml").read_text())["package"]["version"]}


def clean(out, names):
    """Removes what an earlier run wrote to `out`, and nothing else."""
    for name in names:
        path = out / name
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink(missing_ok=True)


def git_head(path):
    return subprocess.run(["git", "-C", path, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()


def write_meta(out, tc, ids, **extra):
    meta = {"toolchain": tc, "rustc": rustc_info(tc)["release"], "llvm": str(llvm_bin(tc)), "commit": git_head(ROOT),
            **ids, **extra}
    (out / "meta.json").write_text(json.dumps(meta, indent=1) + "\n")


def cargo_executables(cmd, cwd, env, log):
    """Runs a cargo command with `--message-format=json`; returns {test target name: executable}."""
    with open(log, "w") as err:
        proc = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=err, text=True)
    executables = {}
    for line in proc.stdout.splitlines():
        message = json.loads(line) if line.startswith("{") else {}
        if message.get("reason") == "compiler-artifact" and message.get("executable") \
                and message["profile"]["test"]:
            executables[message["target"]["name"]] = message["executable"]
    return proc.returncode, executables


def safe(test_id):
    return re.sub(r"[^A-Za-z0-9_.-]", "_", test_id)


def run_tests(executables, out, skip, profile, jobs):
    """Runs every test of every executable alone; writes `results.tsv` (id, status, profile prefix)."""
    logs = out / "logs"
    logs.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, CARGO_MANIFEST_DIR=str(ROOT / "mozjs"))
    # Listing runs the instrumented binaries too; without this they write profiles into `mozjs/`.
    list_env = dict(env, LLVM_PROFILE_FILE=str(out / "list-prof" / "%p.profraw"))
    tests = []
    for target, exe in sorted(executables.items()):
        listing = subprocess.run([exe, "--list", "--format", "terse"], cwd=ROOT / "mozjs", env=list_env,
                                 capture_output=True, text=True).stdout
        for name in re.findall(r"^(.*): test$", listing, re.M):
            if not any(pattern in name for pattern in skip):
                tests.append((f"{target}::{name}", exe, name))

    def run(test):
        test_id, exe, name = test
        test_env = dict(env)
        if profile:
            test_env["LLVM_PROFILE_FILE"] = str(out / "prof" / f"{safe(test_id)}.%c%p.profraw")
        log = logs / f"{safe(test_id)}.log"
        try:
            with open(log, "w") as file:
                subprocess.run([exe, "--exact", name, "--test-threads=1"], cwd=ROOT / "mozjs", env=test_env,
                               stdout=file, stderr=subprocess.STDOUT, timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            return test_id, "timeout"
        # Read libtest's summary, not the exit code: SpiderMonkey crashes in a static destructor at exit.
        summary = re.search(r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored",
                            log.read_text(errors="replace"))
        if not summary:
            return test_id, "crash"
        passed, failed, ignored = map(int, summary.groups())
        return test_id, "pass" if passed else "ignored" if ignored and not failed else "fail"

    with concurrent.futures.ThreadPoolExecutor(jobs) as pool:
        results = dict(pool.map(run, tests))
    with open(out / "results.tsv", "w") as file:
        for test_id in sorted(results):
            file.write(f"{test_id}\t{results[test_id]}\t{safe(test_id)}\n")
    return results


def summarize(label, results):
    counts = collections.Counter(results.values())
    print(f"{label}: {len(results)} tests, " + ", ".join(f"{n} {s}" for s, n in sorted(counts.items())))


def units(args):
    out = Path(args.out)
    clean(out, RUN_OUTPUTS)
    (out / "prof").mkdir(parents=True)
    tc = toolchain(args.servo_dir)
    ids = identity()
    # Build scripts are instrumented too; keep their profiles out of the source tree.
    env = dict(os.environ, RUSTC_BOOTSTRAP="1", RUSTFLAGS=RUSTFLAGS, CARGO_TARGET_DIR=args.target,
               LLVM_PROFILE_FILE=str(out / "build-prof" / "%p.profraw"))
    status, executables = cargo_executables(
        ["cargo", f"+{tc}", "test", "-p", "mozjs", "--no-run", "--message-format=json"], ROOT, env, out / "build.log")
    if status:
        sys.exit(f"instrumented build failed, see {out / 'build.log'}")
    (out / "objects.txt").write_text("".join(f"{exe}\n" for exe in sorted(executables.values())))
    write_meta(out, tc, ids)
    summarize("QuickJS", run_tests(executables, out, args.skip, True, args.jobs))


def sm(args):
    out = Path(args.out)
    clean(out, RUN_OUTPUTS)
    (out / "prof").mkdir(parents=True)
    crate = out / "crate"
    (crate / "src").mkdir(parents=True, exist_ok=True)
    (crate / "src" / "lib.rs").write_text("")
    package = tomllib.loads((ROOT / "mozjs" / "Cargo.toml").read_text())["package"]
    files = sorted((ROOT / "mozjs" / "tests").glob("*.rs"))
    manifest = [
        "# Generated by tools/coverage.py: mozjs/tests against the crates.io mozjs (SpiderMonkey).",
        "[package]", 'name = "mozjs-tests-on-spidermonkey"', 'version = "0.0.0"',
        f'edition = "{package["edition"]}"', "publish = false", "autotests = false", "",
        "[dependencies]", f'mozjs = {{ version = "={package["version"]}", features = ["intl"] }}',
        # The shim's tests use the shim crate's own dependencies.
        'encoding_rs = "0.8"', "", "[workspace]", "",
    ]
    for file in files:
        manifest += ["[[test]]", f'name = "{file.stem}"', f'path = "{file}"', ""]
    (crate / "Cargo.toml").write_text("\n".join(manifest))
    tc = toolchain(args.servo_dir)
    ids = identity()
    env = dict(os.environ, CARGO_TARGET_DIR=args.target)
    env.pop("RUSTFLAGS", None)
    extra = {}
    if args.instrument:
        env.update(spidermonkey_env(tc, out / "build-prof"), RUSTC_BOOTSTRAP="1", RUSTFLAGS=RUSTFLAGS)
    # Files that use the shim's own API do not compile against SpiderMonkey; `--keep-going` builds the rest.
    _, executables = cargo_executables(
        ["cargo", f"+{tc}", "build", "--tests", "--keep-going", "--message-format=json"], crate, env,
        out / "build.log")
    stems = {f.stem for f in files}
    executables = {name: exe for name, exe in executables.items() if name in stems}
    if args.instrument:
        crates = spidermonkey_crates(crate)
        (out / "objects.txt").write_text("".join(f"{exe}\n" for exe in sorted(executables.values())))
        extra.update(engine="spidermonkey", sources=spidermonkey_identity(crates),
                     source_paths=spidermonkey_sources(crates))
    results = run_tests(executables, out, [], args.instrument, args.jobs)
    broken = sorted(stems - set(executables))
    with open(out / "results.tsv", "a") as file:
        for stem in broken:
            file.write(f"{stem}::*\tcompile-error\t-\n")
    write_meta(out, tc, ids, **extra)
    summarize("SpiderMonkey", results)
    print(f"SpiderMonkey: {len(broken)} of {len(files)} test files do not compile: {' '.join(broken)}")


def servo_toolchain(servo_dir):
    """The rustc that cargo uses in the Servo checkout (its rust-toolchain.toml), and its llvm-tools."""
    def rustc(*flags):
        return subprocess.run(["rustc", *flags], cwd=servo_dir, check=True, capture_output=True, text=True).stdout

    info = dict(line.split(": ", 1) for line in rustc("-vV").splitlines() if ": " in line)
    return info["release"], Path(rustc("--print", "sysroot").strip()) / "lib" / "rustlib" / info["host"] / "bin"


def build_record(binary):
    return Path(binary).parent / "servoshell.coverage.json"


def servo(args):
    """Builds servoshell with the engine instrumented wherever it is compiled (tools/coverage-rustc-wrapper.sh).

    Writes `servoshell.coverage.json` next to it: the sources and the rustc that the counters belong to.
    """
    env = dict(os.environ, RUSTC_WRAPPER=str(ROOT / "tools" / "coverage-rustc-wrapper.sh"),
               CARGO_TARGET_DIR=args.target)
    cmd = ["cargo", "build", "-p", "servoshell", "--profile", args.profile]
    if args.engine == "quickjs":
        env["SERVO_QUICKJS_DEBUGGER"] = "1"
        cmd += ["--features", "js-quickjs", "--config", f'patch.crates-io.mozjs.path="{ROOT / "mozjs"}"']
        sources = identity()["sources"]
    else:
        env.update(spidermonkey_env(toolchain(args.servo_dir), Path(args.target) / "build-prof"))
        sources = spidermonkey_identity(spidermonkey_crates(args.servo_dir))
    record = build_record(Path(args.target) / ("debug" if args.profile == "dev" else args.profile) / "servoshell")
    record.unlink(missing_ok=True)
    status = subprocess.run(cmd, cwd=args.servo_dir, env=env).returncode
    if status:
        sys.exit(status)
    if args.engine == "quickjs" and identity()["sources"] != sources:
        sys.exit("the shim's sources changed during the build; build again")
    rustc, llvm = servo_toolchain(args.servo_dir)
    record.write_text(json.dumps({"engine": args.engine, "sources": sources, "rustc": rustc, "llvm": str(llvm),
                                  "commit": git_head(ROOT), "servo_commit": git_head(args.servo_dir)}, indent=1) + "\n")


def merge_and_export(llvm, profiles, objects, out_json, sources, normalize):
    """Merges the profiles and exports the coverage of the functions in `sources`."""
    if not profiles:
        # An empty group, e.g. no test passes on both engines: nothing covered.
        out_json.write_text(json.dumps({"functions": []}))
        return
    listing = out_json.with_suffix(".list")
    listing.write_text("".join(f"{p}\n" for p in profiles))
    profdata = out_json.with_suffix(".profdata")
    subprocess.run([llvm / "llvm-profdata", "merge", "-sparse", "-f", listing, "-o", profdata], check=True)
    cmd = [llvm / "llvm-cov", "export", f"--instr-profile={profdata}", "--skip-expansions"]
    for obj in objects:
        cmd += ["--object", obj]
    exported = json.loads(subprocess.run([*cmd, "--sources", *sources], capture_output=True, check=True).stdout)
    profdata.unlink()
    if exported["version"].split(".")[0] != EXPORT_MAJOR:
        sys.exit(f"llvm-cov export format {exported['version']}: check the array positions in tools/coverage.py, "
                 f"which follow format {EXPORT_MAJOR}.x")
    # `--sources` filters files, not functions; servoshell has a hundred thousand functions that aren't ours.
    functions = [f for f in exported["data"][0]["functions"] if any(map(normalize, f["filenames"]))]
    out_json.write_text(json.dumps({"functions": functions}))


def norm(path):
    """`mozjs/src/...` or `servo-quickjs/src/...`, wherever the shim was checked out."""
    match = re.search(r"((?:mozjs|servo-quickjs)/src/.*)", path)
    return match.group(1) if match and "/tests/" not in path else None


def norm_spidermonkey(path):
    """`js/src/...`, `mozjs_sys/src/...` or `mozjs/src/...` in the crates.io crates."""
    for pattern, prefix in ((r"/mozjs_sys-[^/]+/mozjs/(js/src/.*)$", ""), (r"/mozjs_sys-[^/]+/(src/.*)$", "mozjs_sys/"),
                            (r"/mozjs-[0-9][^/]*/(src/.*)$", "mozjs/")):
        match = re.search(pattern, path)
        if match:
            return prefix + match.group(1)
    return None


def engine_sources(base, engine):
    """(source paths, path normalizer) of an engine; SpiderMonkey's come from its unit run."""
    if engine == "quickjs":
        return SOURCES, norm
    meta = json.loads((base / ENGINES[engine]["units"] / "meta.json").read_text())
    if "source_paths" not in meta:
        sys.exit("run `just coverage-spidermonkey` first: it records SpiderMonkey's sources")
    # Functions reach into other files through inlining; keep only the selected files' code.
    files = {norm_spidermonkey(p) for p in meta["source_paths"] if p.endswith(".cpp")}
    dirs = tuple(norm_spidermonkey(p + "/") for p in meta["source_paths"] if not p.endswith(".cpp"))

    def normalize(path):
        name = norm_spidermonkey(path)
        return name if name and (name in files or name.startswith(dirs)) else None

    return meta["source_paths"], normalize


def read_build_record(binary, engine):
    """The build record that `servo` wrote next to the binary; refuses binaries built some other way."""
    binary, record = Path(binary), build_record(binary)
    if not record.exists() or record.stat().st_mtime < binary.stat().st_mtime:
        sys.exit(f"{binary} has no build record ({record.name}) or was rebuilt after it, so its counters can't be "
                 "matched to sources; build it with `just coverage-servo` or `just coverage-servo-sm`")
    build = json.loads(record.read_text())
    if build["engine"] != engine:
        sys.exit(f"{binary} was built for {build['engine']}, not {engine}")
    return build


def export_reach(out, binary, engine):
    build = read_build_record(binary, engine)
    profiles = sorted(glob.glob(str(out / "prof" / "*.profraw")))
    if not profiles:
        sys.exit(f"no profiles in {out / 'prof'}")
    sources, normalize = engine_sources(out.parent, engine)
    merge_and_export(Path(build["llvm"]), profiles, [binary], out / "reach.json", sources, normalize)
    # The counters belong to the binary's build, whatever the checkout holds now.
    meta = {**build, "profiles": len(profiles), "binary": str(binary)}
    (out / "meta.json").write_text(json.dumps(meta, indent=1) + "\n")
    print(f"exported {len(profiles)} profiles to {out / 'reach.json'}")


def export(args):
    export_reach(Path(args.out), args.binary, args.engine)


def wpt(args):
    """Runs WPT with an instrumented servoshell, one profile per browser, and exports what it reaches."""
    out = Path(args.out)
    read_build_record(args.binary, args.engine)
    clean(out, WPT_OUTPUTS)
    (out / "prof").mkdir(parents=True)
    mach_args = args.mach_args[1:] if args.mach_args[:1] == ["--"] else args.mach_args
    # `%c` keeps the counters of browsers that wptrunner kills.
    env = dict(os.environ, LLVM_PROFILE_FILE=str(out / "prof" / "%c%p.profraw"))
    status = subprocess.run(["./mach", "test-wpt", "--bin", args.binary, "--timeout-multiplier", "2",
                             "--log-raw", str(out / "wpt.log"), *mach_args], cwd=args.servo_dir, env=env).returncode
    print(f"test-wpt exited with {status}; unexpected results do not stop the export")
    export_reach(out, args.binary, args.engine)


def load(path, normalize):
    """Code regions and condition outcomes of an export as {Region: count} and {Outcome: count}, and the
    functions of each key."""
    data = json.load(open(path))
    regions = collections.defaultdict(int)
    outcomes = collections.defaultdict(int)
    owners = collections.defaultdict(set)
    for function in data["functions"] if "functions" in data else data["data"][0]["functions"]:
        # `#[cfg(test)] mod tests` code exists only in unit builds.
        if "5tests" in function["name"]:
            continue
        files = [normalize(f) for f in function["filenames"]]
        for r in function["regions"]:
            if r[REGION_KIND] == CODE_REGION and files[r[REGION_FILE]]:
                key = Region(files[r[REGION_FILE]], Span(*r[:4]))
                regions[key] = max(regions[key], r[REGION_COUNT])
                owners[key].add(function["name"])
        for b in function.get("branches", []):
            if files[b[BRANCH_FILE]]:
                for value, count in ((True, b[BRANCH_TRUE]), (False, b[BRANCH_FALSE])):
                    key = Outcome(files[b[BRANCH_FILE]], Span(*b[:4]), value)
                    outcomes[key] = max(outcomes[key], count)
                    owners[key].add(function["name"])
    return regions, outcomes, owners


def hit(counts):
    return {k for k, c in counts.items() if c > 0}


def demangle(symbols):
    """Readable Rust and C++ names without crate hashes; the symbols themselves without c++filt."""
    names = {}
    # Internal C++ functions are recorded as `file;symbol`.
    plain = {s: s.rsplit(";", 1)[-1] for s in symbols}
    for style, picked in ((["-s", "rust"], [s for s in symbols if plain[s].startswith("_R")]),
                          ([], [s for s in symbols if not plain[s].startswith("_R")])):
        try:
            paths = subprocess.run(["c++filt", *style], input="\n".join(plain[s] for s in picked),
                                   capture_output=True, text=True, check=True).stdout.splitlines()
        except (OSError, subprocess.CalledProcessError):
            paths = []
        if len(paths) != len(picked):
            paths = [plain[s] for s in picked]
        names.update((s, re.sub(r"\[[0-9a-f]+\]", "", p)) for s, p in zip(picked, paths))
    return names


def read_results(path):
    results = {}
    for line in open(path):
        test_id, status, prefix = line.rstrip("\n").split("\t")
        results[test_id] = (status, prefix)
    return results


def percent(part, whole):
    return f"{100 * part / whole:5.1f}%" if whole else "    -"


def times(count):
    # A derived count that wrapped around: its counters were updated without synchronization.
    return "an inconsistent number of times" if count >= 2**62 else f"{count:,}x"


def print_gaps(label, gap_regions, gap_outcomes, reach, names, limit):
    """The functions with code WPT runs and `label` does not cover, hottest first."""
    reach_regions, reach_outcomes, reach_owners = reach
    def owner(key):
        # Of the functions that share the code, e.g. a generic's instantiations, the shortest name.
        return min((names[s] for s in reach_owners[key]), key=lambda name: (len(name), name))

    per_function = collections.defaultdict(list)
    for key in gap_regions:
        per_function[owner(key)].append((reach_regions[key], key))
    for key in gap_outcomes:
        per_function[owner(key)].append((reach_outcomes[key], key))
    ranked = sorted(per_function.items(), key=lambda item: (-max(count for count, _ in item[1]), item[0]))
    print(f"\ncode WPT runs that `{label}` does not cover, by function, hottest first "
          f"({len(ranked)} functions; showing {min(limit, len(ranked))}):")
    for name, items in ranked[:limit]:
        regions = [(count, key) for count, key in items if isinstance(key, Region)]
        conditions = sorted(key for _, key in items if isinstance(key, Outcome))
        print(f"  {name}  ({items[0][1].file})")
        if regions:
            lines = sorted({line for _, key in regions for line in range(key.span.line, key.span.end_line + 1)})
            ranges, start = [], lines[0]
            for previous, line in zip(lines, lines[1:] + [None]):
                if line != previous + 1:
                    ranges.append(f"{start}-{previous}" if start != previous else f"{start}")
                    start = line
            print(f"    lines {', '.join(ranges)}: WPT ran them up to {times(max(c for c, _ in regions))}")
        for key in conditions:
            print(f"    condition at {key.span.line}:{key.span.column}: `{label}` never makes it "
                  f"{'true' if key.value else 'false'}; WPT did {times(reach_outcomes[key])}")


def write_lcov(path, reach, selected, covered_regions, covered_outcomes):
    """Lines and conditions WPT runs, as hit when the group covers them and missed when it doesn't.

    As in llvm-cov, a line belongs to the regions that start on it and to the innermost region it lies
    in. It is missed when any of those that WPT ran is uncovered, and left out when WPT ran none of
    them or they are outside `--match`, so an editor shows nothing there.
    """
    reach_regions, reach_outcomes, _ = reach
    by_file = collections.defaultdict(list)
    for key in reach_regions:
        by_file[key.file].append(key)
    with open(path, "w") as out:
        for file in sorted(by_file):
            starts, inside = collections.defaultdict(list), {}
            # Outer regions first, so that each line ends up with the innermost region it continues.
            for key in sorted(by_file[file], key=lambda k: (k.span.end_line - k.span.line,
                                                            k.span.end_column - k.span.column), reverse=True):
                starts[key.span.line].append(key)
                for line in range(key.span.line + 1, key.span.end_line + 1):
                    inside[line] = key
            lines = {}
            for line in sorted(set(starts) | set(inside)):
                owners = starts.get(line, []) + ([inside[line]] if line in inside else [])
                relevant = [k for k in owners if reach_regions[k] > 0 and selected(k)]
                if relevant:
                    lines[line] = int(all(k in covered_regions for k in relevant))
            conditions = sorted(k for k in reach_outcomes if k.file == file and reach_outcomes[k] > 0 and selected(k))
            if not lines and not conditions:
                continue
            out.write(f"SF:{file}\n")
            blocks = {}
            for key in conditions:
                block = blocks.setdefault(key.span, len(blocks))
                taken = 1 if key in covered_outcomes else 0
                out.write(f"BRDA:{key.span.line},{block},{0 if key.value else 1},{taken}\n")
            out.write(f"BRF:{len(conditions)}\nBRH:{sum(k in covered_outcomes for k in conditions)}\n")
            for line in sorted(lines):
                out.write(f"DA:{line},{lines[line]}\n")
            out.write(f"LF:{len(lines)}\nLH:{sum(lines.values())}\nend_of_record\n")
    print(f"\nwrote {path}: WPT-reached lines and conditions, hit when covered by the group")


def report(args):
    base = Path(args.dir)
    engine = ENGINES[args.engine]
    units_dir = base / engine["units"]
    if not (units_dir / "meta.json").exists():
        sys.exit(f"no unit results in {units_dir}")
    meta = json.loads((units_dir / "meta.json").read_text())
    tests = read_results(units_dir / "results.tsv")
    other_path = base / engine["other"] / "results.tsv"
    other = read_results(other_path) if other_path.exists() else None
    if other is not None:
        # Results join by test name; a test that changed under its name must not agree with its old self.
        other_meta = json.loads((other_path.parent / "meta.json").read_text())
        for key, what in (("tests", "test sources (mozjs/tests)"), ("mozjs", "mozjs versions")):
            if meta.get(key) is None or other_meta.get(key) != meta.get(key):
                sys.exit(f"{units_dir.name} and {other_path.parent.name} used different {what}: "
                         f"{meta.get(key)} and {other_meta.get(key)}; rerun the older one before comparing them")
    ran = ("pass", "fail", "timeout", "crash")
    groups = {"all": [t for t, (s, _) in tests.items() if s in ran]}
    if other is None:
        print(f"no {engine['other_label']} results in {other_path.parent}: reporting all tests only\n")
    else:
        groups[f"runs on {engine['other_label']}"] = [t for t in groups["all"] if other.get(t, ("",))[0] in ran]
        groups["agrees"] = [t for t in groups["all"] if tests[t][0] == "pass" and other.get(t, ("",))[0] == "pass"]
    focus = args.group or list(groups)[-1]
    if focus not in groups:
        sys.exit(f"--group must be one of: {', '.join(groups)}")
    sources, normalize = engine_sources(base, args.engine)
    out = base / "report" / args.engine
    out.mkdir(parents=True, exist_ok=True)
    objects = (units_dir / "objects.txt").read_text().split()
    llvm = Path(meta["llvm"])
    loaded = {}
    for label, members in groups.items():
        profiles = [p for t in members for p in glob.glob(str(units_dir / "prof" / f"{tests[t][1]}.*.profraw"))]
        path = out / f"export-{label.replace(' ', '-')}.json"
        merge_and_export(llvm, profiles, objects, path, sources, normalize)
        loaded[label] = load(path, normalize)
    reach_path = base / engine["wpt"] / "reach.json"
    reach = load(reach_path, normalize) if reach_path.exists() else None

    owners = collections.defaultdict(set)
    for _, _, keys in [*loaded.values(), *([reach] if reach else [])]:
        for key, symbols in keys.items():
            owners[key] |= symbols
    names = demangle({s for symbols in owners.values() for s in symbols})
    pattern = re.compile(args.match) if args.match else None
    memo = {}

    def selected(key):
        if pattern is None:
            return True
        if key not in memo:
            memo[key] = bool(pattern.search(key.file) or any(pattern.search(names[s]) for s in owners[key]))
        return memo[key]

    def pick(keys):
        return {k for k in keys if selected(k)}

    universe_regions = pick(loaded["all"][0])
    universe_outcomes = pick(loaded["all"][1])
    covered = {label: (pick(hit(regions)), pick(hit(outcomes))) for label, (regions, outcomes, _) in loaded.items()}
    scope = f", matching /{args.match}/" if pattern else ""
    what = "SpiderMonkey's API layer" if args.engine == "spidermonkey" else "the shim"
    print(f"{what} ({meta['sources']}), rustc {meta['rustc']}{scope}: {len(universe_regions)} regions, "
          f"{len(universe_outcomes)} condition outcomes in the unit builds")
    summary = {"engine": args.engine, "commit": meta["commit"], "sources": meta["sources"], "rustc": meta["rustc"],
               "match": args.match, "groups": {}}
    print(f"\n{'group':16s} {'tests':>6s} {'regions':>16s} {'outcomes':>16s}")
    for label, (reg, out_cov) in covered.items():
        print(f"{label:16s} {len(groups[label]):6d} {len(reg):8d} {percent(len(reg), len(universe_regions))}"
              f" {len(out_cov):8d} {percent(len(out_cov), len(universe_outcomes))}")
        summary["groups"][label] = {"tests": len(groups[label]), "regions": len(reg), "outcomes": len(out_cov)}

    if reach is None:
        print(f"\nno WPT reach set in {reach_path.parent}")
        (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
        return
    wpt_meta = json.loads((reach_path.parent / "meta.json").read_text())
    if wpt_meta.get("sources") != meta.get("sources"):
        # Region keys are source spans: different sources don't line up, and the numbers would be wrong.
        print(f"\nthe WPT reach set belongs to other sources ({wpt_meta.get('sources')}; the units: "
              f"{meta.get('sources')}), so it is not compared; rebuild servoshell and rerun WPT for these sources")
        (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
        return
    if wpt_meta.get("rustc") != meta.get("rustc"):
        print(f"\nwarning: servoshell was built with rustc {wpt_meta.get('rustc')}, the units with {meta.get('rustc')}; "
              "region spans can differ between versions")
    w_reg, w_out = pick(hit(reach[0])), pick(hit(reach[1]))
    print(f"\nWPT ({wpt_meta['profiles']} profiles) reaches {len(w_reg)} regions and takes {len(w_out)} "
          "condition outcomes. Of those, covered by:")
    print(f"{'group':16s} {'regions':>16s} {'gap':>6s} {'outcomes':>16s} {'gap':>6s}")
    for label, (reg, out_cov) in covered.items():
        print(f"{label:16s} {len(w_reg & reg):8d} {percent(len(w_reg & reg), len(w_reg))} "
              f"{len(w_reg - reg):6d} {len(w_out & out_cov):8d} {percent(len(w_out & out_cov), len(w_out))} "
              f"{len(w_out - out_cov):6d}")
        summary["groups"][label].update(wpt_regions=len(w_reg & reg), wpt_outcomes=len(w_out & out_cov))
    summary.update(wpt_regions=len(w_reg), wpt_outcomes=len(w_out))
    reg, out_cov = covered[focus]
    per_file = collections.Counter(k.file for k in w_reg)
    gap = collections.Counter(k.file for k in w_reg - reg)
    outcomes_per_file = collections.Counter(k.file for k in w_out)
    outcome_gap = collections.Counter(k.file for k in w_out - out_cov)
    print(f"\nlargest gaps for `{focus}`: file, WPT-reached regions, covered, gap, WPT-taken outcomes, covered")
    for file, n in sorted(gap.items(), key=lambda item: (-item[1], item[0]))[:args.files]:
        print(f"  {file:44s} {per_file[file]:6d} {percent(per_file[file] - n, per_file[file])} {n:6d}"
              f" {outcomes_per_file[file]:6d} "
              f"{percent(outcomes_per_file[file] - outcome_gap[file], outcomes_per_file[file])}")
    if args.gaps:
        print_gaps(focus, w_reg - reg, w_out - out_cov, reach, names, args.gaps)
    if args.lcov:
        write_lcov(args.lcov, reach, selected, reg, out_cov)
    (out / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name, function in (("units", units), ("sm", sm)):
        command = commands.add_parser(name)
        command.add_argument("out")
        command.add_argument("--target", required=True)
        command.add_argument("--servo-dir", required=True)
        command.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 2))
        command.add_argument("--skip", action="append", default=[])
        if name == "sm":
            command.add_argument("--instrument", action="store_true",
                                 help="build SpiderMonkey from source and instrument it and the tests")
        command.set_defaults(function=function)
    command = commands.add_parser("servo")
    command.add_argument("engine", choices=ENGINES)
    command.add_argument("--target", required=True)
    command.add_argument("--servo-dir", required=True)
    command.add_argument("--profile", required=True)
    command.set_defaults(function=servo)
    command = commands.add_parser("wpt")
    command.add_argument("engine", choices=ENGINES)
    command.add_argument("out")
    command.add_argument("--binary", required=True)
    command.add_argument("--servo-dir", required=True)
    command.add_argument("mach_args", nargs=argparse.REMAINDER, help="after `--`: arguments for ./mach test-wpt")
    command.set_defaults(function=wpt)
    command = commands.add_parser("export")
    command.add_argument("out")
    command.add_argument("--binary", required=True)
    command.add_argument("--engine", choices=ENGINES, default="quickjs")
    command.set_defaults(function=export)
    command = commands.add_parser("report")
    command.add_argument("dir")
    command.add_argument("--engine", choices=ENGINES, default="quickjs",
                         help="the shim (quickjs) or SpiderMonkey's API layer (spidermonkey)")
    command.add_argument("--match", help="only code whose file or function path matches this regex")
    command.add_argument("--group", help="the group for the gap list and the lcov file (default: the last)")
    command.add_argument("--files", type=int, default=20, help="files in the per-file gap table")
    command.add_argument("--gaps", type=int, default=0, help="list this many functions with gaps")
    command.add_argument("--lcov", help="write WPT-reached code as an lcov file, hit where the group covers it")
    command.set_defaults(function=report)
    args = parser.parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
