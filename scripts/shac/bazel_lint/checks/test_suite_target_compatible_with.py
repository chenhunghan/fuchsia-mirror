# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that `test_suite()` targets do not set `target_compatible_with`.

This only sees `target_compatible_with` passed as a literal keyword, not
attributes forwarded to `native.test_suite()` through `**kwargs`.
"""

from framework import (
    BazelCheck,
    CallInfo,
    FileContext,
    FileKind,
    register_check,
)

_REFERENCE_URL = "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/tests#test-suite-target-compatible-with"

_ERROR_MSG = (
    "Do not set `target_compatible_with` on `test_suite()`; set it on the "
    "leaf tests instead. Bazel silently skips an incompatible `test_suite()` "
    "and every test in it, which hides misplaced tests from tests.json."
)


@register_check
class TestSuiteTargetCompatibleWithCheck(BazelCheck):
    """Disallows `target_compatible_with` on `test_suite()` targets."""

    name = "bazel_test_suite_target_compatible_with"
    file_kinds = frozenset({FileKind.BUILD, FileKind.BZL})

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        if call.func_name != "test_suite":
            return
        kw = call.keywords.get("target_compatible_with")
        if kw is None:
            return
        # Only warn until in-flight changes that add new violations have
        # landed, so they don't break postsubmit.
        # TODO(https://fxbug.dev/540003943): Switch to "error".
        fc.report(_ERROR_MSG, kw, level="warning", url=_REFERENCE_URL)
