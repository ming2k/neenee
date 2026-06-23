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
                subagent_meta: None,
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

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        assert_eq!(arguments, "{\"value\":1}");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("read".to_string())
    }
}

fn test_pursuit_service() -> PursuitService {
    PursuitService::new(PursuitStore::open_in_memory_blocking().expect("in-memory pursuit store"))
}

fn agent() -> Agent {
    Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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

    let prompt = agent.build_system_prompt();

    assert!(prompt.contains("ship the harness"));
    assert!(prompt.contains("complete_pursuit"));
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
fn pursuit_gate_lets_turn_end_without_active_goal() {
    let agent = agent();
    agent.arm_pursuit();
    let resp = Message::new(Role::Assistant, "working".to_string());
    assert!(agent.pursuit_continuation(&resp).is_none());
}

#[test]
fn pursuit_gate_lets_turn_end_when_goal_already_complete() {
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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

/// A provider whose SSE stream never yields and never ends simulates a stalled
/// connection (server stops sending but keeps the socket open). Without an idle
/// timeout the turn loop blocks on `stream.next()` forever — the UI spins
/// "running · responding" and only a user interrupt can break it. The
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    let mut messages = vec![Message::new(Role::User, "hello")];

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
        fn access(&self) -> ToolAccess {
            ToolAccess::Read
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
    assert!(!recorded
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolResult { .. })));
    assert!(recorded
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCall { name, .. } if name == "stream_read")));
}

#[test]
fn repeated_tool_calls_are_bounded() {
    let agent = agent();
    let call = ToolCall {
        id: "call".to_string(),
        name: "read_file".to_string(),
        arguments: "{\"path\":\"README.md\"}".to_string(),
    };
    let mut previous = None;
    let mut repeats = 0;

    for _ in 0..MAX_REPEATED_TOOL_CALLS {
        assert!(agent
            .guard_repeated_call(&call, &mut previous, &mut repeats)
            .is_ok());
    }
    assert!(agent
        .guard_repeated_call(&call, &mut previous, &mut repeats)
        .is_err());
}

#[tokio::test]
async fn plan_mode_blocks_tools_unless_explicitly_read_only() {
    let agent = Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Plan,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    let call = ToolCall {
        id: "call".to_string(),
        name: "write_test".to_string(),
        arguments: "{}".to_string(),
    };

    assert!(agent
        .execute_tool_evented(&call, "call", &CancellationToken::new(), &mut |_| {})
        .await
        .unwrap()
        .to_text()
        .contains("[Plan mode]"));
}

#[tokio::test]
async fn plan_exit_asks_user_and_implements_when_approved() {
    // Write a real plan file so plan_exit can read its content and seed
    // PlanProgress for the sticky panel.
    let cwd = std::env::current_dir().unwrap();
    let plans_dir = cwd.join(".neenee/plans");
    std::fs::create_dir_all(&plans_dir).unwrap();
    let plan_path = plans_dir.join("approval-approve.md");
    std::fs::write(&plan_path, "# Approve Me\n\n## Summary\n- step 1\n- step 2").unwrap();

    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        AgentMode::Plan,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    ));

    let relative = ".neenee/plans/approval-approve.md";
    let arguments = format!("{{\"plan_path\":\"{}\"}}", relative.replace('\\', "\\\\"));
    let call = ToolCall {
        id: "call".to_string(),
        name: "plan_exit".to_string(),
        arguments,
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

    // First event is the approval prompt.
    let request = match event_rx.recv().await.unwrap() {
        AgentEvent::UserQuestionRequest(request) => request,
        event => panic!("unexpected event: {:?}", event),
    };
    assert!(!task.is_finished(), "task should block on user reply");
    assert!(
        request.questions[0]
            .options
            .iter()
            .any(|opt| opt.label == "Approve"),
        "approval option missing"
    );

    assert!(agent.reply_user_question(&request.id, vec![vec!["Approve".to_string()]],));

    let output = task.await.unwrap().unwrap().to_text();
    assert!(output.contains("Plan approved."), "{}", output);
    assert!(output.contains("step 1"), "plan content echoed: {}", output);
    assert_eq!(agent.get_mode(), AgentMode::Build);
    assert_eq!(
        agent.active_plan_path(),
        Some(std::path::PathBuf::from(relative))
    );
    // Plan progress is seeded from the approved plan's `## Summary` heading.
    let progress = agent.plan_progress().expect("progress seeded");
    assert!(
        progress.sections.iter().any(|s| s.name == "Summary"),
        "sections parsed from plan: {:?}",
        progress.sections
    );
}

