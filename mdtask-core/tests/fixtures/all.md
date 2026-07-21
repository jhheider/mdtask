# Project

Intro prose under a section heading (no fence - so this is not a task).

Env: SHARED=base

## build

Build the release binary.

```sh
cargo build --release
```

## greet

Positional args, per-task env, `{{ }}` and `$var` in one script.

Args: name greeting
Env: MOOD=cheery

```sh
echo "$greeting, {{ name }} ($MOOD)"
```

## render

Run in the target file's own folder via `dirname` (works before it exists).

Args: file
Dir: dirname({{ file }})

```zsh
print -r -- "rendering {{ file }}"
```

## deploy

A dependency and the agent gate.

Requires: build
Agent: allow

```bash
echo deploying
```

## notes

A non-shell interpreter, and a metadata line placed after the fence.

```python
print("hi")
```

Env: LATE=1
