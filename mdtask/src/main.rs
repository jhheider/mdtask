//! `mdtask` runs tasks defined in markdown. It is a thin CLI over `mdtask-core`.
//!
//! ```text
//! mdtask                 list tasks (walks up for tasks.md / maskfile.md / README.md)
//! mdtask <name> [args...]  run a task; positional args fill its `Args:` in order
//! mdtask lint [TASK]       shellcheck the shell scripts (all tasks, or one)
//! mdtask mcp               serve agent-allowed tasks to an MCP client (--features mcp)
//! mdtask -f FILE ...       use a specific task file (no directory walk)
//! ```
//!
//! A task with `Requires:` runs its dependencies first (resolved across the
//! layered files, deps before dependents, aborting on the first failure).
//!
//! Task files are layered like just's `set fallback`: files nearer the current
//! directory shadow those farther up, so a project inherits a baseline of tasks
//! from its parents and overrides them where it wants.
//!
//! The library is the point, so this is deliberately small, with no arg-parser
//! dependency.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mdtask_core::{Task, TaskFile};

mod lint;
#[cfg(feature = "mcp")]
mod mcp;

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

    // Reserved subcommands, checked before task dispatch. `lint` takes an optional
    // task name; `mcp` serves the agent-allowed tasks over stdio.
    if name == "lint" {
        return lint::run(&files, args.get(1).map(String::as_str));
    }
    if name == "mcp" {
        return run_mcp(&files);
    }

    // Nearest file that defines a task wins (the fallback layering). This lookup
    // resolves both the target and each `Requires:` dependency the same way.
    let nearest = |n: &str| {
        files
            .iter()
            .find_map(|(p, tf)| tf.task(n).map(|t| (p, tf, t)))
    };

    if nearest(&name).is_none() {
        eprintln!("mdtask: no task named {name:?}");
        return ExitCode::FAILURE;
    }

    // Resolve the `Requires:` chain: dependencies first, the target last, each
    // task once. A missing or cyclic dependency is a hard error.
    let order = match mdtask_core::dependency_order(&name, |n| {
        nearest(n).map(|(_, _, t)| t.requires.clone())
    }) {
        Ok(order) => order,
        Err(e) => {
            eprintln!("mdtask: {e}");
            return ExitCode::FAILURE;
        }
    };

    let positional: Vec<String> = args.iter().skip(1).cloned().collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Run each task in order. Dependencies run with no positional args (their
    // defaults fill in); only the named target receives the CLI args. Abort on the
    // first non-zero exit, the way make stops a failing build.
    for step in &order {
        let (path, tf, task) = nearest(step).expect("a resolved name still exists");
        let task_file_dir = path.parent().unwrap_or(Path::new("."));
        let step_args: &[String] = if step == &name { &positional } else { &[] };

        let values = match TaskFile::bind(task, step_args) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("mdtask: {e} (usage: mdtask {step} {})", usage(task));
                return ExitCode::FAILURE;
            }
        };
        let inv = match tf.invocation(task, &values, &cwd, Some(task_file_dir)) {
            Ok(inv) => inv,
            Err(e) => {
                eprintln!("mdtask: {e} (usage: mdtask {step} {})", usage(task));
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
            Ok(s) if s.success() => {}
            // A failing child: surface a non-zero code. `code()` can exceed 255
            // (Windows returns the full 32-bit code; POSIX has already masked to
            // 8 bits), so clamp with `try_from` and never let it round to 0.
            Ok(s) => {
                let code = u8::try_from(s.code().unwrap_or(1)).unwrap_or(1).max(1);
                return ExitCode::from(code);
            }
            Err(e) => {
                eprintln!("mdtask: could not run {}: {e}", inv.program);
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

/// Dispatch `mdtask mcp`. With the `mcp` feature, serve over stdio; without it,
/// say so rather than silently pretending the subcommand does not exist.
#[cfg(feature = "mcp")]
fn run_mcp(files: &[(PathBuf, TaskFile)]) -> ExitCode {
    mcp::run(files)
}
#[cfg(not(feature = "mcp"))]
fn run_mcp(_files: &[(PathBuf, TaskFile)]) -> ExitCode {
    eprintln!("mdtask: built without the `mcp` feature (rebuild with `--features mcp`)");
    ExitCode::FAILURE
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
