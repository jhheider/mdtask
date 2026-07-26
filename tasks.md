# mdtask tasks

mdtask's own task file, run with mdtask. If a task here is awkward to write,
that is the tool telling you something.

Note there is no `Opts: include-parent`: this is a leaf, and nothing above the
repository should be able to supply or shadow a task name in it.

## check

Everything CI checks, in the order that fails fastest. Run this before pushing.

`--all-features` matters: `mcp` is off by default, so without it the MCP surface
compiles in CI and not here, which is the wrong way round for a pre-push gate.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## fmt

Format everything, in place.

```sh
cargo fmt --all
```

## test

The test suite, including the `mcp` feature.

Takes an optional filter, so `mdtask test requires` runs just the dependency
tests.

Args: filter=''

```sh
cargo test --workspace --all-features -- "$filter"
```

## lint

Clippy over every target, warnings denied, the way CI runs it.

```sh
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## docs

Build the docs exactly as docs.rs will, with broken intra-doc links denied.

Worth its own task because a link that resolves inside the crate can still break
on docs.rs, and nothing else in the gate catches it: `cargo doc` warnings are not
part of CI, so they surface first as broken links on a published page.

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

## audit

Check dependencies for advisories, as the weekly workflow does.

```sh
cargo audit
```

## msrv

Verify the declared `rust-version` still builds. Slow: it bisects real
toolchains, so this is a release-time check, not a per-commit one.

```sh
cargo msrv verify --path mdtask-core
cargo msrv verify --path mdtask
```

## demo

Run mdtask against this very file, through the CLI, as a smoke test of the parse
and listing paths.

`inherit-cwd` is not set, so it runs in the repository root regardless of where
you invoke it from.

```sh
cargo run --quiet -- --file tasks.md
```

## release

Tag a release. The workflow does the rest: it builds the five target binaries,
publishes both crates in dependency order, and refreshes the homebrew formula.

Both crate versions and the tag have to agree, so this reads the version rather
than taking it as an argument.

Requires: check

```sh
ver=$(edikt '.package.version' mdtask-core/Cargo.toml)
cli=$(edikt '.package.version' mdtask/Cargo.toml)

test "$ver" = "$cli" || { echo "version mismatch: core $ver, cli $cli"; exit 1; }
test -z "$(git status --porcelain)" || { echo "working tree is dirty"; exit 1; }

echo "tagging v$ver"
git tag -a "v$ver" -m "v$ver"
echo "now: git push origin v$ver, then publish the release on GitHub"
```
