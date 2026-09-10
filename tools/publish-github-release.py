#!/usr/bin/env python3
"""Publish one exact, immutable GitHub release through a draft state machine.

The module deliberately keeps the GitHub API behind :class:`ReleaseApi`.  The
workflow can use :class:`GitHubApi`, while tests and release dry-runs can use a
small fixture implementation without making network calls.

The irreversible operation is ``publish_release``.  Everything before that
operation is either validation, draft creation, or an upload that can be
reconciled by reading the draft again.  Assets are never replaced or deleted.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import socket
import stat
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol, Sequence
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode, urljoin, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener


RELEASE_TAG = re.compile(
    r"^v((?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*))$",
    re.ASCII,
)
SHA256 = re.compile(r"^[0-9a-f]{64}$", re.ASCII)
COMMIT_SHA = re.compile(r"^[0-9a-f]{40}$", re.IGNORECASE)
REPOSITORY = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
MAX_ASSET_BYTES = 512 * 1024 * 1024


class _NoRedirectHandler(HTTPRedirectHandler):
    """Keep credentials and release bytes on the explicitly checked origin."""

    def redirect_request(self, *_args: Any, **_kwargs: Any) -> None:
        return None


_NO_REDIRECT_OPENER = build_opener(_NoRedirectHandler())


class PublicationError(RuntimeError):
    """A release cannot be published without a potentially unsafe mutation."""


class ManifestError(PublicationError):
    """The local release directory and its exact manifest disagree."""


class ApiError(PublicationError):
    """A GitHub API request failed.

    ``status`` is ``None`` for transport failures.  ``timeout`` means that a
    mutating request may have reached GitHub and must be reconciled by a
    subsequent read before another mutation is attempted.
    """

    def __init__(
        self,
        message: str,
        *,
        status: int | None = None,
        timeout: bool = False,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.timeout = timeout


class ApiTimeout(ApiError):
    def __init__(self, message: str) -> None:
        super().__init__(message, timeout=True)


@dataclass(frozen=True)
class ExpectedAsset:
    """One local file explicitly allowed to become a release asset."""

    name: str
    path: Path
    size: int
    sha256: str

    def verify_local(self) -> bytes:
        """Read the file and prove it still matches the manifest."""

        try:
            metadata = self.path.lstat()
        except OSError as error:
            raise ManifestError(f"asset disappeared: {self.name}: {error}") from error
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise ManifestError(f"asset is not a regular file: {self.name}")
        if metadata.st_size != self.size:
            raise ManifestError(
                f"local size changed for {self.name}: "
                f"expected {self.size}, found {metadata.st_size}"
            )
        if metadata.st_size > MAX_ASSET_BYTES:
            raise ManifestError(
                f"asset exceeds the {MAX_ASSET_BYTES}-byte publication limit: {self.name}"
            )
        try:
            data = self.path.read_bytes()
        except OSError as error:
            raise ManifestError(f"cannot read asset {self.name}: {error}") from error
        actual = hashlib.sha256(data).hexdigest()
        if actual != self.sha256:
            raise ManifestError(
                f"local digest changed for {self.name}: "
                f"expected {self.sha256}, found {actual}"
            )
        return data


@dataclass(frozen=True)
class ReleaseAsset:
    name: str
    size: int
    sha256: str | None
    state: str = "uploaded"

    @classmethod
    def from_json(cls, value: dict[str, Any]) -> "ReleaseAsset":
        if not isinstance(value, dict):
            raise ApiError(f"GitHub returned a non-object release asset: {value!r}")
        digest = value.get("digest")
        if isinstance(digest, str) and digest.startswith("sha256:"):
            digest = digest.removeprefix("sha256:")
        elif not isinstance(digest, str):
            digest = None
        size = value.get("size")
        if not isinstance(size, int) or isinstance(size, bool):
            raise ApiError(f"GitHub returned an invalid asset size for {value!r}")
        name = value.get("name")
        if not isinstance(name, str):
            raise ApiError(f"GitHub returned an invalid asset name for {value!r}")
        return cls(
            name=name,
            size=size,
            sha256=digest,
            state=str(value.get("state", "uploaded")),
        )


@dataclass(frozen=True)
class Release:
    id: int
    tag_name: str
    draft: bool
    prerelease: bool
    immutable: bool
    assets: tuple[ReleaseAsset, ...]
    upload_url: str | None = None

    @classmethod
    def from_json(cls, value: dict[str, Any]) -> "Release":
        if not isinstance(value, dict):
            raise ApiError(f"GitHub returned a non-object release: {value!r}")
        release_id = value.get("id")
        tag_name = value.get("tag_name")
        if not isinstance(release_id, int) or isinstance(release_id, bool):
            raise ApiError(f"GitHub returned an invalid release id for {value!r}")
        if not isinstance(tag_name, str):
            raise ApiError(f"GitHub returned an invalid release tag for {value!r}")
        assets_value = value.get("assets", [])
        if not isinstance(assets_value, list):
            raise ApiError(f"GitHub returned an invalid release asset list for {tag_name}")
        for asset in assets_value:
            if not isinstance(asset, dict):
                raise ApiError(f"GitHub returned a non-object asset for {tag_name}")

        def bool_field(name: str) -> bool:
            # Treat malformed API data as an error instead of relying on
            # Python's truthiness (for example, bool("false") is True).
            value_for_field = value.get(name)
            if not isinstance(value_for_field, bool):
                raise ApiError(
                    f"GitHub returned an invalid {name} flag for release {tag_name}"
                )
            return value_for_field

        return cls(
            id=release_id,
            tag_name=tag_name,
            draft=bool_field("draft"),
            prerelease=bool_field("prerelease"),
            immutable=bool_field("immutable"),
            assets=tuple(ReleaseAsset.from_json(item) for item in assets_value),
            upload_url=(
                value.get("upload_url")
                if isinstance(value.get("upload_url"), str)
                else None
            ),
        )


@dataclass(frozen=True)
class TagRef:
    peeled_commit: str
    annotated: bool


@dataclass(frozen=True)
class TagRuleset:
    """The subset of a GitHub tag ruleset relevant to release immutability."""

    target: str
    enforcement: str
    includes: tuple[str, ...]
    excludes: tuple[str, ...]
    restrictions: frozenset[str]
    # ``None`` means the API did not disclose this field.  GitHub omits
    # bypass actors for callers without sufficient administration access; an
    # omitted field must not be treated as proof that no bypass exists.
    bypass_actors: tuple[Any, ...] | None = ()

    @classmethod
    def from_json(cls, value: dict[str, Any]) -> "TagRuleset":
        conditions = value.get("conditions", {})
        if not isinstance(conditions, dict):
            conditions = {}
        refs = conditions.get("ref_name", {})
        if not isinstance(refs, dict):
            refs = {}
        include_value = refs.get("include", [])
        exclude_value = refs.get("exclude", [])
        includes = tuple(
            item for item in include_value if isinstance(item, str)
        ) if isinstance(include_value, list) else ()
        excludes = tuple(
            item for item in exclude_value if isinstance(item, str)
        ) if isinstance(exclude_value, list) else ()
        rules_value = value.get("rules", [])
        if not isinstance(rules_value, list):
            rules_value = []
        restrictions = frozenset(
            item.get("type")
            for item in rules_value
            if isinstance(item, dict) and isinstance(item.get("type"), str)
        )
        bypass_value = value.get("bypass_actors")
        bypass = tuple(bypass_value) if isinstance(bypass_value, list) else None
        return cls(
            target=value.get("target") if isinstance(value.get("target"), str) else "",
            enforcement=(
                value.get("enforcement")
                if isinstance(value.get("enforcement"), str)
                else ""
            ),
            includes=includes,
            excludes=excludes,
            restrictions=restrictions,
            bypass_actors=bypass,
        )

    @staticmethod
    def _matches(pattern: str, tag: str) -> bool:
        normalized = pattern
        if normalized.startswith("refs/tags/"):
            normalized = normalized.removeprefix("refs/tags/")
        if normalized in ("*", "~ALL"):
            normalized = "*"
        return fnmatch.fnmatchcase(tag, normalized)

    def protects(self, tag: str) -> bool:
        if self.target != "tag" or self.enforcement != "active":
            return False
        if not any(self._matches(pattern, tag) for pattern in self.includes):
            return False
        if any(self._matches(pattern, tag) for pattern in self.excludes):
            return False
        if not {"update", "deletion"} <= self.restrictions:
            return False
        # GitHub omits bypass actors unless the caller can write the ruleset.
        # Absence is therefore not proof that no actor can race the final tag
        # check; accept only an explicitly disclosed empty list.
        return self.bypass_actors == ()


class ReleaseApi(Protocol):
    """The minimal API required by :class:`ReleasePublisher`."""

    def immutable_releases_enabled(self) -> bool: ...

    def tag_rulesets(self) -> Sequence[TagRuleset]: ...

    def tag_ref(self, tag: str) -> TagRef: ...

    def get_release(self, tag: str) -> Release | None: ...

    def list_releases(self) -> Sequence[Release]: ...

    def create_draft(self, tag: str, title: str) -> Release: ...

    def upload_asset(self, release: Release, asset: ExpectedAsset) -> ReleaseAsset: ...

    def publish_release(self, release: Release, *, make_latest: bool) -> Release: ...


def _validate_asset_name(name: str) -> None:
    path = Path(name)
    if (
        not name
        or "\x00" in name
        or path.name != name
        or name in {".", ".."}
        or any(part in {"", ".", ".."} for part in path.parts)
        or any(character.isspace() for character in name)
    ):
        raise ManifestError(f"release asset name must be one safe file name: {name!r}")


def _load_manifest_records(manifest: Path) -> list[tuple[str, str, int | None]]:
    try:
        data = manifest.read_bytes()
    except OSError as error:
        raise ManifestError(f"cannot read manifest {manifest}: {error}") from error
    try:
        decoded = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        decoded = None

    if decoded is not None:
        if isinstance(decoded, dict) and isinstance(decoded.get("assets"), list):
            entries = decoded["assets"]
        elif isinstance(decoded, dict):
            entries = [
                {"name": name, **(value if isinstance(value, dict) else {})}
                for name, value in decoded.items()
            ]
        else:
            raise ManifestError("JSON manifest must contain an assets array or object")
        records: list[tuple[str, str, int | None]] = []
        for entry in entries:
            if not isinstance(entry, dict):
                raise ManifestError("JSON manifest asset entries must be objects")
            name = entry.get("name")
            digest = entry.get("sha256", entry.get("digest"))
            size = entry.get("size")
            if isinstance(digest, str) and digest.startswith("sha256:"):
                digest = digest.removeprefix("sha256:")
            if not isinstance(name, str) or not isinstance(digest, str):
                raise ManifestError("JSON manifest entries require name and sha256")
            if size is not None and (not isinstance(size, int) or isinstance(size, bool)):
                raise ManifestError(f"manifest size must be an integer: {name}")
            records.append((name, digest.lower(), size))
        return records

    records = []
    try:
        lines = data.decode("ascii").splitlines()
    except UnicodeDecodeError as error:
        raise ManifestError(f"manifest is neither JSON nor ASCII checksums: {manifest}") from error
    for line_number, line in enumerate(lines, 1):
        if not line.strip() or line.startswith("#"):
            continue
        pieces = line.split(maxsplit=1)
        if len(pieces) != 2:
            raise ManifestError(f"invalid checksum manifest line {line_number}")
        digest, name = pieces
        name = name.removeprefix("*")
        records.append((name, digest.lower(), None))
    return records


def _load_manifest_controls(
    manifest: Path,
    release_dir: Path,
) -> dict[str, tuple[str | None, int | None]]:
    """Return explicitly named control/evidence files in a JSON manifest.

    ``tools/release_manifest.py`` intentionally cannot hash its own
    ``RELEASE-MANIFEST.json`` and keeps ``SHA256SUMS``/SLSA evidence outside
    the subject records.  Those files are nevertheless release assets.  The
    publisher therefore accepts only control names explicitly present in the
    manifest and verifies any digest/size that the manifest supplies.  A
    checksum-format manifest has no control metadata and keeps its historical
    subject-only behavior.
    """

    try:
        decoded = json.loads(manifest.read_bytes().decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        decoded = None
    if not isinstance(decoded, dict):
        decoded = {}

    controls: dict[str, tuple[str | None, int | None]] = {}
    # The manifest itself is a control only when it lives in the published
    # directory.  A workflow may keep it outside the directory and pass it as
    # a pure allowlist file.
    try:
        if manifest.resolve().parent == release_dir.resolve():
            controls[manifest.name] = (None, None)
    except (OSError, RuntimeError):
        # The caller already checked both paths; a failed comparison must not
        # turn an external manifest into a release asset.
        pass

    def add_control(value: Any, *, field: str) -> None:
        if isinstance(value, str):
            name = value
            digest = None
            size = None
        elif isinstance(value, dict):
            name = value.get("name")
            digest = value.get("sha256", value.get("digest"))
            size = value.get("size")
            if isinstance(digest, str) and digest.startswith("sha256:"):
                digest = digest.removeprefix("sha256:")
            if digest is not None and not isinstance(digest, str):
                raise ManifestError(f"manifest control digest must be a string: {field}")
            if size is not None and (not isinstance(size, int) or isinstance(size, bool)):
                raise ManifestError(f"manifest control size must be an integer: {field}")
        else:
            raise ManifestError(f"manifest control must be a name or object: {field}")
        if not isinstance(name, str):
            raise ManifestError(f"manifest control name must be a string: {field}")
        _validate_asset_name(name)
        normalized_digest = digest.lower() if isinstance(digest, str) else None
        if name in controls and controls[name] != (normalized_digest, size):
            raise ManifestError(f"manifest control is declared inconsistently: {name}")
        controls[name] = (normalized_digest, size)

    checksums = decoded.get("checksums")
    if isinstance(checksums, dict):
        add_control(checksums.get("name"), field="checksums.name")
    elif checksums is not None:
        raise ManifestError("manifest checksums metadata must be an object")

    provenance = decoded.get("provenance")
    if isinstance(provenance, dict):
        add_control(provenance, field="provenance")
    elif provenance is not None:
        raise ManifestError("manifest provenance metadata must be an object")

    explicit_controls = decoded.get("control_files")
    if explicit_controls is not None:
        if not isinstance(explicit_controls, list):
            raise ManifestError("manifest control_files must be a list")
        for index, control in enumerate(explicit_controls):
            add_control(control, field=f"control_files[{index}]")
    return controls


def load_manifest(release_dir: Path, manifest: Path) -> tuple[ExpectedAsset, ...]:
    """Load and fully validate an explicit JSON or SHA256SUMS allowlist."""

    try:
        root_stat = release_dir.lstat()
    except OSError as error:
        raise ManifestError(f"release directory is unavailable: {release_dir}: {error}") from error
    if stat.S_ISLNK(root_stat.st_mode) or not stat.S_ISDIR(root_stat.st_mode):
        raise ManifestError(f"release directory must be a real directory: {release_dir}")
    try:
        manifest_stat = manifest.lstat()
    except OSError as error:
        raise ManifestError(f"manifest is unavailable: {manifest}: {error}") from error
    if stat.S_ISLNK(manifest_stat.st_mode) or not stat.S_ISREG(manifest_stat.st_mode):
        raise ManifestError(f"manifest must be a real file: {manifest}")
    records = _load_manifest_records(manifest)
    if not records:
        raise ManifestError("release manifest contains no assets")
    controls = _load_manifest_controls(manifest, release_dir)

    names: set[str] = set()
    assets: list[ExpectedAsset] = []
    root = release_dir.resolve()
    for name, digest, declared_size in records:
        _validate_asset_name(name)
        if name in names:
            raise ManifestError(f"duplicate asset in manifest: {name}")
        names.add(name)
        if SHA256.fullmatch(digest) is None:
            raise ManifestError(f"invalid SHA-256 digest for {name}")
        path = release_dir / name
        try:
            resolved = path.resolve(strict=True)
        except (OSError, RuntimeError) as error:
            raise ManifestError(f"manifest asset is missing: {name}") from error
        if resolved.parent != root:
            raise ManifestError(f"manifest asset escapes release directory: {name}")
        try:
            metadata = path.lstat()
        except OSError as error:
            raise ManifestError(f"cannot stat manifest asset {name}: {error}") from error
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise ManifestError(f"manifest asset is not a regular file: {name}")
        size = metadata.st_size
        if size > MAX_ASSET_BYTES:
            raise ManifestError(
                f"asset exceeds the {MAX_ASSET_BYTES}-byte publication limit: {name}"
            )
        if declared_size is not None and declared_size != size:
            raise ManifestError(
                f"manifest size mismatch for {name}: expected {declared_size}, found {size}"
            )
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != digest:
            raise ManifestError(
                f"manifest digest mismatch for {name}: expected {digest}, found {actual}"
            )
        assets.append(ExpectedAsset(name=name, path=path, size=size, sha256=digest))

    # Add only the explicitly named control/evidence files that are present in
    # the release directory.  A missing control declaration is an error; an
    # external manifest simply contributes no control file of its own.
    for name, (declared_digest, declared_size) in controls.items():
        if name in names:
            existing = next(asset for asset in assets if asset.name == name)
            if declared_size is not None and declared_size != existing.size:
                raise ManifestError(f"manifest control size mismatch for {name}")
            if declared_digest is not None and declared_digest != existing.sha256:
                raise ManifestError(f"manifest control digest mismatch for {name}")
            continue
        path = release_dir / name
        try:
            metadata = path.lstat()
        except OSError as error:
            raise ManifestError(f"manifest control is missing: {name}") from error
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise ManifestError(f"manifest control is not a regular file: {name}")
        data = path.read_bytes()
        size = len(data)
        if size > MAX_ASSET_BYTES:
            raise ManifestError(
                f"manifest control exceeds the {MAX_ASSET_BYTES}-byte publication limit: {name}"
            )
        digest = hashlib.sha256(data).hexdigest()
        if declared_size is not None and declared_size != size:
            raise ManifestError(
                f"manifest control size mismatch for {name}: "
                f"expected {declared_size}, found {size}"
            )
        if declared_digest is not None:
            if SHA256.fullmatch(declared_digest) is None:
                raise ManifestError(f"invalid SHA-256 digest for manifest control {name}")
            if declared_digest != digest:
                raise ManifestError(
                    f"manifest control digest mismatch for {name}: "
                    f"expected {declared_digest}, found {digest}"
                )
        names.add(name)
        assets.append(ExpectedAsset(name=name, path=path, size=size, sha256=digest))

    try:
        entries = list(release_dir.iterdir())
    except OSError as error:
        raise ManifestError(f"cannot enumerate release directory: {error}") from error
    for entry in entries:
        if entry.name not in names:
            raise ManifestError(f"release directory contains unlisted member: {entry.name}")
    return tuple(sorted(assets, key=lambda asset: asset.name))


def _version(tag: str) -> tuple[int, int, int] | None:
    match = RELEASE_TAG.fullmatch(tag)
    if match is None:
        return None
    return tuple(int(component) for component in match.group(1).split("."))  # type: ignore[return-value]


def decide_latest(candidate_tag: str, releases: Sequence[Release]) -> bool:
    """Decide latest from stable semantic versions before publication."""

    candidate = _version(candidate_tag)
    if candidate is None:
        raise PublicationError(f"release tag is not a stable vMAJOR.MINOR.PATCH tag: {candidate_tag}")
    versions = [
        version
        for release in releases
        if not release.draft
        and not release.prerelease
        and (version := _version(release.tag_name)) is not None
        and release.tag_name != candidate_tag
    ]
    return not versions or candidate > max(versions)


@dataclass(frozen=True)
class PublishResult:
    release: Release
    idempotent: bool
    make_latest: bool | None


class ReleasePublisher:
    """Fail-closed release state machine.

    The caller must run this object under a repository-wide publication mutex;
    latest selection is deterministic under that external serialization.
    """

    def __init__(
        self,
        api: ReleaseApi,
        *,
        tag: str,
        expected_commit: str,
        title: str,
        assets: Sequence[ExpectedAsset],
    ) -> None:
        self.api = api
        self.tag = tag
        self.expected_commit = expected_commit.lower()
        self.title = title
        self.assets = tuple(assets)
        self._asset_by_name = {asset.name: asset for asset in self.assets}
        if _version(tag) is None:
            raise PublicationError(f"invalid stable release tag: {tag}")
        if COMMIT_SHA.fullmatch(expected_commit) is None:
            raise PublicationError(f"invalid expected commit SHA: {expected_commit}")
        if not self.assets or len(self._asset_by_name) != len(self.assets):
            raise PublicationError("release assets must be a non-empty unique manifest")

    def _check_repository_guards(self) -> None:
        if not self.api.immutable_releases_enabled():
            raise PublicationError("immutable GitHub releases are not enabled")
        rulesets = self.api.tag_rulesets()
        if not any(ruleset.protects(self.tag) for ruleset in rulesets):
            raise PublicationError(
                f"no active tag ruleset prevents update and deletion of {self.tag}"
            )

    def _check_tag(self) -> TagRef:
        reference = self.api.tag_ref(self.tag)
        if not reference.annotated:
            raise PublicationError(
                f"remote tag {self.tag} must be an annotated tag"
            )
        if reference.peeled_commit.lower() != self.expected_commit:
            raise PublicationError(
                f"remote tag {self.tag} resolves to {reference.peeled_commit}, "
                f"expected {self.expected_commit}"
            )
        return reference

    def _release_asset_map(self, release: Release) -> dict[str, ReleaseAsset]:
        result: dict[str, ReleaseAsset] = {}
        for asset in release.assets:
            if asset.name in result:
                raise PublicationError(f"release contains duplicate asset name: {asset.name}")
            result[asset.name] = asset
        return result

    def _validate_release_identity(self, release: Release) -> None:
        if release.tag_name != self.tag:
            raise PublicationError(
                f"GitHub returned release for {release.tag_name}, expected {self.tag}"
            )
        if release.prerelease:
            raise PublicationError("stable release cannot be a prerelease")

    def _validate_assets(self, release: Release, *, complete: bool) -> None:
        self._validate_release_identity(release)
        remote = self._release_asset_map(release)
        expected_names = set(self._asset_by_name)
        unexpected = sorted(set(remote) - expected_names)
        if unexpected:
            raise PublicationError(
                "release contains unlisted assets: " + ", ".join(unexpected)
            )
        if complete and set(remote) != expected_names:
            missing = sorted(expected_names - set(remote))
            raise PublicationError("release is missing assets: " + ", ".join(missing))
        for name, remote_asset in remote.items():
            expected = self._asset_by_name[name]
            if remote_asset.state != "uploaded":
                raise PublicationError(f"release asset is not uploaded: {name}")
            if remote_asset.size != expected.size:
                raise PublicationError(
                    f"release asset size mismatch for {name}: "
                    f"expected {expected.size}, found {remote_asset.size}"
                )
            if remote_asset.sha256 != expected.sha256:
                raise PublicationError(
                    f"release asset digest mismatch for {name}: "
                    f"expected {expected.sha256}, found {remote_asset.sha256}"
                )

    def _refresh(self) -> Release:
        release = self.api.get_release(self.tag)
        if release is None:
            raise PublicationError(f"release {self.tag} disappeared during publication")
        return release

    def _create_or_reuse_draft(self) -> tuple[Release, bool]:
        existing = self.api.get_release(self.tag)
        if existing is not None:
            self._validate_release_identity(existing)
            if not existing.draft:
                return existing, False
            self._validate_assets(existing, complete=False)
            return existing, True
        try:
            draft = self.api.create_draft(self.tag, self.title)
        except ApiError as error:
            # A concurrent publisher or a retried request may have created the
            # draft.  Reconcile by reading; never create a second release.
            if error.timeout:
                draft = self._refresh()
            else:
                draft = self.api.get_release(self.tag)
                if draft is None:
                    raise
        self._validate_release_identity(draft)
        if not draft.draft:
            # A request can race another publisher and return a published
            # release (or a proxy can lose the response after publication).
            # Reconcile as an idempotent result; never PATCH it again.
            return draft, False
        self._validate_assets(draft, complete=False)
        return draft, True

    def _upload_missing(self, draft: Release) -> Release:
        remote = self._release_asset_map(draft)
        for asset in self.assets:
            existing = remote.get(asset.name)
            if existing is not None:
                continue
            asset.verify_local()
            try:
                uploaded = self.api.upload_asset(draft, asset)
            except ApiError as error:
                # A request may have succeeded before a transport failure.  A
                # read-only refresh is the only safe recovery here.
                refreshed = self._refresh()
                refreshed_map = self._release_asset_map(refreshed)
                recovered = refreshed_map.get(asset.name)
                if recovered is not None:
                    remote[asset.name] = recovered
                    continue
                if error.timeout:
                    raise PublicationError(
                        f"asset upload timed out and {asset.name} is absent; rerun to reconcile"
                    ) from error
                raise
            if uploaded.name != asset.name:
                raise PublicationError(
                    f"GitHub uploaded unexpected asset {uploaded.name} for {asset.name}"
                )
            remote[asset.name] = uploaded
        refreshed = self._refresh()
        self._validate_assets(refreshed, complete=True)
        return refreshed

    def _validate_published(self, release: Release) -> None:
        if release.draft:
            raise PublicationError("release is still a draft")
        if not release.immutable:
            raise PublicationError("published release is not immutable")
        self._validate_assets(release, complete=True)

    def publish(self) -> PublishResult:
        self._check_repository_guards()
        self._check_tag()
        release, is_draft = self._create_or_reuse_draft()
        if not is_draft:
            self._check_tag()
            self._validate_published(release)
            return PublishResult(release=release, idempotent=True, make_latest=None)

        # Every local file is re-read before a mutating upload and once more
        # before the irreversible draft=false transition.
        release = self._upload_missing(release)
        if not release.draft:
            self._check_tag()
            self._validate_published(release)
            return PublishResult(release=release, idempotent=True, make_latest=None)
        for asset in self.assets:
            asset.verify_local()
        make_latest = decide_latest(self.tag, self.api.list_releases())
        # Repository policy and the tag are mutable administrative state.  A
        # preflight check alone is not sufficient: re-read both immediately
        # before the one-way draft=false transition.
        self._check_repository_guards()
        self._check_tag()
        try:
            published = self.api.publish_release(release, make_latest=make_latest)
        except ApiError as error:
            # Any failed PATCH may have reached GitHub before the response
            # failed (including a 5xx).  Reconcile by reading; never retry a
            # possibly immutable transition.
            try:
                reconciled = self._refresh()
            except PublicationError:
                raise error
            if not reconciled.draft:
                self._check_tag()
                self._validate_published(reconciled)
                return PublishResult(
                    release=reconciled,
                    idempotent=True,
                    make_latest=make_latest,
                )
            if error.timeout:
                raise PublicationError(
                    "publish timed out and the release remains a draft; rerun to reconcile"
                ) from error
            raise PublicationError(
                "publish failed and the release remains a draft; rerun to reconcile"
            ) from error
        # Confirm that the immutable transition still corresponds to the
        # expected peeled tag.  The pre-PATCH check is the authorization gate;
        # this postcondition catches an administrative race or a stale API
        # response before reporting success.
        self._check_tag()
        self._validate_published(published)
        return PublishResult(
            release=published,
            idempotent=False,
            make_latest=make_latest,
        )


class GitHubApi:
    """Small stdlib-only REST adapter used by the command-line entry point."""

    def __init__(
        self,
        repo: str,
        token: str,
        *,
        policy_token: str,
        api_base_url: str = "https://api.github.com",
        timeout: float = 30.0,
    ) -> None:
        if REPOSITORY.fullmatch(repo) is None:
            raise ValueError(f"repository must be OWNER/REPOSITORY: {repo}")
        if not token:
            raise ValueError("a GitHub token is required")
        if not policy_token:
            raise ValueError("a separate administration policy token is required")
        parsed_base = urlsplit(api_base_url)
        if (
            parsed_base.scheme != "https"
            or not parsed_base.netloc
            or parsed_base.username is not None
            or parsed_base.password is not None
        ):
            raise ValueError("GitHub API base URL must use HTTPS")
        self.repo = repo
        self.token = token
        self.policy_token = policy_token
        self.api_base_url = api_base_url.rstrip("/") + "/"
        self.api_netloc = parsed_base.netloc.lower()
        self.upload_netlocs = {self.api_netloc}
        if self.api_netloc == "api.github.com":
            self.upload_netlocs.add("uploads.github.com")
        self.timeout = timeout

    def _request(
        self,
        method: str,
        path: str,
        *,
        payload: dict[str, Any] | None = None,
        body: bytes | None = None,
        content_type: str = "application/json",
        policy: bool = False,
        allow_upload_host: bool = False,
    ) -> Any:
        url = urljoin(self.api_base_url, path.lstrip("/"))
        parsed_url = urlsplit(url)
        allowed_netlocs = self.upload_netlocs if allow_upload_host else {self.api_netloc}
        if (
            parsed_url.scheme != "https"
            or parsed_url.netloc.lower() not in allowed_netlocs
            or parsed_url.username is not None
            or parsed_url.password is not None
            or parsed_url.fragment
        ):
            raise ApiError(f"GitHub API request refused an unexpected origin: {url}")
        if payload is not None:
            body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        headers = {
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {self.policy_token if policy else self.token}",
            "X-GitHub-Api-Version": "2026-03-10",
        }
        if body is not None:
            headers["Content-Type"] = content_type
            headers["Content-Length"] = str(len(body))
        request = Request(url, data=body, headers=headers, method=method)
        try:
            with _NO_REDIRECT_OPENER.open(request, timeout=self.timeout) as response:
                response_body = response.read()
        except HTTPError as error:
            try:
                detail = error.read().decode("utf-8", errors="replace")
            except OSError:
                detail = ""
            raise ApiError(
                f"GitHub API {method} {path} failed with HTTP {error.code}: {detail[:500]}",
                status=error.code,
            ) from error
        except (TimeoutError, socket.timeout) as error:
            raise ApiTimeout(f"GitHub API {method} {path} timed out") from error
        except URLError as error:
            reason = error.reason
            if isinstance(reason, (TimeoutError, socket.timeout)):
                raise ApiTimeout(f"GitHub API {method} {path} timed out") from error
            raise ApiError(f"GitHub API {method} {path} failed: {reason}") from error
        if not response_body:
            return None
        try:
            return json.loads(response_body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ApiError(f"GitHub API returned invalid JSON for {method} {path}") from error

    def immutable_releases_enabled(self) -> bool:
        path = f"/repos/{self.repo}/immutable-releases"
        try:
            result = self._request("GET", path, policy=True)
        except ApiError as error:
            if error.status == 404:
                return False
            raise
        return isinstance(result, dict) and result.get("enabled") is True

    def tag_rulesets(self) -> Sequence[TagRuleset]:
        # The list endpoint intentionally returns summary objects and omits
        # conditions and rules. Resolve every page and then every detail using
        # the policy token. GitHub discloses bypass actors only to a caller
        # with ruleset write access, and TagRuleset.protects fails closed when
        # that security-relevant field is absent.
        rulesets: list[TagRuleset] = []
        page = 1
        while True:
            path = f"/repos/{self.repo}/rulesets?{urlencode({'includes_parents': 'true', 'targets': 'tag', 'per_page': 100, 'page': page})}"
            summaries = self._request("GET", path, policy=True)
            if not isinstance(summaries, list):
                raise ApiError("GitHub returned an invalid ruleset list")
            if any(not isinstance(item, dict) for item in summaries):
                raise ApiError("GitHub returned a non-object ruleset")
            for summary in summaries:
                ruleset_id = summary.get("id")
                if not isinstance(ruleset_id, int) or isinstance(ruleset_id, bool):
                    raise ApiError("GitHub returned a ruleset without an integer id")
                detail = self._request(
                    "GET",
                    f"/repos/{self.repo}/rulesets/{ruleset_id}?{urlencode({'includes_parents': 'true'})}",
                    policy=True,
                )
                if not isinstance(detail, dict):
                    raise ApiError(
                        f"GitHub returned an invalid ruleset detail for {ruleset_id}"
                    )
                rulesets.append(TagRuleset.from_json(detail))
            if len(summaries) < 100:
                return tuple(rulesets)
            page += 1

    def tag_ref(self, tag: str) -> TagRef:
        encoded = quote(tag, safe="")
        ref = self._request("GET", f"/repos/{self.repo}/git/ref/tags/{encoded}")
        if not isinstance(ref, dict) or not isinstance(ref.get("object"), dict):
            raise ApiError(f"GitHub returned an invalid tag reference for {tag}")
        object_value = ref["object"]
        object_type = object_value.get("type")
        object_sha = object_value.get("sha")
        if not isinstance(object_sha, str):
            raise ApiError(f"GitHub returned an invalid tag object for {tag}")
        if object_type == "commit":
            commit = object_sha
            annotated = False
        elif object_type == "tag":
            tag_object = self._request("GET", f"/repos/{self.repo}/git/tags/{object_sha}")
            if not isinstance(tag_object, dict) or not isinstance(tag_object.get("object"), dict):
                raise ApiError(f"GitHub returned an invalid annotated tag object for {tag}")
            commit = tag_object["object"].get("sha")
            annotated = True
        else:
            raise ApiError(f"GitHub tag {tag} has unsupported object type {object_type!r}")
        if not isinstance(commit, str) or COMMIT_SHA.fullmatch(commit) is None:
            raise ApiError(f"GitHub tag {tag} does not peel to a commit")
        return TagRef(peeled_commit=commit, annotated=annotated)

    def get_release(self, tag: str) -> Release | None:
        encoded = quote(tag, safe="")
        path = f"/repos/{self.repo}/releases/tags/{encoded}"
        try:
            value = self._request("GET", path)
        except ApiError as error:
            if error.status == 404:
                return None
            raise
        if not isinstance(value, dict):
            raise ApiError(f"GitHub returned an invalid release for {tag}")
        return Release.from_json(value)

    def list_releases(self) -> Sequence[Release]:
        releases: list[Release] = []
        page = 1
        while True:
            path = f"/repos/{self.repo}/releases?{urlencode({'per_page': 100, 'page': page})}"
            value = self._request("GET", path)
            if not isinstance(value, list):
                raise ApiError("GitHub returned an invalid release list")
            if any(not isinstance(item, dict) for item in value):
                raise ApiError("GitHub returned a non-object release")
            releases.extend(Release.from_json(item) for item in value)
            if len(value) < 100:
                return tuple(releases)
            page += 1

    def create_draft(self, tag: str, title: str) -> Release:
        value = self._request(
            "POST",
            f"/repos/{self.repo}/releases",
            payload={
                "tag_name": tag,
                "name": title,
                "draft": True,
                "prerelease": False,
                "make_latest": "false",
                "generate_release_notes": True,
            },
        )
        if not isinstance(value, dict):
            raise ApiError("GitHub returned an invalid draft release")
        return Release.from_json(value)

    def upload_asset(self, release: Release, asset: ExpectedAsset) -> ReleaseAsset:
        if not release.upload_url:
            raise ApiError(f"release {release.tag_name} has no upload URL")
        upload_base = release.upload_url.split("{", 1)[0]
        parsed_upload = urlsplit(upload_base)
        expected_prefix = f"/repos/{self.repo}/releases/"
        if (
            parsed_upload.scheme != "https"
            or parsed_upload.netloc.lower() not in self.upload_netlocs
            or parsed_upload.username is not None
            or parsed_upload.password is not None
            or parsed_upload.fragment
            or not parsed_upload.path.startswith(expected_prefix)
            or not parsed_upload.path.endswith("/assets")
        ):
            raise ApiError("GitHub returned an unexpected release upload URL")
        body = asset.verify_local()
        separator = "&" if "?" in upload_base else "?"
        path = upload_base + separator + urlencode({"name": asset.name})
        value = self._request(
            "POST",
            path,
            body=body,
            content_type="application/octet-stream",
            allow_upload_host=True,
        )
        if not isinstance(value, dict):
            raise ApiError(f"GitHub returned an invalid uploaded asset for {asset.name}")
        return ReleaseAsset.from_json(value)

    def publish_release(self, release: Release, *, make_latest: bool) -> Release:
        value = self._request(
            "PATCH",
            f"/repos/{self.repo}/releases/{release.id}",
            payload={"draft": False, "make_latest": "true" if make_latest else "false"},
        )
        if not isinstance(value, dict):
            raise ApiError("GitHub returned an invalid published release")
        return Release.from_json(value)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="GitHub OWNER/REPOSITORY")
    parser.add_argument("--tag", required=True, help="stable release tag, e.g. v1.2.3")
    parser.add_argument("--expected-commit", required=True, help="peeled tag commit SHA")
    parser.add_argument("--release-dir", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--title", required=True)
    parser.add_argument("--api-base-url", default="https://api.github.com")
    parser.add_argument("--timeout", type=float, default=30.0)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    token = os.environ.get("GITHUB_TOKEN", "")
    policy_token = os.environ.get("GITHUB_POLICY_TOKEN", "")
    try:
        assets = load_manifest(args.release_dir, args.manifest)
        api = GitHubApi(
            args.repo,
            token,
            policy_token=policy_token,
            api_base_url=args.api_base_url,
            timeout=args.timeout,
        )
        result = ReleasePublisher(
            api,
            tag=args.tag,
            expected_commit=args.expected_commit,
            title=args.title,
            assets=assets,
        ).publish()
    except (ManifestError, PublicationError, ValueError) as error:
        print(f"release publication refused: {error}", file=sys.stderr)
        return 1
    print(
        json.dumps(
            {
                "tag": result.release.tag_name,
                "release_id": result.release.id,
                "idempotent": result.idempotent,
                "make_latest": result.make_latest,
                "immutable": result.release.immutable,
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
