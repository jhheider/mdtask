//! `mdtask mcp`: a feature-gated MCP (Model Context Protocol) stdio server that
//! exposes ONLY the agent-allowed tasks (`Agent: allow`) to an MCP client.
//!
//! Security model: fail closed, and the gate lives in `mdtask-core`, not here. This
//! module is only the JSON-RPC surface; `mdtask_core::run_agent` is the enforcement
//! point (allowlist, within-file dependency resolution, injection guard), and
//! `mdtask_core::agent_jobs` is the listing that mirrors it.
//!
//! - A task is invisible and unrunnable unless its nearest definition carries
//!   `Agent: allow`. `tools/list` enumerates only `agent_jobs`, so the rest are not
//!   discoverable, and `run_task` calls `run_agent`, which re-checks the allowlist
//!   before running, so naming a hidden task fails too.
//! - An allowed task's `Requires:` chain is resolved within that task's own file
//!   (inside `run_agent`), not by the global nearest-wins scan the CLI uses. A
//!   nearer, untrusted task file in the invocation directory cannot shadow a
//!   dependency and run attacker-controlled code through an allowed entry point.
//! - A task that raw-templates an argument into its script via `{{ arg }}` is
//!   refused by `run_agent`, since the agent controls the value: an agent-run task
//!   must read the value from the environment instead.
//!
//! Output is captured, not streamed, so the client receives it as tool-result
//! text. The JSON-RPC loop is hand-rolled over serde_json (newline-delimited
//! messages, the MCP stdio framing), with no SDK.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;

use mdtask_core::{Cancel, TaskFile, agent_jobs};
use serde_json::{Value, json};

use crate::{summary, usage};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve the agent-allowed tasks over stdio until stdin closes.
///
/// # Why there are threads in here
///
/// This loop used to read a line, run the task to completion, then read the next
/// line. While a task ran the server read nothing at all, which breaks two
/// things the protocol requires of it:
///
/// - `ping` went unanswered, so a client could reasonably decide the server had
///   died and give up on a task that was running perfectly well.
/// - `notifications/cancelled` sat unread in the pipe until the task it was
///   cancelling had already finished, which is not cancellation.
///
/// So the reader gets its own thread and each task gets its own thread, and this
/// loop only routes between them. It is the only writer to stdout, so responses
/// cannot interleave.
pub fn run(files: &[(PathBuf, TaskFile)]) -> ExitCode {
    let (tx, rx) = mpsc::channel::<Event>();

    // Scoped, so the worker threads may borrow `files` instead of the whole task
    // set being cloned per call.
    std::thread::scope(|scope| {
        let reader = tx.clone();
        scope.spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if reader.send(Event::Line(line)).is_err() {
                    return; // the loop is gone
                }
            }
            let _ = reader.send(Event::Eof);
        });

        let mut stdout = std::io::stdout();
        // The cancel handle for every request still running, by request id.
        let mut inflight: HashMap<String, Cancel> = HashMap::new();

        while let Ok(event) = rx.recv() {
            let response = match event {
                Event::Eof => break,
                Event::Done { id, result } => {
                    // A cancelled request is no longer here, and the protocol
                    // says its response must not be sent. Dropping the result is
                    // the whole obligation.
                    inflight.remove(&key(&id)).map(|_| ok(Some(id), result))
                }
                Event::Line(line) => handle_line(&line, files, &tx, scope, &mut inflight),
            };
            if let Some(resp) = response {
                if writeln!(stdout, "{resp}").is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }

        // stdin closed or stdout broke: the client is gone, so nothing running
        // on its behalf should outlive it. Without this the scope would wait for
        // every task to finish on its own, which for a task that serves or
        // sleeps means never.
        for (_, cancel) in inflight.drain() {
            cancel.cancel();
        }
    });
    ExitCode::SUCCESS
}

/// What the loop waits on: a line from the client, or a task that has finished.
enum Event {
    Line(String),
    Done { id: Value, result: Value },
    Eof,
}

