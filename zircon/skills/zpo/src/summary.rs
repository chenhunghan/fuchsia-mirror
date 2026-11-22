// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A measurement's summary (`summary.json`): what the UI and the HTML report
//! show for it.

use crate::analysis::{self, NoJobs};
use crate::attribution::{Attribution, Bucket};
use crate::device::Log;
use crate::model::Trace;
use crate::profiles_file::{NS_PER_US, NS_PER_US_F64};
use crate::runner::{self, Ctx, Runner, Session, shares_json};
use crate::stats;
use crate::tp::queries;
use anyhow::Context as _;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn read_json(p: &Path) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(
        &std::fs::read_to_string(p).with_context(|| format!("could not read {}", p.display()))?,
    )?)
}

/// The summary of a stats sample as JSON.
pub fn summary_json(xs: &[i64]) -> Value {
    serde_json::to_value(stats::summary_i64(xs)).unwrap_or(Value::Null)
}

fn params_json(p: Option<(i64, i64)>) -> Value {
    p.map_or(Value::Null, |(a, b)| json!([a, b]))
}

/// The checked `--window` of `zpo analyze` (ns), after checking `--quantile`
/// and `--tail`.
pub fn check_analysis_args(
    window: Option<&str>,
    quantile: f64,
    tail: f64,
) -> anyhow::Result<Option<i64>> {
    // A quantile of 1 is the maximum; a tail of 1 would be no tail.
    if !(quantile > 0.0 && quantile <= 1.0) {
        anyhow::bail!("--quantile must be in (0, 1], got {quantile}");
    }
    if !(tail > 0.0 && tail < 1.0) {
        anyhow::bail!("--tail must be in (0, 1), got {tail}");
    }
    match window {
        None => Ok(None),
        Some(w) => match crate::profiles_file::parse_dur_ns(w) {
            Some(ns) if ns > 0 => Ok(Some(ns)),
            _ => anyhow::bail!(
                "--window takes a positive duration such as 850us or 16.6ms, got {w:?}"
            ),
        },
    }
}

/// Fails when a `--role LABEL=ROLE` of `zpo profile-apply` names no
/// proposal, instead of silently applying the others.
pub fn check_role_overrides(
    proposals: &[Value],
    overrides: &[(String, String)],
) -> anyhow::Result<()> {
    for (l, _) in overrides {
        if !proposals.iter().any(|p| p["label"].as_str() == Some(l)) {
            anyhow::bail!("--role {l}: the measurement has no proposal for that thread");
        }
    }
    Ok(())
}

/// (label, deadline params) of each thread in an analysis: the output of
/// `analyze --json` (`threads`: id -> {label, ...}) or a measurement's
/// analysis.json (capture -> {threads: label -> {...}}), first seen first.
pub fn analysis_threads(j: &Value) -> Vec<(String, Option<(i64, i64)>)> {
    let params = |t: &Value| {
        t.get("deadline_params")
            .and_then(Value::as_array)
            .and_then(|a| Some((a.first()?.as_i64()?, a.get(1)?.as_i64()?)))
    };
    let mut out: Vec<(String, Option<(i64, i64)>)> = Vec::new();
    let mut add = |label: &str, t: &Value| {
        if !label.is_empty() && !out.iter().any(|(l, _)| l == label) {
            out.push((label.to_string(), params(t)));
        }
    };
    if let Some(threads) = j.get("threads").and_then(Value::as_object) {
        for t in threads.values() {
            add(t["label"].as_str().unwrap_or(""), t);
        }
    } else {
        for cap in j.as_object().into_iter().flat_map(|o| o.values()) {
            for (label, t) in cap.get("threads").and_then(Value::as_object).into_iter().flatten() {
                add(label, t);
            }
        }
    }
    out
}

/// What a hardware wait waited for: its interrupt vector (0 is a real
/// vector), or -1 / none for a timer or the thread itself.
pub fn hw_wait_detail(vector: Option<i64>) -> String {
    match vector {
        Some(d) if d >= 0 => format!("irq {d}"),
        _ => "timer/self".into(),
    }
}

