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

fn test_goal_service() -> GoalService {
    GoalService::new(GoalStore::open_in_memory_blocking().expect("in-memory goal store"))
}

fn agent() -> Agent {
    Agent::new(
        Arc::new(TestProvider),
        Vec::new(),
        AgentMode::Build,
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    )
}

fn active_goal(objective: &str) -> Goal {
    Goal {
        objective: objective.to_string(),
        is_complete: false,
        checklist: Vec::new(),
    }
}

#[test]
fn goal_is_injected_into_system_prompt() {
    let agent = agent();
    agent.set_goal(active_goal("ship the harness"));

    let prompt = agent.build_system_prompt();

    assert!(prompt.contains("ship the harness"));
    assert!(prompt.contains("update_goal"));
}

#[test]
fn retry_metadata_is_not_exposed_as_public_error_text() {
    let encoded = retryable_error("rate limited", Some(500));
    assert_eq!(public_error_message(&encoded), "rate limited");
    assert_eq!(public_error_message("plain"), "plain");
}

#[test]
fn goal_lifecycle_is_explicit() {
    let agent = agent();
    agent.set_goal(active_goal("verify behavior"));
    assert!(!agent.get_goal().unwrap().is_complete);

    let mut completed = active_goal("verify behavior");
    completed.is_complete = true;
    agent.set_goal(completed);
    assert!(agent.get_goal().unwrap().is_complete);

    agent.clear_goal();
    assert_eq!(agent.get_goal(), None);
}

#[tokio::test]
async fn goal_checklist_controls_completion_readiness() {
    let agent = agent();
    agent.set_goal(active_goal("ship verified work"));
    let tool = agent
        .tools
        .iter()
        .find(|tool| tool.name() == "goal_checklist")
        .unwrap();

    tool.call(
        r#"{"items":[
                {"content":"implement","status":"completed"},
                {"content":"verify","status":"in_progress"}
            ]}"#,
    )
    .await
    .unwrap();
    assert!(!agent.goal_can_complete());

    tool.call(
        r#"{"items":[
                {"content":"implement","status":"completed"},
                {"content":"verify","status":"completed"}
            ]}"#,
    )
    .await
    .unwrap();
    assert!(agent.goal_can_complete());
}

#[tokio::test]
async fn goal_checklist_rejects_multiple_in_progress_items() {
    let agent = agent();
    agent.set_goal(active_goal("track work"));
    let tool = agent
        .tools
        .iter()
        .find(|tool| tool.name() == "goal_checklist")
        .unwrap();

    let error = tool
        .call(
            r#"{"items":[
                    {"content":"one","status":"in_progress"},
                    {"content":"two","status":"in_progress"}
                ]}"#,
        )
        .await
        .unwrap_err();

    assert!(error.contains("At most one"));
}

