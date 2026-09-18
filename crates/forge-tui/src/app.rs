use chrono::Local;
use forge_core::event::AgentEvent;
use forge_core::session::SessionStore as _;
use forge_storage::SqliteSessionStore;


/// A rendered line in the transcript area.
#[derive(Debug, Clone)]
pub enum Line {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool { command: String, output: String, done: bool },
    System(String),
    Warn(String),
    Error(String),
}

/// What the app does after the user submits input.
pub enum CommandAction {
    /// Send the text to the agent as a task.
    Send(String),
    /// Start a new session.
    NewSession,
    /// Open the session picker.
    Resume,
    /// Trigger manual compaction.
    Compact,
    /// Quit.
    Exit,
    /// Not a command; ignored (e.g. "/help").
    None,
}

pub struct App {
    pub lines: Vec<Line>,
    pub input: String,
    pub busy: bool,
    pub session_id: Option<uuid::Uuid>,
    pub session_title: String,
    pub scroll: u16,
    /// Live buffers for the in-flight turn.
    pub streaming_assistant: String,
    pub streaming_reasoning: String,
    /// Open tool calls still receiving output, keyed by call_id.
    pub open_tools: std::collections::HashMap<String, (String, String)>,
    pub status: String,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            input: String::new(),
            busy: false,
            session_id: None,
            session_title: String::from("(new session)"),
            scroll: 0,
            streaming_assistant: String::new(),
            streaming_reasoning: String::new(),
            open_tools: std::collections::HashMap::new(),
            status: String::from("ready"),
            should_quit: false,
        }
    }

    pub fn push_line(&mut self, line: Line) {
        self.lines.push(line);
        self.scroll = 0;
    }

    /// Handle one event from the agent loop. Returns Some(summary text)
    /// when a turn completes and the session needs a title.
    pub fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStarted => {
                self.busy = true;
                self.streaming_assistant.clear();
                self.streaming_reasoning.clear();
                self.status = "thinking…".into();
            }
            AgentEvent::MessageDelta { delta } => {
                self.streaming_assistant.push_str(&delta);
            }
            AgentEvent::ReasoningDelta { delta } => {
                self.streaming_reasoning.push_str(&delta);
            }
            AgentEvent::ToolCallStarted { call_id, name: _, command } => {
                // Flush any streamed assistant text before the tool card.
                self.flush_streaming();
                self.open_tools.insert(call_id, (command, String::new()));
            }
            AgentEvent::ToolCallOutputDelta { call_id, chunk } => {
                if let Some((_, out)) = self.open_tools.get_mut(&call_id) {
                    out.push_str(&chunk);
                }
            }
            AgentEvent::ToolCallCompleted { call_id, output, .. } => {
                if let Some((command, live)) = self.open_tools.remove(&call_id) {
                    let final_out = if output.trim().is_empty() { live } else { output };
                    self.push_line(Line::Tool {
                        command,
                        output: final_out,
                        done: true,
                    });
                }
            }
            AgentEvent::TokenCountUpdated { used, limit } => {
                self.status = format!("tokens ~{used}/{limit}");
            }
            AgentEvent::CompactionStarted => {
                self.push_line(Line::System("compacting context…".into()));
            }
            AgentEvent::CompactionCompleted { tokens_before, tokens_after } => {
                self.push_line(Line::System(format!(
                    "compaction done: ~{tokens_before} → ~{tokens_after} tokens"
                )));
            }
            AgentEvent::Warning { message } => {
                self.push_line(Line::Warn(message));
            }
            AgentEvent::TurnCompleted { .. } => {
                self.busy = false;
                self.flush_streaming();
                self.status = "ready".into();
            }
            AgentEvent::Error { message } => {
                self.push_line(Line::Error(message));
                self.busy = false;
                self.status = "error".into();
            }
        }
    }

    /// Move streamed text into transcript lines.
    fn flush_streaming(&mut self) {
        if !self.streaming_reasoning.is_empty() {
            let r = std::mem::take(&mut self.streaming_reasoning);
            self.push_line(Line::Reasoning(r));
        }
        if !self.streaming_assistant.is_empty() {
            let a = std::mem::take(&mut self.streaming_assistant);
            self.push_line(Line::Assistant(a));
        }
    }

    /// Parse user input; returns the action to take.
    pub fn on_submit(&mut self) -> CommandAction {
        let text = self.input.trim().to_string();
        self.input.clear();
        if text.is_empty() {
            return CommandAction::None;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            let mut parts = cmd.splitn(2, ' ');
            match parts.next() {
                Some("new") => return CommandAction::NewSession,
                Some("resume") => return CommandAction::Resume,
                Some("compact") => return CommandAction::Compact,
                Some("exit") | Some("quit") => return CommandAction::Exit,
                Some("help") => {
                    self.push_line(Line::System(
                        "commands: /new /resume /compact /exit".into(),
                    ));
                    return CommandAction::None;
                }
                _ => {
                    self.push_line(Line::Warn(format!("unknown command: /{cmd}")));
                    return CommandAction::None;
                }
            }
        }
        self.push_line(Line::User(text.clone()));
        CommandAction::Send(text)
    }

    /// Load a session from the store into the transcript.
    pub async fn load_session(&mut self, store: &SqliteSessionStore, id: uuid::Uuid) {
        match store.load_messages(id).await {
            Ok(msgs) => {
                self.lines.clear();
                for m in msgs {
                    match m {
                        forge_core::message::Message::User { content } => {
                            self.push_line(Line::User(content));
                        }
                        forge_core::message::Message::Assistant { content, reasoning, tool_calls } => {
                            if let Some(r) = reasoning {
                                if !r.is_empty() {
                                    self.push_line(Line::Reasoning(r));
                                }
                            }
                            if !content.is_empty() {
                                self.push_line(Line::Assistant(content));
                            }
                            for _c in tool_calls {
                                // Tool results follow as ToolResult messages.
                            }
                        }
                        forge_core::message::Message::ToolResult { content, is_error, .. } => {
                            self.push_line(if is_error {
                                Line::Error(content)
                            } else {
                                Line::Tool { command: "(tool result)".into(), output: content, done: true }
                            });
                        }
                        forge_core::message::Message::System { .. } => {}
                    }
                }
                self.session_id = Some(id);
                self.session_title = format!("session {id}");
                self.status = "resumed".into();
            }
            Err(e) => self.push_line(Line::Error(format!("load failed: {e}"))),
        }
    }

    /// Auto-title from the first user message.
    pub fn maybe_title(&self, first_user_text: &str) -> String {
        let t: String = first_user_text.chars().take(40).collect();
        if t.is_empty() {
            format!("session {}", Local::now().format("%m-%d %H:%M"))
        } else {
            t
        }
    }
}
