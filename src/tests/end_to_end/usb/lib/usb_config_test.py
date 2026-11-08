# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for usb_config library."""

import time
import unittest
from unittest import mock

import usb_lib.usb_config as usb_config


class UsbConfigTest(unittest.TestCase):
    """Tests set_usb_config serial dispatch, error handling, and recovery."""

    def test_set_usb_config_missing_serial_raises_without_ssh_fallback(
        self,
    ) -> None:
        dut = mock.MagicMock()
        dut.serial = None

        with self.assertRaises(RuntimeError) as ctx:
            usb_config.set_usb_config(dut, "sourcesink")
        self.assertIn(
            "Failed to dispatch USB config 'sourcesink'", str(ctx.exception)
        )
        dut.ffx.run_ssh_cmd.assert_not_called()

    def test_set_usb_config_dispatch_failure_raises_without_reboot(
        self,
    ) -> None:
        dut = mock.MagicMock()
        dut.serial.send_and_recv.side_effect = OSError("Serial write error")

        with self.assertRaises(RuntimeError) as ctx:
            usb_config.set_usb_config(dut, "cdc,adb")
        self.assertIn(
            "Failed to dispatch USB config 'cdc,adb'", str(ctx.exception)
        )
        dut.reboot.assert_not_called()
        dut.ffx.run_ssh_cmd.assert_not_called()

    def test_set_usb_config_success(self) -> None:
        dut = mock.MagicMock()
        dut.serial.send_and_recv.return_value = "[usb-cli:DONE]"

        usb_config.set_usb_config(dut, "cdc,adb")
        dut.serial.send_and_recv.assert_called_once_with(
            cmd="usb-cli set-config cdc,adb",
            stop_tokens=("[usb-cli:DONE]", "[usb-cli:ERROR]"),
            timeout_sec=8.0,
        )
        dut.reboot.assert_not_called()
        dut.ffx.run_ssh_cmd.assert_not_called()

    def test_notify_intentional_disconnect_notifies_all_transports(
        self,
    ) -> None:
        """Verifies the wrapper notifies fuchsia_controller then ffx."""
        dut = mock.MagicMock()

        # pylint: disable-next=protected-access
        usb_config._notify_intentional_disconnect(dut)

        self.assertEqual(
            [c[0] for c in dut.mock_calls],
            [
                "fuchsia_controller.before_usb_disconnect",
                "ffx.notify_intentional_disconnect",
            ],
        )

    def test_set_usb_config_notifies_intentional_disconnect(self) -> None:
        """Verifies the FFX monitor is notified before the switch is sent."""
        dut = mock.MagicMock()
        dut.serial.send_and_recv.return_value = "[usb-cli:DONE]"

        usb_config.set_usb_config(dut, "sourcesink")

        # Both hooks run, in order, before the config command goes out over
        # serial.
        self.assertEqual(
            [c[0] for c in dut.mock_calls],
            [
                "fuchsia_controller.before_usb_disconnect",
                "ffx.notify_intentional_disconnect",
                "serial.send_and_recv",
            ],
        )

    def test_get_usb_config_uses_serial_only(self) -> None:
        dut = mock.MagicMock()
        dut.serial.send_and_recv.return_value = (
            '{"functions": ["cdc"]} [usb-cli:DONE]'
        )

        res = usb_config.get_usb_config(dut)
        self.assertIn('"functions": ["cdc"]', res)
        dut.serial.send_and_recv.assert_called_once_with(
            cmd="usb-cli get-config",
            stop_tokens=("[usb-cli:DONE]", "[usb-cli:ERROR]"),
            timeout_sec=5.0,
        )
        dut.ffx.run_ssh_cmd.assert_not_called()

    def test_get_usb_config_missing_serial_raises(self) -> None:
        dut = mock.MagicMock()
        dut.serial = None

        with self.assertRaises(RuntimeError):
            usb_config.get_usb_config(dut)
        dut.ffx.run_ssh_cmd.assert_not_called()

    def test_get_usb_config_strips_klog_only_noise(self) -> None:
        dut = mock.MagicMock()
        dut.serial.send_and_recv.return_value = (
            "[00002.645] 30058:34264> [netstack] INFO: RuleMatcher "
            "{ from: None, functions: None }\r\n"
        )
        self.assertEqual(usb_config.get_usb_config(dut), "")

    def test_parse_usb_config_functions_valid_formats(self) -> None:
        """Verifies valid usb-cli JSON formats including interleaved klog/CRLF/ANSI."""
        netstack_warn = (
            "[00204.077] 30177:34235> [netstack] WARN: "
            "stream::SetIpv6TrafficClass is not supported\r\n"
        )
        interleaved_crlf_ansi = (
            "\x1b[32m$ usb-cli get-config\x1b[0m\r\n"
            "{\r\n"
            '  "configurations": [["cdc", "adb", "vsock"]],\r\n'
            f'  "product": "CDC Ethernet & {netstack_warn}ADB & VSOCK Bridge"\r\n'
            "}\r\n"
            "[usb-cli:DONE]\r\n"
        )
        cases: tuple[tuple[str, list[str]], ...] = (
            (interleaved_crlf_ansi, ["cdc", "adb", "vsock"]),
            ('{"functions": ["cdc", "adb"]} [usb-cli:DONE]', ["cdc", "adb"]),
            (
                '{"configurations": [["cdc"], ["custom_0xff_0x42_0x01"]]} [usb-cli:DONE]',
                ["cdc", "custom_0xff_0x42_0x01"],
            ),
            ('{"configurations": []} [usb-cli:DONE]', []),
        )
        for raw_output, expected in cases:
            with self.subTest(expected=expected):
                self.assertEqual(
                    usb_config.parse_usb_config_functions(raw_output), expected
                )

    def test_parse_usb_config_functions_rejects_corrupt_and_non_json_inputs(
        self,
    ) -> None:
        """Verifies comma-separated, truncated, unprefixed-noise, and error strings fail closed."""
        unprefixed_splice = (
            '{\r\n  "configurations": [["cdc", "adb", "vsock"]],\r\n'
            '  "product": "CDC & debuglog: dropped 12 messages\r\nBridge"\r\n}'
        )
        invalid_inputs = (
            "",
            "[usb-cli:DONE]",
            "cdc,adb [usb-cli:DONE]",
            '{"configurations": [["cdc"]]} [usb-cli:ERROR]',
            '{"configurations": [["cdc"]]} usb-cli error: Failed',
            '{\n  "configurations": [\n    [\n      "cdc",\n      "adb"',
            unprefixed_splice,
            '{"configurations": ["cdc"]}',
            '{"configurations": [["invalid space"]]}',
            '{"functions": "cdc"}',
        )
        for bad_input in invalid_inputs:
            with self.subTest(bad_input=bad_input[:40]):
                self.assertIsNone(usb_config.parse_get_config(bad_input))
                with self.assertRaises(usb_config.UsbConfigParseError):
                    usb_config.parse_usb_config_functions(bad_input)

    @mock.patch.object(time, "sleep")
    def test_read_usb_functions_over_serial_retries_until_valid(
        self, mock_sleep: mock.MagicMock
    ) -> None:
        """Verifies read_usb_functions_over_serial retries empty and garbled outputs."""
        dut = mock.MagicMock()
        with mock.patch.object(
            usb_config,
            "get_usb_config",
            side_effect=[
                "",
                "garbled non-json output",
                '{"configurations": []}',
                '{"configurations": [["cdc", "vsock"]]} [usb-cli:DONE]',
            ],
        ) as mock_get:
            raw, funcs = usb_config.read_usb_functions_over_serial(
                dut, max_attempts=5, timeout_sec=10.0, retry_delay_sec=0.1
            )

        self.assertEqual(mock_get.call_count, 4)
        self.assertEqual(mock_sleep.call_count, 3)
        self.assertEqual(funcs, ["cdc", "vsock"])
        self.assertIn("cdc", raw)

    @mock.patch.object(time, "sleep")
    def test_read_usb_functions_over_serial_failure_modes(
        self, mock_sleep: mock.MagicMock
    ) -> None:
        """Verifies exhausted retries return [] and missing serial raises immediately."""
        dut = mock.MagicMock()
        with mock.patch.object(
            usb_config, "get_usb_config", return_value="bad output"
        ) as mock_get:
            raw, funcs = usb_config.read_usb_functions_over_serial(
                dut, max_attempts=3, timeout_sec=10.0, retry_delay_sec=0.1
            )
        self.assertEqual(mock_get.call_count, 3)
        self.assertEqual(mock_sleep.call_count, 2)
        self.assertEqual((raw, funcs), ("bad output", []))

        with mock.patch.object(
            usb_config,
            "get_usb_config",
            side_effect=RuntimeError("No serial console available"),
        ) as mock_get:
            with self.assertRaises(RuntimeError):
                usb_config.read_usb_functions_over_serial(dut)
        mock_get.assert_called_once_with(dut)


if __name__ == "__main__":
    unittest.main()
