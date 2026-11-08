// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Every SQL query zpo runs against a trace.
//!
//! The trace processor gives intervals still open at the end of the trace
//! `dur = -1`, so sums and maxima of durations count those as 0.
//!
//! Kept in one place so the end-to-end test can run each of them against the
//! real trace processor: a Perfetto roll that renames a table or column fails
//! there, even if the query would return nothing.

use super::Query;

/// A string as an SQL literal.
pub fn str_to_sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A thread's states (`thread_state`), with the CPU number.
pub fn thread_states(utid: i64) -> Query {
    Query {
        name: "thread_states",
        sql: format!(
            "SELECT ts.ts AS ts, ts.dur AS dur, ts.state AS state, COALESCE(c.cpu, -1) AS cpu, \
             ts.waker_utid AS waker_utid FROM thread_state ts LEFT JOIN cpu c ON ts.ucpu = c.id \
             WHERE ts.utid = {utid}"
        ),
        cols: &["ts", "dur", "state", "cpu", "waker_utid"],
        order: "ts",
    }
}

/// A thread's sched slices with the priority (fair weight or packed deadline).
pub fn thread_sched(utid: i64) -> Query {
    Query {
        name: "thread_sched",
        sql: format!(
            "SELECT ts, dur, cpu, COALESCE(priority, 0) AS prio FROM sched WHERE utid = {utid}"
        ),
        cols: &["ts", "dur", "cpu", "prio"],
        order: "ts",
    }
}

fn ipc(where_: &str) -> String {
    format!(
        "WITH ipc AS (\
           SELECT s.id, s.ts, s.name, tt.utid \
           FROM slice s JOIN thread_track tt ON s.track_id = tt.id \
           WHERE s.category = 'kernel:ipc'), \
         pairs AS (\
           SELECT o.utid o_utid, o.ts o_ts, o.name name, i.utid i_utid, i.ts i_ts \
           FROM flow f JOIN ipc o ON f.slice_out = o.id JOIN ipc i ON f.slice_in = i.id) \
         SELECT o_utid AS utid, o_ts AS ts, name, 1 AS outgoing, i_utid AS peer_utid, i_ts AS peer_ts \
         FROM pairs {where_} \
         UNION ALL \
         SELECT i_utid AS utid, i_ts AS ts, name, 0 AS outgoing, o_utid AS peer_utid, o_ts AS peer_ts \
         FROM pairs {where_}"
    )
}

const IPC_COLS: &[&str] = &["utid", "ts", "name", "outgoing", "peer_utid", "peer_ts"];

/// Both sides of every FIDL message a thread sent or received.
pub fn ipc_for_thread(utid: i64) -> Query {
    Query {
        name: "ipc_for_thread",
        sql: ipc(&format!("WHERE o_utid = {utid} OR i_utid = {utid}")),
        cols: IPC_COLS,
        order: "",
    }
}

/// Both sides of every message of one FIDL method.
pub fn ipc_for_method(method: &str) -> Query {
    Query {
        name: "ipc_for_method",
        sql: ipc(&format!("WHERE name = {}", str_to_sql_literal(method))),
        cols: IPC_COLS,
        order: "",
    }
}

/// Trace slices with this exact name, on thread tracks.
pub fn slices_named(name: &str) -> Query {
    Query {
        name: "slices_named",
        sql: format!(
            "SELECT s.ts AS ts, s.dur AS dur, tt.utid AS utid FROM slice s \
             JOIN thread_track tt ON s.track_id = tt.id WHERE s.name = {}",
            str_to_sql_literal(name)
        ),
        cols: &["ts", "dur", "utid"],
        order: "ts",
    }
}

/// Every thread with its process.
pub fn threads() -> Query {
    Query {
        name: "threads",
        sql: "SELECT t.utid AS utid, t.tid AS tid, COALESCE(t.name, '') AS tname, \
              COALESCE(p.pid, 0) AS pid, COALESCE(p.name, '') AS pname \
              FROM thread t LEFT JOIN process p USING(upid)"
            .into(),
        cols: &["utid", "tid", "tname", "pid", "pname"],
        order: "utid",
    }
}

