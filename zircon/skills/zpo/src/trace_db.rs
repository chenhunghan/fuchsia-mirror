// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Loads an .fxt capture into a [`Trace`] through a warm trace processor
//! session.
//!
//! Only the global tables (threads, IRQs, rate counters) are read up front;
//! thread states, sched slices and IPC are loaded per thread on first use,
//! since the analysis only touches the threads on a task's critical path.

use crate::model::{Irq, Trace};
use crate::tp::client::Client;
use crate::tp::queries;
use crate::tp::session::Sessions;
use std::path::Path;
use std::time::Instant;

/// The kernel traces `Rate:CPU` as the processing rate times 1000
/// (`round_1000` in the scheduler), so 1000 is the reference core's rate.
const RATE_COUNTER_SCALE: f64 = 1000.0;

/// Loads `path`, reusing its warm session if one is running.
pub fn load(sessions: &Sessions, path: &Path, log: &dyn Fn(&str)) -> anyhow::Result<Trace> {
    let client = sessions.open(path, log)?;
    load_with(client, path, log)
}

/// Loads a trace through an existing client.
pub fn load_with(client: Client, path: &Path, log: &dyn Fn(&str)) -> anyhow::Result<Trace> {
    let t0 = Instant::now();
    let threads = client.query(&queries::threads())?;
    let irqs = client.query(&queries::irqs())?;
    let rates = client.query(&queries::rates())?;
    let bounds = client.query(&queries::bounds())?;
    let ncpu = client.query(&queries::ncpu())?;
    let mut tr = Trace::lazy(Box::new(client));
    tr.path = path.display().to_string();
    for r in threads.iter() {
        let utid = r.get::<i64>(0);
        tr.threads.insert(
            utid,
            crate::model::ThreadInfo {
                utid,
                tid: r.get::<i64>(1),
                name: r.get::<String>(2),
                pid: r.get::<i64>(3),
                process: r.get::<String>(4),
            },
        );
    }
    // IRQ slices are emitted on kernel pseudo-threads named cpu-N.
    let mut n = 0;
    for r in irqs.iter() {
        let tname = r.get::<String>(2);
        let cpu = tname.split_once('-').and_then(|(_, c)| c.parse::<i64>().ok()).unwrap_or(-1);
        let irq = r.get::<Option<i64>>(3).unwrap_or(-1);
        tr.irqs.entry(cpu).or_default().push(Irq {
            ts: r.get::<i64>(0),
            dur: r.get::<i64>(1),
            cpu,
            irq,
        });
        n += 1;
    }
    for r in rates.iter() {
        let name = r.get::<String>(0);
        let Some(cpu) = name.rsplit_once(':').and_then(|(_, c)| c.parse::<i64>().ok()) else {
            continue;
        };
        tr.rates
            .entry(cpu)
            .or_default()
            .push((r.get::<i64>(1), r.get::<f64>(2) / RATE_COUNTER_SCALE));
    }
    if let Some(b) = bounds.first() {
        tr.start_ts = b.get::<i64>(0);
        tr.end_ts = b.get::<i64>(1);
    }
    if let Some(c) = ncpu.first() {
        tr.ncpu = c.get::<i64>(0);
    }
    tr.finalize();
    let base = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    log(&format!(
        "opened {base}: {} threads, {n} irqs, rates={}, {:.1}s, {:.1}s",
        tr.threads.len(),
        if tr.has_rates { "yes" } else { "no" },
        (tr.end_ts - tr.start_ts) as f64 / 1e9,
        t0.elapsed().as_secs_f64()
    ));
    Ok(tr)
}
