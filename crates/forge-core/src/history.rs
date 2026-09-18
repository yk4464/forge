use crate::message::{Message, ToolCall};
use std::collections::HashMap;

/// The conversation transcript: ordered, append-mostly, replaceable on
/// compaction. Mirrors Codex's ContextManager in miniature.
#[derive(Debug, Clone, Default)]
pub struct History {
    items: Vec<Message>,
    /// Bumped on every rewrite (compaction) so snapshots can detect changes.
    version: u64,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[Message] {
        &self.items
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn push(&mut self, msg: Message) {
        self.items.push(msg);
    }

    pub fn extend(&mut self, msgs: impl IntoIterator<Item = Message>) {
        self.items.extend(msgs);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Drop the oldest item. If it was a tool call, its paired result goes
    /// too (and vice versa) so call/output pairs never dangle — same
    /// invariant as codex-rs `remove_corresponding_for`.
    pub fn remove_first(&mut self) -> Option<Message> {
        if self.items.is_empty() {
            return None;
        }
        let removed = self.items.remove(0);
        self.version += 1;
        match &removed {
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let ids: Vec<String> = tool_calls.iter().map(|c| c.id.clone()).collect();
                self.remove_tool_results(&ids);
            }
            Message::ToolResult { tool_call_id, .. } => {
                self.remove_orphan_calls(tool_call_id);
            }
            _ => {}
        }
        Some(removed)
    }

    fn remove_tool_results(&mut self, ids: &[String]) {
        self.items.retain(|m| {
            !matches!(m, Message::ToolResult { tool_call_id, .. } if ids.contains(tool_call_id))
        });
    }

    fn remove_orphan_calls(&mut self, tool_call_id: &str) {
        self.items.retain(|m| match m {
            Message::Assistant { tool_calls, .. } => {
                // Keep the assistant message; just strip the matching call.
                // Simplest correct form: if any call matches and the message
                // has other content, drop the call; if it only had that call,
                // drop the whole message.
                let has_match = tool_calls.iter().any(|c| c.id == tool_call_id);
                if !has_match {
                    return true;
                }
                match m {
                    Message::Assistant { content, tool_calls, .. } => {
                        let remaining: Vec<ToolCall> = tool_calls
                            .iter()
                            .filter(|c| c.id != tool_call_id)
                            .cloned()
                            .collect();
                        !remaining.is_empty() || !content.is_empty()
                    }
                    _ => true,
                }
            }
            _ => true,
        });
        // Second pass: strip matching calls from surviving assistant messages.
        for m in &mut self.items {
            if let Message::Assistant { tool_calls, .. } = m {
                tool_calls.retain(|c| c.id != tool_call_id);
            }
        }
    }

    /// Remove assistant messages whose tool_calls vec became empty and that
    /// carry no text, plus any results whose call vanished.
    pub fn normalize(&mut self) {
        // Orphan results: no assistant message issues that call id.
        let issued: HashMap<String, ()> = self
            .items
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { tool_calls, .. } => Some(
                    tool_calls
                        .iter()
                        .map(|c| (c.id.clone(), ()))
                        .collect::<HashMap<String, ()>>(),
                ),
                _ => None,
            })
            .fold(HashMap::new(), |mut acc, m| {
                acc.extend(m);
                acc
            });
        self.items.retain(|m| {
            !matches!(m, Message::ToolResult { tool_call_id, .. } if !issued.contains_key(tool_call_id))
        });
        // Empty assistant shells (no text, no reasoning, no calls).
        self.items.retain(|m| {
            !matches!(m, Message::Assistant { content, reasoning, tool_calls }
                if content.is_empty()
                    && tool_calls.is_empty()
                    && reasoning.as_deref().map(str::is_empty).unwrap_or(true))
        });
    }

    /// Replace the entire transcript (compaction).
    pub fn replace(&mut self, items: Vec<Message>) {
        self.items = items;
        self.version += 1;
    }

    pub fn snapshot(&self) -> Vec<Message> {
        self.items.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "shell".into(),
            arguments: json!({"command": "ls"}),
        }
    }

    #[test]
    fn remove_first_drops_paired_result() {
        let mut h = History::new();
        h.push(Message::user("q"));
        h.push(Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call("a"), call("b")],
        });
        h.push(Message::ToolResult {
            tool_call_id: "a".into(),
            content: "out-a".into(),
            is_error: false,
        });
        h.push(Message::ToolResult {
            tool_call_id: "b".into(),
            content: "out-b".into(),
            is_error: false,
        });
        h.remove_first(); // drops user
        h.remove_first(); // drops assistant + both paired results
        assert!(h.is_empty(), "expected empty, got {:?}", h.items);
    }

    #[test]
    fn normalize_drops_orphan_result() {
        let mut h = History::new();
        h.push(Message::user("q"));
        h.push(Message::ToolResult {
            tool_call_id: "ghost".into(),
            content: "x".into(),
            is_error: false,
        });
        h.normalize();
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn normalize_drops_empty_assistant() {
        let mut h = History::new();
        h.push(Message::user("q"));
        h.push(Message::Assistant {
            content: String::new(),
            reasoning: Some("hm".into()),
            tool_calls: vec![],
        });
        h.normalize();
        assert_eq!(h.len(), 2, "reasoning-only assistant should survive");
        h.push(Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![],
        });
        h.normalize();
        assert_eq!(h.len(), 2);
    }
}
