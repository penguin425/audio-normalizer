#!/usr/bin/env python3
"""Build one reproducible Forge platform wheel from a supplied native library."""

from __future__ import annotations

import argparse
import os
import re
import runpy
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path, PurePosixPath

SUPPORTED_PLATFORMS = {
    "macosx_10_12_x86_64": "libforge_normalizer.dylib",
    "macosx_11_0_arm64": "libforge_normalizer.dylib",
    # Linux builds are intentionally emitted with the non-portable tag first.
    # Only auditwheel repair plus check-linux-wheel-abi.py may produce a
    # manylinux_2_34 release candidate.
    "linux_x86_64": "libforge_normalizer.so",
    "win_amd64": "forge_normalizer.dll",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--library", required=True, type=Path)
    parser.add_argument("--library-name", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--outdir", required=True, type=Path)
    parser.add_argument("--version", required=True)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    repository = Path(__file__).resolve().parents[1]
    python_source = repository / "python"
    library = args.library.resolve()
    outdir = args.outdir.resolve()

    if not library.is_file():
        raise SystemExit(f"native library does not exist: {library}")
    required_library_name = SUPPORTED_PLATFORMS.get(args.platform)
    if required_library_name is None:
        raise SystemExit(f"unsupported wheel platform tag: {args.platform}")
    if args.library_name != required_library_name:
        raise SystemExit(
            f"{args.platform} requires {required_library_name}, "
            f"not {args.library_name}"
        )
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        raise SystemExit(f"invalid Forge version: {args.version}")

    source_version = runpy.run_path(
        str(python_source / "src/forge_normalizer/_version.py")
    )["__version__"]
    if source_version != args.version:
        raise SystemExit(
            f"Python package version {source_version} does not match {args.version}"
        )

    wheel_name = (
        f"forge_normalizer-{args.version}-py3-none-{args.platform}.whl"
    )
    outdir.mkdir(parents=True, exist_ok=True)
    output = outdir / wheel_name
    if output.exists():
        raise SystemExit(f"refusing to overwrite existing wheel: {output}")

    with tempfile.TemporaryDirectory(prefix="forge-python-wheel-") as temporary:
        staging = Path(temporary) / "python"
        shutil.copytree(python_source, staging)
        native_directory = staging / "src/forge_normalizer/lib"
        native_directory.mkdir()
        shutil.copy2(library, native_directory / args.library_name)

        environment = os.environ.copy()
        environment["FORGE_WHEEL_PLATFORM"] = args.platform
        subprocess.run(
            [
                sys.executable,
                "-m",
                "build",
                "--wheel",
                "--no-isolation",
                "--skip-dependency-check",
                "--outdir",
                str(outdir),
                str(staging),
            ],
            cwd=repository,
            env=environment,
            check=True,
        )

    if not output.is_file():
        produced = ", ".join(path.name for path in sorted(outdir.glob("*.whl")))
        raise SystemExit(f"expected {wheel_name}, produced: {produced}")

    with zipfile.ZipFile(output) as wheel:
        names = wheel.namelist()
        if len(names) != len(set(names)):
            raise SystemExit("wheel contains duplicate member names")
        for name in names:
            path = PurePosixPath(name)
            if path.is_absolute() or ".." in path.parts:
                raise SystemExit(f"unsafe wheel member: {name}")
        distribution = f"forge_normalizer-{args.version}.dist-info"
        native_member = f"forge_normalizer/lib/{args.library_name}"
        expected_names = {
            "forge_normalizer/__init__.py",
            "forge_normalizer/_binding.py",
            "forge_normalizer/_version.py",
            "forge_normalizer/py.typed",
            native_member,
            f"{distribution}/licenses/LICENSE",
            f"{distribution}/METADATA",
            f"{distribution}/RECORD",
            f"{distribution}/WHEEL",
            f"{distribution}/top_level.txt",
        }
        if set(names) != expected_names:
            missing = sorted(expected_names - set(names))
            unexpected = sorted(set(names) - expected_names)
            raise SystemExit(
                f"wheel member mismatch; missing={missing}, unexpected={unexpected}"
            )
        wheel_metadata_name = f"{distribution}/WHEEL"
        package_metadata_name = f"{distribution}/METADATA"
        wheel_metadata = wheel.read(wheel_metadata_name).decode("utf-8")
        package_metadata = wheel.read(package_metadata_name).decode("utf-8")
        if "Root-Is-Purelib: false" not in wheel_metadata:
            raise SystemExit("wheel payload is not marked as platform-specific")
        if f"Tag: py3-none-{args.platform}" not in wheel_metadata:
            raise SystemExit("wheel metadata has the wrong compatibility tag")
        if f"Version: {args.version}" not in package_metadata:
            raise SystemExit("wheel metadata has the wrong package version")
        if "Requires-Python: >=3.10" not in package_metadata:
            raise SystemExit("wheel metadata has the wrong Python requirement")

    print(output)


if __name__ == "__main__":
    main()
