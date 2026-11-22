// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Edits role profiles in a `.profiles` (JSON5-like) file.
//!
//! Edits are textual so comments and formatting in the file are preserved; new
//! roles are appended before the closing brace of the `"profiles"` object.

use anyhow::bail;

/// A role's settings as written in the file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleText {
    pub capacity: Option<String>,
    pub period: Option<String>,
    pub deadline: Option<String>,
    /// The affinity as written: `[2, 3]`, `"..."` or a number.
    pub affinity: Option<String>,
    pub priority: Option<i64>,
    pub critical: bool,
}

/// One role entry scanned from the top-level `"profiles": { ... }` object.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleEntry<'a> {
    pub name: String,
    pub line_start: usize,
    pub indent: &'a str,
    pub body: &'a str,
    pub close_brace: usize,
    pub has_comma: bool,
    pub line_end: usize,
    pub trailing_comment: &'a str,
}

fn skip_ws_and_comments(b: &[u8], mut i: usize, end: usize) -> usize {
    let end = end.min(b.len());
    while i < end {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if i + 1 < end && b[i] == b'/' && b[i + 1] == b'/' {
            i += 2;
            while i < end && b[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < end && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < end && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(end);
        } else {
            break;
        }
    }
    i
}

fn scan_string(text: &str, open_quote: usize, end: usize) -> Option<(String, usize)> {
    let b = text.as_bytes();
    if open_quote >= end || b.get(open_quote) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut i = open_quote + 1;
    let mut seg = i;
    while i < end {
        if b[i] == b'\\' && i + 1 < end {
            out.push_str(&text[seg..i]);
            // The escaped character may take several bytes.
            let ch = text[i + 1..].chars().next()?;
            out.push(ch);
            i += 1 + ch.len_utf8();
            seg = i;
            continue;
        }
        if b[i] == b'"' {
            out.push_str(&text[seg..i]);
            return Some((out, i + 1));
        }
        i += 1;
    }
    None
}

fn find_matching_delim(
    b: &[u8],
    open: usize,
    end: usize,
    open_ch: u8,
    close_ch: u8,
) -> Option<usize> {
    let end = end.min(b.len());
    if open >= end || b[open] != open_ch {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < end {
        if b[i] == b'"' {
            i += 1;
            while i < end {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        let next = skip_ws_and_comments(b, i, end);
        if next != i {
            i = next;
            continue;
        }
        if b[i] == open_ch {
            depth += 1;
        } else if b[i] == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Returns `(inner_start, close_brace)` of the top-level `"profiles": { ... }` object.
pub fn profiles_span(text: &str) -> Option<(usize, usize)> {
    let (val, val_end) = top_level_value(text, "profiles")?;
    (text.as_bytes()[val] == b'{').then_some((val + 1, val_end - 1))
}

/// The top-level `"scope"` of a `.profiles` file, read like the rest of the
/// file: comments and nested objects that mention "scope" do not count.
pub fn file_scope(text: &str) -> Option<String> {
    let (val, val_end) = top_level_value(text, "scope")?;
    scan_string(text, val, val_end).map(|(s, _)| s)
}

/// The byte range of the value of the root object's `key`.
fn top_level_value(text: &str, want: &str) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let root_open = skip_ws_and_comments(b, 0, b.len());
    let root_close = find_matching_delim(b, root_open, b.len(), b'{', b'}')?;
    object_fields(text, root_open + 1, root_close)
        .into_iter()
        .find(|(key, _, _)| key == want)
        .map(|(_, val, val_end)| (val, val_end))
}

/// `(key, value_start, value_end)` of the `"key": value` entries in
/// `text[i..end]`, the inside of a `{ ... }`, skipping comments.
fn object_fields(text: &str, mut i: usize, end: usize) -> Vec<(String, usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    while i < end {
        i = skip_ws_and_comments(b, i, end);
        if i >= end {
            break;
        }
        if b[i] == b',' {
            i += 1;
            continue;
        }
        let Some((key, after_key)) = scan_string(text, i, end) else {
            i += 1;
            continue;
        };
        let colon = skip_ws_and_comments(b, after_key, end);
        if colon >= end || b[colon] != b':' {
            i = after_key;
            continue;
        }
        let val = skip_ws_and_comments(b, colon + 1, end);
        if val >= end {
            break;
        }
        let val_end = skip_value(text, val, end);
        out.push((key, val, val_end));
        i = val_end;
    }
    out
}

fn skip_value(text: &str, val: usize, end: usize) -> usize {
    let b = text.as_bytes();
    if val >= end {
        return end;
    }
    match b[val] {
        b'{' => find_matching_delim(b, val, end, b'{', b'}').map_or(end, |c| c + 1),
        b'[' => find_matching_delim(b, val, end, b'[', b']').map_or(end, |c| c + 1),
        b'"' => scan_string(text, val, end).map_or(end, |(_, next)| next),
        _ => {
            let mut j = val;
            while j < end
                && !b[j].is_ascii_whitespace()
                && !matches!(b[j], b',' | b'}' | b']')
                && !(j + 1 < end && b[j] == b'/' && matches!(b[j + 1], b'/' | b'*'))
            {
                j += 1;
            }
            j
        }
    }
}

/// Every role entry inside `"profiles": { ... }`, in file order.
pub fn role_entries(text: &str) -> Vec<RoleEntry<'_>> {
    let Some((inner_start, profiles_close)) = profiles_span(text) else {
        return Vec::new();
    };
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = inner_start;
    while i < profiles_close {
        i = skip_ws_and_comments(b, i, profiles_close);
        if i >= profiles_close {
            break;
        }
        if b[i] == b',' {
            i += 1;
            continue;
        }
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        let mut ls = i;
        while ls > inner_start && matches!(b[ls - 1], b' ' | b'\t') {
            ls -= 1;
        }
        let (line_start, indent) =
            if ls == 0 || b[ls - 1] == b'\n' { (ls, &text[ls..i]) } else { (i, "") };
        let Some((name, after_key)) = scan_string(text, i, profiles_close) else { break };
        let colon = skip_ws_and_comments(b, after_key, profiles_close);
        if colon >= profiles_close || b[colon] != b':' {
            i = after_key;
            continue;
        }
        let ob = skip_ws_and_comments(b, colon + 1, profiles_close);
        if ob >= profiles_close || b[ob] != b'{' {
            i = skip_value(text, ob, profiles_close);
            continue;
        }
        let Some(cb) = find_matching_delim(b, ob, profiles_close, b'{', b'}') else { break };
        // Spaces and `/* */` comments may sit between the entry and its comma.
        let mut p = cb + 1;
        loop {
            while p < profiles_close && matches!(b[p], b' ' | b'\t') {
                p += 1;
            }
            if p + 1 < profiles_close && b[p] == b'/' && b[p + 1] == b'*' {
                p = text[p + 2..profiles_close]
                    .find("*/")
                    .map_or(profiles_close, |k| p + 2 + k + 2);
                continue;
            }
            break;
        }
        let has_comma = p < profiles_close && b[p] == b',';
        if has_comma {
            p += 1;
        }
        let mut q = p;
        while q < profiles_close && matches!(b[q], b' ' | b'\t') {
            q += 1;
        }
        let (trailing_comment, line_end) =
            if q + 1 < profiles_close && b[q] == b'/' && b[q + 1] == b'/' {
                let mut r = q + 2;
                while r < profiles_close && b[r] != b'\n' {
                    r += 1;
                }
                (&text[q..r], r)
            } else {
                ("", p)
            };
        out.push(RoleEntry {
            name,
            line_start,
            indent,
            body: &text[ob + 1..cb],
            close_brace: cb,
            has_comma,
            line_end,
            trailing_comment,
        });
        i = line_end.max(cb + 1);
    }
    out
}

/// Top-level `(key, raw_value_slice)` fields inside a `{ ... }` body.
fn body_fields(body: &str) -> Vec<(String, &str)> {
    object_fields(body, 0, body.len())
        .into_iter()
        .map(|(k, val, val_end)| (k, body[val..val_end].trim()))
        .collect()
}

/// Parses the settings inside a role's `{ ... }`.
pub fn parse_role_body(body: &str) -> RoleText {
    let mut out = RoleText::default();
    for (k, raw) in body_fields(body) {
        let unquoted = || scan_string(raw, 0, raw.len()).map(|(s, _)| s);
        match k.as_str() {
            "capacity" => out.capacity = unquoted(),
            "period" => out.period = unquoted(),
            "deadline" => out.deadline = unquoted(),
            "affinity" => out.affinity = Some(raw.to_string()),
            "priority" => out.priority = raw.parse().ok(),
            "critical" => out.critical = raw == "true",
            _ => {}
        }
    }
    out
}

/// A role's settings, or None if the file does not have it.
pub fn read_role(text: &str, role: &str) -> Option<RoleText> {
    role_entries(text).into_iter().find(|e| e.name == role).map(|e| parse_role_body(e.body))
}

/// Sets a role's capacity, period and affinity. An empty `affinity` keeps an
/// existing role's affinity as written and leaves a new role without one.
/// Returns (new text, "updated" or "added").
pub fn set_role(
    text: &str,
    role: &str,
    capacity_us: i64,
    period_us: i64,
    affinity: &[i64],
    comment: &str,
) -> anyhow::Result<(String, &'static str)> {
    // An empty affinity is left out: RoleManager reads `"affinity": []` as a
    // mask with no CPUs, not as "any CPU".
    let aff = if affinity.is_empty() {
        String::new()
    } else {
        let cpus = affinity.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ");
        format!(", \"affinity\": [{cpus}]")
    };
    // The comment goes on a `//` line, so it must stay on one line.
    let comment = comment.replace(['\r', '\n'], " ");
    let comment = comment.trim();
    let entries = role_entries(text);
    if let Some(e) = entries.iter().find(|e| e.name == role) {
        let extras: Vec<String> = body_fields(e.body)
            .into_iter()
            .filter(|(k, _)| {
                !matches!(k.as_str(), "capacity" | "period" | "deadline" | "priority")
                    && (k != "affinity" || affinity.is_empty())
            })
            .map(|(k, v)| format!("\"{k}\": {v}"))
            .collect();
        let extra =
            if extras.is_empty() { String::new() } else { format!(", {}", extras.join(", ")) };
        let body = format!(
            "\"{role}\": {{ \"capacity\": \"{capacity_us}us\", \"period\": \"{period_us}us\"{aff}{extra} }},"
        );
        let trailing = if e.trailing_comment.is_empty() {
            String::new()
        } else {
            format!("  {}", e.trailing_comment)
        };
        return Ok((
            format!("{}{}{body}{trailing}{}", &text[..e.line_start], e.indent, &text[e.line_end..]),
            "updated",
        ));
    }
    let Some((_, close_brace)) = profiles_span(text) else {
        bail!("no \"profiles\" object found");
    };
    let bytes = text.as_bytes();
    let insert = format!(
        "{}    \"{role}\": {{ \"capacity\": \"{capacity_us}us\", \"period\": \"{period_us}us\"{aff} }},\n  ",
        if comment.is_empty() { "\n".to_string() } else { format!("\n    // {comment}\n") }
    );
    // Strip whitespace before the closing brace so blank lines don't pile up,
    // and ensure the previous entry has a comma right after its `}` (before any
    // trailing inline comment).
    let mut j = close_brace.min(bytes.len());
    while j > 0 && matches!(bytes[j - 1], b' ' | b'\t' | b'\n' | b'\r') {
        j -= 1;
    }
    let prefix = match entries.last() {
        Some(last) if !last.has_comma && last.close_brace < j => {
            let comma_at = last.close_brace + 1;
            format!("{},{}", &text[..comma_at], &text[comma_at..j])
        }
        _ => text[..j].to_string(),
    };
    Ok((format!("{prefix}{insert}{}", &text[close_brace.min(text.len())..]), "added"))
}

/// One requested profile change: a role and the parameters a thread needs.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleChange {
    pub role: String,
    pub capacity_us: i64,
    pub period_us: i64,
    /// Empty means: keep an existing role's affinity, and give a new role the
    /// default (see [`apply_role_changes`]).
    pub affinity: Vec<i64>,
    /// The thread the change was derived for ("" when typed by hand).
    pub label: String,
}

impl RoleChange {
    /// Reads `{"role", "capacity_us", "period_us", "affinity", "label"}`.
    pub fn from_json(v: &serde_json::Value) -> RoleChange {
        let str_of = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
        RoleChange {
            role: str_of("role").trim().to_string(),
            capacity_us: v.get("capacity_us").and_then(serde_json::Value::as_i64).unwrap_or(0),
            period_us: v.get("period_us").and_then(serde_json::Value::as_i64).unwrap_or(0),
            affinity: v
                .get("affinity")
                .and_then(serde_json::Value::as_array)
                .map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect())
                .unwrap_or_default(),
            label: str_of("label").to_string(),
        }
    }
}

/// The changes for one role after merging every thread that shares it.
#[derive(Clone, Debug, PartialEq)]
pub struct MergedRole {
    pub role: String,
    pub capacity_us: i64,
    pub period_us: i64,
    /// The union of the explicit affinities; empty when none was given.
    pub affinity: Vec<i64>,
    /// Whether one of the merged changes had no affinity, which asks to keep
    /// the role's current CPUs (or the default for a new role).
    pub keep_existing_affinity: bool,
    pub labels: Vec<String>,
}

/// Merges changes that target the same role: the largest capacity, the
/// shortest period and the union of the affinities, so every thread sharing
/// the role fits. Fails on a change without a role or with a non-positive
/// capacity or period, and when the merged capacity exceeds the merged period
/// (Zircon rejects that).
pub fn merge_role_changes(changes: &[RoleChange]) -> anyhow::Result<Vec<MergedRole>> {
    let mut merged: Vec<MergedRole> = Vec::new();
    for c in changes {
        let (role, cap, per) = (c.role.trim(), c.capacity_us, c.period_us);
        if role.is_empty() || cap <= 0 || per <= 0 {
            bail!(
                "invalid profile change for role '{role}': capacity ({cap}us) and period ({per}us) must be positive"
            );
        }
        // The first change for a role and later ones are handled the same
        // way: CPUs sorted and deduplicated, empty labels left out.
        let i = match merged.iter().position(|m| m.role == role) {
            Some(i) => i,
            None => {
                merged.push(MergedRole {
                    role: role.to_string(),
                    capacity_us: cap,
                    period_us: per,
                    affinity: Vec::new(),
                    keep_existing_affinity: false,
                    labels: Vec::new(),
                });
                merged.len() - 1
            }
        };
        let m = &mut merged[i];
        m.capacity_us = m.capacity_us.max(cap);
        m.period_us = m.period_us.min(per);
        m.affinity.extend_from_slice(&c.affinity);
        m.affinity.sort_unstable();
        m.affinity.dedup();
        m.keep_existing_affinity |= c.affinity.is_empty();
        if !c.label.is_empty() {
            m.labels.push(c.label.clone());
        }
    }
    for m in &merged {
        if m.capacity_us > m.period_us {
            bail!(
                "threads {} share role '{}' but need {}us of capacity in a {}us period; \
                 give them separate roles",
                m.labels.join(", "),
                m.role,
                m.capacity_us,
                m.period_us
            );
        }
    }
    Ok(merged)
}

/// Applies `changes` to a `.profiles` file's text (see [`merge_role_changes`]).
///
/// A role without an explicit affinity keeps its current one when the file
/// already has it (whoever wrote it chose those CPUs), and a new role gets
/// `default_affinity`. Each role's comment is `comment` when given, else "zpo:
/// derived for" the threads it was derived for. Returns the new text and one
/// line per role.
pub fn apply_role_changes_to_text(
    text: &str,
    changes: &[RoleChange],
    default_affinity: &[i64],
    comment: Option<&str>,
) -> anyhow::Result<(String, Vec<String>)> {
    let mut text = text.to_string();
    let mut log = Vec::new();
    for m in merge_role_changes(changes)? {
        let lbl = m.labels.join(", ");
        let derived =
            if lbl.is_empty() { String::new() } else { format!("zpo: derived for {lbl}") };
        // A change without CPUs keeps the role's current ones (the default
        // for a new role); merged with changes that name CPUs, the role gets
        // the union, and an unrestricted role stays unrestricted. An empty
        // list leaves the role's CPUs as written.
        let aff = if !m.keep_existing_affinity {
            m.affinity.clone()
        } else {
            let base = match read_role(&text, &m.role) {
                None => Some(default_affinity.to_vec()),
                Some(r) => match r.affinity.as_deref().map(parse_affinity) {
                    None | Some(Some(Affinity::All)) => None,
                    // Only "keep" changes for an existing role.
                    Some(Some(_)) if m.affinity.is_empty() => None,
                    Some(Some(Affinity::Cpus(cpus))) => Some(cpus),
                    Some(None) => bail!("role '{}' has an affinity zpo cannot read", m.role),
                },
            };
            match base {
                None => Vec::new(),
                Some(mut cpus) => {
                    cpus.extend(&m.affinity);
                    cpus.sort_unstable();
                    cpus.dedup();
                    cpus
                }
            }
        };
        let (t, act) = set_role(
            &text,
            &m.role,
            m.capacity_us,
            m.period_us,
            &aff,
            comment.unwrap_or(&derived),
        )?;
        text = t;
        let aff_text = if aff.is_empty() { "kept".to_string() } else { format!("{aff:?}") };
        log.push(format!(
            "{act} {} -> {}us/{}us affinity {aff_text}",
            m.role, m.capacity_us, m.period_us
        ));
    }
    Ok((text, log))
}

/// [`apply_role_changes_to_text`] on the roles file at `path`: (current
/// text, new text, one line per role). Nothing is written.
pub fn apply_role_changes(
    path: &std::path::Path,
    changes: &[RoleChange],
    default_affinity: &[i64],
    comment: Option<&str>,
) -> anyhow::Result<(String, String, Vec<String>)> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", path.display()))?;
    let (new, log) = apply_role_changes_to_text(&text, changes, default_affinity, comment)?;
    Ok((text, new, log))
}

/// Duration suffixes and their length in nanoseconds; the order matters
/// ("ms" before "s"). The UI validates through the server, which uses this.
/// Both the micro sign and the Greek mu are accepted for "µs".
/// Nanoseconds per microsecond: profiles files and reports use microseconds,
/// traces nanoseconds.
pub const NS_PER_US: i64 = 1_000;
/// [`NS_PER_US`] for converting measured (float) durations.
pub const NS_PER_US_F64: f64 = NS_PER_US as f64;
/// Nanoseconds per millisecond, for durations shown in milliseconds.
pub const NS_PER_MS_F64: f64 = 1e6;

const DURATION_UNITS: [(&str, f64); 6] =
    [("ms", 1e6), ("us", 1e3), ("\u{b5}s", 1e3), ("\u{3bc}s", 1e3), ("ns", 1.0), ("s", 1e9)];

/// Parses "100us", "850µs", "1.5ms", "1s", "500ns" or a bare number of
/// microseconds, into microseconds.
pub fn parse_us(s: &str) -> anyhow::Result<i64> {
    parse_dur_ns(s)
        .map(|ns| ns.div_euclid(NS_PER_US))
        .ok_or_else(|| anyhow::anyhow!("not a duration: {s:?}"))
}

/// Parses a duration in nanoseconds: "850us", "850µs", "16.6ms", "1s",
/// "500ns"; a bare number is microseconds.
pub fn parse_dur_ns(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let (num, mul) = DURATION_UNITS
        .iter()
        .find_map(|(suf, mul)| s.strip_suffix(suf).map(|v| (v.trim(), *mul)))
        .unwrap_or((s.as_str(), 1_000.0));
    // "nan", "inf" and negative numbers parse as f64 but are not durations,
    // and values past i64 nanoseconds would wrap.
    let ns = num.parse::<f64>().ok().filter(|f| f.is_finite() && *f >= 0.0)? * mul;
    (ns < i64::MAX as f64).then_some(ns as i64)
}

/// A role's CPUs, as [`parse_affinity`] reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Affinity {
    /// Every CPU (`"all"`).
    All,
    /// These CPUs, sorted and without repeats.
    Cpus(Vec<i64>),
}

