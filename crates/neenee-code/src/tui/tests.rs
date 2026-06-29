use super::*;
use neenee_core::{Message, Role, ToolCall};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;

use crate::tui::app::{App, CaretOwner};
use crate::tui::completion::CompletionKind;
use crate::tui::completion::{manual_walk, mention_range_at, path_query_match};
use crate::tui::config;
use crate::tui::event_loop::{display_status, focused_messages_mut};
use crate::tui::layout::{InteractiveTarget, LayoutMap};
use crate::tui::render::Theme;
use crate::tui::selection::{SelectionDrag, SelectionState};
use crate::tui::transcript::{
    finalize_streaming_reasoning, transcript_message_from_core, transcript_messages_from_core,
};
use crate::tui::{ActivityTab, Modal};
use neenee_core::{AgentRequest, ProviderPickerSnapshot};

use std::collections::HashMap;

#[test]
fn restored_history_hides_harness_messages() {
    assert!(transcript_message_from_core(Message::hidden(Role::User, "internal")).is_none());
    assert!(transcript_message_from_core(Message::new(Role::System, "system")).is_none());
}
#[test]
fn restored_history_uses_command_display_content() {
    let message = Message::new(Role::User, "Expanded internal prompt")
        .with_display_content("/review working-tree");
    let restored = transcript_message_from_core(message).unwrap();
    assert_eq!(restored.raw, "/review working-tree");
}

#[test]
fn restored_user_message_origin_inferred_from_shape() {
    use crate::tui::document::UserMessageOrigin;
    // A genuine chat prompt: no display_content, no leading `!`.
    let chat = transcript_message_from_core(Message::new(Role::User, "fix the bug")).unwrap();
    assert_eq!(chat.origin, UserMessageOrigin::Chat);

    // A slash command carries a `display_content` whose text is the literal
    // `/cmd` (its real content is the harness-expanded form) → Slash.
    let slash = Message::new(Role::User, "expanded pursue body")
        .with_display_content("/pursue ship the release");
    let slash = transcript_message_from_core(slash).unwrap();
    assert_eq!(slash.origin, UserMessageOrigin::Slash);

    // A shell passthrough persists as the `!command` the user typed → Shell.
    let shell = transcript_message_from_core(Message::new(Role::User, "!ls -la")).unwrap();
    assert_eq!(shell.origin, UserMessageOrigin::Shell);

    // A genuine prompt that merely *starts* with `/` (no display_content) is
    // NOT misclassified as a slash command — e.g. "/etc is a path" stays Chat.
    let path_like =
        transcript_message_from_core(Message::new(Role::User, "/etc is a path")).unwrap();
    assert_eq!(path_like.origin, UserMessageOrigin::Chat);
}

#[test]
fn restored_assistant_carries_provider_and_model_attribution() {
    // A persisted assistant message stamped by the harness keeps its
    // provider/model so a resumed session that mixed models stays
    // traceable in the transcript.
    let message = Message::new(Role::Assistant, "Hello from kimi")
        .with_attribution("kimi-code", "kimi-k2.7-code");
    let restored = transcript_message_from_core(message).unwrap();
    assert_eq!(restored.provider.as_deref(), Some("kimi-code"));
    assert_eq!(restored.model.as_deref(), Some("kimi-k2.7-code"));
    assert_eq!(
        restored.attribution_label(),
        Some(("kimi-code".to_string(), "kimi-k2.7-code".to_string()))
    );

    // A plain user message carries no attribution.
    let user = transcript_message_from_core(Message::new(Role::User, "hi")).unwrap();
    assert!(user.attribution_label().is_none());

    // A provider without an id still surfaces the model alone.
    let model_only = Message::new(Role::Assistant, "x").with_attribution("", "gpt-4o");
    let restored = transcript_message_from_core(model_only).unwrap();
    assert_eq!(
        restored.attribution_label(),
        Some((String::new(), "gpt-4o".to_string()))
    );
}

#[test]
fn restored_reasoning_is_not_shown_as_running() {
    let message = Message {
        role: Role::Assistant,
        content: String::new(),
        content_blob: None,
        display_content: None,
        reasoning_content: Some("step-by-step reasoning".to_string()),
        provider_meta: None,
        tool_calls: None,
        tool_call_id: None,
        images: None,
        provider: None,
        model: None,
        hidden: false,
        children: None,
        envoy_meta: None,
        origin: None,
    };

    let restored = transcript_messages_from_core(vec![message], &config::TuiConfig::default());
    assert_eq!(restored.len(), 1);
    let thinking = &restored[0];
    assert!(thinking.is_thinking());
    // A finished reasoning block must not be rendered with a live spinner.
    assert!(
        thinking.thinking_summary().unwrap().contains("0ms"),
        "restored thinking should have a finished duration, got {:?}",
        thinking.thinking_summary()
    );
}

#[test]
fn finalize_streaming_reasoning_freezes_orphaned_traces() {
    // An interrupt mid-reasoning leaves the in-flight Thinking message
    // with `duration_ms: None`, which the renderer treats as "running"
    // (breathing spinner). The sweep must stamp every such trace so the
    // spinner stops, while leaving already-finished traces untouched.
    let streaming = TranscriptMessage::thinking("partial reasoning");
    assert!(
        streaming.is_thinking_streaming(),
        "a fresh thinking trace should be in the streaming state"
    );

    let mut finished = TranscriptMessage::thinking("done reasoning");
    finished.set_thinking_duration(1234);
    assert!(
        !finished.is_thinking_streaming(),
        "a trace with a stamped duration is not streaming"
    );

    let other = TranscriptMessage::new(Role::User, "hi");

    let mut messages = vec![streaming.clone(), finished.clone(), other];
    finalize_streaming_reasoning(&mut messages, Some(500));

    // The orphaned streaming trace is frozen with the supplied duration.
    assert!(
        !messages[0].is_thinking_streaming(),
        "streaming trace must be finalized by the sweep"
    );
    assert!(
        messages[0].thinking_summary().unwrap().contains("500ms"),
        "expected the supplied duration to be stamped, got {:?}",
        messages[0].thinking_summary()
    );

    // The already-finished trace keeps its original duration (no overwrite
    // of real timing with the sweep's value).
    assert!(
        messages[1].thinking_summary().unwrap().contains("1.2s"),
        "finished trace must keep its original duration, got {:?}",
        messages[1].thinking_summary()
    );

    // A missing duration falls back to 0 so the trace still leaves the
    // streaming state even when the start instant was already consumed.
    let mut messages = vec![streaming];
    finalize_streaming_reasoning(&mut messages, None);
    assert!(
        !messages[0].is_thinking_streaming(),
        "a None duration must still finalize the trace"
    );
    assert!(
        messages[0].thinking_summary().unwrap().contains("0ms"),
        "expected 0ms fallback, got {:?}",
        messages[0].thinking_summary()
    );
}

#[test]
fn restored_native_tool_calls_are_visible() {
    let message = Message {
        role: Role::Assistant,
        content: String::new(),
        content_blob: None,
        display_content: None,
        reasoning_content: None,
        provider_meta: None,
        tool_calls: Some(vec![ToolCall {
            id: "call".to_string(),
            name: "read_text".to_string(),
            arguments: "{\"path\":\"README.md\"}".to_string(),
        }]),
        tool_call_id: None,
        images: None,
        provider: None,
        model: None,
        hidden: false,
        children: None,
        envoy_meta: None,
        origin: None,
    };

    let restored = transcript_message_from_core(message).unwrap();
    assert!(restored.raw.contains("read_text"));
}

