# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for HelloFuchsia affordance."""

import abc

from honeydew.affordances import affordance


class HelloFuchsia(affordance.Affordance):
    """Abstract base class for HelloFuchsia affordance."""

    @abc.abstractmethod
    async def greeting(self, name: str | None = None) -> str:
        """Returns a string that contains greeting's message.

        Args:
            name: Optional name to greet. Defaults to the target device name.

        Returns:
            greeting's message str

        Raises:
            HelloFuchsiaAffordanceError: Failed to construct greeting's message.
        """
