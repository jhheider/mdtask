//! One fixture exercising the public surface end to end: parse the metadata,
//! list the agent jobs, and run through `run_captured` and `run_agent`.
//! Complements the focused unit tests in the lib (which reach the internals).

use std::path::{Path, PathBuf};

use mdtask_core::{TaskFile, agent_jobs, parse, run_agent, run_captured};

const ALL: &str = include_str!("fixtures/all.md");

fn files() -> Vec<(PathBuf, TaskFile)> {
    vec![(PathBuf::from("tasks.md"), parse(ALL))]
}

#[test]
fn every_feature_parses_and_combines() {
    let tf = parse(ALL);

    // A well-formed file has no warnings (all fences closed, all langs known).
    assert!(
        tf.warnings().is_empty(),
        "unexpected warnings: {:?}",
        tf.warnings()
    );

    let names: Vec<_> = tf.jobs().iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, ["build", "greet", "render", "deploy", "notes"]);

    // greet: declared args (one required, one defaulted, a trailing variadic).
    let greet = tf.job("greet").unwrap();
    let argnames: Vec<_> = greet.args.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(argnames, ["name", "greeting", "extra"]);
    assert!(greet.args[0].default.is_none() && !greet.args[0].variadic);
    assert_eq!(greet.args[1].default.as_deref(), Some("hello"));
    assert!(greet.args[2].variadic);

    // render declares its `file` arg (its inherit-cwd/zsh mechanics are internal).
    let render = tf.job("render").unwrap();
    assert_eq!(
        render
            .args
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["file"]
    );

    // deploy: a dependency and the agent gate, both public metadata.
    let deploy = tf.job("deploy").unwrap();
    assert_eq!(deploy.requires, ["build"]);
    assert!(deploy.agent_allow);

    // Only `deploy` is exposed to an agent surface.
    let files = files();
    let agent: Vec<_> = agent_jobs(&files).iter().map(|j| j.name.as_str()).collect();
    assert_eq!(agent, ["deploy"]);
}

#[test]
fn run_captured_threads_env_args_and_substitution() {
    // build proves the hoisted `Env: SHARED=base` reaches a task and sh runs it.
    let out = run_captured(&files(), "build", &[], Path::new(".")).unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "building (base)"
    );

    // greet proves {{ name }} substitution, an arg default, per-task `Env: MOOD`,
    // and a `$var` read from the environment (the safe path for quoted values).
    let out = run_captured(&files(), "greet", &["sam".into()], Path::new(".")).unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("hello, sam (cheery)"), "got: {text}");
}

#[test]
fn run_agent_runs_an_allowed_task_and_its_dependency() {
    // deploy is `Agent: allow` and requires build; the agent path runs build first,
    // then deploy, resolving the chain within deploy's own file.
    let out = run_agent(&files(), "deploy", &[], Path::new(".")).unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("building"), "dependency did not run: {text}");
    assert!(text.contains("deploying"), "target did not run: {text}");
}
