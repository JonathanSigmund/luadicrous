#!/usr/bin/env python3
"""Emit a stable structural inventory from Python Luadicrous."""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path)
    parser.add_argument("--python-root", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    python_root = args.python_root.resolve()
    sys.path.insert(0, str(python_root))

    from luadicrous import deserialize

    bytecode = args.input.read_bytes()
    noise = io.StringIO()
    with contextlib.redirect_stdout(noise), contextlib.redirect_stderr(noise):
        result = deserialize(bytecode)

    protos, main_idx, strings, version, types_version, consumed, *_ = result
    inventory = {
        "luau_version": version,
        "types_version": types_version,
        "string_count": len(strings),
        "proto_count": len(protos),
        "main_proto": main_idx,
        "consumed_bytes": consumed,
        "trailing_bytes": max(0, len(bytecode) - consumed),
        "instruction_counts": [len(proto.instructions) for proto in protos],
        "constant_counts": [len(proto.constants) for proto in protos],
    }
    json.dump(inventory, sys.stdout, separators=(",", ":"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
