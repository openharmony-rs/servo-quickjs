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
fields = ["fields.value"]
reason = "QuickJS differs in this field"
owner = "servo-quickjs"
"""

ENTRY_WITHOUT_FIELDS = """
[[difference]]
id = "case.one"
reason = "QuickJS differs in this case"
owner = "servo-quickjs"
status = "accepted"
severity = "P3"
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
        issue = (
            'issue = "servo-quickjs#1"\n'
            if status in ("pending", "bug", "known-bug")
            else ""
        )
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
        self.assertIn("1 known QuickJS defects, 0 known QuickJS bugs, 0 unaccepted", output)

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

    def test_known_bug_is_reported_without_failing(self):
        result, output = self.run_compare("known-bug", "P2")
        self.assertEqual(result, 0, output)
        self.assertIn("known QuickJS bug: case.one", output)
        self.assertIn("servo-quickjs#1", output)
        self.assertIn("1 known QuickJS bugs, 0 unaccepted", output)

    def test_known_bug_requires_an_issue(self):
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER + 'status = "known-bug"\n' + 'severity = "P2"\n',
        )
        with self.assertRaisesRegex(SystemExit, "has no tracking issue"):
            compare.load_differences(differences)

    def test_p0_and_p1_cannot_be_released(self):
        for status in ("accepted", "wontfix", "known-bug"):
            for severity in ("P0", "P1"):
                with self.subTest(status=status, severity=severity):
                    with self.assertRaisesRegex(SystemExit, "cannot release"):
                        self.run_compare(status, severity)

    def record(self, name, fields, **keys):
        record = {
            "id": "case.one",
            "engine": self.engine_of(name),
            "fields": fields,
            "exception": None,
            "apis": ["JS_Test"],
        }
        record.update(keys)
        return self.write(name, json.dumps(record) + "\n")

    def entry(self, *lines):
        """An accepted P3 entry for `case.one` with the given extra TOML lines."""
        return self.write(
            "quickjs-difference-allowlist.toml",
            '[[difference]]\nid = "case.one"\n'
            + "".join(f"{line}\n" for line in lines)
            + 'reason = "QuickJS differs"\nowner = "servo-quickjs"\n'
            + 'status = "accepted"\nseverity = "P3"\n',
        )

    def compare_case(self, sm_fields, qjs_fields, *entry_lines, sm_keys=None, qjs_keys=None):
        sm = self.record("spidermonkey-contract-results.jsonl", sm_fields, **(sm_keys or {}))
        qjs = self.record("quickjs-contract-results.jsonl", qjs_fields, **(qjs_keys or {}))
        argv = ["compare.py", sm, qjs]
        if entry_lines:
            argv.append(self.entry(*entry_lines))
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(argv)
        return result, output.getvalue()

    def test_rejects_an_entry_that_releases_the_whole_observation(self):
        with self.assertRaisesRegex(SystemExit, "release the whole observation"):
            compare.load_differences(self.entry('fields = ["fields"]'))

    def test_rejects_an_unknown_field_name(self):
        for field in ("value", "exit", "fields.", "fields.panic", "fields.step_failures"):
            with self.subTest(field=field):
                with self.assertRaisesRegex(SystemExit, "unknown field"):
                    compare.load_differences(self.entry(f'fields = ["{field}"]'))

    def test_rejects_a_repeated_field(self):
        with self.assertRaisesRegex(SystemExit, "repeats a field"):
            compare.load_differences(self.entry('fields = ["fields.value", "fields.value"]'))

    def test_rejects_an_entry_that_releases_nothing(self):
        with self.assertRaisesRegex(SystemExit, "releases nothing"):
            compare.load_differences(self.entry())

    def test_rejects_steps_without_a_panic(self):
        with self.assertRaisesRegex(SystemExit, "steps without a panic"):
            compare.load_differences(self.entry('fields = ["fields.value"]', 'steps = ["one"]'))

    def test_rejects_an_unknown_entry_key(self):
        with self.assertRaisesRegex(SystemExit, r"unknown key\(s\) step;"):
            compare.load_differences(self.entry('fields = ["fields.value"]', 'step = ["one"]'))

    def test_compares_the_unlisted_fields_of_an_allowlisted_case(self):
        result, output = self.compare_case(
            {"value": True, "other": 1}, {"value": False, "other": 2}, 'fields = ["fields.value"]'
        )
        self.assertEqual(result, 1, output)
        self.assertIn("mismatch: case.one", output)
        self.assertIn("not covered: fields.other differs", output)

    def test_rejects_a_field_recorded_only_by_quickjs(self):
        result, output = self.compare_case(
            {"value": True}, {"value": False, "extra": 1}, 'fields = ["fields.value"]'
        )
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: fields.extra is recorded only by QuickJS", output)

    def test_rejects_missing_fields_without_a_panic(self):
        result, output = self.compare_case(
            {"value": True, "later": 1}, {"value": False}, 'fields = ["fields.value"]'
        )
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: fields.later is missing on QuickJS", output)

    def test_accepts_fields_lost_to_a_fingerprinted_panic(self):
        result, output = self.compare_case(
            {"value": True, "later": 1}, {"panic": "boom"}, 'panic = "boom"'
        )
        self.assertEqual(result, 0, output)
        self.assertIn("(status: accepted, severity: P3, panic: matched)", output)

    def test_rejects_a_panic_that_does_not_match_the_fingerprint(self):
        result, output = self.compare_case(
            {"value": True, "later": 1}, {"panic": "bang"}, 'panic = "boom"'
        )
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: QuickJS panicked: 'bang', the entry's fingerprint is 'boom'", output)
        self.assertIn("not covered: 2 SpiderMonkey fields are missing after the QuickJS panic", output)
        self.assertNotIn("is missing on QuickJS", output)

    def test_rejects_a_panic_without_a_fingerprint(self):
        result, output = self.compare_case(
            {"value": True}, {"panic": "bang"}, 'fields = ["fields.value"]'
        )
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: QuickJS panicked: 'bang'", output)
        self.assertNotIn("fingerprint is", output)

    def test_rejects_an_unlisted_top_level_difference(self):
        result, output = self.compare_case(
            {"value": True},
            {"value": False},
            'fields = ["fields.value"]',
            qjs_keys={"apis": ["JS_Other"]},
        )
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: apis differs", output)

    def test_rejects_an_unexplained_exit_behind_a_matching_panic(self):
        result, output = self.compare_case(
            {"value": True},
            {"panic": "boom"},
            'panic = "boom"',
            qjs_keys={"exit": "terminated by signal 11"},
        )
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: exit differs", output)

    def test_says_when_a_differing_case_has_no_entry(self):
        result, output = self.compare_case({"value": True}, {"value": False})
        self.assertEqual(result, 1, output)
        self.assertIn("not covered: no allowlist entry for this case", output)

    def assert_stale(self, result, output, reason):
        self.assertEqual(result, 1, output)
        self.assertIn("stale known QuickJS defect: case.one", output)
        self.assertIn(reason, output)
        self.assertNotIn("mismatch: case.one", output)

    def test_reports_a_listed_field_that_no_longer_differs_as_stale(self):
        result, output = self.compare_case(
            {"value": True, "other": 1},
            {"value": True, "other": 2},
            'fields = ["fields.value", "fields.other"]',
        )
        self.assert_stale(result, output, "fields.value no longer differs; remove it from `fields`")

    def test_reports_a_listed_top_level_key_that_no_longer_differs_as_stale(self):
        result, output = self.compare_case(
            {"value": True}, {"value": False}, 'fields = ["apis", "fields.value"]'
        )
        self.assert_stale(result, output, "apis no longer differs; remove it from `fields`")

    def test_reports_a_field_neither_engine_records_as_stale(self):
        result, output = self.compare_case(
            {"value": True}, {"value": False}, 'fields = ["fields.value", "fields.typo"]'
        )
        self.assert_stale(result, output, "fields.typo is recorded by neither engine")

    def test_reports_a_listed_field_lost_to_the_panic_as_stale(self):
        result, output = self.compare_case(
            {"value": True, "later": 1},
            {"panic": "boom"},
            'fields = ["fields.later"]',
            'panic = "boom"',
        )
        self.assert_stale(result, output, "fields.later is not recorded by QuickJS")

    def test_reports_a_panic_that_no_longer_happens_as_stale(self):
        result, output = self.compare_case(
            {"value": True}, {"value": False}, 'fields = ["fields.value"]', 'panic = "boom"'
        )
        self.assert_stale(result, output, "QuickJS no longer panics; remove `panic`")

    def test_reports_a_listed_step_that_no_longer_fails_as_stale(self):
        result, output = self.compare_case(
            {"value": True},
            {
                "panic": "assertion failed",
                "step_failures": [{"step": "one", "panic": "assertion failed"}],
            },
            'steps = ["one", "two"]',
            'panic = "assertion failed"',
        )
        self.assert_stale(result, output, "step two no longer fails; remove it from `steps`")

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
            DIFFERENCES_HEADER + 'status = "accepted"\n' + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1)
        self.assertIn("mismatch: case.one", output.getvalue())
        self.assertNotIn("stale known QuickJS defect: case.one", output.getvalue())

    def test_rejects_a_reference_panic_absorbed_by_a_case_level_entry(self):
        """A crash in the reference engine is a regression, not a QuickJS difference to accept."""
        sm = self.observation(
            "spidermonkey-contract-results.jsonl",
            fields={"panic": "spidermonkey reference crashed", "value": True},
        )
        qjs = self.observation("quickjs-contract-results.jsonl", fields={"value": False})
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER + 'status = "accepted"\n' + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1, output.getvalue())
        self.assertIn("mismatch: case.one", output.getvalue())
        self.assertNotIn("known QuickJS defect: case.one", output.getvalue())

    def test_rejects_a_reference_panic_even_with_a_matching_quickjs_fingerprint(self):
        """A `panic` fingerprint describes the QuickJS side; it cannot license a reference crash."""
        sm = self.observation(
            "spidermonkey-contract-results.jsonl",
            fields={"panic": "spidermonkey reference crashed", "value": True},
        )
        qjs = self.observation(
            "quickjs-contract-results.jsonl",
            fields={"panic": "assertion failed", "value": False},
        )
        differences = self.write(
            "quickjs-difference-allowlist.toml",
            DIFFERENCES_HEADER
            + 'panic = "assertion failed"\n'
            + 'status = "accepted"\n'
            + 'severity = "P3"\n',
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1, output.getvalue())
        self.assertIn("mismatch: case.one", output.getvalue())

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
            ENTRY_WITHOUT_FIELDS.replace(
                'owner =',
                'steps = ["old_step"]\npanic = "assertion `left == right` failed"\nowner =',
            ),
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            result = compare.main(["compare.py", sm, qjs, differences])
        self.assertEqual(result, 1, output.getvalue())
        self.assertIn("mismatch: case.one", output.getvalue())
        self.assertIn("step new_step failed but is not in `steps`", output.getvalue())

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
            ENTRY_WITHOUT_FIELDS.replace(
                'owner =', 'steps = ["one"]\npanic = "assertion failed"\nowner ='
            ),
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
            DIFFERENCES_HEADER.replace('fields = ["fields.value"]', 'fields = ["fields.value"]\nsteps = []')
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
            + 'fields = ["fields.value"]\n'
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
