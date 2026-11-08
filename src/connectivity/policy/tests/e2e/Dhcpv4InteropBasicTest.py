#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import logging
import re
from datetime import timedelta

import dhcp_testing
import fuchsia_wlan_base_test
from honeydew.utils.deadline import Deadline
from legacy_access_point.ap_lib import dhcp_config
from mobly import asserts, test_runner
from openwrt_access_point.lib.dhcp_config import DhcpConfig, Lan

_LOGGER = logging.getLogger(__name__)


class Dhcpv4InteropBasicTest(fuchsia_wlan_base_test.FuchsiaWlanBaseTest):
    """DhcpV4 tests which validate basic DHCP client/server interactions.

    Wire-level DHCPv4 option parsing, malformed/unknown option handling (such as
    Domain Name Option 15 and Domain Search Option 119, which `netcfg` and
    `dhcpv4_client` do not request or read), and maximum unfragmented message
    size handling are unit-tested hermetically in
    `//src/connectivity/network/dhcpv4/client/core/src/parse.rs`.
    """

    async def setup_class(self) -> None:
        await super().setup_class()
        self.dhcp = dhcp_testing.DhcpHelper(
            dut=self.dut,
            openwrt_ap=self.openwrt_ap,
            access_point=self.access_point,
            log_path=self.log_path,
        )

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.dut.wlan_policy.ensure_clean_state()

    async def teardown_test(self) -> None:
        await self.dut.wlan_policy.ensure_clean_state()
        if self.access_point:
            self.access_point.stop_all_aps()
        await super().teardown_test()

    async def test_basic_dhcp_assignment(self) -> None:
        await self.dhcp.run_test_case_expect_dhcp_success()

    async def test_pool_disallows_unknown_clients(self) -> None:
        ap_params = self.dhcp.setup_ap()

        if self.openwrt_ap:
            self.openwrt_ap.dhcp.start_dhcp(
                config=DhcpConfig(lan=Lan(dynamic_dhcp=False))
            )
        elif self.access_point:
            subnet_conf = dhcp_config.Subnet(
                subnet=ap_params.network,
                router=ap_params.ip,
                additional_parameters={"deny": "unknown-clients"},
            )
            dhcp_conf = dhcp_config.DhcpConfig(subnets=[subnet_conf])
            self.access_point.start_dhcp(dhcp_conf=dhcp_conf)

        security = ap_params.security.to_fidl_wlan_policy()
        await self.dut.wlan_policy.save_network(
            ap_params.ssid, security, ap_params.password
        )
        await self.dut.wlan_policy.connect(ap_params.ssid, security)
        with asserts.assert_raises(ConnectionError):
            await self.dhcp.get_device_ipv4_addr()

        dhcp_logs = self.dhcp.get_dhcp_logs()

        pattern = ""
        if self.openwrt_ap:
            # dnsmasq logs "no address available" when dynamic DHCP is disabled and the client is unknown.
            pattern = r"DHCPDISCOVER.*no address available"
        elif self.access_point:
            # ISC DHCPD logs "no free leases" when it cannot offer a lease due to "deny unknown-clients".
            pattern = r"DHCPDISCOVER.*no free leases"

        asserts.assert_true(
            re.search(pattern, dhcp_logs),
            f"Did not find expected message in dhcp logs: {dhcp_logs}\n",
        )

    async def test_lease_renewal(self) -> None:
        """Validates that a client renews their DHCP lease."""
        ap_params = self.dhcp.setup_ap()
        LEASE_TIME = 30
        RENEWAL_TIME_SECS = 3
        OPT_NUM_RENEWAL_TIME = 58
        if self.openwrt_ap:
            # OpenWrt dnsmasq enforces a 2m minimum lease time and a >2s
            # minimum renewal time; force an early 3s renewal time by sending
            # the DHCP renewal time option (58) directly.
            LEASE_TIME = 120
            self.openwrt_ap.dhcp.start_dhcp(
                config=DhcpConfig(
                    lan=Lan(
                        lease_time=f"{LEASE_TIME}s",
                        dhcp_options_force=[
                            f"{OPT_NUM_RENEWAL_TIME},{RENEWAL_TIME_SECS}"
                        ],
                    )
                )
            )
        elif self.access_point:
            # Keep default requested options (1, 3, 6) when forcing Option 58.
            subnet_conf = dhcp_config.Subnet(
                subnet=ap_params.network,
                router=ap_params.ip,
                additional_options={
                    "dhcp-renewal-time": RENEWAL_TIME_SECS,
                    "dhcp-parameter-request-list": (
                        f"1, 3, 6, {OPT_NUM_RENEWAL_TIME}"
                    ),
                },
            )
            dhcp_conf = dhcp_config.DhcpConfig(
                subnets=[subnet_conf],
                default_lease_time=LEASE_TIME,
                max_lease_time=LEASE_TIME,
            )
            self.access_point.start_dhcp(dhcp_conf=dhcp_conf)

        security = ap_params.security.to_fidl_wlan_policy()
        await self.dut.wlan_policy.save_network(
            ap_params.ssid, security, ap_params.password
        )
        await self.dut.wlan_policy.connect(ap_params.ssid, security)
        ip = await self.dhcp.get_device_ipv4_addr()

        def count_requests_and_acks(logs: str) -> tuple[int, int]:
            # Example log lines:
            # DHCPREQUEST for 192.168.9.2 from 01:23:45:67:89:ab via wlan1
            # DHCPACK on 192.168.9.2 to 01:23:45:67:89:ab via wlan1
            escaped_ip = re.escape(str(ip))
            requests = len(re.findall(rf"DHCPREQUEST.*{escaped_ip}", logs))
            acks = len(re.findall(rf"DHCPACK.*{escaped_ip}", logs))
            return requests, acks

        # Snapshot post-bind counts to ignore initial REQUESTING retransmits.
        baseline_logs = self.dhcp.get_dhcp_logs()
        baseline_requests, baseline_acks = count_requests_and_acks(
            baseline_logs
        )

        # Poll until a post-bind renewal DHCPREQUEST + DHCPACK arrives (~2s).
        deadline = Deadline.from_timeout(timedelta(seconds=LEASE_TIME + 3))
        while True:
            dhcp_logs = self.dhcp.get_dhcp_logs()
            request_matches, ack_matches = count_requests_and_acks(dhcp_logs)
            if (
                request_matches > baseline_requests
                and ack_matches > baseline_acks
            ) or deadline.is_due():
                break
            await asyncio.sleep(1)

        asserts.assert_greater(
            request_matches,
            baseline_requests,
            f"Expected DHCP renewal DHCPREQUEST after binding ({baseline_requests=}): {dhcp_logs}\n",
        )
        asserts.assert_greater(
            ack_matches,
            baseline_acks,
            f"Expected DHCP renewal DHCPACK after binding ({baseline_acks=}): {dhcp_logs}\n",
        )

    async def test_no_dhcp_server_started(self) -> None:
        """Validates that DUT fails to get an address when no DHCP server is running."""
        ap_params = self.dhcp.setup_ap()
        if self.access_point:
            # Allow Whirlwind AP radio to finish initializing and broadcasting beacons
            await asyncio.sleep(5)
        security = ap_params.security.to_fidl_wlan_policy()
        await self.dut.wlan_policy.save_network(
            ap_params.ssid, security, ap_params.password
        )
        await self.dut.wlan_policy.connect(ap_params.ssid, security)
        with asserts.assert_raises(ConnectionError):
            await self.dhcp.get_device_ipv4_addr()


if __name__ == "__main__":
    test_runner.main()
