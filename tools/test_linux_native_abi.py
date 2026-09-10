from __future__ import annotations

import importlib.util
import io
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-linux-native-abi.py")
PACKAGE_SCRIPT = Path(__file__).with_name("package-linux-release.sh")
SPEC = importlib.util.spec_from_file_location("check_linux_native_abi", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
abi = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(abi)


def elf_header(marker: bytes = b"", *, architecture: str = "x86_64") -> bytes:
    header = bytearray(64)
    header[:4] = b"\x7fELF"
    header[4] = 2
    header[5] = 1
    header[6] = 1
    header[16:18] = (3).to_bytes(2, "little")
    machine = abi.contract_for_architecture(architecture).elf_machine
    header[18:20] = machine.to_bytes(2, "little")
    return bytes(header) + marker


def make_archive(
    root: Path,
    *,
    architecture: str = "x86_64",
    extra: tuple[str, ...] = (),
) -> None:
    (root / "lib").mkdir(parents=True)
    (root / "forge").write_bytes(
        elf_header(b"forge", architecture=architecture)
    )
    (root / "lib" / "libforge_normalizer.so").write_bytes(
        elf_header(b"library", architecture=architecture)
    )
    for name in extra:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(
            elf_header(name.encode("utf-8"), architecture=architecture)
        )


def accept_elf(_path: Path, _member: str, _readelf: str) -> None:
    return None


def write_tar(source: Path, archive: Path, root_name: str = "forge-package") -> None:
    with tarfile.open(archive, "w:gz") as bundle:
        bundle.add(source, arcname=root_name, recursive=True)


class NativeArchiveTests(unittest.TestCase):
    def test_aarch64_archive_uses_aarch64_machine_contract(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root, architecture="aarch64")
            self.assertEqual(
                abi.verify_archive(
                    root,
                    architecture="aarch64",
                    inspector=accept_elf,
                ),
                ["forge", "lib/libforge_normalizer.so"],
            )

    def test_aarch64_archive_is_rejected_by_x86_contract(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root, architecture="aarch64")
            with self.assertRaisesRegex(abi.WheelAbiError, "expected x86-64"):
                abi.verify_archive(root, inspector=accept_elf)

    def test_bounded_tar_extraction_yields_one_verified_root(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source = base / "source"
            make_archive(source)
            archive = base / "forge.tar.gz"
            write_tar(source, archive)
            destination = base / "extracted"
            destination.mkdir()

            root = abi.extract_bounded_archive(
                archive, destination, expected_root="forge-package"
            )
            self.assertEqual(root, destination / "forge-package")
            self.assertEqual(
                abi.verify_archive(root, inspector=accept_elf),
                ["forge", "lib/libforge_normalizer.so"],
            )

    def test_tar_preflight_rejects_links_traversal_and_resource_excess(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            destination = base / "extracted"
            destination.mkdir()

            special = base / "special.tar.gz"
            with tarfile.open(special, "w:gz") as bundle:
                root = tarfile.TarInfo("forge-package")
                root.type = tarfile.DIRTYPE
                bundle.addfile(root)
                link = tarfile.TarInfo("forge-package/link")
                link.type = tarfile.SYMTYPE
                link.linkname = "../../outside"
                bundle.addfile(link)
            with self.assertRaisesRegex(abi.WheelAbiError, "link or special"):
                abi.extract_bounded_archive(special, destination)

            traversal = base / "traversal.tar.gz"
            with tarfile.open(traversal, "w:gz") as bundle:
                member = tarfile.TarInfo("../outside")
                member.size = 1
                bundle.addfile(member, fileobj=io.BytesIO(b"x"))
            with self.assertRaisesRegex(abi.WheelAbiError, "unsafe archive member"):
                abi.extract_bounded_archive(traversal, destination)

            source = base / "source"
            make_archive(source)
            ordinary = base / "ordinary.tar.gz"
            write_tar(source, ordinary)
            with self.assertRaisesRegex(abi.WheelAbiError, "preflight byte limit"):
                abi.extract_bounded_archive(
                    ordinary, destination, max_total_bytes=1
                )
            with self.assertRaisesRegex(abi.WheelAbiError, "too many members"):
                abi.extract_bounded_archive(ordinary, destination, max_members=1)

    def test_tar_extraction_requires_empty_destination_and_expected_root(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source = base / "source"
            make_archive(source)
            archive = base / "forge.tar.gz"
            write_tar(source, archive)
            destination = base / "extracted"
            destination.mkdir()

            with self.assertRaisesRegex(abi.WheelAbiError, "archive root"):
                abi.extract_bounded_archive(
                    archive, destination, expected_root="different-root"
                )
            (destination / "occupied").write_text("x", encoding="ascii")
            with self.assertRaisesRegex(abi.WheelAbiError, "must be empty"):
                abi.extract_bounded_archive(archive, destination)

    def test_missing_required_files_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "forge").write_bytes(elf_header())
            with self.assertRaisesRegex(abi.WheelAbiError, "lib/libforge_normalizer.so"):
                abi.verify_archive(root, inspector=accept_elf)

    @unittest.skipUnless(hasattr(os, "symlink"), "symbolic links are unavailable")
    def test_symlink_is_rejected_even_when_it_points_inside_root(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root)
            (root / "README").write_text("payload", encoding="utf-8")
            (root / "alias").symlink_to("README")
            with self.assertRaisesRegex(abi.WheelAbiError, "symbolic link"):
                abi.verify_archive(root, inspector=accept_elf)

    @unittest.skipUnless(hasattr(os, "symlink"), "symbolic links are unavailable")
    def test_symlinked_root_is_rejected_before_resolution(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            root = parent / "payload"
            make_archive(root)
            link = parent / "root-link"
            link.symlink_to(root, target_is_directory=True)
            with self.assertRaisesRegex(abi.WheelAbiError, "root.*symbolic link"):
                abi.verify_archive(link, inspector=accept_elf)

    def test_per_file_count_and_total_limits_are_enforced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root)
            with self.assertRaisesRegex(abi.WheelAbiError, "too large"):
                abi.verify_archive(
                    root, inspector=accept_elf, max_file_bytes=10
                )
            with self.assertRaisesRegex(abi.WheelAbiError, "too many"):
                abi.verify_archive(
                    root, inspector=accept_elf, max_file_count=1
                )
            with self.assertRaisesRegex(abi.WheelAbiError, "byte limit"):
                abi.verify_archive(
                    root, inspector=accept_elf, max_total_bytes=100
                )

    def test_every_elf_is_inspected_in_sorted_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root, extra=("zeta", "bin/alpha", "lib/secondary.so"))
            inspected: list[str] = []

            def inspect(_path: Path, member: str, _readelf: str) -> None:
                inspected.append(member)

            result = abi.verify_archive(root, inspector=inspect)
            self.assertEqual(result, inspected)
            self.assertEqual(
                inspected,
                ["bin/alpha", "forge", "lib/libforge_normalizer.so", "lib/secondary.so", "zeta"],
            )

    def test_non_elf_regular_files_are_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root)
            (root / "README").write_bytes(b"not an ELF")
            inspected: list[str] = []

            def inspect(_path: Path, member: str, _readelf: str) -> None:
                inspected.append(member)

            abi.verify_archive(root, inspector=inspect)
            self.assertNotIn("README", inspected)
            self.assertEqual(len(inspected), 2)

    def test_inspector_errors_are_not_silenced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root)

            def inspect(_path: Path, member: str, _readelf: str) -> None:
                raise abi.WheelAbiError(f"rejected {member}")

            with self.assertRaisesRegex(abi.WheelAbiError, "rejected forge"):
                abi.verify_archive(root, inspector=inspect)

    def test_cli_success_and_error_exit_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_archive(root)
            fake_readelf = root / "fake-readelf"
            fake_readelf.write_text(
                "#!/bin/sh\nprintf '%s\\n' 'GNU_STACK 0x0 0x0 0x0 0x0 0x0 RW 0x10'\n",
                encoding="utf-8",
            )
            fake_readelf.chmod(0o755)
            success = subprocess.run(
                [sys.executable, str(SCRIPT), str(root), "--readelf", str(fake_readelf)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(success.returncode, 0, success.stderr)
            self.assertIn("verified", success.stdout)

            (root / "lib" / "libforge_normalizer.so").unlink()
            failure = subprocess.run(
                [sys.executable, str(SCRIPT), str(root)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(failure.returncode, 1)
            self.assertIn("verification failed", failure.stderr)


class NativePackageArgumentTests(unittest.TestCase):
    def test_package_script_rejects_unknown_architecture(self) -> None:
        result = subprocess.run(
            [
                str(PACKAGE_SCRIPT),
                "0.189.16",
                "x86_64-unknown-linux-gnu",
                "0",
                ".",
                "riscv64",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("unsupported Linux architecture", result.stderr)

    def test_package_script_rejects_target_architecture_mismatch(self) -> None:
        result = subprocess.run(
            [
                str(PACKAGE_SCRIPT),
                "0.189.16",
                "x86_64-unknown-linux-gnu",
                "0",
                ".",
                "aarch64",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("does not match Rust target", result.stderr)


if __name__ == "__main__":
    unittest.main()
