# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import time
from typing import TYPE_CHECKING, Any, Final

from daemon.handlers.handles import parse_handles_table_output
from pydap.models import StackTraceArguments
from shared.protocol.base import Response
from shared.protocol.diagnose_hang import (
    COMMAND_NAME,
    ClassifiedThread,
    DiagnoseHangRequest,
    DiagnoseHangResponse,
)
from shared.protocol.handles import HandleEntry
from zxdb_dap import (
    AsyncTaskNode,
    ZxdbAsyncBacktraceArguments,
    ZxdbConsoleCommandArguments,
)

__all__ = [
    "COMMAND_NAME",
    "classify_thread_stack",
    "handle",
    "resume_process_safely",
]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

_MAX_CLASSIFIED_FRAMES: Final[int] = 8
_MS_PER_SECOND: Final[float] = 1000.0

# Async task roots that group other tasks rather than being tasks themselves.
_SCOPE_ROOT_NAMES: Final[frozenset[str]] = frozenset({"fdf::Dispatcher"})

_LOCK_PATTERNS: Final[tuple[str, ...]] = (
    "sync_mutex_lock",
    "libsync::Mutex::Lock",
    "fbl::Mutex::Acquire",
    "pthread_mutex_lock",
    "std::__2::mutex::lock",
    "std::mutex::lock",
    "sync_completion_wait",
    "__zx_futex_wait",
    "_zx_futex_wait",
    "zx_futex_wait",
)

_IPC_PATTERNS: Final[tuple[str, ...]] = (
    "_zx_channel_call",
    "zx_channel_call",
    "ChannelTransport::Call",
    "WireSyncClient",
    "SyncClient",
)

_IRQ_WAIT_PATTERNS: Final[tuple[str, ...]] = (
    "_zx_interrupt_wait",
    "zx_interrupt_wait",
)

_PORT_WAIT_PATTERNS: Final[tuple[str, ...]] = (
    "_zx_port_wait",
    "zx_port_wait",
)

_SLEEP_PATTERNS: Final[tuple[str, ...]] = (
    "_zx_nanosleep",
    "zx_nanosleep",
    "_zx_object_wait_one",
    "zx_object_wait_one",
    "_zx_object_wait_many",
    "zx_object_wait_many",
)


def _format_frame(frame: Any) -> str:
    """Formats a DAP stack frame as 'function (file:line)'."""
    name = getattr(frame, "name", None) or "<unknown>"
    src = getattr(frame, "source", None)
    line = getattr(frame, "line", None)
    if src is not None:
        src_name = getattr(src, "name", None) or getattr(src, "path", None)
        if src_name and line:
            return f"{name} ({src_name}:{line})"
    return name


def _match_frame_pattern(
    frame_names: list[str], patterns: tuple[str, ...]
) -> tuple[int, str] | None:
    """Returns the index and first frame name matching any substring in patterns."""
    for idx, fname in enumerate(frame_names):
        if any(pat in fname for pat in patterns):
            return idx, fname
    return None


def _find_caller_frame(
    frame_names: list[str],
    wrapper_patterns: tuple[str, ...],
    fallback: str,
    start_index: int = 0,
) -> str:
    """Returns the first frame after start_index outside wrapper_patterns and the Zircon vDSO."""
    last_non_vdso = fallback
    for fn in frame_names[start_index:]:
        if not fn or "vdso" in fn.lower():
            continue
        if not any(p in fn for p in wrapper_patterns):
            return fn
        last_non_vdso = fn
    return last_non_vdso


