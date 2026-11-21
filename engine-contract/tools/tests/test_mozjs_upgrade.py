# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import mozjs_upgrade


SHIM_CARGO = """\
[package]
name = "mozjs"
version = "0.26.14"
"""


class ShimCheckTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = self.temporary.name
        self.qjs = os.path.join(self.root, "servo-quickjs")
        for name in mozjs_upgrade.REQUIRED_SHIM_FILES:
            self.write(os.path.join(self.qjs, name), "")
        self.write(os.path.join(self.qjs, "mozjs", "Cargo.toml"), SHIM_CARGO)
        self.git("init", "--quiet")
        self.git("add", ".")
        self.git(
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "shim",
        )

    def tearDown(self):
        self.temporary.cleanup()

    def write(self, path, contents):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as file:
            file.write(contents)

    def git(self, *arguments):
        subprocess.run(["git", "-C", self.qjs, *arguments], check=True, capture_output=True)

    def test_check_accepts_a_matching_shim(self):
        args = type("Args", (), {"qjs_dir": self.qjs, "version": "0.26.14"})()

        self.assertEqual(mozjs_upgrade.check(args), 0)

    def test_check_rejects_a_stale_shim_version(self):
        args = type("Args", (), {"qjs_dir": self.qjs, "version": "0.26.15"})()

        with self.assertRaises(SystemExit) as raised:
            mozjs_upgrade.check(args)

        self.assertIn("0.26.14", str(raised.exception))
        self.assertIn("land the shim", str(raised.exception))

    def test_check_rejects_a_missing_sparse_checkout_file(self):
        os.remove(os.path.join(self.qjs, "mozjs", "stub_bindings.py"))
        args = type("Args", (), {"qjs_dir": self.qjs, "version": "0.26.14"})()

        with self.assertRaises(SystemExit) as raised:
            mozjs_upgrade.check(args)

        self.assertIn("stub_bindings.py", str(raised.exception))


class PinUpdateTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = self.temporary.name
        self.servo = os.path.join(self.root, "servo")
        self.qjs = os.path.join(self.root, "servo-quickjs")
        self.write(
            os.path.join(self.servo, "Cargo.toml"),
            '[workspace.dependencies]\n'
            'js = { package = "mozjs", version = "=0.26.13", default-features = false }\n'
            'other = "1"\n',
        )
        self.write(
            os.path.join(self.servo, "support", "quickjs", "cargo-config.toml"),
            "# comment\n"
            "[patch.crates-io]\n"
            'mozjs = { git = "https://github.com/openharmony-rs/servo-quickjs", '
            'rev = "544db84d9210b332e98baa8bb39a51c4c769001d" }\n',
        )
        self.write(os.path.join(self.qjs, "mozjs", "Cargo.toml"), SHIM_CARGO)
        subprocess.run(["git", "-C", self.qjs, "init", "--quiet"], check=True)
        subprocess.run(
            ["git", "-C", self.qjs, "add", "."], check=True, capture_output=True
        )
        subprocess.run(
            [
                "git",
                "-C",
                self.qjs,
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "shim",
            ],
            check=True,
            capture_output=True,
        )

    def tearDown(self):
        self.temporary.cleanup()

    def write(self, path, contents):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as file:
            file.write(contents)

    def read(self, path):
        with open(path, encoding="utf-8") as file:
            return file.read()

    def test_update_pins_keeps_the_exact_requirement_and_uses_the_shim_head(self):
        head = mozjs_upgrade.shim_head(self.qjs)

        mozjs_upgrade.update_pins(self.servo, self.qjs, "0.26.14")

        cargo = self.read(os.path.join(self.servo, "Cargo.toml"))
        config = self.read(
            os.path.join(self.servo, "support", "quickjs", "cargo-config.toml")
        )
        self.assertIn('js = { package = "mozjs", version = "=0.26.14"', cargo)
        self.assertNotIn("==0.26.14", cargo)
        self.assertIn(f'rev = "{head}"', config)

    def test_update_pins_is_idempotent_after_the_first_run(self):
        mozjs_upgrade.update_pins(self.servo, self.qjs, "0.26.14")
        first_cargo = self.read(os.path.join(self.servo, "Cargo.toml"))
        first_config = self.read(
            os.path.join(self.servo, "support", "quickjs", "cargo-config.toml")
        )

        mozjs_upgrade.update_pins(self.servo, self.qjs, "0.26.14")

        self.assertEqual(
            self.read(os.path.join(self.servo, "Cargo.toml")), first_cargo
        )
        self.assertEqual(
            self.read(os.path.join(self.servo, "support", "quickjs", "cargo-config.toml")),
            first_config,
        )

    def test_update_pins_rejects_a_manifest_without_the_js_dependency(self):
        self.write(
            os.path.join(self.servo, "Cargo.toml"),
            '[workspace.dependencies]\nother = "1"\n',
        )

        with self.assertRaises(SystemExit) as raised:
            mozjs_upgrade.update_pins(self.servo, self.qjs, "0.26.14")

        self.assertIn("expected one", str(raised.exception))

    def test_apply_restores_every_file_when_cargo_update_fails(self):
        cargo = os.path.join(self.servo, "Cargo.toml")
        config = os.path.join(self.servo, "support", "quickjs", "cargo-config.toml")
        lockfile = os.path.join(self.servo, "Cargo.lock")
        self.write(lockfile, "lockfile before\n")
        before = {path: self.read(path) for path in (cargo, config, lockfile)}
        args = mock.Mock(
            servo_root=self.servo,
            qjs_dir=self.qjs,
            version="0.26.14",
            skip_cargo_update=False,
        )

        with mock.patch.object(mozjs_upgrade, "update_lockfile", return_value=False):
            with self.assertRaises(SystemExit) as raised:
                mozjs_upgrade.apply(args)

        self.assertIn("restored", str(raised.exception))
        for path, contents in before.items():
            self.assertEqual(self.read(path), contents)

    def test_apply_restores_earlier_pin_when_the_second_pin_is_missing(self):
        cargo = os.path.join(self.servo, "Cargo.toml")
        config = os.path.join(self.servo, "support", "quickjs", "cargo-config.toml")
        before = self.read(cargo)
        self.write(config, "# no patch here\n")
        args = mock.Mock(
            servo_root=self.servo,
            qjs_dir=self.qjs,
            version="0.26.14",
            skip_cargo_update=False,
        )

        with self.assertRaises(SystemExit):
            mozjs_upgrade.apply(args)

        self.assertEqual(self.read(cargo), before)

    def test_apply_removes_a_lockfile_created_by_a_failed_update(self):
        lockfile = os.path.join(self.servo, "Cargo.lock")
        self.assertFalse(os.path.exists(lockfile))
        args = mock.Mock(
            servo_root=self.servo,
            qjs_dir=self.qjs,
            version="0.26.14",
            skip_cargo_update=False,
        )

        def create_then_fail(_servo_root):
            self.write(lockfile, "partial lockfile\n")
            return False

        with mock.patch.object(
            mozjs_upgrade, "update_lockfile", side_effect=create_then_fail
        ):
            with self.assertRaises(SystemExit):
                mozjs_upgrade.apply(args)

        self.assertFalse(os.path.exists(lockfile))


class UpgradeReportTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = self.temporary.name
        self.out_dir = os.path.join(self.root, "target", "contract")
        self.contract = os.path.join(self.root, "engine-contract")
        self.write(
            os.path.join(self.out_dir, "mozjs-api-changes.toml"),
            "[manifest]\nchanges = 2\n\n"
            "[[change]]\n"
            'kind = "signature"\nname = "UsedEverywhere"\nstatus = "implemented"\n\n'
            "[[change]]\n"
            'kind = "added"\nname = "UnusedNew"\nstatus = "todo"\n',
        )
        self.write(
            os.path.join(self.contract, "servo-mozjs-api-usage.toml"),
            "[scope]\nentries = 1\n\n"
            "[[entry]]\n"
            'name = "UsedEverywhere"\nstatus = "implemented"\nmode = "direct"\n'
            'provenance = ["components/script/x.rs:1"]\n',
        )
        self.write(
            os.path.join(self.contract, "property_test.rs"),
            'record_case(engine, "property.used", &["UsedEverywhere"], |cx, global| {});\n',
        )

    def tearDown(self):
        self.temporary.cleanup()

    def write(self, path, contents):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as file:
            file.write(contents)

    def self_args(self):
        return type(
            "Args", (), {"servo_root": self.root, "qjs_dir": self.root, "out_dir": self.out_dir}
        )()

    def report(self):
        with mock.patch.object(mozjs_upgrade, "CONTRACT_DIR", self.contract):
            return mozjs_upgrade.report(self.self_args())

    def test_report_marks_used_entries_for_update_and_skips_unused_ones(self):
        with redirect_stdout() as output:
            self.report()

        self.assertIn("1 changed entries used by Servo", output())
        self.assertIn("UPDATE UsedEverywhere [implemented]", output())
        self.assertIn("property_test.rs", output())
        self.assertIn("1 unused changed entries need no case", output())
        self.assertNotIn("UnusedNew [", output())
        self.assertIn("engine-contract/tools/run.sh run-contract-gate", output())

    def test_report_marks_a_used_entry_without_a_case_for_addition(self):
        self.write(
            os.path.join(self.contract, "property_test.rs"),
            'record_case(engine, "property.other", &["Other"], |cx, global| {});\n',
        )

        with redirect_stdout() as output:
            self.report()

        self.assertIn("ADD    UsedEverywhere [implemented]", output())

    def test_report_says_no_cases_are_needed_when_nothing_changed(self):
        self.write(
            os.path.join(self.root, "target", "contract", "mozjs-api-changes.toml"),
            "[manifest]\nchanges = 0\n",
        )

        with redirect_stdout() as output:
            self.report()

        self.assertIn("no cases are affected", output())


class PrepareEntryPointTest(unittest.TestCase):
    def test_run_sh_dispatches_prepare_mozjs_upgrade(self):
        script = os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "run.sh"
        )

        completed = subprocess.run(
            [script, "prepare-mozjs-upgrade", "--unknown"],
            text=True,
            capture_output=True,
        )

        self.assertEqual(completed.returncode, 2)
        self.assertIn("unknown argument", completed.stderr)
        self.assertNotIn("usage: ", completed.stderr)


@contextlib.contextmanager
def redirect_stdout():
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        yield buffer.getvalue


if __name__ == "__main__":
    unittest.main()