#[test]
fn restored_tool_results_merge_into_steps_in_fifo_order() {
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: String::new(),
            content_blob: None,
            display_content: None,
            reasoning_content: None,
            provider_meta: None,
            tool_calls: Some(vec![
                ToolCall {
                    id: "one".to_string(),
                    name: "read_text".to_string(),
                    arguments: r#"{"path":"one"}"#.to_string(),
                },
                ToolCall {
                    id: "two".to_string(),
                    name: "read_text".to_string(),
                    arguments: r#"{"path":"two"}"#.to_string(),
                },
            ]),
            tool_call_id: None,
            images: None,
            provider: None,
            model: None,
            hidden: false,
            children: None,
            envoy_meta: None,
            origin: None,
        },
        Message::tool_result(
            &ToolCall {
                id: "one".to_string(),
                name: "read_text".to_string(),
                arguments: String::new(),
            },
            "[read_text result]:\nfirst",
        ),
        Message::tool_result(
            &ToolCall {
                id: "two".to_string(),
                name: "read_text".to_string(),
                arguments: String::new(),
            },
            "[read_text result]:\nsecond",
        ),
    ];

    let mut restored = transcript_messages_from_core(messages, &config::TuiConfig::default());
    assert_eq!(restored.len(), 2);
    restored[0].set_tool_step_expanded(true);
    restored[1].set_tool_step_expanded(true);
    assert!(restored[0].raw.contains("first"));
    assert!(!restored[0].raw.contains("second"));
    assert!(restored[1].raw.contains("second"));
}

#[test]
fn tool_activity_is_semantic_and_loop_progress_is_preserved() {
    assert_eq!(
        event_loop::tool_activity_status("grep"),
        "searching codebase"
    );
    assert_eq!(
        event_loop::tool_activity_status("edit_file"),
        "making edits"
    );
    assert_eq!(
        event_loop::tool_activity_status("mcp__github__search"),
        "using MCP"
    );
    assert_eq!(
        display_status("loop 2/8", "running command", false),
        "loop 2/8 · running command"
    );
    assert_eq!(
        display_status("loop 2/8", "running command", true),
        "loop 2/8 · awaiting permission"
    );
    assert_eq!(
        event_loop::compact_retry_reason("rate limited\nfull response body"),
        "rate limited"
    );
}

/// Build a small conversation with two sibling envoy tasks, each with a
/// couple of child messages, for focus-navigation tests.
fn conversation_with_envoys() -> Vec<TranscriptMessage> {
    let mut a = TranscriptMessage::tool_step(
        "task_a",
        "envoy",
        r#"{"description":"explore a","prompt":"..."}"#,
    );
    a.envoy_children_mut()
        .unwrap()
        .push(TranscriptMessage::new(Role::Assistant, "child A1"));
    let mut b = TranscriptMessage::tool_step(
        "task_b",
        "envoy",
        r#"{"description":"explore b","prompt":"..."}"#,
    );
    b.envoy_children_mut()
        .unwrap()
        .push(TranscriptMessage::new(Role::Assistant, "child B1"));
    vec![
        TranscriptMessage::new(Role::User, "hi"),
        a,
        TranscriptMessage::new(Role::Assistant, "ok"),
        b,
    ]
}

#[test]
fn resolve_focused_mut_indexes_root_when_unfocused() {
    let mut messages = conversation_with_envoys();
    let focus: Vec<crate::tui::app::ZoomFrame> = Vec::new();
    let resolved = event_loop::resolve_focused_mut(&mut messages, &focus, 2);
    assert_eq!(resolved.map(|m| m.raw.clone()).as_deref(), Some("ok"));
}

#[test]
fn resolve_focused_mut_indexes_children_when_focused() {
    let mut messages = conversation_with_envoys();
    let focus = vec![crate::tui::app::ZoomFrame {
        call_id: "task_b".to_string(),
        saved_scroll: crate::tui::app::ScrollSnapshot::default(),
    }];
    // Index 0 inside task_b's children => "child B1".
    let resolved = event_loop::resolve_focused_mut(&mut messages, &focus, 0);
    assert_eq!(resolved.map(|m| m.raw.clone()).as_deref(), Some("child B1"));
    // Indexing task_a's children via task_b focus returns none / out of range.
    assert!(event_loop::resolve_focused_mut(&mut messages, &focus, 5).is_none());
}

#[test]
fn focused_tool_steps_mut_only_touches_focused_envoy_children() {
    let mut messages = conversation_with_envoys();
    // Focused on task_a: its single child is an assistant message (not a
    // tool step), so the focused stream has 1 message and 0 tool steps.
    let focus = vec![crate::tui::app::ZoomFrame {
        call_id: "task_a".to_string(),
        saved_scroll: crate::tui::app::ScrollSnapshot::default(),
    }];
    let total = focused_messages_mut(&mut messages, &focus).count();
    assert_eq!(total, 1);
    let tool_steps = focused_messages_mut(&mut messages, &focus)
        .filter(|m| m.is_tool_step())
        .count();
    assert_eq!(tool_steps, 0);

    // Root view: 4 messages total, 2 of which are tool steps.
    let focus: Vec<crate::tui::app::ZoomFrame> = Vec::new();
    assert_eq!(focused_messages_mut(&mut messages, &focus).count(), 4);
    let tool_steps = focused_messages_mut(&mut messages, &focus)
        .filter(|m| m.is_tool_step())
        .count();
    assert_eq!(tool_steps, 2);
}

// ----- `@path` completion tests -----

#[test]
fn mention_range_detects_at_start_of_input() {
    // Cursor at end of `@src`: range covers the whole token.
    assert_eq!(mention_range_at("@src", 4), Some((0, 4)));
}

#[test]
fn mention_range_detects_inline_after_whitespace() {
    // `look at @src`: the `@` follows a space, so the range starts at the
    // `@` and ends at the cursor.
    assert_eq!(mention_range_at("look at @src", 12), Some((8, 12)));
}

#[test]
fn mention_range_rejects_email_style_at() {
    // `user@host` — the char before `@` is non-whitespace, so no mention.
    assert_eq!(mention_range_at("user@host", 9), None);
}

#[test]
fn mention_range_rejects_whitespace_between_at_and_cursor() {
    // `@src foo`: the cursor sits after a space, walking back crosses
    // whitespace before reaching `@`, so no mention.
    assert_eq!(mention_range_at("@src foo", 8), None);
}

#[test]
fn mention_range_rejects_cursor_before_at() {
    // Cursor before the `@`: nothing to walk back to.
    assert_eq!(mention_range_at("look @src", 4), None);
}

#[test]
fn mention_range_handles_multibyte_before_at() {
    // `😀😁 @x` — the `@` is preceded by an ASCII space, so we detect it
    // even when multibyte chars appear earlier in the input.
    let s = "😀😁 @x";
    // Byte offset of the cursor at end (after `x`).
    let cursor_byte = s.len();
    let at_byte = s.find('@').unwrap();
    assert_eq!(
        mention_range_at(s, cursor_byte),
        Some((at_byte, cursor_byte))
    );
}

