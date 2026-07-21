//! `mdtask` — run tasks defined in markdown. A thin CLI over `mdtask-core`.
//!
//! ```text
//! mdtask                 list tasks in ./tasks.md (or maskfile.md / README.md)
//! mdtask <name> [args…]  run a task; positional args fill its `Args:` in order
//! mdtask -f FILE …       use a specific task file
//! ```
//!
//! The library is the point; this is deliberately small (no arg-parser
//! dependency). Execution and MCP live here, not in the core.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // -f/--file FILE selects the task file; otherwise search the default names.
    let mut file: Option<PathBuf> = None;
    if let Some(i) = args.iter().position(|a| a == "-f" || a == "--file") {
        if i + 1 >= args.len() {
            eprintln!("mdtask: {} needs a path", args[i]);
            return ExitCode::FAILURE;
        }
        file = Some(PathBuf::from(args.remove(i + 1)));
        args.remove(i);
    }

    let (path, src) = match load(file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mdtask: {e}");
            return ExitCode::FAILURE;
        }
    };
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let tf = mdtask_core::parse(&src);

    let Some(name) = args.first().cloned() else {
        return list(&tf);
    };
    let Some(task) = tf.task(&name) else {
        eprintln!("mdtask: no task named {name:?} in {}", path.display());
        return ExitCode::FAILURE;
    };

    // Positional CLI args fill the task's declared Args in order.
    let values: BTreeMap<String, String> = task
        .args
        .iter()
        .cloned()
        .zip(args.iter().skip(1).cloned())
        .collect();

    let inv = match tf.invocation(task, &values, &root) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("mdtask: {e} (usage: mdtask {name} {})", usage(task));
            return ExitCode::FAILURE;
        }
    };
    // Inherit stdio so the task's output streams straight through.
    let status = std::process::Command::new(&inv.program)
        .args(&inv.args)
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&inv.cwd)
        .status();
    match status {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!("mdtask: could not run {}: {e}", inv.program);
            ExitCode::FAILURE
        }
    }
}

/// Load the chosen file, or search `tasks.md`, `maskfile.md`, `README.md` in cwd.
fn load(file: Option<PathBuf>) -> std::io::Result<(PathBuf, String)> {
    if let Some(p) = file {
        let src = std::fs::read_to_string(&p)?;
        return Ok((p, src));
    }
    for name in ["tasks.md", "maskfile.md", "README.md"] {
        let p = PathBuf::from(name);
        if let Ok(src) = std::fs::read_to_string(&p) {
            return Ok((p, src));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no tasks.md, maskfile.md, or README.md here (use -f FILE)",
    ))
}

/// Print the task list (name, args, first line of description).
fn list(tf: &mdtask_core::TaskFile) -> ExitCode {
    if tf.tasks.is_empty() {
        eprintln!("mdtask: no tasks found");
        return ExitCode::FAILURE;
    }
    for t in &tf.tasks {
        let args = usage(t);
        let sep = if args.is_empty() { "" } else { " " };
        let desc = t.description.lines().next().unwrap_or("");
        println!("{}{sep}{args}\t{desc}", t.name);
    }
    ExitCode::SUCCESS
}

/// `<arg1> <arg2>` for a task's declared args, for usage/list output.
fn usage(t: &mdtask_core::Task) -> String {
    t.args
        .iter()
        .map(|a| format!("<{a}>"))
        .collect::<Vec<_>>()
        .join(" ")
}