/// A role's `"affinity"` as written, as RoleManager reads it: `"all"` (or
/// no affinity) is every CPU, a number is a CPU bitmask, and a list may
/// carry comments and a trailing comma. A list comes back sorted and without
/// repeats, so `[3, 2]`, `[2, 3]` and the bitmask `12` compare equal. None
/// is an affinity zpo cannot read.
pub fn parse_affinity(raw: &str) -> Option<Affinity> {
    let raw = raw.trim();
    if raw == "\"all\"" {
        return Some(Affinity::All);
    }
    if let Ok(mask) = raw.parse::<u64>() {
        return Some(Affinity::Cpus((0..64).filter(|i| mask & (1u64 << i) != 0).collect()));
    }
    let inner = raw.strip_prefix('[')?.strip_suffix(']')?;
    let b = inner.as_bytes();
    let mut cpus = Vec::new();
    let mut i = 0;
    while i < b.len() {
        i = skip_ws_and_comments(b, i, b.len());
        if i >= b.len() {
            break;
        }
        if b[i] == b',' {
            i += 1;
            continue;
        }
        let j = skip_value(inner, i, b.len());
        cpus.push(inner[i..j].parse::<i64>().ok().filter(|c| *c >= 0)?);
        i = j.max(i + 1);
    }
    cpus.sort_unstable();
    cpus.dedup();
    Some(Affinity::Cpus(cpus))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"{
  "profiles": {
    // Touch input.
    "fuchsia.devices.input.touch": { "capacity": "20us", "period": "850us", "affinity": [2, 3] },  // keep
    "fuchsia.fair": { "priority": 20 }
  }
}
"#;

    fn change(role: &str, cap: i64, per: i64, aff: &[i64], label: &str) -> RoleChange {
        RoleChange {
            role: role.into(),
            capacity_us: cap,
            period_us: per,
            affinity: aff.to_vec(),
            label: label.into(),
        }
    }

    #[test]
    fn merges_role_changes_with_positive_periods() {
        let m = merge_role_changes(&[
            change("r.a", 100, 1000, &[2, 3], "t1"),
            change("r.a", 150, 850, &[4], "t2"),
        ])
        .unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].capacity_us, m[0].period_us), (150, 850));
        assert_eq!(m[0].affinity, vec![2, 3, 4]);
        assert_eq!(m[0].labels, vec!["t1", "t2"]);
        assert!(merge_role_changes(&[change("r.a", 100, 0, &[], "")]).is_err());
        assert!(merge_role_changes(&[change("", 100, 850, &[], "")]).is_err());
        let err = merge_role_changes(&[
            change("r.a", 600, 1000, &[], "t1"),
            change("r.a", 100, 500, &[], "t2"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("give them separate roles"), "{err}");
    }

    #[test]
    fn new_roles_take_the_default_and_existing_roles_keep_their_affinity() {
        let new_role = [change("x.new", 30, 900, &[], "p/t[1]")];
        let (new, log) = apply_role_changes_to_text(FILE, &new_role, &[2, 3], None).unwrap();
        assert!(
            new.contains(
                r#""x.new": { "capacity": "30us", "period": "900us", "affinity": [2, 3] }"#
            ),
            "{new}"
        );
        assert!(new.contains("zpo: derived for p/t[1]"), "{new}");
        assert_eq!(log, vec!["added x.new -> 30us/900us affinity [2, 3]"]);
        let touch = [change("fuchsia.devices.input.touch", 40, 850, &[], "")];
        let (new, log) = apply_role_changes_to_text(FILE, &touch, &[7], Some("by hand")).unwrap();
        assert!(
            new.contains(
                r#""fuchsia.devices.input.touch": { "capacity": "40us", "period": "850us", "affinity": [2, 3] }"#
            ),
            "{new}"
        );
        assert_eq!(log, vec!["updated fuchsia.devices.input.touch -> 40us/850us affinity kept"]);
        // An explicit affinity replaces the existing one.
        let (new, _) = apply_role_changes_to_text(
            FILE,
            &[change("fuchsia.devices.input.touch", 40, 850, &[7], "")],
            &[],
            None,
        )
        .unwrap();
        assert!(new.contains(r#""period": "850us", "affinity": [7] }"#), "{new}");
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.profiles");
        std::fs::write(&p, FILE).unwrap();
        let (old, new, _) = apply_role_changes(&p, &new_role, &[2], None).unwrap();
        assert_eq!(old, FILE);
        assert!(new.contains("x.new"));
        assert!(apply_role_changes(&d.path().join("missing"), &new_role, &[2], None).is_err());
    }

    #[test]
    fn updates_in_place_keeping_comments() {
        let (new, act) =
            set_role(FILE, "fuchsia.devices.input.touch", 100, 850, &[2, 3, 4], "").unwrap();
        assert_eq!(act, "updated");
        assert!(new.contains(
            r#"    "fuchsia.devices.input.touch": { "capacity": "100us", "period": "850us", "affinity": [2, 3, 4] },  // keep"#
        ));
        assert!(new.contains("// Touch input."));
        let r = read_role(&new, "fuchsia.devices.input.touch").unwrap();
        assert_eq!(r.capacity.as_deref(), Some("100us"));
        assert_eq!(r.affinity.as_deref(), Some("[2, 3, 4]"));
    }

    #[test]
    fn appends_new_roles() {
        let (new, act) =
            set_role(FILE, "new.role", 50, 1000, &[7], "zpo: derived for p/t[1]").unwrap();
        assert_eq!(act, "added");
        assert!(new.contains(
            "\"fuchsia.fair\": { \"priority\": 20 },\n    // zpo: derived for p/t[1]\n    \"new.role\": { \"capacity\": \"50us\", \"period\": \"1000us\", \"affinity\": [7] },\n  }"
        ), "{new}");
        assert!(set_role("{}", "x", 1, 2, &[], "").is_err());
    }

    #[test]
    fn handles_nested_objects_and_inline_comments_on_last_entry() {
        let complex = r#"{
  "profiles": {
    "fuchsia.ktrace.reader": {
      "capacity": "2300us",
      "period": "8ms",
      "critical": true,
      "output_parameters": { "max_readout_time": 2300000, "poll_period": 10000000 }
    },
    "fuchsia.last": { "priority": 20 }  // trailing comment without comma
  }
}
"#;
        let r = read_role(complex, "fuchsia.ktrace.reader").unwrap();
        assert_eq!(r.capacity.as_deref(), Some("2300us"));
        assert!(r.critical);
        let (updated, act) =
            set_role(complex, "fuchsia.ktrace.reader", 3000, 8000, &[2, 3], "").unwrap();
        assert_eq!(act, "updated");
        assert!(
            updated.contains(
                r#""output_parameters": { "max_readout_time": 2300000, "poll_period": 10000000 }"#
            ),
            "{updated}"
        );
        assert!(updated.contains(r#""critical": true"#), "{updated}");

        let (added, act) = set_role(complex, "fuchsia.new", 100, 1000, &[2], "").unwrap();
        assert_eq!(act, "added");
        assert!(
            added.contains(
                r#""fuchsia.last": { "priority": 20 },  // trailing comment without comma"#
            ),
            "{added}"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_us("1.5ms").unwrap(), 1500);
        assert_eq!(parse_us(" 20us").unwrap(), 20);
        assert_eq!(parse_us("2500ns").unwrap(), 2);
        assert_eq!(parse_us("7").unwrap(), 7);
        assert!(parse_us("abc").is_err());
        assert_eq!(parse_dur_ns("850us"), Some(850_000));
        assert_eq!(parse_dur_ns("16.6ms"), Some(16_600_000));
        assert_eq!(parse_dur_ns("5"), Some(5_000));
        assert_eq!(parse_dur_ns(""), None);
        assert_eq!(parse_dur_ns("1s"), Some(1_000_000_000));
        assert_eq!(parse_dur_ns("850\u{b5}s"), Some(850_000));
        assert_eq!(parse_dur_ns("850\u{3bc}s"), Some(850_000));
        assert_eq!(parse_us("2s").unwrap(), 2_000_000);
        for bad in ["nan", "inf", "-5ms", "-1", "1e300s"] {
            assert_eq!(parse_dur_ns(bad), None, "{bad}");
        }
    }

    #[test]
    fn affinities_read_like_role_manager() {
        assert_eq!(parse_affinity("\"all\""), Some(Affinity::All));
        assert_eq!(parse_affinity("6"), Some(Affinity::Cpus(vec![1, 2])));
        assert_eq!(parse_affinity("[2, 3]"), Some(Affinity::Cpus(vec![2, 3])));
        assert_eq!(
            parse_affinity("[\n  2, // mid\n  3, /* x */\n]"),
            Some(Affinity::Cpus(vec![2, 3]))
        );
        // Lists are canonical, like bitmasks.
        assert_eq!(parse_affinity("[3, 2, 2]"), parse_affinity("12"));
        assert_eq!(parse_affinity("\"some\""), None);
        assert_eq!(parse_affinity("[1, x]"), None);
    }

    #[test]
    fn reads_the_top_level_scope_only() {
        let f = r#"{
  // "scope": "product"
  "profiles": { "a": { "output_parameters": { "scope": "board" } } },
  "scope": "core",
}"#;
        assert_eq!(file_scope(f).as_deref(), Some("core"));
        assert_eq!(file_scope(r#"{ "profiles": { "a": { "scope": "board" } } }"#), None);
    }

    #[test]
    fn scans_escapes_and_block_comments() {
        let f = "{ \"profiles\": { \"a\\\u{e9}b\": { \"priority\": 1 } /* x */, \"c\": { \"priority\": 2 } } }";
        let names: Vec<String> = role_entries(f).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a\u{e9}b", "c"]);
        assert!(role_entries(f)[0].has_comma);
    }

    #[test]
    fn keeping_an_affinity_unions_it_with_the_named_cpus() {
        // One thread keeps the role's CPUs, another needs CPU 4: the role gets
        // both instead of only [4].
        let both = [
            change("fuchsia.devices.input.touch", 40, 850, &[], "a"),
            change("fuchsia.devices.input.touch", 30, 850, &[4], "b"),
        ];
        let (new, _) = apply_role_changes_to_text(FILE, &both, &[7], None).unwrap();
        assert!(new.contains(r#""period": "850us", "affinity": [2, 3, 4] }"#), "{new}");
        // A new role: the default joins the named CPUs.
        let fresh = [change("x.new", 40, 850, &[], "a"), change("x.new", 30, 850, &[4], "b")];
        let (new, _) = apply_role_changes_to_text(FILE, &fresh, &[2, 3], None).unwrap();
        assert!(
            new.contains(
                r#""x.new": { "capacity": "40us", "period": "850us", "affinity": [2, 3, 4] }"#
            ),
            "{new}"
        );
        // An unrestricted role stays unrestricted.
        let all = "{ \"profiles\": { \"r\": { \"capacity\": \"10us\", \"period\": \"1ms\", \"affinity\": \"all\" } } }";
        let (new, _) = apply_role_changes_to_text(
            all,
            &[change("r", 20, 1000, &[], ""), change("r", 20, 1000, &[4], "")],
            &[2],
            None,
        )
        .unwrap();
        assert!(new.contains(r#""affinity": "all""#), "{new}");
    }

    #[test]
    fn empty_affinity_is_left_out_and_comments_stay_on_one_line() {
        let text = "{\n  \"profiles\": {\n    \"a.role\": { \"capacity\": \"10us\", \"period\": \"1ms\" },\n  },\n}\n";
        let (new, how) = set_role(text, "b.role", 20, 100, &[], "derived\nfor x").unwrap();
        assert_eq!(how, "added");
        assert!(new.contains(r#""b.role": { "capacity": "20us", "period": "100us" },"#), "{new}");
        assert!(new.contains("// derived for x\n"), "{new}");
        let (new, how) = set_role(text, "a.role", 30, 1000, &[], "").unwrap();
        assert_eq!(how, "updated");
        assert!(!new.contains("affinity"), "{new}");
    }
}
