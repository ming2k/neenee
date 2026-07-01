use super::*;
use futures::stream::{self, BoxStream};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Serialises tests that mutate process-wide env vars (`NEENEE_DATA_DIR`).
/// Tests that touch env vars MUST take this lock to avoid racing other
/// parallel tests that read paths via [`neenee_store::paths::get`].
static ENV_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestProvider;
struct PermissionTestProvider(AtomicUsize);
struct StreamingToolProvider(AtomicUsize);
struct WriteTestTool;
struct StreamingReadTool(Arc<AtomicUsize>);

#[async_trait]
impl Provider for TestProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Ok(Message::new(Role::Assistant, "done"))
    }

    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        Ok(Box::pin(stream::empty()))
    }
}

#[async_trait]
impl Provider for PermissionTestProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(Message {
                role: Role::Assistant,
                content: String::new(),
                content_blob: None,
                display_content: None,
                reasoning_content: None,
                provider_meta: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call".to_string(),
                    name: "write_test".to_string(),
                    arguments: "{}".to_string(),
                }]),
                tool_call_id: None,
                images: None,
                provider: None,
                model: None,
                hidden: false,
                children: None,
                envoy_meta: None,
                origin: None,
            })
        } else {
            Ok(Message::new(Role::Assistant, "done"))
        }
    }

    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        Ok(Box::pin(stream::empty()))
    }
}

#[async_trait]
impl Provider for StreamingToolProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Err("non-streaming path should not be used".to_string())
    }

    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        Ok(Box::pin(stream::empty()))
    }

    async fn stream_chat_events(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        let events = if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                Ok(ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("stream_".to_string()),
                    arguments: "{\"value\":".to_string(),
                }),
                Ok(ProviderStreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: Some("read".to_string()),
                    arguments: "1}".to_string(),
                }),
            ]
        } else {
            vec![
                Ok(ProviderStreamEvent::TextDelta("do".to_string())),
                Ok(ProviderStreamEvent::TextDelta("ne".to_string())),
            ]
        };
        Ok(Box::pin(stream::iter(events)))
    }
}

#[async_trait]
impl Tool for WriteTestTool {
    fn name(&self) -> &str {
        "write_test"
    }

    fn description(&self) -> &str {
        "test write tool"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn scope_target(&self, _arguments: &str) -> neenee_core::ScopeTarget {
        neenee_core::ScopeTarget::Path(std::path::PathBuf::from("/tmp/test"))
    }

    async fn call(&self, _arguments: &str) -> Result<String, String> {
        Ok("should not run".to_string())
    }
}

#[async_trait]
impl Tool for StreamingReadTool {
    fn name(&self) -> &str {
        "stream_read"
    }

    fn description(&self) -> &str {
        "streaming test tool"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        assert_eq!(arguments, "{\"value\":1}");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("read".to_string())
    }
}

fn agent() -> Agent {
    Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    )
}

fn active_pursuit(objective: &str) -> Pursuit {
    Pursuit {
        objective: objective.to_string(),
        is_complete: false,
    }
}

#[test]
fn pursuit_is_injected_into_system_prompt() {
    let agent = agent();
    agent.set_pursuit(active_pursuit("ship the harness"));

    // Drive the real placement path: rebuild the head system message from
    // live agent state and read it back off the message list (ADR-0039).
    let mut messages: Vec<Message> = Vec::new();
    agent.ensure_system_prompt(&mut messages);
    let prompt = messages[0].content.clone();

    assert!(prompt.contains("ship the harness"));
}

/// Regression for ADR-0039 stage 6: the `/review` reviewer envoy's head
/// system message must actually carry the review composition (REVIEW persona +
/// registered dimensions + JSON contract). Previously the reviewer pre-seeded
/// a system message that `ensure_system_prompt` clobbered on round 1, so none
/// of it reached the model; the reviewer now carries a dedicated registry and
/// `ensure_system_prompt` rebuilds the composition every round.
#[test]
fn reviewer_system_message_carries_persona_dimensions_and_contract() {
    use neenee_core::{REVIEW, Role};

    let reviewer = Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let mut reviewer = reviewer;
    let dimensions = crate::session_review::default_reviews();
    reviewer.set_prompt_registry(crate::prompt::reviewer_prompt_registry(&dimensions));

    // Drive the same placement path the streaming loop uses: the registry
    // composes the head system message from the reviewer's sections.
    let mut messages: Vec<Message> = vec![Message::new(Role::User, "transcript snapshot")];
    reviewer.ensure_system_prompt(&mut messages);

    let system = &messages[0];
    assert_eq!(system.role, Role::System);
    assert!(
        system.content.starts_with(REVIEW.system_prompt),
        "system message should open with the REVIEW persona"
    );
    assert!(
        system.content.contains("Assess each of these dimensions"),
        "the dimensions preamble composes in"
    );
    assert!(
        system.content.contains("`looping`"),
        "the registered 'looping' dimension is listed"
    );
    assert!(
        system.content.contains("Return ONLY a JSON object"),
        "the JSON verdict contract composes in"
    );
    assert_eq!(
        system.origin.as_ref().map(|o| o.kind),
        Some(neenee_core::InjectionKind::SystemPrompt)
    );
}

/// Golden layout test for ADR-0039 stage 2: the registry-assembled system
/// message must reproduce the legacy `parts.join("\n")` layout byte-for-byte
/// for a representative state (identity + pursuit set, no ask_user tool, no
/// skills). The always-on conciseness and persistence sections compose in
/// unconditionally. Sections that need a gap carry their own leading `\n`,
/// so a single-`\n` join yields a stable, readable layout.
#[test]
fn system_prompt_registry_reproduces_legacy_layout() {
    let mut agent = agent();
    // The `agent()` helper ships an empty identity; give it one so the
    // preamble section is active and exercises the full layout.
    agent.identity = crate::AgentIdentity::new("neenee", "an expert AI coding assistant");
    agent.set_pursuit(active_pursuit("ship the harness"));

    let mut messages: Vec<Message> = Vec::new();
    agent.ensure_system_prompt(&mut messages);
    let prompt = &messages[0].content;

    // preamble \n\n conciseness \n todo \n\n persistence \n\n pursuit.
    let expected = "You are neenee, an expert AI coding assistant.\n\
     \n\
     Be concise. Address only the task at hand; skip tangents, greetings, and recaps \
     of what you just did. Scale depth to change size — a one-line answer for a small \
     fix, a short bullet list for a multi-file change. Never paste whole files or \
     before/after blocks; cite file paths and symbol names instead.\n\
     Task tracking: for work that spans multiple steps, use the `todo` tool to lay \
     out the steps up front, then update each item's status with `todo_update` (or \
     `todo` for a full restructure) as you progress — move a step to in_progress \
     when you start it and completed/cancelled the moment it is done. Keep the \
     list honest: it is the single source of truth shown to the user, so don't \
     let it drift from reality. At most one item may be in_progress at a time. \
     Skip the list entirely for single-step requests.\n\
     \n\
     See the task through to a real result in this turn. Don't stop at analysis \
     or a partial fix — carry the work through implementation and verification. \
     If a tool call fails or you hit a blocker, try to resolve it yourself before \
     yielding; only hand back to the user when the work is actually done or you \
     genuinely need their input.\n\
     \n\
     Active harness pursuit (active):\n\
     ship the harness";
    assert_eq!(
        prompt, expected,
        "registry output must match the composed layout"
    );

    // Origin is the channel canonical kind, regardless of how many sections
    // composed the message.
    assert_eq!(
        messages[0].origin.as_ref().map(|o| o.kind),
        Some(crate::InjectionKind::SystemPrompt)
    );
}