/// The slowest job's segments, for the timeline view.
pub fn timeline(a: &Attribution, tr: &Trace) -> Value {
    let segs: Vec<Value> = a
        .segments
        .iter()
        .map(|sg| {
            let detail = match sg.bucket {
                Bucket::BlockedOn => tr.thread_label(sg.detail),
                Bucket::HwWait => hw_wait_detail(sg.detail),
                _ => String::new(),
            };
            json!({
                "t0_us": (sg.t0 - a.t0) as f64 / NS_PER_US_F64, "dur_us": sg.dur() as f64 / NS_PER_US_F64, "bucket": sg.bucket.as_str(),
                "thread": tr.label(sg.utid), "depth": sg.depth, "cpu": sg.cpu, "detail": detail,
            })
        })
        .collect();
    // t0_ns / t1_ns are raw trace timestamps (ui.perfetto.dev's visStart/End).
    json!({
        "thread": tr.label(a.utid), "total_us": a.total() as f64 / NS_PER_US_F64,
        "t0_ns": a.t0, "t1_ns": a.t0 + a.total(), "segments": segs,
    })
}

/// Full analysis of every capture of a measurement and its conclusion
/// ([`Runner::conclude`]); writes analysis.json, report.md and summary.json.
/// The UI and the command line (`measure`, `session-analyze`) both call this,
/// so they show the same verdict and proposals; this function only lays the
/// conclusion out.
pub fn analyze_measurement(
    ctx: &Ctx,
    sdir: &Path,
    name: &str,
    log: Log,
    check_cancel: Option<runner::CheckCancel>,
) -> anyhow::Result<Value> {
    let s_ = Session::load(sdir)?;
    let r = Runner::new(
        s_.clone(),
        ctx.clone(),
        None,
        std::sync::Arc::new(|_| Ok(())),
        log.clone(),
        check_cancel,
    );
    let mdir = sdir.join(name);
    // An explanation of the previous analysis (another task or window) no
    // longer matches.
    for f in ["explain.md", "explain.json"] {
        let _ = std::fs::remove_file(mdir.join(f));
    }
    let results = r.analyze(name, analysis::MAX_JOBS)?;
    let info = read_json(&mdir.join("measurement.json"))?;
    let capture_rows = |results: &IndexMap<String, Value>| -> Vec<Value> {
        results
            .iter()
            .map(|(t, v)| {
                let resp: Vec<f64> = v
                    .get("response_ns")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_f64).collect())
                    .unwrap_or_default();
                let q = |x: f64| {
                    if resp.is_empty() { json!(0) } else { json!(stats::quantile(&resp, x)) }
                };
                json!({
                    "tag": t, "condition": v["condition"], "jobs": resp.len(),
                    "p50": q(0.5), "p90": q(0.9), "p99": q(0.99),
                    "verdict": v.pointer("/verdict/primary").cloned().unwrap_or(Value::Null),
                    "error": v.get("error").cloned().unwrap_or(Value::Null),
                })
            })
            .collect()
    };
    let mc = match r.conclude(name, &results) {
        Ok(mc) => mc,
        Err(e) if e.downcast_ref::<NoJobs>().is_some() => {
            let mut out = json!({"measurement": name, "error": e.to_string(), "captures": capture_rows(&results)});
            // The gate still runs (`conclude` would have run it): it is what
            // flags a candidate's empty captures.
            if name != "baseline" && sdir.join("baseline/analysis.json").exists() {
                if let Ok(g) = runner::gate_files(sdir, "baseline", name) {
                    out["vs_baseline"] = serde_json::to_value(g)?;
                }
            }
            runner::write_atomic(
                &mdir.join("summary.json"),
                serde_json::to_string_pretty(&out)?.as_bytes(),
            )?;
            return Ok(out);
        }
        Err(e) => return Err(e),
    };
    let (res, tr, vd) = (&mc.res, &mc.tr, &mc.conclusion.verdict);
    let proposals: Vec<Value> = mc
        .conclusion
        .shown
        .iter()
        .zip(&mc.shown)
        .map(|(p, sp)| {
            json!({
                "utid": p.utid, "label": p.label, "capacity_us": p.capacity_ns.div_euclid(NS_PER_US),
                "period_us": p.period_ns.div_euclid(NS_PER_US), "affinity": p.affinity, "utilization": p.utilization,
                "existing": params_json(p.existing), "changed": p.changed(), "profiles_json": p.profiles_json(),
                "notes": p.notes, "work_per_period_ns": p.work_per_period_ns,
                "roles": sp.roles.iter().map(|x| json!({"role": x.role, "score": x.score, "why": x.why})).collect::<Vec<_>>(),
                "recommended": sp.recommended,
            })
        })
        .collect();
    let slowest = (0..res.attributions.len())
        .max_by(|&a, &b| {
            res.attributions[a].total().cmp(&res.attributions[b].total()).then(b.cmp(&a))
        })
        .unwrap_or(0);
    let tl = timeline(&res.attributions[slowest], tr);
    let mut out = json!({
        "measurement": name,
        "report_md": mc.report_md,
        "verdict": {
            "primary": vd.primary, "headline": vd.headline, "recommend_profiles": vd.recommend_profiles,
            "shares_tail": shares_json(&vd.shares_tail),
            "findings": vd.findings,
        },
        "response": summary_json(&res.response_ns),
        "condition": mc.condition,
        "buckets": Value::Object(res.bucket_per_job.iter().map(|(b, xs)| (b.as_str().to_string(), summary_json(xs))).collect()),
        "proposals": proposals,
        "slowest_job": tl,
        "report_capture": mc.report_tag,
        "captures": capture_rows(&results),
    });
    if let Some(g) = &mc.vs_baseline {
        out["vs_baseline"] = serde_json::to_value(g)?;
    }
    // CPU profile captures: where the slowest jobs' run time goes, by function.
    if !runner::profile_captures(&info).is_empty() {
        out["run_functions"] = json!(r.analyze_profiles(name, analysis::MAX_JOBS)?);
    } else if let Some(e) = info.get("profile_error") {
        out["run_functions_error"] = e.clone();
    }
    runner::write_atomic(&mdir.join("report.md"), mc.report_md.as_bytes())?;
    runner::write_atomic(
        &mdir.join("summary.json"),
        serde_json::to_string_pretty(&out)?.as_bytes(),
    )?;
    Ok(out)
}