def classify_thread_stack(
    thread_id: int, name: str, frames: list[Any]
) -> ClassifiedThread:
    """Classifies a thread's stack frames into a concurrency/hang state."""
    if not frames:
        return ClassifiedThread(
            thread_id=thread_id,
            name=name,
            classification="unknown",
            blocking_symbol=None,
            summary=f"Thread {thread_id} ({name}): no stack frames available.",
            top_frames=[],
        )

    top_slice = frames[:_MAX_CLASSIFIED_FRAMES]
    formatted = [_format_frame(f) for f in top_slice]
    frame_names = [getattr(f, "name", "") or "" for f in top_slice]

    if match := _match_frame_pattern(frame_names, _IPC_PATTERNS):
        idx, fname = match
        caller = _find_caller_frame(
            frame_names, _IPC_PATTERNS, fname, start_index=idx + 1
        )
        return ClassifiedThread(
            thread_id=thread_id,
            name=name,
            classification="blocked_ipc",
            blocking_symbol=fname,
            summary=(
                f"Thread {thread_id} ({name}) is blocked in a synchronous"
                f" IPC call ({fname}) from {caller}."
            ),
            top_frames=formatted,
        )

    # Mutex acquisitions block inside futex wait syscalls, so futex wait
    # primitives are included directly in _LOCK_PATTERNS and checked before
    # other wait/sleep states.
    if match := _match_frame_pattern(frame_names, _LOCK_PATTERNS):
        idx, fname = match
        caller = _find_caller_frame(
            frame_names, _LOCK_PATTERNS, fname, start_index=idx + 1
        )
        return ClassifiedThread(
            thread_id=thread_id,
            name=name,
            classification="blocked_lock",
            blocking_symbol=fname,
            summary=(
                f"Thread {thread_id} ({name}) is blocked on a lock/futex"
                f" ({fname}) called from {caller}."
            ),
            top_frames=formatted,
        )

    if match := _match_frame_pattern(frame_names, _IRQ_WAIT_PATTERNS):
        fname = match[1]
        return ClassifiedThread(
            thread_id=thread_id,
            name=name,
            classification="interrupt_wait",
            blocking_symbol=fname,
            summary=(
                f"Thread {thread_id} ({name}) is waiting on a"
                f" hardware/virtual interrupt ({fname})."
            ),
            top_frames=formatted,
        )

    if match := _match_frame_pattern(frame_names, _PORT_WAIT_PATTERNS):
        fname = match[1]
        return ClassifiedThread(
            thread_id=thread_id,
            name=name,
            classification="port_wait",
            blocking_symbol=fname,
            summary=(
                f"Thread {thread_id} ({name}) is idle waiting on a port"
                f" ({fname})."
            ),
            top_frames=formatted,
        )

    if match := _match_frame_pattern(frame_names, _SLEEP_PATTERNS):
        fname = match[1]
        return ClassifiedThread(
            thread_id=thread_id,
            name=name,
            classification="sleeping",
            blocking_symbol=fname,
            summary=(
                f"Thread {thread_id} ({name}) is sleeping or waiting on a"
                f" kernel object ({fname})."
            ),
            top_frames=formatted,
        )

    top_sym = frame_names[0]
    return ClassifiedThread(
        thread_id=thread_id,
        name=name,
        classification="active",
        blocking_symbol=None,
        summary=(
            f"Thread {thread_id} ({name}) is actively executing in {top_sym}."
        ),
        top_frames=formatted,
    )


async def resume_process_safely(daemon: Daemon, pid: int | None) -> bool:
    """Resumes the target process via a scoped console command, returning True if resumed."""
    try:
        cmd_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=daemon.scope_console_command("continue", pid)
            )
        )
        if cmd_resp.success:
            if pid is not None and (proc := daemon.processes.get(pid)):
                proc.resume()
            elif pid is None:
                for proc in daemon.processes.values():
                    proc.resume()
                for t in daemon.threads.values():
                    t.resume()
            return True
        return False
    except Exception:
        return False


async def _prefetch_process_handles(
    daemon: Daemon,
    pid: int | None = None,
) -> tuple[list[HandleEntry], list[HandleEntry], list[HandleEntry]]:
    """Fetches interrupt, channel, and port handles before pausing threads."""
    interrupt_handles: list[HandleEntry] = []
    channel_handles: list[HandleEntry] = []
    port_handles: list[HandleEntry] = []

    try:
        h_resp = await daemon.dap_client.zxdb_console_command(
            ZxdbConsoleCommandArguments(
                command=daemon.scope_console_command("handle -x", pid)
            )
        )
        if h_resp.success and h_resp.body and h_resp.body.output:
            all_handles = parse_handles_table_output(h_resp.body.output)
            interrupt_handles = [
                h for h in all_handles if "INTERRUPT" in h.type.upper()
            ]
            channel_handles = [
                h for h in all_handles if "CHANNEL" in h.type.upper()
            ]
            port_handles = [h for h in all_handles if "PORT" in h.type.upper()]
    except Exception:
        pass

    return interrupt_handles, channel_handles, port_handles


