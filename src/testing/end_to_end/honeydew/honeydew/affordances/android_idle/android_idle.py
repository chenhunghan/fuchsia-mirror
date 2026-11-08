# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""AndroidIdle affordance implementation using ADB."""

import asyncio
import datetime
import logging
from collections.abc import Callable
from datetime import timedelta

from honeydew import errors
from honeydew.affordances import affordance
from honeydew.affordances.android_idle import errors as idle_errors
from honeydew.affordances.android_session import android_session
from honeydew.affordances.android_session import errors as session_errors
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.utils import control_flows, deadline

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Chosen to provide regular progress logging and Android session health checks
# during long post-boot idle periods.
_IDLE_CHUNK: timedelta = timedelta(seconds=30)
_SYSPROP_TIMEOUT_SECS: float = 10.0


def elapsed_since_timestamp(
    raw_timestamp: str, now: datetime.datetime | None = None
) -> timedelta | None:
    """Returns duration elapsed since an integer epoch timestamp string.

    Args:
        raw_timestamp: String containing a UTC epoch timestamp in seconds.
        now: Optional reference current UTC datetime (defaults to current UTC
            time).

    Returns:
        Elapsed timedelta since `raw_timestamp` (clamped to >= 0s), or None if
        `raw_timestamp` is empty or not a valid integer.
    """
    cleaned = raw_timestamp.strip()
    if not cleaned or not cleaned.isdigit():
        return None
    current = now or datetime.datetime.now(datetime.timezone.utc)
    try:
        recorded = datetime.datetime.fromtimestamp(
            int(cleaned), tz=datetime.timezone.utc
        )
    except (OverflowError, OSError, ValueError):
        return None
    return max(timedelta(seconds=0), current - recorded)