#[test]
fn retry_metadata_is_not_exposed_as_public_error_text() {
    let encoded = retryable_error("rate limited", Some(500));
    assert_eq!(public_error_message(&encoded), "rate limited");
    assert_eq!(public_error_message("plain"), "plain");
}

#[test]
fn pursuit_lifecycle_is_explicit() {
    let agent = agent();
    agent.set_pursuit(active_pursuit("verify behavior"));
    assert!(!agent.get_pursuit().unwrap().is_complete);

    let mut completed = active_pursuit("verify behavior");
    completed.is_complete = true;
    agent.set_pursuit(completed);
    assert!(agent.get_pursuit().unwrap().is_complete);

    agent.clear_pursuit();
    assert_eq!(agent.get_pursuit(), None);
}

// ── Pursuit stop-gate ──────────────────────────────────────────────────

#[test]
fn pursuit_gate_is_inert_until_armed() {
    let agent = agent();
    agent.set_pursuit(active_pursuit("ship"));
    let resp = Message::new(Role::Assistant, "working".to_string());
    assert!(!agent.is_pursuit_armed());
    assert!(agent.pursuit_continuation(&resp).is_none());
}

#[test]
fn pursuit_gate_returns_continuation_when_armed_and_active() {
    let agent = agent();
    agent.set_pursuit(active_pursuit("ship the feature"));
    agent.arm_pursuit();
    assert!(agent.is_pursuit_armed());
    assert_eq!(agent.pursuit_iterations(), 0);

    let resp = Message::new(Role::Assistant, "I will keep working".to_string());
    let prompt = agent
        .pursuit_continuation(&resp)
        .expect("armed + active pursuit + no marker => continue");
    assert!(prompt.contains("ship the feature"));
    // The predicate does not bump the counter; the turn loop does, on consume.
    assert_eq!(agent.pursuit_iterations(), 0);
}

#[test]
fn pursuit_gate_lets_turn_end_on_completion_marker() {
    let agent = agent();
    agent.set_pursuit(active_pursuit("ship"));
    agent.arm_pursuit();
    let resp = Message::new(
        Role::Assistant,
        format!("all done {}", crate::PURSUIT_COMPLETE_MARKER),
    );
    assert!(agent.pursuit_continuation(&resp).is_none());
}

#[test]
fn pursuit_gate_lets_turn_end_without_active_pursuit() {
    let agent = agent();
    agent.arm_pursuit();
    let resp = Message::new(Role::Assistant, "working".to_string());
    assert!(agent.pursuit_continuation(&resp).is_none());
}

#[test]
fn pursuit_gate_lets_turn_end_when_pursuit_already_complete() {
    let agent = agent();
    let mut done = active_pursuit("ship");
    done.is_complete = true;
    agent.set_pursuit(done);
    agent.arm_pursuit();
    let resp = Message::new(Role::Assistant, "working".to_string());
    assert!(agent.pursuit_continuation(&resp).is_none());
}

#[test]
fn disarm_pursuit_turns_the_gate_off() {
    let agent = agent();
    agent.set_pursuit(active_pursuit("ship"));
    agent.arm_pursuit();
    let resp = Message::new(Role::Assistant, "working".to_string());
    assert!(agent.pursuit_continuation(&resp).is_some());
    agent.disarm_pursuit();
    assert!(agent.pursuit_continuation(&resp).is_none());
}

#[tokio::test]
async fn streaming_tool_deltas_are_reassembled_and_executed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        Arc::new(StreamingToolProvider(AtomicUsize::new(0))),
        vec![Arc::new(StreamingReadTool(calls.clone()))],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let mut messages = vec![Message::new(Role::User, "run")];
    let mut events = Vec::new();

    let response = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event)
        })
        .await
        .unwrap();

    assert_eq!(response.message.content, "done");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let model_rounds = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelRequestStarted { tool_round } => Some(*tool_round),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(model_rounds, vec![0, 1]);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCall { name, arguments, .. }
            if name == "stream_read" && arguments == "{\"value\":1}"
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::AssistantEnd(content)) if content == "done"
    ));
}

#[tokio::test]
async fn round_persist_fires_at_each_tool_round_boundary() {
    // ADR-0035: the mid-turn save point must fire once per completed tool
    // round, carrying the full history including that round's tool results.
    // `StreamingToolProvider` does two rounds (round 0 = tool call, round 1 =
    // terminal text), so exactly one round boundary is crossed and the
    // callback should see three messages: user prompt + assistant + tool
    // result. The final round (plain text, no tools) does not cross a
    // boundary and must not fire the callback.
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_lengths: Arc<std::sync::Mutex<Vec<usize>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let agent = Agent::new(
        Arc::new(StreamingToolProvider(AtomicUsize::new(0))),
        vec![Arc::new(StreamingReadTool(calls.clone()))],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let seen_for_cb = Arc::clone(&seen_lengths);
    agent.set_turn_persist(Arc::new(move |messages: &[Message]| {
        let len = messages.len();
        seen_for_cb.lock().unwrap().push(len);
        // Snapshot the slice for the 'static future (the closure itself does
        // not borrow; the persistence target is external in production).
        let _ = messages.to_vec();
        Box::pin(async { Ok(()) })
    }));

    let mut messages = vec![Message::new(Role::User, "run")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.message.content, "done");

    // Exactly one boundary crossing (after round 0's tool result). The
    // callback receives the full live history, which includes the
    // `ensure_system_prompt` message at index 0: [system, user, assistant,
    // tool_result] = 4. The final round (plain text, no tools) does not
    // cross a boundary and must not fire the callback.
    let recorded = seen_lengths.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![4],
        "round persist fires once with the full history"
    );
}

/// A provider whose SSE stream never yields and never ends simulates a stalled
/// connection (server stops sending but keeps the socket open). Without an idle
/// timeout the turn loop blocks on `stream.next()` forever — the UI spins/// "running · responding" and only a user interrupt can break it. The
/// `STREAM_IDLE_TIMEOUT` guard surfaces this as a retryable error instead.
/// `start_paused` makes tokio auto-advance the clock past the 120 s bound so
/// the test is instantaneous.
#[tokio::test(start_paused = true)]
async fn stalled_provider_stream_times_out_as_retryable() {
    struct StalledStreamProvider;
    #[async_trait]
    impl Provider for StalledStreamProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            unreachable!("streaming path should be used")
        }
        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }
        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
            Ok(Box::pin(stream::pending()))
        }
    }

    let agent = Agent::new(
        Arc::new(StalledStreamProvider),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let mut messages = vec![Message::new(Role::User, "hello")];

    let result = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;

    assert!(
        matches!(result, Err(HarnessError::Retryable { .. })),
        "a stalled stream should surface as a retryable error, not hang forever; got: {result:?}"
    );
}

