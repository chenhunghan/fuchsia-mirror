// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::LazyLock;

static SESSION_ID_ENV_VARS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    include_str!("../../../../tools/devshell/lib/agent_session_ids.txt")
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .collect()
});

static AGENTS_ENV_VARS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    include_str!("../../../../tools/devshell/lib/agents.txt")
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .chain(SESSION_ID_ENV_VARS.iter().copied())
        .collect()
});

pub trait EnvironmentSource {
    fn has_var(&self, key: &str) -> bool {
        self.var(key).is_some_and(|v| !v.trim().is_empty())
    }

    fn var(&self, key: &str) -> Option<String>;
}

pub struct SystemEnvironment;
impl EnvironmentSource for SystemEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Returns true if the current process appears to be invoked by an AI agent.
pub fn is_invoked_by_agent<E: EnvironmentSource>(env: &E) -> bool {
    AGENTS_ENV_VARS.iter().any(|&name| env.has_var(name))
}

/// Returns true if the current process appears to be invoked by an AI agent, using the system environment.
pub fn is_agent_env() -> bool {
    is_invoked_by_agent(&SystemEnvironment)
}

/// Resolves the calling agent's session ID from `$FFX_AGENT_SESSION_ID`,
/// `$ANTIGRAVITY_CONVERSATION_ID`, or `$CLAUDE_CODE_SESSION_ID`.
pub fn resolve_session_id<E: EnvironmentSource>(env: &E) -> Option<String> {
    for &key in SESSION_ID_ENV_VARS.iter() {
        if let Some(val) = env.var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

impl<V: AsRef<str>> EnvironmentSource for std::collections::HashMap<String, V> {
    fn has_var(&self, key: &str) -> bool {
        self.contains_key(key)
    }

    fn var(&self, key: &str) -> Option<String> {
        self.get(key).map(|v| v.as_ref().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv {
        vars: HashMap<String, String>,
    }

    impl EnvironmentSource for FakeEnv {
        fn has_var(&self, key: &str) -> bool {
            self.vars.contains_key(key)
        }

        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
    }

    #[test]
    fn test_is_invoked_by_agent() {
        let vars = HashMap::new();
        let env = FakeEnv { vars };
        assert!(!is_invoked_by_agent(&env));

        for var in [
            "ANTIGRAVITY_AGENT",
            "ANTIGRAVITY_CONVERSATION_ID",
            "GEMINI_CLI",
            "ANTIGRAVITY_EDITOR_APP_ROOT",
            "CLAUDECODE",
            "CLAUDE_CODE_SESSION_ID",
            "FFX_AGENT_SESSION_ID",
        ] {
            let mut vars = HashMap::new();
            vars.insert(var.into(), "1".into());
            let env = FakeEnv { vars };
            assert!(is_invoked_by_agent(&env), "expected true for {var}");
        }
    }

    #[test]
    fn test_resolve_session_id() {
        let env = FakeEnv { vars: HashMap::new() };
        assert_eq!(resolve_session_id(&env), None);

        let env_both = FakeEnv {
            vars: HashMap::from([
                ("FFX_AGENT_SESSION_ID".into(), "explicit-id".into()),
                ("ANTIGRAVITY_CONVERSATION_ID".into(), "conv-id".into()),
            ]),
        };
        assert_eq!(resolve_session_id(&env_both), Some("explicit-id".to_string()));

        let env_fallback = FakeEnv {
            vars: HashMap::from([
                ("FFX_AGENT_SESSION_ID".into(), "   ".into()),
                ("ANTIGRAVITY_CONVERSATION_ID".into(), "conv-id".into()),
            ]),
        };
        assert_eq!(resolve_session_id(&env_fallback), Some("conv-id".to_string()));

        let env_claude = FakeEnv {
            vars: HashMap::from([("CLAUDE_CODE_SESSION_ID".into(), "claude-id".into())]),
        };
        assert_eq!(resolve_session_id(&env_claude), Some("claude-id".to_string()));
    }
}
