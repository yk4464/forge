//! Minimal runtime interface (S1 §14 运行接口): `submit` / `cancel` /
//! per-turn event streams, extracted from the TUI entry point so other
//! frontends (headless, Web, desktop) reuse the same orchestration instead
//! of re-implementing it. `approve` / `reply` join here with the S2
//! permission layer and S6 multi-client work.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::agent_loop::Agent;
use crate::cancel::CancelHandle;
use crate::error::{Error, Result};
use crate::event::AgentEvent;
use crate::message::Usage;

/// One submitted turn: its id and its event stream. The stream ends right
/// after the terminal event (`Error` or `TurnCompleted`), so every run has
/// exactly one observable end (S1 §12: 每次运行有唯一终态). The busy flag
/// is always cleared BEFORE the stream closes, so a consumer that drained
/// the stream can immediately submit again.
pub struct TurnHandle {
    pub turn_id: u64,
    events: mpsc::UnboundedReceiver<AgentEvent>,
}

impl TurnHandle {
    /// Next event of this turn; `None` once the turn has ended.
    pub async fn next(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// Take the raw receiver (for poll-style consumers like the TUI).
    pub fn into_events(self) -> mpsc::UnboundedReceiver<AgentEvent> {
        self.events
    }
}

/// Drives one agent: serialized submits, cancellation, per-turn streams.
pub struct Runtime {
    agent: Arc<Agent>,
    busy: AtomicBool,
    next_turn_id: AtomicU64,
    cancel: Mutex<Option<CancelHandle>>,
}

impl Runtime {
    pub fn new(agent: Arc<Agent>) -> Arc<Self> {
        // A runtime always carries a frontend that can answer approvals.
        agent.approvals_enabled.store(true, Ordering::SeqCst);
        Arc::new(Self {
            agent,
            busy: AtomicBool::new(false),
            next_turn_id: AtomicU64::new(1),
            cancel: Mutex::new(None),
        })
    }

    /// The wrapped agent (for out-of-band commands like `/compact`).
    pub fn agent(&self) -> Arc<Agent> {
        self.agent.clone()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Submit a user turn. Rejected while another turn is running: one
    /// session has one active turn (S1 §12 并发提交受控).
    pub async fn submit(self: &Arc<Self>, input: impl Into<String>) -> Result<TurnHandle> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::Busy);
        }
        let turn_id = self.next_turn_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = CancelHandle::default();
        let token = handle.token();
        *self.cancel.lock().await = Some(handle);

        let this = self.clone();
        let agent = self.agent.clone();
        let text = input.into();
        // Guard sender: held by the supervisor so the stream stays open
        // until after the busy flag is cleared (ordering contract above).
        let tx_guard = tx.clone();
        tokio::spawn(async move {
            // run_turn guarantees the terminal events; the guarded inner
            // task only covers a panicked agent task (Error + TurnCompleted
            // are synthesized so the stream still has a unique end).
            let inner = tokio::spawn(async move { agent.run_turn(&text, tx, &token).await });
            if let Err(join) = inner.await {
                let _ = tx_guard.send(AgentEvent::Error {
                    message: format!("agent task crashed: {join}"),
                });
                let _ = tx_guard.send(AgentEvent::TurnCompleted {
                    usage: Usage::default(),
                });
            }
            this.busy.store(false, Ordering::SeqCst);
            *this.cancel.lock().await = None;
            drop(tx_guard);
        });

