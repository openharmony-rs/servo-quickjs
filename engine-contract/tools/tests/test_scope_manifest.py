# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import collections
import os
import sys
import unittest


sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import scope_manifest


class ReviewedUsageTest(unittest.TestCase):
    def test_reviewed_usage_adds_direct_and_indirect_entries(self):
        direct = collections.defaultdict(list)
        reached = collections.defaultdict(set)
        reviewed = {
            "JS_InitializePropertiesFromCompatibleNativeObject": {
                "mode": "direct",
                "evidence": "reviewed: wrap.rs:134",
            },
            "AppendToEnvironmentChain": {
                "mode": "indirect",
                "evidence": "reviewed: rust.rs:1263",
            },
        }
        entries = {
            "JS_InitializePropertiesFromCompatibleNativeObject",
            "AppendToEnvironmentChain",
        }

        scope_manifest.merge_reviewed_usage(direct, reached, reviewed, entries)

        self.assertEqual(
            direct["JS_InitializePropertiesFromCompatibleNativeObject"],
            ["reviewed: wrap.rs:134"],
        )
        self.assertEqual(reached["AppendToEnvironmentChain"], {"reviewed: rust.rs:1263"})
        self.assertNotIn("AppendToEnvironmentChain", direct)

    def test_unknown_reviewed_entry_is_rejected(self):
        with self.assertRaises(SystemExit):
            scope_manifest.merge_reviewed_usage(
                collections.defaultdict(list),
                collections.defaultdict(set),
                {"NotInManifest": {"mode": "indirect", "evidence": "reviewed: nowhere"}},
                {"JS_NewPlainObject"},
            )

    def test_rendered_mode_matches_the_checker_for_reviewed_entries(self):
        by_name = {
            "AddAssociatedMemory": [{
                "status": "implemented",
                "kind": "function",
                "signature": "AddAssociatedMemory()",
            }],
            "Other": [{
                "status": "implemented",
                "kind": "function",
                "signature": "Other()",
            }],
        }
        result = {
            "manifest": by_name,
            "direct": {"AddAssociatedMemory": ["servo/a.rs:1"]},
            "via": {"AddAssociatedMemory": {"reviewed: wrap.rs:1"}},
            "used": {"AddAssociatedMemory"},
            "reviewed": {
                "AddAssociatedMemory": {
                    "mode": "indirect",
                    "evidence": "reviewed: wrap.rs:1",
                }
            },
        }

        rendered = scope_manifest.render_scope(result)

        self.assertIn('mode = "indirect"', rendered)
        self.assertIn("reviewed: wrap.rs:1", rendered)

    def test_regeneration_hint_uses_the_public_scan_command(self):
        rendered = scope_manifest.render_scope(
            {"manifest": {}, "direct": {}, "via": {}, "used": set(), "reviewed": {}}
        )
        self.assertIn("engine-contract/tools/run.sh scan-mozjs-surface", rendered)

class ScanMozjsTest(unittest.TestCase):
    def scan(self, files):
        import tempfile

        with tempfile.TemporaryDirectory() as root:
            for name, body in files.items():
                path = os.path.join(root, name)
                os.makedirs(os.path.dirname(path), exist_ok=True)
                with open(path, "w", encoding="utf-8") as handle:
                    handle.write(body)
            return scope_manifest.scan_mozjs([root], {"One", "Two"}, lambda text: text)

    def test_trait_dispatch_through_deref_is_followed(self):
        calls, _deps = self.scan(
            {
                "vectors.rs": """
impl IdVector {
    fn handle_mut(&self) -> *mut Handle {
        GetIdVectorAddress(self.0)
    }
}
impl Deref for IdVector {
    type Target = [jsid];
    fn deref(&self) -> &[jsid] {
        SliceRootedIdVector(self.0, &mut length)
    }
}
"""
            }
        )

        self.assertIn("SliceRootedIdVector", calls["deref"])

    def test_same_name_method_on_two_types_is_not_followed_blindly(self):
        calls, _deps = self.scan(
            {
                "vectors.rs": """
impl EnvironmentChain {
    fn append(&self) {
        AppendToEnvironmentChain(self.chain, obj);
    }
}
impl RootedObjectVectorWrapper {
    fn append(&self) {
        AppendToRootedObjectVector(self.ptr, obj);
    }
}
fn write(final_: bool) {
    let _ = OpenOptions::new().append(true).open(path);
}
"""
            }
        )

        self.assertIn("AppendToEnvironmentChain", calls["append"])
        self.assertIn("AppendToRootedObjectVector", calls["append"])
        self.assertNotIn("append", calls["write"])

    def test_self_method_on_a_colliding_name_is_followed(self):
        calls, _deps = self.scan(
            {
                "vectors.rs": """
impl EnvironmentChain {
    fn append(&self) {
        self.append_inner(obj);
    }
    fn append_inner(&self, obj: *mut JSObject) {
        AppendToEnvironmentChain(self.chain, obj);
    }
}
impl RootedObjectVectorWrapper {
    fn append(&self) {
        AppendToRootedObjectVector(self.ptr, obj);
    }
}
"""
            }
        )

        self.assertIn("append_inner", calls["append"])
        self.assertIn("AppendToEnvironmentChain", calls["append_inner"])

    def test_quickjs_ffi_is_not_the_mozjs_entry(self):
        calls, _deps = self.scan(
            {"object.rs": "fn is_constructor(v: JSValue) -> bool { q::JS_IsConstructor(v) }"}
        )

        self.assertNotIn("JS_IsConstructor", calls["is_constructor"])

    def test_bodyless_declaration_does_not_swallow_the_next_function(self):
        calls, _deps = self.scan(
            {
                "wrappers.rs": """
wrap!(jsapi: pub fn JS_NewPlainObject(cx: &mut JSContext) -> bool);
fn real(cx: &mut JSContext) -> bool {
    JS_NewPlainObject(cx)
}
"""
            }
        )

        self.assertEqual(calls["real"], {"JS_NewPlainObject"})
        self.assertNotIn("JS_NewPlainObject", calls)



if __name__ == "__main__":
    unittest.main()
