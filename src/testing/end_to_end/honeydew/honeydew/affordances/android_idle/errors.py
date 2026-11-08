# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by AndroidIdle affordance."""

from honeydew import errors
from honeydew.affordances.android_session import errors as session_errors


class AndroidIdleError(errors.AffordanceError):
    """Exception to be raised by AndroidIdle affordance."""


class AndroidIdleSessionError(
    AndroidIdleError, session_errors.AndroidSessionError
):
    """Raised when the Android session changes unexpectedly while idling."""
