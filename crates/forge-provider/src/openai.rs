//! OpenAI-compatible provider: POST {base_url}/v1/chat/completions with
//! stream=true. Works with any vendor speaking the chat/completions wire
//! format (DeepSeek, Zhipu, Qwen/DashScope compatible mode, Moonshot, ...).
//!
//! Streaming deltas are parsed from SSE `data:` frames; reasoning models
//! that emit `reasoning_content` are surfaced as ReasoningDelta. Usage is
//! taken from the terminal chunk when `stream_options.include_usage` is
//! honored; DeepSeek/OpenAI-compatible vendors include it in the final
//! chunk with empty choices.

use std::io;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use forge_core::error::{Error, Result};
use forge_core::message::{Message, ToolCall, Usage};
use forge_core::traits::{ModelProvider, ModelRequest, ProviderEvent};

/// Connect/read timeouts shared by all providers. `read_timeout` bounds
/// the wait for each body chunk (covers stalled SSE streams) without
/// capping total stream duration, which is unbounded for long generations.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub(crate) const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct OpenAiProvider {
    http: reqwest::Client,
    base_url: String,
    label: String,
}

impl OpenAiProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent("forge/0.1")
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()
                .expect("reqwest client"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            label: "openai-compatible".into(),
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// The normalized base URL (tests use this).
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn endpoint(&self) -> String {
        // Vendors differ on whether base_url includes /v1; accept both.
        if self.base_url.ends_with("/v1") {
            format!("{}/chat/completions", self.base_url)
        } else {
            format!("{}/v1/chat/completions", self.base_url)
        }
    }
}

/// Convert canonical messages to chat/completions wire format.
fn to_wire_messages(msgs: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(msgs.len());
    let mut pending_tool: Option<&ToolCall> = None;
    let _ = &mut pending_tool;
    for m in msgs {
        match m {
            Message::System { content } => {
                out.push(json!({"role": "system", "content": content}));
            }
            Message::User { content } => {
                out.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant {
                content,
                reasoning: _,
                tool_calls,
            } => {
                let mut obj = json!({"role": "assistant", "content": content});
                if !tool_calls.is_empty() {
                    obj["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        "arguments": c.arguments.to_string(),
                                    },
                                })
                            })
                            .collect(),
                    );
                }
                out.push(obj);
            }
            Message::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let text = if *is_error {
                    format!("[error] {content}")
                } else {
                    content.clone()
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": text,
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
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.parameters,
                },
            })
        })
        .collect()
}

/// Map a provider HTTP error onto forge errors; context-overflow markers
/// become Error::ContextWindowExceeded so the compaction retry can react.
pub(crate) fn map_http_error(status: u16, body: &str) -> Error {
    let lower = body.to_lowercase();
    let overflow = lower.contains("context length")
        || lower.contains("context_length")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
        || lower.contains("reduce the length")
        // Anthropic: "prompt is too long: N tokens > M maximum context"
        || lower.contains("prompt is too long")
        || (lower.contains("context") && lower.contains("window"));
    if overflow {
        return Error::ContextWindowExceeded { used: 0, limit: 0 };
    }
    Error::Provider(format!("API error (HTTP {status}): {}", extract_error_message(body)))
}

/// Pull a human-readable message out of an error body: JSON
/// `{error:{message}}` (OpenAI/DeepSeek/Zhipu style), `{message}`, or a
/// `msg_detail` field (Zhipu), falling back to truncated raw text.
fn extract_error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let msg = v
            .pointer("/error/message")
            .or_else(|| v.pointer("/message"))
            .or_else(|| v.pointer("/msg_detail"))
            .or_else(|| v.pointer("/error"))
            .and_then(Value::as_str);
        if let Some(m) = msg {
            return m.to_string();
        }
        // As last resort serialize compactly.
        return serde_json::to_string(&v).unwrap_or_else(|_| body.to_string());
    }
    truncate_body(body)
}

