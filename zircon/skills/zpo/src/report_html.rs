// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `zpo report --format html`: one self-contained HTML file of saved sessions.
//!
//! The page is the UI itself, with every API response it would fetch read
//! from disk and inlined, like babeldiff's HTML report: no server, no external
//! scripts. Reading works as in the UI (tabs, hover cards, function tables);
//! anything that needs a live zpo (measure, build + OTA, apply, the agent, trace
//! downloads and Perfetto) is disabled.

use crate::runner::Ctx;
use crate::server::{self, INDEX_HTML, State};
use anyhow::{Context as _, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const OLD_API: &str = "async function api(path, body) {
  const headers = zpoHeaders();
  let r;
  try {
    r = await fetch(path, body ? {method:'POST', headers:{...headers, 'Content-Type':'application/json'}, body: JSON.stringify(body)} : {headers});
  } catch (e) { connLost(); return {error: lostText()}; }
  if (!fromZpo(r)) { connLost(); return {error: lostText()}; }
  let j;
  try { j = await r.json(); } catch (e) { return {error: `bad response (${r.status})`}; }
  if (r.status === 403 && j.signed_in === false) connBanner('auth');
  return j;
}";

/// Renders the sessions in `dirs` (each a session directory) as one page.
pub fn render(ctx: &Ctx, dirs: &[PathBuf], first: Option<&str>) -> anyhow::Result<String> {
    let norm: Vec<PathBuf> = dirs
        .iter()
        .map(|d| {
            std::fs::canonicalize(d)
                .or_else(|_| std::path::absolute(d))
                .with_context(|| format!("could not resolve {}", d.display()))
        })
        .collect::<anyhow::Result<_>>()?;
    if norm.is_empty() {
        bail!("no sessions to report");
    }
    // The UI names sessions by directory name, so names must be unique; the
    // directories themselves can live anywhere.
    let mut wanted: Vec<String> = Vec::new();
    let mut states: Vec<(String, Arc<State>)> = Vec::new();
    for d in &norm {
        let name = d
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .context("a session directory has no name")?;
        if wanted.contains(&name) {
            bail!("two --session directories are both named {name:?}");
        }
        let parent = d.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf);
        let parent = parent.context("a session directory has no parent")?;
        states.push((name.clone(), Arc::new(State::new(parent, ctx.clone(), None)?)));
        wanted.push(name);
    }
    // Requests not about one session (lessons, checkout) go to the first.
    let first_st = states[0].1.clone();
    let get = |p: &str| {
        let st = states
            .iter()
            .find(|(n, _)| {
                let enc = format!("/api/session/{}/", encode(n));
                let raw = format!("/api/session/{n}/");
                p.starts_with(&enc) || p.starts_with(&raw)
            })
            .map_or(&first_st, |(_, st)| st);
        server::get_json(st, p)
    };
    let mut mock = Map::new();
    mock.insert("/api/lessons".into(), get("/api/lessons").unwrap_or(Value::Null));
    // Reports get attached to bugs and reviews: no local paths (the checkout,
    // session dirs) in them. The page hides a null checkout.
    mock.insert("/api/checkout".into(), Value::Null);
    mock.insert("/api/device".into(), json!({"ok": false, "error": "static report"}));
    mock.insert(
        "/api/agent".into(),
        json!({"available": false, "name": "", "hint": "static report"}),
    );
    mock.insert("/api/jobs".into(), json!([]));
    let mut sessions: Vec<Value> = Vec::new();
    for ((name, st), d) in states.iter().zip(&norm) {
        let mut found = server::get_json(st, "/api/sessions")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .find(|s| s["name"] == name.as_str())
            .with_context(|| {
                format!("{} is not a zpo session (it has no session.json)", d.display())
            })?;
        if let Some(o) = found.as_object_mut() {
            o.remove("dir");
        }
        sessions.push(found);
    }
    for s in &sessions {
        let n = s["name"].as_str().unwrap_or("");
        let enc = encode(n);
        let gates = get(&format!("/api/session/{enc}/gates")).unwrap_or(json!([]));
        mock.insert(format!("/api/session/{n}/gates"), gates.clone());
        mock.insert(format!("/api/session/{enc}/gates"), gates);
        for m in s["measurements"].as_array().into_iter().flatten() {
            if m["analyzed"].as_bool() != Some(true) {
                continue;
            }
            let mn = m["name"].as_str().unwrap_or("");
            let enc_m = encode(mn);
            if let Some(mut summ) = get(&format!("/api/session/{enc}/{enc_m}/summary")) {
                // No trace downloads or Perfetto in a static page.
                if let Some(ts) = summ.get_mut("traces").and_then(Value::as_array_mut) {
                    for t in ts {
                        t["exists"] = json!(false);
                    }
                }
                summ["agent"] = json!({"available": false, "name": "", "hint": "static report"});
                mock.insert(format!("/api/session/{n}/{mn}/summary"), summ.clone());
                mock.insert(format!("/api/session/{enc}/{enc_m}/summary"), summ);
            }
        }
    }
    if let Some(f) = first {
        sessions.sort_by_key(|s| if s["name"] == f { 0 } else { 1 });
    }
    mock.insert("/api/sessions".into(), json!(sessions));
    if !INDEX_HTML.contains(OLD_API) {
        bail!("the UI's api() function changed; update report_html.rs");
    }
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    // `</script>` or `<!--` inside JSON can confuse HTML `<script>` parsing.
    let mock_js = serde_json::to_string(&Value::Object(mock))?
        .replace("</", "<\\/")
        .replace("<!--", "<\\!--");
    let new_api = format!(
        "const ZPO_STATIC = true;
const MOCK = {mock_js};
const STATIC_MSG = 'This is a static zpo report (written {when}). Measuring, applying, OTA and agent questions need a running zpo: run `zpo ui start`.';
async function api(path, body) {{
  if (body) {{ await alertModal({{title: 'Static report', html: '<p>' + esc(STATIC_MSG) + '</p>'}}); return {{error: STATIC_MSG, static_report: true}}; }}
  if (path in MOCK) return JSON.parse(JSON.stringify(MOCK[path]));
  return {{error: 'not in this report: ' + path}};
}}
window.addEventListener('DOMContentLoaded', () => {{
  const b = document.createElement('div');
  b.style.cssText = 'background:#3a2d12;border-bottom:1px solid #c58b1a;color:#ffe3a3;padding:8px 20px;font-size:13px';
  b.innerHTML = '<b>Static report</b>: saved results written {when}; actions that need a device or an agent are disabled.';
  document.body.insertBefore(b, document.body.firstChild);
}});"
    );
    let html = INDEX_HTML.replacen(OLD_API, &new_api, 1).replacen(
        "pollDevice(); setInterval(pollDevice, 20000);",
        "pollDevice();",
        1,
    );
    Ok(html)
}

