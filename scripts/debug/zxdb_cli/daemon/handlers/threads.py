# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import TYPE_CHECKING

from shared.protocol.base import Response
from shared.protocol.threads import (
    COMMAND_NAME,
    ThreadEntry,
    ThreadsRequest,
    ThreadsResponse,
)

__all__ = ["COMMAND_NAME", "handle"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon


async def handle(
    daemon: Daemon, _req: ThreadsRequest
) -> Response[ThreadsResponse]:
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    try:
        # TODO(https://fxbug.dev/552813275): Wrap zxdb requests with
        # capability.enable(zxdb) before calling zxdb_threads.
        resp = await daemon.dap_client.zxdb_threads()
        if resp.body is not None and resp.body.threads is not None:
            daemon.update_thread_cache(resp.body.threads)
            thread_entries = [
                ThreadEntry(
                    thread_id=t.id,
                    name=t.name,
                    process_id=t.process_id,
                )
                for t in resp.body.threads
            ]
        else:
            thread_entries = []
        return Response(
            success=True,
            body=ThreadsResponse(threads=thread_entries),
        )
    except Exception as e:
        return Response(success=False, message=f"Failed to get threads: {e}")
