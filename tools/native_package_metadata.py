#!/usr/bin/env python3
"""Generate relocatable native-package metadata for Forge Normalizer.

The generator deliberately has a small, fixed output surface.  It only writes
metadata beneath an existing package root and never interpolates that root
into generated files; consumers therefore remain free to move the package.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from pathlib import Path
from typing import Mapping, Sequence


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
PACKAGING_ROOT = REPOSITORY_ROOT / "packaging"

SUPPORTED_PLATFORMS = ("linux", "macos", "windows")
RELEASE_VERSION = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\Z",
    re.ASCII,
)
UNEXPANDED_TOKEN = re.compile(r"@[A-Za-z_][A-Za-z0-9_]*@")

TEMPLATE_CONFIG = PACKAGING_ROOT / "ForgeNormalizerConfig.cmake.in"
TEMPLATE_VERSION = PACKAGING_ROOT / "ForgeNormalizerConfigVersion.cmake.in"
TEMPLATE_PC = PACKAGING_ROOT / "forge-normalizer.pc.in"


class MetadataError(ValueError):
    """The requested package metadata cannot be generated safely."""


PLATFORM_LAYOUT: dict[str, dict[str, str | None]] = {
    "linux": {
        "library": "lib/libforge_normalizer.so",
        "import_library": None,
    },
    "macos": {
        "library": "lib/libforge_normalizer.dylib",
        "import_library": None,
    },
    "windows": {
        "library": "bin/forge_normalizer.dll",
        "import_library": "lib/forge_normalizer.lib",
    },
}


def validate_version(version: str) -> str:
    """Validate and return Forge's numeric release-version form."""

    if not isinstance(version, str) or not version:
        raise MetadataError("version must be a non-empty ASCII release version")
    try:
        version.encode("ascii")
    except UnicodeEncodeError as error:
        raise MetadataError("version must contain ASCII characters only") from error
    if RELEASE_VERSION.fullmatch(version) is None:
        raise MetadataError(
            "version must use numeric MAJOR.MINOR.PATCH form without leading zeros"
        )
    return version


def validate_platform(platform: str) -> str:
    """Validate and return one of the supported native package platforms."""

    if platform not in SUPPORTED_PLATFORMS:
        choices = ", ".join(SUPPORTED_PLATFORMS)
        raise MetadataError(f"platform must be one of: {choices}")
    return platform


def _resolved_root(root: str | os.PathLike[str]) -> Path:
    if not isinstance(root, (str, os.PathLike)):
        raise MetadataError("root must name an existing directory")
    try:
        root_text = os.fspath(root)
    except TypeError as error:
        raise MetadataError("root must name an existing directory") from error
    if not isinstance(root_text, str):
        raise MetadataError("root must be a text filesystem path")
    if not root_text or not root_text.strip():
        raise MetadataError("root must not be empty or whitespace")
    candidate = Path(root_text)
    try:
        resolved = candidate.resolve(strict=True)
    except (OSError, RuntimeError, ValueError) as error:
        raise MetadataError(f"root cannot be resolved: {candidate}") from error
    if not resolved.is_dir():
        raise MetadataError(f"root is not an existing directory: {candidate}")
    # Writing below a filesystem root is almost certainly an accidental broad
    # target and is not a package prefix.
    if resolved.parent == resolved:
        raise MetadataError("root must be a package directory, not a filesystem root")
    return resolved


def _validate_package_files(root: Path, platform: str) -> None:
    layout = PLATFORM_LAYOUT[platform]
    expected = [layout["library"]]
    if layout["import_library"] is not None:
        expected.append(layout["import_library"])
    # Metadata is useful only when it describes an actual package.  Reject
    # symlinked payloads so a package cannot make the generator write metadata
    # for files outside its prefix accidentally.
    for relative in expected:
        assert relative is not None
        path = root / relative
        if path.is_symlink():
            raise MetadataError(f"expected library must not be a symlink: {relative}")
        if not path.is_file():
            raise MetadataError(f"missing expected library: {relative}")
    if platform == "windows":
        stray_pc = root / "lib/pkgconfig/forge-normalizer.pc"
        if stray_pc.exists() or stray_pc.is_symlink():
            raise MetadataError(
                "Windows native packages must not contain pkg-config metadata"
            )


