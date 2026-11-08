# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from pydap.models import StackTraceArguments

__all__ = ["FrameResolutionError", "resolve_frame_id"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


class FrameResolutionError(Exception):
    """A non-DAP precondition of resolving a frame failed.

    DAP request failures are reported as `DapError`, whose message already
    names the failing request. This covers the remaining checks, e.g. pausing
    the thread or an out-of-range frame index.
    """


async def resolve_frame_id(
    daemon: Daemon, thread_id: int, frame_index: int
) -> int:
    """Resolves the DAP frameId for evaluating an expression on a thread."""
    try:
        await daemon.ensure_stopped(thread_id)
    except Exception as e:
        # ensure_stopped raises DapError from the pause request, or a plain
        # Exception if the thread does not report stopped in time.
        raise FrameResolutionError(
            f"pause thread {thread_id} failed: {e}"
        ) from e
    stack_resp = await daemon.dap_client.stack_trace(
        StackTraceArguments(thread_id=thread_id)
    )
    if not stack_resp.body or not stack_resp.body.stack_frames:
        raise FrameResolutionError(
            f"stackTrace returned no frames for thread {thread_id}"
        )
    num_frames = len(stack_resp.body.stack_frames)
    if frame_index < 0 or frame_index >= num_frames:
        raise FrameResolutionError(
            f"frame index {frame_index} out of range for thread {thread_id}"
            f" (stack has {num_frames} frames)"
        )
    return stack_resp.body.stack_frames[frame_index].id
