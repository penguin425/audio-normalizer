#!/usr/bin/env python3
"""Fail closed when a Linux wheel exceeds Forge's published ABI contract."""

from __future__ import annotations

import argparse
import os
import re
import stat
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import Callable, NamedTuple, Sequence


MAX_GLIBC = (2, 34)
MAX_MEMBER_BYTES = 256 * 1024 * 1024
MAX_UNCOMPRESSED_BYTES = 512 * 1024 * 1024
ET_EXEC = 2
ET_DYN = 3

WHEEL_NAME = re.compile(
    r"^forge_normalizer-(?P<version>[0-9]+\.[0-9]+\.[0-9]+)-"
    r"py3-none-(?P<platform>[^.]+)\.whl$"
)
SYMBOL_VERSION = re.compile(
    r"(?<![A-Za-z0-9_])(?P<namespace>GLIBCXX|GLIBC|CXXABI)_"
    r"(?P<version>PRIVATE|[0-9]+(?:\.[0-9]+)*|[A-Za-z][A-Za-z0-9_]*)\b"
)
NEEDED = re.compile(r"\(NEEDED\).*?Shared library: \[([^\]]+)\]")
SEARCH_PATH = re.compile(r"\((?:RPATH|RUNPATH)\)")
DYNAMIC_TEXTREL = re.compile(
    r"\(TEXTREL\)|\((?:FLAGS|FLAGS_1)\).*?\bTEXTREL\b"
)
INTERPRETER = re.compile(
    r"Requesting\s+program\s+interpreter:\s*([^\]\s]+)",
    re.IGNORECASE,
)
NON_BASELINE_ISA = re.compile(r"x86[-_]64[-_]v[234]", re.IGNORECASE)
AARCH64_NON_BASELINE_FEATURE = re.compile(
    r"(?:"
    r"AArch64\s+feature:.*\b(?:AES|BTI|CRC|FP16|PAC|SHA1|SHA2|SVE|SVE2|SME|SME2)\b"
    r"|\b(?:AES|CRC|FP16|SHA1|SHA2|SVE|SVE2|SME|SME2)\b"
    r"|Tag_CPU_arch:\s*v(?:8\.[1-9]|9)"
    r")",
    re.IGNORECASE | re.DOTALL,
)


class LinuxAbiContract(NamedTuple):
    """Architecture-specific part of the published Linux ABI contract."""

    architecture: str
    platform: str
    elf_machine: int
    elf_machine_name: str
    dynamic_loader: str
    allowed_needed: frozenset[str]
    baseline_description: str

    @property
    def wheel_tag(self) -> str:
        return f"py3-none-{self.platform}"

    @property
    def native_member(self) -> str:
        return "forge_normalizer/lib/libforge_normalizer.so"


_COMMON_NEEDED = frozenset(
    {
        "libc.so.6",
        "libdl.so.2",
        "libgcc_s.so.1",
        "libm.so.6",
        "libpthread.so.0",
        "librt.so.1",
        "libutil.so.1",
    }
)

ABI_CONTRACTS = {
    "x86_64": LinuxAbiContract(
        architecture="x86_64",
        platform="manylinux_2_34_x86_64",
        elf_machine=62,
        elf_machine_name="x86-64",
        dynamic_loader="ld-linux-x86-64.so.2",
        allowed_needed=_COMMON_NEEDED | {"ld-linux-x86-64.so.2"},
        baseline_description="x86-64-v2+ ISA",
    ),
    "aarch64": LinuxAbiContract(
        architecture="aarch64",
        platform="manylinux_2_34_aarch64",
        elf_machine=183,
        elf_machine_name="AArch64",
        dynamic_loader="ld-linux-aarch64.so.1",
        allowed_needed=_COMMON_NEEDED | {"ld-linux-aarch64.so.1"},
        baseline_description="optional AArch64 ISA extension",
    ),
}
ARCHITECTURES = tuple(ABI_CONTRACTS)


class WheelAbiError(ValueError):
    """The candidate does not satisfy the published Linux wheel contract."""


def contract_for_architecture(architecture: str = "x86_64") -> LinuxAbiContract:
    try:
        return ABI_CONTRACTS[architecture]
    except (KeyError, TypeError) as error:
        choices = ", ".join(ARCHITECTURES)
        raise WheelAbiError(
            f"unsupported Linux architecture {architecture!r}; expected one of {choices}"
        ) from error


