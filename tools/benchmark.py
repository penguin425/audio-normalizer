#!/usr/bin/env python3
"""Reproducible CPU/memory benchmark harness for Forge.

Fixtures are generated outside the measured interval and are removed after
each case unless --keep-fixtures is used. The script only uses Python's
standard library; ffmpeg is required for the FLAC, MP3, and Opus cases.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable


SCHEMA = "https://penguin425.github.io/audio-normalizer/schema/performance-benchmark-v1"
GENERATOR = "forge-benchmark/1"
DEFAULT_CASES = (
    "wav-stereo-analyze",
    "wav-stereo-normalize",
    "wav-stereo-verify",
    "wav-to-flac-verify",
    "wav-stereo-resample-normalize",
    "wav-stereo-batch-normalize",
    "wav-stereo-batch-cache-hit-normalize",
    "wav-stereo-batch-cache-miss-normalize",
    "wav-stereo-album-normalize",
    "wav-stereo-album-cache-hit-normalize",
    "wav-stereo-album-cache-miss-normalize",
    "wav-7.1-normalize",
    "flac-stereo-analyze",
    "flac-stereo-normalize",
    "mp3-stereo-analyze",
    "mp3-stereo-normalize",
    "pathological-wave-qc",
)
DSD_CASES = (
    "dsf-stereo-analyze",
    "dsdiff-stereo-analyze",
)
OPUS_CASES = ("opus-stereo-analyze",)
LIMITER_CASES = (
    "wav-stereo-limiter-idle",
    "wav-stereo-limiter-active",
)
OPTIONAL_CASES = (*DSD_CASES, *OPUS_CASES, *LIMITER_CASES)
ALL_CASES = (*DEFAULT_CASES, *OPTIONAL_CASES)
MAX_DURATION_SECONDS = 3_600
MAX_PATHOLOGICAL_CHUNKS = 100_001
MAX_ITERATIONS = 100
CHUNK_FRAMES = 8_192
ALBUM_TRACKS = 8
DSD_SAMPLE_RATE = 2_822_400
DSF_BLOCK_BYTES = 4_096
ALBUM_CASES = (
    "wav-stereo-album-normalize",
    "wav-stereo-album-cache-hit-normalize",
    "wav-stereo-album-cache-miss-normalize",
)
CACHE_HIT_CASES = (
    "wav-stereo-batch-cache-hit-normalize",
    "wav-stereo-album-cache-hit-normalize",
)
CACHE_MISS_CASES = (
    "wav-stereo-batch-cache-miss-normalize",
    "wav-stereo-album-cache-miss-normalize",
)
MULTI_INPUT_CASES = (
    "wav-stereo-batch-normalize",
    "wav-stereo-batch-cache-hit-normalize",
    "wav-stereo-batch-cache-miss-normalize",
    *ALBUM_CASES,
)


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def cpu_model() -> str:
    if sys.platform.startswith("linux"):
        try:
            for line in Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
                if line.lower().startswith("model name"):
                    return line.split(":", 1)[1].strip()
        except OSError:
            pass
    return platform.processor() or "unknown"


def pcm_bytes(duration_seconds: int, sample_rate: int, channels: int) -> int:
    return duration_seconds * sample_rate * channels * 2


def write_pcm16_wave(
    path: Path,
    duration_seconds: int,
    sample_rate: int,
    channels: int,
    *,
    full_scale_transient: bool = False,
) -> int:
    data_bytes = pcm_bytes(duration_seconds, sample_rate, channels)
    riff_size = 36 + data_bytes
    if riff_size > 0xFFFF_FFFF:
        raise ValueError("fixture exceeds the RIFF/WAVE 32-bit size limit")
    path.parent.mkdir(parents=True, exist_ok=True)
    block = bytearray()
    for frame_index in range(CHUNK_FRAMES):
        samples = [
            ((frame_index * (97 + channel * 31) + channel * 1_009) % 20_000)
            - 10_000
            for channel in range(channels)
        ]
        block.extend(struct.pack("<" + "h" * channels, *samples))
    frame_bytes = channels * 2
    frames_left = duration_seconds * sample_rate
    with path.open("wb") as output:
        output.write(b"RIFF")
        output.write(struct.pack("<I", riff_size))
        output.write(b"WAVEfmt ")
        output.write(struct.pack("<IHHIIHH", 16, 1, channels, sample_rate,
                                 sample_rate * channels * 2, channels * 2, 16))
        output.write(b"data")
        output.write(struct.pack("<I", data_bytes))
        while frames_left:
            count = min(frames_left, CHUNK_FRAMES)
            output.write(block if count == CHUNK_FRAMES else block[: count * frame_bytes])
            frames_left -= count
    if full_scale_transient:
        transient_frame = duration_seconds * sample_rate // 2
        with path.open("r+b") as output:
            output.seek(44 + transient_frame * frame_bytes)
            output.write(struct.pack("<" + "h" * channels, *([32_767] * channels)))
    return path.stat().st_size


def dsd_pattern(channels: int, frames: int) -> bytes:
    return bytes(
        (frame * (97 + channel * 31) + channel * 53) & 0xff
        for frame in range(frames)
        for channel in range(channels)
    )


def write_dsf(path: Path, duration_seconds: int, channels: int) -> int:
    if channels not in (1, 2):
        raise ValueError("the deterministic DSF fixture supports mono or stereo")
    samples = duration_seconds * DSD_SAMPLE_RATE
    bytes_per_channel = samples // 8
    rounds = (bytes_per_channel + DSF_BLOCK_BYTES - 1) // DSF_BLOCK_BYTES
    data_bytes = rounds * DSF_BLOCK_BYTES * channels
    path.parent.mkdir(parents=True, exist_ok=True)
    patterns = [
        bytes(
            (frame * (97 + channel * 31) + channel * 53) & 0xff
            for frame in range(DSF_BLOCK_BYTES)
        )
        for channel in range(channels)
    ]
    with path.open("wb") as output:
        output.write(b"DSD ")
        output.write(struct.pack("<QQQ", 28, 92 + data_bytes, 0))
        output.write(b"fmt ")
        output.write(struct.pack("<Q", 52))
        channel_type = 1 if channels == 1 else 2
        output.write(struct.pack(
            "<IIIIIIQII", 1, 0, channel_type, channels, DSD_SAMPLE_RATE, 1,
            samples, DSF_BLOCK_BYTES, 0,
        ))
        output.write(b"data")
        output.write(struct.pack("<Q", data_bytes + 12))
        for round_index in range(rounds):
            valid = min(
                DSF_BLOCK_BYTES,
                bytes_per_channel - round_index * DSF_BLOCK_BYTES,
            )
            for pattern in patterns:
                output.write(pattern[:valid])
                if valid < DSF_BLOCK_BYTES:
                    output.write(bytes(DSF_BLOCK_BYTES - valid))
    return path.stat().st_size


def dsdiff_chunk(identifier: bytes, body: bytes) -> bytes:
    value = identifier + struct.pack(">Q", len(body)) + body
    return value + (b"\0" if len(body) % 2 else b"")


def write_dsdiff(path: Path, duration_seconds: int, channels: int) -> int:
    if channels not in (1, 2):
        raise ValueError("the deterministic DSDIFF fixture supports mono or stereo")
    bytes_per_channel = duration_seconds * DSD_SAMPLE_RATE // 8
    data_bytes = bytes_per_channel * channels
    properties = b"SND "
    properties += dsdiff_chunk(b"FS  ", struct.pack(">I", DSD_SAMPLE_RATE))
    channel_ids = b"SLFTSRGT"[:channels * 4]
    properties += dsdiff_chunk(
        b"CHNL", struct.pack(">H", channels) + channel_ids
    )
    properties += dsdiff_chunk(b"CMPR", b"DSD \x03DSD")
    prefix = b"DSD "
    prefix += dsdiff_chunk(b"FVER", struct.pack(">I", 0x0105_0000))
    prefix += dsdiff_chunk(b"PROP", properties)
    body_bytes = len(prefix) + 12 + data_bytes + data_bytes % 2
    path.parent.mkdir(parents=True, exist_ok=True)
    pattern_frames = DSF_BLOCK_BYTES
    pattern = dsd_pattern(channels, pattern_frames)
    full_patterns, remaining_frames = divmod(bytes_per_channel, pattern_frames)
    with path.open("wb") as output:
        output.write(b"FRM8")
        output.write(struct.pack(">Q", body_bytes))
        output.write(prefix)
        output.write(b"DSD ")
        output.write(struct.pack(">Q", data_bytes))
        for _ in range(full_patterns):
            output.write(pattern)
        output.write(pattern[:remaining_frames * channels])
        if data_bytes % 2:
            output.write(b"\0")
    return path.stat().st_size


def write_pathological_wave(path: Path, chunks: int, sample_rate: int = 48_000) -> int:
    if not 1 <= chunks <= MAX_PATHOLOGICAL_CHUNKS:
        raise ValueError(f"pathological chunk count must be 1..{MAX_PATHOLOGICAL_CHUNKS}")
    riff_size = 4 + (8 + 16) + chunks * 8 + 8
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as output:
        output.write(b"RIFF")
        output.write(struct.pack("<I", riff_size))
        output.write(b"WAVEfmt ")
        output.write(
            struct.pack(
                "<IHHIIHH", 16, 1, 1, sample_rate, sample_rate * 2, 2, 16
            )
        )
        for _ in range(chunks):
            output.write(b"JUNK\x00\x00\x00\x00")
        output.write(b"data\x00\x00\x00\x00")
    return path.stat().st_size


def command_version(executable: Path, args: Iterable[str] = ("--version",)) -> str:
    try:
        completed = subprocess.run(
            [str(executable), *args],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=10,
        )
        return completed.stdout.strip().splitlines()[0][:300]
    except (OSError, subprocess.TimeoutExpired, IndexError):
        return "unavailable"


def max_rss_bytes(ru_maxrss: int) -> int:
    # getrusage(2): bytes on macOS, KiB on Linux and the BSDs supported here.
    return ru_maxrss if sys.platform == "darwin" else ru_maxrss * 1024


def run_measured(
    command: list[str], timeout_seconds: int, stdout_path: Path, stderr_path: Path
) -> dict[str, Any]:
    started = time.monotonic()
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        usage = None
        if hasattr(os, "wait4"):
            deadline = started + timeout_seconds
            while True:
                pid, status, usage = os.wait4(process.pid, os.WNOHANG)
                if pid:
                    process.returncode = os.waitstatus_to_exitcode(status)
                    break
                if time.monotonic() >= deadline:
                    process.kill()
                    pid, status, usage = os.wait4(process.pid, 0)
                    process.returncode = os.waitstatus_to_exitcode(status)
                    raise TimeoutError(f"command exceeded {timeout_seconds} seconds")
                time.sleep(0.001)
        else:
            try:
                process.wait(timeout=timeout_seconds)
            except subprocess.TimeoutExpired as error:
                process.kill()
                process.wait()
                raise TimeoutError(
                    f"command exceeded {timeout_seconds} seconds"
                ) from error
    wall = time.monotonic() - started
    user = usage.ru_utime if usage is not None else None
    system = usage.ru_stime if usage is not None else None
    rss = max_rss_bytes(usage.ru_maxrss) if usage is not None else None
    cpu = ((user + system) / wall * 100.0) if usage is not None and wall else None
    return {
        "exit_code": process.returncode,
        "wall_seconds": round(wall, 6),
        "user_cpu_seconds": round(user, 6) if user is not None else None,
        "system_cpu_seconds": round(system, 6) if system is not None else None,
        "cpu_percent": round(cpu, 3) if cpu is not None else None,
        "peak_rss_bytes": rss,
    }


def aggregate_measurements(measurements: list[dict[str, Any]]) -> dict[str, Any]:
    """Summarize repeated runs without hiding peak memory consumption."""

    def median(key: str, digits: int) -> float | None:
        values = [item[key] for item in measurements if item[key] is not None]
        return round(statistics.median(values), digits) if values else None

    rss_values = [
        item["peak_rss_bytes"]
        for item in measurements
        if item["peak_rss_bytes"] is not None
    ]
    return {
        "wall_seconds": median("wall_seconds", 6),
        "user_cpu_seconds": median("user_cpu_seconds", 6),
        "system_cpu_seconds": median("system_cpu_seconds", 6),
        "cpu_percent": median("cpu_percent", 3),
        "peak_rss_bytes": max(rss_values) if rss_values else None,
    }


def require_space(directory: Path, required_bytes: int) -> None:
    free = shutil.disk_usage(directory).free
    reserve = 1 << 30
    if free < required_bytes + reserve:
        raise RuntimeError(
            f"insufficient free space: need {required_bytes + reserve} bytes "
            f"including reserve, have {free}"
        )


def encode_fixture(
    ffmpeg: Path,
    source: Path,
    destination: Path,
    codec_args: list[str],
    timeout_seconds: int,
) -> None:
    completed = subprocess.run(
        [str(ffmpeg), "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
         "-i", str(source), *codec_args, str(destination)],
        check=False,
        timeout=timeout_seconds,
    )
    if completed.returncode != 0:
        raise RuntimeError(f"ffmpeg fixture encoding failed with {completed.returncode}")


def sanitized_command(case_id: str) -> list[str]:
    commands = {
        "wav-stereo-analyze": ["forge", "<input.wav>", "--analyze", "--json"],
        "wav-stereo-normalize": [
            "forge", "<input.wav>", "--overwrite", "-o", "<output.wav>"
        ],
        "wav-stereo-verify": [
            "forge", "<input.wav>", "--verify", "--overwrite", "-o", "<output.wav>"
        ],
        "wav-stereo-limiter-idle": [
            "forge", "<input.wav>", "--limiter", "--overwrite", "-o", "<output.wav>"
        ],
        "wav-stereo-limiter-active": [
            "forge", "<input.wav>", "--target=-6", "--ceiling=-3", "--limiter",
            "--overwrite", "-o", "<output.wav>",
        ],
        "wav-to-flac-verify": [
            "forge", "<input.wav>", "--format", "flac", "--verify",
            "--overwrite", "-o", "<output.flac>",
        ],
        "wav-stereo-resample-normalize": [
            "forge", "<input.wav>", "--sample-rate", "<alternate-rate>",
            "--overwrite", "-o", "<output.wav>",
        ],
        "wav-stereo-batch-normalize": [
            "forge",
            *[f"<input-{index:02d}.wav>" for index in range(1, ALBUM_TRACKS + 1)],
            "--jobs", str(ALBUM_TRACKS), "--overwrite", "-o", "<output-dir>",
        ],
        "wav-stereo-batch-cache-hit-normalize": [
            "forge",
            *[f"<input-{index:02d}.wav>" for index in range(1, ALBUM_TRACKS + 1)],
            "--jobs", str(ALBUM_TRACKS), "--analysis-cache", "<warm-cache-dir>",
            "--overwrite", "-o", "<output-dir>",
        ],
        "wav-stereo-batch-cache-miss-normalize": [
            "forge",
            *[f"<input-{index:02d}.wav>" for index in range(1, ALBUM_TRACKS + 1)],
            "--jobs", str(ALBUM_TRACKS), "--analysis-cache", "<empty-cache-dir>",
            "--overwrite", "-o", "<output-dir>",
        ],
        "wav-stereo-album-normalize": [
            "forge",
            *[f"<input-{index:02d}.wav>" for index in range(1, ALBUM_TRACKS + 1)],
            "--album", "--overwrite", "-o", "<output-dir>",
        ],
        "wav-stereo-album-cache-hit-normalize": [
            "forge",
            *[f"<input-{index:02d}.wav>" for index in range(1, ALBUM_TRACKS + 1)],
            "--album", "--analysis-cache", "<warm-cache-dir>",
            "--overwrite", "-o", "<output-dir>",
        ],
        "wav-stereo-album-cache-miss-normalize": [
            "forge",
            *[f"<input-{index:02d}.wav>" for index in range(1, ALBUM_TRACKS + 1)],
            "--album", "--analysis-cache", "<empty-cache-dir>",
            "--overwrite", "-o", "<output-dir>",
        ],
        "wav-7.1-normalize": [
            "forge", "<input.wav>", "--channel-layout", "7.1", "--overwrite",
            "-o", "<output.wav>",
        ],
        "flac-stereo-analyze": ["forge", "<input.flac>", "--analyze", "--json"],
        "flac-stereo-normalize": [
            "forge", "<input.flac>", "--overwrite", "-o", "<output.wav>"
        ],
        "mp3-stereo-analyze": ["forge", "<input.mp3>", "--analyze", "--json"],
        "mp3-stereo-normalize": [
            "forge", "<input.mp3>", "--overwrite", "-o", "<output.wav>"
        ],
        "opus-stereo-analyze": ["forge", "<input.opus>", "--analyze", "--json"],
        "dsf-stereo-analyze": ["forge", "<input.dsf>", "--analyze", "--json"],
        "dsdiff-stereo-analyze": ["forge", "<input.dff>", "--analyze", "--json"],
        "pathological-wave-qc": [
            "forge-container-qc", "<input.wav>", "--compact", "-o", "<report.json>"
        ],
    }
    return commands[case_id]


def case_spec(case_id: str) -> tuple[str, str, int, str]:
    specs = {
        "wav-stereo-analyze": ("lossless", "wav", 2, "analyze"),
        "wav-stereo-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-verify": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-limiter-idle": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-limiter-active": ("lossless", "wav", 2, "normalize"),
        "wav-to-flac-verify": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-resample-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-batch-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-batch-cache-hit-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-batch-cache-miss-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-album-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-album-cache-hit-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-stereo-album-cache-miss-normalize": ("lossless", "wav", 2, "normalize"),
        "wav-7.1-normalize": ("multichannel", "wav", 8, "normalize"),
        "flac-stereo-analyze": ("lossless", "flac", 2, "analyze"),
        "flac-stereo-normalize": ("lossless", "flac", 2, "normalize"),
        "mp3-stereo-analyze": ("lossy", "mp3", 2, "analyze"),
        "mp3-stereo-normalize": ("lossy", "mp3", 2, "normalize"),
        "opus-stereo-analyze": ("lossy", "opus", 2, "analyze"),
        "dsf-stereo-analyze": ("lossless", "dsf", 2, "analyze"),
        "dsdiff-stereo-analyze": ("lossless", "dsdiff", 2, "analyze"),
        "pathological-wave-qc": ("pathological", "wav", 1, "container-qc"),
    }
    return specs[case_id]


def run_case(
    case_id: str,
    workspace: Path,
    forge: Path,
    container_qc: Path,
    ffmpeg: Path | None,
    duration: int,
    sample_rate: int,
    pathological_chunks: int,
    timeout_seconds: int,
    keep_fixtures: bool,
    iterations: int,
) -> dict[str, Any]:
    category, input_format, channels, operation = case_spec(case_id)
    case_dir = workspace / case_id
    case_dir.mkdir(parents=True, exist_ok=True)
    output_paths: list[Path] = []
    output_directory: Path | None = None
    cache_directory: Path | None = None
    output_sample_rate: int | None = None
    result_sample_rate = sample_rate
    expected = [0]

    if case_id == "pathological-wave-qc":
        input_path = case_dir / "input.wav"
        input_bytes = write_pathological_wave(
            input_path, pathological_chunks, sample_rate
        )
        output_path = case_dir / "report.json"
        output_paths.append(output_path)
        command = [
            str(container_qc), str(input_path), "--compact", "-o", str(output_path)
        ]
        expected = [1]
        measured_duration: float | None = None
    else:
        estimated = pcm_bytes(duration, sample_rate, channels)
        if case_id in DSD_CASES:
            estimated = duration * DSD_SAMPLE_RATE * channels // 8
            require_space(case_dir, estimated)
            if input_format == "dsf":
                input_path = case_dir / "input.dsf"
                input_bytes = write_dsf(input_path, duration, channels)
            else:
                input_path = case_dir / "input.dff"
                input_bytes = write_dsdiff(input_path, duration, channels)
            measured_duration = float(duration)
            result_sample_rate = DSD_SAMPLE_RATE
            output_sample_rate = 88_200
            command = [str(forge), str(input_path), "--analyze", "--json"]
        elif case_id in MULTI_INPUT_CASES:
            require_space(case_dir, estimated * ALBUM_TRACKS * 2)
            input_directory = case_dir / "inputs"
            input_paths = [
                input_directory / f"input-{index:02d}.wav"
                for index in range(1, ALBUM_TRACKS + 1)
            ]
            input_bytes = sum(
                write_pcm16_wave(path, duration, sample_rate, channels)
                for path in input_paths
            )
            output_directory = case_dir / "outputs"
            output_directory.mkdir()
            command = [
                str(forge), *[str(path) for path in input_paths],
            ]
            if case_id in ALBUM_CASES:
                command.append("--album")
            else:
                command += ["--jobs", str(ALBUM_TRACKS)]
            if case_id in (*CACHE_HIT_CASES, *CACHE_MISS_CASES):
                cache_directory = case_dir / "analysis-cache"
                command += ["--analysis-cache", str(cache_directory)]
            if case_id in CACHE_HIT_CASES:
                warm_mode = ["--album"] if case_id in ALBUM_CASES else [
                    "--jobs", str(ALBUM_TRACKS)
                ]
                warm = subprocess.run(
                    [
                        str(forge), *[str(path) for path in input_paths],
                        *warm_mode,
                        "--analysis-cache", str(cache_directory),
                        "--dry-run", "-o", str(output_directory),
                    ],
                    check=False,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    timeout=timeout_seconds,
                )
                if warm.returncode != 0:
                    detail = warm.stderr.decode("utf-8", errors="replace")[-2_000:]
                    raise RuntimeError(
                        f"analysis-cache warm-up failed with {warm.returncode}: {detail}"
                    )
            command += ["--overwrite", "-o", str(output_directory)]
            measured_duration = float(duration * ALBUM_TRACKS)
        else:
            wave_path = case_dir / "source.wav"
            require_space(case_dir, estimated * (2 if operation == "normalize" else 1))
            write_pcm16_wave(
                wave_path,
                duration,
                sample_rate,
                channels,
                full_scale_transient=case_id == "wav-stereo-limiter-active",
            )
            input_path = wave_path
            if input_format == "flac":
                if ffmpeg is None:
                    raise RuntimeError("ffmpeg is required for the FLAC benchmark")
                input_path = case_dir / "input.flac"
                encode_fixture(
                    ffmpeg, wave_path, input_path, ["-c:a", "flac"], timeout_seconds
                )
                wave_path.unlink()
            elif input_format == "mp3":
                if ffmpeg is None:
                    raise RuntimeError("ffmpeg is required for the MP3 benchmark")
                input_path = case_dir / "input.mp3"
                encode_fixture(
                    ffmpeg, wave_path, input_path,
                    ["-c:a", "libmp3lame", "-b:a", "320k"],
                    timeout_seconds,
                )
                wave_path.unlink()
            elif input_format == "opus":
                if ffmpeg is None:
                    raise RuntimeError("ffmpeg is required for the Opus benchmark")
                input_path = case_dir / "input.opus"
                encode_fixture(
                    ffmpeg,
                    wave_path,
                    input_path,
                    [
                        "-map_metadata", "-1",
                        "-c:a", "libopus",
                        "-b:a", "128k",
                        "-vbr", "off",
                        "-application", "audio",
                        "-frame_duration", "20",
                    ],
                    timeout_seconds,
                )
                wave_path.unlink()
            input_bytes = input_path.stat().st_size
            measured_duration = float(duration)
            if operation == "normalize":
                output_path = case_dir / (
                    "output.flac" if case_id == "wav-to-flac-verify" else "output.wav"
                )
                output_paths.append(output_path)
                command = [str(forge), str(input_path)]
                if case_id == "wav-to-flac-verify":
                    command += ["--format", "flac"]
                if case_id in ("wav-stereo-verify", "wav-to-flac-verify"):
                    command.append("--verify")
                if case_id in LIMITER_CASES:
                    if case_id == "wav-stereo-limiter-active":
                        command += ["--target=-6", "--ceiling=-3"]
                    command.append("--limiter")
                if case_id == "wav-stereo-resample-normalize":
                    output_sample_rate = 48_000 if sample_rate != 48_000 else 44_100
                    command += ["--sample-rate", str(output_sample_rate)]
                if channels == 8:
                    command += ["--channel-layout", "7.1"]
                command += ["--overwrite", "-o", str(output_path)]
            else:
                command = [str(forge), str(input_path), "--analyze", "--json"]

    measurements = []
    for iteration in range(iterations):
        # Measure the same complete write path every time. Outputs live only in
        # the private benchmark workspace, so removing them is deterministic.
        for output_path in output_paths:
            output_path.unlink(missing_ok=True)
        if output_directory is not None:
            for output_path in output_directory.iterdir():
                if output_path.is_file():
                    output_path.unlink()
        if (
            case_id in CACHE_MISS_CASES
            and cache_directory is not None
            and cache_directory.exists()
        ):
            shutil.rmtree(cache_directory)
        measurements.append(
            run_measured(
                command,
                timeout_seconds,
                stdout_path=case_dir / f"stdout-{iteration + 1:03d}.log",
                stderr_path=case_dir / f"stderr-{iteration + 1:03d}.log",
            )
        )
    metrics = aggregate_measurements(measurements)
    unexpected_exit_codes = [
        item["exit_code"]
        for item in measurements
        if item["exit_code"] not in expected
    ]
    metrics["exit_code"] = (
        unexpected_exit_codes[0]
        if unexpected_exit_codes
        else measurements[-1]["exit_code"]
    )
    if output_directory is not None:
        output_bytes = sum(
            path.stat().st_size for path in output_directory.iterdir() if path.is_file()
        )
    elif output_paths:
        output_bytes = sum(path.stat().st_size for path in output_paths if path.exists())
    else:
        output_bytes = None
    result = {
        "id": case_id,
        "category": category,
        "input_format": input_format,
        "operation": operation,
        "channels": channels,
        "sample_rate_hz": result_sample_rate,
        "output_sample_rate_hz": output_sample_rate,
        "duration_seconds": measured_duration,
        "input_bytes": input_bytes,
        "output_bytes": output_bytes,
        "command": sanitized_command(case_id),
        **metrics,
        "realtime_factor": (
            round(measured_duration / metrics["wall_seconds"], 3)
            if measured_duration is not None and metrics["wall_seconds"] > 0
            else None
        ),
        "expected_exit_codes": expected,
        "iterations": iterations,
        "samples": measurements,
        "passed": not unexpected_exit_codes,
        "regression": None,
    }
    if not keep_fixtures:
        shutil.rmtree(case_dir)
    return result


def load_baseline(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as source:
        data = json.load(source)
    if data.get("$schema") != SCHEMA:
        raise ValueError("baseline uses an incompatible schema")
    return data


def compare_baseline(
    report: dict[str, Any],
    baseline: dict[str, Any],
    wall_limit: float,
    rss_limit: float,
) -> bool:
    if not baseline.get("passed") or baseline.get("error") is not None:
        raise ValueError("baseline must be a successful benchmark report")
    identity = ("os", "architecture", "cpu_model", "cpu_count")
    if any(report["system"].get(key) != baseline["system"].get(key) for key in identity):
        raise ValueError("baseline host OS, architecture, CPU model, or CPU count differs")
    config_keys = (
        "duration_seconds",
        "sample_rate_hz",
        "pathological_chunks",
        "iterations",
        "cases",
    )
    if any(
        report["configuration"].get(key, 1 if key == "iterations" else None)
        != baseline["configuration"].get(key, 1 if key == "iterations" else None)
        for key in config_keys
    ):
        raise ValueError("baseline benchmark configuration differs")
    previous = {item["id"]: item for item in baseline["results"]}
    if len(previous) != len(baseline["results"]):
        raise ValueError("baseline contains duplicate result identifiers")
    passed = True
    for current in report["results"]:
        old = previous.get(current["id"])
        if old is None:
            raise ValueError(f"baseline is missing case {current['id']}")
        wall_change = (
            (current["wall_seconds"] / old["wall_seconds"] - 1.0) * 100.0
            if old["wall_seconds"] > 0 else None
        )
        old_rss, current_rss = old.get("peak_rss_bytes"), current.get("peak_rss_bytes")
        rss_change = (
            (current_rss / old_rss - 1.0) * 100.0
            if old_rss and current_rss is not None else None
        )
        regression_passed = (
            (wall_change is None or wall_change <= wall_limit)
            and (rss_change is None or rss_change <= rss_limit)
        )
        current["regression"] = {
            "wall_change_percent": round(wall_change, 3) if wall_change is not None else None,
            "peak_rss_change_percent": round(rss_change, 3) if rss_change is not None else None,
            "max_wall_regression_percent": wall_limit,
            "max_peak_rss_regression_percent": rss_limit,
            "passed": regression_passed,
        }
        passed &= regression_passed
    return passed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--forge", type=Path, required=True)
    parser.add_argument("--container-qc", type=Path)
    parser.add_argument("--ffmpeg", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--case", choices=ALL_CASES, action="append", dest="cases")
    parser.add_argument("--duration-seconds", type=positive_int, default=3_600)
    parser.add_argument("--sample-rate", type=positive_int, default=48_000)
    parser.add_argument("--pathological-chunks", type=positive_int, default=100_001)
    parser.add_argument("--timeout-seconds", type=positive_int, default=7_200)
    parser.add_argument("--iterations", type=positive_int, default=1)
    parser.add_argument("--keep-fixtures", action="store_true")
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--max-wall-regression-percent", type=float, default=15.0)
    parser.add_argument("--max-rss-regression-percent", type=float, default=15.0)
    return parser.parse_args()


def executable(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        raise ValueError(f"{label} is not executable: {resolved}")
    return resolved


def main() -> int:
    args = parse_args()
    if args.duration_seconds > MAX_DURATION_SECONDS:
        raise ValueError(f"duration must not exceed {MAX_DURATION_SECONDS} seconds")
    if not 8_000 <= args.sample_rate <= 192_000:
        raise ValueError("sample rate must be 8000..192000 Hz")
    if args.pathological_chunks > MAX_PATHOLOGICAL_CHUNKS:
        raise ValueError(
            f"pathological chunk count must not exceed {MAX_PATHOLOGICAL_CHUNKS}"
        )
    if args.iterations > MAX_ITERATIONS:
        raise ValueError(f"iterations must not exceed {MAX_ITERATIONS}")
    if args.max_wall_regression_percent < 0 or args.max_rss_regression_percent < 0:
        raise ValueError("regression limits must not be negative")
    forge = executable(args.forge, "forge")
    suffix = ".exe" if os.name == "nt" else ""
    container_qc = executable(
        args.container_qc
        or forge.with_name(f"forge-container-qc{suffix}"),
        "forge-container-qc",
    )
    cases = args.cases or list(DEFAULT_CASES)
    if len(cases) != len(set(cases)):
        raise ValueError("benchmark cases must not be repeated")
    ffmpeg = args.ffmpeg
    if any(case_spec(case)[1] in ("flac", "mp3", "opus") for case in cases):
        ffmpeg = executable(
            ffmpeg or Path(shutil.which("ffmpeg") or ""), "ffmpeg"
        )

    workspace_parent = (
        args.work_dir.expanduser().resolve()
        if args.work_dir is not None
        else Path(tempfile.gettempdir())
    )
    workspace_parent.mkdir(parents=True, exist_ok=True)
    workspace = Path(
        tempfile.mkdtemp(prefix="forge-benchmark-", dir=workspace_parent)
    )
    report = {
        "$schema": SCHEMA,
        "generator": GENERATOR,
        "generated_unix_ms": int(time.time() * 1_000),
        "system": {
            "os": platform.system(),
            "os_release": platform.release(),
            "architecture": platform.machine(),
            "cpu_model": cpu_model(),
            "cpu_count": os.cpu_count(),
            "python_version": platform.python_version(),
            "forge_version": command_version(forge),
            "ffmpeg_version": command_version(ffmpeg) if ffmpeg else None,
        },
        "configuration": {
            "duration_seconds": args.duration_seconds,
            "sample_rate_hz": args.sample_rate,
            "pathological_chunks": args.pathological_chunks,
            "timeout_seconds": args.timeout_seconds,
            "iterations": args.iterations,
            "cases": cases,
        },
        "results": [],
        "error": None,
        "passed": False,
    }
    try:
        for case_id in cases:
            report["results"].append(
                run_case(
                    case_id, workspace, forge, container_qc, ffmpeg,
                    args.duration_seconds, args.sample_rate,
                    args.pathological_chunks, args.timeout_seconds,
                    args.keep_fixtures, args.iterations,
                )
            )
        passed = all(item["passed"] for item in report["results"])
        if args.baseline:
            passed &= compare_baseline(
                report, load_baseline(args.baseline),
                args.max_wall_regression_percent,
                args.max_rss_regression_percent,
            )
        report["passed"] = passed
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.output.with_name(f".{args.output.name}.tmp-{os.getpid()}")
        with temporary.open("w", encoding="utf-8", newline="\n") as destination:
            json.dump(report, destination, indent=2, sort_keys=True)
            destination.write("\n")
            destination.flush()
            os.fsync(destination.fileno())
        os.replace(temporary, args.output)
        if not args.keep_fixtures:
            shutil.rmtree(workspace)
        else:
            print(f"forge-benchmark: kept fixtures in {workspace}", file=sys.stderr)
    return 0 if report["passed"] else 3


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"forge-benchmark: error: {error}", file=sys.stderr)
        raise SystemExit(2)
