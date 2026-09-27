#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
from bazel_action_utils import BazelTargetInfosMap
from bazel_ninja_delayed_actions import (
    TargetWithPlatform,
    compute_sources_by_owner,
    read_extra_bazel_targets,
    validate_extra_bazel_targets,
)

_HOST = "//build/bazel/platforms:linux_x64"
_FUCHSIA = "//build/bazel/platforms:fuchsia_x64"


def _target_infos(
    *targets: tuple[str, str],
) -> BazelTargetInfosMap:
    """Builds a BazelTargetInfosMap from (bazel_target, platform) pairs."""
    return BazelTargetInfosMap(
        [
            {
                "bazel_target": target,
                "bazel_platform_label": platform,
                "bazel_platform_config": "host",
                "ninja_depfile": f"gen/{target}.d",
                "gn_targets_manifest": f"gen/{target}.manifest",
                "stamp_path": f"obj/{target}.stamp",
                "update_rust_project": False,
                "type": "file",
                "bazel_file": f"bazel-out/{target}",
                "ninja_file": f"obj/{target}",
            }
            for target, platform in targets
        ]
    )


class ReadExtraBazelTargetsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.addCleanup(self._td.cleanup)
        self.path = Path(self._td.name) / "suites.txt"

    def test_one_label_per_line(self) -> None:
        self.path.write_text("//foo:tests\n//bar:tests\n")
        self.assertEqual(
            read_extra_bazel_targets(self.path), ["//foo:tests", "//bar:tests"]
        )

    def test_missing_trailing_newline(self) -> None:
        self.path.write_text("//foo:tests\n//bar:tests")
        self.assertEqual(
            read_extra_bazel_targets(self.path), ["//foo:tests", "//bar:tests"]
        )

    def test_blank_lines_and_surrounding_whitespace_are_ignored(self) -> None:
        self.path.write_text("\n  //foo:tests  \n\n//bar:tests\n\n")
        self.assertEqual(
            read_extra_bazel_targets(self.path), ["//foo:tests", "//bar:tests"]
        )

    def test_empty_file(self) -> None:
        # A generated_file() whose metadata walk collected nothing still
        # exists, so an empty list must not be an error.
        self.path.write_text("")
        self.assertEqual(read_extra_bazel_targets(self.path), [])

    def test_missing_file_raises_value_error(self) -> None:
        with self.assertRaisesRegex(
            ValueError, r"extra_bazel_targets_file .* does not exist"
        ):
            read_extra_bazel_targets(self.path)


class ValidateExtraBazelTargetsTest(unittest.TestCase):
    def test_no_overlap(self) -> None:
        validate_extra_bazel_targets(
            {
                TargetWithPlatform("//build/bazel/host_tests:stamp", _HOST): [
                    "//foo:tests",
                    "//bar:tests",
                ]
            },
            _target_infos(("//build/bazel/host_tests:stamp", _HOST)),
        )

    def test_extra_target_also_declared_as_bazel_target(self) -> None:
        with self.assertRaisesRegex(
            ValueError, r"//foo:tests is both declared by a bazel_action\(\)"
        ):
            validate_extra_bazel_targets(
                {
                    TargetWithPlatform(
                        "//build/bazel/host_tests:stamp", _HOST
                    ): ["//foo:tests"]
                },
                _target_infos(
                    ("//build/bazel/host_tests:stamp", _HOST),
                    ("//foo:tests", _HOST),
                ),
            )

    def test_extra_target_claimed_by_two_actions(self) -> None:
        with self.assertRaisesRegex(
            ValueError, r"//foo:tests is named by the extra_bazel_targets_file"
        ):
            validate_extra_bazel_targets(
                {
                    TargetWithPlatform("//a:stamp", _HOST): ["//foo:tests"],
                    TargetWithPlatform("//b:stamp", _HOST): ["//foo:tests"],
                },
                _target_infos(("//a:stamp", _HOST), ("//b:stamp", _HOST)),
            )

    def test_same_label_on_different_platforms_is_allowed(self) -> None:
        # The same Bazel label built in two configurations is two distinct
        # builds, and each gets its own batch.
        validate_extra_bazel_targets(
            {
                TargetWithPlatform("//a:stamp", _HOST): ["//foo:tests"],
                TargetWithPlatform("//b:stamp", _FUCHSIA): ["//foo:tests"],
            },
            _target_infos(("//a:stamp", _HOST), ("//b:stamp", _FUCHSIA)),
        )


