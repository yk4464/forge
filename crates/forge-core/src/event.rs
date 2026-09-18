use crate::message::Usage;

/// Everything the UI and storage layers observe, in order. The agent loop is
/// the only producer; TUI renders them, storage persists them.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A user turn has begun (model request about to be sent).
    TurnStarted,

    /// Incremental assistant text output.
    MessageDelta { delta: String },

    /// Incremental chain-of-thought output (reasoning_content etc.).
    ReasoningDelta { delta: String },

    /// The model requested a tool call; execution is starting.
    ToolCallStarted { call_id: String, name: String, command: String },

    /// Live chunk of tool output (stdout/stderr merged), for incremental UI.
    ToolCallOutputDelta { call_id: String, chunk: String },

    /// The tool call needs user approval before it may run. The UI answers
    /// via `Runtime::approve(call_id, approved)` (optionally promoting the
    /// tool to a session rule first); the turn blocks until then.
    ApprovalRequested { call_id: String, tool: String, command: String },

    /// Tool finished. `output` is the final (possibly truncated) merged text.
    ToolCallCompleted {
        call_id: String,
        exit_code: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
        output: String,
    },

    /// Token accounting refreshed.
    TokenCountUpdated { used: i64, limit: i64 },

    /// Context compaction began.
    CompactionStarted,

    /// Context compaction finished.
    CompactionCompleted { tokens_before: i64, tokens_after: i64 },

    /// Non-fatal warning (e.g. accuracy note after compaction).
    Warning { message: String },

    /// A turn finished (success or error); `usage` is cumulative for the turn.
    TurnCompleted { usage: Usage },

    /// Fatal or per-turn error surfaced to the user.
    Error { message: String },
}
