# Luadicrous RS

[![CI](https://github.com/JonathanSigmund/luadicrous/actions/workflows/ci.yml/badge.svg)](https://github.com/JonathanSigmund/luadicrous/actions/workflows/ci.yml)

An experimental, correctness-first Lua and Luau decompiler written in stable Rust.

This repository is a fork of [Medal](https://github.com/shrimp-nz/medal), but the
active implementation is intentionally being rebuilt around a different contract:

1. preserve serialized facts before attempting recovery;
2. use a typed, loss-aware IR between the bytecode and source layers;
3. score recovered structure instead of silently guessing;
4. compare every Rust milestone against the Python Luadicrous reference corpus;
5. never trade constant correctness for wider byte coverage.

The original Medal CFG/SSA/AST crates have been stabilized, moved onto released
`petgraph`, and integrated behind Luadicrous' strict parser and validation policy.

## Current milestone

The current milestone provides:

- a bounded Luau byte reader;
- PUC Lua 5.1 deserialization for little-endian chunks with 32-bit or 64-bit
  `size_t`, plus CFG/SSA source recovery and recursive disassembly;
- strict standard/Roblox constant-stream parsing;
- preservation of strings, type blobs, instructions, constants, line data, and
  debug locals;
- a typed wire-to-IR lowering stage;
- objective structural validation;
- version-aware canonical, legacy-v6, and scrambled-v7 opcode decoding;
- verified Luau v10 class-shape parsing and validation;
- Luau v11 feedback slots and `CALLFB` decoding, plus v12 size-prefixed
  prototypes, inline costs, and preserved extension bytes;
- Luau v13 double-vector constants preserved without reducing their precision;
- Luau v14 `FASTPCALL` validation and recovery through the ordinary protected-call fallback;
- native, validated custom raw-opcode table normalization;
- constant, closure, child-prototype, AUX-word, and jump-target validation;
- a linear-time static-data evaluator and deterministic Luau emitter;
- stable-Rust CFG construction, SSA recovery, region structuring, and AST emission;
- a flat Luau state-machine fallback for irreducible or exceptionally large CFGs;
- per-function panic recovery that preserves the raw register graph;
- bounded temporary spilling for Luau's 200-local source limit;
- exact lowering of numeric and generic low-level loop nodes;
- deterministic local-variable identities; timing watermarks can vary between runs;
- faithful decoded disassembly for runtime-dependent chunks;
- an `inspect` CLI with human and JSON output;
- a Python-oracle parity command.
- a native Axum HTTP server with bounded uploads, timed responses, queue limits,
  CORS configuration, multipart/raw uploads, and graceful shutdown.

Static payloads use the linear evaluator. Runtime code uses CFG/SSA recovery when
safe, with register-faithful fallback for functions whose graph complexity would
make SSA quadratic. Release panics unwind into that fallback instead of terminating
the process.

`CALLFB` lifts as an ordinary call while its feedback metadata remains available
in the parsed model.
`CMPPROTO` is decoded and disassembled, but source recovery stops rather than
guessing when this runtime-only proto-identity guard appears.

## Build

```sh
cargo build --workspace
cargo test --workspace
```

For a reproducible release build, use the checked-in lockfile:

```sh
cargo build --release --locked -p luadicrous-cli -p luadicrous-server
cargo test --release --workspace --locked
```

Rust 1.88 or newer is required. The CLI is `target/release/luadicrous-rs`;
the HTTP server is `target/release/luadicrous-server`.

## Inspect bytecode

```sh
cargo run -p luadicrous-cli -- inspect path/to/script.luac
cargo run -p luadicrous-cli -- inspect path/to/script.luac --json
cargo run -p luadicrous-cli -- deserialize path/to/script.luac --json
```

## Decompile or disassemble

```sh
cargo run -p luadicrous-cli -- decompile path/to/script.luac -o output.lua
cargo run -p luadicrous-cli -- disassemble path/to/script.luac -o output.asm
cargo run -p luadicrous-cli -- decompile obfuscated.luac \
  --opcode-table opcode-table.json -o output.lua
```

Format detection is automatic. PUC Lua support currently targets official Lua
5.1 chunks using little-endian 32-bit instructions and IEEE-754 doubles; LuaJIT
bytecode and PUC Lua 5.2-5.4 are reported as unsupported rather than guessed.

## Native HTTP server

```sh
cargo run --release -p luadicrous-server
curl --data-binary @script.luac \
  -H 'Content-Type: application/octet-stream' \
  http://127.0.0.1:8000/luadicrous/decompile
```

Routes are available both with and without the `/luadicrous` prefix:

- `GET /luadicrous/health`
- `POST /luadicrous/decompile`
- `POST /luadicrous/disasm`

Responses include `X-Luadicrous-Engine: rust-native` and
`X-Luadicrous-Time`. Decompiled source also includes elapsed time in its
watermark. Raw bytecode and multipart `file` uploads are accepted.

Custom opcode tables can be supplied through the
`X-Luadicrous-Opcode-Table` JSON header or multipart `opcode_table` /
`opcode_table_json` field. Accepted forms match Python Luadicrous, including
`raw_to_opcode`, `opcode_to_raw`, direct mappings, and indexed lists. Every
used raw opcode must be mapped, and declared operand/AUX layouts must match the
canonical opcode before normalization is accepted.

Server controls:

- `HOST`/`PORT` or `LUADICROUS_HOST`/`LUADICROUS_PORT`
- `LUADICROUS_MAX_UPLOAD_BYTES` and `LUADICROUS_MAX_OUTPUT_BYTES`
- `LUADICROUS_ENGINE_TIMEOUT` and `LUADICROUS_QUEUE_TIMEOUT`
- `LUADICROUS_MAX_CONCURRENT`
- `LUADICROUS_CORS_ORIGINS`

The static-data path reconstructs generated tables without entering the CFG
pipeline. This repository does not currently publish a reproducible performance
comparison against other decompilers.

## Verification

The current snapshot passed 87 Rust workspace tests in a local release build:

```sh
cargo test --release --workspace --locked
```

The public semantic harness contains 14 authored examples. It compiles them,
decompiles them, and compares observable behavior at optimization levels 0, 1,
and 2. All 42 comparisons passed locally with Luau 0.739:

```sh
python3 tools/semantic_roundtrip.py
```

Install `luau` and `luau-compile` first, or pass their paths with `--luau` and
`--luau-compile`. The suite does not redistribute extracted game scripts.

These checks establish the behavior of those specific test cases, not arbitrary
input equivalence. Recompilation alone checks syntax, not runtime correctness.
Stripped original names cannot be recovered reliably, and `CMPPROTO` guards
remain unsupported for source recovery. CI results are reported by GitHub
Actions separately; local results are not a claim that every hosted runner passes.

Set `LUADICROUS_RS_DEBUG_LIFTER=1` to print per-prototype lift and fallback
decisions without changing normal output.
Set `LUADICROUS_RS_DEBUG_TIMINGS=1` to print phase-level lifter timings, and
`LUADICROUS_DEBUG_UPVALUES=1` to restore closure-capture comments for diagnostics.

## Compare with Python Luadicrous

```sh
cargo run -p luadicrous-cli -- parity path/to/script.luac \
  --python-root ../luadicrous \
  --python-runtime pypy3
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and
[`docs/PARITY.md`](docs/PARITY.md) for the migration plan and acceptance gates.

## Heritage

Luadicrous RS retains Medal's MIT license and history. The original Medal README
credited Jujhar Singh (KowalskiFX) and Mathias Pedersen (Costomality); this fork
continues to preserve that attribution and their work.

## Contributing and releases

See [CONTRIBUTING.md](CONTRIBUTING.md), [CHANGELOG.md](CHANGELOG.md), and
[SECURITY.md](SECURITY.md). Automated checks run on pushes and pull requests.
Release tags matching `v*` build Linux, macOS, and Windows archives, including
the CLI, native server, and original license. A tagged build is not a guarantee
of semantic recovery for every possible input.
