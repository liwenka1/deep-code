use super::*;
use deep_code_agent::SessionStore;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn app_with_restore_point(project_config: Option<&str>) -> (tempfile::TempDir, App, SessionRecord) {
    use deep_code_agent::{CheckpointId, CheckpointRecord, SessionEntry, TurnRecord};

    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("original");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = deep_code_agent::canonicalize(&workspace).unwrap();
    let store = JsonSessionStore::for_pinned_workspace(&workspace).unwrap();
    if let Some(project_config) = project_config {
        std::fs::write(workspace.join(".deep-code/config.toml"), project_config).unwrap();
    }
    let mut record = SessionRecord::new(workspace.clone(), "system");
    for (question, answer) in [
        ("first question", "first answer"),
        ("later question", "later answer"),
    ] {
        let turn = deep_code_agent::TurnId::new();
        record.entries.push(Arc::new(
            SessionEntry::user(question).with_turn_id(turn.clone()),
        ));
        record.entries.push(Arc::new(
            SessionEntry::assistant(answer, None, Vec::new()).with_turn_id(turn.clone()),
        ));
        if record.turns.is_empty() {
            record.checkpoints.push(CheckpointRecord::new(
                CheckpointId("after_turn_1".into()),
                turn.clone(),
                "after_turn",
            ));
        }
        record.turns.push(TurnRecord::new(turn));
    }
    store.save(&mut record).unwrap();
    let snapshot = workspace.join(".deep-code/checkpoints/after_turn_1");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(snapshot.join("message.txt"), "first version").unwrap();
    std::fs::write(workspace.join("message.txt"), "latest version").unwrap();
    let mut app = App::launch(LaunchConfig {
        workspace: Some(workspace),
        resume: Some(record.clone()),
        ..Default::default()
    })
    .unwrap();
    app.lang = Lang::Zh;
    app.global_config_path = Some(parent.path().join("unused-global-config.toml"));
    app.prompt_history_path = None;
    (parent, app, record)
}

