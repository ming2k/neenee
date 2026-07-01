//! Session-review runner (ADR-0018, superseding the periodic ADR-0016 design):
//! the orchestration side of the on-demand transcript diagnostic.
//!
//! Domain types ([`SessionReview`], [`ReviewVerdict`]) live in `neenee-core`;
//! this module owns the LLM-backed runner that lives next to [`crate::EnvoyTool`]
//! because — like the `task` tool — it spawns a bounded read-only envoy via
//! [`crate::Agent`]. The difference is who drives it: `task` is a *model* tool
//! call, whereas the review runner is *user* driven, fired by the `/review`
//! command ([`Agent::review_now`]) rather than on a round cadence.
//!
//! ## The built-in dimension
//!
//! [`LoopingReview`] is the first registered dimension ("is the agent stuck in
//! an exploration loop?"). Adding a dimension is a new [`SessionReview`] impl
//! registered on the agent — no dispatch changes, no extra model calls, since
//! the runner asks one envoy to verdict every dimension at once.

use std::sync::Arc;

use neenee_core::{
    DEFAULT_REVIEWER_HARD_STOP, Message, REVIEW, ReviewStatus, ReviewVerdict, Role, SessionReview,
};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::skills::SkillRegistry;

/// Character budget for the transcript snapshot handed to the diagnostic
/// envoy. Keeps the reviewer's prompt cheap while still showing enough
/// recent tool traffic to judge progress. The most recent messages are kept.
const TRANSCRIPT_SNAPSHOT_BUDGET_CHARS: usize = 8_000;

/// The first session-review dimension: is the agent stuck in an unproductive
/// exploration loop? Distinct from a model that is legitimately reading its
/// way through a large task — the reviewer is asked to tell those apart from
/// the transcript, which a dumb round counter cannot.
#[derive(Debug, Default)]
pub struct LoopingReview;

impl SessionReview for LoopingReview {
    fn id(&self) -> &'static str {
        "looping"
    }
    fn label(&self) -> &'static str {
        "Exploration loop"
    }
    fn instruction(&self) -> &'static str {
        "Is the agent stuck in an unproductive loop — repeating the same or \
         similar read-only actions without making changes or converging on an \
         answer? Distinguish a genuinely stuck loop from a model that is \
         methodically working through a large but productive task. Consider \
         whether the same files or queries are being revisited and whether any \
         edit or command has actually landed."
    }
}

/// The default set of review dimensions registered on a primary agent.
/// Envoys register none (they have no `/review` path), so this is only
/// consulted when [`Agent::review_now`] runs.
pub fn default_reviews() -> Vec<Arc<dyn SessionReview>> {
    vec![Arc::new(LoopingReview)]
}

