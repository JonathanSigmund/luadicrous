# Contributing

Luadicrous is correctness-first. Fix serialized facts and observable behavior
before changing names, whitespace, or other presentation details.

## Development

Use Rust 1.88 or newer and keep `Cargo.lock` checked in. Normal builds and tests
do not require the Python reference implementation or a Roblox installation.

```sh
cargo build --release --locked -p luadicrous-cli -p luadicrous-server
cargo test --release --workspace --locked
```

When `luau` and `luau-compile` are available, also run:

```sh
python3 tools/semantic_roundtrip.py
```

The Python comparison command is optional and requires a separate checkout of
the Python reference. Do not add private local paths or downloaded game dumps
as required public test dependencies.

## Changes and evidence

- Add a minimal, authored regression for a bug whenever practical.
- Distinguish parsing, successful recompilation, and runtime equivalence.
- Preserve call counts, evaluation order, closures, and constant precision.
- Reject unsupported layouts explicitly rather than replacing them with guesses.
- Keep public interfaces compatible or document their changes.
- Preserve the original Medal license and attribution.

Use meaningful commits that describe real work. Do not split an existing
snapshot into an artificial maintenance history or backdate release activity.

## Reporting a bug

Include the bytecode version, encoding, invocation, diagnostics, and the
smallest source-generated example that reproduces the problem. Say which
compiler and optimization/debug settings produced it. Share a bytecode sample
only when you have permission to distribute it. Do not upload account tokens,
private scripts, proprietary game assets, or personal filesystem details.

## Releasing

1. Run the Rust and semantic gates and record their results.
2. Review the staged file list for secrets, generated dumps, and binary builds.
3. Update `CHANGELOG.md` with verified changes and known limitations.
4. Confirm the destination repository and account before pushing.
5. Create a version tag only when its release is intended. The release workflow
   publishes prerelease CLI/server archives after platform tests pass.

The macOS desktop application lives in the Python companion checkout and is
not included in the Rust CLI/server release workflow.