/// A provider whose `stream_chat_events` future never resolves simulates a
/// server that accepts the TCP connection but never sends HTTP response
/// headers (overloaded upstream, dropped proxy). Without the idle-timeout on
/// the outer select the turn would hang on `.send()` forever. `start_paused`
/// advances the clock past `STREAM_IDLE_TIMEOUT` instantly.
#[tokio::test(start_paused = true)]
async fn stream_request_that_never_resolves_times_out() {
    use std::future::pending;

    struct PendingStreamProvider;
    #[async_trait]
    impl Provider for PendingStreamProvider {
        async fn chat(&self, _: Vec<Message>) -> Result<Message, String> {
            unreachable!("streaming path should be used")
        }
        async fn stream_chat(
            &self,
            _: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            unreachable!("stream_chat_events should be called directly")
        }
        async fn stream_chat_events(
            &self,
            _: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
            // Never resolves.
            pending().await
        }
    }

    let agent = Agent::new(
        Arc::new(PendingStreamProvider),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let mut messages = vec![Message::new(Role::User, "hello")];

    let result = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;

    assert!(
        matches!(result, Err(HarnessError::Retryable { .. })),
        "a stream request that never resolves should time out as retryable; got: {result:?}"
    );
}

/// A provider whose non-streaming `chat()` never resolves simulates a stalled
/// endpoint during the non-streaming ReAct path (used by `Agent::run` and
/// compaction). Without a timeout the call blocks forever.
#[tokio::test(start_paused = true)]
async fn non_streaming_chat_that_never_resolves_times_out() {
    use std::future::pending;

    struct PendingChatProvider;
    #[async_trait]
    impl Provider for PendingChatProvider {
        async fn chat(&self, _: Vec<Message>) -> Result<Message, String> {
            pending().await
        }
        async fn stream_chat(
            &self,
            _: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }
    }

    let agent = Agent::new(
        Arc::new(PendingChatProvider),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let mut messages = vec![Message::new(Role::User, "hello")];

    let result = agent
        .run_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;

    assert!(
        matches!(result, Err(HarnessError::Retryable { .. })),
        "a non-streaming chat that never resolves should time out as retryable; got: {result:?}"
    );
}

/// A reasoning model may emit reasoning deltas but no text and no tool call
/// (e.g. a truncated or cut-off response). Before the fix to
/// [`valid_assistant_response`], such a response was incorrectly classified
/// as an empty assistant response and surfaced as a terminal error.
/// Reasoning is a legitimate payload from reasoning-model providers, so the
/// turn should complete normally instead of erroring.
#[tokio::test]
async fn reasoning_only_response_is_accepted_not_treated_as_empty() {
    struct ReasoningOnlyProvider;
    #[async_trait]
    impl Provider for ReasoningOnlyProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            unreachable!("streaming path should be used")
        }
        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }
        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
            Ok(Box::pin(stream::iter(vec![Ok(
                ProviderStreamEvent::ReasoningDelta("let me think...".to_string()),
            )])))
        }
    }

    let agent = Agent::new(
        Arc::new(ReasoningOnlyProvider),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_nudge_config(neenee_core::NudgeConfig {
        enabled: true,
        ..neenee_core::NudgeConfig::default()
    });

    let mut messages = vec![Message::new(Role::User, "go")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;

    let outcome = outcome.expect("reasoning-only response should not be treated as empty");
    assert_eq!(outcome.message.content, "");
    assert_eq!(
        outcome.message.reasoning_content.as_deref(),
        Some("let me think...")
    );
}

#[tokio::test]
async fn cancelling_during_tool_execution_emits_tool_cancelled() {
    use std::future::pending;
    use std::sync::Mutex;
    use tokio::sync::Notify;

    struct BlockingTool {
        started: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for BlockingTool {
        fn name(&self) -> &str {
            "stream_read"
        }
        fn description(&self) -> &str {
            "blocks until the turn is cancelled"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            self.started.notify_one();
            let _: () = pending().await;
            unreachable!("the turn is cancelled before this returns")
        }
    }

    let started = Arc::new(Notify::new());
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Arc::new(StreamingToolProvider(AtomicUsize::new(0))),
        vec![Arc::new(BlockingTool {
            started: started.clone(),
        })],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let token = CancellationToken::new();
    let mut messages = vec![Message::new(Role::User, "run")];
    let events_for_run = events.clone();

    let run_token = token.clone();
    let handle = tokio::spawn(async move {
        agent
            .run_streaming_with_events(&mut messages, &run_token, |event| {
                if let Ok(mut guard) = events_for_run.lock() {
                    guard.push(event);
                }
            })
            .await
    });

    // Wait until the tool is actually in flight, then interrupt.
    started.notified().await;
    token.cancel();

    let outcome = handle.await.expect("turn task panicked");
    assert!(
        matches!(outcome, Err(HarnessError::Interrupted)),
        "expected the turn to be interrupted, got {outcome:?}"
    );

    let recorded = events.lock().expect("events lock poisoned").clone();
    // Every announced ToolCall converges on a terminal event: here a
    // ToolCancelled, never a ToolResult (the turn was aborted).
    assert!(recorded.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCancelled { name, .. } if name == "stream_read"
    )));
    assert!(
        !recorded
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolResult { .. }))
    );
    assert!(
        recorded.iter().any(
            |event| matches!(event, AgentEvent::ToolCall { name, .. } if name == "stream_read")
        )
    );
}

#[tokio::test]
async fn write_tool_waits_for_permission_and_always_is_cached() {
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    ));
    let call = ToolCall {
        id: "call".to_string(),
        name: "write_test".to_string(),
        arguments: "{}".to_string(),
    };
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task_agent = agent.clone();
    let task_call = call.clone();
    let task = tokio::spawn(async move {
        task_agent
            .execute_tool_evented(
                &task_call,
                "call",
                &CancellationToken::new(),
                &mut |event| {
                    let _ = event_tx.send(event);
                },
            )
            .await
    });

    let request = match event_rx.recv().await.unwrap() {
        AgentEvent::PermissionRequest(request) => request,
        event => panic!("unexpected event: {:?}", event),
    };
    // The default `permission_label`/`permission_description` fall back to
    // the tool's name/description, which the request must carry verbatim
    // (regression for the `PermissionRequest.label` wiring).
    assert_eq!(request.tool, "write_test");
    assert_eq!(request.label, "write_test");
    assert_eq!(request.description, "test write tool");
    assert!(!task.is_finished());
    assert!(agent.reply_permission(&request.id, PermissionDecision::Always));
    assert_eq!(task.await.unwrap().unwrap().to_text(), "should not run");
    assert_eq!(
        agent.allowed_tools(),
        vec!["write_test /tmp/test".to_string()]
    );

    let mut prompted_again = false;
    let output = agent
        .execute_tool_evented(&call, "call", &CancellationToken::new(), &mut |event| {
            if matches!(event, AgentEvent::PermissionRequest(_)) {
                prompted_again = true;
            }
        })
        .await
        .unwrap();
    assert_eq!(output.to_text(), "should not run");
    assert!(!prompted_again);
}