/// Every interrupt handler run. IRQ slices are on kernel pseudo-threads named
/// cpu-N.
pub fn irqs() -> Query {
    Query {
        name: "irqs",
        sql:
            "SELECT s.ts AS ts, s.dur AS dur, t.name AS tname, CAST(a.display_value AS INT) AS irq \
              FROM slice s JOIN thread_track tt ON s.track_id = tt.id JOIN thread t USING(utid) \
              LEFT JOIN args a ON a.arg_set_id = s.arg_set_id AND a.key IN ('irq', 'irq #') \
              WHERE s.category = 'kernel:irq'"
                .into(),
        cols: &["ts", "dur", "tname", "irq"],
        order: "ts",
    }
}

/// The kernel's per-CPU processing rate counters (kernel:power).
pub fn rates() -> Query {
    Query {
        name: "rates",
        sql: "SELECT ct.name AS name, c.ts AS ts, c.value AS value FROM counter_track ct \
              JOIN counter c ON c.track_id = ct.id WHERE ct.name LIKE 'Rate:CPU:%'"
            .into(),
        cols: &["name", "ts", "value"],
        order: "name, ts",
    }
}

/// The time span the thread states cover.
pub fn bounds() -> Query {
    Query {
        name: "bounds",
        sql: "SELECT MIN(ts) AS s, MAX(ts + MAX(dur, 0)) AS e FROM thread_state".into(),
        cols: &["s", "e"],
        order: "",
    }
}

/// The number of CPUs.
pub fn ncpu() -> Query {
    Query { name: "ncpu", sql: "SELECT COUNT(*) AS n FROM cpu".into(), cols: &["n"], order: "" }
}

/// Threads with a resident deadline profile: more than half of their sched
/// slices (and at least 20) have priority < 0. The packed priority is the mode
/// of the negative priorities, so transient priority-inheritance boosts do not
/// shift it.
pub fn deadline_threads() -> Query {
    Query {
        name: "deadline_threads",
        sql: "WITH by_prio AS (\
               SELECT utid, priority, COUNT(*) cnt, SUM(MAX(dur, 0)) dur_ns \
               FROM sched WHERE utid != 0 GROUP BY utid, priority\
             ), totals AS (\
               SELECT utid, \
               SUM(CASE WHEN priority < 0 THEN cnt ELSE 0 END) dl_cnt, \
               SUM(cnt) total_cnt, \
               SUM(CASE WHEN priority < 0 THEN dur_ns ELSE 0 END) run_ns \
               FROM by_prio GROUP BY utid \
               HAVING dl_cnt >= 20 AND dl_cnt * 2 > total_cnt\
             ), mode_prio AS (\
               SELECT utid, priority, \
               ROW_NUMBER() OVER (PARTITION BY utid ORDER BY cnt DESC, dur_ns DESC, priority ASC) rn \
               FROM by_prio WHERE priority < 0\
             ) SELECT t.utid AS utid, t.run_ns AS run_ns, m.priority AS prio \
             FROM totals t JOIN mode_prio m USING(utid) WHERE m.rn = 1"
            .into(),
        cols: &["utid", "run_ns", "prio"],
        order: "",
    }
}

/// Busy time per CPU (everything but the idle threads).
pub fn cpu_busy() -> Query {
    Query {
        name: "cpu_busy",
        sql: "SELECT cpu, SUM(MAX(dur, 0)) AS busy FROM sched WHERE utid != 0 GROUP BY cpu".into(),
        cols: &["cpu", "busy"],
        order: "",
    }
}

/// The CPU profiler's samples.
pub fn perf_samples() -> Query {
    Query {
        name: "perf_samples",
        sql: "SELECT ts, utid, callsite_id FROM perf_sample \
              WHERE utid IS NOT NULL AND callsite_id IS NOT NULL"
            .into(),
        cols: &["ts", "utid", "callsite_id"],
        order: "ts",
    }
}

/// Every callsite with its frame's function and binary.
pub fn callsites() -> Query {
    Query {
        name: "callsites",
        sql: "SELECT c.id AS id, c.parent_id AS parent_id, COALESCE(f.name, '') AS fn, \
              COALESCE(m.name, '') AS mp FROM stack_profile_callsite c \
              JOIN stack_profile_frame f ON c.frame_id = f.id \
              LEFT JOIN stack_profile_mapping m ON f.mapping = m.id"
            .into(),
        cols: &["id", "parent_id", "fn", "mp"],
        order: "",
    }
}

/// BOOTTIME minus MONOTONIC at the first clock snapshot.
pub fn clock_offset() -> Query {
    Query {
        name: "clock_offset",
        sql: "SELECT b.clock_value - m.clock_value AS off \
              FROM clock_snapshot b JOIN clock_snapshot m ON b.snapshot_id = m.snapshot_id \
              WHERE b.clock_name = 'BOOTTIME' AND m.clock_name = 'MONOTONIC' LIMIT 1"
            .into(),
        cols: &["off"],
        order: "",
    }
}

