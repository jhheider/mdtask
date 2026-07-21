//! One fixture exercising every feature and their combinations, end to end.
//! Complements the focused unit tests in the lib.

use std::collections::BTreeMap;
use std::path::Path;

use mdtask_core::parse;

const ALL: &str = include_str!("fixtures/all.md");

fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn every_feature_parses_and_combines() {
    let tf = parse(ALL);

    // The `# Project` section is not a task; its `Env:` is hoisted to all tasks.
    assert_eq!(tf.env, vec![("SHARED".into(), "base".into())]);
    let names: Vec<_> = tf.tasks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["build", "greet", "render", "deploy", "notes"]);

    // A well-formed file has no warnings (all fences closed, all langs known).
    assert!(
        tf.warnings.is_empty(),
        "unexpected warnings: {:?}",
        tf.warnings
    );

    // greet: args (required / defaulted / variadic), per-task env, shell interp.
    let greet = tf.task("greet").unwrap();
    let argnames: Vec<_> = greet.args.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(argnames, ["name", "greeting", "extra"]);
    assert!(greet.args[0].default.is_none() && !greet.args[0].variadic);
    assert_eq!(greet.args[1].default.as_deref(), Some("hello"));
    assert!(greet.args[2].variadic);
    assert_eq!(greet.env, vec![("MOOD".into(), "cheery".into())]);
    assert_eq!(greet.lang, "sh");

    // render: `Dir: .` pins to the task file's dir, zsh interpreter.
    let render = tf.task("render").unwrap();
    assert_eq!(render.dir.as_deref(), Some("."));
    assert_eq!(render.lang, "zsh");

    // deploy: dependency + agent gate.
    let deploy = tf.task("deploy").unwrap();
    assert_eq!(deploy.requires, ["build"]);
    assert!(deploy.agent_allow);
    assert_eq!(deploy.lang, "bash");

    // notes: non-shell interpreter, and a metadata line AFTER the fence is still
    // captured onto the task.
    let notes = tf.task("notes").unwrap();
    assert_eq!(notes.lang, "python");
    assert_eq!(notes.env, vec![("LATE".into(), "1".into())]);

    // Only `deploy` is exposed to an agent surface.
    let agent: Vec<_> = tf.agent_tasks().map(|t| t.name.as_str()).collect();
    assert_eq!(agent, ["deploy"]);
}

#[test]
fn invocations_combine_substitution_env_interpreter_and_dir() {
    let tf = parse(ALL);

    // greet: {{ name }} substituted; $greeting/$MOOD/$SHARED all in the env. Only
    // `name` is supplied - `greeting` falls back to its default, `extra` is empty.
    let greet = tf.task("greet").unwrap();
    let inv = tf
        .invocation(
            greet,
            &args(&[("name", "sam")]),
            Path::new("/cwd"),
            Some(Path::new("/proj")),
        )
        .unwrap();
    assert_eq!(inv.program, "sh");
    // {{ name }}/{{ extra }} substituted; $greeting / $MOOD stay shell vars
    // (resolved from the env at run time - the safe path for quoted values).
    assert!(
        inv.args[1].contains("$greeting, sam ($MOOD) "),
        "got {:?}",
        inv.args[1]
    );
    for want in [
        ("SHARED", "base"),
        ("MOOD", "cheery"),
        ("name", "sam"),
        ("greeting", "hello"), // filled from the arg default
    ] {
        assert!(
            inv.env.contains(&(want.0.to_string(), want.1.to_string())),
            "missing env {want:?} in {:?}",
            inv.env
        );
    }
    // No Dir: -> the invocation directory, not where the task file lives.
    assert_eq!(inv.cwd, Path::new("/cwd"));

    // render: `Dir: .` pins to the task file's own directory.
    let render = tf.task("render").unwrap();
    let inv = tf
        .invocation(
            render,
            &args(&[("file", "notes/deep/a.md")]),
            Path::new("/cwd"),
            Some(Path::new("/proj")),
        )
        .unwrap();
    assert_eq!(inv.program, "zsh");
    assert_eq!(inv.cwd, Path::new("/proj"));
    assert!(inv.args[1].contains("rendering notes/deep/a.md"));
}
