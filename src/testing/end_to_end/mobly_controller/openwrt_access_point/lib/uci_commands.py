# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared helpers for building OpenWrt UCI shell commands."""

from collections.abc import Mapping


def format_uci_value(value: object) -> str:
    """Formats a Python value into an OpenWrt UCI string."""
    return str(int(value)) if isinstance(value, bool) else str(value)


def build_custom_uci_commands(
    config: str, section: str, options: Mapping[str, object]
) -> list[str]:
    """Builds UCI commands to apply custom UCI options to a section.

    Args:
        config: The UCI configuration file name (e.g., "wireless", "dhcp").
        section: The UCI section name or selector (e.g., "lan", "@dnsmasq[0]").
        options: Mapping of UCI option names to scalar or list values.

    Returns:
        A list of UCI commands setting or appending the custom options.
    """
    commands: list[str] = []
    for option, value in options.items():
        if isinstance(value, list):
            for item in value:
                v = format_uci_value(item)
                commands.append(
                    f"uci add_list {config}.{section}.{option}='{v}'"
                )
        else:
            v = format_uci_value(value)
            commands.append(f"uci set {config}.{section}.{option}='{v}'")
    return commands
