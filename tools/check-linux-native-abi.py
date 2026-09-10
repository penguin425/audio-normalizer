#!/usr/bin/env python3
"""Fail closed when an extracted Linux release exceeds the ABI contract.

The release archive is inspected after extraction.  In particular, this
checker deliberately does not use ``Path.rglob``: an archive may contain a
link (including a link to a directory), and following one while walking the
tree would make the verification root ambiguous.  Every directory entry is
therefore inspected with ``follow_symlinks=False`` before it is considered.

The ELF policy is kept in ``check-linux-wheel-abi.py``.  Keeping one copy of
the policy matters: the wheel and native archive must have the same glibc,
dependency, relocation, stack, and ISA guarantees.
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import stat
import sys
import tarfile
from pathlib import Path
from pathlib import PurePosixPath
from typing import Callable, Sequence


def _load_wheel_abi_module():
    script = Path(__file__).with_name("check-linux-wheel-abi.py")
    spec = importlib.util.spec_from_file_location("check_linux_wheel_abi", script)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load ABI policy module: {script}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_WHEEL_ABI = _load_wheel_abi_module()
WheelAbiError = _WHEEL_ABI.WheelAbiError
ElfInspector = Callable[[Path, str, str], None]
contract_for_architecture = _WHEEL_ABI.contract_for_architecture

# Keep these limits deliberately independent of the number of ELF files.  A
# regular, non-ELF file still consumes archive extraction and stat budget and
# is consequently included in both the count and byte limits.
MAX_FILE_BYTES = 256 * 1024 * 1024
MAX_TOTAL_BYTES = 512 * 1024 * 1024
MAX_FILE_COUNT = 4096
MAX_ARCHIVE_BYTES = 256 * 1024 * 1024
MAX_ARCHIVE_MEMBERS = 8192

# Descriptive aliases for callers that use the terminology from the wheel
# checker.  The canonical names above are used by the implementation.
MAX_MEMBER_BYTES = MAX_FILE_BYTES
MAX_UNCOMPRESSED_BYTES = MAX_TOTAL_BYTES
MAX_FILES = MAX_FILE_COUNT

REQUIRED_FILES = (Path("forge"), Path("lib") / "libforge_normalizer.so")
EXPECTED_NATIVE_LIBRARY = REQUIRED_FILES[1]


# Re-export the policy entry points.  Besides making the ownership boundary
# obvious, this makes it straightforward for tests to replace ``inspector``
# with a deterministic fake without invoking readelf.
inspect_elf = _WHEEL_ABI.inspect_elf
validate_elf_header = _WHEEL_ABI.validate_elf_header


def _relative_to_root(path: Path, root: Path) -> str:
    """Return a stable POSIX member name, rejecting a root escape."""

    try:
        relative = path.relative_to(root)
    except ValueError as error:
        raise WheelAbiError(f"archive path escapes verification root: {path}") from error
    return relative.as_posix()


def _assert_inside_root(path: Path, root: Path) -> None:
    """Check the resolved path as a second defence against link escapes."""

    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise WheelAbiError(f"cannot resolve archive path {path}: {error}") from error
    try:
        resolved.relative_to(root)
    except ValueError as error:
        raise WheelAbiError(f"archive path escapes verification root: {path}") from error


def _walk_regular_files(
    root: Path,
    *,
    max_file_bytes: int,
    max_total_bytes: int,
    max_file_count: int,
) -> list[tuple[str, Path, int]]:
    """Walk *root* without following links and return bounded regular files."""

    if max_file_bytes < 0 or max_total_bytes < 0 or max_file_count < 0:
        raise ValueError("archive limits must be non-negative")

    try:
        root_stat = root.lstat()
    except OSError as error:
        raise WheelAbiError(f"cannot stat archive root {root}: {error}") from error
    if stat.S_ISLNK(root_stat.st_mode):
        raise WheelAbiError(f"archive root must not be a symbolic link: {root}")
    if not stat.S_ISDIR(root_stat.st_mode):
        raise WheelAbiError(f"archive root is not a directory: {root}")
    root = root.resolve(strict=True)

    files: list[tuple[str, Path, int]] = []
    total_bytes = 0

    def visit(directory: Path) -> None:
        nonlocal total_bytes
        try:
            with os.scandir(directory) as scan:
                entries = sorted(scan, key=lambda entry: entry.name)
        except OSError as error:
            raise WheelAbiError(f"cannot read archive directory {directory}: {error}") from error

        for entry in entries:
            candidate = Path(entry.path)
            # is_symlink uses lstat semantics and must happen before any test
            # that could follow the link.
            try:
                if entry.is_symlink():
                    raise WheelAbiError(
                        f"archive contains a symbolic link: "
                        f"{_relative_to_root(candidate, root)}"
                    )
                entry_stat = entry.stat(follow_symlinks=False)
            except WheelAbiError:
                raise
            except OSError as error:
                raise WheelAbiError(f"cannot stat archive path {candidate}: {error}") from error

            _assert_inside_root(candidate, root)
            relative = _relative_to_root(candidate, root)
            mode = entry_stat.st_mode
            if stat.S_ISDIR(mode):
                visit(candidate)
                continue
            if not stat.S_ISREG(mode):
                raise WheelAbiError(f"archive contains a non-regular file: {relative}")

            size = entry_stat.st_size
            if size > max_file_bytes:
                raise WheelAbiError(
                    f"archive file is too large: {relative} ({size} bytes, "
                    f"limit {max_file_bytes})"
                )
            if len(files) >= max_file_count:
                raise WheelAbiError(
                    f"archive contains too many regular files (limit {max_file_count})"
                )
            total_bytes += size
            if total_bytes > max_total_bytes:
                raise WheelAbiError(
                    "archive expands beyond the verification byte limit "
                    f"({max_total_bytes} bytes)"
                )
            files.append((relative, candidate, size))

    visit(root)
    return files


def _limit(value: int | None, *, default: int, label: str) -> int:
    if value is None:
        value = default
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"{label} must be a non-negative integer")
    return value


def extract_bounded_archive(
    archive_path: Path,
    destination_path: Path,
    *,
    expected_root: str | None = None,
    max_archive_bytes: int = MAX_ARCHIVE_BYTES,
    max_member_bytes: int = MAX_FILE_BYTES,
    max_total_bytes: int = MAX_TOTAL_BYTES,
    max_members: int = MAX_ARCHIVE_MEMBERS,
) -> Path:
    """Safely extract a bounded ``.tar.gz`` and return its single root."""

    for value, label in (
        (max_archive_bytes, "max_archive_bytes"),
        (max_member_bytes, "max_member_bytes"),
        (max_total_bytes, "max_total_bytes"),
        (max_members, "max_members"),
    ):
        _limit(value, default=value, label=label)

    archive_input = Path(archive_path)
    try:
        archive_stat = archive_input.lstat()
    except OSError as error:
        raise WheelAbiError(f"archive does not exist: {archive_input}") from error
    if stat.S_ISLNK(archive_stat.st_mode) or not stat.S_ISREG(archive_stat.st_mode):
        raise WheelAbiError(f"archive must be a regular non-symlink file: {archive_input}")
    if archive_stat.st_size > max_archive_bytes:
        raise WheelAbiError(
            f"compressed archive is too large ({archive_stat.st_size} bytes, "
            f"limit {max_archive_bytes})"
        )

    destination_input = Path(destination_path)
    try:
        destination_stat = destination_input.lstat()
    except OSError as error:
        raise WheelAbiError(
            f"extraction destination does not exist: {destination_input}"
        ) from error
    if stat.S_ISLNK(destination_stat.st_mode) or not stat.S_ISDIR(
        destination_stat.st_mode
    ):
        raise WheelAbiError(
            "extraction destination must be a non-symlink directory: "
            f"{destination_input}"
        )
    destination = destination_input.resolve(strict=True)
    try:
        with os.scandir(destination) as scan:
            if next(scan, None) is not None:
                raise WheelAbiError(
                    f"extraction destination must be empty: {destination}"
                )
    except OSError as error:
        raise WheelAbiError(
            f"cannot inspect extraction destination {destination}: {error}"
        ) from error

    names: set[str] = set()
    roots: set[str] = set()
    members: list[tarfile.TarInfo] = []
    total_bytes = 0
    try:
        with tarfile.open(archive_input, "r:gz") as bundle:
            for index, member in enumerate(bundle):
                if index >= max_members:
                    raise WheelAbiError(
                        f"archive contains too many members (limit {max_members})"
                    )
                name = member.name
                member_path = PurePosixPath(name)
                if (
                    not name
                    or member_path.is_absolute()
                    or ".." in member_path.parts
                    or "\\" in name
                    or "\0" in name
                    or not member_path.parts
                ):
                    raise WheelAbiError(f"unsafe archive member: {name!r}")
                normalized = member_path.as_posix()
                if normalized in names:
                    raise WheelAbiError(f"archive contains duplicate member: {name}")
                names.add(normalized)
                roots.add(member_path.parts[0])
                if not (member.isdir() or member.isfile()):
                    raise WheelAbiError(
                        f"archive contains a link or special member: {name}"
                    )
                if member.isfile():
                    if member.size < 0 or member.size > max_member_bytes:
                        raise WheelAbiError(
                            f"archive member is too large: {name} ({member.size} bytes, "
                            f"limit {max_member_bytes})"
                        )
                    total_bytes += member.size
                    if total_bytes > max_total_bytes:
                        raise WheelAbiError(
                            "archive expands beyond the preflight byte limit "
                            f"({max_total_bytes} bytes)"
                        )
                members.append(member)

            if not members:
                raise WheelAbiError("archive contains no members")
            if len(roots) != 1:
                raise WheelAbiError(
                    "archive must contain exactly one top-level directory; found "
                    + ", ".join(sorted(roots))
                )
            root_name = next(iter(roots))
            if expected_root is not None and root_name != expected_root:
                raise WheelAbiError(
                    f"archive root is {root_name!r}, expected {expected_root!r}"
                )
            if not any(
                member.isdir() and PurePosixPath(member.name).as_posix() == root_name
                for member in members
            ):
                raise WheelAbiError(
                    f"archive does not declare its top-level directory: {root_name}"
                )
            bundle.extractall(destination, members=members, filter="data")
    except (OSError, tarfile.TarError) as error:
        raise WheelAbiError(f"cannot extract archive {archive_input}: {error}") from error

    extracted_root = destination / root_name
    _assert_inside_root(extracted_root, destination)
    return extracted_root


def verify_archive(
    root_path: Path,
    *,
    architecture: str = "x86_64",
    readelf: str = "readelf",
    inspector: ElfInspector = inspect_elf,
    max_file_bytes: int | None = None,
    max_total_bytes: int | None = None,
    max_file_count: int | None = None,
    # ``max_files`` is a convenient spelling for callers and keeps the API
    # compatible with small standalone tests that model a file-count budget.
    max_files: int | None = None,
) -> list[str]:
    """Verify an extracted release and return relative names of ELF files."""

    contract_for_architecture(architecture)

    if max_file_count is not None and max_files is not None:
        raise ValueError("specify only one of max_file_count and max_files")
    if max_file_count is None:
        max_file_count = max_files
    file_limit = _limit(
        max_file_bytes, default=MAX_FILE_BYTES, label="max_file_bytes"
    )
    total_limit = _limit(
        max_total_bytes, default=MAX_TOTAL_BYTES, label="max_total_bytes"
    )
    count_limit = _limit(
        max_file_count, default=MAX_FILE_COUNT, label="max_file_count"
    )

    root_input = Path(root_path)
    try:
        input_stat = root_input.lstat()
    except OSError as error:
        raise WheelAbiError(f"archive root does not exist: {root_input}") from error
    if stat.S_ISLNK(input_stat.st_mode):
        raise WheelAbiError(f"archive root must not be a symbolic link: {root_input}")
    root = root_input.resolve(strict=True)
    files = _walk_regular_files(
        root,
        max_file_bytes=file_limit,
        max_total_bytes=total_limit,
        max_file_count=count_limit,
    )
    by_name = {relative: path for relative, path, _size in files}

    for required in REQUIRED_FILES:
        name = required.as_posix()
        candidate = by_name.get(name)
        if candidate is None:
            raise WheelAbiError(f"archive is missing required file: {name}")

    inspected: list[str] = []
    elf_names: set[str] = set()
    for relative, path, _size in files:
        try:
            with path.open("rb") as stream:
                header = stream.read(20)
        except OSError as error:
            raise WheelAbiError(f"cannot read archive file {relative}: {error}") from error
        if not header.startswith(b"\x7fELF"):
            continue
        validate_elf_header(
            header,
            member=relative,
            architecture=architecture,
        )
        elf_names.add(relative)
        if inspector is inspect_elf:
            inspect_elf(
                path,
                relative,
                readelf,
                architecture=architecture,
            )
        else:
            inspector(path, relative, readelf)
        inspected.append(relative)

    if EXPECTED_NATIVE_LIBRARY.as_posix() not in elf_names:
        raise WheelAbiError(
            f"required native library is not an ELF object: "
            f"{EXPECTED_NATIVE_LIBRARY.as_posix()}"
        )
    if not inspected:
        raise WheelAbiError("archive contains no ELF payload")
    return inspected


# Explicit aliases make the function discoverable under either terminology
# used by release tooling while keeping one implementation and one policy.
verify_native_archive = verify_archive
check_archive = verify_archive


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="verify the Linux ABI of an extracted Forge release archive"
    )
    parser.add_argument("root", nargs="?", type=Path, help="extracted archive root")
    parser.add_argument("--archive", type=Path, help="native .tar.gz to extract safely")
    parser.add_argument(
        "--extract-to", type=Path, help="existing empty extraction directory"
    )
    parser.add_argument("--expected-root", help="required top-level archive directory")
    parser.add_argument(
        "--architecture",
        choices=_WHEEL_ABI.ARCHITECTURES,
        default="x86_64",
        help="Linux ELF contract to verify (default: x86_64)",
    )
    parser.add_argument("--readelf", default="readelf")
    args = parser.parse_args(argv)
    if args.archive is None:
        if args.root is None:
            parser.error("ROOT is required unless --archive is supplied")
        if args.extract_to is not None or args.expected_root is not None:
            parser.error("--extract-to/--expected-root require --archive")
    else:
        if args.root is not None:
            parser.error("ROOT and --archive are mutually exclusive")
        if args.extract_to is None:
            parser.error("--extract-to is required with --archive")
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        root = args.root
        if args.archive is not None:
            root = extract_bounded_archive(
                args.archive,
                args.extract_to,
                expected_root=args.expected_root,
            )
        assert root is not None
        files = verify_archive(
            root,
            architecture=args.architecture,
            readelf=args.readelf,
        )
    except (OSError, UnicodeError, tarfile.TarError, WheelAbiError, ValueError) as error:
        print(
            f"Linux native archive ABI verification failed: {error}",
            file=sys.stderr,
        )
        return 1
    contract = contract_for_architecture(args.architecture)
    print(
        f"verified {Path(root).resolve()}: {contract.architecture}, "
        "GLIBC <= 2.34, "
        f"no declared {contract.baseline_description} requirement, "
        f"{len(files)} ELF file(s)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
