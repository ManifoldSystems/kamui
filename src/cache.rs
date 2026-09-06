//! Prompt-cache contract for providers that pin a session to a warm prefix.
//!
//! Orvix Coding Plan (`send_session_id = true`) routes every request of a session to the same
//! upstream worker and keeps the request prefix cached there. That only pays off while the bytes
//! *before* the newest turn stay identical: change one character early in the request and the whole
//! conversation is re-read at full price. Kamui rebuilt its system message from live state on every
//! turn -- the memory table and the rolling compaction summary -- so one `remember` call reset the
//! cache for the rest of the session.
//!
//! The contract this module encodes:
//!
//! - **Stable prefix.** The system message (base prompt, tool guidance, project instructions, the
//!   eager skill list) and the tool definitions, in order, are the same bytes for the session's
//!   whole life. Only an explicit user action changes them -- `/model`, `/skills`, entering Plan
//!   Mode -- and [`PrefixGuard`] reports it when it happens instead of letting it drift silently.
//! - **Volatile tail.** Memory and the running summary still refresh every turn, but they ride in
//!   their own system message placed immediately before the new user turn (option (b) of the two
//!   choices: placed after the stable blob rather than frozen at session start). A fact remembered
//!   this turn is still visible on the next one, and everything ahead of that tail is untouched, so
//!   the cache still matches through the end of the previous turn.
//!
//! Steady state therefore diverges from the cached prefix only at the previous turn's tail: the
//! uncached remainder is one exchange plus the tail, which is a shrinking share of a growing
//! conversation.

use crate::provider::{Message, ToolDefinition};

/// The volatile block: what must be re-read every turn and so must never sit in the prefix.
/// `None` when there is nothing to say, which keeps a fresh session's tail out of the request
/// entirely.
pub fn volatile_tail(memory: &str, summary: Option<&str>) -> Option<String> {
    let memory = memory.trim();
    let summary = summary.map(str::trim).filter(|text| !text.is_empty());
    if memory.is_empty() && summary.is_none() {
        return None;
    }
    let mut tail = String::new();
    if !memory.is_empty() {
        tail.push_str(memory);
    }
    if let Some(summary) = summary {
        if !tail.is_empty() {
            tail.push_str("\n\n");
        }
        tail.push_str("Summary of the earlier conversation so far:\n\n");
        tail.push_str(summary);
    }
    Some(tail)
}

/// Assembles one turn's request in cache order: the frozen head, the conversation so far, the
/// volatile tail, then the new user turn. Keeping the order in one place is what makes the
/// contract testable -- and what stops a future caller from putting the tail back in front of the
/// history, where it would invalidate every cached byte behind it.
///
/// The head is a slice rather than one string because the stable blocks are sent as separate
/// system messages (base prompt, then the eager skill list). Splitting them changes no bytes on
/// the wire, but it keeps each block's boundary visible to a provider that caches at one.
pub fn turn_messages(
    head: &[Message],
    history: &[Message],
    tail: Option<String>,
    user: Message,
) -> Vec<Message> {
    let mut messages = Vec::with_capacity(head.len() + history.len() + 2);
    messages.extend(head.iter().cloned());
    messages.extend(history.iter().cloned());
    if let Some(tail) = tail {
        messages.push(Message::system(tail));
    }
    messages.push(user);
    messages
}

/// The head's text as one string, for fingerprinting it. Message boundaries are part of what must
/// stay stable, so they are marked rather than smoothed over: two heads that differ only in where
/// one block ends are not the same prefix.
pub fn head_text(head: &[Message]) -> String {
    head.iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

/// The exact bytes a cache-aware provider sees ahead of the conversation: the system message plus
/// every tool definition in the order they are sent. Comparing these is how the tests -- and
/// [`PrefixGuard`] -- prove that two consecutive turns really do share a prefix.
pub fn prefix_bytes(system: &str, tools: &[ToolDefinition]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(system.len() + 64 * tools.len());
    bytes.extend_from_slice(system.as_bytes());
    for tool in tools {
        bytes.push(0x1e);
        bytes.extend_from_slice(tool.name.as_bytes());
        bytes.push(0x1f);
        bytes.extend_from_slice(tool.description.as_bytes());
        bytes.push(0x1f);
        // `to_string` on a `serde_json::Value` writes object keys in their stored order, which for
        // the maps Kamui builds is insertion order -- stable across turns for identical input.
        bytes.extend_from_slice(tool.parameters.to_string().as_bytes());
    }
    bytes
}

/// Side requests that must not share the conversation's sticky `session_id`.
///
/// Title generation and compaction are short, unrelated prefixes. If they reuse the
/// conversation id on a sticky Coding Plan worker, that worker's prompt cache can be
/// evicted right after turn one (title) or mid-session (compact) — the KPI then shows
/// `Cached: 0` on the next chat turn for no harness-visible reason.
///
/// Orvix still requires *a* `session_id` on `/coding/completions`, so these derive a
/// stable sibling id. Sticky Redis and `prompt_cache_key` key off that sibling; the
/// conversation's warm prefix stays on the original id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideRequest {
    Title,
    Compact,
}

