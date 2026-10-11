# Copyright 2026 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import TypeAlias

Json: TypeAlias = (
    dict[str, "Json"] | list["Json"] | str | int | float | bool | None
)
"""JSON serializable data."""

ControllerConfig: TypeAlias = dict[str, Json]
"""Mobly configuration specific to a controller.

Defined in the Mobly config under TestBeds -> Controllers ->
<MOBLY_CONTROLLER_CONFIG_NAME>.
"""
