use crate::error::Result;
use crate::message::{Message, ToolCall, Usage};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

/// A request ready to be sent to a model. Providers translate `messages`
/// plus tool specs into their wire format.
pub struct ModelRequest {
    pub messages: Vec<Message>,
    /// Tool specs in JSON-schema form (name/description/parameters).
    pub tools: Vec<ToolSpec>,
    pub model: String,
    pub temperature: Option<f64>,
    pub max_tokens: u32,
    /// Hint that the client wants reasoning_content streamed when available.
    pub stream_reasoning: bool,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

/// One streaming event from a provider.
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    /// Assistant text增量.
    MessageDelta { delta: String },
    /// Chain-of-thought增量 (reasoning_content etc.).
    ReasoningDelta { delta: String },
    /// A tool-call streaming fragment (index-keyed reassembly happens in
    /// the provider; core only ever sees complete calls unless a provider
    /// forwards fragments).
    ToolCallFragment {
        index: usize,
        id: String,
        name: String,
        args: String,
    },
    /// Complete tool call(s) requested by the model.
    ToolCalls { calls: Vec<ToolCall> },
    /// Final usage for the response.
    Usage { usage: Usage },
    /// Mid-stream provider error (SSE error frame). Consumers treat the
    /// stream as finished after this.
    ProviderError { message: String },
    /// Stream finished normally.
    Done,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Human-readable name for logs/UI (e.g. "deepseek (openai)").
    fn name(&self) -> &str;

    /// Stream a completion for the request. Yields ProviderEvents in order;
    /// must yield exactly one Usage and end with Done on success.
    async fn stream(
        &self,
        req: ModelRequest,
        api_key: &str,
    ) -> Result<BoxStream<'static, ProviderEvent>>;
}

/// Result of executing a tool.
pub struct ToolOutput {
    /// Final merged text returned to the model (possibly truncated).
    pub content: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
}

/// Callbacks a tool uses to report live progress.
#[async_trait]
pub trait ToolCallbacks: Send + Sync {
    async fn output_delta(&self, call_id: &str, chunk: String);
}

/// A built-in or extension tool. `call_id` lets implementations correlate
/// progress events; `emit` may be a no-op for tools without streaming.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;

    async fn execute(
        &self,
        call_id: &str,
        arguments: Value,
        emit: &dyn ToolCallbacks,
    ) -> Result<ToolOutput>;
}

impl crate::registry::Named for dyn Tool {
    fn name(&self) -> &str {
        Tool::name(self)
    }
}