async fn finish_restore_request(app: &mut App) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.restore_scan.is_some() || app.session_switch.is_some() {
            app.poll_session_scan();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_lists_the_round_and_switches_to_an_independently_resumable_workspace() {
    let (_parent, mut app, source) = app_with_restore_point(None);
    assert!(app.handle_slash_command("/restore"));
    finish_restore_request(&mut app).await;
    let picker = app.restore_picker.as_ref().expect("restore picker");
    assert_eq!(picker.rounds[0].round, 2);
    assert!(picker.rounds[0].checkpoints.is_empty());
    assert_eq!(picker.rounds[1].prompt.as_deref(), Some("first question"));
    assert_eq!(picker.rounds[1].checkpoints[0].id.0, "after_turn_1");
    app.accept_restore_picker();
    assert!(app.session_switch.is_none(), "round 2 has no snapshot");
    app.preview_restore_round();
    finish_restore_request(&mut app).await;
    let reader = app
        .restore_reader
        .as_mut()
        .expect("view round without a snapshot");
    assert_eq!(reader.round, 2);
    assert!(matches!(&reader.entries[0].kind,
        deep_code_agent::EntryKind::User { content } if content.text() == "later question"));
    reader.move_entry(1);
    assert!(matches!(&reader.entries[reader.selected].kind,
        deep_code_agent::EntryKind::Assistant { content, .. } if content == "later answer"));
    app.close_restore_overlay();
    assert!(app.restore_picker.is_some());
    app.move_restore_picker(1);
    app.accept_restore_picker();
    assert_eq!(app.workspace, source.workspace);
    finish_restore_request(&mut app).await;
    assert_ne!(app.workspace, source.workspace, "{}", app.status);
    assert_ne!(app.session_id.as_deref(), Some(source.id.as_str()));
    assert!(app.workspace_files.iter().any(|path| path == "message.txt"));
    assert_eq!(
        std::fs::read_to_string(app.workspace.join("message.txt")).unwrap(),
        "first version"
    );
    assert_eq!(
        std::fs::read_to_string(source.workspace.join("message.txt")).unwrap(),
        "latest version"
    );
    assert!(
        app.history
            .iter()
            .any(|cell| matches!(cell, HistoryCell::User { text, .. }
        if text == "first question"))
    );
    assert!(
        !app.history
            .iter()
            .any(|cell| matches!(cell, HistoryCell::User { text, .. }
        if text == "later question"))
    );

    let fork_id = deep_code_agent::SessionId::parse(app.session_id.as_deref().unwrap()).unwrap();
    let fork_store = JsonSessionStore::for_pinned_workspace(&app.workspace).unwrap();
    let fork = fork_store.load(&fork_id).unwrap();
    let mut resumed = App::launch(LaunchConfig {
        workspace: Some(app.workspace.clone()),
        resume: Some(fork),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(resumed.session_id, app.session_id);
    assert!(
        resumed
            .history
            .iter()
            .any(|cell| matches!(cell, HistoryCell::User { text, .. }
        if text == "first question"))
    );
    assert!(resumed.handle_slash_command("/restore view 1"));
    finish_restore_request(&mut resumed).await;
    assert_eq!(resumed.restore_reader.as_ref().unwrap().entries.len(), 2);
    assert_eq!(
        JsonSessionStore::for_pinned_workspace(&source.workspace)
            .unwrap()
            .load(&source.id)
            .unwrap()
            .entries[1..],
        source.entries[1..]
    );
    resumed.stop_current_runtime().await;
    app.stop_current_runtime().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_combines_active_limits_with_tighter_settings_on_disk() {
    let (_parent, mut app, source) = app_with_restore_point(Some("[sandbox]\nnetwork = 'never'\n"));
    app.permission_mode.set(PermissionMode::AcceptEdits);
    std::fs::write(
        app.global_config(),
        "[approval]\ndefault_mode = 'accept_edits'\n",
    )
    .unwrap();
    std::fs::write(
        source.workspace.join(".deep-code/config.toml"),
        "[sandbox]\nnetwork = 'prompt'\n[lsp]\nenabled = false\n[approval]\ndefault_mode = 'default'\n",
    )
    .unwrap();

    assert!(app.handle_slash_command("/restore 1"));
    finish_restore_request(&mut app).await;
    assert_ne!(app.workspace, source.workspace, "{}", app.status);
    let config = AgentConfig::load_with(
        app.global_config_path.clone(),
        Some(app.workspace.join(".deep-code/config.toml")),
        &|_| None,
    )
    .config;
    assert_eq!(config.sandbox_network, deep_code_agent::NetworkMode::Never);
    assert!(!config.lsp_enabled);
    assert_eq!(config.default_permission_mode, PermissionMode::Default);
    assert_eq!(app.permission_mode(), PermissionMode::Default);
    assert_eq!(app.runtime.permission_mode(), PermissionMode::Default);
    app.stop_current_runtime().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_or_failed_restore_keeps_the_original_window_and_workspace() {
    let (parent, mut app, source) = app_with_restore_point(None);
    let original = Arc::clone(&app.runtime);
    assert!(app.handle_slash_command("/restore 999"));
    finish_restore_request(&mut app).await;
    assert!(app.status.contains("999"), "{}", app.status);

    assert!(app.handle_slash_command("/restore 1"));
    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;
    app.handle_escape();
    assert_eq!(app.status, app.tr(TextId::StatusRestoreCancelled));
    wait_for_ready(|| std::fs::read_dir(parent.path()).unwrap().count() == 1).await;
    assert!(Arc::ptr_eq(&original, &app.runtime));
    assert_eq!(app.session_id.as_deref(), Some(source.id.as_str()));
    assert_eq!(app.workspace, source.workspace);
    assert_eq!(
        std::fs::read_to_string(app.workspace.join("message.txt")).unwrap(),
        "latest version"
    );
    app.stop_current_runtime().await;
}

fn app_with_saved_target() -> (tempfile::TempDir, App, SessionRecord) {
    let workspace = tempfile::tempdir().unwrap();
    let mut app = App::launch(LaunchConfig {
        workspace: Some(workspace.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    app.lang = Lang::Zh;
    app.prompt_history_path = None;
    // Keep the live client offline even on a machine with user configuration.
    app.shutdown_current_runtime();
    app.adopt_runtime(
        deep_code_agent::launch_runtime(&AgentConfig::builtin(), app.workspace.clone(), None)
            .unwrap(),
        app.permission_mode(),
    );
    app.global_config_path = Some(workspace.path().join("unused-global-config.toml"));
    let store = JsonSessionStore::for_pinned_workspace(&app.workspace).unwrap();
    let mut target = SessionRecord::new(app.workspace.clone(), "saved target system prompt");
    target
        .entries
        .push(Arc::new(deep_code_agent::SessionEntry::user(
            "the target conversation",
        )));
    store.save(&mut target).unwrap();
    (workspace, app, target)
}

fn record_path(app: &App, id: &str) -> PathBuf {
    app.workspace
        .join(".deep-code/sessions")
        .join(format!("{id}.json"))
}

/// The watchdog releases the actual store lock if a blocking regression keeps
/// a UI call from returning, allowing a timing assertion to report failure.
struct WriterLock {
    release: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WriterLock {
    fn hold(app: &App) -> Self {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(app.workspace.join(".deep-code/sessions/.store.lock"))
            .unwrap();
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            file.lock().unwrap();
            started.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
            file.unlock().unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            release: Some(release),
            worker: Some(worker),
        }
    }

    fn release(mut self) {
        self.release.take().unwrap().send(()).unwrap();
        self.worker.take().unwrap().join().unwrap();
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        self.release = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn wait_for_ready(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("session worker did not deliver its result");
}

async fn finish_offline_turn(runtime: &Arc<AgentRuntime>) {
    let mut events = runtime
        .submit_user("continue after cancelling resume")
        .await;
    finish_turn_events(&mut events).await;
}

async fn finish_turn_events(events: &mut deep_code_agent::RuntimeEventReceiver) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if matches!(event, RuntimeEvent::TurnFinished { .. }) {
                return;
            }
        }
        panic!("offline turn ended without TurnFinished");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applying_a_loaded_resume_keeps_flush_wait_off_the_ui_and_cancellable() {
    for cancel_input in [App::handle_escape, App::handle_ctrl_c] {
        let (_workspace, mut app, target) = app_with_saved_target();
        let current = Arc::clone(&app.runtime);
        let current_id = app.session_id.clone();
        let target_path = record_path(&app, target.id.as_str());
        let target_bytes = std::fs::read(&target_path).unwrap();

        assert!(app.handle_slash_command(&format!("/resume {}", target.id.as_str())));
        wait_for_ready(|| app.session_scan.as_ref().unwrap().task.is_ready()).await;
        // The real load is complete. A writer now wins the exact race reported
        // by review, before the UI applies that result and flushes the old runtime.
        let writer = WriterLock::hold(&app);
        let started = Instant::now();
        assert!(app.poll_session_scan());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(app.session_scan.is_none());
        assert!(app.session_switch.is_some());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!app.session_switch.as_ref().unwrap().task.is_ready());

        app.push_char('x');
        let started = Instant::now();
        cancel_input(&mut app);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(app.session_switch.is_none());
        assert!(!app.should_quit);
        assert_eq!(app.input, "x");
        assert_eq!(app.session_id, current_id);
        assert!(Arc::ptr_eq(&app.runtime, &current));
        assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);

        let mut events = current
            .submit_user("continue while the store is still locked")
            .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(event) = events.recv().await {
                if matches!(event, RuntimeEvent::AssistantDelta { .. }) {
                    return;
                }
            }
            panic!("the retained runtime did not produce its offline reply");
        })
        .await
        .unwrap();
        writer.release();
        finish_turn_events(&mut events).await;
        app.poll_session_scan();
        assert_eq!(app.session_id, current_id);
        assert!(Arc::ptr_eq(&app.runtime, &current));
        app.shutdown_runtime().await;
        assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_ready_candidate_does_not_save_it_or_retire_the_live_runtime() {
    let (_workspace, mut app, target) = app_with_saved_target();
    let current = Arc::clone(&app.runtime);
    let current_id = app.session_id.clone();
    let target_path = record_path(&app, target.id.as_str());
    let target_bytes = std::fs::read(&target_path).unwrap();
    app.switch_session(target).unwrap();
    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;

    app.handle_escape();
    assert!(app.session_switch.is_none());
    assert!(!app.poll_session_scan());
    assert_eq!(app.session_id, current_id);
    assert!(Arc::ptr_eq(&app.runtime, &current));
    finish_offline_turn(&current).await;
    app.shutdown_runtime().await;
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_retires_a_ready_resume_candidate_without_saving_its_target() {
    let (_workspace, mut app, target) = app_with_saved_target();
    let current = Arc::clone(&app.runtime);
    let current_id = app.session_id.clone();
    let target_path = record_path(&app, target.id.as_str());
    let target_bytes = std::fs::read(&target_path).unwrap();
    app.switch_session(target).unwrap();
    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;

    app.start_compaction(None);
    assert!(app.session_switch.is_none());
    assert!(app.compacting());
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.compacting() {
            app.drain_stream_updates();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("compaction did not complete on the retained runtime");
    assert!(!app.poll_session_scan());
    assert!(app.session_cleanup.is_empty());
    assert_eq!(app.session_id, current_id);
    assert!(Arc::ptr_eq(&app.runtime, &current));
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
    finish_offline_turn(&current).await;
    app.shutdown_runtime().await;
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_turn_not_yet_visible_to_the_ui_prevents_resume_preparation() {
    let (_workspace, mut app, target) = app_with_saved_target();
    let current = Arc::clone(&app.runtime);
    let current_id = app.session_id.clone();
    let target_path = record_path(&app, target.id.as_str());
    let target_bytes = std::fs::read(&target_path).unwrap();
    let turn_id = current
        .begin_turn("not yet consumed by the UI")
        .await
        .unwrap();
    assert!(!app.runtime_busy());
    assert_eq!(current.live_turn_id().await, Some(turn_id.clone()));

    app.switch_session(target).unwrap();
    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;
    assert!(app.poll_session_scan());
    assert!(app.session_switch.is_none());
    assert!(app.session_cleanup.is_empty());
    assert_eq!(app.session_id, current_id);
    assert!(Arc::ptr_eq(&app.runtime, &current));
    assert_eq!(current.live_turn_id().await, Some(turn_id));
    assert!(app.status.contains("active turn"));
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);

    // The rejected switch must leave the already-started turn runnable.
    let mut events = current.drive_turn().await;
    finish_turn_events(&mut events).await;
    app.shutdown_runtime().await;
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_started_after_preparation_prevents_committing_a_ready_candidate() {
    let (_workspace, mut app, target) = app_with_saved_target();
    let current = Arc::clone(&app.runtime);
    let current_id = app.session_id.clone();
    let target_path = record_path(&app, target.id.as_str());
    let target_bytes = std::fs::read(&target_path).unwrap();
    app.switch_session(target).unwrap();
    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;
    let turn_id = current
        .begin_turn("started after resume preparation")
        .await
        .unwrap();
    assert!(!app.runtime_busy());

    assert!(app.poll_session_scan());
    assert!(app.session_switch.is_none());
    assert!(app.session_cleanup.is_empty());
    assert_eq!(app.status, app.busy_status(TextId::BusySwitchSession));
    assert_eq!(app.session_id, current_id);
    assert!(Arc::ptr_eq(&app.runtime, &current));
    assert_eq!(current.live_turn_id().await, Some(turn_id));
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);

    let mut events = current.drive_turn().await;
    finish_turn_events(&mut events).await;
    app.shutdown_runtime().await;
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committing_a_session_switch_clears_pending_yolo_confirmation() {
    use deep_code_agent::PermissionMode;

    let (_workspace, mut app, target) = app_with_saved_target();
    let target_id = target.id.clone();
    app.permission_mode.set(PermissionMode::AcceptEdits);
    app.switch_session(target).unwrap();
    app.cycle_permission_mode();
    let size = Size::new(80, 24);
    app.yolo_confirmation.set_visible(Some(size));
    app.yolo_confirmation.on_input_idle();
    app.yolo_confirmation.set_visible(Some(size));
    app.yolo_confirmation.on_input_idle();
    app.handle_yolo_key(YoloKey::F2);
    assert_eq!(
        app.yolo_confirmation,
        YoloConfirmation::Ready {
            size,
            step: YoloStep::Finish
        }
    );

    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;
    assert!(app.poll_session_scan());
    assert_eq!(app.session_id.as_deref(), Some(target_id.as_str()));
    assert_eq!(app.yolo_confirmation, YoloConfirmation::Inactive);
    app.handle_yolo_key(YoloKey::F7);
    assert_eq!(app.runtime.permission_mode(), PermissionMode::AcceptEdits);
    assert!(app.status.contains(target_id.as_str()), "{}", app.status);
    app.shutdown_runtime().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committing_a_ready_switch_never_waits_for_reacquired_store_lock_and_exit_drains_save() {
    let (_workspace, mut app, target) = app_with_saved_target();
    let current_path = record_path(&app, app.session_id.as_deref().unwrap());
    let target_path = record_path(&app, target.id.as_str());
    let target_bytes = std::fs::read(&target_path).unwrap();
    app.switch_session(target.clone()).unwrap();
    wait_for_ready(|| app.session_switch.as_ref().unwrap().task.is_ready()).await;
    let current_bytes = std::fs::read(&current_path).unwrap();

    // Preparation has also finished. Applying the candidate must be an
    // in-memory commit even if persistence becomes blocked again here.
    let writer = WriterLock::hold(&app);
    let started = Instant::now();
    assert!(app.poll_session_scan());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(app.session_id.as_deref(), Some(target.id.as_str()));
    assert!(app.session_switch.is_none());
    assert_eq!(app.session_cleanup.len(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(app.session_cleanup[0].try_result().is_none());
    assert_eq!(std::fs::read(&target_path).unwrap(), target_bytes);

    let shutdown = tokio::spawn(async move {
        app.shutdown_runtime().await;
        app
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!shutdown.is_finished(), "exit must drain the pending save");
    writer.release();
    let app = tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(app.session_cleanup[0].try_result().unwrap().is_ok());
    let store = JsonSessionStore::for_pinned_workspace(&app.workspace).unwrap();
    let restored = store.load(&target.id).unwrap();
    assert_eq!(restored.id, target.id);
    assert_eq!(restored.entries.len(), target.entries.len());
    assert_ne!(std::fs::read(&target_path).unwrap(), target_bytes);
    assert_eq!(
        std::fs::read(&current_path).unwrap(),
        current_bytes,
        "retiring an already-flushed runtime must not persist it again"
    );
}
