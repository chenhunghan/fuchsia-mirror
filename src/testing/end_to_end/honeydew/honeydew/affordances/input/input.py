# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Input affordances (touch gestures, buttons) for Starnix Android devices.

Architectural relationship across Honeydew UI and input affordances:
- `honeydew.affordances.ui.user_input`: Low-level Fuchsia input injection
  (`TouchDevice`, `KeyboardDevice`, `RotaryDevice`) backed by
  `fuchsia.ui.test.input`.
- `honeydew.affordances.input`: Higher-level gesture and button semantics
  (`Gestures` for relative/absolute taps, swipes, and edge swipes built on top
  of `user_input.TouchDevice`; `Buttons` for hardware/system key events and
  fallback ADB shell input).
- `honeydew.affordances.ui_navigation`: Semantic UI hierarchy inspection,
  element matching, and app/screen navigation powered by Android UiAutomator
  snippets.
"""

import logging
from collections.abc import Callable
from datetime import timedelta

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.input import errors as input_errors
from honeydew.affordances.ui.user_input import types as ui_types
from honeydew.affordances.ui.user_input import (
    user_input as user_input_affordance,
)
from honeydew.transports.adb import adb as adb_transport

_LOGGER: logging.Logger = logging.getLogger(__name__)
_SWIPE_EVENT_HZ_DEFAULT = 120
_DEFAULT_SWIPE_DURATION = timedelta(milliseconds=300)


class Gestures(affordance.Affordance):
    """Affordance for touch gesture input."""

    def __init__(
        self,
        user_input: user_input_affordance.UserInput,
        screen_size: ui_types.Size | Callable[[], ui_types.Size],
    ) -> None:
        """Initializes the Gestures affordance.

        Args:
            user_input: The underlying Honeydew UserInput affordance.
            screen_size: Fixed screen size or callable returning the current
                screen size in pixels.

        Raises:
            errors.NotSupportedError: If UserInput is not supported on the
                device.
            input_errors.InputError: If resolving screen size or creating the
                touch device fails.
        """
        self._user_input: user_input_affordance.UserInput = user_input
        if callable(screen_size):
            self._screen_size_fn: Callable[[], ui_types.Size] = screen_size
        else:
            fixed_size = screen_size
            self._screen_size_fn = lambda: fixed_size
        self.verify_supported()
        try:
            self._screen_size: ui_types.Size = self._screen_size_fn()
            self._touch_device: user_input_affordance.TouchDevice = (
                self._user_input.create_touch_device(self._screen_size)
            )
        except (
            errors.NotSupportedError,
            errors.NotEnabledError,
            errors.ConfigError,
        ):
            raise
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to create touch device: {e}"
            ) from e

    def _get_touch_device(self) -> user_input_affordance.TouchDevice:
        """Returns the touch device, recreating it if screen size changed.

        Returns:
            The active TouchDevice instance.

        Raises:
            input_errors.InputError: If recreating the touch device fails.
        """
        current_size: ui_types.Size | None = None
        try:
            current_size = self._screen_size_fn()
            if current_size != self._screen_size:
                self._touch_device = self._user_input.create_touch_device(
                    current_size
                )
                self._screen_size = current_size
        except (
            errors.NotSupportedError,
            errors.NotEnabledError,
            errors.ConfigError,
        ):
            raise
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to recreate touch device for screen size {current_size or self._screen_size}: {e}"
            ) from e
        return self._touch_device

    def verify_supported(self) -> None:
        """Verifies that Gestures affordance is supported by the device.

        Raises:
            errors.NotSupportedError: If Gestures affordance is not supported.
        """
        self._user_input.verify_supported()

    async def make_ready(self) -> None:
        """Eagerly initializes and registers the underlying touch device.

        Raises:
            input_errors.InputError: If initializing the touch device fails.
        """
        touch_device = self._get_touch_device()
        if isinstance(touch_device, affordance.AsyncLazyReady):
            try:
                await touch_device.make_ready()
            except input_errors.InputError:
                raise
            except errors.HoneydewError as e:
                raise input_errors.InputError(
                    f"Failed to initialize touch device: {e}"
                ) from e

    def screen_coordinate(
        self, x_percent: float, y_percent: float
    ) -> ui_types.Coordinate:
        """Converts normalized (x_percent, y_percent) coordinates into pixels.

        Args:
            x_percent: Normalized horizontal coordinate in [0.0, 1.0].
            y_percent: Normalized vertical coordinate in [0.0, 1.0].

        Returns:
            Pixel Coordinate corresponding to (x_percent, y_percent).

        Raises:
            input_errors.InputError: If `x_percent` or `y_percent` is outside
                `[0.0, 1.0]`, or if refreshing the touch device screen size
                fails.
        """
        if not (0.0 <= x_percent <= 1.0 and 0.0 <= y_percent <= 1.0):
            raise input_errors.InputError(
                f"Normalized coordinates must be in [0.0, 1.0], got ({x_percent}, {y_percent})"
            )
        self._get_touch_device()
        max_x = max(0, self._screen_size.width - 1)
        max_y = max(0, self._screen_size.height - 1)
        return ui_types.Coordinate(
            min(max(0, int(x_percent * self._screen_size.width)), max_x),
            min(max(0, int(y_percent * self._screen_size.height)), max_y),
        )

    async def tap(
        self, x: int, y: int, duration_of_one_tap_ms: int = 0
    ) -> None:
        """Taps at pixel coordinates (x, y).

        Args:
            x: Horizontal pixel coordinate.
            y: Vertical pixel coordinate.
            duration_of_one_tap_ms: Duration of the tap in milliseconds.

        Raises:
            input_errors.InputError: If the tap gesture fails.
        """
        try:
            await self._get_touch_device().tap(
                location=ui_types.Coordinate(x, y),
                duration_of_one_tap_ms=duration_of_one_tap_ms,
            )
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to tap at ({x}, {y}): {e}"
            ) from e

    async def tap_bottom(self) -> None:
        """Taps near the bottom center of the screen.

        Raises:
            input_errors.InputError: If the tap gesture fails.
        """
        coord = self.screen_coordinate(0.5, 0.9)
        try:
            await self._get_touch_device().tap(coord)
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to tap bottom at {coord}: {e}"
            ) from e

    async def swipe_left(
        self,
        duration: timedelta = _DEFAULT_SWIPE_DURATION,
        start_x_percent: float = 0.9,
        end_x_percent: float = 0.1,
        y_percent: float = 0.5,
    ) -> None:
        """Starting from right side of screen, swipe left.

        Args:
            duration: Total duration of the swipe gesture.
            start_x_percent: Normalized starting horizontal coordinate.
            end_x_percent: Normalized ending horizontal coordinate.
            y_percent: Normalized vertical coordinate along which to swipe.

        Raises:
            input_errors.InputError: If the swipe gesture fails.
        """
        await self.drag(
            start_x_percent, y_percent, end_x_percent, y_percent, duration
        )

    async def swipe_right(
        self,
        duration: timedelta = _DEFAULT_SWIPE_DURATION,
        start_x_percent: float = 0.1,
        end_x_percent: float = 0.9,
        y_percent: float = 0.5,
    ) -> None:
        """Starting from left side of screen, swipe right.

        Args:
            duration: Total duration of the swipe gesture.
            start_x_percent: Normalized starting horizontal coordinate.
            end_x_percent: Normalized ending horizontal coordinate.
            y_percent: Normalized vertical coordinate along which to swipe.

        Raises:
            input_errors.InputError: If the swipe gesture fails.
        """
        await self.drag(
            start_x_percent, y_percent, end_x_percent, y_percent, duration
        )

    async def swipe_up(
        self,
        duration: timedelta = _DEFAULT_SWIPE_DURATION,
        start_y_percent: float = 0.9,
        end_y_percent: float = 0.1,
        x_percent: float = 0.5,
    ) -> None:
        """Starting from bottom of screen, swipe up.

        Args:
            duration: Total duration of the swipe gesture.
            start_y_percent: Normalized starting vertical coordinate.
            end_y_percent: Normalized ending vertical coordinate.
            x_percent: Normalized horizontal coordinate along which to swipe.

        Raises:
            input_errors.InputError: If the swipe gesture fails.
        """
        await self.drag(
            x_percent, start_y_percent, x_percent, end_y_percent, duration
        )

    async def swipe_down(
        self,
        duration: timedelta = _DEFAULT_SWIPE_DURATION,
        start_y_percent: float = 0.1,
        end_y_percent: float = 0.9,
        x_percent: float = 0.5,
    ) -> None:
        """Starting from top of screen, swipe down.

        Args:
            duration: Total duration of the swipe gesture.
            start_y_percent: Normalized starting vertical coordinate.
            end_y_percent: Normalized ending vertical coordinate.
            x_percent: Normalized horizontal coordinate along which to swipe.

        Raises:
            input_errors.InputError: If the swipe gesture fails.
        """
        await self.drag(
            x_percent, start_y_percent, x_percent, end_y_percent, duration
        )

    async def drag(
        self,
        start_x: float,
        start_y: float,
        end_x: float,
        end_y: float,
        duration: timedelta = _DEFAULT_SWIPE_DURATION,
    ) -> None:
        """Drags from normalized (start_x, start_y) to (end_x, end_y).

        Args:
            start_x: Normalized starting horizontal coordinate.
            start_y: Normalized starting vertical coordinate.
            end_x: Normalized ending horizontal coordinate.
            end_y: Normalized ending vertical coordinate.
            duration: Total duration of the drag gesture.

        Raises:
            input_errors.InputError: If the drag gesture fails or duration <= 0.
        """
        if duration <= timedelta(0):
            raise input_errors.InputError(
                f"Drag duration must be strictly positive, got {duration}"
            )
        start_loc = self.screen_coordinate(start_x, start_y)
        end_loc = self.screen_coordinate(end_x, end_y)
        try:
            await self._get_touch_device().swipe(
                start_location=start_loc,
                end_location=end_loc,
                move_event_count=max(
                    1,
                    int(duration.total_seconds() * _SWIPE_EVENT_HZ_DEFAULT) - 1,
                ),
                duration_ms=int(duration.total_seconds() * 1000),
            )
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to drag from {start_loc} to {end_loc}: {e}"
            ) from e


class Buttons(affordance.Affordance):
    """Affordance for hardware button and rotary crown input."""

    def __init__(
        self,
        adb: adb_transport.Adb,
        user_input: user_input_affordance.UserInput | None = None,
    ) -> None:
        """Initializes the Buttons affordance.

        Args:
            adb: ADB transport instance used for Android keyevent injection.
            user_input: Optional Honeydew UserInput affordance used for rotary
                crown scrolling via a mouse device.

        Raises:
            errors.NotSupportedError: If ADB is not supported on the device.
        """
        self._adb: adb_transport.Adb = adb
        self._user_input: user_input_affordance.UserInput | None = user_input
        self._mouse_device_cache: (
            user_input_affordance.AsyncMouseDevice | None
        ) = None
        self.verify_supported()

    def verify_supported(self) -> None:
        """Verifies that Buttons affordance is supported by the device.

        Raises:
            errors.NotSupportedError: If Buttons affordance is not supported.
        """
        self._adb.verify_supported()

    @property
    def _mouse_device(self) -> user_input_affordance.AsyncMouseDevice:
        if self._mouse_device_cache is None:
            if self._user_input is None:
                raise errors.NotSupportedError(
                    "UserInput affordance is required for crown_scroll"
                )
            try:
                self._mouse_device_cache = (
                    self._user_input.create_mouse_device()
                )
            except errors.NotSupportedError:
                raise
            except input_errors.InputError:
                raise
            except errors.HoneydewError as e:
                raise input_errors.InputError(
                    f"Failed to create mouse device for crown_scroll: {e}"
                ) from e
        return self._mouse_device_cache

    async def crown_scroll(
        self,
        scroll_v_detent: int = 1,
        *,
        delta: int | None = None,
        scroll_v: int | None = None,
    ) -> None:
        """Scrolls the rotary crown via the Fuchsia mouse device.

        Args:
            scroll_v_detent: Vertical scroll detent count (default 1).
            delta: Optional alias for vertical scroll detent count. Takes
                precedence over `scroll_v` and `scroll_v_detent` when provided.
            scroll_v: Optional alias for vertical scroll detent count. Takes
                precedence over `scroll_v_detent` when `delta` is None.

        Raises:
            errors.NotSupportedError: If `user_input` was not configured.
            input_errors.InputError: If scrolling the mouse device fails.
        """
        detent = (
            delta
            if delta is not None
            else (scroll_v if scroll_v is not None else scroll_v_detent)
        )
        try:
            await self._mouse_device.scroll(scroll_v_detent=detent)
        except errors.NotSupportedError:
            raise
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to scroll rotary crown (detent={detent}): {e}"
            ) from e

    def _send_keyevent(self, keycode: str) -> None:
        """Sends an Android keyevent via ADB.

        Args:
            keycode: Android keycode name (e.g. "KEYCODE_POWER").

        Raises:
            input_errors.InputError: If the ADB keyevent command fails.
        """
        try:
            self._adb.run(["shell", "input", "keyevent", keycode])
        except input_errors.InputError:
            raise
        except errors.HoneydewError as e:
            raise input_errors.InputError(
                f"Failed to send keyevent {keycode}: {e}"
            ) from e

    def power(self) -> None:
        """Presses the power button.

        Raises:
            input_errors.InputError: If the ADB keyevent command fails.
        """
        self._send_keyevent("KEYCODE_POWER")

    def home(self) -> None:
        """Presses the home button.

        Raises:
            input_errors.InputError: If the ADB keyevent command fails.
        """
        self._send_keyevent("KEYCODE_HOME")

    def volume_up(self) -> None:
        """Presses the volume up button.

        Raises:
            input_errors.InputError: If the ADB keyevent command fails.
        """
        self._send_keyevent("KEYCODE_VOLUME_UP")

    def volume_down(self) -> None:
        """Presses the volume down button.

        Raises:
            input_errors.InputError: If the ADB keyevent command fails.
        """
        self._send_keyevent("KEYCODE_VOLUME_DOWN")

    def volume_mute(self) -> None:
        """Presses the volume mute button.

        Raises:
            input_errors.InputError: If the ADB keyevent command fails.
        """
        self._send_keyevent("KEYCODE_VOLUME_MUTE")
