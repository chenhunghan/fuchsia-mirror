#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Utilities for trace model tests."""

import unittest
from collections.abc import Mapping, Sequence

import trace_processing.trace_model as trace_model
import trace_processing.trace_time as trace_time

TEST_MODEL_BEGIN_TIME_IN_US: float = 697503138.0
TEST_MODEL_END_TIME_IN_US: float = 698607465.375


def get_test_model(is_fxt: bool = False) -> trace_model.Model:
    def us_tp(us: float) -> trace_time.TimePoint:
        return trace_time.TimePoint.from_epoch_delta(
            trace_time.TimeDelta.from_microseconds(us)
        )

    read_event = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(698607461.7395687)
        - trace_time.TimeDelta.from_microseconds(697503138.9531089),
        parent=None,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="Read",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697503138.9531089)
            ),
            pid=7009,
            tid=7021,
            args={},
        ),
    )
    write_event = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(90254.0)
        if is_fxt
        else (
            trace_time.TimeDelta.from_microseconds(697868582.5994568)
            - trace_time.TimeDelta.from_microseconds(697778328.2160872)
        ),
        parent=None,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="Write",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697778328.2160872)
            ),
            pid=7009,
            tid=7022,
            args={},
        ),
    )
    async_read_write_event = trace_model.AsyncEvent(
        # Perfetto does not store the correlation ID for async events.
        id=0 if is_fxt else 43,
        duration=trace_time.TimeDelta.from_microseconds(698607461.0)
        - trace_time.TimeDelta.from_microseconds(TEST_MODEL_BEGIN_TIME_IN_US),
        base=trace_model.Event(
            category="io",
            name="AsyncReadWrite",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(
                    int(TEST_MODEL_BEGIN_TIME_IN_US)
                )
            ),
            pid=7009,
            # Async events are process scoped in Perfetto.
            tid=0 if is_fxt else 7022,
            args={},
        ),
    )
    read_event2 = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(386.0)
        if is_fxt
        else (
            trace_time.TimeDelta.from_microseconds(697868571.6018075)
            - trace_time.TimeDelta.from_microseconds(697868185.3588456)
        ),
        parent=None,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="Read",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(
                    697868185.358 if is_fxt else 697868185.3588456
                )
            ),
            pid=7010,
            tid=7023,
            args={},
        ),
    )
    # Note on flow event timestamps and names (FXT vs Legacy JSON):
    # This is NOT a precision loss or rounding jitter issue. Rather, it reflects a difference in the underlying source records:
    # - In legacy JSON, flow events exist as independent JSON objects with generic names (e.g., "ReadWriteFlow") and
    #   their own explicitly offset timestamps (e.g., 697778305.5 for flow_start, 697779328.2160872 for flow_step, and
    #   697868050.2160872 for flow_end).
    # - In direct FXT ingestion via Perfetto, flow linkages are retrieved from Perfetto's `flow` table, referencing the
    #   exact slice IDs where they originate or terminate. The loader constructs the `FlowEvent` by referencing the
    #   base `Event` of the associated duration slice directly. Thus, FXT flow events inherit the exact names and start
    #   timestamps of their bound duration slices:
    #   - flow_start inherits from ReadSubBegin (starting at 697778305.0)
    #   - flow_step inherits from WriteSubBegin (starting at 697778329.0)
    #   - flow_end inherits from WriteSubEnd (starting at 697868049.0)
    flow_start = trace_model.FlowEvent(
        id="1" if is_fxt else "0",
        phase=trace_model.FlowEventPhase.START,
        enclosing_duration=None,
        previous_flow=None,
        next_flow=None,
        base=trace_model.Event(
            category="io",
            name="ReadSubBegin" if is_fxt else "ReadWriteFlow",
            start=us_tp(697778305.0) if is_fxt else us_tp(697778305.5),
            pid=7009,
            tid=7021,
            args={},
        ),
    )
    flow_step = trace_model.FlowEvent(
        id="1" if is_fxt else "0",
        phase=trace_model.FlowEventPhase.STEP,
        enclosing_duration=None,
        previous_flow=None,
        next_flow=None,
        base=trace_model.Event(
            category="io",
            name="WriteSubBegin" if is_fxt else "ReadWriteFlow",
            start=us_tp(697778329.0) if is_fxt else us_tp(697779328.2160872),
            pid=7009,
            tid=7022,
            args={},
        ),
    )
    flow_end = trace_model.FlowEvent(
        id="1" if is_fxt else "0",
        phase=trace_model.FlowEventPhase.END,
        enclosing_duration=None,
        previous_flow=None,
        next_flow=None,
        base=trace_model.Event(
            category="io",
            name="WriteSubEnd" if is_fxt else "ReadWriteFlow",
            start=us_tp(697868049.0) if is_fxt else us_tp(697868050.2160872),
            pid=7009,
            tid=7022,
            args={},
        ),
    )

    read_sub_begin = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(1.0),
        parent=read_event,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="ReadSubBegin",
            start=us_tp(697778305.0),
            pid=7009,
            tid=7021,
            args={},
        ),
    )
    read_sub_end = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(2.0),
        parent=read_event,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="ReadSubEnd",
            start=us_tp(697868583.0),
            pid=7009,
            tid=7021,
            args={},
        ),
    )
    write_sub_begin = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(1000.0),
        parent=write_event,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="WriteSubBegin",
            start=us_tp(697778329.0),
            pid=7009,
            tid=7022,
            args={},
        ),
    )
    write_sub_end = trace_model.DurationEvent(
        duration=trace_time.TimeDelta.from_microseconds(2.0),
        parent=write_event,
        child_durations=[],
        child_flows=[],
        base=trace_model.Event(
            category="io",
            name="WriteSubEnd",
            start=us_tp(697868049.0),
            pid=7009,
            tid=7022,
            args={},
        ),
    )

    counter_event1 = trace_model.CounterEvent(
        id=0 if is_fxt else None,
        base=trace_model.Event(
            category="" if is_fxt else "system_metrics",
            name="cpu_usage",
            start=us_tp(697503150.0),
            pid=7010,
            tid=0 if is_fxt else 7023,
            args={
                "average_cpu_percentage": 0.1,
                "max_cpu_usage": 0.2,
            },
        ),
    )
    counter_event2 = trace_model.CounterEvent(
        id=0 if is_fxt else None,
        base=trace_model.Event(
            category="" if is_fxt else "system_metrics",
            name="cpu_usage",
            start=us_tp(698000000.0),
            pid=7010,
            tid=0 if is_fxt else 7023,
            args={
                "average_cpu_percentage": 0.5,
                "max_cpu_usage": 0.6,
            },
        ),
    )
    counter_event3 = trace_model.CounterEvent(
        id=0 if is_fxt else None,
        base=trace_model.Event(
            category="" if is_fxt else "system_metrics",
            name="cpu_usage",
            start=us_tp(698607465.375),
            pid=7010,
            tid=0 if is_fxt else 7023,
            args={
                "average_cpu_percentage": 0.89349317793,
                "max_cpu_usage": 0.1234,
            },
        ),
    )
    # Testing instant events under different scopes.
    # Note: Perfetto's Fuchsia Trace Parser tokenizer unconditionally registers all
    # Fuchsia Instant Events onto thread tracks in the database. Consequently, under
    # direct FXT ingestion (is_fxt == True), all fuchsia instant events are associated
    # with their emitting thread tracks and resolve exactly to InstantEventScope.THREAD.
    # Legacy JSON parsing still respects GLOBAL and PROCESS scoping. Scoping does
    # not affect fuchsia metric evaluations.
    instant_event = trace_model.InstantEvent(
        scope=trace_model.InstantEventScope.THREAD
        if is_fxt
        else trace_model.InstantEventScope.GLOBAL,
        base=trace_model.Event(
            category="log",
            name="global_instant",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697820000.0)
            ),
            pid=7009,
            tid=7022,
            args={"message": "Global scoped instant event"},
        ),
    )

    process_instant_event = trace_model.InstantEvent(
        scope=trace_model.InstantEventScope.THREAD
        if is_fxt
        else trace_model.InstantEventScope.PROCESS,
        base=trace_model.Event(
            category="log",
            name="process_instant",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697821000.0)
            ),
            pid=7009,
            tid=7022,
            args={"message": "Process scoped instant event"},
        ),
    )

    thread_instant_event = trace_model.InstantEvent(
        scope=trace_model.InstantEventScope.THREAD,
        base=trace_model.Event(
            category="log",
            name="thread_instant",
            start=trace_time.TimePoint.from_epoch_delta(
                trace_time.TimeDelta.from_microseconds(697822000.0)
            ),
            pid=7009,
            tid=7022,
            args={"message": "Thread scoped instant event"},
        ),
    )

    flow_start.enclosing_duration = read_sub_begin
    flow_start.next_flow = flow_step

    flow_step.enclosing_duration = write_sub_begin
    flow_step.previous_flow = flow_start
    flow_step.next_flow = flow_end

    flow_end.enclosing_duration = write_sub_end
    flow_end.previous_flow = flow_step

    read_sub_begin.child_flows = [flow_start]
    write_sub_begin.child_flows = [flow_step]
    write_sub_end.child_flows = [flow_end]

    read_event.child_durations = [read_sub_begin, read_sub_end]
    write_event.child_durations = [write_sub_begin, write_sub_end]

    # No trace events for the following two threads.
    thread1036 = trace_model.Thread(tid=1036, name="memory-pressure-loop")
    thread5555 = trace_model.Thread(tid=5555, name="initial-thread")

    thread7021 = trace_model.Thread(
        tid=7021,
        name="tid: 7021",
        events=[
            read_event,
            read_sub_begin,
            flow_start,
            read_sub_end,
        ],
    )

    # In FXT (via Perfetto), async events and counters are process-scoped and
    # are mapped to a pseudo-thread with TID 0. In legacy JSON, they were
    # associated with the emitting thread (e.g. thread 7023 for counters).
    thread0_7009 = trace_model.Thread(
        tid=0,
        name="tid: 0",
        events=[async_read_write_event] if is_fxt else [process_instant_event],
    )

    if is_fxt:
        thread0_7010 = trace_model.Thread(
            tid=0,
            name="tid: 0",
            events=[counter_event1, counter_event2, counter_event3],
        )
        thread7023 = trace_model.Thread(
            tid=7023,
            name="tid: 7023",
            events=[read_event2],
        )
    else:
        thread7023 = trace_model.Thread(
            tid=7023,
            name="tid: 7023",
            events=[
                counter_event1,
                read_event2,
                counter_event2,
                counter_event3,
            ],
        )

    thread7022 = trace_model.Thread(
        tid=7022,
        name="initial-thread",
        events=[
            write_event,
            write_sub_begin,
            flow_step,
            instant_event,
            process_instant_event,
            thread_instant_event,
            write_sub_end,
            flow_end,
        ]
        if is_fxt
        else [
            async_read_write_event,
            write_event,
            write_sub_begin,
            flow_step,
            instant_event,
            process_instant_event,
            thread_instant_event,
            write_sub_end,
            flow_end,
        ],
    )

    process7010 = trace_model.Process(
        pid=7010, threads=[thread0_7010, thread7023] if is_fxt else [thread7023]
    )
    process7009 = trace_model.Process(
        pid=7009,
        name="process_foo",
        threads=[thread0_7009, thread1036, thread7021, thread7022]
        if is_fxt
        else [thread1036, thread7021, thread7022],
    )
    process7011 = trace_model.Process(
        pid=7011, name="process-with-no-trace-events", threads=[thread5555]
    )

    processes = [process7009, process7010, process7011]

    scheduling_records = {}

    scheduling_records[0] = [
        trace_model.ContextSwitch(
            start=us_tp(697503118.9531089),
            incoming_tid=7021,
            outgoing_tid=0 if is_fxt else 1036,
            incoming_prio=3122,
            outgoing_prio=None if is_fxt else 3122,
            outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_RUNNING
            if is_fxt
            else trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            args={},
        ),
        trace_model.ContextSwitch(
            start=us_tp(697778308.2160872),
            incoming_tid=7022,
            outgoing_tid=7021,
            incoming_prio=3122,
            outgoing_prio=3122,
            outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            args={},
        ),
        trace_model.Waking(
            start=us_tp(697868585),
            tid=7021,
            prio=0 if is_fxt else 3122,
            args={},
        ),
        trace_model.ContextSwitch(
            start=us_tp(697868597.5994568),
            incoming_tid=7021,
            outgoing_tid=7022,
            incoming_prio=3122,
            outgoing_prio=3122,
            outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            args={},
        ),
        trace_model.ContextSwitch(
            start=us_tp(698607476.739 if is_fxt else 698607476.7395687),
            incoming_tid=0 if is_fxt else 1036,
            outgoing_tid=7021,
            incoming_prio=-2147483648,
            outgoing_prio=3122,
            outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            args={},
        ),
    ]

    scheduling_records[1] = [
        trace_model.ContextSwitch(
            start=us_tp(697868165.358 if is_fxt else 697868165.3588456),
            incoming_tid=7023,
            outgoing_tid=0 if is_fxt else 1037,
            incoming_prio=3122,
            outgoing_prio=None if is_fxt else 3122,
            outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_RUNNING
            if is_fxt
            else trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            args={},
        ),
        trace_model.ContextSwitch(
            start=us_tp(697868586.601 if is_fxt else 697868586.6018075),
            incoming_tid=0 if is_fxt else 1037,
            outgoing_tid=7023,
            incoming_prio=-2147483648,
            outgoing_prio=3122,
            outgoing_state=trace_model.ThreadState.ZX_THREAD_STATE_BLOCKED,
            args={},
        ),
    ]

    return trace_model.Model(processes, scheduling_records)