#[tokio::test]
async fn goal_checklist_cannot_be_silently_cleared() {
    let agent = agent();
    agent.set_goal(active_goal("track work"));
    let tool = agent
        .tools
        .iter()
        .find(|tool| tool.name() == "goal_checklist")
        .unwrap();
    tool.call(r#"{"items":[{"content":"verify","status":"pending"}]}"#)
        .await
        .unwrap();

    let error = tool.call(r#"{"items":[]}"#).await.unwrap_err();

    assert!(error.contains("cannot be cleared"));
    assert!(!agent.goal_can_complete());
}

#[test]
fn goal_checklist_updates_emit_harness_state() {
    let agent = agent();
    agent.set_goal(active_goal("track"));
    let call = ToolCall {
        id: "call".to_string(),
        name: "goal_checklist".to_string(),
        arguments: "{}".to_string(),
    };
    let mut events = Vec::new();

    agent.emit_goal_update(&call, &mut |event| events.push(event));

    assert!(matches!(
        events.as_slice(),
        [AgentEvent::GoalUpdated(Goal { objective, .. })] if objective == "track"
    ));
}

#[tokio::test]
async fn streaming_tool_deltas_are_reassembled_and_executed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        Arc::new(StreamingToolProvider(AtomicUsize::new(0))),
        vec![Arc::new(StreamingReadTool(calls.clone()))],
        AgentMode::Build,
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
    let goal_service = GoalService::new(
        GoalStore::open_in_memory()
            .await
            .expect("in-memory goal store"),
    );
    let agent = Agent::new(
        Arc::new(PermissionTestProvider(AtomicUsize::new(0))),
        vec![Arc::new(WriteTestTool)],
        AgentMode::Build,
        goal_service,
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
            AgentEvent::GoalUpdated(_) => "goal-updated".to_string(),
            AgentEvent::ModeChanged(mode) => format!("mode-changed {mode:?}"),
            AgentEvent::PlanProgressUpdated(progress) => format!(
                "plan-progress {:?}",
                progress.as_ref().map(|p| p.sections.len())
            ),
            AgentEvent::AutoApproveChanged(enabled) => format!("auto-approve {enabled}"),
            AgentEvent::StallWarning { consecutive_rounds } => {
                format!("stall-warning rounds={consecutive_rounds}")
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
        test_goal_service(),
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
    // 64 distinct tool rounds — well past the stall hard-stop — followed
    // by a text answer. Each read round uses a distinct argument so the
    // repeated-call guard never trips, and every 4th round is a Write so
    // the stall detector's read-only streak resets before it can trip.
    // This mirrors the new contract: the agent is bounded by *productive*
    // rounds, not by raw round count (ADR-0009 + stalled-agent-detection).
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
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Always).await;

    assert_eq!(outcome.unwrap().message.content, "all done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::StallWarning { .. })),
        "interleaved writes must keep the streak under the threshold"
    );
    assert!(
        !events.iter().any(|event| {
            matches!(event, AgentEvent::StallWarning { .. })
        }),
        "no convergence nudge should be injected anymore"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Stall detection (stalled-agent-detection.md)
// ─────────────────────────────────────────────────────────────────────

/// Build a turn of N distinct read-only `read_file` calls (each with a
/// different path so `guard_repeated_call` does not trip), followed by a
/// final text round. Used to drive the streak counter without touching
/// the exact-repeat guard.
fn readonly_rounds(n: usize, suffix: &str) -> Vec<Vec<ProviderStreamEvent>> {
    let mut rounds: Vec<Vec<ProviderStreamEvent>> = (0..n)
        .map(|i| tool_round(&[("c", "alpha", &format!("{{\"path\":\"f{i}\"}}"))]))
        .collect();
    rounds.push(text_round(suffix));
    rounds
}

#[tokio::test]
async fn stall_warning_fires_once_when_threshold_reached() {
    // STALL_THRESHOLD read-only rounds, then the model gives a text
    // answer. The streak trips exactly at the threshold, emitting one
    // StallWarning and one reflection nudge message.
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(readonly_rounds(
            STALL_THRESHOLD,
            "done",
        ))),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let mut messages = vec![Message::new(Role::User, "go")];
    let mut events = Vec::new();
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event);
        })
        .await
        .expect("turn completes after threshold + final text");

    assert_eq!(outcome.message.content, "done");
    let warnings: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::StallWarning { .. }))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "stall warning must fire exactly once per episode, got {warnings:?}"
    );
    assert!(messages.iter().any(|m| {
        m.role == Role::User
            && m.hidden
            && m.content.contains("exploration loop")
    }));
}

#[tokio::test]
async fn stall_hard_stops_after_nudge_is_ignored() {
    // The model keeps issuing distinct read-only calls well past the
    // hard-stop line. The harness aborts with a clear error rather than
    // looping indefinitely. Only one reflection nudge should ever fire
    // per episode (it's one-shot); subsequent StallWarning events do
    // keep firing so the activity-bar counter ticks up.
    let hard_stop = STALL_THRESHOLD + STALL_HARD_STOP_DELTA;
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(readonly_rounds(
            hard_stop + 1,
            "done",
        ))),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let mut messages = vec![Message::new(Role::User, "go")];
    let mut events = Vec::new();
    let error = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event);
        })
        .await
        .expect_err("hard-stop must abort the turn");

    let message = match error {
        HarnessError::Other(message) => message,
        other => panic!("expected HarnessError::Other, got {other:?}"),
    };
    assert!(
        message.contains("exploration loop"),
        "error must explain the stall, got: {message}"
    );
    let warnings = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::StallWarning { .. }))
        .count();
    // One StallWarning fires every round at/above the threshold (so the
    // TUI counter ticks up live), so over (hard_stop - threshold) rounds
    // we expect that many warnings — but only one reflection-nudge
    // message, since the nudge is one-shot.
    let expected_warnings = (hard_stop + 1) - STALL_THRESHOLD;
    assert_eq!(
        warnings, expected_warnings,
        "StallWarning must fire every round while stalled, but got {warnings}"
    );
    let nudges = messages.iter().filter(|m| {
        m.role == Role::User
            && m.hidden
            && m.content.contains("exploration loop")
    }).count();
    assert_eq!(nudges, 1, "reflection nudge must be one-shot per episode");
}

#[tokio::test]
async fn stall_streak_resets_after_a_productive_round() {
    // STALL_THRESHOLD - 1 read-only rounds (just under the threshold),
    // then a Write round resets the streak, then STALL_THRESHOLD - 1
    // more read-only rounds, then a final text round. Neither half
    // reaches the threshold on its own, so no StallWarning ever fires
    // and the turn ends cleanly.
    let read = RecordingTool::read("alpha", "A-out");
    let write = RecordingTool::write("writer", "WROTE");
    let sub = STALL_THRESHOLD - 1;
    let mut rounds: Vec<Vec<ProviderStreamEvent>> = (0..sub)
        .map(|i| tool_round(&[("c", "alpha", &format!("{{\"path\":\"a{i}\"}}"))]))
        .collect();
    rounds.push(tool_round(&[("cw", "writer", "{\"path\":\"x\"}")]));
    rounds.extend((0..sub).map(|i| {
        tool_round(&[("c", "alpha", &format!("{{\"path\":\"b{i}\"}}"))])
    }));
    rounds.push(text_round("done"));

    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(rounds)),
        vec![Arc::new(read), Arc::new(write)],
        AgentMode::Build,
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    );

    let (events, outcome) = run_golden_turn(&agent, "go", PermissionDecision::Always).await;
    assert_eq!(outcome.unwrap().message.content, "done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::StallWarning { .. })),
        "productive round must reset the streak so neither half trips the threshold"
    );
}

