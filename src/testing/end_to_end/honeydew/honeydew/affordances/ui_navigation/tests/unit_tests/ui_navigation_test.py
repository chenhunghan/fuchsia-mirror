# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.ui_navigation."""

import inspect
import pathlib
import tempfile
import unittest
from datetime import timedelta
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.affordances.ui.screenshot import (
    screenshot as screenshot_affordance,
)
from honeydew.affordances.ui.screenshot.types import ScreenshotImage
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.affordances.ui_navigation import (
    BlackScreenUiAutomatorError,
    UiAutomatorRetriableTaskError,
    UiNavigation,
    UnknownStateUiAutomatorError,
    ui_navigation,
)
from honeydew.device_classes.android_device import AndroidDevice
from honeydew.fuchsia_device.fuchsia_device import FuchsiaDevice
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import adb_server as adb_server_mod
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.typing import custom_types
from honeydew.utils import control_flows
from mobly import signals
from mobly.controllers import android_device as mobly_android_device
from mobly.controllers.android_device_lib import adb as mobly_adb
from mobly.controllers.android_device_lib import service_manager
from snippet_uiautomator import errors as uiautomator_errors
from snippet_uiautomator import (
    uiautomator,
    uidevice,
    uiobject2,
)

# pylint: disable=protected-access


def _create_mock_uiautomator_device() -> Any:
    """Creates an autospecced Mobly AndroidDevice with UiDevice and UiObject2 mocks."""
    mock_ui_dev: Any = mock.create_autospec(
        mobly_android_device.AndroidDevice, instance=True
    )
    mock_ui_dev.services = mock.create_autospec(
        service_manager.ServiceManager, instance=True
    )
    mock_ui: Any = mock.create_autospec(uidevice.UiDevice, instance=True)
    mock_ui._spec_signature = inspect.signature(
        uidevice.UiDevice.__call__.__get__(object(), uidevice.UiDevice)
    )
    mock_ui.press = mock.create_autospec(uidevice._Press, instance=True)
    mock_ui.press._spec_signature = inspect.signature(
        uidevice._Press.__call__.__get__(object(), uidevice._Press)
    )
    mock_ui_obj: Any = mock.create_autospec(uiobject2.UiObject2, instance=True)
    mock_ui_obj.wait = mock.create_autospec(uiobject2._Wait, instance=True)
    mock_ui.return_value = mock_ui_obj
    mock_ui_dev.ui = mock_ui
    return mock_ui_dev


class _TestNavDevice(AndroidDevice):
    """Concrete test subclass with navigation profile attributes."""

    launcher_package = "com.android.launcher3"
    setup_wizard_package = "com.google.android.setupwizard"
    sysui_package = "com.android.systemui"
    adb_auth_activity = "com.android.systemui/.usb.UsbDebuggingActivity"
    idle_screen_resource_id = "com.android.launcher3:id/workspace"
    apps_list_resource_id = "com.android.launcher3:id/apps_list_view"
    apps_list_activity: str | None = "com.android.launcher3/.AllAppsActivity"
    _default_screen_size = ui_types.Size(123, 456)
    _default_after_boot = timedelta(seconds=30)