# Compatibility aliases for callers and tests that predate the explicit
# architecture argument.  The default remains the published x86-64 contract.
_X86_64 = ABI_CONTRACTS["x86_64"]
EXPECTED_PLATFORM = _X86_64.platform
EXPECTED_TAG = _X86_64.wheel_tag
EXPECTED_NATIVE_MEMBER = _X86_64.native_member
ALLOWED_NEEDED = _X86_64.allowed_needed
AUDITWHEEL_POLICY = re.compile(
    r"consistent\s+with\s+the\s+following\s+platform\s+tag:\s*"
    r"[\"']manylinux_(?P<major>[0-9]+)_(?P<minor>[0-9]+)_x86_64[\"']",
    re.IGNORECASE | re.DOTALL,
)


ElfInspector = Callable[[Path, str, str], None]
AuditwheelChecker = Callable[[Path, str], None]


def _run_checked(command: Sequence[str]) -> str:
    environment = os.environ.copy()
    environment["LC_ALL"] = "C"
    try:
        completed = subprocess.run(
            list(command),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=environment,
        )
    except OSError as error:
        raise WheelAbiError(f"cannot run {command[0]!r}: {error}") from error
    if completed.returncode != 0:
        raise WheelAbiError(
            f"{' '.join(command)} failed with status {completed.returncode}:\n"
            f"{completed.stdout}"
        )
    return completed.stdout


def _version_tuple(text: str) -> tuple[int, ...]:
    return tuple(int(part) for part in text.split("."))


def validate_version_info(output: str, *, member: str = "ELF") -> None:
    for match in SYMBOL_VERSION.finditer(output):
        namespace = match.group("namespace")
        version = match.group("version")
        symbol = f"{namespace}_{version}"
        if namespace == "GLIBC" and version == "PRIVATE":
            raise WheelAbiError(f"{member} references forbidden {symbol}")
        if namespace == "GLIBC":
            if not version[0].isdigit():
                raise WheelAbiError(
                    f"{member} references unexpected GLIBC version namespace {symbol}"
                )
            if _version_tuple(version) > MAX_GLIBC:
                raise WheelAbiError(
                    f"{member} references {symbol}, above "
                    f"GLIBC_{MAX_GLIBC[0]}.{MAX_GLIBC[1]}"
                )
        if namespace in {"GLIBCXX", "CXXABI"}:
            raise WheelAbiError(
                f"{member} references unexpected C++ runtime symbol version {symbol}"
            )


def validate_dynamic_section(
    output: str, *, member: str = "ELF", architecture: str = "x86_64"
) -> None:
    contract = contract_for_architecture(architecture)
    needed = set(NEEDED.findall(output))
    unexpected = sorted(needed - contract.allowed_needed)
    if unexpected:
        raise WheelAbiError(
            f"{member} has unexpected {contract.architecture} DT_NEEDED entries: "
            f"{', '.join(unexpected)}"
        )
    if SEARCH_PATH.search(output):
        raise WheelAbiError(f"{member} has a forbidden DT_RPATH or DT_RUNPATH")
    if DYNAMIC_TEXTREL.search(output):
        raise WheelAbiError(f"{member} has forbidden text relocations")


def validate_notes(
    output: str, *, member: str = "ELF", architecture: str = "x86_64"
) -> None:
    contract = contract_for_architecture(architecture)
    if contract.architecture == "x86_64":
        requirement = NON_BASELINE_ISA.search(output)
        if requirement is not None:
            raise WheelAbiError(
                f"{member} requires non-baseline x86 ISA {requirement.group(0)}"
            )
        return

    requirement = AARCH64_NON_BASELINE_FEATURE.search(output)
    if requirement is not None:
        raise WheelAbiError(
            f"{member} requires a non-baseline AArch64 feature: "
            f"{requirement.group(0)}"
        )


def validate_program_headers(output: str, *, member: str = "ELF") -> None:
    stack_headers = [
        line for line in output.splitlines() if line.lstrip().startswith("GNU_STACK")
    ]
    if len(stack_headers) != 1:
        raise WheelAbiError(
            f"{member} must have exactly one non-executable GNU_STACK header"
        )
    flags = [
        token
        for token in stack_headers[0].split()[1:]
        if token and set(token) <= set("RWE")
    ]
    if len(flags) != 1:
        raise WheelAbiError(f"cannot parse {member} GNU_STACK permissions")
    if "E" in flags[0]:
        raise WheelAbiError(f"{member} requests an executable stack")


def validate_interpreter(
    output: str, *, member: str = "ELF", architecture: str = "x86_64"
) -> None:
    """Reject an ELF interpreter from another Linux architecture."""

    contract = contract_for_architecture(architecture)
    interpreters = INTERPRETER.findall(output)
    for interpreter in interpreters:
        if Path(interpreter).name != contract.dynamic_loader:
            raise WheelAbiError(
                f"{member} requests interpreter {interpreter!r}, expected "
                f"{contract.dynamic_loader}"
            )


