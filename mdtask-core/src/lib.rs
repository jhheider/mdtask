//! `mdtask-core` parses a markdown task file into a typed job tree and runs jobs
//! from it. It is embeddable, execution-capable, and dependency-free.
//!
//! A task file is ordinary markdown (a `tasks.md`, a `maskfile.md`, or a project
//! `README.md`): a heading is a job, the first fenced code block under it is the
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
//! let job = tf.job("greet").unwrap();
//! assert_eq!(job.args[0].name, "name");
//! ```
//!
//! Parsing is pure. A consumer sees only jobs and their metadata: interpreter
//! selection, argv building, working-directory resolution, and spawning are all
//! internal. Three entry points run a job and its `Requires:` chain: [`run`]
//! inherits stdio (streaming, for a CLI), [`run_captured`] captures the aggregated
//! output (for an embedder), and [`run_agent`] adds the agent allow gate and the
//! injection guard (for an MCP or agent surface). The parser is line-based (no
//! CommonMark dependency), so a `#` or `Key:` inside a fenced block is never
//! mistaken for structure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A parsed task file: the jobs, any file-level environment hoisted to all of
/// them (an `Env:` under a section heading applies to **every** job regardless of
/// where in the document it appears; hoisting is not positional), and any parse
/// warnings (an unterminated fence, a duplicate job, an unknown fence language).
/// Parsing is infallible. A malformed file still yields what it can, so an
/// embedder should surface `warnings()` rather than trust silence. The internal
/// fields carry execution mechanics; a consumer reaches jobs through [`jobs`] and
/// [`job`], and runs them through [`run`], [`run_captured`], or [`run_agent`].
///
/// [`jobs`]: TaskFile::jobs
/// [`job`]: TaskFile::job
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFile {
    pub(crate) env: Vec<(String, String)>,
    pub(crate) jobs: Vec<Job>,
    pub(crate) warnings: Vec<String>,
}

/// One job: a named script with its metadata. The script, its interpreter
/// language, its `Opts:` flags, and its extra environment are internal mechanics;
/// a consumer deals in the name, description, declared args, dependencies, and the
/// agent gate, and runs the job through [`run`], [`run_captured`], or
/// [`run_agent`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Job {
    /// The heading text (the job name).
    pub name: String,
    /// Prose in the job body that is not a recognized `Key: value` line.
    pub description: String,
    /// `Args:` declares positional arguments in just's syntax. A bare `name` is
    /// required, `name='default'` is optional, and a trailing `*name` is variadic
    /// (it collects the rest, space-joined). Each one is substituted as
    /// `{{ name }}` in the script and also exported as `$name`. Note that
    /// **`{{ name }}` is raw text substitution**, spliced in before the interpreter
    /// parses the script, so `{{ name }}` is NOT injection-safe for untrusted values
    /// in any language. The safe form is to read the value from the environment,
    /// never to template it: `"$name"` in a shell, `os.environ["name"]` in Python,
    /// `process.env.name` in Node, and so on. Reserve `{{ }}` for developer-authored
    /// templates. An agent-run job that raw-templates an arg is refused by
    /// [`run_agent`].
    pub args: Vec<Arg>,
    /// `Requires:` names the jobs this one depends on. The `run*` entry points
    /// resolve the transitive order (deps first, cycle and typo detected) and run
    /// each in turn, stopping on the first non-success step.
    pub requires: Vec<String>,
    /// `Agent: allow` opts a job in to being listed and run by an MCP or agent
    /// surface. The flag alone enforces nothing: [`run_agent`] is the gate that
    /// checks it (and [`agent_jobs`] the listing that filters on it), so a plain
    /// [`run`] or [`run_captured`] ignores it. It stays public as advisory data an
    /// embedder can read.
    pub agent_allow: bool,
    /// The fenced block's info-string language (`sh`, `zsh`, `python`, ...); empty
    /// means an unlabeled fence (treated as `sh`).
    pub(crate) lang: String,
    /// The script (the fenced block's contents), verbatim.
    pub(crate) script: String,
    /// `Opts:` carries per-job boolean flags, space-separated. The only flag today
    /// is `inherit-cwd`: run the job in the directory mdtask was invoked from,
    /// rather than the default (the directory of the task file that defines it).
    pub(crate) opts: Vec<String>,
    /// `Env:` adds extra environment for this job.
    pub(crate) env: Vec<(String, String)>,
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

/// A runnable command built from a job: what to exec, with what environment, in
/// which directory. Internal mechanics: the `run*` functions build it and spawn
/// it, and no consumer ever sees the program, argv, or interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// A declared argument had no value supplied when binding a job's args.
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
    /// A `Requires:` named a job that does not exist.
    Missing { task: String, required_by: String },
    /// A dependency cycle, reported at the job where the back edge closes.
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

