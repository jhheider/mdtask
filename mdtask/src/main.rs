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

use std::path::{Path, PathBuf};
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
  mdtask --mcp              serve agent-allowed tasks to an MCP client (needs `mcp`)
  mdtask mcp                the same, unless a task is named `mcp`, which wins
  mdtask -s, --show NAME    print a task's script and metadata without running it
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

    // --mcp always serves, whatever the task file contains. The bare `mcp`
    // subcommand below is the older spelling and yields to a task of that name.
    let serve_mcp = matches!(args.first().map(String::as_str), Some("--mcp"));
    if serve_mcp {
        args.remove(0);
    }

    // --show NAME: print the task instead of running it. After -f, so
    // `mdtask -f other.md --show build` works, and leading-position-only like
    // every other flag, so a task's own `--show` still reaches the task.
    let mut show_name: Option<String> = None;
    if matches!(args.first().map(String::as_str), Some("-s" | "--show")) {
        if args.len() < 2 {
            eprintln!("mdtask: {} needs a task name", args[0]);
            return ExitCode::FAILURE;
        }
        show_name = Some(args.remove(1));
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

    if serve_mcp {
        return run_mcp(&files);
    }

    if let Some(name) = show_name {
        return show(&files, &name);
    }

    let Some(name) = args.first().cloned() else {
        return list(&files);
    };

    // The `mcp` subcommand serves the agent-allowed tasks over stdio, but only
    // when the task file does not define a task by that name. A task file is the
    // authority on what its task names mean, and a reserved word that silently
    // shadows one is a name you cannot use and are never told about. `--mcp`
    // above is the unambiguous spelling.
    if name == "mcp" && job_named(&files, "mcp").is_none() {
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
///
/// Two shapes, because a listing has two audiences. On a terminal it is a padded
/// column with the description wrapped under it, grouped by source file when
/// more than one contributes: which file a task came from is the first thing you
/// need when a name resolves to something you did not expect. Piped, it stays
/// one tab-separated line per task, so it is still greppable.
///
/// Neither shape truncates. The description used to be cut at its first physical
/// line, which in a hard-wrapped markdown paragraph is not a sentence, or even a
/// clause: listings ended mid-phrase on "One license only permits one".
fn list(files: &[(PathBuf, TaskFile)]) -> ExitCode {
    let mut seen = std::collections::BTreeSet::new();
    let mut groups: Vec<(&Path, Vec<(String, String)>)> = Vec::new();
    for (path, tf) in files {
        let mut rows = Vec::new();
        for job in tf.jobs() {
            if seen.insert(job.name.clone()) {
                let args = usage(job);
                let sep = if args.is_empty() { "" } else { " " };
                rows.push((
                    format!("{}{sep}{args}", job.name),
                    summary(&job.description),
                ));
            }
        }
        if !rows.is_empty() {
            groups.push((path.as_path(), rows));
        }
    }

    if groups.is_empty() {
        eprintln!("mdtask: no tasks found");
        return ExitCode::FAILURE;
    }

    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        for (_, rows) in &groups {
            for (label, desc) in rows {
                println!("{label}\t{desc}");
            }
        }
        return ExitCode::SUCCESS;
    }

    let width = terminal_width();
    // Wide enough for the labels, but never so wide that the description is
    // squeezed into a gutter by one long task name.
    let longest = groups
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(l, _)| l.chars().count()))
        .max()
        .unwrap_or(0);
    let column = longest.min(width / 3).max(1) + 2;
    let show_source = groups.len() > 1;

    for (i, (path, rows)) in groups.iter().enumerate() {
        if show_source {
            if i > 0 {
                println!();
            }
            println!("{}", display_path(path));
        }
        for (label, desc) in rows {
            let indent = if show_source { 2 } else { 0 };
            let pad = " ".repeat(indent);
            if desc.is_empty() {
                println!("{pad}{label}");
                continue;
            }
            // A label longer than the column gets the description on the next
            // line rather than shoving the whole row out of alignment.
            let lines = wrap(desc, width.saturating_sub(column + indent).max(20));
            let mut lines = lines.iter();
            if label.chars().count() < column {
                let gap = column - label.chars().count();
                println!("{pad}{label}{}{}", " ".repeat(gap), lines.next().unwrap());
            } else {
                println!("{pad}{label}");
            }
            for rest in lines {
                println!("{pad}{}{rest}", " ".repeat(column));
            }
        }
    }
    ExitCode::SUCCESS
}

/// A task's description as one line: its first paragraph, unwrapped.
///
/// Markdown descriptions are hard-wrapped prose, so the first *line* is an
/// arbitrary fragment. The first *paragraph* is the author's opening thought,
/// which is what a listing wants.
fn summary(description: &str) -> String {
    description
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Greedy word wrap. Counts characters, not display columns: mdtask has no
/// dependencies and a task description is overwhelmingly ASCII, so a wide
/// character wraps a column early rather than pulling in a unicode-width crate.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Terminal width from `COLUMNS`, else 80.
///
/// The ioctl would need libc, and this binary's whole dependency list is
/// `serde_json`, behind an off-by-default feature. Eighty is right when the
/// variable is absent and a shell that exports it gets the real thing.
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w >= 40)
        .unwrap_or(80)
}

