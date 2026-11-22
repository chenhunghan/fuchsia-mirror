#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Shared FXT graphics trace-builder helpers for trace_processing tests."""

import tempfile

from reporting import metrics
from trace_processing import trace_metrics
from trace_writing import fxt

SCENIC_PID = 1
SCENIC_TID = 1
DISPLAY_PID = 2
DISPLAY_TID = 2
APP_PID = 3
APP_TID = 3
OTHER_PID = 4


def new_gfx_builder(*, include_app: bool = False) -> fxt.Builder:
    """Creates an fxt.Builder pre-populated with scenic, display, and optional app tracks."""
    builder = fxt.Builder()
    builder.add_process(SCENIC_PID, "scenic")
    builder.add_thread(SCENIC_PID, SCENIC_TID, "scenic-main")
    builder.add_process(DISPLAY_PID, "display")
    builder.add_thread(DISPLAY_PID, DISPLAY_TID, "display-main")
    if include_app:
        builder.add_process(APP_PID, "app")
        builder.add_thread(APP_PID, APP_TID, "app-main")
    return builder


def add_flow(
    builder: fxt.Builder,
    category: str,
    name: str,
    flow_id: int,
    src_pid: int,
    src_tid: int,
    src_ts_ns: int,
    dst_pid: int,
    dst_tid: int,
    dst_ts_ns: int,
) -> None:
    """Appends a flow_begin/flow_end pair to `builder`."""
    builder.add_flow_begin(category, name, src_ts_ns, src_pid, src_tid, flow_id)
    builder.add_flow_end(category, name, dst_ts_ns, dst_pid, dst_tid, flow_id)


def add_render_frame(
    builder: fxt.Builder,
    ts_ns: int,
    dur_ns: int = 10_000,
    pid: int = SCENIC_PID,
    tid: int = SCENIC_TID,
) -> None:
    """Appends a complete `gfx` `RenderFrame` slice to `builder`."""
    builder.add_duration_complete("gfx", "RenderFrame", ts_ns, dur_ns, pid, tid)


def add_vsync(
    builder: fxt.Builder,
    ts_ns: int,
    dur_ns: int = 5_000,
    pid: int = DISPLAY_PID,
    tid: int = DISPLAY_TID,
) -> None:
    """Appends a complete `gfx` `Flatland::DisplayCompositor::OnVsync` slice."""
    builder.add_duration_complete(
        "gfx",
        "Flatland::DisplayCompositor::OnVsync",
        ts_ns,
        dur_ns,
        pid,
        tid,
    )


def process_fxt(
    builder: fxt.Builder,
    processor: trace_metrics.MetricsProcessor,
) -> list[metrics.TestCaseResult]:
    """Writes `builder` to a temporary FXT file and runs `processor.process_metrics_with_fxt`."""
    with tempfile.NamedTemporaryFile(suffix=".fxt") as tmp:
        builder.write_to_file(tmp.name)
        return list(processor.process_metrics_with_fxt(tmp.name))
