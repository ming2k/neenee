use super::*;
use futures::stream::{self, BoxStream};
use std::sync::atomic::{AtomicUsize, Ordering};

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
        status: GoalStatus::Active,
        checklist: Vec::new(),
        tokens_used: 0,
        token_budget: None,
        time_used_seconds: 0,
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
    assert_eq!(agent.get_goal().unwrap().status, GoalStatus::Active);

    let mut completed = active_goal("verify behavior");
    completed.status = GoalStatus::Complete;
    agent.set_goal(completed);
    assert_eq!(agent.get_goal().unwrap().status, GoalStatus::Complete);

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