#[tokio::test]
async fn rejected_permission_does_not_execute_tool() {
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    ));
    let call = ToolCall {
        id: "call".to_string(),
        name: "write_test".to_string(),
        arguments: "{}".to_string(),
    };
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task_agent = agent.clone();
    let task = tokio::spawn(async move {
        task_agent
            .execute_tool_evented(&call, "call", &CancellationToken::new(), &mut |event| {
                let _ = event_tx.send(event);
            })
            .await
    });

    let request = match event_rx.recv().await.unwrap() {
        AgentEvent::PermissionRequest(request) => request,
        event => panic!("unexpected event: {:?}", event),
    };
    assert!(agent.reply_permission(&request.id, PermissionDecision::Reject));
    assert!(
        task.await
            .unwrap()
            .unwrap()
            .to_text()
            .contains("Permission denied")
    );
}

#[tokio::test]
async fn headless_run_rejects_write_tools_without_hanging() {
    let agent = Agent::new(
        Arc::new(PermissionTestProvider(AtomicUsize::new(0))),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    let mut messages = vec![Message::new(Role::User, "write something")];

    let outcome = agent.run(&mut messages).await.unwrap();

    // Permission rejection now terminates the turn instead of letting the
    // model continue, so the final assistant message is empty.
    assert!(outcome.message.content.is_empty());
    assert!(
        messages
            .iter()
            .any(|message| message.content.contains("Permission denied"))
    );
}

// ---- Golden-transcript harness ----------------------------------------
//
// `ScriptedProvider` replays a fixed list of streamed events — one script
// per model round — so a whole agent turn runs deterministically and its
// emitted `AgentEvent` stream can be asserted as a stable golden
// transcript. This pins the loop's externally-visible contract (tool-call
// ordering, native vs text-fallback dispatch, concurrent result ordering,
// the repeated-call guard, and permission gating) independently of any real
// provider, so the refactors that follow can lean on it as a safety net.

/// A model round that streams a single chunk of assistant text.
fn text_round(text: &str) -> Vec<ProviderStreamEvent> {
    vec![ProviderStreamEvent::TextDelta(text.to_string())]
}

/// A model round that streams native tool calls as `(id, name, arguments)`.
fn tool_round(calls: &[(&str, &str, &str)]) -> Vec<ProviderStreamEvent> {
    calls
        .iter()
        .enumerate()
        .map(
            |(index, (id, name, arguments))| ProviderStreamEvent::ToolCallDelta {
                index,
                id: Some(id.to_string()),
                name: Some(name.to_string()),
                arguments: arguments.to_string(),
            },
        )
        .collect()
}

struct ScriptedProvider {
    rounds: std::sync::Mutex<std::collections::VecDeque<Vec<ProviderStreamEvent>>>,
}

impl ScriptedProvider {
    fn new(rounds: Vec<Vec<ProviderStreamEvent>>) -> Self {
        Self {
            rounds: std::sync::Mutex::new(rounds.into_iter().collect()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Err("scripted provider is streaming-only".to_string())
    }

    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        Ok(Box::pin(stream::empty()))
    }

    async fn stream_chat_events(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        // A turn that runs past its script gets a terminal "done" so the
        // loop exits rather than hanging on a missing round.
        let round = self
            .rounds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| text_round("done"));
        Ok(Box::pin(stream::iter(round.into_iter().map(Ok))))
    }
}

/// A tool that records every invocation's arguments and returns canned
/// output. The `write` variant declares a [`ScopeTarget::Path`] so the
/// permission broker fires for it; the `read` variant leaves the default
/// [`ScopeTarget::Unspecified`] and skips the broker.
struct RecordingTool {
    name: &'static str,
    output: String,
    declares_target: bool,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RecordingTool {
    fn read(name: &'static str, output: &str) -> Self {
        Self {
            name,
            output: output.to_string(),
            declares_target: false,
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn write(name: &'static str, output: &str) -> Self {
        Self {
            declares_target: true,
            ..Self::read(name, output)
        }
    }

    fn calls_handle(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
        Arc::clone(&self.calls)
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "recording test tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn scope_target(&self, arguments: &str) -> neenee_core::ScopeTarget {
        if self.declares_target {
            // Pull a path from the args if present, else a fixed sentinel, so
            // the broker fires for the `write` variant.
            let path = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(str::to_string))
                .unwrap_or_else(|| "/tmp/recording".to_string());
            neenee_core::ScopeTarget::Path(std::path::PathBuf::from(path))
        } else {
            neenee_core::ScopeTarget::Unspecified
        }
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(arguments.to_string());
        Ok(self.output.clone())
    }
}

/// Normalise an event stream into a stable, assertable transcript by
/// dropping non-deterministic fields (generated call ids and durations).
fn transcript(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::Notice(notice) => {
                format!("notice {:?} {:?}", notice.kind, notice.title)
            }
            AgentEvent::ModelRequestStarted { tool_round } => {
                format!("model-request turn={tool_round}")
            }
            AgentEvent::AssistantDelta { delta, start } => {
                format!("assistant-delta start={start} {delta:?}")
            }
            AgentEvent::AssistantEnd(content) => format!("assistant-end {content:?}"),
            AgentEvent::AssistantDiscard => "assistant-discard".to_string(),
            AgentEvent::ReasoningDelta { delta, start } => {
                format!("reasoning-delta start={start} {delta:?}")
            }
            AgentEvent::ReasoningEnd(content) => format!("reasoning-end {content:?}"),
            AgentEvent::ToolCall {
                name, arguments, ..
            } => {
                format!("tool-call {name} {arguments}")
            }
            AgentEvent::ToolResult { name, output, .. } => {
                format!("tool-result {name} {output:?}")
            }
            AgentEvent::ToolStream { id, stream } => {
                format!("tool-stream {} {:?}", id, stream)
            }
            AgentEvent::ToolCancelled { name, .. } => {
                format!("tool-cancelled {name}")
            }
            AgentEvent::PursuitUpdated(_) => "pursuit-updated".to_string(),
            AgentEvent::UnattendedChanged(enabled) => format!("unattended {enabled}"),
            AgentEvent::SessionReview { alert } => {
                format!("session-review alert={alert:?}")
            }
            AgentEvent::PermissionRequest(request) => {
                format!("permission-request {} {}", request.tool, request.scope)
            }
            AgentEvent::UserQuestionRequest(request) => {
                format!("user-question {}", request.questions.len())
            }
            AgentEvent::InputRequest(request) => {
                format!(
                    "input-request {} (secret={})",
                    request.command, request.secret
                )
            }
            AgentEvent::Envoy { .. } => "subtask".to_string(),
            AgentEvent::TodosUpdated(list) => {
                format!("todos {} items", list.len())
            }
        })
        .collect()
}

/// Drive one full turn, auto-answering any permission prompt with `decision`
/// so write-capable tools don't deadlock the loop.
async fn run_golden_turn(
    agent: &Agent,
    prompt: &str,
    decision: PermissionDecision,
) -> (Vec<AgentEvent>, Result<RoundOutcome, HarnessError>) {
    let mut messages = vec![Message::new(Role::User, prompt)];
    let mut events = Vec::new();
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            if let AgentEvent::PermissionRequest(request) = &event {
                agent.reply_permission(&request.id, decision);
            }
            events.push(event);
        })
        .await;
    (events, outcome)
}

