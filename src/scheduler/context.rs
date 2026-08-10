//! LLM conversation context with a sliding window and summary compaction.

use serde::{Deserialize, Serialize};

use crate::llm::Message;

/// Rough token estimate: one token per four characters, plus message overhead.
pub fn estimate_tokens(message: &Message) -> usize {
    message.content.chars().count() / 4 + 4
}

/// A bounded chat history.
///
/// `needs_compaction` reports when the estimated size exceeds `max_tokens`.
/// The scheduler then summarizes `history_for_summary` through the LLM and
/// calls `apply_summary` to replace the summarized prefix with one system
/// message. The trailing `keep_recent` messages are never summarized.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Context {
    messages: Vec<Message>,
    max_tokens: usize,
    keep_recent: usize,
}

impl Context {
    pub fn new(max_tokens: usize, keep_recent: usize) -> Context {
        Context {
            messages: Vec::new(),
            max_tokens,
            keep_recent: keep_recent.max(1),
        }
    }

    /// Rebuild a context from persisted messages, preserving the window
    /// settings that created it.
    pub fn from_messages(messages: Vec<Message>, max_tokens: usize, keep_recent: usize) -> Context {
        Context {
            messages,
            max_tokens,
            keep_recent: keep_recent.max(1),
        }
    }

    /// Append a message, truncating oversized tool results.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Append a tool result as a user message, truncated to `max_chars` to
    /// bound the context. The plan-execution model has no assistant tool
    /// calls, so results are labeled user messages rather than `tool` role
    /// messages (which some APIs reject without a matching tool call).
    pub fn push_tool_result(&mut self, tool_name: &str, content: &str, max_chars: usize) {
        let truncated = if content.chars().count() > max_chars {
            let mut out: String = content.chars().take(max_chars).collect();
            out.push_str("\n... [truncated]");
            out
        } else {
            content.to_string()
        };
        self.messages.push(Message::user(format!(
            "[tool {tool_name} result]\n{truncated}"
        )));
    }

    /// All messages in order.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Total estimated tokens for all messages.
    pub fn estimated_tokens(&self) -> usize {
        self.messages.iter().map(estimate_tokens).sum()
    }

    /// Whether compaction is needed to stay within `max_tokens`.
    pub fn needs_compaction(&self) -> bool {
        self.estimated_tokens() > self.max_tokens
    }

    /// The messages eligible for summarization (everything except the most
    /// recent `keep_recent` messages), rendered as plain text.
    pub fn history_for_summary(&self) -> String {
        let cutoff = self.messages.len().saturating_sub(self.keep_recent);
        self.messages[..cutoff]
            .iter()
            .map(|m| {
                let role = match m.role {
                    crate::llm::Role::System => "system",
                    crate::llm::Role::User => "user",
                    crate::llm::Role::Assistant => "assistant",
                    crate::llm::Role::Tool => "tool",
                };
                format!("[{role}] {}", m.content)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Replace the summarizable prefix with a single system message.
    ///
    /// The scheduler must only call this when compaction is warranted.
    pub fn apply_summary(&mut self, summary: &str) {
        let cutoff = self.messages.len().saturating_sub(self.keep_recent);
        if cutoff == 0 {
            return;
        }
        self.messages.drain(..cutoff);
        self.messages.insert(
            0,
            Message::system(format!("Summary of the earlier conversation:\n{summary}")),
        );
    }

    /// The number of messages currently held.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_stable() {
        let msg = Message::user("hello world, this is a test message");
        assert_eq!(estimate_tokens(&msg), msg.content.chars().count() / 4 + 4);
    }

    #[test]
    fn compaction_triggers_above_max_tokens() {
        let mut ctx = Context::new(40, 3);
        let line = "x".repeat(100); // ~25 tokens each
        for _ in 0..4 {
            ctx.push(Message::user(line.clone()));
        }
        assert!(ctx.needs_compaction());
        assert_eq!(ctx.estimated_tokens(), 4 * (25 + 4));
    }

    #[test]
    fn summary_replaces_prefix_and_keeps_recent() {
        let mut ctx = Context::new(1_000_000, 3);
        for i in 0..6 {
            ctx.push(Message::user(format!("message {i}")));
        }
        ctx.apply_summary("compressed everything");
        assert_eq!(ctx.len(), 4); // summary + 3 recent
        assert_eq!(
            ctx.messages()[0].content,
            "Summary of the earlier conversation:\ncompressed everything"
        );
        assert_eq!(ctx.messages()[1].content, "message 3");
        assert_eq!(ctx.messages()[3].content, "message 5");
        assert!(!ctx.needs_compaction());
    }

    #[test]
    fn summary_is_noop_when_nothing_to_summarize() {
        let mut ctx = Context::new(1_000_000, 5);
        for i in 0..4 {
            ctx.push(Message::user(format!("message {i}")));
        }
        ctx.apply_summary("ignored");
        assert_eq!(ctx.len(), 4);
    }

    #[test]
    fn tool_results_are_truncated() {
        let mut ctx = Context::new(1_000_000, 2);
        ctx.push_tool_result("read_file", &"y".repeat(1000), 50);
        let content = &ctx.messages()[0].content;
        assert!(content.contains("[truncated]"));
        assert!(content.chars().count() < 100);
        assert!(content.starts_with("[tool read_file result]"));
    }
}
