# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ADB Root/Unroot Stress and Starnix FunctionFS Inspect E2E Test.

Verifies repeated `adb root` / `adb unroot` and `su_root` / `su_unroot`
transitions, USB ADB endpoint teardown and re-enablement, and Starnix
FunctionFS Inspect diagnostics (`is_online`, `has_input_output_endpoints`,
and non-empty `event_history`). Covers Test Case TC-43
(https://fxbug.dev/578656909).

Pre-conditions:
  1. Target AndroidDevice DUT is connected over USB with `adb` and `ffx` active.

Test Steps:
  1. For each Mobly-generated stress cycle (`num_iterations`, default 10):
     a. Ensure starting state is unprivileged `uid=2000(shell)`.
     b. Cycle `adb.root()` -> `adb.unroot()`, polling for `uid=0(root)` and
        `uid=2000(shell)` transitions across USB FunctionFS endpoint resets.
     c. Exercise `use_adb_root()`, `su_root()` / `su_unroot()`, and
        `use_su_root()`, restoring `uid=2000(shell)` on both failure and
        normal exit.
     d. Wait for Fuchsia target online readiness (`wait_for_online()`).
  2. After iteration 1's USB endpoint resets, verify active running drivers
     (`ffx driver list --running`) and Starnix FunctionFS Inspect diagnostics
     (`core/starnix_runner/*:root`).

Verification Criteria:
  - Every `adb root` and `adb unroot` transition switches UIDs cleanly within
    the retry deadline without hanging `adbd` or dropping the DUT offline.
  - Starnix FunctionFS Inspect exposes `is_online=True`,
    `has_input_output_endpoints=True`, and a non-empty `event_history`.
"""

import datetime
import logging
from collections.abc import Mapping
from typing import Any

import fuchsia_base_test
from fuchsia_inspect import InspectDataError
from honeydew import errors as honeydew_errors
from honeydew.device_classes import android_device
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import types as ffx_types
from honeydew.utils import control_flows
from honeydew.utils.deadline import Deadline
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_FFX_CMD_TIMEOUT: datetime.timedelta = datetime.timedelta(seconds=30)
_ADB_ROOT_TIMEOUT_SECS: float = 30.0
_WAIT_FOR_DEVICE_TIMEOUT_SECS: float = 30.0
_UID_TRANSITION_TIMEOUT: datetime.timedelta = datetime.timedelta(seconds=30)
_UID_POLL_DELAY: datetime.timedelta = datetime.timedelta(seconds=1)
_UID_POLL_CMD_TIMEOUT_SECS: float = 5.0
_UID_POLL_CMD_ATTEMPTS: int = 1
_INSPECT_VERIFY_TIMEOUT: datetime.timedelta = datetime.timedelta(seconds=30)
_INSPECT_POLL_DELAY: datetime.timedelta = datetime.timedelta(seconds=2)

# Inspect selector matching Starnix container FunctionFS diagnostics.
_STARNIX_FUNCTIONFS_SELECTOR: str = "core/starnix_runner/*:root"


def parse_bool_param(
    user_params: Mapping[str, object], key: str, *, default: bool = False
) -> bool:
    """Parses a boolean Mobly user parameter from bool, int, or string."""
    val = user_params.get(key, default)
    if isinstance(val, bool):
        return val
    if isinstance(val, str):
        return val.strip().lower() in ("true", "1", "yes")
    return bool(val)


def _find_functionfs_node(node: Any) -> dict[str, Any] | None:
    """Recursively searches Inspect payload for the Starnix FunctionFS node.

    Args:
        node: Arbitrary Inspect subtree (dict, list, or primitive).

    Returns:
        The dictionary representing the Starnix FunctionFS Inspect node if
        found; otherwise `None`.
    """
    if isinstance(node, dict):
        if "has_input_output_endpoints" in node or "ep1_bulk_out" in node:
            return node
        for value in node.values():
            found = _find_functionfs_node(value)
            if found is not None:
                return found
    return None


class AdbRootUnrootStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test for loop/stress testing ADB root/unroot.

    Optional Mobly Test Params:
        num_iterations (int): Number of root/unroot stress cycles (default 10).
        num_cycles (int): Optional alias for `num_iterations` (takes precedence
            if provided).
        verify_inspect (bool): Whether to verify FunctionFS Inspect diagnostics
            after cycle 1 (default True).
    """

    dut: android_device.AndroidDevice
    _adb_privilege_dirty: bool = False

    async def pre_run(self) -> None:
        """Mobly method used to generate the test cases at run time."""
        test_arg_tuple_list: list[tuple[int]] = []

        raw_cycles = self.user_params.get(
            "num_cycles", self.user_params.get("num_iterations", 10)
        )
        num_cycles = max(1, int(raw_cycles))
        for iteration in range(1, num_cycles + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    def _restore_adb_unroot(self, *, best_effort: bool) -> None:
        """Restores ADB session to unprivileged `uid=2000(shell)` state.

        Args:
            best_effort: If True, log errors instead of raising them so that
                the original failure from the test iteration is preserved.
        """
        try:
            # adbd may still be restarting after the last root/unroot.
            self.dut.adb.run(
                ["wait-for-device"], timeout=_WAIT_FOR_DEVICE_TIMEOUT_SECS
            )
            self.dut.adb.su_unroot()
            self.dut.adb.unroot(timeout=_ADB_ROOT_TIMEOUT_SECS)
            output = self.dut.adb.run(["shell", "id"])
            asserts.assert_in(
                "uid=2000(shell)",
                output,
                msg=f"Expected shell user (uid=2000) but got: {output}",
            )
            self._adb_privilege_dirty = False
        except Exception as err:
            if not best_effort:
                raise
            _LOGGER.warning(
                "Best-effort ADB unroot cleanup encountered error: %s",
                err,
                exc_info=True,
            )

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        self._adb_privilege_dirty = False
        assert isinstance(self.dut, android_device.AndroidDevice)
        # Ensure ADB is supported and enabled on this device before starting
        # the test (raises NotSupportedError or NotEnabledError otherwise).
        _ = self.dut.adb
        # TODO(b/564946370): Remove this unbind once AndroidDevice.adb's
        # temporary su_root monkey-patch is removed.
        for attr in ("root", "unroot", "use_adb_root"):
            self.dut.adb.__dict__.pop(attr, None)
        # Point 1 cleanup: clear any stale root state from a prior run.
        self._restore_adb_unroot(best_effort=True)

    async def teardown_test(self) -> None:
        """Ensures ADB is restored to unprivileged shell state on teardown."""
        # Point 3 cleanup: fallback cleanup if Point 2 cleanup failed or the
        # test was interrupted.
        if self._adb_privilege_dirty:
            self._restore_adb_unroot(best_effort=True)
        await super().teardown_test()

    async def _wait_for_expected_uid(self, expected_token: str) -> str:
        """Polls `adb shell id` until `expected_token` is observed."""

        async def _poll_uid() -> str:
            try:
                output = self.dut.adb.run(
                    ["shell", "id"],
                    timeout=_UID_POLL_CMD_TIMEOUT_SECS,
                    attempts=_UID_POLL_CMD_ATTEMPTS,
                )
            except (
                adb_errors.AdbError,
                honeydew_errors.HoneydewError,
            ) as err:
                raise control_flows.RetriableError(
                    f"ADB transport not yet ready while polling id: {err}"
                ) from err
            if expected_token not in output:
                raise control_flows.RetriableError(
                    f"Expected {expected_token!r} in `id` output, got: {output}"
                )
            return output

        return await control_flows.retry_until_deadline(
            task=_poll_uid,
            deadline=Deadline.from_timeout(_UID_TRANSITION_TIMEOUT),
            retry_delay=_UID_POLL_DELAY,
        )

    async def _verify_functionfs_diagnostics(self) -> None:
        """Verifies running USB drivers and Starnix FunctionFS Inspect state."""
        # `ffx driver list --running` does not emit structured JSON output;
        # request raw output explicitly instead of Honeydew's JSON default.
        running_drivers = self.dut.ffx.run(
            ["driver", "list", "--running"],
            timeout=_FFX_CMD_TIMEOUT.total_seconds(),
            machine=ffx_types.MachineFormat.RAW,
        )
        asserts.assert_greater(
            len(running_drivers.strip()),
            0,
            "Expected non-empty `ffx driver list --running` output",
        )

        async def _poll_functionfs_inspect() -> dict[str, Any]:
            try:
                collection = self.dut.get_inspect_data(
                    selectors=[_STARNIX_FUNCTIONFS_SELECTOR]
                )
            except (
                InspectDataError,
                ffx_errors.FfxCommandError,
                honeydew_errors.HoneydewError,
            ) as err:
                raise control_flows.RetriableError(
                    f"Transient error querying FunctionFS Inspect: {err}"
                ) from err

            for entry in collection.data:
                if entry.payload is not None:
                    found = _find_functionfs_node(entry.payload)
                    if found is not None:
                        if not bool(found.get("is_online", False)):
                            raise control_flows.RetriableError(
                                "FunctionFS is_online is not True after reconnect"
                            )
                        return found
            raise control_flows.RetriableError(
                f"FunctionFS node not found in {_STARNIX_FUNCTIONFS_SELECTOR}"
            )

        ffs_node = await control_flows.retry_until_deadline(
            task=_poll_functionfs_inspect,
            deadline=Deadline.from_timeout(_INSPECT_VERIFY_TIMEOUT),
            retry_delay=_INSPECT_POLL_DELAY,
        )
        _LOGGER.info("FunctionFS Inspect keys: %s", list(ffs_node.keys()))
        asserts.assert_in(
            "is_online",
            ffs_node,
            "Expected FunctionFS `is_online` key in Inspect diagnostics",
        )
        asserts.assert_true(
            ffs_node["is_online"],
            "Expected FunctionFS `is_online` to be True after root/unroot",
        )
        asserts.assert_in(
            "has_input_output_endpoints",
            ffs_node,
            "Expected FunctionFS `has_input_output_endpoints` key in Inspect diagnostics",
        )
        asserts.assert_true(
            ffs_node["has_input_output_endpoints"],
            "Expected FunctionFS `has_input_output_endpoints` to be True",
        )
        asserts.assert_in(
            "event_history",
            ffs_node,
            "Expected FunctionFS `event_history` node in Inspect diagnostics",
        )
        event_history = ffs_node["event_history"]
        asserts.assert_true(
            isinstance(event_history, dict),
            "Expected FunctionFS `event_history` to be a dict, "
            f"got {type(event_history).__name__}",
        )
        asserts.assert_greater(
            len(event_history),
            0,
            "Expected non-empty FunctionFS `event_history` after root/unroot teardown",
        )

    async def _test_logic(self, iteration: int) -> None:
        """Test case logic that performs root/unroot and su_root/su_unroot."""
        _LOGGER.info(
            "Starting the ADB Root/Unroot test iteration# %s", iteration
        )
        self._adb_privilege_dirty = True

        try:
            # Step 1a: Ensure we start as non-root `uid=2000(shell)`.
            self.dut.adb.su_unroot()
            self.dut.adb.unroot(timeout=_ADB_ROOT_TIMEOUT_SECS)
            await self._wait_for_expected_uid("uid=2000(shell)")

            # Step 1b: Switch to root (`adb root`), triggering USB ADB endpoint
            # teardown and re-enumeration.
            self.dut.adb.root(timeout=_ADB_ROOT_TIMEOUT_SECS)
            await self._wait_for_expected_uid("uid=0(root)")

            # Switch back to non-root (`adb unroot`).
            self.dut.adb.unroot(timeout=_ADB_ROOT_TIMEOUT_SECS)
            await self._wait_for_expected_uid("uid=2000(shell)")

            # Step 1c: Test `use_adb_root` context manager and verify automatic
            # restoration to `uid=2000(shell)` on exit.
            with self.dut.adb.use_adb_root(timeout=_ADB_ROOT_TIMEOUT_SECS):
                await self._wait_for_expected_uid("uid=0(root)")

            # Ensure previous non-root state was restored upon exiting context.
            await self._wait_for_expected_uid("uid=2000(shell)")

            # Switch to root via `su_root`.
            self.dut.adb.su_root()
            await self._wait_for_expected_uid("uid=0(root)")

            # Switch back to non-root via `su_unroot`.
            self.dut.adb.su_unroot()
            await self._wait_for_expected_uid("uid=2000(shell)")

            # Test `use_su_root` context manager.
            with self.dut.adb.use_su_root():
                await self._wait_for_expected_uid("uid=0(root)")

            # Ensure previous non-root state was restored upon exiting
            # `use_su_root` context.
            await self._wait_for_expected_uid("uid=2000(shell)")
        except BaseException:
            # Restore non-root state on a best-effort basis and re-raise the
            # original error. When root/unroot fails, adbd is often still
            # restarting (b/580019721), so cleanup can fail too and would
            # otherwise mask the original iteration failure.
            self._restore_adb_unroot(best_effort=True)
            raise

        # Point 2 cleanup: restore non-root state; failures here fail the
        # iteration.
        self._restore_adb_unroot(best_effort=False)

        # Step 1d: Ensure Fuchsia side is also online and IP is resolved (in
        # case USB reset changed it).
        await self.dut.wait_for_online()

        # Step 2: Validate Starnix FunctionFS Inspect `event_history` and
        # endpoint diagnostics after the first root/unroot cycle.
        verify_inspect = parse_bool_param(
            self.user_params, "verify_inspect", default=True
        )
        if iteration == 1 and verify_inspect:
            await self._verify_functionfs_diagnostics()

        _LOGGER.info(
            "Successfully ended the ADB Root/Unroot test iteration# %s",
            iteration,
        )

    def _name_func(self, iteration: int) -> str:
        """Generates name for each iteration test case."""
        return f"test_adb_root_unroot_{iteration}"


if __name__ == "__main__":
    test_runner.main()
