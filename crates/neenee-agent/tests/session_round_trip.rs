//! End-to-end session-persistence round-trip.
//!
//! Inline unit tests inside `neenee-agent` and `neenee-store` cover each half
//! of this flow in isolation. The purpose of this file is to verify the seam
//! between them composes correctly: a turn driven against a fresh on-disk
//! `SessionStore` (via `execute_turn`) must leave enough state on disk that a
//! brand-new `SessionStore` opened at the same path can `resume` the saved id
//! and recover the exact message sequence.
//!
//! This is the kind of regression that no inline test catches: a change to the
//! session event format or to `execute_turn`'s save points can leave both
//! halves internally consistent while breaking the round-trip.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use neenee_agent::orchestration::{execute_turn, CompactionSettings, TurnContext, TurnInput};
use neenee_agent::skills::SkillRegistry;
use neenee_agent::Agent;
use neenee_core::{AgentMode, PursuitService, PursuitStore, Role};
use neenee_providers::MockProvider;
use neenee_store::session::SessionStore;

/// Concatenation of the chunks emitted by `MockProvider::stream_chat`. Kept
/// here rather than imported so a change to the mock's payload shows up as a
/// test diff rather than a silent recompile.
const MOCK_REPLY: &str = "This is a streaming mock response from neenee!";

#[tokio::test]
async fn execute_turn_persists_a_session_that_resume_reopens() {
    let directory = std::env::temp_dir().join(format!(
        "neenee-it-session-roundtrip-{}",
        uuid::Uuid::new_v4()
    ));
    let session_path = directory.join("session.json");
    let session = Arc::new(SessionStore::for_path(session_path.clone()));
    let pursuit_service =
        PursuitService::new(PursuitStore::open_in_memory_blocking().expect("in-memory pursuit store"));
    let agent = Arc::new(Agent::new(
        Arc::new(MockProvider),
        Vec::new(),
        AgentMode::Build,
        pursuit_service.clone(),
        SkillRegistry::empty(),
    ));
    let (tx, _rx) = mpsc::unbounded_channel();

    let prompt = "hello, mock";
    let completed = execute_turn(
        TurnContext {
            agent: agent.clone(),
            history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            tx,
            token: CancellationToken::new(),
            session: session.clone(),
            pursuit_service,
            compaction: CompactionSettings {
                max_chars: 100_000,
                preserve_turns: 6,
                summarize: false,
                prune: false,
                prune_protect_chars: 0,
            },
            retry_max_attempts: 1,
            retry_base_ms: 1,
            retry_max_ms: 1,
        },
        TurnInput {
            prompt: prompt.to_string(),
            hidden: false,
            display_prompt: None,
            images: Vec::new(),
        },
    )
    .await
    .expect("turn completes with the mock provider");

    // The bool return is pursuit-completion (the model emitted the marker AND the
    // pursuit checklist allows it), not turn-completion. With no pursuit set the
    // value is always false; the turn still ran end to end and persisted.
    assert!(!completed, "no pursuit is set, so completion flag is false");

    // Snapshot the live state before dropping everything.
    let saved_id = session.id().await;
    let live_messages = session.messages().await;
    assert!(
        live_messages
            .iter()
            .any(|message| message.role == Role::User && message.content == prompt),
        "live session should contain the user prompt"
    );
    assert!(
        live_messages
            .iter()
            .any(|message| message.role == Role::Assistant && message.content == MOCK_REPLY),
        "live session should contain the mock assistant reply"
    );

    // Drop all in-memory state. The next line intentionally drops `agent`,
    // `session`, and the channel so the only thing left is the on-disk file.
    drop(agent);
    drop(session);

    // Reopen from disk by id. This is the integration seam: a fresh
    // `SessionStore` at the same path should recover the prior turn when asked
    // to resume the saved id.
    let reopened = SessionStore::for_path(session_path.clone());
    let resumed_id = reopened
        .resume(Some(&saved_id))
        .await
        .expect("resume reopens the saved session by id");
    assert_eq!(resumed_id, saved_id);

    let reopened_messages = reopened.messages().await;
    assert_eq!(
        reopened_messages.len(),
        live_messages.len(),
        "reopened session should have the same message count as the live one"
    );
    for (reopened_message, live_message) in reopened_messages.iter().zip(live_messages.iter()) {
        assert_eq!(reopened_message.role, live_message.role);
        assert_eq!(reopened_message.content, live_message.content);
    }

    let _ = std::fs::remove_dir_all(directory);
}
