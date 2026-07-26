//! `mdtask` runs tasks defined in markdown. It is a thin CLI over `mdtask-core`.
//!
//! ```text
//! mdtask                 list tasks (walks up for tasks.md / maskfile.md / README.md)
//! mdtask <name> [args...]  run a task; positional args fill its `Args:` in order
//! mdtask mcp               serve agent-allowed tasks to an MCP client (--features mcp)
//! mdtask -f FILE ...       use a specific task file (no directory walk)
//! mdtask -V / --version    print the version
//! mdtask -h / --help       print this help
//! ```
//!
//! `-V`/`--version` and `-h`/`--help` are honoured only in the first (subcommand)
//! position, so `mdtask <task> --help` still forwards `--help` to the task, the
//! way a task runner should. That is also why there is no arg-parser dependency:
//! everything after the task name belongs to the task, which a real parser would
//! fight to claim.
//!
//! A task with `Requires:` runs its dependencies first (resolved across the
//! layered files, deps before dependents, aborting on the first failure).
//!
//! Task files are layered like just's `set fallback`: files nearer the current
//! directory shadow those farther up, so a project inherits a baseline of tasks
//! from its parents and overrides them where it wants.
//!
//! The library is the point, so this is deliberately small, with no arg-parser
//! dependency. All of the run mechanics (interpreter, argv, dependency order,
//! spawn) live in `mdtask-core`; this binary just resolves files and calls
//! `mdtask_core::run`.

use std::path::PathBuf;
use std::process::ExitCode;

use mdtask_core::{Job, RunError, TaskFile};

#[cfg(feature = "mcp")]
mod mcp;

/// Usage text for `-h`/`--help` (kept in sync with the module docs above).
const HELP: &str = "\
mdtask runs tasks defined in markdown (tasks.md / maskfile.md / README.md).

Usage:
  mdtask                    list the available tasks
  mdtask <name> [args...]   run a task; positional args fill its `Args:` in order
  mdtask mcp                serve agent-allowed tasks to an MCP client (needs `mcp`)
  mdtask -f, --file FILE    use a specific task file (no directory walk)
  mdtask -V, --version      print the version
  mdtask -h, --help         print this help
";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Version/help are recognised only in the first (subcommand) position, so
    // `mdtask <task> --help` still forwards `--help` to the task.
    match args.first().map(String::as_str) {
        Some("-V" | "--version") => {
            println!("mdtask {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("-h" | "--help") => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    // -f/--file FILE selects one task file (no walk); else walk up from cwd.
    //
    // Only in the leading position, like -h and -V above. Scanning all of argv
    // meant a task's own arguments were stolen: `mdtask run -f config.yml` tried
    // to read `config.yml` as a task file, and any task taking a `-f` flag of its
    // own (`grep -f`, `docker -f`, `rsync -f`) was unusable with no way to
    // escape. Everything after the task name belongs to the task, which is what
    // this module's own docs promise.
    let mut file: Option<PathBuf> = None;
    if matches!(args.first().map(String::as_str), Some("-f" | "--file")) {
        if args.len() < 2 {
            eprintln!("mdtask: {} needs a path", args[0]);
            return ExitCode::FAILURE;
        }
        file = Some(PathBuf::from(args.remove(1)));
        args.remove(0);
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
        for w in tf.warnings() {
            eprintln!("mdtask: {}: {w}", path.display());
        }
    }

    let Some(name) = args.first().cloned() else {
        return list(&files);
    };

    // The `mcp` subcommand serves the agent-allowed tasks over stdio; it is checked
    // before task dispatch.
    if name == "mcp" {
        return run_mcp(&files);
    }

    let positional: Vec<String> = args.iter().skip(1).cloned().collect();

    // Surplus positionals are a typo, not an offering. They used to be dropped
    // in silence, so a stale flag or a second task name after the first ran the
    // task anyway and exited 0. Checked here rather than in core: the library
    // binding is a mechanism, and an embedder passing a vector is doing so
    // deliberately, but a person typing extra words at a shell is not.
    if let Some(job) = job_named(&files, &name) {
        let declared = job.args.len();
        let takes_rest = job.args.last().is_some_and(|a| a.variadic);
        if !takes_rest && positional.len() > declared {
            eprintln!(
                "mdtask: {} unexpected argument(s) [{}] (usage: {})",
                positional.len() - declared,
                positional[declared..].join(", "),
                invocation_usage(&name, job)
            );
            return ExitCode::FAILURE;
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Core owns the whole run: resolve the target and its `Requires:` chain across
    // the layered files, then run each step (inheriting stdio), stopping on the
    // first non-zero exit.
    match mdtask_core::run(&files, &name, &positional, &cwd) {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        // A failing child: surface a non-zero code. `code()` can exceed 255
        // (Windows returns the full 32-bit code; POSIX has already masked to
        // 8 bits), so clamp with `try_from` and never let it round to 0.
        Ok(status) => {
            let code = u8::try_from(status.code().unwrap_or(1)).unwrap_or(1).max(1);
            ExitCode::from(code)
        }
        Err(e) => {
            report(&files, &name, &e);
            ExitCode::FAILURE
        }
    }
}

/// Surface a `RunError` on stderr. `NotFound` and a missing argument get a tailored
/// line (the latter with the task's usage); everything else prints its `Display`.
fn report(files: &[(PathBuf, TaskFile)], name: &str, e: &RunError) {
    match e {
        RunError::NotFound(n) => eprintln!("mdtask: no task named {n:?}"),
        RunError::MissingArg(_) => match job_named(files, name) {
            Some(job) => eprintln!("mdtask: {e} (usage: {})", invocation_usage(name, job)),
            None => eprintln!("mdtask: {e}"),
        },
        _ => eprintln!("mdtask: {e}"),
    }
}

/// How to invoke a job: `mdtask <name>` plus its declared arguments, with no
/// trailing space when it declares none. The old form always appended a space
/// and the argument list, so a task with no arguments advertised itself as
/// `mdtask release ` with nothing after it.
fn invocation_usage(name: &str, job: &Job) -> String {
    let args = usage(job);
    if args.is_empty() {
        format!("mdtask {name}")
    } else {
        format!("mdtask {name} {args}")
    }
}

/// The nearest definition of `name` across the layered files, for usage output.
fn job_named<'a>(files: &'a [(PathBuf, TaskFile)], name: &str) -> Option<&'a Job> {
    files.iter().find_map(|(_, tf)| tf.job(name))
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
        for job in tf.jobs() {
            if seen.insert(job.name.clone()) {
                let a = usage(job);
                let sep = if a.is_empty() { "" } else { " " };
                let desc = job.description.lines().next().unwrap_or("");
                println!("{}{sep}{a}\t{desc}", job.name);
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

/// `<arg1> [arg2] [rest...]` for a job's declared args, for usage/list output:
/// required args in angle brackets, defaulted/variadic in square brackets.
fn usage(job: &Job) -> String {
    job.args
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
