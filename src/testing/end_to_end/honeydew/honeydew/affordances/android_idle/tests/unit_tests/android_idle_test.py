# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.android_idle.android_idle.py."""

import asyncio
import datetime
import unittest
from collections.abc import Callable
from datetime import timedelta
from typing import Any
from unittest import mock

from honeydew import errors
from honeydew.affordances.android_idle import android_idle
from honeydew.affordances.android_idle import errors as idle_errors
from honeydew.affordances.android_session import android_session
from honeydew.affordances.android_session import errors as session_errors
from honeydew.transports.adb import adb as adb_transport
from honeydew.transports.adb import errors as adb_errors
from honeydew.utils import control_flows

# pylint: disable=protected-access

_PROP: str = "debug.test.idled_after_boot"


class AndroidIdleTests(unittest.IsolatedAsyncioTestCase):
    """Unit tests for AndroidIdle."""

    def setUp(self) -> None:
        super().setUp()
        self.mock_adb: Any = mock.create_autospec(
            adb_transport.Adb, instance=True
        )
        self.mock_session: Any = mock.create_autospec(
            android_session.AndroidSession, instance=True
        )
        self.after_boot_duration: Any = mock.create_autospec(
            lambda: timedelta(minutes=4), return_value=timedelta(minutes=4)
        )
        self.android_idle_obj = self._create(_PROP)

    def _create(
        self, idled_after_boot_prop: str | Callable[[], str]
    ) -> android_idle.AndroidIdle:
        return android_idle.AndroidIdle(
            device_name="fuchsia-emulator",
            adb=self.mock_adb,
            session=self.mock_session,
            idled_after_boot_prop=idled_after_boot_prop,
            after_boot_duration=self.after_boot_duration,
        )

    def test_verify_supported_called_in_init(self) -> None:
        """Verifies that __init__ calls verify_supported() and delegates to adb and session."""
        self.mock_adb.verify_supported.assert_called_once_with()
        self.mock_session.verify_supported.assert_called_once_with()
        with mock.patch.object(
            android_idle.AndroidIdle,
            "verify_supported",
            autospec=True,
        ) as mock_verify_supported:
            self._create(_PROP)
        mock_verify_supported.assert_called_once()

    def test_verify_supported_not_supported(self) -> None:
        """Verifies verify_supported() raises NotSupportedError when adb or session is unsupported."""
        self.mock_adb.verify_supported.side_effect = errors.NotSupportedError(
            "adb not supported"
        )
        with self.assertRaises(errors.NotSupportedError):
            self._create(_PROP)

        self.mock_adb.verify_supported.side_effect = None
        self.mock_session.verify_supported.side_effect = (
            errors.NotSupportedError("session not supported")
        )
        with self.assertRaises(errors.NotSupportedError):
            self._create(_PROP)

    def test_close_unregisters_callback(self) -> None:
        """Verifies close() unregisters the session-change callback."""
        self.android_idle_obj.close()
        self.mock_session.unregister_session_change_callback.assert_called_once_with(
            self.android_idle_obj._clear_last_idle_time
        )

    def test_after_boot_duration_concrete_timedelta(self) -> None:
        """Verifies after_boot_duration accepts a concrete timedelta in addition to a callable."""
        idle_obj = android_idle.AndroidIdle(
            device_name="fuchsia-emulator",
            adb=self.mock_adb,
            session=self.mock_session,
            after_boot_duration=timedelta(seconds=42),
            idled_after_boot_prop=_PROP,
        )
        self.assertEqual(idle_obj._after_boot_duration, timedelta(seconds=42))

    def test_elapsed_since_timestamp(self) -> None:
        """Verifies elapsed_since_timestamp parses valid epoch strings and
        rejects invalid or out-of-range ones."""
        now = datetime.datetime(
            2026, 1, 1, 12, 0, 30, tzinfo=datetime.timezone.utc
        )
        ts = int(
            datetime.datetime(
                2026, 1, 1, 12, 0, 0, tzinfo=datetime.timezone.utc
            ).timestamp()
        )
        self.assertEqual(
            android_idle.elapsed_since_timestamp(str(ts), now=now),
            timedelta(seconds=30),
        )
        future_ts = int(
            datetime.datetime(
                2026, 1, 1, 12, 1, 0, tzinfo=datetime.timezone.utc
            ).timestamp()
        )
        self.assertEqual(
            android_idle.elapsed_since_timestamp(str(future_ts), now=now),
            timedelta(seconds=0),
        )
        self.assertIsNone(android_idle.elapsed_since_timestamp("", now=now))
        self.assertIsNone(
            android_idle.elapsed_since_timestamp("invalid", now=now)
        )
        self.assertIsNone(
            android_idle.elapsed_since_timestamp("9" * 30, now=now)
        )

    def test_last_idle_duration_from_sysprop(self) -> None:
        """Verifies _last_idle_duration computes duration from the sysprop."""
        self.mock_adb.getprop.return_value = "1767268800"

        with mock.patch.object(
            android_idle,
            "elapsed_since_timestamp",
            return_value=timedelta(seconds=10),
            autospec=True,
        ) as mock_elapsed:
            dur = self.android_idle_obj._last_idle_duration()
        self.assertEqual(dur, timedelta(seconds=10))
        self.mock_adb.getprop.assert_called_once_with(
            _PROP,
            timeout=android_idle._SYSPROP_TIMEOUT_SECS,
            attempts=1,
        )
        mock_elapsed.assert_called_once_with("1767268800")
        self.mock_adb.setprop.assert_not_called()

    def test_last_idle_duration_returns_none_when_no_prior_record(self) -> None:
        """Verifies _last_idle_duration returns None without mutating sysprop when unset."""
        self.mock_adb.getprop.return_value = ""

        dur = self.android_idle_obj._last_idle_duration()
        self.assertIsNone(dur)
        self.mock_adb.setprop.assert_not_called()

    def test_reset_last_idle_time(self) -> None:
        """Verifies _reset_last_idle_time writes the sysprop on the device."""
        self.android_idle_obj._reset_last_idle_time()
        self.mock_adb.setprop.assert_called_once()
        prop_arg, val_arg = self.mock_adb.setprop.call_args[0]
        self.assertEqual(prop_arg, _PROP)
        self.assertTrue(val_arg.isdigit())
        self.assertEqual(
            self.mock_adb.setprop.call_args[1],
            {"timeout": android_idle._SYSPROP_TIMEOUT_SECS, "attempts": 1},
        )

    def test_last_idle_duration_falls_back_to_host_timestamp_when_setprop_fails(
        self,
    ) -> None:
        """Verifies _last_idle_duration falls back to _last_idle_timestamp when setprop fails and getprop returns empty."""
        self.mock_adb.setprop.side_effect = adb_errors.AdbCommandError(
            "SELinux denial"
        )
        self.mock_adb.getprop.return_value = ""

        with self.assertLogs(level="WARNING") as log:
            self.android_idle_obj._reset_last_idle_time()
        self.assertIn("Failed to write sysprop", log.output[0])
        self.assertIsNotNone(self.android_idle_obj._last_idle_timestamp)

        dur = self.android_idle_obj._last_idle_duration()
        self.assertIsNotNone(dur)
        assert dur is not None
        self.assertGreaterEqual(dur, timedelta(seconds=0))

    def test_last_idle_duration_empty_sysprop_after_reboot_returns_none(
        self,
    ) -> None:
        """Verifies _last_idle_duration returns None and clears host timestamp when sysprop is empty after out-of-band reboot."""
        self.android_idle_obj._reset_last_idle_time()
        self.assertIsNotNone(self.android_idle_obj._last_idle_timestamp)

        self.mock_adb.getprop.return_value = ""
        dur = self.android_idle_obj._last_idle_duration()
        self.assertIsNone(dur)
        self.assertIsNone(self.android_idle_obj._last_idle_timestamp)

    def test_last_idle_duration_falls_back_when_getprop_fails(self) -> None:
        """Verifies _last_idle_duration logs warning and falls back to _last_idle_timestamp when getprop raises AdbError."""
        self.android_idle_obj._reset_last_idle_time()
        self.mock_adb.getprop.side_effect = adb_errors.AdbCommandError(
            "adb read error"
        )

        with self.assertLogs(level="WARNING") as log:
            dur = self.android_idle_obj._last_idle_duration()
        self.assertIn("Failed to read", log.output[0])
        self.assertIsNotNone(dur)
        assert dur is not None
        self.assertGreaterEqual(dur, timedelta(seconds=0))

    def test_dynamic_idled_after_boot_prop_callable(self) -> None:
        """Verifies idled_after_boot_prop accepts a callable resolved dynamically."""
        current_prop = ""
        idle_obj = self._create(lambda: current_prop)
        self.assertEqual(idle_obj._idled_after_boot_prop, "")
        idle_obj._reset_last_idle_time()
        self.mock_adb.setprop.assert_not_called()

        current_prop = "debug.dynamic.idled"
        self.assertEqual(idle_obj._idled_after_boot_prop, "debug.dynamic.idled")
        idle_obj._reset_last_idle_time()
        self.mock_adb.setprop.assert_called_once()
        self.assertEqual(
            self.mock_adb.setprop.call_args[0][0], "debug.dynamic.idled"
        )

    def test_empty_idled_after_boot_prop_skips_sysprop(self) -> None:
        """Verifies host-side idle tracking is used without touching sysprops when
        idled_after_boot_prop is empty."""
        android_idle_obj = self._create("")
        self.mock_session.register_session_change_callback.assert_called_with(
            android_idle_obj._clear_last_idle_time
        )
        self.assertIsNone(android_idle_obj._last_idle_duration())
        self.assertIsNone(android_idle_obj._last_idle_timestamp)
        android_idle_obj._reset_last_idle_time()
        self.assertIsNotNone(android_idle_obj._last_idle_timestamp)
        dur = android_idle_obj._last_idle_duration()
        self.assertIsNotNone(dur)
        assert dur is not None
        self.assertGreaterEqual(dur, timedelta(seconds=0))
        android_idle_obj._clear_last_idle_time()
        self.assertIsNone(android_idle_obj._last_idle_timestamp)
        self.mock_adb.getprop.assert_not_called()
        self.mock_adb.setprop.assert_not_called()

    async def test_idle(self) -> None:
        """Verifies idle calls sleep_for_duration with and without msg."""
        with mock.patch(
            "honeydew.utils.control_flows.sleep_for_duration",
            autospec=True,
        ) as mock_sleep:
            with self.assertLogs(level="INFO") as log_with_msg:
                await self.android_idle_obj.idle(
                    timedelta(seconds=5), "testing idle"
                )
            self.assertIn(
                "Idling for 0:00:05: testing idle", log_with_msg.output[0]
            )
            mock_sleep.assert_awaited_once_with(timedelta(seconds=5))

            mock_sleep.reset_mock()
            with self.assertLogs(level="INFO") as log_no_msg:
                await self.android_idle_obj.idle(timedelta(seconds=5))
            self.assertTrue(log_no_msg.output[0].endswith("Idling for 0:00:05"))
            mock_sleep.assert_awaited_once_with(timedelta(seconds=5))

    async def test_idle_after_boot_skips_when_already_idled(self) -> None:
        """Verifies idle_after_boot skips idling if remaining time <= 0."""
        with (
            mock.patch.object(
                self.android_idle_obj,
                "_last_idle_duration",
                return_value=timedelta(minutes=5),
                autospec=True,
            ),
            mock.patch.object(
                self.android_idle_obj, "idle", autospec=True
            ) as mock_idle,
        ):
            await self.android_idle_obj.idle_after_boot(
                duration=timedelta(minutes=1)
            )
        mock_idle.assert_not_awaited()
        self.mock_session.record_session.assert_not_called()
        self.mock_adb.setprop.assert_not_called()

    async def test_idle_after_boot_chunks_duration(self) -> None:
        """Verifies idle_after_boot idles in 30-second chunks and commits timestamp on completion."""
        with (
            mock.patch.object(
                self.android_idle_obj,
                "_last_idle_duration",
                return_value=None,
                autospec=True,
            ),
            mock.patch.object(
                self.android_idle_obj, "idle", autospec=True
            ) as mock_idle,
        ):
            await self.android_idle_obj.idle_after_boot(
                duration=timedelta(seconds=65)
            )
        self.assertEqual(mock_idle.await_count, 3)
        calls = [c[0][0] for c in mock_idle.await_args_list]
        self.assertEqual(calls[0], timedelta(seconds=30))
        self.assertEqual(calls[1], timedelta(seconds=30))
        self.assertAlmostEqual(calls[2].total_seconds(), 5.0, delta=0.1)
        self.mock_session.record_session.assert_called_once()
        self.assertEqual(self.mock_session.verify_same_session.call_count, 3)
        self.mock_adb.setprop.assert_called_once()
        self.after_boot_duration.assert_not_called()

    async def test_idle_after_boot_back_to_back_without_mocking_last_idle_duration(
        self,
    ) -> None:
        """Verifies calling idle_after_boot twice in a row skips sleeping on the second call."""
        simulated_now = datetime.datetime(
            2026, 1, 1, 12, 0, 0, tzinfo=datetime.timezone.utc
        )
        prop_state: dict[str, str] = {}

        def _fake_getprop(prop: str, **_: Any) -> str:
            return prop_state.get(prop, "")

        def _fake_setprop(prop: str, val: str, **_: Any) -> None:
            prop_state[prop] = val

        async def _fake_sleep(duration: timedelta) -> None:
            nonlocal simulated_now
            simulated_now += duration

        self.mock_adb.getprop.side_effect = _fake_getprop
        self.mock_adb.setprop.side_effect = _fake_setprop

        with (
            mock.patch.object(
                control_flows,
                "sleep_for_duration",
                side_effect=_fake_sleep,
                autospec=True,
            ) as mock_sleep,
            mock.patch.object(
                datetime, "datetime", wraps=datetime.datetime
            ) as mock_dt,
        ):
            mock_dt.now.side_effect = lambda tz=None: simulated_now
            await self.android_idle_obj.idle_after_boot(
                duration=timedelta(seconds=5)
            )
            self.assertEqual(mock_sleep.await_count, 1)

            mock_sleep.reset_mock()
            await self.android_idle_obj.idle_after_boot(
                duration=timedelta(seconds=5)
            )
            mock_sleep.assert_not_awaited()

    async def test_idle_after_boot_uses_after_boot_duration_callable(
        self,
    ) -> None:
        """Verifies the default duration comes from after_boot_duration."""
        self.after_boot_duration.return_value = timedelta(seconds=45)
        with (
            mock.patch.object(
                self.android_idle_obj,
                "_last_idle_duration",
                return_value=timedelta(seconds=0),
                autospec=True,
            ),
            mock.patch.object(
                self.android_idle_obj, "idle", autospec=True
            ) as mock_idle,
        ):
            await self.android_idle_obj.idle_after_boot()
        self.after_boot_duration.assert_called_once()
        self.assertEqual(mock_idle.await_count, 2)

    async def test_idle_after_boot_zero_duration_does_not_use_default(
        self,
    ) -> None:
        """Verifies passing duration=timedelta(seconds=0) skips idling instead
        of falling back to after_boot_duration."""
        with (
            mock.patch.object(
                self.android_idle_obj,
                "_last_idle_duration",
                return_value=timedelta(seconds=0),
                autospec=True,
            ),
            mock.patch.object(
                self.android_idle_obj, "idle", autospec=True
            ) as mock_idle,
        ):
            await self.android_idle_obj.idle_after_boot(
                duration=timedelta(seconds=0)
            )
        mock_idle.assert_not_awaited()
        self.after_boot_duration.assert_not_called()
        self.mock_session.record_session.assert_not_called()

    async def test_idle_after_boot_negative_duration_raises(self) -> None:
        """Verifies idle_after_boot raises AndroidIdleError on negative duration."""
        with self.assertRaisesRegex(
            idle_errors.AndroidIdleError,
            "Idle duration must be non-negative",
        ):
            await self.android_idle_obj.idle_after_boot(
                duration=timedelta(seconds=-1)
            )

    async def test_idle_after_boot_raises_if_infinite_deadline(self) -> None:
        """Verifies idle_after_boot raises AndroidIdleError on infinite deadline."""
        with self.assertRaisesRegex(
            idle_errors.AndroidIdleError, "Tried to idle forever!"
        ):
            await self.android_idle_obj.idle_after_boot(duration=timedelta.max)

    async def test_idle_after_boot_detects_android_restart(self) -> None:
        """Verifies idle_after_boot raises AndroidIdleSessionError and clears timestamp if Android restarts."""
        self.mock_session.verify_same_session.side_effect = (
            session_errors.AndroidSessionError("Android session changed")
        )
        with (
            mock.patch.object(
                self.android_idle_obj,
                "_last_idle_duration",
                return_value=timedelta(seconds=0),
                autospec=True,
            ),
            mock.patch.object(self.android_idle_obj, "idle", autospec=True),
        ):
            with self.assertRaises(
                idle_errors.AndroidIdleSessionError
            ) as err_ctx:
                await self.android_idle_obj.idle_after_boot(
                    duration=timedelta(seconds=25)
                )
        self.assertIsInstance(err_ctx.exception, idle_errors.AndroidIdleError)
        self.assertIsInstance(
            err_ctx.exception, session_errors.AndroidSessionError
        )
        self.mock_adb.setprop.assert_called_once_with(
            _PROP,
            "",
            timeout=android_idle._SYSPROP_TIMEOUT_SECS,
            attempts=1,
        )

    async def test_idle_after_boot_wraps_honeydew_error_and_clears_timestamp(
        self,
    ) -> None:
        """Verifies HoneydewError during idle_after_boot is wrapped in
        AndroidIdleError and clears the recorded timestamp."""
        self.mock_session.record_session.side_effect = (
            adb_errors.AdbCommandError("adb error")
        )
        with mock.patch.object(
            self.android_idle_obj,
            "_last_idle_duration",
            return_value=timedelta(seconds=0),
            autospec=True,
        ):
            with self.assertRaisesRegex(
                idle_errors.AndroidIdleError, "adb error"
            ):
                await self.android_idle_obj.idle_after_boot(
                    duration=timedelta(seconds=10)
                )
        self.mock_adb.setprop.assert_called_once_with(
            _PROP,
            "",
            timeout=android_idle._SYSPROP_TIMEOUT_SECS,
            attempts=1,
        )

    async def test_idle_after_boot_cancelled_skips_setprop_in_finally(
        self,
    ) -> None:
        """Verifies CancelledError clears _last_idle_timestamp without blocking on setprop."""
        self.android_idle_obj._last_idle_timestamp = datetime.datetime.now(
            datetime.timezone.utc
        )
        with (
            mock.patch.object(
                self.android_idle_obj,
                "_last_idle_duration",
                return_value=timedelta(seconds=0),
                autospec=True,
            ),
            mock.patch.object(
                self.android_idle_obj,
                "idle",
                autospec=True,
                side_effect=asyncio.CancelledError(),
            ),
        ):
            with self.assertRaises(asyncio.CancelledError):
                await self.android_idle_obj.idle_after_boot(
                    duration=timedelta(seconds=10)
                )
        self.assertIsNone(self.android_idle_obj._last_idle_timestamp)
        self.mock_adb.setprop.assert_not_called()


if __name__ == "__main__":
    unittest.main()
