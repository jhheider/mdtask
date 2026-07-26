# mdtask

**An embeddable, execution-capable, markdown task runner for Rust.** Define your
tasks in the same markdown you already write, whether a `tasks.md`, a
`maskfile.md`, or a project `README.md`, and run them from a library, a CLI, or
an agent-safe MCP surface.

````markdown
# Tasks

Env: PKGX_DISABLE_UPDATE=1        # hoisted to every task

## build

Build the release binary.

```sh
cargo build --release
```

## pdf

Render a note to PDF. `inherit-cwd` runs it where you invoke it, so
`mdtask pdf notes/a.md` resolves the path relative to your current directory.

Args: file
Opts: inherit-cwd

```sh
pandoc -t pdf -o "${file%md}pdf" "$file"
```
````

```console
$ mdtask                 # list tasks (see "Finding task files" below)
$ mdtask build           # run one
$ mdtask pdf notes/a.md  # positional args fill `Args:` in order
$ mdtask --show build    # print what a task will run, without running it
```

## Why this exists (honestly)

I wanted an **embeddable Rust library** that parses *and runs* markdown-defined
tasks, so that an editor, a TUI, or an agent host could offer "run the tasks in
this file" without shelling out to another tool. I went looking, and found
nothing that fit:

- **xc**, **Task**, and **runme** are excellent, but they are Go, so there is no
  Rust you can link.
- **just** is Rust, but it uses a Makefile-style DSL, not markdown.
- **mask** has a Rust crate (`mask-parser`), but it is *parse-only*,
  maskfile-locked, and untouched since 2024.

So `mdtask` is the million-and-first task runner, with no apology. It collects
the features I liked best from the others into a small, dependency-light **core
library** (`mdtask-core`) that anything can embed. The CLI and the feature-gated
MCP server are thin wrappers over it. The library is the point.

It is **not** compatible with any one of them, and promises only a little. It
borrows conventions (xc's clean `Key: value` metadata vocabulary, mask's per-fence
interpreter and positional args), so the simplest xc or mask files may parse, but
it owns its grammar and makes no compatibility or round-trip promise. Treat that
overlap as a convenience, not a contract.

## The format

- **A task is a heading** (`## name`); the **first fenced block** under it is the
  script, and the fence language picks the interpreter (`sh`, `bash`, `zsh`,
  `fish`, `python`, `ruby`, `node`, with an unlabeled fence running as `sh`).
- **A heading with no script is a section**, not a task, so a `# Tasks` container
  is fine.
