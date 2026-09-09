from __future__ import annotations

import importlib.util
import io
import os
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path


TOOLS = Path(__file__).resolve().parent
PROJECT_ROOT = TOOLS.parent
SCRIPT = TOOLS / "check-release-content.py"
SPEC = importlib.util.spec_from_file_location("check_release_content", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

METADATA_SCRIPT = TOOLS / "native_package_metadata.py"
METADATA_SPEC = importlib.util.spec_from_file_location(
    "native_package_metadata_for_release_content", METADATA_SCRIPT
)
assert METADATA_SPEC is not None and METADATA_SPEC.loader is not None
metadata = importlib.util.module_from_spec(METADATA_SPEC)
METADATA_SPEC.loader.exec_module(metadata)


def make_native_prefix(base: Path, platform: str) -> Path:
    root = base / platform
    (root / "include").mkdir(parents=True)
    (root / "include/forge_normalizer.h").write_text(
        "/* C ABI fixture */\n", encoding="ascii"
    )
    for relative in checker.NATIVE_LAYOUT[platform]["libraries"]:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"native fixture")
    metadata.generate_metadata(root, "0.189.16", platform)
    return root


class NativeReleaseContentTests(unittest.TestCase):
    def test_all_platform_layouts_and_generated_metadata_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            for platform in checker.NATIVE_PLATFORMS:
                with self.subTest(platform=platform):
                    root = make_native_prefix(base, platform)
                    checker.verify_native_package(
                        root,
                        repo_root=PROJECT_ROOT,
                        platform=platform,
                        version="0.189.16",
                    )

    def test_missing_compatibility_or_canonical_library_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "linux")
            (root / "libforge_normalizer.so").unlink()
            with self.assertRaisesRegex(SystemExit, "libforge_normalizer.so"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

    def test_compatibility_library_must_match_canonical_payload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "linux")
            (root / "libforge_normalizer.so").write_bytes(b"different")
            with self.assertRaisesRegex(SystemExit, "differs from canonical"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

    def test_wrong_platform_extension_and_static_library_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "linux")
            (root / "lib" / "libforge_normalizer.dylib").write_bytes(b"wrong")
            with self.assertRaisesRegex(SystemExit, "unexpected path"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

            (root / "lib" / "libforge_normalizer.dylib").unlink()
            (root / "lib" / "libforge_normalizer.a").write_bytes(b"static")
            with self.assertRaisesRegex(SystemExit, "static"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

    def test_windows_does_not_accept_pkg_config_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "windows")
            stray = root / "lib" / "pkgconfig" / "forge-normalizer.pc"
            stray.parent.mkdir(parents=True)
            stray.write_text("Name: forge-normalizer\n", encoding="ascii")
            with self.assertRaisesRegex(SystemExit, "pkg-config"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="windows",
                    version="0.189.16",
                )

    def test_metadata_must_match_versioned_generator_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "linux")
            config = root / checker.NATIVE_LAYOUT["linux"]["metadata"][0]
            config.write_bytes(config.read_bytes().replace(b"0.189.16", b"0.189.15"))
            with self.assertRaisesRegex(SystemExit, "unexpected version"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

    def test_unexpanded_tokens_and_absolute_repository_paths_fail_early(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "linux")
            config = root / checker.NATIVE_LAYOUT["linux"]["metadata"][0]
            config.write_bytes(config.read_bytes() + b"\n@PACKAGE_VERSION@\n")
            with self.assertRaisesRegex(SystemExit, "unexpanded token"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

            config.write_bytes(
                config.read_bytes()
                .replace(b"\n@PACKAGE_VERSION@\n", b"")
                + str(PROJECT_ROOT).encode("ascii")
                + b"\n"
            )
            with self.assertRaisesRegex(SystemExit, "repository path"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

    @unittest.skipUnless(hasattr(os, "symlink"), "symbolic links are unavailable")
    def test_any_staged_symlink_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = make_native_prefix(Path(directory), "linux")
            (root / "README.md").write_text("doc", encoding="ascii")
            (root / "docs-link").symlink_to("README.md")
            with self.assertRaisesRegex(SystemExit, "symbolic link"):
                checker.verify_native_package(
                    root,
                    repo_root=PROJECT_ROOT,
                    platform="linux",
                    version="0.189.16",
                )

    def test_argument_parser_preserves_legacy_invocation_and_requires_native_version(
        self,
    ) -> None:
        legacy = checker.build_parser().parse_args(["staged"])
        self.assertIsNone(legacy.native_platform)
        self.assertIsNone(legacy.version)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stream = io.StringIO()
            with redirect_stderr(stream):
                with self.assertRaises(SystemExit) as raised:
                    checker.main([str(root), "--native-platform", "linux"])
            self.assertEqual(raised.exception.code, 2)
            self.assertIn("--version is required", stream.getvalue())


if __name__ == "__main__":
    unittest.main()
