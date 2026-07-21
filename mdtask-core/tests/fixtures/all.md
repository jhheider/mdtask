# Project

Intro prose under a section heading (no fence, so this is not a task).

Env: SHARED=base

## build

Stand-in for a real build (echo, so running it in a test stays hermetic).

```sh
echo "building ($SHARED)"
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

`Opts: inherit-cwd` runs the task where the command was invoked, the inverse of
the default (the task file's own directory).

Args: file
Opts: inherit-cwd

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
