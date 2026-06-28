//! Session store tests — extracted to keep the production module focused.
//! Items are reached via `super::*` as before.

use super::*;
use neenee_core::async_trait;

/// Tests that touch process-global state (`paths::set_test_default` or
/// process env vars) cannot run in parallel. We serialise them through
/// this lock; pure-computation tests skip the guard.
static GLOBAL_GUARD: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

macro_rules! locked {
    ($body:block) => {{
        let _guard = GLOBAL_GUARD.lock().await;
        $body
    }};
}

#[tokio::test]
async fn session_data_round_trips() {
    let directory =
        std::env::temp_dir().join(format!("neenee-session-test-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    let messages = vec![Message::new(neenee_core::Role::User, "hello")];
    store.replace_messages(messages.clone()).await.unwrap();
    store
        .set_checkpoint(Some(PursuitCheckpoint {
            pursuit: "test".to_string(),
            iteration: 2,
            max_iterations: 8,
            status: "running".to_string(),
        }))
        .await
        .unwrap();

    let data: SessionData = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(data.messages[0].content, messages[0].content);
    assert_eq!(data.loop_checkpoint.unwrap().iteration, 2);
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn schema_version_defaults_to_current_and_serialises() {
    let data = SessionData::default();
    assert_eq!(data.schema_version, CURRENT_SCHEMA_VERSION);
    let raw = serde_json::to_string(&data).unwrap();
    assert!(raw.contains("\"schema_version\":"));
}

#[test]
fn legacy_session_without_schema_version_loads_as_current() {
    let data: SessionData = serde_json::from_str(
        r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "messages": [],
            "archived_messages": [],
            "loop_checkpoint": null,
            "compaction": null
        }"#,
    )
    .unwrap();
    // Missing fields inherit the serde default, which is the current
    // schema version, so legacy files appear up-to-date.
    assert_eq!(data.schema_version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn schema_migration_bumps_version() {
    let data = SessionData {
        schema_version: 0,
        ..SessionData::default()
    };
    let migrated = migrate_session_data(data);
    assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn checksum_computes_and_verifies() {
    let mut data = SessionData::default();
    data.messages = vec![Message::new(neenee_core::Role::User, "hello")];
    data.checksum = Some(compute_checksum(&data));
    assert!(verify_checksum(&data).is_ok());

    // Tamper with a field: verification must fail.
    data.messages[0].content = "goodbye".to_string();
    assert!(verify_checksum(&data).is_err());
}

#[test]
fn checksum_is_none_for_legacy_files() {
    let data: SessionData = serde_json::from_str(
        r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "messages": [],
            "archived_messages": [],
            "schema_version": 1
        }"#,
    )
    .unwrap();
    assert!(data.checksum.is_none());
    assert!(
        verify_checksum(&data).is_ok(),
        "missing checksum is allowed"
    );
}

#[tokio::test]
async fn session_persists_subagent_children_round_trip() {
    // End-to-end persistence contract: a session that contains a `task`
    // tool call must round-trip the subagent's nested transcript through
    // session.json, so a subsequent `SessionStore::load_for_project` (the
    // production resume path) restores the children intact. Before Phase 3
    // children were silently dropped because `Message::children` did not
    // exist and the harness only persisted the textual summary.
    let directory =
        std::env::temp_dir().join(format!("neenee-subagent-persist-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());

    let call = neenee_core::ToolCall {
        id: "call_sub1".to_string(),
        name: "subagent".to_string(),
        arguments: r#"{"description":"d","prompt":"p"}"#.to_string(),
    };
    let assistant = Message::new(neenee_core::Role::Assistant, "")
        .with_attribution("kimi-code", "kimi-k2.7-code");
    let assistant = Message {
        tool_calls: Some(vec![call.clone()]),
        ..assistant
    };
    let subagent_transcript = vec![
        Message::new(neenee_core::Role::User, "find foo"),
        Message::new(neenee_core::Role::Assistant, "looking..."),
        Message::new(neenee_core::Role::Assistant, "foo is at src/foo.rs"),
    ];
    let tool = Message::tool_result(&call, "[task result]:\nfoo is at src/foo.rs")
        .with_children(subagent_transcript);
    store
        .replace_messages(vec![
            Message::new(neenee_core::Role::User, "where is foo?"),
            assistant,
            tool,
        ])
        .await
        .unwrap();

    // Reload from disk as production code would.
    let loaded = fs::read_to_string(&path).unwrap();
    let data: SessionData = serde_json::from_str(&loaded).unwrap();
    let tool_msg = data
        .messages
        .iter()
        .find(|m| m.role == neenee_core::Role::Tool)
        .expect("tool result message persisted");
    let children = tool_msg.children.as_ref().expect("children persisted");
    assert_eq!(children.len(), 3);
    assert!(children.iter().any(|m| m.content == "find foo"));
    assert!(children.iter().any(|m| m.content.contains("src/foo.rs")));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn legacy_single_session_snapshot_migrates_with_defaults() {
    let data: SessionData = serde_json::from_str(
        r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "messages": [],
            "archived_messages": [],
            "loop_checkpoint": null,
            "compaction": null
        }"#,
    )
    .unwrap();

    assert_eq!(data.parent_id, None);
    assert!(data.created_at > 0);
    assert!(data.updated_at > 0);
    // Phase 2: project_root defaults to current cwd for legacy snapshots
    // missing the field.
    assert!(!data.project_root.as_os_str().is_empty());
}

#[tokio::test]
async fn load_for_project_isolates_sessions_per_cwd() {
    locked!({
        let root = std::env::temp_dir().join(format!("neenee-proj-iso-{}", uuid::Uuid::new_v4()));
        let dirs = paths::Dirs::resolve(&paths::PathsOverride {
            data_dir: Some(root.join("data")),
            state_dir: Some(root.join("state")),
            config_dir: Some(root.join("config")),
            cache_dir: Some(root.join("cache")),
        });
        dirs.ensure().unwrap();
        paths::set_test_default(Some(dirs.clone()));
        // Build two stores bound to different project roots.
        let store_a = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));
        let store_b = SessionStore::load_for_project(PathBuf::from("/projects/beta"));

        store_a
            .replace_messages(vec![Message::new(Role::User, "alpha work")])
            .await
            .unwrap();
        store_b
            .replace_messages(vec![Message::new(Role::User, "beta work")])
            .await
            .unwrap();

        let bucket_a = crate::paths::project_bucket_name(&PathBuf::from("/projects/alpha"));
        let bucket_b = crate::paths::project_bucket_name(&PathBuf::from("/projects/beta"));
        assert_ne!(bucket_a, bucket_b);

        // ADR-0018: each instance pins its own `sessions/<id>.json`. There
        // is no longer a project-root `session.json`.
        let id_a = store_a.id().await;
        let id_b = store_b.id().await;
        assert!(
            dirs.project_sessions_dir(&PathBuf::from("/projects/alpha"))
                .join(format!("{id_a}.json"))
                .exists()
        );
        assert!(
            dirs.project_sessions_dir(&PathBuf::from("/projects/beta"))
                .join(format!("{id_b}.json"))
                .exists()
        );

        // Reloading alpha starts fresh but the prior session is resumable,
        // and alpha never sees beta's messages.
        let reloaded_a = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));
        reloaded_a.resume(Some(&id_a)).await.unwrap();
        assert_eq!(reloaded_a.model_window().await[0].content, "alpha work");
        let reloaded_b = SessionStore::load_for_project(PathBuf::from("/projects/beta"));
        reloaded_b.resume(Some(&id_b)).await.unwrap();
        assert_eq!(reloaded_b.model_window().await[0].content, "beta work");

        // list() is scoped per project — alpha only sees its own session.
        let alpha_sessions = reloaded_a.list().await.unwrap();
        assert!(alpha_sessions.iter().all(|s| !s.overview.contains("beta")));
        let beta_sessions = reloaded_b.list().await.unwrap();
        assert!(beta_sessions.iter().all(|s| !s.overview.contains("alpha")));

        paths::set_test_default(None);
        let _ = std::fs::remove_dir_all(root);
    });
}

#[tokio::test]
async fn migrate_flat_sessions_buckets_by_project_root() {
    locked!({
        let root =
            std::env::temp_dir().join(format!("neenee-flat-migrate-{}", uuid::Uuid::new_v4()));
        let dirs = paths::Dirs::resolve(&paths::PathsOverride {
            data_dir: Some(root.join("data")),
            state_dir: Some(root.join("state")),
            config_dir: Some(root.join("config")),
            cache_dir: Some(root.join("cache")),
        });
        dirs.ensure().unwrap();
        paths::set_test_default(Some(dirs.clone()));

        let legacy_dir = dirs.legacy_sessions_dir();
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let alpha_active = SessionData {
            project_root: PathBuf::from("/projects/alpha"),
            ..SessionData::default()
        };
        let alpha_active_id = alpha_active.id.clone();
        fsutil::atomic_write_json(&dirs.data_dir.join("session.json"), &alpha_active).unwrap();
        let alpha_archive = SessionData {
            id: "aaaaaaaa-0000-0000-0000-000000000001".to_string(),
            project_root: PathBuf::from("/projects/alpha"),
            ..SessionData::default()
        };
        let beta_archive = SessionData {
            id: "bbbbbbbb-0000-0000-0000-000000000002".to_string(),
            project_root: PathBuf::from("/projects/beta"),
            ..SessionData::default()
        };
        fsutil::atomic_write_json(
            &legacy_dir.join(format!("{}.json", alpha_archive.id)),
            &alpha_archive,
        )
        .unwrap();
        fsutil::atomic_write_json(
            &legacy_dir.join(format!("{}.json", beta_archive.id)),
            &beta_archive,
        )
        .unwrap();

        let _ = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));

        let alpha_dir = dirs.project_dir(&PathBuf::from("/projects/alpha"));
        // ADR-0018: the legacy active session is migrated all the way into
        // `sessions/<id>.json`, not left at the project-root `session.json`.
        assert!(
            alpha_dir
                .join("sessions")
                .join(format!("{alpha_active_id}.json"))
                .exists()
        );
        assert!(!alpha_dir.join("session.json").exists());
        assert!(
            alpha_dir
                .join("sessions")
                .join(format!("{}.json", alpha_archive.id))
                .exists()
        );
        let beta_dir = dirs.project_dir(&PathBuf::from("/projects/beta"));
        assert!(!beta_dir.join("session.json").exists());
        assert!(
            beta_dir
                .join("sessions")
                .join(format!("{}.json", beta_archive.id))
                .exists()
        );
        assert!(
            !legacy_dir
                .join(format!("{}.json", alpha_archive.id))
                .exists()
        );
        assert!(
            !legacy_dir
                .join(format!("{}.json", beta_archive.id))
                .exists()
        );
        assert!(dirs.data_dir.join(".migrated-v3").exists());

        paths::set_test_default(None);
        let _ = std::fs::remove_dir_all(root);
    });
}

