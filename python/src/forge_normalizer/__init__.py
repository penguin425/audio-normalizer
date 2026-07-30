"""Dependency-free Python bindings for Forge loudness analysis."""

from ._binding import (
    ANALYSIS_V1_SIZE,
    C_API_VERSION,
    Analysis,
    AnalysisError,
    AbiMismatchError,
    ForgeError,
    ForgeStatus,
    LibraryNotFoundError,
    analyze_file,
    c_api_version,
    native_version,
)
from ._version import __version__

__all__ = [
    "ANALYSIS_V1_SIZE",
    "C_API_VERSION",
    "AbiMismatchError",
    "Analysis",
    "AnalysisError",
    "ForgeError",
    "ForgeStatus",
    "LibraryNotFoundError",
    "__version__",
    "analyze_file",
    "c_api_version",
    "native_version",
]
