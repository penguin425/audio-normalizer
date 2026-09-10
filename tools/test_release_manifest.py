from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import stat
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


TOOLS = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("release_manifest", TOOLS / "release_manifest.py")
assert SPEC is not None and SPEC.loader is not None
release_manifest = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release_manifest
SPEC.loader.exec_module(release_manifest)


VERSION = "0.189.17"
EPOCH = 1_700_000_000


def _fixture_sboms(root: Path, name: str) -> None:
    payload = root / name
    payload.write_bytes(b"fixture payload\n")
    digest = hashlib.sha256(payload.read_bytes()).hexdigest()
    spdx = {
        "spdxVersion": "SPDX-2.3",
        "name": "/tmp/random-stage",
        "documentNamespace": "file:///tmp/random-stage",
        "creationInfo": {"created": "2026-09-10T12:34:56Z"},
        "packages": [],
    }
    cdx = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": "urn:uuid:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "metadata": {
            "timestamp": "2026-09-10T12:34:56Z",
            "component": {"name": "/private/tmp/random-stage"},
        },
        "components": [],
    }
    (root / f"{name}.spdx.json").write_bytes(
        release_manifest.canonical_json_bytes(
            release_manifest.normalize_sbom_data(
                spdx,
                format_name=release_manifest.SPDX_FORMAT,
                version=VERSION,
                artifact_name=name,
                artifact_sha256=digest,
                source_date_epoch=EPOCH,
            )
        )
    )
    (root / f"{name}.cdx.json").write_bytes(
        release_manifest.canonical_json_bytes(
            release_manifest.normalize_sbom_data(
                cdx,
                format_name=release_manifest.CYCLONEDX_FORMAT,
                version=VERSION,
                artifact_name=name,
                artifact_sha256=digest,
                source_date_epoch=EPOCH,
            )
        )
    )


def _fixture_spec(name: str = "forge-v0.189.17-linux-x86_64.tar.gz") -> release_manifest.AssetSpec:
    return release_manifest.AssetSpec(name, "native-archive", sbom=True)


class ExpectedContractTests(unittest.TestCase):
    def test_contract_includes_new_arm_npm_and_crate_assets(self) -> None:
        names = set(release_manifest.expected_asset_names(VERSION))
        self.assertIn(f"forge-v{VERSION}-linux-aarch64.tar.gz", names)
        self.assertIn(
            f"forge_normalizer-{VERSION}-py3-none-manylinux_2_34_aarch64.whl", names
        )
        self.assertIn(f"forge-normalizer-wasm-{VERSION}.tgz", names)
        self.assertIn(f"forge-normalizer-{VERSION}.crate", names)

    def test_public_profiles_are_explicitly_opt_in(self) -> None:
        private = set(release_manifest.expected_asset_names(VERSION))
        public = set(release_manifest.expected_asset_names(VERSION, include_pgo=True))
        self.assertFalse(any("pgo-" in name for name in private))
        self.assertEqual(len(public - private), 8)


class NormalizationTests(unittest.TestCase):
    def test_syft_time_and_temp_path_noise_normalizes_to_identical_bytes(self) -> None:
        digest = hashlib.sha256(b"payload").hexdigest()
        first = {
            "spdxVersion": "SPDX-2.3",
            "name": "/tmp/first-123",
            "documentNamespace": "https://anchore.com/syft/dir/first-uuid",
            "creationInfo": {"created": "2026-01-01T00:00:00Z"},
            "packages": [{"name": "foo", "sourceInfo": "/tmp/first-123/foo"}],
        }
        second = {
            "spdxVersion": "SPDX-2.3",
            "name": "/tmp/second-456",
            "documentNamespace": "https://anchore.com/syft/dir/second-uuid",
            "creationInfo": {"created": "2030-01-01T00:00:00Z"},
            "packages": [{"name": "foo", "sourceInfo": "/tmp/second-456/foo"}],
        }
        first_bytes = release_manifest.canonical_json_bytes(
            release_manifest.normalize_sbom_data(
                first,
                format_name="spdx",
                version=VERSION,
                artifact_name="artifact.tar.gz",
                artifact_sha256=digest,
                source_date_epoch=EPOCH,
            )
        )
        second_bytes = release_manifest.canonical_json_bytes(
            release_manifest.normalize_sbom_data(
                second,
                format_name="spdx",
                version=VERSION,
                artifact_name="artifact.tar.gz",
                artifact_sha256=digest,
                source_date_epoch=EPOCH,
            )
        )
        self.assertEqual(first_bytes, second_bytes)
        self.assertNotIn(b"/tmp/", first_bytes)
        self.assertIn(digest.encode("ascii"), first_bytes)


