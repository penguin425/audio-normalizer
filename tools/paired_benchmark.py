#!/usr/bin/env python3
"""Paired end-to-end benchmark for PCM spool and analysis-pipeline paths.

The generic benchmark harness measures one Forge binary per fixture. This
specialized harness keeps one deterministic fixture hot and alternates the
baseline and candidate processes in small balanced blocks, so hosted-runner
speed drift affects both binaries equally.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Callable

import benchmark


CASES = ("flac-stereo-normalize", "wav-stereo-resample-normalize")
BASELINE = "baseline"
CANDIDATE = "candidate"
MAX_ROUNDS = 32


def alternating_schedule(rounds: int, *, inverted: bool = False) -> list[str]:
    """Return balanced B-C-C-B / C-B-B-C process ordering."""

    schedule = []
    for round_index in range(rounds):
        baseline_first = (round_index + int(inverted)) % 2 == 0
        if baseline_first:
            schedule.extend((BASELINE, CANDIDATE, CANDIDATE, BASELINE))
        else:
            schedule.extend((CANDIDATE, BASELINE, BASELINE, CANDIDATE))
    return schedule


def paired_round_changes(
    result: dict[str, Any],
    value: Callable[[dict[str, Any]], float],
) -> list[float]:
    """Return one baseline/candidate change for each balanced four-run block.

    Each B-C-C-B or C-B-B-C block brackets both binaries against the same
    short hosted-runner interval. Reducing within the block before comparing
    prevents monotonic runner drift from being misclassified as a binary
    regression, which pooling all baseline and candidate samples separately
    cannot guarantee.
    """

    schedule = result["schedule"]
    baseline_samples = result["baseline_samples"]
    candidate_samples = result["candidate_samples"]
    if len(schedule) % 4 != 0:
        raise ValueError("paired benchmark schedule is not a whole number of blocks")

    baseline_index = 0
    candidate_index = 0
    changes = []
    for start in range(0, len(schedule), 4):
        block = schedule[start : start + 4]
        if block.count(BASELINE) != 2 or block.count(CANDIDATE) != 2:
            raise ValueError("paired benchmark block is not balanced")
        baseline_values = []
        candidate_values = []
        for label in block:
            if label == BASELINE:
                if baseline_index >= len(baseline_samples):
                    raise ValueError("paired benchmark is missing a baseline sample")
                baseline_values.append(value(baseline_samples[baseline_index]))
                baseline_index += 1
            elif label == CANDIDATE:
                if candidate_index >= len(candidate_samples):
                    raise ValueError("paired benchmark is missing a candidate sample")
                candidate_values.append(value(candidate_samples[candidate_index]))
                candidate_index += 1
            else:
                raise ValueError(f"unknown paired benchmark schedule label: {label}")
        baseline_value = statistics.fmean(baseline_values)
        candidate_value = statistics.fmean(candidate_values)
        if baseline_value <= 0.0:
            raise ValueError("paired benchmark baseline value must be positive")
        changes.append((candidate_value / baseline_value - 1.0) * 100.0)

    if baseline_index != len(baseline_samples):
        raise ValueError("paired benchmark has unused baseline samples")
    if candidate_index != len(candidate_samples):
        raise ValueError("paired benchmark has unused candidate samples")
    return changes


def paired_median_change_percent(
    result: dict[str, Any],
    value: Callable[[dict[str, Any]], float],
) -> float:
    """Return the median percent change across balanced schedule blocks."""

    changes = paired_round_changes(result, value)
    if not changes:
        raise ValueError("paired benchmark has no complete blocks")
    return statistics.median(changes)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-forge", type=Path, required=True)
    parser.add_argument("--candidate-forge", type=Path, required=True)
    parser.add_argument("--ffmpeg", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument(
        "--duration-seconds", type=benchmark.positive_int, required=True
    )
    parser.add_argument("--rounds", type=benchmark.positive_int, default=8)
    parser.add_argument(
        "--timeout-seconds", type=benchmark.positive_int, default=7_200
    )
    return parser.parse_args()


def command_for_case(
    case_id: str,
    forge: Path,
    wave_path: Path,
    flac_path: Path,
    output_path: Path,
) -> list[str]:
    if case_id == "flac-stereo-normalize":
        return [str(forge), str(flac_path), "--overwrite", "-o", str(output_path)]
    if case_id == "wav-stereo-resample-normalize":
        return [
            str(forge),
            str(wave_path),
            "--sample-rate",
            "44100",
            "--overwrite",
            "-o",
            str(output_path),
        ]
    raise ValueError(f"unsupported paired benchmark case: {case_id}")


def run_warmup(command: list[str], output_path: Path, timeout_seconds: int) -> None:
    output_path.unlink(missing_ok=True)
    completed = subprocess.run(
        command,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        timeout=timeout_seconds,
    )
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", errors="replace")[-2_000:]
        raise RuntimeError(
            f"paired benchmark warm-up failed with {completed.returncode}: {detail}"
        )


def run_case(
    case_id: str,
    case_index: int,
    workspace: Path,
    binaries: dict[str, Path],
    wave_path: Path,
    flac_path: Path,
    rounds: int,
    timeout_seconds: int,
) -> dict[str, Any]:
    case_dir = workspace / case_id
    case_dir.mkdir()
    output_path = case_dir / "output.wav"
    commands = {
        label: command_for_case(
            case_id, forge, wave_path, flac_path, output_path
        )
        for label, forge in binaries.items()
    }

    # Prime both executable and input pages without measuring them. Reverse the
    # order for the second case so warm-up order cannot favor one binary across
    # the complete report.
    warmup = (
        (BASELINE, CANDIDATE)
        if case_index % 2 == 0
        else (CANDIDATE, BASELINE)
    )
    for label in warmup:
        run_warmup(commands[label], output_path, timeout_seconds)

    schedule = alternating_schedule(rounds, inverted=case_index % 2 == 1)
    samples = {BASELINE: [], CANDIDATE: []}
    for sample_index, label in enumerate(schedule, start=1):
        output_path.unlink(missing_ok=True)
        measurement = benchmark.run_measured(
            commands[label],
            timeout_seconds,
            case_dir / f"stdout-{sample_index:03d}-{label}.log",
            case_dir / f"stderr-{sample_index:03d}-{label}.log",
        )
        if measurement["exit_code"] != 0:
            stderr_path = case_dir / f"stderr-{sample_index:03d}-{label}.log"
            detail = stderr_path.read_text(encoding="utf-8", errors="replace")[-2_000:]
            raise RuntimeError(
                f"{case_id} {label} failed with {measurement['exit_code']}: {detail}"
            )
        samples[label].append(measurement)

    expected = rounds * 2
    if any(len(values) != expected for values in samples.values()):
        raise RuntimeError(f"{case_id}: incomplete paired sample population")
    return {
        "id": case_id,
        "schedule": schedule,
        "samples_per_binary": expected,
        "baseline_samples": samples[BASELINE],
        "candidate_samples": samples[CANDIDATE],
    }


def write_report(path: Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    with temporary.open("w", encoding="utf-8", newline="\n") as destination:
        json.dump(report, destination, indent=2, sort_keys=True)
        destination.write("\n")
        destination.flush()
        os.fsync(destination.fileno())
    os.replace(temporary, path)


def main() -> int:
    args = parse_args()
    if args.duration_seconds > benchmark.MAX_DURATION_SECONDS:
        raise ValueError(
            f"duration must not exceed {benchmark.MAX_DURATION_SECONDS} seconds"
        )
    if args.rounds > MAX_ROUNDS:
        raise ValueError(f"rounds must not exceed {MAX_ROUNDS}")

    binaries = {
        BASELINE: benchmark.executable(args.baseline_forge, "baseline forge"),
        CANDIDATE: benchmark.executable(args.candidate_forge, "candidate forge"),
    }
    ffmpeg = benchmark.executable(args.ffmpeg, "ffmpeg")
    output = args.output.expanduser().resolve()
    workspace_parent = (
        args.work_dir.expanduser().resolve()
        if args.work_dir is not None
        else Path(tempfile.gettempdir())
    )
    workspace_parent.mkdir(parents=True, exist_ok=True)

    report = {
        "generator": "forge-paired-benchmark/1",
        "generated_unix_ms": int(time.time() * 1_000),
        "system": {
            "os": platform.system(),
            "os_release": platform.release(),
            "architecture": platform.machine(),
            "cpu_model": benchmark.cpu_model(),
            "cpu_count": os.cpu_count(),
            "python_version": platform.python_version(),
        },
        "configuration": {
            "duration_seconds": args.duration_seconds,
            "rounds": args.rounds,
            "samples_per_binary": args.rounds * 2,
            "cases": list(CASES),
        },
        "versions": {
            label: benchmark.command_version(path)
            for label, path in binaries.items()
        },
        "results": [],
        "error": None,
        "passed": False,
    }

    try:
        with tempfile.TemporaryDirectory(
            prefix="forge-paired-benchmark-", dir=workspace_parent
        ) as directory:
            workspace = Path(directory)
            estimated = benchmark.pcm_bytes(args.duration_seconds, 48_000, 2)
            benchmark.require_space(workspace, estimated * 3)
            wave_path = workspace / "input.wav"
            flac_path = workspace / "input.flac"
            benchmark.write_pcm16_wave(
                wave_path, args.duration_seconds, 48_000, 2
            )
            benchmark.encode_fixture(
                ffmpeg, wave_path, flac_path, ["-map_metadata", "-1", "-c:a", "flac"],
                args.timeout_seconds,
            )
            for case_index, case_id in enumerate(CASES):
                report["results"].append(
                    run_case(
                        case_id,
                        case_index,
                        workspace,
                        binaries,
                        wave_path,
                        flac_path,
                        args.rounds,
                        args.timeout_seconds,
                    )
                )
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        write_report(output, report)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"forge-paired-benchmark: error: {error}", file=sys.stderr)
        raise SystemExit(2)
