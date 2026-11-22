# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any, ClassVar, Final, Literal

from pydantic import Field
from shared.protocol.base import THREAD_ID_ALIAS, BaseRequest

COMMAND_NAME: Final = "continue"


class ContinueRequest(BaseRequest[dict[str, Any]]):
    """Request to resume execution of a thread."""

    command: Literal["continue"] = COMMAND_NAME
    thread_id: int = Field(validation_alias=THREAD_ID_ALIAS)
    single_thread: bool | None = None
    response_type: ClassVar[Any] = dict[str, Any]
