use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{mpsc, Mutex};

use crate::compact;
use crate::context::ContextManager;
use crate::error::{Error, Result};
use crate::event::AgentEvent;
use crate::message::{Message, ToolCall, Usage};
use crate::registry::Registry;
use crate::session::{PermissionPolicy, SessionStore};
use crate::traits::{ModelProvider, Tool, ToolCallbacks};

/// Wires the agent together. All collaborators come in as traits; the loop
/// knows nothing about TUI, HTTP, or SQLite.
pub struct Agent {
    pub provider: Arc<dyn ModelProvider>,
    pub tools: Registry<dyn Tool>,
    pub store: Arc<dyn SessionStore>,
    pub permissions: Arc<dyn PermissionPolicy>,
    pub context: Mutex<ContextManager>,
    pub api_key: String,
    /// Model name sent with every request (e.g. "deepseek-flash").
    pub model: String,
    pub temperature: Option<f64>,
    pub max_tokens: u32,
    /// Session id for persistence; None = ephemeral.
    pub session_id: Option<uuid::Uuid>,
}

impl Agent {
    /// Run one user turn to completion: request -> stream events ->
    /// execute tool calls -> repeat until the model produces a final
    /// answer with no pending tool calls. Auto-compaction is checked
    /// pre-turn and after every tool output lands in history.
    pub async fn run_turn(
        &self,
        user_input: &str,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<String> {
        let send = |ev: AgentEvent| -> bool {
            events.send(ev).is_ok()
        };

        {
            let cm = self.context.lock().await;
            let (used, limit) = cm.token_status();
            send(AgentEvent::TokenCountUpdated { used, limit });
        }

        // Pre-turn compaction.
        self.maybe_compact(&events).await?;

        self.context.lock().await.push(Message::user(user_input));
        self.persist_new_items().await;

        send(AgentEvent::TurnStarted);

        let mut turn_usage = Usage::default();
        #[allow(unused_assignments)]
        let mut final_text = String::new();

        loop {
            // ---- one model request ----
            let (history, tool_specs) = {
                let cm = self.context.lock().await;
                let specs = self
                    .tools
                    .all()
                    .iter()
                    .map(|t| crate::traits::ToolSpec {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters: t.parameters_schema(),
                    })
                    .collect::<Vec<_>>();
                (cm.history.snapshot(), specs)
            };

            let req = crate::traits::ModelRequest {
                messages: history,
                tools: tool_specs,
                model: self.model.clone(),
                temperature: self.temperature,
                max_tokens: self.max_tokens,
                stream_reasoning: true,
            };

            let stream = self
                .provider
                .stream(req, &self.api_key)
                .await
                .map_err(|e| {
                    send(AgentEvent::Error { message: e.to_string() });
                    e
                })?;
            let mut stream = stream;

            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();

            while let Some(ev) = stream.next().await {
                match ev {
                    crate::traits::ProviderEvent::MessageDelta { delta } => {
                        text.push_str(&delta);
                        send(AgentEvent::MessageDelta { delta });
                    }
                    crate::traits::ProviderEvent::ReasoningDelta { delta } => {
                        send(AgentEvent::ReasoningDelta { delta });
                    }
                    crate::traits::ProviderEvent::ToolCallFragment { .. } => {
                        // Providers reassemble fragments before the loop
                        // sees them; defensive no-op.
                    }
                    crate::traits::ProviderEvent::ToolCalls { calls: c } => {
                        calls.extend(c);
                    }
                    crate::traits::ProviderEvent::Usage { usage } => {
                        turn_usage = usage;
                        self.context.lock().await.record_usage(usage);
                        let cm = self.context.lock().await;
                        let (used, limit) = cm.token_status();
                        drop(cm);
                        send(AgentEvent::TokenCountUpdated { used, limit });
                    }
                    crate::traits::ProviderEvent::ProviderError { message } => {
                        send(AgentEvent::Error { message: message.clone() });
                        send(AgentEvent::TurnCompleted { usage: turn_usage });
                        return Err(Error::Provider(message));
                    }
                    crate::traits::ProviderEvent::Done => break,
                }
            }

            // Record the assistant message.
            let assistant = Message::Assistant {
                content: text.clone(),
                reasoning: None,
                tool_calls: calls.clone(),
            };
            self.context.lock().await.push(assistant);
            self.persist_new_items().await;

            if calls.is_empty() {
                final_text = text;
                break;
            }

            // ---- execute each tool call ----
            for call in &calls {
                let args_json = call.arguments.to_string();
                if !self.permissions.approve(&call.name, &call.arguments).await {
                    let refusal = format!("Permission denied for tool {0}", call.name);
                    self.context.lock().await.push(Message::ToolResult {
                        tool_call_id: call.id.clone(),
                        content: refusal.clone(),
                        is_error: true,
                    });
                    self.persist_new_items().await;
                    send(AgentEvent::ToolCallCompleted {
                        call_id: call.id.clone(),
                        exit_code: None,
                        timed_out: false,
                        duration_ms: 0,
                        output: refusal,
                    });
                    continue;
                }

                send(AgentEvent::ToolCallStarted {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    command: args_json.clone(),
                });

                let started = std::time::Instant::now();
                let emitter = EventEmitter {
                    call_id: call.id.clone(),
                    events: events.clone(),
                };
                let result = match self.tools.get(&call.name) {
                    Some(tool) => tool
                        .execute(&call.id, call.arguments.clone(), &emitter)
                        .await,
                    None => Err(Error::Tool(format!("unknown tool: {}", call.name))),
                };

                let (content, is_error) = match result {
                    Ok(out) => {
                        send(AgentEvent::ToolCallCompleted {
                            call_id: call.id.clone(),
                            exit_code: out.exit_code,
                            timed_out: out.timed_out,
                            duration_ms: out.duration_ms,
                            output: out.content.clone(),
                        });
                        let _ = started;
                        (out.content, false)
                    }
                    Err(e) => {
                        let msg = format!("tool error: {e}");
                        send(AgentEvent::ToolCallCompleted {
                            call_id: call.id.clone(),
                            exit_code: None,
                            timed_out: false,
                            duration_ms: started.elapsed().as_millis() as u64,
                            output: msg.clone(),
                        });
                        (msg, true)
                    }
                };

                // Truncate to the configured per-output budget before it
                // enters history (codex TruncationPolicy::Bytes).
                let budget = {
                    let cm = self.context.lock().await;
                    cm.config().tool_output_max_bytes
                };
                let content = compact::truncate_middle_bytes(&content, budget);

                self.context.lock().await.push(Message::ToolResult {
                    tool_call_id: call.id.clone(),
                    content,
                    is_error,
                });
                self.persist_new_items().await;

                // Mid-turn compaction check after every tool output
                // (avoids the codex #16033 between-turns-only bug).
                self.maybe_compact(&events).await?;
            }
        }

        send(AgentEvent::TurnCompleted { usage: turn_usage });
        self.persist_new_items().await;
        Ok(final_text)
    }

    /// Manual compaction entry (/compact): compact now regardless of the
    /// auto threshold. No-op on an empty history.
    pub async fn compact_manual(
        &self,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<(i64, i64)> {
        if self.context.lock().await.history.is_empty() {
            return Ok((0, 0));
        }
        let provider = self.provider.clone();
        let (before, after) = compact::run_compaction(
            &mut *self.context.lock().await,
            provider,
            &self.api_key,
            &self.model,
            self.temperature,
            self.max_tokens,
            events,
        )
        .await?;
        if let Some(sid) = self.session_id {
            let msgs = self.context.lock().await.history.snapshot();
            self.store
                .replace_messages(sid, &msgs)
                .await
                .map_err(|e| Error::Storage(e.to_string()))?;
        }
        Ok((before, after))
    }

    async fn maybe_compact(
        &self,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<()> {
        let needed = {
            let cm = self.context.lock().await;
            cm.should_compact()
        };
        if !needed {
            return Ok(());
        }
        let provider = self.provider.clone();
        let (before, after) = compact::run_compaction(
            &mut *self.context.lock().await,
            provider,
            &self.api_key,
            &self.model,
            self.temperature,
            self.max_tokens,
            events,
        )
        .await?;
        // Replace persisted transcript to match compacted history.
        if let Some(sid) = self.session_id {
            let msgs = self.context.lock().await.history.snapshot();
            self.store
                .replace_messages(sid, &msgs)
                .await
                .map_err(|e| Error::Storage(e.to_string()))?;
        }
        let _ = (before, after);
        Ok(())
    }

    /// Persist items not yet saved. We track by comparing against the last
    /// persisted count stored in session meta.
    async fn persist_new_items(&self) {
        if let Some(sid) = self.session_id {
            let msgs = self.context.lock().await.history.snapshot();
            if let Err(e) = self.store.replace_messages(sid, &msgs).await {
                tracing::error!("persist failed: {e}");
            }
        }
    }
}

/// Bridges tool progress into AgentEvents.
struct EventEmitter {
    call_id: String,
    events: mpsc::UnboundedSender<AgentEvent>,
}

#[async_trait::async_trait]
impl ToolCallbacks for EventEmitter {
    async fn output_delta(&self, call_id: &str, chunk: String) {
        let _ = self.events.send(AgentEvent::ToolCallOutputDelta {
            call_id: call_id.to_string(),
            chunk,
        });
        let _ = &self.call_id;
    }
}
