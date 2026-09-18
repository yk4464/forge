//! Reserved extension points. M1 ships the traits only; concrete skills,
//! hooks and plugin adapters arrive in later milestones (recorded decision:
//! MVP不含Skill，trait留位).

use serde_json::Value;

use crate::error::Result;

/// Lifecycle events on which hooks can fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    BeforeAgent,
    AfterAgent,
    BeforeModel,
    AfterModel,
    BeforeTool,
    AfterTool,
    OnMessage,
    OnError,
    OnSessionStart,
    OnSessionEnd,
}

/// A hook observes (and in future may veto) pipeline points. Payloads are
/// JSON to keep the trait protocol-agnostic.
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    async fn on(&self, point: HookPoint, payload: &Value) -> Result<()>;
}

/// Placeholder for M2+: a Skill contributes system-prompt fragments and
/// optionally additional tools, discovered from local dirs / config / MCP.
#[async_trait::async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// Text injected into the system prompt when the skill is active.
    fn prompt_fragment(&self) -> &str;
    /// Optional extra tool specs contributed by this skill.
    fn tool_specs(&self) -> Vec<crate::traits::ToolSpec> {
        Vec::new()
    }
}
