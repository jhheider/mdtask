use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::run::{interpreter, substitute};

// Referenced only by the intra-doc links in this module's documentation, which
// resolve in module scope.
#[allow(unused_imports)]
use crate::{
    discover::find_task_files,
    run::{agent_jobs, run, run_agent, run_captured},
};

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
    /// File-level `Opts:`, set before the first task heading. Currently just
    /// `include-parent`; see [`find_task_files`].
    pub(crate) opts: Vec<String>,
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
    pub requires: Vec<Requirement>,
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

/// One entry in a `Requires:` list: a job to run first, and the arguments to run
/// it with.
///
/// Comma-separated, and an entry in parentheses carries arguments, borrowing
/// just's `(dist module)` shape:
///
/// ```text
/// Requires: lint, (dist bonus-die)
/// Requires: (dist {{ module }})
/// ```
///
/// A bare name takes no arguments, which is what every `Requires:` meant before
/// this existed, so old files keep working.
///
/// `{{ name }}` inside an argument resolves against the arguments of the job
/// that *declares* the requirement. Unlike `{{ }}` in a script this is not an
/// injection risk: the value becomes an argument to the dependency, which binds
/// it as an environment variable, and is never spliced into a script's source.
/// A dependency that then templates it into its own script is refused by
/// [`run_agent`] exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// The job to run first.
    pub name: String,
    /// Positional arguments for it, as written, before `{{ }}` resolution.
    pub args: Vec<String>,
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

/// The `Opts:` flags mdtask recognizes. An `Opts:` value outside this set is
/// recorded as a warning and otherwise ignored, so a file written for a newer
/// mdtask does not hard-fail on an older one.
pub(crate) const KNOWN_OPTS: &[&str] = &["inherit-cwd", "no-strict"];

/// `Opts:` flags that only mean something at file level, before the first task.
pub(crate) const KNOWN_FILE_OPTS: &[&str] = &["include-parent"];

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

    /// Whether this file declared `Opts: include-parent` before its first task
    /// heading, asking [`find_task_files`] to keep walking up and layer the
    /// parent's tasks underneath its own.
    pub fn includes_parent(&self) -> bool {
        self.opts.iter().any(|o| o == "include-parent")
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
        // An unrecognized language resolves to a *strict* sh, never a bare one:
        // the bare fallback is what let a ```console block report success on a
        // failing step. The parser has already warned that this is happening.
        let lang = interpreter(&job.lang);
        let (program, flag) = (lang.program, lang.flag);
        let script = match lang.prelude {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
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
}
