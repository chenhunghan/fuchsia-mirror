// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Thread -> role-name resolution.
//!
//! The trace knows threads and the deadline parameters in effect; the
//! `.profiles` file knows role names and parameters. The link (which role a
//! thread requested) is only in the driver/component source, so candidates are
//! ranked by parameter match and name-token overlap, and the user (or the
//! agent) confirms.

use crate::profiles_file::{
    Affinity, NS_PER_US, RoleText, parse_affinity, parse_role_body, parse_us, role_entries,
};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// Every role in a `.profiles` file's text, as written. Like RoleManager, the
/// first definition of a role wins (later ones are ignored with a warning).
///
/// RoleManager reads these files with comments and trailing commas allowed, so
/// they are not plain JSON, and zpo edits them in place keeping comments and
/// layout. That is why this reads the file with the small scanner in
/// `profiles_file` rather than a JSON parser.
pub fn parse_roles_full(text: &str) -> IndexMap<String, RoleText> {
    let mut roles = IndexMap::new();
    for e in role_entries(text) {
        if !roles.contains_key(&e.name) {
            roles.insert(e.name, parse_role_body(e.body));
        }
    }
    roles
}

/// A role's parameters in microseconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleParams {
    pub capacity: Option<i64>,
    pub period: Option<i64>,
    pub deadline: Option<i64>,
    pub priority: Option<i64>,
}

/// Every role in a `.profiles` file's text with its parameters. Like
/// RoleManager, the first definition of a role wins.
pub fn parse_roles(text: &str) -> IndexMap<String, RoleParams> {
    let us = |v: &Option<String>| v.as_deref().and_then(|s| parse_us(s).ok());
    parse_roles_full(text)
        .into_iter()
        .map(|(name, r)| {
            let params = RoleParams {
                capacity: us(&r.capacity),
                period: us(&r.period),
                deadline: us(&r.deadline),
                priority: r.priority,
            };
            (name, params)
        })
        .collect()
}

/// [`parse_roles`] of a file.
pub fn load_roles(path: &std::path::Path) -> anyhow::Result<IndexMap<String, RoleParams>> {
    Ok(parse_roles(&std::fs::read_to_string(path)?))
}

/// A role's parameters normalized so the device's JSON and the checkout's file
/// compare equal.
pub fn normalize(v: &Value) -> Value {
    let mut out = Map::new();
    for k in ["capacity", "period", "deadline"] {
        if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
            let s = match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if let Ok(us) = parse_us(&s) {
                out.insert(k.into(), json!(us));
            }
        }
    }
    if let Some(p) = v.get("priority") {
        if let Some(i) = p
            .as_i64()
            .or_else(|| p.as_f64().map(|f| f as i64))
            .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
        {
            out.insert("priority".into(), json!(i));
        }
    }
    if let Some(c) = v.get("critical") {
        out.insert("critical".into(), json!(truthy(c)));
    }
    // The affinity as written (a string from the scanner) or as JSON; both
    // read through the same parser, so a bitmask and a list compare equal.
    let raw = match v.get("affinity") {
        None | Some(Value::Null) => None,
        Some(Value::String(s))
            if s.trim_start().starts_with(['[', '"']) || s.trim().parse::<u64>().is_ok() =>
        {
            Some(s.clone())
        }
        Some(other) => Some(other.to_string()),
    };
    match raw.as_deref().map(parse_affinity) {
        None | Some(Some(Affinity::All)) => {}
        Some(Some(Affinity::Cpus(cpus))) => {
            out.insert("affinity".into(), json!(cpus));
        }
        Some(None) => {
            out.insert("affinity".into(), json!(raw));
        }
    }
    Value::Object(out)
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A role as written, as JSON for [`normalize`].
pub fn role_text_json(r: &RoleText) -> Value {
    let mut m = Map::new();
    for (k, v) in [("capacity", &r.capacity), ("period", &r.period), ("deadline", &r.deadline)] {
        if let Some(s) = v {
            m.insert(k.into(), json!(s));
        }
    }
    if let Some(p) = r.priority {
        m.insert("priority".into(), json!(p));
    }
    if r.critical {
        m.insert("critical".into(), json!(true));
    }
    if let Some(a) = &r.affinity {
        m.insert("affinity".into(), json!(a));
    }
    Value::Object(m)
}

const STOP: &[&str] = &[
    "fuchsia",
    "devices",
    "drivers",
    "driver",
    "host",
    "thread",
    "dispatcher",
    "fdf",
    "cm",
    "initial",
    "main",
    "com",
    "google",
    "android",
    "the",
    "a",
];

fn tokens(s: &str) -> BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|t| !t.is_empty() && !STOP.contains(t) && t.chars().count() > 1)
        .map(str::to_string)
        .collect()
}

/// One ranked role candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub role: String,
    pub score: f64,
    pub why: String,
}

/// A duration in microseconds as a trace shows it: Zircon writes deadline
/// parameters in 10 us units, truncated and clamped to 1..=9999 units
/// (`get_weight_or_packed_deadline_params` in the kernel's `scheduler.rs`).
pub(crate) fn as_traced_us(us: i64) -> i64 {
    (us.div_euclid(10)).clamp(1, 9_999) * 10
}

