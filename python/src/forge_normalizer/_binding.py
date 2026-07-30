"""Safe Python wrapper for Forge C ABI major version 1."""

from __future__ import annotations

import ctypes
import ctypes.util
import operator
import os
import sys
from dataclasses import dataclass
from enum import IntEnum
from functools import lru_cache
from pathlib import Path
from typing import Final

C_API_VERSION: Final = 1
ANALYSIS_V1_SIZE: Final = 80
MAX_U64: Final = (1 << 64) - 1
ERROR_CAPACITY: Final = 4096
LIBRARY_ENV: Final = "FORGE_NORMALIZER_LIBRARY"


class ForgeStatus(IntEnum):
    """Stable integer statuses returned by Forge C ABI v1."""

    OK = 0
    NULL_POINTER = 1
    BUFFER_TOO_SMALL = 2
    INVALID_UTF8 = 3
    INVALID_ARGUMENT = 4
    ANALYSIS_FAILED = 5


class ForgeError(Exception):
    """Base class for all binding and native-analysis errors."""


class LibraryNotFoundError(ForgeError):
    """Raised when no loadable Forge native library can be found."""


class AbiMismatchError(ForgeError):
    """Raised when a library does not implement the required C ABI v1."""


class AnalysisError(ForgeError):
    """Raised when Forge rejects or cannot analyze an input."""

    def __init__(self, status: ForgeStatus | int, message: str) -> None:
        self.status = status
        self.message = message
        status_name = status.name if isinstance(status, ForgeStatus) else str(status)
        super().__init__(f"{status_name}: {message}")


@dataclass(frozen=True, slots=True)
class Analysis:
    """Immutable loudness-analysis result returned by Forge C ABI v1."""

    sample_rate_hz: int
    channels: int
    frames: int
    integrated_lufs: float
    max_momentary_lufs: float
    max_short_term_lufs: float
    loudness_range_lu: float
    rms_dbfs: float
    sample_peak_dbfs: float
    true_peak_dbtp: float


class _AnalysisV1(ctypes.Structure):
    _fields_ = [
        ("struct_size", ctypes.c_uint32),
        ("api_version", ctypes.c_uint32),
        ("sample_rate_hz", ctypes.c_uint32),
        ("channels", ctypes.c_uint32),
        ("frames", ctypes.c_uint64),
        ("integrated_lufs", ctypes.c_double),
        ("max_momentary_lufs", ctypes.c_double),
        ("max_short_term_lufs", ctypes.c_double),
        ("loudness_range_lu", ctypes.c_double),
        ("rms_dbfs", ctypes.c_double),
        ("sample_peak_dbfs", ctypes.c_double),
        ("true_peak_dbtp", ctypes.c_double),
    ]


class _NativeLibrary:
    __slots__ = ("analyze", "library", "native_version", "path")

    def __init__(self, path: str) -> None:
        try:
            library = ctypes.CDLL(path)
        except OSError as error:
            raise LibraryNotFoundError(
                f"cannot load Forge native library {path!r}: {error}"
            ) from error

        try:
            version = library.forge_normalizer_c_api_version
            version.argtypes = ()
            version.restype = ctypes.c_uint32

            analysis_size = library.forge_normalizer_analysis_v1_size
            analysis_size.argtypes = ()
            analysis_size.restype = ctypes.c_size_t

            package_version = library.forge_normalizer_version
            package_version.argtypes = ()
            package_version.restype = ctypes.c_char_p

            analyze = library.forge_normalizer_analyze_file_v1
            analyze.argtypes = (
                ctypes.c_char_p,
                ctypes.c_uint64,
                ctypes.POINTER(_AnalysisV1),
                ctypes.c_size_t,
                ctypes.POINTER(ctypes.c_char),
                ctypes.c_size_t,
            )
            analyze.restype = ctypes.c_int32
        except AttributeError as error:
            raise AbiMismatchError(
                f"native library {path!r} does not export the complete Forge C ABI v1"
            ) from error

        actual_version = int(version())
        if actual_version != C_API_VERSION:
            raise AbiMismatchError(
                f"Forge C ABI {actual_version} is incompatible with required "
                f"version {C_API_VERSION}"
            )

        native_size = int(analysis_size())
        local_size = ctypes.sizeof(_AnalysisV1)
        if native_size != ANALYSIS_V1_SIZE or local_size != ANALYSIS_V1_SIZE:
            raise AbiMismatchError(
                "ForgeAnalysisV1 size mismatch: "
                f"library={native_size}, Python={local_size}, "
                f"required={ANALYSIS_V1_SIZE}"
            )

        version_bytes = package_version()
        if version_bytes is None:
            raise AbiMismatchError("Forge returned a null package-version string")
        try:
            native_version = version_bytes.decode("utf-8")
        except UnicodeDecodeError as error:
            raise AbiMismatchError(
                "Forge returned a non-UTF-8 package-version string"
            ) from error
        if not native_version:
            raise AbiMismatchError("Forge returned an empty package-version string")

        self.analyze = analyze
        self.library = library
        self.native_version = native_version
        self.path = path