#[tokio::test]
async fn golden_native_tool_round_then_final_text() {
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            tool_round(&[("c1", "alpha", "{\"k\":1}"), ("c2", "beta", "{\"k\":2}")]),
            text_round("all done"),
        ])),
        vec![
            Arc::new(RecordingTool::read("alpha", "A-out")),
            Arc::new(RecordingTool::read("beta", "B-out")),
        ],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert_eq!(outcome.unwrap().message.content, "all done");
    // Calls are announced up front, then results land in input (FIFO) order
    // regardless of concurrent execution.
    assert_eq!(
        transcript(&events),
        vec![
            "model-request turn=0",
            "tool-call alpha {\"k\":1}",
            "tool-call beta {\"k\":2}",
            "tool-result alpha \"A-out\"",
            "tool-result beta \"B-out\"",
            "model-request turn=1",
            "assistant-delta start=true \"all done\"",
            "assistant-end \"all done\"",
        ]
    );
}

#[tokio::test]
async fn golden_text_fallback_tool_call_is_discarded_then_dispatched() {
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            text_round("{\"tool\":\"alpha\",\"arguments\":{\"k\":1}}"),
            text_round("finished"),
        ])),
        vec![Arc::new(RecordingTool::read("alpha", "A-out"))],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert_eq!(outcome.unwrap().message.content, "finished");
    // The streamed JSON is shown, then discarded once recognised as a tool
    // call, so the UI never leaves raw tool JSON on screen.
    assert_eq!(
        transcript(&events),
        vec![
            "model-request turn=0",
            "assistant-delta start=true \"{\\\"tool\\\":\\\"alpha\\\",\\\"arguments\\\":{\\\"k\\\":1}}\"",
            "assistant-end \"{\\\"tool\\\":\\\"alpha\\\",\\\"arguments\\\":{\\\"k\\\":1}}\"",
            "assistant-discard",
            "tool-call alpha {\"k\":1}",
            "tool-result alpha \"A-out\"",
            "model-request turn=1",
            "assistant-delta start=true \"finished\"",
            "assistant-end \"finished\"",
        ]
    );
}

