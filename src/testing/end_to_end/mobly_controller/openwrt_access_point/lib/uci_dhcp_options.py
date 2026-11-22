# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""UCI option and section types for OpenWrt DHCP and dnsmasq configuration."""

import dataclasses
from collections.abc import Mapping
from typing import TypedDict


class UciDhcpOptions(TypedDict, total=False):
    """Typed dictionary for OpenWrt UCI `config dhcp` section options."""

    dhcp_option_force: list[str]
    """Unconditional DHCP options sent regardless of client request (`uci add_list dhcp.<section>.dhcp_option_force`)."""


class UciDnsmasqOptions(TypedDict, total=False):
    """Typed dictionary for OpenWrt UCI `config dnsmasq` section options."""

    noping: bool
    """Disable ICMP ping check before assigning an IP address (`--no-ping`)."""


@dataclasses.dataclass
class UciSection:
    """An additional named OpenWrt UCI section to configure.

    Attributes:
        section_type: The UCI section type (e.g., `"mac"`, `"host"`).
        options: Mapping of UCI option names to scalar or list values on the
            section.
    """

    section_type: str
    options: Mapping[str, object] = dataclasses.field(default_factory=dict)
