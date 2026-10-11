# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for Honeydew affordance."""

import abc
import functools
from collections.abc import Callable, Coroutine
from typing import Any, ParamSpec, TypeVar

T = TypeVar("T")
P = ParamSpec("P")


class Affordance(abc.ABC):
    """Abstract base class for Honeydew affordance.

    Every Honeydew affordance contract should inherit from this class and thus required to implement
    the methods defined in this class.
    """

    @abc.abstractmethod
    def verify_supported(self) -> None:
        """Verifies that affordance implementation is supported by the Fuchsia device.

        This method should be called in every affordance implementation's `__init__()` so that if an
        affordance is used on a Fuchsia device that does not support it, it will raise
        NotSupportedError.

        Raises:
            NotSupportedError: If affordance is not supported.
        """


class AsyncLazyReady:
    def __init__(self) -> None:
        self._ready = False
        # Guard to prevent intermediate super().make_ready() calls from prematurely
        # marking the instance as ready before subclass initialization completes.
        self._making_ready = False

    async def make_ready(self) -> None:
        """Base implementation.

        If called directly on AsyncLazyReady (or if a subclass does not override it),
        marks the instance as ready. If invoked via super().make_ready() inside an
        overridden subclass method, this is a no-op to allow the subclass to complete
        its initialization before marking readiness.
        """
        if not getattr(self, "_making_ready", False):
            self._ready = True

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        # Transparently wrap subclass make_ready() so that self._ready is only set to
        # True if the entire initialization chain succeeds, even when make_ready() is
        # invoked directly (e.g. as an on-device resume or boot callback).
        if "make_ready" in cls.__dict__:
            original_make_ready = cls.__dict__["make_ready"]

            @functools.wraps(original_make_ready)
            async def wrapped_make_ready(
                self: AsyncLazyReady, *args: Any, **kwargs: Any
            ) -> None:
                # If already within an outer make_ready() call (e.g. subclass super() call),
                # delegate directly to the method.
                if getattr(self, "_making_ready", False):
                    await original_make_ready(self, *args, **kwargs)
                    return

                self._making_ready = True
                self._ready = False
                success = False
                try:
                    await original_make_ready(self, *args, **kwargs)
                    success = True
                finally:
                    # Using finally ensures self._ready is False on any error or cancellation
                    # (including asyncio.CancelledError / BaseException) while letting exceptions
                    # bubble up untouched.
                    self._ready = success
                    self._making_ready = False

            cls.make_ready = wrapped_make_ready  # type: ignore[method-assign]


def ensure_ready(
    method: Callable[P, Coroutine[Any, Any, T]]
) -> Callable[P, Coroutine[Any, Any, T]]:
    @functools.wraps(method)
    async def wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
        self: AsyncLazyReady = args[0]  # type: ignore[assignment]
        assert isinstance(self, AsyncLazyReady)
        # Avoid re-entering make_ready() if a subclass make_ready() calls an @ensure_ready method.
        if not self._ready and not getattr(
            self, "_making_ready", False
        ):  # pylint: disable=protected-access
            await self.make_ready()
        return await method(*args, **kwargs)

    return wrapper