def _assert_event_links_in_model(
    test: unittest.TestCase, model: trace_model.Model
) -> None:
    """Asserts that every event neighbor reference points to an event in `model`."""
    model_events = set(model.all_events())
    for event in model.all_events():
        if isinstance(event, trace_model.DurationEvent):
            if event.parent is not None:
                test.assertIn(event.parent, model_events)
            for child in event.child_durations:
                test.assertIn(child, model_events)
            for flow in event.child_flows:
                test.assertIn(flow, model_events)
        elif isinstance(event, trace_model.FlowEvent):
            if event.enclosing_duration is not None:
                test.assertIn(event.enclosing_duration, model_events)
            if event.previous_flow is not None:
                test.assertIn(event.previous_flow, model_events)
            if event.next_flow is not None:
                test.assertIn(event.next_flow, model_events)


def assertEventsEqual(
    test: unittest.TestCase,
    a: trace_model.Event | None,
    b: trace_model.Event | None,
    visited: set[tuple[trace_model.Event, trace_model.Event]] | None = None,
) -> None:
    test.assertEqual(a is None, b is None)
    if a is None or b is None:
        return
    # Event links form cycles (e.g. `parent <-> child_durations`,
    # `enclosing_duration <-> child_flows`, `previous_flow <-> next_flow`), so
    # record `(a, b)` before recursing to avoid infinite recursion.
    if visited is None:
        visited = set()
    pair = (a, b)
    if pair in visited:
        # We have already checked (or are already checking) that a and b are equal.
        return
    visited.add(pair)

    test.assertIs(type(a), type(b))

    # Check basic [trace_model.Event] fields.
    if not isinstance(a, trace_model.CounterEvent):
        test.assertEqual(a.category, b.category)
    test.assertEqual(a.name, b.name)
    # Note: Comparing event start times requires bounded precision because Perfetto natively extracts timestamps
    # as 64-bit nanosecond integers. When converted to floating-point microseconds, minor float rounding and string
    # conversion representation variances can occur against legacy JSON floating-point expectations.
    # places=3 guarantees agreement within 0.001 microseconds (exactly 1 nanosecond).
    test.assertAlmostEqual(
        a.start.to_epoch_delta().to_microseconds_f(),
        b.start.to_epoch_delta().to_microseconds_f(),
        places=3,
    )
    if isinstance(a, trace_model.InstantEvent) and isinstance(
        b, trace_model.InstantEvent
    ):
        if a.scope == trace_model.InstantEventScope.PROCESS:
            test.assertEqual(a.pid, b.pid)
        elif a.scope == trace_model.InstantEventScope.THREAD:
            test.assertEqual(a.pid, b.pid)
            test.assertEqual(a.tid, b.tid)
    else:
        test.assertEqual(a.pid, b.pid)
        test.assertEqual(a.tid, b.tid)

    # The [args] field of an [trace_model.Event] should never be null.
    test.assertIsNotNone(a.args)
    test.assertIsNotNone(b.args)

    # Note: Rather than trying to handling the possibly complicated object
    # structure on each event here for equality, we just verify that their
    # key sets are equal.  This is safe, as this function is only used for
    # testing, rather than publicy exposed.
    test.assertEqual(len(a.args), len(b.args))
    test.assertEqual(set(a.args.keys()), b.args.keys())

    if isinstance(a, trace_model.InstantEvent) and isinstance(
        b, trace_model.InstantEvent
    ):
        test.assertEqual(a.scope, b.scope)
    elif isinstance(a, trace_model.CounterEvent) and isinstance(
        b, trace_model.CounterEvent
    ):
        test.assertEqual(a.id, b.id)
    elif isinstance(a, trace_model.DurationEvent) and isinstance(
        b, trace_model.DurationEvent
    ):
        assert a.duration is not None and b.duration is not None
        test.assertAlmostEqual(
            a.duration.to_microseconds(), b.duration.to_microseconds()
        )
        assertEventsEqual(test, a.parent, b.parent, visited)
        test.assertEqual(len(a.child_durations), len(b.child_durations))
        for ca, cb in zip(a.child_durations, b.child_durations):
            test.assertIs(ca.parent, a)
            test.assertIs(cb.parent, b)
            assertEventsEqual(test, ca, cb, visited)
        test.assertEqual(len(a.child_flows), len(b.child_flows))
        for fa, fb in zip(a.child_flows, b.child_flows):
            test.assertIs(fa.enclosing_duration, a)
            test.assertIs(fb.enclosing_duration, b)
            assertEventsEqual(test, fa, fb, visited)
    elif isinstance(a, trace_model.AsyncEvent) and isinstance(
        b, trace_model.AsyncEvent
    ):
        test.assertEqual(a.id, b.id)
        assert a.duration is not None and b.duration is not None
        test.assertAlmostEqual(
            a.duration.to_microseconds(), b.duration.to_microseconds()
        )
    elif isinstance(a, trace_model.FlowEvent) and isinstance(
        b, trace_model.FlowEvent
    ):
        test.assertEqual(a.id, b.id)
        test.assertEqual(a.phase, b.phase)
        assertEventsEqual(
            test, a.enclosing_duration, b.enclosing_duration, visited
        )
        assertEventsEqual(test, a.previous_flow, b.previous_flow, visited)
        assertEventsEqual(test, a.next_flow, b.next_flow, visited)
    else:
        test.fail(f"event {a} and event {b} were unrecognized types")


