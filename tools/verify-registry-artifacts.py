#!/usr/bin/env python3
"""Verify registry artifacts and read registry metadata without publishing.

The release workflow deliberately separates artifact assembly from publishing.
This checker is the read-only boundary between those jobs: it selects one
exact wheel set, one npm tarball, and one Cargo crate, validates their local
metadata and digests, and then performs GET-only checks against PyPI, npm, and
crates.io.  A package version may be absent (the normal pre-publish state) or
already exact (an idempotent retry); partial, extra, and digest-mismatched
states always fail.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import stat
import sys
import tarfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Mapping, Sequence


PYPI_PROJECT = "forge-normalizer"
NPM_PACKAGE = "@forge-normalizer/wasm"
CRATE_NAME = "forge-normalizer"
REPOSITORY_URL = "https://github.com/penguin425/audio-normalizer"

DEFAULT_PYPI_URL = f"https://pypi.org/pypi/{PYPI_PROJECT}/json"
DEFAULT_NPM_URL = (
    "https://registry.npmjs.org/"
    + urllib.parse.quote(NPM_PACKAGE, safe="")
)
DEFAULT_CRATES_URL = (
    f"https://crates.io/api/v1/crates/{CRATE_NAME}/{{version}}"
)

NPM_MEMBERS = frozenset(
    {
        "package/LICENSE",
        "package/README.md",
        "package/forge_normalizer_wasm.js",
        "package/forge_normalizer_wasm_bg.wasm",
        "package/forge_normalizer_wasm_bg.wasm.d.ts",
        "package/index.js",
        "package/index.d.ts",
        "package/package.json",
    }
)
NPM_FILES_FIELD = frozenset(
    {
        "LICENSE",
        "README.md",
        "forge_normalizer_wasm.js",
        "forge_normalizer_wasm_bg.wasm",
        "forge_normalizer_wasm_bg.wasm.d.ts",
        "index.js",
        "index.d.ts",
    }
)
DEFAULT_WHEEL_PLATFORMS = frozenset(
    {
        "manylinux_2_34_aarch64",
        "manylinux_2_34_x86_64",
        "macosx_10_12_x86_64",
        "macosx_11_0_arm64",
        "win_amd64",
    }
)
REGISTRIES = ("pypi", "npm", "crates")
MAX_CRATE_MEMBERS = 50_000
MAX_CRATE_EXPANDED_BYTES = 128 * 1024 * 1024
MAX_CRATE_METADATA_BYTES = 1024 * 1024


class VerificationError(ValueError):
    """A local artifact or remote registry state failed closed."""


@dataclass(frozen=True)
class Artifact:
    kind: str
    name: str
    version: str
    path: Path
    digests: Mapping[str, str]
    platform: str | None = None

    def as_json(self) -> dict[str, object]:
        result: dict[str, object] = {
            "kind": self.kind,
            "name": self.name,
            "version": self.version,
            "path": str(self.path),
            "digests": dict(self.digests),
        }
        if self.platform is not None:
            result["platform"] = self.platform
        return result


def _regular_file(path: Path, label: str) -> Path:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise VerificationError(f"{label} does not exist: {path}: {error}") from error
    if stat.S_ISLNK(metadata.st_mode):
        raise VerificationError(f"{label} must not be a symbolic link: {path}")
    if not stat.S_ISREG(metadata.st_mode):
        raise VerificationError(f"{label} must be a regular file: {path}")
    return path


def _digest_file(path: Path) -> dict[str, str]:
    sha256 = hashlib.sha256()
    sha1 = hashlib.sha1()
    sha512 = hashlib.sha512()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            sha256.update(chunk)
            sha1.update(chunk)
            sha512.update(chunk)
    return {
        "sha256": sha256.hexdigest(),
        "sha1": sha1.hexdigest(),
        "integrity": "sha512-"
        + base64.b64encode(sha512.digest()).decode("ascii"),
    }


def _safe_member_name(name: str) -> None:
    if not name or "\x00" in name or "\\" in name:
        raise VerificationError(f"unsafe archive member name: {name!r}")
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or "." in path.parts:
        raise VerificationError(f"unsafe archive member name: {name!r}")


def _read_text(data: bytes, label: str) -> str:
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise VerificationError(f"{label} is not valid UTF-8") from error


def _headers(text: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        result.setdefault(key.strip().lower(), value.strip())
    return result


def inspect_wheel(path: Path, version: str) -> Artifact:
    """Validate one wheel filename and its core metadata."""

    path = _regular_file(path, "wheel")
    match = re.fullmatch(
        r"forge_normalizer-(?P<version>[0-9A-Za-z][0-9A-Za-z.+!_-]*)-"
        r"py3-none-(?P<platform>[A-Za-z0-9_.]+)\.whl",
        path.name,
    )
    if match is None:
        raise VerificationError(f"unexpected wheel filename: {path.name}")
    if match.group("version") != version:
        raise VerificationError(
            f"wheel {path.name} has version {match.group('version')}, expected {version}"
        )
    platform = match.group("platform")

    try:
        archive = zipfile.ZipFile(path)
    except (OSError, zipfile.BadZipFile) as error:
        raise VerificationError(f"cannot read wheel {path}: {error}") from error
    with archive:
        members = archive.infolist()
        names: list[str] = []
        for member in members:
            _safe_member_name(member.filename)
            if member.filename.endswith("/"):
                raise VerificationError(f"wheel contains a directory member: {member.filename}")
            mode = (member.external_attr >> 16) & 0o177777
            if stat.S_ISLNK(mode):
                raise VerificationError(f"wheel contains a symbolic link: {member.filename}")
            if member.filename in names:
                raise VerificationError(f"wheel contains duplicate member: {member.filename}")
            names.append(member.filename)

        metadata_members = [name for name in names if name.endswith(".dist-info/METADATA")]
        wheel_members = [name for name in names if name.endswith(".dist-info/WHEEL")]
        if len(metadata_members) != 1 or len(wheel_members) != 1:
            raise VerificationError(
                f"wheel must contain one METADATA and WHEEL member: {path.name}"
            )
        metadata = _headers(
            _read_text(archive.read(metadata_members[0]), f"{path.name} METADATA")
        )
        wheel_text = _read_text(archive.read(wheel_members[0]), f"{path.name} WHEEL")
        normalized_name = re.sub(r"[-_.]+", "-", metadata.get("name", "")).lower()
        if normalized_name != PYPI_PROJECT:
            raise VerificationError(
                f"wheel {path.name} has package name {metadata.get('name')!r}"
            )
        if metadata.get("version") != version:
            raise VerificationError(
                f"wheel {path.name} metadata has version {metadata.get('version')!r}"
            )
        expected_tag = f"py3-none-{platform}"
        tags = {
            line.split(":", 1)[1].strip()
            for line in wheel_text.splitlines()
            if ":" in line and line.split(":", 1)[0].strip().lower() == "tag"
        }
        if expected_tag not in tags:
            raise VerificationError(
                f"wheel {path.name} is missing compatibility tag {expected_tag}"
            )

    return Artifact(
        kind="python-wheel",
        name=PYPI_PROJECT,
        version=version,
        path=path,
        digests=_digest_file(path),
        platform=platform,
    )


def _is_arm_platform(platform: str) -> bool:
    lowered = platform.lower()
    return "aarch64" in lowered or lowered.endswith("arm64")


def select_wheels(
    artifact_dir: Path | None,
    explicit: Sequence[Path],
    version: str,
    expected_platforms: Sequence[str] | None = None,
    require_arm64: bool = True,
) -> list[Artifact]:
    """Select all and only the requested wheel set.

    When ``explicit`` is empty every ``*.whl`` below ``artifact_dir`` is the
    candidate set.  When explicit paths are supplied, an additional wheel in
    the artifact directory is rejected instead of silently being omitted.
    ``expected_platforms`` defaults to the v0.189.17 five-platform release
    contract.  A caller may provide an explicit set for a staged rollout, but
    it is always compared exactly rather than treating a glob as an allowlist.
    """

    if explicit:
        selected_paths = [Path(item) for item in explicit]
        if len({path.resolve() for path in selected_paths}) != len(selected_paths):
            raise VerificationError("the Python wheel selection contains duplicates")
        if artifact_dir is not None:
            candidates = {
                path.resolve()
                for path in artifact_dir.rglob("*.whl")
                if path.is_file()
            }
            selected = {path.resolve() for path in selected_paths}
            extra = sorted(candidates - selected)
            if extra:
                rendered = ", ".join(str(path) for path in extra)
                raise VerificationError(f"unselected wheel artifacts are present: {rendered}")
    else:
        if artifact_dir is None:
            raise VerificationError("--artifact-dir is required when --wheel is omitted")
        selected_paths = sorted(
            path for path in artifact_dir.rglob("*.whl") if path.is_file()
        )
    if not selected_paths:
        raise VerificationError("the exact Python wheel set is empty")

    wheels = [inspect_wheel(path, version) for path in selected_paths]
    platforms = [wheel.platform for wheel in wheels]
    assert all(platform is not None for platform in platforms)
    platform_set = {platform for platform in platforms if platform is not None}
    if len(platform_set) != len(platforms):
        raise VerificationError("the Python wheel set contains duplicate platform tags")
    expected = set(
        DEFAULT_WHEEL_PLATFORMS
        if expected_platforms is None
        else expected_platforms
    )
    if platform_set != expected:
        raise VerificationError(
            "Python wheel platform set mismatch; "
            f"missing={sorted(expected - platform_set)}, "
            f"extra={sorted(platform_set - expected)}"
        )
    if require_arm64 and not any(_is_arm_platform(platform) for platform in platform_set):
        raise VerificationError(
            "Python wheel set must include an ARM64/aarch64 wheel"
        )
    return sorted(wheels, key=lambda wheel: wheel.name)


def inspect_npm_tarball(
    path: Path, version: str, package_name: str = NPM_PACKAGE
) -> Artifact:
    """Verify the exact public npm package payload emitted by build-wasm-package."""

    path = _regular_file(path, "npm tarball")
    expected_filename = "forge-normalizer-wasm-" + version + ".tgz"
    if path.name != expected_filename:
        raise VerificationError(
            f"unexpected npm tarball filename {path.name}; expected {expected_filename}"
        )
    try:
        archive = tarfile.open(path, mode="r:gz")
    except (OSError, tarfile.TarError) as error:
        raise VerificationError(f"cannot read npm tarball {path}: {error}") from error
    with archive:
        members = archive.getmembers()
        names: list[str] = []
        for member in members:
            _safe_member_name(member.name)
            if not member.isfile():
                raise VerificationError(f"npm tarball contains a non-file member: {member.name}")
            if member.name in names:
                raise VerificationError(f"npm tarball contains duplicate member: {member.name}")
            names.append(member.name)
        actual = set(names)
        if actual != NPM_MEMBERS:
            raise VerificationError(
                "npm tarball member mismatch; "
                f"missing={sorted(NPM_MEMBERS - actual)}, "
                f"extra={sorted(actual - NPM_MEMBERS)}"
            )
        package_member = archive.extractfile("package/package.json")
        if package_member is None:
            raise VerificationError("npm tarball has no package/package.json")
        try:
            package = json.loads(package_member.read().decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise VerificationError("npm package.json is not valid UTF-8 JSON") from error
        if not isinstance(package, dict):
            raise VerificationError("npm package.json must be an object")
        if package.get("name") != package_name:
            raise VerificationError(
                f"npm package name is {package.get('name')!r}; expected {package_name!r}"
            )
        if package.get("version") != version:
            raise VerificationError(
                f"npm package version is {package.get('version')!r}; expected {version!r}"
            )
        repository = package.get("repository")
        if (
            not isinstance(repository, dict)
            or repository.get("type") != "git"
            or repository.get("url") != REPOSITORY_URL
        ):
            raise VerificationError(
                "npm package repository.url does not match the canonical GitHub URL"
            )
        publish_config = package.get("publishConfig")
        if not isinstance(publish_config, dict) or publish_config.get("access") != "public":
            raise VerificationError("npm package must explicitly set publishConfig.access=public")
        if package.get("private") is True:
            raise VerificationError("npm package must not be private")
        files = package.get("files")
        if (
            not isinstance(files, list)
            or len(files) != len(NPM_FILES_FIELD)
            or any(not isinstance(item, str) for item in files)
            or set(files) != NPM_FILES_FIELD
        ):
            raise VerificationError("npm package files field does not match the exact payload")

    return Artifact(
        kind="npm",
        name=package_name,
        version=version,
        path=path,
        digests=_digest_file(path),
    )


def inspect_crate(path: Path, version: str, crate_name: str = CRATE_NAME) -> Artifact:
    """Verify a Cargo package archive without invoking Cargo or unpacking it."""

    path = _regular_file(path, "crate")
    expected_filename = f"{crate_name}-{version}.crate"
    if path.name != expected_filename:
        raise VerificationError(
            f"unexpected crate filename {path.name}; expected {expected_filename}"
        )
    prefix = f"{crate_name}-{version}/"
    try:
        archive = tarfile.open(path, mode="r:gz")
    except (OSError, tarfile.TarError) as error:
        raise VerificationError(f"cannot read crate {path}: {error}") from error
    with archive:
        members = archive.getmembers()
        if len(members) > MAX_CRATE_MEMBERS:
            raise VerificationError(
                f"crate has {len(members)} members, above the {MAX_CRATE_MEMBERS}-member limit"
            )
        names: set[str] = set()
        expanded_bytes = 0
        for member in members:
            _safe_member_name(member.name)
            if member.name in names:
                raise VerificationError(f"crate contains duplicate member: {member.name}")
            names.add(member.name)
            if not member.name.startswith(prefix):
                raise VerificationError(
                    f"crate member is outside {prefix}: {member.name}"
                )
            if not (member.isfile() or member.isdir()):
                raise VerificationError(f"crate contains an unsafe member: {member.name}")
            if member.isfile():
                if member.size < 0:
                    raise VerificationError(f"crate member has a negative size: {member.name}")
                expanded_bytes += member.size
                if expanded_bytes > MAX_CRATE_EXPANDED_BYTES:
                    raise VerificationError(
                        "crate expanded size exceeds "
                        f"{MAX_CRATE_EXPANDED_BYTES} bytes"
                    )
        if not members:
            raise VerificationError("crate archive is empty")
        cargo_toml_name = prefix + "Cargo.toml"
        cargo_toml = next(
            (member for member in members if member.name == cargo_toml_name),
            None,
        )
        if cargo_toml is None or not cargo_toml.isfile():
            raise VerificationError("crate has no regular Cargo.toml")
        if cargo_toml.size > MAX_CRATE_METADATA_BYTES:
            raise VerificationError("crate Cargo.toml exceeds the metadata size limit")
        metadata_stream = archive.extractfile(cargo_toml)
        if metadata_stream is None:
            raise VerificationError("crate Cargo.toml cannot be read")
        try:
            cargo_metadata = tomllib.loads(
                _read_text(metadata_stream.read(), "crate Cargo.toml")
            )
        except tomllib.TOMLDecodeError as error:
            raise VerificationError("crate Cargo.toml is not valid TOML") from error
        package = cargo_metadata.get("package")
        if not isinstance(package, dict):
            raise VerificationError("crate Cargo.toml has no package table")
        if package.get("name") != crate_name:
            raise VerificationError(
                f"crate Cargo.toml name is {package.get('name')!r}; expected {crate_name!r}"
            )
        if package.get("version") != version:
            raise VerificationError(
                f"crate Cargo.toml version is {package.get('version')!r}; expected {version!r}"
            )
        if package.get("repository") != REPOSITORY_URL:
            raise VerificationError(
                "crate Cargo.toml repository does not match the canonical GitHub URL"
            )

    return Artifact(
        kind="crate",
        name=crate_name,
        version=version,
        path=path,
        digests=_digest_file(path),
    )


def _url(template: str, version: str, package: str) -> str:
    return template.replace("{version}", urllib.parse.quote(version, safe="")).replace(
        "{package}", urllib.parse.quote(package, safe="")
    )


def fetch_json(url: str, timeout: float = 20.0) -> Mapping[str, Any] | None:
    """GET one JSON document; return ``None`` only for HTTP 404.

    There is intentionally no token, POST, PUT, DELETE, or retry-on-write path
    in this helper.  Registry verification must be safe to run before and
    after an interrupted publish.
    """

    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/json",
            "User-Agent": "forge-normalizer-registry-verifier/1",
        },
        method="GET",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise VerificationError(f"registry GET {url} returned HTTP {error.code}") from error
    except (OSError, urllib.error.URLError) as error:
        raise VerificationError(f"registry GET {url} failed: {error}") from error
    try:
        document = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"registry GET {url} returned invalid JSON") from error
    if not isinstance(document, dict):
        raise VerificationError(f"registry GET {url} returned a non-object JSON document")
    return document


def _state(
    status: str,
    *,
    missing: Iterable[str] = (),
    extra: Iterable[str] = (),
    mismatched: Iterable[str] = (),
) -> dict[str, object]:
    return {
        "state": status,
        "missing": sorted(missing),
        "extra": sorted(extra),
        "mismatched": sorted(mismatched),
    }


def compare_pypi(document: Mapping[str, Any] | None, version: str, wheels: Sequence[Artifact]) -> dict[str, object]:
    expected = {
        wheel.path.name: (wheel.digests["sha256"], wheel.path.stat().st_size)
        for wheel in wheels
    }
    if document is None:
        return _state("absent")
    info = document.get("info")
    if isinstance(info, dict) and info.get("name") not in (None, PYPI_PROJECT):
        return _state("mismatch", mismatched=["package-name"])
    releases = document.get("releases")
    if not isinstance(releases, dict):
        raise VerificationError("PyPI response has no releases object")
    entries = releases.get(version)
    if entries is None or entries == []:
        return _state("absent")
    if not isinstance(entries, list):
        raise VerificationError("PyPI release entry is not a list")
    actual: dict[str, tuple[str, int]] = {}
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("filename"), str):
            raise VerificationError("PyPI release entry is malformed")
        filename = entry["filename"]
        digest = entry.get("digests")
        sha256 = digest.get("sha256") if isinstance(digest, dict) else None
        if not isinstance(sha256, str):
            return _state("mismatch", mismatched=[filename + ":sha256"])
        size = entry.get("size")
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            return _state("mismatch", mismatched=[filename + ":size"])
        if filename in actual:
            return _state("mismatch", mismatched=[filename + ":duplicate"])
        actual[filename] = (sha256.lower(), size)
    missing = set(expected) - set(actual)
    extra = set(actual) - set(expected)
    mismatched = {
        filename for filename in expected.keys() & actual.keys()
        if expected[filename] != actual[filename]
    }
    if mismatched:
        return _state("mismatch", missing=missing, extra=extra, mismatched=mismatched)
    if missing:
        return _state("partial", missing=missing, extra=extra)
    if extra:
        return _state("extra", extra=extra)
    return _state("exact")


def compare_npm(
    document: Mapping[str, Any] | None, version: str, npm: Artifact
) -> dict[str, object]:
    if document is None:
        return _state("absent")
    if document.get("name") not in (None, npm.name):
        return _state("mismatch", mismatched=["package-name"])
    versions = document.get("versions")
    if not isinstance(versions, dict):
        raise VerificationError("npm response has no versions object")
    entry = versions.get(version)
    if entry is None:
        return _state("absent")
    if not isinstance(entry, dict):
        return _state("mismatch", mismatched=["version-metadata"])
    dist = entry.get("dist")
    if not isinstance(dist, dict):
        return _state("mismatch", mismatched=["dist"])
    mismatched: list[str] = []
    if entry.get("name") not in (None, npm.name):
        mismatched.append("name")
    if entry.get("version") not in (None, version):
        mismatched.append("version")
    if dist.get("shasum", "").lower() != npm.digests["sha1"]:
        mismatched.append("dist.shasum")
    if dist.get("integrity") != npm.digests["integrity"]:
        mismatched.append("dist.integrity")
    if mismatched:
        return _state("mismatch", mismatched=mismatched)
    return _state("exact")


def compare_crates(
    document: Mapping[str, Any] | None, version: str, crate: Artifact
) -> dict[str, object]:
    if document is None:
        return _state("absent")
    entry = document.get("version")
    if not isinstance(entry, dict):
        return _state("mismatch", mismatched=["version-metadata"])
    mismatched: list[str] = []
    if entry.get("crate") not in (None, crate.name):
        mismatched.append("crate")
    if entry.get("num") not in (None, version):
        mismatched.append("num")
    if entry.get("checksum") != crate.digests["sha256"]:
        mismatched.append("checksum")
    if mismatched:
        return _state("mismatch", mismatched=mismatched)
    return _state("exact")


def _enforce_states(registries: Mapping[str, Mapping[str, object]], mode: str) -> None:
    allowed = {"exact"} if mode == "post" else {"absent", "exact"}
    failures = {
        name: value.get("state")
        for name, value in registries.items()
        if value.get("state") not in allowed
    }
    if failures:
        rendered = ", ".join(f"{name}={state}" for name, state in sorted(failures.items()))
        raise VerificationError(
            f"registry state is not safe for {mode} verification: {rendered}"
        )


def _one_artifact(
    artifact_dir: Path | None,
    explicit: Path | None,
    suffix: str,
    label: str,
) -> Path:
    if explicit is not None:
        return explicit
    if artifact_dir is None:
        raise VerificationError(f"--artifact-dir is required to select the {label}")
    candidates = sorted(
        path for path in artifact_dir.rglob(f"*{suffix}") if path.is_file()
    )
    if len(candidates) != 1:
        rendered = ", ".join(str(path) for path in candidates)
        raise VerificationError(
            f"expected exactly one {label} in {artifact_dir}; found {len(candidates)}"
            + (f": {rendered}" if rendered else "")
        )
    return candidates[0]


def verify_npm_only(path: Path, version: str) -> dict[str, object]:
    return inspect_npm_tarball(path, version).as_json()


def verify(
    *,
    version: str,
    artifact_dir: Path | None,
    wheel_paths: Sequence[Path],
    expected_wheel_platforms: Sequence[str] | None,
    require_arm64: bool,
    npm_path: Path | None,
    crate_path: Path | None,
    state_mode: str,
    pypi_url: str,
    npm_url: str,
    crates_url: str,
    timeout: float,
    registries: Sequence[str] | None = None,
) -> dict[str, object]:
    wheels = select_wheels(
        artifact_dir,
        wheel_paths,
        version,
        expected_platforms=expected_wheel_platforms,
        require_arm64=require_arm64,
    )
    npm = inspect_npm_tarball(
        _one_artifact(artifact_dir, npm_path, ".tgz", "npm tarball"), version
    )
    crate = inspect_crate(
        _one_artifact(artifact_dir, crate_path, ".crate", "crate"), version
    )

    requested = tuple(registries or REGISTRIES)
    if len(set(requested)) != len(requested) or any(
        name not in REGISTRIES for name in requested
    ):
        raise VerificationError(f"invalid registry selection: {requested!r}")
    registry_states: dict[str, dict[str, object]] = {}
    if state_mode != "local":
        if "pypi" in requested:
            pypi_document = fetch_json(_url(pypi_url, version, PYPI_PROJECT), timeout)
            registry_states["pypi"] = compare_pypi(pypi_document, version, wheels)
        if "npm" in requested:
            npm_document = fetch_json(_url(npm_url, version, NPM_PACKAGE), timeout)
            registry_states["npm"] = compare_npm(npm_document, version, npm)
        if "crates" in requested:
            crates_document = fetch_json(_url(crates_url, version, CRATE_NAME), timeout)
            registry_states["crates"] = compare_crates(crates_document, version, crate)
        _enforce_states(registry_states, state_mode)
    return {
        "version": version,
        "artifacts": {
            "python": [wheel.as_json() for wheel in wheels],
            "npm": npm.as_json(),
            "crate": crate.as_json(),
        },
        "registries": registry_states,
        "state_mode": state_mode,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Verify Forge registry artifacts and GET-only registry state."
    )
    parser.add_argument("--version", help="release version, for example 0.189.17")
    parser.add_argument(
        "--artifact-dir",
        "--artifacts",
        "--dist-dir",
        dest="artifact_dir",
        type=Path,
        help="directory containing the exact wheel/npm/crate artifact set",
    )
    parser.add_argument(
        "--wheel",
        "--python-wheel",
        dest="wheel_paths",
        type=Path,
        action="append",
        default=[],
        help="explicit Python wheel (repeat for the exact set)",
    )
    parser.add_argument(
        "--wheel-platform",
        dest="wheel_platforms",
        action="append",
        help="expected wheel platform tag (repeat; exact set comparison)",
    )
    parser.add_argument(
        "--allow-missing-arm64",
        action="store_true",
        help="disable the default requirement for an ARM64/aarch64 wheel",
    )
    parser.add_argument(
        "--npm",
        "--npm-tarball",
        dest="npm_path",
        type=Path,
        help="explicit npm .tgz path",
    )
    parser.add_argument(
        "--crate",
        "--crate-package",
        dest="crate_path",
        type=Path,
        help="explicit Cargo .crate path",
    )
    parser.add_argument(
        "--state",
        choices=("local", "pre", "post"),
        default="pre",
        help=(
            "local skips network; pre allows absent or exact (idempotent retry); "
            "post requires exact"
        ),
    )
    parser.add_argument(
        "--registry",
        choices=REGISTRIES,
        action="append",
        help="registry to query (repeatable; defaults to all)",
    )
    parser.add_argument("--pypi-url", "--pypi-index", default=DEFAULT_PYPI_URL)
    parser.add_argument("--npm-url", "--npm-registry", default=DEFAULT_NPM_URL)
    parser.add_argument("--crates-url", "--crates-index", default=DEFAULT_CRATES_URL)
    parser.add_argument("--timeout", type=float, default=20.0)
    local_verification = parser.add_mutually_exclusive_group()
    local_verification.add_argument(
        "--verify-npm",
        type=Path,
        help="verify one npm tarball locally and skip all registry/network checks",
    )
    local_verification.add_argument(
        "--verify-crate",
        type=Path,
        help="verify one Cargo .crate locally and skip all registry/network checks",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if not args.version:
        raise SystemExit("--version is required")
    if args.verify_npm is not None:
        report = verify_npm_only(args.verify_npm, args.version)
    elif args.verify_crate is not None:
        report = inspect_crate(args.verify_crate, args.version).as_json()
    else:
        if args.artifact_dir is None and not args.wheel_paths:
            raise SystemExit("--artifact-dir or at least one --wheel is required")
        report = verify(
            version=args.version,
            artifact_dir=args.artifact_dir,
            wheel_paths=args.wheel_paths,
            expected_wheel_platforms=args.wheel_platforms,
            require_arm64=not args.allow_missing_arm64,
            npm_path=args.npm_path,
            crate_path=args.crate_path,
            state_mode=args.state,
            pypi_url=args.pypi_url,
            npm_url=args.npm_url,
            crates_url=args.crates_url,
            timeout=args.timeout,
            registries=args.registry,
        )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except VerificationError as error:
        print(f"registry artifact verification failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
