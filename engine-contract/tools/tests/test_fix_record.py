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
import fix_record


BASE = """
[[difference]]
id = "module.one"
fields = ["fields"]
reason = "old"
owner = "servo-quickjs"
status = "accepted"
severity = "P3"

[[difference]]
id = "module.two"
fields = ["fields"]
reason = "still present"
owner = "servo-quickjs"
status = "accepted"
severity = "P3"
"""

CURRENT = """
[[difference]]
id = "module.two"
fields = ["fields"]
reason = "still present"
owner = "servo-quickjs"
status = "accepted"
severity = "P3"
"""

RECORD = """# Fixes

## Template

```markdown
## <case id>
- Date: YYYY-MM-DD
```

## Records

## module.one

- Date: 2026-11-14
- Owner: shim team
- Issue: servo-quickjs#123
- Previous status: accepted
- Previous severity: P3
- Fixed in: abc1234
- Shim change: implemented the missing entry point
- Regression: `run.sh run-spidermonkey-contract --case module.one && run.sh run-quickjs-contract --case module.one && run.sh compare-engine-results`
- Result: `0 mismatches, 0 unaccepted, 0 missing`
- Follow-up: none
"""


class FixRecordTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)

    def write(self, name, content):
        path = os.path.join(self.temporary.name, name)
        with open(path, "w", encoding="utf-8") as file:
            file.write(textwrap.dedent(content))
        return path

    def run_check(self, baseline=BASE, current=CURRENT, record=RECORD, cases=None, complete=True):
        baseline_path = self.write("baseline.toml", baseline)
        current_path = self.write("quickjs-difference-allowlist.toml", current)
        record_path = self.write("quickjs-shim-fix-records.md", record)
        arguments = [
            "check",
            "--baseline",
            baseline_path,
            "--allowlist",
            current_path,
            "--record",
            record_path,
        ]
        if cases is not None:
            cases_path = os.path.join(self.temporary.name, "cases.json")
            with open(cases_path, "w", encoding="utf-8") as file:
                json.dump({"version": 1, "cases": cases, "complete": complete}, file)
            arguments.extend(["--cases", cases_path])
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = fix_record.main(arguments)
        return result, output.getvalue()

    def test_accepts_a_complete_record_for_a_removed_difference(self):
        result, output = self.run_check(cases=["module.one", "module.two"])
        self.assertEqual(result, 0, output)
        self.assertIn("1 removed difference(s), 1 record(s), 0 problems", output)

    def test_rejects_a_removed_difference_without_a_record(self):
        result, output = self.run_check(record="## Records\n")
        self.assertEqual(result, 1)
        self.assertIn("removed difference 'module.one' has no fix record", output)

    def test_ignores_the_fenced_template(self):
        result, output = self.run_check(cases=["module.one", "module.two"])
        self.assertEqual(result, 0, output)

    def test_rejects_a_record_with_a_template_placeholder(self):
        record = RECORD.replace("implemented the missing entry point", "<what changed>")
        result, output = self.run_check(record=record, cases=["module.one", "module.two"])
        self.assertEqual(result, 1)
        self.assertIn("template placeholder in Shim change", output)

    def test_rejects_a_missing_required_field(self):
        record = RECORD.replace("- Result: `0 mismatches, 0 unaccepted, 0 missing`\n", "")
        result, output = self.run_check(record=record, cases=["module.one", "module.two"])
        self.assertEqual(result, 1)
        self.assertIn("missing Result", output)

    def test_rejects_an_unknown_previous_status(self):
        record = RECORD.replace("Previous status: accepted", "Previous status: fixed")
        result, output = self.run_check(record=record, cases=["module.one", "module.two"])
        self.assertEqual(result, 1)
        self.assertIn("unknown previous status 'fixed'", output)

    def test_rejects_a_record_outside_the_runtime_inventory(self):
        result, output = self.run_check(cases=["module.two"])
        self.assertEqual(result, 1)
        self.assertIn("record 'module.one' is not in the runtime case inventory", output)

    def test_accepts_records_outside_a_selected_inventory(self):
        result, output = self.run_check(cases=["module.two"], complete=False)
        self.assertEqual(result, 0, output)
        self.assertIn("1 removed difference(s), 1 record(s), 0 problems", output)

    def test_rejects_a_non_boolean_inventory_complete_flag(self):
        baseline_path = self.write("baseline.toml", BASE)
        current_path = self.write("quickjs-difference-allowlist.toml", CURRENT)
        record_path = self.write("quickjs-shim-fix-records.md", RECORD)
        cases_path = os.path.join(self.temporary.name, "cases.json")
        with open(cases_path, "w", encoding="utf-8") as file:
            json.dump({"version": 1, "cases": ["module.one"], "complete": "yes"}, file)
        with self.assertRaisesRegex(SystemExit, "expected 'complete' to be a boolean"):
            fix_record.main(
                [
                    "check",
                    "--baseline",
                    baseline_path,
                    "--allowlist",
                    current_path,
                    "--record",
                    record_path,
                    "--cases",
                    cases_path,
                ]
            )

    def test_rejects_a_missing_records_section(self):
        with self.assertRaisesRegex(SystemExit, "missing '## Records' section"):
            self.run_check(record="# Fixes\n")

    def test_reads_the_pre_rename_allowlist_from_git(self):
        repository = os.path.join(self.temporary.name, "repo")
        contract = os.path.join(repository, "engine-contract")
        os.makedirs(contract)
        old_path = os.path.join(contract, "known-differences.toml")
        new_path = os.path.join(contract, "quickjs-difference-allowlist.toml")
        with open(old_path, "w", encoding="utf-8") as file:
            file.write(BASE)
        for command in (
            ["git", "init", "--quiet"],
            ["git", "config", "user.email", "test@example.invalid"],
            ["git", "config", "user.name", "test"],
            ["git", "add", "engine-contract/known-differences.toml"],
            ["git", "commit", "--quiet", "-m", "old allowlist"],
        ):
            subprocess.run(command, cwd=repository, check=True, capture_output=True)
        os.rename(old_path, new_path)
        with open(new_path, "w", encoding="utf-8") as file:
            file.write(CURRENT)

        current_path = self.write("current.toml", CURRENT)
        record_path = self.write("record.md", RECORD)
        cases_path = self.write("cases.json", '{"version": 1, "cases": ["module.one", "module.two"]}')
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            previous = os.getcwd()
            os.chdir(repository)
            try:
                result = fix_record.main(
                    [
                        "check",
                        "--baseline",
                        "HEAD:engine-contract/quickjs-difference-allowlist.toml",
                        "--allowlist",
                        current_path,
                        "--record",
                        record_path,
                        "--cases",
                        cases_path,
                    ]
                )
            finally:
                os.chdir(previous)
        self.assertEqual(result, 0, output)
        self.assertIn("using pre-rename baseline", output.getvalue())


if __name__ == "__main__":
    unittest.main()
