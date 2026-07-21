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

/// A parsed task file: the tasks, any file-level environment hoisted to all of
/// them (an `Env:` under a section heading, applied to **every** task regardless
/// of where in the document it appears — hoisting is not positional), and any
/// parse warnings (an unterminated fence, a duplicate task, an unknown fence
/// language). Parsing is infallible — a malformed file still yields what it can —
/// so an embedder should surface `warnings` rather than trust silence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFile {
    pub env: Vec<(String, String)>,
    pub tasks: Vec<Task>,
    pub warnings: Vec<String>,
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
    /// `{{ arg }}`, and `dirname(PATH)` takes a path's lexical parent (so
    /// `Dir: dirname({{ file }})` runs in the file's folder). Resolution is
    /// purely lexical — no filesystem access.
    pub dir: Option<String>,
    /// `Env:` — extra environment for this task.
    pub env: Vec<(String, String)>,
    /// `Args:` — positional argument names. Each is substituted as `{{ name }}`
    /// in the script AND exported as `$name`. **`{{ name }}` is raw text
    /// substitution** (it happens before the shell parses the script), so
    /// `"{{ name }}"` is NOT injection-safe for untrusted values — prefer
    /// `"$name"`, which the shell quotes. Reserve `{{ }}` for `Dir:` and for
    /// developer-authored templates.
    pub args: Vec<String>,
    /// `Requires:` — task names this one depends on (parsed; the caller sequences
    /// them — mdtask-core does not run dependencies itself yet).
    pub requires: Vec<String>,
    /// `Agent: allow` — opt in to being listed/run by an MCP/agent surface.
    /// Advisory data: nothing in mdtask-core's execution path checks it. A caller
    /// exposing tasks to an agent must filter with [`TaskFile::agent_tasks`] (off
    /// by default), which is the enforcement point — the flag alone is not.
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
    /// Find a task by name (exact, case-sensitive — heading text as written). The
    /// first definition wins if a name is duplicated (a warning is recorded).
    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.name == name)
    }

    /// The tasks that opted in to an agent/MCP surface via `Agent: allow`. A
    /// caller exposing tasks to an agent should list and run **only** these — the
    /// flag is advisory data on `Task`, so this iterator is the enforcement point,
    /// not the field. (Direct field access bypasses the gate by design; the gate
    /// lives at the surface that chooses what to expose.)
    pub fn agent_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| t.agent_allow)
    }

    /// Build the invocation for `task`, given `args` (name → value). Combines the
    /// file-level hoisted env, the task env, and the args-as-env; substitutes
    /// `{{ arg }}` in the script and `Dir:`; resolves the working directory
    /// against `root`. Errors if a declared arg has no value.
    ///
    /// Pure and cheap: no filesystem or process access, so it is safe to call
    /// straight from a UI event handler / render path — build the `Invocation`
    /// here, run it elsewhere.
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

        // Purely lexical — no filesystem access, so `invocation` stays a cheap,
        // deterministic, off-thread-safe builder. `Dir: dirname({{ file }})` runs
        // in the file's parent directory (the `[no-cd]`-from-dirname idea), and
        // works whether or not the file exists yet (a task that *creates* it).
        let cwd = match &task.dir {
            None => root.to_path_buf(),
            Some(d) => root.join(resolve_dir(d, args)),
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

/// Parse a markdown task file. Line-based (no CommonMark dependency): a heading
/// starts a task, the first fenced block under it is the script, and `Key: value`
/// lines set metadata. Infallible — problems are reported in [`TaskFile::warnings`],
/// not by dropping to silence. CRLF endings are normalized.
pub fn parse(src: &str) -> TaskFile {
    let mut file = TaskFile::default();
    let mut cur: Option<Task> = None;
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut have_script = false; // first fence per task only
    let mut script = String::new();

    for raw in src.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw); // normalize CRLF
        if in_fence {
            // A fence is closed only by a BARE marker line (CommonMark): ` ``` `
            // with an info string opens, it does not close, so a stray fence-open
            // cannot accidentally terminate an unterminated block early.
            if is_closing_fence(line, fence_marker) {
                in_fence = false;
                if let Some(t) = cur.as_mut()
                    && !have_script
                {
                    t.script = std::mem::take(&mut script);
                    have_script = true;
                }
                script.clear();
            } else if cur.is_some() && !have_script {
                script.push_str(line);
                script.push('\n');
            }
            continue;
        }
        if let Some(marker) = opening_fence(line) {
            in_fence = true;
            fence_marker = marker;
            if let Some(t) = cur.as_mut()
                && !have_script
            {
                t.lang = info_string(line, marker);
            }
            script.clear();
            continue;
        }
        if let Some(name) = heading(line) {
            finalize(cur.take(), &mut file);
            cur = Some(Task {
                name,
                ..Task::default()
            });
            have_script = false;
            continue;
        }
        apply_line(line, cur.as_mut(), &mut file.env);
    }
    // An unterminated fence at EOF: still capture the script (do not lose the
    // task), but warn — a forgotten closing fence is a common authoring slip.
    if in_fence {
        if let Some(t) = cur.as_mut()
            && !have_script
        {
            t.script = std::mem::take(&mut script);
        }
        let name = cur.as_ref().map(|t| t.name.clone()).unwrap_or_default();
        file.warnings
            .push(format!("unterminated code fence in task {name:?}"));
    }
    finalize(cur.take(), &mut file);
    file
}

