# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

from typing import TYPE_CHECKING

from daemon.handlers.frame_utils import resolve_frame_id
from pydap.models import DataBreakpointInfoArguments
from shared.protocol.base import Response
from shared.protocol.tracepoint import (
    COMMAND_NAME,
    TracepointInfo,
    TracepointRequest,
    TracepointResponse,
)
from zxdb_dap.models import ZxdbTracepointSpec, ZxdbTracepointWatchSpec

__all__ = ["COMMAND_NAME", "handle", "sync_tracepoints_with_dap"]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

_MAX_DAP_INT64 = (1 << 63) - 1


def _parse_hex_address(expr: str) -> tuple[int | None, str | None]:
    """Parses a 0x-prefixed hex address within signed 64-bit DAP bounds."""
    cleaned = expr.strip()
    lower = cleaned.lower()
    if lower.startswith("-0x"):
        return None, f"Watch address '{cleaned}' must be non-negative"
    if not lower.startswith("0x"):
        return None, None
    try:
        addr = int(cleaned, 16)
    except ValueError:
        return None, None
    if addr < 0 or addr > _MAX_DAP_INT64:
        return (
            None,
            f"Watch address '{cleaned}' exceeds signed 64-bit integer range",
        )
    return addr, None


def _matches_code_location(tp: TracepointInfo, req: TracepointRequest) -> bool:
    """Returns True if tp has the same function or file:line target as req."""
    if req.function is not None and tp.function == req.function:
        return True
    return (
        req.file is not None
        and req.line is not None
        and tp.file == req.file
        and tp.line == req.line
    )


def _matches_target_location(
    tp: TracepointInfo, req: TracepointRequest, data_id: str | None
) -> bool:
    """Returns True if tp has the same resolved target location as req.

    The same watch expression resolves to a different dataId in another
    thread, frame, or process, so watch tracepoints match on the resolved
    dataId as well as the expression text.
    """
    if _matches_code_location(tp, req):
        return True
    return (
        req.watch is not None
        and tp.watch == req.watch
        and tp.data_id == data_id
    )


def _matches_request(tp: TracepointInfo, req: TracepointRequest) -> bool:
    """Returns True if tp matches req by ID when set, or by target text."""
    if req.tracepoint_id is not None:
        return tp.id == req.tracepoint_id
    if _matches_code_location(tp, req):
        return True
    return req.watch is not None and tp.watch == req.watch


def _format_tracepoint_location(req: TracepointRequest) -> str:
    """Formats a human-readable location label for a tracepoint request."""
    if req.function is not None:
        return req.function
    if req.watch is not None:
        return f"watch:{req.watch}"
    return f"{req.file}:{req.line}"


def _dap_enabled(tp: TracepointInfo, rearm: bool) -> bool | None:
    """Returns the enabled value to send for tp in a zxdb.SetTracepoints sync.

    Only explicit user actions send a value. A user disable sends False, and
    rearm sends True. Otherwise the field is omitted so that the server keeps
    its own auto-disabled state, even when the daemon has not yet processed
    the zxdb.Tracepoint event that reported it.
    """
    if rearm:
        return True
    if not tp.enabled and not tp.auto_disabled:
        return False
    return None


def _build_dap_spec(
    tp: TracepointInfo, rearm: bool = False
) -> ZxdbTracepointSpec:
    """Converts a daemon TracepointInfo into a ZxdbTracepointSpec wire model."""
    watch_spec: ZxdbTracepointWatchSpec | None = None
    if tp.watch is not None:
        address_val, _ = _parse_hex_address(tp.watch)
        watch_spec = ZxdbTracepointWatchSpec(
            data_id=tp.data_id,
            address=address_val if tp.data_id is None else None,
            expression=tp.watch,
            byte_size=tp.size,
            access_type=tp.access_type or "write",
        )
    return ZxdbTracepointSpec(
        id=tp.id,
        function=tp.function,
        file=tp.file,
        line=tp.line,
        watch=watch_spec,
        condition=tp.condition,
        expressions=list(tp.expressions) if tp.expressions else None,
        max_matching_frames=tp.max_matching_frames,
        max_hits=tp.max_hits,
        frame="frame" in tp.capture,
        backtrace="backtrace" in tp.capture,
        raw_backtrace=tp.raw_backtrace,
        locals="locals" in tp.capture,
        registers="registers" in tp.capture,
        enabled=_dap_enabled(tp, rearm),
    )


