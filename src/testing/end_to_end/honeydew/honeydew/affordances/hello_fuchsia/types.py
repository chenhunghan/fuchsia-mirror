# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data types used by HelloFuchsia affordance."""

import enum


class Implementation(enum.StrEnum):
    """Different HelloFuchsia affordance implementations available."""

    # Use HelloFuchsia affordance that is implemented using Fuchsia-Controller
    FUCHSIA_CONTROLLER = "fuchsia-controller"

    # Use HelloFuchsia affordance that is implemented using FFX
    FFX = "ffx"
