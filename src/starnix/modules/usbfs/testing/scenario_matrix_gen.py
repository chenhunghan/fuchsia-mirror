#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Generator for systematic usbfs validation and errno-precedence scenario JSONs."""

import argparse
import json
import os
import sys
from collections.abc import Sequence
from typing import Any


def default_device() -> dict[str, Any]:
    return {
        "vendor_id": 0x1234,
        "product_id": 0x5678,
        "bcd_usb": 0x0200,
        "bcd_device": 0x0543,
        "device_class": 0,
        "device_subclass": 0,
        "device_protocol": 0,
        "max_packet_size0": 64,
        "speed": "high",
        "configs": [
            {
                "config_value": 1,
                "bm_attributes": 0x80,
                "max_power_ma": 100,
                "interfaces": [
                    {
                        "interface_number": 0,
                        "alt_setting": 0,
                        "interface_class": 0xFF,
                        "interface_subclass": 0,
                        "interface_protocol": 0,
                        "endpoints": [
                            {
                                "address": 0x81,
                                "xfer_type": "bulk",
                                "max_packet_size": 512,
                                "interval": 0,
                            },
                            {
                                "address": 0x02,
                                "xfer_type": "bulk",
                                "max_packet_size": 512,
                                "interval": 0,
                            },
                        ],
                    }
                ],
            }
        ],
    }


def printer_device() -> dict[str, Any]:
    dev = default_device()
    dev["configs"][0]["interfaces"][0][
        "interface_class"
    ] = 7  # USB_CLASS_PRINTER
    return dev


def fido_hid_device() -> dict[str, Any]:
    """Single-interface USB 2.0 FIDO2/U2F raw HID authenticator (`USB_CLASS_HID = 0x03`)."""
    dev = default_device()
    iface = dev["configs"][0]["interfaces"][0]
    iface["interface_class"] = 0x03  # USB_CLASS_HID (no boot subclass)
    iface["endpoints"] = (
        {
            "address": 0x81,
            "xfer_type": "interrupt",
            "max_packet_size": 64,
            "interval": 5,
        },
        {
            "address": 0x02,
            "xfer_type": "interrupt",
            "max_packet_size": 64,
            "interval": 5,
        },
    )
    return dev


def _ctaphid_init_packet(
    cid: Sequence[int], cmd: int, bcnt: int, payload: Sequence[int]
) -> Sequence[int]:
    """Builds a 64-byte CTAPHID initialization packet (`cid[4] | (cmd|0x80) | bcnt_be16 | data[57]`)."""
    pkt = (
        *cid[:4],
        cmd | 0x80,
        (bcnt >> 8) & 0xFF,
        bcnt & 0xFF,
        *payload[:57],
    )
    return pkt + (0x00,) * (64 - len(pkt))


def _ctaphid_cont_packet(
    cid: Sequence[int], seq: int, payload: Sequence[int]
) -> Sequence[int]:
    """Builds a 64-byte CTAPHID continuation packet (`cid[4] | seq | data[59]`)."""
    pkt = (*cid[:4], seq & 0x7F, *payload[:59])
    return pkt + (0x00,) * (64 - len(pkt))


def build_discovery_and_descriptors() -> dict[str, Any]:
    steps: list[dict[str, Any]] = [
        {"type": "stat_dev"},
    ]
    for attr in [
        "idVendor",
        "idProduct",
        "bcdDevice",
        "bDeviceClass",
        "bDeviceSubClass",
        "bDeviceProtocol",
        "bNumConfigurations",
        "bConfigurationValue",
        "busnum",
        "devnum",
        "speed",
        "uevent",
        "descriptors",
    ]:
        steps.append({"type": "read_sysfs", "attr": attr})

    steps.extend(
        [
            {"type": "open", "fd": "fd_ro", "mode": "rdonly"},
            {"type": "read", "fd": "fd_ro", "size": 64},
            {"type": "read", "fd": "fd_ro", "size": 64},
            {
                "type": "lseek",
                "fd": "fd_ro",
                "offset": 18,
                "whence": "SEEK_SET",
            },
            {"type": "read", "fd": "fd_ro", "size": 9},
            {"type": "pread", "fd": "fd_ro", "offset": 0, "size": 18},
            {
                "type": "lseek",
                "fd": "fd_ro",
                "offset": -1,
                "whence": "SEEK_SET",
            },
            {"type": "get_speed", "fd": "fd_ro"},
            {"type": "open", "fd": "fd_rw", "mode": "rdwr"},
            {"type": "write", "fd": "fd_rw", "data": [1, 2, 3, 4]},
            {"type": "get_speed", "fd": "fd_rw"},
            {"type": "get_capabilities", "fd": "fd_rw", "arg_ptr": "valid"},
            {"type": "connectinfo", "fd": "fd_rw", "arg_ptr": "valid"},
            {"type": "close", "fd": "fd_ro"},
            {"type": "close", "fd": "fd_rw"},
        ]
    )
    return {
        "name": "discovery_and_descriptors",
        "device": default_device(),
        "steps": steps,
    }


