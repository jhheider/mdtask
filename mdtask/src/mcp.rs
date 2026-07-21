//! `mdtask mcp`: a feature-gated MCP (Model Context Protocol) stdio server that
//! exposes ONLY the agent-allowed tasks (`Agent: allow`) to an MCP client.
//!
//! Security model: fail closed. A task is invisible and unrunnable unless its
//! nearest definition carries `Agent: allow`. `tools/list` enumerates only the
//! allowed tasks, so the rest are not discoverable, and `run_task` re-checks the
//! allowlist before running, so naming a hidden task fails too. A `Requires:`
//! dependency of an allowed task still runs, since the author vouched for the
//! whole task when they allowed it, but a dependency is never listed and never
//! independently callable: the gate sits at the entry point the agent controls.
//! Output is captured, not streamed, so the client receives it as tool-result
//! text. The JSON-RPC loop is hand-rolled over serde_json (newline-delimited
//! messages, the MCP stdio framing), with no SDK.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mdtask_core::{Task, TaskFile};
use serde_json::{Value, json};

use crate::usage;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve the agent-allowed tasks over stdio until stdin closes.
pub fn run(files: &[(PathBuf, TaskFile)]) -> ExitCode {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue; // ignore a malformed line rather than crash the server
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => Some(ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mdtask", "version": env!("CARGO_PKG_VERSION") },
                }),
            )),
            "tools/list" => Some(ok(id, json!({ "tools": tools_list() }))),
            "tools/call" => Some(ok(id, handle_call(files, req.get("params")))),
            "ping" => Some(ok(id, json!({}))),
            // A notification (no id, e.g. notifications/initialized) gets no
            // reply; an unknown request does.
            _ => id.map(|id| {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}})
            }),
        };
        if let Some(resp) = response {
            if writeln!(stdout, "{resp}").is_err() {
                break;
            }
            let _ = stdout.flush();
        }
    }
    ExitCode::SUCCESS
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
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    match tool {
        "list_tasks" => text_result(list_tasks_text(files), false),
        "run_task" => run_task(files, &arguments),
        other => text_result(format!("unknown tool {other:?}"), true),
    }
}

/// A tool result: a single text block, flagged as an error or not.
fn text_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// The agent-allowed tasks, one per name using the nearest definition (so a
/// nearer non-allowed task shadows a farther allowed one, matching run
/// semantics), keeping only those whose nearest definition opts in.
fn allowed(files: &[(PathBuf, TaskFile)]) -> Vec<(&Path, &TaskFile, &Task)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (p, tf) in files {
        for t in &tf.tasks {
            if seen.insert(t.name.clone()) && t.agent_allow {
                out.push((p.as_path(), tf, t));
            }
        }
    }
    out
}

/// The nearest definition of `name` across the layered files (the one that runs).
fn nearest<'a>(
    files: &'a [(PathBuf, TaskFile)],
    name: &str,
) -> Option<(&'a Path, &'a TaskFile, &'a Task)> {
    files
        .iter()
        .find_map(|(p, tf)| tf.task(name).map(|t| (p.as_path(), tf, t)))
}

fn list_tasks_text(files: &[(PathBuf, TaskFile)]) -> String {
    let tasks = allowed(files);
    if tasks.is_empty() {
        return "No tasks are exposed to agents. Mark a task with `Agent: allow` to expose it."
            .to_string();
    }
    let mut s = String::new();
    for (_, _, t) in tasks {
        let u = usage(t);
        let sep = if u.is_empty() { "" } else { " " };
        let desc = t.description.lines().next().unwrap_or("");
        s.push_str(&t.name);
        s.push_str(sep);
        s.push_str(&u);
        if !desc.is_empty() {
            s.push('\t');
            s.push_str(desc);
        }
        s.push('\n');
    }
    s
}

fn run_task(files: &[(PathBuf, TaskFile)], arguments: &Value) -> Value {
    let name = arguments.get("name").and_then(Value::as_str).unwrap_or("");
    if name.is_empty() {
        return text_result("run_task requires a `name`".to_string(), true);
    }
    // Fail closed: only the allowlist is runnable, and naming a hidden task fails.
    if !allowed(files).iter().any(|(_, _, t)| t.name == name) {
        return text_result(
            format!("task {name:?} is not available to agents (it lacks `Agent: allow`)"),
            true,
        );
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

    // Resolve the Requires: chain (deps first, target last), then run each with
    // captured output. Dependencies run argless; only the target gets `args`.
    let order = match mdtask_core::dependency_order(name, |n| {
        nearest(files, n).map(|(_, _, t)| t.requires.clone())
    }) {
        Ok(order) => order,
        Err(e) => return text_result(format!("{e}"), true),
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut out = String::new();
    for step in &order {
        let (path, tf, task) = nearest(files, step).expect("a resolved name still exists");
        let step_args: &[String] = if step == name { &positional } else { &[] };
        let values = match TaskFile::bind(task, step_args) {
            Ok(v) => v,
            Err(e) => {
                out.push_str(&format!("{e}\n"));
                return text_result(out, true);
            }
        };
        let inv = match tf.invocation(task, &values, &cwd, path.parent()) {
            Ok(inv) => inv,
            Err(e) => {
                out.push_str(&format!("{e}\n"));
                return text_result(out, true);
            }
        };
        match std::process::Command::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&inv.cwd)
            .output()
        {
            Ok(o) => {
                out.push_str(&String::from_utf8_lossy(&o.stdout));
                out.push_str(&String::from_utf8_lossy(&o.stderr));
                if !o.status.success() {
                    out.push_str(&format!(
                        "\n[task {step:?} exited with {}]\n",
                        o.status.code().unwrap_or(-1)
                    ));
                    return text_result(out, true);
                }
            }
            Err(e) => {
                out.push_str(&format!("could not run {}: {e}\n", inv.program));
                return text_result(out, true);
            }
        }
    }
    text_result(out, false)
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
        let names: Vec<_> = allowed(&f)
            .iter()
            .map(|(_, _, t)| t.name.as_str())
            .collect();
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
        assert!(allowed(&f).is_empty());
    }
}
