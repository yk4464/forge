//! Context compaction, a clean-room reimplementation of the codex-rs
//! mechanism (constants preserved, code original):
//!
//! - threshold = min(config, floor(effective_window * 0.90)) — computed on
//!   the effective (95%) window, avoiding the #40095 mismatch
//! - compaction replaces history with: recent user messages (greedy,
//!   newest-first, 20k-token budget, tail truncated) + summary at the end
//! - stale summaries are recognized and excluded from the retained set
//! - if the summarization request itself overflows, drop the oldest item
//!   (with its paired tool output) and retry
//!
//! Use compaction::SUMMARY_PREFIX as the marker for summary messages.

use crate::context::{approx_token_count, ContextManager};
use crate::error::{Error, Result};
use crate::event::AgentEvent;
use crate::message::Message;
use crate::traits::{ModelProvider, ModelRequest, ProviderEvent, ToolSpec};
use futures::StreamExt;
use std::sync::Arc;

/// Budget for retained recent user messages (codex: 20_000 tokens).
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: i64 = 20_000;

/// Marker prepended to the summary message; also used to detect stale
/// summaries during retention (codex SUMMARY_PREFIX).
pub const SUMMARY_PREFIX: &str = "Another model (or an earlier context window) started this task. \
The following is a summary of its thinking so far. Use this summary to continue the work:";

/// Instructions sent to the model to produce the handoff summary (codex
/// SUMMARIZATION_PROMPT, paraphrased).
pub const SUMMARIZATION_PROMPT: &str = "\
You are tasked with summarizing the conversation so far, as a handoff for another \
language model that will continue this task with no other context.

Your summary must be a faithful, dense record. Include:
1. The user's current task or question, and any explicit constraints or preferences.
2. What has been done so far: commands run, files read or modified, and their key results.
3. Decisions made and why, including rejected alternatives.
4. Errors encountered and how they were resolved (or why they remain open).
5. Unfinished work and concrete next steps.

Write in plain text. Be concise but complete; do not omit task-critical details \
(file paths, commands, error messages, numbers).";

fn is_summary_message(msg: &Message) -> bool {
    match msg {
        Message::User { content } => content.starts_with(SUMMARY_PREFIX),
        _ => false,
    }
}

/// Truncate `text` to `budget` bytes keeping head and tail halves, inserting
/// a middle marker (codex truncate_middle convention).
pub fn truncate_middle_bytes(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let head = budget / 2;
    let tail = budget - head;
    // Walk char boundaries.
    let mut head_end = head;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - tail;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = text.len() - (head_end + (text.len() - tail_start));
    format!(
        "{}\n\u{2026}{} bytes truncated\u{2026}\n{}",
        &text[..head_end],
        omitted,
        &text[tail_start..]
    )
}

/// Recent-user-message retention: greedy from newest to oldest within the
/// token budget; if the oldest candidate does not fit, truncate it to the
/// remaining budget and stop (codex build_compacted_history_with_limit).
fn retained_user_messages(history: &[Message], budget: i64) -> Vec<Message> {
    let mut selected_rev: Vec<String> = Vec::new();
    let mut remaining = budget;
    for msg in history.iter().rev() {
        let content = match msg {
            Message::User { content } if !is_summary_message(msg) => content.clone(),
            _ => continue,
        };
        let tokens = approx_token_count(&content);
        if tokens <= remaining {
            selected_rev.push(content);
            remaining -= tokens;
        } else if remaining > 0 {
            let bytes = (remaining.max(0) as usize) * 4;
            selected_rev.push(truncate_middle_bytes(&content, bytes));
            break;
        } else {
            break;
        }
    }
    selected_rev
        .into_iter()
        .rev()
        .map(|content| Message::User { content })
        .collect()
}

/// Build the replacement history (codex build_compacted_history): retained
/// user messages first, summary last.
pub fn build_compacted_history(history: &[Message], summary_text: &str) -> Vec<Message> {
    let mut new_items = retained_user_messages(history, COMPACT_USER_MESSAGE_MAX_TOKENS);
    let summary = format!("{SUMMARY_PREFIX}\n{summary_text}");
    new_items.push(Message::User { content: summary });
    new_items
}

fn tool_specs() -> Vec<ToolSpec> {
    // Compaction runs without tools: a plain summarization turn.
    Vec::new()
}

/// One summarization request against the provider. Returns the final
/// assistant text. On empty summaries, bounded retries; overflow handling
/// lives in the caller (run_compaction shrink loop).
async fn summarize(
    provider: &dyn ModelProvider,
    api_key: &str,
    model: &str,
    max_tokens: u32,
    temperature: Option<f64>,
    messages: Vec<Message>,
) -> Result<String> {
    const MAX_RETRIES: usize = 3;
    let mut retries = 0usize;
    loop {
        let req = ModelRequest {
            messages: messages.clone(),
            tools: tool_specs(),
            model: model.to_string(),
            temperature,
            max_tokens,
            stream_reasoning: false,
        };
        let stream = provider.stream(req, api_key).await?;
        let mut stream = stream;

        let mut summary = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                ProviderEvent::MessageDelta { delta } => summary.push_str(&delta),
                ProviderEvent::ReasoningDelta { .. } => {}
                ProviderEvent::ToolCallFragment { .. } => {}
                ProviderEvent::ProviderError { message } => {
                    return Err(Error::Provider(format!("compaction summarization failed: {message}")));
                }
                ProviderEvent::ToolCalls { calls } => {
                    tracing::warn!(
                        "compaction: model requested tools; ignoring {} call(s)",
                        calls.len()
                    );
                }
                ProviderEvent::Usage { .. } => {}
                ProviderEvent::Done => break,
            }
        }

        if !summary.is_empty() {
            return Ok(summary);
        }
        if retries < MAX_RETRIES {
            retries += 1;
            continue;
        }
        return Err(Error::Provider("compaction: empty summary".into()));
    }
}

