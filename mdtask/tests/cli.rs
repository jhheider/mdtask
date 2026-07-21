//! CLI smoke tests for the thin binary. Invokes the built binary directly via
//! Cargo's `CARGO_BIN_EXE_mdtask`, so there is no test-harness dependency.

use std::process::Command;

fn mdtask() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mdtask"))
}

#[test]
fn version_flag_prints_the_crate_version() {
    for flag in ["--version", "-V"] {
        let out = mdtask().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            stdout.trim(),
            format!("mdtask {}", env!("CARGO_PKG_VERSION")),
            "{flag} output",
        );
    }
}

#[test]
fn help_flag_prints_usage() {
    for flag in ["--help", "-h"] {
        let out = mdtask().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag} should exit 0");
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(stdout.contains("Usage:"), "{flag} missing usage");
        assert!(stdout.contains("mdtask <name>"), "{flag} missing run line");
    }
}

#[test]
fn version_after_a_task_name_is_forwarded_not_intercepted() {
    // The whole point of the hand-rolled parser: everything after the task name
    // belongs to the task. `mdtask <task> --version` must NOT print mdtask's own
    // version banner. Run from an empty temp dir so no task file resolves; it
    // errors (to stderr), and crucially stdout carries no version banner.
    let out = mdtask()
        .args(["some-task-name", "--version"])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains(&format!("mdtask {}", env!("CARGO_PKG_VERSION"))),
        "--version after a task name was intercepted instead of forwarded",
    );
}
