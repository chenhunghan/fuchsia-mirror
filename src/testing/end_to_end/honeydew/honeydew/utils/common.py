# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Common utils used across Honeydew."""

import asyncio
import inspect
import logging
import math
import os
import signal
import time
import types
from collections.abc import Awaitable, Callable, Generator, Mapping
from contextlib import contextmanager
from typing import Any

from honeydew import errors
from honeydew.utils import decorators
from mobly import signals

_LOGGER: logging.Logger = logging.getLogger(__name__)


def is_infra(environ: Mapping[str, str] | None = None) -> bool:
    """Returns True if running in a Fuchsia infrastructure environment."""
    if environ is None:
        environ = os.environ
    return (
        environ.get("BOTANIST_CONFIG") is not None
        or environ.get("SWARMING_TASK_ID") is not None
    )


def _retry_condition(end_time: float | None = None) -> bool:
    if end_time is None:
        return True

    return time.time() < end_time


@decorators.liveness_check
async def wait_for_state(
    state_fn: Callable[[], bool] | Callable[[], Awaitable[bool]],
    expected_state: bool,
    timeout: float | None = None,
    wait_time: float = 1,
) -> None:
    """Wait for specified time for state_fn to return expected_state.

    Args:
        state_fn: function to call for getting current state.
        expected_state: expected state to wait for.
        timeout: How long in sec to wait. By default, no timeout is set (None or 0 means wait indefinitely).
        wait_time: How long in sec to wait between the retries.

    Raises:
        errors.HoneydewTimeoutError: If state_fn does not return the
            expected_state with in specified timeout.
    """
    message: str = (
        f"Waiting for {state_fn.__qualname__} to return {expected_state}..."
    )
    end_time: float | None = None

    if timeout:
        start_time: float = time.time()
        end_time = start_time + timeout

        message = (
            f"Waiting for {timeout} sec for {state_fn.__qualname__} "
            f"to return {expected_state}..."
        )

    _LOGGER.info(message)

    while _retry_condition(end_time):
        _LOGGER.debug("calling %s", state_fn.__qualname__)
        try:
            current_state = state_fn()
            if inspect.isawaitable(current_state):
                current_state = await current_state

            _LOGGER.debug(
                "%s returned %s", state_fn.__qualname__, current_state
            )
            if current_state == expected_state:
                return
        except signals.TestSignal:
            # Mobly signals (e.g. `TestAbortAll` on test timeout) must stop the
            # test, so propagate them instead of retrying.
            raise
        except Exception as err:  # pylint: disable=broad-except
            # `state_fn()` raised an exception. Retry again
            _LOGGER.debug(err)
        await asyncio.sleep(wait_time)
    else:
        message = f"{state_fn.__qualname__} didn't return {expected_state}"
        if timeout:
            message += f" in {timeout} sec"
        raise errors.HoneydewTimeoutError(message)


@decorators.liveness_check
def wait_for_state_sync(
    state_fn: Callable[[], bool],
    expected_state: bool,
    timeout: float | None = None,
    wait_time: float = 1,
) -> None:
    """Wait for specified time for state_fn to return expected_state synchronously.

    Args:
        state_fn: function to call for getting current state.
        expected_state: expected state to wait for.
        timeout: How long in sec to wait. By default, no timeout is set (None or 0 means wait indefinitely).
        wait_time: How long in sec to wait between the retries.

    Raises:
        errors.HoneydewTimeoutError: If state_fn does not return the
            expected_state with in specified timeout.
    """
    message: str = (
        f"Waiting for {state_fn.__qualname__} to return {expected_state}..."
    )
    end_time: float | None = None

    if timeout:
        start_time: float = time.time()
        end_time = start_time + timeout

        message = (
            f"Waiting for {timeout} sec for {state_fn.__qualname__} "
            f"to return {expected_state}..."
        )

    _LOGGER.info(message)

    while _retry_condition(end_time):
        _LOGGER.debug("calling %s", state_fn.__qualname__)
        try:
            current_state = state_fn()
            _LOGGER.debug(
                "%s returned %s", state_fn.__qualname__, current_state
            )
            if current_state == expected_state:
                return
        except signals.TestSignal:
            # Mobly signals (e.g. `TestAbortAll` on test timeout) must stop the
            # test, so propagate them instead of retrying.
            raise
        except Exception as err:  # pylint: disable=broad-except
            # `state_fn()` raised an exception. Retry again
            _LOGGER.debug(err)
        time.sleep(wait_time)
    else:
        message = f"{state_fn.__qualname__} didn't return {expected_state}"
        if timeout:
            message += f" in {timeout} sec"
        raise errors.HoneydewTimeoutError(message)