async def _collect_process_thread_stacks(
    daemon: Daemon, pid: int
) -> tuple[list[int], list[ClassifiedThread]]:
    """Collects stack frames and classifications for all threads in pid."""
    threads_to_inspect: list[tuple[int, str]] = []
    threads_resp = await daemon.dap_client.zxdb_threads()
    if threads_resp.body and threads_resp.body.threads:
        daemon.update_thread_cache(threads_resp.body.threads)
        threads_to_inspect = [
            (t.id, t.name)
            for t in threads_resp.body.threads
            if t.process_id == pid
        ]
    if not threads_to_inspect and (proc := daemon.processes.get(pid)):
        threads_to_inspect = [
            (tid, t_obj.name) for tid, t_obj in proc.threads.items()
        ]

    thread_stacks: dict[int, list[Any]] = {}
    thread_names: dict[int, str] = {}
    for tid, t_name in threads_to_inspect:
        thread_names[tid] = t_name
        try:
            st_resp = await daemon.dap_client.stack_trace(
                StackTraceArguments(thread_id=tid)
            )
            thread_stacks[tid] = (
                st_resp.body.stack_frames
                if st_resp.body and st_resp.body.stack_frames
                else []
            )
        except Exception:
            thread_stacks[tid] = []

    classified_threads = [
        classify_thread_stack(
            tid, thread_names.get(tid, f"thread-{tid}"), frames
        )
        for tid, frames in thread_stacks.items()
    ]
    return list(thread_stacks.keys()), classified_threads


def _task_key(node: AsyncTaskNode) -> str:
    """Returns a key that identifies the same async task across threads.

    zxdb omits `id` for tasks whose ID is zero, so ID-less nodes are keyed by
    their full contents and only merge with identical nodes.
    """
    if node.id is not None:
        return f"id:{node.id}"
    return f"value:{node.model_dump_json()}"


def _merge_task_nodes(
    merged: list[AsyncTaskNode], incoming: list[AsyncTaskNode]
) -> None:
    """Merges incoming task nodes into merged, recursing into children.

    Each thread reports the tasks it can see. Driver host threads that share an
    fdf dispatcher report the same dispatcher root with different active and
    queued children, so children are merged and deduplicated instead of keeping
    only the first thread's view.
    """
    by_key = {_task_key(node): node for node in merged}
    for node in incoming:
        key = _task_key(node)
        existing = by_key.get(key)
        if existing is None:
            copy = node.model_copy(deep=True)
            merged.append(copy)
            by_key[key] = copy
        else:
            _merge_task_nodes(existing.children, node.children)


async def _collect_async_tasks(
    daemon: Daemon, thread_ids: list[int]
) -> list[AsyncTaskNode]:
    """Queries `zxdb/asyncBacktrace` on a best-effort basis across threads."""
    merged: list[AsyncTaskNode] = []
    for tid in thread_ids:
        try:
            resp = await daemon.dap_client.zxdb_async_backtrace(
                ZxdbAsyncBacktraceArguments(thread_id=tid)
            )
            if resp.success and resp.body and resp.body.tasks:
                _merge_task_nodes(merged, resp.body.tasks)
        except Exception:
            continue
    return merged


def _count_pending_tasks(async_tasks: list[AsyncTaskNode]) -> int:
    """Counts top-level async tasks, looking through scope roots.

    An fdf dispatcher is reported as a `fdf::Dispatcher` scope root whose
    children are its active, queued, delayed, and waiting tasks. An idle
    dispatcher is an empty scope and does not count as pending work.
    """
    count = 0
    for node in async_tasks:
        if node.name in _SCOPE_ROOT_NAMES:
            count += _count_pending_tasks(node.children)
        else:
            count += 1
    return count


