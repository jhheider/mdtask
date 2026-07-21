# Project

Intro prose under a section heading (no fence, so this is not a task).

Env: SHARED=base

## build

Build the release binary.

```sh
cargo build --release
```

## greet

Positional args (one required, one with a default, a trailing variadic),
per-task env, `{{ }}` and `$var` in one script.

Args: name greeting='hello' *extra
Env: MOOD=cheery

```sh
echo "$greeting, {{ name }} ($MOOD) {{ extra }}"
```

## render

`Dir: .` pins the task to the task file's own directory (the inverse of the
default, which runs where the command was invoked).

Args: file
Dir: .

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