/// Why a `run*` call could not complete. It reports the failure to resolve or
/// dispatch a job; a job that runs to a non-zero exit is not an error here (the
/// exit status rides back in the `Ok`). Only `Debug` is derived, because `Io`
/// wraps a [`std::io::Error`], which is neither `Clone` nor `PartialEq`.
#[derive(Debug)]
pub enum RunError {
    /// No job by that name across the resolved files (from [`run`]/[`run_captured`]).
    NotFound(String),
    /// The nearest definition of the named job is not `Agent: allow`, so an agent
    /// surface may not run it (from [`run_agent`] only). A nearer non-allowed
    /// definition shadowing a farther allowed one lands here too: fail closed.
    NotAllowed(String),
    /// The agent target raw-templates a declared arg into its script via
    /// `{{ arg }}` (from [`run_agent`] only). `args` lists the offending names.
    /// The job must read the value from the environment instead before an agent
    /// may run it.
    Injects { task: String, args: Vec<String> },
    /// A required positional argument had no value.
    MissingArg(MissingArg),
    /// The `Requires:` chain could not be resolved (a typo or a cycle).
    Dependency(DepError),
    /// Spawning a step failed (the interpreter is missing, the directory is gone).
    Io(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::NotFound(name) => write!(f, "no task named {name:?}"),
            RunError::NotAllowed(name) => write!(
                f,
                "task {name:?} is not available to agents (it lacks `Agent: allow`)"
            ),
            RunError::Injects { task, args } => write!(
                f,
                "task {task:?} interpolates argument(s) [{}] into its script via {{{{ }}}} \
                 (raw substitution, an injection risk with agent-supplied values); it must \
                 read them from the environment instead (\"$arg\", os.environ[\"arg\"], ...) \
                 before an agent can run it. Refused.",
                args.join(", ")
            ),
            RunError::MissingArg(e) => e.fmt(f),
            RunError::Dependency(e) => e.fmt(f),
            RunError::Io(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::MissingArg(e) => Some(e),
            RunError::Dependency(e) => Some(e),
            RunError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Resolve the run order for `target` and its transitive `Requires:`: each
/// dependency comes before the job that needs it, `target` comes last, and every
/// job appears at most once (a diamond runs its shared dependency once). The
/// caller supplies `requires_of`, which returns a job's declared dependency names,
/// or `None` if the name is not a known job (so a typo in `Requires:` is a hard
/// error, not a silent skip). Pure: no filesystem or process access.
///
/// The traversal is iterative (an explicit work stack, not native recursion), so a
/// pathologically deep chain cannot overflow the call stack and abort the process.
pub(crate) fn dependency_order(
    target: &str,
    requires_of: impl Fn(&str) -> Option<Vec<String>>,
) -> Result<Vec<String>, DepError> {
    // Each frame is a job whose dependencies we are still walking (`next` is the
    // index of the next dependency to descend into). A post-order DFS: a frame
    // moves to `order` only once all its dependencies are done.
    struct Frame {
        name: String,
        deps: Vec<String>,
        next: usize,
    }

    let mut order = Vec::new();
    let mut done = BTreeSet::new();
    let mut on_stack = BTreeSet::new();
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
pub(crate) const KNOWN_OPTS: &[&str] = &["inherit-cwd", "no-strict"];

impl Job {
    /// Whether this job opted into `Opts: inherit-cwd`: run it in the invocation
    /// directory rather than the default (the task file's own directory).
    pub(crate) fn inherits_cwd(&self) -> bool {
        self.opts.iter().any(|o| o == "inherit-cwd")
    }

    /// Whether shell strictness applies. On unless `Opts: no-strict`.
    ///
    /// Strict is the default because the failure modes are asymmetric. A strict
    /// default fails loudly when an author did not expect it, and they add
    /// `no-strict`. A lenient default fails SILENTLY: a shell runs the whole
    /// fenced block as one script, so an early failure is swallowed and the task
    /// exits with the status of the last command. That turns a multi-step gate
    /// into one that cannot fail, and it will report success while `cargo fmt`
    /// is failing inside it.
    ///
    /// The other evidence is that authors were already writing the prelude by
    /// hand: every multi-step task in mdtask's own dogfood repos opened with
    /// `set -euo pipefail`. When everyone writes the same first line, it belongs
    /// in the tool.
    pub(crate) fn is_strict(&self) -> bool {
        !self.opts.iter().any(|o| o == "no-strict")
    }

    /// The declared argument names this job interpolates into its **script** via
    /// `{{ arg }}` (raw text substitution, spliced in before the interpreter parses
    /// the script). Because it is not quoted, each of these is an injection point
    /// for an untrusted argument value, in any language, so [`run_agent`] refuses a
    /// job that has any. Empty for a job that reads its args from the environment,
    /// the safe form.
    pub(crate) fn script_arg_templates(&self) -> Vec<&str> {
        let declared: BTreeSet<&str> = self.args.iter().map(|a| a.name.as_str()).collect();
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
    /// The jobs in this file, in document order.
    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    /// Find a job by name. The match is exact and case-sensitive, against the
    /// heading text as written. The first definition wins if a name is duplicated
    /// (a warning is recorded).
    pub fn job(&self, name: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.name == name)
    }

    /// Any parse warnings (an unterminated fence, a duplicate job, an unknown fence
    /// language). Parsing is infallible, so surface these rather than trust silence.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Build the invocation for `job`, given `args` mapping each name to a value.
    /// It substitutes `{{ arg }}` in the script, exports the args and env, and
    /// resolves the working directory: by default the job runs in `job_file_dir`
    /// (the directory of the file that defines it; `None` or empty falls back to
    /// `cwd`), while `Opts: inherit-cwd` runs it in `cwd`. Missing optional and
    /// variadic args are filled from their defaults; only a missing required arg is
    /// an error.
    pub(crate) fn invocation(
        &self,
        job: &Job,
        args: &BTreeMap<String, String>,
        cwd: &Path,
        job_file_dir: Option<&Path>,
    ) -> Result<Invocation, MissingArg> {
        // Fill defaults for any declared arg the caller did not supply.
        let mut effective = args.clone();
        for a in &job.args {
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

        let script = substitute(&job.script, &effective);
        let (program, flag) = interpreter(&job.lang);
        let script = match strict_prelude(&job.lang) {
            Some(prelude) if job.is_strict() => format!("{prelude}\n{script}"),
            _ => script,
        };

        // Env precedence: hoisted, then job, then args. Args win, being the most
        // specific, so `$name` resolves to the passed value.
        let mut env = self.env.clone();
        env.extend(job.env.iter().cloned());
        env.extend(effective.iter().map(|(k, v)| (k.clone(), v.clone())));

        // The job's own directory is the default anchor; `inherit-cwd` opts into
        // the invocation directory. An absent or empty job_file_dir (a bare
        // filename with no directory part) falls back to cwd, since running in an
        // empty path would fail.
        let run_cwd = match job_file_dir {
            _ if job.inherits_cwd() => cwd.to_path_buf(),
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

    /// Bind positional argument values to a job's declared `Args:`, applying
    /// defaults and collecting a trailing `*variadic` from the rest. This feeds
    /// [`TaskFile::invocation`] and errors on a missing required arg.
    pub(crate) fn bind(
        job: &Job,
        positional: &[String],
    ) -> Result<BTreeMap<String, String>, MissingArg> {
        let mut map = BTreeMap::new();
        let mut i = 0;
        for a in &job.args {
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

/// The jobs a set of layered files exposes to an agent or MCP surface: one per
/// name using the **nearest** definition (so a nearer non-allowed job shadows a
/// farther allowed one, matching run semantics), keeping only those whose nearest
/// definition carries `Agent: allow`. This is the enforcement point for listing;
/// [`run_agent`] is the enforcement point for running. A surface exposing jobs to
/// an agent should list only these.
pub fn agent_jobs(files: &[(PathBuf, TaskFile)]) -> Vec<&Job> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (_, tf) in files {
        for job in &tf.jobs {
            if seen.insert(job.name.clone()) && job.agent_allow {
                out.push(job);
            }
        }
    }
    out
}

/// The nearest definition of `name` across the layered files, plus the file that
/// owns it and that file's directory (`None` when the path has no directory part).
/// A nearer definition wins (the fallback layering), so this resolves both a
/// target and each `Requires:` dependency the same way the CLI does.
fn trusted_lookup<'a>(
    files: &'a [(PathBuf, TaskFile)],
    name: &str,
) -> Option<(&'a TaskFile, &'a Job, Option<&'a Path>)> {
    files
        .iter()
        .find_map(|(p, tf)| tf.job(name).map(|j| (tf, j, p.parent())))
}

/// Resolve `target` and its `Requires:` chain across the layered files (deps
/// first, target last, each once). A name that resolves nowhere is `NotFound`.
fn trusted_order(files: &[(PathBuf, TaskFile)], target: &str) -> Result<Vec<String>, RunError> {
    if trusted_lookup(files, target).is_none() {
        return Err(RunError::NotFound(target.to_string()));
    }
    dependency_order(target, |n| {
        trusted_lookup(files, n).map(|(_, j, _)| j.requires.clone())
    })
    .map_err(RunError::Dependency)
}

/// Build the ordered, ready-to-spawn invocations for `order`. `lookup` resolves
/// each step to its file, job, and directory; only `target` receives `args`,
/// while every dependency runs argless (its own defaults fill in).
fn plan_invocations<'a>(
    order: &[String],
    target: &str,
    args: &[String],
    cwd: &Path,
    lookup: impl Fn(&str) -> Option<(&'a TaskFile, &'a Job, Option<&'a Path>)>,
) -> Result<Vec<Invocation>, RunError> {
    let mut plan = Vec::with_capacity(order.len());
    for step in order {
        let (tf, job, dir) = lookup(step).expect("a resolved name still resolves");
        let step_args: &[String] = if step == target { args } else { &[] };
        let values = TaskFile::bind(job, step_args).map_err(RunError::MissingArg)?;
        let inv = tf
            .invocation(job, &values, cwd, dir)
            .map_err(RunError::MissingArg)?;
        plan.push(inv);
    }
    Ok(plan)
}

/// Run a planned chain with captured output, aggregating stdout and stderr across
/// steps and stopping on the first non-success step. The returned status is that
/// step's (or the last step's on full success). The plan always holds the target,
/// so it is never empty.
fn run_plan_captured(plan: &[Invocation]) -> Result<std::process::Output, RunError> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    for inv in plan {
        let out = inv.run_captured().map_err(RunError::Io)?;
        stdout.extend_from_slice(&out.stdout);
        stderr.extend_from_slice(&out.stderr);
        let failed = !out.status.success();
        status = Some(out.status);
        if failed {
            break;
        }
    }
    Ok(std::process::Output {
        status: status.expect("the plan always contains the target"),
        stdout,
        stderr,
    })
}

/// Run `name` and its `Requires:` chain across the layered `files`, inheriting the
/// parent's stdio so output streams straight through (the CLI path: a job is an
/// interactive command, not a captured subprocess). Dependencies run first, each
/// once, and each with its own defaults; only `name` receives `args`. Returns the
/// first failing step's exit status, or the last step's on full success. This is
/// **trusted**: it applies no agent gate, so a caller must not hand it a name from
/// an untrusted source.
pub fn run(
    files: &[(PathBuf, TaskFile)],
    name: &str,
    args: &[String],
    cwd: &Path,
) -> Result<std::process::ExitStatus, RunError> {
    let order = trusted_order(files, name)?;
    let plan = plan_invocations(&order, name, args, cwd, |n| trusted_lookup(files, n))?;
    let mut last = None;
    for inv in &plan {
        let status = inv.run_inherit().map_err(RunError::Io)?;
        if !status.success() {
            return Ok(status);
        }
        last = Some(status);
    }
    Ok(last.expect("the plan always contains the target"))
}

/// Like [`run`], but captured: run `name` and its `Requires:` chain across the
/// layered `files` with output aggregated across steps into a single
/// [`std::process::Output`] (its status is the failing step's, or the last on
/// success). For an embedder (a TUI, an editor) that wants the text rather than a
/// stream. Also **trusted**: no agent gate.
pub fn run_captured(
    files: &[(PathBuf, TaskFile)],
    name: &str,
    args: &[String],
    cwd: &Path,
) -> Result<std::process::Output, RunError> {
    let order = trusted_order(files, name)?;
    let plan = plan_invocations(&order, name, args, cwd, |n| trusted_lookup(files, n))?;
    run_plan_captured(&plan)
}

/// The agent gate: run `name` for an MCP or agent surface, captured, failing
/// closed. Enforced, in order:
///
/// - The **nearest** definition of `name` across the layered files must carry
///   `Agent: allow`, else [`RunError::NotAllowed`]. A nearer non-allowed
///   definition shadows a farther allowed one (still `NotAllowed`), and a name
///   that resolves nowhere is `NotAllowed` too: the agent never learns whether a
///   hidden job exists.
/// - The target must not raw-template a declared arg into its script via
///   `{{ arg }}` (the agent controls the value), else [`RunError::Injects`]. The
///   author must read the value from the environment instead.
/// - The `Requires:` chain is resolved **within the target's own file**, not by
///   the cross-file nearest-wins scan [`run`] uses. The author who wrote
///   `Agent: allow` vouched for their file's jobs; a nearer, untrusted task file in
///   the invocation directory must not be able to shadow a dependency and run
///   attacker-controlled code through an allowed entry point. A dependency is never
///   independently callable and never listed.
///
/// Only the target receives `args`; dependencies run argless with author-controlled
/// defaults, so the target is the sole injection surface.
pub fn run_agent(
    files: &[(PathBuf, TaskFile)],
    name: &str,
    args: &[String],
    cwd: &Path,
) -> Result<std::process::Output, RunError> {
    // The nearest definition wins. If it is not allowed (or the name resolves
    // nowhere), refuse: fail closed.
    let mut target: Option<(&Path, &TaskFile, &Job)> = None;
    for (p, tf) in files {
        if let Some(job) = tf.job(name) {
            if job.agent_allow {
                target = Some((p.as_path(), tf, job));
            }
            break; // the nearest definition decides, allowed or not
        }
    }
    let Some((target_path, target_tf, target_job)) = target else {
        return Err(RunError::NotAllowed(name.to_string()));
    };

    // Refuse a target that raw-templates an untrusted arg into its script.
    let templated = target_job.script_arg_templates();
    if !templated.is_empty() {
        return Err(RunError::Injects {
            task: name.to_string(),
            args: templated.iter().map(|s| s.to_string()).collect(),
        });
    }

    // Resolve the Requires: chain WITHIN the target's own file (the security
    // boundary), not the cross-file scan.
    let order = dependency_order(name, |n| target_tf.job(n).map(|j| j.requires.clone()))
        .map_err(RunError::Dependency)?;
    let dir = target_path.parent();
    let plan = plan_invocations(&order, name, args, cwd, |n| {
        target_tf.job(n).map(|j| (target_tf, j, dir))
    })?;
    run_plan_captured(&plan)
}

impl Invocation {
    /// The `std::process::Command` for this invocation (program, argv, env, cwd).
    fn command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd);
        cmd
    }

    /// Run inheriting the parent's stdio so output streams straight through.
    fn run_inherit(&self) -> std::io::Result<std::process::ExitStatus> {
        self.command().status()
    }

    /// Run capturing stdout and stderr.
    fn run_captured(&self) -> std::io::Result<std::process::Output> {
        self.command().output()
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

/// The strictness prelude for a language, if it has one.
///
/// Only shells, and only the settings that are about *detecting failure*, which
/// is the task runner's job:
///
/// - `set -e` stops at the first failing command, so a gate cannot pass while a
///   step inside it fails.
/// - `pipefail` extends that through a pipeline, where the exit status would
///   otherwise be the last stage's and a failing producer would go unnoticed.
///
/// Deliberately NOT `set -u`. Catching an unset variable is a lint rather than
/// failure detection, and it changes the meaning of correct scripts: reading an
/// optional variable is ordinary in a task file, and defaulting it to a hard
/// error would break working tasks to catch a typo. Authors who want it can
/// still write it themselves.
///
/// `pipefail` is not POSIX, so plain `sh` gets only `set -e`: dash rejects
/// `set -o pipefail` outright, which would break every task on a Debian-ish
/// `/bin/sh`. `fish` gets nothing, having neither the syntax nor the semantics,
/// and non-shells are left alone entirely.
fn strict_prelude(lang: &str) -> Option<&'static str> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "" | "sh" | "shell" => Some("set -e"),
        "bash" | "zsh" => Some("set -e\nset -o pipefail"),
        _ => None,
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
/// heading starts a job, the first fenced block under it is the script, and
/// `Key: value` lines set metadata. Parsing is infallible; problems are reported
/// in [`TaskFile::warnings`] rather than dropped to silence. CRLF endings are
/// normalized.
pub fn parse(src: &str) -> TaskFile {
    let mut file = TaskFile::default();
    let mut cur: Option<Job> = None;
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut have_script = false; // first fence per job only
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
            cur = Some(Job {
                name,
                ..Job::default()
            });
            have_script = false;
            continue;
        }
        apply_line(line, cur.as_mut(), &mut file.env, &mut file.warnings);
    }
    // An unterminated fence at EOF: still capture the script so the job is not
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

/// Finalize a heading into the file. A heading with a script is a job; one without
/// (a `# Tasks` section) is not, but its `Env:` hoists to all jobs. Records
/// warnings for a duplicate name or an unknown fence language.
fn finalize(job: Option<Job>, file: &mut TaskFile) {
    let Some(mut t) = job else {
        return;
    };
    if t.script.is_empty() {
        file.env.append(&mut t.env); // section heading, so hoist its env
        return;
    }
    t.description = t.description.trim().to_string();
    if file.jobs.iter().any(|x| x.name == t.name) {
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
    file.jobs.push(t);
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
/// `README.md` that parses to at least one job is taken. The CLI layers these
/// child-first, so a nearer file shadows a farther one by job name (like just's
/// `set fallback`, letting a project inherit a baseline of jobs from a parent).
/// Embedders with their own project root can ignore this and call [`parse`].
pub fn find_task_files(start: &Path) -> Vec<(PathBuf, TaskFile)> {
    let mut found = Vec::new();
    for dir in start.ancestors() {
        for name in ["tasks.md", "maskfile.md", "README.md"] {
            let path = dir.join(name);
            if let Ok(src) = std::fs::read_to_string(&path) {
                let tf = parse(&src);
                if !tf.jobs.is_empty() {
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
/// key, xc vocabulary); anything else is description. `Env:` before the first job
/// accumulates into the hoisted `file_env`.
fn apply_line(
    line: &str,
    job: Option<&mut Job>,
    file_env: &mut Vec<(String, String)>,
    warnings: &mut Vec<String>,
) {
    if let Some((key, value)) = split_key(line) {
        let value = value.trim();
        match key.as_str() {
            "env" | "environment" => {
                let pairs = parse_env(value);
                match job {
                    Some(t) => t.env.extend(pairs),
                    None => file_env.extend(pairs), // hoisted
                }
                return;
            }
            "opts" | "options" => {
                if let Some(t) = job {
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
                if let Some(t) = job {
                    t.args = parse_args(value);
                }
                return;
            }
            "requires" | "req" => {
                if let Some(t) = job {
                    t.requires = value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                return;
            }
            "agent" => {
                if let Some(t) = job {
                    t.agent_allow = value.eq_ignore_ascii_case("allow");
                }
                return;
            }
            _ => {}
        }
    }
    // Description (only within a job; drop stray prose outside one).
    if let Some(t) = job
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

    fn files(pairs: &[(&str, &str)]) -> Vec<(PathBuf, TaskFile)> {
        pairs
            .iter()
            .map(|(path, src)| (PathBuf::from(path), parse(src)))
            .collect()
    }

    #[test]
    fn parses_named_jobs_with_interpreter() {
        let tf =
            parse("## build\n\n```sh\ncargo build\n```\n\n## check\n\n```zsh\nprint hi\n```\n");
        assert_eq!(tf.jobs.len(), 2);
        assert_eq!(tf.jobs[0].name, "build");
        assert_eq!(tf.jobs[0].lang, "sh");
        assert_eq!(tf.jobs[0].script.trim(), "cargo build");
        assert_eq!(tf.jobs[1].lang, "zsh");
    }

    #[test]
    fn metadata_keys_are_case_insensitive() {
        let tf = parse(
            "## deploy\n\nOPTS: inherit-cwd\nEnv: REGION=us, TIER=prod\nArgs: target\nRequires: build, test\nAgent: allow\n\n```sh\necho go\n```\n",
        );
        let t = &tf.jobs[0];
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
        assert!(!tf.jobs[0].agent_allow);
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
        assert_eq!(tf.jobs.len(), 1);
        assert!(tf.jobs[0].script.contains("## not a task"));
        assert!(tf.jobs[0].env.is_empty());
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
        let j = tf.job("greet").unwrap();
        let inv = tf
            .invocation(
                j,
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
        let j = tf.job("t").unwrap();
        assert_eq!(
            tf.invocation(j, &args(&[]), Path::new("/here"), None),
            Err(MissingArg("file".into()))
        );
    }

    #[test]
    fn optional_and_variadic_args_fill_from_defaults() {
        let tf = parse(
            "## t\n\nArgs: a b='fallback' *rest\n\n```sh\necho {{ a }} {{ b }} {{ rest }}\n```\n",
        );
        let j = tf.job("t").unwrap();
        assert!(!j.args[0].variadic && j.args[0].default.is_none());
        assert_eq!(j.args[1].default.as_deref(), Some("fallback"));
        assert!(j.args[2].variadic);
        // Only `a` supplied: `b` uses its default, `rest` is empty.
        let inv = tf
            .invocation(j, &args(&[("a", "x")]), Path::new("/here"), None)
            .unwrap();
        assert!(inv.args[1].contains("echo x fallback "));
        // bind() collects a trailing variadic from the leftover positionals.
        let bound =
            TaskFile::bind(j, &["x".into(), "y".into(), "one".into(), "two".into()]).unwrap();
        assert_eq!(bound.get("b").map(String::as_str), Some("y"));
        assert_eq!(bound.get("rest").map(String::as_str), Some("one two"));
    }

    #[test]
    fn default_cwd_is_the_task_file_dir() {
        let tf = parse("## t\n\n```sh\ntrue\n```\n");
        let j = tf.job("t").unwrap();
        // Default: the file's directory, not where invoked.
        let inv = tf
            .invocation(j, &args(&[]), Path::new("/here"), Some(Path::new("/proj")))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/proj"));
        // With no job_file_dir known (headless), it falls back to cwd.
        let inv = tf
            .invocation(j, &args(&[]), Path::new("/here"), None)
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/here"));
        // An empty job_file_dir (a bare filename's parent) also falls back to cwd,
        // since running in an empty path would fail.
        let inv = tf
            .invocation(j, &args(&[]), Path::new("/here"), Some(Path::new("")))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/here"));
    }

    #[test]
    fn inherit_cwd_runs_in_the_invocation_dir() {
        let tf = parse("## t\n\nOpts: inherit-cwd\n\n```sh\ntrue\n```\n");
        let j = tf.job("t").unwrap();
        assert!(j.inherits_cwd());
        let inv = tf
            .invocation(j, &args(&[]), Path::new("/here"), Some(Path::new("/proj")))
            .unwrap();
        assert_eq!(inv.cwd, Path::new("/here"));
    }

    #[test]
    fn an_unknown_opt_warns_but_is_ignored() {
        let tf = parse("## t\n\nOpts: inherit-cwd bogus\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.jobs[0].opts, vec!["inherit-cwd", "bogus"]);
        assert!(tf.jobs[0].inherits_cwd()); // the known flag still applies
        assert!(tf.warnings.iter().any(|w| w.contains("bogus")));
    }

    // A `requires_of` for tests: a map from job name to its dependency names.
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
        let j = tf.job("t").unwrap();
        assert_eq!(j.script_arg_templates(), vec!["name"]);

        // A job that only uses $arg has no raw template interpolation.
        let safe = parse("## t\n\nArgs: name\n\n```sh\necho \"$name\"\n```\n");
        assert!(safe.job("t").unwrap().script_arg_templates().is_empty());
    }

    #[test]
    fn crlf_scripts_are_normalized() {
        let tf = parse("## t\r\n\r\n```sh\r\necho foo\r\necho bar\r\n```\r\n");
        assert_eq!(tf.jobs[0].script, "echo foo\necho bar\n");
        assert!(!tf.jobs[0].script.contains('\r'));
    }

    #[test]
    fn an_unterminated_fence_warns_but_keeps_the_job() {
        let tf = parse("## a\n\n```sh\necho hi\n"); // no closing fence
        assert_eq!(tf.jobs.len(), 1);
        assert_eq!(tf.jobs[0].script.trim(), "echo hi");
        assert!(tf.warnings.iter().any(|w| w.contains("unterminated")));
    }

    #[test]
    fn a_stray_fence_open_does_not_close_an_unterminated_block() {
        // ```sh has an info string, so it opens rather than closes; only a bare
        // ``` closes. (The trailing block here is what closes it.)
        let tf = parse("## a\n\n```sh\none\n```sh\ntwo\n```\n");
        assert!(tf.jobs[0].script.contains("one"));
        assert!(tf.jobs[0].script.contains("```sh\ntwo"));
    }

    #[test]
    fn indented_metadata_is_recognized() {
        let tf = parse("## a\n\n- steps:\n  Env: KEY=val\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.jobs[0].env, vec![("KEY".into(), "val".into())]);
    }

    #[test]
    fn duplicate_and_unknown_lang_warn() {
        let tf = parse("## a\n\n```json\n{}\n```\n\n## a\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.jobs.len(), 2);
        assert!(tf.warnings.iter().any(|w| w.contains("duplicate")));
        assert!(tf.warnings.iter().any(|w| w.contains("json")));
    }

    #[test]
    fn agent_jobs_filters_to_the_gated_ones() {
        let f = files(&[(
            "tasks.md",
            "## open\n\nAgent: allow\n\n```sh\ntrue\n```\n\n## closed\n\n```sh\ntrue\n```\n",
        )]);
        let names: Vec<_> = agent_jobs(&f).iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["open"]);
    }

    #[test]
    fn agent_jobs_shadows_a_farther_allowed_with_a_nearer_non_allowed() {
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
            files[0].1.job("shared").unwrap().script.trim(),
            "echo child"
        );
        // The parent still supplies `base` as an inherited baseline.
        assert!(files[1].1.job("base").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn run_captured_returns_stdout() {
        let f = files(&[("tasks.md", "## hello\n\n```sh\necho hello-out\n```\n")]);
        let out = run_captured(&f, "hello", &[], Path::new(".")).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello-out");
    }

    #[test]
    fn run_captured_runs_requires_deps_first() {
        let f = files(&[(
            "tasks.md",
            "## a\n\nRequires: b\n\n```sh\necho A\n```\n\n## b\n\n```sh\necho B\n```\n",
        )]);
        let out = run_captured(&f, "a", &[], Path::new(".")).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        // b runs before a (deps first).
        let bpos = text.find('B').expect("B in output");
        let apos = text.find('A').expect("A in output");
        assert!(bpos < apos, "deps must run first: {text}");
    }

    #[test]
    fn run_reports_an_unknown_target_as_not_found() {
        let f = files(&[("tasks.md", "## a\n\n```sh\ntrue\n```\n")]);
        match run_captured(&f, "ghost", &[], Path::new(".")) {
            Err(RunError::NotFound(n)) => assert_eq!(n, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // The two RCE regressions, exercised through the public agent gate.

    #[test]
    fn run_agent_resolves_requires_within_the_targets_file_not_a_nearer_shadow() {
        // A nearer, untrusted `build` must NOT run when the allowed ancestor
        // `deploy` (which requires build) is invoked by name. The chain resolves
        // within deploy's own file, so the ancestor's real build runs, not PWNED.
        let f = files(&[
            ("child/tasks.md", "## build\n\n```sh\necho PWNED\n```\n"),
            (
                "tasks.md",
                "## deploy\n\nAgent: allow\nRequires: build\n\n```sh\necho real-deploy\n```\n\n## build\n\n```sh\necho real-build\n```\n",
            ),
        ]);
        let out = run_agent(&f, "deploy", &[], Path::new(".")).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("real-build"), "got: {text}");
        assert!(text.contains("real-deploy"), "got: {text}");
        assert!(!text.contains("PWNED"), "nearer build ran: {text}");
        assert!(out.status.success());
    }

    #[test]
    fn run_agent_refuses_a_target_that_injects_an_arg_via_double_brace() {
        // greet interpolates {{ name }} raw into its script; an agent-supplied
        // value would be shell-injectable, so run_agent must refuse before running.
        let f = files(&[(
            "tasks.md",
            "## greet\n\nAgent: allow\nArgs: name\n\n```sh\necho hi {{ name }}\n```\n",
        )]);
        match run_agent(&f, "greet", &["x; echo PWNED".into()], Path::new(".")) {
            Err(RunError::Injects { task, args }) => {
                assert_eq!(task, "greet");
                assert_eq!(args, vec!["name".to_string()]);
            }
            other => panic!("expected Injects, got {other:?}"),
        }
    }

    #[test]
    fn run_agent_refuses_a_non_allowed_target() {
        let f = files(&[("tasks.md", "## secret\n\n```sh\ntrue\n```\n")]);
        match run_agent(&f, "secret", &[], Path::new(".")) {
            Err(RunError::NotAllowed(n)) => assert_eq!(n, "secret"),
            other => panic!("expected NotAllowed, got {other:?}"),
        }
    }

    #[test]
    fn run_agent_refuses_when_a_nearer_non_allowed_shadows_an_allowed_one() {
        // The nearest `deploy` lacks the gate; it shadows the farther allowed one.
        let f = files(&[
            ("child/tasks.md", "## deploy\n\n```sh\necho PWNED\n```\n"),
            (
                "tasks.md",
                "## deploy\n\nAgent: allow\n\n```sh\necho real\n```\n",
            ),
        ]);
        match run_agent(&f, "deploy", &[], Path::new(".")) {
            Err(RunError::NotAllowed(n)) => assert_eq!(n, "deploy"),
            other => panic!("expected NotAllowed, got {other:?}"),
        }
    }
    /// The bug this default exists to prevent: a shell runs the whole block as
    /// one script, so without `set -e` a failing early step is swallowed and the
    /// task exits with the status of the LAST command. A gate that cannot fail
    /// is worse than no gate, because it is trusted.
    #[test]
    fn a_failing_early_step_fails_the_job() {
        let tf = parse("## check\n\n```sh\nfalse\ntrue\n```\n");
        let out = run_captured(
            &[(PathBuf::from("tasks.md"), tf)],
            "check",
            &[],
            Path::new("."),
        )
        .expect("runs");
        assert!(
            !out.status.success(),
            "a job whose first command fails must not report success"
        );
    }

    #[test]
    fn no_strict_restores_the_old_lenient_behavior() {
        let tf = parse("## check\n\nOpts: no-strict\n\n```sh\nfalse\ntrue\n```\n");
        let out = run_captured(
            &[(PathBuf::from("tasks.md"), tf)],
            "check",
            &[],
            Path::new("."),
        )
        .expect("runs");
        assert!(
            out.status.success(),
            "no-strict should exit with the last command's status"
        );
    }

    #[test]
    fn a_passing_job_is_unaffected() {
        let tf = parse("## ok\n\n```sh\ntrue\necho fine\n```\n");
        let out = run_captured(
            &[(PathBuf::from("tasks.md"), tf)],
            "ok",
            &[],
            Path::new("."),
        )
        .expect("runs");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("fine"));
    }

    /// Existing task files already open with `set -euo pipefail` by hand, so the
    /// prelude has to be harmlessly redundant rather than conflicting.
    #[test]
    fn a_hand_written_prelude_still_works() {
        let tf = parse("## ok\n\n```sh\nset -eu\necho fine\n```\n");
        let out = run_captured(
            &[(PathBuf::from("tasks.md"), tf)],
            "ok",
            &[],
            Path::new("."),
        )
        .expect("runs");
        assert!(out.status.success());
    }

    /// `pipefail` is not POSIX and dash rejects it outright, so plain `sh` must
    /// not receive it or every task breaks on a Debian-ish /bin/sh.
    #[test]
    fn plain_sh_does_not_get_pipefail() {
        assert_eq!(strict_prelude("sh"), Some("set -e"));
        assert_eq!(strict_prelude(""), Some("set -e"));
        assert!(strict_prelude("bash").unwrap().contains("pipefail"));
        assert!(strict_prelude("zsh").unwrap().contains("pipefail"));
    }

    /// Injecting shell syntax into another language would be a syntax error, so
    /// non-shells are left alone.
    #[test]
    fn non_shells_get_no_prelude() {
        for lang in ["python", "ruby", "node", "fish"] {
            assert_eq!(
                strict_prelude(lang),
                None,
                "{lang} must not be given shell syntax"
            );
        }
    }

    /// A python job still runs, which is the real check that the prelude is not
    /// being spliced into a language that cannot parse it.
    #[test]
    fn a_python_job_is_untouched() {
        let tf = parse("## py\n\n```python\nprint(\"hi\")\n```\n");
        let out = run_captured(
            &[(PathBuf::from("tasks.md"), tf)],
            "py",
            &[],
            Path::new("."),
        )
        .expect("runs");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    }

    #[test]
    fn no_strict_is_a_known_opt_and_warns_no_one() {
        let tf = parse("## t\n\nOpts: no-strict\n\n```sh\ntrue\n```\n");
        assert!(
            tf.warnings().is_empty(),
            "no-strict must be recognized: {:?}",
            tf.warnings()
        );
    }
}