#[tokio::test]
async fn plan_exit_keeps_planning_when_rejected() {
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        AgentMode::Plan,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    ));
    let call = ToolCall {
        id: "call".to_string(),
        name: "plan_exit".to_string(),
        arguments: r#"{"plan_path":".neenee/plans/missing-but-ok.md"}"#.to_string(),
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
        AgentEvent::UserQuestionRequest(request) => request,
        event => panic!("unexpected event: {:?}", event),
    };

    // User picks "Keep planning".
    assert!(agent.reply_user_question(&request.id, vec![vec!["Keep planning".to_string()]],));

    let output = task.await.unwrap().unwrap().to_text();
    assert!(output.contains("User wants to keep planning"), "{}", output);
    assert_eq!(agent.get_mode(), AgentMode::Plan);
    assert_eq!(agent.active_plan_path(), None);
    assert_eq!(agent.plan_progress(), None);
}

#[tokio::test]
async fn manual_mode_switch_to_plan_clears_plan_state() {
    let agent = Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_active_plan_path(Some(std::path::PathBuf::from(".neenee/plans/was-here.md")));
    agent.set_plan_progress(Some(plan::PlanProgress::from_markdown(
        std::path::PathBuf::from(".neenee/plans/was-here.md"),
        "## X\n",
    )));
    assert!(agent.active_plan_path().is_some());
    assert!(agent.plan_progress().is_some());

    // Manual /mode plan clears both (mirrors plan_enter's behavior).
    agent.set_mode(AgentMode::Plan);
    assert_eq!(agent.active_plan_path(), None);
    assert_eq!(agent.plan_progress(), None);

    // Switching back to Build does not resurrect them.
    agent.set_mode(AgentMode::Build);
    assert_eq!(agent.active_plan_path(), None);
    assert_eq!(agent.plan_progress(), None);
}

#[tokio::test]
async fn write_tool_waits_for_permission_and_always_is_cached() {
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
    assert_eq!(agent.allowed_tools(), vec!["write_test *".to_string()]);

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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
    assert!(task
        .await
        .unwrap()
        .unwrap()
        .to_text()
        .contains("Permission denied"));
}

