use std::path::{Path, PathBuf};

use crate::model::TaskFile;
use crate::parse::parse;

/// Search for task files from `start` upward, **nearest first**. In each
/// directory the first of `tasks.md`, `maskfile.md`, `README.md` that parses to
/// at least one job is taken.
///
/// **The walk stops at the first file found**, unless that file opts in with a
/// file-level `Opts: include-parent` before its first task heading. A file that
/// opts in is layered under by its own parent, on the same terms, so a chain
/// continues only as far as every link agrees.
///
/// Inheritance has to be opt-in because the walk previously ran to the
/// filesystem root, and every file it passed could define or *shadow* a task
/// name. Running `mdtask build` in a freshly cloned repository could therefore
/// run a script from a directory above it, chosen by a file the caller never
/// looked at and quite possibly did not know existed. Stopping at the first file
/// means what runs is what is written in the file you can see from where you are
/// standing, and a project that genuinely wants a shared baseline says so.
///
/// Where layering does happen, it is child-first: a nearer file shadows a
/// farther one by job name, like just's `set fallback`.
///
/// Embedders with their own project root can ignore this and call [`parse`].
pub fn find_task_files(start: &Path) -> Vec<(PathBuf, TaskFile)> {
    let mut found = Vec::new();
    for (depth, dir) in start.ancestors().enumerate() {
        for name in ["tasks.md", "maskfile.md", "README.md"] {
            let path = dir.join(name);
            if let Some(src) = read_candidate(&path, depth == 0) {
                let tf = parse(&src);
                if !tf.jobs.is_empty() {
                    let inherits = tf.includes_parent();
                    found.push((path, tf));
                    if !inherits {
                        return found;
                    }
                    break; // one file per directory
                }
            }
        }
    }
    found
}

/// The largest candidate we will read. A task file is hand-written markdown;
/// four mebibytes is orders of magnitude past any real one, and the cap is what
/// stops a `README.md` that happens to be a multi-gigabyte generated dump from
/// being pulled into memory by a walk nobody asked for.
const MAX_TASK_FILE: u64 = 4 * 1024 * 1024;

/// Read a candidate task file, or `None` if it is absent or something we should
/// not block on.
///
/// The walk touches every ancestor directory up to the root, so it reads files
/// the user never mentioned. That makes an unbounded read the wrong default:
///
/// - **Not a regular file.** `read_to_string` on a FIFO blocks until someone
///   writes to it, which may be never. A `tasks.md` FIFO in any ancestor
///   directory would hang every `mdtask` invocation run beneath it, and hang an
///   embedder like gloaming on startup with no way out. Character devices are
///   the same problem with a worse ending.
/// - **Too large.** See [`MAX_TASK_FILE`].
/// - **A cloud placeholder** (macOS). iCloud and Dropbox leave dataless stubs;
///   reading one triggers an on-demand download and blocks until it lands, or
///   forever if the provider is offline. `explicit` is the directory the caller
///   actually named: there, a download is what was asked for. In an ancestor it
///   is a passive read, and passive reads must not stall or mass-download.
///
/// Stat-then-read is a race in principle. It is not a security boundary: anyone
/// who can swap this path can also write the shell script it contains.
fn read_candidate(path: &Path, explicit: bool) -> Option<String> {
    // Follows symlinks deliberately, so a symlink pointing at a FIFO is judged
    // by what it resolves to rather than by being a link.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_TASK_FILE {
        return None;
    }
    #[cfg(target_os = "macos")]
    if !explicit && is_dataless(&meta) {
        return None;
    }
    let _ = explicit;
    std::fs::read_to_string(path).ok()
}