def build_sync_control_and_bulk() -> dict[str, Any]:
    return {
        "name": "sync_control_and_bulk",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "open", "fd": "fd2", "mode": "rdwr"},
            {
                "type": "device_script_control",
                "b_request": 1,
                "action": {"reply": [0xCA, 0xFE, 0xBA, 0xBE]},
            },
            {
                "type": "control",
                "fd": "fd1",
                "bm_request_type": 0x40,
                "b_request": 1,
                "w_value": 0x1234,
                "w_index": 0,
                "w_length": 4,
                "timeout_ms": 2000,
                "data": [0xCA, 0xFE, 0xBA, 0xBE],
            },
            {
                "type": "control",
                "fd": "fd1",
                "bm_request_type": 0xC0,
                "b_request": 1,
                "w_value": 0x1234,
                "w_index": 0,
                "w_length": 4,
                "timeout_ms": 2000,
            },
            {
                "type": "control",
                "fd": "fd1",
                "bm_request_type": 0x40,
                "b_request": 2,
                "w_value": 0,
                "w_index": 0,
                "w_length": 0,
                "timeout_ms": 2000,
                "data_ptr": "null",
            },
            {
                "type": "device_script_control",
                "b_request": 0xFF,
                "action": {"stall": True},
            },
            {
                "type": "control",
                "fd": "fd1",
                "bm_request_type": 0xC0,
                "b_request": 0xFF,
                "w_value": 0,
                "w_index": 0,
                "w_length": 4,
                "timeout_ms": 2000,
            },
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {
                "type": "bulk",
                "fd": "fd1",
                "ep": 0x02,
                "len": 8,
                "timeout_ms": 2000,
                "data": [0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4],
            },
            {"type": "device_ep_assert_empty", "ep": 0x02},
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {"type": "device_ep_assert_empty", "ep": 0x02},
            {
                "type": "device_ep_push_in",
                "ep": 0x81,
                "data": [0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4],
            },
            {
                "type": "bulk",
                "fd": "fd1",
                "ep": 0x81,
                "len": 64,
                "timeout_ms": 2000,
            },
            {
                "type": "device_script_control",
                "b_request": 0x30,
                "action": {"hold": "h_ctrl_1"},
            },
            {
                "type": "start_op",
                "task": "t_ctrl",
                "inner": {
                    "type": "control",
                    "fd": "fd1",
                    "bm_request_type": 0xC0,
                    "b_request": 0x30,
                    "w_value": 0,
                    "w_index": 0,
                    "w_length": 4,
                    "timeout_ms": 2000,
                },
            },
            {
                "type": "device_release_control",
                "hold": "h_ctrl_1",
                "action": {"reply": [0xAA, 0xBB, 0xCC, 0xDD]},
            },
            {"type": "await_task", "task": "t_ctrl", "timeout_ms": 2000},
            {
                "type": "start_op",
                "task": "t_bulk_in",
                "inner": {
                    "type": "bulk",
                    "fd": "fd1",
                    "ep": 0x81,
                    "len": 64,
                    "timeout_ms": 2000,
                },
            },
            {
                "type": "device_ep_push_in",
                "ep": 0x81,
                "data": [0x11, 0x22, 0x33, 0x44],
            },
            {"type": "await_task", "task": "t_bulk_in", "timeout_ms": 2000},
            {"type": "await_task", "task": "t_nonexistent", "timeout_ms": 100},
            {
                "type": "bulk",
                "fd": "fd2",
                "ep": 0x02,
                "len": 4,
                "timeout_ms": 1000,
                "data": [1, 2, 3, 4],
            },
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd1"},
            {"type": "close", "fd": "fd2"},
        ],
    }


