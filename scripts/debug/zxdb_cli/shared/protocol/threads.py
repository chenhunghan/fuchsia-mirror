# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import BaseModel, ConfigDict
from shared.protocol.base import BaseRequest

COMMAND_NAME: Final = "threads"


class ThreadEntry(BaseModel):
    """Information about an active thread."""

    model_config = ConfigDict(extra="forbid")

    thread_id: int
    name: str
    process_id: int | None = None


class ThreadsResponse(BaseModel):
    """Response for threads command."""

    model_config = ConfigDict(extra="forbid")

    threads: list[ThreadEntry]


class ThreadsRequest(BaseRequest[ThreadsResponse]):
    """Request list of threads."""

    command: Literal["threads"] = COMMAND_NAME
    response_type: ClassVar[Any] = ThreadsResponse
