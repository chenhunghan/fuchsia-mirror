# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import tempfile
import unittest

import bazel_tests


class TestBazelLabelsMissingFromTests(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.addCleanup(self._td.cleanup)
        self.fuchsia_dir = self._td.name
        for d, build_file in (
            ("src/bazel", "BUILD.bazel"),
            ("src/gn_only", "BUILD.gn"),
        ):
            os.makedirs(os.path.join(self.fuchsia_dir, d))
            with open(os.path.join(self.fuchsia_dir, d, build_file), "w"):
                pass

    def test_returns_unknown_labels_in_bazel_packages(self) -> None:
        self.assertEqual(
            bazel_tests.bazel_labels_missing_from_tests(
                [
                    "//src/bazel:new_test",
                    "@@//src/bazel:other_test",
                    # Both raw forms map to the same normalized label.
                    "@//src/bazel:new_test",
                    # Bazel and GN both expand this to //src/bazel:bazel,
                    # including with a trailing slash from tab-completion.
                    "//src/bazel/",
                ],
                [],
                self.fuchsia_dir,
            ),
            {
                "//src/bazel:new_test": "//src/bazel:new_test",
                "@@//src/bazel:other_test": "//src/bazel:other_test",
                "@//src/bazel:new_test": "//src/bazel:new_test",
                "//src/bazel/": "//src/bazel:bazel",
            },
        )

    def test_skips_known_and_non_bazel_args(self) -> None:
        self.assertEqual(
            bazel_tests.bazel_labels_missing_from_tests(
                [
                    "//src/bazel:known_test",
                    "//src/bazel:gn_test",
                    # Bare //src/bazel already substring-matches exported tests
                    # under //src/bazel in tests.json.
                    "//src/bazel",
                    "//src/gn_only:tests",
                    "//src/missing:tests",
                    "foo_test",
                    # Operands to --package and --component aren't target labels.
                    "--package",
                    "//src/bazel:unexported_pkg",
                    "--component",
                    "//src/bazel:unexported_comp",
                    "//src/bazel:gn_test_pkg",
                ],
                [
                    "@@//src/bazel:known_test",
                    "//src/bazel:gn_test(//build/toolchain/host:x64)",
                    "//src/bazel:gn_test_pkg(//build/toolchain/fuchsia:x64)",
                ],
                self.fuchsia_dir,
            ),
            {},
        )

    def test_explicit_bazel_repo_prefix_ignores_gn_twin(self) -> None:
        # Passing both the GN label and `@//` Bazel twin only returns `@//`.
        self.assertEqual(
            bazel_tests.bazel_labels_missing_from_tests(
                [
                    "//src/bazel:gn_test",
                    "@//src/bazel:gn_test",
                    "@@//src/bazel:known_test",
                ],
                [
                    "@@//src/bazel:known_test",
                    "//src/bazel:gn_test(//build/toolchain/host:x64)",
                ],
                self.fuchsia_dir,
            ),
            {"@//src/bazel:gn_test": "//src/bazel:gn_test"},
        )

    def test_trailing_slash_only_skipped_for_descendant_tests(self) -> None:
        # //src/bazel/ doesn't substring-match //src/bazel:known_test (colon,
        # not slash), so it still resolves to //src/bazel:bazel.
        self.assertEqual(
            bazel_tests.bazel_labels_missing_from_tests(
                ["//src/bazel/"],
                ["@@//src/bazel:known_test"],
                self.fuchsia_dir,
            ),
            {"//src/bazel/": "//src/bazel:bazel"},
        )
        # It does substring-match tests in subdirectories under //src/bazel/.
        self.assertEqual(
            bazel_tests.bazel_labels_missing_from_tests(
                ["//src/bazel/"],
                ["@@//src/bazel/sub:known_test"],
                self.fuchsia_dir,
            ),
            {},
        )

    def test_includes_bazel_target_patterns(self) -> None:
        # Patterns are passed to bazel_tests_json so that it can report that
        # they aren't supported.
        self.assertEqual(
            bazel_tests.bazel_labels_missing_from_tests(
                [
                    "//src/bazel:all",
                    "@//src/bazel:*",
                    "//src/bazel/...",
                    "//...",
                    # Relative patterns are never treated as Bazel labels.
                    "...",
                    ":all",
                ],
                [],
                self.fuchsia_dir,
            ),
            {
                "//src/bazel:all": "//src/bazel:all",
                "@//src/bazel:*": "@//src/bazel:*",
                "//src/bazel/...": "//src/bazel/...",
                "//...": "//...",
            },
        )


if __name__ == "__main__":
    unittest.main()
