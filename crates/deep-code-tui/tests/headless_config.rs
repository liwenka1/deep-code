use std::process::{Command, Stdio};

use serde_json::Value;

#[test]
fn unknown_model_reports_an_error_before_launch_in_every_output_format() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let model = "unrecognized-model-test-value";

    for format in ["json", "stream-json", "text"] {
        let output = Command::new(env!("CARGO_BIN_EXE_deep-code"))
            .args(["-p", "check configuration", "--output-format", format])
            .current_dir(workspace.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("DEEP_CODE_MODEL", model)
            .env_remove("DEEPSEEK_API_KEY")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{format}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("unknown model"), "{format}: {stderr}");
        assert!(!stderr.contains(model), "{format}: {stderr}");
        assert!(!workspace.path().join(".deep-code").exists(), "{format}");

        if format == "text" {
            assert!(output.stdout.is_empty());
            continue;
        }
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let report = if format == "stream-json" {
            assert_eq!(value["item"]["kind"], "print.result");
            assert_eq!(value["seq"], 1);
            &value["item"]["payload"]
        } else {
            &value
        };
        assert_eq!(report["status"], "error");
        assert_eq!(report["result"], "");
        assert!(report.get("session_id").is_none());
        let error = report["error"].as_str().unwrap();
        assert!(error.contains("unknown model"), "{format}: {error}");
        assert!(!error.contains(model), "{format}: {error}");
    }
}
