// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Access to Perfetto's trace processor through the prebuilt
//! `trace_processor_shell`.
//!
//! A capture is loaded once into a warm `server unix` session ([`session`]),
//! then queried with `query --remote` ([`client`]). Every SQL query zpo runs is
//! in [`queries`]. Results come back as chunked JSON built by SQLite, because
//! the shell's CSV output is lossy (NULLs, quotes and floats) and one RPC
//! message is capped at 64 MiB.

pub mod client;
pub mod queries;
pub mod session;

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The trace processor's file name next to the `zpo` binary. The build fetches
/// the prebuilt `trace_processor_shell` and installs it there under this name.
pub const INSTALLED_NAME: &str = "zpo_trace_processor_shell";

/// The trace processor zpo runs: `$ZPO_TRACE_PROCESSOR` if set, otherwise the
/// one installed next to `exe` (the running `zpo`).
pub fn bin_path(env_override: Option<&str>, exe: Option<&Path>) -> PathBuf {
    if let Some(p) = env_override.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    exe.and_then(Path::parent).unwrap_or(Path::new(".")).join(INSTALLED_NAME)
}

/// [`bin_path`] for this process.
pub fn default_bin() -> PathBuf {
    bin_path(
        std::env::var("ZPO_TRACE_PROCESSOR").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

/// One SQL query and the columns it returns.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// A short name, for errors and tests.
    pub name: &'static str,
    /// The SQL. It may have its own ORDER BY and LIMIT.
    pub sql: String,
    /// The names of the result columns, in order.
    pub cols: &'static [&'static str],
    /// The order of the result rows, in terms of `cols` (e.g. "ts"), or "" when
    /// the order does not matter.
    pub order: &'static str,
}

/// The rows a query returned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|r| Row(r))
    }

    pub fn first(&self) -> Option<Row<'_>> {
        self.rows.first().map(|r| Row(r))
    }
}

/// One result row.
#[derive(Clone, Copy, Debug)]
pub struct Row<'a>(pub &'a [Value]);

impl Row<'_> {
    /// Column i converted to `T`. NULL (or a value that does not convert)
    /// gives `T::default()`: 0, 0.0 or "" for plain types, and None for
    /// `Option`s, which is how nullable columns are read.
    pub fn get<T: FromColumn + Default>(&self, i: usize) -> T {
        T::from_column(self.0.get(i).unwrap_or(&Value::Null)).unwrap_or_default()
    }
}

/// A type a result column can be read as with [`Row::get`].
pub trait FromColumn: Sized {
    /// The column's value as `Self`, None for NULL or a value that does not
    /// convert.
    fn from_column(v: &Value) -> Option<Self>;
}

impl FromColumn for i64 {
    /// Floats are truncated.
    fn from_column(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Value::Bool(b) => Some(*b as i64),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }
}

impl FromColumn for f64 {
    fn from_column(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }
}

impl FromColumn for String {
    /// Numbers are formatted.
    fn from_column(v: &Value) -> Option<Self> {
        match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }
}

impl FromColumn for Value {
    /// The column as it came back.
    fn from_column(v: &Value) -> Option<Self> {
        Some(v.clone())
    }
}

impl<T: FromColumn> FromColumn for Option<T> {
    fn from_column(v: &Value) -> Option<Self> {
        Some(T::from_column(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_path_prefers_the_override_then_the_installed_copy() {
        assert_eq!(bin_path(Some("/x/tp"), Some(Path::new("/h/zpo"))), PathBuf::from("/x/tp"));
        assert_eq!(
            bin_path(Some(""), Some(Path::new("/h/zpo"))),
            PathBuf::from("/h").join(INSTALLED_NAME)
        );
        assert_eq!(bin_path(None, None), PathBuf::from(".").join(INSTALLED_NAME));
    }
}