/// A request id as a map key. Ids may be numbers or strings, and the same id
/// must key the same way whichever it is.
fn key(id: &Value) -> String {
    id.to_string()
}

/// Route one JSON-RPC message. Returns a response to write, or `None` for a
/// notification, an unparseable line, or a request whose answer will arrive
/// later on a worker thread.
fn handle_line<'a>(
    line: &str,
    files: &'a [(PathBuf, TaskFile)],
    tx: &mpsc::Sender<Event>,
    scope: &'a std::thread::Scope<'a, '_>,
    inflight: &mut HashMap<String, Cancel>,
) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    let Ok(req) = serde_json::from_str::<Value>(line) else {
        return None; // ignore a malformed line rather than crash the server
    };
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    match method {
        "initialize" => Some(ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mdtask", "version": env!("CARGO_PKG_VERSION") },
            }),
        )),
        "tools/list" => Some(ok(id, json!({ "tools": tools_list() }))),
        // Answered while tasks run, which is the point of the whole arrangement:
        // a client's health check must not depend on how long a task takes.
        "ping" => Some(ok(id, json!({}))),
        "notifications/cancelled" => {
            let target = req
                .get("params")
                .and_then(|p| p.get("requestId"))
                .map(key)
                .and_then(|k| inflight.remove(&k));
            // Best effort by design: an id that already finished, or was never
            // ours, is not an error. A notification gets no reply either way.
            if let Some(cancel) = target {
                cancel.cancel();
            }
            None
        }
        "tools/call" => {
            let params = req.get("params");
            let tool = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // Only running a task can take real time. Listing is a read of
            // already-parsed state, and going through a thread for it would add
            // a round trip to hide nothing.
            if tool != "run_task" {
                return Some(ok(id, handle_call(files, params)));
            }
            let Some(id) = id else {
                return None; // a call with no id has nowhere to send a result
            };
            let arguments = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let cancel = Cancel::new();
            inflight.insert(key(&id), cancel.clone());

            let tx = tx.clone();
            scope.spawn(move || {
                let result = run_task(files, &arguments, &cancel);
                // The loop may already be gone; nothing to do about it here.
                let _ = tx.send(Event::Done { id, result });
            });
            None
        }
        // A notification (no id, e.g. notifications/initialized) gets no reply;
        // an unknown request does.
        _ => id.map(|id| {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}})
        }),
    }
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// The two tools. `list_tasks` reveals the allowed tasks; `run_task` runs one.
fn tools_list() -> Value {
    json!([
        {
            "name": "list_tasks",
            "description": "List the tasks this working set exposes to agents (only those marked `Agent: allow`), with their argument usage.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "run_task",
            "description": "Run an agent-allowed task by name and return its captured output. Positional `args` fill the task's declared Args in order.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The task name. Must be marked `Agent: allow`." },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Positional arguments, in order." }
                },
                "required": ["name"]
            }
        }
    ])
}

fn handle_call(files: &[(PathBuf, TaskFile)], params: Option<&Value>) -> Value {
    let tool = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match tool {
        "list_tasks" => text_result(list_tasks_text(files), false),
        // `run_task` is routed by the loop, not here: it needs a cancel handle
        // and a thread of its own. Reaching it through this path would mean
        // running it on the loop thread, which is the bug this all replaced.
        "run_task" => text_result(
            "run_task is dispatched by the server loop".to_string(),
            true,
        ),
        other => text_result(format!("unknown tool {other:?}"), true),
    }
}

/// A tool result: a single text block, flagged as an error or not.
fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn list_tasks_text(files: &[(PathBuf, TaskFile)]) -> String {
    let jobs = agent_jobs(files);
    if jobs.is_empty() {
        return "No tasks are exposed to agents. Mark a task with `Agent: allow` to expose it."
            .to_string();
    }
    let mut s = String::new();
    for job in jobs {
        let u = usage(job);
        let sep = if u.is_empty() { "" } else { " " };
        // The first paragraph, unwrapped, not the first physical line. These
        // descriptions are hard-wrapped markdown, so a line is a fragment: the
        // agent choosing between tools was reading things like "One license only
        // permits one". Same treatment as the CLI listing, deliberately.
        let desc = summary(&job.description);
        s.push_str(&job.name);
        s.push_str(sep);
        s.push_str(&u);
        if !desc.is_empty() {
            s.push('\t');
            s.push_str(&desc);
        }
        s.push('\n');
    }
    s
}

