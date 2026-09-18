use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use forge_core::error::{Error, Result};
use forge_core::message::Message;
use forge_core::session::{SessionStore, SessionSummary};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Pool, Row, Sqlite};
use std::path::Path;
use std::str::FromStr;
use uuid::Uuid;

/// SQLite-backed SessionStore. Messages are stored as serde-JSON of the
/// canonical Message enum, so schema changes never lose transcripts.
pub struct SqliteSessionStore {
    pool: Pool<Sqlite>,
}

impl SqliteSessionStore {
    pub async fn open(db_path: &Path) -> Result<Self> {
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::Storage(format!("cannot create {}: {e}", dir.display())))?;
        }
        let url = format!("sqlite://{}", db_path.display().to_string().replace('\\', "/"));
        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(|e| Error::Storage(e.to_string()))?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(
            r#"
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS messages (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    role TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, ordinal);
CREATE TABLE IF NOT EXISTS session_meta (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (session_id, key)
);
"#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    async fn bump_session(&self, id: Uuid) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE sessions SET updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).single().unwrap_or_default())
}

fn row_to_summary(row: SqliteRow) -> SessionSummary {
    let id: String = row.get("id");
    SessionSummary {
        id: Uuid::parse_str(&id).unwrap_or_else(|_| Uuid::nil()),
        title: row.get("title"),
        created_at: parse_ts(row.get::<String, _>("created_at").as_str()),
        updated_at: parse_ts(row.get::<String, _>("updated_at").as_str()),
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create_session(&self, title: &str) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO sessions (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(id.to_string())
            .bind(title)
            .bind(&now)
            .bind(&now)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(id)
    }

    async fn list_sessions(&self, limit: u32) -> Result<Vec<SessionSummary>> {
        let rows = sqlx::query(
            "SELECT id, title, created_at, updated_at FROM sessions \
             ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(row_to_summary).collect())
    }

    async fn load_messages(&self, session_id: Uuid) -> Result<Vec<Message>> {
        let rows = sqlx::query(
            "SELECT payload FROM messages WHERE session_id = ? ORDER BY ordinal ASC",
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Storage(e.to_string()))?;
        let mut msgs = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: String = r.get("payload");
            let msg: Message = serde_json::from_str(&payload)
                .map_err(|e| Error::Storage(format!("corrupt message payload: {e}")))?;
            msgs.push(msg);
        }
        Ok(msgs)
    }

    async fn append_messages(&self, session_id: Uuid, msgs: &[Message]) -> Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        let base: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(ordinal), -1) FROM messages WHERE session_id = ?",
        )
        .bind(session_id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Error::Storage(e.to_string()))?;
        for (i, m) in msgs.iter().enumerate() {
            let payload =
                serde_json::to_string(m).map_err(|e| Error::Storage(e.to_string()))?;
            let role = message_role(m).to_string();
            sqlx::query(
                "INSERT INTO messages (session_id, ordinal, role, payload) VALUES (?, ?, ?, ?)",
            )
            .bind(session_id.to_string())
            .bind(base + 1 + i as i64)
            .bind(role)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        self.bump_session(session_id).await
    }

    async fn replace_messages(&self, session_id: Uuid, msgs: &[Message]) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        for (i, m) in msgs.iter().enumerate() {
            let payload =
                serde_json::to_string(m).map_err(|e| Error::Storage(e.to_string()))?;
            let role = message_role(m).to_string();
            sqlx::query(
                "INSERT INTO messages (session_id, ordinal, role, payload) VALUES (?, ?, ?, ?)",
            )
            .bind(session_id.to_string())
            .bind(i as i64)
            .bind(role)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        self.bump_session(session_id).await
    }

    async fn rename_session(&self, session_id: Uuid, title: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
            .bind(title)
            .bind(session_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    async fn set_meta(&self, session_id: Uuid, key: &str, value: &Value) -> Result<()> {
        sqlx::query(
            "INSERT INTO session_meta (session_id, key, value) VALUES (?, ?, ?) \
             ON CONFLICT(session_id, key) DO UPDATE SET value = excluded.value",
        )
        .bind(session_id.to_string())
        .bind(key)
        .bind(value.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(())
    }

    async fn get_meta(&self, session_id: Uuid, key: &str) -> Result<Option<Value>> {
        let row: Option<SqliteRow> =
            sqlx::query("SELECT value FROM session_meta WHERE session_id = ? AND key = ?")
                .bind(session_id.to_string())
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::Storage(e.to_string()))?;
        match row {
            None => Ok(None),
            Some(r) => {
                let raw: String = r.get("value");
                let v = serde_json::from_str(&raw)
                    .map_err(|e| Error::Storage(format!("corrupt meta: {e}")))?;
                Ok(Some(v))
            }
        }
    }
}

fn message_role(m: &Message) -> &'static str {
    match m {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
        Message::ToolResult { .. } => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::message::ToolCall;

    async fn memory_store() -> SqliteSessionStore {
        let dir = std::env::temp_dir().join(format!("forge-test-{}", Uuid::new_v4()));
        SqliteSessionStore::open(&dir.join("test.db")).await.unwrap()
    }

    #[tokio::test]
    async fn session_roundtrip() {
        let store = memory_store().await;
        let sid = store.create_session("hello task").await.unwrap();

        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::Assistant {
                content: "running".into(),
                reasoning: Some("think".into()),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
            },
            Message::ToolResult {
                tool_call_id: "c1".into(),
                content: "a.txt".into(),
                is_error: false,
            },
        ];
        store.append_messages(sid, &msgs).await.unwrap();
        let loaded = store.load_messages(sid).await.unwrap();
        assert_eq!(loaded, msgs);

        // Replace (compaction path).
        let compacted = vec![Message::user("hi"), Message::user("summary")];
        store.replace_messages(sid, &compacted).await.unwrap();
        assert_eq!(store.load_messages(sid).await.unwrap(), compacted);

        // Meta.
        store.set_meta(sid, "usage", &serde_json::json!({"total": 42})).await.unwrap();
        assert_eq!(
            store.get_meta(sid, "usage").await.unwrap().unwrap(),
            serde_json::json!({"total": 42})
        );

        // List.
        let list = store.list_sessions(10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "hello task");
    }

    #[tokio::test]
    async fn replace_to_empty_then_append() {
        let store = memory_store().await;
        let sid = store.create_session("t").await.unwrap();
        store
            .append_messages(sid, &[Message::user("a"), Message::user("b")])
            .await
            .unwrap();
        store.replace_messages(sid, &[]).await.unwrap();
        assert!(store.load_messages(sid).await.unwrap().is_empty());
        store
            .append_messages(sid, &[Message::user("c"), Message::user("d")])
            .await
            .unwrap();
        let loaded = store.load_messages(sid).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(matches!(&loaded[0], Message::User { content } if content == "c"));
    }
}
