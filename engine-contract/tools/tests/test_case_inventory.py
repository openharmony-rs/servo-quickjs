# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest


sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import case_inventory


class CaseInventoryTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.tests_dir = os.path.join(self.temporary.name, "engine_contract")
        os.makedirs(self.tests_dir)
        self.output = os.path.join(self.temporary.name, "cases.json")

    def write(self, name, source):
        path = os.path.join(self.tests_dir, name)
        with open(path, "w", encoding="utf-8") as file:
            file.write(textwrap.dedent(source))

    def run_scan(self, *args):
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = case_inventory.main(
                [
                    "scan",
                    "--tests-dir",
                    self.tests_dir,
                    "--output",
                    self.output,
                    *args,
                ]
            )
        return result, output.getvalue()

    def load(self):
        with open(self.output, encoding="utf-8") as file:
            return json.load(file)

    def test_scans_direct_and_wrapped_calls(self):
        self.write(
            "sample_test.rs",
            """
            pub fn run(engine: &JSEngine) {
                record_case(engine, "module.direct", APIS, |cx, global| {});
                run_group(engine, "module.wrapped", each_type);
                isolate_case("module.isolated", || {});
                run_case("module.manual", &[], || Observation::ok(Value::Boolean(true)));
                record("module.recorded", &observation, &[]);
            }
            """,
        )
        result, output = self.run_scan()
        self.assertEqual(result, 0, output)
        self.assertEqual(
            self.load(),
            {
                "version": 1,
                "cases": [
                    "module.direct",
                    "module.isolated",
                    "module.manual",
                    "module.recorded",
                    "module.wrapped",
                ],
            },
        )

    def test_ignores_non_case_strings(self):
        self.write(
            "sample_test.rs",
            """
            pub fn run(engine: &JSEngine) {
                let filename = "test.js";
                record_case(engine, "module.real", APIS, |cx, global| {});
            }
            """,
        )
        result, output = self.run_scan()
        self.assertEqual(result, 0, output)
        self.assertEqual(self.load()["cases"], ["module.real"])

    def test_filters_to_selected_cases(self):
        self.write(
            "sample_test.rs",
            """
            pub fn run(engine: &JSEngine) {
                record_case(engine, "module.one", APIS, |cx, global| {});
                record_case(engine, "module.two", APIS, |cx, global| {});
            }
            """,
        )
        result, output = self.run_scan("--case", "module.two")
        self.assertEqual(result, 0, output)
        self.assertEqual(self.load()["cases"], ["module.two"])

    def test_rejects_an_unknown_selected_case(self):
        self.write(
            "sample_test.rs",
            'record_case(engine, "module.real", APIS, |cx, global| {});\n',
        )
        result, output = self.run_scan("--cases", "module.missing")
        self.assertEqual(result, 1)
        self.assertIn("module.missing", output)

    def test_rejects_a_directory_without_cases(self):
        self.write("sample_test.rs", "pub fn run(engine: &JSEngine) {}\n")
        result, output = self.run_scan()
        self.assertEqual(result, 1)
        self.assertIn("no contract cases found", output)

    def write_run_sh_repo(self, name, tool_tests=None):
        tools = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        # The suite is its own checkout with `tools/` at its root; `SERVO_DIR` is separate.
        repository = os.path.join(self.temporary.name, name)
        contract = os.path.join(repository, "engine-contract")
        servo = os.path.join(repository, "servo")
        os.makedirs(os.path.join(contract, "tools"))
        if tool_tests is not None:
            os.makedirs(os.path.join(contract, "tools", "tests"))
            with open(
                os.path.join(contract, "tools", "tests", "test_tool.py"),
                "w",
                encoding="utf-8",
            ) as file:
                file.write(tool_tests)
        shutil.copy(os.path.join(tools, "run.sh"), os.path.join(contract, "tools", "run.sh"))
        shutil.copy(
            os.path.join(tools, "api_inventory.py"),
            os.path.join(contract, "tools", "api_inventory.py"),
        )
        shutil.copy(
            os.path.join(tools, "compare.py"),
            os.path.join(contract, "tools", "compare.py"),
        )
        shutil.copy(
            os.path.join(tools, "fix_record.py"),
            os.path.join(contract, "tools", "fix_record.py"),
        )
        shutil.copy(
            os.path.join(tools, "scope_manifest.py"),
            os.path.join(contract, "tools", "scope_manifest.py"),
        )
        with open(
            os.path.join(contract, "quickjs-difference-allowlist.toml"), "w", encoding="utf-8"
        ):
            pass
        with open(
            os.path.join(contract, "quickjs-shim-fix-records.md"), "w", encoding="utf-8"
        ) as file:
            file.write("## Records\n")
        with open(
            os.path.join(contract, "baseline-quickjs-difference-allowlist.toml"),
            "w",
            encoding="utf-8",
        ) as file:
            file.write("")
        # The version check runs before every coverage check, so the fixture needs the pinned
        # mozjs version in both repositories.
        os.makedirs(os.path.join(servo, "support", "quickjs"), exist_ok=True)
        with open(os.path.join(servo, "Cargo.toml"), "w", encoding="utf-8") as file:
            file.write(
                '[workspace.dependencies]\n'
                'js = { package = "mozjs", version = "=0.26.14" }\n'
            )
        with open(
            os.path.join(servo, "support", "quickjs", "cargo-config.toml"),
            "w",
            encoding="utf-8",
        ) as file:
            file.write(
                '[patch.crates-io]\n'
                'mozjs = { git = "https://example.invalid/servo-quickjs", '
                'rev = "e' + "0" * 39 + '" }\n'
            )
        os.makedirs(os.path.join(self.temporary.name, "mozjs"), exist_ok=True)
        with open(
            os.path.join(self.temporary.name, "mozjs", "Cargo.toml"), "w", encoding="utf-8"
        ) as file:
            file.write('[package]\nname = "mozjs"\nversion = "0.26.14"\n')
        return repository, contract

    def run_diff(self, repository, output_dir):
        return subprocess.run(
            [
                os.path.join(repository, "engine-contract", "tools", "run.sh"),
                "compare-engine-results",
            ],
            cwd=repository,
            env={
                **os.environ,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "OUT_DIR": output_dir,
            },
            text=True,
            capture_output=True,
        )

    def write_observations(self, output_dir, cases):
        for name, engine in (
            ("spidermonkey-contract-results.jsonl", "spidermonkey"),
            ("quickjs-contract-results.jsonl", "quickjs"),
        ):
            with open(os.path.join(output_dir, name), "w", encoding="utf-8") as file:
                for case_id in cases:
                    file.write(
                        json.dumps(
                            {
                                "id": case_id,
                                "engine": engine,
                                "fields": {"value": True},
                                "exception": None,
                                "apis": ["JS_Test"],
                            }
                        )
                        + "\n"
                    )

    def test_run_sh_diff_uses_runtime_case_inventories(self):
        repository, _ = self.write_run_sh_repo("repo-runtime")
        output_dir = os.path.join(self.temporary.name, "out-runtime")
        os.makedirs(output_dir)
        with open(
            os.path.join(output_dir, "spidermonkey-contract-cases.json"),
            "w",
            encoding="utf-8",
        ) as file:
            json.dump({"version": 1, "cases": ["module.real", "module.stale"]}, file)
        with open(
            os.path.join(output_dir, "quickjs-contract-cases.json"), "w", encoding="utf-8"
        ) as file:
            json.dump({"version": 1, "cases": ["module.real", "module.stale"]}, file)
        self.write_observations(output_dir, ["module.real"])
        completed = self.run_diff(repository, output_dir)
        self.assertEqual(completed.returncode, 1, completed.stdout + completed.stderr)
        self.assertIn("missing from SpiderMonkey case inventory: module.stale", completed.stdout)
        self.assertIn("using runtime case inventories:", completed.stdout)

    def test_run_sh_diff_accepts_a_selected_run(self):
        repository, _ = self.write_run_sh_repo("repo-selected")
        output_dir = os.path.join(self.temporary.name, "out-selected")
        os.makedirs(output_dir)
        for name in (
            "spidermonkey-contract-cases.json",
            "quickjs-contract-cases.json",
        ):
            with open(os.path.join(output_dir, name), "w", encoding="utf-8") as file:
                json.dump({"version": 1, "cases": ["module.one"]}, file)
        self.write_observations(output_dir, ["module.one"])
        completed = self.run_diff(repository, output_dir)
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("compared 1 cases", completed.stdout)
        self.assertIn("0 missing from SpiderMonkey case inventory", completed.stdout)
        self.assertIn("0 unexpected in SpiderMonkey case inventory", completed.stdout)

    def test_run_sh_diff_rejects_different_engine_inventories(self):
        repository, _ = self.write_run_sh_repo("repo-inventory-mismatch")
        output_dir = os.path.join(self.temporary.name, "out-inventory-mismatch")
        os.makedirs(output_dir)
        with open(
            os.path.join(output_dir, "spidermonkey-contract-cases.json"),
            "w",
            encoding="utf-8",
        ) as file:
            json.dump({"version": 1, "cases": ["module.one"]}, file)
        with open(
            os.path.join(output_dir, "quickjs-contract-cases.json"), "w", encoding="utf-8"
        ) as file:
            json.dump({"version": 1, "cases": ["module.two"]}, file)
        self.write_observations(output_dir, ["module.one", "module.two"])
        completed = self.run_diff(repository, output_dir)
        self.assertEqual(completed.returncode, 1, completed.stdout + completed.stderr)
        self.assertIn("case inventory mismatch between engines: module.one", completed.stdout)
        self.assertIn("case inventory mismatch between engines: module.two", completed.stdout)

    def write_fake_cargo(self, path, fail_on):
        script = f"""#!/usr/bin/env bash
set -eu
if [[ "$*" == *"{fail_on}"* ]]; then
    exit 1
fi
cat >"${{CONTRACT_CASES_OUT:?}}" <<'JSON'
{{"version": 1, "cases": ["module.one"]}}
JSON
cat >"${{CONTRACT_OUT:?}}" <<JSONL
{{"id": "module.one", "engine": "${{CONTRACT_ENGINE:-unknown}}", "fields": {{"value": true}}, "exception": null, "apis": ["JS_Test"]}}
JSONL
"""
        with open(path, "w", encoding="utf-8") as file:
            file.write(script)
        os.chmod(path, 0o755)

    def test_run_sh_enables_per_case_isolation_by_default(self):
        repository, contract = self.write_run_sh_repo("repo-isolation")
        bin_dir = os.path.join(self.temporary.name, "bin-isolation")
        os.makedirs(bin_dir)
        fake_cargo = os.path.join(bin_dir, "cargo")
        with open(fake_cargo, "w", encoding="utf-8") as file:
            file.write(
                """#!/usr/bin/env bash
set -eu
echo "${CONTRACT_ISOLATE_CASES:-<unset>}" >>"${ISOLATION_LOG:?}"
cat >"${CONTRACT_CASES_OUT:?}" <<'JSON'
{"version": 1, "cases": ["module.one"]}
JSON
cat >"${CONTRACT_OUT:?}" <<JSONL
{"id": "module.one", "engine": "${CONTRACT_ENGINE:-unknown}", "fields": {"value": true}, "exception": null, "apis": ["JS_Test"]}
JSONL
"""
            )
        os.chmod(fake_cargo, 0o755)
        log = os.path.join(self.temporary.name, "isolation.log")
        for expected, extra in (("1", {}), ("0", {"CONTRACT_ISOLATE_CASES": "0"})):
            with open(log, "w", encoding="utf-8"):
                pass
            completed = subprocess.run(
                [
                    os.path.join(contract, "tools", "run.sh"),
                    "run-spidermonkey-contract",
                    "--case",
                    "module.one",
                ],
                cwd=repository,
                env={
                    **os.environ,
                    **extra,
                    "OUT_DIR": os.path.join(self.temporary.name, "out-isolation"),
                    "ISOLATION_LOG": log,
                    "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                },
                text=True,
                capture_output=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
            with open(log, encoding="utf-8") as file:
                self.assertEqual(file.read().strip(), expected)

    def test_run_sh_all_checks_records_when_sm_writes_no_inventory(self):
        repository, contract = self.write_run_sh_repo("repo-no-inventory")
        bin_dir = os.path.join(self.temporary.name, "bin-no-inventory")
        os.makedirs(bin_dir)
        fake_cargo = os.path.join(bin_dir, "cargo")
        self.write_fake_cargo(fake_cargo, "--locked")
        output_dir = os.path.join(self.temporary.name, "out-no-inventory")
        os.makedirs(output_dir)
        completed = subprocess.run(
            [os.path.join(contract, "tools", "run.sh"), "all"],
            cwd=repository,
            env={
                **os.environ,
                "OUT_DIR": output_dir,
                "QJS_DIR": self.temporary.name,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                "FIX_CHECK_BASELINE": os.path.join(
                    contract, "baseline-quickjs-difference-allowlist.toml"
                ),
            },
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 1, completed.stdout + completed.stderr)
        self.assertIn(
            "check-quickjs-shim-fixes (no runtime inventory; checking records only)",
            completed.stdout,
        )
        self.assertIn(
            "fix record: 0 removed difference(s), 0 record(s), 0 problems",
            completed.stdout,
        )
        self.assertNotIn("No such file or directory", completed.stderr)
        self.assertIn(
            "engine contract steps failed: run-spidermonkey-contract compare-engine-results "
            "check-contract-case-coverage",
            completed.stderr,
        )

    def test_run_sh_all_with_case_skips_coverage(self):
        repository, contract = self.write_run_sh_repo("repo-all")
        bin_dir = os.path.join(self.temporary.name, "bin")
        os.makedirs(bin_dir)
        fake_cargo = os.path.join(bin_dir, "cargo")
        self.write_fake_cargo(fake_cargo, "__never__")
        output_dir = os.path.join(self.temporary.name, "out-all")
        os.makedirs(output_dir)
        completed = subprocess.run(
            [
                os.path.join(contract, "tools", "run.sh"),
                "all",
                "--case",
                "module.one",
            ],
            cwd=repository,
            env={
                **os.environ,
                "OUT_DIR": output_dir,
                "QJS_DIR": self.temporary.name,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                "FIX_CHECK_BASELINE": os.path.join(
                    contract, "baseline-quickjs-difference-allowlist.toml"
                ),
            },
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn(
            "check-contract-case-coverage (skipped: coverage requires the full case set)",
            completed.stdout,
        )
        self.assertIn("=> run-spidermonkey-contract", completed.stdout)
        self.assertIn("=> run-quickjs-contract", completed.stdout)
        self.assertIn("=> compare-engine-results", completed.stdout)
        for name in (
            "spidermonkey-contract-cases.json",
            "quickjs-contract-cases.json",
        ):
            with open(os.path.join(output_dir, name), encoding="utf-8") as file:
                self.assertEqual(json.load(file)["cases"], ["module.one"])

    def test_run_sh_all_continues_after_a_failed_step(self):
        repository, contract = self.write_run_sh_repo("repo-aggregate")
        bin_dir = os.path.join(self.temporary.name, "bin-failing")
        os.makedirs(bin_dir)
        fake_cargo = os.path.join(bin_dir, "cargo")
        self.write_fake_cargo(fake_cargo, "js-quickjs")
        output_dir = os.path.join(self.temporary.name, "out-aggregate")
        os.makedirs(output_dir)
        completed = subprocess.run(
            [os.path.join(contract, "tools", "run.sh"), "all"],
            cwd=repository,
            env={
                **os.environ,
                "OUT_DIR": output_dir,
                "QJS_DIR": self.temporary.name,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                "FIX_CHECK_BASELINE": os.path.join(
                    contract, "baseline-quickjs-difference-allowlist.toml"
                ),
            },
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 1, completed.stdout + completed.stderr)
        self.assertIn("==> run-spidermonkey-contract", completed.stdout)
        self.assertIn("==> run-quickjs-contract", completed.stdout)
        self.assertIn("==> compare-engine-results", completed.stdout)
        self.assertIn("==> check-contract-case-coverage", completed.stdout)
        self.assertIn("engine contract steps failed:", completed.stderr)

    def test_run_sh_all_passes_when_quickjs_fails_but_comparison_accepts(self):
        repository, contract = self.write_run_sh_repo("repo-known-difference")
        bin_dir = os.path.join(self.temporary.name, "bin-known-difference")
        os.makedirs(bin_dir)
        fake_cargo = os.path.join(bin_dir, "cargo")
        with open(fake_cargo, "w", encoding="utf-8") as file:
            file.write(
                """#!/usr/bin/env bash
set -eu
cat >"${CONTRACT_CASES_OUT:?}" <<'JSON'
{"version": 1, "cases": ["module.one"]}
JSON
cat >"${CONTRACT_OUT:?}" <<JSONL
{"id": "module.one", "engine": "${CONTRACT_ENGINE:-unknown}", "fields": {"value": true}, "exception": null, "apis": ["JS_Test"]}
JSONL
if [[ "$*" == *"js-quickjs"* ]]; then
    exit 1
fi
"""
            )
        os.chmod(fake_cargo, 0o755)
        output_dir = os.path.join(self.temporary.name, "out-known-difference")
        os.makedirs(output_dir)
        completed = subprocess.run(
            [
                os.path.join(contract, "tools", "run.sh"),
                "run-contract-gate",
                "--case",
                "module.one",
            ],
            cwd=repository,
            env={
                **os.environ,
                "OUT_DIR": output_dir,
                "QJS_DIR": self.temporary.name,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                "FIX_CHECK_BASELINE": os.path.join(
                    contract, "baseline-quickjs-difference-allowlist.toml"
                ),
            },
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("==> run-quickjs-contract", completed.stdout)
        self.assertIn(
            "check-contract-case-coverage (skipped: coverage requires the full case set)",
            completed.stdout,
        )
        self.assertIn("compared 1 cases", completed.stdout)
        self.assertIn("run-quickjs-contract exited non-zero", completed.stderr)
        self.assertNotIn("engine contract steps failed:", completed.stderr)

    def test_run_sh_all_runs_tool_tests_first(self):
        repository, contract = self.write_run_sh_repo(
            "repo-tool-tests",
            tool_tests=(
                "import unittest\n"
                "\n"
                "\n"
                "class ToolTest(unittest.TestCase):\n"
                "    def test_passes(self):\n"
                "        self.assertTrue(True)\n"
            ),
        )
        bin_dir = os.path.join(self.temporary.name, "bin-tool-tests")
        os.makedirs(bin_dir)
        self.write_fake_cargo(os.path.join(bin_dir, "cargo"), "__never__")
        output_dir = os.path.join(self.temporary.name, "out-tool-tests")
        os.makedirs(output_dir)
        completed = subprocess.run(
            [
                os.path.join(contract, "tools", "run.sh"),
                "run-contract-gate",
                "--case",
                "module.one",
            ],
            cwd=repository,
            env={
                **os.environ,
                "OUT_DIR": output_dir,
                "QJS_DIR": self.temporary.name,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                "FIX_CHECK_BASELINE": os.path.join(
                    contract, "baseline-quickjs-difference-allowlist.toml"
                ),
            },
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("==> run-tool-tests", completed.stdout)
        self.assertIn("Ran 1 test", completed.stderr)
        # The tool tests must run before the engines, so a broken manifest tool stops the gate
        # instead of letting a meaningless comparison pass.
        self.assertLess(
            completed.stdout.index("==> run-tool-tests"),
            completed.stdout.index("==> run-spidermonkey-contract"),
        )

    def test_run_sh_all_fails_when_a_tool_test_fails(self):
        repository, contract = self.write_run_sh_repo(
            "repo-tool-tests-failing",
            tool_tests=(
                "import unittest\n"
                "\n"
                "\n"
                "class ToolTest(unittest.TestCase):\n"
                "    def test_fails(self):\n"
                "        self.fail('manifest tool regression')\n"
            ),
        )
        bin_dir = os.path.join(self.temporary.name, "bin-tool-tests-failing")
        os.makedirs(bin_dir)
        self.write_fake_cargo(os.path.join(bin_dir, "cargo"), "__never__")
        output_dir = os.path.join(self.temporary.name, "out-tool-tests-failing")
        os.makedirs(output_dir)
        completed = subprocess.run(
            [
                os.path.join(contract, "tools", "run.sh"),
                "run-contract-gate",
                "--case",
                "module.one",
            ],
            cwd=repository,
            env={
                **os.environ,
                "OUT_DIR": output_dir,
                "QJS_DIR": self.temporary.name,
                "SERVO_DIR": os.path.join(repository, "servo"),
                "PATH": bin_dir + os.pathsep + os.environ["PATH"],
                "FIX_CHECK_BASELINE": os.path.join(
                    contract, "baseline-quickjs-difference-allowlist.toml"
                ),
            },
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 1, completed.stdout + completed.stderr)
        self.assertIn("engine contract steps failed: run-tool-tests", completed.stderr)


if __name__ == "__main__":
    unittest.main()
