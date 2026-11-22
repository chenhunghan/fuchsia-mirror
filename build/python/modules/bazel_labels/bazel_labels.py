# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helpers for Bazel labels, e.g. ones passed on the command line.

This module only uses the standard library, so that tools that aren't part
of the build, like `fx test`, can use it too.
"""


def normalize_label(label: str) -> str:
    """Return the given label in its normalized form (never starting with "@@//" or "@//")."""
    for prefix in ("@@//", "@//"):
        if label.startswith(prefix):
            return "//" + label.removeprefix(prefix)
    return label


def expand_shorthand_label(label: str) -> str:
    """Normalize `label`, turning `//pkg` or `//pkg/` into `//pkg:pkg`."""
    label = normalize_label(label)
    if ":" in label:
        return label
    if label != "//":
        label = label.rstrip("/")
    return f"{label}:{label.rsplit('/', 1)[-1]}"


def is_target_pattern(label: str) -> bool:
    """Return whether `label` is an absolute target pattern like `//pkg/...`."""
    pkg, _, name = normalize_label(label).partition(":")
    return pkg.startswith("//") and (
        pkg == "//..."
        or pkg.endswith("/...")
        or name in ("all", "*", "all-targets")
    )