        Ok(TurnHandle { turn_id, events: rx })
    }

    /// Cancel the running turn; a no-op when idle. The stream still ends
    /// with the usual terminal events.
    pub async fn cancel(&self) {
        if let Some(handle) = self.cancel.lock().await.take() {
            handle.cancel();
        }
    }

    /// Answer a pending approval request (S2). Returns false when no such
    /// request is pending.
    pub async fn approve(&self, call_id: &str, approved: bool) -> bool {
        self.agent.approval_gate.respond(call_id, approved).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextManager;
    use crate::registry::Registry;
    use crate::session::{AllowAll, SessionStore, SessionSummary, ToolEvent, ToolEventState};
    use crate::traits::{ModelProvider, ModelRequest, ProviderEvent};
    use async_trait::async_trait;
    use futures::StreamExt as _;
    use futures::stream::BoxStream;
    use std::time::Duration;

    /// Provider whose single response streams after a delay.
    struct SlowProvider(Duration);

    #[async_trait]
    impl ModelProvider for SlowProvider {
        fn name(&self) -> &str {
            "slow"
        }
        async fn stream(
            &self,
            _req: ModelRequest,
            _api_key: &str,
        ) -> Result<BoxStream<'static, ProviderEvent>> {
            let delay = self.0;
            Ok(Box::pin(
                futures::stream::once(async move {
                    tokio::time::sleep(delay).await;
                    ProviderEvent::MessageDelta {
                        delta: "finally".into(),
                    }
                })
                .chain(futures::stream::once(async { ProviderEvent::Done })),
            ))
        }
    }

    /// Session store that never touches disk.
    struct MemStore;

    #[async_trait]
    impl SessionStore for MemStore {
        async fn create_session(&self, _t: &str) -> Result<uuid::Uuid> {
            Ok(uuid::Uuid::new_v4())
        }
        async fn list_sessions(&self, _l: u32) -> Result<Vec<SessionSummary>> {
            Ok(vec![])
        }
        async fn load_messages(&self, _s: uuid::Uuid) -> Result<Vec<crate::message::Message>> {
            Ok(vec![])
        }
        async fn append_messages(
            &self,
            _s: uuid::Uuid,
            _m: &[crate::message::Message],
        ) -> Result<()> {
            Ok(())
        }
        async fn replace_messages(
            &self,
            _s: uuid::Uuid,
            _m: &[crate::message::Message],
        ) -> Result<()> {
            Ok(())
        }
        async fn rename_session(&self, _s: uuid::Uuid, _t: &str) -> Result<()> {
            Ok(())
        }
        async fn record_tool_started(
            &self,
            _: uuid::Uuid,
            _: &str,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<()> {
            Ok(())
        }
        async fn record_tool_finished(
            &self,
            _: uuid::Uuid,
            _: &str,
            _: ToolEventState,
            _: &str,
        ) -> Result<()> {
            Ok(())
        }
        async fn load_tool_events(&self, _: uuid::Uuid) -> Result<Vec<ToolEvent>> {
            Ok(vec![])
        }
        async fn set_meta(&self, _: uuid::Uuid, _: &str, _: &serde_json::Value) -> Result<()> {
            Ok(())
        }
        async fn get_meta(&self, _: uuid::Uuid, _: &str) -> Result<Option<serde_json::Value>> {
            Ok(None)
        }
    }

    fn agent_with(provider: Arc<dyn ModelProvider>) -> Arc<Agent> {
        Arc::new(Agent {
            provider,
            tools: Registry::new(),
            store: Arc::new(MemStore),
            permissions: Arc::new(AllowAll),
            context: Mutex::new(ContextManager::new(Default::default())),
            api_key: "k".into(),
            model: "m".into(),
            temperature: None,
            max_tokens: 64,
            budget: crate::config::BudgetConfig::default(),
            session_id: None,
            persisted_len: std::sync::atomic::AtomicUsize::new(0),
            history_replaced: std::sync::atomic::AtomicBool::new(false),
            persist_failed: std::sync::atomic::AtomicBool::new(false),
            approval_gate: Arc::new(crate::approval::ApprovalGate::new()),
            approvals_enabled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    #[tokio::test]
    async fn submit_rejects_concurrent_turns_then_recovers() {
        let rt = Runtime::new(agent_with(Arc::new(SlowProvider(Duration::from_millis(150)))));
        let mut h1 = rt.submit("first").await.unwrap();
        assert!(rt.is_busy());
        let err = match rt.submit("second").await {
            Err(e) => e,
            Ok(_) => panic!("concurrent submit must be rejected"),
        };
        assert!(matches!(err, Error::Busy), "got {err:?}");

        // Drain the first turn; the stream ends only after busy is
        // cleared, so the next submit must succeed immediately.
        while let Some(_ev) = h1.next().await {}
        assert!(!rt.is_busy());
        let mut h3 = rt.submit("third").await.unwrap();
        assert_eq!(h3.turn_id, 2);
        assert!(h3.next().await.is_some());
    }

    #[tokio::test]
    async fn cancel_terminates_run_with_terminal_events() {
        let rt = Runtime::new(agent_with(Arc::new(SlowProvider(Duration::from_secs(30)))));
        let mut h = rt.submit("long").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        rt.cancel().await;

        // Stream ends with Error(cancelled) followed by TurnCompleted.
        let mut tail = Vec::new();
        while let Some(ev) = h.next().await {
            tail.push(ev);
            if tail.len() > 2 {
                tail.remove(0);
            }
        }
        assert!(matches!(tail.last(), Some(AgentEvent::TurnCompleted { .. })));
        assert!(
            tail.iter()
                .any(|e| matches!(e, AgentEvent::Error { message } if message.contains("cancel"))),
            "cancellation must surface as Error: {tail:?}"
        );
        assert!(!rt.is_busy());
    }
}
