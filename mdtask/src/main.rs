//! `mdtask` - run tasks defined in markdown. A thin CLI over `mdtask-core`.
//!
//! ```text
//! mdtask                 list tasks (walks up for tasks.md / maskfile.md / README.md)
//! mdtask <name> [args...]  run a task; positional args fill its `Args:` in order
//! mdtask -f FILE ...       use a specific task file (no directory walk)
//! ```
//!
//! Task files are layered like just's `set fallback`: files nearer the current
//! directory shadow those farther up, so a project inherits a baseline of tasks
//! from its parents and overrides them where it wants.
//!
//! The library is the point; this is deliberately small (no arg-parser
//! dependency).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mdtask_core::{Task, TaskFile};

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // -f/--file FILE selects one task file (no walk); else walk up from cwd.
    let mut file: Option<PathBuf> = None;
    if let Some(i) = args.iter().position(|a| a == "-f" || a == "--file") {
        if i + 1 >= args.len() {
            eprintln!("mdtask: {} needs a path", args[i]);
            return ExitCode::FAILURE;
        }
        file = Some(PathBuf::from(args.remove(i + 1)));
        args.remove(i);
    }

    // Discover the layered task files, nearest first.
    let files: Vec<(PathBuf, TaskFile)> = match file {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(src) => vec![(p, mdtask_core::parse(&src))],
            Err(e) => {
                eprintln!("mdtask: {}: {e}", p.display());
                return ExitCode::FAILURE;
            }
        },
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            mdtask_core::find_task_files(&cwd)
        }
    };
    if files.is_empty() {
        eprintln!("mdtask: no tasks.md, maskfile.md, or README.md with tasks (use -f FILE)");
        return ExitCode::FAILURE;
    }
    // Parse warnings are worth seeing (an unterminated fence mis-runs silently).
    for (path, tf) in &files {
        for w in &tf.warnings {
            eprintln!("mdtask: {}: {w}", path.display());
        }
    }

    let Some(name) = args.first().cloned() else {
        return list(&files);
    };

    // Nearest file that defines the task wins (the fallback layering).
    let Some((path, tf, task)) = files
        .iter()
        .find_map(|(p, tf)| tf.task(&name).map(|t| (p, tf, t)))
    else {
        eprintln!("mdtask: no task named {name:?}");
        return ExitCode::FAILURE;
    };
    // The task file's own directory anchors a relative `Dir:`; the current
    // directory is where a task runs by default (no `Dir:`).
    let task_file_dir = path.parent().unwrap_or(Path::new("."));
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Positional CLI args bind to the task's declared Args (defaults + variadic).
    let positional: Vec<String> = args.iter().skip(1).cloned().collect();
    let values = match TaskFile::bind(task, &positional) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mdtask: {e} (usage: mdtask {name} {})", usage(task));
            return ExitCode::FAILURE;
        }
    };

    let inv = match tf.invocation(task, &values, &cwd, Some(task_file_dir)) {
        Ok(inv) => inv,
        Err(e) => {
            eprintln!("mdtask: {e} (usage: mdtask {name} {})", usage(task));
            return ExitCode::FAILURE;
        }
    };
    // Inherit stdio so the task's output streams straight through.
    match std::process::Command::new(&inv.program)
        .args(&inv.args)
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&inv.cwd)
        .status()
    {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1) as u8),
        Err(e) => {
            eprintln!("mdtask: could not run {}: {e}", inv.program);
            ExitCode::FAILURE
        }
    }
}

/// List tasks across the layered files, nearest first, each name once (a nearer
/// definition shadows a farther one).
fn list(files: &[(PathBuf, TaskFile)]) -> ExitCode {
    let mut seen = std::collections::BTreeSet::new();
    let mut any = false;
    for (_, tf) in files {
        for t in &tf.tasks {
            if seen.insert(t.name.clone()) {
                let args = usage(t);
                let sep = if args.is_empty() { "" } else { " " };
                let desc = t.description.lines().next().unwrap_or("");
                println!("{}{sep}{args}\t{desc}", t.name);
                any = true;
            }
        }
    }
    if any {
        ExitCode::SUCCESS
    } else {
        eprintln!("mdtask: no tasks found");
        ExitCode::FAILURE
    }
}

/// `<arg1> [arg2] [rest...]` for a task's declared args, for usage/list output:
/// required args in angle brackets, defaulted/variadic in square brackets.
fn usage(t: &Task) -> String {
    t.args
        .iter()
        .map(|a| {
            if a.variadic {
                format!("[{}...]", a.name)
            } else if a.default.is_some() {
                format!("[{}]", a.name)
            } else {
                format!("<{}>", a.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
