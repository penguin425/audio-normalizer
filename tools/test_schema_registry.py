from __future__ import annotations

import copy
import importlib.util
import io
import json
import shutil
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CHECKER_PATH = Path(__file__).with_name("check-schema-registry.py")
SPEC = importlib.util.spec_from_file_location("check_schema_registry", CHECKER_PATH)
assert SPEC is not None and SPEC.loader is not None
CHECKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECKER)


class SchemaRegistryCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="forge-schema-registry-")
        self.root = Path(self.temporary.name)
        shutil.copytree(PROJECT_ROOT / "schema", self.root / "schema")
        self.registry_path = self.root / "schema/schema-registry-v1.json"

        registry = self.load_registry()
        references = {
            reference
            for entry in registry["entries"]
            for field in ("producers", "consumers", "validators", "samples")
            for reference in entry.get(field, [])
            if reference.startswith("repo:")
        }
        for reference in references:
            relative = reference.removeprefix("repo:")
            destination = self.root / relative
            if destination.exists():
                continue
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.touch()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def load_registry(self) -> dict[str, object]:
        return json.loads(self.registry_path.read_text(encoding="utf-8"))

    def write_registry(self, registry: dict[str, object]) -> None:
        self.registry_path.write_text(
            json.dumps(registry, indent=2) + "\n", encoding="utf-8"
        )

    def run_checker(self) -> str:
        with (
            mock.patch.object(CHECKER, "REPO_ROOT", self.root),
            mock.patch.object(CHECKER, "SCHEMA_ROOT", self.root / "schema"),
            mock.patch.object(CHECKER, "REGISTRY_PATH", self.registry_path),
            redirect_stdout(io.StringIO()),
        ):
            with self.assertRaises(SystemExit) as raised:
                CHECKER.main()
        return str(raised.exception)

    def test_current_registry_passes(self) -> None:
        with (
            mock.patch.object(CHECKER, "REPO_ROOT", self.root),
            mock.patch.object(CHECKER, "SCHEMA_ROOT", self.root / "schema"),
            mock.patch.object(CHECKER, "REGISTRY_PATH", self.registry_path),
            redirect_stdout(io.StringIO()),
        ):
            CHECKER.main()

    def test_rejects_missing_self_registration(self) -> None:
        registry = self.load_registry()
        registry["entries"] = [
            entry
            for entry in registry["entries"]
            if entry["path"] != "schema/schema-registry-v1.json"
        ]
        self.write_registry(registry)
        self.assertIn("registry coverage mismatch", self.run_checker())

    def test_rejects_unregistered_document(self) -> None:
        shutil.copyfile(
            self.root / "schema/service-health-v1.schema.json",
            self.root / "schema/unregistered-v1.schema.json",
        )
        self.assertIn("unregistered=", self.run_checker())

    def test_rejects_duplicate_paths(self) -> None:
        registry = self.load_registry()
        registry["entries"].append(copy.deepcopy(registry["entries"][-1]))
        self.write_registry(registry)
        self.assertIn("registered paths must be unique", self.run_checker())

    def test_rejects_unsorted_paths(self) -> None:
        registry = self.load_registry()
        registry["entries"][0], registry["entries"][1] = (
            registry["entries"][1],
            registry["entries"][0],
        )
        self.write_registry(registry)
        self.assertIn("entries must be sorted by path", self.run_checker())

    def test_rejects_new_legacy_identifier(self) -> None:
        registry = self.load_registry()
        target_path = "schema/audio-comparison-v1.schema.json"
        target = next(
            entry for entry in registry["entries"] if entry["path"] == target_path
        )
        changed_id = (
            "https://penguin425.github.io/audio-normalizer/schema/audio-comparison-v1"
        )
        target["document_id"] = changed_id
        schema_path = self.root / target_path
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        schema["$id"] = changed_id
        schema_path.write_text(json.dumps(schema), encoding="utf-8")
        self.write_registry(registry)
        self.assertIn("changes its published legacy document ID", self.run_checker())

    def test_rejects_removing_a_published_legacy_contract(self) -> None:
        registry = self.load_registry()
        target_path = "schema/audio-comparison-v1.schema.json"
        registry["entries"] = [
            entry for entry in registry["entries"] if entry["path"] != target_path
        ]
        (self.root / target_path).unlink()
        self.write_registry(registry)
        self.assertIn(
            "published legacy contracts are no longer registered", self.run_checker()
        )

    def test_rejects_broken_local_reference(self) -> None:
        path = self.root / "schema/service-health-v1.schema.json"
        schema = json.loads(path.read_text(encoding="utf-8"))
        schema["allOf"] = [{"$ref": "#/$defs/does-not-exist"}]
        path.write_text(json.dumps(schema), encoding="utf-8")
        self.assertIn("unresolved local $ref", self.run_checker())

    def test_rejects_external_reference(self) -> None:
        path = self.root / "schema/service-health-v1.schema.json"
        schema = json.loads(path.read_text(encoding="utf-8"))
        schema["allOf"] = [{"$ref": "https://example.invalid/external-v1"}]
        path.write_text(json.dumps(schema), encoding="utf-8")
        self.assertIn("non-local JSON Pointer $ref", self.run_checker())

    def test_rejects_missing_sample(self) -> None:
        registry = self.load_registry()
        target = next(
            entry
            for entry in registry["entries"]
            if entry["path"] == "schema/service-health-v1.schema.json"
        )
        target["samples"] = ["repo:tests/fixtures/missing-service-health-v1.json"]
        self.write_registry(registry)
        self.assertIn("sample does not name a file", self.run_checker())

    def test_rejects_path_traversal(self) -> None:
        registry = self.load_registry()
        registry["entries"][0]["path"] = "schema/../escape-v1.schema.json"
        self.write_registry(registry)
        self.assertIn("registered path escapes schema/", self.run_checker())

    def test_rejects_missing_successor(self) -> None:
        registry = self.load_registry()
        target = next(
            entry
            for entry in registry["entries"]
            if entry["path"] == "schema/batch-job-v1.schema.json"
        )
        target["successor_path"] = "schema/missing-batch-job-v2.schema.json"
        self.write_registry(registry)
        self.assertIn("must name a registered successor", self.run_checker())

    def test_rejects_cross_family_successor(self) -> None:
        registry = self.load_registry()
        target = next(
            entry
            for entry in registry["entries"]
            if entry["path"] == "schema/batch-job-v1.schema.json"
        )
        target["successor_path"] = "schema/service-analysis-v3.schema.json"
        self.write_registry(registry)
        self.assertIn("successor must belong to the same family", self.run_checker())

    def test_rejects_non_increasing_successor_version(self) -> None:
        registry = self.load_registry()
        target = next(
            entry
            for entry in registry["entries"]
            if entry["path"] == "schema/service-analysis-v2.schema.json"
        )
        target["successor_path"] = "schema/service-analysis-v1.schema.json"
        self.write_registry(registry)
        self.assertIn("successor version must increase", self.run_checker())


if __name__ == "__main__":
    unittest.main()
