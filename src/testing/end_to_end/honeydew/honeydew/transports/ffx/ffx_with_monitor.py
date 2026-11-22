# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""FFX transport using `ffx monitor` for Host-Target status interactions."""

import json
import logging
import time
from dataclasses import asdict
from typing import Any

from honeydew import errors
from honeydew.affordances_capable import FuchsiaDeviceAddressChange
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import errors as ffx_errors
from honeydew.transports.ffx import ffx_impl
from honeydew.transports.ffx.types import (
    MonitorTargetInfo,
)
from honeydew.typing import custom_types

_FFX_MONITOR_CMDS: dict[str, list[str]] = {
    "MONITOR_STATUS": ["monitor", "status"],
}

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Maximum time to wait for `ffx monitor` to observe a disconnection that has
# already been confirmed by querying the target directly.
_MONITOR_DISCONNECTION_TIMEOUT_SECS: float = 30.0
# Delay between `ffx monitor status` polls while waiting for the monitor to
# observe a change in connection state.
_MONITOR_CONNECTION_POLL_INTERVAL_SECS: float = 0.5


class FFXWithMonitor(ffx_impl.FfxImpl):
    """Provides methods for Host-(Fuchsia)Target interactions via FFX with `ffx monitor`.

    # FFX monitor session management:
    # For infra runs, ffx monitor session is started and managed by botanist.
    # Currently it is not started by default and is only started on a
    # specific builder basis. But once ffx monitor is thoroughly tested, it
    # will be started by default on all infra builders.
    #
    # For local runs, ffx monitor eventually will be started and managed by
    # `fx test`. Until this support is added (b/455924189), local users would
    # have to manually start monitors through `ffx monitor start`.

    Args:
        target_name: Human-readable name of the target.
        config_data: Configuration associated with FFX.
        target_addr: Fuchsia device target address (IP or USB).
        device_address_change: Object that implements FuchsiaDeviceAddressChange to handle Fuchsia
            device address changes.

    Raises:
        ValueError: If target_addr is provided and device_address_change is not provided.
        FfxMonitorNotRunningError: If no running monitor is detected.
        FfxConnectionError: In case of failed to check FFX connection.
        FfxCommandError: In case of failure.
    """

    def __init__(
        self,
        target_name: str,
        config_data: ffx_config.FfxConfigData,
        target_addr: custom_types.TargetAddr | None = None,
        device_address_change: FuchsiaDeviceAddressChange | None = None,
    ) -> None:
        _LOGGER.info("Using FFXWithMonitor for target %s", target_name)
        super().__init__(
            target_name=target_name,
            config_data=config_data,
            target_addr=target_addr,
            device_address_change=device_address_change,
        )

    def check_connection(self, timeout: float | None = None) -> None:
        """Checks the FFX connection from host to Fuchsia device.

        Args:
            timeout: Maximum time in seconds to wait for the connection. If
                None (default), waits indefinitely.

        Raises:
            FfxMonitorNotRunningError: If no running monitor is detected.
            FfxConnectionError: If FFX connection check fails.
        """
        if not self._check_running_monitor():
            raise ffx_errors.FfxMonitorNotRunningError(
                "No running monitor detected."
            )
        super().check_connection(timeout=timeout)

    def get_target_info_from_target_list(self) -> dict[str, Any]:
        """Executed and returns the target status from `ffx monitor status`.

        For monitor, the "targets" field of `ffx monitor status`
        response is identical with `ffx target list`, so they can share
        the same format.

        Returns:
            Target status dictionary from `ffx monitor status`.

        Raises:
            FfxCommandError: In case of FFX command failure.
        """
        return asdict(self._get_target_status())

    def get_target_name(self) -> str:
        """Returns the target name.

        Returns:
            Target name.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        target = self._get_target_status()
        return target.nodename

    def get_target_address(
        self, refresh: bool = False
    ) -> custom_types.TargetAddr:
        """Returns the target's address from `ffx monitor status`.

        Args:
            refresh: Unused in FFXWithMonitor (monitor status is queried directly).

        Returns:
            TargetAddr from `ffx monitor status`.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        monitor_target = self._get_target_status()
        if not monitor_target.addresses:
            raise ffx_errors.FfxCommandError(
                f"No addresses found for target '{self._target_name}' in monitor status"
            )
        return monitor_target.addresses[0]

    def get_target_board(self) -> str:
        """Returns the target's board.

        Returns:
            Target's board.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        target = self._get_target_status()
        if target.rcs_state != "Y":
            raise errors.DeviceNotConnectedError(
                f"{self._target_name} is not connected to host"
            )
        return target.target_type.split(".")[1]

    def get_target_product(self) -> str:
        """Returns the target's product.

        Returns:
            Target's product.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxCommandError: In case of other FFX command failure.
        """
        target = self._get_target_status()
        if target.rcs_state != "Y":
            raise errors.DeviceNotConnectedError(
                f"{self._target_name} is not connected to host"
            )
        return target.target_type.split(".", maxsplit=1)[0]

    def wait_for_rcs_connection(
        self,
        include_target_name: bool = False,
        timeout: float | None = None,
    ) -> None:
        """Wait until FFX is able to establish a RCS connection to the target.

        Args:
            include_target_name: Unused in FFXWithMonitor (monitor always queries by target name).
            timeout: Maximum time in seconds to wait for the RCS connection.
                If None (default), waits indefinitely.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target within the
                timeout.
            FfxCommandError: In case of other FFX command failure.
        """
        _LOGGER.info("Waiting for %s to connect to host...", self._target_name)
        deadline: float | None = (
            time.monotonic() + timeout if timeout is not None else None
        )
        while True:
            target = self._get_target_status()
            if target.target_state == "Product" and target.rcs_state == "Y":
                _LOGGER.info("%s is connected to host", self._target_name)
                return
            if deadline is not None and time.monotonic() >= deadline:
                raise errors.DeviceNotConnectedError(
                    f"{self._target_name} did not connect to host within "
                    f"{timeout} seconds."
                )
            time.sleep(_MONITOR_CONNECTION_POLL_INTERVAL_SECS)

    def wait_for_rcs_disconnection(
        self,
        timeout: float | None = None,
    ) -> None:
        """Wait until FFX observes that the RCS connection to the target has been disconnected.

        In addition to confirming the disconnection by querying the target
        directly, this waits for `ffx monitor` to observe the disconnection.
        Otherwise, a subsequent `wait_for_rcs_connection()` may observe stale
        monitor state from before the disconnection and return immediately,
        e.g. while the target is still rebooting.

        If the monitor does not observe the disconnection within
        `_MONITOR_DISCONNECTION_TIMEOUT_SECS` (or the remaining `timeout`), a
        warning is logged and this method returns.

        Args:
            timeout: Maximum time in seconds to wait for the RCS disconnection.
                If None (default), waits indefinitely for the target to
                disconnect and up to `_MONITOR_DISCONNECTION_TIMEOUT_SECS` for
                `ffx monitor` to observe it.

        Raises:
            DeviceNotConnectedError: If FFX fails to reach target.
            FfxTimeoutError: If the target does not disconnect within the
                timeout.
            FfxCommandError: In case of other FFX command failure.
        """
        caller_deadline: float | None = (
            time.monotonic() + timeout if timeout is not None else None
        )
        super().wait_for_rcs_disconnection(timeout=timeout)

        _LOGGER.info(
            "Waiting for ffx monitor to observe that %s disconnected...",
            self._target_name,
        )
        now: float = time.monotonic()
        monitor_timeout: float = (
            _MONITOR_DISCONNECTION_TIMEOUT_SECS
            if caller_deadline is None
            else min(
                _MONITOR_DISCONNECTION_TIMEOUT_SECS,
                max(0.0, caller_deadline - now),
            )
        )
        deadline: float = now + monitor_timeout
        while True:
            target = self._get_target_status()
            if not (
                target.target_state == "Product" and target.rcs_state == "Y"
            ):
                _LOGGER.info(
                    "ffx monitor observed that %s disconnected",
                    self._target_name,
                )
                return
            if time.monotonic() >= deadline:
                _LOGGER.warning(
                    "ffx monitor did not observe that %s disconnected within "
                    "%s seconds.",
                    self._target_name,
                    monitor_timeout,
                )
                return
            time.sleep(_MONITOR_CONNECTION_POLL_INTERVAL_SECS)

    def _get_target_status(self) -> MonitorTargetInfo:
        """Gets the status information of the target node from 'ffx monitor status'.

        This method is valid only when 'ffx monitor start` session was running
        for this target. It parses the output of `ffx --machine json monitor status`
        to find the dictionary corresponding to the current target node.

        This method will only provide status information when the Ffx object
        node name is provided.

        Returns:
            A MonitorTargetInfo containing the status information of the target
            node, an empty MonitorTargetInfo if the target is not found in the
            monitor output.
        """
        cmd = _FFX_MONITOR_CMDS["MONITOR_STATUS"]
        statuses = self.run(
            cmd=cmd,
            include_target=False,
        )
        targets = json.loads(statuses).get("targets", [])
        for target in targets:
            if target["nodename"] == self._target_name:
                addresses = []
                for addr in target.get("addresses", []):
                    ssh_port = addr["ssh_port"]
                    if ssh_port == 0:
                        ssh_port = None

                    addresses.append(
                        custom_types.IpPort(ip=addr["ip"], port=ssh_port)
                    )
                target["addresses"] = addresses
                return MonitorTargetInfo(
                    **target
                )  # pytype: disable=wrong-arg-types
        return MonitorTargetInfo()