#[tokio::test]
async fn headless_run_rejects_write_tools_without_hanging() {
    let pursuit_service = PursuitService::new(
        PursuitStore::open_in_memory()
            .await
            .expect("in-memory pursuit store"),
    );
    let agent = Agent::new(
        Arc::new(PermissionTestProvider(AtomicUsize::new(0))),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        pursuit_service,
        crate::skills::SkillRegistry::empty(),
    );
    let mut messages = vec![Message::new(Role::User, "write something")];

    let outcome = agent.run(&mut messages).await.unwrap();

    // Permission rejection now terminates the turn instead of letting the
    // model continue, so the final assistant message is empty.
    assert!(outcome.message.content.is_empty());
    assert!(messages
        .iter()
        .any(|message| message.content.contains("Permission denied")));
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
/// output, with a configurable access level for permission tests.
struct RecordingTool {
    name: &'static str,
    access: ToolAccess,
    output: String,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RecordingTool {
    fn read(name: &'static str, output: &str) -> Self {
        Self {
            name,
            access: ToolAccess::Read,
            output: output.to_string(),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn write(name: &'static str, output: &str) -> Self {
        Self {
            access: ToolAccess::Write,
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
    fn access(&self) -> ToolAccess {
        self.access
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
            AgentEvent::ModelRequestStarted { tool_round } => {
                format!("model-request round={tool_round}")
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
            AgentEvent::ModeChanged(mode) => format!("mode-changed {mode:?}"),
            AgentEvent::PlanProgressUpdated(progress) => format!(
                "plan-progress {:?}",
                progress.as_ref().map(|p| p.sections.len())
            ),
            AgentEvent::AutoApproveChanged(enabled) => format!("auto-approve {enabled}"),
            AgentEvent::SessionReview { alert } => {
                format!("session-review alert={alert:?}")
            }
            AgentEvent::PermissionRequest(request) => {
                format!("permission-request {} {}", request.tool, request.scope)
            }
            AgentEvent::UserQuestionRequest(request) => {
                format!("user-question {}", request.questions.len())
            }
            AgentEvent::SubTask { .. } => "subtask".to_string(),
        })
        .collect()
}

/// Drive one full turn, auto-answering any permission prompt with `decision`
/// so write-capable tools don't deadlock the loop.
async fn run_golden_turn(
    agent: &Agent,
    prompt: &str,
    decision: PermissionDecision,
) -> (Vec<AgentEvent>, Result<TurnOutcome, HarnessError>) {
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert_eq!(outcome.unwrap().message.content, "all done");
    // Calls are announced up front, then results land in input (FIFO) order
    // regardless of concurrent execution.
    assert_eq!(
        transcript(&events),
        vec![
            "model-request round=0",
            "tool-call alpha {\"k\":1}",
            "tool-call beta {\"k\":2}",
            "tool-result alpha \"A-out\"",
            "tool-result beta \"B-out\"",
            "model-request round=1",
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert_eq!(outcome.unwrap().message.content, "finished");
    // The streamed JSON is shown, then discarded once recognised as a tool
    // call, so the UI never leaves raw tool JSON on screen.
    assert_eq!(
            transcript(&events),
            vec![
                "model-request round=0",
                "assistant-delta start=true \"{\\\"tool\\\":\\\"alpha\\\",\\\"arguments\\\":{\\\"k\\\":1}}\"",
                "assistant-end \"{\\\"tool\\\":\\\"alpha\\\",\\\"arguments\\\":{\\\"k\\\":1}}\"",
                "assistant-discard",
                "tool-call alpha {\"k\":1}",
                "tool-result alpha \"A-out\"",
                "model-request round=1",
                "assistant-delta start=true \"finished\"",
                "assistant-end \"finished\"",
            ]
        );
}

#[tokio::test]
async fn golden_repeated_identical_tool_calls_abort_the_turn() {
    let tool = RecordingTool::read("alpha", "A-out");
    let calls = tool.calls_handle();
    // Four identical rounds: the guard trips on the fourth.
    let identical = || tool_round(&[("c", "alpha", "{}")]);
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            identical(),
            identical(),
            identical(),
            identical(),
        ])),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let (_events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert!(matches!(
        outcome.unwrap_err(),
        HarnessError::Other(message) if message.contains("repeating the same")
    ));
    // The first MAX_REPEATED_TOOL_CALLS calls run; the fourth is blocked.
    assert_eq!(
        calls.lock().unwrap().len(),
        MAX_REPEATED_TOOL_CALLS,
        "guard must stop before executing the repeat"
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
    assert!(lines
        .iter()
        .any(|line| line == "permission-request writer *"));
    assert!(lines
        .iter()
        .any(|line| line.starts_with("tool-result writer") && line.contains("Permission denied")));
}

#[tokio::test]
async fn golden_reasoning_precedes_text_in_the_same_round() {
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![vec![
            ProviderStreamEvent::ReasoningDelta("think".to_string()),
            ProviderStreamEvent::TextDelta("answer".to_string()),
        ]])),
        Vec::new(),
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Reject).await;

    assert_eq!(outcome.unwrap().message.content, "answer");
    // Deltas surface in stream-arrival order (reasoning first here), but the
    // round closes with AssistantEnd before ReasoningEnd.
    assert_eq!(
        transcript(&events),
        vec![
            "model-request round=0",
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
    assert!(lines
        .iter()
        .any(|line| line.starts_with("tool-result ask_user") && line.contains("thiserror")));
}

// ---- Persistent permissions (cross-session) -------------------------------
//
// Verifies the per-project `Always` allowlist round-trips through disk:
// approving `Always` on one agent is visible to a fresh agent constructed
// against the same project root, and revoking is mirrored to disk too.
// Sub-agents (no project root) stay ephemeral and never touch the file.

#[tokio::test]
async fn always_permission_persists_across_agents_for_same_project() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = std::env::temp_dir().join(format!("neenee-perms-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    // `paths::get()` reads `NEENEE_DATA_DIR` on every call (no caching), so
    // pointing the env var at a tempdir redirects the project bucket there.
    std::env::set_var("NEENEE_DATA_DIR", &tmp);
    let project_root = std::path::PathBuf::from("/tmp/neenee-perms-fixture-project");
    let perms_path = neenee_store::paths::get().project_permissions(&project_root);

    // First agent: prompt for a write_test permission and approve Always.
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
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
    assert_eq!(on_disk["rules"][0]["scope"], "*");

    // A brand-new agent in the same project should inherit the rule without
    // ever prompting — that is the whole point of cross-session persistence.
    let agent2 = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    ));
    agent2.set_project_root(Some(project_root.clone()));
    assert_eq!(
        agent2.allowed_tools(),
        vec!["write_test *".to_string()],
        "fresh agent in the same project should inherit persisted Always rule"
    );

    // Revoking on agent2 must remove the rule from disk as well, so the next
    // session doesn't silently resurrect it.
    assert!(agent2.revoke_allowed_tool("write_test", "*"));
    let after_revoke: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&perms_path).unwrap()).unwrap();
    assert_eq!(after_revoke["rules"].as_array().unwrap().len(), 0);

    // A different project root must NOT see the first project's rules.
    let other_root = std::path::PathBuf::from("/tmp/neenee-perms-fixture-other-project");
    let agent3 = Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent3.set_project_root(Some(other_root));
    assert!(
        agent3.allowed_tools().is_empty(),
        "unrelated project must not inherit another project's rules"
    );

    std::env::remove_var("NEENEE_DATA_DIR");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn agent_without_project_root_never_writes_permissions_file() {
    let _guard = ENV_GUARD.lock().await;
    let tmp = std::env::temp_dir().join(format!("neenee-perms-noset-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).expect("create temp data dir");
    std::env::set_var("NEENEE_DATA_DIR", &tmp);
    let project_root = std::path::PathBuf::from("/tmp/neenee-perms-noset-fixture");
    let perms_path = neenee_store::paths::get().project_permissions(&project_root);

    // No set_project_root call: the agent stays ephemeral, so an Always
    // approval must not write any file (sub-agents behave the same way).
    let agent = Arc::new(Agent::new(
        Arc::new(TestProvider),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    ));
    // Mutations of the allowlist must be no-ops on disk when no project root
    // is set: no panic, no file created.
    agent.clear_allowed_tools();
    assert!(!agent.revoke_allowed_tool("anything", "*"));
    assert!(
        !perms_path.exists(),
        "ephemeral agent must not create a permissions file"
    );

    std::env::remove_var("NEENEE_DATA_DIR");
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
    // choosing to stop, not by raw round count (ADR-0009). Review is disabled
    // so the periodic diagnostic does not consume the shared scripted stream
    // at round 64 (ADR-0016).
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
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_review_config(ReviewConfig::disabled());

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Always).await;

    assert_eq!(outcome.unwrap().message.content, "all done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::SessionReview { .. })),
        "review disabled must keep the uncapped turn free of review events"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Session review (ADR-0016)
// ─────────────────────────────────────────────────────────────────────

/// Build a turn of N distinct read-only `alpha` calls (each with a different
/// path so `guard_repeated_call` does not trip), optionally followed by a
/// final text round. Drives the round counter past a review/hard-stop line
/// without tripping the exact-repeat guard.
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
fn review_config_getter_round_trips_setter() {
    // The /review slash command reads via `get_review_config` after writing
    // via `set_review_config`; the pair must round-trip, including the
    // disabled sentinel and a custom hard-stop budget.
    let agent = agent();
    assert_eq!(agent.get_review_config(), ReviewConfig::default());
    let mut cfg = agent.get_review_config();
    cfg.review_start_round = 5;
    cfg.review_interval_rounds = 4;
    cfg.hard_stop_rounds = 99;
    agent.set_review_config(cfg);
    let live = agent.get_review_config();
    assert_eq!(live.review_start_round, 5);
    assert_eq!(live.review_interval_rounds, 4);
    assert_eq!(live.hard_stop_rounds, 99);
    // disabled() suppresses review entirely.
    agent.set_review_config(ReviewConfig::disabled());
    assert!(!agent.review_enabled());
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

#[tokio::test]
async fn review_disabled_emits_no_event() {
    // start = 0 disables review: even with many distinct read rounds, no
    // SessionReview event fires and no reflection nudge is pushed. This is
    // pure ADR-0009 behaviour, and also the config sub-agents get.
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(distinct_read_rounds(
            20,
            Some("done"),
        ))),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_review_config(ReviewConfig::disabled());

    let mut messages = vec![Message::new(Role::User, "go")];
    let mut events = Vec::new();
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event);
        })
        .await
        .expect("disabled review → turn runs to completion");

    assert_eq!(outcome.message.content, "done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::SessionReview { .. })),
        "review disabled must suppress all review events"
    );
}

#[tokio::test]
async fn hard_stop_aborts_when_budget_configured() {
    // hard_stop_rounds is the only opt-in execution cap. With it set to 3 and
    // review disabled, the 3rd tool round trips the budget and the turn
    // aborts with the budget in the message.
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(distinct_read_rounds(10, None))),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    let mut cfg = ReviewConfig::disabled();
    cfg.hard_stop_rounds = 3;
    agent.set_review_config(cfg);

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
async fn review_fires_at_start_and_emits_event() {
    // With a low start line, the diagnostic fires after the start round. The
    // reviewer sub-agent shares the scripted provider: after the main loop's
    // two read rounds, it pops the next scripted round (a JSON verdict) and
    // returns it, emitting exactly one SessionReview event. A "stuck" verdict
    // also pushes the one-shot reflection nudge.
    let verdict_json =
        r#"{"verdicts":[{"dimension":"looping","status":"stuck","detail":"re-reading"}]}"#;
    let mut rounds = distinct_read_rounds(2, None);
    rounds.push(text_round(verdict_json));
    rounds.push(text_round("done"));

    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(rounds)),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_review_config(ReviewConfig {
        review_start_round: 2,
        review_interval_rounds: 16,
        hard_stop_rounds: 0,
    });

    let mut messages = vec![Message::new(Role::User, "go")];
    let mut events = Vec::new();
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event);
        })
        .await
        .expect("turn completes after the review + final text");

    assert_eq!(outcome.message.content, "done");
    let alerts: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::SessionReview { alert } if !alert.is_empty() => Some(alert.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        alerts.len(),
        1,
        "exactly one non-empty SessionReview alert must fire, got {alerts:?}"
    );
    assert!(alerts[0].contains("stuck"), "{}", alerts[0]);
    // The stuck verdict pushes the one-shot reflection nudge.
    assert!(messages.iter().any(|m| {
        m.role == Role::User
            && m.hidden
            && m.content
                .contains("session-health review judged this turn stuck")
    }));
}