/// Ranks the roles a thread may request, best first.
pub fn suggest(
    label: &str,
    params: Option<(i64, i64)>,
    roles: &IndexMap<String, RoleParams>,
    top: usize,
) -> Vec<Suggestion> {
    let lt = tokens(label);
    let is_driver_host = label.starts_with("driver-host-");
    let mut out = Vec::new();
    // Weights chosen so a parameter match outweighs up to two shared name
    // tokens, and a fair match outweighs a name-only match. The tests pin this
    // ordering.
    for (role, d) in roles {
        let mut score = 0.0;
        let mut why: Vec<String> = Vec::new();
        // The kernel schedules and traces the relative deadline: RoleManager
        // takes it from "deadline", or from "period" when there is none.
        let role_deadline =
            d.capacity.zip(d.deadline.or(d.period)).filter(|&(c, p)| c != 0 && p != 0);
        match (params, role_deadline) {
            (Some((cap, per)), Some((role_cap, role_per))) => {
                let (cap_us, per_us) = (cap.div_euclid(NS_PER_US), per.div_euclid(NS_PER_US));
                if as_traced_us(role_cap) == cap_us && as_traced_us(role_per) == per_us {
                    score += 2.5;
                    why.push("parameters match the profile in effect".into());
                } else {
                    score -= 0.5;
                }
            }
            (None, None) if d.priority.is_some() => {
                score += 1.5;
                why.push("fair role in the profiles file matches fair thread in trace".into());
            }
            (None, Some(_)) => score -= 0.5,
            _ => {}
        }
        let rt = tokens(role);
        let overlap: Vec<&String> = lt.intersection(&rt).collect();
        if !overlap.is_empty() {
            score += 1.0 + 0.5 * overlap.len() as f64;
            why.push(format!(
                "name overlap: {}",
                overlap.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")
            ));
            if is_driver_host && (role.contains(".devices.") || role.contains(".drivers.")) {
                score += 1.0;
                why.push("driver-host thread -> device role".into());
            }
        }
        if score > 0.0 {
            out.push(Suggestion { role: role.clone(), score, why: why.join("; ") });
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(top);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"{
  // comment with "fake.role": { "priority": 1 }
  "profiles": {
    "fuchsia.devices.input.touch": { "capacity": "20us", "period": "850us", "affinity": [3, 2] },
    "fuchsia.graphics.display": {
      "priority": 24,
      "critical": true,
      "output_parameters": { "capacity": "99ms", "scope": "board" }
    },
  },
  "other": { "x.y": { "priority": 2 } }
}"#;

    #[test]
    fn parses_only_the_profiles_object() {
        let r = parse_roles(FILE);
        assert_eq!(
            r.keys().collect::<Vec<_>>(),
            vec!["fuchsia.devices.input.touch", "fuchsia.graphics.display"]
        );
        assert_eq!(r["fuchsia.devices.input.touch"].capacity, Some(20));
        assert_eq!(r["fuchsia.graphics.display"].priority, Some(24));
        assert_eq!(r["fuchsia.graphics.display"].capacity, None);
        let full = parse_roles_full(FILE);
        assert!(full["fuchsia.graphics.display"].critical);
    }

    #[test]
    fn normalizes_for_comparison() {
        let full = parse_roles_full(FILE);
        let local = normalize(&role_text_json(&full["fuchsia.devices.input.touch"]));
        let device =
            normalize(&json!({"capacity": "20us", "period": "0.85ms", "affinity": [2, 3]}));
        assert_eq!(local, device);
    }

    #[test]
    fn a_constrained_deadline_matches_its_traced_deadline() {
        let roles = parse_roles(
            r#"{ "profiles": { "c": { "capacity": "6ms", "deadline": "15ms", "period": "20ms" } } }"#,
        );
        let s = suggest("p/t[1]", Some((6_000_000, 15_000_000)), &roles, 3);
        assert!(s[0].why.contains("parameters match"), "{:?}", s[0].why);
    }

    #[test]
    fn suggests_matching_roles_first() {
        let r = parse_roles(FILE);
        let s = suggest("driver-host-#input/touch-irq[123]", Some((20_000, 850_000)), &r, 5);
        assert_eq!(s[0].role, "fuchsia.devices.input.touch");
        assert!(s[0].why.contains("parameters match"));
        assert!(s[0].why.contains("name overlap: input,touch"));
        assert_eq!(s[0].score, 2.5 + 1.0 + 0.5 * 2.0 + 1.0);
    }

    #[test]
    fn a_parameter_match_outweighs_two_shared_name_tokens() {
        let roles = parse_roles(
            r#"{ "profiles": {
              "x.match": { "capacity": "20us", "period": "850us" },
              "touch.input.fair": { "priority": 20 },
              "touch.input.other": { "capacity": "50us", "period": "1ms" }
            } }"#,
        );
        let s = suggest("input/touch[1]", Some((20_000, 850_000)), &roles, 5);
        let order: Vec<&str> = s.iter().map(|x| x.role.as_str()).collect();
        assert_eq!(order, vec!["x.match", "touch.input.fair", "touch.input.other"], "{s:?}");
    }

    #[test]
    fn a_fair_match_outweighs_a_name_only_match() {
        let roles = parse_roles(
            r#"{ "profiles": {
              "x.fair": { "priority": 20 },
              "touch.deadline": { "capacity": "20us", "period": "850us" }
            } }"#,
        );
        let s = suggest("input/touch[1]", None, &roles, 5);
        let order: Vec<&str> = s.iter().map(|x| x.role.as_str()).collect();
        assert_eq!(order, vec!["x.fair", "touch.deadline"], "{s:?}");
    }

    #[test]
    fn traced_params_match_roles_truncated_to_10us() {
        let roles = parse_roles(
            r#"{ "profiles": { "a.ui": { "capacity": "1005us", "deadline": "16666us" } } }"#,
        );
        let s = suggest("p/ui[1]", Some((1_000_000, 16_660_000)), &roles, 5);
        assert!(s[0].why.contains("parameters match"), "{s:?}");
    }
}
