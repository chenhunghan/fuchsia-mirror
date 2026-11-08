# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Android UI navigation affordance and UiAutomator helpers for Honeydew."""

from honeydew.affordances.ui_navigation.errors import (
    BlackScreenUiAutomatorError,
    UiAutomatorRetriableTaskError,
    UiNavigationError,
    UnknownStateUiAutomatorError,
)
from honeydew.affordances.ui_navigation.ui_navigation import (
    WAIT_ON_SCREEN_SECONDS,
    UiAutomatorDevice,
    UiNavigation,
    create_uiautomator_device,
    dismiss_adb_auth_dialog,
    is_black_screenshot,
    is_idle_screen,
    return_to_home,
    return_to_idle_screen,
    run_uiautomator_task,
)

__all__ = [
    "BlackScreenUiAutomatorError",
    "UiAutomatorDevice",
    "UiAutomatorRetriableTaskError",
    "UiNavigation",
    "UiNavigationError",
    "UnknownStateUiAutomatorError",
    "WAIT_ON_SCREEN_SECONDS",
    "create_uiautomator_device",
    "dismiss_adb_auth_dialog",
    "is_black_screenshot",
    "is_idle_screen",
    "return_to_home",
    "return_to_idle_screen",
    "run_uiautomator_task",
]