#[tokio::test]
async fn golden_repeated_identical_tool_calls_run_without_hard_abort() {
    // The equality-guard hard abort was removed in favour of the soft
    // loop-review intervention. Identical calls now all execute; the turn
    // ends when the model stops calling tools (the scripted provider runs
    // out of rounds).
    let tool = RecordingTool::read("alpha", "A-out");
    let calls = tool.calls_handle();
    let identical = || tool_round(&[("c", "alpha", "{}")]);
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            identical(),
            identical(),
            identical(),
            identical(),
        ])),
        vec![Arc::new(tool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_nudge_config(neenee_core::NudgeConfig::disabled());

    let (_events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    // No hard abort — all 4 rounds execute.
    assert_eq!(calls.lock().unwrap().len(), 4);
    // The turn completes normally (provider exhausts its rounds).
    let _ = outcome.unwrap();
}

/// Wiring test: three identical reads must trip the deterministic loop guard and
/// land exactly one hidden anti-anchoring nudge in the turn history. Complements
/// the unit tests in `loop_guard`, which cover the detector in isolation.
#[tokio::test]
async fn read_loop_guard_injects_one_nudge_after_repeated_reads() {
    let read = || tool_round(&[("c", "reader", r#"{"path":"big.rs"}"#)]);
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            read(),
            read(),
            read(),
            text_round("done"),
        ])),
        vec![Arc::new(RecordingTool::read("reader", "R-out"))],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_nudge_config(neenee_core::NudgeConfig {
        enabled: true,
        ..neenee_core::NudgeConfig::default()
    });

    let mut messages = vec![Message::new(Role::User, "go")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;
    assert_eq!(outcome.unwrap().message.content, "done");

    let nudges: Vec<&Message> = messages
        .iter()
        .filter(|m| m.origin.as_ref().map(|o| &o.kind) == Some(&InjectionKind::LoopReviewNudge))
        .collect();
    assert_eq!(nudges.len(), 1, "exactly one nudge for one loop streak");
    assert!(
        nudges[0].content.contains("reader big.rs"),
        "nudge names the repeated read: {}",
        nudges[0].content
    );
    assert!(nudges[0].hidden, "nudge is a hidden steering injection");
}

/// End-to-end: a model that keeps issuing the *same* read past the nudge gets
/// that read hard-blocked at the dispatch layer. The `RecordingTool` must NOT
/// be invoked for the blocked rounds (its call count proves the short-circuit
/// ran before execution), and the model must receive a `[loop guard]` error
/// `ToolResult` for each blocked read (proving the block message, not silent
/// execution, reached the transcript). This is the integration counterpart to
/// the `RoundGuardState::is_blocked` unit tests — it exercises the real
/// `dispatch_tool_calls` → mask → short-circuit path through a mock provider.
#[tokio::test]
async fn read_loop_guard_hard_blocks_repeating_read_at_dispatch() {
    let reader = RecordingTool::read("reader", "R-out");
    let calls = reader.calls_handle();
    // Eight identical reads (nudge at 3, block at 6; rounds 6-8 blocked), then
    // a terminal text round so the turn completes.
    let read = || tool_round(&[("c", "reader", r#"{"path":"big.rs"}"#)]);
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            read(),
            read(),
            read(),
            read(),
            read(),
            read(),
            read(),
            read(),
            text_round("done"),
        ])),
        vec![Arc::new(reader)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_nudge_config(neenee_core::NudgeConfig {
        enabled: true,
        ..neenee_core::NudgeConfig::default()
    });

    let mut messages = vec![Message::new(Role::User, "go")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;
    assert_eq!(outcome.unwrap().message.content, "done");

    // The tool body ran for the first 5 reads (before the block mask was set at
    // round 6's boundary); reads 6, 7, 8 were short-circuited and never reached
    // it. (ESCALATE_AT=6 fires the Block on the 6th read; the mask is checked
    // from round 7 on, so rounds 7 and 8 are blocked too — 3 blocked reads.)
    let executed = calls.lock().unwrap().len();
    assert!(
        executed <= 6,
        "the looping read should be blocked after round 6; tool ran {executed} times"
    );

    // Each blocked read produced a [loop guard] error as a tool-role message
    // the model sees, proving the block is communicated (not silent).
    let blocked_results: Vec<&Message> = messages
        .iter()
        .filter(|m| {
            m.role == Role::Tool
                && m.content.contains("[loop guard]")
                && m.content.contains("blocked")
        })
        .collect();
    assert!(
        !blocked_results.is_empty(),
        "at least one blocked read should surface a [loop guard] result to the model"
    );
}

/// End-to-end: the block is surgical. A read blocked for `big.rs` does NOT
/// block a read of a *different* file in the same turn — the model can still
/// read other files, which is exactly the behavior that lets it recover.
#[tokio::test]
async fn read_loop_block_is_surgical_across_files() {
    let big = RecordingTool::read("reader", "BIG");
    let big_calls = big.calls_handle();
    let other = RecordingTool::read("other", "OTHER");
    let other_calls = other.calls_handle();
    // Six reads of big.rs (blocks it), then a read of small.rs (must succeed),
    // then done.
    let read_big = || tool_round(&[("c", "reader", r#"{"path":"big.rs"}"#)]);
    let read_small = || tool_round(&[("c", "other", r#"{"path":"small.rs"}"#)]);
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            read_big(),
            read_big(),
            read_big(),
            read_big(),
            read_big(),
            read_big(),
            read_small(),
            text_round("done"),
        ])),
        vec![Arc::new(big), Arc::new(other)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_nudge_config(neenee_core::NudgeConfig {
        enabled: true,
        ..neenee_core::NudgeConfig::default()
    });

    let mut messages = vec![Message::new(Role::User, "go")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;
    assert_eq!(outcome.unwrap().message.content, "done");

    // The `other` tool (different file) ran exactly once — the big.rs block did
    // not over-reach and block unrelated reads.
    assert_eq!(
        other_calls.lock().unwrap().len(),
        1,
        "a read of a different file must not be blocked by a big.rs block"
    );
    // And the small.rs read returned its real content, not a block error.
    assert!(
        messages.iter().any(|m| m.content.contains("OTHER")),
        "the unblocked read should return its real content"
    );
    let _ = big_calls; // suppress unused warning; big.rs execution count is
    // asserted in the sibling test above.
}

/// The guard is gated by `set_nudge_config`: disabled (the default), the same
/// looping transcript injects no nudge (envoys and the review diagnostic rely
/// on this). The test is explicit about the disabled state rather than
/// relying on the default so the assertion stays meaningful if the default
/// ever flips.
#[tokio::test]
async fn read_loop_guard_suppressed_when_disabled() {
    let read = || tool_round(&[("c", "reader", r#"{"path":"big.rs"}"#)]);
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            read(),
            read(),
            read(),
            text_round("done"),
        ])),
        vec![Arc::new(RecordingTool::read("reader", "R-out"))],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_nudge_config(neenee_core::NudgeConfig::disabled());

    let mut messages = vec![Message::new(Role::User, "go")];
    let _ = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await;

    assert!(
        messages
            .iter()
            .all(|m| m.origin.as_ref().map(|o| &o.kind) != Some(&InjectionKind::LoopReviewNudge)),
        "disabled guard must not inject"
    );
}

#[tokio::test]
async fn golden_rejected_write_tool_terminates_turn() {
    let tool = RecordingTool::write("writer", "WROTE");
    let calls = tool.calls_handle();
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            tool_round(&[("c1", "writer", "{\"path\":\"x\"}")]),
            text_round("stopped"),
        ])),
        vec![Arc::new(tool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    // The turn ends immediately after the denied permission; the second
    // model round ("stopped") is never reached.
    assert_eq!(outcome.unwrap().message.content, "");
    assert!(
        calls.lock().unwrap().is_empty(),
        "rejected write tool must not execute"
    );
    let lines = transcript(&events);
    assert!(
        lines
            .iter()
            .any(|line| line == "permission-request writer x")
    );
    assert!(
        lines.iter().any(
            |line| line.starts_with("tool-result writer") && line.contains("Permission denied")
        )
    );
}

#[tokio::test]
async fn golden_reasoning_precedes_text_in_the_same_round() {
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![vec![
            ProviderStreamEvent::ReasoningDelta("think".to_string()),
            ProviderStreamEvent::TextDelta("answer".to_string()),
        ]])),
        Vec::new(),
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert_eq!(outcome.unwrap().message.content, "answer");
    // Deltas surface in stream-arrival order (reasoning first here), but the
    // round closes with AssistantEnd before ReasoningEnd.
    assert_eq!(
        transcript(&events),
        vec![
            "model-request turn=0",
            "reasoning-delta start=true \"think\"",
            "assistant-delta start=true \"answer\"",
            "assistant-end \"answer\"",
            "reasoning-end \"think\"",
        ]
    );
}

#[tokio::test]
async fn ask_user_tool_blocks_and_returns_selected_answers() {
    let ask_args = serde_json::json!({
        "questions": [{
            "header": "style",
            "question": "Which error handling style?",
            "options": [
                { "label": "anyhow (Recommended)", "description": "Simple" },
                { "label": "thiserror", "description": "Structured" }
            ],
            "multi_select": false
        }]
    });
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            tool_round(&[("c1", "ask_user", &ask_args.to_string())]),
            text_round("done"),
        ])),
        vec![Arc::new(neenee_tools::AskUserTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    let mut messages = vec![Message::new(Role::User, "choose")];
    let mut events = Vec::new();
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            if let AgentEvent::UserQuestionRequest(request) = &event {
                agent.reply_user_question(&request.id, vec![vec!["thiserror".to_string()]]);
            }
            events.push(event);
        })
        .await;

    assert_eq!(outcome.unwrap().message.content, "done");
    let lines = transcript(&events);
    assert!(lines.iter().any(|line| line.starts_with("user-question")));
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("tool-result ask_user") && line.contains("thiserror"))
    );
}

// ---- Persistent permissions (cross-session) -------------------------------
//
// Verifies the per-project `Always` allowlist round-trips through disk:
// approving `Always` on one agent is visible to a fresh agent constructed
// against the same project root, and revoking is mirrored to disk too.
// Envoys (no project root) stay ephemeral and never touch the file.

#[tokio::test]
async fn always_permission_persists_across_agents_for_same_project() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = std::env::temp_dir().join(format!("neenee-perms-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    // `paths::get()` reads `NEENEE_DATA_DIR` on every call (no caching), so
    // pointing the env var at a tempdir redirects the project bucket there.
    unsafe {
        std::env::set_var("NEENEE_DATA_DIR", &tmp);
    }
    let project_root = std::path::PathBuf::from("/tmp/neenee-perms-fixture-project");
    let perms_path = neenee_store::paths::get().project_permissions(&project_root);

    // First agent: prompt for a write_test permission and approve Always.
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    ));
    agent.set_project_root(Some(project_root.clone()));
    assert!(agent.allowed_tools().is_empty());

    let call = ToolCall {
        id: "call".to_string(),
        name: "write_test".to_string(),
        arguments: "{}".to_string(),
    };
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let task_agent = agent.clone();
    let task = tokio::spawn(async move {
        task_agent
            .execute_tool_evented(&call, "call", &CancellationToken::new(), &mut |event| {
                let _ = event_tx.send(event);
            })
            .await
    });
    let request = match event_rx.recv().await.unwrap() {
        AgentEvent::PermissionRequest(request) => request,
        event => panic!("unexpected event: {:?}", event),
    };
    assert!(agent.reply_permission(&request.id, PermissionDecision::Always));
    let _ = task.await;

    // The Always decision should have triggered an atomic write to disk.
    assert!(
        perms_path.exists(),
        "permissions file should exist at {}",
        perms_path.display()
    );
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&perms_path).unwrap()).unwrap();
    assert_eq!(on_disk["version"].as_u64(), Some(1));
    assert_eq!(on_disk["rules"].as_array().unwrap().len(), 1);
    assert_eq!(on_disk["rules"][0]["tool"], "write_test");
    assert_eq!(on_disk["rules"][0]["scope"], "/tmp/test");

    // A brand-new agent in the same project should inherit the rule without
    // ever prompting — that is the whole point of cross-session persistence.
    let agent2 = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    ));
    agent2.set_project_root(Some(project_root.clone()));
    assert_eq!(
        agent2.allowed_tools(),
        vec!["write_test /tmp/test".to_string()],
        "fresh agent in the same project should inherit persisted Always rule"
    );

    // Revoking on agent2 must remove the rule from disk as well, so the next
    // session doesn't silently resurrect it.
    assert!(agent2.revoke_allowed_tool("write_test", "/tmp/test"));
    let after_revoke: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&perms_path).unwrap()).unwrap();
    assert_eq!(after_revoke["rules"].as_array().unwrap().len(), 0);

    // A different project root must NOT see the first project's rules.
    let other_root = std::path::PathBuf::from("/tmp/neenee-perms-fixture-other-project");
    let agent3 = Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent3.set_project_root(Some(other_root));
    assert!(
        agent3.allowed_tools().is_empty(),
        "unrelated project must not inherit another project's rules"
    );

    unsafe {
        std::env::remove_var("NEENEE_DATA_DIR");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn agent_without_project_root_never_writes_permissions_file() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = std::env::temp_dir().join(format!("neenee-perms-noset-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    unsafe {
        std::env::set_var("NEENEE_DATA_DIR", &tmp);
    }
    let project_root = std::path::PathBuf::from("/tmp/neenee-perms-noset-fixture");
    let perms_path = neenee_store::paths::get().project_permissions(&project_root);

    // No set_project_root call: the agent stays ephemeral, so an Always
    // approval must not write any file (envoys behave the same way).
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    ));
    // Mutations of the allowlist must be no-ops on disk when no project root
    // is set: no panic, no file created.
    agent.clear_allowed_tools();
    assert!(!agent.revoke_allowed_tool("anything", "*"));
    assert!(
        !perms_path.exists(),
        "ephemeral agent must not create a permissions file"
    );

    unsafe {
        std::env::remove_var("NEENEE_DATA_DIR");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- Uncapped tool rounds ----------------------------------------------
//
// The per-turn tool-round cap was removed (along with the soft convergence
// nudge) to align with the codex / claude-code agentic-loop model: the
// turn runs until the model stops calling tools, with context compaction
// as the backstop. This test pins the new behaviour — a long sequence of
// distinct tool calls runs well past the previous hard cap of 32 and only
// stops when the model finally emits a text answer.

#[tokio::test]
async fn turn_runs_uncapped_until_model_emits_text() {
    // 64 distinct tool rounds — well past any historical cap — followed by a
    // text answer. Each read round uses a distinct argument so the
    // repeated-call guard never trips, and every 4th round is a Write. This
    // mirrors the uncapped contract: the turn is bounded by the model
    // choosing to stop, not by raw round count (ADR-0009). Session review is
    // on-demand only (`/review`), so the turn loop never fires a diagnostic
    // to consume the shared scripted stream (ADR-0018).
    let write = RecordingTool::write("writer", "WROTE");
    let read = RecordingTool::read("alpha", "out");
    let mut rounds: Vec<Vec<ProviderStreamEvent>> = Vec::new();
    for i in 0..64 {
        if i > 0 && i % 4 == 0 {
            rounds.push(tool_round(&[("cw", "writer", &format!("{{\"i\":{i}}}"))]));
        } else {
            rounds.push(tool_round(&[("c", "alpha", &format!("{{\"i\":{i}}}"))]));
        }
    }
    rounds.push(text_round("all done"));
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(rounds)),
        vec![Arc::new(read), Arc::new(write)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Always).await;

    assert_eq!(outcome.unwrap().message.content, "all done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::SessionReview { .. })),
        "the turn loop must not emit review events; review is on-demand only"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Session review (ADR-0018, superseding the periodic ADR-0016 design)
// ─────────────────────────────────────────────────────────────────────

/// Build a turn of N distinct read-only `alpha` calls (each with a different
/// path so they count as distinct calls rather than repeats), optionally
/// followed by a final text round. Drives the round counter past a review
/// line without accumulating repeated-call counts.
fn distinct_read_rounds(n: usize, suffix: Option<&str>) -> Vec<Vec<ProviderStreamEvent>> {
    let mut rounds: Vec<Vec<ProviderStreamEvent>> = (0..n)
        .map(|i| tool_round(&[("c", "alpha", &format!("{{\"path\":\"f{i}\"}}"))]))
        .collect();
    if let Some(s) = suffix {
        rounds.push(text_round(s));
    }
    rounds
}

#[test]
fn hard_stop_turns_getter_round_trips_setter() {
    // The `/hard-stop` path (and config seed) writes via `set_hard_stop_turns`
    // and reads via `get_hard_stop_turns`; the pair must round-trip. Default
    // is 0 (uncapped, ADR-0009).
    let agent = agent();
    assert_eq!(agent.get_hard_stop_turns(), 0);
    agent.set_hard_stop_turns(99);
    assert_eq!(agent.get_hard_stop_turns(), 99);
    agent.set_hard_stop_turns(0);
    assert_eq!(agent.get_hard_stop_turns(), 0);
}

#[test]
fn render_review_alert_collapses_verdicts() {
    use crate::agent::Agent;
    // All healthy → empty string (clears the activity-bar alert).
    let healthy = vec![ReviewVerdict::healthy("looping")];
    assert_eq!(Agent::render_review_alert(&healthy, 64), "");
    // Stuck dominates Watch; both non-healthy details fold in, round count
    // surfaces, and the worst status label wins.
    let mixed = vec![
        ReviewVerdict {
            dimension: "looping".into(),
            status: ReviewStatus::Watch,
            detail: "slow".into(),
        },
        ReviewVerdict {
            dimension: "other".into(),
            status: ReviewStatus::Stuck,
            detail: "re-reading f.rs".into(),
        },
    ];
    let alert = Agent::render_review_alert(&mixed, 80);
    assert!(alert.starts_with("review: stuck"), "{alert}");
    assert!(alert.contains("80 rounds"), "{alert}");
    assert!(alert.contains("re-reading f.rs"), "{alert}");
    assert!(alert.contains("slow"), "{alert}");
}

#[test]
fn estimate_tool_rounds_counts_assistant_tool_call_messages() {
    use crate::agent::Agent;
    // No messages → 0.
    assert_eq!(Agent::estimate_tool_rounds(&[]), 0);
    let mut msgs = vec![
        Message::new(Role::User, "go"),
        Message::new(Role::Assistant, "thinking"),
    ];
    // Assistant without tool calls → not a round.
    assert_eq!(Agent::estimate_tool_rounds(&msgs), 0);
    // Two assistant messages carrying tool calls → two rounds; a plain text
    // assistant message in between does not inflate the count.
    let mut with_calls = msgs[1].clone();
    with_calls.tool_calls = Some(vec![neenee_core::ToolCall {
        id: "c1".into(),
        name: "read_text".into(),
        arguments: "{}".into(),
    }]);
    msgs[1] = with_calls;
    msgs.push(Message::new(Role::Assistant, "more text"));
    let mut third = Message::new(Role::Assistant, String::new());
    third.tool_calls = Some(vec![neenee_core::ToolCall {
        id: "c2".into(),
        name: "edit_file".into(),
        arguments: "{}".into(),
    }]);
    msgs.push(third);
    assert_eq!(Agent::estimate_tool_rounds(&msgs), 2);
}

#[tokio::test]
async fn hard_stop_aborts_when_budget_configured() {
    // hard_stop_turns is the only opt-in execution cap. With it set to 3, the
    // 3rd tool round trips the budget and the turn aborts with the budget in
    // the message.
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(distinct_read_rounds(10, None))),
        vec![Arc::new(tool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );
    agent.set_hard_stop_turns(3);

    let mut messages = vec![Message::new(Role::User, "go")];
    let error = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await
        .expect_err("hard-stop budget must abort the turn");

    let message = match error {
        HarnessError::Other(message) => message,
        other => panic!("expected HarnessError::Other, got {other:?}"),
    };
    assert!(
        message.contains("hard-stop budget of 3"),
        "error must name the budget, got: {message}"
    );
}

#[tokio::test]
async fn review_now_runs_diagnostic_and_returns_verdict() {
    // On-demand review (`/review` → `Agent::review_now`) feeds the transcript
    // to the REVIEW envoy, which shares the scripted provider. The next
    // scripted round is the reviewer's verdict JSON; `review_now` parses it
    // back into a `ReviewVerdict` keyed to the `looping` dimension.
    let verdict_json =
        r#"{"verdicts":[{"dimension":"looping","status":"stuck","detail":"re-reading"}]}"#;
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![text_round(verdict_json)])),
        vec![Arc::new(tool)],
        crate::skills::SkillRegistry::empty(),
        crate::AgentIdentity::default(),
    );

    // A transcript with one tool round so the estimate is meaningful.
    let mut transcript = vec![Message::new(Role::User, "go")];
    let mut assistant = Message::new(Role::Assistant, String::new());
    assistant.tool_calls = Some(vec![neenee_core::ToolCall {
        id: "c1".into(),
        name: "read_text".into(),
        arguments: "{\"path\":\"f\"}".into(),
    }]);
    transcript.push(assistant);

    let verdicts = agent.review_now(&transcript).await;
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].dimension, "looping");
    assert_eq!(verdicts[0].status, ReviewStatus::Stuck);
    assert_eq!(verdicts[0].detail, "re-reading");
    // The on-demand alert renders with the estimated round count.
    let alert = crate::agent::Agent::render_review_alert(
        &verdicts,
        crate::agent::Agent::estimate_tool_rounds(&transcript),
    );
    assert!(alert.contains("review: stuck"), "{alert}");
    assert!(alert.contains("1 rounds"), "{alert}");
}

