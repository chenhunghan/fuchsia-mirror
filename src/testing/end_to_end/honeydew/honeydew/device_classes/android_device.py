# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidDevice device class implementation.

This layer owns all device-facing Android interactions in Starnix, keeping
product variations isolated from test logic while reusing FuchsiaDevice
capabilities (including self.adb) directly via inheritance.
"""

import contextlib
import functools
import ipaddress
import json
import logging
import re
import shutil
import time
import types
from datetime import timedelta
from pathlib import Path
from typing import Any

from honeydew import errors
from honeydew.affordances.android_diagnostics import (
    android_diagnostics as android_diagnostics_affordance,
)
from honeydew.affordances.android_idle import (
    android_idle as android_idle_affordance,
)
from honeydew.affordances.android_package import (
    android_package as android_package_affordance,
)
from honeydew.affordances.android_session import (
    android_session as android_session_affordance,
)
from honeydew.affordances.input import input as input_affordance
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.affordances.ui_navigation import (
    ui_navigation as ui_navigation_affordance,
)
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.ffx.config import FfxConfigData
from honeydew.typing import custom_types
from honeydew.utils import common, host_shell, properties

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEFAULT_TCP_ADB_HOST: str = "127.0.0.1"
_DEFAULT_TCP_ADB_PORT: int = 5555
_DEFAULT_TCP_ADB_SERIAL: str = (
    f"{_DEFAULT_TCP_ADB_HOST}:{_DEFAULT_TCP_ADB_PORT}"
)
_PONTIS_STATUS_TIMEOUT_SECS: float = 5.0

_WAIT_FOR_DISCONNECT_TIMEOUT_SECS: float = 10.0
_BOOT_SETUP_CMD_TIMEOUT_SECS: float = 10.0
_BOOT_COMPLETED_GETPROP_TIMEOUT_SECS: float = 10.0
_DEFAULT_WAIT_FOR_BOOT_COMPLETE_TIMEOUT_SECS: float = 300.0
_DEFAULT_RESTART_SYSUI_TIMEOUT_SECS: float = 60.0
_DEFAULT_BOOT_POLL_INTERVAL_SECS: float = 1.0

# Bitmask for Settings.Global.STAY_ON_WHILE_PLUGGED_IN:
# BATTERY_PLUGGED_AC (1) | BATTERY_PLUGGED_USB (2) | BATTERY_PLUGGED_WIRELESS (4).
_STAY_ON_WHILE_PLUGGED_IN_ALL: str = "7"


# TODO(b/564946370): Remove temporary wrapper once 'adb root' operations are
# stable on USB.
class _SuRootContextManager(
    contextlib.AbstractContextManager[None],
    contextlib.AbstractAsyncContextManager[None],
):
    """Wrapper around use_su_root supporting both sync and async contexts."""

    def __init__(self, cm: contextlib.AbstractContextManager[None]) -> None:
        self._cm = cm

    def __enter__(self) -> None:
        return self._cm.__enter__()

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> bool | None:
        return self._cm.__exit__(exc_type, exc_val, exc_tb)

    async def __aenter__(self) -> None:
        return self.__enter__()

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: types.TracebackType | None,
    ) -> bool | None:
        return self.__exit__(exc_type, exc_val, exc_tb)


def _parse_screen_size(screen_size: Any) -> ui_types.Size | None:
    """Parses a "widthxheight" screen size string (e.g. "480x800").

    Args:
        screen_size: Screen size string, or None if not provided.

    Returns:
        Parsed screen size, or None if `screen_size` is None.

    Raises:
        errors.ConfigError: If `screen_size` is not in "widthxheight" format.
    """
    if screen_size is None:
        return None
    try:
        width_str, height_str = screen_size.split("x")
        return ui_types.Size(int(width_str), int(height_str))
    except (ValueError, AttributeError) as e:
        raise errors.ConfigError(
            f"Invalid screen_size parameter: {screen_size!r}. "
            "Expected format 'widthxheight' (e.g., '480x800')."
        ) from e


class AndroidDevice(fuchsia_device.FuchsiaDevice):
    """Base Android-in-Starnix device inheriting from FuchsiaDevice."""

    # Product profile class attributes - overridden by product specializations.
    sysui_package: str = ""
    launcher_package: str = ""
    setup_wizard_package: str = ""
    adb_auth_activity: str = ""
    idle_screen_resource_id: str = ""
    apps_list_resource_id: str = ""
    apps_list_activity: str | None = None
    standardize_screenshots: bool = False
    idled_after_boot_prop: str = ""
    after_boot_duration: timedelta | None = None
    enable_black_screen_mitigation: bool = False

    _default_screen_size: ui_types.Size | None = None
    _default_after_boot: timedelta = timedelta(seconds=0)

    def __init__(
        self,
        *,
        device_info: custom_types.DeviceInfo,
        ffx_config_data: FfxConfigData,
        config: dict[str, Any] | None = None,
    ) -> None:
        """Initializes AndroidDevice.

        Args:
            device_info: Device information.
            ffx_config_data: Configuration that needs to be used while running
                FFX commands.
            config: Honeydew configuration dictionary, if any. Expected keys:
                {
                    "screen_size": str,  # e.g., "480x800"
                    "configure_on_boot": bool,  # Default True; applies post-boot settings
                    "transports": {
                        "adb": {
                            "use_adb_usb": bool,  # Prefer USB serial over IP
                        }
                    }
                }
        """
        self._screen_size_override: ui_types.Size | None = _parse_screen_size(
            (config or {}).get("screen_size")
        )
        self._screen_size_cache: ui_types.Size | None = None
        idle_config = (
            (config or {}).get("affordances", {}).get("android_idle", {})
        )
        after_boot_secs = idle_config.get("after_boot_duration_seconds")
        if after_boot_secs is not None:
            self.after_boot_duration = timedelta(seconds=float(after_boot_secs))
        idled_prop = idle_config.get("idled_after_boot_prop")
        if idled_prop is not None:
            self.idled_after_boot_prop = str(idled_prop)
        super().__init__(
            device_info=device_info,
            ffx_config_data=ffx_config_data,
            config=config,
        )
        self.initialize()

    def _get_target_addr(self) -> custom_types.TargetAddr | None:
        """Returns the target address from DeviceInfo or FFX, or None.

        Returns:
            TargetAddr if available, or None otherwise.
        """
        target_addr: custom_types.TargetAddr | None = self._device_info.address
        if target_addr is None:
            try:
                target_addr = self.ffx.get_target_address()
            except errors.HoneydewError as err:
                _LOGGER.debug(
                    "Failed to get target address from FFX for %s: %s",
                    self.device_name,
                    err,
                )
                return None
        return target_addr

    def _get_pontis_target_addr(self) -> custom_types.IpPort | None:
        """Returns the target IpPort if connected via Pontis, else None.

        Returns:
            The target IpPort if connected via Pontis, or None otherwise.
        """
        target_addr = self._get_target_addr()

        if isinstance(target_addr, custom_types.IpPort) and isinstance(
            target_addr.ip, ipaddress.IPv6Address
        ):
            scope_id: str | None = target_addr.ip.scope_id
            if scope_id is not None and "pontis" in scope_id.lower():
                return target_addr

        return None

    def _is_pontis_target(self) -> bool:
        """Returns True if the target device is connected via Pontis.

        Returns:
            True if connected via Pontis, or False otherwise.
        """
        return self._get_pontis_target_addr() is not None

    def _is_loopback_target(self) -> bool:
        """Returns True if the target device is connected via loopback."""
        target_addr = self._get_target_addr()
        return (
            isinstance(target_addr, custom_types.IpPort)
            and target_addr.ip.is_loopback
        )

    def _resolve_pontis_adb_serial(
        self,
        pontis_addr: custom_types.IpPort,
        hardware_serial: str | None = None,
    ) -> str:
        """Resolves the TCP ADB serial endpoint for a Pontis-connected target.

        Queries `pontis status --format=json` for a local ADB proxy listen port
        (matching `hardware_serial` when multiple ADB devices are present) and
        falls back to `[<pontis_ip>]:5555`. Expected command or JSON parsing
        errors are caught and logged at debug level so resolution falls back
        gracefully; unexpected exceptions (such as Mobly test signals) propagate
        to the caller.

        Args:
            pontis_addr: The Pontis IPv6 target address and port.
            hardware_serial: Optional hardware serial number to match when
                multiple ADB devices are present in `pontis status`.

        Returns:
            The resolved `<host>:<port>` TCP ADB serial string.
        """
        if shutil.which("pontis") is not None:
            try:
                # Relevant structure of `pontis status --format=json`:
                # {
                #   "bridgeStatus": [
                #     {
                #       "deviceStatus": [
                #         {
                #           "deviceType": "ADB with optimizations",
                #           "localDeviceId": "ADBFuchsia<hardware_serial>",
                #           "listenPort": 39181
                #         }
                #       ]
                #     }
                #   ]
                # }
                output = host_shell.run(
                    cmd=["pontis", "status", "--format=json"],
                    capture_output=True,
                    timeout=_PONTIS_STATUS_TIMEOUT_SECS,
                )
                status = json.loads(output or "{}")
                candidates: list[tuple[str, int]] = []
                for bridge in status.get("bridgeStatus", []):
                    if not isinstance(bridge, dict):
                        continue
                    for device in bridge.get("deviceStatus", []):
                        if not isinstance(device, dict):
                            continue
                        device_type = str(device.get("deviceType", "")).upper()
                        if (
                            "ADB" in device_type
                            and "WIRELESS_ADB" not in device_type
                        ):
                            listen_port = device.get("listenPort")
                            if (
                                isinstance(listen_port, int)
                                and not isinstance(listen_port, bool)
                                and listen_port > 0
                            ):
                                local_id = str(device.get("localDeviceId", ""))
                                candidates.append((local_id, listen_port))

                if hardware_serial:
                    for local_id, listen_port in candidates:
                        if hardware_serial in local_id:
                            return f"{_DEFAULT_TCP_ADB_HOST}:{listen_port}"
                elif len(candidates) == 1:
                    return f"{_DEFAULT_TCP_ADB_HOST}:{candidates[0][1]}"
                if len(candidates) > 1:
                    _LOGGER.warning(
                        "Multiple Pontis ADB candidates (%d) found for %s, but "
                        "none matched hardware_serial=%r; falling back to "
                        "[%s]:%d",
                        len(candidates),
                        self.device_name,
                        hardware_serial,
                        pontis_addr.ip,
                        _DEFAULT_TCP_ADB_PORT,
                    )
            except (
                errors.HoneydewError,
                json.JSONDecodeError,
                KeyError,
                TypeError,
                ValueError,
            ) as err:
                _LOGGER.debug(
                    "Failed to resolve Pontis ADB port via `pontis status` for %s: %s",
                    self.device_name,
                    err,
                )
        else:
            _LOGGER.debug(
                "`pontis` binary not found in PATH; skipping `pontis status` for %s",
                self.device_name,
            )

        return str(
            custom_types.IpPort(ip=pontis_addr.ip, port=_DEFAULT_TCP_ADB_PORT)
        )

    def _get_hardware_serial(self) -> str | None:
        """Returns the hardware serial number from DeviceInfo or FFX, or None.

        Returns:
            Hardware serial number string if available, or None otherwise.
        """
        serial_number: str | None = self._device_info.serial_number
        if serial_number is None:
            try:
                serial_number = self.ffx.serial_number
            except errors.HoneydewError as err:
                _LOGGER.debug(
                    "Failed to get serial number from FFX for %s: %s",
                    self.device_name,
                    err,
                )
                return None
        return serial_number

    def _resolve_adb_serial(self, *, use_adb_usb: bool = True) -> str:
        """Resolves the ADB serial identifier (USB serial or TCP `<ip>:<port>`) for the target.

        Args:
            use_adb_usb: Whether to connect to ADB over USB. When False, connects
                to ADB over TCP instead of USB.

        Returns:
            The resolved ADB serial string.

        Raises:
            errors.NotSupportedError: If the ADB serial could not be resolved.
        """
        adb_serial: str | None = None
        # ADB Over USB
        if use_adb_usb is True:
            adb_serial = self._get_hardware_serial()
        # ADB Over TCP
        else:
            pontis_addr: custom_types.IpPort | None = (
                self._get_pontis_target_addr()
            )
            # ADB Over TCP on remote/Pontis based setup
            if pontis_addr:
                adb_serial = self._resolve_pontis_adb_serial(
                    pontis_addr=pontis_addr,
                    hardware_serial=self._get_hardware_serial(),
                )
            # ADB Over TCP local setup
            else:
                target_addr: custom_types.TargetAddr | None = (
                    self._get_target_addr()
                )
                if isinstance(target_addr, custom_types.IpPort):
                    adb_serial = str(
                        custom_types.IpPort(
                            ip=target_addr.ip, port=_DEFAULT_TCP_ADB_PORT
                        )
                    )
                else:
                    adb_serial = _DEFAULT_TCP_ADB_SERIAL

        if adb_serial is None:
            _LOGGER.warning(
                "ADB transport is not supported on %s as 'serial_number' was not provided and could not be retrieved",
                self.device_name,
            )
            raise errors.NotSupportedError(
                f"ADB transport is not supported on {self.device_name} "
                f"as 'serial_number' was not provided and could not be retrieved"
            )
        return adb_serial

    @properties.Transport
    def adb(self) -> adb_transport.Adb:
        """Returns the ADB transport object configured to use su_root methods.

        Note: Returns a modified Adb instance where root/unroot methods (root,
        unroot, and use_adb_root) are redirected to their 'su' counterparts
        (su_root, su_unroot, and use_su_root) as a temporary workaround for
        Starnix (b/564946370).

        Returns:
            ADB transport interface implementation.

        Raises:
            errors.NotEnabledError: If ADB transport is not enabled.
            errors.NotSupportedError: If ADB transport is not supported by Fuchsia device.
        """
        run_isolated_server = True
        vendor_keys_path = None
        enabled = True
        output_dir: Path | str | None = None
        attempts = adb_transport._DEFAULT_RUN_ATTEMPTS
        adb_config: dict[str, Any] = {}

        if self._config:
            adb_config = self._config.get("transports", {}).get("adb", {})
            run_isolated_server = adb_config.get("run_isolated_server", True)
            vendor_keys_path = adb_config.get("vendor_keys_path")
            enabled = adb_config.get("enabled", True)
            output_dir = adb_config.get("output_dir")
            attempts = adb_config.get(
                "attempts", adb_transport._DEFAULT_RUN_ATTEMPTS
            )

        if not output_dir and self._ffx_config_data.logs_dir:
            ffx_logs_path = Path(self._ffx_config_data.logs_dir)
            if ffx_logs_path.name == "ffx":
                output_dir = ffx_logs_path.parent / "adb"
            else:
                output_dir = ffx_logs_path / "adb"

        # Note - An existing ADB implementation in //vendor/google is used by
        # some Lacewing tests. Running two ADB server implementations against
        # the same device causes conflicts. Tests that manage ADB externally
        # can set `enabled` to False to disable Honeydew's ADB transport.
        # TODO(b/559563915): Delete this `enabled` config once legacy ADB
        # server implementations are removed and all Lacewing tests use
        # Honeydew's ADB transport.
        if not enabled:
            _LOGGER.info(
                "ADB transport is not enabled for '%s'",
                self.device_name,
            )
            raise errors.NotEnabledError(
                f"ADB transport is not enabled for '{self.device_name}'"
            )

        use_adb_usb: bool = (
            adb_config["use_adb_usb"]
            if "use_adb_usb" in adb_config
            else not (self._is_pontis_target() or self._is_loopback_target())
        )
        adb_serial: str = self._resolve_adb_serial(use_adb_usb=use_adb_usb)

        adb_obj: adb_transport.Adb = adb_transport.Adb(
            device_name=self.device_name,
            serial_number=adb_serial,
            run_isolated_server=run_isolated_server,
            output_dir=output_dir,
            vendor_keys_path=vendor_keys_path,
            ffx_transport=self.ffx,
            attempts=attempts,
        )
        # TODO(b/564946370): Remove temporary workaround once 'adb root' is
        # stable on USB connections.
        adb_obj.root = adb_obj.su_root  # type: ignore[method-assign]
        adb_obj.unroot = adb_obj.su_unroot  # type: ignore[method-assign]

        def _use_su_root(*args: Any, **kwargs: Any) -> _SuRootContextManager:
            return _SuRootContextManager(adb_obj.use_su_root(*args, **kwargs))

        adb_obj.use_adb_root = _use_su_root  # type: ignore[method-assign,assignment]
        self.register_for_on_device_close(adb_obj.close)
        return adb_obj

    def _check_connections(self) -> None:
        """Checks health of all device transports including ADB."""
        super()._check_connections()

        try:
            with self._health_check_leg("adb"):
                self.adb.check_connection()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping the ADB connection check.",
                self.device_name,
            )

    def wait_for_offline(self) -> None:
        """Wait for Android ADB and Fuchsia device to go offline.

        Raises:
            errors.FuchsiaDeviceError: If Fuchsia device is not offline.
        """
        try:
            adb = self.adb
            _LOGGER.info(
                "Waiting for ADB to disconnect on %s...", self.device_name
            )
            adb.run(
                ["wait-for-disconnect"],
                timeout=_WAIT_FOR_DISCONNECT_TIMEOUT_SECS,
                attempts=1,
            )
        except (errors.NotSupportedError, errors.NotEnabledError):
            _LOGGER.info(
                "ADB is not enabled or supported on %s, so skipping ADB wait_for_offline.",
                self.device_name,
            )
        except adb_errors.AdbError as err:
            # ADB may have already disconnected before wait-for-disconnect
            # started, or timed out during soft reboot teardown; log a warning
            # and proceed to Fuchsia wait_for_offline().
            _LOGGER.warning(
                "Ignoring error while waiting for ADB disconnect on %s: %s",
                self.device_name,
                err,
            )
        super().wait_for_offline()

    async def wait_for_online(
        self,
        timeout: float | None = None,
        poll_interval: float | None = None,
    ) -> None:
        """Wait for Fuchsia and Android to go online and complete boot.

        Waits for:
        1. Fuchsia device online (`super().wait_for_online()`)
        2. ADB device connection (`adb wait-for-device`)
        3. `sys.boot_completed` == '1'
        4. User 0 unlocked (`sys.user.0.ce_available` or `dumpsys user`)
        5. `/data` or `/storage/emulated` mounted (`/proc/self/mountinfo`)

        Args:
            timeout: Maximum time in seconds to wait for Fuchsia and Android
                boot to complete. Defaults to 300.0 seconds.
            poll_interval: Interval in seconds between polling attempts.
                Defaults to 1.0 second.

        Raises:
            errors.FuchsiaDeviceError: If Fuchsia or Android fails to go online
                and complete boot within timeout (matching the
                `RebootCapableDevice.wait_for_online` exception contract).
        """
        resolved_timeout: float = (
            _DEFAULT_WAIT_FOR_BOOT_COMPLETE_TIMEOUT_SECS
            if timeout is None
            else timeout
        )
        resolved_poll_interval: float = (
            _DEFAULT_BOOT_POLL_INTERVAL_SECS
            if poll_interval is None
            else poll_interval
        )
        start_time: float = time.time()
        await super().wait_for_online()
        try:
            adb = self.adb
        except (errors.NotSupportedError, errors.NotEnabledError):
            _LOGGER.info(
                "ADB is not enabled or supported on %s, so skipping Android wait_for_online.",
                self.device_name,
            )
            return

        _LOGGER.info(
            "Waiting for Android boot complete on %s...", self.device_name
        )
        remaining_timeout: float = resolved_timeout - (time.time() - start_time)
        if remaining_timeout <= 0:
            raise errors.FuchsiaDeviceError(
                f"Timed out after {resolved_timeout} seconds waiting for "
                f"{self.device_name} to complete boot (ADB connection)."
            )
        try:
            adb.check_connection(timeout=remaining_timeout)
        except adb_errors.AdbError as err:
            raise errors.FuchsiaDeviceError(
                f"Failed waiting for ADB connection on {self.device_name} "
                f"during wait_for_online: {err}"
            ) from err
        steps = (
            ("sys.boot_completed != '1'", self._is_boot_completed),
            ("user not unlocked", self._is_user_unlocked),
            ("/data storage not mounted", self._is_storage_mounted),
        )
        for step_desc, state_fn in steps:
            elapsed: float = time.time() - start_time
            remaining_timeout = resolved_timeout - elapsed
            if remaining_timeout <= 0:
                raise errors.FuchsiaDeviceError(
                    f"Timed out after {resolved_timeout} seconds waiting for "
                    f"{self.device_name} to complete boot ({step_desc})."
                )
            cmd_timeout: float = min(
                remaining_timeout, _BOOT_COMPLETED_GETPROP_TIMEOUT_SECS
            )
            step_state_fn = (
                functools.update_wrapper(
                    functools.partial(state_fn, timeout=cmd_timeout),
                    state_fn,
                )
                if cmd_timeout < _BOOT_COMPLETED_GETPROP_TIMEOUT_SECS
                else state_fn
            )
            try:
                await common.wait_for_state(
                    state_fn=step_state_fn,
                    expected_state=True,
                    timeout=remaining_timeout,
                    wait_time=resolved_poll_interval,
                )
            except errors.HoneydewTimeoutError as err:
                raise errors.FuchsiaDeviceError(
                    f"Timed out after {resolved_timeout} seconds waiting for "
                    f"{self.device_name} to complete boot ({step_desc})."
                ) from err
        _LOGGER.info("Reached Android boot complete on %s.", self.device_name)

    def _close_uiautomator(self) -> None:
        """Closes any active UiAutomator service and clears the cached device."""
        if "ui_navigation" in self.__dict__:
            self.ui_navigation.close()

    async def close(self) -> None:
        """Clean up method."""
        self._close_uiautomator()
        await super().close()

    async def on_device_boot(self) -> None:
        """Take actions after the device is rebooted.

        Raises:
            adb_errors.AdbError: If ADB fails to reconnect or post-boot setup
                fails.
            errors.HealthCheckError: If post-boot transport health check fails.
            errors.HoneydewError: On Fuchsia Controller communications failure.
        """
        self._close_uiautomator()
        adb_enabled: bool = True
        try:
            self.adb.on_device_boot()
        except (errors.NotSupportedError, errors.NotEnabledError):
            adb_enabled = False
            _LOGGER.info(
                "ADB is not enabled or supported on %s, so skipping ADB on_device_boot.",
                self.device_name,
            )

        await super().on_device_boot()
        self._screen_size_cache = None
        self.__dict__.pop("gestures", None)
        self.__dict__.pop("buttons", None)
        self.__dict__.pop("ui_navigation", None)

        if adb_enabled:
            self._on_boot_complete()

    async def reboot(self) -> None:
        """Reboots the device and waits for Android boot completion.

        Raises:
            errors.FuchsiaDeviceError: On failure to reboot Fuchsia device or
                wait for Android boot completion.
            errors.HoneydewError: If ADB or Android post-boot setup fails.
        """
        # TODO(b/565917205): Update to trigger reboot via 'adb reboot'.
        _LOGGER.info("Rebooting %s...", self.device_name)
        try:
            self.android_session.session_will_change()
        except (errors.NotSupportedError, errors.NotEnabledError):
            _LOGGER.debug(
                "ADB is not enabled or supported on %s, so skipping session_will_change.",
                self.device_name,
            )
        await super().reboot()

    def _disable_background_dexopt(self) -> None:
        """Disables background dexopt jobs (`pm bg-dexopt-job --disable`).

        Uses `use_su_root` rather than `use_adb_root` to set
        `pm.dexopt.disable_bg_dexopt` without restarting `adbd` or resetting
        composite USB, then disables `bg-dexopt-job` in a separate `try` block.
        Best-effort optimization helper: `AdbError` is intentionally suppressed
        when `su` or `pm bg-dexopt-job` is unavailable on unrooted builds.
        """
        try:
            # TODO(b/564946370): ADB.root()/unroot() needs more changes to
            # handle USB stack reset. Until then use ADB.use_su_root(), and
            # replace with use_adb_root() once 'adb root' is stable on USB
            # connections.
            with self.adb.use_su_root(timeout=_BOOT_SETUP_CMD_TIMEOUT_SECS):
                self.adb.setprop(
                    "pm.dexopt.disable_bg_dexopt",
                    "true",
                    timeout=_BOOT_SETUP_CMD_TIMEOUT_SECS,
                    attempts=1,
                )
        except adb_errors.AdbError as err:
            _LOGGER.debug(
                "Failed to set pm.dexopt.disable_bg_dexopt on %s: %s",
                self.device_name,
                err,
            )
        try:
            self.adb.run(
                ["shell", "pm", "bg-dexopt-job", "--disable"],
                timeout=_BOOT_SETUP_CMD_TIMEOUT_SECS,
                attempts=1,
            )
        except adb_errors.AdbError as err:
            _LOGGER.debug(
                "Failed to disable background dexopt job on %s: %s",
                self.device_name,
                err,
            )

    def _enable_stay_on_while_plugged_in(self) -> None:
        """Keeps the screen on while the device is plugged in.

        Best-effort helper: logs a warning and continues if the setting could
        not be applied via ADB.
        """
        try:
            self.adb.run(
                [
                    "shell",
                    "settings",
                    "put",
                    "global",
                    "stay_on_while_plugged_in",
                    _STAY_ON_WHILE_PLUGGED_IN_ALL,
                ],
                timeout=_BOOT_SETUP_CMD_TIMEOUT_SECS,
                attempts=1,
            )
        except adb_errors.AdbError as err:
            _LOGGER.warning(
                "Failed to set stay_on_while_plugged_in on %s: %s",
                self.device_name,
                err,
            )

    def _is_boot_completed(
        self,
        timeout: float = _BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
    ) -> bool:
        """Checks whether `sys.boot_completed` is '1'.

        Args:
            timeout: Maximum time in seconds for the ADB command.

        Returns:
            True if `sys.boot_completed` is '1', or False otherwise.
        """
        try:
            return (
                self.adb.getprop(
                    "sys.boot_completed",
                    timeout=timeout,
                    attempts=1,
                )
                == "1"
            )
        except adb_errors.AdbError as err:
            _LOGGER.debug(
                "Error querying sys.boot_completed on %s during boot: %s",
                self.device_name,
                err,
            )
            return False

    def _is_user_unlocked(
        self,
        timeout: float = _BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
    ) -> bool:
        """Checks whether user 0 is unlocked (`sys.user.0.ce_available` or `dumpsys user`).

        Args:
            timeout: Maximum time in seconds for each ADB command.

        Returns:
            True if user 0 is unlocked, or False otherwise.
        """
        try:
            if (
                self.adb.getprop(
                    "sys.user.0.ce_available",
                    timeout=timeout,
                    attempts=1,
                )
                == "true"
            ):
                return True
            out: str = self.adb.run(
                ["shell", "dumpsys", "user"],
                timeout=timeout,
                attempts=1,
            )
            return (
                "mUserUnlocked=true" in out
                or "State: RUNNING_UNLOCKED" in out
                or "RUNNING_UNLOCKED" in out
            )
        except adb_errors.AdbError as err:
            _LOGGER.debug(
                "Error checking user unlock state on %s during boot: %s",
                self.device_name,
                err,
            )
            return False

    def _is_storage_mounted(
        self,
        timeout: float = _BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
    ) -> bool:
        """Checks whether `/data` or `/storage/emulated` is mounted (`/proc/self/mountinfo`).

        Args:
            timeout: Maximum time in seconds for the ADB command.

        Returns:
            True if `/data` or `/storage/emulated` storage is mounted, or False
            otherwise.
        """
        try:
            out: str = self.adb.run(
                ["shell", "cat", "/proc/self/mountinfo"],
                timeout=timeout,
                attempts=1,
            )
            return any(
                "/storage/emulated" in line or "/data" in line
                for line in out.splitlines()
            )
        except adb_errors.AdbError as err:
            _LOGGER.debug(
                "Error checking storage mount state on %s during boot: %s",
                self.device_name,
                err,
            )
            return False

    def _is_sysui_running(
        self,
        timeout: float = _BOOT_COMPLETED_GETPROP_TIMEOUT_SECS,
    ) -> bool:
        """Checks whether the configured SystemUI package process is running.

        Args:
            timeout: Maximum time in seconds for the ADB command.

        Returns:
            True if `pidof <sysui_package>` returns a non-empty PID, or False
            otherwise.
        """
        try:
            out: str = self.adb.run(
                ["shell", "pidof", self.sysui_package],
                timeout=timeout,
                attempts=1,
            )
            return bool(out.strip())
        except adb_errors.AdbError as err:
            _LOGGER.debug(
                "Error checking if SystemUI (%s) is running on %s: %s",
                self.sysui_package,
                self.device_name,
                err,
            )
            return False

    async def _wait_for_sysui_ready(
        self,
        timeout: float = _DEFAULT_RESTART_SYSUI_TIMEOUT_SECS,
        poll_interval: float = _DEFAULT_BOOT_POLL_INTERVAL_SECS,
    ) -> None:
        """Waits for the SystemUI process to be running.

        Args:
            timeout: Maximum time in seconds to wait for SystemUI.
            poll_interval: Interval in seconds between polling attempts.

        Raises:
            errors.FuchsiaDeviceError: If SystemUI does not start within
                timeout.
        """
        try:
            await common.wait_for_state(
                state_fn=self._is_sysui_running,
                expected_state=True,
                timeout=timeout,
                wait_time=poll_interval,
            )
        except errors.HoneydewTimeoutError as err:
            raise errors.FuchsiaDeviceError(
                f"Timed out after {timeout} seconds waiting for SystemUI "
                f"({self.sysui_package}) to restart on {self.device_name}."
            ) from err

    async def restart_sysui(
        self,
        timeout: float = _DEFAULT_RESTART_SYSUI_TIMEOUT_SECS,
    ) -> None:
        """Restarts SystemUI and waits for the SystemUI process to be ready.

        Args:
            timeout: Maximum time in seconds to wait for SystemUI to restart.
                Defaults to 60.0 seconds.

        Raises:
            errors.FuchsiaDeviceError: If no SystemUI package is configured or
                waiting for SystemUI to restart times out.
            adb_errors.AdbError: If stopping SystemUI fails.
        """
        if not self.sysui_package:
            raise errors.FuchsiaDeviceError(
                f"No SystemUI package is configured for {self.device_name}."
            )
        _LOGGER.info("Restarting SystemUI on %s...", self.device_name)
        self.adb.run(["shell", "am", "force-stop", self.sysui_package])
        await self._wait_for_sysui_ready(timeout=timeout)
        self._refresh_screen_size()

    def initialize(self) -> None:
        """Initializes the Android device connection and product state."""
        if not self.sysui_package:
            return
        try:
            self.screen_size()
        except (errors.NotSupportedError, errors.NotEnabledError):
            _LOGGER.info(
                "ADB is not enabled or supported on %s, so skipping Android initialization.",
                self.device_name,
            )
            return
        self._product_setup()

    def _product_setup(self) -> None:
        """Product-specific setup hook, no-op by default."""

    def _on_screen_size_changed(self) -> None:
        """Hook called when cached screen size changes."""
        self.__dict__.pop("gestures", None)

    def _refresh_screen_size(self) -> None:
        """Populates the cached screen size if it is not already known."""
        try:
            self.screen_size()
        except (
            errors.NotSupportedError,
            errors.NotEnabledError,
            errors.ConfigError,
        ) as err:
            _LOGGER.debug(
                "Skipping screen size refresh on %s: %s",
                self.device_name,
                err,
            )

    def _on_boot_complete(self) -> None:
        """Hook invoked on device boot once Android has booted.

        Applies post-boot Android settings (unless `configure_on_boot` is set
        to False in Honeydew config), records the new Android session,
        populates the cached screen size if it is not already known, and
        dismisses blocking dialogs.
        """
        configure_on_boot: bool = (
            self._config.get("configure_on_boot", True)
            if self._config
            else True
        )
        if configure_on_boot:
            self._disable_background_dexopt()
            self._enable_stay_on_while_plugged_in()
        try:
            self.android_session.record_session()
        except (
            errors.NotSupportedError,
            errors.NotEnabledError,
            errors.HoneydewError,
        ) as err:
            _LOGGER.warning(
                "Failed to record Android session on %s: %s",
                self.device_name,
                err,
            )
        self._screen_size_cache = None
        self._refresh_screen_size()
        if not self.sysui_package:
            return
        self._product_setup()
        try:
            self.dismiss_blocking_dialogs()
        except errors.NotEnabledError:
            _LOGGER.info(
                "ADB is not enabled on %s, so skipping dismiss_blocking_dialogs.",
                self.device_name,
            )

    def screen_size(self) -> ui_types.Size:
        """Returns the screen size for this device.

        If 'screen_size' was provided in config, it must be a string in the
        form "widthxheight" (e.g., "480x800") and takes precedence.

        Returns:
            Resolved screen size.

        Raises:
            errors.NotEnabledError: If ADB transport is disabled.
            errors.NotSupportedError: If ADB transport is not supported.
            errors.ConfigError: If screen size is not configured and cannot be
                queried from the device.
        """
        if self._screen_size_override is not None:
            return self._screen_size_override

        if self._screen_size_cache is not None:
            return self._screen_size_cache

        try:
            output = self.adb.run(["shell", "wm", "size"])
            override_match = re.search(r"Override size:\s*(\d+)x(\d+)", output)
            physical_match = re.search(r"Physical size:\s*(\d+)x(\d+)", output)
            match = override_match or physical_match
            if match:
                new_size = ui_types.Size(
                    int(match.group(1)), int(match.group(2))
                )
                self._screen_size_cache = new_size
                self._on_screen_size_changed()
                return new_size
        except errors.NotEnabledError:
            raise
        except (adb_errors.AdbError, errors.HoneydewError) as e:
            _LOGGER.warning(f"Failed to query screen size via wm size: {e}")

        if self._default_screen_size is not None:
            return self._default_screen_size

        raise errors.ConfigError("Screen size not configured for this device")

    def _resolve_after_boot_duration(self) -> timedelta:
        """Returns the configured or default post-boot idle duration."""
        if self.after_boot_duration is not None:
            return self.after_boot_duration
        return self._default_after_boot

    def _resolve_idled_after_boot_prop(self) -> str:
        """Returns the configured or default post-boot idle sysprop."""
        return self.idled_after_boot_prop

    # --- UI Navigation Hooks ---

    def _prepare_for_idle_screen(self) -> None:
        """Hook run before entering the idle screen wait loop."""

    def _handle_unexpected_screen(self, ui: Any) -> bool:
        """Handles unexpected intermediate screens during idle navigation.

        Returns:
            True if recognized and acted upon (prompting a retry), else False.
        """
        return False

    def _mitigate_stuck_ui(self, count: int) -> None:
        """Mitigates stuck UI when consecutive black screens occur."""
        if self.setup_wizard_package:
            _LOGGER.info(
                f"Force-stopping {self.setup_wizard_package} to mitigate stuck UI..."
            )
            try:
                self.adb.run(
                    ["shell", "am", "force-stop", self.setup_wizard_package]
                )
            except errors.HoneydewError as e:
                _LOGGER.warning(f"Failed to force-stop setup wizard: {e}")

    def dismiss_blocking_dialogs(self) -> bool:
        """Dismisses any active blocking dialogs such as ADB auth prompt.

        Returns:
            True if an active blocking dialog was detected and dismissed,
            False otherwise.

        Raises:
            errors.HoneydewError: If querying or dismissing dialogs via ADB
                fails.
        """
        return self.ui_navigation.dismiss_blocking_dialogs()

    # --- Affordances ---

    @properties.Affordance
    def ui_navigation(self) -> ui_navigation_affordance.UiNavigation:
        """Returns UiNavigation affordance object.

        Returns:
            ui_navigation_affordance.UiNavigation object

        Raises:
            errors.NotEnabledError: If adb is not enabled on this device.
            errors.NotSupportedError: If UiNavigation affordance is not
                supported.
        """
        if "ui_navigation" not in self.__dict__:
            self.__dict__[
                "ui_navigation"
            ] = ui_navigation_affordance.UiNavigation(self.adb, self)
        return self.__dict__["ui_navigation"]

    @properties.Affordance
    def gestures(self) -> input_affordance.Gestures:
        """Returns Gestures affordance object.

        Returns:
            input_affordance.Gestures object

        Raises:
            errors.NotEnabledError: If user_input is not enabled on this device.
            errors.NotSupportedError: If Gestures affordance is not supported.
        """
        return input_affordance.Gestures(self.user_input, self.screen_size)

    @properties.Affordance
    def buttons(self) -> input_affordance.Buttons:
        """Returns Buttons affordance object.

        Returns:
            input_affordance.Buttons object

        Raises:
            errors.NotEnabledError: If adb is not enabled.
            errors.NotSupportedError: If Buttons affordance is not supported.
        """
        adb = self.adb
        user_input = None
        try:
            user_input = self.user_input
        except (errors.NotSupportedError, errors.NotEnabledError):
            pass
        return input_affordance.Buttons(adb, user_input)

    @properties.Affordance
    def android_session(self) -> android_session_affordance.AndroidSession:
        """Returns an Android session affordance object.

        Returns:
            android_session.AndroidSession object
        """
        return android_session_affordance.AndroidSession(
            device_name=self.device_name, adb=self.adb
        )

    @properties.Affordance
    def diagnostics(self) -> android_diagnostics_affordance.AndroidDiagnostics:
        """Returns an Android diagnostics affordance object.

        Returns:
            android_diagnostics.AndroidDiagnostics object
        """
        return android_diagnostics_affordance.AndroidDiagnostics(
            device_name=self.device_name, adb=self.adb
        )

    @properties.Affordance
    def android_package(self) -> android_package_affordance.AndroidPackage:
        """Returns an Android package affordance object.

        Returns:
            android_package.AndroidPackage object
        """
        return android_package_affordance.AndroidPackage(
            device_name=self.device_name, adb=self.adb
        )

    @properties.Affordance
    def android_idle(self) -> android_idle_affordance.AndroidIdle:
        """Returns an Android idle affordance object.

        Returns:
            android_idle.AndroidIdle object

        Raises:
            errors.NotSupportedError: If AndroidIdle affordance is not supported
                on the device.
        """
        idle_obj = android_idle_affordance.AndroidIdle(
            device_name=self.device_name,
            adb=self.adb,
            session=self.android_session,
            idled_after_boot_prop=self._resolve_idled_after_boot_prop,
            after_boot_duration=self._resolve_after_boot_duration,
        )
        self.register_for_on_device_close(idle_obj.close)
        return idle_obj
