//! Session-level AI title runner (ADR-0022): the LLM-backed side of the
//! [`TITLE`] profile.
//!
//! Mirrors the split established by [`crate::session_review`]: the domain
//! vocabulary and the pure post-processing ([`clean_title`]) live in
//! `neenee-core`, while the provider call lives here next to the `Agent`.
//! Like `session_review` this runs a bounded subagent of the primary agent,
//! but the title task is pure text-in/text-out — it needs no tools and no
//! ReAct loop — so the runner is a single `Provider::chat` call framed by
//! the [`TITLE`] profile's system prompt, not a full
//! [`Agent::run_streaming_with_events`] turn. The model is told to output
//! only the title; any tool calls it (incorrectly) emits are ignored, since
//! only [`Message::content`] is read.
//!
//! ## Lifecycle
//!
//! [`Agent::generate_title`] returns a cleaned title or `None` (on provider
//! error, timeout, or an unparseable answer). Callers decide whether to store
//! it: the first-turn auto-trigger and the on-demand `/title` refresh both
//! skip the write when the stored title is manual (ADR-0022's lock rule).
//!
//! [`clean_title`]: neenee_core::clean_title
//! [`TITLE`]: neenee_core::TITLE

use std::time::Duration;

#[cfg(test)]
use neenee_core::Provider;
use neenee_core::{Message, Role, TITLE, clean_title};

use crate::agent::Agent;

/// Character budget for the transcript excerpt handed to the title subagent.
/// Generous enough to show the opening request and the recent arc (so an
/// on-demand regen after a topic shift sees the new direction), bounded enough
/// that the call stays cheap. The opening user message is always included in
/// full because the title usually captures what the session is *about*.
const TRANSCRIPT_BUDGET_CHARS: usize = 2_000;

/// Wall-clock budget for the title call. Title generation is best-effort: a
/// stalled endpoint must not leak a background task forever. On timeout the
/// caller simply gets `None` and the session keeps whatever title it had (or
/// the first-user-message fallback).
const TITLE_CALL_TIMEOUT: Duration = Duration::from_secs(45);

impl Agent {
    /// Generate a session title from `transcript`, or `None` on failure.
    ///
    /// A single `Provider::chat` call framed by the `TITLE` profile's
    /// system prompt. The transcript is condensed to a compact excerpt
    /// (`serialize_for_title`); the model's free-form answer is normalized
    /// by [`clean_title`]. Best-effort throughout: provider errors, timeouts,
    /// and empty/unparseable answers all return `None` so the caller can leave
    /// the stored title untouched.
    ///
    /// Runs against the session's own provider (`self.provider`), matching how
    /// `run_session_review` shares the primary provider — neenee's catalog has
    /// no "small model" concept, so a dedicated cheap channel is out of scope
    /// (ADR-0022).
    pub async fn generate_title(&self, transcript: &[Message]) -> Option<String> {
        let excerpt = serialize_for_title(transcript, TRANSCRIPT_BUDGET_CHARS);
        if excerpt.trim().is_empty() {
            return None;
        }
        let messages = vec![
            Message::new(Role::System, TITLE.system_prompt),
            Message::new(
                Role::User,
                format!("Generate a title for this conversation:\n\n{excerpt}"),
            ),
        ];
        // The title task is pure text-in/text-out. The primary turn has
        // already finished when this runs (auto-trigger is post-turn; `/title`
        // is on-demand at idle), so the shared provider is not mid-request.
        // We do not call `prepare_tools`: a stale toolset from the prior turn
        // may be advertised, but the model is instructed to output only a
        // title and only `content` is read, so any tool call it emits is
        // ignored.
        let response =
            match tokio::time::timeout(TITLE_CALL_TIMEOUT, self.provider.chat(messages)).await {
                Ok(Ok(message)) => message,
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "title subagent provider call failed");
                    return None;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = TITLE_CALL_TIMEOUT.as_secs(),
                        "title subagent call timed out"
                    );
                    return None;
                }
            };
        clean_title(&response.content)
    }
}

