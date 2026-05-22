//! Smoke example for execution policy, sandbox detection, and checkpoints.
//!
//! Run from the workspace root:
//! `cargo run -p deep-code-agent --example policy_checkpoint_smoke`

use deep_code_agent::{
    CheckpointStore, ExecPolicy, PolicyVerdict, detect_capabilities, evaluate_shell_command,
};
use std::fs;
fn main() -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    println!("workspace: {}", workspace.display());

    let caps = detect_capabilities();
    println!(
        "sandbox: {} ({}) — {}",
        caps.backend.id(),
        if caps.available { "available" } else { "unavailable" },
        caps.detail
    );

    let policy = ExecPolicy::default();
    for command in ["git status", "rm -rf /tmp", "python -V"] {
        let plan = evaluate_shell_command(&policy, command);
        println!("shell '{command}' => {:?} (approval={})", plan.verdict, plan.requires_approval);
    }

    let write_plan = policy.evaluate_tool("write_file", &serde_json::json!({"path": "x", "content": "y"}));
    println!("write_file => {:?}", write_plan.verdict);

    let store = CheckpointStore::new(&workspace)?;
    let probe = workspace.join(".deep-code-checkpoint-probe.txt");
    fs::write(&probe, "v1")?;
    let id = store.snapshot("smoke_before")?;
    fs::write(&probe, "v2")?;
    store.restore(&id)?;
    let restored = fs::read_to_string(&probe)?;
    println!("checkpoint round-trip: {restored}");
    fs::remove_file(&probe)?;

    if matches!(evaluate_shell_command(&policy, "rm -rf /").verdict, PolicyVerdict::Deny { .. }) {
        println!("policy deny path: ok");
    }

    Ok(())
}