def build_async_urb_basic() -> dict[str, Any]:
    return {
        "name": "async_urb_basic",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {"type": "reap_urb_ndelay", "fd": "fd1"},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_in_cancel",
                "urb_type": "bulk",
                "ep": 0x81,
                "buffer_len": 64,
                "usercontext": 0x1111,
            },
            {
                "type": "poll",
                "fd": "fd1",
                "events": ["POLLOUT", "POLLWRNORM"],
                "timeout_ms": 0,
            },
            {"type": "reap_urb_ndelay", "fd": "fd1"},
            {"type": "discard_urb", "fd": "fd1", "urb": "u_in_cancel"},
            {"type": "discard_urb", "fd": "fd1", "urb": "u_in_cancel"},
            {"type": "reap_urb_ndelay", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_in_cancel"},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_out",
                "urb_type": "bulk",
                "ep": 0x02,
                "buffer_len": 4,
                "data": [0x10, 0x20, 0x30, 0x40],
                "usercontext": 0x2222,
            },
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {
                "type": "poll",
                "fd": "fd1",
                "events": ["POLLOUT", "POLLWRNORM"],
                "timeout_ms": 2000,
            },
            {"type": "reap_urb", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_out"},
            {
                "type": "device_ep_push_in",
                "ep": 0x81,
                "data": [0x10, 0x20, 0x30, 0x40],
            },
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_in",
                "urb_type": "bulk",
                "ep": 0x81,
                "buffer_len": 64,
                "usercontext": 0x3333,
            },
            {
                "type": "epoll_check",
                "fd": "fd1",
                "events": ["EPOLLOUT"],
                "timeout_ms": 2000,
            },
            {"type": "reap_urb", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_in"},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_async_wait",
                "urb_type": "bulk",
                "ep": 0x81,
                "buffer_len": 64,
                "usercontext": 0x4444,
            },
            {
                "type": "start_op",
                "task": "t_reap",
                "inner": {"type": "reap_urb", "fd": "fd1"},
            },
            {
                "type": "device_ep_push_in",
                "ep": 0x81,
                "data": [0xDE, 0xAD, 0xBE, 0xEF],
            },
            {"type": "await_task", "task": "t_reap", "timeout_ms": 2000},
            {"type": "inspect_urb", "urb": "u_async_wait"},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd1"},
        ],
    }


def build_error_precedence_rdonly_and_ptrs() -> dict[str, Any]:
    steps: list[dict[str, Any]] = [
        {"type": "open", "fd": "fd_ro", "mode": "rdonly"},
        {"type": "open", "fd": "fd_rw", "mode": "rdwr"},
    ]
    for ptr_kind in ["null", "unmapped"]:
        for op_spec in [
            {"type": "get_capabilities"},
            {"type": "connectinfo"},
            {"type": "get_driver", "interface": 0},
            {"type": "claim_interface", "interface": 0},
            {"type": "release_interface", "interface": 0},
            {"type": "disconnect_claim", "interface": 0, "driver": "usbfs"},
            {"type": "set_interface", "interface": 0, "altsetting": 0},
            {"type": "set_configuration", "config": 1},
            {"type": "clear_halt", "endpoint": 0x81},
            {
                "type": "control",
                "bm_request_type": 0xC0,
                "b_request": 1,
                "w_value": 0,
                "w_index": 0,
                "w_length": 4,
                "timeout_ms": 500,
            },
            {"type": "bulk", "ep": 0x02, "len": 4, "timeout_ms": 500},
            {
                "type": "submit_urb",
                "urb": f"u_bad_{ptr_kind}",
                "urb_type": "bulk",
                "ep": 0x02,
                "buffer_len": 4,
            },
            {"type": "discard_urb", "urb": "u_none"},
            {"type": "reap_urb_ndelay"},
            {"type": "raw_ioctl", "request": 0xDEAD_BEEF},
        ]:
            ro_step = dict(op_spec, fd="fd_ro", arg_ptr=ptr_kind)
            steps.append(ro_step)
            rw_step = dict(op_spec, fd="fd_rw", arg_ptr=ptr_kind)
            steps.append(rw_step)

    steps.extend(
        [
            {"type": "close", "fd": "fd_ro"},
            {"type": "close", "fd": "fd_rw"},
        ]
    )
    return {
        "name": "error_precedence_rdonly_and_ptrs",
        "device": default_device(),
        "steps": steps,
    }


def build_interface_claims_and_getdriver() -> dict[str, Any]:
    return {
        "name": "interface_claims_and_getdriver",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "open", "fd": "fd2", "mode": "rdwr"},
            {"type": "get_driver", "fd": "fd1", "interface": 0},
            {"type": "claim_interface", "fd": "fd1", "interface": 5},
            {"type": "claim_interface", "fd": "fd1", "interface": 99},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {"type": "get_driver", "fd": "fd1", "interface": 0},
            {"type": "claim_interface", "fd": "fd2", "interface": 0},
            {"type": "release_interface", "fd": "fd2", "interface": 0},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {
                "type": "disconnect_claim",
                "fd": "fd2",
                "interface": 0,
                "flags": ["EXCEPT_DRIVER"],
                "driver": "other",
            },
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd2"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd1"},
        ],
    }