async def sync_tracepoints_with_dap(
    daemon: Daemon, rearm_id: int | None = None
) -> None:
    """Synchronizes daemon tracepoints with the zxdb DAP server.

    rearm_id names the tracepoint that the user explicitly set or enabled in
    this request. The server re-enables it even if it was auto-disabled.
    """
    specs = [
        _build_dap_spec(tp, rearm=tp_id == rearm_id)
        for tp_id, tp in daemon.tracepoints.items()
    ]
    dap_resp = await daemon.dap_client.zxdb_set_tracepoints(specs)
    info_by_id = {info.id: info for info in dap_resp.body.tracepoints}
    for tp_id, tp in daemon.tracepoints.items():
        if (info := info_by_id.get(tp_id)) is not None:
            tp.verified = info.verified
            tp.hit_count = info.hit_count
            tp.enabled = info.enabled
            tp.auto_disabled = info.auto_disabled
            tp.message = info.message


async def _handle_clear(daemon: Daemon) -> Response[TracepointResponse]:
    """Deletes all tracepoints and syncs an empty list with the DAP server."""
    removed = dict(daemon.tracepoints)
    daemon.tracepoints.clear()
    try:
        await sync_tracepoints_with_dap(daemon)
    except Exception as e:
        daemon.tracepoints.update(removed)
        return Response(
            success=False,
            message=f"Failed to sync tracepoints with DAP: {e}",
        )
    return Response(success=True, body=TracepointResponse(tracepoints=[]))


async def _handle_delete(
    daemon: Daemon, req: TracepointRequest
) -> Response[TracepointResponse]:
    """Deletes tracepoints matching req and syncs with the DAP server."""
    to_remove = [
        tp_id
        for tp_id, tp in daemon.tracepoints.items()
        if _matches_request(tp, req)
    ]
    if not to_remove:
        return Response(
            success=False,
            message="No matching tracepoint found to delete.",
        )
    if req.watch is not None and len(to_remove) > 1:
        ids = ", ".join(str(tp_id) for tp_id in to_remove)
        return Response(
            success=False,
            message=(
                f"Multiple watch tracepoints match '{req.watch}' ({ids})."
                " Delete by tracepoint ID."
            ),
        )
    removed = {tp_id: daemon.tracepoints.pop(tp_id) for tp_id in to_remove}
    try:
        await sync_tracepoints_with_dap(daemon)
    except Exception as e:
        daemon.tracepoints.update(removed)
        return Response(
            success=False,
            message=f"Failed to sync tracepoints with DAP: {e}",
        )
    return Response(
        success=True,
        body=TracepointResponse(tracepoints=list(daemon.tracepoints.values())),
    )


async def _handle_set_enabled(
    daemon: Daemon, req: TracepointRequest
) -> Response[TracepointResponse]:
    """Enables or disables an existing tracepoint by ID and syncs with DAP."""
    assert req.tracepoint_id is not None and req.enabled is not None
    tp = daemon.tracepoints.get(req.tracepoint_id)
    if tp is None:
        return Response(
            success=False,
            message=f"Tracepoint {req.tracepoint_id} not found.",
        )
    prev_tp = tp.model_copy(deep=True)
    if req.enabled:
        if not tp.enabled:
            tp.enabled = True
            tp.auto_disabled = False
            tp.hit_count = 0
    else:
        tp.enabled = False
        tp.auto_disabled = False

    rearm_id = req.tracepoint_id if req.enabled else None
    try:
        await sync_tracepoints_with_dap(daemon, rearm_id=rearm_id)
    except Exception as e:
        daemon.tracepoints[req.tracepoint_id] = prev_tp
        return Response(
            success=False,
            message=f"Failed to sync tracepoints with DAP: {e}",
        )
    return Response(
        success=True,
        body=TracepointResponse(tracepoints=list(daemon.tracepoints.values())),
    )


def _resolve_target_id(
    daemon: Daemon, req: TracepointRequest, data_id: str | None
) -> tuple[int | None, str | None]:
    """Resolves or allocates the tracepoint ID while rejecting cross-ID collisions."""
    loc_owner_id = next(
        (
            tp_id
            for tp_id, tp in daemon.tracepoints.items()
            if _matches_target_location(tp, req, data_id)
        ),
        None,
    )
    if req.tracepoint_id is not None:
        if loc_owner_id is not None and loc_owner_id != req.tracepoint_id:
            loc = _format_tracepoint_location(req)
            return (
                None,
                f"Location '{loc}' is already used by tracepoint {loc_owner_id}",
            )
        tp_id = req.tracepoint_id
        daemon.next_tracepoint_id = max(daemon.next_tracepoint_id, tp_id + 1)
        return tp_id, None

    if loc_owner_id is not None:
        return loc_owner_id, None

    tp_id = daemon.next_tracepoint_id
    daemon.next_tracepoint_id += 1
    return tp_id, None


