//! Anthropic Messages API provider: POST {base_url}/v1/messages with
//! stream=true. Standard wire shape:
//!
//! - `content_block_delta` with delta.type == "text_delta" → MessageDelta
//! - `content_block_delta` with delta.type == "thinking_delta" → ReasoningDelta
//! - `content_block_start` with content_block.type == "tool_use" opens a
//!   call (id, name); `input_json_delta` partial_json fragments append args
//! - `message_delta` carries stop_reason; `message_start` carries the
//!   input usage, `message_delta.usage.output_tokens` the output count
//! - terminal `data: [DONE]` (some gateways) or stream end
//! - Auth: `x-api-key` + `anthropic-version` header.

use std::collections::BTreeMap;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use forge_core::error::{Error, Result};
use forge_core::message::{Message, ToolCall, Usage};
use forge_core::traits::{ModelProvider, ModelRequest, ProviderEvent};

use super::openai::map_http_error;

pub struct AnthropicProvider {
    http: reqwest::Client,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent("forge/0.1")
                .build()
                .expect("reqwest client"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    fn endpoint(&self) -> String {
        if self.base_url.ends_with("/v1") {
            format!("{}/messages", self.base_url)
        } else {
            format!("{}/v1/messages", self.base_url)
        }
    }
}

/// Canonical messages → Messages API `messages` (system pulled out).
fn to_wire(msgs: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        match m {
            Message::System { content } => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(content);
            }
            Message::User { content } => {
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": content}],
                }));
            }
            Message::Assistant { content, tool_calls, .. } => {
                let mut blocks = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                for c in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.arguments,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult { tool_call_id, content, is_error } => {
                let text = if *is_error {
                    format!("[error] {content}")
                } else {
                    content.clone()
                };
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": text,
                    }],
                }));
            }
        }
    }
    (system, out)
}

fn to_wire_tools(specs: &[forge_core::traits::ToolSpec]) -> Vec<Value> {
    specs
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "input_schema": s.parameters,
            })
        })
        .collect()
}

#[derive(Default)]
struct StreamState {
    calls: BTreeMap<u64, ToolCall>,
    args: BTreeMap<u64, String>,
    /// Open tool_use block index → call slot index.
    block_to_call: BTreeMap<u64, u64>,
    usage_in: i64,
    usage_out: i64,
    error: Option<String>,
}

impl StreamState {
    fn finalize(self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let mut calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .map(|(idx, mut c)| {
                let args = self.args.get(&idx).cloned().unwrap_or_default();
                c.arguments = if args.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&args).unwrap_or_else(|_| json!({"_raw": args}))
                };
                c
            })
            .collect();
        calls.retain(|c| !c.id.is_empty());
        if !calls.is_empty() {
            out.push(ProviderEvent::ToolCalls { calls });
        }
        if self.usage_in > 0 || self.usage_out > 0 {
            out.push(ProviderEvent::Usage {
                usage: Usage {
                    input_tokens: self.usage_in,
                    output_tokens: self.usage_out,
                    total_tokens: self.usage_in + self.usage_out,
                },
            });
        }
        if let Some(err) = self.error {
            out.push(ProviderEvent::ProviderError { message: err });
        }
        out.push(ProviderEvent::Done);
        out
    }
}

fn handle_frame(data: &str, st: &mut StreamState) -> Vec<ProviderEvent> {
    let payload = data.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("messages: bad SSE frame: {e}");
            return Vec::new();
        }
    };
    if let Some(ty) = v.get("type").and_then(Value::as_str) {
        if ty == "error" {
            st.error = Some(
                v.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("messages stream error")
                    .to_string(),
            );
            return vec![];
        }
    }

    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "message_start" => {
            st.usage_in = v
                .pointer("/message/usage/input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            vec![]
        }
        "content_block_start" => {
            let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(block) = v.get("content_block") {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let slot = st.calls.len() as u64;
                    st.calls.insert(
                        slot,
                        ToolCall {
                            id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                            name: block.get("name").and_then(Value::as_str).unwrap_or("").into(),
                            arguments: Value::String(String::new()),
                        },
                    );
                    st.args.insert(slot, String::new());
                    st.block_to_call.insert(idx, slot);
                }
            }
            vec![]
        }
        "content_block_delta" => {
            let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
            match v.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => match v.pointer("/delta/text").and_then(Value::as_str) {
                    Some(t) if !t.is_empty() => {
                        vec![ProviderEvent::MessageDelta { delta: t.to_string() }]
                    }
                    _ => vec![],
                },
                Some("thinking_delta") => {
                    match v.pointer("/delta/thinking").and_then(Value::as_str) {
                        Some(t) if !t.is_empty() => {
                            vec![ProviderEvent::ReasoningDelta { delta: t.to_string() }]
                        }
                        _ => vec![],
                    }
                }
                Some("input_json_delta") => {
                    if let Some(p) = v.pointer("/delta/partial_json").and_then(Value::as_str) {
                        if let Some(&slot) = st.block_to_call.get(&idx) {
                            st.args.entry(slot).or_default().push_str(p);
                        }
                    }
                    vec![]
                }
                _ => vec![],
            }
        }
        "message_delta" => {
            st.usage_out = v
                .pointer("/usage/output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(st.usage_out);
            vec![]
        }
        _ => vec![],
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic-messages"
    }

    async fn stream(
        &self,
        req: ModelRequest,
        api_key: &str,
    ) -> Result<BoxStream<'static, ProviderEvent>> {
        let (system, messages) = to_wire(&req.messages);
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(to_wire_tools(&req.tools));
        }

        let resp = self
            .http
            .post(self.endpoint())
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(map_http_error(status.as_u16(), &text));
        }

        let sse = resp.bytes_stream().eventsource();
        let end_state = std::sync::Arc::new(std::sync::Mutex::new(StreamState::default()));
        let state = end_state.clone();

        let per_frame = sse.map(move |ev| {
            let st = &mut *state.lock().unwrap();
            match ev {
                Ok(ev) => handle_frame(&ev.data, st),
                Err(e) => {
                    tracing::warn!("messages SSE decode error: {e}");
                    Vec::new()
                }
            }
        });

        let post = per_frame
            .flat_map(futures::stream::iter)
            .chain(futures::stream::once(async move {
                let final_events = std::mem::take(&mut *end_state.lock().unwrap()).finalize();
                final_events
            })
            .flat_map(futures::stream::iter))
            .boxed();

        Ok(post)
    }
}
