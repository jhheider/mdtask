//! `mdtask-core` parses a markdown task file into a typed command tree and builds
//! runnable invocations. It is embeddable, execution-capable, and dependency-free.
//!
//! A task file is ordinary markdown (a `tasks.md`, a `maskfile.md`, or a project
//! `README.md`): a heading is a task, the first fenced code block under it is the
//! script, and `Key: value` lines in the body carry metadata. The format is its
//! own grammar, a graceful superset that borrows xc's metadata vocabulary and
//! mask's runtime shape (per-fence interpreter, positional args). It reads cleanly
//! in those tools where the features overlap, but claims no compatibility.
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
//! assert_eq!(task.args[0].name, "name");
//! ```
//!
//! Parsing is pure. `Task` and `TaskFile` build an [`Invocation`] (program, args,
//! env, cwd) that the caller runs on its own worker or thread, or executes with
//! [`Invocation::run`]. The parser is line-based (no CommonMark dependency), so a
//! `#` or `Key:` inside a fenced block is never mistaken for structure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A parsed task file: the tasks, any file-level environment hoisted to all of
/// them (an `Env:` under a section heading applies to **every** task regardless
/// of where in the document it appears; hoisting is not positional), and any
/// parse warnings (an unterminated fence, a duplicate task, an unknown fence
/// language). Parsing is infallible. A malformed file still yields what it can,
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
    /// The fenced block's info-string language (`sh`, `zsh`, `python`, ...); empty
    /// means an unlabeled fence (treated as `sh`).
    pub lang: String,
    /// The script (the fenced block's contents), verbatim.
    pub script: String,
    /// `Opts:` carries per-task boolean flags, space-separated. The only flag
    /// today is **`inherit-cwd`**: run the task in the directory mdtask was invoked
    /// from, rather than the default (the directory of the task file that defines
    /// it). Use it for a task that operates on a path relative to where you are (a
    /// carry-around task such as `pdf a-note.md`). An unrecognized flag is recorded
    /// as a warning and otherwise ignored. See [`TaskFile::invocation`].
    pub opts: Vec<String>,
    /// `Env:` adds extra environment for this task.
    pub env: Vec<(String, String)>,
    /// `Args:` declares positional arguments in just's syntax. A bare `name` is
    /// required, `name='default'` is optional, and a trailing `*name` is variadic
    /// (it collects the rest, space-joined). Each one is substituted as
    /// `{{ name }}` in the script and also exported as `$name`. Note that
    /// **`{{ name }}` is raw text substitution**, spliced in before the interpreter
    /// parses the script, so `{{ name }}` is NOT injection-safe for untrusted values
    /// in any language. The safe form is to read the value from the environment,
    /// never to template it: `"$name"` in a shell, `os.environ["name"]` in Python,
    /// `process.env.name` in Node, and so on. Reserve `{{ }}` for developer-authored
    /// templates.
    pub args: Vec<Arg>,
    /// `Requires:` names the tasks this one depends on. mdtask-core does not run
    /// them (execution stays the caller's), but [`dependency_order`] resolves the
    /// transitive run order (deps first, cycle and typo detected) so a caller can
    /// run each in turn. The mdtask CLI does exactly that.
    pub requires: Vec<String>,
    /// `Agent: allow` opts a task in to being listed and run by an MCP or agent
    /// surface. It is advisory data: nothing in mdtask-core's execution path
    /// checks it. A caller exposing tasks to an agent must filter with
    /// [`TaskFile::agent_tasks`] (off by default), which is the enforcement point.
    /// The flag alone enforces nothing.
    pub agent_allow: bool,
}

/// One declared positional argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    pub name: String,
    /// `*name`: collects all remaining positionals, space-joined.
    pub variadic: bool,
    /// `name='default'`: optional, with this value when not supplied.
    pub default: Option<String>,
}

/// A runnable command built from a task: what to exec, with what environment, in
/// which directory. The caller runs it however it likes (on a worker, on a
/// thread), keeping execution off any hot path.
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

/// A `Requires:` dependency chain could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepError {
    /// A `Requires:` named a task that does not exist.
    Missing { task: String, required_by: String },
    /// A dependency cycle, reported at the task where the back edge closes.
    Cycle(String),
}