def _safe_output_parent(root: Path, relative: str) -> Path:
    """Create/check a fixed relative parent without following symlinks."""

    parts = Path(relative).parts
    if not parts or Path(relative).is_absolute() or ".." in parts:
        raise MetadataError(f"unsafe metadata output path: {relative}")
    parent = root
    for part in parts[:-1]:
        parent = parent / part
        if parent.exists() or parent.is_symlink():
            if parent.is_symlink() or not parent.is_dir():
                raise MetadataError(f"metadata parent is not a directory: {parent}")
        else:
            parent.mkdir()
    return parent


def _render_template(path: Path, replacements: Mapping[str, str]) -> str:
    try:
        rendered = path.read_text(encoding="ascii")
    except (OSError, UnicodeError) as error:
        raise MetadataError(f"cannot read metadata template: {path}") from error
    for key, value in replacements.items():
        rendered = rendered.replace(f"@{key}@", value)
    remaining = UNEXPANDED_TOKEN.search(rendered)
    if remaining is not None:
        raise MetadataError(
            f"metadata template contains an unexpanded token: {remaining.group(0)}"
        )
    if not rendered.endswith("\n"):
        rendered += "\n"
    return rendered


def _check_relocatable(text: str, root: Path) -> None:
    # The generated text must derive the prefix from the installed config/pc
    # location.  Catch accidental interpolation of the concrete build path.
    candidates = {str(root), root.as_posix()}
    for candidate in candidates:
        if candidate and candidate != os.path.sep and candidate in text:
            raise MetadataError("generated metadata embeds an absolute package root")


def generate_metadata(
    root: str | os.PathLike[str], version: str, platform: str
) -> tuple[Path, ...]:
    """Generate package metadata and return the newly created output paths.

    ``root`` must already contain the canonical native library (and, on
    Windows, its import library).  Existing metadata files are never replaced.
    """

    checked_version = validate_version(version)
    checked_platform = validate_platform(platform)
    resolved_root = _resolved_root(root)
    _validate_package_files(resolved_root, checked_platform)

    layout = PLATFORM_LAYOUT[checked_platform]
    replacements = {
        "PACKAGE_VERSION": checked_version,
        "FORGE_NORMALIZER_LIBRARY_RELATIVE": layout["library"] or "",
        "FORGE_NORMALIZER_IMPORT_LIBRARY_RELATIVE": layout["import_library"] or "",
        "FORGE_NORMALIZER_IMPORT_PROPERTY": (
            "            IMPORTED_IMPLIB\n"
            "                \"${_FORGE_NORMALIZER_PREFIX}/"
            f"{layout['import_library']}\""
            if layout["import_library"] is not None
            else ""
        ),
    }
    outputs: list[tuple[Path, str]] = [
        (
            resolved_root / "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake",
            _render_template(TEMPLATE_CONFIG, replacements),
        ),
        (
            resolved_root
            / "lib/cmake/ForgeNormalizer/ForgeNormalizerConfigVersion.cmake",
            _render_template(TEMPLATE_VERSION, replacements),
        ),
    ]
    if checked_platform != "windows":
        outputs.append(
            (
                resolved_root / "lib/pkgconfig/forge-normalizer.pc",
                _render_template(TEMPLATE_PC, replacements),
            )
        )

    for path, text in outputs:
        _check_relocatable(text, resolved_root)
        if path.exists() or path.is_symlink():
            raise MetadataError(f"refusing to overwrite existing file: {path}")

    created: list[Path] = []
    try:
        for path, text in outputs:
            parent = _safe_output_parent(resolved_root, str(path.relative_to(resolved_root)))
            # Exclusive creation makes a concurrent invocation fail closed too.
            with path.open("x", encoding="utf-8", newline="\n") as stream:
                created.append(path)
                stream.write(text)
    except (OSError, ValueError) as error:
        cleanup_errors: list[str] = []
        for created_path in reversed(created):
            try:
                created_path.unlink(missing_ok=True)
            except OSError as cleanup_error:
                cleanup_errors.append(f"{created_path}: {cleanup_error}")
        cleanup_detail = (
            "; cleanup also failed: " + "; ".join(cleanup_errors)
            if cleanup_errors
            else ""
        )
        raise MetadataError(
            f"cannot write metadata: {error}{cleanup_detail}"
        ) from error
    return tuple(created)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, help="existing package root")
    parser.add_argument(
        "--version", required=True, help="numeric MAJOR.MINOR.PATCH release version"
    )
    parser.add_argument(
        "--platform",
        required=True,
        choices=SUPPORTED_PLATFORMS,
        help="native package platform",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        outputs = generate_metadata(args.root, args.version, args.platform)
    except MetadataError as error:
        parser.error(str(error))
    for output in outputs:
        print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
