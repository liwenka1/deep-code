use super::*;
use deep_code_agent::SessionStore;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn app_with_saved_target() -> (tempfile::TempDir, App, SessionRecord) {
    let workspace = tempfile::tempdir().unwrap();
    let mut app = App::launch(LaunchConfig {
        workspace: Some(workspace.path().to_path_buf()),
        ..Default::default()
    });
    app.lang = Lang::Zh;
    app.prompt_history_path = None;
    // Keep the live client offline even on a machine with user configuration.
    app.shutdown_current_runtime();
    app.adopt_runtime(deep_code_agent::launch_runtime(
        &AgentConfig::builtin(),
        app.workspace.clone(),
        None,
    ));
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
    let turn_id = current.begin_turn("not yet consumed by the UI").await;
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
    let turn_id = current.begin_turn("started after resume preparation").await;
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
