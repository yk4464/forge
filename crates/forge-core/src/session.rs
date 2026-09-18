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

    /// Store raw JSON payloads keyed for diagnostics (e.g. last usage).
    async fn set_meta(&self, session_id: Uuid, key: &str, value: &Value) -> Result<()>;

    async fn get_meta(&self, session_id: Uuid, key: &str) -> Result<Option<Value>>;
}

/// No-op PermissionPolicy: M1 allows everything (decision recorded in the
/// plan). The trait exists so approval UIs can slot in without touching the
/// agent loop.
#[async_trait]
pub trait PermissionPolicy: Send + Sync {
    /// Return true to allow execution.
    async fn approve(&self, tool_name: &str, arguments: &Value) -> bool;
}

/// Always-allow policy (M1 default).
pub struct AllowAll;

#[async_trait]
impl PermissionPolicy for AllowAll {
    async fn approve(&self, _tool_name: &str, _arguments: &Value) -> bool {
        true
    }
}
