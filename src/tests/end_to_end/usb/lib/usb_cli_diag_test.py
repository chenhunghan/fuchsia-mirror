# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host unit tests for `usb_lib.usb_cli_diag`."""

import unittest
from typing import Any
from unittest import mock

from honeydew import errors as honeydew_errors
from usb_lib import usb_cli_diag
from usb_lib.usb_cli_diag import (
    find_diag_problems,
    find_dut_diag_problems,
    find_stuck_endpoints,
    get_endpoint_bytes,
    get_endpoint_types,
    parse_dwc3_endpoints,
    run_usb_cli_diag,
)

_DIAG_OUTPUT = """\
=== USB Function Health ===
  [OK]          CDC:            Configured
  [OK]          ADB:            Configured
=== Device-Side USB Inspect Diagnostics ===
=== Synopsys DWC3 Controller Diagnostics ===
  dwc3:
    endpoints:
      endpoint-0x00:
        enabled: true
        type: 0 (Control)
        stalled: true
      endpoint-0x02:
        enabled: true
        stalled: false
        type: 2 (Bulk)
        total_bytes: 5242880
        total_transfers: 50
        trb_fifo:
          total_slots: 128
          read_offset: 10
          active_trbs:
            0:
              hardware_owned: false
            1:
              hardware_owned: true
      endpoint-0x03:
        enabled: true
        type: 3 (Interrupt)
        total_bytes: 64
      endpoint-0x04:
        enabled: false
        type: 2 (Bulk)
"""


def _diag_with_ep2_ring(
    *,
    read_offset: int,
    total_transfers: int,
    hwo_bits: list[bool],
    total_slots: int = 128,
) -> str:
    trb_lines = "\n".join(
        f"            {i}:\n"
        f"              hardware_owned: {'true' if bit else 'false'}"
        for i, bit in enumerate(hwo_bits)
    )
    return f"""\
=== USB Function Health ===
  [OK]          CDC:            Configured
=== Device-Side USB Inspect Diagnostics ===
=== Synopsys DWC3 Controller Diagnostics ===
  dwc3:
    endpoints:
      endpoint-0x02:
        enabled: true
        stalled: false
        type: 2 (Bulk)
        total_bytes: 5242880
        total_transfers: {total_transfers}
        trb_fifo:
          total_slots: {total_slots}
          read_offset: {read_offset}
          active_trbs:
{trb_lines}
"""


class ParseDwc3EndpointsTest(unittest.TestCase):
    """Tests `parse_dwc3_endpoints`."""

    def test_parses_nested_inspect_nodes(self) -> None:
        endpoints = parse_dwc3_endpoints(_DIAG_OUTPUT)
        self.assertEqual(
            sorted(endpoints),
            [
                "endpoint-0x00",
                "endpoint-0x02",
                "endpoint-0x03",
                "endpoint-0x04",
            ],
        )
        self.assertIs(endpoints["endpoint-0x02"]["enabled"], True)
        self.assertEqual(endpoints["endpoint-0x02"]["type"], "2 (Bulk)")
        self.assertIs(
            endpoints["endpoint-0x02"]["trb_fifo"]["active_trbs"]["1"][
                "hardware_owned"
            ],
            True,
        )

    def test_drops_kernel_log_lines(self) -> None:
        output = _DIAG_OUTPUT.replace(
            "      endpoint-0x03:\n",
            "      endpoint-0x03:\n"
            "[00024.593] 15009:35497> [driver, usb-phy] INFO: noise\n",
        )
        self.assertEqual(
            parse_dwc3_endpoints(output), parse_dwc3_endpoints(_DIAG_OUTPUT)
        )

    def test_ignores_other_sections(self) -> None:
        self.assertEqual(parse_dwc3_endpoints("=== Other ===\n  dwc3:\n"), {})


class FindStuckEndpointsTest(unittest.TestCase):
    """Tests `find_stuck_endpoints`."""

    def test_no_endpoints(self) -> None:
        self.assertEqual(
            find_stuck_endpoints({}), ["usb-cli diag shows no DWC3 endpoints"]
        )

    def test_healthy_endpoints_and_ep0_stall(self) -> None:
        # EP0 stalls by design, and pending TRBs after completed ones are fine.
        self.assertEqual(
            find_stuck_endpoints(parse_dwc3_endpoints(_DIAG_OUTPUT)), []
        )

    def test_stalled_endpoint(self) -> None:
        endpoints = {"endpoint-0x02": {"enabled": True, "stalled": True}}
        self.assertEqual(
            find_stuck_endpoints(endpoints), ["endpoint-0x02 is stalled"]
        )

    def test_pending_trb_before_completed_trb(self) -> None:
        endpoints = {
            "endpoint-0x02": {
                "enabled": True,
                "trb_fifo": {
                    "active_trbs": {
                        "0": {"hardware_owned": True},
                        "1": {"hardware_owned": False},
                    }
                },
            }
        }
        self.assertEqual(
            find_stuck_endpoints(endpoints),
            ["endpoint-0x02 has TRB HWO bits [1, 0]"],
        )

    def test_disabled_endpoint_is_ignored(self) -> None:
        endpoints = {"endpoint-0x04": {"enabled": False, "stalled": True}}
        self.assertEqual(find_stuck_endpoints(endpoints), [])


