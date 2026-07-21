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
Dir: {{ file }}

```sh
pandoc -t pdf -o "${file%md}pdf" "{{ file }}"
```
```

```console
$ mdtask                 # list tasks
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
    `$name`, so `${name%md}`-style shell expansion works too.
  - `Dir:` — the working directory (relative to the run root; may use `{{ arg }}`;
    if it resolves to a file, its parent is used — the `[no-cd]` idea).
  - `Env:` — extra environment (`KEY=VALUE, KEY2=VALUE2`). An `Env:` under a
    section heading is **hoisted** to every task.
  - `Requires:` — task dependencies (parsed; sequencing is the caller's for now).
  - `Agent: allow` — opt a task in to an MCP/agent surface. **Off by default**,
    so handing a task file to an agent never exposes arbitrary shell.

The parser is line-based (no CommonMark dependency), so a `#` or `Key:` inside a
fenced block is never mistaken for structure.

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
