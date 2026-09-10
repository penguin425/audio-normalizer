#!/usr/bin/env python3
"""Create and verify the exact, reproducible Forge release file set.

The release workflow deliberately keeps this module independent from the
workflow itself.  A flat staging directory is treated as an untrusted input:
links, directories, unsafe names, oversized files, and unlisted files are
rejected before a file is opened.  Payloads are bound to their SPDX and
CycloneDX sidecars by the payload SHA-256, while checksum and SLSA files are
kept as evidence outside the subject set.  This avoids the otherwise
unavoidable checksum/provenance/manifest hash cycle.

The module has no third-party dependencies.  Its public functions are useful
from a workflow step as well as from the CLI::

    python3 tools/release_manifest.py expected --version 0.189.17
    python3 tools/release_manifest.py finalize --input-dir dist \
        --version 0.189.17 --include-pgo
    python3 tools/release_manifest.py verify --input-dir dist
    python3 tools/release_manifest.py normalize-sbom --format spdx ...

``finalize`` uses the full v0.189.17 release contract by default.  CI may
select a smaller, explicit contract with repeated ``--asset`` options or an
asset-list file while a new artifact job is being rolled out.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import dataclasses
import datetime as _datetime
import hashlib
import json
import os
import re
import stat
import sys
import tempfile
import subprocess
import tarfile
import uuid
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Iterator, Mapping, Sequence


SCHEMA = "forge.release-manifest/v1"
SPDX_FORMAT = "spdx"
CYCLONEDX_FORMAT = "cyclonedx"
DEFAULT_MANIFEST_NAME = "RELEASE-MANIFEST.json"
DEFAULT_CHECKSUM_NAME = "SHA256SUMS"
DEFAULT_MAX_ASSETS = 128
DEFAULT_MAX_FILE_BYTES = 2 * 1024 * 1024 * 1024
DEFAULT_MAX_TOTAL_BYTES = 12 * 1024 * 1024 * 1024
DEFAULT_MAX_ARCHIVE_MEMBERS = 100_000
DEFAULT_MAX_MEMBER_BYTES = 512 * 1024 * 1024
DEFAULT_MAX_EXPANDED_BYTES = 4 * 1024 * 1024 * 1024

_VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\Z",
    re.ASCII,
)
_DIGEST_RE = re.compile(r"[0-9a-f]{64}\Z", re.ASCII)
_NAME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+@-]*\Z", re.ASCII)
_WINDOWS_ABSOLUTE_RE = re.compile(r"^[A-Za-z]:[\\/]")


class ReleaseManifestError(ValueError):
    """A user-facing release contract or safety violation."""


@dataclasses.dataclass(frozen=True)
class AssetSpec:
    """One explicitly expected top-level release asset.

    ``sbom`` means that both ``<name>.spdx.json`` and ``<name>.cdx.json``
    must exist and be normalized by :func:`normalize_sbom`.
    """

    name: str
    kind: str
    sbom: bool = False
    subject: bool = True

    def __post_init__(self) -> None:
        validate_asset_name(self.name)
        if self.kind not in {
            "native-archive",
            "python-wheel",
            "wasm-archive",
            "npm-package",
            "rust-crate",
            "package-metadata",
            "pgo-profile",
            "payload",
        }:
            raise ReleaseManifestError(f"unknown release asset kind: {self.kind}")
        if self.sbom and not self.subject:
            raise ReleaseManifestError("an SBOM payload must be a subject")


def validate_version(version: str) -> str:
    if not isinstance(version, str) or not _VERSION_RE.fullmatch(version):
        raise ReleaseManifestError(f"invalid release version: {version!r}")
    return version


def validate_asset_name(name: str) -> str:
    """Validate a flat release name before it is joined to a directory."""

    if not isinstance(name, str) or not name:
        raise ReleaseManifestError("release asset name must be a non-empty string")
    if (
        "\x00" in name
        or any(character.isspace() for character in name)
        or "/" in name
        or "\\" in name
        or name in {".", ".."}
        or name.startswith(".")
        or not _NAME_RE.fullmatch(name)
    ):
        raise ReleaseManifestError(f"unsafe top-level release asset name: {name!r}")
    return name


def _sha256_hex(value: str) -> str:
    if not isinstance(value, str) or not _DIGEST_RE.fullmatch(value):
        raise ReleaseManifestError(f"invalid SHA-256 digest: {value!r}")
    return value


def _timestamp(epoch: int) -> str:
    if isinstance(epoch, bool) or not isinstance(epoch, int) or epoch < 0:
        raise ReleaseManifestError("source-date-epoch must be a non-negative integer")
    try:
        instant = _datetime.datetime.fromtimestamp(epoch, _datetime.timezone.utc)
    except (OverflowError, OSError, ValueError) as error:
        raise ReleaseManifestError(f"source-date-epoch is outside the supported range: {epoch}") from error
    return instant.isoformat(timespec="seconds").replace("+00:00", "Z")


def canonical_json_bytes(value: Any) -> bytes:
    """Render JSON in the byte-for-byte canonical form used by this tool."""

    try:
        rendered = json.dumps(
            value,
            ensure_ascii=False,
            allow_nan=False,
            sort_keys=True,
            separators=(",", ":"),
        )
    except (TypeError, ValueError) as error:
        raise ReleaseManifestError(f"value cannot be rendered as canonical JSON: {error}") from error
    return (rendered + "\n").encode("utf-8")


def _safe_open_read(path: Path) -> Any:
    """Open a regular file without following a final symbolic link."""

    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ReleaseManifestError(f"cannot open release file {path.name}: {error}") from error
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise ReleaseManifestError(f"release member is not a regular file: {path.name}")
        return os.fdopen(descriptor, "rb", closefd=True)
    except BaseException:
        os.close(descriptor)
        raise


def _read_bytes(path: Path, *, max_bytes: int | None = None) -> bytes:
    with _safe_open_read(path) as stream:
        if max_bytes is not None:
            metadata = os.fstat(stream.fileno())
            if metadata.st_size > max_bytes:
                raise ReleaseManifestError(
                    f"release file {path.name} is {metadata.st_size} bytes, above the {max_bytes}-byte limit"
                )
        return stream.read()


def _hash_and_size(path: Path, *, max_bytes: int | None = None) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    with _safe_open_read(path) as stream:
        while True:
            block = stream.read(1024 * 1024)
            if not block:
                break
            size += len(block)
            if max_bytes is not None and size > max_bytes:
                raise ReleaseManifestError(
                    f"release file {path.name} is above the {max_bytes}-byte limit"
                )
            digest.update(block)
    return size, digest.hexdigest()


def safe_top_level_files(
    root: Path,
    *,
    max_assets: int = DEFAULT_MAX_ASSETS,
    max_file_bytes: int = DEFAULT_MAX_FILE_BYTES,
    max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
) -> dict[str, Path]:
    """Return a bounded flat directory while rejecting links and directories.

    Release publication consumes a flat ``dist`` directory.  Refusing nested
    directories here makes path traversal and accidental artifact extraction
    impossible, and keeps both hashing and exact-set checks unambiguous.
    """

    root = Path(root)
    if max_assets < 1 or max_file_bytes < 0 or max_total_bytes < 0:
        raise ReleaseManifestError("release bounds must be positive (or zero for bytes)")
    try:
        root_metadata = root.lstat()
    except OSError as error:
        raise ReleaseManifestError(f"release input directory does not exist: {root}: {error}") from error
    if stat.S_ISLNK(root_metadata.st_mode) or not stat.S_ISDIR(root_metadata.st_mode):
        raise ReleaseManifestError(f"release input must be a real directory: {root}")

    try:
        entries = sorted(os.scandir(root), key=lambda entry: entry.name)
    except OSError as error:
        raise ReleaseManifestError(f"cannot read release input directory {root}: {error}") from error
    files: dict[str, Path] = {}
    total = 0
    for entry in entries:
        validate_asset_name(entry.name)
        candidate = root / entry.name
        try:
            entry_metadata = entry.stat(follow_symlinks=False)
        except OSError as error:
            raise ReleaseManifestError(f"cannot stat release member {entry.name}: {error}") from error
        if stat.S_ISLNK(entry_metadata.st_mode):
            raise ReleaseManifestError(f"release input contains a symbolic link: {entry.name}")
        if stat.S_ISDIR(entry_metadata.st_mode):
            raise ReleaseManifestError(f"release input must be flat; found directory: {entry.name}")
        if not stat.S_ISREG(entry_metadata.st_mode):
            raise ReleaseManifestError(f"release member is not a regular file: {entry.name}")
        if entry_metadata.st_size > max_file_bytes:
            raise ReleaseManifestError(
                f"release file {entry.name} is {entry_metadata.st_size} bytes, above the {max_file_bytes}-byte limit"
            )
        total += entry_metadata.st_size
        if total > max_total_bytes:
            raise ReleaseManifestError(
                f"release input is {total} bytes, above the {max_total_bytes}-byte limit"
            )
        files[entry.name] = candidate
        if len(files) > max_assets:
            raise ReleaseManifestError(
                f"release input has {len(files)} files, above the {max_assets}-file limit"
            )
    return files


def _native(name: str) -> AssetSpec:
    return AssetSpec(name, "native-archive", sbom=True)


def _wheel(name: str) -> AssetSpec:
    return AssetSpec(name, "python-wheel", sbom=True)


def _software(name: str, kind: str) -> AssetSpec:
    return AssetSpec(name, kind, sbom=True)


def expected_asset_specs(
    version: str,
    *,
    include_aarch64: bool = True,
    include_npm: bool = True,
    include_crate: bool = True,
    include_package_metadata: bool = True,
    include_pgo: bool = False,
) -> tuple[AssetSpec, ...]:
    """Return the explicit v0.189.17 release contract.

    PGO training inputs are deliberately opt-in: they are useful build
    evidence, but are not public release payloads unless the workflow chooses
    to publish them.  ARM64, npm, and the crates.io package are part of the
    intended distributable contract and therefore enabled by default.
    """

    version = validate_version(version)
    specs: list[AssetSpec] = [
        _native(f"forge-v{version}-linux-x86_64.tar.gz"),
        _native(f"forge-v{version}-linux-x86_64-v3.tar.gz"),
        _native(f"forge-v{version}-macos-x86_64.tar.gz"),
        _native(f"forge-v{version}-macos-aarch64.tar.gz"),
        _native(f"forge-v{version}-windows-x86_64.zip"),
        _wheel(f"forge_normalizer-{version}-py3-none-manylinux_2_34_x86_64.whl"),
        _wheel(f"forge_normalizer-{version}-py3-none-macosx_10_12_x86_64.whl"),
        _wheel(f"forge_normalizer-{version}-py3-none-macosx_11_0_arm64.whl"),
        _wheel(f"forge_normalizer-{version}-py3-none-win_amd64.whl"),
        _software(f"forge-v{version}-wasm-web.tar.gz", "wasm-archive"),
    ]
    if include_aarch64:
        specs.insert(1, _native(f"forge-v{version}-linux-aarch64.tar.gz"))
        specs.insert(
            6,
            _wheel(f"forge_normalizer-{version}-py3-none-manylinux_2_34_aarch64.whl"),
        )
    if include_npm:
        specs.append(_software(f"forge-normalizer-wasm-{version}.tgz", "npm-package"))
    if include_crate:
        specs.append(_software(f"forge-normalizer-{version}.crate", "rust-crate"))
    if include_package_metadata:
        specs.extend(
            AssetSpec(name, "package-metadata", sbom=False)
            for name in (
                "forge.rb",
                "forge-scoop.json",
                "Penguin425.Forge.yaml",
                "Penguin425.Forge.locale.en-US.yaml",
                "Penguin425.Forge.installer.yaml",
            )
        )
    if include_pgo:
        for platform in (
            "linux-x86_64",
            "linux-aarch64",
            "linux-x86_64-v3",
            "macos-aarch64",
        ):
            specs.extend(
                (
                    AssetSpec(f"forge-v{version}-{platform}.pgo-profile.txt", "pgo-profile"),
                    AssetSpec(f"forge-v{version}-{platform}.pgo-training.json", "pgo-profile"),
                )
            )
    return tuple(specs)


def expected_asset_names(version: str, **kwargs: Any) -> tuple[str, ...]:
    return tuple(spec.name for spec in expected_asset_specs(version, **kwargs))


def _sidecar_names(payload_name: str) -> tuple[str, str]:
    validate_asset_name(payload_name)
    return f"{payload_name}.spdx.json", f"{payload_name}.cdx.json"


def _specs_from_names(names: Iterable[str]) -> tuple[AssetSpec, ...]:
    specs: list[AssetSpec] = []
    seen: set[str] = set()
    for name in names:
        validate_asset_name(name)
        if name in seen:
            raise ReleaseManifestError(f"duplicate expected release asset: {name}")
        seen.add(name)
        if name.endswith(".whl"):
            kind, sbom = "python-wheel", True
        elif name.endswith((".tar.gz", ".zip")):
            kind, sbom = "native-archive", True
            if "wasm" in name:
                kind = "wasm-archive"
        elif name.endswith(".tgz"):
            kind, sbom = "npm-package", True
        elif name.endswith(".crate"):
            kind, sbom = "rust-crate", True
        elif ".pgo-" in name:
            kind, sbom = "pgo-profile", False
        else:
            kind, sbom = "package-metadata", False
        specs.append(AssetSpec(name, kind, sbom=sbom))
    if not specs:
        raise ReleaseManifestError("at least one expected release asset is required")
    return tuple(specs)


def _asset_uuid(digest: str) -> str:
    raw = bytearray(bytes.fromhex(_sha256_hex(digest)[:32]))
    # RFC 4122 version 5 and variant bits make the deterministic value valid
    # anywhere a CycloneDX UUID is expected while retaining the asset binding.
    raw[6] = (raw[6] & 0x0F) | 0x50
    raw[8] = (raw[8] & 0x3F) | 0x80
    return str(uuid.UUID(bytes=bytes(raw)))


def sbom_binding(format_name: str, *, version: str, artifact_name: str, digest: str) -> dict[str, str]:
    """Return deterministic root identifiers expected in a normalized SBOM."""

    version = validate_version(version)
    validate_asset_name(artifact_name)
    digest = _sha256_hex(digest)
    if format_name == SPDX_FORMAT:
        return {
            "documentNamespace": f"https://forge.dev/sbom/{version}/{digest}/spdx",
            "name": artifact_name,
        }
    if format_name == CYCLONEDX_FORMAT:
        return {
            "serialNumber": f"urn:uuid:{_asset_uuid(digest)}",
            "name": artifact_name,
        }
    raise ReleaseManifestError(f"unsupported SBOM format: {format_name}")


_PATH_KEYS = {
    "path",
    "filepath",
    "file_path",
    "filename",
    "file_name",
    "location",
    "sourcepath",
    "source_path",
    "documentnamespace",
}
_TIME_KEYS = {
    "created",
    "creationtime",
    "timestamp",
    "createdat",
    "updatedat",
    "modified",
    "lastmodified",
}


def _is_absolute_string(value: str) -> bool:
    if value.startswith("file://"):
        return True
    if value.startswith("/") or _WINDOWS_ABSOLUTE_RE.match(value):
        return True
    return False


def _stable_path(value: str) -> str:
    if value.startswith("file://"):
        value = value[7:]
    value = value.replace("\\", "/")
    name = PurePosixPath(value).name
    return f"./{name}" if name else "./artifact"


def _canonicalize_sbom_value(value: Any, *, key: str, timestamp: str) -> Any:
    key_lower = key.lower()
    if isinstance(value, Mapping):
        return {
            str(child_key): _canonicalize_sbom_value(child_value, key=str(child_key), timestamp=timestamp)
            for child_key, child_value in value.items()
        }
    if isinstance(value, list):
        normalized = [
            _canonicalize_sbom_value(item, key=key, timestamp=timestamp) for item in value
        ]
        # Syft normally emits sorted arrays, but sorting here also covers
        # catalogues whose traversal order depends on filesystem enumeration.
        # JSON rendering is deterministic even for heterogeneous arrays.
        return sorted(
            normalized,
            key=lambda item: json.dumps(
                item, ensure_ascii=False, sort_keys=True, separators=(",", ":")
            ),
        )
    if isinstance(value, str):
        if key_lower in _TIME_KEYS or key_lower.endswith("timestamp"):
            # Only replace timestamp-looking fields.  A package version or
            # free-form description must retain its original value.
            if re.search(r"\d{4}-\d{2}-\d{2}T\d{2}:", value):
                return timestamp
        if _is_absolute_string(value) or key_lower in _PATH_KEYS and value.startswith("/"):
            return _stable_path(value)
        # Syft may put a temporary absolute path in a descriptive source
        # string.  Keep URLs and ordinary prose intact, but never publish the
        # common temp/runner path prefixes.
        # Syft has emitted these prefixes in source descriptions and package
        # evidence fields across versions.  Scrub them regardless of the
        # surrounding JSON key; the root identifiers above are then stable
        # even when the temporary directory name changes.
        value = re.sub(
            r"(?:/tmp|/private/tmp|/home/runner|/Users/runner|/workspace)(?:/[^\s,;\"']*)?",
            "<staged-path>",
            value,
        )
        return value
    return value


def _assert_no_absolute_paths(value: Any, path: str = "$") -> None:
    if isinstance(value, Mapping):
        for key, child in value.items():
            _assert_no_absolute_paths(child, f"{path}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _assert_no_absolute_paths(child, f"{path}[{index}]")
    elif isinstance(value, str) and _is_absolute_string(value):
        raise ReleaseManifestError(f"normalized SBOM contains an absolute path at {path}")


def _validate_sbom_shape(data: Any, format_name: str) -> None:
    if not isinstance(data, dict):
        raise ReleaseManifestError("SBOM root must be a JSON object")
    if format_name == SPDX_FORMAT:
        if not isinstance(data.get("spdxVersion"), str) or not isinstance(data.get("creationInfo"), dict):
            raise ReleaseManifestError("SPDX SBOM is missing spdxVersion or creationInfo")
    elif format_name == CYCLONEDX_FORMAT:
        if data.get("bomFormat") != "CycloneDX" or not isinstance(data.get("metadata"), dict):
            raise ReleaseManifestError("CycloneDX SBOM is missing bomFormat or metadata")
    else:
        raise ReleaseManifestError(f"unsupported SBOM format: {format_name}")


def normalize_sbom_data(
    data: Any,
    *,
    format_name: str,
    version: str,
    artifact_name: str,
    artifact_sha256: str,
    source_date_epoch: int,
) -> dict[str, Any]:
    """Normalize a Syft JSON object without depending on Syft internals."""

    _validate_sbom_shape(data, format_name)
    version = validate_version(version)
    validate_asset_name(artifact_name)
    artifact_sha256 = _sha256_hex(artifact_sha256)
    timestamp = _timestamp(source_date_epoch)
    normalized = _canonicalize_sbom_value(data, key="", timestamp=timestamp)
    assert isinstance(normalized, dict)

    if format_name == SPDX_FORMAT:
        normalized["documentNamespace"] = sbom_binding(
            format_name, version=version, artifact_name=artifact_name, digest=artifact_sha256
        )["documentNamespace"]
        normalized["name"] = artifact_name
        creation = normalized.setdefault("creationInfo", {})
        if not isinstance(creation, dict):
            raise ReleaseManifestError("SPDX creationInfo must be an object")
        creation["created"] = timestamp
    else:
        normalized["serialNumber"] = sbom_binding(
            format_name, version=version, artifact_name=artifact_name, digest=artifact_sha256
        )["serialNumber"]
        metadata = normalized.setdefault("metadata", {})
        if not isinstance(metadata, dict):
            raise ReleaseManifestError("CycloneDX metadata must be an object")
        metadata["timestamp"] = timestamp
        component = metadata.get("component")
        if not isinstance(component, dict):
            component = {}
            metadata["component"] = component
        component["name"] = artifact_name
        component["version"] = version
        component["bom-ref"] = f"artifact:{artifact_sha256}"
    _assert_no_absolute_paths(normalized)
    return normalized


def normalize_sbom(
    input_path: Path,
    output_path: Path,
    *,
    format_name: str,
    version: str,
    artifact_name: str,
    artifact_sha256: str,
    source_date_epoch: int,
) -> None:
    """Read, normalize, and canonically write one SBOM sidecar."""

    try:
        data = json.loads(_read_bytes(Path(input_path), max_bytes=DEFAULT_MAX_FILE_BYTES))
    except json.JSONDecodeError as error:
        raise ReleaseManifestError(f"SBOM is not valid JSON: {input_path}: {error}") from error
    normalized = normalize_sbom_data(
        data,
        format_name=format_name,
        version=version,
        artifact_name=artifact_name,
        artifact_sha256=artifact_sha256,
        source_date_epoch=source_date_epoch,
    )
    _write_atomic(Path(output_path), canonical_json_bytes(normalized))


def _archive_member_path(name: str) -> PurePosixPath:
    """Validate one archive member without ever resolving it on the host."""

    if "\x00" in name or "\\" in name:
        raise ReleaseManifestError(f"archive member has an unsafe path: {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or any(part in {"", ".", ".."} for part in path.parts):
        raise ReleaseManifestError(f"archive member has an unsafe path: {name!r}")
    return path


def _copy_archive_member(
    stream: Any,
    output: Path,
    *,
    member_name: str,
    size: int,
    expanded: list[int],
    max_member_bytes: int,
    max_expanded_bytes: int,
) -> None:
    if size < 0 or size > max_member_bytes:
        raise ReleaseManifestError(
            f"archive member {member_name} is {size} bytes, above the {max_member_bytes}-byte limit"
        )
    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        with output.open("wb") as destination:
            remaining = size
            while remaining:
                block = stream.read(min(1024 * 1024, remaining))
                if not block:
                    raise ReleaseManifestError(f"archive member ended early: {member_name}")
                destination.write(block)
                remaining -= len(block)
                expanded[0] += len(block)
                if expanded[0] > max_expanded_bytes:
                    raise ReleaseManifestError(
                        f"expanded archive exceeds the {max_expanded_bytes}-byte limit"
                    )
    except ReleaseManifestError:
        raise
    except OSError as error:
        raise ReleaseManifestError(f"cannot extract archive member {member_name}: {error}") from error


def _extract_tar(
    archive: Path,
    destination: Path,
    *,
    max_members: int,
    max_member_bytes: int,
    max_expanded_bytes: int,
) -> None:
    expanded = [0]
    seen: set[str] = set()
    try:
        opened = tarfile.open(archive, mode="r:*")
    except (OSError, tarfile.TarError) as error:
        raise ReleaseManifestError(f"cannot read release archive {archive.name}: {error}") from error
    with opened as bundle:
        for index, member in enumerate(bundle, 1):
            if index > max_members:
                raise ReleaseManifestError(
                    f"archive has more than the {max_members}-member limit"
                )
            # GNU tar commonly records the selected staging root as `./`
            # before its regular members.  It has no filesystem payload and
            # is safe to ignore, while a non-directory root marker remains an
            # error.
            if member.name.rstrip("/") == "." and member.isdir():
                continue
            relative = _archive_member_path(member.name)
            normalized = relative.as_posix()
            if normalized in seen:
                raise ReleaseManifestError(f"archive contains duplicate member: {normalized}")
            seen.add(normalized)
            output = destination.joinpath(*relative.parts)
            if member.isdir():
                try:
                    output.mkdir(parents=True, exist_ok=True)
                except OSError as error:
                    raise ReleaseManifestError(
                        f"cannot create archive directory {normalized}: {error}"
                    ) from error
            elif member.isreg():
                stream = bundle.extractfile(member)
                if stream is None:
                    raise ReleaseManifestError(f"cannot read archive member: {normalized}")
                with stream:
                    _copy_archive_member(
                        stream,
                        output,
                        member_name=normalized,
                        size=member.size,
                        expanded=expanded,
                        max_member_bytes=max_member_bytes,
                        max_expanded_bytes=max_expanded_bytes,
                    )
            else:
                # Symlinks, hardlinks, devices, fifos, and other special files
                # have no place in an SBOM staging tree.
                raise ReleaseManifestError(
                    f"archive member is not a regular file or directory: {normalized}"
                )


def _zip_member_is_link(member: zipfile.ZipInfo) -> bool:
    mode = (member.external_attr >> 16) & 0xFFFF
    return stat.S_ISLNK(mode) or stat.S_ISCHR(mode) or stat.S_ISBLK(mode) or stat.S_ISFIFO(mode)


def _extract_zip(
    archive: Path,
    destination: Path,
    *,
    max_members: int,
    max_member_bytes: int,
    max_expanded_bytes: int,
) -> None:
    expanded = [0]
    seen: set[str] = set()
    try:
        bundle = zipfile.ZipFile(archive)
    except (OSError, zipfile.BadZipFile) as error:
        raise ReleaseManifestError(f"cannot read release archive {archive.name}: {error}") from error
    with bundle:
        members = bundle.infolist()
        if len(members) > max_members:
            raise ReleaseManifestError(f"archive has more than the {max_members}-member limit")
        for member in members:
            relative = _archive_member_path(member.filename)
            normalized = relative.as_posix()
            if normalized in seen:
                raise ReleaseManifestError(f"archive contains duplicate member: {normalized}")
            seen.add(normalized)
            if _zip_member_is_link(member):
                raise ReleaseManifestError(f"archive member is a symbolic or special file: {normalized}")
            output = destination.joinpath(*relative.parts)
            if member.is_dir() or member.filename.endswith("/"):
                try:
                    output.mkdir(parents=True, exist_ok=True)
                except OSError as error:
                    raise ReleaseManifestError(
                        f"cannot create archive directory {normalized}: {error}"
                    ) from error
            else:
                with bundle.open(member, "r") as stream:
                    _copy_archive_member(
                        stream,
                        output,
                        member_name=normalized,
                        size=member.file_size,
                        expanded=expanded,
                        max_member_bytes=max_member_bytes,
                        max_expanded_bytes=max_expanded_bytes,
                    )


def _stage_payload(
    payload: Path,
    destination: Path,
    *,
    max_members: int,
    max_member_bytes: int,
    max_expanded_bytes: int,
) -> None:
    if payload.name.endswith((".tar.gz", ".tgz", ".crate")):
        _extract_tar(
            payload,
            destination,
            max_members=max_members,
            max_member_bytes=max_member_bytes,
            max_expanded_bytes=max_expanded_bytes,
        )
    elif payload.name.endswith((".zip", ".whl")):
        _extract_zip(
            payload,
            destination,
            max_members=max_members,
            max_member_bytes=max_member_bytes,
            max_expanded_bytes=max_expanded_bytes,
        )
    else:
        # This branch is intentionally conservative for a future payload kind:
        # Syft can still inspect the file, but the original filename remains
        # deterministic and no archive parser is guessed.
        destination.mkdir(parents=True, exist_ok=True)
        output = destination / payload.name
        size, _ = _hash_and_size(payload, max_bytes=max_member_bytes)
        with _safe_open_read(payload) as source, output.open("wb") as target:
            remaining = size
            while remaining:
                block = source.read(min(1024 * 1024, remaining))
                if not block:
                    raise ReleaseManifestError(f"payload ended early: {payload.name}")
                target.write(block)
                remaining -= len(block)


def generate_sboms(
    root: Path,
    *,
    version: str,
    specs: Sequence[AssetSpec] | None = None,
    syft: str = "syft",
    source_date_epoch: int = 0,
    max_file_bytes: int = DEFAULT_MAX_FILE_BYTES,
    max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
    max_members: int = DEFAULT_MAX_ARCHIVE_MEMBERS,
    max_member_bytes: int = DEFAULT_MAX_MEMBER_BYTES,
    max_expanded_bytes: int = DEFAULT_MAX_EXPANDED_BYTES,
) -> tuple[str, ...]:
    """Generate and normalize SPDX/CycloneDX SBOMs for every SBOM payload.

    Syft scans a bounded, link-free extraction tree, never the repository or
    the release directory.  Its output is normalized before it is moved into
    the release directory, so temporary extraction names and wall-clock
    timestamps cannot enter a published sidecar.
    """

    version = validate_version(version)
    if specs is None:
        specs = expected_asset_specs(version)
    specs = _validate_specs(specs)
    files = safe_top_level_files(
        root,
        max_assets=max(DEFAULT_MAX_ASSETS, len(specs) * 3),
        max_file_bytes=max_file_bytes,
        max_total_bytes=max_total_bytes,
    )
    generated: list[str] = []
    with tempfile.TemporaryDirectory(prefix="forge-sbom-") as temporary:
        staging_root = Path(temporary) / "stage"
        raw_root = Path(temporary) / "raw"
        staging_root.mkdir()
        raw_root.mkdir()
        for spec in specs:
            if not spec.sbom:
                continue
            payload = files.get(spec.name)
            if payload is None:
                raise ReleaseManifestError(f"release input is missing files: {spec.name}")
            _, payload_digest = _hash_and_size(payload, max_bytes=max_file_bytes)
            stage = staging_root / hashlib.sha256(spec.name.encode("utf-8")).hexdigest()[:16]
            stage.mkdir()
            _stage_payload(
                payload,
                stage,
                max_members=max_members,
                max_member_bytes=max_member_bytes,
                max_expanded_bytes=max_expanded_bytes,
            )
            raw_spdx = raw_root / f"{spec.name}.spdx.json"
            raw_cdx = raw_root / f"{spec.name}.cdx.json"
            command = [
                syft,
                "scan",
                f"dir:{stage}",
                "--source-name",
                spec.name,
                "--source-version",
                version,
                "--base-path",
                str(stage),
                "--output",
                f"spdx-json={raw_spdx}",
                "--output",
                f"cyclonedx-json={raw_cdx}",
            ]
            try:
                subprocess.run(command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            except FileNotFoundError as error:
                raise ReleaseManifestError(f"Syft executable was not found: {syft}") from error
            except subprocess.CalledProcessError as error:
                detail = error.stderr.decode("utf-8", errors="replace").strip()
                raise ReleaseManifestError(f"Syft failed for {spec.name}: {detail}") from error
            spdx_name, cdx_name = _sidecar_names(spec.name)
            normalize_sbom(
                raw_spdx,
                Path(root) / spdx_name,
                format_name=SPDX_FORMAT,
                version=version,
                artifact_name=spec.name,
                artifact_sha256=payload_digest,
                source_date_epoch=source_date_epoch,
            )
            normalize_sbom(
                raw_cdx,
                Path(root) / cdx_name,
                format_name=CYCLONEDX_FORMAT,
                version=version,
                artifact_name=spec.name,
                artifact_sha256=payload_digest,
                source_date_epoch=source_date_epoch,
            )
            generated.extend((spdx_name, cdx_name))
    return tuple(generated)


def _parse_format_from_name(name: str) -> str:
    if name.endswith(".spdx.json"):
        return SPDX_FORMAT
    if name.endswith(".cdx.json") or name.endswith(".cyclonedx.json"):
        return CYCLONEDX_FORMAT
    raise ReleaseManifestError(f"cannot infer SBOM format from filename: {name}")


def _load_json(path: Path) -> Any:
    try:
        return json.loads(_read_bytes(path, max_bytes=DEFAULT_MAX_FILE_BYTES))
    except json.JSONDecodeError as error:
        raise ReleaseManifestError(f"invalid JSON in {path.name}: {error}") from error


def _check_normalized_sbom(
    path: Path,
    *,
    format_name: str,
    version: str,
    artifact_name: str,
    artifact_sha256: str,
    source_date_epoch: int,
) -> None:
    raw = _read_bytes(path, max_bytes=DEFAULT_MAX_FILE_BYTES)
    data = _load_json(path)
    _validate_sbom_shape(data, format_name)
    _assert_no_absolute_paths(data)
    expected = sbom_binding(
        format_name, version=version, artifact_name=artifact_name, digest=artifact_sha256
    )
    for key, value in expected.items():
        if data.get(key) != value and not (
            format_name == CYCLONEDX_FORMAT
            and key == "name"
            and data.get("metadata", {}).get("component", {}).get("name") == value
        ):
            raise ReleaseManifestError(
                f"{path.name} is not bound to {artifact_name} ({key} mismatch)"
            )
    expected_timestamp = _timestamp(source_date_epoch)
    if format_name == SPDX_FORMAT:
        if data.get("creationInfo", {}).get("created") != expected_timestamp:
            raise ReleaseManifestError(
                f"{path.name} SPDX creation timestamp does not match source_date_epoch"
            )
    elif data.get("metadata", {}).get("timestamp") != expected_timestamp:
        raise ReleaseManifestError(
            f"{path.name} CycloneDX timestamp does not match source_date_epoch"
        )
    if raw != canonical_json_bytes(data):
        raise ReleaseManifestError(f"{path.name} is not canonical JSON")


def _write_atomic(path: Path, data: bytes) -> None:
    path = Path(path)
    if path.name in {"", ".", ".."} or "/" in path.name or "\\" in path.name:
        raise ReleaseManifestError(f"output path must name a single file: {path}")
    if path.exists() and path.is_symlink():
        raise ReleaseManifestError(f"refusing to overwrite symbolic link: {path.name}")
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_name, path)
    except OSError as error:
        try:
            os.unlink(temporary_name)
        except (OSError, UnboundLocalError):
            pass
        raise ReleaseManifestError(f"cannot write {path}: {error}") from error


def _validate_specs(specs: Sequence[AssetSpec]) -> tuple[AssetSpec, ...]:
    if not specs:
        raise ReleaseManifestError("at least one release asset is required")
    seen: set[str] = set()
    normalized: list[AssetSpec] = []
    for spec in specs:
        if not isinstance(spec, AssetSpec):
            raise ReleaseManifestError("release asset specifications must be AssetSpec values")
        if spec.name in seen:
            raise ReleaseManifestError(f"duplicate expected release asset: {spec.name}")
        seen.add(spec.name)
        normalized.append(spec)
    return tuple(normalized)


def _subject_records(manifest: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    records = manifest.get("assets")
    if not isinstance(records, list):
        raise ReleaseManifestError("release manifest assets must be a list")
    subjects: list[Mapping[str, Any]] = []
    for record in records:
        if not isinstance(record, dict):
            raise ReleaseManifestError("release manifest asset records must be objects")
        if record.get("subject") is True:
            subjects.append(record)
    return subjects


def build_manifest(
    root: Path,
    *,
    version: str,
    specs: Sequence[AssetSpec] | None = None,
    source_date_epoch: int = 0,
    manifest_name: str = DEFAULT_MANIFEST_NAME,
    checksum_name: str = DEFAULT_CHECKSUM_NAME,
    slsa_bundle: str | None = None,
    max_assets: int = DEFAULT_MAX_ASSETS,
    max_file_bytes: int = DEFAULT_MAX_FILE_BYTES,
    max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
) -> dict[str, Any]:
    """Build a canonical manifest and verify every expected sidecar.

    The returned object is not written by this function, which keeps it easy
    to test and lets callers add no circular self-hash.  ``slsa_bundle`` is an
    evidence path and is intentionally absent from ``assets``/subjects.
    """

    version = validate_version(version)
    _timestamp(source_date_epoch)
    validate_asset_name(manifest_name)
    validate_asset_name(checksum_name)
    if manifest_name == checksum_name:
        raise ReleaseManifestError("manifest and checksum names must differ")
    if slsa_bundle is not None:
        validate_asset_name(slsa_bundle)
        if slsa_bundle in {manifest_name, checksum_name}:
            raise ReleaseManifestError("SLSA bundle must not reuse manifest/checksum name")
    if specs is None:
        specs = expected_asset_specs(version)
    specs = _validate_specs(specs)
    files = safe_top_level_files(
        root,
        max_assets=max_assets,
        max_file_bytes=max_file_bytes,
        max_total_bytes=max_total_bytes,
    )
    expected_names = {spec.name for spec in specs}
    sidecar_names: set[str] = set()
    for spec in specs:
        if spec.sbom:
            sidecar_names.update(_sidecar_names(spec.name))
    allowed = expected_names | sidecar_names | {manifest_name, checksum_name}
    if slsa_bundle is not None:
        allowed.add(slsa_bundle)
    unexpected = sorted(set(files) - allowed)
    if unexpected:
        raise ReleaseManifestError(
            "release input contains unlisted files: " + ", ".join(unexpected)
        )
    missing = sorted(expected_names - set(files))
    if missing:
        raise ReleaseManifestError("release input is missing files: " + ", ".join(missing))

    records: list[dict[str, Any]] = []
    for spec in specs:
        path = files[spec.name]
        size, digest = _hash_and_size(path, max_bytes=max_file_bytes)
        record: dict[str, Any] = {
            "kind": spec.kind,
            "name": spec.name,
            "sha256": digest,
            "size": size,
            "subject": bool(spec.subject),
        }
        if spec.sbom:
            spdx_name, cdx_name = _sidecar_names(spec.name)
            for sidecar_name, format_name in (
                (spdx_name, SPDX_FORMAT),
                (cdx_name, CYCLONEDX_FORMAT),
            ):
                if sidecar_name not in files:
                    raise ReleaseManifestError(
                        f"{spec.name} is missing required {format_name} SBOM sidecar {sidecar_name}"
                    )
                _check_normalized_sbom(
                    files[sidecar_name],
                    format_name=format_name,
                    version=version,
                    artifact_name=spec.name,
                    artifact_sha256=digest,
                    source_date_epoch=source_date_epoch,
                )
                sidecar_size, sidecar_digest = _hash_and_size(
                    files[sidecar_name], max_bytes=max_file_bytes
                )
                records.append(
                    {
                        "format": format_name,
                        "kind": "sbom",
                        "name": sidecar_name,
                        "sha256": sidecar_digest,
                        "size": sidecar_size,
                        "subject": True,
                        "subject_name": spec.name,
                    }
                )
            record["sbom"] = {"cyclonedx": cdx_name, "spdx": spdx_name}
        records.append(record)

    records.sort(key=lambda record: str(record["name"]))
    manifest: dict[str, Any] = {
        "assets": records,
        "checksums": {
            "name": checksum_name,
            "subject_count": len([record for record in records if record["subject"] is True]),
        },
        "schema": SCHEMA,
        "source_date_epoch": source_date_epoch,
        "version": version,
    }
    if slsa_bundle is not None:
        bundle_path = files.get(slsa_bundle)
        if bundle_path is None:
            raise ReleaseManifestError(f"SLSA bundle does not exist in release input: {slsa_bundle}")
        bundle_size, bundle_digest = _hash_and_size(bundle_path, max_bytes=max_file_bytes)
        subjects = {(str(record["name"]), str(record["sha256"])) for record in _subject_records(manifest)}
        bundle_subjects = parse_slsa_subjects(bundle_path, max_bytes=max_file_bytes)
        if bundle_subjects != subjects:
            missing_subjects = sorted(subjects - bundle_subjects)
            extra_subjects = sorted(bundle_subjects - subjects)
            raise ReleaseManifestError(
                f"SLSA subject set mismatch: missing={missing_subjects}, extra={extra_subjects}"
            )
        manifest["provenance"] = {
            "kind": "slsa-provenance",
            "name": slsa_bundle,
            "sha256": bundle_digest,
            "size": bundle_size,
            "subject_count": len(subjects),
        }
    return manifest


def render_checksums(manifest: Mapping[str, Any]) -> bytes:
    """Render SHA256SUMS for exactly the manifest subject set."""

    subjects = _subject_records(manifest)
    lines: list[str] = []
    seen: set[str] = set()
    for record in sorted(subjects, key=lambda item: str(item.get("name", ""))):
        name = validate_asset_name(str(record.get("name", "")))
        if name in seen:
            raise ReleaseManifestError(f"duplicate checksum subject: {name}")
        seen.add(name)
        digest = _sha256_hex(str(record.get("sha256", "")))
        # actions/attest preserves the checksum filename verbatim as the SLSA
        # subject name.  Keep the canonical flat asset name here (without a
        # harmless-looking "./" prefix) so the attestation and manifest bind
        # the same identity as well as the same bytes.
        lines.append(f"{digest}  {name}")
    if not lines:
        raise ReleaseManifestError("cannot render an empty SHA256SUMS subject set")
    return ("\n".join(lines) + "\n").encode("ascii")


def _parse_checksums(data: bytes) -> dict[str, str]:
    try:
        text = data.decode("ascii")
    except UnicodeDecodeError as error:
        raise ReleaseManifestError("SHA256SUMS must be ASCII") from error
    parsed: dict[str, str] = {}
    lines = text.splitlines()
    if not lines or text and not text.endswith("\n"):
        raise ReleaseManifestError("SHA256SUMS must be a non-empty newline-terminated file")
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([^\n]+)", line)
        if match is None:
            raise ReleaseManifestError(f"invalid SHA256SUMS line: {line!r}")
        digest, name = match.groups()
        validate_asset_name(name)
        if name in parsed:
            raise ReleaseManifestError(f"duplicate SHA256SUMS entry: {name}")
        parsed[name] = digest
    return parsed


def verify_checksums(root: Path, manifest: Mapping[str, Any], *, checksum_name: str | None = None) -> None:
    name = checksum_name or str(manifest.get("checksums", {}).get("name", DEFAULT_CHECKSUM_NAME))
    validate_asset_name(name)
    path = Path(root) / name
    entries = _parse_checksums(_read_bytes(path, max_bytes=DEFAULT_MAX_FILE_BYTES))
    expected = {
        str(record["name"]): str(record["sha256"])
        for record in _subject_records(manifest)
    }
    if entries != expected:
        missing = sorted(set(expected) - set(entries))
        extra = sorted(set(entries) - set(expected))
        wrong = sorted(name for name in expected.keys() & entries.keys() if expected[name] != entries[name])
        raise ReleaseManifestError(
            f"SHA256SUMS does not match manifest: missing={missing}, extra={extra}, digest_mismatch={wrong}"
        )
    for asset_name, expected_digest in entries.items():
        path = Path(root) / asset_name
        _, actual_digest = _hash_and_size(path, max_bytes=DEFAULT_MAX_FILE_BYTES)
        if actual_digest != expected_digest:
            raise ReleaseManifestError(
                f"SHA256SUMS digest mismatch for {asset_name}: expected {expected_digest}, found {actual_digest}"
            )


def _json_objects_from_jsonl(path: Path, *, max_bytes: int) -> Iterator[Any]:
    data = _read_bytes(path, max_bytes=max_bytes)
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ReleaseManifestError(f"SLSA bundle is not UTF-8: {path.name}") from error
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError as error:
            raise ReleaseManifestError(f"invalid SLSA JSONL at line {line_number}: {error}") from error


def _walk_subjects(value: Any) -> Iterator[tuple[str, str]]:
    if isinstance(value, Mapping):
        direct = value.get("subject")
        if isinstance(direct, list):
            for item in direct:
                if not isinstance(item, Mapping):
                    continue
                name = item.get("name")
                digest = item.get("digest")
                if isinstance(name, str) and isinstance(digest, Mapping) and isinstance(digest.get("sha256"), str):
                    validate_asset_name(name)
                    yield name, _sha256_hex(str(digest["sha256"]))
        envelope = value.get("dsseEnvelope")
        if isinstance(envelope, Mapping) and isinstance(envelope.get("payload"), str):
            encoded = str(envelope["payload"])
            try:
                decoded = base64.b64decode(
                    encoded + "=" * (-len(encoded) % 4),
                    altchars=b"-_",
                    validate=True,
                )
                payload = json.loads(decoded)
            except (ValueError, binascii.Error, json.JSONDecodeError) as error:
                raise ReleaseManifestError("SLSA DSSE payload is not valid base64 JSON") from error
            yield from _walk_subjects(payload)
        for child in value.values():
            if child is not direct and child is not envelope:
                yield from _walk_subjects(child)
    elif isinstance(value, list):
        for child in value:
            yield from _walk_subjects(child)


def parse_slsa_subjects(path: Path, *, max_bytes: int = DEFAULT_MAX_FILE_BYTES) -> set[tuple[str, str]]:
    subjects: set[tuple[str, str]] = set()
    for obj in _json_objects_from_jsonl(Path(path), max_bytes=max_bytes):
        subjects.update(_walk_subjects(obj))
    if not subjects:
        raise ReleaseManifestError(f"SLSA bundle contains no SHA-256 subjects: {Path(path).name}")
    return subjects


def verify_manifest(
    root: Path,
    *,
    manifest_name: str = DEFAULT_MANIFEST_NAME,
    expected_version: str | None = None,
    expected_specs: Sequence[AssetSpec] | None = None,
    max_assets: int = DEFAULT_MAX_ASSETS,
    max_file_bytes: int = DEFAULT_MAX_FILE_BYTES,
    max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
) -> dict[str, Any]:
    """Verify exact files, hashes, sidecars, checksums, and SLSA subjects."""

    validate_asset_name(manifest_name)
    files = safe_top_level_files(
        root,
        max_assets=max_assets,
        max_file_bytes=max_file_bytes,
        max_total_bytes=max_total_bytes,
    )
    if manifest_name not in files:
        raise ReleaseManifestError(f"release input is missing {manifest_name}")
    manifest = _load_json(files[manifest_name])
    if not isinstance(manifest, dict):
        raise ReleaseManifestError("release manifest root must be an object")
    if _read_bytes(files[manifest_name], max_bytes=max_file_bytes) != canonical_json_bytes(manifest):
        raise ReleaseManifestError("release manifest is not canonical JSON")
    if manifest.get("schema") != SCHEMA:
        raise ReleaseManifestError(f"unsupported release manifest schema: {manifest.get('schema')!r}")
    version = validate_version(str(manifest.get("version", "")))
    if expected_version is not None and version != validate_version(expected_version):
        raise ReleaseManifestError(
            f"release manifest version {version} does not match expected {expected_version}"
        )
    source_epoch = manifest.get("source_date_epoch")
    _timestamp(source_epoch)
    records = manifest.get("assets")
    if not isinstance(records, list) or not records:
        raise ReleaseManifestError("release manifest must contain a non-empty assets list")
    names: set[str] = set()
    record_map: dict[str, Mapping[str, Any]] = {}
    for record in records:
        if not isinstance(record, dict):
            raise ReleaseManifestError("release manifest asset records must be objects")
        name = validate_asset_name(str(record.get("name", "")))
        if name in names:
            raise ReleaseManifestError(f"duplicate manifest asset: {name}")
        names.add(name)
        record_map[name] = record
        _sha256_hex(str(record.get("sha256", "")))
        if (
            not isinstance(record.get("size"), int)
            or isinstance(record.get("size"), bool)
            or record["size"] < 0
        ):
            raise ReleaseManifestError(f"invalid size for manifest asset {name}")
        if record.get("subject") not in {True, False}:
            raise ReleaseManifestError(f"manifest asset subject must be boolean: {name}")
        if name not in files:
            raise ReleaseManifestError(f"manifest asset is missing from release input: {name}")
        size, digest = _hash_and_size(files[name], max_bytes=max_file_bytes)
        if size != record["size"] or digest != record["sha256"]:
            raise ReleaseManifestError(f"manifest digest/size mismatch for {name}")
    if expected_specs is not None:
        specs = _validate_specs(expected_specs)
        expected_names = {spec.name for spec in specs}
        actual_payload_names = {
            name for name, record in record_map.items() if record.get("kind") != "sbom"
        }
        if actual_payload_names != expected_names:
            raise ReleaseManifestError(
                f"manifest payload set mismatch: missing={sorted(expected_names - actual_payload_names)}, "
                f"extra={sorted(actual_payload_names - expected_names)}"
            )
        expected_sidecars: dict[str, tuple[str, str]] = {}
        for spec in specs:
            record = record_map[spec.name]
            if record.get("kind") != spec.kind:
                raise ReleaseManifestError(
                    f"manifest kind mismatch for {spec.name}: "
                    f"expected {spec.kind}, found {record.get('kind')!r}"
                )
            if record.get("subject") is not spec.subject:
                raise ReleaseManifestError(
                    f"manifest subject policy mismatch for {spec.name}"
                )
            if spec.sbom:
                spdx_name, cdx_name = _sidecar_names(spec.name)
                if record.get("sbom") != {
                    "cyclonedx": cdx_name,
                    "spdx": spdx_name,
                }:
                    raise ReleaseManifestError(
                        f"manifest SBOM references are incomplete for {spec.name}"
                    )
                expected_sidecars[spdx_name] = (spec.name, SPDX_FORMAT)
                expected_sidecars[cdx_name] = (spec.name, CYCLONEDX_FORMAT)
            elif "sbom" in record:
                raise ReleaseManifestError(
                    f"manifest unexpectedly assigns SBOMs to {spec.name}"
                )
        actual_sidecars = {
            name for name, record in record_map.items() if record.get("kind") == "sbom"
        }
        if actual_sidecars != set(expected_sidecars):
            raise ReleaseManifestError(
                "manifest SBOM set mismatch: "
                f"missing={sorted(set(expected_sidecars) - actual_sidecars)}, "
                f"extra={sorted(actual_sidecars - set(expected_sidecars))}"
            )
        for sidecar_name, (subject_name, format_name) in expected_sidecars.items():
            sidecar = record_map[sidecar_name]
            if (
                sidecar.get("subject") is not True
                or sidecar.get("subject_name") != subject_name
                or sidecar.get("format") != format_name
            ):
                raise ReleaseManifestError(
                    f"manifest SBOM policy mismatch for {sidecar_name}"
                )

    expected_files = set(names) | {manifest_name}
    checksum_info = manifest.get("checksums")
    if not isinstance(checksum_info, dict):
        raise ReleaseManifestError("release manifest is missing checksums evidence")
    checksum_name = validate_asset_name(str(checksum_info.get("name", "")))
    if checksum_info.get("subject_count") != len(_subject_records(manifest)):
        raise ReleaseManifestError("checksum subject_count does not match manifest assets")
    expected_files.add(checksum_name)
    if checksum_name not in files:
        raise ReleaseManifestError(f"release input is missing checksum evidence: {checksum_name}")
    provenance = manifest.get("provenance")
    if provenance is not None:
        if not isinstance(provenance, dict):
            raise ReleaseManifestError("release manifest provenance evidence must be an object")
        bundle_name = validate_asset_name(str(provenance.get("name", "")))
        expected_files.add(bundle_name)
        if bundle_name not in files:
            raise ReleaseManifestError(f"release input is missing provenance evidence: {bundle_name}")
        bundle_size, bundle_digest = _hash_and_size(files[bundle_name], max_bytes=max_file_bytes)
        if bundle_size != provenance.get("size") or bundle_digest != provenance.get("sha256"):
            raise ReleaseManifestError(f"provenance evidence digest/size mismatch: {bundle_name}")
        expected_subjects = {(str(record["name"]), str(record["sha256"])) for record in _subject_records(manifest)}
        if parse_slsa_subjects(files[bundle_name], max_bytes=max_file_bytes) != expected_subjects:
            raise ReleaseManifestError("SLSA provenance subjects do not match release manifest subjects")
    unexpected = sorted(set(files) - expected_files)
    if unexpected:
        raise ReleaseManifestError("release input contains unlisted files: " + ", ".join(unexpected))

    # Every subject-sidecar relationship is checked after all hashes have been
    # checked so a malicious sidecar cannot hide behind a trusted filename.
    for name, record in record_map.items():
        if record.get("kind") != "sbom":
            continue
        parent = record.get("subject_name")
        if not isinstance(parent, str) or parent not in record_map:
            raise ReleaseManifestError(f"SBOM sidecar has invalid subject_name: {name}")
        parent_record = record_map[parent]
        expected_digest = str(parent_record["sha256"])
        format_name = str(record.get("format", ""))
        _check_normalized_sbom(
            files[name],
            format_name=format_name,
            version=version,
            artifact_name=parent,
            artifact_sha256=expected_digest,
            source_date_epoch=source_epoch,
        )
    for name, record in record_map.items():
        sbom = record.get("sbom")
        if sbom is None:
            continue
        if not isinstance(sbom, dict) or sbom.get("spdx") not in record_map or sbom.get("cyclonedx") not in record_map:
            raise ReleaseManifestError(f"asset {name} has incomplete SBOM references")
        for sidecar_name in (str(sbom["spdx"]), str(sbom["cyclonedx"])):
            if record_map[sidecar_name].get("subject_name") != name:
                raise ReleaseManifestError(f"SBOM sidecar {sidecar_name} is bound to the wrong asset")
    verify_checksums(root, manifest, checksum_name=checksum_name)
    return manifest


def finalize_release(
    root: Path,
    *,
    version: str,
    specs: Sequence[AssetSpec] | None = None,
    source_date_epoch: int = 0,
    manifest_name: str = DEFAULT_MANIFEST_NAME,
    checksum_name: str = DEFAULT_CHECKSUM_NAME,
    slsa_bundle: str | None = None,
    max_assets: int = DEFAULT_MAX_ASSETS,
    max_file_bytes: int = DEFAULT_MAX_FILE_BYTES,
    max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
) -> dict[str, Any]:
    manifest = build_manifest(
        root,
        version=version,
        specs=specs,
        source_date_epoch=source_date_epoch,
        manifest_name=manifest_name,
        checksum_name=checksum_name,
        slsa_bundle=slsa_bundle,
        max_assets=max_assets,
        max_file_bytes=max_file_bytes,
        max_total_bytes=max_total_bytes,
    )
    _write_atomic(Path(root) / manifest_name, canonical_json_bytes(manifest))
    _write_atomic(Path(root) / checksum_name, render_checksums(manifest))
    # Verify again after writing: this catches an output race or a stale
    # checksum file and makes the CLI a complete gate for the next workflow
    # step.
    return verify_manifest(
        root,
        manifest_name=manifest_name,
        expected_version=version,
        expected_specs=specs,
        max_assets=max_assets,
        max_file_bytes=max_file_bytes,
        max_total_bytes=max_total_bytes,
    )


def _read_asset_list(path: Path) -> tuple[AssetSpec, ...]:
    data = _read_bytes(path, max_bytes=1024 * 1024)
    try:
        decoded = json.loads(data)
    except json.JSONDecodeError:
        decoded = None
    if isinstance(decoded, list):
        names: list[str] = []
        specs: list[AssetSpec] = []
        for item in decoded:
            if isinstance(item, str):
                names.append(item)
            elif isinstance(item, dict):
                specs.append(
                    AssetSpec(
                        str(item["name"]),
                        str(item.get("kind", "payload")),
                        bool(item.get("sbom", False)),
                        bool(item.get("subject", True)),
                    )
                )
            else:
                raise ReleaseManifestError("asset list JSON entries must be strings or objects")
        return _validate_specs(tuple(specs) + _specs_from_names(names) if names else tuple(specs))
    names = [line.strip() for line in data.decode("utf-8").splitlines() if line.strip() and not line.lstrip().startswith("#")]
    return _specs_from_names(names)


def _cli_specs(args: argparse.Namespace, version: str) -> tuple[AssetSpec, ...]:
    if args.asset:
        return _specs_from_names(args.asset)
    if args.asset_list is not None:
        return _read_asset_list(args.asset_list)
    return expected_asset_specs(
        version,
        include_aarch64=not args.exclude_aarch64,
        include_npm=not args.exclude_npm,
        include_crate=not args.exclude_crate,
        include_package_metadata=not args.exclude_package_metadata,
        include_pgo=args.include_pgo,
    )


def _add_contract_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--asset", action="append", help="explicit top-level payload name (repeatable)")
    parser.add_argument("--asset-list", type=Path, help="newline or JSON asset specification file")
    parser.add_argument("--exclude-aarch64", action="store_true")
    parser.add_argument("--exclude-npm", action="store_true")
    parser.add_argument("--exclude-crate", action="store_true")
    parser.add_argument("--exclude-package-metadata", action="store_true")
    parser.add_argument(
        "--include-pgo",
        "--include-profiles",
        dest="include_pgo",
        action="store_true",
        help="publish the PGO profile/training evidence files",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    expected = subparsers.add_parser("expected", help="print the exact expected asset contract")
    expected.add_argument("--version", required=True)
    _add_contract_options(expected)

    finalize = subparsers.add_parser("finalize", help="write RELEASE-MANIFEST.json and SHA256SUMS")
    finalize.add_argument("--input-dir", required=True, type=Path)
    finalize.add_argument("--version", required=True)
    finalize.add_argument("--source-date-epoch", type=int, default=int(os.environ.get("SOURCE_DATE_EPOCH", "0")))
    finalize.add_argument("--manifest-name", default=DEFAULT_MANIFEST_NAME)
    finalize.add_argument("--checksum-name", default=DEFAULT_CHECKSUM_NAME)
    finalize.add_argument("--slsa-bundle", help="existing in-directory SLSA JSONL evidence filename")
    finalize.add_argument("--max-assets", type=int, default=DEFAULT_MAX_ASSETS)
    finalize.add_argument("--max-file-bytes", type=int, default=DEFAULT_MAX_FILE_BYTES)
    finalize.add_argument("--max-total-bytes", type=int, default=DEFAULT_MAX_TOTAL_BYTES)
    _add_contract_options(finalize)

    verify = subparsers.add_parser("verify", help="verify a finalized release directory")
    verify.add_argument("--input-dir", required=True, type=Path)
    verify.add_argument("--version")
    verify.add_argument("--manifest-name", default=DEFAULT_MANIFEST_NAME)
    verify.add_argument("--max-assets", type=int, default=DEFAULT_MAX_ASSETS)
    verify.add_argument("--max-file-bytes", type=int, default=DEFAULT_MAX_FILE_BYTES)
    verify.add_argument("--max-total-bytes", type=int, default=DEFAULT_MAX_TOTAL_BYTES)
    _add_contract_options(verify)

    normalize = subparsers.add_parser("normalize-sbom", help="normalize one Syft SPDX/CycloneDX JSON sidecar")
    normalize.add_argument("--format", choices=(SPDX_FORMAT, CYCLONEDX_FORMAT), required=True)
    normalize.add_argument("--input", required=True, type=Path)
    normalize.add_argument("--output", required=True, type=Path)
    normalize.add_argument("--artifact", required=True)
    normalize.add_argument("--artifact-sha256", required=True)
    normalize.add_argument("--version", required=True)
    normalize.add_argument("--source-date-epoch", type=int, default=int(os.environ.get("SOURCE_DATE_EPOCH", "0")))

    generate = subparsers.add_parser(
        "generate-sboms",
        help="bounded-scan each expected payload with Syft and write normalized sidecars",
    )
    generate.add_argument("--input-dir", required=True, type=Path)
    generate.add_argument("--version", required=True)
    generate.add_argument("--syft", default="syft")
    generate.add_argument("--source-date-epoch", type=int, default=int(os.environ.get("SOURCE_DATE_EPOCH", "0")))
    generate.add_argument("--max-file-bytes", type=int, default=DEFAULT_MAX_FILE_BYTES)
    generate.add_argument("--max-total-bytes", type=int, default=DEFAULT_MAX_TOTAL_BYTES)
    generate.add_argument("--max-members", type=int, default=DEFAULT_MAX_ARCHIVE_MEMBERS)
    generate.add_argument("--max-member-bytes", type=int, default=DEFAULT_MAX_MEMBER_BYTES)
    generate.add_argument("--max-expanded-bytes", type=int, default=DEFAULT_MAX_EXPANDED_BYTES)
    _add_contract_options(generate)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.command == "expected":
            specs = _cli_specs(args, validate_version(args.version))
            print(canonical_json_bytes([dataclasses.asdict(spec) for spec in specs]).decode("utf-8"), end="")
        elif args.command == "normalize-sbom":
            normalize_sbom(
                args.input,
                args.output,
                format_name=args.format,
                version=args.version,
                artifact_name=args.artifact,
                artifact_sha256=args.artifact_sha256,
                source_date_epoch=args.source_date_epoch,
            )
        elif args.command == "generate-sboms":
            version = validate_version(args.version)
            specs = _cli_specs(args, version)
            generated = generate_sboms(
                args.input_dir,
                version=version,
                specs=specs,
                syft=args.syft,
                source_date_epoch=args.source_date_epoch,
                max_file_bytes=args.max_file_bytes,
                max_total_bytes=args.max_total_bytes,
                max_members=args.max_members,
                max_member_bytes=args.max_member_bytes,
                max_expanded_bytes=args.max_expanded_bytes,
            )
            for name in generated:
                print(name)
        elif args.command == "finalize":
            version = validate_version(args.version)
            specs = _cli_specs(args, version)
            manifest = finalize_release(
                args.input_dir,
                version=version,
                specs=specs,
                source_date_epoch=args.source_date_epoch,
                manifest_name=args.manifest_name,
                checksum_name=args.checksum_name,
                slsa_bundle=args.slsa_bundle,
                max_assets=args.max_assets,
                max_file_bytes=args.max_file_bytes,
                max_total_bytes=args.max_total_bytes,
            )
            print(f"finalized {manifest['version']} with {len(manifest['assets'])} assets")
        elif args.command == "verify":
            expected_specs = None
            if args.asset or args.asset_list is not None or args.version is not None:
                manifest_version = args.version or "0.0.0"
                expected_specs = _cli_specs(args, validate_version(manifest_version))
            manifest = verify_manifest(
                args.input_dir,
                manifest_name=args.manifest_name,
                expected_version=args.version,
                expected_specs=expected_specs,
                max_assets=args.max_assets,
                max_file_bytes=args.max_file_bytes,
                max_total_bytes=args.max_total_bytes,
            )
            print(f"verified {manifest['version']} with {len(manifest['assets'])} assets")
        else:  # pragma: no cover - argparse prevents this
            parser.error(f"unknown command: {args.command}")
    except ReleaseManifestError as error:
        print(f"release manifest error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
