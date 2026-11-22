# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Runs `usb-cli diag` on a DUT and parses what it prints.

`usb-cli diag` prints a health dashboard for each USB function, then the
device-side USB Inspect data, including the DWC3 endpoints and their TRBs.
These helpers find health errors and stuck DWC3 endpoints in that output.
"""

import logging
import re
import time
from typing import Any

from honeydew import errors as honeydew_errors
from honeydew.transports.ffx.types import MachineFormat

from .usb_config import KERNEL_LOG_RE

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Timeout of every `usb-cli diag` command.
DIAG_TIMEOUT_SEC: float = 60.0
# Maximum `usb-cli diag` reads when checking whether an out-of-order TRB ring
# is actively retiring transfers or wedged on the same TRB slot.
_MAX_DIAG_ATTEMPTS = 3
_DIAG_RETRY_DELAY_SEC = 1.0

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


def _get_hwo_bits(endpoint: dict[str, Any]) -> list[int]:
    """Returns the `hardware_owned` bits (0 or 1) of active TRBs."""
    trbs = _get(endpoint, "trb_fifo", "active_trbs")
    hwo: list[int] = []
    # Oldest first. Skip any TRB that the serial console garbled.
    for i in sorted((i for i in trbs if i.isdigit()), key=int):
        owned = _get(trbs, i).get("hardware_owned")
        if isinstance(owned, bool):
            hwo.append(int(owned))
    return hwo


def _first_blocking_hwo_offset(endpoint: dict[str, Any]) -> int | None:
    """Returns the offset from `read_offset` of the first `HWO=1` TRB.

    Returns `None` if an earlier TRB index in `active_trbs` was dropped or
    garbled on serial, since its `hardware_owned` bit is unknown.
    """
    trbs = _get(endpoint, "trb_fifo", "active_trbs")
    for idx in range(len(trbs)):
        owned = _get(trbs, str(idx)).get("hardware_owned")
        if not isinstance(owned, bool):
            return None
        if owned:
            return idx
    return None


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
        hwo = _get_hwo_bits(endpoint)
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


def run_usb_cli_diag(
    dut: Any, *, over_ssh: bool, timeout_sec: float = DIAG_TIMEOUT_SEC
) -> str:
    """Runs `usb-cli diag` over SSH if `over_ssh`, else over serial.

    Falls back to serial if SSH fails, e.g. because a CDC endpoint is stuck.
    Serial doesn't use USB.

    Raises:
        honeydew.errors.HoneydewError: The command failed on every transport
            that was tried.
    """
    ssh_error: honeydew_errors.HoneydewError | None = None
    if over_ssh:
        try:
            # SSH is faster, and kernel logs don't get mixed in.
            return dut.ffx.run(
                ["target", "ssh", "usb-cli diag"],
                timeout=timeout_sec,
                log_output=False,
                machine=MachineFormat.RAW,
                disable_controlmaster=True,
            )
        except honeydew_errors.HoneydewError as e:
            _LOGGER.warning(
                "usb-cli diag over SSH failed, so using serial: %s", e
            )
            ssh_error = e
    try:
        serial = dut.serial
    except honeydew_errors.HoneydewError as e:
        # The testbed has no serial console. Report the SSH failure, which
        # is the real problem, rather than the missing fallback.
        if ssh_error is not None:
            raise ssh_error from e
        raise
    try:
        return serial.send_and_recv(
            cmd="usb-cli diag",
            stop_tokens=USB_CLI_END_TOKENS,
            timeout_sec=timeout_sec,
            log_output=False,
        )
    except honeydew_errors.HoneydewError as e:
        if ssh_error is not None:
            raise e from ssh_error
        raise


def _blocking_hwo_slots(
    endpoints: dict[str, Any],
) -> dict[str, tuple[str, tuple[int, int, int] | None]]:
    """Maps each endpoint with unsorted HWO bits to `(problem, slot_state)`.

    Scatter-gather OUT requests chain TRBs with `TRB_CHN | TRB_CSP` in
    `Dwc3::QueueNextRequest`. On a short packet, DWC3 hardware clears `HWO` on
    the short-packet TRB, skips the remaining chained TRBs in that request
    without clearing their `HWO` bits, and moves to the next request. If
    hardware completes a TRB there before `Dwc3::UserEpCompleteTransfers` runs
    on the driver dispatcher and calls `TrbFifo::SkipTrbs`, `active_trbs`
    momentarily shows `[0, 1, 0]` on a healthy ring.

    `UserEpCompleteTransfers` drains TRBs from `read_offset` in order and stops
    at the first `HWO=1` TRB (`offset` from `_first_blocking_hwo_offset`). Even
    if the `offset` completed (`HWO=0`) TRBs ahead of it are drained,
    `read_offset` cannot advance past `slot = (read_offset + offset) %
    total_slots` and `total_transfers` cannot grow by more than `offset` until
    that `HWO=1` TRB is retired.

    `slot_state` is `(slot, total_transfers, max_transfers_if_unretired)` when
    `trb_fifo` has `read_offset`, `total_slots`, and an ungarbled prefix up to
    the first `HWO=1` TRB, or `None` otherwise.
    """
    result: dict[str, tuple[str, tuple[int, int, int] | None]] = {}
    for name in sorted(endpoints):
        endpoint = _get(endpoints, name)
        if name in _DWC3_EP0 or endpoint.get("enabled") is not True:
            continue
        hwo = _get_hwo_bits(endpoint)
        if hwo == sorted(hwo):
            continue
        problem = f"{name} has TRB HWO bits {hwo}"
        fifo = _get(endpoint, "trb_fifo")
        read_offset = str(fifo.get("read_offset"))
        total_slots = str(fifo.get("total_slots"))
        if not (read_offset.isdigit() and total_slots.isdigit()):
            result[name] = (problem, None)
            continue
        slots = int(total_slots)
        if slots <= 0:
            result[name] = (problem, None)
            continue
        first_hwo1 = _first_blocking_hwo_offset(endpoint)
        if first_hwo1 is None:
            result[name] = (problem, None)
            continue
        slot = (int(read_offset) + first_hwo1) % slots
        transfers_str = str(endpoint.get("total_transfers"))
        transfers = int(transfers_str) if transfers_str.isdigit() else -1
        max_unretired = transfers + first_hwo1 if transfers >= 0 else -1
        result[name] = (problem, (slot, transfers, max_unretired))
    return result


def _did_hwo_slot_advance(
    prev: tuple[int, int, int] | None,
    curr: tuple[int, int, int] | None,
) -> bool:
    """Returns True if the previous blocking `HWO=1` TRB slot was retired."""
    if prev is None or curr is None:
        return False
    prev_slot, _, prev_max_unretired = prev
    curr_slot, curr_transfers, _ = curr
    return curr_slot != prev_slot or (
        prev_max_unretired >= 0 and curr_transfers > prev_max_unretired
    )


def find_dut_diag_problems(
    dut: Any, *, over_ssh: bool, timeout_sec: float = DIAG_TIMEOUT_SEC
) -> tuple[list[str], str]:
    """Runs `usb-cli diag`, and returns the problems it shows and its output.

    A single read can catch a scatter-gather OUT short packet before
    `Dwc3::UserEpCompleteTransfers` calls `TrbFifo::SkipTrbs`, producing a
    transient `[0, 1, 0]` in `active_trbs`. When the first read shows problems,
    this re-reads `usb-cli diag` after `_DIAG_RETRY_DELAY_SEC`. On retries, if
    the only problems are unsorted TRB HWO bits whose blocking `HWO=1` ring
    slot advanced since the previous read, the ring is actively retiring TRBs
    rather than wedged.
    """
    output = ""
    problems: list[str] = []
    prev_blocking: dict[str, tuple[str, tuple[int, int, int] | None]] = {}
    for attempt in range(1, _MAX_DIAG_ATTEMPTS + 1):
        if attempt > 1:
            time.sleep(_DIAG_RETRY_DELAY_SEC)
        output = run_usb_cli_diag(
            dut, over_ssh=over_ssh, timeout_sec=timeout_sec
        )
        problems = find_diag_problems(output)
        if not problems:
            return [], output
        curr_blocking = _blocking_hwo_slots(parse_dwc3_endpoints(output))
        if attempt == 1:
            prev_blocking = curr_blocking
            continue
        advancing_problems = {
            problem
            for name, (problem, curr_state) in curr_blocking.items()
            if (
                name in prev_blocking
                and _did_hwo_slot_advance(prev_blocking[name][1], curr_state)
            )
            or (
                attempt < _MAX_DIAG_ATTEMPTS
                and (
                    name not in prev_blocking or prev_blocking[name][1] is None
                )
                and curr_state is not None
            )
        }
        non_advancing = [p for p in problems if p not in advancing_problems]
        if non_advancing:
            problems = non_advancing
            break
        prev_blocking = curr_blocking
    else:
        problems = []
    if problems:
        _LOGGER.warning("usb-cli diag output: %s", output)
    return problems, output
