# Changelog

Only released tags are releases. The entries below describe the current
unreleased snapshot and do not imply a historical release schedule.

## Unreleased

### Added

- A bounded, version-aware bytecode reader, typed inventory, and diagnostics.
- Luau v3-v14 layouts, including feedback slots, size-prefixed prototypes,
  double-vector constants, and protected-call fallback handling.
- Lua 5.1 parsing and source recovery for supported little-endian chunks.
- CLI decompile, disassemble, deserialize, inspect, and Python parity commands.
- A native HTTP server accepting bounded raw and multipart bytecode requests.
- Custom opcode maps validated against their operand and AUX layouts.
- Static-data recovery and stabilized CFG/SSA/AST recovery from Medal.
- Authored semantic regressions for loops, closures, multiple returns,
  short-circuiting, side effects, and register spilling.
- Locked builds, contributor guidance, CI, and tagged binary packaging.

### Known limitations

- Recompilable output is not proof of equivalent behavior for arbitrary scripts.
- Stripped original names cannot be recovered reliably.
- `CMPPROTO` is inspectable but not safely source-liftable.
- LuaJIT and PUC Lua 5.2-5.4 bytecode are not supported.
- Runtime-specialized or protected layouts can require an explicit opcode map
  or be rejected when the decoder cannot preserve their structure safely.

### Attribution

This project preserves Medal's existing commit history and MIT license.