/// The conclusion of a summary as the command line prints it: the verdict
/// headline, the comparison with the baseline, and the proposals, with the
/// same numbers the UI shows.
pub fn conclusion_text(summary: &Value) -> String {
    let mut l = Vec::new();
    if let Some(e) = summary.get("error").and_then(Value::as_str) {
        l.push(format!("error: {e}"));
        return l.join("\n");
    }
    l.push(format!(
        "verdict: {} ({})",
        s(&summary["verdict"], "headline"),
        s(&summary["verdict"], "primary")
    ));
    if let Some(g) = summary.get("vs_baseline") {
        l.push(format!("vs. baseline: {}", s(g, "status").to_uppercase()));
    }
    let props = summary.get("proposals").and_then(Value::as_array).cloned().unwrap_or_default();
    if !props.is_empty() {
        l.push("proposals (capacity / period, CPUs):".into());
    }
    for p in &props {
        let existing = p
            .get("existing")
            .and_then(Value::as_array)
            .and_then(|a| {
                Some(format!(
                    "{}us / {}us",
                    a.first()?.as_i64()? / NS_PER_US,
                    a.get(1)?.as_i64()? / NS_PER_US
                ))
            })
            .unwrap_or_else(|| "fair".into());
        let aff: Vec<String> = p
            .get("affinity")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| c.to_string())
            .collect();
        let role = p.pointer("/roles/0/role").and_then(Value::as_str).unwrap_or("?");
        l.push(format!(
            "  {} {}: {}us / {}us, CPUs {} (now {existing}; role {role})",
            if p["recommended"].as_bool() == Some(true) { "*" } else { " " },
            s(p, "label"),
            p["capacity_us"],
            p["period_us"],
            aff.join(",")
        ));
    }
    if props.iter().any(|p| p["recommended"].as_bool() == Some(true)) {
        l.push("  (* recommended by the verdict)".into());
    }
    l.join("\n")
}