def build_config_interface_and_halt() -> dict[str, Any]:
    return {
        "name": "config_interface_and_halt",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "open", "fd": "fd2", "mode": "rdwr"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {
                "type": "set_interface",
                "fd": "fd1",
                "interface": 0,
                "altsetting": 0,
            },
            {
                "type": "set_interface",
                "fd": "fd1",
                "interface": 0,
                "altsetting": 5,
            },
            {
                "type": "set_interface",
                "fd": "fd2",
                "interface": 0,
                "altsetting": 0,
            },
            {"type": "set_configuration", "fd": "fd1", "config": 1},
            {"type": "clear_halt", "fd": "fd1", "endpoint": 0x81},
            {"type": "clear_halt", "fd": "fd1", "endpoint": 0x02},
            {"type": "clear_halt", "fd": "fd1", "endpoint": 0x89},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "set_configuration", "fd": "fd1", "config": 1},
            {"type": "set_configuration", "fd": "fd1", "config": 99},
            {"type": "reset", "fd": "fd1"},
            {"type": "close", "fd": "fd1"},
            {"type": "close", "fd": "fd2"},
        ],
    }


def build_control_recip_and_printer_quirk() -> dict[str, Any]:
    return {
        "name": "control_recip_and_printer_quirk",
        "device": printer_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "open", "fd": "fd2", "mode": "rdwr"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {
                "type": "device_script_control",
                "b_request": 0,
                "action": {"reply": [0x00, 0x04, 0x50, 0x52]},
            },
            {
                "type": "control",
                "fd": "fd2",
                "bm_request_type": 0xA1,
                "b_request": 0,
                "w_value": 0,
                "w_index": 0,
                "w_length": 4,
                "timeout_ms": 2000,
            },
            {
                "type": "control",
                "fd": "fd2",
                "bm_request_type": 0x41,
                "b_request": 9,
                "w_value": 0,
                "w_index": 0,
                "w_length": 0,
                "timeout_ms": 2000,
            },
            {
                "type": "control",
                "fd": "fd2",
                "bm_request_type": 0x21,
                "b_request": 2,
                "w_value": 0,
                "w_index": 0,
                "w_length": 0,
                "timeout_ms": 2000,
            },
            {
                "type": "control",
                "fd": "fd1",
                "bm_request_type": 0xC0,
                "b_request": 1,
                "w_value": 0,
                "w_index": 0,
                "w_length": 4097,
                "timeout_ms": 2000,
            },
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd1"},
            {"type": "close", "fd": "fd2"},
        ],
    }


def build_urb_flags_and_cancellation() -> dict[str, Any]:
    return {
        "name": "urb_flags_and_cancellation",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {
                "type": "device_ep_push_in",
                "ep": 0x81,
                "data": [],
            },
            {
                "type": "bulk",
                "fd": "fd1",
                "ep": 0x81,
                "len": 64,
                "timeout_ms": 2000,
            },
            {
                "type": "device_ep_push_in",
                "ep": 0x81,
                "data": [1, 2, 3, 4],
            },
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_short",
                "urb_type": "bulk",
                "ep": 0x81,
                "flags": ["SHORT_NOT_OK"],
                "buffer_len": 64,
                "usercontext": 0x4444,
            },
            {"type": "reap_urb", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_short"},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_zlp_out",
                "urb_type": "bulk",
                "ep": 0x02,
                "flags": ["ZERO_PACKET"],
                "buffer_len": 512,
                "data": [0x55] * 512,
                "usercontext": 0x5555,
            },
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {"type": "device_ep_assert_empty", "ep": 0x02},
            {"type": "reap_urb", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_zlp_out"},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_cancel_in",
                "urb_type": "bulk",
                "ep": 0x81,
                "buffer_len": 64,
                "usercontext": 0x6666,
            },
            {"type": "discard_urb", "fd": "fd1", "urb": "u_cancel_in"},
            {"type": "reap_urb", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_cancel_in"},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd1"},
        ],
    }


def build_hotplug_disconnect_and_reconnect() -> dict[str, Any]:
    return {
        "name": "hotplug_disconnect_and_reconnect",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_done",
                "urb_type": "bulk",
                "ep": 0x02,
                "buffer_len": 4,
                "data": [1, 2, 3, 4],
                "usercontext": 0x101,
            },
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {
                "type": "poll",
                "fd": "fd1",
                "events": ["POLLOUT", "POLLWRNORM"],
                "timeout_ms": 2000,
            },
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": "u_inflight",
                "urb_type": "bulk",
                "ep": 0x81,
                "buffer_len": 64,
                "usercontext": 0x202,
            },
            {"type": "device_disconnect"},
            {
                "type": "poll",
                "fd": "fd1",
                "events": ["POLLOUT", "POLLWRNORM", "POLLERR", "POLLHUP"],
                "timeout_ms": 500,
            },
            {"type": "get_speed", "fd": "fd1"},
            {"type": "read", "fd": "fd1", "size": 18},
            {"type": "reap_urb_ndelay", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_done"},
            {"type": "reap_urb_ndelay", "fd": "fd1"},
            {"type": "inspect_urb", "urb": "u_inflight"},
            {"type": "reap_urb_ndelay", "fd": "fd1"},
            {"type": "close", "fd": "fd1"},
            {"type": "device_reconnect"},
            {"type": "open", "fd": "fd_new", "mode": "rdwr"},
            {"type": "get_speed", "fd": "fd_new"},
            {"type": "close", "fd": "fd_new"},
        ],
    }