// ─────────────────────────────────────────────────────────────────────
// Verify hard nudge
// ─────────────────────────────────────────────────────────────────────

#[test]
fn should_nudge_verify_only_when_build_mode_and_active_plan_and_no_prior_call() {
    use crate::agent::TurnState;
    let agent = agent();
    let mut state = TurnState::default();

    // No active plan → no nudge, even in Build mode.
    assert!(!agent.should_nudge_verify(&state));

    agent.set_active_plan_path(Some(std::path::PathBuf::from(".neenee/plans/x.md")));
    // Active plan + Build + no verify call + not nudged → nudge.
    assert!(agent.should_nudge_verify(&state));

    // Verify was called this turn → no nudge.
    state.verify_called_this_turn = true;
    assert!(!agent.should_nudge_verify(&state));

    // Reset and flip to Plan mode → no nudge, even with an active plan
    // (Plan mode means there is nothing to verify yet).
    state.verify_called_this_turn = false;
    agent.set_mode(AgentMode::Plan);
    assert!(!agent.should_nudge_verify(&state));

    // Back to Build, but already nudged → no second nudge.
    agent.set_mode(AgentMode::Build);
    state.verify_nudged = true;
    assert!(!agent.should_nudge_verify(&state));
}

#[tokio::test]
async fn verify_nudge_fires_once_then_lets_model_wrap_up() {
    // No tools needed — the gate fires on the text-only round before the
    // turn ends, before the model even has a chance to call anything.
    // The first round is text-only, which triggers the nudge; the second
    // round is text-only again, which the harness lets through because
    // `verify_nudged` is now true.
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![
            text_round("all done"),
            text_round("really done"),
        ])),
        Vec::new(),
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_active_plan_path(Some(std::path::PathBuf::from(".neenee/plans/x.md")));

    let mut messages = vec![Message::new(Role::User, "go")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await
        .expect("turn completes after the model's second text round");

    assert_eq!(outcome.message.content, "really done");
    let nudge_count = messages
        .iter()
        .filter(|m| m.role == Role::User && m.hidden && m.content.contains("verify_plan_execution"))
        .count();
    assert_eq!(
        nudge_count, 1,
        "verify nudge must fire exactly once per turn"
    );
}