/// Response-time quantiles per condition, all repeats of a condition pooled
/// (cached next to analysis.json).
pub fn by_condition(sdir: &Path, mname: &str) -> Value {
    let ap = sdir.join(mname).join("analysis.json");
    let cp = sdir.join(mname).join("by_condition.json");
    let Ok(amt) = ap.metadata().and_then(|m| m.modified()) else { return json!({}) };
    if let Ok(cmt) = cp.metadata().and_then(|m| m.modified()) {
        // Strictly newer: a cache written in the same timestamp tick as a new
        // analysis.json may predate it.
        if cmt > amt {
            if let Ok(v) = read_json(&cp) {
                return v;
            }
        }
    }
    let Ok(ana) = read_json(&ap) else { return json!({}) };
    let mut pool: IndexMap<String, Vec<f64>> = IndexMap::new();
    for v in ana.as_object().into_iter().flat_map(|o| o.values()) {
        let c = v.get("condition").and_then(Value::as_str).unwrap_or("?").to_string();
        let xs = v
            .get("response_ns")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>());
        pool.entry(c).or_default().extend(xs.unwrap_or_default());
    }
    let mut out = Map::new();
    for (c, xs) in pool {
        if xs.is_empty() {
            continue;
        }
        out.insert(
            c,
            json!({"p50": stats::quantile(&xs, 0.5), "p90": stats::quantile(&xs, 0.9), "p99": stats::quantile(&xs, 0.99), "n": xs.len()}),
        );
    }
    let out = Value::Object(out);
    let _ = runner::write_atomic(&cp, serde_json::to_string(&out).unwrap_or_default().as_bytes());
    out
}