class ExactSetTests(unittest.TestCase):
    def test_finalize_and_verify_bind_payload_sidecars_and_checksums(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = "forge-v0.189.17-linux-x86_64.tar.gz"
            _fixture_sboms(root, name)
            specs = (_fixture_spec(name),)
            manifest = release_manifest.finalize_release(
                root,
                version=VERSION,
                specs=specs,
                source_date_epoch=EPOCH,
            )
            self.assertEqual(manifest["checksums"]["subject_count"], 3)
            checksum_lines = root.joinpath("SHA256SUMS").read_text(encoding="ascii").splitlines()
            self.assertTrue(checksum_lines)
            self.assertTrue(all("  ./" not in line for line in checksum_lines))
            checked = release_manifest.verify_manifest(
                root,
                expected_version=VERSION,
                expected_specs=specs,
            )
            self.assertEqual(checked["version"], VERSION)

    def test_sbom_timestamps_must_match_manifest_source_epoch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = "artifact.tar.gz"
            _fixture_sboms(root, name)
            with self.assertRaisesRegex(
                release_manifest.ReleaseManifestError, "timestamp does not match"
            ):
                release_manifest.build_manifest(
                    root,
                    version=VERSION,
                    specs=(_fixture_spec(name),),
                    source_date_epoch=EPOCH + 1,
                )

    def test_missing_and_extra_assets_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "missing files"):
                release_manifest.finalize_release(
                    root,
                    version=VERSION,
                    specs=(_fixture_spec("other.tar.gz"),),
                )
            _fixture_sboms(root, "artifact.tar.gz")
            root.joinpath("extra.txt").write_text("extra", encoding="ascii")
            with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "unlisted files"):
                release_manifest.build_manifest(
                    root,
                    version=VERSION,
                    specs=(_fixture_spec("artifact.tar.gz"),),
                )

    def test_duplicate_names_and_unsafe_paths_are_rejected(self) -> None:
        with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "duplicate"):
            release_manifest._validate_specs((_fixture_spec("a.tar.gz"), _fixture_spec("a.tar.gz")))
        for name in ("../escape", "/absolute", "a/b", "a\\b", "a b"):
            with self.subTest(name=name), self.assertRaises(release_manifest.ReleaseManifestError):
                release_manifest.validate_asset_name(name)

    def test_size_and_symlink_bounds_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _fixture_sboms(root, "artifact.tar.gz")
            with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "above"):
                release_manifest.safe_top_level_files(root, max_file_bytes=2)
            if hasattr(os, "symlink"):
                link = root / "linked"
                link.symlink_to(root / "artifact.tar.gz")
                with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "symbolic link"):
                    release_manifest.safe_top_level_files(root)

    def test_digest_tampering_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = "artifact.tar.gz"
            _fixture_sboms(root, name)
            specs = (_fixture_spec(name),)
            release_manifest.finalize_release(
                root, version=VERSION, specs=specs, source_date_epoch=EPOCH
            )
            root.joinpath(name).write_bytes(b"tampered")
            with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "digest/size mismatch"):
                release_manifest.verify_manifest(root, expected_specs=specs)

    def test_expected_contract_rejects_manifest_policy_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = "artifact.tar.gz"
            _fixture_sboms(root, name)
            specs = (_fixture_spec(name),)
            release_manifest.finalize_release(
                root, version=VERSION, specs=specs, source_date_epoch=EPOCH
            )
            manifest_path = root / release_manifest.DEFAULT_MANIFEST_NAME
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            payload = next(item for item in manifest["assets"] if item["name"] == name)
            payload["kind"] = "payload"
            manifest_path.write_bytes(release_manifest.canonical_json_bytes(manifest))
            with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "kind mismatch"):
                release_manifest.verify_manifest(root, expected_specs=specs)

    def test_slsa_bundle_is_evidence_and_not_a_circular_subject(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            name = "artifact.tar.gz"
            _fixture_sboms(root, name)
            digest = hashlib.sha256((root / name).read_bytes()).hexdigest()
            bundle_name = "forge-v0.189.17.slsa.jsonl"
            # The sidecars are also subjects, so derive the exact set through
            # a first manifest/checksum pass before creating the test bundle.
            specs = (_fixture_spec(name),)
            initial = release_manifest.finalize_release(
                root, version=VERSION, specs=specs, source_date_epoch=EPOCH
            )
            subjects = [
                {"name": record["name"], "digest": {"sha256": record["sha256"]}}
                for record in initial["assets"]
                if record["subject"]
            ]
            (root / bundle_name).write_text(json.dumps({"subject": subjects}) + "\n", encoding="utf-8")
            final = release_manifest.finalize_release(
                root,
                version=VERSION,
                specs=specs,
                source_date_epoch=EPOCH,
                slsa_bundle=bundle_name,
            )
            self.assertEqual(final["provenance"]["name"], bundle_name)
            self.assertEqual(final["provenance"]["subject_count"], 3)
            self.assertEqual(digest, next(record["sha256"] for record in initial["assets"] if record["name"] == name))


class ArchiveBoundsTests(unittest.TestCase):
    def test_archive_path_traversal_is_rejected_before_extraction(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "unsafe.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                info = tarfile.TarInfo("../escape")
                info.size = 1
                bundle.addfile(info, fileobj=__import__("io").BytesIO(b"x"))
            with self.assertRaisesRegex(release_manifest.ReleaseManifestError, "unsafe path"):
                release_manifest._extract_tar(
                    archive,
                    root / "stage",
                    max_members=10,
                    max_member_bytes=10,
                    max_expanded_bytes=10,
                )

    def test_common_dot_root_tar_member_is_ignored_safely(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "dot-root.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                root_info = tarfile.TarInfo("./")
                root_info.type = tarfile.DIRTYPE
                bundle.addfile(root_info)
                info = tarfile.TarInfo("./payload.txt")
                info.size = 1
                bundle.addfile(info, fileobj=__import__("io").BytesIO(b"x"))
            destination = root / "stage"
            destination.mkdir()
            release_manifest._extract_tar(
                archive,
                destination,
                max_members=10,
                max_member_bytes=10,
                max_expanded_bytes=10,
            )
            self.assertEqual((destination / "payload.txt").read_bytes(), b"x")


class GenerateSbomTests(unittest.TestCase):
    @unittest.skipIf(os.name == "nt", "fixture uses a POSIX executable Syft stub")
    def test_generate_sboms_scans_extracted_payload_and_writes_sidecars(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "dist"
            root.mkdir()
            payload_name = "artifact.whl"
            with __import__("zipfile").ZipFile(root / payload_name, "w") as archive:
                archive.writestr("package/data.txt", "fixture")
            syft = Path(directory) / "syft-stub"
            syft.write_text(
                "#!/usr/bin/env python3\n"
                "import json, pathlib, sys\n"
                "for argument in sys.argv:\n"
                "    if argument.startswith('spdx-json='):\n"
                "        pathlib.Path(argument.split('=', 1)[1]).write_text(json.dumps({'spdxVersion': 'SPDX-2.3', 'creationInfo': {'created': '2020-01-01T00:00:00Z'}}))\n"
                "    if argument.startswith('cyclonedx-json='):\n"
                "        pathlib.Path(argument.split('=', 1)[1]).write_text(json.dumps({'bomFormat': 'CycloneDX', 'metadata': {'timestamp': '2020-01-01T00:00:00Z'}}))\n",
                encoding="utf-8",
            )
            syft.chmod(0o755)
            spec = release_manifest.AssetSpec(payload_name, "python-wheel", sbom=True)
            generated = release_manifest.generate_sboms(
                root,
                version=VERSION,
                specs=(spec,),
                syft=str(syft),
                source_date_epoch=EPOCH,
                max_expanded_bytes=1024,
            )
            self.assertEqual(
                generated,
                (f"{payload_name}.spdx.json", f"{payload_name}.cdx.json"),
            )
            release_manifest.finalize_release(
                root, version=VERSION, specs=(spec,), source_date_epoch=EPOCH
            )


if __name__ == "__main__":
    unittest.main()