def build_compat_arch32_ioctls() -> dict[str, Any]:
    return {
        "name": "compat_arch32_ioctls",
        "device": default_device(),
        "steps": [
            {"type": "open", "fd": "fd1", "mode": "rdwr"},
            {"type": "claim_interface", "fd": "fd1", "interface": 0},
            {
                "type": "device_script_control",
                "b_request": 1,
                "action": {"reply": [9, 8, 7, 6]},
            },
            {
                "type": "control",
                "fd": "fd1",
                "arch32": True,
                "bm_request_type": 0xC0,
                "b_request": 1,
                "w_value": 0,
                "w_index": 0,
                "w_length": 4,
                "timeout_ms": 2000,
            },
            {
                "type": "bulk",
                "fd": "fd1",
                "arch32": True,
                "ep": 0x02,
                "len": 4,
                "timeout_ms": 2000,
                "data": [5, 6, 7, 8],
            },
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {
                "type": "submit_urb",
                "fd": "fd1",
                "arch32": True,
                "urb": "u32_out",
                "urb_type": "bulk",
                "ep": 0x02,
                "buffer_len": 4,
                "data": [11, 22, 33, 44],
                "usercontext": 0x7777,
            },
            {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 2000},
            {"type": "reap_urb", "fd": "fd1", "arch32": True},
            {"type": "inspect_urb", "urb": "u32_out"},
            {"type": "release_interface", "fd": "fd1", "interface": 0},
            {"type": "close", "fd": "fd1"},
        ],
    }