class UiNavigationTest(unittest.IsolatedAsyncioTestCase):
    """Verifies UI navigation, UiAutomator snippet execution, and black screen recovery."""

    def setUp(self) -> None:
        super().setUp()
        self.dev_info = custom_types.DeviceInfo(
            name="fuchsia-emulator",
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        self.ffx_cfg = mock.create_autospec(
            ffx_config.FfxConfigData, instance=True
        )
        self.mock_ffx = mock.create_autospec(ffx_transport.FFX, instance=True)
        self.mock_screenshot = mock.create_autospec(
            screenshot_affordance.Screenshot, instance=True
        )
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )
        self.mock_adb.run.return_value = ""
        self.mock_adb.serial_number = "127.0.0.1:5555"
        self.mock_adb._serial_number = "127.0.0.1:5555"
        self.mock_adb.binary_path = "adb"
        self.mock_adb._adb_binary_path = "adb"
        self.mock_adb.server_host_port = None
        self.mock_adb.adb_server = mock.create_autospec(
            adb_server_mod.AdbServer, instance=True
        )
        self.mock_adb.adb_server.host.return_value = "127.0.0.1"
        self.mock_adb.adb_server.port.return_value = 5037
        self.mock_adb._adb_server = self.mock_adb.adb_server

        for patcher in (
            mock.patch.object(FuchsiaDevice, "health_check", autospec=True),
            mock.patch.object(
                FuchsiaDevice,
                "ffx",
                new_callable=mock.PropertyMock,
                return_value=self.mock_ffx,
            ),
            mock.patch.object(
                FuchsiaDevice,
                "screenshot",
                new_callable=mock.PropertyMock,
                return_value=self.mock_screenshot,
            ),
            mock.patch.object(
                AndroidDevice,
                "adb",
                new_callable=mock.PropertyMock,
                return_value=self.mock_adb,
            ),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

        self.device = _TestNavDevice(
            device_info=self.dev_info,
            ffx_config_data=self.ffx_cfg,
        )
        self.mock_adb.run.reset_mock()

    def test_is_black_screenshot(self) -> None:
        """Verifies is_black_screenshot detects empty, all-zero, and blank RGBA buffers."""
        empty_img = ScreenshotImage(size=ui_types.Size(0, 0), data=b"")
        self.assertTrue(
            self.device.ui_navigation.is_black_screenshot(empty_img)
        )

        zeros_img = ScreenshotImage(
            size=ui_types.Size(10, 10), data=b"\x00" * 400
        )
        self.assertTrue(
            self.device.ui_navigation.is_black_screenshot(zeros_img)
        )

        rgba_black_img = ScreenshotImage(
            size=ui_types.Size(10, 10), data=b"\x00\x00\x00\xff" * 100
        )
        self.assertTrue(
            self.device.ui_navigation.is_black_screenshot(rgba_black_img)
        )

        argb_black_img = ScreenshotImage(
            size=ui_types.Size(10, 10), data=b"\xff\x00\x00\x00" * 100
        )
        self.assertTrue(
            self.device.ui_navigation.is_black_screenshot(argb_black_img)
        )

        color_img = ScreenshotImage(
            size=ui_types.Size(10, 10), data=b"\x12\x34\x56\xff" * 100
        )
        self.assertFalse(
            self.device.ui_navigation.is_black_screenshot(color_img)
        )

        partial_img = ScreenshotImage(
            size=ui_types.Size(2, 1),
            data=b"\x00\x00\x00\xff\x12\x34\x56\xff",
        )
        self.assertFalse(
            self.device.ui_navigation.is_black_screenshot(partial_img)
        )

    def test_is_idle_screen(self) -> None:
        """Verifies is_idle_screen checks launcher package focus and workspace resource."""
        mock_ui_dev = _create_mock_uiautomator_device()
        mock_ui = mock_ui_dev.ui
        mock_ui.package_name = "com.android.launcher3"
        mock_ui.return_value.exists = True

        self.assertTrue(self.device.ui_navigation.is_idle_screen(mock_ui))
        mock_ui.assert_called_with(res="com.android.launcher3:id/workspace")

        mock_ui.package_name = "com.other.app"
        self.assertFalse(self.device.ui_navigation.is_idle_screen(mock_ui))

    def test_mitigate_stuck_ui(self) -> None:
        """Verifies _mitigate_stuck_ui force-stops setup_wizard_package via self.adb.run."""
        self.device._mitigate_stuck_ui(5)
        self.mock_adb.run.assert_called_once_with(
            ["shell", "am", "force-stop", "com.google.android.setupwizard"]
        )

    def test_dismiss_blocking_dialogs(self) -> None:
        """Verifies dismiss_blocking_dialogs stops sysui only when adb_auth_activity is resumed."""
        self.mock_adb.run.return_value = "mResumedActivity: ActivityRecord{com.android.systemui/.usb.UsbDebuggingActivity}"
        self.assertTrue(self.device.dismiss_blocking_dialogs())
        self.assertTrue(self.device.ui_navigation.dismiss_blocking_dialogs())
        self.mock_adb.run.assert_any_call(
            ["shell", "am", "stop-app", "com.android.systemui"]
        )

        self.mock_adb.run.reset_mock()
        self.mock_adb.run.return_value = (
            "Hist #1: ActivityRecord{com.android.systemui/.usb.UsbDebuggingActivity}\n"
            "mResumedActivity: ActivityRecord{com.android.launcher3/.Launcher}"
        )
        self.assertFalse(self.device.dismiss_blocking_dialogs())
        self.assertFalse(self.device.ui_navigation.dismiss_blocking_dialogs())

    def test_on_boot_complete_dismisses_blocking_dialogs(self) -> None:
        """Verifies _on_boot_complete calls dismiss_blocking_dialogs."""
        with (
            mock.patch.object(
                self.device,
                "android_session",
                autospec=True,
            ),
            mock.patch.object(
                self.device,
                "dismiss_blocking_dialogs",
                autospec=True,
            ) as mock_dismiss,
        ):
            self.device._on_boot_complete()
            mock_dismiss.assert_called_once_with()

    def test_resolve_uiautomator_apk_raises_when_missing(self) -> None:
        """Verifies _resolve_uiautomator_apk raises NotSupportedError when APK is missing."""
        with (
            tempfile.TemporaryDirectory() as tmp_dir,
            mock.patch.object(
                ui_navigation,
                "_CANDIDATE_UIAUTOMATOR_APK_PATHS",
                (str(pathlib.Path(tmp_dir) / "nonexistent.apk"),),
            ),
            mock.patch(
                "honeydew.affordances.ui_navigation.ui_navigation.uiautomator_utils.get_uiautomator_apk",
                return_value=None,
                create=True,
            ),
        ):
            with self.assertRaises(errors.NotSupportedError):
                ui_navigation._resolve_uiautomator_apk()

    def test_verify_supported_validates_explicit_apk_path(self) -> None:
        """Verifies UiNavigation.verify_supported validates an explicit apk_path."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            missing_apk = pathlib.Path(tmp_dir) / "missing.apk"
            with self.assertRaises(errors.NotSupportedError):
                UiNavigation(self.mock_adb, self.device, apk_path=missing_apk)

            existing_apk = pathlib.Path(tmp_dir) / "uiautomator.apk"
            existing_apk.write_bytes(b"apk")
            nav = UiNavigation(
                self.mock_adb, self.device, apk_path=existing_apk
            )
            self.assertIsInstance(nav, UiNavigation)

    async def test_run_uiautomator_task_fails_fast_on_not_supported_error(
        self,
    ) -> None:
        """Verifies run_uiautomator_task fails immediately without retrying on NotSupportedError."""
        self.mock_adb.run.return_value = ""
        with (
            mock.patch.object(
                ui_navigation,
                "_resolve_uiautomator_apk",
                side_effect=errors.NotSupportedError("missing apk"),
            ) as mock_resolve,
            mock.patch.object(
                control_flows,
                "sleep_for_duration",
                new_callable=mock.AsyncMock,
            ) as mock_sleep,
        ):
            with self.assertRaises(errors.NotSupportedError):
                await self.device.ui_navigation.run_uiautomator_task(
                    lambda _ui_dev: None,
                    timeout=timedelta(seconds=30),
                    wait_between_tries=timedelta(seconds=2),
                )
            mock_resolve.assert_called_once()
            mock_sleep.assert_not_awaited()

    def test_create_uiautomator_device_missing_serial_raises(self) -> None:
        """Verifies create_uiautomator_device raises UiNavigationError when serial_number is missing."""
        self.mock_adb.run.return_value = ui_navigation._UIAUTOMATOR_PACKAGE_NAME
        self.mock_adb.serial_number = None
        self.mock_adb.target_id = None
        self.mock_adb._serial_number = None
        with self.assertRaises(ui_navigation.UiNavigationError):
            ui_navigation.create_uiautomator_device(self.mock_adb)

    async def test_run_uiautomator_task_propagates_unexpected_exceptions(
        self,
    ) -> None:
        """Verifies non-transport exceptions (e.g. ValueError or Mobly signals) propagate without retrying or resetting device."""
        mock_ui_dev = _create_mock_uiautomator_device()
        self.device.ui_navigation._uiautomator_device = mock_ui_dev

        for exc in (
            ValueError("unexpected programmer error"),
            signals.TestSkip("skip test"),
            signals.TestAbortAll("abort all tests"),
        ):
            mock_ui_dev.services.stop_all.reset_mock()

            def bad_task(_ui_dev: Any, exc_to_raise: Exception = exc) -> None:
                raise exc_to_raise

            with (
                mock.patch.object(
                    control_flows,
                    "sleep_for_duration",
                    new_callable=mock.AsyncMock,
                ) as mock_sleep,
                self.assertRaises(type(exc)),
            ):
                await self.device.ui_navigation.run_uiautomator_task(
                    bad_task,
                    timeout=timedelta(seconds=5),
                    wait_between_tries=timedelta(milliseconds=1),
                )
            mock_ui_dev.services.stop_all.assert_not_called()
            mock_sleep.assert_not_awaited()

    async def test_run_uiautomator_task_retries_on_uiautomator_base_error(
        self,
    ) -> None:
        """Verifies snippet-uiautomator BaseError subclasses are retried and reset the cached device."""
        mock_ui_dev = _create_mock_uiautomator_device()
        self.device.ui_navigation._uiautomator_device = mock_ui_dev
        calls = 0

        def flaky_task(_ui_dev: Any) -> str:
            nonlocal calls
            calls += 1
            if calls == 1:
                raise uiautomator_errors.UiAutomatorError("ui hierarchy stale")
            return "recovered"

        with (
            mock.patch.object(
                ui_navigation,
                "create_uiautomator_device",
                return_value=mock_ui_dev,
            ),
            mock.patch.object(
                control_flows,
                "sleep_for_duration",
                new_callable=mock.AsyncMock,
            ) as mock_sleep,
        ):
            res = await self.device.ui_navigation.run_uiautomator_task(
                flaky_task,
                timeout=timedelta(seconds=5),
                wait_between_tries=timedelta(milliseconds=1),
            )
        self.assertEqual(res, "recovered")
        self.assertEqual(calls, 2)
        mock_ui_dev.services.stop_all.assert_called_once_with()
        mock_sleep.assert_awaited_once()

    async def test_create_uiautomator_device_and_run_task(self) -> None:
        """Verifies create_uiautomator_device installs APK if missing and run_uiautomator_task stops services and recovers on error."""
        self.mock_adb.run.return_value = ""
        self.mock_adb.server_host_port = ("127.0.0.1", 5037)
        with (
            mock.patch.object(
                mobly_android_device, "AndroidDevice", autospec=True
            ) as mock_mobly_cls,
            mock.patch.object(
                ui_navigation,
                "_resolve_uiautomator_apk",
                return_value="/tmp/uiautomator.apk",
            ),
        ):
            mock_ui_dev = mock_mobly_cls.return_value
            mock_ui_dev.services = mock.create_autospec(
                service_manager.ServiceManager, instance=True
            )

            calls = 0

            async def sample_task(ui_dev: Any) -> str:
                nonlocal calls
                calls += 1
                if calls == 1:
                    raise mobly_adb.Error("adb error")
                return "done"

            res = await self.device.ui_navigation.run_uiautomator_task(
                sample_task,
                timeout=timedelta(seconds=5),
                wait_between_tries=timedelta(milliseconds=1),
            )
            self.assertEqual(res, "done")
            self.assertEqual(calls, 2)
            mock_ui_dev.services.stop_all.assert_called_once_with()
            self.mock_adb.check_connection.assert_called_once_with()
            self.mock_adb.run.assert_any_call(
                ["install", "-d", "-g", "-r", "/tmp/uiautomator.apk"]
            )
            mock_mobly_cls.assert_called_with(
                serial="127.0.0.1:5555",
                host="127.0.0.1",
                port=5037,
            )

    def test_honeydew_mobly_adb_proxy_construct_cmd(self) -> None:
        """Verifies _HoneydewMoblyAdbProxy command formatting across serial, host, port, and shell modes."""
        proxy = ui_navigation._HoneydewMoblyAdbProxy(
            serial="127.0.0.1:5555",
            host="127.0.0.1",
            port=5037,
            adb_binary="/bin/adb",
        )
        cmd = proxy._construct_adb_cmd("devices", ["-l"], shell=False)
        self.assertEqual(
            cmd,
            [
                "/bin/adb",
                "-H",
                "127.0.0.1",
                "-P",
                "5037",
                "-s",
                "127.0.0.1:5555",
                "devices",
                "-l",
            ],
        )

        cmd_shell = proxy._construct_adb_cmd("shell", "ls /sdcard", shell=True)
        self.assertEqual(
            cmd_shell,
            '"/bin/adb" -H "127.0.0.1" -P "5037" -s "127.0.0.1:5555" shell ls /sdcard',
        )

        proxy_noserial = ui_navigation._HoneydewMoblyAdbProxy(
            serial="",
            host="localhost",
            port=5038,
            adb_binary="/bin/adb",
        )
        cmd_noserial = proxy_noserial._construct_adb_cmd(
            "devices", [], shell=False
        )
        self.assertEqual(
            cmd_noserial,
            ["/bin/adb", "-H", "localhost", "-P", "5038", "devices"],
        )

        proxy_serialonly = ui_navigation._HoneydewMoblyAdbProxy(
            serial="device123",
            host="",
            port=None,
            adb_binary="adb",
        )
        cmd_serial_shell = proxy_serialonly._construct_adb_cmd(
            "get-state", "", shell=True
        )
        self.assertEqual(cmd_serial_shell, '"adb" -s "device123" get-state')

    async def test_wait_for_idle_screen_success(self) -> None:
        """Verifies wait_for_idle_screen wakes screen, skips black screen check by default, and succeeds."""
        mock_ui_device = _create_mock_uiautomator_device()
        mock_ui = mock_ui_device.ui
        mock_ui.package_name = "com.android.launcher3"
        mock_ui.return_value.exists = True
        mock_ui.return_value.wait.gone.return_value = False

        async def fake_run_uiautomator(
            callback: Any, *args: Any, **kwargs: Any
        ) -> Any:
            return await callback(mock_ui_device)

        with (
            mock.patch.object(
                self.device.ui_navigation,
                "run_uiautomator_task",
                autospec=True,
                side_effect=fake_run_uiautomator,
            ),
            mock.patch.object(
                self.device.ui_navigation,
                "check_and_mitigate_black_screen",
                new_callable=mock.AsyncMock,
            ) as mock_black_screen_check,
        ):
            await self.device.ui_navigation.wait_for_idle_screen()
            mock_black_screen_check.assert_not_awaited()
        mock_ui.press.assert_called_with(uiautomator.KeyEvent.KEYCODE_WAKEUP)

    async def test_wait_for_idle_screen_recovery_branches(self) -> None:
        """Verifies wait_for_idle_screen recovery branches for workspace gone, unexpected screen, dialogs, and fallback."""
        mock_ui_device = _create_mock_uiautomator_device()
        mock_ui = mock_ui_device.ui

        async def fake_run_uiautomator(
            callback: Any, *args: Any, **kwargs: Any
        ) -> Any:
            return await callback(mock_ui_device)

        # 1. Workspace disappears before settling -> raises UiAutomatorRetriableTaskError
        mock_ui.package_name = "com.android.launcher3"
        mock_ui.return_value.exists = True
        mock_ui.return_value.wait.gone.return_value = True

        with mock.patch.object(
            self.device.ui_navigation,
            "run_uiautomator_task",
            autospec=True,
            side_effect=fake_run_uiautomator,
        ):
            with self.assertRaises(UiAutomatorRetriableTaskError) as ctx:
                await self.device.ui_navigation.wait_for_idle_screen()
            self.assertIn("didn't settle", str(ctx.exception))

        # 2. _handle_unexpected_screen returns True -> raises UiAutomatorRetriableTaskError
        mock_ui.return_value.exists = False
        with (
            mock.patch.object(
                self.device.ui_navigation,
                "run_uiautomator_task",
                autospec=True,
                side_effect=fake_run_uiautomator,
            ),
            mock.patch.object(
                self.device,
                "_handle_unexpected_screen",
                return_value=True,
            ),
        ):
            with self.assertRaises(UiAutomatorRetriableTaskError) as ctx:
                await self.device.ui_navigation.wait_for_idle_screen()
            self.assertIn("Handled unexpected screen", str(ctx.exception))

        # 3. dismiss_blocking_dialogs returns True -> raises UiAutomatorRetriableTaskError
        with (
            mock.patch.object(
                self.device.ui_navigation,
                "run_uiautomator_task",
                autospec=True,
                side_effect=fake_run_uiautomator,
            ),
            mock.patch.object(
                self.device,
                "_handle_unexpected_screen",
                return_value=False,
            ),
            mock.patch.object(
                self.device,
                "dismiss_blocking_dialogs",
                return_value=True,
            ),
        ):
            with self.assertRaises(UiAutomatorRetriableTaskError) as ctx:
                await self.device.ui_navigation.wait_for_idle_screen()
            self.assertIn("Dismissed blocking dialog", str(ctx.exception))

        # 4. Unknown screen falls back to KEYCODE_HOME and raises UnknownStateUiAutomatorError
        mock_ui.press.reset_mock()
        with (
            mock.patch.object(
                self.device.ui_navigation,
                "run_uiautomator_task",
                autospec=True,
                side_effect=fake_run_uiautomator,
            ),
            mock.patch.object(
                self.device,
                "_handle_unexpected_screen",
                return_value=False,
            ),
            mock.patch.object(
                self.device,
                "dismiss_blocking_dialogs",
                return_value=False,
            ),
        ):
            with self.assertRaises(UnknownStateUiAutomatorError):
                await self.device.ui_navigation.wait_for_idle_screen()
            mock_ui.press.assert_called_with(uiautomator.KeyEvent.KEYCODE_HOME)

    async def test_wait_for_idle_screen_black_screen_mitigation(self) -> None:
        """Verifies 5 consecutive black screens trigger _mitigate_stuck_ui when enabled."""
        self.device.enable_black_screen_mitigation = True
        mock_ui_device = _create_mock_uiautomator_device()
        self.mock_screenshot.take.return_value = ScreenshotImage(
            size=ui_types.Size(10, 10),
            data=b"\x00" * 400,
        )

        invocations = 0

        async def fake_run_uiautomator(
            callback: Any, *args: Any, **kwargs: Any
        ) -> None:
            nonlocal invocations
            for _ in range(5):
                invocations += 1
                try:
                    await callback(mock_ui_device)
                except UiAutomatorRetriableTaskError:
                    pass

        with (
            mock.patch.object(
                self.device.ui_navigation,
                "run_uiautomator_task",
                autospec=True,
                side_effect=fake_run_uiautomator,
            ),
            mock.patch.object(
                self.device, "_mitigate_stuck_ui", new_callable=mock.AsyncMock
            ) as mock_mitigate,
        ):
            await self.device.ui_navigation.wait_for_idle_screen()
            self.assertEqual(invocations, 5)
            mock_mitigate.assert_called_once_with(5)
            self.mock_screenshot.take.assert_called_with(standardize=False)

    async def test_wait_for_idle_screen_black_screen_flake_failure(
        self,
    ) -> None:
        """Verifies 10 consecutive black screens abort real run_uiautomator_task with BlackScreenUiAutomatorError."""
        self.device.enable_black_screen_mitigation = True
        mock_ui_device = _create_mock_uiautomator_device()
        self.device.ui_navigation._uiautomator_device = mock_ui_device
        self.mock_screenshot.take.return_value = ScreenshotImage(
            size=ui_types.Size(10, 10),
            data=b"\x00" * 400,
        )

        with mock.patch.object(
            control_flows,
            "sleep_for_duration",
            new_callable=mock.AsyncMock,
        ):
            with self.assertRaises(BlackScreenUiAutomatorError) as ctx:
                await self.device.ui_navigation.wait_for_idle_screen()
            self.assertIsInstance(ctx.exception, UnknownStateUiAutomatorError)
            self.assertEqual(
                self.device.ui_navigation._consecutive_black_screen_count, 10
            )
            self.assertEqual(self.mock_screenshot.take.call_count, 10)

    async def test_verify_is_idle_screen(self) -> None:
        """Verifies verify_is_idle_screen succeeds on match and raises UiNavigationError on mismatch."""
        with mock.patch.object(
            self.device.ui_navigation,
            "run_uiautomator_task",
            autospec=True,
            return_value=True,
        ) as mock_run:
            await self.device.ui_navigation.verify_is_idle_screen()

            mock_run.return_value = False
            with self.assertRaises(ui_navigation.UiNavigationError):
                await self.device.ui_navigation.verify_is_idle_screen(
                    "Custom failure"
                )

    async def test_wait_for_launcher_screen(self) -> None:
        """Verifies wait_for_launcher_screen waits for idle then starts activity."""
        mock_ui_device = _create_mock_uiautomator_device()
        mock_ui_device.ui.return_value.exists = False
        mock_ui_device.ui.return_value.wait.exists.return_value = True

        async def fake_run_uiautomator(
            callback: Any, *args: Any, **kwargs: Any
        ) -> None:
            callback(mock_ui_device)

        with (
            mock.patch.object(
                self.device.ui_navigation,
                "run_uiautomator_task",
                autospec=True,
                side_effect=fake_run_uiautomator,
            ),
            mock.patch.object(
                self.device.ui_navigation,
                "wait_for_idle_screen",
                new_callable=mock.AsyncMock,
            ) as mock_wait_idle,
        ):
            await self.device.ui_navigation.wait_for_launcher_screen()
            mock_wait_idle.assert_called_once()
            self.mock_adb.run.assert_called_once_with(
                [
                    "shell",
                    "am",
                    "start",
                    "com.android.launcher3/.AllAppsActivity",
                ]
            )

    async def test_wait_for_launcher_screen_unsupported_raises(self) -> None:
        """Verifies wait_for_launcher_screen raises NotSupportedError when activity is unset."""
        self.device.apps_list_activity = None
        with mock.patch.object(
            self.device.ui_navigation,
            "wait_for_idle_screen",
            new_callable=mock.AsyncMock,
        ):
            with self.assertRaises(errors.NotSupportedError):
                await self.device.ui_navigation.wait_for_launcher_screen()

    def test_close(self) -> None:
        """Verifies close stops services, clears cached instance, and handles errors gracefully."""
        mock_ui_dev = _create_mock_uiautomator_device()
        self.device.ui_navigation._uiautomator_device = mock_ui_dev
        self.device.ui_navigation.close()
        mock_ui_dev.services.stop_all.assert_called_once_with()
        self.assertIsNone(self.device.ui_navigation._uiautomator_device)

        # Exception during stop_all should be caught and logged
        mock_ui_dev2 = _create_mock_uiautomator_device()
        mock_ui_dev2.services.stop_all.side_effect = OSError(
            "connection closed"
        )
        self.device.ui_navigation._uiautomator_device = mock_ui_dev2
        self.device.ui_navigation.close()
        self.assertIsNone(self.device.ui_navigation._uiautomator_device)

    def test_return_to_idle_screen(self) -> None:
        """Verifies return_to_idle_screen fires HOME category intent and raises UiNavigationError on failure."""
        self.device.ui_navigation.return_to_idle_screen()
        self.mock_adb.run.assert_called_with(
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
        self.mock_adb.run.side_effect = errors.HoneydewError("adb failed")
        with self.assertRaises(ui_navigation.UiNavigationError):
            self.device.ui_navigation.return_to_idle_screen()

    def test_ui_navigation_affordance_methods(self) -> None:
        """Verifies UiNavigation affordance methods delegate properly."""
        nav = self.device.ui_navigation
        self.assertIsInstance(nav, UiNavigation)
        self.assertIs(nav.adb, self.mock_adb)
        self.mock_adb.verify_supported.assert_called()

        img = ScreenshotImage(size=ui_types.Size(0, 0), data=b"")
        self.assertTrue(nav.is_black_screenshot(img))

        nav.return_to_home()
        self.mock_adb.run.assert_called_with(
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


if __name__ == "__main__":
    unittest.main()