def assertEventListsEqual(
    test: unittest.TestCase,
    a_events: Sequence[trace_model.Event],
    b_events: Sequence[trace_model.Event],
) -> None:
    test.assertEqual(len(a_events), len(b_events))
    visited: set[tuple[trace_model.Event, trace_model.Event]] = set()
    for a_event, b_event in zip(a_events, b_events):
        assertEventsEqual(test, a_event, b_event, visited)
    # Check that `visited` pairs events one-to-one: every Event object
    # reachable from `a_events` pairs with at most one Event object from
    # `b_events`, and every Event object reachable from `b_events` pairs with
    # at most one Event object from `a_events`.
    #
    # This matters because `Event` uses object identity for equality and
    # hashing, and downstream consumers rely on links (`parent`,
    # `enclosing_duration`, `child_flows`, `next_flow`) pointing to the exact
    # `Event` instances stored in the model rather than detached copies or
    # accidentally shared instances:
    #   * `Model.slice()` remaps neighbor references via a `dict[Event, Event]`
    #     keyed by the events in `thread.events`, silently dropping any link
    #     that points to a detached copy.
    #   * `trace_utils.get_following_flow_events()` walks `enclosing_duration`,
    #     `child_flows`, and `next_flow` with a `set[Event]`; a link to a
    #     detached copy misses flows/children attached to the real instance,
    #     while aliasing two distinct events into one instance collapses them.
    # `assertEventsEqual` alone would not catch either bug because it keys
    # `visited` on `(a, b)` pairs and compares fields by value, allowing one
    # Event on one side to match multiple distinct Events with identical fields
    # on the other side.
    test.assertEqual(len({id(a) for a, _ in visited}), len(visited))
    test.assertEqual(len({id(b) for _, b in visited}), len(visited))


