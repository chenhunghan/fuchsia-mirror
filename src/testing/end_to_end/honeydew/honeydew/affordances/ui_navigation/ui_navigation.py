# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Android UI navigation, UiAutomator snippet runner, and idle screen helpers.

All direct ADB operations in this module and AndroidDevice (`dumpsys`, `am start`,
`pm list packages`, APK installation) use Honeydew's `honeydew.transports.adb.Adb`
transport. Because `third_party/mobly_uiautomator` (`snippet_uiautomator`) is a
Mobly snippet client whose `UiAutomatorService` requires a Mobly `AndroidDevice`
service host to manage the on-device RPC snippet lifecycle and port forwarding,
`create_uiautomator_device` constructs a lightweight snippet host strictly for
`UiAutomatorService` registration.
"""

import asyncio
import inspect
import logging
import os
import sys
from collections.abc import Callable
from datetime import timedelta
from pathlib import Path
from typing import Any, Protocol

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.ui.screenshot.types import ScreenshotImage
from honeydew.affordances.ui_navigation.errors import (
    BlackScreenUiAutomatorError,
    UiAutomatorRetriableTaskError,
    UiNavigationError,
    UnknownStateUiAutomatorError,
)
from honeydew.transports.adb import adb as adb_transport
from honeydew.utils.control_flows import RetryAbortingError, retry_for_duration
from mobly import utils as mobly_utils
from mobly.controllers import android_device as mobly_android_device
from mobly.controllers.android_device_lib import adb as mobly_adb
from snippet_uiautomator import errors as uiautomator_errors
from snippet_uiautomator import uiautomator
from snippet_uiautomator import utils as uiautomator_utils

_LOGGER: logging.Logger = logging.getLogger(__name__)

WAIT_ON_SCREEN_SECONDS: int = 5


class UiAutomatorDevice(Protocol):
    """Protocol representing a connected UiAutomator snippet host.

    Exposes the snippet-uiautomator client via the `ui` property.
    """

    @property
    def ui(self) -> Any:
        """Snippet UiDevice client instance.

        Returns:
            The snippet-uiautomator UiDevice object.
        """
        ...


class UiNavigationHost(Protocol):
    """Protocol for the host AndroidDevice capabilities used by UiNavigation."""

    @property
    def adb(self) -> Any:
        """ADB transport instance.

        Returns:
            The underlying ADB transport instance.
        """

    @property
    def screenshot(self) -> Any:
        """Screenshot affordance instance.

        Returns:
            The screenshot affordance instance.
        """

    @property
    def launcher_package(self) -> str:
        """Android launcher package name.

        Returns:
            The package name of the device launcher.
        """

    @property
    def idle_screen_resource_id(self) -> str:
        """Android idle screen workspace resource ID.

        Returns:
            The resource ID representing the idle workspace.
        """

    @property
    def adb_auth_activity(self) -> str:
        """Android ADB authorization activity component.

        Returns:
            The activity component string for the USB debugging prompt.
        """

    @property
    def sysui_package(self) -> str:
        """Android SystemUI package name.

        Returns:
            The package name of SystemUI.
        """

    @property
    def apps_list_activity(self) -> str | None:
        """Android all-apps activity component, if any.

        Returns:
            The activity component string for the all-apps list, or None.
        """

    @property
    def apps_list_resource_id(self) -> str:
        """Android all-apps list resource ID.

        Returns:
            The resource ID representing the all-apps list container.
        """

    @property
    def standardize_screenshots(self) -> bool:
        """Whether screenshots should be standardized.

        Returns:
            True if screenshots should be converted to RGBA, False otherwise.
        """

    @property
    def enable_black_screen_mitigation(self) -> bool:
        """Whether black screen mitigation is enabled during idle wait.

        Returns:
            True if black screen mitigation is enabled, False otherwise.
        """

    def dismiss_blocking_dialogs(self) -> Any:
        """Dismisses blocking dialogs.

        Returns:
            True if a blocking dialog was dismissed, False otherwise.
        """

    def _prepare_for_idle_screen(self) -> Any:
        """Runs pre-idle preparation steps.

        Returns:
            Optional coroutine or None.
        """

    def _handle_unexpected_screen(self, ui: Any) -> Any:
        """Handles unexpected intermediate screens.

        Args:
            ui: UiDevice snippet client instance.

        Returns:
            True if handled, False otherwise.
        """

    def _mitigate_stuck_ui(self, count: int) -> Any:
        """Mitigates stuck UI when consecutive black screens occur.

        Args:
            count: Number of consecutive black screens detected.

        Returns:
            Optional coroutine or None.
        """


_UIAUTOMATOR_PACKAGE_NAME: str = "com.google.android.mobly.snippet.uiautomator"
_CANDIDATE_UIAUTOMATOR_APK_PATHS: tuple[str, ...] = (
    "host_x64/test_data/honeydew/uiautomator.apk",
    "host_arm64/test_data/honeydew/uiautomator.apk",
    "test_data/honeydew/uiautomator.apk",
)
_RESUMED_ACTIVITY_KEYS: tuple[str, ...] = (
    "topResumedActivity",
    "ResumedActivity",
    "mResumedActivity",
    "mFocusedApp",
    "mCurrentFocus",
)
_UIAUTOMATOR_TASK_ERRORS: tuple[type[Exception], ...] = (
    errors.HoneydewError,
    mobly_adb.Error,
    OSError,
    RuntimeError,
    uiautomator_errors.BaseError,
)


class _HoneydewMoblyAdbProxy(mobly_adb.AdbProxy):
    """Instance-scoped Mobly AdbProxy using a resolved ADB binary path."""

    def __init__(
        self,
        serial: str = "",
        host: str = "",
        port: int | None = None,
        adb_binary: str = "adb",
    ) -> None:
        super().__init__(serial=serial, host=host, port=port)
        self._adb_binary: str = adb_binary

    def _construct_adb_cmd(
        self, raw_name: str, args: Any, shell: bool
    ) -> str | list[str]:
        args = args or ""
        name = raw_name.replace("_", "-")
        server_flags_str = ""
        if self.host and self.port:
            server_flags_str += f'-H "{self.host}" -P "{self.port}" '
        if self.serial:
            server_flags_str += f'-s "{self.serial}" '

        if shell:
            args = mobly_utils.cli_cmd_to_string(args)
            if server_flags_str:
                return f'"{self._adb_binary}" {server_flags_str}{name} {args}'.rstrip()
            return f'"{self._adb_binary}" {name} {args}'.rstrip()
        adb_cmd: list[str] = [self._adb_binary]
        if self.host and self.port:
            adb_cmd.extend(["-H", self.host, "-P", str(self.port)])
        if self.serial:
            adb_cmd.extend(["-s", self.serial])
        adb_cmd.append(name)
        if args:
            if isinstance(args, str):
                adb_cmd.append(args)
            else:
                adb_cmd.extend(args)
        return adb_cmd


def _resolve_adb_binary_path(adb_obj: Any) -> str | None:
    """Resolves the ADB binary path from a Honeydew Adb transport instance."""
    adb_binary = (
        getattr(adb_obj, "binary_path", None)
        or getattr(adb_obj, "_adb_binary_path", None)
        or getattr(adb_obj, "_adb_binary", None)
    )
    if isinstance(adb_binary, os.PathLike):
        adb_binary = os.fspath(adb_binary)
    if isinstance(adb_binary, str) and adb_binary:
        return (
            os.path.abspath(adb_binary)
            if os.path.sep in adb_binary
            else adb_binary
        )
    return None


def _configure_mobly_adb(adb_obj: Any, ui_dev: Any = None) -> None:
    """Configures Mobly's AdbProxy to use the ADB binary from adb_obj.

    Assigns an instance-scoped `_HoneydewMoblyAdbProxy` on `ui_dev` when
    provided, and ensures `mobly_adb.ADB` is populated for module-level helpers
    such as `mobly_adb.list_occupied_adb_ports()` in hermetic Infra sandboxes.
    """
    resolved_adb = _resolve_adb_binary_path(adb_obj)
    if not resolved_adb:
        return
    if ui_dev is not None and isinstance(
        getattr(ui_dev, "adb", None), mobly_adb.AdbProxy
    ):
        ui_dev.adb = _HoneydewMoblyAdbProxy(
            serial=ui_dev.adb.serial,
            host=ui_dev.adb.host,
            port=ui_dev.adb.port,
            adb_binary=resolved_adb,
        )
        if ui_dev.adb.port:
            os.environ.setdefault(
                "ANDROID_ADB_SERVER_PORT", str(ui_dev.adb.port)
            )
    if not mobly_adb.ADB or mobly_adb.ADB == "adb":
        mobly_adb.ADB = resolved_adb


def _resolve_uiautomator_apk(apk_path: str | Path | None = None) -> str:
    """Resolves the local path to uiautomator.apk from explicit path, runtime deps, or package resources.

    Args:
        apk_path: Optional explicit path to `uiautomator.apk`.

    Returns:
        Resolved absolute path to `uiautomator.apk`.

    Raises:
        errors.NotSupportedError: If `uiautomator.apk` cannot be found on the host.
    """
    if apk_path is not None:
        resolved = Path(apk_path)
        if resolved.is_file():
            return str(resolved.resolve())
        raise errors.NotSupportedError(
            f"Configured uiautomator.apk does not exist: {apk_path}"
        )

    env_apk = os.environ.get("HONEYDEW_UIAUTOMATOR_APK")
    if env_apk and Path(env_apk).is_file():
        return str(Path(env_apk).resolve())

    runtime_roots = [Path(sys.argv[0]).resolve().parent, Path(".")]
    searched: list[str] = []
    for root in runtime_roots:
        for candidate in _CANDIDATE_UIAUTOMATOR_APK_PATHS:
            candidate_path = root / candidate
            searched.append(str(candidate_path))
            if candidate_path.is_file():
                return str(candidate_path.resolve())
    apk_getter = getattr(uiautomator_utils, "get_uiautomator_apk", None)
    if callable(apk_getter):
        try:
            getter_apk_path = apk_getter()
            if getter_apk_path and Path(getter_apk_path).is_file():
                return str(Path(getter_apk_path).resolve())
        except (OSError, RuntimeError, ValueError) as e:
            _LOGGER.debug(f"get_uiautomator_apk() failed: {e}")
    raise errors.NotSupportedError(
        f"Could not find uiautomator.apk; searched: {searched}"
    )


def create_uiautomator_device(
    adb_obj: Any, *, apk_path: str | Path | None = None
) -> Any:
    """Creates and configures a snippet host with UiAutomatorService.

    APK presence checks and installation are performed via Honeydew's `adb_obj`
    transport (`skip_installing=True`), while the returned snippet host manages
    the `com.google.android.mobly.snippet.uiautomator` RPC service lifecycle.

    Args:
        adb_obj: Honeydew Adb transport instance.
        apk_path: Optional explicit path to `uiautomator.apk`.

    Returns:
        Configured Mobly AndroidDevice snippet host.

    Raises:
        errors.NotSupportedError: If `uiautomator.apk` is required and missing.
        UiNavigationError: If `adb_obj` has no serial number.
    """
    _configure_mobly_adb(adb_obj)
    package_verified = getattr(adb_obj, "_uiautomator_package_verified", False)
    if not package_verified:
        packages_out = adb_obj.run(
            ["shell", "pm", "list", "packages", _UIAUTOMATOR_PACKAGE_NAME]
        )
        if _UIAUTOMATOR_PACKAGE_NAME not in packages_out:
            resolved_apk_path = _resolve_uiautomator_apk(apk_path)
            _LOGGER.info(
                f"Installing {_UIAUTOMATOR_PACKAGE_NAME} from {resolved_apk_path}..."
            )
            adb_obj.run(["install", "-d", "-g", "-r", resolved_apk_path])
        try:
            adb_obj._uiautomator_package_verified = True
        except (AttributeError, TypeError):
            pass

    serial = (
        getattr(adb_obj, "serial_number", None)
        or getattr(adb_obj, "target_id", None)
        or getattr(adb_obj, "_serial_number", None)
    )
    if not serial:
        raise UiNavigationError(
            "ADB serial_number is required to create UiAutomatorDevice"
        )
    server_host_port = getattr(adb_obj, "server_host_port", None)
    if isinstance(server_host_port, tuple) and len(server_host_port) == 2:
        ui_dev = mobly_android_device.AndroidDevice(
            serial=serial,
            host=server_host_port[0],
            port=server_host_port[1],
        )
    else:
        ui_dev = mobly_android_device.AndroidDevice(serial=serial)

    _configure_mobly_adb(adb_obj, ui_dev)
    ui_dev.services.register(
        uiautomator.ANDROID_SERVICE_NAME,
        uiautomator.UiAutomatorService,
        uiautomator.UiAutomatorConfigs(skip_installing=True),
    )
    return ui_dev


def _reset_uiautomator_device(device: Any) -> None:
    """Stops any active UiAutomator services on device and clears the cached instance."""
    close_fn = getattr(device, "_close_uiautomator", None)
    if callable(close_fn):
        close_fn()
        return
    ui_dev = getattr(device, "_uiautomator_device", None)
    if ui_dev is not None:
        try:
            ui_dev.services.stop_all()
        except (mobly_adb.Error, OSError, RuntimeError) as e:
            _LOGGER.debug(f"Failed to stop UiAutomator services: {e}")
        device._uiautomator_device = None


class _AbortingNotSupportedError(errors.NotSupportedError, RetryAbortingError):
    """Internal subclass so NotSupportedError aborts retry_for_duration immediately."""


class _UnexpectedTaskError(RetryAbortingError):
    """Internal wrapper so unexpected non-retriable exceptions abort retry_for_duration immediately."""

    def __init__(self, original: Exception) -> None:
        super().__init__(str(original))
        self.original: Exception = original


async def run_uiautomator_task(
    device: Any,
    task: Callable[[UiAutomatorDevice], Any],
    timeout: timedelta = timedelta(minutes=2),
    wait_between_tries: timedelta = timedelta(seconds=2),
    *,
    apk_path: str | Path | None = None,
) -> Any:
    """Runs a UiAutomator task against device with retry and connection recovery.

    Args:
        device: Target device or UiNavigation instance exposing `adb` and
            `_uiautomator_device`.
        task: Synchronous or asynchronous callable accepting a
            `UiAutomatorDevice` and returning a result.
        timeout: Maximum total duration to retry retriable task or transport
            errors before failing.
        wait_between_tries: Delay between retry attempts.
        apk_path: Optional explicit path to `uiautomator.apk`.

    Returns:
        The value returned by `task`.

    Raises:
        errors.NotSupportedError: If `uiautomator.apk` is missing or ADB is
            unsupported.
        UiAutomatorRetriableTaskError: If the task continues to raise a
            retriable UI state error after `timeout` expires.
        UiNavigationError: If transport or snippet errors fail after timeout.
    """

    async def _attempt() -> Any:
        try:
            if getattr(device, "_uiautomator_device", None) is None:
                device._uiautomator_device = create_uiautomator_device(
                    device.adb, apk_path=apk_path
                )
            result = task(device._uiautomator_device)
            if inspect.isawaitable(result):
                return await result
            return result
        except errors.NotSupportedError as e:
            raise _AbortingNotSupportedError(str(e)) from e
        except (
            UiAutomatorRetriableTaskError,
            RetryAbortingError,
        ):
            raise
        except mobly_adb.Error:
            if hasattr(device.adb, "check_connection"):
                await asyncio.to_thread(device.adb.check_connection)
            _reset_uiautomator_device(device)
            raise
        except _UIAUTOMATOR_TASK_ERRORS:
            _reset_uiautomator_device(device)
            raise
        except Exception as e:
            raise _UnexpectedTaskError(e) from e

    try:
        return await retry_for_duration(
            task=_attempt,
            duration=timeout,
            retry_delay=wait_between_tries,
        )
    except _UnexpectedTaskError as e:
        raise e.original
    except (mobly_adb.Error, uiautomator_errors.BaseError) as err:
        raise UiNavigationError(
            f"UiAutomator task failed after {timeout}: {err}"
        ) from err


def is_black_screenshot(screenshot: ScreenshotImage) -> bool:
    """Detects whether screenshot data is all black or repeating blank bytes.

    Args:
        screenshot: Captured `ScreenshotImage` buffer to inspect.

    Returns:
        True if the image buffer is empty or consists entirely of black pixels,
        False otherwise.
    """
    data = screenshot.data
    data_len = len(data)
    if data_len == 0:
        return True
    if not any(data):
        return True
    first_pixel = data[:4]
    if first_pixel not in (b"\x00\x00\x00\xff", b"\xff\x00\x00\x00"):
        return False
    return all(data[i : i + 4] == first_pixel for i in range(4, data_len, 4))


def is_idle_screen(
    ui: Any, launcher_package: str, idle_screen_resource_id: str
) -> bool:
    """Checks if current UI state matches the configured idle screen.

    Args:
        ui: `UiDevice` snippet object used to inspect the active UI hierarchy.
        launcher_package: Expected Android package name of the home launcher.
        idle_screen_resource_id: Resource ID of the home launcher workspace.

    Returns:
        True if the launcher package is focused and the workspace resource ID
        exists, False otherwise.
    """
    if not idle_screen_resource_id or not launcher_package:
        return False
    is_launcher_focused = ui.package_name == launcher_package
    return bool(ui(res=idle_screen_resource_id).exists and is_launcher_focused)


def dismiss_adb_auth_dialog(
    adb: Any, adb_auth_activity: str, sysui_package: str
) -> bool:
    """Dismisses any active ADB authorization prompt.

    Args:
        adb: Honeydew `Adb` transport instance.
        adb_auth_activity: Activity component string for the USB debugging
            authorization dialog.
        sysui_package: SystemUI package name to stop when the dialog is active.

    Returns:
        True if an active ADB authorization dialog was detected and dismissed,
        False otherwise.

    Raises:
        errors.HoneydewError: If querying or dismissing dialogs via ADB fails.
    """
    if not adb_auth_activity:
        return False

    activities_log = adb.run(["shell", "dumpsys", "activity", "activities"])
    is_resumed = any(
        adb_auth_activity in line
        and any(key in line for key in _RESUMED_ACTIVITY_KEYS)
        for line in activities_log.splitlines()
    )
    if is_resumed:
        _LOGGER.info(f"Dismissing ADB auth prompt: {adb_auth_activity}")
        if sysui_package:
            adb.run(["shell", "am", "stop-app", sysui_package])
        return True
    return False


def return_to_home(adb: Any) -> None:
    """Fires the HOME category intent to return the device to the idle screen.

    Args:
        adb: Honeydew `Adb` transport instance.

    Raises:
        UiNavigationError: If launching the HOME intent via ADB fails.
    """
    _LOGGER.info("Returning device to idle screen...")
    try:
        adb.run(
            [
                "shell",
                "am",
                "start",
                "-a",
                "android.intent.action.MAIN",
                "-c",
                "android.intent.category.HOME",
            ]
        )
    except errors.HoneydewError as e:
        raise UiNavigationError(f"Failed to return to home screen: {e}") from e


def return_to_idle_screen(adb: Any) -> None:
    """Returns the device to the idle / home screen.

    This delegates to `return_to_home()` and is provided for callers
    expecting idle screen navigation semantics.

    Args:
        adb: Honeydew `Adb` transport instance.

    Raises:
        UiNavigationError: If launching the HOME intent via ADB fails.
    """
    return_to_home(adb)


class UiNavigation(affordance.Affordance):
    """Affordance for Android UI navigation and UiAutomator snippet execution."""

    def __init__(
        self,
        adb: adb_transport.Adb,
        device: UiNavigationHost | None = None,
        *,
        screenshot: Any | None = None,
        apk_path: str | Path | None = None,
    ) -> None:
        """Initializes the UiNavigation affordance.

        Args:
            adb: Honeydew `Adb` transport instance.
            device: Optional host `AndroidDevice` implementing `UiNavigationHost`
                for UI configuration attributes and screen recovery hooks.
            screenshot: Optional explicit `Screenshot` affordance instance.
            apk_path: Optional explicit host path to `uiautomator.apk`.

        Raises:
            errors.NotSupportedError: If ADB is not supported or an explicit
                `apk_path` does not exist on the host.
        """
        self._adb: adb_transport.Adb = adb
        self._device: UiNavigationHost | None = device
        self._screenshot: Any | None = (
            screenshot
            if screenshot is not None
            else getattr(device, "screenshot", None)
        )
        self._apk_path: str | Path | None = apk_path
        self._uiautomator_device: Any = None
        self._consecutive_black_screen_count: int = 0
        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that UiNavigation affordance is supported by the device.

        Note: When `apk_path` is explicitly provided, its existence on the host
        is validated eagerly here. When `apk_path` is None, host APK resolution
        is deferred until `create_uiautomator_device()` queries `pm list packages`
        on the target device because the target image may already have
        `com.google.android.mobly.snippet.uiautomator` pre-installed.

        Raises:
            errors.NotSupportedError: If ADB is not supported or an explicit
                `apk_path` does not exist on the host.
        """
        self._adb.verify_supported()
        if self._apk_path is not None:
            _resolve_uiautomator_apk(self._apk_path)

    def close(self) -> None:
        """Stops any active UiAutomator service and clears the cached device."""
        if self._uiautomator_device is not None:
            try:
                self._uiautomator_device.services.stop_all()
            except (mobly_adb.Error, OSError, RuntimeError) as e:
                _LOGGER.debug(f"Failed to stop UiAutomator services: {e}")
            self._uiautomator_device = None

    def _close_uiautomator(self) -> None:
        """Alias for close() used by _reset_uiautomator_device."""
        self.close()

    def create_uiautomator_device(self) -> Any:
        """Creates and configures a Mobly AndroidDevice with UiAutomatorService.

        Returns:
            Configured `UiAutomatorDevice` instance.

        Raises:
            errors.NotSupportedError: If `uiautomator.apk` is required and missing.
            UiNavigationError: If `self._adb` has no serial number.
        """
        return create_uiautomator_device(self._adb, apk_path=self._apk_path)

    async def run_uiautomator_task(
        self,
        task: Callable[[UiAutomatorDevice], Any],
        timeout: timedelta = timedelta(minutes=2),
        wait_between_tries: timedelta = timedelta(seconds=2),
    ) -> Any:
        """Runs a UiAutomator task against the device with retry and recovery.

        Args:
            task: Synchronous or asynchronous callable accepting a
                `UiAutomatorDevice` and returning a result.
            timeout: Maximum total duration to retry retriable task or transport
                errors before failing.
            wait_between_tries: Delay between retry attempts.

        Returns:
            The value returned by `task`.

        Raises:
            errors.NotSupportedError: If `uiautomator.apk` is missing or ADB is
                unsupported.
            UiAutomatorRetriableTaskError: If the task continues to raise a
                retriable UI state error after `timeout` expires.
        """
        return await run_uiautomator_task(
            self,
            task,
            timeout=timeout,
            wait_between_tries=wait_between_tries,
            apk_path=self._apk_path,
        )

    @property
    def adb(self) -> adb_transport.Adb:
        """Returns the underlying ADB transport.

        Returns:
            The `adb_transport.Adb` instance used by this affordance.
        """
        return self._adb

    def is_black_screenshot(self, screenshot: ScreenshotImage) -> bool:
        """Detects whether screenshot data is all black or repeating blank bytes.

        Args:
            screenshot: Captured `ScreenshotImage` buffer to inspect.

        Returns:
            True if the image buffer is empty or consists entirely of black
            pixels, False otherwise.
        """
        return is_black_screenshot(screenshot)

    def is_idle_screen(
        self,
        ui: Any,
        launcher_package: str | None = None,
        idle_screen_resource_id: str | None = None,
    ) -> bool:
        """Checks if current UI state matches the configured idle screen.

        Args:
            ui: `UiDevice` snippet object used to inspect the active UI
                hierarchy.
            launcher_package: Optional launcher package override; defaults to
                the host device's `launcher_package`.
            idle_screen_resource_id: Optional workspace resource ID override;
                defaults to the host device's `idle_screen_resource_id`.

        Returns:
            True if the launcher package is focused and the workspace resource
            ID exists, False otherwise.
        """
        if launcher_package is None and self._device is not None:
            launcher_package = getattr(self._device, "launcher_package", "")
        if idle_screen_resource_id is None and self._device is not None:
            idle_screen_resource_id = getattr(
                self._device, "idle_screen_resource_id", ""
            )
        return is_idle_screen(
            ui, launcher_package or "", idle_screen_resource_id or ""
        )

    def dismiss_adb_auth_dialog(
        self,
        adb_auth_activity: str | None = None,
        sysui_package: str | None = None,
    ) -> bool:
        """Dismisses any active ADB authorization prompt.

        Args:
            adb_auth_activity: Optional activity component override; defaults to
                the host device's `adb_auth_activity`.
            sysui_package: Optional SystemUI package override; defaults to the
                host device's `sysui_package`.

        Returns:
            True if an active ADB authorization dialog was detected and
            dismissed, False otherwise.
        """
        if adb_auth_activity is None and self._device is not None:
            adb_auth_activity = getattr(self._device, "adb_auth_activity", "")
        if sysui_package is None and self._device is not None:
            sysui_package = getattr(self._device, "sysui_package", "")
        return dismiss_adb_auth_dialog(
            self._adb, adb_auth_activity or "", sysui_package or ""
        )

    def dismiss_blocking_dialogs(self) -> bool:
        """Dismisses any active blocking dialogs such as ADB auth prompt.

        Returns:
            True if an active blocking dialog was detected and dismissed,
            False otherwise.
        """
        return self.dismiss_adb_auth_dialog()

    async def check_and_mitigate_black_screen(self) -> None:
        """Checks if the screen is black and runs stuck-UI mitigation if repeated.

        Raises:
            UiNavigationError: If no host device instance is configured.
            BlackScreenUiAutomatorError: If 10 consecutive black screenshots are
                detected and UI mitigation fails to recover the display.
            UiAutomatorRetriableTaskError: If a black screen is detected (<10
                consecutive times) to trigger a retry in `run_uiautomator_task`.
        """
        device = self._device
        screenshot_obj = self._screenshot or (
            device.screenshot if device else None
        )
        if screenshot_obj is None:
            raise UiNavigationError(
                "Screenshot affordance or device instance is required for check_and_mitigate_black_screen"
            )
        screenshot = await asyncio.to_thread(
            screenshot_obj.take,
            standardize=getattr(device, "standardize_screenshots", False),
        )
        if self.is_black_screenshot(screenshot):
            self._consecutive_black_screen_count += 1
            if self._consecutive_black_screen_count == 5:
                if device is not None:
                    mitigate_res = device._mitigate_stuck_ui(
                        self._consecutive_black_screen_count
                    )
                    if inspect.isawaitable(mitigate_res):
                        await mitigate_res
                raise UiAutomatorRetriableTaskError(
                    "Attempted mitigation to recover from black screen state."
                )
            elif self._consecutive_black_screen_count >= 10:
                raise BlackScreenUiAutomatorError(
                    "UI mitigation failed for black screen."
                )
            raise UiAutomatorRetriableTaskError(
                f"Black screen detected ({self._consecutive_black_screen_count}/10), waiting for display to draw..."
            )
        self._consecutive_black_screen_count = 0

    async def wait_for_idle_screen(self) -> None:
        """Navigates to and waits for the idle screen to settle.

        Raises:
            UiNavigationError: If no host device instance is configured.
            BlackScreenUiAutomatorError: If repeated black screens persist after
                mitigation.
            UnknownStateUiAutomatorError: If the device fails to reach the idle
                screen within the UiAutomator task timeout.
        """
        if self._device is None:
            raise UiNavigationError(
                "Device instance is required for wait_for_idle_screen"
            )
        device = self._device
        prepare_res = device._prepare_for_idle_screen()
        if inspect.isawaitable(prepare_res):
            await prepare_res

        self._consecutive_black_screen_count = 0

        async def _navigate_to_idle(uiautomator_device: Any) -> None:
            try:
                uiautomator_device.ui.press(uiautomator.KeyEvent.KEYCODE_WAKEUP)
            except (mobly_adb.Error, OSError):
                raise
            except (errors.HoneydewError, RuntimeError) as e:
                _LOGGER.debug(f"Failed to send KEYCODE_WAKEUP: {e}")

            if getattr(device, "enable_black_screen_mitigation", False):
                await self.check_and_mitigate_black_screen()

            if self.is_idle_screen(uiautomator_device.ui):
                _LOGGER.info("Reached idle screen, waiting for window idle...")
                # Verify that the idle screen workspace remains continuously
                # present in the foreground for WAIT_ON_SCREEN_SECONDS without
                # being preempted by a late-arriving dialog (e.g. USB auth or
                # Setup Wizard). If a dialog preempts the workspace during this
                # window, wait.gone() returns True immediately and we retry.
                if not uiautomator_device.ui(
                    res=device.idle_screen_resource_id
                ).wait.gone(timedelta(seconds=WAIT_ON_SCREEN_SECONDS)):
                    return
                raise UiAutomatorRetriableTaskError(
                    "Reached idle screen, but didn't settle."
                )

            unexpected_res = device._handle_unexpected_screen(
                uiautomator_device.ui
            )
            if inspect.isawaitable(unexpected_res):
                unexpected_res = await unexpected_res
            if unexpected_res:
                raise UiAutomatorRetriableTaskError(
                    "Handled unexpected screen, retrying..."
                )

            dismiss_res = device.dismiss_blocking_dialogs()
            if inspect.isawaitable(dismiss_res):
                dismiss_res = await dismiss_res
            if dismiss_res:
                raise UiAutomatorRetriableTaskError(
                    "Dismissed blocking dialog, retrying..."
                )

            try:
                uiautomator_device.ui.press(uiautomator.KeyEvent.KEYCODE_HOME)
            except (mobly_adb.Error, OSError):
                raise
            except _UIAUTOMATOR_TASK_ERRORS as e:
                _LOGGER.debug(f"Failed to send KEYCODE_HOME: {e}")
                _reset_uiautomator_device(self)
                raise UnknownStateUiAutomatorError(
                    "In unknown state, sending KEYCODE_HOME."
                ) from e

            raise UnknownStateUiAutomatorError(
                "In unknown state, sending KEYCODE_HOME."
            )

        _LOGGER.info("Waiting for idle screen...")
        await self.run_uiautomator_task(_navigate_to_idle)
        _LOGGER.info("Reached idle screen.")

    async def verify_is_idle_screen(
        self, failure_message: str = "Device is not on idle screen"
    ) -> None:
        """Verifies that the current screen is the idle screen.

        Args:
            failure_message: Error message if the device is not on the idle
                screen.

        Raises:
            UiNavigationError: If the current screen is not the idle screen.
        """
        if self._device is None:
            raise UiNavigationError(
                "Device instance is required for verify_is_idle_screen"
            )

        def _check(uiautomator_device: Any) -> bool:
            return bool(self.is_idle_screen(uiautomator_device.ui))

        if not await self.run_uiautomator_task(_check):
            raise UiNavigationError(failure_message)

    async def wait_for_launcher_screen(self) -> None:
        """Navigates to launcher / all apps list.

        Raises:
            UiNavigationError: If no host device instance is configured.
            errors.NotSupportedError: If launcher navigation is not supported
                on this device profile (missing apps_list_activity or
                apps_list_resource_id).
            UiAutomatorRetriableTaskError: If the launcher apps list does not
                appear within the UiAutomator task timeout.
        """
        if self._device is None:
            raise UiNavigationError(
                "Device instance is required for wait_for_launcher_screen"
            )
        device = self._device
        await self.wait_for_idle_screen()
        apps_list_activity = getattr(device, "apps_list_activity", None)
        apps_list_resource_id = getattr(device, "apps_list_resource_id", "")
        if not apps_list_activity or not apps_list_resource_id:
            raise errors.NotSupportedError(
                "Launcher navigation is not supported on this device profile: "
                "apps_list_activity or apps_list_resource_id is not defined."
            )

        def _navigate_to_launcher(uiautomator_device: Any) -> None:
            if uiautomator_device.ui(res=apps_list_resource_id).exists:
                return
            self._adb.run(["shell", "am", "start", apps_list_activity])
            if not uiautomator_device.ui(res=apps_list_resource_id).wait.exists(
                timedelta(seconds=5)
            ):
                raise UiAutomatorRetriableTaskError(
                    f"Waiting for apps list activity: {apps_list_activity}"
                )

        _LOGGER.info("Waiting for launcher screen...")
        await self.run_uiautomator_task(_navigate_to_launcher)
        _LOGGER.info("Reached launcher screen.")

    def return_to_home(self) -> None:
        """Fires the HOME category intent to return the device to the idle screen.

        Raises:
            UiNavigationError: If launching the HOME intent via ADB fails.
        """
        return_to_home(self._adb)

    def return_to_idle_screen(self) -> None:
        """Returns the device to the idle / home screen.

        This delegates to `return_to_home()` and is provided for callers
        expecting idle screen navigation semantics.

        Raises:
            UiNavigationError: If launching the HOME intent via ADB fails.
        """
        self.return_to_home()


__all__ = [
    "BlackScreenUiAutomatorError",
    "UiAutomatorDevice",
    "UiAutomatorRetriableTaskError",
    "UiNavigation",
    "UiNavigationError",
    "UiNavigationHost",
    "UnknownStateUiAutomatorError",
    "WAIT_ON_SCREEN_SECONDS",
    "create_uiautomator_device",
    "dismiss_adb_auth_dialog",
    "is_black_screenshot",
    "is_idle_screen",
    "return_to_home",
    "return_to_idle_screen",
    "run_uiautomator_task",
]
