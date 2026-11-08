// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs queries against a warm session with `trace_processor_shell query
//! --remote`.
//!
//! Each query is wrapped so that SQLite returns its rows as JSON arrays, at
//! most [`CHUNK_ROWS`] rows per output cell. Each row carries its position, so
//! the original order survives the chunking.

use super::{Query, Rows};
use crate::model::TraceSource;
use crate::proc::Runner;
use anyhow::{Context, bail};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Rows per JSON cell. `query --remote` caps one RPC message at 64 MiB; the
/// widest zpo query is about 32 bytes per row.
pub const CHUNK_ROWS: usize = 100_000;

const QUERY_TIMEOUT: Duration = Duration::from_secs(600);

/// The SQL that returns `q`'s rows as chunked JSON: one output row per chunk,
/// one cell holding `[[position, col1, col2, ...], ...]`.
pub fn wrap(q: &Query, chunk_rows: usize) -> String {
    let cols: Vec<String> = q.cols.iter().map(|c| format!("zpo_n.\"{c}\"")).collect();
    let over = if q.order.is_empty() {
        "OVER ()".to_string()
    } else {
        format!("OVER (ORDER BY {})", qualify_order(q.order))
    };
    // A trailing `;` would end the statement inside the subquery, and a trailing
    // `--` comment would swallow the closing parenthesis, so drop the first and
    // put the parenthesis on its own line.
    let sql = q.sql.trim_end().trim_end_matches(';');
    format!(
        "SELECT JSON_GROUP_ARRAY(JSON_ARRAY(zpo_rn, {cols})) AS zpo_chunk FROM (\
         SELECT zpo_q.*, ROW_NUMBER() {over} - 1 AS zpo_rn FROM ({sql}\n) AS zpo_q) AS zpo_n \
         GROUP BY zpo_rn / {chunk_rows} ORDER BY zpo_rn / {chunk_rows}",
        cols = cols.join(", "),
    )
}

/// Qualifies the column names of an ORDER BY list ("name, ts DESC").
fn qualify_order(order: &str) -> String {
    order
        .split(',')
        .map(|part| {
            let part = part.trim();
            let (col, dir) = part.split_once(' ').unwrap_or((part, ""));
            let dir = dir.trim();
            if dir.is_empty() {
                format!("zpo_q.\"{col}\"")
            } else {
                format!("zpo_q.\"{col}\" {dir}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses the CSV printed for a [`wrap`]ped query back into rows, in order.
pub fn parse(stdout: &str) -> anyhow::Result<Rows> {
    let mut numbered: Vec<(i64, Vec<Value>)> = Vec::new();
    for line in stdout.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        // The shell quotes the cell but does not escape the quotes inside it.
        let cell = line
            .strip_prefix('"')
            .and_then(|l| l.strip_suffix('"'))
            .with_context(|| format!("unexpected trace processor output: {line:.200}"))?;
        let chunk: Vec<Vec<Value>> =
            serde_json::from_str(cell).context("trace processor returned malformed JSON")?;
        for mut row in chunk {
            if row.is_empty() {
                bail!("trace processor returned an empty row");
            }
            let rn = row.remove(0).as_i64().unwrap_or(0);
            numbered.push((rn, row));
        }
    }
    numbered.sort_by_key(|(rn, _)| *rn);
    Ok(Rows { rows: numbered.into_iter().map(|(_, r)| r).collect() })
}

/// Queries one warm session.
#[derive(Clone)]
pub struct Client {
    runner: Arc<dyn Runner>,
    tp: PathBuf,
    session: String,
    chunk_rows: usize,
}

impl Client {
    pub fn new(runner: Arc<dyn Runner>, tp: PathBuf, session: String) -> Self {
        Client { runner, tp, session, chunk_rows: CHUNK_ROWS }
    }

    /// Uses smaller chunks (tests).
    pub fn with_chunk_rows(mut self, n: usize) -> Self {
        self.chunk_rows = n.max(1);
        self
    }

    /// The session this client queries.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Runs `q` and returns its rows in order.
    pub fn query(&self, q: &Query) -> anyhow::Result<Rows> {
        let args = vec![
            self.tp.display().to_string(),
            "query".into(),
            "--remote".into(),
            self.session.clone(),
            "--no-agent".into(),
            "--quiet".into(),
            "--format".into(),
            "csv".into(),
            wrap(q, self.chunk_rows),
        ];
        let out = self
            .runner
            .run(&args, QUERY_TIMEOUT)
            .with_context(|| format!("could not run {}", self.tp.display()))?;
        if out.timed_out {
            bail!("trace processor query `{}` timed out", q.name);
        }
        if !out.success() {
            let msg = out.combined();
            let msg = msg.trim();
            let tail: String =
                msg.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
            bail!("trace processor query `{}` failed: {}", q.name, tail);
        }
        parse(&out.stdout).with_context(|| format!("in trace processor query `{}`", q.name))
    }
}

impl TraceSource for Client {
    fn query(&self, q: &Query) -> anyhow::Result<Rows> {
        Client::query(self, q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn q(order: &'static str) -> Query {
        Query { name: "t", sql: "SELECT 1 AS a, 'x' AS b".into(), cols: &["a", "b"], order }
    }

    #[test]
    fn wraps_with_position_and_chunks() {
        let w = wrap(&q(""), 100);
        assert!(w.contains("JSON_ARRAY(zpo_rn, zpo_n.\"a\", zpo_n.\"b\")"), "{w}");
        assert!(w.contains("ROW_NUMBER() OVER () - 1"), "{w}");
        assert!(w.contains("GROUP BY zpo_rn / 100 ORDER BY zpo_rn / 100"), "{w}");
        let w = wrap(&q("b DESC, a"), 7);
        assert!(w.contains("OVER (ORDER BY zpo_q.\"b\" DESC, zpo_q.\"a\")"), "{w}");
    }

    #[test]
    fn wrap_survives_a_trailing_semicolon_or_comment() {
        let mut query = q("");
        query.sql = "SELECT 1 AS a, 'x' AS b; \n".into();
        assert!(wrap(&query, 100).contains("FROM (SELECT 1 AS a, 'x' AS b\n) AS zpo_q"));
        query.sql = "SELECT 1 AS a, 'x' AS b -- why".into();
        assert!(wrap(&query, 100).contains("-- why\n) AS zpo_q"));
    }

    #[test]
    fn parses_chunks_in_order() {
        // Cells as the shell prints them: quoted, inner quotes not escaped.
        let out = "\"zpo_chunk\"\n\"[[2,3,\"c\"],[3,4,null]]\"\n\"[[0,1,\"a,\\\"q\\\"\"],[1,2,\"b\\nx\"]]\"\n";
        let rows = parse(out).unwrap();
        assert_eq!(
            rows.rows,
            vec![
                vec![json!(1), json!("a,\"q\"")],
                vec![json!(2), json!("b\nx")],
                vec![json!(3), json!("c")],
                vec![json!(4), Value::Null],
            ]
        );
        let r = rows.iter().nth(3).unwrap();
        assert_eq!(r.get::<Option<String>>(1), None);
        assert_eq!(r.get::<i64>(0), 4);
        assert_eq!(parse("\"zpo_chunk\"\n").unwrap().len(), 0);
        assert_eq!(parse("").unwrap().len(), 0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("\"zpo_chunk\"\nnot json\n").is_err());
    }
}