#[tokio::test]
async fn fork_preserves_both_durable_branches() {
    let directory =
        std::env::temp_dir().join(format!("neenee-session-fork-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "parent")])
        .await
        .unwrap();
    let parent_id = store.id().await;

    let (fork_id, source_id) = store.fork().await.unwrap();
    assert_eq!(source_id, parent_id);
    assert_eq!(store.parent_id().await.as_deref(), Some(parent_id.as_str()));
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "fork")])
        .await
        .unwrap();

    let sessions = store.list().await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().any(|item| item.id == parent_id));
    assert!(
        sessions
            .iter()
            .any(|item| item.id == fork_id && item.active)
    );

    store.open(&parent_id[..8]).await.unwrap();
    assert_eq!(store.model_window().await[0].content, "parent");
    store.open(&fork_id[..8]).await.unwrap();
    assert_eq!(store.model_window().await[0].content, "fork");
    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn fork_to_side_leaves_primary_active_pointer_intact() {
    // ADR-0017: a side fork must NOT repoint the primary's active pointer.
    // The primary keeps its id, history, and (by construction) any in-flight
    // turn; only a self-contained sibling file is written.
    let directory =
        std::env::temp_dir().join(format!("neenee-session-side-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "parent")])
        .await
        .unwrap();
    let parent_id = store.id().await;

    let (side_id, source_id) = store.fork_to_side().await.unwrap();
    assert_eq!(source_id, parent_id);
    assert_ne!(side_id, parent_id);

    // The primary is untouched: same id, still holds "parent", and has no
    // parent link (it did not become a child).
    assert_eq!(store.id().await, parent_id);
    assert_eq!(store.model_window().await[0].content, "parent");
    assert!(store.parent_id().await.is_none());

    // The side loads into its own store with the inherited history and the
    // parent lineage recorded.
    let side = store.open_side(&side_id).await.unwrap();
    assert_eq!(side.id().await, side_id);
    assert_eq!(side.parent_id().await.as_deref(), Some(parent_id.as_str()));
    assert_eq!(side.model_window().await[0].content, "parent");

    // Writing to the side never reaches the primary.
    side.replace_messages(vec![Message::new(neenee_core::Role::User, "side")])
        .await
        .unwrap();
    assert_eq!(store.model_window().await[0].content, "parent");

    // The side is independently resumable from disk (self-contained file).
    let reopened = store.open_side(&side_id).await.unwrap();
    assert_eq!(reopened.model_window().await[0].content, "side");

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn list_skips_active_session_when_it_has_no_content() {
    // Regression: a fresh empty active session used to be appended to
    // every `list()` call. Because `updated_at` is bumped on startup,
    // it sorted to the top and permanently showed "(empty session)".
    // The picker should only surface the active session once it has
    // real content (messages, archived messages, a loop checkpoint, or
    // a compaction marker).
    let directory = std::env::temp_dir().join(format!(
        "neenee-session-list-empty-{}",
        uuid::Uuid::new_v4()
    ));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());

    // Seed one archived session so the picker has something to show,
    // then keep the active session empty (the default state).
    let archived = SessionData {
        project_root: directory.clone(),
        messages: vec![Message::new(neenee_core::Role::User, "archived branch")],
        ..Default::default()
    };
    store.persist_archive(&archived).unwrap();

    let sessions = store.list().await.unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "empty active session must not appear in the list"
    );
    assert_eq!(sessions[0].id, archived.id);
    assert!(!sessions[0].active);

    // Once the active session gets content it should reappear, marked
    // active so the picker can badge it.
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "live branch")])
        .await
        .unwrap();
    let sessions = store.list().await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().any(|item| item.active));
    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn todos_round_trip_through_disk() {
    let directory =
        std::env::temp_dir().join(format!("neenee-todos-state-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    assert!(store.todos().await.is_empty());

    // Seed via reconcile and persist.
    let mut list = neenee_core::TodoList::new();
    list.reconcile(
        &[
            ("Summary".to_string(), neenee_core::TodoStatus::Pending),
            ("Key Changes".to_string(), neenee_core::TodoStatus::Pending),
            ("Test Plan".to_string(), neenee_core::TodoStatus::Pending),
        ],
        1000,
        3,
    );
    store.set_todos(list.clone()).await.unwrap();

    // Mutate (mark progress) and persist again — identity must survive.
    list.update("summary", neenee_core::TodoStatus::Completed, 2000, 4);
    store.set_todos(list.clone()).await.unwrap();

    // Reload from disk via the event log + snapshot and confirm round-trip.
    let reloaded = SessionStore::for_path(path.clone());
    let loaded = reloaded.todos().await;
    assert_eq!(loaded.len(), 3, "all items round-trip through disk");
    assert_eq!(loaded.items[0].content, "Summary");
    assert_eq!(loaded.items[0].status, neenee_core::TodoStatus::Completed);
    assert_eq!(loaded.updated_at_turn, 4);
    // Identity is stable: the first item's id is unchanged after the update.
    assert_eq!(loaded.items[0].id, list.items[0].id);

    // Clearing persists (empty list is the "no active list" state).
    reloaded
        .set_todos(neenee_core::TodoList::default())
        .await
        .unwrap();
    assert!(reloaded.todos().await.is_empty());

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn startup_new_session_can_resume_most_recent_cache() {
    let directory =
        std::env::temp_dir().join(format!("neenee-session-resume-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "previous")])
        .await
        .unwrap();
    let previous_id = store.id().await;

    let new_id = store.reset().await.unwrap();
    assert_ne!(new_id, previous_id);
    assert!(store.model_window().await.is_empty());

    let resumed_id = store.resume(None).await.unwrap();
    assert_eq!(resumed_id, previous_id);
    assert_eq!(store.model_window().await[0].content, "previous");
    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn event_log_is_authoritative_on_reload() {
    let directory =
        std::env::temp_dir().join(format!("neenee-events-reload-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "first")])
        .await
        .unwrap();
    let first_id = store.id().await;

    // Corrupt the snapshot cache: the event log must still restore state.
    let mut corrupted: SessionData =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    corrupted.messages[0].content = "tampered".to_string();
    let test_blobs = BlobStore::new(directory.join("blobs"));
    write_session_file(&path, &corrupted, &test_blobs).unwrap();

    // Re-open: for_path replays the event log, not the snapshot.
    let reloaded = SessionStore::for_path(path.clone());
    assert_eq!(reloaded.id().await, first_id);
    assert_eq!(reloaded.model_window().await[0].content, "first");

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn append_round_persists_delta_and_survives_reload() {
    // The mid-turn save point (ADR-0035): `append_round` writes only the
    // new tail as a `MessagesAppended` event, and a fresh `SessionStore`
    // at the same path must replay it to recover the full history. This
    // is the resume-after-crash contract — the whole point of the feature.
    let directory =
        std::env::temp_dir().join(format!("neenee-append-round-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());

    // Turn opens with one user message, durably written.
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "user prompt")])
        .await
        .unwrap();

    // Round 1 adds an assistant response + a tool result. The caller
    // passes the *full* current history; the store appends only the tail.
    let round1 = vec![
        Message::new(neenee_core::Role::User, "user prompt"),
        Message::new(neenee_core::Role::Assistant, "I will run a tool"),
        Message::new(neenee_core::Role::Tool, "tool output"),
    ];
    store.append_round(&round1).await.unwrap();

    // Round 2 adds more. The snapshot cache is still at the turn-open
    // state (one message); only the event log has grown.
    let round2 = vec![
        Message::new(neenee_core::Role::User, "user prompt"),
        Message::new(neenee_core::Role::Assistant, "I will run a tool"),
        Message::new(neenee_core::Role::Tool, "tool output"),
        Message::new(neenee_core::Role::Assistant, "done"),
    ];
    store.append_round(&round2).await.unwrap();

    // The live in-memory state reflects all appends.
    let live = store.model_window().await;
    assert_eq!(live.len(), 4);
    assert_eq!(live[3].content, "done");

    // A brand-new store replays the event log and recovers everything,
    // including the appended tail the snapshot never recorded.
    let reloaded = SessionStore::for_path(path.clone());
    let recovered = reloaded.model_window().await;
    assert_eq!(recovered.len(), 4, "appended rounds survive reload");
    assert_eq!(recovered[2].content, "tool output");
    assert_eq!(recovered[3].content, "done");

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn append_round_is_noop_when_nothing_new() {
    // Passing a history no longer than the durable baseline (e.g. right
    // after a compaction rewrote the window via `replace_messages`) must
    // not corrupt anything or write a spurious event.
    let directory =
        std::env::temp_dir().join(format!("neenee-append-noop-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    let messages = vec![Message::new(neenee_core::Role::User, "hi")];
    store.replace_messages(messages.clone()).await.unwrap();

    // Same length, same content → no-op.
    store.append_round(&messages).await.unwrap();
    assert_eq!(store.model_window().await.len(), 1);

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn append_round_falls_back_to_replace_on_divergent_prefix() {
    // If the incoming prefix disagrees with the durable state (a bug or a
    // compaction that bypassed `replace_messages`), `append_round` must
    // fall back to a full replace rather than splice a corrupt tail.
    let directory =
        std::env::temp_dir().join(format!("neenee-append-diverge-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, "original")])
        .await
        .unwrap();

    // Incoming history where the durable prefix was *rewritten* — the
    // first message content differs.
    let divergent = vec![
        Message::new(neenee_core::Role::User, "rewritten"),
        Message::new(neenee_core::Role::Assistant, "new"),
    ];
    store.append_round(&divergent).await.unwrap();

    // The fallback replaced everything with the incoming history.
    let live = store.model_window().await;
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].content, "rewritten");
    assert_eq!(live[1].content, "new");

    // And a reload recovers the replaced state, not a corrupt splice.
    let reloaded = SessionStore::for_path(path.clone());
    let recovered = reloaded.model_window().await;
    assert_eq!(recovered[0].content, "rewritten");

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn snapshot_without_event_log_gets_imported() {
    locked!({
        let root =
            std::env::temp_dir().join(format!("neenee-snapshot-import-{}", uuid::Uuid::new_v4()));
        let dirs = paths::Dirs::resolve(&paths::PathsOverride {
            data_dir: Some(root.join("data")),
            state_dir: Some(root.join("state")),
            config_dir: Some(root.join("config")),
            cache_dir: Some(root.join("cache")),
        });
        dirs.ensure().unwrap();
        paths::set_test_default(Some(dirs.clone()));

        let project_root = PathBuf::from("/projects/event-import-test");
        let project_dir = dirs.project_dir(&project_root);
        let path = project_dir.join("session.json");
        std::fs::create_dir_all(&project_dir).unwrap();

        let snapshot = SessionData {
            id: "00000000-0000-0000-0000-000000000001".to_string(),
            messages: vec![Message::new(neenee_core::Role::User, "from snapshot")],
            ..Default::default()
        };
        let blob_store = BlobStore::new(dirs.blobs_dir());
        write_session_file(&path, &snapshot, &blob_store).unwrap();

        let store = SessionStore::load_for_project(project_root);
        // ADR-0018: load_for_project pins a fresh session and migrates the
        // legacy project-root snapshot into sessions/<id>.json. The legacy
        // content is reached by resuming the migrated id, which also seeds
        // the per-session event log from the snapshot.
        let migrated_snapshot = project_dir
            .join("sessions")
            .join(format!("{}.json", snapshot.id));
        assert!(migrated_snapshot.exists(), "legacy snapshot migrated");
        assert!(!path.exists(), "legacy project-root session.json removed");

        store.resume(Some(&snapshot.id)).await.unwrap();
        assert_eq!(store.id().await, snapshot.id);
        assert_eq!(store.model_window().await[0].content, "from snapshot");
        assert!(
            project_dir
                .join("sessions")
                .join(format!("{}.jsonl", snapshot.id))
                .exists(),
            "event log should be seeded from snapshot on resume"
        );

        paths::set_test_default(None);
        let _ = std::fs::remove_dir_all(root);
    });
}

#[tokio::test]
async fn large_message_content_is_offloaded_to_blob_store() {
    let directory =
        std::env::temp_dir().join(format!("neenee-blob-session-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    let big = "x".repeat(8_192);
    store
        .replace_messages(vec![Message::new(neenee_core::Role::User, &big)])
        .await
        .unwrap();

    // Snapshot on disk should reference the blob.
    let raw = fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("content_blob"),
        "large content should be offloaded"
    );
    assert!(
        !raw.contains(&big),
        "raw content should not appear in snapshot"
    );

    // Replaying the event log rehydrates content from the blob store.
    let reloaded = SessionStore::for_path(path.clone());
    let messages = reloaded.model_window().await;
    assert_eq!(messages[0].content, big);
    assert!(
        messages[0].content_blob.is_none(),
        "memory uses inline content"
    );

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn injection_origin_survives_persist_and_reload() {
    // A harness-injected message's provenance must round-trip through the
    // snapshot cache AND the event-log replay path — the contract that lets
    // a resumed session faithfully reconstruct what was injected and why.
    // This is the end-to-end (store-layer) companion to the message-level
    // round-trip test in neenee-core.
    use neenee_core::{HookEventKind, InjectionKind, InjectionOrigin};
    let directory =
        std::env::temp_dir().join(format!("neenee-origin-session-{}", uuid::Uuid::new_v4()));
    let path = directory.join("session.json");
    let store = SessionStore::for_path(path.clone());
    let injected = Message::injected(
        neenee_core::Role::User,
        "setup context",
        InjectionOrigin::new(InjectionKind::Hook(HookEventKind::SessionStart))
            .with_reason("onstart.sh"),
    );
    store.replace_messages(vec![injected]).await.unwrap();

    // The snapshot file carries the origin object.
    let raw = fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"origin\""),
        "snapshot must persist origin: {raw}"
    );
    // HookEventKind serialises in PascalCase (no rename_all), so the wire
    // tag is "SessionStart". Pretty-printed with a space after the colon.
    assert!(
        raw.contains("\"hook\": \"SessionStart\""),
        "snapshot must persist the hook kind: {raw}"
    );

    // Reload via the event-log path (authoritative) rehydrates it intact.
    let reloaded = SessionStore::for_path(path.clone());
    let messages = reloaded.model_window().await;
    assert_eq!(messages.len(), 1);
    let origin = messages[0].origin.as_ref().expect("origin reloaded");
    assert_eq!(
        origin.kind,
        InjectionKind::Hook(HookEventKind::SessionStart)
    );
    assert_eq!(origin.reason.as_deref(), Some("onstart.sh"));

    let _ = fs::remove_dir_all(directory);
}

#[tokio::test]
async fn legacy_snapshot_without_origin_loads_as_none() {
    // A pre-C4 snapshot file (no `origin` key on any message) must load
    // with `origin: None` for every message — the store-layer side of the
    // backward-compat contract. Provenance is simply absent for old data.
    let directory =
        std::env::temp_dir().join(format!("neenee-legacy-origin-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("session.json");
    // Minimal pre-C4 snapshot: no origin key, schema_version 3.
    let legacy = serde_json::json!({
        "id": "legacy",
        "parent_id": null,
        "created_at": 1u64,
        "updated_at": 1u64,
        "messages": [
            {"role":"User","content":"old user input","hidden":false},
            {"role":"Assistant","content":"old reply","hidden":false}
        ],
        "archived_messages": [],
        "loop_checkpoint": null,
        "last_relief": null,
        "project_root": ".",
        "todos": [],
        "schema_version": 3,
        "checksum": null,
        "title": null,
        "title_manual": false,
        "pursuit": null
    });
    fs::write(&path, legacy.to_string()).unwrap();

    let store = SessionStore::for_path(path.clone());
    let messages = store.model_window().await;
    assert_eq!(messages.len(), 2);
    for (i, m) in messages.iter().enumerate() {
        assert!(
            m.origin.is_none(),
            "legacy message {i} must load with origin None, got {:?}",
            m.origin
        );
    }

    let _ = fs::remove_dir_all(directory);
}

#[test]
fn compaction_keeps_recent_complete_turns() {
    let messages = vec![
        Message::new(neenee_core::Role::System, "system"),
        Message::new(neenee_core::Role::User, "old question"),
        Message::new(neenee_core::Role::Assistant, "old answer"),
        Message::new(neenee_core::Role::Tool, "old tool result"),
        Message::new(neenee_core::Role::User, "recent question"),
        Message::new(neenee_core::Role::Assistant, "recent answer"),
    ];

    let result = compact_messages(&messages, 10_000, 1).unwrap();

    assert_eq!(result.checkpoint.operation, ContextProjectionKind::Compact);
    assert_eq!(result.model_window[0].role, neenee_core::Role::User);
    assert!(result.model_window[0].hidden);
    assert_eq!(result.model_window[1].content, "recent question");
    assert_eq!(result.model_window[2].content, "recent answer");
    assert!(
        result
            .archived_originals
            .iter()
            .any(|message| message.content == "old tool result")
    );
    assert!(
        !result
            .archived_originals
            .iter()
            .any(|message| message.role == neenee_core::Role::System)
    );
}

#[test]
fn compaction_requires_an_older_complete_turn() {
    let messages = vec![
        Message::new(neenee_core::Role::User, "question"),
        Message::new(neenee_core::Role::Assistant, "answer"),
    ];
    assert!(compact_messages(&messages, 10_000, 1).is_none());
}

#[test]
fn excerpt_summary_keeps_recent_first_under_budget() {
    // A large old tool result and a small recent user message. With a tiny
    // budget only the recent message (chosen newest-first) survives; the old
    // verbose tool result is omitted instead of crowding it out.
    let archived = vec![
        Message::new(Role::Tool, "X".repeat(3_000)),
        Message::new(Role::User, "recent critical detail"),
    ];

    let summary = build_excerpt_summary(&archived, 90, None);

    assert!(summary.contains("recent critical detail"));
    assert!(!summary.contains('X'));
}

#[test]
fn excerpt_summary_carries_forward_previous_summary() {
    let archived = vec![Message::new(Role::User, "what is 2+2")];
    let summary = build_excerpt_summary(&archived, 4_000, Some("prev anchored facts"));

    assert!(summary.starts_with("[Previous summary]\n"));
    assert!(summary.contains("prev anchored facts"));
    assert!(summary.contains("[Recent history]"));
    assert!(summary.contains("what is 2+2"));
}

#[test]
fn select_compaction_extracts_previous_summary() {
    let prior = checkpoint_message("prev summary body");
    let messages = vec![
        Message::new(Role::System, "system"),
        prior,
        Message::new(Role::User, "q1"),
        Message::new(Role::Assistant, "a1"),
        Message::new(Role::User, "q2"),
        Message::new(Role::Assistant, "a2"),
    ];

    let selection = select_compaction(&messages, 1).unwrap();
    assert_eq!(
        selection.previous_summary.as_deref(),
        Some("prev summary body")
    );
    // The prior checkpoint lands in the archived head, not the tail.
    assert!(
        selection
                .archived
            .iter()
            .any(|message| message.content.starts_with("[Conversation checkpoint]"))
    );
    assert_eq!(selection.tail.last().unwrap().content, "a2");
}

#[tokio::test]
async fn run_compaction_uses_provider_summary() {
    use neenee_providers::MockProvider;

    let mut history = vec![
        Message::new(Role::System, "system"),
        Message::new(Role::User, "old question"),
        Message::new(Role::Assistant, "old answer"),
        Message::new(Role::User, "recent question"),
        Message::new(Role::Assistant, "recent answer"),
    ];
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);

    let result = run_compaction(&mut history, 10_000, 1, Some(provider), Vec::new())
        .await
        .unwrap()
        .unwrap();

    // The mock provider's canned reply becomes the checkpoint summary.
    assert!(result.model_window[0].content.contains("mock AI"));
    assert_eq!(result.model_window[1].content, "recent question");
    assert!(result.model_window[0].hidden);
}

#[tokio::test]
async fn run_compaction_falls_back_when_provider_errors() {
    use neenee_providers::MockProvider;

    // MockProvider succeeds, so to exercise the fallback we instead pass a
    // provider that always errors and assert we still get an excerpt-based
    // checkpoint.
    struct FailingProvider;
    #[async_trait]
    impl Provider for FailingProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("boom".to_string())
        }
        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
            Err("boom".to_string())
        }
    }

    let mut history = vec![
        Message::new(Role::User, "old question"),
        Message::new(Role::Assistant, "old answer"),
        Message::new(Role::User, "recent question"),
        Message::new(Role::Assistant, "recent answer"),
    ];
    let provider: Arc<dyn Provider> = Arc::new(FailingProvider);

    let result = run_compaction(&mut history, 10_000, 1, Some(provider), Vec::new())
        .await
        .unwrap()
        .unwrap();

    // Fallback excerpt summary references the old question.
    assert!(result.model_window[0].content.contains("old question"));
    // Silence the unused MockProvider import warning while keeping the path
    // documented for the success-case test above.
    let _: &dyn Provider = &MockProvider;
}