impl std::fmt::Display for DepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DepError::Missing { task, required_by } => {
                write!(f, "task {required_by:?} requires unknown task {task:?}")
            }
            DepError::Cycle(name) => write!(f, "dependency cycle through task {name:?}"),
        }
    }
}
impl std::error::Error for DepError {}

/// Resolve the run order for `target` and its transitive `Requires:`: each
/// dependency comes before the task that needs it, `target` comes last, and every
/// task appears at most once (a diamond runs its shared dependency once). This is
/// the sequencing mdtask-core does not do inside `invocation`; the caller supplies
/// `requires_of`, which returns a task's declared dependency names, or `None` if
/// the name is not a known task (so a typo in `Requires:` is a hard error, not a
/// silent skip). Pure: no filesystem or process access.
///
/// The traversal is iterative (an explicit work stack, not native recursion), so a
/// pathologically deep chain cannot overflow the call stack and abort the process.
pub fn dependency_order(
    target: &str,
    requires_of: impl Fn(&str) -> Option<Vec<String>>,
) -> Result<Vec<String>, DepError> {
    // Each frame is a task whose dependencies we are still walking (`next` is the
    // index of the next dependency to descend into). A post-order DFS: a frame
    // moves to `order` only once all its dependencies are done.
    struct Frame {
        name: String,
        deps: Vec<String>,
        next: usize,
    }

    let mut order = Vec::new();
    let mut done = std::collections::BTreeSet::new();
    let mut on_stack = std::collections::BTreeSet::new();
    let mut stack: Vec<Frame> = Vec::new();

    let deps = requires_of(target).ok_or_else(|| DepError::Missing {
        task: target.to_string(),
        required_by: target.to_string(),
    })?;
    on_stack.insert(target.to_string());
    stack.push(Frame {
        name: target.to_string(),
        deps,
        next: 0,
    });

    loop {
        // Decide the next move using a short-lived borrow of the top frame, so the
        // stack is free to push/pop afterwards.
        let descend = {
            let Some(frame) = stack.last_mut() else { break };
            if frame.next < frame.deps.len() {
                let dep = frame.deps[frame.next].clone();
                frame.next += 1;
                Some(dep)
            } else {
                None
            }
        };
        match descend {
            Some(dep) => {
                if done.contains(&dep) {
                    continue; // already resolved via another path (a diamond)
                }
                if on_stack.contains(&dep) {
                    return Err(DepError::Cycle(dep));
                }
                let required_by = stack.last().expect("a top frame exists").name.clone();
                let deps = requires_of(&dep).ok_or(DepError::Missing {
                    task: dep.clone(),
                    required_by,
                })?;
                on_stack.insert(dep.clone());
                stack.push(Frame {
                    name: dep,
                    deps,
                    next: 0,
                });
            }
            None => {
                let frame = stack.pop().expect("a top frame exists");
                on_stack.remove(&frame.name);
                done.insert(frame.name.clone());
                order.push(frame.name);
            }
        }
    }
    Ok(order)
}

/// The `Opts:` flags mdtask recognizes. An `Opts:` value outside this set is
/// recorded as a warning and otherwise ignored, so a file written for a newer
/// mdtask does not hard-fail on an older one.
pub const KNOWN_OPTS: &[&str] = &["inherit-cwd"];

impl Task {
    /// Whether this task opted into `Opts: inherit-cwd`: run it in the invocation
    /// directory rather than the default (the task file's own directory).
    pub fn inherits_cwd(&self) -> bool {
        self.opts.iter().any(|o| o == "inherit-cwd")
    }

    /// The declared argument names this task interpolates into its **script** via
    /// `{{ arg }}` (raw text substitution, spliced in before the interpreter parses
    /// the script). Because it is not quoted, each of these is an injection point
    /// for an untrusted argument value, in any language. A surface that runs a task
    /// with caller-controlled argument values (an agent/MCP surface) should refuse
    /// a task that has any; the author should read the value from the environment
    /// instead (`"$arg"`, `os.environ["arg"]`, ...). Empty for a task that reads its
    /// args from the environment, the safe form. See [`TaskFile::invocation`].
    pub fn script_arg_templates(&self) -> Vec<&str> {
        let declared: std::collections::BTreeSet<&str> =
            self.args.iter().map(|a| a.name.as_str()).collect();
        let mut found: Vec<&str> = Vec::new();
        let mut rest = self.script.as_str();
        while let Some(open) = rest.find("{{") {
            let after = &rest[open + 2..];
            let Some(close) = after.find("}}") else { break };
            let tok = after[..close].trim();
            if declared.contains(tok) && !found.contains(&tok) {
                found.push(tok);
            }
            rest = &after[close + 2..];
        }
        found
    }
}