class FindDiagProblemsTest(unittest.TestCase):
    """Tests `find_diag_problems`."""

    def test_healthy_output(self) -> None:
        self.assertEqual(find_diag_problems(_DIAG_OUTPUT), [])

    def test_reports_dashboard_errors(self) -> None:
        output = _DIAG_OUTPUT.replace(
            "  [OK]          ADB:            Configured",
            "  [ERROR]       ADB:            Endpoint error",
        )
        self.assertEqual(
            find_diag_problems(output),
            ["[ERROR]       ADB:            Endpoint error"],
        )

    def test_ignores_tags_after_dashboard(self) -> None:
        # Inspect data after the dashboard can contain any text.
        output = _DIAG_OUTPUT.replace(
            "        total_bytes: 64\n",
            "        total_bytes: 64\n        last_state: [DISCONN]\n",
        )
        self.assertEqual(find_diag_problems(output), [])


class EndpointSummaryTest(unittest.TestCase):
    """Tests `get_endpoint_types` and `get_endpoint_bytes`."""

    def test_endpoint_types_skip_ep0_and_disabled(self) -> None:
        endpoints = parse_dwc3_endpoints(_DIAG_OUTPUT)
        self.assertEqual(
            get_endpoint_types(endpoints),
            {"endpoint-0x02": "Bulk", "endpoint-0x03": "Interrupt"},
        )

    def test_endpoint_bytes(self) -> None:
        endpoints = parse_dwc3_endpoints(_DIAG_OUTPUT)
        self.assertEqual(
            get_endpoint_bytes(endpoints),
            {"endpoint-0x02": 5242880, "endpoint-0x03": 64},
        )


class RunUsbCliDiagTest(unittest.TestCase):
    """Tests `run_usb_cli_diag`."""

    def test_ssh_preferred(self) -> None:
        dut = mock.MagicMock()
        dut.ffx.run.return_value = _DIAG_OUTPUT
        self.assertEqual(run_usb_cli_diag(dut, over_ssh=True), _DIAG_OUTPUT)
        dut.serial.send_and_recv.assert_not_called()

    def test_ssh_failure_falls_back_to_serial(self) -> None:
        dut = mock.MagicMock()
        dut.ffx.run.side_effect = honeydew_errors.HoneydewError("ssh down")
        dut.serial.send_and_recv.return_value = _DIAG_OUTPUT
        self.assertEqual(run_usb_cli_diag(dut, over_ssh=True), _DIAG_OUTPUT)
        dut.serial.send_and_recv.assert_called_once()

    def test_serial_only(self) -> None:
        dut = mock.MagicMock()
        dut.serial.send_and_recv.return_value = _DIAG_OUTPUT
        run_usb_cli_diag(dut, over_ssh=False)
        dut.ffx.run.assert_not_called()

    def test_ssh_failure_without_serial_reports_ssh_error(self) -> None:
        dut = mock.MagicMock()
        ssh_error = honeydew_errors.HoneydewError("ssh down")
        dut.ffx.run.side_effect = ssh_error
        type(dut).serial = mock.PropertyMock(
            side_effect=honeydew_errors.FuchsiaDeviceError("no serial_socket")
        )
        with self.assertRaises(honeydew_errors.HoneydewError) as ctx:
            run_usb_cli_diag(dut, over_ssh=True)
        self.assertIs(ctx.exception, ssh_error)

    def test_both_transports_fail(self) -> None:
        dut = mock.MagicMock()
        ssh_error = honeydew_errors.HoneydewError("ssh down")
        dut.ffx.run.side_effect = ssh_error
        dut.serial.send_and_recv.side_effect = honeydew_errors.HoneydewError(
            "serial down"
        )
        with self.assertRaisesRegex(
            honeydew_errors.HoneydewError, "serial"
        ) as ctx:
            run_usb_cli_diag(dut, over_ssh=True)
        self.assertIs(ctx.exception.__cause__, ssh_error)


