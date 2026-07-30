from __future__ import annotations

import math
import os
import struct
import tempfile
import wave
from pathlib import Path

import forge_normalizer


def main() -> None:
    if os.environ.get("FORGE_NORMALIZER_LIBRARY"):
        raise RuntimeError("wheel smoke test must use the bundled native library")
    if forge_normalizer.c_api_version() != 1:
        raise RuntimeError("unexpected Forge C ABI version")
    if forge_normalizer.native_version() != forge_normalizer.__version__:
        raise RuntimeError("Python and bundled native versions differ")

    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "wheel-smoke.wav"
        frames = bytearray()
        for frame in range(48_000):
            sample = round(math.sin(2.0 * math.pi * 997.0 * frame / 48_000) * 6_000)
            frames.extend(struct.pack("<hh", sample, sample))
        with wave.open(str(path), "wb") as output:
            output.setnchannels(2)
            output.setsampwidth(2)
            output.setframerate(48_000)
            output.writeframes(frames)
        analysis = forge_normalizer.analyze_file(
            path,
            max_decoded_samples=96_000,
        )
    if (
        analysis.sample_rate_hz != 48_000
        or analysis.channels != 2
        or analysis.frames != 48_000
        or not math.isfinite(analysis.integrated_lufs)
    ):
        raise RuntimeError(f"unexpected wheel analysis: {analysis!r}")


if __name__ == "__main__":
    main()