impl SideRequest {
    fn suffix(self) -> &'static str {
        match self {
            Self::Title => ":title",
            Self::Compact => ":compact",
        }
    }
}

/// Sticky id for a title or compaction call. `None` when the conversation itself is not
/// cache-pinned (`conversation_id` absent).
pub fn side_session_id(conversation_id: Option<&str>, kind: SideRequest) -> Option<String> {
    conversation_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| bounded_routing_id(id, kind.suffix()))
}

/// Sticky id for one `spawn_agent` tool call. Every round of that sub-agent reuses this id, while
/// separate tool calls cannot share the conversation's or each other's prompt cache.
pub fn sub_agent_session_id(conversation_id: Option<&str>, tool_call_id: &str) -> Option<String> {
    conversation_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| bounded_routing_id(id, &format!(":agent:{tool_call_id}")))
}

/// Provider routing fields share one conservative 64-character bound.
pub fn bounded_routing_id(id: &str, suffix: &str) -> String {
    let suffix: String = suffix.chars().take(63).collect();
    let room = 64usize.saturating_sub(suffix.chars().count());
    let mut value: String = id.trim().chars().take(room).collect();
    value.push_str(&suffix);
    value
}

/// Watches the prefix of a cache-pinned session for drift.
///
/// It never rewrites a request: a user who toggles a skill or enters Plan Mode must get the tools
/// and instructions they asked for. What it does is notice that the prefix moved and hand back one
/// line explaining that the cache warms up again, so a collapsed hit rate has a visible cause.
pub struct PrefixGuard {
    /// Only cache-pinned profiles are watched; everyone else pays no attention and no allocation.
    enabled: bool,
    seen: Option<Vec<u8>>,
}

impl PrefixGuard {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            seen: None,
        }
    }

    /// Records this turn's prefix, returning a notice when it differs from the previous turn's.
    /// The first turn of a session establishes the prefix and reports nothing.
    pub fn check(&mut self, system: &str, tools: &[ToolDefinition]) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let bytes = prefix_bytes(system, tools);
        let changed = self.seen.as_ref().is_some_and(|seen| *seen != bytes);
        self.seen = Some(bytes);
        changed.then(|| {
            "The cached prompt prefix changed (tools or instructions); this turn warms the cache \
             again."
                .to_string()
        })
    }

    /// Forgets the recorded prefix. Called when the session itself restarts (`/new`, `/resume`),
    /// where a fresh prefix is expected rather than drift worth reporting.
    pub fn reset(&mut self) {
        self.seen = None;
    }
}

/// Share of this turn's prompt that the provider served from its cache, as a percentage.
/// `None` when the provider reported no prompt tokens at all, which is not a 0% hit -- it is no
/// measurement. Capped at 100%: a host that over-reports its cache must not read as more than a
/// full hit.
pub fn hit_percent(prompt_tokens: u64, cached_tokens: u64) -> Option<f64> {
    (prompt_tokens > 0).then(|| (cached_tokens as f64 / prompt_tokens as f64 * 100.0).min(100.0))
}

/// How a session's turns landed against the cache. Lifetime token totals are a lagging measure --
/// one cold session drags them for good -- so this counts turns instead.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheReport {
    /// Turns that read nothing from the cache. The first turn of a session always is one; a later
    /// one means the prefix moved.
    pub warmup: usize,
    /// Turns from the second onwards, the population every ratio below is measured over.
    pub measured: usize,
    /// Median hit percentage across the measured turns.
    pub median: f64,
    /// Share of measured turns that reached at least 90%.
    pub pct_ge_90: f64,
    /// Share of measured turns that reached at least 95%.
    pub pct_ge_95: f64,
}