/// The captures of a measurement with their trace files and any Trace Viewer
/// link already uploaded.
pub fn trace_files(sdir: &Path, mname: &str) -> Value {
    let Ok(info) = read_json(&sdir.join(mname).join("measurement.json")) else { return json!([]) };
    let urls = read_json(&sdir.join(mname).join("trace_urls.json")).unwrap_or(json!({}));
    let rows: Vec<Value> = runner_captures(&info)
        .iter()
        .map(|c| {
            json!({
                "tag": c["tag"], "condition": c.get("condition").cloned().unwrap_or(json!("")),
                "exists": Path::new(s(c, "trace")).exists(),
                "url": urls.get(s(c, "tag")).cloned().unwrap_or(Value::Null),
                "profile": c.get("profile").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    json!(rows)
}

fn runner_captures(info: &Value) -> Vec<Value> {
    info.get("captures").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// The trace file of one capture, if it exists.
pub fn trace_path(sdir: &Path, mname: &str, tag: &str) -> Option<PathBuf> {
    let info = read_json(&sdir.join(mname).join("measurement.json")).ok()?;
    runner_captures(&info)
        .iter()
        .find(|c| s(c, "tag") == tag && Path::new(s(c, "trace")).exists())
        .map(|c| PathBuf::from(s(c, "trace")))
}

/// Facts about a short trace for the agent's task discovery.
pub fn trace_facts(tr: &Trace) -> anyhow::Result<String> {
    let mut l = vec!["Top FIDL methods (kernel:ipc write instants):".to_string()];
    for r in tr.query(&queries::facts_methods())?.iter() {
        l.push(format!("  {:6}  {}", r.get::<i64>(1), r.get::<String>(0)));
    }
    l.push("Interrupts (irq number: count, mean us):".into());
    for r in tr.query(&queries::facts_irqs())?.iter() {
        l.push(format!(
            "  irq {}: {} ({:.1} us)",
            r.get::<Option<String>>(0).unwrap_or_else(|| "None".into()),
            r.get::<i64>(1),
            r.get::<f64>(2)
        ));
    }
    l.push("Busiest threads (run ms):".into());
    for r in tr.query(&queries::facts_threads())?.iter() {
        l.push(format!(
            "  {:8.1} ms  {:6} slices  {}/{}",
            r.get::<f64>(2),
            r.get::<i64>(3),
            r.get::<Option<String>>(0).unwrap_or_else(|| "None".into()),
            r.get::<Option<String>>(1).unwrap_or_else(|| "None".into())
        ));
    }
    l.push("Non-kernel trace slices by name (top):".into());
    for r in tr.query(&queries::facts_slices())?.iter() {
        l.push(format!("  {:6}  [{}] {}", r.get::<i64>(2), r.get::<String>(0), r.get::<String>(1)));
    }
    Ok(l.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_zero_is_an_interrupt() {
        assert_eq!(hw_wait_detail(Some(0)), "irq 0");
        assert_eq!(hw_wait_detail(Some(-1)), "timer/self");
        assert_eq!(hw_wait_detail(None), "timer/self");
    }

    #[test]
    fn by_condition_recomputes_a_cache_as_old_as_the_analysis() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("m")).unwrap();
        let (ap, cp) = (d.path().join("m/analysis.json"), d.path().join("m/by_condition.json"));
        std::fs::write(&ap, r#"{"idle-1": {"condition": "idle", "response_ns": [5, 5]}}"#).unwrap();
        std::fs::write(&cp, r#"{"stale": {}}"#).unwrap();
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for p in [&ap, &cp] {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        }
        let v = by_condition(d.path(), "m");
        assert_eq!(v["idle"]["n"], 2);
        assert!(v.get("stale").is_none());
    }

    #[test]
    fn analyze_arguments_are_checked() {
        assert_eq!(check_analysis_args(Some("850us"), 0.99, 0.9).unwrap(), Some(850_000));
        assert_eq!(check_analysis_args(None, 1.0, 0.9).unwrap(), None);
        for (w, q, t) in [("850usec", 0.99, 0.9), ("0ms", 0.99, 0.9), ("-5ms", 0.99, 0.9)] {
            assert!(check_analysis_args(Some(w), q, t).is_err(), "{w}");
        }
        assert!(check_analysis_args(None, 0.0, 0.9).is_err());
        assert!(check_analysis_args(None, 0.99, 1.0).is_err());
    }

    #[test]
    fn an_unmatched_role_override_is_an_error() {
        let props = vec![json!({"label": "p/t[1]"})];
        let ov = |l: &str| vec![(l.to_string(), "r".to_string())];
        assert!(check_role_overrides(&props, &ov("p/t[1]")).is_ok());
        assert!(check_role_overrides(&props, &ov("p/x[2]")).is_err());
    }

    #[test]
    fn roles_read_both_analysis_shapes() {
        let cli = json!({"threads": {"7": {"label": "p/t[1]", "deadline_params": [100, 1000]}}});
        let measured = json!({
            "idle-1": {"threads": {"p/t[1]": {"deadline_params": [100, 1000]}, "p/u[2]": {}}},
            "stress-1": {"threads": {"p/t[1]": {}}},
        });
        assert_eq!(analysis_threads(&cli), vec![("p/t[1]".to_string(), Some((100, 1000)))]);
        assert_eq!(
            analysis_threads(&measured),
            vec![("p/t[1]".to_string(), Some((100, 1000))), ("p/u[2]".to_string(), None)]
        );
    }
}
