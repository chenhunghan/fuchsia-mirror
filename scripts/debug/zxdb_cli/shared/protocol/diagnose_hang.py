# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict, Field
from shared.protocol.base import BaseRequest
from shared.protocol.handles import HandleEntry
from zxdb_dap import AsyncTaskNode

COMMAND_NAME: Final = "diagnose-hang"

__all__ = [
    "COMMAND_NAME",
    "ClassifiedThread",
    "DiagnoseHangRequest",
    "DiagnoseHangResponse",
    "ThreadHangClassification",
]

ThreadHangClassification = Literal[
    "blocked_lock",
    "blocked_ipc",
    "port_wait",
    "interrupt_wait",
    "sleeping",
    "active",
    "unknown",
]


class ClassifiedThread(BaseModel):
    """Stack and concurrency classification for a target thread."""

    model_config = ConfigDict(extra="forbid")

    thread_id: int = Field(description="Thread KOID.")
    name: str = Field(description="Thread name.")
    classification: ThreadHangClassification = Field(
        description=(
            "Concurrency state classification: blocked_lock, blocked_ipc,"
            " port_wait, interrupt_wait, sleeping, active, or unknown."
        )
    )
    blocking_symbol: str | None = Field(
        default=None,
        description=(
            "Top blocking syscall or synchronization primitive symbol, if"
            " applicable."
        ),
    )
    summary: str = Field(
        description=(
            "Concise human/agent-readable explanation of what this thread is"
            " doing."
        )
    )
    top_frames: list[str] = Field(
        default_factory=list,
        description="Formatted top stack frames for this thread.",
    )


class DiagnoseHangResponse(BaseModel):
    """Single-shot concurrency and hang snapshot of a process."""

    model_config = ConfigDict(extra="forbid")

    pid: int | None = Field(
        default=None,
        description="Target process KOID.",
    )
    dwell_ms: float = Field(
        default=0.0,
        description=(
            "Total time (ms) the process was paused during the snapshot."
        ),
    )
    auto_resumed: bool = Field(
        default=True,
        description=(
            "Whether threads paused for the snapshot were automatically"
            " resumed."
        ),
    )
    summary: list[str] = Field(
        default_factory=list,
        description=(
            "High-level diagnostic findings (blocked locks, sync IPCs, async"
            " tasks, unserviced interrupts)."
        ),
    )
    threads: list[ClassifiedThread] = Field(
        default_factory=list,
        description=(
            "All threads in the process classified by their current"
            " wait/execution state."
        ),
    )
    async_tasks: list[AsyncTaskNode] = Field(
        default_factory=list,
        description=(
            "Active async task trees in the process (from zxdb"
            " async-backtrace)."
        ),
    )
    interrupt_handles: list[HandleEntry] = Field(
        default_factory=list,
        description="Open ZX_OBJ_TYPE_INTERRUPT handles in the process.",
    )
    channel_handles: list[HandleEntry] = Field(
        default_factory=list,
        description="Open ZX_OBJ_TYPE_CHANNEL handles in the process.",
    )
    port_handles: list[HandleEntry] = Field(
        default_factory=list,
        description="Open ZX_OBJ_TYPE_PORT handles in the process.",
    )


class DiagnoseHangRequest(BaseRequest[DiagnoseHangResponse]):
    """Capture a hang/concurrency diagnostic snapshot of a process."""

    command: Literal["diagnose-hang"] = COMMAND_NAME
    pid: int | None = Field(
        default=None,
        description="Target process KOID (defaults to attached process).",
    )
    auto_resume: bool = Field(
        default=True,
        description=(
            "Automatically resume threads paused during snapshot (guaranteed"
            " via finally)."
        ),
    )
    response_type: ClassVar[Any] = DiagnoseHangResponse
