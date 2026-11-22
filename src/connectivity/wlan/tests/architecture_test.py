# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests verifying WLAN Python test skip and setup_class abort rules."""

import textwrap
import unittest

from wlan_architecture_check import architecture_check

_SELF_REL_PATH = "src/connectivity/wlan/tests/architecture_test.py"
_BUILD_GN_PATH = "src/connectivity/wlan/tests/BUILD.gn"

_SCANNED_DIRECTORIES = frozenset(
    {
        "src/connectivity/wlan/tests",
        "src/testing/end_to_end/mobly_base_tests/fuchsia_wlan_base_test",
        "src/testing/end_to_end/honeydew/honeydew/affordances/connectivity/wlan/wlan_policy/tests",
        "src/testing/end_to_end/honeydew/honeydew/affordances/connectivity/wlan/wlan_policy_ap/tests",
    }
)

# Pre-existing runtime skips (file path, enclosing qualname).
# New WLAN tests must manage testbed exclusions in GN instead.
_RUNTIME_SKIP_ALLOWLIST = frozenset(
    {
        (
            "src/connectivity/wlan/tests/wlancfg/RebootTest.py",
            "RebootTest.run_reboot_test",
        ),
    }
)


class ArchitectureTests(unittest.TestCase):
    """Verifies WLAN Python test skip and setup_class abort rules via AST."""

    def test_tracked_wlan_python_files_complete(self) -> None:
        architecture_check.verify_tracked_files_complete(
            self,
            scanned_directories=_SCANNED_DIRECTORIES,
            self_rel_path=_SELF_REL_PATH,
            build_gn_path=_BUILD_GN_PATH,
        )

    def test_no_banned_skips_or_setup_class_aborts(self) -> None:
        architecture_check.verify_no_banned_skips_or_aborts(
            self,
            runtime_skip_allowlist=_RUNTIME_SKIP_ALLOWLIST,
        )

    def test_find_violations(self) -> None:
        sample = textwrap.dedent(
            '''\
            """asserts.skip("ignored in docstring")"""
            from mobly import asserts, signals
            from mobly.asserts import abort_all, skip_if

            class SampleTest:
                async def setup_class(self) -> None:
                    raise signals.TestError("allowed in setup_class")
                    raise signals.TestAbortClass("banned")
                    asserts.abort_class_if(True, "banned")
                    abort_all("banned")

                def test_case(self) -> None:
                    # signals.TestSkip("ignored in comment")
                    asserts.abort_class("allowed outside setup_class")
                    asserts.skip("banned")
                    raise signals.TestSkip("banned")
                    skip_if(True, "banned")
            '''
        )
        self.assertEqual(
            architecture_check.find_violations(
                "src/connectivity/wlan/tests/wlanix/SampleTest.py", sample
            ),
            (
                (
                    (8, "SampleTest.setup_class"),
                    (9, "SampleTest.setup_class"),
                    (10, "SampleTest.setup_class"),
                ),
                (
                    (15, "SampleTest.test_case"),
                    (16, "SampleTest.test_case"),
                    (17, "SampleTest.test_case"),
                ),
            ),
        )
        self.assertEqual(
            architecture_check.find_violations(
                "src/connectivity/wlan/tests/core/PhyCompliance11ACTest.py",
                sample,
            ),
            (
                (
                    (8, "SampleTest.setup_class"),
                    (9, "SampleTest.setup_class"),
                    (10, "SampleTest.setup_class"),
                ),
                (),
            ),
        )

    def test_setup_class_helper_call_graph_intra_file(self) -> None:
        sample = textwrap.dedent(
            """\
            from mobly import asserts, signals
            from mobly.asserts import abort_class

            def _module_setup_helper() -> None:
                abort_class("banned in module helper called from setup_class")

            def _module_test_only_helper() -> None:
                abort_class("allowed in helper only called from test_case")

            class SampleTest:
                async def setup_class(self) -> None:
                    await self._configure_ap()
                    await retry(self._poll_ready)
                    self._cycle_a()

                async def _configure_ap(self) -> None:
                    self._check_band()

                def _check_band(self) -> None:
                    try:
                        _module_setup_helper()
                    except signals.TestAbortSignal:
                        raise
                    raise signals.TestAbortClass("banned in transitive helper")

                def _poll_ready(self) -> None:
                    asserts.abort_all("banned in setup_class callback")

                def _cycle_a(self) -> None:
                    self._cycle_b()

                def _cycle_b(self) -> None:
                    self._cycle_a()
                    asserts.abort_class_if(True, "banned in cyclic helper")

                async def setup_test(self) -> None:
                    self._test_only_helper()

                def test_case(self) -> None:
                    self._test_only_helper()
                    _module_test_only_helper()
                    asserts.abort_class("allowed in test_case")

                def _test_only_helper(self) -> None:
                    raise signals.TestAbortClass("allowed in test-only helper")
            """
        )
        aborts, skips = architecture_check.find_violations(
            "src/connectivity/wlan/tests/wlancfg/SampleTest.py", sample
        )
        self.assertEqual(skips, ())
        self.assertEqual(
            aborts,
            (
                (5, "_module_setup_helper"),
                (24, "SampleTest._check_band"),
                (27, "SampleTest._poll_ready"),
                (34, "SampleTest._cycle_b"),
            ),
        )

    def test_setup_class_helper_call_graph_cross_file(self) -> None:
        base_path = (
            "src/testing/end_to_end/mobly_base_tests/"
            "fuchsia_wlan_base_test/__init__.py"
        )
        utils_path = "src/connectivity/wlan/tests/utils/helper_utils.py"
        sub_path = "src/connectivity/wlan/tests/wlanix/SubTest.py"

        sources = {
            base_path: textwrap.dedent(
                """\
                from mobly import signals

                class FuchsiaWlanBaseTest:
                    async def setup_class(self) -> None:
                        self.reset_attenuators()

                    def reset_attenuators(self) -> None:
                        pass

                    async def wait_for_interface(self) -> None:
                        raise signals.TestAbortClass("banned base setup helper")

                    def test_only_base_helper(self) -> None:
                        raise signals.TestAbortClass("allowed test-only base helper")
                """
            ),
            utils_path: textwrap.dedent(
                """\
                from mobly import asserts, signals

                class SetupClient:
                    def __init__(self) -> None:
                        asserts.abort_class("banned in helper __init__")

                    async def connect(self) -> None:
                        raise signals.TestAbortAll("banned in helper method")

                def test_only_util() -> None:
                    asserts.abort_class("allowed in test-only util")
                """
            ),
            sub_path: textwrap.dedent(
                """\
                import fuchsia_wlan_base_test
                from wlan_test_utils import helper_utils
                from mobly import asserts

                class SubTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
                    async def setup_class(self) -> None:
                        await super().setup_class()
                        await self.wait_for_interface()
                        self.client = helper_utils.SetupClient()
                        await self.client.connect()

                    def reset_attenuators(self) -> None:
                        asserts.abort_class("banned in overridden hook")

                    async def test_connect(self) -> None:
                        self.test_only_base_helper()
                        helper_utils.test_only_util()
                        asserts.abort_class("allowed in test")
                """
            ),
        }

        violations = architecture_check.find_violations_in_sources(sources)
        self.assertEqual(
            violations[base_path],
            (((11, "FuchsiaWlanBaseTest.wait_for_interface"),), ()),
        )
        self.assertEqual(
            violations[utils_path],
            (
                (
                    (5, "SetupClient.__init__"),
                    (8, "SetupClient.connect"),
                ),
                (),
            ),
        )
        self.assertEqual(
            violations[sub_path],
            (((13, "SubTest.reset_attenuators"),), ()),
        )


if __name__ == "__main__":
    unittest.main()