/// Whether a macOS File Provider left this file dataless: present in the
/// directory listing, with no local blocks behind it. `metadata` reports the
/// flag without materializing the file, which is the whole point of checking.
#[cfg(target_os = "macos")]
fn is_dataless(meta: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "Opts: include-parent\n\n## shared\n\n```sh\necho child\n```\n\n## only\n\n```sh\ntrue\n```\n",
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

    /// The default, and the reason inheritance became opt-in. The walk used to
    /// run to the filesystem root, and every file it passed could define or
    /// *shadow* a task name, so `mdtask build` in a fresh clone could run a
    /// script from a directory above it that the caller never looked at.
    #[test]
    fn the_walk_stops_at_the_first_file_by_default() {
        let base = std::env::temp_dir().join(format!("mdtask-stop-{}", std::process::id()));
        let child = base.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(
            base.join("tasks.md"),
            "## build\n\n```sh\necho hijacked\n```\n",
        )
        .unwrap();
        std::fs::write(child.join("tasks.md"), "## only\n\n```sh\ntrue\n```\n").unwrap();

        let files = find_task_files(&child);
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(files.len(), 1, "the parent is not consulted");
        assert!(
            files[0].1.job("build").is_none(),
            "and cannot supply a name"
        );
    }

    /// A chain continues only as far as every link agrees: the middle file opts
    /// in, the top one does not, so the walk takes the top file and stops there
    /// rather than continuing past it.
    #[test]
    fn opting_in_is_per_file_all_the_way_up() {
        let base = std::env::temp_dir().join(format!("mdtask-chain-{}", std::process::id()));
        let mid = base.join("mid");
        let leaf = mid.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(base.join("tasks.md"), "## top\n\n```sh\ntrue\n```\n").unwrap();
        std::fs::write(
            mid.join("tasks.md"),
            "Opts: include-parent\n\n## middle\n\n```sh\ntrue\n```\n",
        )
        .unwrap();
        std::fs::write(
            leaf.join("tasks.md"),
            "Opts: include-parent\n\n## leaf\n\n```sh\ntrue\n```\n",
        )
        .unwrap();

        let files = find_task_files(&leaf);
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(files.len(), 3, "leaf, mid, top");
        assert!(files[2].1.job("top").is_some());
    }

    /// A FIFO named `tasks.md` in an ancestor directory used to hang every
    /// invocation run beneath it, forever, with no output and no way out: the
    /// walk read it unconditionally and `read_to_string` on a FIFO blocks until
    /// someone writes. An embedder loading tasks at startup just never started.
    ///
    /// The timeout is the assertion. A regression here does not fail the test,
    /// it hangs the whole test binary, so the wait has to be bounded and the
    /// work has to happen somewhere it can be abandoned.
    #[cfg(unix)]
    #[test]
    fn a_fifo_task_file_does_not_hang_the_walk() {
        let base = std::env::temp_dir().join(format!("mdtask-fifo-{}", std::process::id()));
        let child = base.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let fifo = base.join("tasks.md");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            std::fs::remove_dir_all(&base).ok();
            return; // no mkfifo here; nothing to prove
        }
        // A real file below it, so we can also see the walk carried on.
        std::fs::write(child.join("tasks.md"), "## only\n\n```sh\ntrue\n```\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let probe = child.clone();
        std::thread::spawn(move || {
            let _ = tx.send(find_task_files(&probe).len());
        });
        let found = rx.recv_timeout(std::time::Duration::from_secs(10));
        std::fs::remove_dir_all(&base).ok();

        let found = found.expect("the walk returned instead of blocking on the FIFO");
        assert_eq!(
            found, 1,
            "the FIFO was skipped and the real file still read"
        );
    }

    /// A `README.md` is a candidate, and a README can be a generated dump. The
    /// walk should not pull an arbitrarily large one into memory to discover it
    /// has no task headings in it.
    #[test]
    fn an_oversized_candidate_is_skipped() {
        let base = std::env::temp_dir().join(format!("mdtask-big-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("tasks.md");

        let real = "## only\n\n```sh\ntrue\n```\n";
        std::fs::write(&path, real).unwrap();
        assert!(
            read_candidate(&path, true).is_some(),
            "an ordinary file reads"
        );

        let padding = "x".repeat(MAX_TASK_FILE as usize + 1);
        std::fs::write(&path, padding).unwrap();
        assert!(
            read_candidate(&path, true).is_none(),
            "an oversized one does not"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// A directory named `tasks.md` is not a task file, and must not stop the
    /// walk from finding the real one further up.
    #[test]
    fn a_directory_named_like_a_task_file_is_skipped() {
        let base = std::env::temp_dir().join(format!("mdtask-dir-{}", std::process::id()));
        let child = base.join("child");
        std::fs::create_dir_all(child.join("tasks.md")).unwrap();
        std::fs::write(base.join("tasks.md"), "## only\n\n```sh\ntrue\n```\n").unwrap();

        let files = find_task_files(&child);
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(files.len(), 1);
        assert!(
            files[0].1.job("only").is_some(),
            "the real one, one level up"
        );
    }
}
