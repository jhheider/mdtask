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

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use mdtask_core::{TaskFile, agent_jobs};
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
        let desc = job.description.lines().next().unwrap_or("");
        s.push_str(&job.name);
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

/// Run an agent-allowed task and return its captured output. The allowlist, the
/// within-file dependency resolution, and the injection guard are all enforced by
/// `mdtask_core::run_agent`; this only shapes the arguments and the tool result.
fn run_task(files: &[(PathBuf, TaskFile)], arguments: &Value) -> Value {
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
    match mdtask_core::run_agent(files, name, &positional, &cwd) {
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
        let res = run_task(&f, &json!({ "name": "deploy" }));
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
        let res = run_task(&f, &json!({ "name": "greet", "args": ["x; echo PWNED"] }));
        let text = call_text(&res);
        assert_eq!(res["isError"], json!(true), "should be refused: {text}");
        assert!(text.contains("Refused"), "got: {text}");
        assert!(
            !text.contains("PWNED"),
            "the script must not have run: {text}"
        );
    }
}
