//! `mdtask-core` — parse a markdown task file into a typed command tree and build
//! runnable invocations. Embeddable, execution-capable, and dependency-free.
//!
//! A task file is ordinary markdown (a `tasks.md`, a `maskfile.md`, or a project
//! `README.md`): a heading is a task, the first fenced code block under it is the
//! script, and `Key: value` lines in the body carry metadata. The format is an
//! own grammar — a graceful superset that borrows xc's metadata vocabulary and
//! mask's runtime shape (per-fence interpreter, positional args), readable by
//! those tools where the features overlap but not claiming compatibility.
//!
//! ```
//! let tf = mdtask_core::parse("\
//! ## greet\n\
//! \n\
//! Args: name\n\
//! \n\
//! ```sh\n\
//! echo \"hello {{ name }}\"\n\
//! ```\n");
//! let task = tf.task("greet").unwrap();
//! assert_eq!(task.args, ["name"]);
//! ```
//!
//! Parsing is pure; `Task`/`TaskFile` build an [`Invocation`] (program, args,
//! env, cwd) which the caller runs — on its own worker/thread — or executes with
//! [`Invocation::run`]. The parser is line-based (no CommonMark dependency), so a
//! `#` or `Key:` inside a fenced block is never mistaken for structure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A parsed task file: the tasks, and any file-level environment hoisted to all
/// of them (an `Env:` line before the first task).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFile {
    pub env: Vec<(String, String)>,
    pub tasks: Vec<Task>,
}

/// One task: a named script with its interpreter and metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Task {
    /// The heading text (the command name).
    pub name: String,
    /// Prose in the task body that is not a recognized `Key: value` line.
    pub description: String,
    /// The fenced block's info-string language (`sh`, `zsh`, `python`, …); empty
    /// means an unlabeled fence (treated as `sh`).
    pub lang: String,
    /// The script (the fenced block's contents), verbatim.
    pub script: String,
    /// `Dir:` — the working directory, relative to the run root. May contain
    /// `{{ arg }}`; if it resolves to a file, the file's parent is used.
    pub dir: Option<String>,
    /// `Env:` — extra environment for this task.
    pub env: Vec<(String, String)>,
    /// `Args:` — positional argument names, substituted as `{{ name }}` in the
    /// script and also exported as `$name`.
    pub args: Vec<String>,
    /// `Requires:` — task names this one depends on (parsed; the caller sequences
    /// them — mdtask-core does not run dependencies itself yet).
    pub requires: Vec<String>,
    /// `Agent: allow` — opt in to being listed/run by an MCP/agent surface. Off
    /// by default, so exposing a task file to an agent is safe.
    pub agent_allow: bool,
}

/// A runnable command built from a task: what to exec, with what environment, in
/// which directory. The caller runs it however it likes (a worker, a thread),
/// keeping execution off any hot path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// A declared argument had no value supplied when building an invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingArg(pub String);

impl std::fmt::Display for MissingArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing value for argument `{}`", self.0)
    }
}
impl std::error::Error for MissingArg {}

impl TaskFile {
    /// Find a task by name (exact, case-sensitive — heading text as written).
    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.name == name)
    }

    /// Build the invocation for `task`, given `args` (name → value). Combines the
    /// file-level hoisted env, the task env, and the args-as-env; substitutes
    /// `{{ arg }}` in the script and `Dir:`; resolves the working directory
    /// against `root`. Errors if a declared arg has no value.
    pub fn invocation(
        &self,
        task: &Task,
        args: &BTreeMap<String, String>,
        root: &Path,
    ) -> Result<Invocation, MissingArg> {
        for name in &task.args {
            if !args.contains_key(name) {
                return Err(MissingArg(name.clone()));
            }
        }
        let script = substitute(&task.script, args);
        let (program, flag) = interpreter(&task.lang);

        // Env precedence: hoisted, then task, then args (args win — they are the
        // most specific), so `$name` resolves to the passed value.
        let mut env = self.env.clone();
        env.extend(task.env.iter().cloned());
        env.extend(args.iter().map(|(k, v)| (k.clone(), v.clone())));

        let cwd = match &task.dir {
            None => root.to_path_buf(),
            Some(d) => {
                let resolved = root.join(substitute(d, args));
                // A file target means "run in its directory" (the [no-cd] idea).
                if resolved.is_file() {
                    resolved
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.to_path_buf())
                } else {
                    resolved
                }
            }
        };

        Ok(Invocation {
            program: program.to_string(),
            args: vec![flag.to_string(), script],
            env,
            cwd,
        })
    }
}

impl Invocation {
    /// Execute the invocation and wait for it, capturing output. Convenience for
    /// the CLI; an embedder that must not block a thread should run the fields
    /// itself. Args/env are exported to the child; nothing is inherited beyond
    /// the parent environment plus `env`.
    pub fn run(&self) -> std::io::Result<std::process::Output> {
        std::process::Command::new(&self.program)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .output()
    }
}

