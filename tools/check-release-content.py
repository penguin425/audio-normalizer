#!/usr/bin/env python3
"""Verify that a staged native release carries the complete public file set."""

from __future__ import annotations

import hashlib
import subprocess
import sys
from pathlib import Path


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
        ["git", "cat-file", "blob", f"HEAD:{relative}"],
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


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} STAGED_RELEASE_DIR")

    repo_root = Path(__file__).resolve().parents[1]
    staged_root = Path(sys.argv[1]).resolve()
    if not staged_root.is_dir():
        raise SystemExit(f"staged release directory does not exist: {staged_root}")

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
    compare_files(
        "JSON schema set",
        repo_root,
        selected_files(repo_schema, ("*.json",)),
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
        f"{len(selected_files(repo_schema, ('*.json',)))} JSON schemas, and "
        f"{len(files_under(repo_schema / 'ebu-qc-2026-04'))} EBU QC files"
    )


if __name__ == "__main__":
    main()
