# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for openwrt_access_point.lib.dhcp_controller."""

import unittest
from unittest import mock

from openwrt_access_point import DhcpConfig, Dnsmasq, Lan
from openwrt_access_point.lib.dhcp_controller import DhcpController
from openwrt_access_point.lib.uci_dhcp_options import UciSection


class DhcpControllerTest(unittest.TestCase):
    """Tests batched UCI command generation in DhcpController.start_dhcp."""

    def test_start_dhcp_defaults(self) -> None:
        ssh = mock.MagicMock()
        controller = DhcpController(ssh)

        controller.start_dhcp()

        self.assertEqual(ssh.run.call_count, 3)
        self.assertEqual(
            ssh.run.call_args_list[0],
            mock.call(
                "cmp -s /rom/etc/config/dhcp /etc/config/dhcp && pidof dnsmasq >/dev/null "
                "|| (cp /rom/etc/config/dhcp /etc/config/dhcp && /etc/init.d/dnsmasq restart)"
            ),
        )
        self.assertEqual(
            ssh.run.call_args_list[1],
            mock.call(
                " && ".join(
                    [
                        "uci set dhcp.lan.dynamicdhcp='1'",
                        "uci set dhcp.lan.leasetime='12h'",
                        "uci commit dhcp",
                        "/etc/init.d/dnsmasq restart",
                    ]
                )
            ),
        )
        self.assertEqual(
            ssh.run.call_args_list[2],
            mock.call("logger 'dnsmasq-dhcp: TEST_START'"),
        )

    def test_start_dhcp_with_custom_options_and_sections(self) -> None:
        ssh = mock.MagicMock()
        controller = DhcpController(ssh)

        controller.start_dhcp(
            config=DhcpConfig(
                lan=Lan(
                    dynamic_dhcp=False,
                    lease_time="120s",
                    start=100,
                    limit=5,
                    custom_uci_options={"dhcp_option_force": ["58,3"]},
                ),
                dnsmasq=Dnsmasq(custom_uci_options={"noping": True}),
                custom_uci_sections={
                    "broadcast_all": UciSection(
                        section_type="mac",
                        options={
                            "networkid": "needs-broadcast",
                            "mac": "*:*:*:*:*:*",
                        },
                    ),
                },
            )
        )

        self.assertEqual(ssh.run.call_count, 3)
        self.assertEqual(
            ssh.run.call_args_list[1],
            mock.call(
                " && ".join(
                    [
                        "uci set dhcp.lan.dynamicdhcp='0'",
                        "uci set dhcp.lan.leasetime='120s'",
                        "uci set dhcp.lan.start='100'",
                        "uci set dhcp.lan.limit='5'",
                        "uci add_list dhcp.lan.dhcp_option_force='58,3'",
                        "uci set dhcp.@dnsmasq[0].noping='1'",
                        "uci set dhcp.broadcast_all='mac'",
                        "uci set dhcp.broadcast_all.networkid='needs-broadcast'",
                        "uci set dhcp.broadcast_all.mac='*:*:*:*:*:*'",
                        "uci commit dhcp",
                        "/etc/init.d/dnsmasq restart",
                    ]
                )
            ),
        )


if __name__ == "__main__":
    unittest.main()