#[test]
fn path_query_match_empty_query_keeps_top_level_only() {
    // Empty query: only top-level entries survive.
    assert!(path_query_match("Cargo.toml", ""));
    assert!(path_query_match("src/", ""));
    assert!(!path_query_match("src/main.rs", ""));
    assert!(!path_query_match("src/nested/deep.rs", ""));
}

#[test]
fn path_query_match_substring_case_insensitive() {
    // `@cargo` matches `Cargo.toml` regardless of case.
    assert!(path_query_match("Cargo.toml", "cargo"));
    assert!(path_query_match("src/Cargo.toml", "cargo"));
    assert!(!path_query_match("README.md", "cargo"));
}

#[test]
fn path_query_match_directory_descend_on_trailing_slash() {
    // `@src/` is a directory descend: prefix-match to enumerate its
    // descendants, NOT every path containing `src/` anywhere.
    assert!(path_query_match("src/main.rs", "src/"));
    assert!(path_query_match("src/components/button.rs", "src/"));
    assert!(!path_query_match("tests/src_runner.rs", "src/"));
}

#[test]
fn path_query_match_mid_path_substring() {
    // `@src/foo` falls through to plain substring (no trailing slash),
    // so it only matches paths that literally contain `src/foo`.
    assert!(path_query_match("src/foo.rs", "src/foo"));
    assert!(path_query_match("src/foo/bar.rs", "src/foo"));
    // `src/components/foo.rs` does NOT contain `src/foo` as a substring,
    // so it is excluded — the user can type `@foo` instead for a wider
    // filename match.
    assert!(!path_query_match("src/components/foo.rs", "src/foo"));
    assert!(!path_query_match("src/bar.rs", "src/foo"));
}

#[test]
fn history_rows_browses_reverse_then_ranks_search() {
    // The App-level view of the Ctrl+R modal. Browse mode (and any empty
    // query) lists history newest-first, unhighlighted; the search sub-layer
    // surfaces only the subsequence matches ordered by score with input order
    // on ties.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.input_history = vec![
        "scatter".to_string(),     // idx 0 — 'cat' mid-word, lowest score
        "catalog".to_string(),     // idx 1 — 'cat' at boundary, high score
        "cargo build".to_string(), // idx 2 — 'cat' is not a subsequence
        "the cat sat".to_string(), // idx 3 — 'cat' at boundary, high score
    ];

    // Browse mode → reverse-chronological (newest first), score 0, no
    // highlights. The newest entry (idx 3) is on top so an immediate Enter
    // re-inserts the last-typed prompt.
    app.history_search = false;
    app.input.clear();
    let rows = app.history_rows();
    let indices: Vec<usize> = rows.iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![3, 2, 1, 0], "newest first");
    for (_, m) in &rows {
        assert_eq!(m.score, 0);
        assert!(m.positions.is_empty());
    }

    // Search mode with an empty query still shows the reverse browse list.
    app.history_search = true;
    let indices: Vec<usize> = app.history_rows().iter().map(|(i, _)| *i).collect();
    assert_eq!(indices, vec![3, 2, 1, 0]);

    // Search "cat" → matches catalog, "the cat sat", and scatter; not
    // "cargo build" (no 't' after the 'ca'). Boundary matches outrank
    // scatter, and stable-sort keeps catalog before "the cat sat".
    app.input = "cat".to_string();
    let rows = app.history_rows();
    let indices: Vec<usize> = rows.iter().map(|(i, _)| *i).collect();
    assert_eq!(
        indices,
        vec![1, 3, 0],
        "boundary matches first, then scatter"
    );
    assert!(rows[0].1.score > rows[2].1.score);
    // Every matched entry exposes highlight positions, one per query char.
    for (_, m) in &rows {
        assert_eq!(m.positions.len(), 3);
    }

    // Query with no subsequence match → empty list (the renderer turns this
    // into the "no matches" placeholder).
    app.input = "xyz".to_string();
    assert!(app.history_rows().is_empty());
}

#[test]
fn history_modal_is_click_dismissable_and_restores_draft() {
    use crate::tui::Modal;
    // The history modal and the flat model picker join the click-outside-to-
    // dismiss set (their filter is ephemeral, the draft is parked); entry modals
    // that hold precious input (the editor) stay non-dismissable.
    assert!(Modal::HistorySearch.dismissable_by_outside_click());
    assert!(Modal::Provider.dismissable_by_outside_click());
    assert!(!Modal::ModelEditor.dismissable_by_outside_click());

    // restore_history_draft hands the parked composer draft back and clears the
    // search/preview sub-state — the shared teardown for Esc and outside-click.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.stashed_input = "my draft".to_string();
    app.input = "git".to_string(); // the live fuzzy query
    app.cursor_position = 3;
    app.history_search = true;
    app.history_preview = true;
    app.modal_index = 4;

    app.restore_history_draft();

    assert_eq!(app.input, "my draft", "draft restored from the stash");
    assert_eq!(app.cursor_position, "my draft".chars().count());
    assert!(app.stashed_input.is_empty());
    assert!(!app.history_search);
    assert!(!app.history_preview);
    assert_eq!(app.modal_index, 0);
}

