//! OpenAI Responses API provider: POST {base_url}/v1/responses with
//! stream=true. Wire shape verified against a live endpoint:
//!
//! - `response.output_text.delta` → MessageDelta
//! - `response.output_item.added` (item.type == "function_call") opens a
//!   call slot keyed by output_index; `response.function_call_arguments.delta`
//!   appends argument fragments; `response.function_call_arguments.done`
//!   finalizes them
//! - `response.completed` carries usage {input_tokens, output_tokens,
//!   total_tokens}; `response.failed`/`error` frames carry errors
//! - some gateways append a `data: {"error":...}` frame after
//!   response.completed but before [DONE]; when text/usage already arrived
//!   we surface it as ProviderError only if nothing else was received
//! - terminal `data: [DONE]`

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

pub struct ResponsesProvider {
    http: reqwest::Client,
    base_url: String,
}

impl ResponsesProvider {
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
            format!("{}/responses", self.base_url)
        } else {
            format!("{}/v1/responses", self.base_url)
        }
    }
}

/// Canonical messages → Responses `input` array.
fn to_wire_input(msgs: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        match m {
            Message::System { content } => {
                // Responses prefers top-level `instructions`, but we keep
                // system text as a developer-role input item so ordering
                // with the rest of the transcript is preserved.
                out.push(json!({
                    "role": "developer",
                    "content": [{"type": "input_text", "text": content}],
                }));
            }
            Message::User { content } => {
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": content}],
                }));
            }
            Message::Assistant { content, tool_calls, .. } => {
                if !content.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}],
                    }));
                }
                for c in tool_calls {
                    out.push(json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.name,
                        "arguments": c.arguments.to_string(),
                    }));
                }
            }
            Message::ToolResult { tool_call_id, content, is_error } => {
                let text = if *is_error {
                    format!("[error] {content}")
                } else {
                    content.clone()
                };
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": text,
                }));
            }
        }
    }
    out
}

fn to_wire_tools(specs: &[forge_core::traits::ToolSpec]) -> Vec<Value> {
    specs
        .iter()
        .map(|s| {
            json!({
                "type": "function",
                "name": s.name,
                "description": s.description,
                "parameters": s.parameters,
            })
        })
        .collect()
}

/// Cross-frame accumulation for one response stream.
#[derive(Default)]
struct StreamState {
    calls: BTreeMap<u64, ToolCall>,
    args: BTreeMap<u64, String>,
    usage: Option<Usage>,
    error: Option<String>,
    /// True once any content event arrived (used to downgrade trailing
    /// gateway error frames).
    got_content: bool,
}

impl StreamState {
    fn finalize(self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        // args are keyed by the same output_index as calls.
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
        calls.retain(|c| !c.id.is_empty() || !c.name.is_empty());
        if !calls.is_empty() {
            out.push(ProviderEvent::ToolCalls { calls });
        }
        if let Some(usage) = self.usage {
            out.push(ProviderEvent::Usage { usage });
        }
        if let Some(err) = self.error {
            if !self.got_content {
                out.push(ProviderEvent::ProviderError { message: err });
            } else {
                tracing::warn!("responses: trailing error after content (ignored): {err}");
            }
        }
        out.push(ProviderEvent::Done);
        out
    }
}

/// Handle one SSE frame; mutates state and returns downstream events.
fn handle_frame(event_name: &str, data: &str, st: &mut StreamState) -> Vec<ProviderEvent> {
    let payload = data.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Vec::new();
    }
    let v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("responses: bad SSE frame: {e}");
            return Vec::new();
        }
    };
    let ty = v
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event_name)
        .to_string();

    match ty.as_str() {
        "response.output_text.delta" => {
            st.got_content = true;
            match v.get("delta").and_then(Value::as_str) {
                Some(d) if !d.is_empty() => {
                    vec![ProviderEvent::MessageDelta { delta: d.to_string() }]
                }
                _ => vec![],
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            match v.get("delta").and_then(Value::as_str) {
                Some(d) if !d.is_empty() => {
                    vec![ProviderEvent::ReasoningDelta { delta: d.to_string() }]
                }
                _ => vec![],
            }
        }
        "response.output_item.added" => {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let idx = v.get("output_index").and_then(Value::as_u64).unwrap_or(0);
                    st.calls.insert(
                        idx,
                        ToolCall {
                            id: item.get("call_id").and_then(Value::as_str).unwrap_or("").into(),
                            name: item.get("name").and_then(Value::as_str).unwrap_or("").into(),
                            arguments: Value::String(String::new()),
                        },
                    );
                    st.args.insert(idx, String::new());
                }
            }
            vec![]
        }
        "response.function_call_arguments.delta" => {
            let idx = v.get("output_index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(d) = v.get("delta").and_then(Value::as_str) {
                st.args.entry(idx).or_default().push_str(d);
            }
            vec![]
        }
        "response.function_call_arguments.done" => {
            let idx = v.get("output_index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(d) = v.get("arguments").and_then(Value::as_str) {
                if let Some(buf) = st.args.get_mut(&idx) {
                    *buf = d.to_string();
                }
            }
            vec![]
        }
        "response.completed" => {
            if let Some(u) = v.pointer("/response/usage") {
                st.usage = Some(Usage {
                    input_tokens: u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
                    output_tokens: u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
                    total_tokens: u.get("total_tokens").and_then(Value::as_i64).unwrap_or(0),
                });
            }
            if let Some(e) = v.pointer("/response/error").filter(|e| !e.is_null()) {
                st.error = Some(
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("response error")
                        .to_string(),
                );
            }
            vec![]
        }
        "response.failed" => {
            st.error = Some(
                v.pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response failed")
                    .to_string(),
            );
            vec![]
        }
        "error" => {
            // Frame-level error (also used by gateways mid-stream).
            st.error = Some(
                v.pointer("/error/message")
                    .or_else(|| v.pointer("/message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| "responses stream error".into()),
            );
            vec![]
        }
        _ => vec![],
    }
}

#[async_trait]
impl ModelProvider for ResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
    }

    async fn stream(
        &self,
        req: ModelRequest,
        api_key: &str,
    ) -> Result<BoxStream<'static, ProviderEvent>> {
        let mut body = json!({
            "model": req.model,
            "input": to_wire_input(&req.messages),
            "stream": true,
            "max_output_tokens": req.max_tokens,
        });
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(to_wire_tools(&req.tools));
        }

        let resp = self
            .http
            .post(self.endpoint())
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(map_http_error(status.as_u16(), &text));
        }

        // Fold SSE frames through shared state; flush finalize() at the end.
        let sse = resp.bytes_stream().eventsource();
        let end_state = std::sync::Arc::new(std::sync::Mutex::new(StreamState::default()));
        let state = end_state.clone();

        let per_frame = sse.map(move |ev| {
            let st = &mut *state.lock().unwrap();
            match ev {
                Ok(ev) => handle_frame(ev.event.as_str(), &ev.data, st),
                Err(e) => {
                    tracing::warn!("responses SSE decode error: {e}");
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