class ComputeSourcesByOwnerTest(unittest.TestCase):
    def test_requested_targets_own_their_own_sources(self) -> None:
        self.assertEqual(
            compute_sources_by_owner(
                {"//a:stamp": ["a.cc"], "//b:stamp": ["b.cc"]},
                ["//a:stamp", "//b:stamp"],
                {},
                _HOST,
            ),
            {
                TargetWithPlatform("//a:stamp", _HOST): ["a.cc"],
                TargetWithPlatform("//b:stamp", _HOST): ["b.cc"],
            },
        )

    def test_requested_target_without_sources_is_still_present(self) -> None:
        # The caller writes stamp files while iterating the result, so an
        # action whose suite list is empty still needs an entry.
        self.assertEqual(
            compute_sources_by_owner({}, ["//a:stamp"], {}, _HOST),
            {TargetWithPlatform("//a:stamp", _HOST): []},
        )

    def test_expanded_suite_members_belong_to_the_extras_owner(self) -> None:
        owner = TargetWithPlatform("//host_tests:stamp", _HOST)
        self.assertEqual(
            compute_sources_by_owner(
                {
                    "//host_tests:stamp": ["stamp.sh"],
                    "//foo:foo_test": ["foo_test.cc"],
                    "//bar:bar_test": ["bar_test.cc"],
                },
                ["//host_tests:stamp"],
                {owner: ["//foo:tests", "//bar:tests"]},
                _HOST,
            ),
            {owner: ["stamp.sh", "foo_test.cc", "bar_test.cc"]},
        )

    def test_directly_named_extra_targets_belong_only_to_their_owner(
        self,
    ) -> None:
        first = TargetWithPlatform("//a:stamp", _HOST)
        second = TargetWithPlatform("//b:stamp", _HOST)
        self.assertEqual(
            compute_sources_by_owner(
                {
                    "//foo:tests": ["foo/BUILD.bazel"],
                    "//bar:tests": ["bar/BUILD.bazel"],
                },
                ["//a:stamp", "//b:stamp"],
                {first: ["//foo:tests"], second: ["//bar:tests"]},
                _HOST,
            ),
            {
                first: ["foo/BUILD.bazel"],
                second: ["bar/BUILD.bazel"],
            },
        )

    def test_expanded_members_go_to_every_extras_owner_in_batch(self) -> None:
        # Bazel does not report which command line target a member was
        # expanded from, so the depfile is over-approximated rather than
        # under-approximated when several actions supplied extra targets.
        first = TargetWithPlatform("//a:stamp", _HOST)
        second = TargetWithPlatform("//b:stamp", _HOST)
        unbatched = TargetWithPlatform("//c:stamp", _HOST)
        self.assertEqual(
            compute_sources_by_owner(
                {"//foo:foo_test": ["foo_test.cc"]},
                ["//a:stamp", "//b:stamp"],
                {
                    first: ["//foo:tests"],
                    second: ["//bar:tests"],
                    unbatched: ["//baz:tests"],
                },
                _HOST,
            ),
            {first: ["foo_test.cc"], second: ["foo_test.cc"]},
        )

    def test_unaccountable_target_is_an_error(self) -> None:
        # Without this, the sources would be dropped and the depfile would
        # silently under-report the action's inputs.
        with self.assertRaisesRegex(
            ValueError, r"//foo:foo_test .* which no bazel_action\(\) requested"
        ):
            compute_sources_by_owner(
                {"//foo:foo_test": ["foo_test.cc"]},
                ["//a:stamp"],
                {},
                _HOST,
            )


if __name__ == "__main__":
    unittest.main()
