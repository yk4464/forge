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
/// final answer. Records every request so tests can assert the second
/// request actually contains the tool result fed back.
struct MockProvider {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    script: Mutex<Vec<ScriptedTurn>>,
}

type ScriptedTurn = Box<dyn FnOnce() -> forge_core::Result<Vec<ProviderEvent>> + Send>;

impl MockProvider {
    fn new(script: Vec<ScriptedTurn>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            requests: Arc::new(Mutex::new(Vec::new())),
            script: Mutex::new(script),
        }
    }
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
        let turn = self
            .script
            .lock()
            .unwrap()
            .get_mut(n)
            .map(|f| std::mem::replace(f, Box::new(|| Ok(vec![])))());
        match turn {
            Some(events) => Ok(Box::pin(futures::stream::iter(events?))),
            None => Ok(Box::pin(futures::stream::iter(vec![ProviderEvent::Done]))),
        }
    }
}

/// A default two-request script: tool call, then final answer.
fn two_turn_script() -> Vec<ScriptedTurn> {
    vec![
        Box::new(|| {
            Ok(vec![
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
            ])
        }),
        Box::new(|| {
            Ok(vec![
                ProviderEvent::MessageDelta { delta: "All ".into() },
                ProviderEvent::MessageDelta { delta: "done!".to_string() },
                ProviderEvent::Usage {
                    usage: Usage { input_tokens: 90, output_tokens: 5, total_tokens: 95 },
                },
                ProviderEvent::Done,
            ])
        }),
    ]
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

/// In-memory session store. Counts replace calls so tests can assert that
/// a readonly agent never overwrites the stored transcript.
#[derive(Default)]
struct MemStore {
    sessions: Mutex<Vec<uuid::Uuid>>,
    msgs: Mutex<Vec<(uuid::Uuid, Vec<Message>)>>,
    replaces: AtomicUsize,
    load_fails: std::sync::atomic::AtomicBool,
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
        if self.load_fails.load(Ordering::SeqCst) {
            return Err(forge_core::Error::Storage("simulated corrupt payload".into()));
        }
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
        self.replaces.fetch_add(1, Ordering::SeqCst);
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

struct DenyAll;

#[async_trait]
impl forge_core::session::PermissionPolicy for DenyAll {
    async fn approve(&self, _tool: &str, _args: &serde_json::Value) -> bool {
        false
    }
}

fn test_agent(
    provider: Arc<dyn ModelProvider>,
    store: Arc<dyn SessionStore>,
    permissions: Arc<dyn forge_core::session::PermissionPolicy>,
    session_id: Option<uuid::Uuid>,
) -> Agent {
    Agent {
        provider,
        tools: {
            let mut r: Registry<dyn Tool> = Registry::new();
            r.register(Arc::new(EchoTool));
            r
        },
        store,
        permissions,
        context: tokio::sync::Mutex::new(forge_core::context::ContextManager::new(
            Default::default(),
        )),
        api_key: "k".into(),
        model: "test-model".into(),
        temperature: None,
        max_tokens: 1024,
        session_id,
    }
}

#[tokio::test]
async fn agent_loop_full_cycle() {
    let mock = Arc::new(MockProvider::new(two_turn_script()));
    let provider: Arc<dyn ModelProvider> = mock.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let sid = store.create_session("test").await.unwrap();
    let agent = test_agent(provider, store.clone(), Arc::new(AllowAll), Some(sid));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let final_text = agent.run_turn("do the thing", tx).await.unwrap();

    assert_eq!(final_text, "All done!");

    // The second request must contain: user msg, assistant with tool_call,
    // and the tool result fed back.
    let reqs = mock.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].len(), 3, "second request: {:?}", reqs[1]);
    assert!(matches!(&reqs[1][0], Message::User { content } if content == "do the thing"));
    match &reqs[1][1] {
        Message::Assistant { content, tool_calls, .. } => {
            assert_eq!(content, "Let me check. ");
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_1");
        }
        other => panic!("expected assistant with tool call, got {other:?}"),
    }
    match &reqs[1][2] {
        Message::ToolResult { tool_call_id, content, is_error } => {
            assert_eq!(tool_call_id, "call_1");
            assert_eq!(content, "echo: hi");
            assert!(!*is_error);
        }
        other => panic!("expected tool result, got {other:?}"),
    }

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

    // Events: streamed deltas and tool lifecycle fired in order; usage is
    // CUMULATIVE for the turn (60 + 95), matching the event contract.
    let mut saw_tool_started = false;
    let mut saw_tool_completed = false;
    let mut turn_usage = None;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::ToolCallStarted { .. } => saw_tool_started = true,
            AgentEvent::ToolCallCompleted { .. } => saw_tool_completed = true,
            AgentEvent::TurnCompleted { usage } => turn_usage = Some(usage),
            _ => {}
        }
    }
    assert!(saw_tool_started && saw_tool_completed);
    let usage = turn_usage.expect("TurnCompleted event");
    assert_eq!(usage.total_tokens, 60 + 95, "usage must accumulate");
}