impl TaskFile {
    /// Find a task by name. The match is exact and case-sensitive, against the
    /// heading text as written. The first definition wins if a name is duplicated
    /// (a warning is recorded).
    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.name == name)
    }

    /// The tasks that opted in to an agent or MCP surface via `Agent: allow`. A
    /// caller exposing tasks to an agent should list and run **only** these. The
    /// flag is advisory data on `Task`, so this iterator is the enforcement point,
    /// not the field. (Direct field access bypasses the gate by design; the gate
    /// lives at the surface that chooses what to expose.)
    pub fn agent_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| t.agent_allow)
    }

    /// Build the invocation for `task`, given `args` mapping each name to a value
    /// (from [`TaskFile::bind`] or an embedder's prompts). It substitutes
    /// `{{ arg }}` in the script, exports the args and env, and resolves the
    /// working directory:
    ///
    /// - **By default a task runs in `task_file_dir`**, the directory of the file
    ///   that defines it (`None` falls back to `cwd`). A task script is written
    ///   against its project's layout, so it runs from that project's root, the way
    ///   `just` runs a recipe from its justfile's directory. For a task reached by
    ///   the layered tree-walk, that is the directory of the ancestor file that
    ///   defined it, again matching `just`'s fallback.
    /// - **`Opts: inherit-cwd`** runs the task in `cwd`, the invocation directory
    ///   instead, for a carry-around task that operates on a path relative to where
    ///   you are (`just`'s `[no-cd]`). Anything more specific than these two anchors
    ///   is a `cd` in the script.
    ///
    /// Missing optional and variadic args are filled from their defaults, so a
    /// partial `args` map is fine; only a missing *required* arg is an error.
    ///
    /// The call is pure and cheap, with no filesystem or process access, so it is
    /// safe to call straight from a UI event handler or render path. Build the
    /// `Invocation` here and run it elsewhere.
    pub fn invocation(
        &self,
        task: &Task,
        args: &BTreeMap<String, String>,
        cwd: &Path,
        task_file_dir: Option<&Path>,
    ) -> Result<Invocation, MissingArg> {
        // Fill defaults for any declared arg the caller did not supply.
        let mut effective = args.clone();
        for a in &task.args {
            if !effective.contains_key(&a.name) {
                if a.variadic {
                    effective.insert(a.name.clone(), String::new());
                } else if let Some(d) = &a.default {
                    effective.insert(a.name.clone(), d.clone());
                } else {
                    return Err(MissingArg(a.name.clone()));
                }
            }
        }

        let script = substitute(&task.script, &effective);
        let (program, flag) = interpreter(&task.lang);

        // Env precedence: hoisted, then task, then args. Args win, being the most
        // specific, so `$name` resolves to the passed value.
        let mut env = self.env.clone();
        env.extend(task.env.iter().cloned());
        env.extend(effective.iter().map(|(k, v)| (k.clone(), v.clone())));

        // The task's own directory is the default anchor; `inherit-cwd` opts into
        // the invocation directory. An absent or empty task_file_dir (a bare
        // filename with no directory part, e.g. `-f tasks.md`) falls back to cwd,
        // since running in an empty path would fail.
        let run_cwd = match task_file_dir {
            _ if task.inherits_cwd() => cwd.to_path_buf(),
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => cwd.to_path_buf(),
        };

        Ok(Invocation {
            program: program.to_string(),
            args: vec![flag.to_string(), script],
            env,
            cwd: run_cwd,
        })
    }

    /// Bind positional argument values (for example, from the CLI) to a task's
    /// declared `Args:`, applying defaults and collecting a trailing `*variadic`
    /// from the rest. This feeds [`TaskFile::invocation`] and errors on a missing
    /// required arg.
    pub fn bind(
        task: &Task,
        positional: &[String],
    ) -> Result<BTreeMap<String, String>, MissingArg> {
        let mut map = BTreeMap::new();
        let mut i = 0;
        for a in &task.args {
            if a.variadic {
                map.insert(
                    a.name.clone(),
                    positional[i.min(positional.len())..].join(" "),
                );
                i = positional.len();
            } else if i < positional.len() {
                map.insert(a.name.clone(), positional[i].clone());
                i += 1;
            } else if let Some(d) = &a.default {
                map.insert(a.name.clone(), d.clone());
            } else {
                return Err(MissingArg(a.name.clone()));
            }
        }
        Ok(map)
    }
}

