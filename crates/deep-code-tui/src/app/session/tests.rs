use super::*;
use crate::session_scan::{SessionListing, SessionScan};
use deep_code_agent::CancellationToken;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn test_app() -> (tempfile::TempDir, App) {
    let workspace = tempfile::tempdir().unwrap();
    let mut app = App::launch(LaunchConfig {
        workspace: Some(workspace.path().to_path_buf()),
        ..Default::default()
    });
    app.lang = Lang::Zh;
    app.prompt_history_path = None;
    (workspace, app)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfiguration_keeps_live_history_after_a_failed_save() {
    let (workspace, mut app) = test_app();
    app.shutdown_current_runtime();
    app.adopt_runtime(deep_code_agent::launch_runtime(
        &deep_code_agent::AgentConfig::builtin(),
        workspace.path().to_path_buf(),
        None,
    ));
    app.global_config_path = Some(workspace.path().join("unused-global-config.toml"));
    let id = app.session_id.clone();
    // Escaping these bytes takes the encoded record past the storage limit.
    // The last durable record therefore has no user conversation.
    let mut events = app.runtime.submit_user("\u{1}".repeat(1_500_000)).await;
    while let Some(event) = events.recv().await {
        if matches!(event, RuntimeEvent::TurnFinished { .. }) {
            break;
        }
    }
    assert!(app.runtime.last_save_error().is_some());
    let messages = app.runtime.session_messages().await;
    assert!(
        messages
            .iter()
            .any(|message| message.role == deep_code_agent::Role::User)
    );
    let old_runtime = Arc::clone(&app.runtime);
    app.relaunch_runtime().unwrap();
    assert!(!Arc::ptr_eq(&old_runtime, &app.runtime));
    assert_eq!(app.session_id, id);
    assert_eq!(&app.runtime.session_messages().await[1..], &messages[1..]);
    assert!(app.runtime.last_save_error().is_some());
    app.shutdown_current_runtime();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconfiguration_keeps_unpersisted_conversation_and_can_add_a_root() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join(".deep-code"), "blocked").unwrap();
    let mut app = App::launch(LaunchConfig {
        workspace: Some(workspace.path().to_path_buf()),
        ..Default::default()
    });
    app.shutdown_current_runtime();
    app.adopt_runtime(deep_code_agent::launch_runtime(
        &deep_code_agent::AgentConfig::builtin(),
        workspace.path().to_path_buf(),
        None,
    ));
    app.global_config_path = Some(workspace.path().join("unused-global-config.toml"));
    let mut events = app
        .runtime
        .submit_user("keep this unsaved conversation")
        .await;
    while let Some(event) = events.recv().await {
        if matches!(event, RuntimeEvent::TurnFinished { .. }) {
            break;
        }
    }
    let before = app.runtime.session_messages().await;
    assert!(app.session_id.is_none());
    let extra = tempfile::tempdir().unwrap();
    app.add_dir_command(extra.path().to_str().unwrap());
    assert_eq!(
        app.extra_roots,
        vec![deep_code_agent::canonicalize(extra.path()).unwrap()]
    );
    assert!(app.session_id.is_none());
    let after = app.runtime.session_messages().await;
    assert_eq!(&before[1..], &after[1..]);
    app.shutdown_current_runtime();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reconfiguration_retains_the_live_runtime() {
    let (_workspace, mut app) = test_app();
    let old_runtime = Arc::clone(&app.runtime);
    let id = app.session_id.clone();
    // Protect against a turn starting before the UI has consumed TurnStarted.
    app.runtime.begin_turn("not yet visible in UI").await;
    assert!(app.relaunch_runtime().is_err());
    assert!(Arc::ptr_eq(&old_runtime, &app.runtime));
    assert_eq!(app.session_id, id);
    assert!(app.runtime.live_turn_id().await.is_some());
    app.shutdown_current_runtime();
}

#[test]
fn unavailable_store_keeps_the_existing_persisted_runtime() {
    let (_workspace, mut app) = test_app();
    let old_runtime = Arc::clone(&app.runtime);
    let id = app.session_id.clone();
    std::fs::rename(
        app.workspace.join(".deep-code"),
        app.workspace.join("previous-state"),
    )
    .unwrap();
    std::fs::write(app.workspace.join(".deep-code"), "unavailable").unwrap();
    assert!(app.relaunch_runtime().is_err());
    assert!(Arc::ptr_eq(&old_runtime, &app.runtime));
    assert_eq!(app.session_id, id);
}

fn finish_scan(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.poll_session_scan() {
        assert!(Instant::now() < deadline, "session scan did not complete");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn session_commands_return_before_results_are_applied() {
    let (_workspace, mut app) = test_app();
    let store = JsonSessionStore::for_pinned_workspace(&app.workspace).unwrap();
    let mut record = SessionRecord::new(app.workspace.clone(), "system");
    record
        .entries
        .push(Arc::new(deep_code_agent::SessionEntry::user(
            "find this session",
        )));
    store.save(&mut record).unwrap();

    let previous_history = app.history.len();
    assert!(app.handle_slash_command("/sessions"));
    assert!(app.session_scan.is_some());
    assert_eq!(app.history.len(), previous_history);
    finish_scan(&mut app);
    assert!(app.history.iter().any(|cell| matches!(cell,
        HistoryCell::System { text } if text.contains(record.id.as_str()))));

    assert!(app.handle_slash_command("/resume"));
    assert!(app.session_scan.is_some());
    assert!(app.resume_picker.is_none());
    finish_scan(&mut app);
    assert!(
        app.resume_picker
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .any(|session| session.id == record.id)
    );
}

/// Hold a real worker at a channel barrier: responsiveness and cancellation
/// assertions do not depend on a fast machine finishing a large fixture.
fn held_scan(app: &mut App) -> (CancellationToken, mpsc::Sender<()>, mpsc::Receiver<()>) {
    let (release, wait) = mpsc::channel();
    let (started, ready) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let task = SessionScan::spawn(move |cancel| {
        started.send(cancel).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        finished.send(()).unwrap();
        // Deliberately deliver a late result even after cancellation: the UI
        // must have retired the receiver, independently of the worker's check.
        Ok(SessionListing {
            records: Vec::new(),
            diagnostics: vec!["late result".to_string()],
        })
    })
    .unwrap();
    app.session_scan = Some(PendingSessionScan {
        purpose: SessionScanPurpose::Picker,
        task,
    });
    (
        ready.recv_timeout(Duration::from_secs(5)).unwrap(),
        release,
        done,
    )
}

#[test]
fn escape_and_ctrl_c_cancel_scans_without_clearing_the_draft_or_quitting() {
    for cancel_input in [App::handle_escape, App::handle_ctrl_c] {
        let (_workspace, mut app) = test_app();
        let (cancel, release, done) = held_scan(&mut app);
        assert!(!app.poll_session_scan());
        // The composer and runtime-event path remain usable during a scan.
        app.push_char('x');
        app.apply_runtime_event(RuntimeEvent::AssistantDelta {
            turn_id: deep_code_agent::TurnId::new(),
            text: "still streaming".to_string(),
        });
        cancel_input(&mut app);
        assert!(cancel.is_cancelled());
        assert!(app.session_scan.is_none());
        assert!(!app.should_quit);
        assert_eq!(app.input, "x");
        let history_len = app.history.len();
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!app.poll_session_scan());
        assert!(app.resume_picker.is_none());
        assert_eq!(app.history.len(), history_len);
    }
}

#[test]
fn a_new_scan_retires_the_old_receivers_results() {
    let (_workspace, mut app) = test_app();
    let (cancel, release, done) = held_scan(&mut app);
    app.list_sessions();
    assert!(cancel.is_cancelled());
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    finish_scan(&mut app);
    assert!(app.resume_picker.is_none());
    assert!(!app.history.iter().any(|cell| matches!(cell,
        HistoryCell::System { text } if text.contains("late result"))));
}

#[test]
fn clearing_the_session_cancels_its_pending_scan() {
    let (_workspace, mut app) = test_app();
    let (cancel, release, done) = held_scan(&mut app);
    app.start_new_conversation();
    assert!(cancel.is_cancelled());
    assert!(app.session_scan.is_none());
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!app.poll_session_scan());
}