/// Whether the session in `dir` has something to report: at least one
/// measurement that is analyzed and has a saved summary (`measurement.json`,
/// `analysis.json` and `summary.json`), which is what [`render`] inlines. The
/// UI shows its Download Report button only then.
pub fn session_has_report(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| {
        let m = e.path();
        ["measurement.json", "analysis.json", "summary.json"].iter().all(|f| m.join(f).is_file())
    })
}

/// Like JavaScript's encodeURIComponent, which the UI uses to build paths.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_javascript() {
        assert_eq!(encode("touch fair#1"), "touch%20fair%231");
        assert_eq!(encode("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn inlines_saved_sessions() {
        let d = tempfile::tempdir().unwrap();
        let sdir = d.path().join("sessions/touch");
        let s = crate::runner::Session::new(&sdir, "thread:a/b", crate::runner::LoadDef::manual());
        s.save().unwrap();
        std::fs::create_dir_all(sdir.join("baseline")).unwrap();
        std::fs::write(sdir.join("baseline/measurement.json"), r#"{"captures": []}"#).unwrap();
        std::fs::write(sdir.join("baseline/analysis.json"), "{}").unwrap();
        std::fs::write(
            sdir.join("baseline/summary.json"),
            r#"{"measurement": "baseline", "note": "</script>"}"#,
        )
        .unwrap();
        let ctx = Ctx {
            fuchsia_dir: d.path().to_path_buf(),
            sessions: Arc::new(crate::tp::session::Sessions::new(
                Arc::new(crate::proc::HostRunner),
                d.path().join("tp"),
            )),
            ext_dirs: vec![],
            checkout: Default::default(),
            lessons: Default::default(),
        };
        let html = render(&ctx, &[sdir.clone()], Some("touch")).unwrap();
        assert!(html.contains("const ZPO_STATIC = true;"));
        assert!(html.contains("\"/api/session/touch/baseline/summary\""));
        assert!(!html.contains("\"note\":\"</script>\""));
        assert!(!html.contains("await fetch(path"));
        // No local paths: neither the checkout nor the session's directory.
        assert!(html.contains("\"/api/checkout\":null"));
        assert!(!html.contains(&format!("\"dir\":\"{}\"", sdir.display())));
        // A directory that is not a session (here a measurement) is an error,
        // not an empty report.
        let e = render(&ctx, &[sdir.join("baseline")], None).unwrap_err();
        assert!(e.to_string().contains("is not a zpo session"), "{e}");
    }

    fn ctx(d: &Path) -> Ctx {
        Ctx {
            fuchsia_dir: d.to_path_buf(),
            sessions: Arc::new(crate::tp::session::Sessions::new(
                Arc::new(crate::proc::HostRunner),
                d.join("tp"),
            )),
            ext_dirs: vec![],
            checkout: Default::default(),
            lessons: Default::default(),
        }
    }

    #[test]
    fn a_session_has_a_report_once_a_measurement_is_summarized() {
        let d = tempfile::tempdir().unwrap();
        let sdir = d.path().join("sessions/touch");
        crate::runner::Session::new(&sdir, "thread:a/b", crate::runner::LoadDef::manual())
            .save()
            .unwrap();
        let st = Arc::new(State::new(d.path().join("sessions"), ctx(d.path()), None).unwrap());
        let available =
            || server::get_json(&st, "/api/sessions").unwrap()[0]["report_available"].clone();
        assert!(!session_has_report(&sdir));
        assert_eq!(available(), json!(false));
        // Recorded and analyzed, but no summary yet.
        std::fs::create_dir_all(sdir.join("baseline")).unwrap();
        std::fs::write(sdir.join("baseline/measurement.json"), r#"{"captures": []}"#).unwrap();
        std::fs::write(sdir.join("baseline/analysis.json"), "{}").unwrap();
        assert!(!session_has_report(&sdir));
        std::fs::write(sdir.join("baseline/summary.json"), r#"{"measurement": "baseline"}"#)
            .unwrap();
        assert!(session_has_report(&sdir));
        assert_eq!(available(), json!(true));
    }

    #[test]
    fn downloads_a_sessions_report() {
        let d = tempfile::tempdir().unwrap();
        let st = Arc::new(State::new(d.path().join("sessions"), ctx(d.path()), None).unwrap());
        let get = |p: &str| {
            server::get(
                &st,
                &server::Request {
                    method: "GET".into(),
                    path: p.into(),
                    query: String::new(),
                    body: vec![],
                },
            )
        };
        let sdir = d.path().join("sessions/touch");
        crate::runner::Session::new(&sdir, "thread:a/b", crate::runner::LoadDef::manual())
            .save()
            .unwrap();
        assert!(matches!(get("/api/session/touch/report.html"), server::Response::Json(404, _)));
        assert!(matches!(get("/api/session/../report.html"), server::Response::NotFound));
        std::fs::create_dir_all(sdir.join("baseline")).unwrap();
        std::fs::write(sdir.join("baseline/measurement.json"), r#"{"captures": []}"#).unwrap();
        std::fs::write(sdir.join("baseline/analysis.json"), "{}").unwrap();
        std::fs::write(sdir.join("baseline/summary.json"), r#"{"measurement": "baseline"}"#)
            .unwrap();
        let server::Response::Bytes { code, ctype, body, headers } =
            get("/api/session/touch/report.html")
        else {
            panic!("not a download");
        };
        assert_eq!((code, ctype), (200, "text/html; charset=utf-8"));
        assert_eq!(headers, vec!["Content-Disposition: attachment; filename=\"zpo-touch.html\""]);
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("const ZPO_STATIC = true;"));
        assert!(html.contains("\"/api/session/touch/baseline/summary\""));
    }

    #[test]
    fn normalizes_relative_session_paths_and_special_names() {
        let d = tempfile::tempdir().unwrap();
        let sdir = d.path().join("sessions/touch fair#1");
        let s = crate::runner::Session::new(&sdir, "thread:a/b", crate::runner::LoadDef::manual());
        s.save().unwrap();
        std::fs::create_dir_all(sdir.join("baseline")).unwrap();
        std::fs::write(sdir.join("baseline/measurement.json"), r#"{"captures": []}"#).unwrap();
        std::fs::write(sdir.join("baseline/analysis.json"), "{}").unwrap();
        std::fs::write(
            sdir.join("baseline/summary.json"),
            r#"{"measurement": "baseline", "note": "<!-- comment -->"}"#,
        )
        .unwrap();
        let ctx = Ctx {
            fuchsia_dir: d.path().to_path_buf(),
            sessions: Arc::new(crate::tp::session::Sessions::new(
                Arc::new(crate::proc::HostRunner),
                d.path().join("tp"),
            )),
            ext_dirs: vec![],
            checkout: Default::default(),
            lessons: Default::default(),
        };
        let rel_with_dot = sdir.join(".");
        let html = render(&ctx, &[rel_with_dot], Some("touch fair#1")).unwrap();
        assert!(html.contains("\"/api/session/touch%20fair%231/baseline/summary\""));
        assert!(html.contains("\"/api/session/touch fair#1/baseline/summary\""));
        assert!(!html.contains("<!-- comment -->"));
        assert!(html.contains("static_report: true"));
    }
}
