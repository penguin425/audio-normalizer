#!/usr/bin/env python3
"""Check the complete, offline Forge JSON contract registry."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Iterator


REPO_ROOT = Path(__file__).resolve().parents[1]
SCHEMA_ROOT = REPO_ROOT / "schema"
REGISTRY_PATH = SCHEMA_ROOT / "schema-registry-v1.json"
PAGES_SCHEMA_BASE = "https://penguin425.github.io/audio-normalizer/schema/"
GITHUB_SCHEMA_BASE = "https://github.com/penguin425/audio-normalizer/schema/"
REGISTRY_ID = PAGES_SCHEMA_BASE + "schema-registry-v1.json"
REGISTRY_SCHEMA_ID = PAGES_SCHEMA_BASE + "schema-registry-v1"
DIALECT_ID = "https://json-schema.org/draft/2020-12/schema"
VERSION_RE = re.compile(r"-v([0-9]+)(?:[-.]|$)")
LEGACY_DOCUMENT_IDS = {
    "schema/audio-comparison-v1.schema.json": PAGES_SCHEMA_BASE
    + "audio-comparison-v1.schema.json",
    "schema/binaural-qc-report-v1.schema.json": GITHUB_SCHEMA_BASE
    + "binaural-qc-report-v1.schema.json",
    "schema/binaural-qc-request-v1.schema.json": GITHUB_SCHEMA_BASE
    + "binaural-qc-request-v1.schema.json",
    "schema/doctor-report-v1.schema.json": GITHUB_SCHEMA_BASE
    + "doctor-report-v1.schema.json",
    "schema/downmix-qc-report-v1.schema.json": GITHUB_SCHEMA_BASE
    + "downmix-qc-report-v1.schema.json",
    "schema/downmix-qc-request-v1.schema.json": GITHUB_SCHEMA_BASE
    + "downmix-qc-request-v1.schema.json",
    "schema/metadata-repair-report-v1.schema.json": PAGES_SCHEMA_BASE
    + "metadata-repair-report-v1.schema.json",
    "schema/metadata-repair-report-v2.schema.json": PAGES_SCHEMA_BASE
    + "metadata-repair-report-v2.schema.json",
    "schema/metadata-repair-request-v1.schema.json": PAGES_SCHEMA_BASE
    + "metadata-repair-request-v1.schema.json",
    "schema/metadata-repair-request-v2.schema.json": PAGES_SCHEMA_BASE
    + "metadata-repair-request-v2.schema.json",
    "schema/multi-delivery-report-v1.schema.json": PAGES_SCHEMA_BASE
    + "multi-delivery-report-v1.schema.json",
    "schema/multi-delivery-request-v1.schema.json": PAGES_SCHEMA_BASE
    + "multi-delivery-request-v1.schema.json",
    "schema/normalization-difference-v1.schema.json": PAGES_SCHEMA_BASE
    + "normalization-difference-v1.schema.json",
    "schema/remediation-report-v1.schema.json": PAGES_SCHEMA_BASE
    + "remediation-report-v1.schema.json",
    "schema/remediation-request-v1.schema.json": PAGES_SCHEMA_BASE
    + "remediation-request-v1.schema.json",
}


def fail(message: str) -> None:
    raise SystemExit(f"schema registry error: {message}")


def load_object(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot read {path.relative_to(REPO_ROOT)}: {error}")
    if not isinstance(value, dict):
        fail(f"{path.relative_to(REPO_ROOT)} must contain a JSON object")
    return value


def iter_refs(value: Any, location: str = "$") -> Iterator[tuple[str, str]]:
    if isinstance(value, dict):
        for key, child in value.items():
            child_location = f"{location}/{key}"
            if key == "$ref":
                if not isinstance(child, str):
                    fail(f"{child_location} must be a string")
                yield child_location, child
            yield from iter_refs(child, child_location)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            yield from iter_refs(child, f"{location}/{index}")


def repo_file(reference: str, field: str, entry_path: str) -> Path:
    if not reference.startswith("repo:"):
        fail(f"{entry_path} {field} entry is not a repository reference: {reference}")
    relative = reference.removeprefix("repo:")
    candidate = (REPO_ROOT / relative).resolve()
    try:
        candidate.relative_to(REPO_ROOT)
    except ValueError:
        fail(f"{entry_path} {field} escapes the repository: {reference}")
    if not candidate.is_file():
        fail(f"{entry_path} {field} does not name a file: {reference}")
    return candidate


def check_owners(entry: dict[str, Any], field: str, entry_path: str) -> None:
    values = entry.get(field)
    if not isinstance(values, list) or not values:
        fail(f"{entry_path} must declare at least one {field} entry")
    if len(values) != len(set(values)):
        fail(f"{entry_path} contains duplicate {field} entries")
    for reference in values:
        if not isinstance(reference, str):
            fail(f"{entry_path} {field} entries must be strings")
        if reference.startswith("repo:"):
            repo_file(reference, field, entry_path)
        elif not reference.startswith("external:"):
            fail(f"{entry_path} {field} has an unknown owner kind: {reference}")


def check_version(entry: dict[str, Any]) -> None:
    path = entry["path"]
    path_versions = VERSION_RE.findall(Path(path).name)
    id_versions = VERSION_RE.findall(entry["document_id"].rsplit("/", 1)[-1])
    if not path_versions:
        fail(f"{path} has no explicit vN version")
    if not id_versions or path_versions[-1] != id_versions[-1]:
        fail(
            f"{path} version does not match document ID {entry['document_id']}"
        )
    if entry.get("version") != int(path_versions[-1]):
        fail(f"{path} version field does not match its filename")

    basename = Path(path).name.removesuffix(".schema.json").removesuffix(".json")
    expected_family = VERSION_RE.sub("-", basename, count=1).strip("-")
    if entry.get("family") != expected_family:
        fail(
            f"{path} family {entry.get('family')!r} must be {expected_family!r}"
        )


def expected_id_policy(entry: dict[str, Any]) -> str:
    filename = Path(entry["path"]).name
    if entry["document_kind"] == "json-schema":
        endpoint = filename.removesuffix(".schema.json")
    else:
        endpoint = filename
    expected = f"{PAGES_SCHEMA_BASE}{endpoint}"
    legacy = LEGACY_DOCUMENT_IDS.get(entry["path"])
    if legacy is not None:
        if entry["document_id"] != legacy:
            fail(f"{entry['path']} changes its published legacy document ID")
        return "legacy-preserved"
    if entry["document_id"] == expected:
        return "canonical"
    fail(f"{entry['path']} introduces an unapproved non-canonical document ID")


def expected_discriminator(document: dict[str, Any], path: str) -> dict[str, Any]:
    properties = document.get("properties")
    if not isinstance(properties, dict):
        fail(f"{path} has no root properties object for an instance discriminator")
    for field in ("schema", "schema_version", "version", "$schema"):
        candidate = properties.get(field)
        if not isinstance(candidate, dict):
            continue
        if "const" in candidate:
            return {"field": field, "value": candidate["const"]}
        enum = candidate.get("enum")
        if isinstance(enum, list) and len(enum) == 1:
            return {"field": field, "value": enum[0]}
    fail(f"{path} must expose a constant top-level instance discriminator")


def resolve_local_ref(document: Any, reference: str, path: str, location: str) -> None:
    if reference == "#":
        return
    if not reference.startswith("#/"):
        fail(f"{path} has a non-local JSON Pointer $ref at {location}: {reference}")
    current = document
    for encoded_token in reference[2:].split("/"):
        if re.search(r"~(?![01])", encoded_token):
            fail(f"{path} has an invalid JSON Pointer escape at {location}: {reference}")
        token = encoded_token.replace("~1", "/").replace("~0", "~")
        if isinstance(current, dict) and token in current:
            current = current[token]
        elif (
            isinstance(current, list)
            and token.isdecimal()
            and int(token) < len(current)
        ):
            current = current[int(token)]
        else:
            fail(f"{path} has an unresolved local $ref at {location}: {reference}")


def main() -> None:
    registry = load_object(REGISTRY_PATH)
    if registry.get("$schema") != REGISTRY_SCHEMA_ID:
        fail(f"registry $schema must be {REGISTRY_SCHEMA_ID}")
    if registry.get("registry_id") != REGISTRY_ID:
        fail(f"registry_id must be {REGISTRY_ID}")
    if registry.get("registry_version") != 1:
        fail("registry_version must be 1")

    entries = registry.get("entries")
    if not isinstance(entries, list) or not entries:
        fail("entries must be a non-empty array")

    paths: list[str] = []
    ids: list[str] = []
    by_id: dict[str, dict[str, Any]] = {}
    by_path: dict[str, dict[str, Any]] = {}
    documents: dict[str, dict[str, Any]] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            fail("every registry entry must be an object")
        path = entry.get("path")
        document_id = entry.get("document_id")
        if not isinstance(path, str) or not path.startswith("schema/"):
            fail(f"invalid schema path: {path!r}")
        if not isinstance(document_id, str) or not document_id.startswith("https://"):
            fail(f"{path} has an invalid document_id")
        paths.append(path)
        ids.append(document_id)
        by_id[document_id] = entry
        by_path[path] = entry

        candidate = (REPO_ROOT / path).resolve()
        try:
            candidate.relative_to(SCHEMA_ROOT)
        except ValueError:
            fail(f"registered path escapes schema/: {path}")
        if candidate.parent != SCHEMA_ROOT or not candidate.is_file():
            fail(f"registered JSON document is missing or nested: {path}")
        document = load_object(candidate)
        documents[path] = document

        kind = entry.get("document_kind")
        if kind == "json-schema":
            if not path.endswith(".schema.json"):
                fail(f"{path} is a JSON Schema but lacks the .schema.json suffix")
            if document.get("$schema") != DIALECT_ID:
                fail(f"{path} must declare JSON Schema draft 2020-12")
            if document.get("$id") != document_id:
                fail(
                    f"{path} $id {document.get('$id')!r} does not match "
                    f"registry ID {document_id!r}"
                )
            discriminator = expected_discriminator(document, path)
            if entry.get("instance_discriminator") != discriminator:
                fail(f"{path} has an incorrect instance_discriminator")
        elif kind == "registry":
            if path != "schema/schema-registry-v1.json":
                fail(f"unexpected registry document: {path}")
            if document.get("registry_id") != document_id:
                fail(f"{path} registry_id does not match its document ID")
        elif kind == "data":
            if path.endswith(".schema.json"):
                fail(f"{path} is marked data but uses the schema suffix")
        else:
            fail(f"{path} has unknown document_kind {kind!r}")

        check_version(entry)
        expected_policy = expected_id_policy(entry)
        if entry.get("id_policy") != expected_policy:
            fail(
                f"{path} id_policy must be {expected_policy} for {document_id}"
            )
        check_owners(entry, "producers", path)
        check_owners(entry, "consumers", path)
        validators = entry.get("validators")
        if not isinstance(validators, list) or not validators:
            fail(f"{path} must declare at least one validator")
        if len(validators) != len(set(validators)):
            fail(f"{path} contains duplicate validators")
        for validator in validators:
            if not isinstance(validator, str):
                fail(f"{path} validator entries must be strings")
            repo_file(validator, "validator", path)
        samples = entry.get("samples", [])
        if not isinstance(samples, list) or len(samples) != len(set(samples)):
            fail(f"{path} samples must be a unique array")
        for sample in samples:
            if not isinstance(sample, str):
                fail(f"{path} sample entries must be strings")
            repo_file(sample, "sample", path)

    if paths != sorted(paths):
        fail("entries must be sorted by path")
    if len(paths) != len(set(paths)):
        fail("registered paths must be unique")
    if len(ids) != len(set(ids)):
        fail("document IDs must be unique")

    actual_paths = {
        path.relative_to(REPO_ROOT).as_posix() for path in SCHEMA_ROOT.glob("*.json")
    }
    registered_paths = set(paths)
    if actual_paths != registered_paths:
        missing = sorted(actual_paths - registered_paths)
        stale = sorted(registered_paths - actual_paths)
        fail(f"registry coverage mismatch; unregistered={missing}, stale={stale}")
    missing_legacy = sorted(set(LEGACY_DOCUMENT_IDS) - registered_paths)
    if missing_legacy:
        fail(f"published legacy contracts are no longer registered: {missing_legacy}")

    for entry in entries:
        path = entry["path"]
        lifecycle = entry.get("lifecycle")
        successor_path = entry.get("successor_path")
        if lifecycle == "supported-legacy":
            if not isinstance(successor_path, str) or successor_path not in by_path:
                fail(f"{path} must name a registered successor")
            if successor_path == path:
                fail(f"{path} cannot supersede itself")
            successor = by_path[successor_path]
            if successor.get("family") != entry.get("family"):
                fail(f"{path} successor must belong to the same family")
            if successor.get("version", 0) <= entry.get("version", 0):
                fail(f"{path} successor version must increase")
        elif lifecycle == "current":
            if successor_path is not None:
                fail(f"{path} is current and must not declare a successor")
        elif lifecycle == "retired":
            if successor_path is not None:
                if not isinstance(successor_path, str) or successor_path not in by_path:
                    fail(f"{path} names an unregistered retired successor")
                successor = by_path[successor_path]
                if successor.get("family") != entry.get("family"):
                    fail(f"{path} retired successor must belong to the same family")
                if successor.get("version", 0) <= entry.get("version", 0):
                    fail(f"{path} retired successor version must increase")
        else:
            fail(f"{path} has unknown lifecycle {lifecycle!r}")

        policy = entry.get("change_policy")
        artifact_class = entry.get("artifact_class")
        if artifact_class == "cache" and policy != "invalidate":
            fail(f"{path} cache contracts must use the invalidate policy")
        if (
            artifact_class == "durable"
            and lifecycle == "supported-legacy"
            and policy != "migrate"
        ):
            fail(f"{path} supported durable contracts must use the migrate policy")
        if policy == "external-pin" and entry.get("document_kind") != "data":
            fail(f"{path} external-pin policy is only valid for data documents")
        if policy == "registry-bootstrap" and artifact_class != "registry":
            fail(f"{path} registry-bootstrap policy is only valid for registry artifacts")

    for entry in entries:
        if entry["document_kind"] != "json-schema":
            continue
        document = documents[entry["path"]]
        for location, reference in iter_refs(document):
            resolve_local_ref(document, reference, entry["path"], location)

    if by_id.get(REGISTRY_ID, {}).get("document_kind") != "registry":
        fail("the registry must register itself")
    if by_id.get(REGISTRY_SCHEMA_ID, {}).get("document_kind") != "json-schema":
        fail("the registry schema must be registered")

    schema_count = sum(
        entry["document_kind"] == "json-schema" for entry in entries
    )
    print(
        "schema registry ready: "
        f"{len(entries)} JSON documents, {schema_count} schemas, "
        f"{len(entries) - schema_count} governed data documents"
    )


if __name__ == "__main__":
    main()
