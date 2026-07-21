# mdtask

**An embeddable, execution-capable, markdown task runner for Rust.** Define your
tasks in the same markdown you already write — a `tasks.md`, a `maskfile.md`, or
a project `README.md` — and run them from a library, a CLI, or (later) an
agent-safe MCP surface.

```markdown
# Tasks

Env: PKGX_DISABLE_UPDATE=1        ← hoisted to every task

## build

Build the release binary.

```sh
cargo build --release
```

## pdf

Render a note to PDF, in its own folder.

Args: file
Dir: dirname({{ file }})

```sh
pandoc -t pdf -o "${file%md}pdf" "$file"
```
```

```console
$ mdtask                 # list tasks (walks up the tree; see below)
$ mdtask build           # run one
$ mdtask pdf notes/a.md  # positional args fill `Args:` in order
```

## Why this exists (honestly)

I wanted an **embeddable Rust library** that parses *and runs* markdown-defined
tasks — so an editor, a TUI, or an agent host could offer "run the tasks in this
file" without shelling out to another tool. I went looking, and found nothing
that fit:

- **xc**, **Task**, **runme** — excellent, but Go: no Rust you can link.
- **just** — Rust, but a Makefile-style DSL, not markdown.
- **mask** — has a Rust crate (`mask-parser`), but it's *parse-only*,
  maskfile-locked, and hasn't been touched since 2024.

So `mdtask` is the million-and-first task runner — with no apology. It collects
the features I liked best from the others into a small, dependency-light **core
library** (`mdtask-core`) that anything can embed. The CLI is a thin wrapper; an
MCP mode is a later afterthought. The library is the point.

It is **not** compatible with any one of them. It borrows conventions — xc's
clean `Key: value` metadata vocabulary, mask's per-fence interpreter and
positional args — and reads many xc/mask task files as-is, but it owns its
grammar and makes no round-trip promise.

## The format

- **A task is a heading** (`## name`); the **first fenced block** under it is the
  script, and the fence language picks the interpreter (`sh`, `bash`, `zsh`,
  `fish`, `python`, `ruby`, `node` — unlabeled runs as `sh`).
- **A heading with no script is a section**, not a task (so a `# Tasks` container
  is fine).
- **Metadata** is `Key: value` lines in the task body (case-insensitive):
  - `Args:` — positional argument names. Passed on the CLI in order; prompted by
    an embedder. Substituted as `{{ name }}` in the script **and** exported as
    `$name`. Prefer **`$name`** in scripts — the shell quotes it (`"$name"` is
    injection-safe, `${name%md}` works). **`{{ name }}` is raw text substitution**
    (before the shell parses the script), so `"{{ name }}"` is *not* safe for
    untrusted values; reserve `{{ }}` for `Dir:` and developer-authored templates.
  - `Dir:` — the working directory (relative to the run root; may use `{{ arg }}`).
    `dirname(PATH)` takes a path's **lexical** parent — `Dir: dirname({{ file }})`
    runs in the file's folder (the `[no-cd]` idea) and works whether or not the
    file exists yet. Resolution never touches the filesystem.
  - `Env:` — extra environment (`KEY=VALUE, KEY2=VALUE2`). An `Env:` under a
    section heading is **hoisted** to every task (regardless of position).
  - `Requires:` — task dependencies (parsed; sequencing is the caller's for now).
  - `Agent: allow` — opt a task in to an MCP/agent surface. **Off by default**;
    a caller must filter with `TaskFile::agent_tasks()` (the enforcement point) so
    handing a task file to an agent never exposes ungated shell.

The parser is line-based (no CommonMark dependency), so a `#` or `Key:` inside a
fenced block is never mistaken for structure. Parsing is infallible but records
problems (an unterminated fence, a duplicate task, an unknown interpreter) in
`TaskFile::warnings` — surface them rather than trusting silence.

### Finding task files

The CLI walks **up** from the current directory (like `make`/`just`/`xc`),
taking the first `tasks.md` / `maskfile.md` / `README.md` in each ancestor that
defines a task. Nearer files **shadow** farther ones by task name — like just's
`set fallback` — so a project inherits a baseline of tasks from its parents and
overrides them where it wants. (`mdtask-core::parse` itself does no filesystem
access; an embedder with its own project root just calls it directly.)

## Embedding

```rust
use std::collections::BTreeMap;
use std::path::Path;

let tf = mdtask_core::parse(&std::fs::read_to_string("tasks.md")?);
let task = tf.task("pdf").expect("a pdf task");

let mut args = BTreeMap::new();
args.insert("file".into(), "notes/a.md".into());

let inv = tf.invocation(task, &args, Path::new("."))?;
// `inv` is { program, args, env, cwd } — run it on your own worker/thread,
// or `inv.run()` for a blocking convenience.
```

`mdtask-core` builds the [`Invocation`] for you and stays out of the way of
*when* and *how* you run it — which is what lets a TUI keep execution off its
render thread.

## Status

Early. The core parser + invocation builder is solid and tested; the CLI is
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