def build_fido_hid_ctap_interrupt_session() -> dict[str, Any]:
    """End-to-end FIDO2/CTAP2 raw HID session over 64-byte interrupt URBs and USBDEVFS_IOCTL."""
    broadcast_cid = (0xFF, 0xFF, 0xFF, 0xFF)
    allocated_cid = (0x01, 0x02, 0x03, 0x04)
    nonce = (0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18)

    init_req = _ctaphid_init_packet(broadcast_cid, 0x06, 8, nonce)
    # 17-byte CTAPHID_INIT response: 8B nonce + 4B allocated CID + proto=2 + v=1.0.4 + caps=0x05
    init_resp_payload = (*nonce, *allocated_cid, 0x02, 0x01, 0x00, 0x04, 0x05)
    init_resp = _ctaphid_init_packet(
        broadcast_cid, 0x06, len(init_resp_payload), init_resp_payload
    )

    # Fragmented 90-byte CTAPHID_CBOR (0x10) request across 1 init packet (57B) + 1 cont packet (33B)
    cbor_frag0 = (0x02, *((i * 7 + 3) & 0xFF for i in range(56)))
    cbor_frag1 = tuple((i * 11 + 5) & 0xFF for i in range(33))
    cbor_out0 = _ctaphid_init_packet(allocated_cid, 0x10, 90, cbor_frag0)
    cbor_out1 = _ctaphid_cont_packet(allocated_cid, 0, cbor_frag1)

    # Status 0x02 (STATUS_UPNEEDED keepalive while waiting for user presence) then CTAP2_OK reply
    keepalive_in = _ctaphid_init_packet(allocated_cid, 0x3B, 1, (0x02,))
    cbor_reply_in = _ctaphid_init_packet(
        allocated_cid, 0x10, 16, (0x00, *(0x5A + i for i in range(15)))
    )

    steps: Sequence[dict[str, Any]] = (
        # Two-step open pattern: read-only descriptor inspection followed by writable reopen.
        {"type": "open", "fd": "fd_ro", "mode": "rdonly"},
        {"type": "read", "fd": "fd_ro", "size": 64},
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd_ro",
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {"type": "close", "fd": "fd_ro"},
        {"type": "open", "fd": "fd1", "mode": "rdwr"},
        # Validate USBDEVFS_IOCTL error checks and kernel-driver disconnect before claiming ifno 0.
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "null",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": 99,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": -1,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": 0,
            "ioctl_code": 0x1234,
            "arg_ptr": "valid",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": -1,
            "ioctl_code": 0x4004_55FF,
            "arg_ptr": "valid",
            "data_ptr": "null",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": 0,
            "ioctl_code": 0x4004_55FF,
            "arg_ptr": "valid",
            "data": (1, 2, 3, 4),
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": 0,
            "ioctl_code": 0x8004_55FF,
            "arg_ptr": "valid",
            "data_ptr": "null",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {"type": "claim_interface", "fd": "fd1", "interface": 0},
        # HID class-specific SET_IDLE (bmRequestType=0x21, bRequest=0x0A, wValue=0, wIndex=0).
        {
            "type": "device_script_control",
            "b_request": 0x0A,
            "action": {"reply": ()},
        },
        {
            "type": "control",
            "fd": "fd1",
            "bm_request_type": 0x21,
            "b_request": 0x0A,
            "w_value": 0,
            "w_index": 0,
            "w_length": 0,
            "timeout_ms": 1000,
        },
        # Pre-post Interrupt IN URB before sending CTAPHID_INIT on Interrupt OUT (realistic FIDO order).
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_init_in",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x1001,
        },
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_init_out",
            "urb_type": "interrupt",
            "ep": 0x02,
            "buffer_len": 64,
            "data": init_req,
            "usercontext": 0x1002,
        },
        {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 1500},
        {"type": "poll", "fd": "fd1", "timeout_ms": 1000},
        {"type": "reap_urb_ndelay", "fd": "fd1"},
        {"type": "device_ep_push_in", "ep": 0x81, "data": init_resp},
        {"type": "poll", "fd": "fd1", "timeout_ms": 1000},
        {"type": "reap_urb_ndelay", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_init_in"},
        # Multi-fragment CTAPHID_CBOR request (2x64B OUT) + keepalive (0x3B) + final CBOR reply (2x64B IN).
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_cbor_out0",
            "urb_type": "interrupt",
            "ep": 0x02,
            "buffer_len": 64,
            "data": cbor_out0,
            "usercontext": 0x2001,
        },
        {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 1500},
        {"type": "reap_urb", "fd": "fd1"},
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_cbor_out1",
            "urb_type": "interrupt",
            "ep": 0x02,
            "buffer_len": 64,
            "data": cbor_out1,
            "usercontext": 0x2002,
        },
        {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 1500},
        {"type": "reap_urb", "fd": "fd1"},
        {
            "type": "device_ep_push_in",
            "ep": 0x81,
            "data": keepalive_in,
        },
        {
            "type": "device_ep_push_in",
            "ep": 0x81,
            "data": cbor_reply_in,
        },
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_keepalive_in",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x2003,
        },
        {"type": "reap_urb", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_keepalive_in"},
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_cbor_in",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x2004,
        },
        {"type": "reap_urb", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_cbor_in"},
        # Detach fd1's active claim via USBDEVFS_IOCTL(DISCONNECT) from fd2, verify handoff, and re-trigger CONNECT.
        {"type": "open", "fd": "fd2", "mode": "rdwr"},
        {"type": "claim_interface", "fd": "fd2", "interface": 0},
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd2",
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd2",
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {"type": "claim_interface", "fd": "fd2", "interface": 0},
        {"type": "release_interface", "fd": "fd2", "interface": 0},
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd2",
            "ifno": 0,
            "sub_ioctl": "connect",
            "arg_ptr": "valid",
        },
        {"type": "close", "fd": "fd2"},
        {"type": "close", "fd": "fd1"},
    )
    return {
        "name": "fido_hid_ctap_interrupt_session",
        "device": fido_hid_device(),
        "steps": steps,
    }


