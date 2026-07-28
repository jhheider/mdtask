use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::cancel::Cancel;
use crate::deps::{Step, dependency_order};
use crate::model::{Invocation, Job, RunError, TaskFile};

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
fn trusted_order(files: &[(PathBuf, TaskFile)], target: &str) -> Result<Vec<Step>, RunError> {
    if trusted_lookup(files, target).is_none() {
        return Err(RunError::NotFound(target.to_string()));
    }
    dependency_order(target, |n| {
        trusted_lookup(files, n).map(|(_, j, _)| j.requires.clone())
    })
    .map_err(RunError::Dependency)
}

/// Build the ordered, ready-to-spawn invocations for `order`. `lookup` resolves
/// each step to its file, job, and directory.
///
/// The target receives the caller's `args`. A dependency receives whatever its
/// `Requires:` entry declared, with `{{ name }}` resolved against **the
/// invocation's** arguments: the values bound to the task actually named on the
/// command line.
///
/// One scope for the whole chain, rather than each job resolving against its own
/// caller. That is a real choice and worth stating: the walk is post-order, so a
/// dependency is planned before the parent that declares it, and a per-caller
/// scope would mean binding parents before children purely to read their values
/// back. One scope is also easier to explain, and it matches what a chain is for
/// (`release bonus-die` should mean bonus-die throughout).
///
/// A dependency that declared no arguments still runs on its own defaults.
fn plan_invocations<'a>(
    order: &[Step],
    target: &str,
    args: &[String],
    cwd: &Path,
    lookup: impl Fn(&str) -> Option<(&'a TaskFile, &'a Job, Option<&'a Path>)>,
) -> Result<Vec<Invocation>, RunError> {
    // The invocation's own bindings, resolved up front so every step in the
    // chain can be written against them.
    let scope = {
        let (_, job, _) = lookup(target).expect("the target resolves");
        TaskFile::bind(job, args).map_err(RunError::MissingArg)?
    };

    // Resolve first, then dedupe. The walk could only dedupe on the templates as
    // written, so `(dist {{ module }})` and `(dist foundry)` looked like two
    // different steps right up until they resolved to the same one. Deduping
    // here keeps the first occurrence, which is still ahead of everything that
    // depends on it.
    let mut seen = BTreeSet::new();
    let mut resolved: Vec<(&str, Vec<String>)> = Vec::with_capacity(order.len());
    for step in order {
        let step_args: Vec<String> = if step.name == target {
            args.to_vec()
        } else {
            step.args.iter().map(|a| substitute(a, &scope)).collect()
        };
        if seen.insert((step.name.clone(), step_args.clone())) {
            resolved.push((step.name.as_str(), step_args));
        }
    }

    let mut plan = Vec::with_capacity(resolved.len());
    for (name, step_args) in resolved {
        let (tf, job, dir) = lookup(name).expect("a resolved name still resolves");
        // Checked for every step, not just the target: a chain is only as
        // runnable as its dependencies, and finding out three steps in is worse
        // than finding out before anything ran.
        let invalid: Vec<String> = job
            .args
            .iter()
            .filter(|a| !a.is_valid_name())
            .map(|a| a.name.clone())
            .collect();
        if !invalid.is_empty() {
            return Err(RunError::InvalidArgName {
                task: name.to_string(),
                args: invalid,
            });
        }
        let values = TaskFile::bind(job, &step_args).map_err(RunError::MissingArg)?;
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
fn run_plan_captured(
    plan: &[Invocation],
    cancel: Option<&Cancel>,
) -> Result<std::process::Output, RunError> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    for inv in plan {
        // Checked before each step as well as during one, so a cancellation
        // arriving between two steps of a `Requires:` chain stops the chain
        // rather than being noticed only once the next step is already running.
        if cancel.is_some_and(Cancel::is_cancelled) {
            return Err(RunError::Cancelled);
        }
        let out = inv.run_captured(cancel).map_err(|e| inv.spawn_error(e))?;
        stdout.extend_from_slice(&out.stdout);
        stderr.extend_from_slice(&out.stderr);
        // A killed step exits non-zero, which is indistinguishable from a
        // failing one by exit code alone. Ask the handle instead.
        if cancel.is_some_and(Cancel::is_cancelled) {
            return Err(RunError::Cancelled);
        }
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
        let status = inv.run_inherit().map_err(|e| inv.spawn_error(e))?;
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
    run_plan_captured(&plan, None)
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
    run_agent_inner(files, name, args, cwd, None)
}

/// [`run_agent`], stoppable through a [`Cancel`] handle held by another thread.
///
/// The agent surface is the one that needs this. A person at a terminal has
/// Ctrl-C; an MCP client has only `notifications/cancelled`, so unless something
/// else can reach the running task, a task that serves, sleeps, or waits on the
/// network runs to completion whatever the client says.
///
/// The gate is identical to [`run_agent`]'s: same allowlist, same injection
/// refusal, same in-file dependency resolution. Cancelling mid-chain stops
/// before the next step and returns [`RunError::Cancelled`], which is
/// deliberately not a failure: nothing went wrong, someone asked it to stop.
pub fn run_agent_cancellable(
    files: &[(PathBuf, TaskFile)],
    name: &str,
    args: &[String],
    cwd: &Path,
    cancel: &Cancel,
) -> Result<std::process::Output, RunError> {
    run_agent_inner(files, name, args, cwd, Some(cancel))
}

fn run_agent_inner(
    files: &[(PathBuf, TaskFile)],
    name: &str,
    args: &[String],
    cwd: &Path,
    cancel: Option<&Cancel>,
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
    run_plan_captured(&plan, cancel)
}

impl Invocation {
    /// Wrap a spawn failure with which task and which program it was.
    fn spawn_error(&self, source: std::io::Error) -> RunError {
        RunError::Io {
            task: self.task.clone(),
            program: self.program.clone(),
            cwd: self.cwd.clone(),
            source,
        }
    }

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
    /// Run to completion, capturing output.
    ///
    /// With a [`Cancel`] handle this spawns into its own **process group** and
    /// records it, so the signal can reach the script's children rather than
    /// only the shell that launched them. Without one it is `output()`, which is
    /// the same thing minus the bookkeeping.
    fn run_captured(&self, cancel: Option<&Cancel>) -> std::io::Result<std::process::Output> {
        let Some(cancel) = cancel else {
            return self.command().output();
        };

        let mut cmd = self.command();
        // What `output()` does for us, spelled out because we spawn by hand:
        // a null stdin (so a task reading stdin gets EOF rather than blocking on
        // a terminal that is not there) and piped output.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Its own group, so cancelling reaches everything the script starts.
            cmd.process_group(0);
        }

        let child = cmd.spawn()?;
        // The child leads its own group, so the group id is its pid.
        cancel.entered(child.id());
        let out = child.wait_with_output();
        cancel.left();
        out
    }
}

/// Map a fence language to `(program, code-flag)`. Unlabeled or unknown falls
/// back to `sh -c`, so a plain ` ``` ` block runs as a shell script.
/// The program, its "run this string" flag, and its strictness prelude.
///
/// One table, deliberately. This was three: `interpreter`, `strict_prelude` and
/// `is_known_lang` each matched the same input separately, and two of them
/// disagreed on the fallback arm. `interpreter` fell back to `sh` for an
/// unrecognized language while `strict_prelude` fell back to `None`, so a block
/// tagged ```console ran as a shell with no `set -e` and a failing step exited
/// 0. That is exactly the failure this crate advertises that it prevents,
/// reachable by one wrong word in a fence.
///
/// An unrecognized language falls back to `sh` **with** the shell prelude, and
/// the parser warns. Forgiving is the right default for a fence tagged
/// `shell-session` or `bash5`, and the strictness is what makes the fallback
/// safe: a block that is not a shell script at all (a ```toml table, say) now
/// fails on its first line instead of running halfway and exiting 0.
pub(crate) fn interpreter(lang: &str) -> Interpreter {
    let (program, flag, prelude, recognized) = match lang.trim().to_ascii_lowercase().as_str() {
        "" | "sh" | "shell" => ("sh", "-c", Some("set -e"), true),
        "bash" => ("bash", "-c", Some("set -e\nset -o pipefail"), true),
        "zsh" => ("zsh", "-c", Some("set -e\nset -o pipefail"), true),
        "fish" => ("fish", "-c", None, true),
        "python" | "py" | "python3" => ("python3", "-c", None, true),
        "ruby" => ("ruby", "-e", None, true),
        "node" | "js" | "javascript" => ("node", "-e", None, true),
        // Unknown: assume a shell, and give it the same failure detection a
        // shell gets. The fallback itself was never the bug; the bug was that
        // this arm resolved to `sh` while the prelude's matching arm resolved
        // to `None`, so the fallback shell ran unstrict.
        _ => ("sh", "-c", Some("set -e"), false),
    };
    Interpreter {
        program,
        flag,
        prelude,
        recognized,
    }
}

/// How to run one language: the program, its flag, and its strictness prelude.
pub(crate) struct Interpreter {
    pub(crate) program: &'static str,
    pub(crate) flag: &'static str,
    /// The strictness prelude, or `None` for a language with no failure-detection
    /// setting worth injecting.
    ///
    /// Only shells, and only the settings that are about *detecting failure*,
    /// which is the task runner's job:
    ///
    /// - `set -e` stops at the first failing command, so a gate cannot pass
    ///   while a step inside it fails.
    /// - `pipefail` extends that through a pipeline, where the exit status
    ///   would otherwise be the last stage's and a failing producer would go
    ///   unnoticed.
    ///
    /// Deliberately NOT `set -u`. Catching an unset variable is a lint rather
    /// than failure detection, and it changes the meaning of correct scripts:
    /// reading an optional variable is ordinary in a task file, and defaulting
    /// it to a hard error would break working tasks to catch a typo. Authors
    /// who want it can still write it themselves.
    ///
    /// `pipefail` is not POSIX, so plain `sh` gets only `set -e`: dash rejects
    /// `set -o pipefail` outright, which would break every task on a
    /// Debian-ish `/bin/sh`. `fish` gets nothing, having neither the syntax nor
    /// the semantics, and non-shells are left alone entirely.
    pub(crate) prelude: Option<&'static str>,
    /// Whether the language was named in the table, as opposed to falling
    /// through to the `sh` assumption. Carried here so that "do we know this
    /// language" is answered by the same table that answers "how do we run it",
    /// rather than by a second list that can drift from it. It drifting is
    /// exactly how the unstrict-fallback bug happened.
    pub(crate) recognized: bool,
}

/// Whether a fence language maps to an interpreter (unlabeled counts as `sh`).
/// Whether `lang` names an interpreter outright, as opposed to falling through
/// to the `sh` assumption. Derived from the same table, so the two cannot drift.
pub(crate) fn is_known_lang(lang: &str) -> bool {
    interpreter(lang).recognized
}

/// Replace `{{ name }}` tokens (any inner whitespace) with `args[name]`. A token
/// whose name is not in `args` is left as written, so a literal `{{x}}` that is
/// not an argument survives.
pub(crate) fn substitute(src: &str, args: &BTreeMap<String, String>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the process group. A shell task is `sh -c <script>`,
    /// so the thing we spawn is a shell and the work is its children. Killing
    /// only the shell leaves the work running while we report the task stopped,
    /// which is worse than not cancelling, because it is a lie.
    ///
    /// The grandchild writes to a file *after* its sleep, so the file existing
    /// afterwards proves it survived the cancellation.
    #[cfg(unix)]
    #[test]
    fn cancelling_kills_the_script_s_children_not_just_the_shell() {
        let dir = std::env::temp_dir().join(format!("mdtask-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let witness = dir.join("survived");
        let src = format!(
            "## t\n\nAgent: allow\n\n```sh\n(sleep 5; touch {}) &\nwait\n```\n",
            witness.display()
        );
        let files = vec![(dir.join("tasks.md"), parse(&src))];

        let cancel = Cancel::new();
        let handle = {
            let cancel = cancel.clone();
            let dir = dir.clone();
            std::thread::spawn(move || run_agent_cancellable(&files, "t", &[], &dir, &cancel))
        };

        std::thread::sleep(std::time::Duration::from_millis(300));
        let started = std::time::Instant::now();
        cancel.cancel();
        let result = handle.join().expect("the run thread did not panic");
        let took = started.elapsed();

        assert!(
            matches!(result, Err(RunError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
        assert!(
            took < std::time::Duration::from_secs(4),
            "cancelling should not wait out the task: took {took:?}"
        );

        // Past when the grandchild would have fired had it survived.
        std::thread::sleep(std::time::Duration::from_secs(6));
        let survived = witness.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(!survived, "the grandchild outlived the cancellation");
    }

    /// Cancelling between two steps of a chain stops the chain, rather than
    /// being noticed only once the next step is already running.
    #[cfg(unix)]
    #[test]
    fn cancelling_stops_the_rest_of_a_requires_chain() {
        let dir = std::env::temp_dir().join(format!("mdtask-chain-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let witness = dir.join("second-ran");
        let src = format!(
            "## first\n\n```sh\nsleep 3\n```\n\n## second\n\nAgent: allow\nRequires: first\n\n```sh\ntouch {}\n```\n",
            witness.display()
        );
        let files = vec![(dir.join("tasks.md"), parse(&src))];

        let cancel = Cancel::new();
        let handle = {
            let cancel = cancel.clone();
            let dir = dir.clone();
            std::thread::spawn(move || run_agent_cancellable(&files, "second", &[], &dir, &cancel))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        cancel.cancel();
        let result = handle.join().expect("the run thread did not panic");

        let ran = witness.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        assert!(!ran, "the target ran even though the chain was cancelled");
    }

    /// Cancelling before the run starts must stop it at the first step, not let
    /// one through because nothing was running when the flag was set.
    #[cfg(unix)]
    #[test]
    fn a_run_cancelled_before_it_starts_never_spawns() {
        let dir = std::env::temp_dir().join(format!("mdtask-precancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let witness = dir.join("ran");
        let src = format!(
            "## t\n\nAgent: allow\n\n```sh\ntouch {}\n```\n",
            witness.display()
        );
        let files = vec![(dir.join("tasks.md"), parse(&src))];

        let cancel = Cancel::new();
        cancel.cancel();
        let result = run_agent_cancellable(&files, "t", &[], &dir, &cancel);

        let ran = witness.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        assert!(!ran, "the task ran despite being cancelled first");
    }

    /// Without a handle, nothing changes: the uncancellable path is still the
    /// one the CLI and every embedder uses.
    #[test]
    fn a_run_with_no_handle_still_completes_normally() {
        let f = files(&[(
            "tasks.md",
            "## t\n\nAgent: allow\n\n```sh\necho done\n```\n",
        )]);
        let out = run_agent(&f, "t", &[], Path::new(".")).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");
    }
    use crate::model::DepError;
    use crate::parse::parse;

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

    fn plan_for(src: &str, target: &str, args: &[&str]) -> Vec<Invocation> {
        let files = vec![(PathBuf::from("tasks.md"), parse(src))];
        let order = trusted_order(&files, target).expect("resolves");
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        plan_invocations(&order, target, &args, Path::new("."), |n| {
            trusted_lookup(&files, n)
        })
        .expect("plans")
    }

    /// The value of an argument as the step will actually see it.
    fn env_of<'a>(inv: &'a Invocation, key: &str) -> Option<&'a str> {
        inv.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    const PARAM: &str = "\
## dist

Args: module

```sh
true
```

## lint

```sh
true
```

## release

Args: module
Requires: lint, (dist {{ module }})

```sh
true
```
";

    #[test]
    fn substitutes_args_and_leaves_unknown_tokens() {
        let out = substitute(
            "hello {{ name }} and {{ other }}",
            &args(&[("name", "world")]),
        );
        assert_eq!(out, "hello world and {{ other }}");
    }

    /// The point of the whole feature. Before this, a job taking an argument
    /// could not be a dependency at all: it would be planned with none and fail
    /// on a missing value, so the chain was unrunnable.
    #[test]
    fn a_placeholder_resolves_to_the_invocations_argument() {
        let plan = plan_for(PARAM, "release", &["foundry"]);
        assert_eq!(plan.len(), 3, "lint, dist, release");
        assert_eq!(env_of(&plan[1], "module"), Some("foundry"), "dist got it");
        assert_eq!(
            env_of(&plan[2], "module"),
            Some("foundry"),
            "and so did release"
        );
    }

    /// Deduplication keys on the arguments too. Two `dist` steps with different
    /// modules are two different pieces of work, and collapsing them to one
    /// would silently skip a build.
    #[test]
    fn the_same_task_with_different_arguments_runs_twice() {
        let src = PARAM.replace(
            "Requires: lint, (dist {{ module }})",
            "Requires: (dist {{ module }}), (dist {{ module }}-docs)",
        );
        let plan = plan_for(&src, "release", &["foundry"]);
        assert_eq!(plan.len(), 3);
        assert_eq!(env_of(&plan[0], "module"), Some("foundry"));
        assert_eq!(env_of(&plan[1], "module"), Some("foundry-docs"));
    }

    #[test]
    fn the_same_task_with_the_same_arguments_still_runs_once() {
        let src = PARAM.replace(
            "Requires: lint, (dist {{ module }})",
            "Requires: (dist {{ module }}), (dist foundry)",
        );
        let plan = plan_for(&src, "release", &["foundry"]);
        assert_eq!(plan.len(), 2, "the two dist steps are the same work");
    }

    /// A placeholder naming something the invocation does not have is left as
    /// written rather than becoming an empty argument, matching how `substitute`
    /// treats an unknown token in a script body.
    #[test]
    fn an_unknown_placeholder_is_left_alone() {
        let src = PARAM.replace("(dist {{ module }})", "(dist {{ nonesuch }})");
        let plan = plan_for(&src, "release", &["foundry"]);
        assert_eq!(env_of(&plan[1], "module"), Some("{{ nonesuch }}"));
    }

    /// Cycle detection keys on the name alone. Keying it on name-plus-arguments
    /// would let `a` require `(a {{ x }}-more)` recurse forever, generating a
    /// longer argument each time and never repeating a key.
    #[test]
    fn a_self_reference_with_different_arguments_is_still_a_cycle() {
        let files = vec![(
            PathBuf::from("tasks.md"),
            parse("## a\n\nArgs: x\nRequires: (a {{ x }}-more)\n\n```sh\ntrue\n```\n"),
        )];
        assert!(matches!(
            trusted_order(&files, "a"),
            Err(RunError::Dependency(DepError::Cycle(_)))
        ));
    }

    /// The bug this whole change exists for. The fallback to `sh` was never the
    /// problem; the fallback running *without* `set -e` was, because a failing
    /// step then exited 0 and a gate passed while it was broken.
    #[test]
    fn the_sh_fallback_is_strict() {
        for lang in [
            "console",
            "shell-session",
            "terminal",
            "cmd",
            "bash5",
            "toml",
            "json",
        ] {
            let i = interpreter(lang);
            assert_eq!(i.program, "sh", "{lang:?} should fall back to sh");
            assert!(!i.recognized, "{lang:?} is not a named language");
            assert!(
                i.prelude.is_some_and(|p| p.contains("set -e")),
                "{lang:?} falls back to sh without failure detection"
            );
        }
    }

    /// The regression this whole change exists for: every language that resolves
    /// to a shell must carry a failure-detecting prelude, and one that resolves
    /// to nothing must not resolve to `sh` behind our backs. These were three
    /// separate `match`es and two of them disagreed.
    #[test]
    fn every_language_that_runs_a_shell_detects_failure() {
        // Anything whose program is a POSIX-ish shell must carry a prelude,
        // named or fallen-back-to alike. This is the invariant that three
        // separate `match`es failed to hold between them.
        for lang in ["", "sh", "shell", "bash", "zsh", "console", "nonsense-tag"] {
            let i = interpreter(lang);
            if matches!(i.program, "sh" | "bash" | "zsh") {
                assert!(
                    i.prelude.is_some_and(|p| p.contains("set -e")),
                    "{lang:?} runs {} with no failure detection",
                    i.program
                );
            }
        }
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

    /// Refused before anything runs. Reaching the script means failing as
    /// `slug: unbound variable`, which names the spelling that is correct and
    /// says nothing about the declaration that is wrong.
    #[test]
    fn a_task_with_an_unusable_argument_name_is_refused_before_it_runs() {
        let f = files(&[(
            "tasks.md",
            "## build\n\nArgs: slug, repo\n\n```sh\necho \"$slug\"\n```\n",
        )]);
        match run_captured(&f, "build", &["a".into(), "b".into()], Path::new(".")) {
            Err(RunError::InvalidArgName { task, args }) => {
                assert_eq!(task, "build");
                assert_eq!(args, vec!["slug,".to_string()]);
            }
            other => panic!("expected InvalidArgName, got {other:?}"),
        }
    }

    /// The message has to name the cause, not just the symptom. Someone reading
    /// it should not have to open the parser to find out what a comma did.
    #[test]
    fn the_refusal_explains_the_comma() {
        let e = RunError::InvalidArgName {
            task: "build".into(),
            args: vec!["slug,".into()],
        };
        let text = e.to_string();
        assert!(text.contains("slug,"), "{text}");
        assert!(text.contains("whitespace-separated"), "{text}");
        assert!(text.contains("Args: a b"), "{text}");
    }

    /// A chain is only as runnable as its dependencies, so the check covers
    /// every step. Finding out three steps in is worse than not starting.
    #[test]
    fn a_dependency_with_an_unusable_argument_name_is_refused_too() {
        let f = files(&[(
            "tasks.md",
            "## a\n\nRequires: b\n\n```sh\ntrue\n```\n\n\
             ## b\n\nArgs: x, y\n\n```sh\ntrue\n```\n",
        )]);
        match run_captured(&f, "a", &[], Path::new(".")) {
            Err(RunError::InvalidArgName { task, .. }) => assert_eq!(task, "b"),
            other => panic!("expected InvalidArgName for the dependency, got {other:?}"),
        }
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
        assert_eq!(interpreter("sh").prelude, Some("set -e"));
        assert_eq!(interpreter("").prelude, Some("set -e"));
        assert!(interpreter("bash").prelude.unwrap().contains("pipefail"));
        assert!(interpreter("zsh").prelude.unwrap().contains("pipefail"));
    }

    /// Injecting shell syntax into another language would be a syntax error, so
    /// non-shells are left alone.
    #[test]
    fn non_shells_get_no_prelude() {
        for lang in ["python", "ruby", "node", "fish"] {
            assert_eq!(
                interpreter(lang).prelude,
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
}
