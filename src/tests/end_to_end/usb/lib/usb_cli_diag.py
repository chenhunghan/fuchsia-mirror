# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Parses the output of `usb-cli diag` on a DUT.

`usb-cli diag` prints a health dashboard for each USB function, then the
device-side USB Inspect data, including the DWC3 endpoints and their TRBs.
These helpers find health errors and stuck DWC3 endpoints in that output.
"""

import re
from typing import Any

from .usb_config import KERNEL_LOG_RE

# What `usb-cli` prints last when it succeeds, and when it fails.
USB_CLI_END_TOKENS = ("[usb-cli:DONE]", "usb-cli error:")
# `usb-cli diag` prints the health dashboard, then this, then Inspect data.
_DIAG_INSPECT_HEADER = "=== Device-Side USB Inspect Diagnostics ==="
# Dashboard status tags for a disconnected, unconfigured, or broken device.
_BAD_HEALTH_TAGS = (
    "[ERROR]",
    "[DISCONN]",
    "[ENUMERATING]",
    "[ADDRESS]",
    "[SUSPENDED]",
    "[UNKNOWN]",
)
# EP0 OUT and IN. Control endpoints stall by design to reject requests.
_DWC3_EP0 = ("endpoint-0x00", "endpoint-0x01")
# How `usb-cli diag` prints a DWC3 endpoint type, e.g. "2 (Bulk)".
_ENDPOINT_TYPE_RE = re.compile(r"\d+ \((\w+)\)")


def _get(node: Any, *names: str) -> dict[str, Any]:
    """Returns `node[names[0]][names[1]]...` if that is a dict, else {}."""
    for name in names:
        node = node.get(name) if isinstance(node, dict) else None
    return node if isinstance(node, dict) else {}


def parse_dwc3_endpoints(output: str) -> dict[str, Any]:
    """Returns the DWC3 `endpoints` Inspect node from `usb-cli diag` output.

    `usb-cli` prints an Inspect node as "name:", then its properties as
    "name: value", then its children, indenting each level by two spaces:

        === Synopsys DWC3 Controller Diagnostics ===
          dwc3:
            endpoints:
              endpoint-0x02:
                enabled: true
                type: 2 (Bulk)
                trb_fifo:
                  active_trbs:
                    0:
                      hardware_owned: true

    This returns nested dicts, as in Inspect's JSON:
    {"endpoint-0x02": {"enabled": True, "type": "2 (Bulk)", ...}}.
    """
    root: dict[str, Any] = {}
    # The nodes from the root down to the current one, with their indents.
    stack: list[tuple[int, dict[str, Any]]] = []
    for line in KERNEL_LOG_RE.sub("", output).splitlines():
        if line.lstrip().startswith("==="):
            # A section header. Only read the DWC3 section.
            stack = [(-1, root)] if "Synopsys DWC3" in line else []
            continue
        name, colon, value = line.strip().partition(":")
        if not stack or not colon:
            continue
        indent = len(line) - len(line.lstrip())
        while stack[-1][0] >= indent:
            stack.pop()
        node = stack[-1][1]
        value = value.strip()
        if value:
            node[name] = {"true": True, "false": False}.get(value, value)
        else:
            node[name] = {}
            stack.append((indent, node[name]))
    return _get(root, "dwc3", "endpoints")


def find_diag_problems(output: str) -> list[str]:
    """Returns the health errors and stuck endpoints in `usb-cli diag`."""
    dashboard = KERNEL_LOG_RE.sub("", output).split(_DIAG_INSPECT_HEADER)[0]
    # E.g. "  [ERROR]       VSOCK:          Endpoint error".
    errors = [
        line.strip()
        for line in dashboard.splitlines()
        if any(tag in line for tag in _BAD_HEALTH_TAGS)
    ]
    return errors + find_stuck_endpoints(parse_dwc3_endpoints(output))


def find_stuck_endpoints(endpoints: dict[str, Any]) -> list[str]:
    """Returns which of the DWC3 `endpoints` look stuck.

    An enabled endpoint is stuck if it is stalled, or if a TRB that the
    hardware still owns comes before one that it has completed. The hardware
    completes TRBs in order, so that older TRB will never complete.
    """
    if not endpoints:
        return ["usb-cli diag shows no DWC3 endpoints"]
    problems: list[str] = []
    for name in sorted(endpoints):
        endpoint = _get(endpoints, name)
        if name in _DWC3_EP0 or endpoint.get("enabled") is not True:
            continue
        if endpoint.get("stalled") is True:
            problems.append(f"{name} is stalled")
        trbs = _get(endpoint, "trb_fifo", "active_trbs")
        hwo: list[int] = []
        # Oldest first. Skip any TRB that the serial console garbled.
        for i in sorted((i for i in trbs if i.isdigit()), key=int):
            owned = _get(trbs, i).get("hardware_owned")
            if isinstance(owned, bool):
                hwo.append(int(owned))
        # Expect completed TRBs (0) before pending ones (1).
        if hwo != sorted(hwo):
            problems.append(f"{name} has TRB HWO bits {hwo}")
    return problems


def get_endpoint_types(endpoints: dict[str, Any]) -> dict[str, str]:
    """Returns the type of each enabled DWC3 endpoint but EP0, e.g. "Bulk"."""
    types: dict[str, str] = {}
    for name in sorted(endpoints):
        endpoint = _get(endpoints, name)
        match = _ENDPOINT_TYPE_RE.fullmatch(str(endpoint.get("type")))
        if name not in _DWC3_EP0 and endpoint.get("enabled") is True and match:
            types[name] = match.group(1)
    return types


def get_endpoint_bytes(endpoints: dict[str, Any]) -> dict[str, int]:
    """Returns how many bytes each DWC3 endpoint has carried since boot."""
    total: dict[str, int] = {}
    for name in endpoints:
        value = str(_get(endpoints, name).get("total_bytes"))
        if value.isdigit():
            total[name] = int(value)
    return total
