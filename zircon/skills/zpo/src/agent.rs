// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The optional coding-agent backend (Explain, Discover, Confirm roles).
//!
//! zpo does not talk to any agent itself. An extension (see
//! [`crate::extensions`]) names a backend executable, which zpo runs as
//! `<backend> <verb>` with a JSON request on stdin; the backend prints a JSON
//! reply on stdout and exits 0, or prints an error and exits non-zero.
//!
//! | verb           | request                                   | reply                                        |
//! |----------------|-------------------------------------------|----------------------------------------------|
//! | `info`         | `{}`                                      | `{"available": bool, "hint": str}`           |
//! | `models`       | `{}`                                      | `{"default": id, "groups": [[label, [[id, name], ...]], ...], "live": bool}` |
//! | `ask`          | `{"prompt", "title", "timeout_s", "model"}` | `{"text": markdown}`                       |
//! | `query-access` | `{"prefs": {...}, "rule": str}`           | `{"allowed": bool, "via": str?, "reason": str}` |
//!
//! `query-access` says whether the agent may run trace processor queries
//! (`rule` is the command it would run) without asking the user; a blocked
//! command would stall a conversation nobody is watching.
//!
//! Without a backend, the UI hides the agent features.

use crate::proc::{HostRunner, Runner};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// A model group: (label, [(id, display name)]).
pub type ModelGroup = (String, Vec<(String, String)>);

/// The models the agent offers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Models {
    pub default: String,
    pub groups: Vec<ModelGroup>,
    #[serde(default)]
    pub live: bool,
}

impl Models {
    /// id -> display name.
    pub fn names(&self) -> indexmap::IndexMap<String, String> {
        self.groups.iter().flat_map(|(_, ms)| ms.iter().cloned()).collect()
    }
}

/// Whether the agent may query the traces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryAccess {
    pub allowed: bool,
    #[serde(default)]
    pub via: Option<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub rule: String,
}

/// How long a quick backend verb (`info`, `models`, `query-access`) may
/// take: these only check the backend's state.
const QUICK_VERB_TIMEOUT: Duration = Duration::from_secs(20);
/// What `ask` allows beyond the timeout the agent is given, for the backend
/// to start and to return the answer.
const ASK_GRACE_S: u64 = 120;

/// A configured agent backend.
#[derive(Clone)]
pub struct Agent {
    pub name: String,
    pub command: PathBuf,
    runner: Arc<dyn Runner>,
}

impl std::fmt::Debug for Agent {
    /// The runner is a trait object, which is not `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

impl Agent {
    pub fn new(name: String, command: PathBuf) -> Self {
        Agent { name, command, runner: Arc::new(HostRunner) }
    }

    /// Uses another runner (tests).
    pub fn with_runner(mut self, runner: Arc<dyn Runner>) -> Self {
        self.runner = runner;
        self
    }

    /// Runs one verb. The request goes through a temporary file on stdin.
    fn call(&self, verb: &str, req: &Value, timeout: Duration) -> anyhow::Result<Value> {
        let dir = tempfile::tempdir()?;
        let req_path = dir.path().join("request.json");
        std::fs::write(&req_path, serde_json::to_vec(req)?)?;
        let args = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exec \"$0\" \"$1\" < \"$2\"".to_string(),
            self.command.display().to_string(),
            verb.to_string(),
            req_path.display().to_string(),
        ];
        let out = self.runner.run(&args, timeout).with_context(|| {
            format!("could not run the {} backend {}", self.name, self.command.display())
        })?;
        if out.timed_out {
            bail!(
                "the {} backend did not answer `{verb}` within {}s",
                self.name,
                timeout.as_secs()
            );
        }
        if !out.success() {
            let msg = out.combined();
            bail!("{}", msg.trim());
        }
        serde_json::from_str(out.stdout.trim())
            .with_context(|| format!("the {} backend's `{verb}` reply is not JSON", self.name))
    }

    /// Asks the backend whether it can run right now. Returns whether it is
    /// available and, when it is not, a hint to show the user (for example how
    /// to start it).
    pub fn info(&self) -> (bool, String) {
        match self.call("info", &json!({}), QUICK_VERB_TIMEOUT) {
            Ok(v) => (
                v.get("available").and_then(Value::as_bool).unwrap_or(false),
                v.get("hint").and_then(Value::as_str).unwrap_or_default().to_string(),
            ),
            Err(e) => (false, e.to_string()),
        }
    }

    /// The models the agent offers right now.
    pub fn models(&self) -> anyhow::Result<Models> {
        let v = self.call("models", &json!({}), QUICK_VERB_TIMEOUT)?;
        Ok(serde_json::from_value(v)?)
    }

    /// Asks the agent and returns its markdown answer.
    pub fn ask(
        &self,
        prompt: &str,
        title: &str,
        timeout_s: u64,
        model: &str,
    ) -> anyhow::Result<String> {
        let req = json!({"prompt": prompt, "title": title, "timeout_s": timeout_s, "model": model});
        let v =
            self.call("ask", &req, Duration::from_secs(timeout_s.saturating_add(ASK_GRACE_S)))?;
        v.get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("the agent's answer has no text")
    }

    /// Whether the agent may run `rule` without asking.
    pub fn query_access(&self, prefs: &Value, rule: &str) -> QueryAccess {
        let req = json!({"prefs": prefs, "rule": rule});
        match self
            .call("query-access", &req, QUICK_VERB_TIMEOUT)
            .and_then(|v| Ok(serde_json::from_value::<QueryAccess>(v)?))
        {
            Ok(mut a) => {
                a.rule = rule.to_string();
                a
            }
            Err(e) => QueryAccess {
                allowed: false,
                via: None,
                reason: e.to_string(),
                rule: rule.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(script: &str) -> (tempfile::TempDir, Agent) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("backend");
        crate::checkout::testing::write_test_script(&p, script);
        (d, Agent::new("Helper".into(), p))
    }

    #[test]
    fn calls_verbs_with_json() {
        let (d, a) = backend(
            "#!/bin/sh\ncase \"$1\" in\n\
             info) echo '{\"available\": true}';;\n\
             models) echo '{\"default\": \"m1\", \"groups\": [[\"G\", [[\"m1\", \"Model 1\"]]]], \"live\": true}';;\n\
             ask) cat > \"$(dirname \"$0\")/request.json\"; echo '{\"text\": \"the answer\"}';;\n\
             query-access) echo '{\"allowed\": false, \"reason\": \"asks first\"}';;\n\
             *) echo \"unknown $1\" >&2; exit 2;;\nesac\n",
        );
        assert_eq!(a.info(), (true, String::new()));
        let m = a.models().unwrap();
        assert_eq!(m.default, "m1");
        assert_eq!(m.names().get("m1").map(String::as_str), Some("Model 1"));
        assert_eq!(a.ask("the prompt", "zpo: t", 10, "m1").unwrap(), "the answer");
        let req: Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("request.json")).unwrap())
                .unwrap();
        assert_eq!(
            req,
            json!({"prompt": "the prompt", "title": "zpo: t", "timeout_s": 10, "model": "m1"})
        );
        let q = a.query_access(&json!({}), "command(tp query --remote)");
        assert!(!q.allowed);
        assert_eq!(q.reason, "asks first");
        assert_eq!(q.rule, "command(tp query --remote)");
    }

    #[test]
    fn failures_become_errors() {
        let (_d, a) = backend("#!/bin/sh\necho 'not running' >&2\nexit 1\n");
        assert!(a.models().unwrap_err().to_string().contains("not running"));
        let (ok, hint) = a.info();
        assert!(!ok);
        assert!(hint.contains("not running"));
    }
}
