#!/usr/bin/env python3
"""Reject mutable external dependencies in GitHub workflow definitions."""

from __future__ import annotations

import re
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
WORKFLOW_ROOTS = (
    REPOSITORY_ROOT / ".github" / "workflows",
    REPOSITORY_ROOT / ".github" / "actions",
)
USES_LINE = re.compile(r"^\s*(?:-\s*)?uses:\s*([^\s#]+)")
COMMIT_SHA = re.compile(r"^[0-9a-fA-F]{40}$")
IMAGE_DIGEST = re.compile(r"^docker://.+@sha256:[0-9a-fA-F]{64}$")


def workflow_files() -> list[Path]:
    files: list[Path] = []
    for root in WORKFLOW_ROOTS:
        if root.is_dir():
            files.extend(root.rglob("*.yml"))
            files.extend(root.rglob("*.yaml"))
    return sorted(set(files))


def main() -> int:
    checked = 0
    violations: list[str] = []

    for path in workflow_files():
        relative = path.relative_to(REPOSITORY_ROOT)
        for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            match = USES_LINE.match(line)
            if match is None:
                continue

            dependency = match.group(1).strip("'\"")
            if dependency.startswith("./"):
                continue

            checked += 1
            if dependency.startswith("docker://"):
                pinned = IMAGE_DIGEST.fullmatch(dependency) is not None
            else:
                _, separator, revision = dependency.rpartition("@")
                pinned = bool(separator) and COMMIT_SHA.fullmatch(revision) is not None

            if not pinned:
                violations.append(f"{relative}:{line_number}: {dependency}")

    if violations:
        print("mutable external workflow dependencies are forbidden:")
        for violation in violations:
            print(f"  {violation}")
        return 1

    print(f"verified {checked} external workflow dependencies at immutable revisions")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
