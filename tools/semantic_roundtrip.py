#!/usr/bin/env python3

from __future__ import annotations

import argparse
import dataclasses
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


@dataclasses.dataclass(frozen=True)
class Case:
    name: str
    source: str


def spill_case() -> str:
    declarations = "\n".join(f"        local value{index}" for index in range(1, 131))
    assignments = "\n".join(
        f"            value{index} = seed + {index}" for index in range(1, 131)
    )
    additions = "\n".join(
        f"        total += value{index} or 0" for index in range(1, 131)
    )
    return f"""
local function stress(seed)
    local total = 0
    for iteration = 1, 2 do
{declarations}
        if iteration == 1 then
{assignments}
        end
{additions}
    end
    return total
end

print(stress(7))
"""


CASES = (
    Case(
        "branches",
        """
local function classify(value)
    if value < 0 then
        return "negative"
    elseif value == 0 then
        return "zero"
    elseif value % 2 == 0 then
        return "positive-even"
    else
        return "positive-odd"
    end
end

for value = -2, 3 do
    print(value, classify(value))
end
""",
    ),
    Case(
        "loops",
        """
local values = { 3, 7, 2, 9, 4 }
local total = 0
for index, value in ipairs(values) do
    if index == 2 then
        continue
    end
    total += value
    if total > 12 then
        break
    end
end

local counter = 0
while counter < 3 do
    counter += 1
    total += counter
end

repeat
    total -= 2
until total <= 10

print(total, counter)
""",
    ),
    Case(
        "short_circuit",
        """
local events = {}
local function mark(name, result)
    table.insert(events, name)
    return result
end

local first = mark("a", false) and mark("b", true) or mark("c", true)
local second = mark("d", true) or mark("e", false)
local third = not mark("f", false) and mark("g", true)
print(first, second, third, table.concat(events, ","))
""",
    ),
    Case(
        "reducer_side_effects",
        """
local duplicateCalls = 0
local function repeated()
    duplicateCalls += 1
    return false
end

local duplicate = repeated() or repeated()

local lengthCalls = 0
local sized = setmetatable({}, {
    __len = function()
        lengthCalls += 1
        return 0
    end,
})
local lengthWasTruthy = false
if #sized then
    lengthWasTruthy = true
end

local guardCalls = 0
local function guard(value)
    guardCalls += 1
    return value
end
if guard(true) and false then
    error("unreachable")
end
if guard(false) or true then
    guardCalls += 10
end

print(duplicate, duplicateCalls, lengthWasTruthy, lengthCalls, guardCalls)
""",
    ),
    Case(
        "comparison_semantics",
        """
local nan = 0 / 0
print(not (nan > 0), not (nan >= 0), not (nan < 0), not (nan <= 0))

local events = {}
local metatable = {}
metatable.__eq = function(left, right)
    table.insert(events, left.tag .. right.tag)
    return true
end
local function make(tag)
    table.insert(events, "make" .. tag)
    return setmetatable({ tag = tag }, metatable)
end

local right = make("R")
local equal = make("L") == right
print(equal, table.concat(events, ","))
""",
    ),
    Case(
        "precomputed_short_circuit",
        """
local events = {}
local function mark(name, value)
    table.insert(events, name)
    return value
end

local function andCase(condition)
    local value = mark("and", 7)
    return condition and value
end

local function orCase(condition)
    local value = mark("or", 9)
    return condition or value
end

print(andCase(false), orCase(true), table.concat(events, ","))
""",
    ),
    Case(
        "multiple_returns",
        """
local function values(seed)
    return seed + 1, seed + 2, seed + 3
end

local first, second, third = values(10)
first, second = second, first
local packed = { values(20) }
print(first, second, third, #packed, packed[1], packed[3])
""",
    ),
    Case(
        "operator_precedence",
        """
local function calculate(a, b, c)
    print(
        a - (b - c),
        (a - b) - c,
        a / (b / c),
        (a / b) / c,
        a ^ (b ^ c),
        (a ^ b) ^ c,
        -(a ^ b),
        (-a) ^ b,
        not (a < b and b < c)
    )
end
calculate(8, 3, 2)
""",
    ),
    Case(
        "varargs",
        """
local function pack(...)
    return select("#", ...), { ... }
end
local count, values = pack("a", nil, "c")
print(count, #values, values[1], tostring(values[2]), values[3])

local function forward(...)
    return ...
end
print(forward(1, 2, 3))
""",
    ),
    Case(
        "evaluation_order",
        """
local events = {}
local function step(name, value)
    table.insert(events, name)
    return value
end

local first, second = step("rhs1", 1), step("rhs2", 2)
local object = {}
object[step("key", 1)] = step("value", 3)
print(first, second, object[1], table.concat(events, ","))
""",
    ),
    Case(
        "closures",
        """
local function counter(start)
    local value = start
    return function(step)
        value += step
        return value
    end
end

local left = counter(4)
local right = counter(20)
print(left(2), left(5), right(-3), left(1))
""",
    ),
    Case(
        "nested_control",
        """
local result = {}
for outer = 1, 4 do
    local inner = 0
    repeat
        inner += 1
        if inner == 2 and outer % 2 == 0 then
            continue
        end
        if outer == 4 and inner == 3 then
            break
        end
        table.insert(result, `{outer}:{inner}`)
    until inner >= 3
end
print(table.concat(result, "|"))
""",
    ),
    Case(
        "protected_calls",
        """
local calls = 0
local function invoke(...)
    calls += 1
    return ...
end
local function values()
    return 7, nil, 11
end
local function fail()
    error("expected", 0)
end
local function handler(message)
    return "handled:" .. tostring(message)
end
local success, first, second, third = pcall(invoke, values())
local failed, message = pcall(fail)
local handled, failure = xpcall(fail, handler)
print(success, first, second, third, calls)
print(failed, message, handled, failure)
""",
    ),
    Case("spill_scope_reset", spill_case()),
)


