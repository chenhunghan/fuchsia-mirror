# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host unit tests for the `usb-cli diag` output parsers."""

import unittest

from usb_lib.usb_cli_diag import (
    find_diag_problems,
    find_stuck_endpoints,
    get_endpoint_bytes,
    get_endpoint_types,
    parse_dwc3_endpoints,
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
        type: 2 (Bulk)
        total_bytes: 5242880
        trb_fifo:
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


if __name__ == "__main__":
    unittest.main()
