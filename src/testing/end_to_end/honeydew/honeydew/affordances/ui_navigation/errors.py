# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by UI navigation affordances."""

from honeydew import errors
from honeydew.utils.control_flows import RetryAbortingError


class UiNavigationError(errors.AffordanceError):
    """Exception to be raised by UI navigation affordance."""


class UiAutomatorRetriableTaskError(UiNavigationError):
    """Exception for transient UI Automator task failures that retry without resetting snippet."""


class UnknownStateUiAutomatorError(UiAutomatorRetriableTaskError):
    """Raised when the device UI is in an unrecognized state during a UI Automator check."""


class BlackScreenUiAutomatorError(
    UnknownStateUiAutomatorError, RetryAbortingError
):
    """Raised when repeated black screen mitigation fails and retry should abort immediately."""
