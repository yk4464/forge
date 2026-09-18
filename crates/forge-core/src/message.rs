use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One entry in the conversation history. This is the provider-agnostic
/// canonical form; each provider adapter converts to/from its wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// Result of a tool execution, keyed to the originating ToolCall.
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Message::User {
            content: content.into(),
        }
    }

    /// Byte size of the model-visible text in this message. Used by the
    /// token estimator (bytes/4 heuristic), not for storage.
    pub fn content_bytes(&self) -> usize {
        match self {
            Message::System { content } | Message::User { content } => content.len(),
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                content.len()
                    + reasoning.as_deref().map(str::len).unwrap_or(0)
                    + tool_calls.iter().map(|c| c.json_bytes()).sum::<usize>()
            }
            Message::ToolResult {
                content,
                is_error: _,
                tool_call_id,
            } => content.len() + tool_call_id.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as produced by the model.
    pub arguments: Value,
}

impl ToolCall {
    fn json_bytes(&self) -> usize {
        serde_json::to_string(self).map(|s| s.len()).unwrap_or(0)
    }
}

/// Usage reported by the provider for a single completed response.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let msgs = vec![
            Message::system("be brief"),
            Message::user("hello"),
            Message::Assistant {
                content: String::new(),
                reasoning: Some("thinking".into()),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                }],
            },
            Message::ToolResult {
                tool_call_id: "c1".into(),
                content: "file.txt".into(),
                is_error: false,
            },
        ];
        let json = serde_json::to_string(&msgs).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msgs);
    }
}
