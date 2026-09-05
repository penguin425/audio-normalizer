from __future__ import annotations

import math
import os
import struct
import tempfile
import unittest
import wave
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import forge_normalizer
from forge_normalizer import (
    AnalysisError,
    ForgeStatus,
    LibraryNotFoundError,
    analyze_file,
    analyze_file_with_layout,
    c_api_version,
    native_version,
)
from forge_normalizer._binding import _AnalysisV1


def _configured_library() -> Path:
    value = os.environ.get("FORGE_NORMALIZER_LIBRARY")
    if not value:
        raise RuntimeError("FORGE_NORMALIZER_LIBRARY is required for source tests")
    path = Path(value).resolve()
    if not path.is_file():
        raise RuntimeError(f"configured Forge library does not exist: {path}")
    return path


def _write_stereo_wave(path: Path, *, seconds: int = 1) -> None:
    sample_rate = 48_000
    frames = bytearray()
    for frame in range(sample_rate * seconds):
        sample = round(math.sin(2.0 * math.pi * 997.0 * frame / sample_rate) * 6_000)
        frames.extend(struct.pack("<hh", sample, sample))
    with wave.open(str(path), "wb") as output:
        output.setnchannels(2)
        output.setsampwidth(2)
        output.setframerate(sample_rate)
        output.writeframes(frames)


def _write_maskless_multichannel_wave(path: Path) -> None:
    with wave.open(str(path), "wb") as output:
        output.setnchannels(6)
        output.setsampwidth(2)
        output.setframerate(48_000)
        output.writeframes(bytes(6 * 2 * 8))


class BindingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.library = _configured_library()

    def test_versions_and_layout_match_native_contract(self) -> None:
        self.assertEqual(c_api_version(library=self.library), 1)
        self.assertEqual(native_version(library=self.library), forge_normalizer.__version__)
        self.assertEqual(forge_normalizer.ANALYSIS_V1_SIZE, 80)
        self.assertEqual(struct.calcsize("P"), 8)
        self.assertEqual(__import__("ctypes").sizeof(_AnalysisV1), 80)

    def test_successful_unicode_analysis_returns_immutable_result(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "音量.wav"
            _write_stereo_wave(path)
            analysis = analyze_file(
                path,
                max_decoded_samples=48_000 * 2,
                library=self.library,
            )
        self.assertEqual(analysis.sample_rate_hz, 48_000)
        self.assertEqual(analysis.channels, 2)
        self.assertEqual(analysis.frames, 48_000)
        self.assertTrue(math.isfinite(analysis.integrated_lufs))
        self.assertTrue(math.isfinite(analysis.true_peak_dbtp))
        with self.assertRaises((AttributeError, TypeError)):
            analysis.frames = 0

    def test_native_failures_preserve_status_and_utf8_message(self) -> None:
        with self.assertRaises(AnalysisError) as missing:
            analyze_file(
                "存在しない.wav",
                max_decoded_samples=1,
                library=self.library,
            )
        self.assertEqual(missing.exception.status, ForgeStatus.ANALYSIS_FAILED)
        self.assertTrue(missing.exception.message)

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bounded.wav"
            _write_stereo_wave(path)
            with self.assertRaises(AnalysisError) as bounded:
                analyze_file(path, max_decoded_samples=10, library=self.library)
        self.assertEqual(bounded.exception.status, ForgeStatus.ANALYSIS_FAILED)
        self.assertIn("exceeds safety limit", bounded.exception.message)

    def test_ambiguous_multichannel_layout_fails_without_override(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "maskless.wav"
            _write_maskless_multichannel_wave(path)
            with self.assertRaisesRegex(AnalysisError, "ambiguous 6-channel layout"):
                analyze_file(path, max_decoded_samples=48, library=self.library)

    def test_exact_layout_override_and_provenance(self) -> None:
        roles = ["main", "main", "main", "lfe", "surround", "surround"]
        layout = {
            "version": 1,
            "assignments": [
                {"kind": "legacy-role", "role": role} for role in roles
            ],
            "provenance": "known-speakers",
            "origin": "explicit-override",
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "maskless.wav"
            _write_maskless_multichannel_wave(path)
            analysis = analyze_file_with_layout(
                path,
                max_decoded_samples=48,
                channel_layout=layout,
                library=self.library,
            )
        self.assertEqual(analysis.channels, 6)
        self.assertEqual(analysis.channel_layout["origin"], "explicit-override")
        self.assertEqual(len(analysis.channel_layout["assignments"]), 6)

    def test_invalid_python_inputs_fail_before_native_code(self) -> None:
        missing_library = Path(tempfile.gettempdir()) / "forge-does-not-exist.so"
        with self.assertRaises(ValueError):
            analyze_file("input.wav", max_decoded_samples=0, library=missing_library)
        with self.assertRaises(ValueError):
            analyze_file("input.wav", max_decoded_samples=1 << 64, library=missing_library)
        with self.assertRaises(TypeError):
            analyze_file("input.wav", max_decoded_samples=True, library=missing_library)
        with self.assertRaises(TypeError):
            analyze_file(b"input.wav", max_decoded_samples=1, library=missing_library)
        with self.assertRaises(ValueError):
            analyze_file("bad\0path.wav", max_decoded_samples=1, library=missing_library)
        with self.assertRaises(LibraryNotFoundError):
            native_version(library=missing_library)

    def test_cached_library_is_safe_for_concurrent_calls(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "parallel.wav"
            _write_stereo_wave(path)

            def measure(_: int) -> float:
                return analyze_file(
                    path,
                    max_decoded_samples=48_000 * 2,
                    library=self.library,
                ).integrated_lufs

            with ThreadPoolExecutor(max_workers=4) as executor:
                results = list(executor.map(measure, range(12)))
        self.assertEqual(results, [results[0]] * len(results))


if __name__ == "__main__":
    unittest.main()