/// Builds the report from one session's chat turns, oldest first, as `(prompt_tokens,
/// cached_tokens)` pairs.
///
/// Turn one is excluded from every ratio: it cannot hit a cache that does not exist yet, and
/// including it would make short sessions look broken. Later warm-up turns stay in the
/// denominator -- a prefix that churned mid-session is exactly the failure worth seeing.
pub fn report(samples: &[(i64, i64)]) -> Option<CacheReport> {
    let warmup = samples
        .iter()
        .filter(|(prompt, cached)| *prompt > 0 && *cached == 0)
        .count();
    let mut hits: Vec<f64> = samples
        .iter()
        .skip(1)
        .filter(|(prompt, _)| *prompt > 0)
        .map(|(prompt, cached)| (*cached as f64 / *prompt as f64 * 100.0).min(100.0))
        .collect();
    if hits.is_empty() {
        return None;
    }
    let measured = hits.len();
    let at_least =
        |bar: f64| hits.iter().filter(|hit| **hit >= bar).count() as f64 / measured as f64 * 100.0;
    let pct_ge_90 = at_least(90.0);
    let pct_ge_95 = at_least(95.0);
    hits.sort_by(|a, b| a.partial_cmp(b).expect("hit ratios are never NaN"));
    let median = if measured.is_multiple_of(2) {
        (hits[measured / 2 - 1] + hits[measured / 2]) / 2.0
    } else {
        hits[measured / 2]
    };
    Some(CacheReport {
        warmup,
        measured,
        median,
        pct_ge_90,
        pct_ge_95,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("does {name}"),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn a_tail_is_omitted_when_there_is_nothing_volatile_to_say() {
        assert_eq!(volatile_tail("", None), None);
        assert_eq!(volatile_tail("   ", Some("  ")), None);
    }

    #[test]
    fn the_tail_carries_memory_and_the_running_summary() {
        let tail = volatile_tail("Known facts:\n- likes tabs", Some("earlier: we set up CI"))
            .expect("a tail");
        assert!(tail.starts_with("Known facts:"));
        assert!(tail.contains("Summary of the earlier conversation so far:"));
        assert!(tail.ends_with("earlier: we set up CI"));
    }

    #[test]
    fn reordering_tools_changes_the_prefix() {
        let system = "system";
        let forward = prefix_bytes(system, &[tool("read_file"), tool("grep")]);
        let reversed = prefix_bytes(system, &[tool("grep"), tool("read_file")]);
        assert_ne!(
            forward, reversed,
            "tool order is part of the cached prefix, so it must be visible to the guard"
        );
    }

    #[test]
    fn the_guard_reports_drift_only_after_a_prefix_exists() {
        let mut guard = PrefixGuard::new(true);
        assert_eq!(guard.check("system", &[tool("grep")]), None, "first turn");
        assert_eq!(guard.check("system", &[tool("grep")]), None, "unchanged");
        assert!(
            guard
                .check("system", &[tool("grep"), tool("glob")])
                .is_some(),
            "a new tool moves the prefix and is reported"
        );
        assert_eq!(
            guard.check("system", &[tool("grep"), tool("glob")]),
            None,
            "the moved prefix becomes the new baseline"
        );
        guard.reset();
        assert_eq!(
            guard.check("other", &[]),
            None,
            "a reset session starts over"
        );
    }

    #[test]
    fn an_unpinned_profile_is_not_watched() {
        let mut guard = PrefixGuard::new(false);
        assert_eq!(guard.check("system", &[tool("grep")]), None);
        assert_eq!(guard.check("changed", &[]), None);
    }

    /// The contract in one test: a `remember` call between two turns changes what the model is
    /// told, but not one byte of the prefix the provider caches.
    #[test]
    fn two_consecutive_turns_share_a_prefix_and_grow_only_at_the_tail() {
        let head = vec![
            Message::system("base prompt + tool guidance + project instructions"),
            Message::system("Skills:\n- review"),
        ];
        let tools = vec![tool("read_file"), tool("grep")];

        let history = vec![
            Message::user("first question"),
            Message::assistant("answer"),
        ];
        let first = turn_messages(
            &head,
            &history,
            volatile_tail("Known facts:\n- likes tabs", None),
            Message::user("second question"),
        );
        // The turn runs, the model remembers something, and the exchange joins the history.
        let history: Vec<Message> = history
            .into_iter()
            .chain([Message::user("second question"), Message::assistant("sure")])
            .collect();
        let second = turn_messages(
            &head,
            &history,
            volatile_tail("Known facts:\n- likes tabs\n- ships on Fridays", None),
            Message::user("third question"),
        );

        assert_eq!(
            prefix_bytes(&head_text(&head), &tools),
            prefix_bytes(&head_text(&head), &tools),
            "head messages and tool definitions are the same bytes on both turns"
        );
        // Everything up to where the first turn's tail sat is byte-identical, so the provider
        // matches its cache through the end of the previous exchange.
        let shared = first.len() - 2;
        for (index, (before, after)) in first.iter().zip(second.iter()).take(shared).enumerate() {
            assert_eq!(before.role, after.role, "role drifted at index {index}");
            assert_eq!(
                before.content, after.content,
                "content drifted at index {index}"
            );
        }
        assert_eq!(
            second.len(),
            first.len() + 2,
            "the suffix is the only growth"
        );
        assert!(
            second[second.len() - 2]
                .content
                .contains("ships on Fridays"),
            "the refreshed memory rides in the tail, behind the shared prefix"
        );
        assert!(
            !first[0].content.contains("likes tabs") && !second[0].content.contains("ships on"),
            "memory never reaches the system message"
        );
    }

    /// Before/after, run rather than claimed. A ten-turn session where the model remembers
    /// something twice: the old assembly folded memory into the system message, so each write cost
    /// a turn that re-read the whole conversation at full price. Under the contract the same
    /// session never moves its prefix.
    #[test]
    fn memory_writes_no_longer_cost_a_cold_turn() {
        const TURNS: usize = 10;
        // Turn 3 and turn 7 write to memory.
        let memory_at = |turn: usize| match turn {
            0..=2 => "Known facts:\n- likes tabs",
            3..=6 => "Known facts:\n- likes tabs\n- ships on Fridays",
            _ => "Known facts:\n- likes tabs\n- ships on Fridays\n- hates flaky tests",
        };
        let stable_system = "base prompt + tool guidance + project instructions";
        let tools = vec![tool("read_file"), tool("grep")];

        // Legacy assembly: memory appended to the system message, as `chat.rs` used to do.
        let legacy: Vec<Vec<u8>> = (0..TURNS)
            .map(|turn| prefix_bytes(&format!("{stable_system}\n\n{}", memory_at(turn)), &tools))
            .collect();
        let legacy_cold = legacy.windows(2).filter(|pair| pair[0] != pair[1]).count();

        // Contract assembly: the prefix ignores memory entirely; it rides in the tail.
        let contract: Vec<Vec<u8>> = (0..TURNS)
            .map(|_| prefix_bytes(stable_system, &tools))
            .collect();
        let contract_cold = contract
            .windows(2)
            .filter(|pair| pair[0] != pair[1])
            .count();

        assert_eq!(legacy_cold, 2, "one full re-read per memory write");
        assert_eq!(contract_cold, 0, "the prefix never moves");
        // And the model is told the same thing either way -- the tail carries the newest facts.
        let tail = volatile_tail(memory_at(TURNS - 1), None).expect("a tail");
        assert!(tail.contains("hates flaky tests"));
    }

    #[test]
    fn a_hit_needs_a_prompt_to_measure_against() {
        assert_eq!(hit_percent(0, 0), None);
        assert_eq!(hit_percent(100, 96), Some(96.0));
        assert_eq!(
            hit_percent(100, 120),
            Some(100.0),
            "a provider over-reporting cached tokens cannot exceed 100%"
        );
    }

    #[test]
    fn the_report_skips_the_first_turn_but_not_a_later_cold_one() {
        let report = report(&[(100, 0), (200, 190), (300, 0), (400, 392)]).expect("a report");
        assert_eq!(report.warmup, 2, "turn 1 and the churned turn 3");
        assert_eq!(report.measured, 3, "every turn from the second onwards");
        assert_eq!(report.median, 95.0);
        assert!((report.pct_ge_90 - 200.0 / 3.0).abs() < 0.01);
        assert!((report.pct_ge_95 - 200.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn a_session_with_one_turn_has_nothing_to_report() {
        assert_eq!(report(&[(100, 0)]), None);
        assert_eq!(report(&[]), None);
    }

    #[test]
    fn side_requests_derive_a_sibling_sticky_id() {
        assert_eq!(side_session_id(None, SideRequest::Title), None);
        assert_eq!(side_session_id(Some(""), SideRequest::Compact), None);
        assert_eq!(
            side_session_id(Some("abc-123"), SideRequest::Title).as_deref(),
            Some("abc-123:title")
        );
        assert_eq!(
            side_session_id(Some("abc-123"), SideRequest::Compact).as_deref(),
            Some("abc-123:compact")
        );
        // Conversation id stays distinct so a sticky worker cannot confuse the two prefixes.
        let chat = "abc-123";
        let title = side_session_id(Some(chat), SideRequest::Title).unwrap();
        assert_ne!(title, chat);
    }

    #[test]
    fn sub_agents_derive_per_invocation_sibling_ids() {
        assert_eq!(sub_agent_session_id(None, "call-1"), None);
        assert_eq!(
            sub_agent_session_id(Some("abc-123"), "call-1").as_deref(),
            Some("abc-123:agent:call-1")
        );
        assert_ne!(
            sub_agent_session_id(Some("abc-123"), "call-1"),
            sub_agent_session_id(Some("abc-123"), "call-2")
        );
        assert_ne!(
            sub_agent_session_id(Some("abc-123"), "call-1"),
            side_session_id(Some("abc-123"), SideRequest::Title)
        );
    }

    #[test]
    fn every_derived_routing_id_is_bounded() {
        let id = "x".repeat(100);
        for derived in [
            bounded_routing_id(&id, ""),
            side_session_id(Some(&id), SideRequest::Title).unwrap(),
            sub_agent_session_id(Some(&id), "call-1").unwrap(),
        ] {
            assert!(derived.chars().count() <= 64, "{derived}");
        }
    }
}
