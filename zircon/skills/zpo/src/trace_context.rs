// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the agent gets to know about the recorded traces, and whether it may
//! query them.
//!
//! The agent always gets a short table per capture (the critical-path threads
//! with their utid in that capture and their running / runnable / blocked
//! totals), so it is useful even without trace access. If its terminal
//! commands run without approval (the backend's `query-access`), it also gets
//! warm trace processor sessions to query.

use crate::profiles_file::NS_PER_MS_F64;
use crate::runner::Ctx;
use crate::tp::client::Client;
use crate::tp::queries;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

/// The preference the UI's "I've allowlisted it" checkbox sets.
pub const PREF_ALLOWLISTED: &str = "agent_tp_allowlisted";

/// The command the agent would run to query a warm session.
pub fn allow_rule(tp: &Path) -> String {
    format!("command({} query --remote)", tp.display())
}

fn koid(label: &str) -> Option<i64> {
    // The trailing "[<koid>]" of a label.
    let digits = label.strip_suffix(']')?.rsplit_once('[')?.1;
    digits.bytes().all(|b| b.is_ascii_digit()).then(|| digits.parse().ok()).flatten()
}

/// Thread labels on the critical path, as the report names them.
pub fn critical_threads(summary: &Value) -> Vec<String> {
    let mut labels: Vec<String> = summary
        .get("proposals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("label").and_then(Value::as_str).map(str::to_string))
        .collect();
    let sj = summary.get("slowest_job").cloned().unwrap_or(json!({}));
    labels.extend(sj.get("thread").and_then(Value::as_str).map(str::to_string));
    labels.extend(
        sj.get("segments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| s.get("thread").and_then(Value::as_str).map(str::to_string)),
    );
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        if !l.is_empty() && !out.contains(&l) && koid(&l).is_some() {
            out.push(l);
        }
    }
    out
}

/// Markdown table: each thread's utid in this capture and its state totals.
pub fn facts(client: &Client, labels: &[String]) -> anyhow::Result<String> {
    let mut koids: IndexMap<i64, String> = IndexMap::new();
    for l in labels {
        if let Some(k) = koid(l) {
            koids.insert(k, l.clone());
        }
    }
    if koids.is_empty() {
        return Ok("(no critical-path threads with a koid)\n".into());
    }
    let tids: Vec<i64> = koids.keys().copied().collect();
    let mut utid: HashMap<i64, i64> = HashMap::new();
    for r in client.query(&queries::threads_by_tid(&tids))?.iter() {
        utid.entry(r.get::<i64>(1)).or_insert(r.get::<i64>(0)); // first utid for a recycled tid
    }
    let mut tot: HashMap<i64, HashMap<String, i64>> = HashMap::new();
    if !utid.is_empty() {
        let mut us: Vec<i64> = utid.values().copied().collect();
        us.sort_unstable();
        for r in client.query(&queries::state_totals(&us))?.iter() {
            tot.entry(r.get::<i64>(0)).or_default().insert(r.get::<String>(1), r.get::<i64>(2));
        }
    }
    let ms = |ns: i64| format!("{:.1}", ns as f64 / NS_PER_MS_F64);
    let mut rows = vec![
        "| thread (as in the report) | utid | Running ms | Runnable (R+W) ms | Blocked (S) ms |"
            .to_string(),
        "|---|---|---|---|---|".to_string(),
    ];
    let empty = HashMap::new();
    for (k, lab) in &koids {
        let u = utid.get(k);
        let t = u.and_then(|u| tot.get(u)).unwrap_or(&empty);
        let g = |s: &str| t.get(s).copied().unwrap_or(0);
        rows.push(format!(
            "| `{lab}` | {} | {} | {} | {} |",
            u.map_or("not in this capture".to_string(), |u| u.to_string()),
            ms(g("Running")),
            ms(g("R") + g("W")),
            ms(g("S"))
        ));
    }
    Ok(rows.join("\n") + "\n")
}

/// Trace context for the agent: {"tp", "warm", "captures": [{"tag",
/// "condition", "session"}], "facts_md"}.
pub fn build(
    ctx: &Ctx,
    sdir: &Path,
    measurement: &str,
    summary: &Value,
    log: &dyn Fn(&str),
) -> anyhow::Result<Value> {
    let info: Value = serde_json::from_str(&std::fs::read_to_string(
        sdir.join(measurement).join("measurement.json"),
    )?)?;
    let mut caps: Vec<Value> = info
        .get("captures")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| {
            Path::new(c.get("trace").and_then(Value::as_str).unwrap_or("")).exists()
                && !c.get("profile").and_then(Value::as_bool).unwrap_or(false)
        })
        .cloned()
        .collect();
    let s = |c: &Value, k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    caps.sort_by_key(|c| (s(c, "condition") != "stress", s(c, "tag")));
    let labels = critical_threads(summary);
    let mut captures = Vec::new();
    let mut facts_md = String::new();
    for c in &caps {
        let client = ctx.sessions.open(Path::new(&s(c, "trace")), log)?;
        let f = facts(&client, &labels)?;
        let cond = if s(c, "condition") == "stress" { "every core busy" } else { "idle system" };
        captures.push(json!({"tag": s(c, "tag"), "condition": cond, "session": client.session()}));
        facts_md += &format!("\n#### `{}` ({cond})\n\n{f}", s(c, "tag"));
    }
    Ok(json!({
        "tp": ctx.sessions.tp_bin().display().to_string(),
        "captures": captures,
        "facts_md": facts_md,
        "warm": !caps.is_empty(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_threads_dedupes_and_keeps_report_order() {
        let summary = json!({
            "proposals": [{"label": "a/t1[11]"}, {"label": "b/t2[22]"}],
            "slowest_job": {"thread": "a/t1[11]", "segments": [{"thread": "c/t3[33]"}, {"thread": "irq"}]},
        });
        assert_eq!(critical_threads(&summary), vec!["a/t1[11]", "b/t2[22]", "c/t3[33]"]);
    }

    #[test]
    fn allow_rule_names_the_trace_processor() {
        assert_eq!(allow_rule(Path::new("/fake/tp")), "command(/fake/tp query --remote)");
    }
}