/// Build a minimal `App` scoped to a tempdir project so we can exercise
/// the completion pipeline end-to-end without touching the user's real
/// filesystem. Mirrors how a real session captures cwd at startup.
fn app_in_tempdir(files: &[&str], dirs: &[&str]) -> (App, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    for d in dirs {
        std::fs::create_dir_all(tmp.path().join(d)).expect("mkdir");
    }
    for f in files {
        // Create parent dirs as needed so `src/foo.rs` lays down cleanly.
        let path = tmp.path().join(f);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir for file");
        }
        std::fs::write(path, "x").expect("write file");
    }
    let cwd = tmp.path().to_path_buf();
    let app = App {
        input: String::new(),
        messages: Vec::new(),
        messages_version: 0,
        side_messages: Vec::new(),
        side_messages_version: 0,
        layout_height_cache: Default::default(),
        in_side_view: false,
        side_session_id: None,
        parent_status: neenee_core::ParentStatus::Idle,
        scroll: 0,
        follow_bottom: true,
        content_lines: 0,
        view_height: 0,
        max_scroll: 0,
        sticky_step: None,
        sticky_rect: None,
        activity_rect: None,
        hint_context_rect: None,
        token_ledger: None,
        token_report_scroll: 0,
        todos_rect: None,
        modal_rect: None,
        sticky_summary_line: None,
        pin_summary_line: None,
        focus_stack: Vec::new(),
        tx: new_test_channel(),
        should_quit: Arc::new(AtomicBool::new(false)),
        serve_tap: Arc::new(tokio::sync::Mutex::new(None)),
        serve_cancel: None,
        suggestion_index: None,
        completion_dismissed: false,
        custom_commands: Vec::new(),
        cursor_position: 0,
        input_scroll: 0,
        active_modal: Modal::None,
        modal_index: 0,
        last_input_rect: neenee_tui::Rect::default(),
        cursor_sync_pending: false,
        cursor_visible: true,
        session_scroll: 0,
        session_modal_follow: true,
        permissions_scroll: 0,
        config_scroll: 0,
        skills_expanded: None,
        history_scroll: 0,
        history_modal_follow: true,
        history_preview: false,
        history_search: false,
        current_provider: "mock".to_string(),
        current_model: "mock".to_string(),
        cwd: cwd.clone(),
        path_scan_cache: None,
        current_pursuit: None,
        session_context: None,
        nudge_config: neenee_core::NudgeConfig::default(),
        loop_status: "idle".to_string(),
        activity_status: String::new(),
        unattended: false,
        todos: None,
        turn_count: 0,
        current_round: 0,
        review_alert: String::new(),
        turn_started_at: None,
        activity_tab: ActivityTab::Activity,
        activity_scroll: 0,
        help_scroll: 0,
        pending_permission: None,
        pending_input: None,
        question: None,
        question_scroll: 0,
        question_modal_follow: true,
        sessions_overview: Vec::new(),
        permission_confirm_always: false,
        permission_show_details: false,
        permission_scroll: 0,
        permission_max_scroll: 0,
        input_history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
        pending_images: Vec::new(),
        pending_text_pastes: Vec::new(),
        pending_dispatch: std::collections::VecDeque::new(),
        selection: SelectionState::None,
        drag: SelectionDrag::default(),
        layout_map: LayoutMap::new(),
        modal_hit_map: crate::tui::layout::ModalHitMap::new(),
        hovered_step: None,
        tool_density: Arc::new(AtomicBool::new(false)),
        transcript_layout: crate::tui::render::layout::Strategy::default(),
        focused_target: None,
        copy_toast_until: None,
        copy_toast_message: String::new(),
        copy_toast_failed: false,
        ctrl_c_armed_ticks: 0,
        esc_armed_ticks: 0,
        spinner_epoch: std::time::Instant::now(),
        stashed_input: String::new(),
        editor_target: None,
        editor_field: 0,
        editor_key: String::new(),
        editor_model: String::new(),
        editor_model_settings_only: false,
        editor_target_is_builtin: false,
        editor_effort: "high".to_string(),
        editor_thinking: true,
        custom_field: 0,
        custom_fields: Vec::new(),
        custom_protocol_wire: String::new(),
        custom_models: Vec::new(),
        custom_url_hint: String::new(),
        custom_suggest_index: 0,
        custom_edit_id: None,
        custom_name: String::new(),
        custom_base_url: String::new(),
        custom_token: String::new(),
        custom_model: String::new(),
        template_choice: 0,
        model_search: false,
        picker_provider: None,
        add_model_provider: None,
        add_model_choice: 0,
        model_scroll: 0,
        model_modal_follow: true,
        key_status: HashMap::new(),
        provider_picker: ProviderPickerSnapshot::default(),
        theme: Theme::default(),
        logo: None,
    };
    (app, tmp)
}

/// Stand-up helper for tests that just need a sender half of the agent
/// channel; the receiver is dropped because no test drives the agent loop.
fn new_test_channel() -> mpsc::UnboundedSender<AgentRequest> {
    let (tx, _rx) = mpsc::unbounded_channel();
    tx
}

#[test]
fn completions_returns_empty_when_input_does_not_trigger() {
    // Plain text without `@` or `/` produces no completions.
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
    app.input = "hello world".to_string();
    app.cursor_position = app.input.chars().count();
    assert!(app.completions().is_empty());
    assert_eq!(app.completion_kind(), CompletionKind::None);
}

#[test]
fn completions_classifies_slash_input_as_slash_kind() {
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
    app.input = "/pu".to_string();
    app.cursor_position = app.input.chars().count();
    let completions = app.completions();
    assert_eq!(app.completion_kind(), CompletionKind::Slash);
    assert!(completions.iter().any(|c| c.label == "/pursue"));
    // Slash candidates replace the whole input.
    for c in &completions {
        assert_eq!(c.replace_start, 0);
        assert_eq!(c.replace_end, app.input.len());
    }
}

/// The OpenAI-compatible template (Name / Base URL / Token / Model) — the
/// richest field set, used to exercise field cycling and the Model filter.
fn openai_template() -> &'static crate::tui::providers::ProviderTemplate {
    crate::tui::PROVIDER_TEMPLATES
        .iter()
        .find(|t| t.protocol == "openai")
        .expect("openai-compatible template")
}

/// The Anthropic relay template (Name / Base URL / Token), which seeds the Claude
/// family and exposes no Model field.
fn anthropic_template() -> &'static crate::tui::providers::ProviderTemplate {
    crate::tui::PROVIDER_TEMPLATES
        .iter()
        .find(|t| t.protocol == "anthropic")
        .expect("anthropic relay template")
}

#[test]
fn add_provider_row_opens_the_template_chooser() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.open_provider_template_chooser();
    assert!(app.active_modal == Modal::ProviderTemplate);
    assert_eq!(app.template_choice, 0);
    // `↑/↓` wrap across the template list.
    let n = crate::tui::PROVIDER_TEMPLATES.len();
    app.move_template_choice(false);
    assert_eq!(app.template_choice, n - 1, "wraps to the last template");
    app.move_template_choice(true);
    assert_eq!(app.template_choice, 0, "wraps back to the first");
}

#[test]
fn custom_provider_editor_opens_empty_on_name_field() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.custom_name = "stale".to_string();
    app.open_custom_provider_editor(openai_template());
    assert!(app.active_modal == Modal::CustomProvider);
    assert_eq!(app.custom_field, 0, "opens on the Name field");
    assert!(app.custom_name.is_empty(), "buffers reset on open");
    assert!(
        app.input.is_empty(),
        "Name field borrows an empty input line"
    );
    // The template seeds the protocol; the OpenAI template exposes a Model field.
    assert_eq!(app.custom_protocol_wire, "openai");
    assert!(app.custom_fields.contains(&crate::tui::CustomField::Model));
}

#[test]
fn anthropic_template_seeds_the_claude_family_without_a_model_field() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.open_custom_provider_editor(anthropic_template());
    assert_eq!(app.custom_protocol_wire, "anthropic");
    // The Claude family is seeded as the provider's model list…
    assert!(app.custom_models.len() > 1, "seeds multiple Claude models");
    assert!(app.custom_models.iter().any(|m| m.starts_with("claude-")));
    // …and there is no Model field (models are fixed by the template).
    assert!(!app.custom_fields.contains(&crate::tui::CustomField::Model));
}

#[test]
fn custom_provider_field_cycle_wraps_and_swaps_buffers() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.open_custom_provider_editor(openai_template());
    // Fields: Name(0) / Base URL(1) / Token(2) / Model(3).
    let n = app.custom_fields.len() as u8;
    // Type a name, then advance: the name is stashed and the Base URL field
    // loads its (empty) buffer.
    app.input = "My Relay".to_string();
    app.cycle_custom_field(true);
    assert_eq!(app.custom_field, 1);
    assert_eq!(app.custom_name, "My Relay");
    assert!(app.input.is_empty(), "Base URL buffer is empty");
    // Wrap backward from Name (0) to the last field (Model).
    app.cycle_custom_field(false); // 1 -> 0
    assert_eq!(app.custom_field, 0);
    assert_eq!(app.input, "My Relay", "Name buffer reloads into the line");
    app.cycle_custom_field(false); // 0 -> n-1 (wrap)
    assert_eq!(app.custom_field, n - 1);
}

