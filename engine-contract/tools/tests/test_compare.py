# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import contextlib
import io
import json
import os
import sys
import tempfile
import textwrap
import unittest


sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import compare


DIFFERENCES_HEADER = """
[[difference]]
id = "case.one"
fields = ["fields"]
reason = "QuickJS differs in this field"
owner = "servo-quickjs"
"""


class CompareTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)

    def write(self, name, content):
        path = os.path.join(self.temporary.name, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(textwrap.dedent(content))
        return path

    @staticmethod
    def engine_of(name):
        return "spidermonkey" if name.startswith("spidermonkey") else "quickjs"

    def coverage(self, name, fields):
        return self.write(
            name,
            json.dumps(
                {
                    "id": "case.one",
                    "engine": self.engine_of(name),
                    "fields": fields,
                    "exception": None,
                    "apis": ["JS_Test"],
                }
            )
            + "\n",
        )

    def observation(self, name, case_id="case.one", fields=None):
        return self.write(
            name,
            json.dumps(
                {
                    "id": case_id,
                    "engine": self.engine_of(name),
                    "fields": fields or {"value": True},
                    "exception": None,
                    "apis": ["JS_Test"],
                }
            )
            + "\n",
        )

    def run_compare(self, status, severity, qjs_fields=None):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", qjs_fields or {"value": False})
        issue = 'issue = "servo-quickjs#1"\n' if status in ("pending", "bug") else ""
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + issue
            + f'status = "{status}"\n'
            + f'severity = "{severity}"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        return result, output.getvalue()

    def test_releases_accepted_difference(self):
        result, output = self.run_compare("accepted", "P3")
        self.assertEqual(result, 0, output)
        self.assertIn("1 known QuickJS defects, 0 unaccepted", output)

    def test_releases_wontfix_difference(self):
        result, output = self.run_compare("wontfix", "P2")
        self.assertEqual(result, 0, output)

    def test_pending_difference_fails(self):
        result, output = self.run_compare("pending", "P3")
        self.assertEqual(result, 1)
        self.assertIn("unaccepted QuickJS defect: case.one", output)

    def test_bug_difference_fails(self):
        result, output = self.run_compare("bug", "P2")
        self.assertEqual(result, 1)
        self.assertIn("status: bug", output)

    def test_p0_and_p1_cannot_be_released(self):
        for severity in ("P0", "P1"):
            with self.subTest(severity=severity):
                with self.assertRaisesRegex(SystemExit, "cannot release"):
                    self.run_compare("accepted", severity)

    def test_rejects_an_empty_results_file(self):
        sm = self.write("spidermonkey-contract-results.jsonl", "")
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": True})
        with self.assertRaisesRegex(SystemExit, "no cases recorded"):
            compare.main(["compare.py", sm, qjs])

    def test_difference_requires_reason_owner_status_and_severity(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            """
            [[difference]]
            id = "case.one"
            fields = ["fields"]
            """,
        )
        with self.assertRaisesRegex(SystemExit, "missing reason, owner, status, severity"):
            compare.main(["compare.py", sm, qjs, differences])

    def test_rejects_unknown_status(self):
        with self.assertRaisesRegex(SystemExit, "unknown status 'ignore'"):
            self.run_compare("ignore", "P3")

    def test_rejects_unknown_severity(self):
        with self.assertRaisesRegex(SystemExit, "unknown severity 'major'"):
            self.run_compare("accepted", "major")

    def test_pending_difference_requires_an_issue(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'status = "pending"\n'
            + 'severity = "P3"\n',
        )
        with self.assertRaisesRegex(SystemExit, "has no tracking issue"):
            compare.main(["compare.py", sm, qjs, differences])

    def test_rejects_an_empty_issue(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'issue = ""\n'
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        with self.assertRaisesRegex(SystemExit, "empty issue"):
            compare.main(["compare.py", sm, qjs, differences])

    def test_accepts_an_issue_reference(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'issue = "servo-quickjs#123"\n'
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 0, output.getvalue())
        self.assertIn("issue: servo-quickjs#123", output.getvalue())

    def test_rejects_duplicate_difference_ids(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        entry = (
            DIFFERENCES_HEADER
            + 'status = "accepted"\n'
            + 'severity = "P3"\n'
        )
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            entry + entry,
        )
        with self.assertRaisesRegex(SystemExit, "duplicate difference id 'case.one'"):
            compare.main(["compare.py", sm, qjs, differences])

    def test_rejects_a_difference_that_no_longer_differs(self):
        sm = self.observation("spidermonkey-contract-results.jsonl")
        qjs = self.observation("quickjs-contract-results.jsonl")
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1)
        self.assertIn("stale known QuickJS defect: case.one", output.getvalue())
        self.assertIn("listed case no longer differs", output.getvalue())

    def test_rejects_a_difference_when_only_listed_fields_match(self):
        sm = self.observation("spidermonkey-contract-results.jsonl", fields={"value": True, "extra": 1})
        qjs = self.observation("quickjs-contract-results.jsonl", fields={"value": False, "extra": 2})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            .replace('fields = ["fields"]', 'fields = ["value"]')
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1)
        self.assertIn("mismatch: case.one", output.getvalue())
        self.assertNotIn("stale known QuickJS defect: case.one", output.getvalue())

    def test_rejects_a_new_step_failure_hidden_by_a_case_level_entry(self):
        sm = self.observation("spidermonkey-contract-results.jsonl", fields={"value": True})
        qjs = self.observation(
            "quickjs-contract-results.jsonl",
            fields={
                "panic": "assertion `left == right` failed",
                "step_failures": [
                    {"step": "old_step", "panic": "assertion `left == right` failed"},
                    {"step": "new_step", "panic": "assertion failed: new divergence"},
                ],
            },
        )
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            .replace('fields = ["fields"]', 'fields = ["fields"]\nsteps = ["old_step"]')
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1, output.getvalue())
        self.assertIn("mismatch: case.one", output.getvalue())

    def test_releases_an_entry_that_names_every_failed_step(self):
        sm = self.observation("spidermonkey-contract-results.jsonl", fields={"value": True})
        qjs = self.observation(
            "quickjs-contract-results.jsonl",
            fields={
                "panic": "assertion failed",
                "step_failures": [{"step": "one", "panic": "assertion failed"}],
            },
        )
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            .replace('fields = ["fields"]', 'fields = ["fields"]\nsteps = ["one"]')
            + 'panic = "assertion failed"\n'
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 0, output.getvalue())

    def test_rejects_an_empty_steps_list(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            .replace('fields = ["fields"]', 'fields = ["fields"]\nsteps = []')
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        with self.assertRaisesRegex(SystemExit, "must list non-empty step names"):
            compare.main(["compare.py", sm, qjs, differences])

    def inventory(self, cases):
        path = os.path.join(self.temporary.name, "cases.json")
        with open(path, "w", encoding="utf-8") as file:
            json.dump({"version": 1, "cases": cases}, file)
        return path

    def test_requires_every_inventoried_case_in_both_runs(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": True})
        cases = self.inventory(["case.one", "case.two"])
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, "--cases", cases])
        self.assertEqual(result, 1)
        self.assertIn("missing from SpiderMonkey case inventory: case.two", output.getvalue())
        self.assertIn("missing from QuickJS case inventory: case.two", output.getvalue())

    def test_rejects_cases_outside_the_inventory(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": True})
        cases = self.inventory(["case.two"])
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, "--cases", cases])
        self.assertEqual(result, 1)
        self.assertIn("unexpected in SpiderMonkey case inventory: case.one", output.getvalue())

    def test_rejects_a_difference_for_a_case_missing_from_complete_inventories(self):
        sm = self.observation("spidermonkey-contract-results.jsonl", case_id="case.two")
        qjs = self.observation("quickjs-contract-results.jsonl", case_id="case.two")
        cases = self.inventory(["case.two"])
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(
                ["compare.py", sm, qjs, differences, "--cases", cases]
            )
        self.assertEqual(result, 1)
        self.assertIn("stale known QuickJS defect: case.one", output.getvalue())
        self.assertIn("listed case no longer exists", output.getvalue())

    def test_does_not_report_a_difference_for_a_case_outside_a_selected_run(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": False})
        selected_path = os.path.join(self.temporary.name, "selected-cases.json")
        with open(selected_path, "w", encoding="utf-8") as file:
            json.dump(
                {"version": 1, "cases": ["case.one"], "complete": False},
                file,
            )
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'status = "accepted"\n'
            + 'severity = "P3"\n'
            + '\n[[difference]]\n'
            + 'id = "case.two"\n'
            + 'fields = ["fields"]\n'
            + 'reason = "not selected"\n'
            + 'owner = "servo-quickjs"\n'
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(
                [
                    "compare.py",
                    sm,
                    qjs,
                    differences,
                    "--sm-cases",
                    selected_path,
                    "--qjs-cases",
                    selected_path,
                ]
            )
        self.assertEqual(result, 0, output.getvalue())
        self.assertNotIn("stale known QuickJS defect: case.two", output.getvalue())

    def test_rejects_an_unsupported_case_inventory(self):
        sm = self.coverage("spidermonkey-contract-results.jsonl", {"value": True})
        qjs = self.coverage("quickjs-contract-results.jsonl", {"value": True})
        path = os.path.join(self.temporary.name, "cases.json")
        with open(path, "w", encoding="utf-8") as file:
            json.dump({"version": 2, "cases": ["case.one"]}, file)
        with self.assertRaisesRegex(SystemExit, "unsupported case inventory version"):
            compare.main(["compare.py", sm, qjs, "--cases", path])


if __name__ == "__main__":
    unittest.main()