class FindDutDiagProblemsTest(unittest.TestCase):
    """Tests `find_dut_diag_problems`."""

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_transient_problem_clears_on_retry(self, _mock_time: Any) -> None:
        transient = _diag_with_ep2_ring(
            read_offset=9, total_transfers=50, hwo_bits=[False, True, False]
        )
        dut = mock.MagicMock()
        dut.ffx.run.side_effect = [transient, _DIAG_OUTPUT]
        problems, output = find_dut_diag_problems(dut, over_ssh=True)
        self.assertEqual(problems, [])
        self.assertEqual(output, _DIAG_OUTPUT)
        self.assertEqual(dut.ffx.run.call_count, 2)

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_serial_path_and_custom_timeout(self, _mock_time: Any) -> None:
        dut = mock.MagicMock()
        dut.serial.send_and_recv.return_value = _DIAG_OUTPUT
        problems, _ = find_dut_diag_problems(
            dut, over_ssh=False, timeout_sec=12.5
        )
        self.assertEqual(problems, [])
        dut.ffx.run.assert_not_called()
        self.assertEqual(
            dut.serial.send_and_recv.call_args.kwargs["timeout_sec"], 12.5
        )

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_persistent_stalled_endpoint_is_reported(
        self, _mock_time: Any
    ) -> None:
        stalled = _DIAG_OUTPUT.replace(
            "        stalled: false", "        stalled: true"
        )
        dut = mock.MagicMock()
        dut.ffx.run.return_value = stalled
        problems, _ = find_dut_diag_problems(dut, over_ssh=True)
        self.assertEqual(problems, ["endpoint-0x02 is stalled"])
        self.assertEqual(dut.ffx.run.call_count, 2)

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_wedged_trb_slot_is_reported_when_leading_trb_drains(
        self, _mock_time: Any
    ) -> None:
        # Read 1: read_offset=9, HWO=[0, 1, 0] -> blocking HWO=1 is at slot 10.
        # Read 2: TRB at slot 9 drained, so read_offset=10, HWO=[1, 0],
        # total_transfers=51 -> blocking HWO=1 is still wedged at slot 10.
        read1 = _diag_with_ep2_ring(
            read_offset=9, total_transfers=50, hwo_bits=[False, True, False]
        )
        read2 = _diag_with_ep2_ring(
            read_offset=10, total_transfers=51, hwo_bits=[True, False]
        )
        dut = mock.MagicMock()
        dut.ffx.run.side_effect = [read1, read2]
        problems, _ = find_dut_diag_problems(dut, over_ssh=True)
        self.assertEqual(problems, ["endpoint-0x02 has TRB HWO bits [1, 0]"])
        self.assertEqual(dut.ffx.run.call_count, 2)

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_advancing_trb_ring_is_not_reported_as_stuck(
        self, _mock_time: Any
    ) -> None:
        # Each read catches a different in-flight chained short-packet TD as
        # the ring advances from slot 10 -> 18 -> 26.
        reads = [
            _diag_with_ep2_ring(
                read_offset=9, total_transfers=50, hwo_bits=[False, True, False]
            ),
            _diag_with_ep2_ring(
                read_offset=17,
                total_transfers=54,
                hwo_bits=[False, True, False],
            ),
            _diag_with_ep2_ring(
                read_offset=25,
                total_transfers=58,
                hwo_bits=[False, True, False],
            ),
        ]
        dut = mock.MagicMock()
        dut.ffx.run.side_effect = reads
        problems, output = find_dut_diag_problems(dut, over_ssh=True)
        self.assertEqual(problems, [])
        self.assertEqual(output, reads[-1])
        self.assertEqual(dut.ffx.run.call_count, 3)

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_ring_that_advances_then_wedges_is_reported(
        self, _mock_time: Any
    ) -> None:
        # Slot 10 retires, but slot 18 stays wedged between read 2 and read 3.
        reads = [
            _diag_with_ep2_ring(
                read_offset=9, total_transfers=50, hwo_bits=[False, True, False]
            ),
            _diag_with_ep2_ring(
                read_offset=17,
                total_transfers=54,
                hwo_bits=[False, True, False],
            ),
            _diag_with_ep2_ring(
                read_offset=18, total_transfers=55, hwo_bits=[True, False]
            ),
        ]
        dut = mock.MagicMock()
        dut.ffx.run.side_effect = reads
        problems, _ = find_dut_diag_problems(dut, over_ssh=True)
        self.assertEqual(problems, ["endpoint-0x02 has TRB HWO bits [1, 0]"])
        self.assertEqual(dut.ffx.run.call_count, 3)

    @mock.patch.object(usb_cli_diag, "time", autospec=True)
    def test_dropped_leading_trb_on_serial_does_not_shift_slot(
        self, _mock_time: Any
    ) -> None:
        # Read 1 drops TRB key `0:` on serial while slot 10 (`read_offset=9` +
        # key `1`) is wedged. Reads 2 and 3 have intact keys with `read_offset`
        # drained to 10, still wedged at slot 10.
        garbled_read1 = _diag_with_ep2_ring(
            read_offset=9, total_transfers=50, hwo_bits=[False, True, False]
        ).replace("            0:\n              hardware_owned: false\n", "")
        wedged_at_10 = _diag_with_ep2_ring(
            read_offset=10, total_transfers=51, hwo_bits=[True, False]
        )
        dut = mock.MagicMock()
        dut.serial.send_and_recv.side_effect = [
            garbled_read1,
            wedged_at_10,
            wedged_at_10,
        ]
        problems, _ = find_dut_diag_problems(dut, over_ssh=False)
        self.assertEqual(problems, ["endpoint-0x02 has TRB HWO bits [1, 0]"])
        self.assertEqual(dut.serial.send_and_recv.call_count, 3)


if __name__ == "__main__":
    unittest.main()