/// The threads with these tids.
pub fn threads_by_tid(tids: &[i64]) -> Query {
    let cond = if tids.is_empty() { "0".into() } else { format!("tid IN ({})", join(tids)) };
    Query {
        name: "threads_by_tid",
        sql: format!("SELECT utid, tid FROM thread WHERE {cond}"),
        cols: &["utid", "tid"],
        order: "utid",
    }
}

/// Total time per thread state for these threads.
pub fn state_totals(utids: &[i64]) -> Query {
    let cond = if utids.is_empty() { "0".into() } else { format!("utid IN ({})", join(utids)) };
    Query {
        name: "state_totals",
        sql: format!(
            "SELECT utid, state, SUM(MAX(dur, 0)) AS d FROM thread_state WHERE {cond} \
             GROUP BY utid, state"
        ),
        cols: &["utid", "state", "d"],
        order: "",
    }
}

fn join(xs: &[i64]) -> String {
    xs.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
}

/// The most frequent FIDL methods.
pub fn facts_methods() -> Query {
    Query {
        name: "facts_methods",
        sql: "SELECT name, COUNT(*) AS c FROM slice WHERE category = 'kernel:ipc' \
              GROUP BY 1 ORDER BY c DESC LIMIT 40"
            .into(),
        cols: &["name", "c"],
        order: "c DESC",
    }
}

/// The most frequent interrupts and their mean duration.
pub fn facts_irqs() -> Query {
    Query {
        name: "facts_irqs",
        sql: "SELECT a.display_value AS irq, COUNT(*) AS c, AVG(CASE WHEN s.dur >= 0 THEN s.dur END) / 1e3 AS us FROM slice s \
              JOIN args a ON a.arg_set_id = s.arg_set_id AND a.key IN ('irq', 'irq #') \
              WHERE s.category = 'kernel:irq' GROUP BY 1 ORDER BY c DESC LIMIT 20"
            .into(),
        cols: &["irq", "c", "us"],
        order: "c DESC",
    }
}

/// The busiest threads.
pub fn facts_threads() -> Query {
    Query {
        name: "facts_threads",
        sql:
            "SELECT p.name AS pn, t.name AS tn, SUM(MAX(s.dur, 0)) / 1e6 AS ms, COUNT(*) AS n FROM sched s \
              JOIN thread t USING(utid) LEFT JOIN process p USING(upid) WHERE s.utid != 0 \
              GROUP BY 1, 2 ORDER BY ms DESC LIMIT 30"
                .into(),
        cols: &["pn", "tn", "ms", "n"],
        order: "ms DESC",
    }
}

/// The most frequent non-kernel trace slices.
pub fn facts_slices() -> Query {
    Query {
        name: "facts_slices",
        sql: "SELECT category, name, COUNT(*) AS c FROM slice WHERE category NOT LIKE 'kernel:%' \
              GROUP BY 1, 2 ORDER BY c DESC LIMIT 30"
            .into(),
        cols: &["category", "name", "c"],
        order: "c DESC",
    }
}

/// One instance of every query, for the end-to-end test.
pub fn all_for_test() -> Vec<Query> {
    vec![
        thread_states(1),
        thread_sched(1),
        ipc_for_thread(1),
        ipc_for_method("fuchsia.example/Echo.Ping"),
        slices_named("it's"),
        threads(),
        irqs(),
        rates(),
        bounds(),
        ncpu(),
        deadline_threads(),
        cpu_busy(),
        perf_samples(),
        callsites(),
        clock_offset(),
        threads_by_tid(&[1, 2]),
        threads_by_tid(&[]),
        state_totals(&[1, 2]),
        state_totals(&[]),
        facts_methods(),
        facts_irqs(),
        facts_threads(),
        facts_slices(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_escaped() {
        assert_eq!(str_to_sql_literal("it's"), "'it''s'");
    }

    #[test]
    fn every_query_names_its_columns() {
        for q in all_for_test() {
            assert!(!q.cols.is_empty(), "{}", q.name);
            for part in q.order.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let col = part.split(' ').next().unwrap();
                assert!(
                    q.cols.contains(&col),
                    "{}: order column {col} is not a result column",
                    q.name
                );
            }
        }
    }
}