/// Run an agent-allowed task and return its captured output. The allowlist, the
/// within-file dependency resolution, and the injection guard are all enforced by
/// `mdtask_core::run_agent`; this only shapes the arguments and the tool result.
fn run_task(files: &[(PathBuf, TaskFile)], arguments: &Value, cancel: &Cancel) -> Value {
    let name = arguments.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return text_result("run_task requires a `name`".to_string(), true);
    }
    let positional: Vec<String> = arguments
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match mdtask_core::run_agent_cancellable(files, name, &positional, &cwd, cancel) {
        Ok(out) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            text_result(text, !out.status.success())
        }
        Err(e) => text_result(format!("{e}"), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdtask_core::parse;

    fn files(pairs: &[(&str, &str)]) -> Vec<(PathBuf, TaskFile)> {
        pairs
            .iter()
            .map(|(path, src)| (PathBuf::from(path), parse(src)))
            .collect()
    }

    #[test]
    fn only_agent_allowed_tasks_are_exposed() {
        let f = files(&[(
            "tasks.md",
            "## open\n\nAgent: allow\n\n```sh\ntrue\n```\n\n## closed\n\n```sh\ntrue\n```\n",
        )]);
        let names: Vec<_> = agent_jobs(&f).iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["open"]);
    }

    #[test]
    fn a_nearer_non_allowed_task_shadows_a_farther_allowed_one() {
        // The child redefines `deploy` WITHOUT the gate; the nearest definition
        // wins and it is not allowed, so `deploy` is not exposed (fail closed).
        let f = files(&[
            ("child/tasks.md", "## deploy\n\n```sh\ntrue\n```\n"),
            (
                "tasks.md",
                "## deploy\n\nAgent: allow\n\n```sh\ntrue\n```\n",
            ),
        ]);
        assert!(agent_jobs(&f).is_empty());
    }

    fn call_text(res: &Value) -> String {
        res["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn a_nearer_file_cannot_shadow_a_required_dependency_of_an_allowed_task() {
        // The regression: a nearer, untrusted `build` must NOT run when the allowed
        // ancestor `deploy` (which requires build) is invoked. The chain resolves
        // within deploy's own file, so the ancestor's real build runs, not PWNED.
        let f = files(&[
            ("child/tasks.md", "## build\n\n```sh\necho PWNED\n```\n"),
            (
                "tasks.md",
                "## deploy\n\nAgent: allow\nRequires: build\n\n```sh\necho real-deploy\n```\n\n## build\n\n```sh\necho real-build\n```\n",
            ),
        ]);
        let res = run_task(&f, &json!({ "name": "deploy" }), &Cancel::new());
        let text = call_text(&res);
        assert!(text.contains("real-build"), "got: {text}");
        assert!(text.contains("real-deploy"), "got: {text}");
        assert!(!text.contains("PWNED"), "nearer build was executed: {text}");
        assert_eq!(res["isError"], json!(false));
    }

    #[test]
    fn a_task_that_injects_an_arg_via_double_brace_is_refused() {
        // greet interpolates {{ name }} raw into its script; an agent-supplied
        // value would be shell-injectable, so run_task must refuse before running.
        let f = files(&[(
            "tasks.md",
            "## greet\n\nAgent: allow\nArgs: name\n\n```sh\necho hi {{ name }}\n```\n",
        )]);
        let res = run_task(
            &f,
            &json!({ "name": "greet", "args": ["x; echo PWNED"] }),
            &Cancel::new(),
        );
        let text = call_text(&res);
        assert_eq!(res["isError"], json!(true), "should be refused: {text}");
        assert!(text.contains("Refused"), "got: {text}");
        assert!(
            !text.contains("PWNED"),
            "the script must not have run: {text}"
        );
    }
}
