# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for Honeydew transport."""

import abc


class Transport(abc.ABC):
    """Abstract base class for Honeydew transport.

    Every Honeydew transport should inherit from this class and thus required to implement
    the methods defined in this class.
    """

    @abc.abstractmethod
    def verify_supported(self) -> None:
        """Verifies that transport implementation is supported by the Fuchsia device.

        This method should be called in every transport implementation's `__init__()` so that if a
        transport is used on a Fuchsia device that does not support it, it will raise
        NotSupportedError.

        Raises:
            NotSupportedError: If transport is not supported.
        """
