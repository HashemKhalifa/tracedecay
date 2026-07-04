use std::process::Command;

#[test]
fn resume_packet_command_renders_recovery_fields() {
    let output = Command::new(env!("CARGO_BIN_EXE_tracedecay"))
        .args([
            "resume-packet",
            "--workflow-id",
            "workflow-7",
            "--session-id",
            "session-9",
            "--branch",
            "codex/resume-packet",
            "--worktree",
            "/tmp/resume-packet",
            "--status",
            "blocked",
            "--failing-test",
            "cargo test resume_packet_command_renders_recovery_fields",
            "--next-command",
            "cargo test -p tracedecay resume_packet",
            "--evidence",
            "exit 101: unresolved subcommand",
            "--evidence",
            "last green: parser tests",
        ])
        .output()
        .unwrap_or_else(|e| panic!("run resume-packet command: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "resume-packet should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("# Resume Packet"));
    assert!(stdout.contains("Workflow: workflow-7"));
    assert!(stdout.contains("Session: session-9"));
    assert!(stdout.contains("Branch: codex/resume-packet"));
    assert!(stdout.contains("Worktree: /tmp/resume-packet"));
    assert!(stdout.contains("Status: blocked"));
    assert!(stdout.contains("- cargo test resume_packet_command_renders_recovery_fields"));
    assert!(stdout.contains("Next command: cargo test -p tracedecay resume_packet"));
    assert!(stdout.contains("- exit 101: unresolved subcommand"));
    assert!(stdout.contains("- last green: parser tests"));
}