impl Agent {
    /// Run the periodic session-review diagnostic against the live transcript
    /// snapshot and return one verdict per registered dimension.
    ///
    /// Spawns a bounded read-only envoy (the [`REVIEW`] profile) with its
    /// own review disabled (so it cannot recurse) and a tight hard stop (so a
    /// runaway reviewer cannot loop). The envoy reasons over a compact,
    /// most-recent-first transcript excerpt and returns structured verdicts.
    ///
    /// Failures are deliberately soft: a provider error or unparseable answer
    /// degrades to a single `Watch` verdict carrying the raw text rather than
    /// silently reporting healthy — the signal is worth surfacing even when
    /// degraded, but it never escalates to a `Stuck` nudge without an explicit
    /// verdict.
    pub(crate) async fn run_session_review(
        &self,
        messages: &[Message],
        tool_rounds: usize,
    ) -> Vec<ReviewVerdict> {
        let dimensions = self.effective_reviews();
        if dimensions.is_empty() {
            return Vec::new();
        }

        // Read-only, non-interactive, non-recursive toolset — the reviewer may
        // open a file to check a looping claim but cannot mutate anything or
        // spawn further agents. The reviewer runs on the same model as the
        // parent, so it carries the parent's model selection (variant overrides
        // + hard capability limits); the REVIEW profile narrows scope to the
        // read-only tools. `resolve_tools` composes both off the full pool.
        let model = neenee_core::resolve_model(&self.provider.model());
        let model_sel = neenee_core::ToolSelection::unrestricted().with_variants(
            self.variant_selection_handle()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        );
        let sub_tools = REVIEW.resolve_tools(&self.toolset, &model, &model_sel);
        let mut reviewer = Agent::new(
            self.provider.clone(),
            sub_tools,
            SkillRegistry::empty(),
            crate::AgentIdentity::default(),
        );
        // The reviewer must not run its own reviews (recursion) and is bounded
        // by a tight hard stop so it cannot loop. It registers no review
        // dimensions of its own.
        reviewer.set_hard_stop_turns(DEFAULT_REVIEWER_HARD_STOP);
        // The reviewer reads a transcript excerpt; disable the deterministic
        // read-loop guard's nudge (ADR-0034) so its own reads are never steered.
        reviewer.set_nudge_config(neenee_core::NudgeConfig::disabled());
        // The reviewer's head system message is the review composition (persona
        // + dimensions + JSON contract), not the default mission-neutral set.
        // Installed as a dedicated registry so `ensure_system_prompt` rebuilds
        // it correctly every round (ADR-0039 stage 6) — previously a pre-seeded
        // system message here was clobbered on round 1 and the review prompt
        // never reached the model.
        reviewer.set_prompt_registry(crate::prompt::reviewer_prompt_registry(&dimensions));

        let transcript = serialize_transcript(messages, TRANSCRIPT_SNAPSHOT_BUDGET_CHARS);
        let user = format!(
            "The agent under review has completed {tool_rounds} tool rounds this turn. \
             Here is a compact, most-recent-last snapshot of its transcript:\n\n\
             {transcript}\n\n\
             Evaluate every dimension listed above and return the JSON object now."
        );
        // The transcript is the user message; the head system message is built
        // by `ensure_system_prompt` from the reviewer registry above. Starting
        // without a pre-seeded system message avoids the round-1 clobber that
        // lost the review prompt before ADR-0039 stage 6.
        let mut child_messages = vec![Message::new(Role::User, user)];

        let cancel = CancellationToken::new();
        // Box the recursive call: `run_session_review` is reached from inside
        // a turn loop, and the reviewer runs that same turn loop, so without
        // indirection the future would be infinitely sized.
        let result =
            Box::pin(reviewer.run_streaming_with_events(&mut child_messages, &cancel, |_| {}))
                .await;

        match result {
            Ok(outcome) => parse_verdicts(&outcome.message.content, &dimensions),
            Err(err) => {
                tracing::warn!(error = %err, "session-review envoy failed");
                vec![ReviewVerdict {
                    dimension: "review".to_string(),
                    status: ReviewStatus::Watch,
                    detail: format!("reviewer error: {err}"),
                }]
            }
        }
    }
}

/// Flatten the transcript into a compact text excerpt the reviewer can read in
/// one glance. Keeps the most recent messages within `budget` chars (older
/// traffic is dropped from the front once the budget is exceeded) because the
/// signal for "stuck now" lives in recent rounds, not the turn's opening.
///
/// Tool calls are summarised by name + arguments; results are truncated to a
/// short prefix so the reviewer sees *what* was called and *whether* it
/// produced output, without the full payload blowing the prompt.
fn serialize_transcript(messages: &[Message], budget: usize) -> String {
    /// One flattened line per message, newest last.
    fn line_for(msg: &Message) -> String {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let mut parts = vec![format!("[{role}]")];
        if !msg.content.trim().is_empty() {
            parts.push(truncate(&msg.content, 160));
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                parts.push(format!(
                    "call {}({})",
                    call.name,
                    truncate(&call.arguments, 80)
                ));
            }
        }
        if role == "tool" {
            // Tool-role messages carry their result in content; keep a taste.
            if msg.content.trim().is_empty() {
                parts.push("<empty result>".to_string());
            }
        }
        parts.join(" ")
    }

    let lines: Vec<String> = messages
        .iter()
        .filter(|m| !m.hidden && m.role != Role::System)
        .map(line_for)
        .collect();
    if lines.is_empty() {
        return "(no visible tool traffic yet)".to_string();
    }
    // Keep the most recent lines within the budget. Walk newest-first, stop
    // once adding another line would exceed the budget, then reverse so the
    // excerpt reads oldest-to-newest (natural reading order).
    let mut kept: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for line in lines.iter().rev() {
        let cost = line.len() + 1; // +1 for the joining newline
        if total + cost > budget && !kept.is_empty() {
            break;
        }
        total += cost;
        kept.push(line.as_str());
    }
    kept.reverse();
    kept.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Parse the reviewer's JSON response into verdicts, keyed back to the
