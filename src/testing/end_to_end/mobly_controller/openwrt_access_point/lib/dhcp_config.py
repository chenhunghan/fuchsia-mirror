# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""DHCP configuration dataclasses for OpenWrt Access Point."""

import dataclasses

from openwrt_access_point.lib.uci_dhcp_options import (
    UciDhcpOptions,
    UciDnsmasqOptions,
    UciSection,
)


@dataclasses.dataclass
class Lan:
    """UCI dhcp.lan configuration.

    Attributes:
        dynamic_dhcp: Whether to enable dynamic DHCP.
        lease_time: The lease time for IP addresses (e.g., '12h', '30m').
        start: The start offset for the DHCP pool.
        limit: The number of addresses in the DHCP pool.
        custom_uci_options: Escape hatch for `dhcp.lan` UCI options.
    """

    dynamic_dhcp: bool = True
    lease_time: str = "12h"
    start: int | None = None
    limit: int | None = None
    custom_uci_options: UciDhcpOptions = dataclasses.field(
        default_factory=lambda: UciDhcpOptions()
    )


@dataclasses.dataclass
class Dnsmasq:
    """UCI dhcp.dnsmasq configuration.

    Attributes:
        custom_uci_options: Escape hatch for `dhcp.@dnsmasq[0]` UCI options.
    """

    custom_uci_options: UciDnsmasqOptions = dataclasses.field(
        default_factory=lambda: UciDnsmasqOptions()
    )


@dataclasses.dataclass
class DhcpConfig:
    """DHCP configuration for OpenWrt.

    Attributes:
        lan: The LAN interface DHCP configuration.
        dnsmasq: The dnsmasq DHCP configuration.
        custom_uci_sections: Escape hatch for additional named UCI sections in
            `/etc/config/dhcp`, mapping section name to `UciSection`.
    """

    lan: Lan = dataclasses.field(default_factory=Lan)
    dnsmasq: Dnsmasq = dataclasses.field(default_factory=Dnsmasq)
    custom_uci_sections: dict[str, UciSection] = dataclasses.field(
        default_factory=dict
    )