/// Truncate an error body for display without ever slicing through a
/// multi-byte UTF-8 character (a raw byte index would panic on CJK text).
fn truncate_body(body: &str) -> String {
    const MAX: usize = 400;
    if body.len() <= MAX {
        return body.to_string();
    }
    let mut end = MAX;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

/// Parse one SSE `data:` payload into zero or more events.
fn parse_chunk(data: &str, acc: &mut Vec<ProviderEvent>) {
    let payload = data.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return;
    }
    let v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("chat/completions: bad SSE chunk: {e}");
            return;
        }
    };
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        let msg = if let Some(s) = err.as_str() {
            s.to_string()
        } else {
            err.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "unknown provider error".into())
        };
        acc.push(ProviderEvent::ProviderError { message: msg });
        return;
    }
    if let Some(usage) = v.get("usage").filter(|u| u.is_object() && !u.as_object().unwrap().is_empty()) {
        let total = usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0);
        // A total of 0 means the gateway dropped the usage fields; skip it
        // so accounting is never re-anchored to 0.
        if total > 0 {
            acc.push(ProviderEvent::Usage {
                usage: Usage {
                    input_tokens: usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
                    output_tokens: usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
                    total_tokens: total,
                },
            });
        }
    }
    if let Some(choices) = v.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let delta = choice.get("delta");
            let Some(delta) = delta else { continue };
            if let Some(rc) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !rc.is_empty() {
                    acc.push(ProviderEvent::ReasoningDelta { delta: rc.to_string() });
                }
            }
            if let Some(rc) = delta.get("reasoning").and_then(Value::as_str) {
                if !rc.is_empty() {
                    acc.push(ProviderEvent::ReasoningDelta { delta: rc.to_string() });
                }
            }
            if let Some(txt) = delta.get("content").and_then(Value::as_str) {
                if !txt.is_empty() {
                    acc.push(ProviderEvent::MessageDelta { delta: txt.to_string() });
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    // Streaming tool calls arrive as fragments; with current
                    // vendors the whole arguments blob typically lands in one
                    // chunk. We reassemble by index to be safe.
                    let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let args = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    acc.push(ProviderEvent::ToolCallFragment {
                        index: idx,
                        id: id.to_string(),
                        name: name.to_string(),
                        args: args.to_string(),
                    });
                }
            }
        }
    }
}

/// Apply one tool-call fragment to the reassembly state. Returns false
/// (and mutates nothing) when the fragment index is bogus and dropped.
fn apply_fragment(st: &mut FragmentState, index: usize, id: &str, name: &str, args: &str) -> bool {
    if index > MAX_TOOL_CALL_INDEX {
        tracing::warn!(
            "chat/completions: dropping tool-call fragment with bogus index {index}"
        );
        return false;
    }
    while st.fragments.len() <= index {
        st.fragments.push(ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: Value::String(String::new()),
        });
        st.args.push(String::new());
    }
    if !id.is_empty() {
        st.fragments[index].id = id.to_string();
    }
    if !name.is_empty() {
        st.fragments[index].name = name.to_string();
    }
    st.args[index].push_str(args);
    true
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.label
    }

    async fn stream(
        &self,
        req: ModelRequest,
        api_key: &str,
    ) -> Result<BoxStream<'static, ProviderEvent>> {
        let mut body = json!({
            "model": req.model,
            "messages": to_wire_messages(&req.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": req.max_tokens,
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

        // Wrap the SSE byte stream; reassemble tool-call fragments into
        // complete ToolCalls, then emit ToolCalls + Done at the end.
        let sse = resp.bytes_stream().eventsource();

        let end_state = std::sync::Arc::new(std::sync::Mutex::new(FragmentState::default()));
        let mut state = end_state.clone();

        let inner = sse.map(move |ev| {
            let state = &mut state;
            match ev {
                Ok(ev) => {
                    let mut acc: Vec<ProviderEvent> = Vec::new();
                    parse_chunk(&ev.data, &mut acc);
                    // Reassemble fragments in place; only pass through the
                    // non-fragment events.
                    let mut out = Vec::new();
                    for ev in acc {
                        match ev {
                            ProviderEvent::ToolCallFragment {
                                index,
                                id,
                                name,
                                args,
                            } => {
                                let mut st = state.lock().unwrap();
                                apply_fragment(&mut st, index, &id, &name, &args);
                            }
                            other => out.push(other),
                        }
                    }
                    out
                }
                Err(e) => {
                    tracing::warn!("SSE decode error: {e}");
                    state.lock().unwrap().transport_error = Some(e.to_string());
                    Vec::new()
                }
            }
        });

        let flattened = inner.flat_map(futures::stream::iter);

        let post = flattened
            .chain(futures::stream::once(async move {
                let mut out = Vec::new();
                let mut calls = Vec::new();
                let mut state = end_state.lock().unwrap();
                for (i, mut c) in state.fragments.iter().cloned().enumerate() {
                    let args_str = state.args[i].clone();
                    let arguments = if args_str.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str::<Value>(&args_str).unwrap_or_else(|_| {
                            json!({"_raw": args_str})
                        })
                    };
                    c.arguments = arguments;
                    calls.push(c);
                }
                // Placeholder slots from sparse indexes (or empty-id calls)
                // would produce requests the vendor rejects; drop them.
                calls.retain(|c| !c.id.is_empty() && !c.name.is_empty());
                if !calls.is_empty() {
                    out.push(ProviderEvent::ToolCalls { calls });
                }
                if let Some(err) = state.transport_error.take() {
                    out.push(ProviderEvent::ProviderError {
                        message: format!("stream interrupted: {err}"),
                    });
                }
                out.push(ProviderEvent::Done);
                out
            })
            .flat_map(futures::stream::iter))
            .boxed();

        Ok(post)
    }
}