def assertThreadsEqual(
    test: unittest.TestCase, a: trace_model.Thread, b: trace_model.Thread
) -> None:
    test.assertEqual(
        a.tid, b.tid, f"Error, thread tids did match: {a.tid} vs {b.tid}"
    )
    test.assertEqual(
        a.name,
        b.name,
        f"Error, thread names (tid={a.tid}) did not match: {a.name} vs "
        f"{b.name}",
    )
    test.assertEqual(
        len(a.events),
        len(b.events),
        f"Error, thread (tid={a.tid}, name={a.name}) events lengths did "
        f"not match: {len(a.events)} vs {len(b.events)}",
    )
    for a_event, b_event in zip(a.events, b.events):
        assertEventsEqual(test, a_event, b_event)


def assertProcessesEqual(
    test: unittest.TestCase, a: trace_model.Process, b: trace_model.Process
) -> None:
    test.assertEqual(
        a.pid, b.pid, f"Error, process pids did match: {a.pid} vs {b.pid}"
    )
    test.assertEqual(
        a.name,
        b.name,
        f"Error, process (pid={a.pid}) names did not match: {a.name} vs "
        f"{b.name}",
    )
    a_threads = {t.tid: t for t in a.threads}
    b_threads = {t.tid: t for t in b.threads}
    test.assertEqual(
        set(a_threads.keys()),
        set(b_threads.keys()),
        f"Thread TIDs do not match for process {a.pid}",
    )

    for tid in a_threads:
        assertThreadsEqual(test, a_threads[tid], b_threads[tid])