/// Render `transcript` as a compact excerpt for the title prompt.
///
/// The opening user message is always included in full (the title usually
/// captures what the session is *about*). Subsequent user/assistant turns are
/// then appended oldest-to-newest, each capped, until the budget is exhausted
/// — so a first-turn session shows its single exchange, and an on-demand regen
/// shows the opening plus the recent arc. System and tool-role messages are
/// dropped: they carry no signal for a one-line title.
fn serialize_for_title(transcript: &[Message], budget: usize) -> String {
    // Opening user message, in full, as the anchor.
    let mut opening: Option<&str> = None;
    for message in transcript {
        if message.role == Role::User && !message.hidden {
            opening = Some(message.content.as_str());
            break;
        }
    }

    // A title captures what the session is *about*, and that is anchored on
    // the user's opening message. A transcript with no user traffic (e.g. only
    // system/assistant turns) carries no titleable intent, so it serializes to
    // empty — `generate_title` then returns `None` and the first-user-message
    // fallback keeps rendering.
    if opening.is_none() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    for message in transcript {
        if message.hidden || matches!(message.role, Role::System | Role::Tool) {
            continue;
        }
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            _ => continue,
        };
        let content = message.content.trim();
        if content.is_empty() {
            continue;
        }
        // Keep the opening user line unshortened (it is the primary signal);
        // cap later turns so the recent arc fits alongside it.
        let is_opening = opening.is_some_and(|o| std::ptr::eq(o, content))
            || opening.is_some_and(|o| o == content);
        let cap = if is_opening { usize::MAX } else { 200 };
        let body = truncate(content, cap);
        let line = format!("{role}: {body}");
        total = total.saturating_add(line.len() + 1);
        if total > budget && !lines.is_empty() {
            break;
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn truncate(s: &str, max: usize) -> &str {
    let s = s.trim();
    if s.chars().count() <= max {
        return s;
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// A provider double that returns a canned assistant message. Captures the
    /// last messages it was handed so a test can assert the title prompt shape.
    struct CannedProvider {
        reply: String,
        last_messages: Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl Provider for CannedProvider {
        async fn chat(&self, messages: Vec<Message>) -> Result<Message, String> {
            *self.last_messages.lock().unwrap() = messages;
            Ok(Message::new(Role::Assistant, self.reply.clone()))
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    async fn agent_with_reply(reply: &str) -> (Agent, Arc<CannedProvider>) {
        let provider = Arc::new(CannedProvider {
            reply: reply.to_string(),
            last_messages: Mutex::new(Vec::new()),
        });
        let agent = Agent::new(
            provider.clone(),
            Vec::new(),
            crate::skills::SkillRegistry::empty(),
            crate::AgentIdentity::default(),
        );
        (agent, provider)
    }

    fn user(content: &str) -> Message {
        Message::new(Role::User, content)
    }
    fn assistant(content: &str) -> Message {
        Message::new(Role::Assistant, content)
    }

    #[tokio::test]
    async fn generate_title_returns_cleaned_model_output() {
        let (agent, _) = agent_with_reply("Fix login button on mobile").await;
        let transcript = vec![user("the login button on mobile is broken, help")];
        let title = agent.generate_title(&transcript).await;
        assert_eq!(title.as_deref(), Some("Fix login button on mobile"));
    }

    #[tokio::test]
    async fn generate_title_normalizes_wrapped_output() {
        let (agent, _) = agent_with_reply("```json\n\"Refactor auth\"```").await;
        let title = agent
            .generate_title(&transcript_of_opening("refactor the auth"))
            .await;
        assert_eq!(title.as_deref(), Some("Refactor auth"));
    }

    #[tokio::test]
    async fn generate_title_none_on_empty_model_output() {
        let (agent, _) = agent_with_reply("   \n  ").await;
        let title = agent.generate_title(&transcript_of_opening("hi")).await;
        assert!(title.is_none());
    }

    #[tokio::test]
    async fn generate_title_none_on_empty_transcript() {
        let (agent, _) = agent_with_reply("anything").await;
        assert!(agent.generate_title(&[]).await.is_none());
    }

    #[tokio::test]
    async fn generate_title_uses_title_profile_system_prompt() {
        let (agent, provider) = agent_with_reply("Some title").await;
        let _ = agent
            .generate_title(&transcript_of_opening("a session about rust"))
            .await;
        let messages = provider.last_messages.lock().unwrap().clone();
        let system = messages
            .iter()
            .find(|m| m.role == Role::System)
            .expect("system message present");
        assert_eq!(system.content, TITLE.system_prompt);
    }

    fn transcript_of_opening(opening: &str) -> Vec<Message> {
        vec![user(opening), assistant("sure, let me help")]
    }

    #[test]
    fn serialize_includes_opening_in_full() {
        let long = format!("the opening: {}", "x".repeat(500));
        let transcript = vec![user(&long), assistant("reply"), user("second turn")];
        let out = serialize_for_title(&transcript, TRANSCRIPT_BUDGET_CHARS);
        assert!(out.contains(&long), "opening must be unshortened");
        assert!(out.contains("second turn"));
    }

    #[test]
    fn serialize_drops_system_and_tool_messages() {
        let transcript = vec![
            Message::new(Role::System, "you are an agent"),
            user("hello"),
            Message::new(Role::Tool, "tool result"),
        ];
        let out = serialize_for_title(&transcript, TRANSCRIPT_BUDGET_CHARS);
        assert!(!out.contains("you are an agent"));
        assert!(!out.contains("tool result"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn serialize_is_empty_for_no_user_traffic() {
        let transcript = vec![Message::new(Role::System, "sys"), assistant("hi")];
        let out = serialize_for_title(&transcript, TRANSCRIPT_BUDGET_CHARS);
        assert!(out.trim().is_empty());
    }
}