def build_fido_hid_control_reports_and_cancel() -> dict[str, Any]:
    """HID control SET_REPORT/GET_REPORT, STALL on unsupported GET_PROTOCOL, CTAP cancel+rearm, unplug."""
    cid = (0x11, 0x22, 0x33, 0x44)
    report_out = _ctaphid_init_packet(cid, 0x01, 4, (0xDE, 0xAD, 0xBE, 0xEF))
    report_in = _ctaphid_init_packet(cid, 0x01, 4, (0xCA, 0xFE, 0xBA, 0xBE))
    cancel_pkt = _ctaphid_init_packet(cid, 0x11, 0, ())
    retry_resp = _ctaphid_init_packet(cid, 0x10, 4, (0x00, 0xAA, 0xBB, 0xCC))

    steps: Sequence[dict[str, Any]] = (
        {"type": "open", "fd": "fd1", "mode": "rdwr"},
        {"type": "claim_interface", "fd": "fd1", "interface": 0},
        # HID class SET_REPORT (0x21, bRequest=0x09, wValue=0x0200 Output Report) with 64-byte frame.
        {
            "type": "device_script_control",
            "b_request": 0x09,
            "action": {"reply": ()},
        },
        {
            "type": "control",
            "fd": "fd1",
            "bm_request_type": 0x21,
            "b_request": 0x09,
            "w_value": 0x0200,
            "w_index": 0,
            "w_length": 64,
            "data": report_out,
            "timeout_ms": 1000,
        },
        # HID class GET_REPORT (0xA1, bRequest=0x01, wValue=0x0100 Input Report) returning 64-byte frame.
        {
            "type": "device_script_control",
            "b_request": 0x01,
            "action": {"reply": report_in},
        },
        {
            "type": "control",
            "fd": "fd1",
            "bm_request_type": 0xA1,
            "b_request": 0x01,
            "w_value": 0x0100,
            "w_index": 0,
            "w_length": 64,
            "timeout_ms": 1000,
        },
        # Unsupported HID boot-subclass GET_PROTOCOL (bRequest=0x03) returns protocol STALL (EPIPE).
        {
            "type": "device_script_control",
            "b_request": 0x03,
            "action": {"stall": True},
        },
        {
            "type": "control",
            "fd": "fd1",
            "bm_request_type": 0xA1,
            "b_request": 0x03,
            "w_value": 0,
            "w_index": 0,
            "w_length": 1,
            "timeout_ms": 1000,
        },
        # Queue Interrupt IN URB awaiting user presence; verify non-blocking reap returns EAGAIN.
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_wait_touch",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x3001,
        },
        {"type": "reap_urb_ndelay", "fd": "fd1"},
        # Host aborts user-presence wait: send CTAPHID_CANCEL on ep 0x02 and discard pending IN URB.
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_cancel_out",
            "urb_type": "interrupt",
            "ep": 0x02,
            "buffer_len": 64,
            "data": cancel_pkt,
            "usercontext": 0x3002,
        },
        {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 1500},
        {"type": "reap_urb", "fd": "fd1"},
        {"type": "discard_urb", "fd": "fd1", "urb": "u_wait_touch"},
        {"type": "reap_urb_ndelay", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_wait_touch"},
        # Immediately re-arm Interrupt IN endpoint on the same fd to verify clean post-cancel recovery.
        {"type": "device_ep_push_in", "ep": 0x81, "data": retry_resp},
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_retry_in",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x3003,
        },
        {"type": "reap_urb", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_retry_in"},
        # Unplug security key while an Interrupt IN URB is parked waiting for touch.
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_unplug_in",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x3004,
        },
        {"type": "device_disconnect"},
        {"type": "poll", "fd": "fd1", "timeout_ms": 500},
        {"type": "reap_urb_ndelay", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_unplug_in"},
        {"type": "close", "fd": "fd1"},
    )
    return {
        "name": "fido_hid_control_reports_and_cancel",
        "device": fido_hid_device(),
        "steps": steps,
    }


def build_fido_hid_pipelined_ring() -> dict[str, Any]:
    """Pipelined 4-deep Interrupt IN ring and bulk-on-interrupt translation."""
    cid = (0x55, 0x66, 0x77, 0x88)
    ring_packets = (
        _ctaphid_init_packet(
            cid, 0x10, 200, (0x00, *(0x10 + i for i in range(56)))
        ),
        _ctaphid_cont_packet(cid, 0, tuple(0x40 + i for i in range(59))),
        _ctaphid_cont_packet(cid, 1, tuple(0x80 + i for i in range(59))),
        _ctaphid_cont_packet(cid, 2, tuple(0xC0 + i for i in range(25))),
    )
    bulk_on_int_out = _ctaphid_init_packet(cid, 0x01, 8, range(1, 9))
    bulk_on_int_in = _ctaphid_init_packet(cid, 0x01, 8, range(11, 19))

    steps: Sequence[dict[str, Any]] = (
        {"type": "open", "fd": "fd1", "mode": "rdwr"},
        {"type": "claim_interface", "fd": "fd1", "interface": 0},
        # 1. Queue 4 Interrupt IN URBs back-to-back before pushing 4 fragmented response packets.
        *(
            {
                "type": "submit_urb",
                "fd": "fd1",
                "urb": f"u_ring{i}",
                "urb_type": "interrupt",
                "ep": 0x81,
                "buffer_len": 64,
                "usercontext": 0x4100 + i,
            }
            for i in range(len(ring_packets))
        ),
        *(
            {"type": "device_ep_push_in", "ep": 0x81, "data": pkt}
            for pkt in ring_packets
        ),
        *({"type": "reap_urb", "fd": "fd1"} for _ in ring_packets),
        *(
            {"type": "inspect_urb", "urb": f"u_ring{i}"}
            for i in range(len(ring_packets))
        ),
        # 2. Synchronous USBDEVFS_BULK and USBDEVFS_URB_TYPE_BULK submitted to Interrupt endpoints.
        {
            "type": "bulk",
            "fd": "fd1",
            "ep": 0x02,
            "len": 64,
            "data": bulk_on_int_out,
            "timeout_ms": 1000,
        },
        {"type": "device_ep_pop_out", "ep": 0x02, "timeout_ms": 1500},
        {"type": "device_ep_push_in", "ep": 0x81, "data": bulk_on_int_in},
        {
            "type": "submit_urb",
            "fd": "fd1",
            "urb": "u_bulk_on_int",
            "urb_type": "bulk",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x4200,
        },
        {"type": "reap_urb", "fd": "fd1"},
        {"type": "inspect_urb", "urb": "u_bulk_on_int"},
        {"type": "release_interface", "fd": "fd1", "interface": 0},
        {"type": "close", "fd": "fd1"},
    )
    return {
        "name": "fido_hid_pipelined_ring",
        "device": fido_hid_device(),
        "steps": steps,
    }


