# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helpers for discovering Bazel tests that aren't in tests.json."""

from collections.abc import Collection, Iterator, Sequence
import os
import re

import bazel_labels

# Bazel labels, allowing a trailing slash from tab completion (`//src/foo/`).
_BAZEL_LABEL_ARG = re.compile(r"^@{0,2}//([^:()]*?)/?(?::([^:()]+))?$")
_GN_TOOLCHAIN_SUFFIX = re.compile(r"\([^)]*\)$")
_SELECTION_VALUE_FLAGS = ("--package", "--component")


def iter_positional_selection_args(
    selection_args: Sequence[str],
) -> Iterator[str]:
    """Yield positional selection arguments, skipping `--and` and flag values."""
    i = 0
    while i < len(selection_args):
        arg = selection_args[i]
        if arg in _SELECTION_VALUE_FLAGS:
            i += 2
            continue
        i += 1
        if arg == "--and":
            continue
        yield arg


def bazel_labels_missing_from_tests(
    selection_args: Sequence[str],
    test_labels: Collection[str],
    fuchsia_dir: str,
) -> dict[str, str]:
    """Return selection arguments that may be Bazel tests not in tests.json.

    Only positional name arguments (not `--package` or `--component` values)
    that look like Bazel labels in a directory with a BUILD.bazel file count,
    so that a typo or a GN-only label doesn't start Bazel. Target patterns
    are always included, so that bazel_tests_json can report that they
    aren't supported rather than fuzzy matching them against tests.json.

    Args:
        selection_args: The test selection arguments from the command line.
        test_labels: The test, package, and component labels of all tests in
            tests.json.
        fuchsia_dir: The Fuchsia source directory.

    Returns:
        A dict mapping each matched raw CLI argument to its normalized Bazel
        label (or the original pattern string for target patterns).
    """
    known_all: set[str] = set()
    known_bazel: set[str] = set()
    known_packages: set[str] = set()
    known_strict_ancestors: set[str] = set()
    for raw_label in test_labels:
        normalized = bazel_labels.expand_shorthand_label(
            _GN_TOOLCHAIN_SUFFIX.sub("", raw_label)
        )
        known_all.add(normalized)
        if raw_label.startswith("@"):
            known_bazel.add(normalized)
        pkg = normalized.split(":", 1)[0]
        if pkg.startswith("//") and pkg != "//":
            known_packages.add(pkg)
            ancestor = pkg.rsplit("/", 1)[0] or "//"
            while ancestor.startswith("//") and ancestor != "//":
                if ancestor in known_strict_ancestors:
                    break
                known_strict_ancestors.add(ancestor)
                ancestor = ancestor.rsplit("/", 1)[0] or "//"

    missing_args: dict[str, str] = {}
    for arg in iter_positional_selection_args(selection_args):
        if arg in missing_args:
            continue
        if not arg.removeprefix("@@").removeprefix("@").startswith("//"):
            continue
        # TODO(https://fxbug.dev/585649932): Support target patterns.
        if bazel_labels.is_target_pattern(arg):
            missing_args[arg] = arg
            continue

        match = _BAZEL_LABEL_ARG.match(arg)
        if not match or (not match.group(1) and not match.group(2)):
            continue
        label = bazel_labels.expand_shorthand_label(arg)
        # Explicit `@//` or `@@//` requests the Bazel target even if a GN twin
        # with the same normalized label is already in tests.json.
        known = known_bazel if arg.startswith("@") else known_all
        if label in known:
            continue
        # Bare `//pkg` (without `:` or `@`) already substring-matches exported
        # tests in or under `//pkg` in tests.json; don't turn it into
        # `//pkg:pkg` and fail if no target has the directory's name. A
        # trailing slash (`//pkg/`) only substring-matches strict descendants,
        # not `//pkg:target`.
        pkg_prefix = f"//{match.group(1)}"
        if ":" not in arg and not arg.startswith("@"):
            if arg.endswith("/"):
                if pkg_prefix in known_strict_ancestors:
                    continue
            elif (
                pkg_prefix in known_packages
                or pkg_prefix in known_strict_ancestors
            ):
                continue
        package_dir = os.path.join(fuchsia_dir, match.group(1))
        if not os.path.isfile(os.path.join(package_dir, "BUILD.bazel")):
            continue
        missing_args[arg] = label
    return missing_args
