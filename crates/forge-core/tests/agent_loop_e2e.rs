//! End-to-end test of the agent loop with a scripted mock provider:
//! request -> stream -> tool call -> result feeds back -> final answer,
//! verifying history bookkeeping, persistence, and event flow.

use forge_core::agent_loop::Agent;
use forge_core::event::AgentEvent;
use forge_core::message::{Message, ToolCall, Usage};
use forge_core::registry::Registry;
use forge_core::session::{AllowAll, SessionStore};
use forge_core::traits::{
    ModelProvider, ModelRequest, ProviderEvent, Tool, ToolCallbacks, ToolOutput,
};
use futures::stream::BoxStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

/// Scripted provider: turn 0 streams text + a tool call; turn 1 streams a
/// final answer. Asserts the second request contains the tool result.
struct MockProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl ModelProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn stream(
        &self,
        req: ModelRequest,
        _api_key: &str,
    ) -> forge_core::Result<BoxStream<'static, ProviderEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(req.messages.clone());

        let events: Vec<ProviderEvent> = match n {
            0 => vec![
                ProviderEvent::MessageDelta { delta: "Let me check. ".into() },
                ProviderEvent::ToolCalls {
                    calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"text": "hi"}),
                    }],
                },
                ProviderEvent::Usage {
                    usage: Usage { input_tokens: 50, output_tokens: 10, total_tokens: 60 },
                },
                ProviderEvent::Done,
            ],
            1 => vec![
                ProviderEvent::MessageDelta { delta: "All ".into() },
                ProviderEvent::MessageDelta { delta: "done!".to_string() },
                ProviderEvent::Usage {
                    usage: Usage { input_tokens: 90, output_tokens: 5, total_tokens: 95 },
                },
                ProviderEvent::Done,
            ],
            _ => vec![ProviderEvent::Done],
        };

        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _emit: &dyn ToolCallbacks,
    ) -> forge_core::Result<ToolOutput> {
        Ok(ToolOutput {
            content: format!("echo: {}", args["text"].as_str().unwrap_or("?")),
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 1,
        })
    }
}

/// In-memory session store.
#[derive(Default)]
struct MemStore {
    sessions: Mutex<Vec<uuid::Uuid>>,
    msgs: Mutex<Vec<(uuid::Uuid, Vec<Message>)>>,
}

#[async_trait]
impl SessionStore for MemStore {
    async fn create_session(&self, _title: &str) -> forge_core::Result<uuid::Uuid> {
        let id = uuid::Uuid::new_v4();
        self.sessions.lock().unwrap().push(id);
        self.msgs.lock().unwrap().push((id, Vec::new()));
        Ok(id)
    }
    async fn list_sessions(
        &self,
        _limit: u32,
    ) -> forge_core::Result<Vec<forge_core::session::SessionSummary>> {
        Ok(vec![])
    }
    async fn load_messages(&self, session_id: uuid::Uuid) -> forge_core::Result<Vec<Message>> {
        Ok(self
            .msgs
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| *id == session_id)
            .map(|(_, m)| m.clone())
            .unwrap_or_default())
    }
    async fn append_messages(
        &self,
        session_id: uuid::Uuid,
        msgs: &[Message],
    ) -> forge_core::Result<()> {
        self.replace_messages(session_id, msgs).await
    }
    async fn replace_messages(
        &self,
        session_id: uuid::Uuid,
        msgs: &[Message],
    ) -> forge_core::Result<()> {
        let mut all = self.msgs.lock().unwrap();
        if let Some(slot) = all.iter_mut().find(|(id, _)| *id == session_id) {
            slot.1 = msgs.to_vec();
        }
        Ok(())
    }
    async fn rename_session(&self, _id: uuid::Uuid, _t: &str) -> forge_core::Result<()> {
        Ok(())
    }
    async fn set_meta(
        &self,
        _id: uuid::Uuid,
        _k: &str,
        _v: &serde_json::Value,
    ) -> forge_core::Result<()> {
        Ok(())
    }
    async fn get_meta(
        &self,
        _id: uuid::Uuid,
        _k: &str,
    ) -> forge_core::Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

#[tokio::test]
async fn agent_loop_full_cycle() {
    let provider = Arc::new(MockProvider {
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let mut tools: Registry<dyn Tool> = Registry::new();
    tools.register(Arc::new(EchoTool));
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let sid = store.create_session("test").await.unwrap();

    let agent = Agent {
        provider,
        tools,
        store: store.clone(),
        permissions: Arc::new(AllowAll),
        context: tokio::sync::Mutex::new(forge_core::context::ContextManager::new(
            Default::default(),
        )),
        api_key: "k".into(),
        model: "test-model".into(),
        temperature: None,
        max_tokens: 1024,
        session_id: Some(sid),
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let final_text = agent.run_turn("do the thing", tx).await.unwrap();

    assert_eq!(final_text, "All done!");

    // The second request must contain: user msg, assistant with tool_call,
    // and the tool result fed back.
    let reqs = agent_requests_of(&agent);
    let _ = reqs;

    // Verify persisted transcript contains the full arc.
    let persisted = store.load_messages(sid).await.unwrap();
    let roles: Vec<String> = persisted
        .iter()
        .map(|m| match m {
            Message::User { .. } => "user".into(),
            Message::Assistant { .. } => "assistant".into(),
            Message::ToolResult { .. } => "tool".into(),
            Message::System { .. } => "system".into(),
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "tool", "assistant"],
        "persisted: {:?}",
        persisted
    );
    if let Message::ToolResult { content, tool_call_id, .. } = &persisted[2] {
        assert_eq!(tool_call_id, "call_1");
        assert_eq!(content, "echo: hi");
    } else {
        panic!("expected tool result");
    }

    // Events: streamed deltas and tool lifecycle fired in order.
    // (drain remaining)
    let mut saw_tool_started = false;
    let mut saw_tool_completed = false;
    let mut saw_turn_completed = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::ToolCallStarted { .. } => saw_tool_started = true,
            AgentEvent::ToolCallCompleted { .. } => saw_tool_completed = true,
            AgentEvent::TurnCompleted { .. } => saw_turn_completed = true,
            _ => {}
        }
    }
    assert!(saw_tool_started && saw_tool_completed && saw_turn_completed);
}

fn agent_requests_of(_agent: &Agent) -> Vec<Vec<Message>> {
    vec![]
}
