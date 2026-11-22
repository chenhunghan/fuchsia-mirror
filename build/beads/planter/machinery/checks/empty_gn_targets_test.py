#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for empty_gn_targets.sh."""

import json
import os
import subprocess
import tempfile
import unittest

CHECKS_DIR = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(CHECKS_DIR, "empty_gn_targets.sh")
FUCHSIA_DIR = os.path.normpath(os.path.join(CHECKS_DIR, *[os.pardir] * 5))

HEADER = """\
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""


def _git(repo: str, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", repo, *args], text=True
    ).strip()


def _write_file(repo: str, rel_path: str, content: str) -> None:
    full_path = os.path.join(repo, rel_path)
    os.makedirs(os.path.dirname(full_path), exist_ok=True)
    with open(full_path, "w", encoding="utf-8") as f:
        f.write(content)


def _init_repo(repo: str, files: dict[str, str]) -> str:
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "test@example.com")
    _git(repo, "config", "user.name", "Test User")
    for rel_path, content in files.items():
        _write_file(repo, rel_path, content)
    _git(repo, "add", ".")
    _git(repo, "commit", "-q", "-m", "base")
    return _git(repo, "rev-parse", "HEAD")


def _run(repo: str, base_sha: str, changed_files: list[str]) -> list[dict]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")}
    env.update(
        {
            "PLANTER_WORKDIR": repo,
            "PLANTER_CHANGE_BASE": base_sha,
            "PLANTER_RESOLVED_CHANGE_BASE": base_sha,
            "PLANTER_CHANGED_FILES": "\n".join(changed_files),
        }
    )
    out = subprocess.run(
        ["bash", SCRIPT], env=env, capture_output=True, text=True, check=True
    )
    return json.loads(out.stdout)


@unittest.skipUnless(
    os.path.isfile(os.path.join(FUCHSIA_DIR, "scripts", "fx")), "fx not found"
)
class EmptyGnTargetsTest(unittest.TestCase):
    def test_emptied_leaf_and_forwarding_files_flagged(self):
        with tempfile.TemporaryDirectory() as repo:
            base = _init_repo(
                repo,
                {
                    "examples/power/BUILD.gn": HEADER
                    + 'group("power") {\n'
                    + '  deps = [\n    "other",\n    "suspend_config",\n  ]\n}\n\n'
                    + 'group("tests") {\n'
                    + '  deps = [\n    "other:tests",\n    "suspend_config:tests",\n  ]\n}\n',
                    "examples/power/suspend_config/BUILD.gn": HEADER
                    + 'group("suspend_config") {\n  deps = [ "rust" ]\n}\n\n'
                    + 'group("tests") {\n  testonly = true\n  deps = [ "rust:tests" ]\n}\n',
                    "examples/power/suspend_config/rust/BUILD.gn": HEADER
                    + 'group("rust") {\n  deps = [ ":bin" ]\n}\n\n'
                    + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n\n'
                    + 'group("tests") {\n  testonly = true\n  deps = [ ":bin" ]\n}\n',
                },
            )
            leaf = "examples/power/suspend_config/rust/BUILD.gn"
            _write_file(
                repo,
                leaf,
                HEADER
                + 'group("rust") {\n  testonly = true\n}\n\n'
                + 'group("tests") {\n  testonly = true\n}\n',
            )
            findings = _run(repo, base, [leaf])
            by_file = {f["file"]: f for f in findings}
            self.assertEqual(
                sorted(by_file),
                ["examples/power/suspend_config/BUILD.gn", leaf],
                findings,
            )
            for f in findings:
                self.assertEqual(f["category"], "empty_gn_build_file")
                self.assertEqual(f["severity"], "error")
            forwarder = by_file["examples/power/suspend_config/BUILD.gn"]
            self.assertIn(
                "examples/power/BUILD.gn:8, examples/power/BUILD.gn:15",
                forwarder["remediation"],
            )

    def test_emptied_group_in_file_with_other_targets(self):
        with tempfile.TemporaryDirectory() as repo:
            rel = "src/foo/BUILD.gn"
            base = _init_repo(
                repo,
                {
                    rel: HEADER
                    + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n\n'
                    + 'group("tests") {\n  testonly = true\n  deps = [ ":bin_test" ]\n}\n',
                },
            )
            _write_file(
                repo,
                rel,
                HEADER
                + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n\n'
                + 'group("tests") {\n  testonly = true\n}\n',
            )
            findings = _run(repo, base, [rel])
            self.assertEqual(len(findings), 1, findings)
            self.assertEqual(findings[0]["category"], "empty_gn_group")
            self.assertEqual(findings[0]["line"], 9)
            self.assertIn("//src/foo:tests", findings[0]["message"])

    def test_preexisting_and_opted_out_empty_groups_not_flagged(self):
        with tempfile.TemporaryDirectory() as repo:
            rel = "zircon/kernel/lib/foo/BUILD.gn"
            base = _init_repo(
                repo,
                {
                    rel: HEADER
                    + 'source_set("foo") {\n  sources = [ "foo.cc" ]\n}\n\n'
                    + 'group("boot_tests") {\n  testonly = true\n}\n\n'
                    + 'group("tests") {\n  testonly = true\n  deps = [ ":foo" ]\n}\n',
                },
            )
            _write_file(
                repo,
                rel,
                HEADER
                + 'source_set("foo") {\n  sources = [ "foo.cc", "bar.cc" ]\n}\n\n'
                + 'group("boot_tests") {\n  testonly = true\n}\n\n'
                + "# Intentionally empty: builders reference this group.\n"
                + 'group("tests") {\n  testonly = true\n}\n',
            )
            self.assertEqual(_run(repo, base, [rel]), [])

    def test_file_left_with_only_imports_flagged(self):
        with tempfile.TemporaryDirectory() as repo:
            rel = "src/bar/BUILD.gn"
            base = _init_repo(
                repo,
                {
                    rel: HEADER
                    + 'import("//build/rust/rustc_library.gni")\n\n'
                    + 'rustc_library("bar") {\n  sources = [ "src/lib.rs" ]\n}\n',
                },
            )
            _write_file(
                repo, rel, HEADER + 'import("//build/rust/rustc_library.gni")\n'
            )
            findings = _run(repo, base, [rel])
            self.assertEqual(
                [(f["category"], f["file"]) for f in findings],
                [("empty_gn_build_file", rel)],
            )

    def test_emptied_group_inside_condition_flagged(self):
        with tempfile.TemporaryDirectory() as repo:
            rel = "src/qux/BUILD.gn"
            base = _init_repo(
                repo,
                {
                    rel: HEADER
                    + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n\n'
                    + 'if (is_host) {\n  group("host_tests") {\n'
                    + '    testonly = true\n    deps = [ ":bin" ]\n  }\n}\n',
                },
            )
            _write_file(
                repo,
                rel,
                HEADER
                + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n\n'
                + 'if (is_host) {\n  group("host_tests") {\n'
                + "    testonly = true\n  }\n}\n",
            )
            findings = _run(repo, base, [rel])
            self.assertEqual(
                [(f["category"], f["line"]) for f in findings],
                [("empty_gn_group", 10)],
            )

    def test_renamed_file_compares_against_old_path(self):
        with tempfile.TemporaryDirectory() as repo:
            content = (
                HEADER
                + 'source_set("foo") {\n  sources = [ "foo.cc" ]\n}\n\n'
                + 'group("boot_tests") {\n  testonly = true\n}\n'
            )
            base = _init_repo(repo, {"src/old/BUILD.gn": content})
            _git(repo, "mv", "src/old", "src/new")
            self.assertEqual(_run(repo, base, ["src/new/BUILD.gn"]), [])

    def test_group_with_metadata_not_flagged(self):
        with tempfile.TemporaryDirectory() as repo:
            rel = "src/baz/BUILD.gn"
            base = _init_repo(repo, {"README.md": "init\n"})
            _write_file(
                repo,
                rel,
                HEADER
                + 'group("baz") {\n  metadata = {\n    foo = [ "bar" ]\n  }\n}\n',
            )
            self.assertEqual(_run(repo, base, [rel]), [])


if __name__ == "__main__":
    unittest.main()