/// A task file's path, relative to the current directory when that is shorter.
fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    let rel = cwd
        .as_ref()
        .and_then(|c| path.strip_prefix(c).ok())
        .map(|p| p.display().to_string());
    match rel {
        Some(r) if !r.is_empty() => r,
        _ => path.display().to_string(),
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

/// Print everything about a task without running it: where it is defined, what
/// it declares, and the script itself.
///
/// A task runner runs shell that someone else wrote, quite possibly in a file
/// you have not opened, and until now the only way to find out what `mdtask
/// deploy` would do was to run `mdtask deploy`. That is a poor trade to offer.
fn show(files: &[(PathBuf, TaskFile)], name: &str) -> ExitCode {
    let Some((path, job)) = files
        .iter()
        .find_map(|(p, tf)| tf.job(name).map(|j| (p, j)))
    else {
        eprintln!("mdtask: no task named {name:?}");
        return ExitCode::FAILURE;
    };

    println!("{}", invocation_usage(name, job));
    println!("  from  {}", display_path(path));

    let lang = if job.lang().is_empty() {
        "(unlabeled fence, runs as sh)".to_string()
    } else {
        job.lang().to_string()
    };
    println!("  runs  {lang}");

    if !job.opts().is_empty() {
        println!("  opts  {}", job.opts().join(" "));
    }
    for (k, v) in job.env() {
        println!("  env   {k}={v}");
    }
    if !job.requires.is_empty() {
        // These run first, in order, and each may pull in dependencies of its
        // own; what is printed is what this task declares, not the flattened
        // chain.
        let deps: Vec<String> = job
            .requires
            .iter()
            .map(|r| {
                if r.args.is_empty() {
                    r.name.clone()
                } else {
                    format!("{} {}", r.name, r.args.join(" "))
                }
            })
            .collect();
        println!("  first {}", deps.join(", "));
    }
    if job.agent_allow {
        println!("  agent allowed (an MCP or agent surface may run this)");
    }

    let desc = job.description.trim();
    if !desc.is_empty() {
        println!();
        for line in desc.lines() {
            println!("{}", indent("  ", line));
        }
    }

    println!();
    for line in job.script().lines() {
        println!("{}", indent("    ", line));
    }
    ExitCode::SUCCESS
}

/// Indent a line, leaving a blank one blank rather than turning it into
/// trailing whitespace.
fn indent(pad: &str, line: &str) -> String {
    if line.is_empty() {
        String::new()
    } else {
        format!("{pad}{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this replaced: a listing showed the first *physical* line of a
    /// hard-wrapped markdown paragraph, which ends wherever the author's editor
    /// happened to wrap. Real output read "One license only permits one".
    #[test]
    fn indent_leaves_a_blank_line_blank() {
        assert_eq!(indent("    ", "echo hi"), "    echo hi");
        assert_eq!(indent("    ", ""), "", "not four spaces of nothing");
    }

    #[test]
    fn summary_unwraps_the_first_paragraph() {
        let desc = "Start the local dev server. One license only permits one\n\
                    instance at a time.\n\
                    \n\
                    A second paragraph, which the listing does not want.\n";
        assert_eq!(
            summary(desc),
            "Start the local dev server. One license only permits one instance at a time."
        );
    }

    #[test]
    fn summary_of_nothing_is_nothing() {
        assert_eq!(summary(""), "");
        assert_eq!(summary("\n\n"), "");
    }

    #[test]
    fn wrap_breaks_on_words_and_fills() {
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
    }

    /// A URL or a long path has no break in it. Overflowing the column is the
    /// right failure: the alternative is severing something meant to be copied.
    #[test]
    fn wrap_does_not_break_inside_a_word() {
        let long = "https://example.com/a/very/long/path/indeed";
        assert_eq!(wrap(long, 10), [long]);
    }

    #[test]
    fn wrap_always_returns_at_least_one_line() {
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn terminal_width_falls_back_when_columns_is_unusable() {
        // Not a number, absurdly narrow, or absent: 80 either way. A width of 3
        // would put one word per line under a column that does not fit.
        for bad in ["", "wide", "0", "12"] {
            unsafe { std::env::set_var("COLUMNS", bad) };
            assert_eq!(terminal_width(), 80, "COLUMNS={bad:?}");
        }
        unsafe { std::env::set_var("COLUMNS", "120") };
        assert_eq!(terminal_width(), 120);
        unsafe { std::env::remove_var("COLUMNS") };
    }
}
