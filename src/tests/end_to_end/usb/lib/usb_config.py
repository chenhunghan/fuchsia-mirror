# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia USB peripheral configuration management library.

Provides utilities for querying, parsing, applying, and restoring USB peripheral
configurations on Fuchsia devices via Honeydew FFX transports and `usb-cli`,
as well as extracting target device identifiers directly from Honeydew device
objects.
"""

import asyncio
import collections.abc
import concurrent.futures
import inspect
import itertools
import json
import logging
import re
import shlex
import time
from typing import Any

from honeydew import errors as honeydew_errors
from honeydew.fuchsia_device import fuchsia_device

_LOGGER: logging.Logger = logging.getLogger(__name__)
_DONE_TOKEN: str = "[usb-cli:DONE]"
_ERROR_TOKEN: str = "[usb-cli:ERROR]"
_USB_CLI_ERR_PREFIX: str = "usb-cli error:"
_IDENT_RE: re.Pattern[str] = re.compile(r"^[a-zA-Z0-9_-]+$")
_ANSI_CSI_RE: re.Pattern[str] = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


class UsbConfigParseError(ValueError):
    """Raised when `usb-cli get-config` output cannot be parsed into valid JSON."""


GOOGLE_USB_VID: int = 0x18D1

# Expected USB PID for each subset of functions (e.g. `cdc,adb` and `adb,cdc`
# share 0xa026; build_config_matrix still tests every ordering).
FUNCTION_PIDS: dict[frozenset[str], int] = {
    frozenset({"cdc"}): 0xA020,
    frozenset({"adb"}): 0xA025,
    frozenset({"vsock"}): 0xA028,
    frozenset({"cdc", "adb"}): 0xA026,
    frozenset({"cdc", "vsock"}): 0xA029,
    frozenset({"adb", "vsock"}): 0xA02A,
    frozenset({"cdc", "adb", "vsock"}): 0xA02B,
}

# Class/subclass/protocol of each function's interfaces, as the host sees them.
FUNCTION_INTERFACES: dict[str, list[str]] = {
    "cdc": ["02/06/00", "0a/00/00"],  # Communication and data interfaces.
    "adb": ["ff/42/01"],
    "vsock": ["ff/43/00"],
}

# A kernel log line on the serial console, e.g.
# "[00024.593] 15009:35497> [driver, usb-phy] INFO: ...". The kernel can print
# one in the middle of a line of command output, so drop its newline too.
KERNEL_LOG_RE: re.Pattern[str] = re.compile(r"\[\d+\.\d+\] \d+:\d+>[^\n]*\n?")


def _strip_serial_noise(output: str) -> str:
    """Strip UART CRs, ANSI CSI escapes, usb-cli tokens, and kernel log lines."""
    cleaned = output.replace("\r\n", "\n").replace("\r", "")
    cleaned = _ANSI_CSI_RE.sub("", cleaned)
    cleaned = KERNEL_LOG_RE.sub("", cleaned)
    cleaned = cleaned.replace(_DONE_TOKEN, "").replace(_ERROR_TOKEN, "")
    return cleaned


def _is_valid_config_payload(value: Any) -> bool:
    """Return True if `value` matches the `usb-cli get-config` JSON schema."""
    if not isinstance(value, dict):
        return False
    if "configurations" in value:
        configs = value["configurations"]
        return isinstance(configs, list) and all(
            isinstance(cfg, list)
            and all(
                isinstance(fn, str) and bool(_IDENT_RE.match(fn)) for fn in cfg
            )
            for cfg in configs
        )
    if "functions" in value:
        funcs = value["functions"]
        return isinstance(funcs, list) and all(
            isinstance(fn, str) and bool(_IDENT_RE.match(fn)) for fn in funcs
        )
    return False


def build_config_matrix(functions: list[str]) -> list[list[str]]:
    """Returns every non-empty subset of `functions`, in every order."""
    return [
        list(p)
        for k in range(1, len(functions) + 1)  # Subset size: 1, 2, ..., n.
        for p in itertools.permutations(functions, k)  # Ordered picks of k.
    ]


def parse_get_config(output: str) -> dict[str, Any] | None:
    """Returns the JSON object that `usb-cli get-config` printed, if any.

    The serial console echoes commands, translates LF to CRLF, and interleaves
    kernel or Archivist log lines even inside JSON strings. Strip serial noise
    first, reject explicit `usb-cli` errors, and scan each `{` until one
    decodes to a valid configuration schema.
    """
    if _ERROR_TOKEN in output:
        return None
    cleaned = _strip_serial_noise(output)
    if _USB_CLI_ERR_PREFIX in cleaned:
        return None
    decoder = json.JSONDecoder()
    for start, char in enumerate(cleaned):
        if char != "{":
            continue
        try:
            value, _ = decoder.raw_decode(cleaned, start)
        except json.JSONDecodeError:
            continue
        if _is_valid_config_payload(value):
            return value
    return None


def _invoke_maybe_async(
    callable_obj: collections.abc.Callable[[], Any],
    timeout_sec: float = 30.0,
) -> Any:
    """Invoke a sync callable or async coroutine function safely."""
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        loop = None

    def _call_worker() -> Any:
        val = callable_obj()
        if inspect.iscoroutine(val):
            return asyncio.run(val)
        return val

    if loop and loop.is_running():
        pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
        try:
            return pool.submit(_call_worker).result(timeout=timeout_sec)
        finally:
            pool.shutdown(wait=False, cancel_futures=True)
    else:
        return _call_worker()


def get_dut_serial(dut: fuchsia_device.FuchsiaDevice | None) -> str | None:
    """Resolve the hardware serial number of a Honeydew FuchsiaDevice.

    Tries, in order:
    1. `dut.serial_number()`, which resolves over FIDL and therefore needs the
       device to still be reachable.
    2. `dut.ffx.get_target_info_from_target_show()`.

    These tests deliberately tear down the USB functions that carry FIDL and
    SSH, so callers should resolve the serial once up front while the device
    is still reachable and then pass it around, rather than relying on the
    later tiers to work mid-test.

    Args:
        dut: FuchsiaDevice object from Honeydew.

    Returns:
        The device serial number string, or None if unavailable.
    """
    if dut is None:
        return None

    # 1. Resolve over FIDL. Honeydew caches the result after the first call.
    try:
        serial_number = _invoke_maybe_async(dut.serial_number, timeout_sec=5.0)
        if isinstance(serial_number, str) and serial_number:
            return serial_number.strip()
    except (honeydew_errors.HoneydewError, OSError, TimeoutError) as e:
        _LOGGER.debug("Failed calling dut.serial_number(): %s", e)

    # 2. Ask ffx what it knows about the target.
    try:
        info = dut.ffx.get_target_info_from_target_show()
        dev_sn = getattr(getattr(info, "device", None), "serial_number", None)
        if dev_sn:
            return str(dev_sn).strip()
    except (honeydew_errors.HoneydewError, OSError) as e:
        _LOGGER.debug("Failed querying ffx target info for serial: %s", e)

    return None


def get_usb_config(dut: fuchsia_device.FuchsiaDevice) -> str:
    """Retrieve the current USB peripheral policy configuration from the DUT.

    Queries the device deterministically over the serial console.

    Args:
        dut: Fuchsia DUT device object.

    Returns:
        The raw configuration string from usb-cli get-config.

    Raises:
        RuntimeError: If no serial console is available or the command could
            not be dispatched over serial.
    """
    _LOGGER.info("Querying current USB peripheral configuration on target...")

    res = _send_serial_command("usb-cli get-config", timeout_sec=5.0, dut=dut)
    if res is None:
        raise RuntimeError(
            "Failed to query USB config from the device over serial console."
        )
    cleaned = _strip_serial_noise(res)
    if (
        (_DONE_TOKEN in res and cleaned.strip())
        or _ERROR_TOKEN in res
        or _USB_CLI_ERR_PREFIX in cleaned
        or "{" in cleaned
        or "functions" in cleaned
        or "configurations" in cleaned
    ):
        return res.strip()

    return ""


def normalize_usb_functions(functions: list[str]) -> list[str]:
    """Normalize function names, deduplicating while preserving order.

    Args:
        functions: List of USB peripheral function names (e.g. ['cdc',
            'vsock']).

    Returns:
        A deduplicated and normalized list of function names.
    """
    if not functions:
        return []
    return list(dict.fromkeys(functions))


def parse_usb_config_functions(config_str: str) -> list[str]:
    """Parse list of active function names from usb-cli get-config output.

    Args:
        config_str: Output string from usb-cli get-config.

    Returns:
        List of function names (e.g. ['cdc', 'adb', 'vsock']).

    Raises:
        UsbConfigParseError: If `config_str` does not contain a valid
            `usb-cli get-config` JSON payload.
    """
    data = parse_get_config(config_str)
    if data is None:
        raise UsbConfigParseError(
            "Failed to parse valid usb-cli get-config JSON from serial "
            f"output: {config_str!r}"
        )

    raw_functions: list[str] = []
    if "configurations" in data:
        raw_functions = [fn for cfg in data["configurations"] for fn in cfg]
    elif "functions" in data:
        raw_functions = list(data["functions"])

    return normalize_usb_functions(raw_functions)


_DEFAULT_CONFIG_READ_MAX_ATTEMPTS: int = 10
_DEFAULT_CONFIG_READ_TIMEOUT_SEC: float = 30.0
_DEFAULT_CONFIG_READ_DELAY_SEC: float = 1.0


def read_usb_functions_over_serial(
    dut: fuchsia_device.FuchsiaDevice,
    max_attempts: int = _DEFAULT_CONFIG_READ_MAX_ATTEMPTS,
    timeout_sec: float = _DEFAULT_CONFIG_READ_TIMEOUT_SEC,
    retry_delay_sec: float = _DEFAULT_CONFIG_READ_DELAY_SEC,
) -> tuple[str, list[str]]:
    """Query and parse the DUT's active USB functions with deadline retries.

    Retries transient empty or garbled serial console reads up to
    `max_attempts` attempts within `timeout_sec`. Propagates `RuntimeError`
    immediately if no serial console transport is available on the DUT.

    Args:
        dut: Fuchsia DUT device object.
        max_attempts: Maximum number of query attempts.
        timeout_sec: Maximum duration in seconds across all retries.
        retry_delay_sec: Delay between retry attempts in seconds.

    Returns:
        A tuple of (last_raw_output, parsed_functions_list). If all attempts
        fail or produce invalid/empty function lists, returns (last_raw, []).

    Raises:
        RuntimeError: Raised by `get_usb_config` if the DUT does not have a
            usable serial console transport.
    """
    deadline = time.monotonic() + timeout_sec
    last_raw = ""
    attempt = 0
    while attempt < max_attempts:
        attempt += 1
        raw_config = get_usb_config(dut)
        last_raw = raw_config
        if not raw_config:
            _LOGGER.warning(
                "Empty usb-cli get-config output on attempt %d/%d.",
                attempt,
                max_attempts,
            )
        else:
            try:
                functions = parse_usb_config_functions(raw_config)
            except (UsbConfigParseError, ValueError) as err:
                _LOGGER.warning(
                    "Failed to parse usb-cli get-config output on attempt "
                    "%d/%d: %s",
                    attempt,
                    max_attempts,
                    err,
                )
            else:
                if functions:
                    return raw_config, functions
                _LOGGER.warning(
                    "Parsed empty USB function list from usb-cli "
                    "get-config on attempt %d/%d: %r",
                    attempt,
                    max_attempts,
                    raw_config,
                )

        if attempt >= max_attempts or time.monotonic() >= deadline:
            break
        time.sleep(retry_delay_sec)

    return last_raw, []


def _send_serial_command(
    cmd: str,
    timeout_sec: float = 5.0,
    dut: fuchsia_device.FuchsiaDevice | None = None,
) -> str | None:
    """Send a command over the Honeydew device serial transport.

    Note on framing: the infra serial server only forwards complete
    newline-terminated lines to the UART, so the command *must* end in `\\n`.
    Honeydew's `Serial.send()` already wraps commands in `\\r\\n`; a hand-rolled
    bare `\\r` is silently swallowed and never reaches the console.

    Note on reading: `Serial.read()` explicitly makes no guarantees that output
    produced by a preceding `send()` is available yet, and each call opens a
    fresh connection to the serial server, does a single `recv()` and closes.
    Output can therefore be delayed or lost between polls. Callers must treat
    an empty result as inconclusive rather than as a failure, and verify the
    effect of the command by other means.

    Args:
        cmd: Shell command string to execute over the serial console.
        timeout_sec: Maximum duration in seconds to wait for command output.
        dut: Honeydew FuchsiaDevice instance managing the target device.

    Returns:
        The console output observed after dispatching the command, which may
        be an empty string if nothing was captured in time. None if the
        command could not be dispatched at all, i.e. the device has no serial
        transport configured or the send itself failed.
    """
    if dut is None:
        return None

    try:
        serial = dut.serial
    except honeydew_errors.HoneydewError as e:
        # Raised when 'serial_socket' was not supplied during device init.
        _LOGGER.debug("No serial transport configured for the DUT: %s", e)
        return None

    if serial is None:
        return None

    try:
        output = serial.send_and_recv(
            cmd=cmd.strip(),
            stop_tokens=(_DONE_TOKEN, _ERROR_TOKEN),
            timeout_sec=timeout_sec,
        )
    except (honeydew_errors.HoneydewError, OSError) as e:
        _LOGGER.warning("Failed dispatching serial command %r: %s", cmd, e)
        return None

    _LOGGER.info(
        "Dispatched serial command %r; console output: %s",
        cmd,
        output.strip() or "<none captured>",
    )
    return output


def _notify_intentional_disconnect(dut: fuchsia_device.FuchsiaDevice) -> None:
    """Combines multiple transport disconnect notifications."""
    dut.fuchsia_controller.before_usb_disconnect()
    dut.ffx.notify_intentional_disconnect()


def set_usb_config(
    dut: fuchsia_device.FuchsiaDevice,
    config: str,
) -> None:
    """Apply a new USB peripheral configuration on the DUT via usb-cli over serial.

    The command is dispatched deterministically over the serial console because
    serial is the only transport that survives the CDC Ethernet teardown caused
    by switching USB peripheral configurations.

    Every switch tears down all USB functions and re-enumerates, so if CDC is
    currently up this drops the FFX and SSH connections. The FFX monitor is
    therefore always notified first that the upcoming disconnect is
    intentional.

    Success is deliberately *not* gated on observing usb-cli's completion
    token, because Honeydew's serial `read()` offers no synchronization with
    `send()`, so the acknowledgement is frequently missed (see
    `_send_serial_command`). Callers confirm the outcome out-of-band instead,
    by waiting for the expected USB device to enumerate on the host.

    Args:
        dut: Fuchsia DUT device object.
        config: Function or configuration string (e.g. 'cdc,adb',
            'sourcesink', 'loopback').

    Raises:
        RuntimeError: If no serial console is available or the USB
            configuration command could not be dispatched over serial.
    """
    # The switch drops CDC, and with it FFX and SSH.
    _notify_intentional_disconnect(dut)

    _LOGGER.info(
        "Applying USB peripheral configuration '%s' on target...", config
    )

    is_test = any(
        f in config.lower() for f in ("sourcesink", "loopback", "test")
    )
    usb_cli_cmd = f"usb-cli set-config {shlex.quote(config)}"

    serial_res = _send_serial_command(usb_cli_cmd, timeout_sec=8.0, dut=dut)
    if serial_res is None:
        raise RuntimeError(
            f"Failed to dispatch USB config '{config}' to the device over "
            f"serial console (serial_res={serial_res!r})"
        )

    acknowledged = _DONE_TOKEN in serial_res or "Cold reboot" in serial_res
    if is_test and not acknowledged:
        _LOGGER.info(
            "usb-cli did not acknowledge '%s' on the console; relying on "
            "host-side USB enumeration to confirm the switch.",
            config,
        )
