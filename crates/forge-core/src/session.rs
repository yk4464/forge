use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;
use crate::message::Message;

/// Metadata row for the /resume picker.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lifecycle state of a recorded tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEventState {
    /// Started executing; no terminal state reached (crash mid-run).
    Started,
    /// Finished successfully.
    Completed,
    /// Finished with a failure (tool error, denial, cancellation).
    Failed,
}

impl ToolEventState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolEventState::Started => "started",
            ToolEventState::Completed => "completed",
            ToolEventState::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "completed" => ToolEventState::Completed,
            "failed" => ToolEventState::Failed,
            // An unrecognized state is treated as ambiguous on purpose:
            // recovery must assume the real-world outcome is unknown.
            _ => ToolEventState::Started,
        }
    }
}

/// One persisted tool-execution record (S1 recovery log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    pub state: ToolEventState,
    pub output: String,
}

/// Persistence boundary. forge-storage provides the SQLite implementation;
/// core depends only on this trait so tests can use memory doubles.
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, title: &str) -> Result<Uuid>;

    async fn list_sessions(&self, limit: u32) -> Result<Vec<SessionSummary>>;

    /// Load full transcript for a session, in insertion order.
    async fn load_messages(&self, session_id: Uuid) -> Result<Vec<Message>>;

    /// Persist messages appended since the last call. Idempotency is the
    /// caller's job (agent loop tracks what it saved).
    async fn append_messages(&self, session_id: Uuid, msgs: &[Message]) -> Result<()>;

    /// Replace the whole transcript (compaction) and update the title.
    async fn replace_messages(&self, session_id: Uuid, msgs: &[Message]) -> Result<()>;

    async fn rename_session(&self, session_id: Uuid, title: &str) -> Result<()>;

    /// Record that a tool call is about to execute — written BEFORE the
    /// tool runs, so recovery can distinguish "never started" from
    /// "started, outcome unknown".
    async fn record_tool_started(
        &self,
        session_id: Uuid,
        call_id: &str,
        tool: &str,
        arguments: &Value,
    ) -> Result<()>;

    /// Record the terminal state of a tool call — written BEFORE the
    /// result enters the transcript, so recovery can reuse a completed
    /// record instead of guessing what happened.
    async fn record_tool_finished(
        &self,
        session_id: Uuid,
        call_id: &str,
        state: ToolEventState,
        output: &str,
    ) -> Result<()>;

    /// All tool-execution records for a session, in write order.
    async fn load_tool_events(&self, session_id: Uuid) -> Result<Vec<ToolEvent>>;

    /// Store raw JSON payloads keyed for diagnostics (e.g. last usage).
    async fn set_meta(&self, session_id: Uuid, key: &str, value: &Value) -> Result<()>;

    async fn get_meta(&self, session_id: Uuid, key: &str) -> Result<Option<Value>>;
}

/// Decision for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run it.
    Allow,
    /// Defer to the user (or refuse when no approver is available — headless
    /// mode must never default to allow).
    Ask,
    /// Refuse it.
    Deny,
}

/// No-op PermissionPolicy: M1 allowed everything; it survives for tests
/// and as the explicit "trust everything" choice.
#[async_trait]
pub trait PermissionPolicy: Send + Sync {
    /// Evaluate a tool call.
    async fn approve(&self, tool_name: &str, arguments: &Value) -> PermissionDecision;
}

/// Always-allow policy (prototype default; S2 adds rule-based policies).
pub struct AllowAll;

#[async_trait]
impl PermissionPolicy for AllowAll {
    async fn approve(&self, _tool_name: &str, _arguments: &Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
}