@decorators.liveness_check
async def retry(
    fn: Callable[[], object] | Callable[[], Awaitable[object]],
    timeout: float | None = None,
    wait_time: int = 1,
) -> object:
    """Wait for specified time for fn to succeed.

    Args:
        fn: function to call.
        timeout: How long in sec to retry in case of failure. By default, no
            timeout is set.
        wait_time: How long in sec to wait between the retries.

    Raises:
        errors.HoneydewTimeoutError: If fn does not succeed with in specified
            timeout.
    """
    message: str = (
        f"Run {fn.__qualname__} until it succeeds, with wait time "
        f"of {wait_time}sec between the retries..."
    )
    end_time: float | None = None

    if timeout:
        start_time: float = time.time()
        end_time = start_time + timeout

        message = (
            f"Run {fn.__qualname__} until it succeeds or {timeout}sec timeout "
            f"hit, with wait time of {wait_time}sec between the retries..."
        )

    _LOGGER.info(message)

    while _retry_condition(end_time):
        _LOGGER.debug("calling %s", fn.__qualname__)
        try:
            ret_value = fn()
            if inspect.isawaitable(ret_value):
                ret_value = await ret_value

            _LOGGER.debug("%s returned %s", fn.__qualname__, ret_value)
            _LOGGER.info("Successfully finished %s.", fn.__qualname__)
            return ret_value
        except signals.TestSignal:
            # Mobly signals (e.g. `TestAbortAll` on test timeout) must stop the
            # test, so propagate them instead of retrying.
            raise
        except Exception as err:  # pylint: disable=broad-except
            # `fn()` raised an exception. Retry again
            _LOGGER.warning(
                "%s failed with error: %s. Retry again in %s sec",
                fn.__qualname__,
                err,
                wait_time,
            )
        await asyncio.sleep(wait_time)
    else:
        raise errors.HoneydewTimeoutError(
            f"{fn.__qualname__} didn't succeed in {timeout} sec"
        )


def read_from_dict(
    d: dict[str, Any],
    key_path: tuple[str, ...],
    should_exist: bool = True,
) -> Any:
    """Given a (nested) dictionary and path to the key, return value at that key.

    Args:
        d: Input nested dictionary
        key_path: path from root key to destination key
        should_exist: If set to True, exception will be raised if key does not exist. Otherwise,
            returns None.

    Returns:
        Value of the key if exist. Return None or raise exception, otherwise

    Raises:
        errors.ConfigError: Failed to traverse to the key using path
    """
    try:
        traversed_path: list[str] = []
        item_value: Any = d
        for item in key_path:
            item_value = item_value[item]
            traversed_path.append(item)
        return item_value
    except KeyError as err:
        traversed_path.append(item)
        _LOGGER.info(
            "'%s' does not exist in the dict: %s",
            traversed_path,
            d,
        )
        if should_exist:
            raise errors.ConfigError(
                f"'{traversed_path}' does not exist in the config dict passed during init."
            ) from err
        return None


class _TimeLimitExpired(BaseException):
    """Internal BaseException raised by SIGALRM in time_limit to unwind the stack
    without being caught or swallowed by inner `except Exception:` or
    `except HoneydewTimeoutError:` blocks.

    Attributes:
        owner: The `sigalarm_handler` function that raised this exception. When
            `time_limit` calls are nested, only the `time_limit` frame whose
            `sigalarm_handler` matches `owner` converts `_TimeLimitExpired` into
            `exception_type`; inner frames re-raise it so an expired outer
            `time_limit` unwinds all the way to the outer frame without being
            caught by inner `except Exception:` / `except HoneydewError:` blocks.
    """

    def __init__(self, message: str, owner: object) -> None:
        super().__init__(message)
        self.owner = owner


