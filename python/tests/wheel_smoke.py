from __future__ import annotations

import base64
import math
import os
import struct
import tempfile
import wave
from pathlib import Path

import forge_normalizer


# A deterministic 0.5 s, 48 kHz stereo Ogg Opus tone. Keeping the fixture in
# the standard-library-only smoke test proves that platform wheels retain the
# release's advertised Opus analysis support without requiring an encoder in
# the clean runtime container.
OPUS_FIXTURE_BASE64 = (
    "T2dnUwACAAAAAAAAAAAAAAAAAAAAABzVxfcBE09wdXNIZWFkAQI4AYC7AAAAAABPZ2dTAAAAAAAA"
    "AAAAAAAAAAABAAAASZW+VAEuT3B1c1RhZ3MGAAAAZmZtcGVnAQAAABQAAABlbmNvZGVyPUxhdmMg"
    "bGlib3B1c09nZ1MABPheAAAAAAAAAAAAAAIAAAC+PDu6GigoKCgoKCgoKCgoKCgoKCgoKCgoKCgo"
    "KCgoeINsnLSP+vGsPx3mUgT9FBOtE3h1rY2LlOnp81DnJXNNJXjLmH9oFHikiIHmj5BddVycHrA9"
    "kyKZZnxVxaSEfZRD47CYK6x8FUtO3TnwNNd4m/ayaLVbRqWqzTJa1/IOPod7hIA7MVXj+0o5ccfw"
    "il3S0d/MWixkeJugL6RBF/Y73HwfEttieIJ0PPovS5qgdLnj9G9oRay6AVf2u+12sniar6+kQRf2"
    "O9x8UNAn0bixDkV+Wjg7ukAcckU0m9PjvfrlMVSMhIZ4mq+vpEEX9jvcfE4bC7u5LW1CfdN7N+1u"
    "oLZbVXel4V//2sVeR+tdeJqvr6RBF/Y73HxQqK9c3cymFYICyGcRmsjrJU109LKZhDN4RsgEnXid"
    "acjNmvQd1946UbZypjzC8wE4XWZ7RgJqFzSFK+RMGcDGSEBVMzR4nWnIzZr0HdfeGYkwAiPVS31N"
    "SeTB3WZ/inKbVSiknfSIhqA56HqiS0ECnJPeQ+qLn4gqgU01jdu7rAVgodaqeLSWpEfgB/8091xG"
    "sdgAAEsBmoQWSaICg2AcXSSBYnfhs6Oed74vHqlr7nLBYLQpBmv77uADo0BLQQCZvy+kQRf2O9x8"
    "wqDpYBBTm98pCHqm3NrFvhFmzlaT5dyscZFASwGZvy+kQRf2O9x8InBywWfh23NxPJONAQKYfKNY"
    "7TZIJgHEU4YpSEiZyiI8O98a+mzgN0ZgNlkiTc8i0lrN7NHVVgU9ZjpKIh1kVUvq0MBLQQCaN2+k"
    "QRf2O9x8GJ7zgrY0hhlgjGTRnDbXlR3c1Z4CmndTZ8pAS0EEmUoenTaIC93H1+rWkSjID2t7bMhw"
    "HrQJUgtTv9Ad81dgAAAAAEiast5D6oufiCqcBXd2JoKqCoP/ssxllMvai85tkyU/BJgPD/W9fYBL"
    "AZwbnkPqi5+IKo5lHw2pRucXdtkGKTBnNObo0OMfDNf5G87BGyJAS0EAmxcSe/VbRqWq0GTcWWE7"
    "vWKks7fEBpWxNnf3iIsb+sRo7+MWgEtBApm/L6RBF/Y73HyJXyw0CuZfr9WdK2Sn52Vb7VeCj5hH"
    "NAaAAABImb8vpEEX9jvcfCRYInfhrDSIWw4Dh+kx1/kWaPLJ63m70V1fYg5YSJm/L6RBF/Y73Hwe"
    "UdNkPEA5Dq9UB70SnoA25TnURZeTwlZ8fSam/ktBBKY4ev3X6Y070TiMR22iE1rrEt2Q8rM+HJKF"
    "vbr93aAMOAAAAABLQQSozComg4wEOrpo9uHtcZBAEZ+f2kKc4nB6j2VB1Iyh44AAAAAAS0EEpFYW"
    "9JZv4oCjk/y1ImZhcNNRZ7PLJn0/HJgqsYohtNGgAAAAAEsBBdFHNT8KNdYQr662sGryvkEeDFmv"
    "KriN/jei0guB1syhGPlfx5w="
)


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
        wav_analysis = forge_normalizer.analyze_file(
            path,
            max_decoded_samples=96_000,
        )
        opus_path = Path(directory) / "wheel-smoke.opus"
        opus_path.write_bytes(
            base64.b64decode(OPUS_FIXTURE_BASE64, validate=True)
        )
        opus_analysis = forge_normalizer.analyze_file(
            opus_path,
            max_decoded_samples=48_000,
        )
    if (
        wav_analysis.sample_rate_hz != 48_000
        or wav_analysis.channels != 2
        or wav_analysis.frames != 48_000
        or not math.isfinite(wav_analysis.integrated_lufs)
    ):
        raise RuntimeError(f"unexpected WAVE wheel analysis: {wav_analysis!r}")
    if (
        opus_analysis.sample_rate_hz != 48_000
        or opus_analysis.channels != 2
        or opus_analysis.frames != 24_000
        or not math.isfinite(opus_analysis.integrated_lufs)
    ):
        raise RuntimeError(f"unexpected Opus wheel analysis: {opus_analysis!r}")


if __name__ == "__main__":
    main()