def assertSchedulingRecordEqual(
    test: unittest.TestCase,
    a: trace_model.SchedulingRecord,
    b: trace_model.SchedulingRecord,
) -> None:
    test.assertIs(type(a), type(b))

    # Check basic [trace_model.SchedulingRecord] fields.
    test.assertEqual(a.start, b.start)
    test.assertEqual(a.tid, b.tid)
    test.assertEqual(a.prio, b.prio)

    # Note: Like for events, rather than trying to handling the possibly complicated object
    # structure on each event here for equality, we just verify that their key sets are equal.
    test.assertEqual(len(a.args), len(b.args))
    test.assertEqual(set(a.args.keys()), b.args.keys())

    if isinstance(a, trace_model.ContextSwitch) and isinstance(
        b, trace_model.ContextSwitch
    ):
        test.assertEqual(a.tid, b.tid)
        test.assertEqual(a.outgoing_tid, b.outgoing_tid)
        test.assertEqual(a.outgoing_prio, b.outgoing_prio)
        test.assertEqual(a.outgoing_state, b.outgoing_state)


def assertSchedulingRecordsEqual(
    test: unittest.TestCase,
    a_records: Mapping[int, Sequence[trace_model.SchedulingRecord]],
    b_records: Mapping[int, Sequence[trace_model.SchedulingRecord]],
) -> None:
    test.assertEqual(
        set(a_records.keys()),
        set(b_records.keys()),
        f"Error, scheduling record CPUs did not match: "
        f"{set(a_records.keys())} vs {set(b_records.keys())}",
    )

    for cpu in sorted(a_records.keys()):
        test.assertEqual(
            len(a_records[cpu]),
            len(b_records[cpu]),
            f"Error, scheduling record counts on CPU {cpu} did not match: "
            f"{len(a_records[cpu])} vs {len(b_records[cpu])}",
        )
        for a_record, b_record in zip(a_records[cpu], b_records[cpu]):
            assertSchedulingRecordEqual(test, a_record, b_record)


def assertModelsEqual(
    test: unittest.TestCase, a: trace_model.Model, b: trace_model.Model
) -> None:
    _assert_event_links_in_model(test, a)
    _assert_event_links_in_model(test, b)

    # Ignore process 0 (kernel/idle) when comparing processes. Perfetto
    # automatically creates a placeholder process with PID 0 to associate scheduling
    # and global events, which is not present in legacy JSON models.
    a_processes = [p for p in a.processes if p.pid != 0]
    b_processes = [p for p in b.processes if p.pid != 0]
    test.assertEqual(
        len(a_processes),
        len(b_processes),
        f"Error, model processes lengths did not match: {len(a_processes)} "
        f"vs {len(b_processes)}",
    )
    for a_process, b_process in zip(a_processes, b_processes):
        assertProcessesEqual(test, a_process, b_process)

    assertSchedulingRecordsEqual(
        test, a.scheduling_records, b.scheduling_records
    )