@contextmanager
def time_limit(
    timeout: int,
    exception_type: type[Exception] = errors.HoneydewTimeoutError,
    exception_message: str = "",
) -> Generator[None, None, None]:
    """Re-entrant context manager that can be used to time limit any function/method.

    Supports arbitrary nesting of `with common.time_limit(...):` blocks using the
    process-wide `SIGALRM` timer:
    - On entry, any active outer `SIGALRM` handler (`prev_handler`) and its
      remaining alarm time (`prev_remaining = signal.alarm(0)`) are saved on the
      current stack frame.
    - Whichever timer has the earlier deadline (this block's `timeout` or the
      outer timer's `prev_remaining`) remains the active `SIGALRM` handler and
      alarm duration, so an inner `time_limit` can never extend an outer
      deadline.
    - On exit (`finally`), the outer `SIGALRM` handler is restored and the outer
      alarm is re-armed with its remaining time (`prev_remaining - elapsed`),
      preventing an inner `time_limit` from disarming an outer `time_limit`.

    Nested timeout call flows:
    1. Outer deadline is shorter (e.g., outer `time_limit(60)` -> inner
       `time_limit(90)`, and 60s expires):
       - The outer handler stays registered (`90 >= 60`) with `signal.alarm(60)`.
       - At 60s, the outer handler raises `_TimeLimitExpired(owner=outer)`.
       - The inner `time_limit` is immediately aborted: its `except` block sees
         `err.owner is not inner_handler`, sets `outer_expired = True`, and
         re-raises `_TimeLimitExpired` without converting it (so inner
         `except Exception:` / `except HoneydewError:` blocks cannot swallow it)
         and without re-arming the alarm in `finally`.
       - The outer `time_limit` catches its own `_TimeLimitExpired`, raises the
         outer `exception_type`, and clears `signal.alarm(0)` in `finally`.
    2. Inner deadline is shorter (e.g., outer `time_limit(75)` -> inner
       `time_limit(50)`, and 50s expires):
       - The inner handler is installed (`50 < 75`) with `signal.alarm(50)`.
       - At 50s, the inner handler raises `_TimeLimitExpired(owner=inner)`.
       - The inner `time_limit` catches it, raises the inner `exception_type`,
         and in `finally` restores the outer handler and re-arms the remaining
         outer time (`signal.alarm(75 - 50 = 25)`).
       - If the outer block catches the inner exception, the remaining 25s
         outer timer continues counting down for the rest of the outer block;
         if the outer block does not catch it, the exception exits the outer
         block too and the outer `finally` immediately clears `signal.alarm(0)`.

    Args:
        timeout: time limit allowed in seconds.
        exception_type: type of exception to be raised on reaching the time limit. If not passed,
            errors.HoneydewTimeoutError exception will be used.
        exception_message: Error message to be used (if any) while raising the exception on reaching
            the time limit.

    Raises:
        errors.HoneydewTimeoutError: If time limit is reached and exception_type is not passed.
    """

    def sigalarm_handler(signum: int, _: types.FrameType | None) -> None:
        _LOGGER.debug(
            "Received signal: %s (%s). Raising exception: %s('%s')",
            signal.Signals(signum).name,
            signum,
            exception_type.__name__,
            exception_message,
        )
        raise _TimeLimitExpired(exception_message, owner=sigalarm_handler)

    _LOGGER.info("Setting a time limit of %s sec...", timeout)
    start_time: float = time.time()
    # Save any active outer SIGALRM handler and cancel/read its remaining seconds
    # so we can restore it when exiting this nested context.
    prev_handler = signal.getsignal(signal.SIGALRM)
    prev_remaining: int = signal.alarm(0)
    outer_expired: bool = False

    if prev_remaining == 0 or timeout < prev_remaining:
        # Either no outer timer is active, or this inner timer has a shorter
        # deadline and will expire before the outer timer.
        signal.signal(signal.SIGALRM, sigalarm_handler)
        signal.alarm(timeout)
    else:
        # An outer timer is already active and will expire before (or at the
        # same time as) this inner timer; keep the outer handler and its
        # remaining alarm duration so the outer deadline takes precedence.
        signal.alarm(prev_remaining)

    try:
        yield
        _LOGGER.info(
            "Finished within the time limit of %s sec (took %.2f sec).",
            timeout,
            time.time() - start_time,
        )
    except _TimeLimitExpired as err:
        if err.owner is not sigalarm_handler:
            # This timeout was raised by an enclosing outer `time_limit` whose
            # deadline was shorter than ours. Re-raise `_TimeLimitExpired`
            # untouched so it bypasses any inner `except Exception:` handlers
            # and is caught by the outer `time_limit` that owns it.
            outer_expired = True
            raise
        _LOGGER.warning(
            "Time limit of %s sec reached. Raising %s('%s')",
            timeout,
            exception_type.__name__,
            exception_message,
        )
        raise exception_type(exception_message) from err
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, prev_handler)
        if prev_remaining > 0 and not outer_expired:
            # Subtract the time spent inside this inner block and re-arm the
            # enclosing outer timer with its remaining seconds.
            elapsed: float = time.time() - start_time
            remaining_outer: int = max(1, math.ceil(prev_remaining - elapsed))
            signal.alarm(remaining_outer)
