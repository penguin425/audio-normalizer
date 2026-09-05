#!/usr/bin/env python3
"""Reject mutable external dependencies in GitHub workflow definitions."""

from __future__ import annotations

import importlib.metadata
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

try:
    import yaml
    from yaml.nodes import MappingNode, Node, ScalarNode, SequenceNode
except ModuleNotFoundError:
    print(
        "workflow pin checking requires the dedicated PyYAML environment; "
        "create a CPython 3.12 venv and run: python -m pip install "
        "--require-hashes --only-binary=:all: "
        "-r tools/workflow-check-requirements.lock",
        file=sys.stderr,
    )
    raise SystemExit(2) from None

REQUIRED_PYYAML_VERSION = "6.0.3"
try:
    installed_pyyaml_version = importlib.metadata.version("PyYAML")
except importlib.metadata.PackageNotFoundError:
    installed_pyyaml_version = "not installed"
if installed_pyyaml_version != REQUIRED_PYYAML_VERSION:
    print(
        "workflow pin checking requires the dedicated PyYAML environment; "
        f"expected {REQUIRED_PYYAML_VERSION}, found {installed_pyyaml_version}. "
        "Install tools/workflow-check-requirements.lock with --require-hashes "
        "and --only-binary=:all:",
        file=sys.stderr,
    )
    raise SystemExit(2)


REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW_ROOTS = (
    REPOSITORY_ROOT / ".github" / "workflows",
    REPOSITORY_ROOT / ".github" / "actions",
)
COMMIT_SHA = re.compile(r"^[0-9a-fA-F]{40}$")
IMAGE_DIGEST = re.compile(r"^docker://.+@sha256:[0-9a-f]{64}$")
CONTAINER_IMAGE_DIGEST = re.compile(
    r"^[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}$"
)
RAW_CONTAINER_ENGINE = re.compile(
    r"(?<![-\w])(?:docker(?:-compose)?|podman(?:-compose)?|nerdctl)(?:\.exe)?"
    r"(?![-\w])",
    re.IGNORECASE,
)


@dataclass
class WorkflowScan:
    source: str
    checked_dependencies: int = 0
    checked_images: int = 0
    violations: list[str] = field(default_factory=list)

    def reject(self, node: Node, message: str) -> None:
        self.violations.append(
            f"{self.source}:{node.start_mark.line + 1}:"
            f"{node.start_mark.column + 1}: {message}"
        )


def workflow_files() -> list[Path]:
    files: list[Path] = []
    for root in WORKFLOW_ROOTS:
        if root.is_dir():
            files.extend(root.rglob("*.yml"))
            files.extend(root.rglob("*.yaml"))
    return sorted(set(files))


def dependency_is_pinned(dependency: str) -> bool:
    if dependency.startswith("docker://"):
        return IMAGE_DIGEST.fullmatch(dependency) is not None
    _, separator, revision = dependency.rpartition("@")
    return bool(separator) and COMMIT_SHA.fullmatch(revision) is not None


def container_image_is_pinned(image: str) -> bool:
    return CONTAINER_IMAGE_DIGEST.fullmatch(image) is not None


def _mapping_entries(
    node: MappingNode,
    scan: WorkflowScan,
) -> list[tuple[str | None, Node, Node]]:
    entries: list[tuple[str | None, Node, Node]] = []
    seen: dict[str, Node] = {}
    for key_node, value_node in node.value:
        if not isinstance(key_node, ScalarNode):
            scan.reject(key_node, "workflow mapping keys must be scalar strings")
            entries.append((None, key_node, value_node))
            continue
        key = key_node.value
        if key in seen:
            scan.reject(key_node, f"duplicate workflow mapping key {key!r}")
        else:
            seen[key] = key_node
        entries.append((key, key_node, value_node))
    return entries


def _direct_mapping_values(node: MappingNode, name: str) -> list[Node]:
    return [
        value_node
        for key_node, value_node in node.value
        if isinstance(key_node, ScalarNode) and key_node.value == name
    ]


def _check_dependency(node: Node, scan: WorkflowScan) -> None:
    if not isinstance(node, ScalarNode):
        scan.reject(node, "uses must be a scalar dependency reference")
        return
    dependency = node.value
    if dependency.startswith("./"):
        return
    scan.checked_dependencies += 1
    if not dependency_is_pinned(dependency):
        scan.reject(node, f"mutable external dependency: {dependency}")


def _check_image(node: Node, scan: WorkflowScan, context: str) -> None:
    if not isinstance(node, ScalarNode):
        scan.reject(node, f"{context} image must be a scalar reference")
        return
    scan.checked_images += 1
    if not container_image_is_pinned(node.value):
        scan.reject(node, f"mutable {context} image: {node.value}")


