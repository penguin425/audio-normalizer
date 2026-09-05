"""Dependency-free Python bindings for Forge loudness analysis."""

from ._binding import (
    ANALYSIS_V1_SIZE,
    C_API_VERSION,
    MAX_CHANNEL_LAYOUT_JSON_BYTES,
    Analysis,
    AnalysisWithLayout,
    AnalysisError,
    AbiMismatchError,
    ForgeError,
    ForgeStatus,
    LibraryNotFoundError,
    analyze_file,
    analyze_file_with_layout,
    c_api_version,
    native_version,
)
from ._version import __version__

__all__ = [
    "ANALYSIS_V1_SIZE",
    "C_API_VERSION",
    "MAX_CHANNEL_LAYOUT_JSON_BYTES",
    "AbiMismatchError",
    "Analysis",
    "AnalysisWithLayout",
    "AnalysisError",
    "ForgeError",
    "ForgeStatus",
    "LibraryNotFoundError",
    "__version__",
    "analyze_file",
    "analyze_file_with_layout",
    "c_api_version",
    "native_version",
]
