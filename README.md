# mdtask

**An embeddable, execution-capable, markdown task runner for Rust.** Define your
tasks in the same markdown you already write, whether a `tasks.md`, a
`maskfile.md`, or a project `README.md`, and run them from a library, a CLI, or
(later) an agent-safe MCP surface.

```markdown
# Tasks

Env: PKGX_DISABLE_UPDATE=1        # hoisted to every task

## build

Build the release binary.

```sh
cargo build --release
```

## pdf

Render a note to PDF. It runs where you invoke it, so `mdtask pdf notes/a.md`
writes next to the note, with no `Dir:` needed.

Args: file

```sh
pandoc -t pdf -o "${file%md}pdf" "$file"
```
```

```console
$ mdtask                 # list tasks (walks up the tree; see below)
$ mdtask build           # run one
$ mdtask pdf notes/a.md  # positional args fill `Args:` in order
$ mdtask lint            # shellcheck the shell scripts it finds
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
library** (`mdtask-core`) that anything can embed. The CLI is a thin wrapper, and
an MCP mode is a later afterthought. The library is the point.

It is **not** compatible with any one of them. It borrows conventions (xc's clean
`Key: value` metadata vocabulary, mask's per-fence interpreter and positional
args) and reads many xc and mask task files as-is, but it owns its grammar and
makes no round-trip promise.

## The format

- **A task is a heading** (`## name`); the **first fenced block** under it is the
  script, and the fence language picks the interpreter (`sh`, `bash`, `zsh`,
  `fish`, `python`, `ruby`, `node`, with an unlabeled fence running as `sh`).
- **A heading with no script is a section**, not a task, so a `# Tasks` container
  is fine.
- **Metadata** is `Key: value` lines in the task body (case-insensitive):
  - `Args:` declares positional arguments in just's syntax. A bare `name` is
    **required**, `name='default'` is **optional** (that value when omitted), and
    a trailing `*name` is **variadic** (it collects the rest, space-joined). They
    are passed on the CLI in order, or prompted by an embedder. Each is
    substituted as `{{ name }}` in the script **and** exported as `$name`. Prefer
    **`$name`** in scripts, because the shell quotes it (`"$name"` is
    injection-safe, and `${name%md}` works). **`{{ name }}` is raw text
    substitution**, applied before the shell parses the script, so `"{{ name }}"`
    is *not* safe for untrusted values. Reserve `{{ }}` for `Dir:` and
    developer-authored templates.
  - `Dir:` overrides the working directory. **Without it, a task runs in the
    directory you invoke it from**, so an inherited or global task acts on your
    current project rather than on wherever the task file happens to live. A
    *relative* value resolves against the **task file's own** directory (`Dir: .`
    pins the task there, the inverse of just's `[no-cd]`); an *absolute* value is
    used verbatim. It may use `{{ arg }}`, and resolution never touches the
    filesystem.
  - `Env:` adds environment (`KEY=VALUE, KEY2=VALUE2`). An `Env:` under a section
    heading is **hoisted** to every task, regardless of position.
  - `Requires:` lists task dependencies (parsed; sequencing is the caller's for
    now).
  - `Agent: allow` opts a task in to an MCP or agent surface. It is **off by
    default**, and a caller must filter with `TaskFile::agent_tasks()` (the
    enforcement point), so handing a task file to an agent never exposes ungated
    shell.

The parser is line-based (no CommonMark dependency), so a `#` or `Key:` inside a
fenced block is never mistaken for structure. Parsing is infallible but records
problems (an unterminated fence, a duplicate task, an unknown interpreter) in
`TaskFile::warnings`; surface them rather than trusting silence.

### Finding task files

The CLI walks **up** from the current directory (like `make`, `just`, and `xc`),
taking the first `tasks.md`, `maskfile.md`, or `README.md` in each ancestor that
defines a task. Nearer files **shadow** farther ones by task name, like just's
`set fallback`, so a project inherits a baseline of tasks from its parents and
overrides them where it wants. (`mdtask-core::parse` itself does no filesystem
access; an embedder with its own project root just calls it directly.)

### Linting

`mdtask lint [TASK]` runs [shellcheck](https://www.shellcheck.net/) over the
shell scripts it finds (all tasks, or one named task). It only checks POSIX-sh
family fences (`sh`, `bash`), since shellcheck cannot analyze `zsh`, `fish`, or a
non-shell interpreter; those are skipped with a note. mdtask does not bundle
shellcheck: it finds one on your `PATH`, falls back to `pkgx shellcheck`, and
otherwise tells you to install it. `SC2154` (referenced but not assigned) is
suppressed, because mdtask exports args and `Env:` as shell variables.

## Embedding

```rust
use std::path::Path;

let tf = mdtask_core::parse(&std::fs::read_to_string("tasks.md")?);
let task = tf.task("pdf").expect("a pdf task");

// Bind positional values to the task's Args (defaults + variadic), or build
// the map yourself from an embedder's prompts.
let args = mdtask_core::TaskFile::bind(task, &["notes/a.md".into()])?;

// `cwd` is where the task runs by default; the second path anchors a relative
// `Dir:` (the task file's own directory). Pass `None` to fall back to cwd.
let cwd = std::env::current_dir()?;
let inv = tf.invocation(task, &args, &cwd, Some(Path::new(".")))?;
// `inv` is { program, args, env, cwd }. Run it on your own worker or thread,
// or call `inv.run()` for a blocking convenience.
```

`mdtask-core` builds the [`Invocation`] for you and stays out of the way of
*when* and *how* you run it, which is what lets a TUI keep execution off its
render thread.

## Status

Early. The core parser and invocation builder are solid and tested; the CLI is
minimal; MCP is planned. In-tree consumers: [gloaming](https://github.com/jhheider/gloaming)
and penknife.

## Credits

The maskfile format is by [@jacobdeichert](https://github.com/jacobdeichert/mask);
the xc format and its metadata conventions are by
[@joerdav](https://github.com/joerdav/xc). `mdtask` is an independent
implementation that drew on both; it is not affiliated with or compatible with
either.

## License

MIT OR Apache-2.0.
