//! Rule-based permission policy (S2 §3 统一权限层).
//!
//! Decisions per tool: allow / ask / deny. Rules come from config
//! (`[permissions]`), session approvals layer on top ("always allow this
//! session"), and an explicit fallback applies to unlisted tools — the
//! shell defaults to ask, everything unknown defaults to deny.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::config::PermissionConfig;
use crate::session::{PermissionDecision, PermissionPolicy};

/// One rule decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Allow,
    Ask,
    Deny,
}

impl Rule {
    pub fn parse(s: &str) -> Option<Rule> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" => Some(Rule::Allow),
            "ask" => Some(Rule::Ask),
            "deny" => Some(Rule::Deny),
            _ => None,
        }
    }
}

/// Rule policy: config rules + session approvals + fallback.
pub struct RulePolicy {
    /// Config-declared rules (tool name → rule).
    config_rules: HashMap<String, Rule>,
    /// Fallback for tools absent from config_rules.
    default: Rule,
    /// Fallback for tools absent everywhere (never configured).
    unlisted: Rule,
    /// Session approvals ("always allow this session"), layered on top.
    session_rules: RwLock<HashMap<String, Rule>>,
}

impl RulePolicy {
    /// Builtin fallbacks: the shell touches the whole system, so it asks;
    /// unknown tools are refused outright.
    const SHELL_FALLBACK: Rule = Rule::Ask;
    const UNLISTED_FALLBACK: Rule = Rule::Deny;

    pub fn from_config(cfg: &PermissionConfig) -> Self {
        let mut config_rules = HashMap::new();
        for (tool, rule) in &cfg.tools {
            if let Some(r) = Rule::parse(rule) {
                config_rules.insert(tool.clone(), r);
            }
        }
        let default = cfg
            .default
            .as_deref()
            .and_then(Rule::parse)
            .unwrap_or(Self::UNLISTED_FALLBACK);
        Self {
            config_rules,
            default,
            unlisted: Self::UNLISTED_FALLBACK,
            session_rules: RwLock::new(HashMap::new()),
        }
    }

    /// Record a session-scoped approval (TUI "always this session").
    pub fn set_session_rule(&self, tool: &str, rule: Rule) {
        self.session_rules
            .write()
            .unwrap()
            .insert(tool.to_string(), rule);
    }

    fn resolve(&self, tool: &str) -> Rule {
        if let Some(r) = self.session_rules.read().unwrap().get(tool) {
            return *r;
        }
        if let Some(r) = self.config_rules.get(tool) {
            return *r;
        }
        if tool == "shell" {
            return Self::SHELL_FALLBACK;
        }
        if self.config_rules.is_empty() && self.default == Self::UNLISTED_FALLBACK {
            return self.unlisted;
        }
        self.default
    }
}

#[async_trait]
impl PermissionPolicy for RulePolicy {
    async fn approve(&self, tool_name: &str, _arguments: &Value) -> PermissionDecision {
        match self.resolve(tool_name) {
            Rule::Allow => PermissionDecision::Allow,
            Rule::Ask => PermissionDecision::Ask,
            Rule::Deny => PermissionDecision::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(toml_str: &str) -> RulePolicy {
        let cfg: PermissionConfig = toml::from_str(toml_str).unwrap();
        RulePolicy::from_config(&cfg)
    }

    #[tokio::test]
    async fn shell_defaults_to_ask_unknown_to_deny() {
        let p = policy("");
        assert_eq!(p.resolve("shell"), Rule::Ask);
        assert_eq!(p.resolve("mystery"), Rule::Deny);
    }

    #[tokio::test]
    async fn config_rules_and_default_apply() {
        let p = policy(
            r#"
default = "allow"
[tools]
shell = "deny"
"#,
        );
        assert_eq!(p.resolve("shell"), Rule::Deny);
        assert_eq!(p.resolve("other"), Rule::Allow);
    }

    #[tokio::test]
    async fn session_rules_override_config() {
        let p = policy(r#"[tools]
shell = "ask""#);
        assert_eq!(p.resolve("shell"), Rule::Ask);
        p.set_session_rule("shell", Rule::Allow);
        assert_eq!(p.resolve("shell"), Rule::Allow);
    }

    #[tokio::test]
    async fn approve_maps_rules() {
        let p = policy(r#"[tools]
echo = "allow"
ghost = "deny""#);
        assert_eq!(
            p.approve("echo", &serde_json::json!({})).await,
            PermissionDecision::Allow
        );
        assert_eq!(
            p.approve("ghost", &serde_json::json!({})).await,
            PermissionDecision::Deny
        );
        assert_eq!(
            p.approve("shell", &serde_json::json!({})).await,
            PermissionDecision::Ask
        );
    }
}
