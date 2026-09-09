from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


TOOLS = Path(__file__).resolve().parent
SCRIPT = TOOLS / "native_package_metadata.py"
SPEC = importlib.util.spec_from_file_location("native_package_metadata", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
metadata = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = metadata
SPEC.loader.exec_module(metadata)


def make_prefix(base: Path, name: str = "prefix") -> Path:
    root = base / name
    (root / "include").mkdir(parents=True)
    (root / "include/forge_normalizer.h").write_text("/* fixture */\n", encoding="ascii")
    return root


def add_library(root: Path, platform: str) -> None:
    layout = metadata.PLATFORM_LAYOUT[platform]
    for relative in (layout["library"], layout["import_library"]):
        if relative is not None:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")


class ValidationTests(unittest.TestCase):
    def test_numeric_release_version(self) -> None:
        for version in ("0.189.16", "1.2.3"):
            self.assertEqual(metadata.validate_version(version), version)
        for version in (
            "",
            " 1.2.3",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.03",
            "1.2.3-alpha.1+build.7",
            "1.2.3-01",
            "１.２.３",
            "1.2.3-β",
            "v1.2.3",
        ):
            with self.subTest(version=version):
                with self.assertRaises(metadata.MetadataError):
                    metadata.validate_version(version)

    def test_existing_root_and_platform_validation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(metadata.MetadataError):
                metadata.generate_metadata(Path(directory) / "missing", "1.2.3", "linux")
            root = make_prefix(Path(directory))
            with self.assertRaises(metadata.MetadataError):
                metadata.generate_metadata(root, "1.2.3", "solaris")
            with self.assertRaises(metadata.MetadataError):
                metadata.generate_metadata(root, "1.2.3", "")

    def test_empty_or_whitespace_root_is_rejected(self) -> None:
        with self.assertRaises(metadata.MetadataError):
            metadata.generate_metadata("", "1.2.3", "linux")
        with self.assertRaises(metadata.MetadataError):
            metadata.generate_metadata("   ", "1.2.3", "linux")
        with self.assertRaisesRegex(metadata.MetadataError, "cannot be resolved"):
            metadata.generate_metadata("bad\0root", "1.2.3", "linux")


class GenerationTests(unittest.TestCase):
    def test_unix_outputs_are_fixed_relocatable_and_have_expected_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_prefix(Path(directory), "prefix with space_日本語")
            add_library(root, "linux")
            outputs = metadata.generate_metadata(root, "0.189.16", "linux")
            self.assertEqual(
                {path.relative_to(root).as_posix() for path in outputs},
                {
                    "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake",
                    "lib/cmake/ForgeNormalizer/ForgeNormalizerConfigVersion.cmake",
                    "lib/pkgconfig/forge-normalizer.pc",
                },
            )
            for path in outputs:
                text = path.read_text(encoding="utf-8")
                self.assertNotRegex(text, r"@[A-Za-z_][A-Za-z0-9_]*@")
                self.assertNotIn(str(root), text)
            config = outputs[0].read_text(encoding="utf-8")
            self.assertIn('"${CMAKE_CURRENT_LIST_DIR}/../../.."', config)
            self.assertIn("Forge::Normalizer", config)
            self.assertIn("add_library(Forge::Normalizer SHARED IMPORTED)", config)
            self.assertNotIn("IMPORTED_IMPLIB", config)
            pc = outputs[2].read_text(encoding="utf-8")
            self.assertIn("prefix=${pcfiledir}/../..", pc)
            self.assertIn('Libs: -L"${libdir}" -lforge_normalizer', pc)
            self.assertIn('Cflags: -I"${includedir}"', pc)
            self.assertNotIn("Libs.private", pc)

    def test_macos_uses_dylib_and_windows_use_dll_and_import_library(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            macos = make_prefix(base, "mac")
            add_library(macos, "macos")
            mac_outputs = metadata.generate_metadata(macos, "1.2.3", "macos")
            mac_config = mac_outputs[0].read_text(encoding="utf-8")
            self.assertIn("lib/libforge_normalizer.dylib", mac_config)
            windows = make_prefix(base, "windows")
            add_library(windows, "windows")
            win_outputs = metadata.generate_metadata(windows, "1.2.3", "windows")
            self.assertEqual(len(win_outputs), 2)
            win_config = win_outputs[0].read_text(encoding="utf-8")
            self.assertIn("bin/forge_normalizer.dll", win_config)
            self.assertIn("lib/forge_normalizer.lib", win_config)
            self.assertIn("IMPORTED_IMPLIB", win_config)
            self.assertFalse((windows / "lib/pkgconfig/forge-normalizer.pc").exists())

    def test_windows_rejects_stray_pkg_config_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_prefix(Path(directory), "windows")
            add_library(root, "windows")
            stray = root / "lib/pkgconfig/forge-normalizer.pc"
            stray.parent.mkdir(parents=True)
            stray.write_text("Name: stale\n", encoding="ascii")
            with self.assertRaisesRegex(metadata.MetadataError, "pkg-config"):
                metadata.generate_metadata(root, "1.2.3", "windows")

    def test_expected_libraries_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_prefix(Path(directory))
            with self.assertRaisesRegex(metadata.MetadataError, "expected library"):
                metadata.generate_metadata(root, "1.2.3", "linux")
            (root / "bin").mkdir()
            (root / "bin/forge_normalizer.dll").write_bytes(b"fixture")
            with self.assertRaisesRegex(metadata.MetadataError, "expected library"):
                metadata.generate_metadata(root, "1.2.3", "windows")

    def test_output_overwrite_is_refused_without_replacing_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_prefix(Path(directory))
            add_library(root, "linux")
            metadata.generate_metadata(root, "1.2.3", "linux")
            config = root / "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake"
            original = config.read_bytes()
            with self.assertRaisesRegex(metadata.MetadataError, "overwrite"):
                metadata.generate_metadata(root, "1.2.3", "linux")
            self.assertEqual(config.read_bytes(), original)

    def test_partial_write_failure_removes_only_new_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_prefix(Path(directory))
            add_library(root, "linux")
            original_safe_parent = metadata._safe_output_parent
            calls = 0
            raced = (
                root
                / "lib/cmake/ForgeNormalizer/ForgeNormalizerConfigVersion.cmake"
            )

            def race_second_output(package_root: Path, relative: str) -> Path:
                nonlocal calls
                parent = original_safe_parent(package_root, relative)
                calls += 1
                if calls == 2:
                    raced.write_text("concurrent writer\n", encoding="ascii")
                return parent

            with mock.patch.object(
                metadata, "_safe_output_parent", side_effect=race_second_output
            ):
                with self.assertRaisesRegex(metadata.MetadataError, "cannot write"):
                    metadata.generate_metadata(root, "1.2.3", "linux")

            first = root / "lib/cmake/ForgeNormalizer/ForgeNormalizerConfig.cmake"
            self.assertFalse(first.exists())
            self.assertEqual(raced.read_text(encoding="ascii"), "concurrent writer\n")

    def test_template_renderer_rejects_unexpanded_tokens(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            template = Path(directory) / "template.in"
            template.write_text("prefix=@KNOWN@\nmissing=@MISSING@\n", encoding="ascii")
            with self.assertRaisesRegex(metadata.MetadataError, "unexpanded"):
                metadata._render_template(template, {"KNOWN": "ok"})


class CliTests(unittest.TestCase):
    def test_cli_reports_success_and_rejects_invalid_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_prefix(Path(directory))
            add_library(root, "linux")
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--root",
                    str(root),
                    "--version",
                    "0.189.16",
                    "--platform",
                    "linux",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("ForgeNormalizerConfig.cmake", completed.stdout)

            invalid = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--root",
                    str(root),
                    "--version",
                    "０.１８９.１６",
                    "--platform",
                    "linux",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(invalid.returncode, 0)
            self.assertIn("ASCII", invalid.stderr)


if __name__ == "__main__":
    unittest.main()