#[tokio::test]
async fn verify_nudge_disabled_when_toggle_off() {
    // `set_verify_nudge_enabled(false)` opts out of the gate: the model
    // can end the turn with an approved plan and no verify call, and the
    // harness does not inject the reminder.
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(vec![text_round("all done")])),
        Vec::new(),
        AgentMode::Build,
        test_pursuit_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_active_plan_path(Some(std::path::PathBuf::from(".neenee/plans/x.md")));
    agent.set_verify_nudge_enabled(false);

    let mut messages = vec![Message::new(Role::User, "go")];
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |_| {})
        .await
        .expect("toggle off → turn ends immediately");

    assert_eq!(outcome.message.content, "all done");
    assert!(
        !messages.iter().any(|m| {
            m.role == Role::User && m.hidden && m.content.contains("verify_plan_execution")
        }),
        "verify nudge must not fire when the toggle is off"
    );
}

#[test]
fn agent_config_defaults_match_runtime_constants() {
    // The config struct's defaults must match the seeds the agent uses when
    // no config is loaded, so a missing `[agent]` table is indistinguishable
    // from one that explicitly sets the defaults (ADR-0016).
    use neenee_store::config::AgentConfig;
    let cfg = AgentConfig::default();
    assert_eq!(cfg.review, ReviewConfig::default());
    assert!(cfg.verify_nudge_enabled);
    // The agent seeds the same review config by default.
    let agent = agent();
    assert_eq!(agent.get_review_config(), ReviewConfig::default());
}

#[test]
fn verify_nudge_getter_round_trips_setter() {
    // Same contract for /verify-nudge: getter/setter pair must round-trip.
    let agent = agent();
    assert!(agent.get_verify_nudge_enabled());
    agent.set_verify_nudge_enabled(false);
    assert!(!agent.get_verify_nudge_enabled());
    agent.set_verify_nudge_enabled(true);
    assert!(agent.get_verify_nudge_enabled());
}