#[test]
fn custom_provider_model_filter_commits_and_offers_custom_id() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.open_custom_provider_editor(openai_template());
    // The default model is the first candidate of the template's (OpenAI) protocol.
    assert!(
        app.custom_model_candidates()
            .contains(&app.custom_model.as_str())
    );
    // Focus the Model filter field (the last field) and type a known model.
    app.custom_field = app.custom_fields.len() as u8 - 1;
    assert_eq!(
        app.current_custom_field(),
        Some(crate::tui::CustomField::Model)
    );
    app.load_custom_field();
    app.input = "gpt-4o".to_string();
    app.on_custom_filter_changed();
    assert_eq!(app.custom_model, "gpt-4o");
    // A query matching nothing in the registry is still offered as a custom id.
    app.input = "my-private-model".to_string();
    app.on_custom_filter_changed();
    assert_eq!(app.custom_model, "my-private-model");
}

#[test]
fn picker_add_row_is_the_trailing_stage1_row() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::Provider;
    app.picker_provider = None;
    // Seed a few snapshot rows so providers_filtered() renders the full list
    // (the picker is snapshot-driven).
    let row = |id: &str| neenee_core::ProviderPickerRow {
        id: id.to_string(),
        name: id.to_string(),
        model: "m".to_string(),
        models: vec!["m".to_string()],
        model_info: Vec::new(),
        builtin: true,
        protocol: String::new(),
        base_url: String::new(),
        key_ready: true,
        favorite: false,
        last_used_ms: None,
    };
    app.provider_picker = neenee_core::ProviderPickerSnapshot {
        default_id: "kimi-code".to_string(),
        rows: vec![row("kimi-code"), row("openai"), row("anthropic")],
    };
    // The add row sits just past the provider rows and is counted as selectable.
    let providers = app.providers_filtered().len();
    assert!(providers > 0, "snapshot seeds the full provider list");
    assert_eq!(app.picker_row_count(), providers + 1);
    app.modal_index = providers;
    assert!(app.picker_on_add_row(), "last stage-1 row is the add row");
    app.modal_index = providers - 1;
    assert!(!app.picker_on_add_row());
}

#[test]
fn accept_slash_completion_does_not_append_trailing_space() {
    // Accepting a slash-command completion must splice the bare label with
    // NO trailing space. A trailing `/pursue ` would immediately match the
    // subcommand prefix and re-trigger the completion menu — the opposite
    // of "Enter/Tab finishes the completion". The user opts into subcommand
    // discovery by typing a space themselves.
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
    app.input = "/pu".to_string();
    app.cursor_position = app.input.chars().count();
    let completions = app.completions();
    let idx = completions
        .iter()
        .position(|c| c.label == "/pursue")
        .expect("/pursue in candidates");
    app.accept_completion(idx);
    // The label is spliced verbatim — no trailing space.
    assert_eq!(app.input, "/pursue");
    assert_eq!(app.cursor_position, "/pursue".chars().count());
    // A slash accept is a terminal commit: the popup must stay hidden and
    // no subcommand menu may fire. This holds for BOTH Tab and Enter since
    // both route through accept_completion for slash commands.
    assert!(
        app.completion_dismissed,
        "slash accept must latch dismissal"
    );
    assert!(app.suggestion_index.is_none(), "highlight cleared");
    assert!(
        app.completions()
            .iter()
            .all(|c| !c.label.starts_with("/pursue ")),
        "subcommand menu must not fire after accepting a slash completion"
    );
}

#[test]
fn accept_path_completion_stays_live_for_tab_cycling() {
    // `@path` accepts are NOT terminal: Tab is meant to keep cycling the
    // surviving candidates, so accept_completion must not latch the
    // dismissal flag for path mentions. This guards against the slash
    // terminal-accept logic accidentally suppressing path cycling.
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml", "README.md"], &[]);
    app.input = "@".to_string();
    app.cursor_position = 1;
    let completions = app.completions();
    assert!(completions.len() >= 2, "multiple path candidates");
    app.accept_completion(0);
    // Path accept must NOT latch dismissal — Tab cycling continues.
    assert!(
        !app.completion_dismissed,
        "path accept must stay live for Tab cycling"
    );
}

#[test]
fn accept_path_completion_appends_trailing_space() {
    // Path mentions still append a trailing space (matches opencode) so the
    // user can keep typing their message. This guards against the slash fix
    // accidentally suppressing the space for file completions too.
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
    app.input = "@Ca".to_string();
    app.cursor_position = app.input.chars().count();
    let completions = app.completions();
    let idx = completions
        .iter()
        .position(|c| c.label == "Cargo.toml")
        .expect("Cargo.toml in candidates");
    app.accept_completion(idx);
    assert_eq!(app.input, "@Cargo.toml ");
}

#[test]
fn completions_path_returns_top_level_for_bare_at() {
    // A bare `@` lists top-level entries only: the file plus the
    // synthesized top-level directory entry.
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml", "src/main.rs", "README.md"], &["src"]);
    app.input = "@".to_string();
    app.cursor_position = 1;
    let completions = app.completions();
    assert_eq!(app.completion_kind(), CompletionKind::Path);

    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // Dirs come first alphabetically, then files alphabetically.
    assert!(labels.contains(&"src/"));
    assert!(labels.contains(&"Cargo.toml"));
    assert!(labels.contains(&"README.md"));
    // No nested paths leak into the bare-`@` menu.
    assert!(!labels.iter().any(|l| l.contains("main.rs")));
    // Replace range points just past the `@` (byte 1), ends at cursor (1).
    for c in &completions {
        assert_eq!(c.replace_start, 1);
        assert_eq!(c.replace_end, 1);
        assert!(c.description.is_empty(), "path menu carries no description");
    }
}

#[test]
fn completions_path_descends_into_subdirectory() {
    // `@src/` triggers directory descend: only paths under `src/` match.
    let (mut app, _tmp) = app_in_tempdir(
        &["src/main.rs", "src/util/mod.rs", "tests/smoke.rs"],
        &["src", "src/util", "tests"],
    );
    app.input = "@src/".to_string();
    app.cursor_position = app.input.chars().count();
    let completions = app.completions();
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"src/"));
    assert!(labels.contains(&"src/main.rs"));
    assert!(labels.contains(&"src/util/"));
    assert!(labels.contains(&"src/util/mod.rs"));
    // Nothing from `tests/` leaks in — descend is a prefix match.
    assert!(!labels.iter().any(|l| l.contains("tests")));
}

#[test]
fn completions_path_substring_match_picks_files_across_dirs() {
    // `@main` finds `src/main.rs` via substring match.
    let (mut app, _tmp) = app_in_tempdir(&["src/main.rs", "lib/other.rs"], &["src", "lib"]);
    app.input = "@main".to_string();
    app.cursor_position = app.input.chars().count();
    let completions = app.completions();
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"src/main.rs"));
    assert!(!labels.iter().any(|l| l.contains("other.rs")));
}