def build_fido_hid_compat32() -> dict[str, Any]:
    """32-bit compat ioctl (`arch32: true`) USBDEVFS_IOCTL32 + SUBMITURB32 + REAPURBNDELAY32 on FIDO HID."""
    cid = (0x55, 0x66, 0x77, 0x88)
    compat32_in = _ctaphid_init_packet(cid, 0x10, 6, (0x00, 1, 2, 3, 4, 5))

    steps: Sequence[dict[str, Any]] = (
        {"type": "open", "fd": "fd1", "mode": "rdwr"},
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "arch32": True,
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "null",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "arch32": True,
            "ifno": -1,
            "ioctl_code": 0x4004_55FF,
            "arg_ptr": "valid",
            "data_ptr": "null",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "arch32": True,
            "ifno": 0,
            "ioctl_code": 0x4004_55FF,
            "arg_ptr": "valid",
            "data": (1, 2, 3, 4),
        },
        {"type": "claim_interface", "fd": "fd1", "interface": 0},
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "arch32": True,
            "ifno": 0,
            "sub_ioctl": "connect",
            "arg_ptr": "valid",
        },
        {"type": "device_ep_push_in", "ep": 0x81, "data": compat32_in},
        {
            "type": "submit_urb",
            "fd": "fd1",
            "arch32": True,
            "urb": "u_compat32_in",
            "urb_type": "interrupt",
            "ep": 0x81,
            "buffer_len": 64,
            "usercontext": 0x4300,
        },
        {"type": "poll", "fd": "fd1", "timeout_ms": 1000},
        {"type": "reap_urb_ndelay", "fd": "fd1", "arch32": True},
        {"type": "inspect_urb", "urb": "u_compat32_in"},
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "arch32": True,
            "ifno": 0,
            "sub_ioctl": "disconnect",
            "arg_ptr": "valid",
        },
        {
            "type": "usbdevfs_ioctl",
            "fd": "fd1",
            "arch32": True,
            "ifno": 0,
            "sub_ioctl": "connect",
            "arg_ptr": "valid",
        },
        {"type": "close", "fd": "fd1"},
    )
    return {
        "name": "fido_hid_compat32",
        "device": fido_hid_device(),
        "steps": steps,
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate usbfs scenario JSONs and scenarios_list.json."
    )
    parser.add_argument(
        "--phase2-only",
        action="store_true",
        help="Generate only the 3 initial Phase 2 scenarios.",
    )
    args = parser.parse_args()

    script_dir = os.path.dirname(os.path.abspath(__file__))
    scenarios_dir = os.path.join(script_dir, "scenarios")
    os.makedirs(scenarios_dir, exist_ok=True)

    builders = [
        build_discovery_and_descriptors,
        build_sync_control_and_bulk,
        build_async_urb_basic,
    ]
    if not args.phase2_only:
        builders.extend(
            [
                build_error_precedence_rdonly_and_ptrs,
                build_interface_claims_and_getdriver,
                build_config_interface_and_halt,
                build_control_recip_and_printer_quirk,
                build_urb_flags_and_cancellation,
                build_hotplug_disconnect_and_reconnect,
                build_compat_arch32_ioctls,
                build_fido_hid_ctap_interrupt_session,
                build_fido_hid_control_reports_and_cancel,
                build_fido_hid_pipelined_ring,
                build_fido_hid_compat32,
            ]
        )

    names: list[str] = []
    for builder in builders:
        scenario = builder()
        name = scenario["name"]
        names.append(name)
        out_path = os.path.join(scenarios_dir, f"{name}.json")
        with open(out_path, "w") as f:
            json.dump(scenario, f, indent=4)
            f.write("\n")

    if not args.phase2_only:
        expected_files = {f"{name}.json" for name in names}
        for existing in os.listdir(scenarios_dir):
            if existing.endswith(".json") and existing not in expected_files:
                os.remove(os.path.join(scenarios_dir, existing))

    list_path = os.path.join(script_dir, "scenarios_list.json")
    with open(list_path, "w") as f:
        json.dump(names, f, indent=4)
        f.write("\n")

    print(f"[scenario_matrix_gen] Generated {len(names)} scenarios.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
