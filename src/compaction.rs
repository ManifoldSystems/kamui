//! Context compaction: when a session's recent history grows large, older messages are folded into
//! a rolling summary so the conversation can continue without overflowing the model's context. The
//! full history is always kept in storage; only the per-request message list is compressed.

use crate::provider::{ChatRequest, Message, Role};

/// Most recent messages always kept verbatim, never summarized.
const KEEP_RECENT: usize = 6;
/// Fallback byte threshold when the active profile has no `context_window`.
const DEFAULT_THRESHOLD_BYTES: usize = 48 * 1024;

/// Byte size at which the recent (un-summarized) history should be compacted. When a context window
/// is known, compact at roughly half of it (assuming ~4 bytes per token); otherwise use a default.
///
/// `cache_pinned` profiles (Orvix Coding Plan, see `crate::cache`) wait far longer. Compaction drops
/// the messages it folded away, so every request after it is a fresh prefix: on a cached session the
/// token saving is charged back as a full re-read of the conversation. It is still allowed -- the
/// context window is a hard limit and a cache miss beats a rejected request -- but only once the
/// history is genuinely close to that limit rather than at the halfway mark.
pub fn threshold(context_window: Option<u64>, cache_pinned: bool) -> usize {
    let share = if cache_pinned { 85 } else { 50 };
    match context_window {
        Some(window) => (window as usize).saturating_mul(4) / 100 * share,
        None => DEFAULT_THRESHOLD_BYTES * share / 50,
    }
}

/// Approximate byte size of the given messages: text content plus any tool-call arguments.
pub fn total_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.content.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
        })
        .sum()
}

/// Index up to which messages should be folded into the summary, keeping the most recent
/// `KEEP_RECENT` verbatim. Returns `None` when there is nothing new worth summarizing.
///
/// The cut is snapped onto an assistant `tool_calls` boundary so a live window never
/// starts on a `role: tool` turn (DeepSeek 400s that).
pub fn cutoff(messages: &[Message], already_summarized: usize) -> Option<usize> {
    let mut cutoff = messages.len().saturating_sub(KEEP_RECENT);
    cutoff = snap_to_tool_group_start(messages, cutoff);
    (cutoff > already_summarized).then_some(cutoff)
}

fn snap_to_tool_group_start(messages: &[Message], mut cutoff: usize) -> usize {
    while cutoff > 0 && messages.get(cutoff).is_some_and(|m| m.role == Role::Tool) {
        cutoff -= 1;
    }
    cutoff
}

/// Render messages as plain text for the summarizer.
pub fn render(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| {
            let who = match message.role_name() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                "system" => "System",
                _ => "?",
            };
            if message.content.is_empty() && !message.tool_calls.is_empty() {
                let names: Vec<&str> = message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect();
                format!("{who} (called tools: {})", names.join(", "))
            } else {
                format!("{who}: {}", message.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Frozen Compaction templates (prefix-cache stability): the summary request must be
/// byte-identical every time except for the prior summary + new messages. Editing
/// these strings resets the side-request baseline the same way editing the system
/// prompt resets the main prefix — so don't, without noting it in the changelog.
pub const SUMMARY_INSTRUCTION: &str = "You maintain a running summary of a coding conversation so it can continue \
     after older messages are dropped. Rewrite the summary to capture the user's \
     goals, key decisions, files and code changed, commands run and their \
     results, and any open threads. Be concise and factual, and output only the \
     summary.";
const NO_PRIOR_SUMMARY: &str = "(none yet)";

/// Build the non-streaming request that folds new messages into the running summary.
pub fn summary_request(
    model: &str,
    existing: Option<&str>,
    rendered: &str,
    session_id: Option<String>,
) -> ChatRequest {
    let prior = existing.unwrap_or(NO_PRIOR_SUMMARY);
    ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(SUMMARY_INSTRUCTION.to_string()),
            Message::user(format!(
                "Prior summary:\n{prior}\n\nNew messages to fold in:\n{rendered}"
            )),
        ],
        tools: Vec::new(),
        session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_scales_with_context_window() {
        assert_eq!(threshold(Some(1000), false), 2000); // 1000 tokens * 4 bytes / 2
        assert_eq!(threshold(None, false), DEFAULT_THRESHOLD_BYTES);
        // A cache-pinned session holds out until the window is nearly full: compacting there
        // throws away the cached prefix, so the token saving is not free.
        assert_eq!(threshold(Some(1000), true), 3400);
        assert!(threshold(None, true) > threshold(None, false));
    }

    #[test]
    fn total_bytes_counts_content_and_tool_arguments() {
        let messages = vec![Message::user("hello"), Message::assistant("hi there")];
        assert_eq!(total_bytes(&messages), "hello".len() + "hi there".len());
    }

    #[test]
    fn cutoff_keeps_recent_messages_and_advances() {
        let messages: Vec<Message> = (0..12).map(|i| Message::user(format!("m{i}"))).collect();
        assert_eq!(cutoff(&messages[..4], 0), None); // fewer than KEEP_RECENT + 1
        assert_eq!(cutoff(&messages[..10], 0), Some(4)); // summarize the first 4, keep 6
        assert_eq!(cutoff(&messages[..10], 4), None); // nothing new past what is already summarized
        assert_eq!(cutoff(&messages[..12], 4), Some(6)); // fold two more in
    }

    #[test]
    fn cutoff_does_not_split_a_tool_batch() {
        use crate::provider::ToolCall;
        let call = |id: &str| ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        };
        let messages = vec![
            Message::user("start"),
            Message::tool_request("", vec![call("c1"), call("c2")]),
            Message::tool_result("c1", "a"),
            Message::tool_result("c2", "b"),
            Message::assistant("ok"),
            Message::user("next"),
            Message::assistant("done"),
            Message::user("again"),
            Message::assistant("later"),
        ];
        // Naive KEEP_RECENT=6 cuts at index 3 (`tool` for c2). Remaining would
        // start on an orphan tool result. Snap back onto the assistant request.
        let cut = cutoff(&messages, 0).expect("enough history");
        assert_eq!(cut, 1);
        assert!(!messages[cut].tool_calls.is_empty());
        assert_ne!(messages[cut].role_name(), "tool");
    }

    #[test]
    fn render_labels_speakers_and_tool_calls() {
        use crate::provider::ToolCall;
        let messages = vec![
            Message::user("fix the bug"),
            Message::tool_request(
                "",
                vec![ToolCall {
                    id: "c1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
        ];
        let rendered = render(&messages);
        assert!(rendered.contains("User: fix the bug"));
        assert!(rendered.contains("called tools: read_file"));
    }

    #[test]
    fn summary_request_shape_is_stable() {
        // Only the prior summary + new messages may vary; instruction, tools,
        // and message roles are frozen so the side request has a stable baseline.
        let request = summary_request("m", Some("prior"), "new stuff", None);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].content, SUMMARY_INSTRUCTION);
        assert!(request.messages[1].content.contains("prior"));
        assert!(request.messages[1].content.contains("new stuff"));
        assert!(request.tools.is_empty());
        let fresh = summary_request("m", None, "new stuff", None);
        assert!(fresh.messages[1].content.contains("(none yet)"));
    }
}