#[test]
fn completions_path_skips_dotgit_directory() {
    // `.git/` is always excluded even though hidden files are kept.
    let (mut app, _tmp) = app_in_tempdir(
        &[".git/HEAD", ".git/config", "src/main.rs", ".env"],
        &[".git", "src"],
    );
    app.input = "@".to_string();
    app.cursor_position = 1;
    let completions = app.completions();
    let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
    // Hidden files like `.env` are listed; `.git/` and its contents are not.
    assert!(labels.contains(&".env"));
    assert!(labels.contains(&"src/"));
    assert!(!labels.iter().any(|l| l.starts_with(".git")));
}

#[test]
fn completions_path_cache_populated_once() {
    // The scan should run only the first time `@` triggers; we verify by
    // observing `path_scan_cache` transitioning from None to Some.
    let (mut app, _tmp) = app_in_tempdir(&["Cargo.toml"], &[]);
    assert!(app.path_scan_cache.is_none());
    app.input = "@".to_string();
    app.cursor_position = 1;
    let _ = app.completions();
    let first_scan = app
        .path_scan_cache
        .as_ref()
        .expect("scan populated")
        .clone();
    // A second call must not re-scan: cache stays the same Vec pointer
    // content. We compare lengths because the Vec itself may move.
    app.input = "@Ca".to_string();
    app.cursor_position = app.input.chars().count();
    let _ = app.completions();
    let second_scan = app
        .path_scan_cache
        .as_ref()
        .expect("scan still populated")
        .clone();
    assert_eq!(first_scan.entries, second_scan.entries);
}

#[test]
fn manual_walk_returns_files_and_synthesized_dirs() {
    // The manual fallback path (used when rg is missing) must still
    // produce directory entries with trailing slashes and skip `.git`.
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
    std::fs::write(tmp.path().join("src/nested/foo.rs"), "x").unwrap();
    std::fs::write(tmp.path().join("top.md"), "x").unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    std::fs::write(tmp.path().join(".git/HEAD"), "x").unwrap();

    let entries = manual_walk(tmp.path());
    assert!(entries.contains(&"top.md".to_string()));
    assert!(entries.contains(&"src/".to_string()));
    assert!(entries.contains(&"src/nested/".to_string()));
    assert!(entries.contains(&"src/nested/foo.rs".to_string()));
    assert!(!entries.iter().any(|e| e.starts_with(".git")));
}

use crate::tui::app::QueuedDispatch;
use crate::tui::document::DeliveryStatus;

/// Build a transcript Vec mimicking what the live SendChat handler
/// produces for a queued user message: a `Role::User` message carrying
/// `DeliveryStatus::Queued`. Used by the queue-semantics tests below.
fn queued_user_message(text: &str) -> TranscriptMessage {
    TranscriptMessage::new(Role::User, text).queued()
}

#[test]
fn queued_dispatch_carries_text_and_images() {
    // Smoke-check the struct's fields are wired as expected by the
    // SendChat and recall paths. Locks the field names + types so a
    // refactor can't quietly drop the images payload.
    let d = QueuedDispatch {
        text: "hello".to_string(),
        images: vec![neenee_core::ImagePart {
            mime: "image/png".to_string(),
            data: "base64".to_string(),
        }],
        text_pastes: Vec::new(),
    };
    assert_eq!(d.text, "hello");
    assert_eq!(d.images.len(), 1);
    assert_eq!(d.images[0].mime, "image/png");
}

#[test]
fn recall_queued_is_lifo_and_restores_input() {
    // Two messages staged in FIFO dispatch order; recall pops them in
    // reverse (LIFO undo), restores each one's text into the input box,
    // and removes the matching transcript marker each time. After both
    // recalls the queue is empty and recall returns false.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.pending_dispatch.push_back(QueuedDispatch {
        text: "first".to_string(),
        images: Vec::new(),
        text_pastes: Vec::new(),
    });
    app.pending_dispatch.push_back(QueuedDispatch {
        text: "second".to_string(),
        images: Vec::new(),
        text_pastes: Vec::new(),
    });
    let mut messages = vec![queued_user_message("first"), queued_user_message("second")];

    // First recall: most-recently-queued = "second".
    assert!(app.recall_queued(&mut messages));
    assert_eq!(app.input, "second");
    assert_eq!(app.cursor_position, "second".chars().count());
    assert_eq!(
        app.history_index, None,
        "history cursor must be cleared so ↓ returns to empty input"
    );
    // The matching transcript marker is removed.
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].raw, "first");
    assert_eq!(messages[0].delivery, DeliveryStatus::Queued);

    // Second recall: now "first".
    assert!(app.recall_queued(&mut messages));
    assert_eq!(app.input, "first");
    assert!(messages.is_empty(), "all queued markers drained");

    // Third recall: queue empty → no-op, returns false.
    assert!(!app.recall_queued(&mut messages));
    assert_eq!(
        app.input, "first",
        "input must be untouched when the queue is empty"
    );
}

#[test]
fn recall_queued_restores_staged_images() {
    // Images staged with the queued message (Ctrl+V before pressing
    // Enter) come back alongside the text so the user can re-edit and
    // resend without losing the attachment.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    let image = neenee_core::ImagePart {
        mime: "image/png".to_string(),
        data: "abc".to_string(),
    };
    app.pending_dispatch.push_back(QueuedDispatch {
        text: "look at this".to_string(),
        images: vec![image.clone()],
        text_pastes: Vec::new(),
    });
    let mut messages = vec![queued_user_message("look at this")];

    assert!(app.recall_queued(&mut messages));
    assert_eq!(app.input, "look at this");
    assert_eq!(
        app.pending_images.len(),
        1,
        "recalled images must land back in pending_images for resend"
    );
    assert_eq!(app.pending_images[0].data, image.data);
}

#[test]
fn recall_queued_skips_delivered_markers() {
    // Only Queued markers are eligible for recall. A delivered user
    // message in the transcript (e.g. one that already shipped) is
    // never removed even if the queue somehow holds an extra entry —
    // the rposition predicate filters by `DeliveryStatus::Queued`.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.pending_dispatch.push_back(QueuedDispatch {
        text: "queued".to_string(),
        images: Vec::new(),
        text_pastes: Vec::new(),
    });
    let delivered = TranscriptMessage::new(Role::User, "already sent");
    let queued = queued_user_message("queued");
    // Delivered user message precedes the queued one in transcript order.
    let mut messages = vec![delivered, queued];

    assert!(app.recall_queued(&mut messages));
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].raw, "already sent");
    assert_eq!(messages[0].delivery, DeliveryStatus::Delivered);
}

#[test]
fn recall_queued_latches_completion_dismissal() {
    // A recall replaces `input` programmatically (not via a keystroke), so it
    // must latch `completion_dismissed` the same way a slash-command accept
    // does. Otherwise recalling a queued `/help` would immediately re-open the
    // slash-completion popup — a spurious "complete" step the user never asked
    // for. Mirrors the latch in the history-navigation paths.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.pending_dispatch.push_back(QueuedDispatch {
        text: "/help".to_string(),
        images: Vec::new(),
        text_pastes: Vec::new(),
    });
    let mut messages = vec![queued_user_message("/help")];

    assert!(app.recall_queued(&mut messages));
    assert_eq!(app.input, "/help");
    assert!(
        app.completion_dismissed,
        "recall must latch dismissal so the slash popup stays hidden"
    );
    assert!(
        app.suggestion_index.is_none(),
        "recall must clear the completion highlight"
    );
    // The completions for `/help` are non-empty, so the latch is the only thing
    // keeping the render gate (`!completion_dismissed`) from drawing the menu.
    assert!(
        !app.completions().is_empty(),
        "`/help` should have candidates"
    );
}