def _check_container(node: Node, scan: WorkflowScan) -> None:
    if isinstance(node, ScalarNode):
        _check_image(node, scan, "job container")
        return
    if not isinstance(node, MappingNode):
        scan.reject(node, "job container must be an image or mapping")
        return
    images = _direct_mapping_values(node, "image")
    if len(images) != 1:
        scan.reject(node, "job container must contain exactly one direct image")
    for image in images:
        _check_image(image, scan, "job container")


def _check_services(node: Node, scan: WorkflowScan) -> None:
    if not isinstance(node, MappingNode):
        scan.reject(node, "services must be a mapping")
        return
    for service_key, service_node in node.value:
        service_name = (
            service_key.value
            if isinstance(service_key, ScalarNode)
            else "<non-scalar>"
        )
        if not isinstance(service_node, MappingNode):
            scan.reject(service_node, f"service {service_name!r} must be a mapping")
            continue
        images = _direct_mapping_values(service_node, "image")
        if len(images) != 1:
            scan.reject(
                service_node,
                f"service {service_name!r} must contain exactly one direct image",
            )
        for image in images:
            _check_image(image, scan, f"service {service_name!r}")


def _check_shell(node: Node, scan: WorkflowScan) -> None:
    if not isinstance(node, ScalarNode):
        scan.reject(node, "run must be a scalar shell program")
        return
    engine = RAW_CONTAINER_ENGINE.search(node.value)
    if engine is not None:
        scan.reject(
            node,
            f"raw {engine.group(0)} in a workflow shell is forbidden; "
            "use tools/run-pinned-container.py",
        )


def _check_action_runs(node: Node, scan: WorkflowScan) -> None:
    if not isinstance(node, MappingNode):
        return
    using = _direct_mapping_values(node, "using")
    if len(using) != 1 or not isinstance(using[0], ScalarNode):
        return
    if using[0].value.lower() != "docker":
        return

    images = _direct_mapping_values(node, "image")
    if len(images) != 1:
        scan.reject(node, "Docker action runs must contain exactly one direct image")
        return
    image = images[0]
    if not isinstance(image, ScalarNode):
        scan.reject(image, "Docker action image must be a scalar reference")
        return
    scan.checked_images += 1
    if not image.value.startswith("docker://"):
        scan.reject(
            image,
            "local Dockerfile actions are forbidden until their FROM chain is audited; "
            "use a docker:// image pinned by SHA256",
        )
    elif not dependency_is_pinned(image.value):
        scan.reject(image, f"mutable Docker action image: {image.value}")


def _walk(node: Node, scan: WorkflowScan, ancestors: frozenset[int]) -> None:
    identity = id(node)
    if identity in ancestors:
        scan.reject(node, "cyclic YAML aliases are forbidden")
        return
    next_ancestors = ancestors | {identity}

    if isinstance(node, MappingNode):
        entries = _mapping_entries(node, scan)
        for key, _key_node, value_node in entries:
            if key == "uses":
                _check_dependency(value_node, scan)
            elif key == "container":
                _check_container(value_node, scan)
            elif key == "services":
                _check_services(value_node, scan)
            elif key == "run":
                _check_shell(value_node, scan)
            elif key == "runs":
                _check_action_runs(value_node, scan)
            _walk(value_node, scan, next_ancestors)
    elif isinstance(node, SequenceNode):
        for item in node.value:
            _walk(item, scan, next_ancestors)


def scan_workflow_text(
    text: str, *, source: str = "<workflow>"
) -> tuple[int, int, list[str]]:
    scan = WorkflowScan(source=source)
    try:
        documents = list(yaml.compose_all(text, Loader=yaml.SafeLoader))
    except yaml.YAMLError as error:
        mark = getattr(error, "problem_mark", None)
        if mark is None:
            scan.violations.append(f"{source}: invalid YAML: {error}")
        else:
            scan.violations.append(
                f"{source}:{mark.line + 1}:{mark.column + 1}: "
                f"invalid YAML: {error}"
            )
        return 0, 0, scan.violations

    nonempty = [document for document in documents if document is not None]
    if len(nonempty) != 1:
        scan.violations.append(
            f"{source}: workflow must contain exactly one YAML document"
        )
    for document in nonempty:
        _walk(document, scan, frozenset())
    return scan.checked_dependencies, scan.checked_images, scan.violations


def main() -> int:
    checked_dependencies = 0
    checked_images = 0
    violations: list[str] = []

    for path in workflow_files():
        relative = path.relative_to(REPOSITORY_ROOT)
        dependencies, images, found = scan_workflow_text(
            path.read_text(encoding="utf-8"), source=str(relative)
        )
        checked_dependencies += dependencies
        checked_images += images
        violations.extend(found)

    if violations:
        print("mutable external workflow dependencies are forbidden:")
        for violation in violations:
            print(f"  {violation}")
        return 1

    print(
        f"verified {checked_dependencies} external workflow dependencies and "
        f"{checked_images} container images at immutable revisions"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
