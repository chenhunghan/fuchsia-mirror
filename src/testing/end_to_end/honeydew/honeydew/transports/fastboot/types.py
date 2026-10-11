# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Types used by Fastboot transport."""

import enum


class BootToFuchsiaMethod(enum.StrEnum):
    """Supported methods for booting a device from Fastboot into Fuchsia mode."""

    REBOOT = "reboot"
    CONTINUE = "continue"