def validate_elf_header(
    data: bytes,
    *,
    member: str,
    architecture: str = "x86_64",
    expected_elf_type: int = ET_DYN,
) -> None:
    contract = contract_for_architecture(architecture)
    expected_type_name = {
        ET_EXEC: "ET_EXEC",
        ET_DYN: "ET_DYN",
    }.get(expected_elf_type)
    if expected_type_name is None:
        raise ValueError(f"unsupported expected ELF type: {expected_elf_type}")
    if len(data) < 20 or data[:4] != b"\x7fELF":
        raise WheelAbiError(f"{member} is not a complete ELF header")
    if data[4] != 2:
        raise WheelAbiError(f"{member} is not an ELF64 object")
    if data[5] != 1:
        raise WheelAbiError(f"{member} is not little-endian")
    elf_type = int.from_bytes(data[16:18], "little")
    if elf_type != expected_elf_type:
        raise WheelAbiError(f"{member} is not an {expected_type_name} object")
    machine = int.from_bytes(data[18:20], "little")
    if machine != contract.elf_machine:
        raise WheelAbiError(
            f"{member} has ELF machine {machine}, expected "
            f"{contract.elf_machine_name} ({contract.elf_machine})"
        )


def inspect_elf(
    path: Path,
    member: str,
    readelf: str = "readelf",
    *,
    architecture: str = "x86_64",
) -> None:
    contract_for_architecture(architecture)
    version_info = _run_checked([readelf, "--version-info", "--wide", str(path)])
    dynamic = _run_checked([readelf, "--dynamic", "--wide", str(path)])
    notes = _run_checked([readelf, "--notes", "--wide", str(path)])
    program_headers = _run_checked(
        [readelf, "--program-headers", "--wide", str(path)]
    )
    validate_version_info(version_info, member=member)
    validate_dynamic_section(dynamic, member=member, architecture=architecture)
    validate_notes(notes, member=member, architecture=architecture)
    validate_program_headers(program_headers, member=member)
    validate_interpreter(
        program_headers,
        member=member,
        architecture=architecture,
    )


def validate_auditwheel_output(
    output: str, *, architecture: str = "x86_64"
) -> None:
    contract = contract_for_architecture(architecture)
    platform_arch = contract.platform.split("manylinux_2_34_", 1)[1]
    policy_pattern = re.compile(
        r"consistent\s+with\s+the\s+following\s+platform\s+tag:\s*"
        rf"[\"']manylinux_(?P<major>[0-9]+)_(?P<minor>[0-9]+)_"
        rf"{re.escape(platform_arch)}[\"']",
        re.IGNORECASE | re.DOTALL,
    )
    policy = policy_pattern.search(output)
    if policy is None:
        raise WheelAbiError(
            f"auditwheel did not report a compatible {contract.architecture} "
            "manylinux policy"
        )
    reported = (int(policy.group("major")), int(policy.group("minor")))
    if reported > MAX_GLIBC:
        raise WheelAbiError(
            f"auditwheel reports manylinux_{reported[0]}_{reported[1]}, "
            f"above manylinux_{MAX_GLIBC[0]}_{MAX_GLIBC[1]}"
        )
    if contract.architecture == "x86_64":
        requirement = NON_BASELINE_ISA.search(output)
        if requirement is not None:
            raise WheelAbiError(
                f"auditwheel reports non-baseline x86 ISA {requirement.group(0)}"
            )
    else:
        requirement = AARCH64_NON_BASELINE_FEATURE.search(output)
        if requirement is not None:
            raise WheelAbiError(
                "auditwheel reports a non-baseline AArch64 feature: "
                f"{requirement.group(0)}"
            )


def check_auditwheel(
    wheel: Path,
    auditwheel: str = "auditwheel",
    *,
    architecture: str = "x86_64",
) -> None:
    output = _run_checked([auditwheel, "show", str(wheel)])
    validate_auditwheel_output(output, architecture=architecture)


def _safe_members(wheel: zipfile.ZipFile) -> list[zipfile.ZipInfo]:
    members = wheel.infolist()
    names = [member.filename for member in members]
    if len(names) != len(set(names)):
        raise WheelAbiError("wheel contains duplicate member names")
    total = 0
    for member in members:
        path = PurePosixPath(member.filename)
        if (
            path.is_absolute()
            or ".." in path.parts
            or "\\" in member.filename
            or "\0" in member.filename
        ):
            raise WheelAbiError(f"unsafe wheel member: {member.filename}")
        member_type = stat.S_IFMT(member.external_attr >> 16)
        if member_type == stat.S_IFLNK:
            raise WheelAbiError(f"wheel contains a symbolic link: {member.filename}")
        if member.file_size > MAX_MEMBER_BYTES:
            raise WheelAbiError(f"wheel member is too large: {member.filename}")
        total += member.file_size
        if total > MAX_UNCOMPRESSED_BYTES:
            raise WheelAbiError("wheel expands beyond the verification byte limit")
    return members