def require_tool(name: str, override: str | None = None) -> str:
    path = override or shutil.which(name)
    if path is None:
        raise SystemExit(f"missing required executable: {name}")
    return path


def run(command: list[str], *, capture_bytes: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=not capture_bytes,
    )


def describe_failure(label: str, result: subprocess.CompletedProcess) -> str:
    stderr = result.stderr.decode(errors="replace") if isinstance(result.stderr, bytes) else result.stderr
    stdout = result.stdout.decode(errors="replace") if isinstance(result.stdout, bytes) else result.stdout
    return f"{label} exited {result.returncode}\nstdout:\n{stdout}\nstderr:\n{stderr}"


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare Luau source behavior with decompiled output")
    parser.add_argument(
        "--decompiler",
        default=os.environ.get(
            "LUADICROUS_RS",
            str(ROOT / "target" / "release" / "luadicrous-rs"),
        ),
    )
    parser.add_argument("--luau")
    parser.add_argument("--luau-compile")
    parser.add_argument("--optimization", type=int, action="append", choices=(0, 1, 2))
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()

    decompiler = require_tool("luadicrous-rs", args.decompiler)
    luau = require_tool("luau", args.luau)
    luau_compile = require_tool("luau-compile", args.luau_compile)
    optimizations = args.optimization or [0, 1, 2]
    failures: list[str] = []

    temporary = tempfile.TemporaryDirectory(prefix="luadicrous-semantic-")
    work = Path(temporary.name)
    for optimization in optimizations:
        for case in CASES:
            stem = f"{case.name}-O{optimization}"
            source_path = work / f"{stem}.luau"
            bytecode_path = work / f"{stem}.luac"
            decompiled_path = work / f"{stem}.decompiled.luau"
            source_path.write_text(case.source.strip() + "\n")

            original = run([luau, f"-O{optimization}", str(source_path)])
            if original.returncode != 0:
                failures.append(describe_failure(f"{stem}: source", original))
                continue

            compiled = run(
                [luau_compile, "--binary", f"-O{optimization}", "-g0", str(source_path)],
                capture_bytes=True,
            )
            if compiled.returncode != 0:
                failures.append(describe_failure(f"{stem}: compile", compiled))
                continue
            bytecode_path.write_bytes(compiled.stdout)

            decompiled = run(
                [decompiler, "decompile", str(bytecode_path), "-o", str(decompiled_path)]
            )
            if decompiled.returncode != 0:
                failures.append(describe_failure(f"{stem}: decompile", decompiled))
                continue

            recovered = run([luau, f"-O{optimization}", str(decompiled_path)])
            if recovered.returncode != 0:
                failures.append(describe_failure(f"{stem}: recovered", recovered))
                continue
            if recovered.stdout != original.stdout:
                failures.append(
                    f"{stem}: output mismatch\n"
                    f"expected:\n{original.stdout}"
                    f"actual:\n{recovered.stdout}"
                    f"decompiled: {decompiled_path}"
                )
                continue
            print(f"PASS {stem}")

    if args.keep or failures:
        temporary._finalizer.detach()
        print(f"artifacts: {work}")
    else:
        temporary.cleanup()

    if failures:
        print("\n\n".join(failures), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
