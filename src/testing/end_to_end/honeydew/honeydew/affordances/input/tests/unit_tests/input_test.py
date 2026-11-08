# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.input (Gestures, Buttons) and AndroidDevice properties."""

import unittest
from datetime import timedelta
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.input import Buttons, Gestures, InputError
from honeydew.affordances.input import errors as input_errors
from honeydew.affordances.input import input as input_affordance
from honeydew.affordances.ui.user_input import errors as user_input_errors
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.affordances.ui.user_input import (
    user_input as user_input_affordance,
)
from honeydew.affordances.ui.user_input import (
    user_input_using_fc,
)
from honeydew.device_classes import android_device
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.transports.ffx import config as ffx_config
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.typing import custom_types

# pylint: disable=protected-access


class TestInputDevice(android_device.AndroidDevice):
    """Concrete test subclass with default screen size configured."""

    _default_screen_size = ui_types.Size(123, 456)


class InputTest(unittest.IsolatedAsyncioTestCase):
    """Tests for input gestures, buttons, and AndroidDevice properties."""

    def setUp(self) -> None:
        self.mock_user_input: Any = mock.create_autospec(
            user_input_affordance.UserInput, instance=True
        )
        self.mock_touch: Any = mock.create_autospec(
            user_input_using_fc.TouchDeviceUsingFc, instance=True
        )
        self.mock_mouse: Any = mock.create_autospec(
            user_input_affordance.AsyncMouseDevice, instance=True
        )
        self.mock_user_input.create_touch_device.return_value = self.mock_touch
        self.mock_user_input.create_mouse_device.return_value = self.mock_mouse
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )
        self.mock_adb.run.return_value = ""
        self.gestures = Gestures(self.mock_user_input, ui_types.Size(100, 200))

    async def test_gestures_coordinates_taps_and_make_ready(self) -> None:
        """Verifies verify_supported, make_ready, screen_coordinate, tap, and tap_bottom."""
        self.assertIs(InputError, input_errors.InputError)
        self.mock_user_input.verify_supported.assert_called_once_with()
        self.assertIsInstance(self.mock_touch, affordance.AsyncLazyReady)
        await self.gestures.make_ready()
        self.mock_touch.make_ready.assert_awaited_once_with()
        self.assertEqual(
            self.gestures.screen_coordinate(0.5, 0.25),
            ui_types.Coordinate(50, 50),
        )
        self.assertEqual(
            self.gestures.screen_coordinate(1.0, 1.0),
            ui_types.Coordinate(99, 199),
        )
        self.assertEqual(
            self.gestures.screen_coordinate(0.0, 0.0),
            ui_types.Coordinate(0, 0),
        )
        for bad_x, bad_y in [(-0.1, 0.5), (1.1, 0.5), (0.5, -0.1), (0.5, 1.1)]:
            with self.assertRaises(input_errors.InputError):
                self.gestures.screen_coordinate(bad_x, bad_y)
        await self.gestures.tap(10, 20, duration_of_one_tap_ms=50)
        self.mock_touch.tap.assert_called_once_with(
            location=ui_types.Coordinate(10, 20),
            duration_of_one_tap_ms=50,
        )
        self.mock_touch.reset_mock()
        await self.gestures.tap_bottom()
        self.mock_touch.tap.assert_called_once_with(
            ui_types.Coordinate(50, 180)
        )

    async def test_gestures_dynamic_screen_size_callable(self) -> None:
        """Verifies Gestures accepts a callable screen_size and recreates touch device on change."""
        current_size = ui_types.Size(100, 200)
        self.mock_user_input.create_touch_device.reset_mock()
        gestures = Gestures(self.mock_user_input, lambda: current_size)
        self.mock_user_input.create_touch_device.assert_called_once_with(
            ui_types.Size(100, 200)
        )
        self.assertEqual(
            gestures.screen_coordinate(0.5, 0.5),
            ui_types.Coordinate(50, 100),
        )

        current_size = ui_types.Size(400, 800)
        self.mock_user_input.create_touch_device.side_effect = (
            errors.HoneydewError("transient error")
        )
        with self.assertRaises(input_errors.InputError):
            gestures.screen_coordinate(0.5, 0.5)
        self.assertEqual(gestures._screen_size, ui_types.Size(100, 200))

        self.mock_user_input.create_touch_device.side_effect = None
        self.assertEqual(
            gestures.screen_coordinate(0.5, 0.5),
            ui_types.Coordinate(200, 400),
        )
        self.assertEqual(self.mock_user_input.create_touch_device.call_count, 3)
        self.mock_user_input.create_touch_device.assert_called_with(
            ui_types.Size(400, 800)
        )

    async def test_gestures_swipes(self) -> None:
        """Verifies swipe_left, swipe_right, swipe_up, and swipe_down."""
        gestures = input_affordance.Gestures(
            self.mock_user_input, ui_types.Size(100, 200)
        )
        dur = timedelta(milliseconds=500)
        swipes: list[tuple[Any, tuple[int, int], tuple[int, int]]] = [
            (gestures.swipe_left, (90, 100), (10, 100)),
            (gestures.swipe_right, (10, 100), (90, 100)),
            (gestures.swipe_up, (50, 180), (50, 20)),
            (gestures.swipe_down, (50, 20), (50, 180)),
        ]
        for fn, start, end in swipes:
            self.mock_touch.reset_mock()
            await fn(dur)
            self.mock_touch.swipe.assert_called_once_with(
                start_location=ui_types.Coordinate(*start),
                end_location=ui_types.Coordinate(*end),
                move_event_count=59,
                duration_ms=500,
            )
            self.mock_touch.reset_mock()
            await fn()
            self.mock_touch.swipe.assert_called_once_with(
                start_location=ui_types.Coordinate(*start),
                end_location=ui_types.Coordinate(*end),
                move_event_count=35,
                duration_ms=300,
            )

    async def test_gestures_drag_invalid_duration(self) -> None:
        """Verifies drag raises InputError when duration is non-positive."""
        for bad_duration in [timedelta(0), timedelta(milliseconds=-10)]:
            with self.assertRaises(input_errors.InputError):
                await self.gestures.drag(
                    0.1, 0.1, 0.9, 0.9, duration=bad_duration
                )

    async def test_gestures_wraps_honeydew_errors_in_input_error(self) -> None:
        """Verifies Gestures wraps HoneydewError in InputError and calls verify_supported first."""
        self.mock_touch.make_ready.side_effect = (
            user_input_errors.UserInputError("make_ready failed")
        )
        with self.assertRaises(input_errors.InputError):
            await self.gestures.make_ready()
        self.mock_touch.make_ready.side_effect = None

        self.mock_touch.tap.side_effect = user_input_errors.UserInputError(
            "tap failed"
        )
        with self.assertRaises(input_errors.InputError):
            await self.gestures.tap(10, 20)
        with self.assertRaises(input_errors.InputError):
            await self.gestures.tap_bottom()
        self.mock_touch.tap.side_effect = None

        self.mock_touch.swipe.side_effect = user_input_errors.UserInputError(
            "swipe failed"
        )
        with self.assertRaises(input_errors.InputError):
            await self.gestures.swipe_up()
        self.mock_touch.swipe.side_effect = None

        def _failing_screen_size() -> ui_types.Size:
            raise errors.HoneydewError("screen_size query failed")

        with self.assertRaises(input_errors.InputError):
            Gestures(self.mock_user_input, _failing_screen_size)

        self.mock_user_input.verify_supported.side_effect = (
            errors.NotSupportedError("user_input not supported")
        )
        with self.assertRaises(errors.NotSupportedError):
            Gestures(self.mock_user_input, _failing_screen_size)
        self.mock_user_input.verify_supported.side_effect = None

        self.mock_user_input.create_touch_device.side_effect = (
            errors.HoneydewError("create_touch_device failed")
        )
        with self.assertRaises(input_errors.InputError):
            Gestures(self.mock_user_input, ui_types.Size(100, 200))

    async def test_buttons(self) -> None:
        """Verifies hardware keyevents and crown_scroll."""
        self.mock_user_input.verify_supported.reset_mock()
        buttons = Buttons(self.mock_adb, self.mock_user_input)
        self.mock_adb.verify_supported.assert_called_once_with()
        self.mock_user_input.verify_supported.assert_not_called()
        for fn, code in [
            (buttons.power, "KEYCODE_POWER"),
            (buttons.home, "KEYCODE_HOME"),
            (buttons.volume_up, "KEYCODE_VOLUME_UP"),
            (buttons.volume_down, "KEYCODE_VOLUME_DOWN"),
            (buttons.volume_mute, "KEYCODE_VOLUME_MUTE"),
        ]:
            fn()
            self.mock_adb.run.assert_called_with(
                ["shell", "input", "keyevent", code]
            )
        await buttons.crown_scroll(scroll_v_detent=3)
        self.mock_mouse.scroll.assert_awaited_once_with(
            scroll_v_detent=3,
        )
        self.mock_mouse.scroll.reset_mock()
        await buttons.crown_scroll(delta=-2)
        self.mock_mouse.scroll.assert_awaited_once_with(
            scroll_v_detent=-2,
        )

        buttons_no_ui = Buttons(self.mock_adb)
        with self.assertRaises(errors.NotSupportedError):
            await buttons_no_ui.crown_scroll()

    async def test_buttons_wraps_honeydew_errors_in_input_error(self) -> None:
        """Verifies Buttons wraps ADB and UserInput HoneydewError in InputError."""
        buttons = Buttons(self.mock_adb, self.mock_user_input)
        self.mock_adb.run.side_effect = adb_errors.AdbCommandError("adb failed")
        for fn in (
            buttons.power,
            buttons.home,
            buttons.volume_up,
            buttons.volume_down,
            buttons.volume_mute,
        ):
            with self.assertRaises(input_errors.InputError):
                fn()
        self.mock_adb.run.side_effect = None

        self.mock_mouse.scroll.side_effect = user_input_errors.UserInputError(
            "scroll failed"
        )
        with self.assertRaises(input_errors.InputError):
            await buttons.crown_scroll(scroll_v_detent=1)
        self.mock_mouse.scroll.side_effect = None

        buttons_fresh = Buttons(self.mock_adb, self.mock_user_input)
        self.mock_user_input.create_mouse_device.side_effect = (
            user_input_errors.UserInputError("create_mouse_device failed")
        )
        with self.assertRaises(input_errors.InputError):
            await buttons_fresh.crown_scroll()

    async def test_android_device_properties_and_cache_invalidation(
        self,
    ) -> None:
        """Verifies AndroidDevice.gestures, buttons, and cache invalidation on screen size change."""
        dev_info = custom_types.DeviceInfo(
            name="fuchsia-emulator",
            serial_number=None,
            address=None,
            serial_socket=None,
        )
        mock_ffx = mock.create_autospec(ffx_transport.FFX, instance=True)
        mock_ffx_cfg = mock.create_autospec(
            ffx_config.FfxConfigData, instance=True
        )
        with (
            mock.patch.object(
                fuchsia_device.FuchsiaDevice, "health_check", autospec=True
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "ffx",
                new_callable=mock.PropertyMock,
                return_value=mock_ffx,
            ),
            mock.patch.object(
                android_device.AndroidDevice,
                "adb",
                new_callable=mock.PropertyMock,
                return_value=self.mock_adb,
            ),
            mock.patch.object(
                fuchsia_device.FuchsiaDevice,
                "user_input",
                new_callable=mock.PropertyMock,
                return_value=self.mock_user_input,
            ),
        ):
            device = TestInputDevice(
                device_info=dev_info,
                ffx_config_data=mock_ffx_cfg,
            )

            gestures1 = device.gestures
            self.assertIs(gestures1, device.gestures)
            self.assertEqual(gestures1._screen_size, ui_types.Size(123, 456))
            self.assertIs(device.buttons, device.buttons)

            self.mock_adb.run.return_value = "Physical size: 321x654\n"
            device.screen_size()

            gestures2 = device.gestures
            self.assertIsNot(gestures1, gestures2)
            self.assertEqual(gestures2._screen_size, ui_types.Size(321, 654))

            device_override = TestInputDevice(
                device_info=dev_info,
                ffx_config_data=mock_ffx_cfg,
                config={"screen_size": "200x400"},
            )
            self.assertEqual(
                device_override.gestures._screen_size, ui_types.Size(200, 400)
            )


if __name__ == "__main__":
    unittest.main()