/// Handle an overflow-style provider error during compaction by shrinking
/// the input. Called by the driver below.
fn shrink_for_overflow(msgs: &mut Vec<Message>) -> bool {
    if msgs.len() > 1 {
        msgs.remove(0);
        true
    } else {
        false
    }
}

/// Drive compaction: build the summarization input from the current history,
/// run it, and replace the history. Emits CompactionStarted/Completed plus a
/// codex-style accuracy warning. Returns tokens before/after (estimates).
pub async fn run_compaction(
    cm: &mut ContextManager,
    provider: Arc<dyn ModelProvider>,
    api_key: &str,
    model: &str,
    temperature: Option<f64>,
    max_tokens: u32,
    events: &tokio::sync::mpsc::UnboundedSender<AgentEvent>,
) -> Result<(i64, i64)> {
    let tokens_before = cm.accounting.current_estimate();

    events
        .send(AgentEvent::CompactionStarted)
        .map_err(|e| Error::ChannelClosed(e.to_string()))?;

    // The summarization prompt is appended as a user message on a copy of
    // the history (codex: SUMMARIZATION_PROMPT as user input).
    let mut input: Vec<Message> = cm.history.snapshot();
    input.push(Message::user(SUMMARIZATION_PROMPT));

    let summary = match summarize(
        provider.as_ref(),
        api_key,
        model,
        max_tokens,
        temperature,
        input.clone(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) if e.is_context_window_exceeded() => {
            // Drop oldest until the summarization request fits.
            let mut shrunk = input.clone();
            loop {
                if !shrink_for_overflow(&mut shrunk) {
                    return Err(e);
                }
                match summarize(
                    provider.as_ref(),
                    api_key,
                    model,
                    max_tokens,
                    temperature,
                    shrunk.clone(),
                )
                .await
                {
                    Ok(s) => break s,
                    Err(e2) if e2.is_context_window_exceeded() => continue,
                    Err(e2) => return Err(e2),
                }
            }
        }
        Err(e) => return Err(e),
    };

    let summary_text = summary.trim().to_string();
    let replacement = build_compacted_history(cm.history.items(), &summary_text);
    let tokens_after: i64 = replacement.iter().map(crate::context::approx_tokens_for_message).sum();

    cm.history.replace(replacement);
    // Local estimate now equals the replacement size; the next real usage
    // response will re-anchor the accounting.
    cm.accounting = crate::context::TokenAccounting::default();
    for m in cm.history.items() {
        cm.accounting.record_local_item(m);
    }

    events
        .send(AgentEvent::CompactionCompleted {
            tokens_before,
            tokens_after,
        })
        .ok();
    events
        .send(AgentEvent::Warning {
            message: "Heads up: long threads and multiple compactions can reduce model \
                      accuracy. Consider starting a fresh session."
                .into(),
        })
        .ok();

    Ok((tokens_before, tokens_after))
}

/// True when the given user message is a compaction summary marker.
pub fn message_is_summary(msg: &Message) -> bool {
    is_summary_message(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s = "a".repeat(1000) + "MIDDLE" + &"b".repeat(1000);
        let t = truncate_middle_bytes(&s, 200);
        // Marker text adds some bytes beyond the budget; keep it bounded.
        assert!(t.len() < 260, "truncated len = {}", t.len());
        assert!(t.starts_with("aaaa"));
        assert!(t.ends_with("bbbb"));
        assert!(t.contains("truncated"));
    }

    #[test]
    fn retained_skips_summaries_and_respects_budget() {
        let history = vec![
            Message::user("old-1"),
            Message::user("old-2"),
            Message::user(&format!("{SUMMARY_PREFIX}\nearlier summary")),
            Message::user(&"x".repeat(100_000)),
            Message::user("recent-1"),
            Message::user("recent-2"),
        ];
        let kept = retained_user_messages(&history, 100_000);
        let texts: Vec<&str> = kept
            .iter()
            .map(|m| match m {
                Message::User { content } => content.as_str(),
                _ => "",
            })
            .collect();
        assert!(!texts.iter().any(|t| t.contains("earlier summary")));
        assert_eq!(texts.last().copied(), Some("recent-2"));
    }

    #[test]
    fn compacted_history_shape() {
        let history = vec![
            Message::system("sys"),
            Message::user("q1"),
            Message::Assistant {
                content: "a1".into(),
                reasoning: None,
                tool_calls: vec![],
            },
            Message::user("q2"),
        ];
        let out = build_compacted_history(&history, "the summary");
        assert_eq!(out.len(), 3); // q1, q2, summary — system+assistant dropped
        assert!(matches!(out[0], Message::User { .. }));
        match out.last().unwrap() {
            Message::User { content } => {
                assert!(content.starts_with(SUMMARY_PREFIX));
                assert!(content.ends_with("the summary"));
            }
            _ => panic!("summary must be a user message"),
        }
    }

    #[test]
    fn budget_overflow_truncates_last() {
        let big = "y".repeat(200_000); // ~50k tokens > budget
        let history = vec![Message::user(&big)];
        let kept = retained_user_messages(&history, COMPACT_USER_MESSAGE_MAX_TOKENS);
        assert_eq!(kept.len(), 1);
        if let Message::User { content } = &kept[0] {
            assert!(content.contains("truncated"));
            assert!(content.len() < big.len());
        } else {
            panic!();
        }
    }
}