class AndroidIdle(affordance.Affordance):
    """AndroidIdle affordance implementation using ADB.

    Queries and persists post-boot settling timestamps in a volatile (`debug.*`)
    Android system property on the target device via ADB, and coordinates with
    `AndroidSession` to invalidate settling state if the Android runtime restarts
    mid-idle. Because `debug.*` properties live in target RAM and are cleared
    automatically by the OS on reboot, storing the timestamp on-device allows
    repeated `idle_after_boot()` calls across separate test processes sharing
    the same booted target to avoid redundant multi-minute post-boot settling
    waits.

    Args:
        device_name: Device name returned by `ffx target list`.
        adb: ADB transport.
        session: AndroidSession affordance of the same device. Used to detect
            Android restarts while idling after boot.
        after_boot_duration: Default after-boot idle duration (or callable
            returning it). Evaluated on every `idle_after_boot()` so that
            updates to the device's duration stay in effect.
        idled_after_boot_prop: Android system property (or callable returning
            it) used to record when the device last idled after boot. When
            empty, on-device sysprop persistence is skipped and `AndroidIdle`
            falls back to in-process host-side tracking via
            `self._last_idle_timestamp`.
    """

    def __init__(
        self,
        device_name: str,
        adb: adb_transport.Adb,
        session: android_session.AndroidSession,
        after_boot_duration: timedelta | Callable[[], timedelta],
        idled_after_boot_prop: str | Callable[[], str] = "",
    ) -> None:
        """Initializes AndroidIdle.

        Args:
            device_name: Device name returned by `ffx target list`.
            adb: ADB transport.
            session: AndroidSession affordance of the same device.
            after_boot_duration: Default after-boot idle duration, or callable
                returning the default after-boot idle duration.
            idled_after_boot_prop: Android system property (or callable
                returning it) used to record when the device last idled after
                boot.

        Raises:
            errors.NotSupportedError: If AndroidIdle is not supported on the
                device.
        """
        self._device_name: str = device_name
        self._adb: adb_transport.Adb = adb
        self._session: android_session.AndroidSession = session
        self.verify_supported()

        self._idled_after_boot_prop_or_fn: str | Callable[
            [], str
        ] = idled_after_boot_prop
        self._after_boot_duration_or_fn: (
            timedelta | Callable[[], timedelta]
        ) = after_boot_duration
        self._last_idle_timestamp: datetime.datetime | None = None
        self._sysprop_write_failed: bool = False

        self._session.register_session_change_callback(
            self._clear_last_idle_time
        )

    @property
    def _idled_after_boot_prop(self) -> str:
        """Returns the current system property name for post-boot idle tracking."""
        if callable(self._idled_after_boot_prop_or_fn):
            return self._idled_after_boot_prop_or_fn()
        return self._idled_after_boot_prop_or_fn

    @property
    def _after_boot_duration(self) -> timedelta:
        """Returns the current default after-boot idle duration."""
        if callable(self._after_boot_duration_or_fn):
            return self._after_boot_duration_or_fn()
        return self._after_boot_duration_or_fn

    def close(self) -> None:
        """Unregisters session-change callbacks registered by AndroidIdle."""
        self._session.unregister_session_change_callback(
            self._clear_last_idle_time
        )

    def verify_supported(self) -> None:
        """Verifies that AndroidIdle affordance is supported by the device.

        Raises:
            errors.NotSupportedError: If affordance is not supported.
        """
        self._adb.verify_supported()
        self._session.verify_supported()

    async def idle(self, duration: timedelta, msg: str | None = None) -> None:
        """Idles for a given duration.

        Args:
            duration: How long to idle.
            msg: Optional reason for idling, included in the log message if
                provided.

        Raises:
            asyncio.CancelledError: If the coroutine is cancelled while idling.
        """
        if msg:
            _LOGGER.info("Idling for %s: %s", duration, msg)
        else:
            _LOGGER.info("Idling for %s", duration)
        await control_flows.sleep_for_duration(duration)

    async def idle_after_boot(self, duration: timedelta | None = None) -> None:
        """Idles after boot to allow post-boot background load to settle.

        Skips the part of `duration` that has already elapsed since the
        recorded post-boot idle timestamp, and verifies that the Android
        session does not change while idling. Records the idle timestamp only
        after idling completes, and clears the recorded timestamp if idling is
        interrupted so a subsequent call idles for the full duration.

        Args:
            duration: How long to idle after boot. Defaults to the device's
                after-boot duration.

        Raises:
            idle_errors.AndroidIdleSessionError: If the Android session changes while idling.
            idle_errors.AndroidIdleError: If session tracking or idle checks fail.
        """
        target_duration = (
            self._after_boot_duration if duration is None else duration
        )
        if target_duration < timedelta(seconds=0):
            raise idle_errors.AndroidIdleError(
                f"Idle duration must be non-negative, got {target_duration}"
            )
        if target_duration >= timedelta.max:
            raise idle_errors.AndroidIdleError("Tried to idle forever!")
        last_idle = self._last_idle_duration()
        remaining_time_to_idle = (
            target_duration
            if last_idle is None
            else target_duration - last_idle
        )
        if remaining_time_to_idle <= timedelta(seconds=0):
            _LOGGER.info("Skipping idling, we've already idled previously.")
            return

        idle_start = datetime.datetime.now(datetime.timezone.utc)
        effective_start = (
            idle_start if last_idle is None else idle_start - last_idle
        )
        idle_deadline = deadline.Deadline.from_timeout(remaining_time_to_idle)
        completed: bool = False
        cancelled: bool = False
        try:
            self._session.record_session()
            while remaining_time_to_idle > timedelta(seconds=0):
                chunk = min(remaining_time_to_idle, _IDLE_CHUNK)
                remaining_str = str(
                    timedelta(
                        seconds=round(remaining_time_to_idle.total_seconds())
                    )
                )
                await self.idle(
                    chunk, f"idling after boot ({remaining_str} remaining)"
                )
                self._session.verify_same_session()
                deadline_remaining = idle_deadline.remaining_duration()
                remaining_time_to_idle = min(
                    remaining_time_to_idle - chunk,
                    (
                        deadline_remaining
                        if deadline_remaining is not None
                        else remaining_time_to_idle - chunk
                    ),
                )
            self._reset_last_idle_time(effective_start)
            completed = True
        except asyncio.CancelledError:
            cancelled = True
            raise
        except session_errors.AndroidSessionError as e:
            raise idle_errors.AndroidIdleSessionError(
                f"Android session changed while idling after boot on {self._device_name}: {e}"
            ) from e
        except errors.HoneydewError as e:
            if not isinstance(e, idle_errors.AndroidIdleError):
                raise idle_errors.AndroidIdleError(
                    f"Failed while idling after boot on {self._device_name}: {e}"
                ) from e
            raise
        finally:
            if not completed:
                self._clear_last_idle_time(clear_sysprop=not cancelled)

    def _last_idle_duration(self) -> timedelta | None:
        """Returns the duration elapsed since the last post-boot idle, or None if not yet idled.

        Returns:
            Elapsed timedelta since the last post-boot idle, or None if no
            valid post-boot idle timestamp is recorded.
        """
        prop = self._idled_after_boot_prop
        if prop:
            last_idle_timestamp: str = ""
            prop_read_failed: bool = False
            try:
                last_idle_timestamp = self._adb.getprop(
                    prop,
                    timeout=_SYSPROP_TIMEOUT_SECS,
                    attempts=1,
                )
            except adb_errors.AdbError as e:
                prop_read_failed = True
                _LOGGER.warning("Failed to read %s: %s", prop, e)
            if not prop_read_failed:
                elapsed = elapsed_since_timestamp(last_idle_timestamp)
                if elapsed is not None:
                    return elapsed
                if not self._sysprop_write_failed:
                    self._last_idle_timestamp = None
                    return None
        if self._last_idle_timestamp is not None:
            return max(
                timedelta(seconds=0),
                datetime.datetime.now(datetime.timezone.utc)
                - self._last_idle_timestamp,
            )

        return None

    def _reset_last_idle_time(
        self, timestamp: datetime.datetime | None = None
    ) -> None:
        """Resets the recorded idle timestamp in host state and optional sysprop.

        On-device sysprop persistence is a best-effort cross-process
        optimization: if `setprop` fails (for example due to SELinux policy on
        restricted builds), `AdbError` is logged at warning level and in-process
        tracking falls back to `self._last_idle_timestamp`.

        Args:
            timestamp: Optional UTC datetime to record as the idle start time.
                Defaults to current UTC time.
        """
        now = timestamp or datetime.datetime.now(datetime.timezone.utc)
        self._last_idle_timestamp = now
        prop = self._idled_after_boot_prop
        if prop:
            try:
                self._adb.setprop(
                    prop,
                    str(int(now.timestamp())),
                    timeout=_SYSPROP_TIMEOUT_SECS,
                    attempts=1,
                )
                self._sysprop_write_failed = False
            except adb_errors.AdbError as e:
                self._sysprop_write_failed = True
                _LOGGER.warning(
                    "Failed to write sysprop %s: %s",
                    prop,
                    e,
                )

    def _clear_last_idle_time(self, clear_sysprop: bool = True) -> None:
        """Clears the recorded idle timestamp after a session change or aborted idle.

        Clearing the on-device sysprop is best-effort because session-change
        callbacks may run while the device is rebooting or ADB is unavailable.

        Args:
            clear_sysprop: If True, also clears the configured on-device
                sysprop via `setprop`.
        """
        self._last_idle_timestamp = None
        self._sysprop_write_failed = False
        prop = self._idled_after_boot_prop
        if clear_sysprop and prop:
            try:
                self._adb.setprop(
                    prop,
                    "",
                    timeout=_SYSPROP_TIMEOUT_SECS,
                    attempts=1,
                )
            except adb_errors.AdbError as e:
                _LOGGER.debug(
                    "Failed to clear sysprop %s: %s",
                    prop,
                    e,
                )
