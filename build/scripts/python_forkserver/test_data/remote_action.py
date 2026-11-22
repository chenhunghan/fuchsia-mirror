# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Stand-in for //build/rbe/remote_action.py in server_test.py's fake checkout.

The RBE wrappers import the real module. This one only counts
init_from_main_once() calls, which each wrapper's __main__ block makes once,
so fake_wrapper.py can report whether a run started from fresh module state.
"""

INIT_COUNT = 0


def init_from_main_once() -> None:
    global INIT_COUNT
    INIT_COUNT += 1
