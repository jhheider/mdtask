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

/// The MCP server must stay responsive while a task runs, and cancellation must
/// actually stop the work.
///
/// This drives the real binary over stdio rather than calling functions,
/// because what broke before was the *arrangement* of the loop, not any one
/// function in it: a correct `run_agent` called from a loop that reads nothing
/// while it waits still cannot answer a ping or honour a cancellation.
///
/// Three claims, all of which failed before:
///
/// - `ping` is answered while a task is running.
/// - A second `run_task` runs alongside the first instead of queueing behind it.
/// - A cancelled request gets **no response at all**, per the protocol, and its
///   process tree is gone rather than orphaned.
#[cfg(all(unix, feature = "mcp"))]
#[test]
fn the_mcp_server_stays_responsive_and_cancellation_reaches_the_work() {
    use std::io::{BufRead, BufReader, Write};

    let dir = std::env::temp_dir().join(format!("mdtask-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The witness is touched only if the task's grandchild outlives the cancel.
    let witness = dir.join("survived");
    std::fs::write(
        dir.join("tasks.md"),
        format!(
            "## slow\n\nAgent: allow\n\n```sh\n(sleep 6; touch {}) &\nwait\n```\n\n\
             ## quick\n\nAgent: allow\n\n```sh\necho quick-done\n```\n",
            witness.display()
        ),
    )
    .unwrap();

    let mut child = mdtask()
        .arg("--mcp")
        .current_dir(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    let mut send = |line: &str| {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };
    let mut recv = move || {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        line
    };

    // Start the slow task, then immediately ask for things that must not wait.
    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run_task","arguments":{"name":"slow"}}}"#,
    );
    send(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
    send(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"run_task","arguments":{"name":"quick"}}}"#,
    );

    let first = recv();
    let second = recv();
    assert!(
        first.contains("\"id\":3"),
        "ping did not come back first: {first}"
    );
    assert!(
        second.contains("\"id\":4") && second.contains("quick-done"),
        "the second task did not run alongside the first: {second}"
    );

    send(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"test"}}"#,
    );
    drop(stdin);

    let status = child.wait().unwrap();
    assert!(status.success(), "the server did not exit cleanly");

    // Past when the grandchild would have fired had it survived.
    std::thread::sleep(std::time::Duration::from_secs(7));
    let leaked = witness.exists();
    std::fs::remove_dir_all(&dir).ok();
    assert!(!leaked, "the cancelled task's grandchild kept running");
}