/// Finalize a heading into the file. A heading with a script is a task; one
/// without (a `# Tasks` section) is not, but its `Env:` hoists to all tasks.
/// Records warnings for a duplicate name or an unknown fence language.
fn finalize(task: Option<Task>, file: &mut TaskFile) {
    let Some(mut t) = task else {
        return;
    };
    if t.script.is_empty() {
        file.env.append(&mut t.env); // section heading — hoist its env
        return;
    }
    t.description = t.description.trim().to_string();
    if file.tasks.iter().any(|x| x.name == t.name) {
        file.warnings.push(format!(
            "duplicate task {:?}; the first defined wins",
            t.name
        ));
    }
    if !is_known_lang(&t.lang) {
        file.warnings.push(format!(
            "task {:?}: fenced language {:?} is not a known interpreter; running as sh",
            t.name, t.lang
        ));
    }
    file.tasks.push(t);
}

/// Resolve a `Dir:` value: substitute `{{ arg }}`, then apply a `dirname(PATH)`
/// wrapper as a lexical parent (no filesystem access). Everything else is used
/// verbatim as a directory path.
fn resolve_dir(dir: &str, args: &BTreeMap<String, String>) -> String {
    let s = substitute(dir, args);
    let s = s.trim();
    if let Some(inner) = s.strip_prefix("dirname(").and_then(|x| x.strip_suffix(')')) {
        return Path::new(inner.trim())
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    s.to_string()
}

/// Whether a fence language maps to an interpreter (unlabeled counts as `sh`).
fn is_known_lang(lang: &str) -> bool {
    matches!(
        lang.trim().to_ascii_lowercase().as_str(),
        "" | "sh"
            | "shell"
            | "bash"
            | "zsh"
            | "fish"
            | "python"
            | "py"
            | "python3"
            | "ruby"
            | "node"
            | "js"
            | "javascript"
    )
}

/// The opening fence marker if `line` starts one, else `None`.
fn opening_fence(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Whether `line` is a bare closing fence for `marker` (only the fence char,
/// no info string — CommonMark's closing rule).
fn is_closing_fence(line: &str, marker: &str) -> bool {
    let ch = marker.as_bytes()[0];
    let t = line.trim();
    t.len() >= 3 && t.bytes().all(|b| b == ch)
}

/// Search for task files from `start` up to the filesystem root, **nearest
/// first**. In each ancestor directory the first of `tasks.md`, `maskfile.md`,
/// `README.md` that parses to at least one task is taken. The CLI layers these
/// child-first (a nearer file shadows a farther one by task name — like just's
/// `set fallback`, so a project can inherit a baseline of tasks from a parent).
/// Embedders with their own project root can ignore this and call [`parse`].
pub fn find_task_files(start: &Path) -> Vec<(PathBuf, TaskFile)> {
    let mut found = Vec::new();
    for dir in start.ancestors() {
        for name in ["tasks.md", "maskfile.md", "README.md"] {
            let path = dir.join(name);
            if let Ok(src) = std::fs::read_to_string(&path) {
                let tf = parse(&src);
                if !tf.tasks.is_empty() {
                    found.push((path, tf));
                    break; // one file per directory
                }
            }
        }
    }
    found
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
/// (a single-word key before the first colon). Leading indentation is allowed —
/// a `Dir:` indented under a list still counts — because only *known* keys act
/// (see `apply_line`), so ordinary prose with a colon stays description.
fn split_key(line: &str) -> Option<(String, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
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

    #[test]
    fn dirname_is_lexical_and_works_before_the_file_exists() {
        // The flagship [no-cd] case: run in a not-yet-created file's folder. No
        // filesystem access, so this is deterministic and needs no real file.
        let tf = parse("## t\n\nArgs: file\nDir: dirname({{ file }})\n\n```sh\ntrue\n```\n");
        let t = tf.task("t").unwrap();
        let inv = tf
            .invocation(t, &args(&[("file", "out/report.pdf")]), Path::new("/root"))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/root/out"));
    }

    #[test]
    fn crlf_scripts_are_normalized() {
        let tf = parse("## t\r\n\r\n```sh\r\necho foo\r\necho bar\r\n```\r\n");
        assert_eq!(tf.tasks[0].script, "echo foo\necho bar\n");
        assert!(!tf.tasks[0].script.contains('\r'));
    }

    #[test]
    fn an_unterminated_fence_warns_but_keeps_the_task() {
        let tf = parse("## a\n\n```sh\necho hi\n"); // no closing fence
        assert_eq!(tf.tasks.len(), 1);
        assert_eq!(tf.tasks[0].script.trim(), "echo hi");
        assert!(tf.warnings.iter().any(|w| w.contains("unterminated")));
    }

    #[test]
    fn a_stray_fence_open_does_not_close_an_unterminated_block() {
        // ```sh has an info string, so it opens, it does not close — a bare ```
        // is the only close. (The trailing block here is what closes it.)
        let tf = parse("## a\n\n```sh\none\n```sh\ntwo\n```\n");
        assert!(tf.tasks[0].script.contains("one"));
        assert!(tf.tasks[0].script.contains("```sh\ntwo"));
    }

    #[test]
    fn indented_metadata_is_recognized() {
        let tf = parse("## a\n\n- steps:\n  Dir: sub\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.tasks[0].dir.as_deref(), Some("sub"));
    }

    #[test]
    fn duplicate_and_unknown_lang_warn() {
        let tf = parse("## a\n\n```json\n{}\n```\n\n## a\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.tasks.len(), 2);
        assert!(tf.warnings.iter().any(|w| w.contains("duplicate")));
        assert!(tf.warnings.iter().any(|w| w.contains("json")));
    }

    #[test]
    fn agent_tasks_filters_to_the_gated_ones() {
        let tf =
            parse("## open\n\nAgent: allow\n\n```sh\ntrue\n```\n\n## closed\n\n```sh\ntrue\n```\n");
        let names: Vec<_> = tf.agent_tasks().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["open"]);
    }

    #[test]
    fn find_task_files_layers_child_over_parent() {
        // parent/tasks.md defines `base` + `shared`; parent/child/tasks.md
        // redefines `shared` + adds `only`. Nearest-first, so child wins.
        let base = std::env::temp_dir().join(format!("mdtask-t-{}", std::process::id()));
        let child = base.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            base.join("tasks.md"),
            "## base\n\n```sh\ntrue\n```\n\n## shared\n\n```sh\necho parent\n```\n",
        )
        .unwrap();
        std::fs::write(
            child.join("tasks.md"),
            "## shared\n\n```sh\necho child\n```\n\n## only\n\n```sh\ntrue\n```\n",
        )
        .unwrap();

        let files = find_task_files(&child);
        assert_eq!(files.len(), 2, "child and parent files found");
        // Nearest first: child then parent.
        assert!(files[0].0.starts_with(&child));
        assert_eq!(
            files[0].1.task("shared").unwrap().script.trim(),
            "echo child"
        );
        // The parent still supplies `base` as an inherited baseline.
        assert!(files[1].1.task("base").is_some());
        std::fs::remove_dir_all(&base).ok();
    }
}
