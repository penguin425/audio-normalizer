from __future__ import annotations

import importlib.util
import io
import json
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import warnings
import zipfile
from pathlib import Path
from unittest import mock


TOOLS = Path(__file__).resolve().parent
PROJECT_ROOT = TOOLS.parent
SCRIPT = TOOLS / "verify-registry-artifacts.py"
SPEC = importlib.util.spec_from_file_location("verify_registry_artifacts", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
checker = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = checker
SPEC.loader.exec_module(checker)


VERSION = "0.189.17"
WHEEL_PLATFORMS = (
    "manylinux_2_34_aarch64",
    "manylinux_2_34_x86_64",
    "macosx_10_12_x86_64",
    "macosx_11_0_arm64",
    "win_amd64",
)
NPM_GENERATED_FIXTURES = {
    # Release validation runs before the WASM build, so these package members
    # must not depend on ignored bindings left behind in a developer checkout.
    "forge_normalizer_wasm.js": b"export default async function init() {}\n",
    "forge_normalizer_wasm_bg.wasm": b"\x00asm\x01\x00\x00\x00",
    "forge_normalizer_wasm_bg.wasm.d.ts": (
        b"export const memory: WebAssembly.Memory;\n"
    ),
}
NPM_TRACKED_FIXTURES = frozenset(
    {
        "README.md",
        "index.js",
        "index.d.ts",
        "package.json",
    }
)
NPM_PAYLOAD_FIXTURES = (
    "README.md",
    "forge_normalizer_wasm.js",
    "forge_normalizer_wasm_bg.wasm",
    "forge_normalizer_wasm_bg.wasm.d.ts",
    "index.js",
    "index.d.ts",
)


def npm_member_bytes(relative: str) -> bytes:
    generated = NPM_GENERATED_FIXTURES.get(relative)
    if generated is not None:
        return generated
    if relative in NPM_TRACKED_FIXTURES:
        return (PROJECT_ROOT / "wasm/package" / relative).read_bytes()
    raise AssertionError(f"no npm test fixture is defined for {relative}")


def zip_member(name: str, mode: int, data: bytes = b"") -> tuple[zipfile.ZipInfo, bytes]:
    member = zipfile.ZipInfo(name)
    member.external_attr = mode << 16
    if stat.S_ISDIR(mode):
        member.external_attr |= 0x10
    return member, data


def make_wheel(
    root: Path,
    platform: str,
    version: str = VERSION,
    extra_members: tuple[tuple[zipfile.ZipInfo, bytes], ...] = (),
) -> Path:
    path = root / f"forge_normalizer-{version}-py3-none-{platform}.whl"
    distribution = f"forge_normalizer-{version}.dist-info"
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for member, data in extra_members:
            archive.writestr(member, data)
        archive.writestr("forge_normalizer/__init__.py", "__version__ = %r\n" % version)
        archive.writestr(
            f"{distribution}/METADATA",
            f"Metadata-Version: 2.4\nName: forge-normalizer\nVersion: {version}\n",
        )
        archive.writestr(
            f"{distribution}/WHEEL",
            "Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: false\n"
            f"Tag: py3-none-{platform}\n",
        )
    return path


def make_npm_tarball(root: Path, version: str = VERSION) -> Path:
    path = root / f"forge-normalizer-wasm-{version}.tgz"
    package = json.loads(
        (PROJECT_ROOT / "wasm/package/package.json").read_text(encoding="utf-8")
    )
    package["version"] = version
    members = {
        "package/package.json": json.dumps(package, indent=2).encode() + b"\n",
        "package/LICENSE": (PROJECT_ROOT / "LICENSE").read_bytes(),
    }
    for relative in NPM_PAYLOAD_FIXTURES:
        members[f"package/{relative}"] = npm_member_bytes(relative)
    with tarfile.open(path, "w:gz") as archive:
        for name in sorted(members):
            data = members[name]
            member = tarfile.TarInfo(name)
            member.size = len(data)
            member.mode = 0o644
            member.mtime = 0
            archive.addfile(member, io.BytesIO(data))
    return path


def make_crate(root: Path, version: str = VERSION) -> Path:
    path = root / f"forge-normalizer-{version}.crate"
    prefix = f"forge-normalizer-{version}"
    with tarfile.open(path, "w:gz") as archive:
        members = {
            f"{prefix}/Cargo.toml": (
                "[package]\n"
                'name = "forge-normalizer"\n'
                f'version = "{version}"\n'
                'repository = "https://github.com/penguin425/audio-normalizer"\n'
            ).encode("utf-8"),
            f"{prefix}/README.md": b"fixture\n",
        }
        for name, data in members.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            member.mode = 0o644
            member.mtime = 0
            archive.addfile(member, io.BytesIO(data))
    return path


class RegistryArtifactTests(unittest.TestCase):
    def test_wheel_accepts_canonical_empty_directory_members(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            directories = (
                zip_member("forge_normalizer/", stat.S_IFDIR | 0o755),
                zip_member(
                    f"forge_normalizer-{VERSION}.dist-info/",
                    stat.S_IFDIR | 0o755,
                ),
            )
            wheel = make_wheel(
                root,
                WHEEL_PLATFORMS[0],
                extra_members=directories,
            )
            artifact = checker.inspect_wheel(wheel, VERSION)
            self.assertEqual(artifact.platform, WHEEL_PLATFORMS[0])

    def test_wheel_rejects_unsafe_directory_members(self) -> None:
        cases = {
            "relative traversal": zip_member("../escape/", stat.S_IFDIR | 0o755),
            "absolute path": zip_member("/absolute/", stat.S_IFDIR | 0o755),
            "dot component": zip_member("./directory/", stat.S_IFDIR | 0o755),
            "empty component": zip_member("nested//directory/", stat.S_IFDIR | 0o755),
            "backslash": zip_member("bad\\directory/", stat.S_IFDIR | 0o755),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index, (label, member) in enumerate(cases.items()):
                with self.subTest(label=label):
                    case_root = root / str(index)
                    case_root.mkdir()
                    wheel = make_wheel(
                        case_root,
                        WHEEL_PLATFORMS[0],
                        extra_members=(member,),
                    )
                    with self.assertRaisesRegex(
                        checker.VerificationError,
                        "unsafe archive member name",
                    ):
                        checker.inspect_wheel(wheel, VERSION)

    def test_wheel_rejects_noncanonical_or_special_members(self) -> None:
        cases = {
            "directory payload": (
                (zip_member("payload/", stat.S_IFDIR | 0o755, b"not empty"),),
                "directory member is not empty",
            ),
            "directory with file mode": (
                (zip_member("regular/", stat.S_IFREG | 0o644),),
                "directory member has a non-directory mode",
            ),
            "directory symlink": (
                (zip_member("link/", stat.S_IFLNK | 0o777),),
                "symbolic link",
            ),
            "special file": (
                (zip_member("fifo", stat.S_IFIFO | 0o644),),
                "non-regular member",
            ),
            "file-directory collision": (
                (
                    zip_member("collision", stat.S_IFREG | 0o644),
                    zip_member("collision/", stat.S_IFDIR | 0o755),
                ),
                "conflicting member paths",
            ),
            "file as parent": (
                (
                    zip_member("parent", stat.S_IFREG | 0o644),
                    zip_member("parent/child", stat.S_IFREG | 0o644),
                ),
                "descends from a regular file",
            ),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index, (label, (members, error)) in enumerate(cases.items()):
                with self.subTest(label=label):
                    case_root = root / str(index)
                    case_root.mkdir()
                    wheel = make_wheel(
                        case_root,
                        WHEEL_PLATFORMS[0],
                        extra_members=members,
                    )
                    with self.assertRaisesRegex(checker.VerificationError, error):
                        checker.inspect_wheel(wheel, VERSION)

    def test_wheel_rejects_duplicate_directory_members(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            duplicate = (
                zip_member("forge_normalizer/", stat.S_IFDIR | 0o755),
                zip_member("forge_normalizer/", stat.S_IFDIR | 0o755),
            )
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                wheel = make_wheel(
                    root,
                    WHEEL_PLATFORMS[0],
                    extra_members=duplicate,
                )
            with self.assertRaisesRegex(checker.VerificationError, "duplicate member"):
                checker.inspect_wheel(wheel, VERSION)

    def test_select_wheels_requires_exact_platforms_and_arm64(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            wheels = [make_wheel(root, platform) for platform in WHEEL_PLATFORMS]
            selected = checker.select_wheels(
                root,
                [],
                VERSION,
                expected_platforms=WHEEL_PLATFORMS,
            )
            self.assertEqual([item.path for item in selected], sorted(wheels))
            only_x86 = root / "only-x86"
            only_x86.mkdir()
            x86 = make_wheel(only_x86, WHEEL_PLATFORMS[1])
            with self.assertRaisesRegex(checker.VerificationError, "ARM64"):
                checker.select_wheels(
                    only_x86,
                    [x86],
                    VERSION,
                    expected_platforms=(WHEEL_PLATFORMS[1],),
                )

    def test_npm_tarball_has_exact_members_and_public_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = make_npm_tarball(Path(directory))
            artifact = checker.inspect_npm_tarball(path, VERSION)
            self.assertEqual(artifact.name, "@forge-normalizer/wasm")
            self.assertEqual(artifact.digests["sha1"], checker._digest_file(path)["sha1"])

            extra_root = Path(directory) / "extra"
            extra_root.mkdir()
            extra = extra_root / path.name
            with tarfile.open(path, "r:gz") as source, tarfile.open(extra, "w:gz") as target:
                for member in source.getmembers():
                    payload = source.extractfile(member)
                    target.addfile(member, payload)
                data = b"unexpected\n"
                member = tarfile.TarInfo("package/forge_normalizer_wasm.d.ts")
                member.size = len(data)
                target.addfile(member, io.BytesIO(data))
            with self.assertRaisesRegex(checker.VerificationError, "member mismatch"):
                checker.inspect_npm_tarball(extra, VERSION)

    def test_registry_comparisons_accept_absent_and_exact_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            wheels = checker.select_wheels(root, [make_wheel(root, p) for p in WHEEL_PLATFORMS], VERSION)
            npm = checker.inspect_npm_tarball(make_npm_tarball(root), VERSION)
            crate = checker.inspect_crate(make_crate(root), VERSION)

            self.assertEqual(checker.compare_pypi(None, VERSION, wheels)["state"], "absent")
            self.assertEqual(checker.compare_npm(None, VERSION, npm)["state"], "absent")
            self.assertEqual(checker.compare_crates(None, VERSION, crate)["state"], "absent")

            pypi = {
                "info": {"name": "forge-normalizer"},
                "releases": {
                    VERSION: [
                        {
                            "filename": item.path.name,
                            "digests": {"sha256": item.digests["sha256"]},
                            "size": item.path.stat().st_size,
                        }
                        for item in wheels
                    ]
                },
            }
            npm_document = {
                "name": npm.name,
                "versions": {
                    VERSION: {
                        "name": npm.name,
                        "version": VERSION,
                        "dist": {
                            "shasum": npm.digests["sha1"],
                            "integrity": npm.digests["integrity"],
                        },
                    }
                },
            }
            crate_document = {
                "version": {
                    "crate": crate.name,
                    "num": VERSION,
                    "checksum": crate.digests["sha256"],
                }
            }
            self.assertEqual(checker.compare_pypi(pypi, VERSION, wheels)["state"], "exact")
            self.assertEqual(checker.compare_npm(npm_document, VERSION, npm)["state"], "exact")
            self.assertEqual(checker.compare_crates(crate_document, VERSION, crate)["state"], "exact")

            pypi["releases"][VERSION][0]["digests"]["sha256"] = "0" * 64
            self.assertEqual(checker.compare_pypi(pypi, VERSION, wheels)["state"], "mismatch")

    def test_local_mode_and_registry_selection_do_not_query_unselected_registries(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for platform in WHEEL_PLATFORMS:
                make_wheel(root, platform)
            make_npm_tarball(root)
            make_crate(root)
            with mock.patch.object(checker, "fetch_json") as fetch:
                report = checker.verify(
                    version=VERSION,
                    artifact_dir=root,
                    wheel_paths=[],
                    expected_wheel_platforms=None,
                    require_arm64=True,
                    npm_path=None,
                    crate_path=None,
                    state_mode="local",
                    pypi_url=checker.DEFAULT_PYPI_URL,
                    npm_url=checker.DEFAULT_NPM_URL,
                    crates_url=checker.DEFAULT_CRATES_URL,
                    timeout=1,
                )
                self.assertEqual(report["registries"], {})
                fetch.assert_not_called()

            with mock.patch.object(checker, "fetch_json", return_value=None) as fetch:
                report = checker.verify(
                    version=VERSION,
                    artifact_dir=root,
                    wheel_paths=[],
                    expected_wheel_platforms=None,
                    require_arm64=True,
                    npm_path=None,
                    crate_path=None,
                    state_mode="pre",
                    pypi_url=checker.DEFAULT_PYPI_URL,
                    npm_url=checker.DEFAULT_NPM_URL,
                    crates_url=checker.DEFAULT_CRATES_URL,
                    timeout=1,
                    registries=["npm"],
                )
                self.assertEqual(set(report["registries"]), {"npm"})
                self.assertEqual(fetch.call_count, 1)

    def test_fetch_json_uses_get_and_accepts_only_404_as_absent(self) -> None:
        class Response:
            def __enter__(self) -> "Response":
                return self

            def __exit__(self, *_args: object) -> None:
                return None

            def read(self) -> bytes:
                return b'{"ok": true}'

        with mock.patch.object(checker.urllib.request, "urlopen", return_value=Response()) as urlopen:
            self.assertEqual(checker.fetch_json("https://example.test/metadata"), {"ok": True})
        request = urlopen.call_args.args[0]
        self.assertEqual(request.method, "GET")
        self.assertEqual(request.headers["Accept"], "application/json")

    @unittest.skipUnless(shutil.which("npm"), "npm is required for the pack smoke test")
    def test_npm_pack_is_byte_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root / "package"
            package.mkdir()
            for relative in ("package.json", *NPM_PAYLOAD_FIXTURES):
                target = package / relative
                target.write_bytes(npm_member_bytes(relative))
            shutil.copy2(PROJECT_ROOT / "LICENSE", package / "LICENSE")
            first = root / "first"
            second = root / "second"
            first.mkdir()
            second.mkdir()
            environment = {"SOURCE_DATE_EPOCH": "0"}
            environment.update(__import__("os").environ)
            for destination in (first, second):
                subprocess.run(
                    ["npm", "pack", "--ignore-scripts", "--json", "--pack-destination", str(destination)],
                    cwd=package,
                    env=environment,
                    check=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
            first_bytes = (first / f"forge-normalizer-wasm-{VERSION}.tgz").read_bytes()
            second_bytes = (second / f"forge-normalizer-wasm-{VERSION}.tgz").read_bytes()
            self.assertEqual(first_bytes, second_bytes)
            checker.inspect_npm_tarball(first / f"forge-normalizer-wasm-{VERSION}.tgz", VERSION)


if __name__ == "__main__":
    unittest.main()
