use super::*;
use deep_code_agent::{EntryKind, SessionEntry};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn args() -> PrintArgs {
    PrintArgs {
        prompt: None,
        intent: StartupIntent::New,
        output: OutputFormat::Json,
        permission_mode: None,
        timeout_secs: None,
        verbose: false,
        add_dirs: Vec::new(),
    }
}

#[tokio::test]
async fn storage_pressure_compacts_before_headless_loses_new_messages() {
    for size_pressure in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
        let entry_count = if size_pressure { 8 } else { 4094 };
        for index in 0..entry_count {
            record.entries.push(Arc::new(if index % 2 == 0 {
                // JSON escaping, rather than model tokens, consumes the byte
                // budget. Put the large entry in the archivable history.
                SessionEntry::user(if size_pressure && index == 0 {
                    "\u{1}".repeat(1_100_000)
                } else {
                    "hi".into()
                })
            } else {
                SessionEntry::assistant("ok", None, Vec::new())
            }));
        }
        store.save(&mut record).unwrap();
        let id = record.id.clone();
        let config = AgentConfig {
            compaction_threshold: Some(u32::MAX),
            ..AgentConfig::builtin()
        };
        let launched = launch_runtime(&config, workspace.path().to_path_buf(), Some(record));
        assert!(
            launched.offline,
            "the regression must never contact a provider"
        );
        let handle = launched.handle.clone();
        let token = CancellationToken::new();
        let out = Capture::default();
        let err = Capture::default();
        let output = Output::new(out.clone(), &token).unwrap();
        let diagnostics = Diagnostics::new(err.clone()).unwrap();
        let code = run_turn(
            args(),
            "final question".into(),
            launched,
            &diagnostics,
            &output,
            &token,
        )
        .await;
        let report: serde_json::Value = serde_json::from_slice(&out.0.lock().unwrap()).unwrap();
        assert_eq!(code, EXIT_OK, "{report}");
        assert_eq!(report["status"], "finished");
        let saved = store.load(&id).unwrap();
        assert!(
            saved
                .entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        );
        assert_eq!(
            deep_code_agent::Session::from_entries(saved.entries).wire_messages(),
            handle.session_messages().await,
            "the successful result must describe a fully saved transcript"
        );
        assert_eq!(handle.last_save_error(), None);
        assert!(
            !String::from_utf8_lossy(&err.0.lock().unwrap()).contains("session could not be saved")
        );
    }
}

#[tokio::test]
async fn unsavable_current_turn_returns_an_error_instead_of_success() {
    let workspace = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let id = record.id.clone();
    let launched = launch_runtime(
        &AgentConfig::builtin(),
        workspace.path().to_path_buf(),
        Some(record),
    );
    assert!(launched.offline);
    let handle = launched.handle.clone();
    let token = CancellationToken::new();
    let out = Capture::default();
    let err = Capture::default();
    let output = Output::new(out.clone(), &token).unwrap();
    let diagnostics = Diagnostics::new(err.clone()).unwrap();
    // The current user message cannot be archived. Escaping makes this exceed
    // the disk limit while remaining below the model's context threshold.
    let code = run_turn(
        args(),
        "\u{1}".repeat(1_500_000),
        launched,
        &diagnostics,
        &output,
        &token,
    )
    .await;
    let report: serde_json::Value = serde_json::from_slice(&out.0.lock().unwrap()).unwrap();
    assert_eq!(code, EXIT_FAILURE);
    assert_eq!(report["status"], "error");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("session could not be saved")
    );
    assert!(handle.last_save_error().is_some());
    assert_eq!(
        store.load(&id).unwrap().entries.len(),
        1,
        "keep the last valid snapshot"
    );
    let stderr = String::from_utf8(err.0.lock().unwrap().clone()).unwrap();
    assert!(stderr.contains("session could not be saved"));
    assert!(!stderr.contains("continue interactively"));
}