impl Invocation {
    /// Execute the invocation and wait for it, inheriting the parent's stdio so
    /// the task's output streams straight through. The CLI wants this, because a
    /// task is an interactive command, not a captured subprocess. An embedder that
    /// must not block a thread, or that wants to capture output, should build the
    /// [`std::process::Command`] from the fields itself.
    pub fn run(&self) -> std::io::Result<std::process::ExitStatus> {
        std::process::Command::new(&self.program)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .status()
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

/// Parse a markdown task file. It is line-based (no CommonMark dependency): a
/// heading starts a task, the first fenced block under it is the script, and
/// `Key: value` lines set metadata. Parsing is infallible; problems are reported
/// in [`TaskFile::warnings`] rather than dropped to silence. CRLF endings are
/// normalized.
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
        apply_line(line, cur.as_mut(), &mut file.env, &mut file.warnings);
    }
    // An unterminated fence at EOF: still capture the script so the task is not
    // lost, but warn, since a forgotten closing fence is a common authoring slip.
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
        file.env.append(&mut t.env); // section heading, so hoist its env
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

/// Whether `line` is a bare closing fence for `marker`: only the fence char, no
/// info string, per CommonMark's closing rule.
fn is_closing_fence(line: &str, marker: &str) -> bool {
    let ch = marker.as_bytes()[0];
    let t = line.trim();
    t.len() >= 3 && t.bytes().all(|b| b == ch)
}

/// Search for task files from `start` up to the filesystem root, **nearest
/// first**. In each ancestor directory the first of `tasks.md`, `maskfile.md`,
/// `README.md` that parses to at least one task is taken. The CLI layers these
/// child-first, so a nearer file shadows a farther one by task name (like just's
/// `set fallback`, letting a project inherit a baseline of tasks from a parent).
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
fn apply_line(
    line: &str,
    task: Option<&mut Task>,
    file_env: &mut Vec<(String, String)>,
    warnings: &mut Vec<String>,
) {
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
            "opts" | "options" => {
                if let Some(t) = task {
                    t.opts = value.split_whitespace().map(str::to_string).collect();
                    for flag in &t.opts {
                        if !KNOWN_OPTS.contains(&flag.as_str()) {
                            warnings.push(format!(
                                "unknown option {flag:?} in `Opts:` (known: {})",
                                KNOWN_OPTS.join(", ")
                            ));
                        }
                    }
                }
                return;
            }
            "args" | "arguments" => {
                if let Some(t) = task {
                    t.args = parse_args(value);
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
/// (a single-word key before the first colon). Leading indentation is allowed, so
/// an `Env:` indented under a list still counts. This is safe because only *known*
/// keys act (see `apply_line`), so ordinary prose with a colon stays description.
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

/// Parse an `Args:` value into declared [`Arg`]s (just's syntax): `name` is
/// required, `*name` collects the rest (variadic), `name='default'` (or
/// `name="default"`) is optional. Tokens are whitespace-separated, but a quoted
/// default may itself contain spaces (`msg='hello world'`).
fn parse_args(value: &str) -> Vec<Arg> {
    tokenize_args(value)
        .into_iter()
        .filter_map(|tok| {
            let (name, default) = match tok.split_once('=') {
                Some((n, d)) => (n, Some(unquote(d).to_string())),
                None => (tok.as_str(), None),
            };
            let (name, variadic) = match name.strip_prefix('*') {
                Some(rest) => (rest, true),
                None => (name, false),
            };
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(Arg {
                name: name.to_string(),
                variadic,
                default,
            })
        })
        .collect()
}

/// Split an `Args:` value on whitespace, but keep a single- or double-quoted run
/// (a default value) together so `msg='a b'` is one token.
fn tokenize_args(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in value.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c == '\'' || c == '"' => {
                cur.push(c);
                quote = Some(c);
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Strip one matching pair of surrounding single or double quotes, if present.
fn unquote(s: &str) -> &str {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
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
    fn metadata_keys_are_case_insensitive() {
        let tf = parse(
            "## deploy\n\nOPTS: inherit-cwd\nEnv: REGION=us, TIER=prod\nArgs: target\nRequires: build, test\nAgent: allow\n\n```sh\necho go\n```\n",
        );
        let t = &tf.tasks[0];
        assert_eq!(t.opts, vec!["inherit-cwd"]);
        assert!(t.inherits_cwd());
        assert_eq!(
            t.env,
            vec![
                ("REGION".into(), "us".into()),
                ("TIER".into(), "prod".into())
            ]
        );
        assert_eq!(
            t.args.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["target"]
        );
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
        let tf = parse("## a\n\n```sh\n## not a task\nEnv: NOPE=1\n```\n");
        assert_eq!(tf.tasks.len(), 1);
        assert!(tf.tasks[0].script.contains("## not a task"));
        assert!(tf.tasks[0].env.is_empty());
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
            .invocation(
                t,
                &args(&[("name", "sam")]),
                Path::new("/here"),
                Some(Path::new("/file")),
            )
            .unwrap();
        assert_eq!(inv.program, "zsh");
        assert_eq!(inv.args[0], "-c");
        assert!(inv.args[1].contains("hi sam"));
        assert!(inv.env.contains(&("name".to_string(), "sam".to_string())));
        // By default it runs in the task file's directory, not where invoked.
        assert_eq!(inv.cwd, Path::new("/file"));
    }

    #[test]
    fn a_missing_required_arg_is_an_error() {
        let tf = parse("## t\n\nArgs: file\n\n```sh\ncat {{ file }}\n```\n");
        let t = tf.task("t").unwrap();
        assert_eq!(
            tf.invocation(t, &args(&[]), Path::new("/here"), None),
            Err(MissingArg("file".into()))
        );
    }

    #[test]
    fn optional_and_variadic_args_fill_from_defaults() {
        let tf = parse(
            "## t\n\nArgs: a b='fallback' *rest\n\n```sh\necho {{ a }} {{ b }} {{ rest }}\n```\n",
        );
        let t = tf.task("t").unwrap();
        assert!(!t.args[0].variadic && t.args[0].default.is_none());
        assert_eq!(t.args[1].default.as_deref(), Some("fallback"));
        assert!(t.args[2].variadic);
        // Only `a` supplied: `b` uses its default, `rest` is empty.
        let inv = tf
            .invocation(t, &args(&[("a", "x")]), Path::new("/here"), None)
            .unwrap();
        assert!(inv.args[1].contains("echo x fallback "));
        // bind() collects a trailing variadic from the leftover positionals.
        let bound =
            TaskFile::bind(t, &["x".into(), "y".into(), "one".into(), "two".into()]).unwrap();
        assert_eq!(bound.get("b").map(String::as_str), Some("y"));
        assert_eq!(bound.get("rest").map(String::as_str), Some("one two"));
    }

    #[test]
    fn default_cwd_is_the_task_file_dir() {
        let tf = parse("## t\n\n```sh\ntrue\n```\n");
        let t = tf.task("t").unwrap();
        // Default: the file's directory, not where invoked.
        let inv = tf
            .invocation(t, &args(&[]), Path::new("/here"), Some(Path::new("/proj")))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/proj"));
        // With no task_file_dir known (headless), it falls back to cwd.
        let inv = tf
            .invocation(t, &args(&[]), Path::new("/here"), None)
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/here"));
        // An empty task_file_dir (a bare filename's parent) also falls back to cwd,
        // since running in an empty path would fail.
        let inv = tf
            .invocation(t, &args(&[]), Path::new("/here"), Some(Path::new("")))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/here"));
    }

    #[test]
    fn inherit_cwd_runs_in_the_invocation_dir() {
        let tf = parse("## t\n\nOpts: inherit-cwd\n\n```sh\ntrue\n```\n");
        let t = tf.task("t").unwrap();
        assert!(t.inherits_cwd());
        let inv = tf
            .invocation(t, &args(&[]), Path::new("/here"), Some(Path::new("/proj")))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/here"));
    }

    #[test]
    fn an_unknown_opt_warns_but_is_ignored() {
        let tf = parse("## t\n\nOpts: inherit-cwd bogus\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.tasks[0].opts, vec!["inherit-cwd", "bogus"]);
        assert!(tf.tasks[0].inherits_cwd()); // the known flag still applies
        assert!(tf.warnings.iter().any(|w| w.contains("bogus")));
    }

    // A `requires_of` for tests: a map from task name to its dependency names.
    fn deps_of<'a>(map: &'a [(&str, &[&str])]) -> impl Fn(&str) -> Option<Vec<String>> + 'a {
        move |name| {
            map.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, ds)| ds.iter().map(|s| s.to_string()).collect())
        }
    }

    #[test]
    fn dependency_order_is_deps_first_target_last() {
        // a -> b -> c, plus a -> c: c runs once, before b, and a is last.
        let g = deps_of(&[("a", &["b", "c"]), ("b", &["c"]), ("c", &[])]);
        assert_eq!(dependency_order("a", g).unwrap(), ["c", "b", "a"]);
    }

    #[test]
    fn dependency_order_dedupes_a_diamond() {
        let g = deps_of(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"]), ("d", &[])]);
        let order = dependency_order("a", g).unwrap();
        assert_eq!(order.iter().filter(|n| *n == "d").count(), 1);
        // d before b and c; a last.
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(pos("d") < pos("b") && pos("d") < pos("c"));
        assert_eq!(order.last().unwrap(), "a");
    }

    #[test]
    fn dependency_order_detects_a_cycle() {
        let g = deps_of(&[("a", &["b"]), ("b", &["a"])]);
        assert_eq!(dependency_order("a", g), Err(DepError::Cycle("a".into())));
    }

    #[test]
    fn dependency_order_flags_a_missing_dependency() {
        let g = deps_of(&[("a", &["ghost"])]);
        assert_eq!(
            dependency_order("a", g),
            Err(DepError::Missing {
                task: "ghost".into(),
                required_by: "a".into(),
            })
        );
    }

    #[test]
    fn dependency_order_survives_a_pathologically_deep_chain() {
        // t0 -> t1 -> ... -> tN. Native recursion overflowed the stack here; the
        // iterative walk must return a full, correctly ordered chain instead.
        const N: usize = 200_000;
        let order = dependency_order("t0", |n| {
            let i: usize = n.strip_prefix('t')?.parse().ok()?;
            Some(if i + 1 < N {
                vec![format!("t{}", i + 1)]
            } else {
                vec![]
            })
        })
        .unwrap();
        assert_eq!(order.len(), N);
        assert_eq!(order.first().unwrap(), &format!("t{}", N - 1)); // deepest runs first
        assert_eq!(order.last().unwrap(), "t0"); // target runs last
    }

    #[test]
    fn script_arg_templates_flags_only_declared_args_in_the_script() {
        // `name` is interpolated raw via {{ name }} (injectable); `safe` uses $safe.
        let tf =
            parse("## t\n\nArgs: name safe\n\n```sh\necho {{ name }} \"$safe\" {{ other }}\n```\n");
        let t = tf.task("t").unwrap();
        assert_eq!(t.script_arg_templates(), vec!["name"]);

        // A task that only uses $arg has no raw template interpolation.
        let safe = parse("## t\n\nArgs: name\n\n```sh\necho \"$name\"\n```\n");
        assert!(safe.task("t").unwrap().script_arg_templates().is_empty());
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
        // ```sh has an info string, so it opens rather than closes; only a bare
        // ``` closes. (The trailing block here is what closes it.)
        let tf = parse("## a\n\n```sh\none\n```sh\ntwo\n```\n");
        assert!(tf.tasks[0].script.contains("one"));
        assert!(tf.tasks[0].script.contains("```sh\ntwo"));
    }

    #[test]
    fn indented_metadata_is_recognized() {
        let tf = parse("## a\n\n- steps:\n  Env: KEY=val\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.tasks[0].env, vec![("KEY".into(), "val".into())]);
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
