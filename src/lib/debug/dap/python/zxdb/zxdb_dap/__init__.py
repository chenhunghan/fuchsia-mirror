# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from pydap.client import DapClient
from pydap.models import ThreadEvent
from zxdb_dap.models import (
    AsyncBacktraceUpdate,
    AsyncTaskNode,
    UnwinderType,
    ZxdbAsyncBacktraceArguments,
    ZxdbAsyncBacktraceResponse,
    ZxdbAsyncBacktraceResponseBody,
    ZxdbConsoleCommandArguments,
    ZxdbConsoleCommandResponse,
    ZxdbConsoleCommandResponseBody,
    ZxdbPauseArguments,
    ZxdbProcessStoppedEvent,
    ZxdbProcessStoppedEventBody,
    ZxdbSetTracepointsArguments,
    ZxdbSetTracepointsResponse,
    ZxdbSetTracepointsResponseBody,
    ZxdbStackFrame,
    ZxdbStackTraceResponse,
    ZxdbStackTraceResponseBody,
    ZxdbThread,
    ZxdbThreadEvent,
    ZxdbThreadEventBody,
    ZxdbThreadsResponse,
    ZxdbThreadsResponseBody,
    ZxdbTracepointEvaluation,
    ZxdbTracepointEvent,
    ZxdbTracepointEventBody,
    ZxdbTracepointFrameCapture,
    ZxdbTracepointFrameRecord,
    ZxdbTracepointFrameSummary,
    ZxdbTracepointInfo,
    ZxdbTracepointSpec,
    ZxdbTracepointValue,
    ZxdbTracepointWatchSpec,
)
from zxdb_dap.zxdb_dap_mixin import (
    ZxdbDapMixin,
    ZxdbDetachArguments,
    ZxdbProcessArguments,
    ZxdbProcessInfo,
    ZxdbProcessResponse,
    ZxdbProcessResponseBody,
    ZxdbStackTraceArguments,
)


class ZxdbDapClient(ZxdbDapMixin, DapClient):
    pass


__all__ = [
    # keep-sorted start
    "AsyncBacktraceUpdate",
    "AsyncTaskNode",
    "ThreadEvent",
    "UnwinderType",
    "ZxdbAsyncBacktraceArguments",
    "ZxdbAsyncBacktraceResponse",
    "ZxdbAsyncBacktraceResponseBody",
    "ZxdbConsoleCommandArguments",
    "ZxdbConsoleCommandResponse",
    "ZxdbConsoleCommandResponseBody",
    "ZxdbDapClient",
    "ZxdbDapMixin",
    "ZxdbDetachArguments",
    "ZxdbPauseArguments",
    "ZxdbProcessArguments",
    "ZxdbProcessInfo",
    "ZxdbProcessResponse",
    "ZxdbProcessResponseBody",
    "ZxdbProcessStoppedEvent",
    "ZxdbProcessStoppedEventBody",
    "ZxdbSetTracepointsArguments",
    "ZxdbSetTracepointsResponse",
    "ZxdbSetTracepointsResponseBody",
    "ZxdbStackFrame",
    "ZxdbStackTraceArguments",
    "ZxdbStackTraceResponse",
    "ZxdbStackTraceResponseBody",
    "ZxdbThread",
    "ZxdbThreadEvent",
    "ZxdbThreadEventBody",
    "ZxdbThreadsResponse",
    "ZxdbThreadsResponseBody",
    "ZxdbTracepointEvaluation",
    "ZxdbTracepointEvent",
    "ZxdbTracepointEventBody",
    "ZxdbTracepointFrameCapture",
    "ZxdbTracepointFrameRecord",
    "ZxdbTracepointFrameSummary",
    "ZxdbTracepointInfo",
    "ZxdbTracepointSpec",
    "ZxdbTracepointValue",
    "ZxdbTracepointWatchSpec",
    # keep-sorted end
]