/// registered dimensions. Unknown dimensions in the response are dropped;
/// dimensions the response omitted get a healthy default so a partial answer
/// never reads as "all clear" falsely. On any parse failure, fall back to a
/// single `Watch` verdict carrying the raw text — degraded but not silent,
/// and never an unstated `Stuck`.
fn parse_verdicts(raw: &str, dimensions: &[Arc<dyn SessionReview>]) -> Vec<ReviewVerdict> {
    #[derive(serde::Deserialize)]
    struct Payload {
        #[serde(default)]
        verdicts: Vec<RawVerdict>,
    }
    #[derive(serde::Deserialize)]
    struct RawVerdict {
        dimension: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        detail: String,
    }

    let cleaned = strip_code_fence(raw).trim();
    match serde_json::from_str::<Payload>(cleaned) {
        Ok(payload) => {
            let by_id: std::collections::HashMap<&str, &RawVerdict> = payload
                .verdicts
                .iter()
                .map(|v| (v.dimension.as_str(), v))
                .collect();
            dimensions
                .iter()
                .map(|dim| match by_id.get(dim.id()) {
                    Some(raw) => ReviewVerdict {
                        dimension: dim.id().to_string(),
                        status: parse_status(&raw.status),
                        detail: raw.detail.clone(),
                    },
                    None => ReviewVerdict::healthy(dim.id()),
                })
                .collect()
        }
        Err(_) => {
            // Unparseable: degrade to a Watch with the raw text so the user
            // still sees *something* happened, but no Stuck nudge fires.
            vec![ReviewVerdict {
                dimension: "review".to_string(),
                status: ReviewStatus::Watch,
                detail: truncate(raw.trim(), 120),
            }]
        }
    }
}

fn parse_status(s: &str) -> ReviewStatus {
    match s.trim().to_ascii_lowercase().as_str() {
        "stuck" | "loop" | "looping" => ReviewStatus::Stuck,
        "watch" | "slow" | "risky" | "warning" => ReviewStatus::Watch,
        _ => ReviewStatus::Healthy,
    }
}

/// Strip a single surrounding ``` fence if the model wrapped its JSON despite
/// being told not to. Only the outermost fence is removed so nested content is
/// untouched.
fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        // Skip an optional language tag on the opening fence (```json).
        let after_tag = match after_open.find('\n') {
            Some(idx) => &after_open[idx + 1..],
            None => after_open,
        };
        if let Some(end) = after_tag.rfind("```") {
            return after_tag[..end].trim();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdicts_maps_known_dimensions() {
        let dims: Vec<Arc<dyn SessionReview>> = default_reviews();
        let raw = r#"{"verdicts":[{"dimension":"looping","status":"stuck","detail":"re-reading f.rs repeatedly"}]}"#;
        let verdicts = parse_verdicts(raw, &dims);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].dimension, "looping");
        assert_eq!(verdicts[0].status, ReviewStatus::Stuck);
    }

    #[test]
    fn parse_verdicts_fills_missing_dimensions_as_healthy() {
        let dims: Vec<Arc<dyn SessionReview>> = default_reviews();
        let raw = r#"{"verdicts":[]}"#;
        let verdicts = parse_verdicts(raw, &dims);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].status, ReviewStatus::Healthy);
    }

    #[test]
    fn parse_verdicts_degrades_unparseable_to_watch() {
        let dims: Vec<Arc<dyn SessionReview>> = default_reviews();
        let verdicts = parse_verdicts("the agent seems fine overall", &dims);
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].status, ReviewStatus::Watch);
        assert_eq!(verdicts[0].dimension, "review");
    }

    #[test]
    fn strip_code_fence_removes_json_fence() {
        assert_eq!(
            strip_code_fence("```json\n{\"verdicts\":[]}\n```"),
            "{\"verdicts\":[]}"
        );
        assert_eq!(strip_code_fence("{\"verdicts\":[]}"), "{\"verdicts\":[]}");
    }

    #[test]
    fn serialize_transcript_keeps_recent_within_budget() {
        let msgs: Vec<Message> = (0..50)
            .map(|i| {
                let mut m = Message::new(
                    Role::User,
                    format!("round {i} with {}", "padding ".repeat(20)),
                );
                m.tool_calls = Some(vec![neenee_core::ToolCall {
                    id: format!("c{i}"),
                    name: "read_text".to_string(),
                    arguments: format!("{{\"path\":\"f{i}\"}}"),
                }]);
                m
            })
            .collect();
        let out = serialize_transcript(&msgs, 500);
        // Budget honoured and the newest round survived the cut.
        assert!(out.len() <= 600);
        assert!(out.contains("round 49"));
        assert!(!out.contains("round 0"));
    }
}
