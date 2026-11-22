# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ABC with methods for Host-(Fuchsia)Target interactions via FFX."""

import abc
import subprocess
from typing import Any

from honeydew.transports import transport
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx.types import (
    MachineFormat,
    TargetInfoData,
)
from honeydew.typing import custom_types
from honeydew.utils import properties


class FFX(transport.Transport):
    """ABC with methods for Host-(Fuchsia)Target interactions via FFX."""

    @properties.PersistentProperty
    @abc.abstractmethod
    def config(self) -> ffx_config.FfxConfigData:
        """Returns the FFX configuration associated with this instance of FFX
        object.

        Returns:
            FfxConfigData
        """

    @properties.PersistentProperty
    @abc.abstractmethod
    def serial_number(self) -> str | None:
        """Returns the device serial number from FFX target show.

        Returns:
            Serial number if available, else None.
        """

    @abc.abstractmethod
    def check_connection(self, timeout: float | None = None) -> None:
        """Checks the FFX connection from host to Fuchsia device.

        Args:
            timeout: Maximum time in seconds to wait for the connection. If
                None (default), waits indefinitely.

        Raises:
            FfxConnectionError
        """

    @abc.abstractmethod
    def get_target_info_from_target_show(self) -> TargetInfoData:
        """Executed and returns the output of `ffx -t {target} target show`.

        Returns:
            Output of `ffx -t {target} target show`.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def get_target_info_from_target_list(self) -> dict[str, Any]:
        """Executed and returns the output of
        `ffx --machine json target list <target>`.

        Returns:
            Output of `ffx --machine json target list <target>`.

        Raises:
            FfxCommandError: In case of FFX command failure.
        """

    @abc.abstractmethod
    def get_target_name(self) -> str:
        """Returns the target name.

        Returns:
            Target name.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def get_target_address(
        self, refresh: bool = False
    ) -> custom_types.TargetAddr:
        """Returns the target's address, resolving and caching it if needed.

        Args:
            refresh: Pass True after operations that may change the target's
                address (such as reboots or USB disconnects) to bypass any
                cached address, re-query FFX, and wait for the target and its
                expected address family to reappear if transiently absent. If
                False (default), returns the cached address when available or
                performs a one-shot initial lookup.

        Returns:
            The resolved TargetAddr.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: If the target address cannot be resolved or parsed.
        """

    @abc.abstractmethod
    def get_target_board(self) -> str:
        """Returns the target's board.

        Returns:
            Target's board.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def get_target_product(self) -> str:
        """Returns the target's product.

        Returns:
            Target's product.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def get_target_status(self) -> str:
        """Returns FFX target status.

        Returns:
            Output of FFX command when capture_output is set to True, otherwise
            an empty string.

        Raises:
            FfxTargetStatusError: In case of HostCmdError.
        """

    @abc.abstractmethod
    def notify_intentional_disconnect(self) -> None:
        """Best-effort notification to the FFX monitor of an upcoming intentional disconnect."""

    @abc.abstractmethod
    def run(
        self,
        cmd: list[str],
        timeout: float | None = None,
        capture_output: bool = True,
        log_output: bool = True,
        include_target: bool = True,
        machine: MachineFormat = MachineFormat.JSON,
        log_status_on_failure: bool = True,
        disable_controlmaster: bool = False,
        config_overrides: dict[str, Any] | None = None,
    ) -> str:
        """Runs an FFX command.

        Args:
            cmd: FFX command to run.
            timeout: Timeout to wait for the ffx command to return. By default,
                timeout is not set.
            capture_output: When True, the stdout/err from the command will be
                captured and returned. When False, the output of the command
                will be streamed to stdout/err accordingly and it won't be
                returned. Defaults to True.
            log_output: When True, logs the output in DEBUG level. Callers
                may set this to False when expecting particularly large
                or spammy output.
            include_target: If set to True, `ffx -t {target} {cmd}` will be run.
                Otherwise, `ffx {cmd}` will be run.
            machine: Specifies the machine format used for the ffx command (defaults
                to "json")
            log_status_on_failure: Whether to run diagnostic triage ('ffx target status')
                if the command fails or times out. Defaults to True.
            disable_controlmaster: boolean value of controlmaster.
            config_overrides: FFX configuration overrides.

        Returns:
            Output of FFX command when capture_output is set to True, otherwise
            an empty string.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxTimeoutError: In case of FFX command timeout.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def popen(  # type: ignore[no-untyped-def]
        self,
        cmd: list[str],
        config_overrides: dict[str, Any] | None = None,
        **kwargs,
    ) -> subprocess.Popen[custom_types.AnyString]:
        """Starts a new process to run the FFX cmd and returns the corresponding
        process.

        Intended for executing daemons or processing streamed output. Given
        the raw nature of this API, it is up to callers to detect and handle
        potential errors, and make sure to close this process eventually
        (e.g. with `popen.terminate` method). Otherwise, use the simpler `run`
        method instead.

        Args:
            cmd: FFX command to run.
            config_overrides: FFX configuration overrides.
            kwargs: Forwarded as-is to subprocess.Popen.

        Returns:
            The Popen object of `ffx -t {target} {cmd}`.
            If text arg of subprocess.Popen is set to True,
            subprocess.Popen[str] will be returned. Otherwise,
            subprocess.Popen[bytes] will be returned.
        """

    @abc.abstractmethod
    def run_test_component(
        self,
        component_url: str,
        ffx_test_args: list[str] | None = None,
        test_component_args: list[str] | None = None,
        capture_output: bool = True,
    ) -> str:
        """Executes and returns the output of
        `ffx -t {target} test run {component_url}` with the given options.

        Args:
            component_url: The URL of the test to run.
            ffx_test_args: args to pass to `ffx test run`.
            test_component_args: args to pass to the test component.
            capture_output: When True, the stdout/err from the command will be captured and
                returned. When False, the output of the command will be streamed to stdout/err
                accordingly and it won't be returned. Defaults to True.

        Returns:
            Output of `ffx -t {target} {cmd}` when capture_output is set to True, otherwise an
            empty string.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def run_ssh_cmd(
        self,
        cmd: str,
        capture_output: bool = True,
    ) -> str:
        """Executes and returns the output of `ffx -t target ssh <cmd>`.

        Args:
            cmd: SSH command to run.
            capture_output: When True, the stdout/err from the command will be
                captured and returned. When False, the output of the command
                will be streamed to stdout/err accordingly and it won't be
                returned. Defaults to True.

        Returns:
            Output of `ffx -t target ssh <cmd>` when capture_output is set to
            True, otherwise an empty string.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def wait_for_rcs_connection(
        self,
        include_target_name: bool = False,
        timeout: float | None = None,
    ) -> None:
        """Wait until FFX is able to establish a RCS connection to the target.

        Args:
            include_target_name: If set to True, target will be specified by query.
                Otherwise, target will be specified by address.
            timeout: Maximum time in seconds to wait for the RCS connection.
                If None (default), waits indefinitely.
        Raises:
            DeviceNotConnectedError: If FFX fails to reach target within the
                timeout.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def wait_for_rcs_disconnection(
        self,
        timeout: float | None = None,
    ) -> None:
        """Wait until FFX observes that the RCS connection to the target has been disconnected.

        Args:
            timeout: Maximum time in seconds to wait for the RCS disconnection.
                If None (default), waits indefinitely.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxTimeoutError: If the target does not disconnect within the
                timeout.
            FfxCommandError: In case of other FFX command failure.
        """

    @abc.abstractmethod
    def generate_ffx_cmd(
        self,
        cmd: list[str],
        include_target: bool = True,
        machine: MachineFormat = MachineFormat.JSON,
        disable_controlmaster: bool = False,
        config_overrides: dict[str, Any] | None = None,
    ) -> list[str]:
        """Generates the FFX command that need to be run.

        Args:
            cmd: FFX command.
            include_target: True to include "-t <target>", False otherwise.
            machine: Specifies the machine format used for the ffx command (defaults
                to "json")
            disable_controlmaster: boolean value of controlmaster.
            config_overrides: FFX configuration overrides.

        Returns:
            FFX command to be run as list of string.
        """