#[test]
fn call_was_productive_recognises_mutating_read_tools() {
    // The four mutating Read tools must count as productive for stall
    // detection, alongside any name we don't recognise (the access-bit
    // lookup happens elsewhere; the name list is the fallback for the
    // text-fallback path).
    use crate::agent::call_was_productive;
    assert!(call_was_productive("goal_checklist"));
    assert!(call_was_productive("plan_enter"));
    assert!(call_was_productive("plan_exit"));
    assert!(call_was_productive("update_plan_progress"));
    assert!(!call_was_productive("read_file"));
    assert!(!call_was_productive("grep"));
    assert!(!call_was_productive("verify_plan_execution"));
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
        test_goal_service(),
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
        .filter(|m| {
            m.role == Role::User
                && m.hidden
                && m.content.contains("verify_plan_execution")
        })
        .count();
    assert_eq!(
        nudge_count, 1,
        "verify nudge must fire exactly once per turn"
    );
}

#[tokio::test]
async fn stall_detection_disabled_when_threshold_is_zero() {
    // `set_stall_threshold(0)` opts out of detection entirely: even with
    // an unbounded stream of distinct read-only calls, no StallWarning
    // fires and no hard-stop trips. This is the documented escape hatch
    // for users who want pure ADR-0009 behaviour.
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(readonly_rounds(
            STALL_THRESHOLD + STALL_HARD_STOP_DELTA + 4,
            "done",
        ))),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_stall_threshold(0);

    let mut messages = vec![Message::new(Role::User, "go")];
    let mut events = Vec::new();
    let outcome = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event);
        })
        .await
        .expect("detection disabled → turn runs to completion");

    assert_eq!(outcome.message.content, "done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::StallWarning { .. })),
        "threshold = 0 must suppress all stall warnings"
    );
    assert!(
        !messages.iter().any(|m| {
            m.role == Role::User
                && m.hidden
                && m.content.contains("exploration loop")
        }),
        "threshold = 0 must suppress the reflection nudge"
    );
}

#[tokio::test]
async fn stall_threshold_can_be_lowered_at_runtime() {
    // A user-supplied threshold lower than the default must trip
    // proportionally sooner, confirming the runtime setter actually
    // reroutes detection (not just the seeded const).
    let custom = 3;
    let tool = RecordingTool::read("alpha", "A-out");
    let agent = Agent::new(
        Arc::new(ScriptedProvider::new(readonly_rounds(custom, "done"))),
        vec![Arc::new(tool)],
        AgentMode::Build,
        test_goal_service(),
        crate::skills::SkillRegistry::empty(),
    );
    agent.set_stall_threshold(custom);

    let mut messages = vec![Message::new(Role::User, "go")];
    let mut events = Vec::new();
    let _ = agent
        .run_streaming_with_events(&mut messages, &CancellationToken::new(), |event| {
            events.push(event);
        })
        .await;

    let warning_rounds: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::StallWarning { consecutive_rounds } if *consecutive_rounds > 0 => {
                Some(*consecutive_rounds)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        warning_rounds.first().copied(),
        Some(custom),
        "first warning must fire at the configured threshold, got {warning_rounds:?}"
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
        test_goal_service(),
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
            m.role == Role::User
                && m.hidden
                && m.content.contains("verify_plan_execution")
        }),
        "verify nudge must not fire when the toggle is off"
    );
}

#[test]
fn agent_config_defaults_match_runtime_constants() {
    // The config struct's defaults must match the const seeds the agent
    // uses when no config is loaded, so a missing `[agent]` table is
    // indistinguishable from one that explicitly sets the defaults.
    use neenee_store::config::AgentConfig;
    let cfg = AgentConfig::default();
    assert_eq!(cfg.stall_threshold, STALL_THRESHOLD);
    assert!(cfg.verify_nudge_enabled);
}

#[test]
fn stall_threshold_getter_round_trips_setter() {
    // The /stall-threshold slash command reads via `get_stall_threshold`
    // after writing via `set_stall_threshold`; the pair must round-trip
    // exactly, including the disable sentinel.
    let agent = agent();
    assert_eq!(agent.get_stall_threshold(), STALL_THRESHOLD);
    agent.set_stall_threshold(3);
    assert_eq!(agent.get_stall_threshold(), 3);
    agent.set_stall_threshold(0);
    assert_eq!(agent.get_stall_threshold(), 0);
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
