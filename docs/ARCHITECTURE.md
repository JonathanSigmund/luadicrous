# Architecture

Luadicrous RS is split into narrow stages with explicit loss boundaries.

```text
bytes
  -> luadicrous-bytecode (wire facts, no structural guessing)
  -> luadicrous-ir       (stable semantic vocabulary)
  -> luadicrous-core     (validation, quality, recovery policy)
  -> static evaluator    (large constant payloads)
  -> CFG + SSA           (ordinary runtime code)
  -> raw CFG fallback    (irreducible/oversized functions)
  -> AST + local spiller (Luau-safe source representation)
  -> Luau emitter
```

## Medal integration

Medal's CFG, SSA, and graph-structuring work was originally coupled to:

- nightly-only Rust features;
- a custom archived `petgraph` branch;
- AST nodes used as both IR and final source representation;
- panic-based handling in several deserialization paths;
- a Cloudflare worker delivery crate.

Luadicrous RS now compiles those crates on stable Rust with released `petgraph`.
The strict parser adapter normalizes canonical, legacy-v6, scrambled-v7, and
validated custom opcode-table instructions before they enter the lifter. Assertions
unwind per function; a failed SSA attempt is retried from the untouched raw CFG.

## Stage contracts

### Bytecode

- Every read is bounds checked.
- Allocation counts are capped by `ParseLimits`.
- Strings retain exact bytes and expose lossy UTF-8 separately.
- Strict parsing rejects unsupported tags rather than inventing boundaries.
- Trailing bytes are retained and reported.
- Luau v10 class-shape constants are preserved and reference-validated.
- Luau v11 feedback descriptions and v12 proto-size and cost fields are retained.
- Unknown v12 per-prototype extension bytes stay within their declared boundary.
- Custom opcode tables normalize only instruction words and never AUX payloads.

### IR

- Wire indices use explicit newtypes.
- Raw instruction words remain available after lowering.
- Debug names are evidence, not mandatory truth.
- Diagnostics are data and can be serialized.

### Core

- Parsing and validation are separate.
- A parse may succeed while quality validation reports corruption.
- Main, child, string, closure, and debug-range references are checked.
- Opcode encoding, AUX layout, constant operands, and jump targets are checked.
- Static source is emitted only for fully evaluable, identity-safe value graphs.
- Runtime source uses CFG/SSA only after structural validation.
- Oversized and irreducible graphs use flat state-machine lowering.
- Excess non-captured temporaries spill into a scoped register table.
- Future protected recovery must beat the strict parse on objective metrics.

## Planned migration

1. Preserve ordinary Luau v3-v14 serialized layouts, with explicit validation
   for version-gated opcodes and double-vector constants.
2. Add opcode-table abstraction and instruction validation. **Complete.**
3. Add recompilation-gated static source recovery. **Complete.**
4. Port Medal CFG construction behind golden tests. **Complete.**
5. Introduce SSA without losing raw register provenance. **Complete.**
6. Port region structuring and retain explicit fallback nodes. **Complete.**
7. Add Luau-safe state-machine and local-pressure fallbacks. **Complete.**
8. Expand source emission behind recompilation and corpus gates. **Complete.**
9. Add constant-safe protected boundary candidates for non-strict chunks.
10. Improve semantic naming without weakening register provenance.