/// Shared reassembly state for streaming tool-call fragments.
#[derive(Default, Clone)]
struct FragmentState {
    fragments: Vec<ToolCall>,
    args: Vec<String>,
    /// Set when the transport broke mid-stream (decode error, connection
    /// reset). Finalize surfaces it so truncated output is never reported
    /// as a complete answer.
    transport_error: Option<String>,
}

/// Sanity cap on tool-call fragment indexes. The index comes from the
/// server (untrusted); a bogus huge value would otherwise grow the
/// fragment vec without bound.
const MAX_TOOL_CALL_INDEX: usize = 1024;

// Keep io import used if lints complain in future edits.
#[allow(dead_code)]
fn _io_marker() -> Option<io::Error> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_body_never_splits_utf8() {
        // 100 CJK chars = 300 bytes; add more so len > 400 and the 400th
        // byte lands mid-character. The old `&body[..400]` panicked here.
        let body = "汉".repeat(200);
        let out = truncate_body(&body);
        assert!(out.chars().count() < 200);
        assert!(out.ends_with('…'));
        // Sanity: identical behavior for ASCII (400 bytes + the 3-byte ellipsis).
        assert_eq!(truncate_body(&"a".repeat(500)).len(), 403);
    }

    #[test]
    fn error_null_frame_is_ignored() {
        let mut acc = Vec::new();
        parse_chunk(r#"{"error":null,"choices":[]}"#, &mut acc);
        assert!(
            !acc.iter().any(|e| matches!(e, ProviderEvent::ProviderError { .. })),
            "error:null must not abort the turn, got {acc:?}"
        );
    }

    #[test]
    fn usage_without_total_is_dropped() {
        let mut acc = Vec::new();
        parse_chunk(r#"{"usage":{"prompt_tokens":10}}"#, &mut acc);
        assert!(
            !acc.iter().any(|e| matches!(e, ProviderEvent::Usage { .. })),
            "usage missing total_tokens must not zero the accounting, got {acc:?}"
        );
    }

    #[test]
    fn tool_call_fragment_index_is_bounded() {
        // The index comes from the server; a bogus huge value must not
        // allocate an unbounded fragment vec.
        let mut st = FragmentState::default();
        assert!(!apply_fragment(&mut st, 100_000_000, "c1", "shell", "{}"));
        assert!(st.fragments.is_empty());
        // A sane index still assembles.
        assert!(apply_fragment(&mut st, 0, "c1", "shell", r#"{"co"#));
        assert!(apply_fragment(&mut st, 0, "", "", r#"mmand":"ls"}"#));
        assert_eq!(st.fragments[0].name, "shell");
    }

    #[test]
    fn finalize_drops_placeholder_calls() {
        // A fragment stream starting at index 1 leaves a placeholder at 0;
        // finalize must not surface the empty call.
        let mut st = FragmentState::default();
        assert!(apply_fragment(&mut st, 1, "c1", "shell", "{}"));
        let mut calls: Vec<ToolCall> = st.fragments.iter().cloned().collect();
        calls.retain(|c| !c.id.is_empty() && !c.name.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "c1");
    }
}
