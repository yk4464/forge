//! Crash recovery for tool calls (S1 §12 恢复规则).
//!
//! A crash between "tool started" and "result persisted" leaves transcript
//! gaps. Recovery closes every gap with an honest record: completed work
//! is reused from the execution log, and anything whose real-world effect
//! is unknown is marked as such — never silently replayed. Side effects
//! (files, processes, external systems) cannot be rolled into a database
//! transaction, so "unknown" is the only safe verdict for interrupted
//! executions.

use crate::message::Message;
use crate::session::{ToolEvent, ToolEventState};
use std::collections::HashMap;

const UNKNOWN_MARKER: &str = "this call started executing before the session was interrupted; \
its real-world effect is UNKNOWN — verify state (files, processes, external systems) \
before retrying";

const NOT_STARTED_MARKER: &str = "this call was never started: the session ended before it ran";

/// Close transcript gaps: every assistant tool call without a recorded
/// result gets one, derived from the execution log. Returns the calls
/// whose outcome is UNKNOWN so the UI can ask the user to verify before
/// continuing. Idempotent: a recovered transcript has no gaps left.
pub fn recover_unanswered_calls(
    messages: &mut Vec<Message>,
    events: &[ToolEvent],
) -> Vec<ToolEvent> {
    let mut answered: HashMap<String, ()> = HashMap::new();
    let mut unanswered: Vec<crate::message::ToolCall> = Vec::new();
    for m in messages.iter() {
        match m {
            Message::Assistant { tool_calls, .. } => {
                for c in tool_calls {
                    unanswered.push(c.clone());
                }
            }
            Message::ToolResult { tool_call_id, .. } => {
                answered.insert(tool_call_id.clone(), ());
            }
            _ => {}
        }
    }
    unanswered.retain(|c| !answered.contains_key(&c.id));

    let by_id: HashMap<&str, &ToolEvent> =
        events.iter().map(|e| (e.call_id.as_str(), e)).collect();
    let mut unknown = Vec::new();
    for c in unanswered {
        let (content, is_error) = match by_id.get(c.id.as_str()) {
            Some(ToolEvent {
                state: ToolEventState::Completed,
                output,
                ..
            }) => (output.clone(), false),
            Some(ToolEvent {
                state: ToolEventState::Failed,
                output,
                ..
            }) => (output.clone(), true),
            Some(ToolEvent {
                state: ToolEventState::Started,
                ..
            }) => {
                unknown.push(by_id[c.id.as_str()].clone());
                (UNKNOWN_MARKER.to_string(), true)
            }
            None => (NOT_STARTED_MARKER.to_string(), true),
        };
        messages.push(Message::ToolResult {
            tool_call_id: c.id.clone(),
            content,
            is_error,
        });
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "shell".into(),
            arguments: json!({"command": "ls"}),
        }
    }

    fn event(id: &str, state: ToolEventState, output: &str) -> ToolEvent {
        ToolEvent {
            call_id: id.into(),
            tool: "shell".into(),
            arguments: json!({}),
            state,
            output: output.into(),
        }
    }

    #[test]
    fn completed_work_is_reused_not_repeated() {
        let mut msgs = vec![
            Message::user("q"),
            Message::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call("c1")],
            },
        ];
        let events = vec![event("c1", ToolEventState::Completed, "file.txt")];
        let unknown = recover_unanswered_calls(&mut msgs, &events);
        assert!(unknown.is_empty());
        match &msgs[2] {
            Message::ToolResult { tool_call_id, content, is_error } => {
                assert_eq!(tool_call_id, "c1");
                assert_eq!(content, "file.txt");
                assert!(!is_error);
            }
            other => panic!("expected reused result, got {other:?}"),
        }
    }

    #[test]
    fn failed_record_is_reused_as_error() {
        let mut msgs = vec![Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call("c1")],
        }];
        let events = vec![event("c1", ToolEventState::Failed, "tool error: boom")];
        let unknown = recover_unanswered_calls(&mut msgs, &events);
        assert!(unknown.is_empty());
        assert!(matches!(
            &msgs[1],
            Message::ToolResult { content, is_error: true, .. } if content.contains("boom")
        ));
    }

    #[test]
    fn interrupted_execution_is_marked_unknown() {
        // Started but never finished: the side effect may or may not have
        // happened. The record must say so — and be reported back.
        let mut msgs = vec![Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call("c1")],
        }];
        let events = vec![event("c1", ToolEventState::Started, "")];
        let unknown = recover_unanswered_calls(&mut msgs, &events);
        assert_eq!(unknown.len(), 1);
        assert!(matches!(
            &msgs[1],
            Message::ToolResult { content, is_error: true, .. } if content.contains("UNKNOWN")
        ));
    }

    #[test]
    fn never_started_call_is_marked_not_started() {
        let mut msgs = vec![Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call("c1")],
        }];
        let unknown = recover_unanswered_calls(&mut msgs, &[]);
        assert!(unknown.is_empty());
        assert!(matches!(
            &msgs[1],
            Message::ToolResult { content, is_error: true, .. } if content.contains("never started")
        ));
    }

    #[test]
    fn recovery_is_idempotent() {
        let mut msgs = vec![
            Message::user("q"),
            Message::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call("c1")],
            },
        ];
        let events = vec![event("c1", ToolEventState::Started, "")];
        let n1 = recover_unanswered_calls(&mut msgs, &events).len();
        let n2 = recover_unanswered_calls(&mut msgs, &events).len();
        assert_eq!(n1, 1);
        assert_eq!(n2, 0, "second pass must find no gaps");
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn answered_calls_are_left_alone() {
        let mut msgs = vec![
            Message::user("q"),
            Message::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call("c1")],
            },
            Message::ToolResult {
                tool_call_id: "c1".into(),
                content: "out".into(),
                is_error: false,
            },
        ];
        let before = msgs.clone();
        let unknown = recover_unanswered_calls(&mut msgs, &[]);
        assert!(unknown.is_empty());
        assert_eq!(msgs, before, "complete transcript must not change");
    }
}