def verify_wheel(
    wheel_path: Path,
    *,
    architecture: str = "x86_64",
    readelf: str = "readelf",
    auditwheel: str = "auditwheel",
    inspector: ElfInspector = inspect_elf,
    auditwheel_checker: AuditwheelChecker = check_auditwheel,
) -> list[str]:
    contract = contract_for_architecture(architecture)
    wheel_path = wheel_path.resolve()
    if not wheel_path.is_file():
        raise WheelAbiError(f"wheel does not exist: {wheel_path}")

    name = WHEEL_NAME.fullmatch(wheel_path.name)
    if name is None:
        raise WheelAbiError(f"unexpected Linux wheel filename: {wheel_path.name}")
    if name.group("platform") != contract.platform:
        raise WheelAbiError(
            f"filename platform is {name.group('platform')}, expected "
            f"{contract.platform}"
        )

    version = name.group("version")
    expected_metadata = f"forge_normalizer-{version}.dist-info/WHEEL"
    elf_members: list[tuple[zipfile.ZipInfo, bytes]] = []
    with zipfile.ZipFile(wheel_path) as wheel:
        members = _safe_members(wheel)
        metadata_members = [
            member for member in members if member.filename == expected_metadata
        ]
        if len(metadata_members) != 1:
            raise WheelAbiError(
                f"wheel must contain exactly one {expected_metadata} member"
            )
        metadata = wheel.read(metadata_members[0]).decode("utf-8", errors="strict")
        tags = {
            line.removeprefix("Tag:").strip()
            for line in metadata.splitlines()
            if line.startswith("Tag:")
        }
        if tags != {contract.wheel_tag}:
            raise WheelAbiError(
                f"WHEEL tags are {sorted(tags)!r}, expected only "
                f"{contract.wheel_tag!r}"
            )
        if "Root-Is-Purelib: false" not in metadata.splitlines():
            raise WheelAbiError("wheel payload is not marked as platform-specific")

        for member in members:
            if member.is_dir():
                continue
            with wheel.open(member) as stream:
                header = stream.read(20)
            if header.startswith(b"\x7fELF"):
                validate_elf_header(
                    header,
                    member=member.filename,
                    architecture=architecture,
                )
                elf_members.append((member, header))

        if not elf_members:
            raise WheelAbiError("wheel contains no ELF payload")
        if contract.native_member not in {
            member.filename for member, _ in elf_members
        }:
            raise WheelAbiError(
                f"wheel is missing ELF payload {contract.native_member}"
            )

        if inspector is inspect_elf:
            def inspect_candidate(
                path: Path, member: str, executable: str
            ) -> None:
                inspect_elf(
                    path,
                    member,
                    executable,
                    architecture=architecture,
                )
        else:
            inspect_candidate = inspector

        with tempfile.TemporaryDirectory(prefix="forge-wheel-abi-") as directory:
            extraction_root = Path(directory)
            inspected: list[str] = []
            for index, (member, _) in enumerate(elf_members):
                extracted = extraction_root / f"elf-{index}"
                with wheel.open(member) as source, extracted.open("wb") as target:
                    while chunk := source.read(1024 * 1024):
                        target.write(chunk)
                inspect_candidate(extracted, member.filename, readelf)
                inspected.append(member.filename)

    # Parse auditwheel's policy conclusion in addition to checking its exit
    # status.  auditwheel has historically returned success for informational
    # incompatibility reports.
    if auditwheel_checker is check_auditwheel:
        check_auditwheel(wheel_path, auditwheel, architecture=architecture)
    else:
        auditwheel_checker(wheel_path, auditwheel)
    return inspected


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("wheel", type=Path)
    parser.add_argument(
        "--architecture",
        choices=ARCHITECTURES,
        default="x86_64",
        help="Linux ELF/wheel contract to verify (default: x86_64)",
    )
    parser.add_argument("--auditwheel", default="auditwheel")
    parser.add_argument("--readelf", default="readelf")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        members = verify_wheel(
            args.wheel,
            architecture=args.architecture,
            auditwheel=args.auditwheel,
            readelf=args.readelf,
        )
    except (OSError, UnicodeError, zipfile.BadZipFile, WheelAbiError) as error:
        print(f"Linux wheel ABI verification failed: {error}", file=sys.stderr)
        return 1
    contract = contract_for_architecture(args.architecture)
    print(
        f"verified {args.wheel}: {contract.wheel_tag}, GLIBC <= 2.34, "
        f"no declared {contract.baseline_description} requirement, "
        f"{len(members)} ELF member(s)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