- **Metadata** is `Key: value` lines in the task body (case-insensitive). A
  metadata line must **begin a block**: it follows the heading, a blank line, a
  fence, a list item, or another metadata line. Inside a paragraph it is prose,
  and warns. Wrapping a sentence so a line of it happened to read as a key would
  otherwise let a paragraph configure its own task, including opening the
  `Agent:` gate, while reading as ordinary prose to every human. Description
  before metadata, the way task files are already written, is unaffected.
  - `Args:` declares positional arguments in just's syntax. A bare `name` is
    **required**, `name='default'` is **optional** (that value when omitted), and
    a trailing `*name` is **variadic** (it collects the rest, space-joined). They
    are passed on the CLI in order, or prompted by an embedder. Each is
    substituted as `{{ name }}` in the script **and** exported as `$name`.
    **`{{ name }}` is raw text substitution**, spliced in before the interpreter
    parses the script, so `{{ name }}` is *not* safe for untrusted values in any
    language. The safe form is to **read the argument from the environment**, never
    to template it: `"$name"` in a shell (the shell quotes it, and `${name%md}`
    works), `os.environ["name"]` in Python, `process.env.name` in Node, and so on.
    Reserve `{{ }}` for developer-authored templates.
  - `Opts:` carries per-task flags, space-separated. An unrecognized flag is
    reported as a warning and ignored.
    - **`inherit-cwd`**: run the task in the directory you invoked mdtask from,
      instead of the default. Use it for a carry-around task that operates on a
      path relative to where you are (this is just's `[no-cd]`).
    - **`no-strict`**: turn off the shell strictness described below.

    Before the first task heading, `Opts:` sets **file-level** options instead,
    which are a separate vocabulary:

    - **`include-parent`**: keep walking up and layer the parent's tasks under
      this file's own. See [Finding task files](#finding-task-files).

    Using one where the other belongs warns and says which way round it goes.
  - `Env:` adds environment (`KEY=VALUE, KEY2=VALUE2`). An `Env:` under a section
    heading is **hoisted** to every task, regardless of position.
  - `Requires:` lists task dependencies, comma-separated. The CLI runs them
    first, resolved across the layered files, dependencies before dependents,
    each once (a diamond runs its shared dependency once), aborting on the first
    failure. A missing or cyclic dependency is a hard error.

    A dependency may take **arguments**, in parentheses, which is just's syntax:

    ```markdown
    Requires: lint, (dist {{ module }}), (deploy "the droplet" now)
    ```

    A bare name runs on its own defaults, as before. Inside the parentheses the
    first word is the task and the rest are its positional arguments, separated
    by whitespace or commas; quote one to include either. `{{ name }}` resolves
    against **the invocation's** arguments — the values bound to the task named
    on the command line — so `mdtask release bonus-die` means `bonus-die`
    throughout the chain, however deep. One scope for the whole chain, rather
    than each job resolving against its own caller.

    Unlike `{{ }}` in a script body, this is not a shell injection risk: the
    value becomes one element of an argument list, never text spliced into
    source. A placeholder naming nothing is left as written.

    Deduplication keys on the task **and its resolved arguments**, so
    `(dist api)` and `(dist web)` both run, while two paths reaching
    `(dist api)` still run it once. Cycle detection keys on the name alone, so
    a task requiring itself is an error however the arguments differ.

    Note that dependencies **re-run on every invocation**: there is no
    `make`-style "already satisfied" mtime or hash check, so `Requires:` is for
    ordering, not for skipping work that is already done.
  - `Agent: allow` opts a task in to an MCP or agent surface. It is **off by
    default**. `run` and `run_captured` ignore it; the gate is `run_agent` (which
    refuses anything not allowed) and `agent_jobs` (which lists only the allowed
    ones), so handing a task file to an agent never exposes ungated shell. See
    [MCP](#mcp).

The parser is line-based (no CommonMark dependency), so a `#` or `Key:` inside a
fenced block is never mistaken for structure. Parsing is infallible but records
problems (an unterminated fence, a duplicate task, an unknown interpreter) in
`TaskFile::warnings`; surface them rather than trusting silence.

### Working directory

A task runs in **the directory of the file that defines it** by default, the way
`just` runs a recipe from its justfile's directory. A task script is written
against its project's layout, so it runs from that project's root, even when you
invoke mdtask from a subdirectory (and, for a task reached by the tree-walk
below, from the directory of the ancestor file that defined it). Add
`Opts: inherit-cwd` for the exception: a carry-around task that should operate on
a path relative to wherever you are. Anything more specific than those two anchors
is a `cd` in the script.

### Shell tasks stop at the first failure

A shell task runs its whole fenced block as one script, so mdtask prepends
`set -e` (plus `pipefail` for `bash` and `zsh`) unless you write
`Opts: no-strict`.

This matters more than it sounds. Without it, a failing early command is
swallowed and the task exits with the status of the **last** command, which
quietly turns a multi-step gate into one that cannot fail:

```sh
cargo fmt --all -- --check    # fails
cargo test                    # passes
                              # ...and the task reports success
```

`just` avoids this in its *default* mode by running each line as its own recipe
line and stopping at the first error. Its shebang recipes, like xc and a
multi-line Taskfile `cmd:`, run the whole body as one script and do **not** add
strictness: there, writing `set -euo pipefail` yourself is the author's job.
mdtask hands the block to a shell too, so it asks the shell for that behavior on
your behalf instead.

Which language counts as a shell is decided by the fence tag. `sh`, `shell`,
`bash`, `zsh` and an untagged fence all get a prelude. `fish` does not: it has
neither `set -e` nor `set -o pipefail`, so a multi-step fish task exits with the
status of its **last** command and you must check `$status` yourself. Neither do
`python`, `ruby` or `node`, which raise on error already.

A tag mdtask does not recognize (`console`, `shell-session`, `bash5`) falls back
to `sh` **with** the prelude, and the parse warns you it did. The fallback is
deliberate, since those tags are usually a shell anyway, but it is a guess:
a fence that is not a shell script at all will fail on its first line rather
than be skipped. If mdtask is reading a `README.md`, that is the behavior to
expect from your ```toml and ```json examples, and the reason to keep tasks in a
`tasks.md` once a project has more than a couple.

> Getting this wrong is how the guarantee above became untrue for a while: the
> fallback resolved to `sh` while its prelude resolved to *nothing*, so a
> ```console block ran unstrict and a failing step exited 0. One table now
> answers both questions, and a test asserts that anything running a shell
> carries failure detection.

Deliberately **not** `set -u`. Catching an unset variable is a lint rather than
failure detection, and it changes the meaning of correct scripts: reading an
optional variable is ordinary in a task file. Write it yourself if you want it.
`pipefail` is skipped for plain `sh`, because it is not POSIX and dash rejects
it. Non-shell tasks (`python`, `node`, `ruby`) get nothing injected.

### Finding task files

The CLI looks for `tasks.md`, `maskfile.md`, or `README.md` in the current
directory, taking the first one that defines a task, and **stops there**.

A file can ask to inherit from above with a file-level option, before its first
task heading:

```markdown
# my project

Opts: include-parent

## build
...
```

Then the walk continues up (like `make`, `just`, and `xc`), and nearer files
**shadow** farther ones by task name, like just's `set fallback`, so a project
inherits a baseline of tasks from its parents and overrides them where it wants.
Each file up the chain decides for itself, so the chain continues only as far as
every link agrees.

Inheritance is opt-in because it used to be unconditional and run to the
filesystem root. Every file the walk passed could define **or shadow** a task
name, so `mdtask build` in a freshly cloned repository could run a script from a
directory above it, chosen by a file the caller never looked at. Stopping at the
first file means what runs is what is written in the file you can see from where
you are standing. (Changed in 0.5; before that, every ancestor was layered in
automatically.)

(`mdtask-core::parse` itself does no filesystem access; an embedder with its own
project root just calls it directly.)

### MCP

Built with `--features mcp`, `mdtask mcp` serves the working set to an MCP client
(Claude Desktop or Code) over stdio, so an agent can run your tasks. It is **fail
closed**: only tasks marked `Agent: allow` are exposed, everything else is
invisible and unrunnable.

- `list_tasks` enumerates **only** the allowed tasks (`agent_jobs`), so the rest
  are not even discoverable.
- `run_task` calls `run_agent`, which re-checks the allowlist before running, so
  naming a hidden task fails too. Its output is captured and returned as the tool
  result.
- A `Requires:` dependency of an allowed task still runs, but is never listed and
  never independently callable. Crucially, the chain is resolved **within the
  allowed task's own file**, not by the global nearest-wins layering the CLI uses:
  a nearer, untrusted `tasks.md` in the invocation directory **cannot** shadow a
  dependency and run its own code through an allowed entry point. The author who
  wrote `Agent: allow` vouched for their file's tasks, and only those run.
- A task that interpolates an argument into its **script** via `{{ arg }}` is
  **refused**: `{{ }}` is raw substitution, so an agent-supplied value would be
  injectable. Expose such a task only after switching it to read the value from
  the environment (`"$arg"` in a shell, `os.environ["arg"]` in Python, and so on).
  (This applies to the target that receives the agent's `args`; dependencies run
  with author-controlled defaults.)

The `mcp` feature is off by default, so a plain build pulls no JSON or server
dependencies.

## Embedding

A consumer deals only in jobs, their metadata, and running them. `parse` and
`find_task_files` give you the jobs; `jobs()`/`job()` read them; `agent_jobs`
lists the agent-exposed ones; and three functions run a job and its `Requires:`
chain: `run` (inherits stdio, streaming), `run_captured` (aggregated output), and
`run_agent` (the agent gate, captured). Interpreter selection, argv, working
directory, and spawning are all internal, so nothing hands you a program or argv.

```rust
let files = mdtask_core::find_task_files(&std::env::current_dir()?);

// Inspect a job's declared args before you run it.
if let Some((_, tf)) = files.first() {
    if let Some(job) = tf.job("pdf") {
        println!("pdf takes {} arg(s)", job.args.len());
    }
}

// Run it with captured output, off your render thread. Positional args fill the
// job's `Args:` in order; the `Requires:` chain runs first, deps before dependents.
let cwd = std::env::current_dir()?;
let out = mdtask_core::run_captured(&files, "pdf", &["notes/a.md".to_string()], &cwd)?;
if out.status.success() {
    print!("{}", String::from_utf8_lossy(&out.stdout));
}
```

`run_captured` builds and spawns the whole chain for you and hands back a single
`std::process::Output`, so a TUI can run it on a worker and keep execution off its
render thread. An MCP or agent host calls `run_agent` instead, which adds the
allow gate and the injection guard.

## Status

Published on crates.io: [`mdtask`](https://crates.io/crates/mdtask) (the CLI) and
[`mdtask-core`](https://crates.io/crates/mdtask-core) (the library). The parser
and runner are tested and in use; the CLI runs tasks and serves MCP.

0.5 tightened several defaults after a security and usability audit, and one of
them is breaking: inheriting tasks from a parent directory is now opt-in. See
[Finding task files](#finding-task-files).

Consumers: [gloaming](https://github.com/jhheider/gloaming) and penknife.

## Credits

The maskfile format is by [@jacobdeichert](https://github.com/jacobdeichert/mask);
the xc format and its metadata conventions are by
[@joerdav](https://github.com/joerdav/xc). `mdtask` is an independent
implementation that drew on both; it is not affiliated with or compatible with
either.

## License

MIT OR Apache-2.0.