#[test]
fn agent_config_defaults_match_runtime_constants() {
    // The config struct's defaults must match the seeds the agent uses when
    // no config is loaded, so a missing `[agent]` table is indistinguishable
    // from one that explicitly sets the defaults (ADR-0018).
    use neenee_store::config::PrincipalConfig;
    let cfg = PrincipalConfig::default();
    assert_eq!(cfg.hard_stop_turns, 0);
    // The agent seeds the same hard-stop budget by default (uncapped).
    let agent = agent();
    assert_eq!(agent.get_hard_stop_turns(), 0);
}

// ── /debug network capture ────────────────────────────────────────────

/// A provider whose `stream_chat_events` emits a fixed two-event sequence, so
/// the streaming capture path can be exercised deterministically.
struct TwoEventProvider;

#[async_trait]
impl Provider for TwoEventProvider {
    async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
        Err("chat path not used by this test".to_string())
    }
    async fn stream_chat(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<String, String>>, String> {
        Err("stream_chat path not used by this test".to_string())
    }
    async fn stream_chat_events(
        &self,
        _messages: Vec<Message>,
    ) -> Result<BoxStream<'static, Result<ProviderStreamEvent, String>>, String> {
        Ok(Box::pin(futures::stream::iter([
            Ok(ProviderStreamEvent::TextDelta("hel".to_string())),
            Ok(ProviderStreamEvent::TextDelta("lo".to_string())),
        ])))
    }
}

