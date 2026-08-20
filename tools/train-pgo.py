#!/usr/bin/env python3
"""Run Forge's deterministic instrumentation-PGO training workload.

The caller builds an instrumented ``forge`` binary and supplies a new, empty
profile directory.  This tool generates bounded fixtures outside the measured
benchmark suite and exercises representative normalization paths with exactly
one worker.  Serial training keeps profile counters reproducible; released
parallel behavior is measured separately by ``tools/benchmark.py``.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NamedTuple

TOOLS_DIRECTORY = Path(__file__).resolve().parent
if str(TOOLS_DIRECTORY) not in sys.path:
    sys.path.insert(0, str(TOOLS_DIRECTORY))

from benchmark import write_pcm16_wave


GENERATOR = "forge-pgo-training/1"
DEFAULT_DURATION_SECONDS = 12
MAX_DURATION_SECONDS = 300
DEFAULT_TRACKS = 4
MAX_TRACKS = 8
SAMPLE_RATE = 48_000


class TrainingCase(NamedTuple):
    label: str
    command: list[str]


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def executable(path: Path) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise ValueError(f"forge is not executable: {resolved}")
    return resolved


def prepare_profile_directory(path: Path) -> Path:
    resolved = path.expanduser().resolve()
    resolved.mkdir(parents=True, exist_ok=True)
    if any(resolved.iterdir()):
        raise ValueError(f"profile directory must be empty: {resolved}")
    return resolved


def create_fixtures(workspace: Path, duration: int, tracks: int) -> dict[str, object]:
    stereo = workspace / "stereo.wav"
    surround = workspace / "surround-7.1.wav"
    track_directory = workspace / "tracks"
    track_directory.mkdir()
    track_paths = [
        track_directory / f"track-{index:02d}.wav"
        for index in range(1, tracks + 1)
    ]
    write_pcm16_wave(stereo, duration, SAMPLE_RATE, 2)
    write_pcm16_wave(surround, max(1, duration // 2), SAMPLE_RATE, 8)
    for path in track_paths:
        write_pcm16_wave(path, duration, SAMPLE_RATE, 2)
    return {"stereo": stereo, "surround": surround, "tracks": track_paths}


def training_plan(
    forge: Path, workspace: Path, fixtures: dict[str, object]
) -> list[TrainingCase]:
    stereo = fixtures["stereo"]
    surround = fixtures["surround"]
    tracks = fixtures["tracks"]
    assert isinstance(stereo, Path)
    assert isinstance(surround, Path)
    assert isinstance(tracks, list)
    jobs = ["--jobs", "1"]

    outputs = workspace / "outputs"
    outputs.mkdir()
    batch_output = outputs / "batch"
    album_output = outputs / "album"
    cache_miss_output = outputs / "cache-miss"
    cache_hit_output = outputs / "cache-hit"
    for directory in (
        batch_output,
        album_output,
        cache_miss_output,
        cache_hit_output,
    ):
        directory.mkdir()

    normalized = outputs / "normalized.wav"
    verified = outputs / "verified.wav"
    resampled = outputs / "resampled.wav"
    dithered = outputs / "dithered.wav"
    limited = outputs / "limited.wav"
    flac = outputs / "training.flac"
    flac_normalized = outputs / "flac-normalized.wav"
    surround_output = outputs / "surround.wav"
    cache = workspace / "analysis-cache"

    forge_arg = str(forge)
    track_args = [str(path) for path in tracks]
    return [
        TrainingCase(
            "wav-analyze",
            [forge_arg, str(stereo), "--analyze", "--json", *jobs],
        ),
        TrainingCase(
            "wav-normalize",
            [forge_arg, str(stereo), "--overwrite", "-o", str(normalized), *jobs],
        ),
        TrainingCase(
            "wav-verify",
            [
                forge_arg,
                str(stereo),
                "--verify",
                "--overwrite",
                "-o",
                str(verified),
                *jobs,
            ],
        ),
        TrainingCase(
            "wav-resample",
            [
                forge_arg,
                str(stereo),
                "--sample-rate",
                "44100",
                "--overwrite",
                "-o",
                str(resampled),
                *jobs,
            ],
        ),
        TrainingCase(
            "wav-dither",
            [
                forge_arg,
                str(stereo),
                "--dither",
                "--overwrite",
                "-o",
                str(dithered),
                *jobs,
            ],
        ),
        TrainingCase(
            "wav-limiter",
            [
                forge_arg,
                str(stereo),
                "--limiter",
                "--overwrite",
                "-o",
                str(limited),
                *jobs,
            ],
        ),
        TrainingCase(
            "wav-to-flac-verify",
            [
                forge_arg,
                str(stereo),
                "--format",
                "flac",
                "--verify",
                "--overwrite",
                "-o",
                str(flac),
                *jobs,
            ],
        ),
        TrainingCase(
            "flac-analyze",
            [forge_arg, str(flac), "--analyze", "--json", *jobs],
        ),
        TrainingCase(
            "flac-normalize",
            [
                forge_arg,
                str(flac),
                "--format",
                "wav",
                "--overwrite",
                "-o",
                str(flac_normalized),
                *jobs,
            ],
        ),
        TrainingCase(
            "surround-normalize",
            [
                forge_arg,
                str(surround),
                "--channel-layout",
                "7.1",
                "--overwrite",
                "-o",
                str(surround_output),
                *jobs,
            ],
        ),
        TrainingCase(
            "batch-normalize",
            [
                forge_arg,
                *track_args,
                "--overwrite",
                "-o",
                str(batch_output),
                *jobs,
            ],
        ),
        TrainingCase(
            "album-normalize",
            [
                forge_arg,
                *track_args,
                "--album",
                "--overwrite",
                "-o",
                str(album_output),
                *jobs,
            ],
        ),
        TrainingCase(
            "cache-miss-normalize",
            [
                forge_arg,
                *track_args,
                "--analysis-cache",
                str(cache),
                "--overwrite",
                "-o",
                str(cache_miss_output),
                *jobs,
            ],
        ),
        TrainingCase(
            "cache-hit-normalize",
            [
                forge_arg,
                *track_args,
                "--analysis-cache",
                str(cache),
                "--overwrite",
                "-o",
                str(cache_hit_output),
                *jobs,
            ],
        ),
    ]


def run_training(
    forge: Path,
    profile_directory: Path,
    work_parent: Path,
    duration: int,
    tracks: int,
) -> dict[str, object]:
    workspace = Path(tempfile.mkdtemp(prefix="forge-pgo-training-", dir=work_parent))
    labels: list[str] = []
    environment = os.environ.copy()
    environment["LLVM_PROFILE_FILE"] = str(
        profile_directory / "forge-%m-%p.profraw"
    )
    environment["RAYON_NUM_THREADS"] = "1"
    try:
        fixtures = create_fixtures(workspace, duration, tracks)
        plan = training_plan(forge, workspace, fixtures)
        for case in plan:
            print(f"forge-pgo-training: {case.label}", file=sys.stderr)
            completed = subprocess.run(
                case.command,
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                env=environment,
            )
            if completed.returncode != 0:
                detail = completed.stderr.decode("utf-8", errors="replace")[-4_000:]
                raise RuntimeError(
                    f"{case.label} failed with {completed.returncode}: {detail}"
                )
            labels.append(case.label)
    finally:
        shutil.rmtree(workspace)

    profiles = sorted(profile_directory.glob("*.profraw"))
    if not profiles:
        raise RuntimeError(
            "instrumented Forge produced no .profraw files; verify -Cprofile-generate"
        )
    return {
        "generator": GENERATOR,
        "duration_seconds": duration,
        "sample_rate_hz": SAMPLE_RATE,
        "tracks": tracks,
        "worker_threads": 1,
        "cases": labels,
        "profile_files": len(profiles),
        "profile_bytes": sum(path.stat().st_size for path in profiles),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--forge", type=Path, required=True)
    parser.add_argument("--profile-dir", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument(
        "--duration-seconds", type=positive_int, default=DEFAULT_DURATION_SECONDS
    )
    parser.add_argument("--tracks", type=positive_int, default=DEFAULT_TRACKS)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.duration_seconds > MAX_DURATION_SECONDS:
        raise ValueError(f"duration must not exceed {MAX_DURATION_SECONDS} seconds")
    if not 2 <= args.tracks <= MAX_TRACKS:
        raise ValueError(f"tracks must be 2..{MAX_TRACKS}")
    forge = executable(args.forge)
    profile_directory = prepare_profile_directory(args.profile_dir)
    work_parent = (
        args.work_dir.expanduser().resolve()
        if args.work_dir is not None
        else Path(tempfile.gettempdir())
    )
    work_parent.mkdir(parents=True, exist_ok=True)
    report = run_training(
        forge,
        profile_directory,
        work_parent,
        args.duration_seconds,
        args.tracks,
    )
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        output = args.output.expanduser().resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        temporary = output.with_name(f".{output.name}.tmp-{os.getpid()}")
        temporary.write_text(rendered, encoding="utf-8", newline="\n")
        os.replace(temporary, output)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"forge-pgo-training: error: {error}", file=sys.stderr)
        raise SystemExit(2)
