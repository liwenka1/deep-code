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