#[test]
fn modal_paste_splices_text_inline_stripping_newlines() {
    // Pasting into a free-text modal field (here the provider editor's
    // API-key field) splices the text at the cursor and collapses newlines
    // so a copied multi-line block pastes as one continuous single line,
    // matching the single-line semantics the modal already enforces. No
    // chip is inserted and no attachment is staged.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::ModelEditor;
    app.editor_field = 0;
    app.input = "sk-".to_string();
    app.cursor_position = app.input.chars().count();

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Text("abc\ndef\n".to_string()),
    );

    assert_eq!(app.input, "sk-abcdef");
    assert_eq!(app.cursor_position, "sk-abcdef".chars().count());
    assert!(
        app.pending_text_pastes.is_empty(),
        "no chip staging in modals"
    );
    assert!(
        !app.input.contains("Pasted text"),
        "no chip label in modals"
    );
}

#[test]
fn modal_paste_inserts_at_cursor_not_at_end() {
    // The splice honors the cursor position, so a paste in the middle of
    // an existing field inserts between the surrounding characters rather
    // than appending.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::ModelEditor;
    app.editor_field = 1;
    app.input = "gpt-4omini".to_string();
    app.cursor_position = "gpt-4o".chars().count();

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Text("turbo".to_string()),
    );

    assert_eq!(app.input, "gpt-4oturbomini");
    assert_eq!(
        app.cursor_position,
        "gpt-4oturbo".chars().count(),
        "cursor lands just past the inserted text"
    );
}

#[test]
fn modal_paste_applies_to_provider_picker_and_history_search() {
    // The inline paste path is shared by every free-text modal that borrows
    // the input line, so the provider picker filter and the history search
    // query paste the same way as the editor.
    for modal in [Modal::Provider, Modal::HistorySearch] {
        let (mut app, _tmp) = app_in_tempdir(&[], &[]);
        app.active_modal = modal;
        app.input = String::new();
        app.cursor_position = 0;

        clipboard_ops::apply_clipboard_paste(
            &mut app,
            crate::tui::clipboard::ClipboardRead::Text("query".to_string()),
        );

        assert_eq!(
            app.input, "query",
            "paste should inline into free-text modal"
        );
        assert_eq!(app.cursor_position, "query".chars().count());
        assert!(app.pending_text_pastes.is_empty());
    }
}

#[test]
fn modal_paste_drops_image_with_failure_toast() {
    // An image paste has nowhere to go in a single-line modal field, so it
    // is dropped with a failure toast rather than silently lost or staged
    // as an attachment.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::ModelEditor;
    app.input = String::new();
    app.cursor_position = 0;

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Image {
            data: vec![0x89, 0x50, 0x4e, 0x47],
            mime: "image/png".to_string(),
        },
    );

    assert!(app.input.is_empty(), "image paste must not insert text");
    assert!(
        app.pending_images.is_empty(),
        "no attachment staging in modals"
    );
    assert!(
        app.copy_toast_failed,
        "image paste in a modal should toast a failure"
    );
    assert!(app.copy_toast_until.is_some());
}

#[test]
fn composer_paste_still_chips_large_text_on_main_prompt() {
    // The main-prompt path is unchanged: a large paste collapses into a
    // `[Pasted text #N +M lines]` chip and stages the full text, so the
    // modal-aware branching did not regress the composer behaviour.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::None;
    app.input = String::new();
    app.cursor_position = 0;
    let big = format!("line\n{}", "x".repeat(2048));

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Text(big.clone()),
    );

    assert!(
        app.input.contains("Pasted text #1"),
        "large paste on the main prompt should produce a chip"
    );
    assert_eq!(app.pending_text_pastes.len(), 1);
    assert_eq!(app.pending_text_pastes[0], big);
}

#[test]
fn paste_in_readonly_modal_is_dropped_silently() {
    // Read-only / non-text modals (Help, Sessions, Permission, ...) drop a
    // paste silently — no insertion, no toast, no attachment.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::Help;
    app.input = String::new();
    app.cursor_position = 0;

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Text("ignored".to_string()),
    );

    assert!(app.input.is_empty());
    assert!(
        app.copy_toast_until.is_none(),
        "readonly modal paste should not toast"
    );
    assert!(app.pending_text_pastes.is_empty());
}

#[test]
fn composer_image_paste_rejected_when_model_lacks_vision() {
    // When the current model doesn't support vision, pasting an image on
    // the main prompt should show a failure toast and leave no attachment.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::None;
    app.current_model = "glm-5.2".to_string(); // vision: false
    app.input = String::new();
    app.cursor_position = 0;

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Image {
            data: vec![0x89, 0x50, 0x4e, 0x47],
            mime: "image/png".to_string(),
        },
    );

    assert!(
        app.pending_images.is_empty(),
        "non-vision model must not stage image attachments"
    );
    assert!(
        app.copy_toast_failed,
        "non-vision model should toast a failure on image paste"
    );
    assert!(
        app.copy_toast_message.contains("does not support images"),
        "toast should say the model doesn't support images, got: {}",
        app.copy_toast_message,
    );
    assert!(app.copy_toast_until.is_some());
}

#[test]
fn composer_image_paste_accepted_when_model_has_vision() {
    // When the current model supports vision, pasting an image on the main
    // prompt should stage the attachment and insert an `[Image #N]` chip.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.active_modal = Modal::None;
    app.current_model = "gpt-4o".to_string(); // vision: true
    app.input = String::new();
    app.cursor_position = 0;

    clipboard_ops::apply_clipboard_paste(
        &mut app,
        crate::tui::clipboard::ClipboardRead::Image {
            data: vec![0x89, 0x50, 0x4e, 0x47],
            mime: "image/png".to_string(),
        },
    );

    assert_eq!(
        app.pending_images.len(),
        1,
        "vision-capable model should stage the image attachment"
    );
    assert!(
        app.input.contains("[Image #1]"),
        "image chip should be inserted into the input, got: {}",
        app.input,
    );
    assert!(
        !app.copy_toast_failed,
        "vision-capable model should show a success toast"
    );
    assert!(app.copy_toast_until.is_some());
}

#[test]
fn set_cursor_marks_immediate_sync_pending() {
    // The IME-correctness fix hinges on every caret move routing through
    // `set_cursor` so the event loop's immediate flush re-anchors the
    // terminal cursor before the next frame. A raw write to
    // `cursor_position` would silently skip it. This locks the contract.
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.input = "hello".to_string();
    app.cursor_sync_pending = false;

    app.set_cursor(3);
    assert_eq!(app.cursor_position, 3);
    assert!(
        app.cursor_sync_pending,
        "set_cursor must arm the immediate cursor sync — the whole IME fix depends on it"
    );

    // set_cursor_end is the common post-replacement helper and must do the same.
    app.cursor_sync_pending = false;
    app.set_cursor_end();
    assert_eq!(app.cursor_position, 5);
    assert!(
        app.cursor_sync_pending,
        "set_cursor_end must also arm the sync"
    );
}