fn capture_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("neenee-capture-{}", uuid::Uuid::new_v4()))
}

#[tokio::test]
async fn debug_network_capture_writes_one_file_per_chat() {
    use crate::orchestration::ProxyProvider;
    use std::sync::{Arc, RwLock};

    let dir = capture_dir();
    let holder: Arc<RwLock<Arc<dyn Provider>>> = Arc::new(RwLock::new(Arc::new(TestProvider)));
    let proxy = ProxyProvider::new(holder);

    // Off by default, and a call while off writes nothing.
    assert!(!proxy.debug_capture_enabled());
    proxy
        .chat(vec![Message::new(Role::User, "hi")])
        .await
        .unwrap();
    let off_count = std::fs::read_dir(&dir).map(|entries| entries.count()).ok();
    assert_eq!(off_count, None, "no directory created while capture is off");

    // Arming creates exactly one JSON file per round-trip on the chat path.
    proxy.set_debug_capture(true, dir.clone());
    assert!(proxy.debug_capture_enabled());
    proxy
        .chat(vec![Message::new(Role::User, "hello")])
        .await
        .unwrap();
    proxy
        .chat(vec![Message::new(Role::User, "again")])
        .await
        .unwrap();
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 2, "one file per round-trip");
    for entry in entries {
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap();
        assert_eq!(value["kind"], "chat");
        assert_eq!(value["provider"], "");
        assert_eq!(value["request"]["messages"][0]["role"], "User");
        assert_eq!(value["response"]["items"][0]["status"], "ok");
        assert_eq!(
            value["response"]["items"][0]["message"]["role"],
            "Assistant"
        );
        assert_eq!(value["response"]["items"][0]["message"]["content"], "done");
    }

    // Disarming stops further writes.
    proxy.set_debug_capture(false, dir.clone());
    assert!(!proxy.debug_capture_enabled());
    proxy
        .chat(vec![Message::new(Role::User, "after off")])
        .await
        .unwrap();
    let after: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(after.len(), 2, "no new file after disabling");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn debug_network_capture_aggregates_a_full_stream_into_one_file() {
    use crate::orchestration::ProxyProvider;
    use futures::StreamExt;
    use std::sync::{Arc, RwLock};

    let dir = capture_dir();
    let holder: Arc<RwLock<Arc<dyn Provider>>> = Arc::new(RwLock::new(Arc::new(TwoEventProvider)));
    let proxy = ProxyProvider::new(holder);
    proxy.set_debug_capture(true, dir.clone());

    // Drive the stream fully; on completion the wrapper drops and flushes the
    // aggregated record.
    let stream = proxy
        .stream_chat_events(vec![Message::new(Role::User, "hi")])
        .await
        .unwrap();
    let items: Vec<_> = stream.collect::<Vec<_>>().await;
    assert_eq!(items.len(), 2);

    let entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1, "one streaming round-trip -> one file");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(entries[0].path()).unwrap()).unwrap();
    assert_eq!(value["kind"], "stream_chat_events");
    let captured = value["response"]["items"].as_array().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0]["Ok"]["TextDelta"], "hel");
    assert_eq!(captured[1]["Ok"]["TextDelta"], "lo");

    let _ = std::fs::remove_dir_all(&dir);
}