/// Map a fence language to `(program, code-flag)`. Unlabeled or unknown falls
/// back to `sh -c`, so a plain ` ``` ` block runs as a shell script.
fn interpreter(lang: &str) -> (&'static str, &'static str) {
    match lang.trim().to_ascii_lowercase().as_str() {
        "" | "sh" | "shell" => ("sh", "-c"),
        "bash" => ("bash", "-c"),
        "zsh" => ("zsh", "-c"),
        "fish" => ("fish", "-c"),
        "python" | "py" | "python3" => ("python3", "-c"),
        "ruby" => ("ruby", "-e"),
        "node" | "js" | "javascript" => ("node", "-e"),
        _ => ("sh", "-c"),
    }
}

/// Replace `{{ name }}` tokens (any inner whitespace) with `args[name]`. A token
/// whose name is not in `args` is left as written, so a literal `{{x}}` that is
/// not an argument survives.
fn substitute(src: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        if let Some(close) = after.find("}}") {
            let name = after[..close].trim();
            match args.get(name) {
                Some(v) => out.push_str(v),
                None => {
                    // Not an argument: keep the token verbatim.
                    out.push_str("{{");
                    out.push_str(&after[..close]);
                    out.push_str("}}");
                }
            }
            rest = &after[close + 2..];
        } else {
            out.push_str("{{");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Parse a markdown task file. Line-based: a heading starts a task, the first
/// fenced block under it is the script, and `Key: value` lines in the body set
/// metadata. `Env:` lines before the first task are hoisted to all tasks.
pub fn parse(src: &str) -> TaskFile {
    let mut file = TaskFile::default();
    let mut cur: Option<Task> = None;
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut have_script = false; // first fence per task only
    let mut script = String::new();

    // Finalize the in-progress heading. A heading with a script is a task; one
    // without (a section like `# Tasks`) is not - but its `Env:` hoists to all
    // tasks, so a shared preamble under a container heading works.
    macro_rules! flush {
        () => {
            if let Some(mut t) = cur.take() {
                if t.script.is_empty() {
                    file.env.append(&mut t.env);
                } else {
                    t.description = t.description.trim().to_string();
                    file.tasks.push(t);
                }
            }
        };
    }

    for raw in src.split('\n') {
        let line = raw;
        // Fence toggling. A fence is ``` or ~~~ (>=3), optionally indented.
        if let Some(marker) = fence_of(line) {
            if in_fence {
                if line.trim_start().starts_with(fence_marker) {
                    in_fence = false;
                    if let Some(t) = cur.as_mut() {
                        if !have_script {
                            t.script = script.clone();
                            have_script = true;
                        }
                    }
                    script.clear();
                }
            } else {
                in_fence = true;
                fence_marker = marker;
                // The info string (language) — record on the current task's first
                // fence.
                if let Some(t) = cur.as_mut()
                    && !have_script
                {
                    t.lang = info_string(line, marker);
                }
                script.clear();
            }
            continue;
        }
        if in_fence {
            if cur.is_some() && !have_script {
                script.push_str(line);
                script.push('\n');
            }
            continue;
        }
        // A heading starts a new task.
        if let Some(name) = heading(line) {
            flush!();
            cur = Some(Task {
                name,
                ..Task::default()
            });
            have_script = false;
            continue;
        }
        // A metadata or description line, in a task or (for hoisted Env) before one.
        apply_line(line, cur.as_mut(), &mut file.env);
    }
    flush!();
    file
}

/// The fence marker (``` or ~~~) if `line` opens/closes a fence, else `None`.
fn fence_of(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// The info-string language after the opening fence marker.
fn info_string(line: &str, marker: &str) -> String {
    line.trim_start()
        .strip_prefix(marker)
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

/// The heading text if `line` is an ATX heading (`#`..`######`), else `None`.
fn heading(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let after = t.trim_start_matches('#');
    // Must have a space after the `#` run (a real ATX heading), and not be all #.
    if after == t || !after.starts_with(' ') {
        return None;
    }
    Some(after.trim().to_string())
}

/// Apply a body line: a recognized `Key: value` sets metadata (case-insensitive
/// key, xc vocabulary); anything else is description. `Env:` before the first
/// task accumulates into the hoisted `file_env`.
fn apply_line(line: &str, task: Option<&mut Task>, file_env: &mut Vec<(String, String)>) {
    if let Some((key, value)) = split_key(line) {
        let value = value.trim();
        match key.as_str() {
            "env" | "environment" => {
                let pairs = parse_env(value);
                match task {
                    Some(t) => t.env.extend(pairs),
                    None => file_env.extend(pairs), // hoisted
                }
                return;
            }
            "dir" | "directory" => {
                if let Some(t) = task {
                    t.dir = Some(value.to_string());
                }
                return;
            }
            "args" | "arguments" => {
                if let Some(t) = task {
                    t.args = value.split_whitespace().map(str::to_string).collect();
                }
                return;
            }
            "requires" | "req" => {
                if let Some(t) = task {
                    t.requires = value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                return;
            }
            "agent" => {
                if let Some(t) = task {
                    t.agent_allow = value.eq_ignore_ascii_case("allow");
                }
                return;
            }
            _ => {}
        }
    }
    // Description (only within a task; drop stray prose outside one).
    if let Some(t) = task
        && !line.trim().is_empty()
    {
        t.description.push_str(line.trim());
        t.description.push('\n');
    }
}

/// Split `Key: value`, returning the lowercased key if the line looks like one
/// (a short, single-word key before the first colon).
fn split_key(line: &str) -> Option<(String, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    // A metadata key is one word, no spaces, not indented as a list, non-empty.
    if key.is_empty() || key.contains(char::is_whitespace) || line.starts_with(char::is_whitespace)
    {
        return None;
    }
    Some((key.to_ascii_lowercase(), &line[colon + 1..]))
}

/// Parse an `Env:` value: comma-separated `KEY=VALUE` pairs.
fn parse_env(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_named_tasks_with_interpreter() {
        let tf =
            parse("## build\n\n```sh\ncargo build\n```\n\n## check\n\n```zsh\nprint hi\n```\n");
        assert_eq!(tf.tasks.len(), 2);
        assert_eq!(tf.tasks[0].name, "build");
        assert_eq!(tf.tasks[0].lang, "sh");
        assert_eq!(tf.tasks[0].script.trim(), "cargo build");
        assert_eq!(tf.tasks[1].lang, "zsh");
    }

    #[test]
    fn metadata_keys_are_xc_vocabulary_case_insensitive() {
        let tf = parse(
            "## deploy\n\nDIR: {{ target }}\nEnv: REGION=us, TIER=prod\nArgs: target\nRequires: build, test\nAgent: allow\n\n```sh\necho go\n```\n",
        );
        let t = &tf.tasks[0];
        assert_eq!(t.dir.as_deref(), Some("{{ target }}"));
        assert_eq!(
            t.env,
            vec![
                ("REGION".into(), "us".into()),
                ("TIER".into(), "prod".into())
            ]
        );
        assert_eq!(t.args, vec!["target"]);
        assert_eq!(t.requires, vec!["build", "test"]);
        assert!(t.agent_allow);
    }

    #[test]
    fn agent_gate_is_off_by_default() {
        let tf = parse("## secret\n\n```sh\nrm -rf /\n```\n");
        assert!(!tf.tasks[0].agent_allow);
    }

    #[test]
    fn top_level_env_is_hoisted() {
        let tf = parse("# Tasks\n\nEnv: SHARED=1\n\n## a\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.env, vec![("SHARED".into(), "1".into())]);
    }

    #[test]
    fn fence_content_is_not_parsed_as_structure() {
        // A `## heading` and a `Key:` line inside a fence stay in the script.
        let tf = parse("## a\n\n```sh\n## not a task\nDir: not-metadata\n```\n");
        assert_eq!(tf.tasks.len(), 1);
        assert!(tf.tasks[0].script.contains("## not a task"));
        assert!(tf.tasks[0].dir.is_none());
    }

    #[test]
    fn substitutes_args_and_leaves_unknown_tokens() {
        let out = substitute(
            "hello {{ name }} and {{ other }}",
            &args(&[("name", "world")]),
        );
        assert_eq!(out, "hello world and {{ other }}");
    }

    #[test]
    fn invocation_substitutes_sets_env_and_picks_interpreter() {
        let tf = parse("## greet\n\nArgs: name\n\n```zsh\nprint \"hi {{ name }}\"\n```\n");
        let t = tf.task("greet").unwrap();
        let inv = tf
            .invocation(t, &args(&[("name", "sam")]), Path::new("/root"))
            .unwrap();
        assert_eq!(inv.program, "zsh");
        assert_eq!(inv.args[0], "-c");
        assert!(inv.args[1].contains("hi sam"));
        assert!(inv.env.contains(&("name".to_string(), "sam".to_string())));
        assert_eq!(inv.cwd, Path::new("/root"));
    }

    #[test]
    fn a_missing_arg_is_an_error() {
        let tf = parse("## t\n\nArgs: file\n\n```sh\ncat {{ file }}\n```\n");
        let t = tf.task("t").unwrap();
        assert_eq!(
            tf.invocation(t, &args(&[]), Path::new("/root")),
            Err(MissingArg("file".into()))
        );
    }

    #[test]
    fn dir_without_the_file_present_joins_the_root() {
        let tf = parse("## t\n\nDir: sub/dir\n\n```sh\ntrue\n```\n");
        let t = tf.task("t").unwrap();
        let inv = tf.invocation(t, &args(&[]), Path::new("/root")).unwrap();
        assert_eq!(inv.cwd, Path::new("/root/sub/dir"));
    }
}