// ── Caret ownership / visibility (IME anchor) ─────────────────────────────
// `App::caret_owner` / `App::caret_visible` are the single source of truth for
// which surface holds the terminal cursor. The IME anchors its composition
// window to that cursor, so any state that owns no caret must hide it —
// otherwise the IME binds to a stale coordinate (the "drift" when a disclosure
// is clicked mid-composition). These lock the contract for every state.

#[test]
fn caret_owner_composer_by_default() {
    let (app, _tmp) = app_in_tempdir(&[], &[]);
    assert_eq!(app.caret_owner(), CaretOwner::Composer);
    assert!(
        app.caret_visible(),
        "no modal, no focus, no selection → visible"
    );
}

#[test]
fn caret_owner_none_when_step_focused() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.focused_target = Some(InteractiveTarget::tool_step(0));
    assert_eq!(app.caret_owner(), CaretOwner::None);
    assert!(
        !app.caret_visible(),
        "a focused transcript step owns no caret → hidden, IME unanchored"
    );
}

#[test]
fn caret_owner_none_in_envoy_view() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    app.enter_envoy("call-1".to_string());
    assert_eq!(app.caret_owner(), CaretOwner::None);
    assert!(
        !app.caret_visible(),
        "envoy zoom has no input line → cursor hidden, IME unanchored"
    );
}

#[test]
fn caret_owner_modal_for_caret_modals() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    for modal in [
        Modal::Provider,
        Modal::ModelEditor,
        Modal::AddModel,
        Modal::CustomProvider,
        Modal::HistorySearch,
    ] {
        app.active_modal = modal;
        assert_eq!(
            app.caret_owner(),
            CaretOwner::Modal,
            "{modal:?} borrows the input line and renders its own caret",
        );
        assert!(
            app.caret_visible(),
            "{modal:?} must keep the cursor visible so the IME anchors to its field",
        );
    }
}

#[test]
fn caret_owner_none_for_read_only_and_decision_modals() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    for modal in [
        Modal::Help,
        Modal::Sessions,
        Modal::Tools,
        Modal::Mcp,
        Modal::Permissions,
        Modal::Activity,
        // `Question` is listed here to cover the *default* state — any option
        // but "Other" highlighted (or no question model at all). Its caret
        // ownership is conditional: see `caret_owner_question_owns_caret_only_on_other`.
        Modal::Question,
        Modal::Permission,
        Modal::InputInjection,
    ] {
        app.active_modal = modal;
        assert_eq!(
            app.caret_owner(),
            CaretOwner::None,
            "{modal:?} renders no caret → cursor must hide so the IME has no stale anchor",
        );
        assert!(
            !app.caret_visible(),
            "{modal:?} must hide the terminal cursor",
        );
    }
}

#[test]
fn caret_owner_question_owns_caret_only_on_other() {
    // The Question modal is a decision sheet (no caret) EXCEPT while the
    // synthetic "Other" free-text row is highlighted — then it is a real
    // text-input surface and must own the terminal cursor so the host IME can
    // anchor its composition window. Navigating to/from "Other" flips
    // ownership, so the IME anchor appears exactly when there is a field to
    // type into and never when there is not.
    use crate::tui::question_model::{QuestionAction, QuestionModel};
    use neenee_core::{UserQuestion, UserQuestionOption, UserQuestionRequest};

    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    let req = UserQuestionRequest {
        id: "q".into(),
        questions: vec![UserQuestion {
            header: None,
            question: "pick".into(),
            options: vec![
                UserQuestionOption {
                    label: "a".into(),
                    description: None,
                },
                UserQuestionOption {
                    label: "b".into(),
                    description: None,
                },
            ],
            multi_select: false,
        }],
    };
    // Open: highlight on row 0 (a real option) → no caret, cursor hidden.
    let model = QuestionModel::open(req);
    app.active_modal = Modal::Question;
    app.question = Some(model.clone());
    assert_eq!(
        app.caret_owner(),
        CaretOwner::None,
        "real option → no caret"
    );
    assert!(
        !app.caret_visible(),
        "a non-Other option must hide the cursor so the IME has no stale anchor",
    );

    // Navigate down to "Other" (index 2) → caret owned, cursor visible.
    let model = model.update(QuestionAction::Down).0; // -> b (1)
    let model = model.update(QuestionAction::Down).0; // -> Other (2)
    app.question = Some(model);
    assert_eq!(
        app.caret_owner(),
        CaretOwner::Modal,
        "Other highlighted → modal owns the caret for the IME",
    );
    assert!(
        app.caret_visible(),
        "the Other field must keep the cursor visible so the IME anchors to it",
    );

    // Navigate back to a real option → ownership reverts to None.
    let model = app.question.take().unwrap().update(QuestionAction::Up).0;
    app.question = Some(model);
    assert_eq!(
        app.caret_owner(),
        CaretOwner::None,
        "leaving Other must drop caret ownership again",
    );
}

#[test]
fn caret_hidden_while_selection_active_even_for_composer() {
    let (mut app, _tmp) = app_in_tempdir(&[], &[]);
    // Composer owns the caret, but an active selection hides the block cursor
    // so it does not clash with the selection background. Ownership is
    // unaffected; only visibility folds in the selection.
    assert_eq!(app.caret_owner(), CaretOwner::Composer);
    app.selection = SelectionState::Range {
        anchor: crate::tui::layout::SemanticCursor::new(0, 0, 0),
        head: crate::tui::layout::SemanticCursor::new(0, 0, 3),
    };
    assert_eq!(app.caret_owner(), CaretOwner::Composer);
    assert!(
        !app.caret_visible(),
        "an active selection hides the cursor regardless of ownership",
    );
}

#[test]
fn modal_owns_caret_matches_renderer_set_cursor_sites() {
    // Every modal that calls `set_cursor_position` in its renderer must be
    // declared in `Modal::owns_caret`, and vice versa — the two lists must
    // stay in lockstep so visibility and paint never disagree.
    //
    // The one deliberate exception is `Modal::Question`: its renderer places
    // the real cursor only while the "Other" free-text row is highlighted, and
    // ownership is decided *state-dependently* in `App::caret_owner` (which
    // consults `QuestionModel::is_other_highlighted`) rather than by the static
    // `owns_caret()`. It therefore appears in neither list here — it is tested
    // separately by `caret_owner_question_owns_caret_only_on_other`.
    let owns = [
        Modal::Provider,
        Modal::ModelEditor,
        Modal::AddModel,
        Modal::CustomProvider,
        Modal::HistorySearch,
    ];
    for m in owns {
        assert!(m.owns_caret(), "{m:?} must own the caret");
    }
    let not_owns = [
        Modal::None,
        Modal::Help,
        Modal::Sessions,
        Modal::Tools,
        Modal::Mcp,
        Modal::Permissions,
        Modal::Activity,
        Modal::Question,
        Modal::Permission,
        Modal::InputInjection,
        Modal::ProviderTemplate,
    ];
    for m in not_owns {
        assert!(!m.owns_caret(), "{m:?} must not own the caret");
    }
}
