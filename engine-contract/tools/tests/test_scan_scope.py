# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import scan_scope


class ScannerTest(unittest.TestCase):
    def write(self, root, relative, source):
        path = os.path.join(root, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as file:
            file.write(textwrap.dedent(source))

    def scan(self, files, manifest_names=("JS_NewPlainObject", "Construct1")):
        with tempfile.TemporaryDirectory() as root:
            self.write(root, "components/crate/Cargo.toml", "")
            for relative, source in files.items():
                self.write(root, relative, source)
            manifest = {name: [{"name": name, "status": "implemented"}] for name in manifest_names}
            return {entry["api"]: entry for entry in scan_scope.scan(root, manifest)}

    def test_comments_and_strings_do_not_count(self):
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    // Construct1(cx)
                    /* JS_NewPlainObject(cx) */
                    fn f() {
                        let _ = "Construct1(cx)";
                    }
                """
            }
        )
        self.assertEqual(found, {})

    def test_test_module_does_not_count(self):
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    #[cfg(test)]
                    mod tests {
                        use js::jsapi::JS_NewPlainObject;
                        fn f() { JS_NewPlainObject(); }
                    }
                """
            }
        )
        self.assertEqual(found, {})

    def test_direct_and_aliased_imports_count(self):
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    use js::jsapi::{JS_NewPlainObject as plain};
                    use js::rust::wrappers2::Construct1;

                    fn f() {
                        plain();
                        Construct1();
                    }
                """
            }
        )
        self.assertEqual(found["JS_NewPlainObject"]["provenance"][0]["line"], 6)
        self.assertEqual(found["Construct1"]["provenance"][0]["line"], 7)

    def test_reexport_is_followed(self):
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    mod reexport {
                        pub(crate) use js::jsapi::JS_NewPlainObject;
                    }
                    use crate::reexport::JS_NewPlainObject;

                    fn f() { JS_NewPlainObject(); }
                """
            }
        )
        self.assertIn("JS_NewPlainObject", found)

    def test_local_function_with_same_name_does_not_count(self):
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    fn Construct1() {}
                    fn f() { Construct1(); }
                """
            }
        )
        self.assertEqual(found, {})

    def test_manifest_status_is_preserved(self):
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    use js::jsapi::Empty;
                    fn f() { Empty(); }
                """
            },
            manifest_names=("Empty",),
        )
        with tempfile.TemporaryDirectory() as root:
            self.write(root, "components/crate/Cargo.toml", "")
            self.write(root, "components/crate/lib.rs", "use js::jsapi::Empty;\nfn f() { Empty(); }\n")
            report = scan_scope.scan(root, {"Empty": [{"name": "Empty", "status": "empty"}]})
        self.assertEqual(report[0]["status"], "empty")

    def test_module_import_forms_resolve_call_sites(self):
        """Every idiomatic way of importing a JS module must keep its call sites visible.

        `use js::jsapi;`, `use js::jsapi::{self}` and `use js::jsapi::*` all bring the module's
        names into scope. Dropping them silently shrinks the coverage scope, which is the failure
        mode that keeps a missing contract case hidden.
        """
        forms = {
            "plain_module": (
                "use js::jsapi;\nfn f() { jsapi::JS_NewPlainObject(); }\n",
                "JS_NewPlainObject",
            ),
            "self_in_list": (
                "use js::jsapi::{self};\nfn f() { jsapi::JS_NewPlainObject(); }\n",
                "JS_NewPlainObject",
            ),
            "self_with_items": (
                "use js::jsapi::{self, MimeType};\nfn f() { jsapi::JS_NewPlainObject(); }\n",
                "JS_NewPlainObject",
            ),
            "glob": (
                "use js::jsapi::*;\nfn f() { JS_NewPlainObject(); }\n",
                "JS_NewPlainObject",
            ),
            "aliased_module": (
                "use js::jsapi as jsapi;\nfn f() { jsapi::JS_NewPlainObject(); }\n",
                "JS_NewPlainObject",
            ),
        }
        for label, (source, expected) in forms.items():
            with self.subTest(label):
                found = self.scan({"components/crate/lib.rs": source})
                self.assertIn(expected, found, found)

    def test_glob_import_does_not_count_unrelated_names(self):
        """A glob import must not turn every identically named Rust function into a JS call."""
        found = self.scan(
            {
                "components/crate/lib.rs": """
                    use std::collections::*;
                    fn f() { HashMap::new(); }
                """
            }
        )
        self.assertEqual(found, {})

    def test_parse_use_tree_module_forms(self):
        cases = {
            "js::jsapi": [("js", "jsapi", "jsapi", False)],
            "js::jsapi::{self}": [("js::jsapi", "self", "jsapi", False)],
            "js::jsapi::*": [("js::jsapi", "*", "*", True)],
            "js::jsapi as api": [("js::jsapi", "self", "api", False)],
            "js::jsapi::Item as api": [("js::jsapi", "Item", "api", False)],
        }
        for tree, expected in cases.items():
            with self.subTest(tree):
                self.assertEqual(list(scan_scope.parse_use_tree(tree)), expected)

    def test_scope_file_generation_is_not_a_scanner_subcommand(self):
        """`scope_manifest.py` owns the scope file; this module must not offer to rewrite it."""
        completed = subprocess.run(
            [sys.executable, scan_scope.__file__, "--help"],
            text=True,
            capture_output=True,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("{scan}", completed.stdout)
        for command in ("check", "export"):
            refused = subprocess.run(
                [sys.executable, scan_scope.__file__, command, "--servo-dir", "."],
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn("invalid choice", refused.stderr)


if __name__ == "__main__":
    unittest.main()