def _build_hang_summary(
    classified_threads: list[ClassifiedThread],
    async_tasks: list[AsyncTaskNode],
    interrupt_handles: list[HandleEntry],
) -> list[str]:
    """Synthesizes hang findings from threads, async tasks, and handles."""
    summary: list[str] = []
    locked = [
        t for t in classified_threads if t.classification == "blocked_lock"
    ]
    ipc_blocked = [
        t for t in classified_threads if t.classification == "blocked_ipc"
    ]
    port_waiters = [
        t for t in classified_threads if t.classification == "port_wait"
    ]
    irq_waiters = [
        t for t in classified_threads if t.classification == "interrupt_wait"
    ]

    for t in locked:
        summary.append(f"LOCK CONTENTION: {t.summary}")
    for t in ipc_blocked:
        summary.append(f"SYNC IPC WAIT: {t.summary}")

    if pending_tasks := _count_pending_tasks(async_tasks):
        summary.append(
            f"ASYNC TASKS: {pending_tasks} top-level async task(s) active"
            " or suspended in executor."
        )

    if interrupt_handles and not irq_waiters and not port_waiters:
        summary.append(
            f"UNSERVICED INTERRUPTS: Process holds {len(interrupt_handles)}"
            " ZX_OBJ_TYPE_INTERRUPT handle(s), but no threads are currently"
            " waiting in zx_interrupt_wait or zx_port_wait."
        )

    if not summary:
        summary.append(
            "No blocked locks, sync IPCs, or unserviced interrupts"
            f" detected ({len(port_waiters)}/{len(classified_threads)}"
            " thread(s) idle in zx_port_wait)."
        )
    return summary


async def handle(
    daemon: Daemon, req: DiagnoseHangRequest
) -> Response[DiagnoseHangResponse]:
    """Handles a diagnose-hang request for an attached process."""
    if not daemon.zxdb_writer:
        return Response(
            success=False, message="Not connected to zxdb DAP server"
        )

    pid, err = daemon.resolve_process_id(req.pid)
    if err is not None:
        return Response(success=False, message=err)
    if pid is None:
        return Response(
            success=False,
            message=(
                "No attached process; attach to a process first or"
                " specify --pid"
            ),
        )

    # Pre-fetch process handles before pausing threads so handle inspection
    # adds zero target dwell time.
    (
        interrupt_handles,
        channel_handles,
        port_handles,
    ) = await _prefetch_process_handles(daemon, pid)

    target_proc = daemon.processes.get(pid) if pid is not None else None
    was_already_stopped = bool(
        target_proc and any(t.is_stopped for t in target_proc.threads.values())
    )

    t_start = time.monotonic()
    auto_resumed = False
    try:
        await daemon.ensure_process_stopped(pid)
        (
            thread_ids,
            classified_threads,
        ) = await _collect_process_thread_stacks(daemon, pid)
        async_tasks = await _collect_async_tasks(daemon, thread_ids)
    except Exception as e:
        return Response(
            success=False,
            message=f"Failed to capture hang diagnostic snapshot: {e}",
        )
    finally:
        if req.auto_resume and not was_already_stopped:
            auto_resumed = await asyncio.shield(
                resume_process_safely(daemon, pid)
            )
    dwell_ms = round((time.monotonic() - t_start) * _MS_PER_SECOND, 2)

    summary = _build_hang_summary(
        classified_threads, async_tasks, interrupt_handles
    )

    return Response(
        success=True,
        body=DiagnoseHangResponse(
            pid=pid,
            dwell_ms=dwell_ms,
            auto_resumed=auto_resumed,
            summary=summary,
            threads=classified_threads,
            async_tasks=async_tasks,
            interrupt_handles=interrupt_handles,
            channel_handles=channel_handles,
            port_handles=port_handles,
        ),
    )