def _platform_library_name() -> str:
    if sys.platform == "win32":
        return "forge_normalizer.dll"
    if sys.platform == "darwin":
        return "libforge_normalizer.dylib"
    if sys.platform.startswith("linux"):
        return "libforge_normalizer.so"
    raise LibraryNotFoundError(f"Forge has no official library for {sys.platform!r}")


def _path_text(value: str | os.PathLike[str], *, name: str) -> str:
    path = os.fspath(value)
    if not isinstance(path, str):
        raise TypeError(f"{name} must resolve to str, not bytes")
    if "\0" in path:
        raise ValueError(f"{name} contains a NUL character")
    return path


def _resolve_library(
    library: str | os.PathLike[str] | None,
) -> str:
    if library is not None:
        explicit = Path(_path_text(library, name="library")).expanduser()
        if not explicit.is_file():
            raise LibraryNotFoundError(f"Forge native library does not exist: {explicit}")
        return str(explicit.resolve())

    configured = os.environ.get(LIBRARY_ENV)
    if configured:
        candidate = Path(_path_text(configured, name=LIBRARY_ENV)).expanduser()
        if not candidate.is_file():
            raise LibraryNotFoundError(
                f"{LIBRARY_ENV} does not name a file: {candidate}"
            )
        return str(candidate.resolve())

    library_name = _platform_library_name()
    bundled = Path(__file__).resolve().parent / "lib" / library_name
    if bundled.is_file():
        return str(bundled)

    discovered = ctypes.util.find_library("forge_normalizer")
    if discovered:
        return discovered

    raise LibraryNotFoundError(
        "cannot find the Forge native library; install an official platform "
        f"wheel, pass library=..., or set {LIBRARY_ENV}"
    )


@lru_cache(maxsize=16)
def _load_library(resolved_path: str) -> _NativeLibrary:
    return _NativeLibrary(resolved_path)


def _native(
    library: str | os.PathLike[str] | None,
) -> _NativeLibrary:
    return _load_library(_resolve_library(library))


def c_api_version(
    *,
    library: str | os.PathLike[str] | None = None,
) -> int:
    """Return the verified C ABI major version (currently always 1)."""

    _native(library)
    return C_API_VERSION


def native_version(
    *,
    library: str | os.PathLike[str] | None = None,
) -> str:
    """Return the package version embedded in the loaded native library."""

    return _native(library).native_version


def analyze_file(
    path: str | os.PathLike[str],
    *,
    max_decoded_samples: int,
    library: str | os.PathLike[str] | None = None,
) -> Analysis:
    """Analyze a local audio file through C ABI v1.

    ``max_decoded_samples`` is mandatory and bounds decoded frames multiplied
    by channels before Forge allocates or processes unbounded audio.
    """

    path_text = _path_text(path, name="path")
    try:
        path_utf8 = path_text.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError("path is not representable as UTF-8") from error

    if isinstance(max_decoded_samples, bool):
        raise TypeError("max_decoded_samples must be an integer, not bool")
    try:
        sample_limit = operator.index(max_decoded_samples)
    except TypeError as error:
        raise TypeError("max_decoded_samples must be an integer") from error
    if sample_limit <= 0 or sample_limit > MAX_U64:
        raise ValueError(f"max_decoded_samples must be in 1..={MAX_U64}")

    native = _native(library)
    result = _AnalysisV1()
    error_buffer = ctypes.create_string_buffer(ERROR_CAPACITY)
    status_value = int(
        native.analyze(
            path_utf8,
            sample_limit,
            ctypes.byref(result),
            ctypes.sizeof(result),
            error_buffer,
            len(error_buffer),
        )
    )
    try:
        error_text = error_buffer.value.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AbiMismatchError("Forge returned non-UTF-8 error text") from error

    try:
        status: ForgeStatus | int = ForgeStatus(status_value)
    except ValueError:
        status = status_value
    if status != ForgeStatus.OK:
        raise AnalysisError(status, error_text or "Forge analysis failed")

    if result.struct_size != ANALYSIS_V1_SIZE or result.api_version != C_API_VERSION:
        raise AbiMismatchError(
            "Forge returned an incompatible result header: "
            f"size={result.struct_size}, version={result.api_version}"
        )
    if error_text:
        raise AbiMismatchError("Forge returned error text with a successful status")

    return Analysis(
        sample_rate_hz=int(result.sample_rate_hz),
        channels=int(result.channels),
        frames=int(result.frames),
        integrated_lufs=float(result.integrated_lufs),
        max_momentary_lufs=float(result.max_momentary_lufs),
        max_short_term_lufs=float(result.max_short_term_lufs),
        loudness_range_lu=float(result.loudness_range_lu),
        rms_dbfs=float(result.rms_dbfs),
        sample_peak_dbfs=float(result.sample_peak_dbfs),
        true_peak_dbtp=float(result.true_peak_dbtp),
    )
