#!/usr/bin/env python3
"""Verify that a staged native release carries the complete public file set."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
NATIVE_METADATA_GENERATOR = REPO_ROOT / "tools" / "native_package_metadata.py"
NATIVE_PLATFORMS = ("linux", "macos", "windows")

# The native package has one deliberately fixed payload name per platform.
# Keep this table independent of the generator: the checker must detect a
# generator or packaging-script drift instead of merely agreeing with it.
NATIVE_LAYOUT: dict[str, dict[str, tuple[str, ...]]] = {
    "linux": {
        "libraries": (
            "lib/libforge_normalizer.so",
            # Existing archives exposed this compatibility copy at their root.
            "libforge_normalizer.so",
        ),
        "metadata": (
            "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake",
            "lib/cmake/ForgeNormalizer/ForgeNormalizerConfigVersion.cmake",
            "lib/pkgconfig/forge-normalizer.pc",
        ),
    },
    "macos": {
        "libraries": (
            "lib/libforge_normalizer.dylib",
            # Existing macOS archives exposed this compatibility copy at root.
            "libforge_normalizer.dylib",
        ),
        "metadata": (
            "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake",
            "lib/cmake/ForgeNormalizer/ForgeNormalizerConfigVersion.cmake",
            "lib/pkgconfig/forge-normalizer.pc",
        ),
    },
    "windows": {
        "libraries": (
            "bin/forge_normalizer.dll",
            "lib/forge_normalizer.lib",
            # Existing Windows archives exposed these compatibility copies at
            # their root.  They are retained for consumers of the old layout.
            "forge_normalizer.dll",
            "forge_normalizer.lib",
        ),
        "metadata": (
            "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake",
            "lib/cmake/ForgeNormalizer/ForgeNormalizerConfigVersion.cmake",
        ),
    },
}

RELEASE_VERSION = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\Z",
    re.ASCII,
)
UNEXPANDED_TOKEN = re.compile(r"@[A-Za-z_][A-Za-z0-9_]*@")


def files_under(root: Path) -> dict[Path, Path]:
    return {
        path.relative_to(root): path
        for path in root.rglob("*")
        if path.is_file()
    }


def compare_files(
    label: str,
    repo_root: Path,
    expected: dict[Path, Path],
    actual: dict[Path, Path],
) -> None:
    missing = set(expected) - set(actual)
    unexpected = set(actual) - set(expected)
    mismatched = {
        relative
        for relative in expected.keys() & actual.keys()
        if committed_bytes(repo_root, expected[relative])
        != actual[relative].read_bytes()
    }
    for problem, paths in (
        ("is missing files", missing),
        ("has unexpected files", unexpected),
        ("contains modified files", mismatched),
    ):
        if paths:
            rendered = ", ".join(str(path) for path in sorted(paths))
            raise SystemExit(f"release {label} {problem}: {rendered}")


def committed_bytes(repo_root: Path, path: Path) -> bytes:
    relative = path.relative_to(repo_root).as_posix()
    result = subprocess.run(
        [
            "git",
            # Container jobs mount the checkout with a host-owned UID. Trust
            # only this resolved repository, and only for this read-only Git
            # invocation, instead of mutating a runner-global configuration.
            "-c",
            f"safe.directory={repo_root}",
            "cat-file",
            "blob",
            f"HEAD:{relative}",
        ],
        cwd=repo_root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise SystemExit(f"cannot read committed release file {relative}: {detail}")
    return result.stdout


def selected_files(root: Path, patterns: tuple[str, ...]) -> dict[Path, Path]:
    paths = {
        path
        for pattern in patterns
        for path in root.glob(pattern)
        if path.is_file()
    }
    return {path.relative_to(root): path for path in paths}


def _relative_member(root: Path, path: Path) -> str:
    try:
        return path.relative_to(root).as_posix()
    except ValueError as error:
        raise SystemExit(f"release path escapes its root: {path}") from error


def reject_staged_symlinks(root: Path) -> None:
    """Reject every link before any release file is opened.

    ``Path.rglob`` does not descend through a symlinked directory, but it does
    report that directory itself.  Checking with ``lstat`` keeps this property
    explicit and avoids accidentally following a link while formatting an
    error.  Native archives are consumed by build systems, so even an
    apparently harmless documentation link is not part of the release ABI.
    """

    try:
        root_stat = root.lstat()
    except OSError as error:
        raise SystemExit(
            f"cannot stat staged release directory {root}: {error}"
        ) from error
    if stat.S_ISLNK(root_stat.st_mode):
        raise SystemExit(f"staged release directory must not be a symlink: {root}")
    if not stat.S_ISDIR(root_stat.st_mode):
        raise SystemExit(f"staged release directory is not a directory: {root}")
    pending = [root]
    while pending:
        directory = pending.pop()
        try:
            entries = sorted(os.scandir(directory), key=lambda entry: entry.name)
        except OSError as error:
            raise SystemExit(
                f"cannot read staged release directory {directory}: {error}"
            ) from error
        for entry in entries:
            candidate = Path(entry.path)
            try:
                if entry.is_symlink():
                    raise SystemExit(
                        "staged release contains a symbolic link: "
                        f"{_relative_member(root, candidate)}"
                    )
                entry_stat = entry.stat(follow_symlinks=False)
            except SystemExit:
                raise
            except OSError as error:
                raise SystemExit(
                    f"cannot stat staged release member "
                    f"{_relative_member(root, candidate)}: {error}"
                ) from error
            if stat.S_ISLNK(entry_stat.st_mode):
                raise SystemExit(
                    "staged release contains a symbolic link: "
                    f"{_relative_member(root, candidate)}"
                )
            if stat.S_ISDIR(entry_stat.st_mode):
                pending.append(candidate)


def _regular_native_file(root: Path, relative: str) -> Path:
    candidate = root / Path(relative)
    try:
        metadata = candidate.lstat()
    except OSError as error:
        raise SystemExit(f"native release is missing {relative}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"native release member is not a regular file: {relative}")
    return candidate


def _native_path_set(root: Path) -> set[str]:
    """Return all regular-file names without following links."""

    members: set[str] = set()
    pending = [root]
    while pending:
        directory = pending.pop()
        try:
            entries = os.scandir(directory)
        except OSError as error:
            raise SystemExit(
                f"cannot read staged release directory {directory}: {error}"
            ) from error
        with entries:
            for entry in entries:
                candidate = Path(entry.path)
                try:
                    entry_stat = entry.stat(follow_symlinks=False)
                except OSError as error:
                    raise SystemExit(
                        f"cannot stat staged release member "
                        f"{_relative_member(root, candidate)}: {error}"
                    ) from error
                if stat.S_ISLNK(entry_stat.st_mode):
                    raise SystemExit(
                        "staged release contains a symbolic link: "
                        f"{_relative_member(root, candidate)}"
                    )
                if stat.S_ISDIR(entry_stat.st_mode):
                    pending.append(candidate)
                elif stat.S_ISREG(entry_stat.st_mode):
                    members.add(_relative_member(root, candidate))
    return members


def _reject_wrong_native_members(root: Path, platform: str) -> None:
    """Reject obvious platform/placement mistakes and static payloads."""

    members = _native_path_set(root)
    allowed = set(NATIVE_LAYOUT[platform]["libraries"])
    # Names are deliberately narrow: arbitrary binaries (the CLI and plug-in
    # payloads) are valid archive members, while these names are the C ABI
    # payload and therefore must never silently select another ABI.
    native_name_re = re.compile(
        r"^(?:lib)?forge_normalizer\.(?:so(?:\.[0-9]+)*|dylib|dll|lib|a)$",
        re.IGNORECASE,
    )
    for member in sorted(members):
        name = Path(member).name
        if not native_name_re.fullmatch(name):
            if name.lower() in {
                "forgenormalizerconfig.cmake",
                "forgenormalizerconfigversion.cmake",
                "forge-normalizer.pc",
            } and member not in NATIVE_LAYOUT[platform]["metadata"]:
                if platform == "windows" and name.lower() == "forge-normalizer.pc":
                    raise SystemExit(
                        "native Windows release must not contain pkg-config "
                        f"metadata: {member}"
                    )
                raise SystemExit(
                    f"native release contains metadata at an unexpected path: "
                    f"{member}"
                )
            continue
        if member in allowed:
            continue
        if name.lower().endswith(".a"):
            raise SystemExit(
                f"native release contains a forbidden static library: {member}"
            )
        raise SystemExit(
            f"native release contains a library at an unexpected path for "
            f"{platform}: {member}"
        )


def _validate_metadata_text(
    path: Path, *, version: str, repo_root: Path
) -> bytes:
    try:
        data = path.read_bytes()
        text = data.decode("ascii")
    except (OSError, UnicodeError) as error:
        raise SystemExit(f"native metadata is not ASCII: {path}: {error}") from error
    token = UNEXPANDED_TOKEN.search(text)
    if token is not None:
        raise SystemExit(
            f"native metadata contains an unexpanded token in "
            f"{path.name}: {token.group(0)}"
        )
    # Do not allow either spelling: metadata is copied between hosts, and a
    # Windows-produced path may use backslashes even when the package is
    # consumed on Unix.
    roots = {
        str(repo_root),
        repo_root.as_posix(),
        str(repo_root).replace("\\", "/"),
    }
    normalized = text.replace("\\", "/")
    for candidate in roots:
        if (
            candidate
            and candidate != "/"
            and candidate.replace("\\", "/") in normalized
        ):
            raise SystemExit(
                f"native metadata embeds an absolute repository path: {path.name}"
            )
    if not text.endswith("\n"):
        raise SystemExit(f"native metadata must end with a newline: {path.name}")
    # The version is checked against the generator's byte-for-byte output
    # below.  This early validation gives a useful error if a malformed
    # version reaches this helper directly.
    if RELEASE_VERSION.fullmatch(version) is None:
        raise SystemExit(
            "native release version must use numeric MAJOR.MINOR.PATCH form "
            f"without leading zeros: {version}"
        )
    return data


def _validate_metadata_version(relative: str, data: bytes, version: str) -> None:
    """Require each metadata format to expose the requested package version."""

    markers = {
        "ForgeNormalizerConfig.cmake":
            f'set(ForgeNormalizer_VERSION "{version}")'.encode("ascii"),
        "ForgeNormalizerConfigVersion.cmake":
            f'set(PACKAGE_VERSION "{version}")'.encode("ascii"),
        "forge-normalizer.pc": f"Version: {version}\n".encode("ascii"),
    }
    marker = markers.get(Path(relative).name)
    if marker is not None and marker not in data:
        raise SystemExit(
            f"native metadata has an unexpected version in {relative}; "
            f"expected {version}"
        )


def _generated_metadata(
    repo_root: Path, version: str, platform: str
) -> dict[str, bytes]:
    """Render the canonical metadata in an isolated temporary prefix."""

    if not NATIVE_METADATA_GENERATOR.is_file():
        raise SystemExit(
            f"native metadata generator is missing: {NATIVE_METADATA_GENERATOR}"
        )
    expected = NATIVE_LAYOUT[platform]["metadata"]
    with tempfile.TemporaryDirectory(prefix="forge-native-metadata-") as directory:
        generated_root = Path(directory)
        # The generator requires actual canonical library entries.  Empty
        # regular files are sufficient here; the ABI checker owns ELF validity.
        canonical_count = 1 if platform != "windows" else 2
        for relative in NATIVE_LAYOUT[platform]["libraries"][:canonical_count]:
            path = generated_root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        result = subprocess.run(
            [
                sys.executable,
                str(NATIVE_METADATA_GENERATOR),
                "--root",
                str(generated_root),
                "--version",
                version,
                "--platform",
                platform,
            ],
            cwd=repo_root,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip()
            raise SystemExit(f"native metadata generator failed: {detail}")
        rendered: dict[str, bytes] = {}
        for relative in expected:
            path = generated_root / relative
            if not path.is_file() or path.is_symlink():
                raise SystemExit(
                    f"native metadata generator did not produce {relative}"
                )
            rendered[relative] = path.read_bytes()
        return rendered


def verify_native_package(
    root: Path, *, repo_root: Path, platform: str, version: str
) -> None:
    """Verify native C ABI layout and relocatable generated metadata."""

    if platform not in NATIVE_PLATFORMS:
        choices = ", ".join(NATIVE_PLATFORMS)
        raise SystemExit(f"native platform must be one of: {choices}")
    if not isinstance(version, str) or RELEASE_VERSION.fullmatch(version) is None:
        raise SystemExit(
            "native release version must use numeric MAJOR.MINOR.PATCH form "
            f"without leading zeros: {version}"
        )

    repo_root = Path(repo_root).resolve()
    reject_staged_symlinks(root)
    _regular_native_file(root, "include/forge_normalizer.h")
    for relative in NATIVE_LAYOUT[platform]["libraries"]:
        _regular_native_file(root, relative)
    libraries = NATIVE_LAYOUT[platform]["libraries"]
    compatibility_pairs = (
        ((libraries[0], libraries[1]),)
        if platform != "windows"
        else ((libraries[0], libraries[2]), (libraries[1], libraries[3]))
    )
    for canonical, compatibility in compatibility_pairs:
        if (root / canonical).read_bytes() != (root / compatibility).read_bytes():
            raise SystemExit(
                "native compatibility library differs from canonical payload: "
                f"{compatibility}"
            )
    _reject_wrong_native_members(root, platform)

    actual: dict[str, bytes] = {}
    for relative in NATIVE_LAYOUT[platform]["metadata"]:
        path = _regular_native_file(root, relative)
        actual[relative] = _validate_metadata_text(
            path, version=version, repo_root=repo_root
        )
        _validate_metadata_version(relative, actual[relative], version)
    expected = _generated_metadata(repo_root, version, platform)
    if set(actual) != set(expected):
        raise SystemExit(
            "native metadata set does not match the generated set: "
            f"expected {sorted(expected)}, found {sorted(actual)}"
        )
    for relative in sorted(expected):
        if actual[relative] != expected[relative]:
            raise SystemExit(
                f"native metadata differs from generator output: {relative}"
            )

    # A .pc file is Unix-only.  Its absence on Windows is checked by the
    # expected metadata set; explicitly reject a stray copy so it cannot be
    # mistaken for a supported Windows discovery mechanism.
    if platform == "windows":
        stray_pc = [
            member
            for member in _native_path_set(root)
            if Path(member).name == "forge-normalizer.pc"
        ]
        if stray_pc:
            raise SystemExit(
                "native Windows release must not contain pkg-config metadata: "
                + ", ".join(sorted(stray_pc))
            )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="verify the public contents of a staged Forge release"
    )
    parser.add_argument("staged_release_dir", type=Path)
    parser.add_argument(
        "--native-platform",
        choices=NATIVE_PLATFORMS,
        help="also verify the native C ABI package layout",
    )
    parser.add_argument(
        "--version",
        help="numeric MAJOR.MINOR.PATCH release version (required with --native-platform)",
    )
    return parser


def registered_json_files(repo_root: Path) -> dict[Path, Path]:
    registry_path = repo_root / "schema" / "schema-registry-v1.json"
    registry = json.loads(registry_path.read_text(encoding="utf-8"))
    registered: dict[Path, Path] = {}
    for entry in registry["entries"]:
        repository_path = Path(entry["path"])
        try:
            relative = repository_path.relative_to("schema")
        except ValueError as error:
            raise SystemExit(
                f"registered JSON path is outside schema/: {repository_path}"
            ) from error
        if len(relative.parts) != 1:
            raise SystemExit(
                f"registered JSON path must be top-level: {repository_path}"
            )
        registered[relative] = repo_root / repository_path
    return registered


def verify_sha256_manifest(root: Path) -> None:
    manifest = root / "SHA256SUMS"
    for line in manifest.read_text(encoding="ascii").splitlines():
        expected, relative_text = line.split(maxsplit=1)
        relative = Path(relative_text.lstrip("*"))
        candidate = (root / relative).resolve()
        if root.resolve() not in candidate.parents:
            raise SystemExit(f"EBU QC checksum path escapes its root: {relative}")
        actual = hashlib.sha256(candidate.read_bytes()).hexdigest()
        if actual != expected:
            raise SystemExit(
                f"EBU QC checksum mismatch for {relative}: "
                f"expected {expected}, found {actual}"
            )


def main(argv: Sequence[str] | None = None) -> None:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.native_platform is None and args.version is not None:
        parser.error("--version requires --native-platform")
    if args.native_platform is not None and args.version is None:
        parser.error("--version is required with --native-platform")

    repo_root = REPO_ROOT
    staged_input = args.staged_release_dir
    if args.native_platform is None:
        # The supplemental v3 archive has no native development payload, but
        # its public files still must not acquire a link during staging.
        if staged_input.is_symlink():
            raise SystemExit(
                f"staged release directory must not be a symlink: {staged_input}"
            )
        try:
            staged_root = staged_input.resolve(strict=True)
        except (OSError, RuntimeError) as error:
            raise SystemExit(
                f"staged release directory does not exist: {staged_input}: {error}"
            ) from error
        if not staged_root.is_dir():
            raise SystemExit(
                f"staged release directory does not exist: {staged_root}"
            )
        reject_staged_symlinks(staged_root)
    else:
        try:
            input_stat = staged_input.lstat()
        except OSError as error:
            raise SystemExit(
                f"staged release directory does not exist: {staged_input}: {error}"
            ) from error
        if stat.S_ISLNK(input_stat.st_mode):
            raise SystemExit(
                f"staged release directory must not be a symlink: {staged_input}"
            )
        try:
            staged_root = staged_input.resolve(strict=True)
        except OSError as error:
            raise SystemExit(
                f"staged release directory cannot be resolved: {staged_input}: {error}"
            ) from error
        if not stat.S_ISDIR(staged_root.stat().st_mode):
            raise SystemExit(
                f"staged release directory is not a directory: {staged_root}"
            )

    if args.native_platform is not None:
        verify_native_package(
            staged_root,
            repo_root=repo_root,
            platform=args.native_platform,
            version=args.version,
        )

    compare_files(
        "documentation set",
        repo_root,
        selected_files(repo_root, ("*.md", "LICENSE")),
        selected_files(staged_root, ("*.md", "LICENSE")),
    )
    compare_files(
        "protocol set",
        repo_root,
        files_under(repo_root / "proto"),
        files_under(staged_root / "proto"),
    )

    repo_schema = repo_root / "schema"
    staged_schema = staged_root / "schema"
    registered_json = registered_json_files(repo_root)
    compare_files(
        "governed JSON set",
        repo_root,
        registered_json,
        selected_files(staged_schema, ("*.json",)),
    )
    compare_files(
        "EBU QC schema set",
        repo_root,
        files_under(repo_schema / "ebu-qc-2026-04"),
        files_under(staged_schema / "ebu-qc-2026-04"),
    )
    verify_sha256_manifest(staged_schema / "ebu-qc-2026-04")

    print(
        "release public file set ready: "
        f"{len(selected_files(repo_root, ('*.md',)))} documents, "
        f"{len(files_under(repo_root / 'proto'))} protocol files, "
        f"{len(registered_json)} governed JSON documents, and "
        f"{len(files_under(repo_schema / 'ebu-qc-2026-04'))} EBU QC files"
        + (
            f"; native {args.native_platform} package verified"
            if args.native_platform is not None
            else ""
        )
    )


if __name__ == "__main__":
    main()
