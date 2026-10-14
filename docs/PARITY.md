# Python parity

Python Luadicrous remains the reference implementation during the Rust migration.

The parity inventory currently compares:

- Luau and types versions;
- string-table size;
- prototype count;
- declared main prototype;
- consumed and trailing bytes;
- instruction count per prototype;
- constant count per prototype.

Rust additionally validates opcode encodings, AUX-word boundaries, jump targets,
and instruction-level constant/closure references. These checks are intentionally
not reduced to Python parity because they can expose reference-parser defects.

Run a single-file comparison with:

```sh
cargo run -p luadicrous-cli -- parity file.luac \
  --python-root ../luadicrous
```

## Acceptance gates

A Rust stage is not authoritative until:

1. all ordinary corpus files match the Python inventory;
2. every mismatch has a stored diagnostic and reproducible fixture;
3. malformed inputs fail deterministically without panic;
4. hard/protected files never show worse early-constant quality;
5. release builds are benchmarked separately from correctness decisions.

Strict parsing remains authoritative. Protected boundary recovery may only replace
it when objective constant, opcode-reference, child-reference, main-root, and
trailing-byte checks all improve.

## Source round-trip gate

Correctness-safe source strategies must pass this stronger gate:

1. decompile the original chunk;
2. compile the emitted source with `luau-compile --binary`;
3. decompile that compiled chunk again;
4. require byte-for-byte identical normalized source.

The decoded Luraph static-program fixture passes this gate. The runtime corpus adds
a syntax/compile gate: 99 workspace chunks and ten hard canaries must all compile
without unknown opcodes, placeholder constants, internal loop markers, Lua
`goto`/labels, or leaked register names.
