//! `mdtask lint [TASK]` runs shellcheck over the shell scripts mdtask finds.
//!
//! Reimplementing shellcheck is off the table: it is Haskell, GPLv3, and its
//! rules are code rather than a portable ruleset. So this delegates. It locates a
//! `shellcheck` binary (PATH first, then `pkgx shellcheck`) and fails loudly if
//! neither is available. Only POSIX-sh-family fences are checked, because
//! shellcheck cannot analyze `zsh`, `fish`, `python`, and the like; those tasks
//! are skipped with a note, never given a false pass.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use mdtask_core::{Task, TaskFile};

/// The shell dialect shellcheck should assume for a fence language, or `None`
/// when shellcheck cannot check it (anything outside the POSIX sh family).
fn dialect(lang: &str) -> Option<&'static str> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "" | "sh" | "shell" => Some("sh"),
        "bash" => Some("bash"),
        _ => None,
    }
}

/// Replace `{{ token }}` templates with a neutral placeholder so shellcheck sees
/// valid shell. The raw `{{ }}` is mdtask's pre-shell substitution, not shell
/// syntax, and would trip the parser. Tokens are single-line in practice, so the
/// line count (and thus shellcheck's reported line numbers) is preserved.
fn despan(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    let mut rest = script;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        if let Some(close) = after.find("}}") {
            out.push_str("mdtask_arg");
            rest = &after[close + 2..];
        } else {
            out.push_str("{{");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Find a runnable `shellcheck` (directly, or via `pkgx`), returning the base
/// command as argv. `None` means neither is available.
fn resolve_shellcheck() -> Option<Vec<String>> {
    for base in [
        vec!["shellcheck".to_string()],
        vec!["pkgx".to_string(), "shellcheck".to_string()],
    ] {
        let ok = Command::new(&base[0])
            .args(&base[1..])
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(base);
        }
    }
    None
}

/// Lint the shell scripts across the layered task files. `only` restricts to one
/// task by name; otherwise every task (nearest definition per name) is checked.
/// Exit: `0` all clean, `1` shellcheck flagged something, `2` shellcheck absent.
pub fn run(files: &[(PathBuf, TaskFile)], only: Option<&str>) -> ExitCode {
    let Some(base) = resolve_shellcheck() else {
        eprintln!(
            "mdtask: shellcheck not found. Install it (for example `pkgx \
             shellcheck`, `brew install shellcheck`, or your package manager), \
             then retry."
        );
        return ExitCode::from(2);
    };

    // Nearest definition per name wins (the fallback layering), as `list`/`run`.
    let mut seen = BTreeSet::new();
    let mut targets: Vec<&Task> = Vec::new();
    for (_, tf) in files {
        for t in &tf.tasks {
            if only.is_some_and(|n| n != t.name) {
                continue;
            }
            if seen.insert(t.name.clone()) {
                targets.push(t);
            }
        }
    }
    if let Some(name) = only
        && targets.is_empty()
    {
        eprintln!("mdtask: no task named {name:?}");
        return ExitCode::FAILURE;
    }

    let mut findings = false;
    let mut checked = 0usize;
    for t in targets {
        let Some(d) = dialect(&t.lang) else {
            eprintln!(
                "mdtask: skipped {} ({}: shellcheck cannot check it)",
                t.name, t.lang
            );
            continue;
        };
        checked += 1;
        println!("==> {} ({d})", t.name);
        match check_one(&base, d, &despan(&t.script)) {
            Ok(true) => {}
            Ok(false) => findings = true,
            Err(e) => {
                eprintln!("mdtask: could not run shellcheck: {e}");
                return ExitCode::from(2);
            }
        }
    }

    if checked == 0 {
        eprintln!("mdtask: no shell scripts to check");
    }
    if findings {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Run shellcheck on one script (fed on stdin), inheriting its output so findings
/// print through. `Ok(true)` = clean. `SC2154` is excluded: mdtask exports args
/// and `Env:` as shell variables, so "referenced but not assigned" is expected.
fn check_one(base: &[String], dialect: &str, script: &str) -> std::io::Result<bool> {
    let mut child = Command::new(&base[0])
        .args(&base[1..])
        .arg(format!("--shell={dialect}"))
        .arg("--exclude=SC2154")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(script.as_bytes())?;
    Ok(child.wait()?.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_covers_the_posix_family_only() {
        assert_eq!(dialect(""), Some("sh"));
        assert_eq!(dialect("SH"), Some("sh"));
        assert_eq!(dialect("bash"), Some("bash"));
        assert_eq!(dialect("zsh"), None);
        assert_eq!(dialect("python"), None);
    }

    #[test]
    fn despan_replaces_templates_and_preserves_lines() {
        let out = despan("echo {{ name }}\ncat {{ file }} > out\n");
        assert_eq!(out, "echo mdtask_arg\ncat mdtask_arg > out\n");
        // A dangling `{{` is left as-is rather than eating the rest.
        assert_eq!(despan("echo {{ x"), "echo {{ x");
    }
}