#[tokio::test]
async fn permission_denial_emits_paired_events() {
    let mock = Arc::new(MockProvider::new(two_turn_script()));
    let provider: Arc<dyn ModelProvider> = mock.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let sid = store.create_session("deny").await.unwrap();
    let agent = test_agent(provider, store.clone(), Arc::new(DenyAll), Some(sid));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn("try the tool", tx).await.unwrap();

    // Every Started must be paired with a Completed; the old denial path
    // emitted only Completed, which UI consumers silently dropped.
    let mut started = Vec::new();
    let mut completed = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::ToolCallStarted { call_id, .. } => started.push(call_id),
            AgentEvent::ToolCallCompleted { call_id, output, .. } => {
                assert!(output.contains("Permission denied"));
                completed.push(call_id);
            }
            _ => {}
        }
    }
    assert_eq!(started, completed, "events must pair up");
    assert_eq!(started, vec!["call_1".to_string()]);

    // The refusal lands in history as an error tool result.
    let persisted = store.load_messages(sid).await.unwrap();
    assert!(persisted.iter().any(|m| matches!(
        m,
        Message::ToolResult { is_error: true, .. }
    )));
}

#[tokio::test]
async fn empty_response_is_not_persisted() {
    // Turn 0: tool call; turn 1: stream ends with no text and no calls.
    // The old loop persisted an empty assistant shell, which providers
    // (Anthropic) later reject and which replays forever.
    let mock = Arc::new(MockProvider::new(vec![
        Box::new(|| {
            Ok(vec![
                ProviderEvent::ToolCalls {
                    calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "echo".into(),
                        arguments: serde_json::json!({"text": "hi"}),
                    }],
                },
                ProviderEvent::Done,
            ])
        }),
        Box::new(|| Ok(vec![ProviderEvent::Done])),
    ]));
    let provider: Arc<dyn ModelProvider> = mock.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let sid = store.create_session("empty").await.unwrap();
    let agent = test_agent(provider, store.clone(), Arc::new(AllowAll), Some(sid));

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let final_text = agent.run_turn("go", tx).await.unwrap();
    assert_eq!(final_text, "");

    let persisted = store.load_messages(sid).await.unwrap();
    let roles: Vec<&str> = persisted
        .iter()
        .map(|m| match m {
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::ToolResult { .. } => "tool",
            Message::System { .. } => "system",
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "tool"],
        "no empty assistant shell after an empty stream: {:?}",
        persisted
    );
}

#[tokio::test]
async fn provider_failure_still_emits_terminal_events() {
    // stream() always fails. run_turn must return Err AND the event
    // channel must end with Error + TurnCompleted so the UI can never
    // stay stuck in the busy state.
    struct FailingProvider;
    #[async_trait]
    impl ModelProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        async fn stream(
            &self,
            _req: ModelRequest,
            _api_key: &str,
        ) -> forge_core::Result<BoxStream<'static, ProviderEvent>> {
            Err(forge_core::Error::Provider("connection refused".into()))
        }
    }
    let provider: Arc<dyn ModelProvider> = Arc::new(FailingProvider);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let agent = test_agent(provider, store.clone(), Arc::new(AllowAll), None);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = agent.run_turn("hi", tx).await;
    assert!(result.is_err());

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { .. })),
        "Err path must emit an Error event, got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnCompleted { .. })),
        "the last event must be TurnCompleted, got {:?}",
        events.last()
    );
}

#[tokio::test]
async fn context_overflow_compacts_and_retries_once() {
    // Turn 0: the request overflows. The loop must compact (which uses
    // the provider to summarize) and retry exactly once; the retried
    // request then answers normally.
    let mock = Arc::new(MockProvider::new(vec![
        Box::new(|| {
            Err(forge_core::Error::ContextWindowExceeded { used: 999, limit: 100 })
        }),
        // Compaction summarization request.
        Box::new(|| {
            Ok(vec![
                ProviderEvent::MessageDelta { delta: "summary of the task so far".into() },
                ProviderEvent::Done,
            ])
        }),
        // Retried request after compaction.
        Box::new(|| {
            Ok(vec![
                ProviderEvent::MessageDelta { delta: "recovered".into() },
                ProviderEvent::Done,
            ])
        }),
    ]));
    let provider: Arc<dyn ModelProvider> = mock.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::default());
    let sid = store.create_session("overflow").await.unwrap();
    let agent = test_agent(provider, store.clone(), Arc::new(AllowAll), Some(sid));
    // The system rules live in history (main.rs pushes them at index 0);
    // they must survive the compaction round-trip.
    agent
        .context
        .lock()
        .await
        .push(Message::system("stay terse; never force-push"));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let final_text = agent.run_turn("long task", tx).await.unwrap();
    assert_eq!(final_text, "recovered");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 3, "overflow + summarize + retry");

    // The retried request (index 2) must still carry the system rules —
    // compaction pins them instead of summarizing them away.
    let reqs = mock.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 3);
    assert!(
        matches!(&reqs[2][0], Message::System { content } if content.contains("never force-push")),
        "system rules lost after compaction: {:?}",
        reqs[2].first()
    );

    let mut saw_compaction = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::CompactionCompleted { .. }) {
            saw_compaction = true;
        }
    }
    assert!(saw_compaction, "compaction must run during the retry");
}
