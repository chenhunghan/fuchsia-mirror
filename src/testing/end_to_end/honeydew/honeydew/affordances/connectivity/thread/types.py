# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data types used by Thread affordance and OpenThread CLI controllers."""

from __future__ import annotations

import enum

import fidl_fuchsia_lowpan_device as f_lowpan_device


class ThreadRole(enum.StrEnum):
    """OpenThread device role."""

    DISABLED = "disabled"
    DETACHED = "detached"
    CHILD = "child"
    ROUTER = "router"
    LEADER = "leader"

    @staticmethod
    def from_fidl(device_state: f_lowpan_device.DeviceState) -> ThreadRole:
        """Converts `fuchsia.lowpan.device/DeviceState` to `ThreadRole`."""
        role = device_state.role
        conn_state = device_state.connectivity_state
        if role == f_lowpan_device.Role.LEADER:
            return ThreadRole.LEADER
        if role in (
            f_lowpan_device.Role.ROUTER,
            f_lowpan_device.Role.SLEEPY_ROUTER,
        ):
            return ThreadRole.ROUTER
        if role in (
            f_lowpan_device.Role.END_DEVICE,
            f_lowpan_device.Role.SLEEPY_END_DEVICE,
        ):
            return ThreadRole.CHILD
        if conn_state in (
            f_lowpan_device.ConnectivityState.INACTIVE,
            f_lowpan_device.ConnectivityState.READY,
        ):
            return ThreadRole.DISABLED
        return ThreadRole.DETACHED
