# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for openwrt_access_point.lib.uci_commands."""

import unittest

from openwrt_access_point.lib.uci_commands import (
    build_custom_uci_commands,
    format_uci_value,
)


class UciCommandsTest(unittest.TestCase):
    """Tests 1:1 UCI command generation in uci_commands."""

    def test_format_uci_value(self) -> None:
        self.assertEqual(format_uci_value(True), "1")
        self.assertEqual(format_uci_value(False), "0")
        self.assertEqual(format_uci_value(100), "100")
        self.assertEqual(format_uci_value("needs-broadcast"), "needs-broadcast")

    def test_build_custom_uci_commands_empty(self) -> None:
        self.assertEqual(build_custom_uci_commands("dhcp", "lan", {}), [])
        self.assertEqual(
            build_custom_uci_commands("dhcp", "lan", {"dhcp_option_force": []}),
            [],
        )

    def test_build_custom_uci_commands_scalars_and_lists(self) -> None:
        commands = build_custom_uci_commands(
            "dhcp",
            "lan",
            {
                "ignore": False,
                "force": True,
                "leasetime": "120s",
                "start": 100,
                "dhcp_option_force": ["58,3", "59,6"],
            },
        )
        self.assertEqual(
            commands,
            [
                "uci set dhcp.lan.ignore='0'",
                "uci set dhcp.lan.force='1'",
                "uci set dhcp.lan.leasetime='120s'",
                "uci set dhcp.lan.start='100'",
                "uci add_list dhcp.lan.dhcp_option_force='58,3'",
                "uci add_list dhcp.lan.dhcp_option_force='59,6'",
            ],
        )

    def test_build_custom_uci_commands_wireless_and_selector_sections(
        self,
    ) -> None:
        self.assertEqual(
            build_custom_uci_commands("dhcp", "@dnsmasq[0]", {"noping": True}),
            ["uci set dhcp.@dnsmasq[0].noping='1'"],
        )
        self.assertEqual(
            build_custom_uci_commands(
                "wireless",
                "radio0",
                {"noscan": True, "supported_rates": [6000, 12000]},
            ),
            [
                "uci set wireless.radio0.noscan='1'",
                "uci add_list wireless.radio0.supported_rates='6000'",
                "uci add_list wireless.radio0.supported_rates='12000'",
            ],
        )


if __name__ == "__main__":
    unittest.main()