async def _resolve_watch_data_id(
    daemon: Daemon, req: TracepointRequest
) -> tuple[str | None, str | None]:
    """Resolves the opaque server dataId for a watchpoint tracepoint."""
    if req.watch is None:
        return None, None

    if (
        req.tracepoint_id is not None
        and (prev_tp := daemon.tracepoints.get(req.tracepoint_id)) is not None
        and prev_tp.watch == req.watch
        and prev_tp.data_id is not None
    ):
        return prev_tp.data_id, None

    hex_addr, hex_err = _parse_hex_address(req.watch)
    if hex_err is not None:
        return None, hex_err
    if hex_addr is not None:
        return None, None

    if req.thread_id is None:
        return (
            None,
            "thread_id (-t/--thread-id) is required when setting a watchpoint"
            " tracepoint on an expression",
        )

    frame_id = await resolve_frame_id(daemon, req.thread_id, req.frame_index)
    info_resp = await daemon.dap_client.data_breakpoint_info(
        DataBreakpointInfoArguments(name=req.watch, frame_id=frame_id)
    )
    if not info_resp.body or not info_resp.body.data_id:
        desc = (
            info_resp.body.description
            if info_resp.body and info_resp.body.description
            else "no dataId returned"
        )
        return None, f"Cannot set watchpoint on '{req.watch}': {desc}"

    return info_resp.body.data_id, None


async def _rollback_tracepoint(
    daemon: Daemon, tp_id: int, prev_tp: TracepointInfo | None
) -> None:
    """Restores `prev_tp` (and re-syncs with DAP) or removes `tp_id` on failure."""
    if prev_tp is not None:
        daemon.tracepoints[tp_id] = prev_tp
        await sync_tracepoints_with_dap(daemon)
    else:
        daemon.tracepoints.pop(tp_id, None)


async def _handle_set(
    daemon: Daemon, req: TracepointRequest
) -> Response[TracepointResponse]:
    """Configures or updates a tracepoint and syncs with the DAP server."""
    data_id, err = await _resolve_watch_data_id(daemon, req)
    if err is not None:
        return Response(success=False, message=err)

    tp_id, id_err = _resolve_target_id(daemon, req, data_id)
    if id_err is not None or tp_id is None:
        return Response(success=False, message=id_err)

    prev_existing = daemon.tracepoints.get(tp_id)
    prev_tp = (
        prev_existing.model_copy(deep=True)
        if prev_existing is not None
        else None
    )

    daemon.tracepoints[tp_id] = TracepointInfo(
        id=tp_id,
        location=_format_tracepoint_location(req),
        file=req.file,
        line=req.line,
        function=req.function,
        watch=req.watch,
        data_id=data_id,
        size=req.size,
        access_type=req.access_type if req.watch is not None else None,
        condition=req.condition,
        expressions=list(req.expressions),
        capture=list(req.capture),
        raw_backtrace=req.raw_backtrace,
        max_matching_frames=req.max_matching_frames,
        max_hits=req.max_hits,
        hit_count=prev_tp.hit_count if prev_tp is not None else 0,
        enabled=True,
        auto_disabled=False,
        verified=False,
    )

    try:
        await sync_tracepoints_with_dap(daemon, rearm_id=tp_id)
    except Exception:
        if prev_tp is not None:
            daemon.tracepoints[tp_id] = prev_tp
        else:
            daemon.tracepoints.pop(tp_id, None)
        raise

    updated_tp = daemon.tracepoints[tp_id]
    if not updated_tp.verified and updated_tp.message:
        err_msg = updated_tp.message
        await _rollback_tracepoint(daemon, tp_id, prev_tp)
        return Response(success=False, message=err_msg)

    return Response(
        success=True,
        body=TracepointResponse(tracepoints=list(daemon.tracepoints.values())),
    )


async def handle(
    daemon: Daemon, req: TracepointRequest
) -> Response[TracepointResponse]:
    """Handles a tracepoint request to list, manage, delete, or configure."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    if req.list_all:
        return Response(
            success=True,
            body=TracepointResponse(
                tracepoints=list(daemon.tracepoints.values())
            ),
        )

    try:
        if req.clear:
            return await _handle_clear(daemon)
        if req.enabled is not None:
            return await _handle_set_enabled(daemon, req)
        if req.delete:
            return await _handle_delete(daemon, req)
        return await _handle_set(daemon, req)
    except Exception as e:
        return Response(
            success=False, message=f"Failed to manage tracepoint: {e}"
        )
