#!/usr/bin/env python3
"""Canonicalize nondeterministic data in an LLVM instrumentation text profile.

Serial training still observes a few randomized registry and runtime-lock
branches, and LLVM may register a varying subset of never-entered functions.
Those records are outside Forge's DSP hot paths but can perturb code layout and
break release reproducibility.  This tool removes original all-zero function
profiles and zeros every counter for an executed function only when that
function's maximum counter is below a fixed threshold.  LLVM value profiles are
removed because their update order is not reproducible even when ordinary
counters are updated atomically.  Hot-function counters remain unchanged.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path


DEFAULT_COLD_THRESHOLD = 10_000
COUNTER_COUNT_MARKER = "# Num Counters:"
COUNTER_VALUES_MARKER = "# Counter Values:"
VALUE_KIND_COUNT_MARKER = "# Num Value Kinds:"


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def canonicalize(text: str, threshold: int) -> tuple[str, int, int, int]:
    if not text.startswith("# IR level Instrumentation Flag\n:ir\n"):
        raise ValueError("input is not an LLVM IR instrumentation text profile")
    lines = text.splitlines(keepends=True)
    canonicalized = 0
    removed_empty_profiles = 0
    removed_value_profiles = 0
    index = 0
    while index < len(lines):
        marker = lines[index].rstrip("\r\n")
        if marker == VALUE_KIND_COUNT_MARKER:
            end = index + 1
            while end < len(lines) and lines[end].strip():
                end += 1
            del lines[index:end]
            removed_value_profiles += 1
            continue
        if marker != COUNTER_COUNT_MARKER:
            index += 1
            continue
        if index + 3 >= len(lines):
            raise ValueError("truncated counter record")
        try:
            count = int(lines[index + 1].strip())
        except ValueError as error:
            raise ValueError("invalid counter count") from error
        if lines[index + 2].rstrip("\r\n") != COUNTER_VALUES_MARKER:
            raise ValueError("counter values marker is missing")
        start = index + 3
        end = start + count
        if end > len(lines):
            raise ValueError("truncated counter values")
        try:
            counters = [int(line.strip()) for line in lines[start:end]]
        except ValueError as error:
            raise ValueError("invalid counter value") from error
        # LLVM may register a different subset of never-entered generic
        # functions between otherwise identical instrumented runs. Missing
        # and all-zero records carry no training evidence, so remove original
        # all-zero records before producing the profile-use input. Executed
        # cold functions that are deliberately zeroed below remain present.
        if counters and not any(counters):
            record_start = index - 3
            if (
                record_start < 2
                or lines[index - 2].rstrip("\r\n") != "# Func Hash:"
            ):
                raise ValueError("function profile header is missing")
            record_end = end
            while record_end < len(lines) and lines[record_end].strip():
                if lines[record_end].rstrip("\r\n") == VALUE_KIND_COUNT_MARKER:
                    removed_value_profiles += 1
                record_end += 1
            if record_end < len(lines):
                record_end += 1
            del lines[record_start:record_end]
            removed_empty_profiles += 1
            index = record_start
            continue
        if counters and max(counters) < threshold and any(counters):
            for counter_index in range(start, end):
                newline = "\r\n" if lines[counter_index].endswith("\r\n") else "\n"
                lines[counter_index] = f"0{newline}"
            canonicalized += 1
        index = end
    return "".join(lines), canonicalized, removed_empty_profiles, removed_value_profiles


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--cold-threshold", type=positive_int, default=DEFAULT_COLD_THRESHOLD
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    source = args.input.expanduser().resolve()
    destination = args.output.expanduser().resolve()
    if source == destination:
        raise ValueError("input and output must be different files")
    rendered, count, removed_empty_profiles, removed_value_profiles = canonicalize(
        source.read_text(encoding="utf-8"), args.cold_threshold
    )
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.tmp-{os.getpid()}")
    temporary.write_text(rendered, encoding="utf-8", newline="")
    os.replace(temporary, destination)
    print(
        f"forge-pgo-canonicalize: zeroed {count} cold functions "
        f"(max counter < {args.cold_threshold}); removed "
        f"{removed_empty_profiles} empty function profiles and "
        f"{removed_value_profiles} value-profile records",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"forge-pgo-canonicalize: error: {error}", file=sys.stderr)
        raise SystemExit(2)
