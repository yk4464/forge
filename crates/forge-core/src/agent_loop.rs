use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    /// Per-turn budget limits and loop-detection threshold.
    pub budget: crate::config::BudgetConfig,
    /// Session id for persistence; None = ephemeral.
    pub session_id: Option<uuid::Uuid>,
    /// How many history items are already stored. Growth appends; a
    /// shrunken history (compaction) triggers one full replace.
    pub persisted_len: AtomicUsize,
    /// Set after compaction replaces the history object: the next persist
    /// must rewrite the transcript (content changed even if the length
    /// did not shrink), and only a successful rewrite clears it.
    pub history_replaced: AtomicBool,
    /// Set while transcript writes are failing, so the user gets one
    /// clear warning per failure streak instead of log-line spam.
    pub persist_failed: AtomicBool,
}

impl Agent {
    /// Run one user turn to completion: request -> stream events ->
    /// execute tool calls -> repeat until the model produces a final
    /// answer with no pending tool calls. Auto-compaction is checked
    /// pre-turn and after every tool output lands in history.
    ///
    /// Terminal-state guarantee: whatever the outcome (error, provider
    /// failure, storage failure), an `AgentEvent::Error` is emitted before
    /// `Err` is returned, and `TurnCompleted` closes every finished turn —
    /// so UI consumers never stay stuck in a busy state.
    pub async fn run_turn(
        &self,
        user_input: &str,
        events: mpsc::UnboundedSender<AgentEvent>,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<String> {
        match self.run_turn_inner(user_input, &events, cancel).await {
            Ok((text, usage)) => {
                let _ = events.send(AgentEvent::TurnCompleted { usage });
                Ok(text)
            }
            Err(e) => {
                let _ = events.send(AgentEvent::Error { message: e.to_string() });
                let _ = events.send(AgentEvent::TurnCompleted { usage: Usage::default() });
                Err(e)
            }
        }
    }

    async fn run_turn_inner(
        &self,
        user_input: &str,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<(String, Usage)> {
        let send = |ev: AgentEvent| -> bool {
            events.send(ev).is_ok()
        };

        {
            let cm = self.context.lock().await;
            let (used, limit) = cm.token_status();
            send(AgentEvent::TokenCountUpdated { used, limit });
        }

        // Pre-turn compaction.
        self.maybe_compact(events, cancel).await?;

        self.context.lock().await.push(Message::user(user_input));
        self.persist_state(Some(events)).await;

        send(AgentEvent::TurnStarted);

        let mut turn_usage = Usage::default();
        let mut turn = TurnBudget::new(self.budget.clone());

        // One attempt = build request, stream response, run tools. On a
        // context-window rejection we compact once and retry (bounded).
        let mut overflow_retried = false;
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(reason) = turn.between_attempts_reason(turn_usage.total_tokens) {
                return Err(Error::BudgetExceeded(reason));
            }
            let attempt = self
                .one_model_attempt(events, &mut turn_usage, cancel, &mut turn)
                .await;
            match attempt {
                Ok(Some(text)) => {
                    self.persist_state(Some(events)).await;
                    return Ok((text, turn_usage));
                }
                Ok(None) => {} // tools ran; issue the next model request
                Err(Error::ContextWindowExceeded { .. }) if !overflow_retried => {
                    overflow_retried = true;
                    send(AgentEvent::Warning {
                        message: "context window exceeded; compacting and retrying once".into(),
                    });
                    let provider = self.provider.clone();
                    compact::run_compaction(
                        &mut *self.context.lock().await,
                        provider,
                        &self.api_key,
                        &self.model,
                        self.temperature,
                        self.max_tokens,
                        events,
                    )
                    .await?;
                    // The history object was rebuilt: the next persist must
                    // rewrite the transcript (content changed even when the
                    // compacted history is not shorter).
                    self.history_replaced.store(true, Ordering::Relaxed);
                    self.persist_state(Some(events)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One request -> stream -> (maybe) execute tools cycle. Returns
    /// `Ok(None)` when tools ran and the outer loop should issue another
    /// model request; `Ok(Some(text))` when the model produced a final
    /// answer; `Err` bubbles provider/tool failures.
    async fn one_model_attempt(
        &self,
        events: &mpsc::UnboundedSender<AgentEvent>,
        turn_usage: &mut Usage,
        cancel: &crate::cancel::CancelToken,
        turn: &mut TurnBudget,
    ) -> Result<Option<String>> {
        let send = |ev: AgentEvent| -> bool {
            events.send(ev).is_ok()
        };
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

        let stream = self.provider.stream(req, &self.api_key).await?;
        let mut stream = stream;


        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();

        // Consume the stream, racing cancellation: dropping the stream
        // aborts the in-flight HTTP request.
        loop {
            let ev = tokio::select! {
                ev = stream.next() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
                _ = cancel.cancelled() => return Err(Error::Cancelled),
            };
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
                    // Usage events are cumulative across a turn: each
                    // response reports the whole prompt so far.
                    turn_usage.input_tokens += usage.input_tokens;
                    turn_usage.output_tokens += usage.output_tokens;
                    turn_usage.total_tokens += usage.total_tokens;
                    self.context.lock().await.record_usage(usage);
                    let cm = self.context.lock().await;
                    let (used, limit) = cm.token_status();
                    drop(cm);
                    send(AgentEvent::TokenCountUpdated { used, limit });
                }
                crate::traits::ProviderEvent::ProviderError { message } => {
                    return Err(Error::Provider(message));
                }
                crate::traits::ProviderEvent::Done => break,
            }
        }

        // An assistant response with neither text nor tool calls is an
        // empty shell; keeping it would replay a contentless turn to the
        // provider forever (some endpoints 400 on empty text blocks).
        if text.is_empty() && calls.is_empty() {
            return Ok(Some(String::new()));
        }

        // Record the assistant message.
        let assistant = Message::Assistant {
            content: text.clone(),
            reasoning: None,
            tool_calls: calls.clone(),
        };
        self.context.lock().await.push(assistant);
        self.persist_state(Some(events)).await;

        if calls.is_empty() {
            return Ok(Some(text));
        }

        // ---- execute each tool call ----
        for (call_idx, call) in calls.iter().enumerate() {
            // Cancellation: every started call must get a recorded result
            // (providers reject dangling tool_use), so mark this and all
            // remaining calls, then stop the turn.
            if cancel.is_cancelled() {
                self.record_skipped_results(&calls[call_idx..], "cancelled by user", events)
                    .await;
                return Err(Error::Cancelled);
            }

            let args_json = call.arguments.to_string();
            // Budgets and runaway detection: skip this and the remaining
            // calls with recorded results, then stop with an explanation.
            if let Some(reason) = turn.before_call_reason(&call.name, &args_json) {
                self.record_skipped_results(
                    &calls[call_idx..],
                    &format!("not executed: {reason}"),
                    events,
                )
                .await;
                return Err(Error::BudgetExceeded(reason));
            }
            if !self.permissions.approve(&call.name, &call.arguments).await {
                let refusal = format!("Permission denied for tool {0}", call.name);
                // Started/Completed must stay paired: UI consumers key
                // completed cards off the started event.
                send(AgentEvent::ToolCallStarted {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    command: args_json.clone(),
                });
                self.context.lock().await.push(Message::ToolResult {
                    tool_call_id: call.id.clone(),
                    content: refusal.clone(),
                    is_error: true,
                });
                self.log_tool_finished(&call.id, crate::session::ToolEventState::Failed, &refusal)
                    .await;
                self.persist_state(Some(events)).await;
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
            turn.tool_calls += 1;
            // Execution log (S1): the started record precedes the tool, so
            // recovery can tell "never ran" from "ran, outcome unknown".
            if let Some(sid) = self.session_id {
                if let Err(e) = self
                    .store
                    .record_tool_started(sid, &call.id, &call.name, &call.arguments)
                    .await
                {
                    tracing::warn!("tool event log (started) failed: {e}");
                }
            }

            let started = std::time::Instant::now();
            let emitter = EventEmitter {
                call_id: call.id.clone(),
                events: events.clone(),
            };
            // Race execution against cancellation: dropping the tool
            // future kills the child process (kill_on_drop + Job Object).
            let result = match self.tools.get(&call.name) {
                Some(tool) => {
                    let fut = tool.execute(&call.id, call.arguments.clone(), &emitter);
                    tokio::pin!(fut);
                    tokio::select! {
                        r = &mut fut => r,
                        _ = cancel.cancelled() => Err(Error::Cancelled),
                    }
                }
                None => Err(Error::Tool(format!("unknown tool: {}", call.name))),
            };

            let (content, is_error) = match result {
                Ok(out) => {
                    // Terminal log state first: a crash after this point
                    // lets recovery REUSE the recorded output instead of
                    // guessing (S1 恢复规则: 已完成操作复用记录).
                    self.log_tool_finished(
                        &call.id,
                        crate::session::ToolEventState::Completed,
                        &out.content,
                    )
                    .await;
                    send(AgentEvent::ToolCallCompleted {
                        call_id: call.id.clone(),
                        exit_code: out.exit_code,
                        timed_out: out.timed_out,
                        duration_ms: out.duration_ms,
                        output: out.content.clone(),
                    });
                    (out.content, false)
                }
                Err(Error::Cancelled) => {
                    self.log_tool_finished(
                        &call.id,
                        crate::session::ToolEventState::Failed,
                        "cancelled by user",
                    )
                    .await;
                    send(AgentEvent::ToolCallCompleted {
                        call_id: call.id.clone(),
                        exit_code: None,
                        timed_out: false,
                        duration_ms: started.elapsed().as_millis() as u64,
                        output: "cancelled by user".into(),
                    });
                    ("cancelled by user".to_string(), true)
                }
                Err(e) => {
                    let msg = format!("tool error: {e}");
                    self.log_tool_finished(
                        &call.id,
                        crate::session::ToolEventState::Failed,
                        &msg,
                    )
                    .await;
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
                content: content.clone(),
                is_error,
            });
            self.persist_state(Some(events)).await;

            // Same-error runaway detection: the failed call's result is
            // recorded above; remaining batch calls get skip records.
            if is_error {
                if let Some(reason) = turn.note_error(&call.name, &content) {
                    self.record_skipped_results(
                        &calls[call_idx + 1..],
                        &format!("not executed: {reason}"),
                        events,
                    )
                    .await;
                    return Err(Error::BudgetExceeded(reason));
                }
            } else {
                turn.note_success();
            }

            // Mid-turn compaction check after every tool output
            // (avoids the codex #16033 between-turns-only bug).
            self.maybe_compact(events, cancel).await?;
        }

        Ok(None)
    }

    /// Best-effort write of a terminal tool-execution record. Failures are
    /// logged: the recovery log is auxiliary to the transcript itself.
    async fn log_tool_finished(&self, call_id: &str, state: crate::session::ToolEventState, output: &str) {
        if let Some(sid) = self.session_id {
            if let Err(e) = self
                .store
                .record_tool_finished(sid, call_id, state, output)
                .await
            {
                tracing::warn!("tool event log (finished) failed: {e}");
            }
        }
    }

    /// Record `reason` results for calls that will never run this turn
    /// (budget stop or cancellation), keeping history protocol-valid.
    async fn record_skipped_results(
        &self,
        calls: &[ToolCall],
        reason: &str,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) {
        for c in calls {
            let _ = events.send(AgentEvent::ToolCallStarted {
                call_id: c.id.clone(),
                name: c.name.clone(),
                command: c.arguments.to_string(),
            });
            self.context.lock().await.push(Message::ToolResult {
                tool_call_id: c.id.clone(),
                content: reason.to_string(),
                is_error: true,
            });
            let _ = events.send(AgentEvent::ToolCallCompleted {
                call_id: c.id.clone(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                output: reason.to_string(),
            });
        }
        self.persist_state(Some(events)).await;
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
        // Manual /compact reports storage failure as an error (explicit
        // user command), but still resyncs the append counter on success.
        if let Some(sid) = self.session_id {
            let msgs = self.context.lock().await.history.snapshot();
            self.store
                .replace_messages(sid, &msgs)
                .await
                .map_err(|e| Error::Storage(e.to_string()))?;
            self.persisted_len.store(msgs.len(), Ordering::Relaxed);
        }
        Ok((before, after))
    }

    async fn maybe_compact(
        &self,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &crate::cancel::CancelToken,
    ) -> Result<()> {
        // A cancelled turn must not spend a model round-trip on a summary.
        if cancel.is_cancelled() {
            return Ok(());
        }
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
        // The history object was rebuilt: the next persist must rewrite
        // the transcript (content changed even when the compacted history
        // is not shorter).
        self.history_replaced.store(true, Ordering::Relaxed);
        self.persist_state(Some(events)).await;
        let _ = (before, after);
        Ok(())
    }

    /// Persist the transcript: append-only while history grows, one full
    /// replace when compaction shrank it. Storage failures surface once
    /// per streak as a user-visible Warning — saving must never fail
    /// silently, and recovery is announced too.
    async fn persist_state(
        &self,
        events: Option<&mpsc::UnboundedSender<AgentEvent>>,
    ) {
        let Some(sid) = self.session_id else { return };
        let msgs = self.context.lock().await.history.snapshot();
        let persisted = self.persisted_len.load(Ordering::Relaxed);
        let force_replace = self.history_replaced.load(Ordering::Relaxed);
        let result = if !force_replace && msgs.len() >= persisted {
            self.store.append_messages(sid, &msgs[persisted..]).await
        } else {
            self.store.replace_messages(sid, &msgs).await
        };
        match result {
            Ok(()) => {
                self.persisted_len.store(msgs.len(), Ordering::Relaxed);
                self.history_replaced.store(false, Ordering::Relaxed);
                if self.persist_failed.swap(false, Ordering::Relaxed) {
                    if let Some(ev) = events {
                        let _ = ev.send(AgentEvent::Warning {
                            message: "transcript saving recovered".into(),
                        });
                    }
                }
            }
            Err(e) => {
                tracing::error!("persist failed: {e}");
                if !self.persist_failed.swap(true, Ordering::Relaxed) {
                    if let Some(ev) = events {
                        let _ = ev.send(AgentEvent::Warning {
                            message: format!(
                                "transcript saving failed: {e} — the conversation continues but is not being saved"
                            ),
                        });
                    }
                }
            }
        }
    }
}

/// Per-turn budget state: wall clock, executed tool calls, and the
/// consecutive-identical streaks used for runaway detection.
struct TurnBudget {
    cfg: crate::config::BudgetConfig,
    started_at: std::time::Instant,
    tool_calls: u32,
    last_call: Option<(String, String)>,
    call_streak: u32,
    last_error: Option<(String, String)>,
    error_streak: u32,
}

impl TurnBudget {
    fn new(cfg: crate::config::BudgetConfig) -> Self {
        Self {
            cfg,
            started_at: std::time::Instant::now(),
            tool_calls: 0,
            last_call: None,
            call_streak: 0,
            last_error: None,
            error_streak: 0,
        }
    }

    /// Why the turn must stop before issuing the next model request.
    fn between_attempts_reason(&self, tokens: i64) -> Option<String> {
        if self.cfg.max_turn_seconds > 0
            && self.started_at.elapsed().as_secs() >= self.cfg.max_turn_seconds
        {
            return Some(format!(
                "turn time budget reached ({}s); send a new message to continue",
                self.cfg.max_turn_seconds
            ));
        }
        if self.cfg.max_turn_tokens > 0 && tokens >= self.cfg.max_turn_tokens {
            return Some(format!(
                "turn token budget reached (~{tokens} tokens); send a new message to continue"
            ));
        }
        None
    }

    /// Why `call` must not execute (checked before each tool call). Also
    /// updates the identical-call streak.
    fn before_call_reason(&mut self, name: &str, args_json: &str) -> Option<String> {
        if self.cfg.max_tool_calls > 0 && self.tool_calls >= self.cfg.max_tool_calls {
            return Some(format!(
                "tool call budget reached ({} calls this turn); send a new message to continue",
                self.cfg.max_tool_calls
            ));
        }
        if self.cfg.loop_threshold > 0 {
            let same = self
                .last_call
                .as_ref()
                .map(|(n, a)| n == name && a == args_json)
                .unwrap_or(false);
            self.call_streak = if same { self.call_streak + 1 } else { 1 };
            self.last_call = Some((name.to_string(), args_json.to_string()));
            if self.call_streak > self.cfg.loop_threshold {
                return Some(format!(
                    "tool loop detected: {name} ran with identical arguments {} times; turn stopped",
                    self.call_streak - 1
                ));
            }
        }
        None
    }

    /// Why the turn must stop after a tool just failed identically again.
    fn note_error(&mut self, name: &str, msg: &str) -> Option<String> {
        if self.cfg.loop_threshold == 0 {
            return None;
        }
        let same = self
            .last_error
            .as_ref()
            .map(|(n, m)| n == name && m == msg)
            .unwrap_or(false);
        self.error_streak = if same { self.error_streak + 1 } else { 1 };
        self.last_error = Some((name.to_string(), msg.to_string()));
        if self.error_streak > self.cfg.loop_threshold {
            return Some(format!(
                "tool loop detected: {name} failed with the same error {} times; turn stopped",
                self.error_streak - 1
            ));
        }
        None
    }

    fn note_success(&mut self) {
        self.error_streak = 0;
        self.last_error = None;
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
