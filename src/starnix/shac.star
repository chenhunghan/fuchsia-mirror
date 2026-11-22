# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""SHAC checks specific to //src/starnix.

shac discovers nested shac.star files automatically, and runs each one with its
file paths scoped to its own directory, so all paths here are relative to
//src/starnix.
"""

load("//src/starnix/lib/selinux/scripts/selinux_policy.star", "register_selinux_policy_checks")

register_selinux_policy_checks()
