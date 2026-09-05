import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


TOOLS = Path(__file__).parent


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    sys.modules[name] = module
    return module


benchmark = load_module("benchmark", TOOLS / "benchmark.py")
paired_benchmark = load_module("paired_benchmark", TOOLS / "paired_benchmark.py")
pcm_gate = load_module("pcm_benchmark_gate", TOOLS / "pcm_benchmark_gate.py")


BASE_SHA = "a" * 40
HEAD_SHA = "b" * 40
BASE_VERSION = "1.0.0"
CANDIDATE_VERSION = "1.0.1"


class PcmBenchmarkGateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.initial = self.root / "initial"
        self.confirmation = self.root / "confirmation"
        self.baseline_root = self.root / "baseline"
        self.candidate_root = self.root / "candidate"
        self.initial.mkdir()
        self.confirmation.mkdir()
        self._write_source_root(self.baseline_root, BASE_VERSION)
        self._write_source_root(self.candidate_root, CANDIDATE_VERSION)

    def tearDown(self):
        self.temporary.cleanup()

    @staticmethod
    def _write_source_root(root, version, revision="forge-bs1770-5-r4"):
        (root / "src").mkdir(parents=True, exist_ok=True)
        (root / "Cargo.toml").write_text(
            f'[package]\nname = "forge-normalizer"\nversion = "{version}"\n',
            encoding="utf-8",
        )
        (root / "src/bound_analysis.rs").write_text(
            f'pub const MEASUREMENT_ALGORITHM_REVISION: &str = "{revision}";\n',
            encoding="utf-8",
        )

    @staticmethod
    def _sample(wall, cpu, rss=64 * 1024 * 1024):
        return {
            "exit_code": 0,
            "wall_seconds": wall,
            "user_cpu_seconds": cpu,
            "system_cpu_seconds": 0.0,
            "cpu_percent": cpu / wall * 100.0,
            "peak_rss_bytes": rss,
        }

    def _result(self, case, *, wall_factor=1.0, cpu_factor=1.0, rss_delta=0):
        schedule = paired_benchmark.alternating_schedule(
            pcm_gate.ROUNDS,
            inverted=paired_benchmark.CASES.index(case) % 2 == 1,
        )
        baseline = []
        candidate = []
        for label in schedule:
            if label == paired_benchmark.BASELINE:
                baseline.append(self._sample(100.0, 80.0))
            else:
                candidate.append(
                    self._sample(
                        100.0 * wall_factor,
                        80.0 * cpu_factor,
                        64 * 1024 * 1024 + rss_delta,
                    )
                )
        return {
            "id": case,
            "schedule": schedule,
            "samples_per_binary": pcm_gate.SAMPLES_PER_BINARY,
            "baseline_samples": baseline,
            "candidate_samples": candidate,
        }

    def _document(self, duration, cases, factors=None):
        factors = factors or {}
        results = []
        for case in cases:
            values = factors.get(case, {})
            results.append(
                self._result(
                    case,
                    wall_factor=values.get("wall", 1.0),
                    cpu_factor=values.get("cpu", 1.0),
                    rss_delta=values.get("rss_delta", 0),
                )
            )
        return {
            "generator": "forge-paired-benchmark/1",
            "system": {
                "os": "Linux",
                "architecture": "x86_64",
                "cpu_model": "test",
                "cpu_count": 4,
                "python_version": "3.test",
            },
            "configuration": {
                "duration_seconds": duration,
                "warmup_rounds": paired_benchmark.WARMUP_ROUNDS,
                "rounds": pcm_gate.ROUNDS,
                "samples_per_binary": pcm_gate.SAMPLES_PER_BINARY,
                "cases": list(cases),
            },
            "versions": {
                "baseline": f"forge {BASE_VERSION}",
                "candidate": f"forge {CANDIDATE_VERSION}",
            },
            "results": results,
            "error": None,
            "passed": True,
        }

    @staticmethod
    def _write_json(path, document):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(document) + "\n", encoding="utf-8")

    def _write_initial(self, factors=None):
        factors = factors or {}
        for length, duration in pcm_gate.LENGTHS.items():
            for mode in pcm_gate.MODES:
                document = self._document(
                    duration,
                    pcm_gate.CASES,
                    factors.get((length, mode), {}),
                )
                self._write_json(
                    self.initial / f"{length}-{mode}.json", document
                )

    def _build_plan(self):
        return pcm_gate.build_plan(
            self.initial,
            baseline_root=self.baseline_root,
            candidate_root=self.candidate_root,
            base_sha=BASE_SHA,
            head_sha=HEAD_SHA,
            runner_os="Linux",
            runner_arch="X64",
        )

    def _write_confirmation(self, plan, factors=None):
        factors = factors or {}
        for population in plan["confirmation_populations"]:
            document = self._document(
                population["duration_seconds"],
                population["cases"],
                factors.get((population["length"], population["mode"]), {}),
            )
            self._write_json(
                self.confirmation / population["output"], document
            )

    def _write_plan(self, plan):
        path = self.root / "plan.json"
        self._write_json(path, plan)
        return path

    def test_no_trip_requires_no_build_or_confirmation_population(self):
        self._write_initial()
        plan = self._build_plan()
        self.assertFalse(plan["confirmation_required"])
        self.assertEqual(plan["detectors"], [])
        self.assertEqual(plan["confirmation_populations"], [])
        self.assertTrue(plan["initial_memory_passed"])
        self.assertEqual(plan["schema"], pcm_gate.PLAN_SCHEMA)
        self.assertEqual(
            plan["source"], {"base_sha": BASE_SHA, "head_sha": HEAD_SHA}
        )
        self.assertEqual(
            plan["initial_runner"], {"os": "Linux", "arch": "X64"}
        )
        self.assertEqual(plan["configuration"]["warmup_rounds"], 2)
        self.assertEqual(plan["configuration"]["rounds"], 8)
        self.assertEqual(plan["configuration"]["samples_per_binary"], 16)

        plan_path = self._write_plan(plan)
        evidence = pcm_gate.build_confirmation_evidence(
            plan,
            plan_path,
            self.confirmation,
            runner_os="Linux",
            runner_arch="X64",
        )
        self.assertFalse(evidence["requested"])
        self.assertTrue(evidence["passed"])
        with mock.patch.object(pcm_gate.subprocess, "run") as run:
            pcm_gate.run_confirmations(
                plan,
                paired_script=self.root / "missing-paired-script",
                baseline_forge=self.root / "missing-baseline",
                candidate_forge=self.root / "missing-candidate",
                ffmpeg=self.root / "missing-ffmpeg",
                work_dir=self.root / "unused-work",
                confirmation_dir=self.confirmation,
            )
        run.assert_not_called()

    def test_normal_thresholds_remain_byte_for_byte_the_existing_budgets(self):
        context = pcm_gate.threshold_context(
            self.baseline_root, self.candidate_root
        )
        self.assertEqual(
            context["limits_percent"],
            {
                "short_control_cpu": 4.0,
                "short_resample_wall": 4.0,
                "short_resample_cpu": 3.0,
                "short_average": 1.0,
                "short_paired_wall": 10.0,
                "short_paired_cpu": 8.0,
                "short_pooled": 20.0,
                "long_isolated": 5.0,
                "long_average": 3.0,
                "long_pooled": 15.0,
            },
        )

    def test_approved_migration_thresholds_remain_unchanged(self):
        self._write_source_root(
            self.baseline_root, BASE_VERSION, "forge-bs1770-5-r3"
        )
        measurement = pcm_gate.threshold_context(
            self.baseline_root, self.candidate_root
        )["limits_percent"]
        self.assertEqual(
            measurement,
            {
                "short_control_cpu": 8.0,
                "short_resample_wall": 8.0,
                "short_resample_cpu": 8.0,
                "short_average": 5.0,
                "short_paired_wall": 10.0,
                "short_paired_cpu": 8.0,
                "short_pooled": 20.0,
                "long_isolated": 8.0,
                "long_average": 5.0,
                "long_pooled": 20.0,
            },
        )

        self._write_source_root(
            self.baseline_root, "0.189.5", "forge-bs1770-5-r4"
        )
        self._write_source_root(
            self.candidate_root, "0.189.6", "forge-bs1770-5-r4"
        )
        durability = pcm_gate.threshold_context(
            self.baseline_root, self.candidate_root
        )["limits_percent"]
        self.assertEqual(
            durability,
            {
                "short_control_cpu": 5.0,
                "short_resample_wall": 5.0,
                "short_resample_cpu": 4.0,
                "short_average": 2.0,
                "short_paired_wall": 10.0,
                "short_paired_cpu": 8.0,
                "short_pooled": 20.0,
                "long_isolated": 8.0,
                "long_average": 3.0,
                "long_pooled": 15.0,
            },
        )

    def test_case_confirmation_preserves_identity_and_same_limit(self):
        self._write_initial({
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.06}
            }
        })
        plan = self._build_plan()
        detectors = [
            detector
            for detector in plan["detectors"]
            if detector["length"] == "long"
        ]
        self.assertEqual(len(detectors), 1)
        detector = detectors[0]
        self.assertEqual(
            (
                detector["mode"],
                detector["case"],
                detector["statistic"],
                detector["metric"],
                detector["limit_percent"],
            ),
            (
                "default",
                "wav-stereo-resample-normalize",
                "paired",
                "wall",
                5.0,
            ),
        )
        self.assertEqual(
            plan["confirmation_populations"][0]["cases"],
            ["wav-stereo-resample-normalize"],
        )

        self._write_confirmation(plan, {
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.04}
            }
        })
        evidence = pcm_gate.build_confirmation_evidence(
            plan,
            self._write_plan(plan),
            self.confirmation,
            runner_os="Linux",
            runner_arch="X64",
        )
        self.assertFalse(evidence["detectors"][0]["reproduced"])
        self.assertTrue(evidence["passed"])
        pcm_gate.enforce_fresh_evidence(plan, evidence)

    def test_same_case_and_statistic_must_exceed_twice(self):
        self._write_initial({
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.06}
            }
        })
        plan = self._build_plan()
        self._write_confirmation(plan, {
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.07, "cpu": 1.50}
            }
        })
        evidence = pcm_gate.build_confirmation_evidence(
            plan,
            self._write_plan(plan),
            self.confirmation,
            runner_os="Linux",
            runner_arch="X64",
        )
        self.assertEqual(len(evidence["detectors"]), 1)
        self.assertTrue(evidence["detectors"][0]["reproduced"])
        self.assertFalse(evidence["passed"])
        with self.assertRaisesRegex(RuntimeError, "timing regression reproduced"):
            pcm_gate.enforce_fresh_evidence(plan, evidence)

    def test_long_pooled_trip_is_no_longer_an_unconfirmed_assert(self):
        self._write_initial({
            ("long", "one"): {
                "flac-stereo-normalize": {"wall": 1.16}
            }
        })
        plan = self._build_plan()
        pooled = [
            detector
            for detector in plan["detectors"]
            if detector["length"] == "long"
            and detector["mode"] == "one"
            and detector["case"] == "flac-stereo-normalize"
            and detector["statistic"] == "pooled"
            and detector["metric"] == "wall"
        ]
        self.assertEqual(len(pooled), 1)
        self.assertEqual(pooled[0]["limit_percent"], 15.0)
        self.assertIn(
            "flac-stereo-normalize",
            next(
                population
                for population in plan["confirmation_populations"]
                if population["length"] == "long" and population["mode"] == "one"
            )["cases"],
        )

        self._write_confirmation(plan, {
            ("long", "one"): {
                "flac-stereo-normalize": {"wall": 1.14}
            }
        })
        evidence = pcm_gate.build_confirmation_evidence(
            plan,
            self._write_plan(plan),
            self.confirmation,
            runner_os="Linux",
            runner_arch="X64",
        )
        pooled_evidence = next(
            entry
            for entry in evidence["detectors"]
            if entry["statistic"] == "pooled"
            and entry["metric"] == "wall"
            and entry["case"] == "flac-stereo-normalize"
        )
        self.assertFalse(pooled_evidence["reproduced"])

    def test_aggregate_trip_requests_a_complete_fresh_population(self):
        factors = {}
        for mode in pcm_gate.MODES:
            factors[("short", mode)] = {
                case: {"wall": 1.02} for case in pcm_gate.CASES
            }
        self._write_initial(factors)
        plan = self._build_plan()
        aggregate = [
            detector
            for detector in plan["detectors"]
            if detector["scope"] == "aggregate"
            and detector["length"] == "short"
            and detector["metric"] == "wall"
        ]
        self.assertEqual(len(aggregate), 1)
        populations = [
            population
            for population in plan["confirmation_populations"]
            if population["length"] == "short"
        ]
        self.assertEqual(len(populations), 2)
        self.assertTrue(
            all(
                population["cases"] == list(pcm_gate.CASES)
                for population in populations
            )
        )

    def test_rss_regression_remains_an_initial_only_gate(self):
        self._write_initial({
            ("short", "default"): {
                "flac-stereo-normalize": {
                    "rss_delta": pcm_gate.RSS_LIMIT_BYTES + 1
                }
            }
        })
        plan = self._build_plan()
        self.assertFalse(plan["initial_memory_passed"])
        failures = [check for check in plan["memory_checks"] if not check["passed"]]
        self.assertEqual(len(failures), 1)
        self.assertEqual(failures[0]["metric"], "median_rss_delta_bytes")
        with self.assertRaisesRegex(RuntimeError, "initial RSS regression"):
            pcm_gate.enforce_initial_memory(plan)
        with self.assertRaisesRegex(RuntimeError, "initial RSS regression"):
            pcm_gate.enforce_fresh_evidence(
                plan,
                {"detectors": [], "passed": True},
            )

    def test_long_gross_rss_ratchet_remains_an_initial_only_gate(self):
        self._write_initial()
        path = self.initial / "long-default.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        candidate = document["results"][0]["candidate_samples"]
        baseline_rss = document["results"][0]["baseline_samples"][0][
            "peak_rss_bytes"
        ]
        candidate[0]["peak_rss_bytes"] = (
            baseline_rss + pcm_gate.GROSS_RSS_LIMIT_BYTES + 1
        )
        candidate[1]["peak_rss_bytes"] = (
            baseline_rss + pcm_gate.GROSS_RSS_LIMIT_BYTES + 1
        )
        self._write_json(path, document)
        plan = self._build_plan()
        gross = [
            check
            for check in plan["memory_checks"]
            if check["metric"] == "gross_rss_samples" and not check["passed"]
        ]
        self.assertEqual(len(gross), 1)
        self.assertEqual(gross[0]["value"], 2)
        self.assertEqual(gross[0]["limit"], 1)

    def test_plan_validation_binds_source_runner_and_raw_hashes(self):
        self._write_initial()
        plan = self._build_plan()
        plan_path = self._write_plan(plan)
        validated = pcm_gate.validate_plan(
            plan_path,
            self.initial,
            baseline_root=self.baseline_root,
            candidate_root=self.candidate_root,
            base_sha=BASE_SHA,
            head_sha=HEAD_SHA,
            runner_os="Linux",
            runner_arch="X64",
        )
        self.assertEqual(validated, plan)
        with self.assertRaisesRegex(ValueError, "does not match"):
            pcm_gate.validate_plan(
                plan_path,
                self.initial,
                baseline_root=self.baseline_root,
                candidate_root=self.candidate_root,
                base_sha=BASE_SHA,
                head_sha="c" * 40,
                runner_os="Linux",
                runner_arch="X64",
            )
        raw_path = self.initial / "short-default.json"
        raw_path.write_text(raw_path.read_text(encoding="utf-8") + " ", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "does not match"):
            pcm_gate.validate_plan(
                plan_path,
                self.initial,
                baseline_root=self.baseline_root,
                candidate_root=self.candidate_root,
                base_sha=BASE_SHA,
                head_sha=HEAD_SHA,
                runner_os="Linux",
                runner_arch="X64",
            )

    def test_missing_nonfinite_and_unbalanced_reports_fail_closed(self):
        self._write_initial()
        (self.initial / "long-one.json").unlink()
        with self.assertRaises(FileNotFoundError):
            self._build_plan()

        self._write_initial()
        path = self.initial / "long-one.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        document["results"][0]["candidate_samples"][0]["wall_seconds"] = float("nan")
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "non-finite"):
            self._build_plan()

        self._write_initial()
        document = json.loads(path.read_text(encoding="utf-8"))
        document["results"][0]["candidate_samples"].pop()
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "incomplete candidate"):
            self._build_plan()

    def test_impossible_sample_shapes_fail_closed(self):
        self._write_initial()
        path = self.initial / "short-default.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        sample = document["results"][0]["candidate_samples"][0]
        sample["exit_code"] = False
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "exit code is not an integer"):
            self._build_plan()

        self._write_initial()
        document = json.loads(path.read_text(encoding="utf-8"))
        sample = document["results"][0]["candidate_samples"][0]
        sample["peak_rss_bytes"] = 1.5
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "peak RSS is not an integer"):
            self._build_plan()

        self._write_initial()
        document = json.loads(path.read_text(encoding="utf-8"))
        sample = document["results"][0]["candidate_samples"][0]
        sample["unexpected"] = 1
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "unexpected measurement fields"):
            self._build_plan()

    def test_bad_confirmation_configuration_fails_closed(self):
        self._write_initial({
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.06}
            }
        })
        plan = self._build_plan()
        self._write_confirmation(plan)
        path = self.confirmation / plan["confirmation_populations"][0]["output"]
        document = json.loads(path.read_text(encoding="utf-8"))
        document["configuration"]["rounds"] = 7
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "unexpected configuration"):
            pcm_gate.build_confirmation_evidence(
                plan,
                self._write_plan(plan),
                self.confirmation,
                runner_os="Linux",
                runner_arch="X64",
            )

    def test_confirmation_summary_is_bound_to_plan_and_raw_report_hashes(self):
        self._write_initial({
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.06}
            }
        })
        plan = self._build_plan()
        plan_path = self._write_plan(plan)
        self._write_confirmation(plan)
        evidence = pcm_gate.build_confirmation_evidence(
            plan,
            plan_path,
            self.confirmation,
            runner_os="Linux",
            runner_arch="X64",
        )
        evidence_path = self.root / "evidence.json"
        self._write_json(evidence_path, evidence)
        self.assertEqual(
            pcm_gate.validate_confirmation_evidence(
                plan,
                plan_path,
                self.confirmation,
                evidence_path,
                runner_os="Linux",
                runner_arch="X64",
            ),
            evidence,
        )
        raw = self.confirmation / plan["confirmation_populations"][0]["output"]
        raw.write_text(raw.read_text(encoding="utf-8") + " ", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "does not match raw reports"):
            pcm_gate.validate_confirmation_evidence(
                plan,
                plan_path,
                self.confirmation,
                evidence_path,
                runner_os="Linux",
                runner_arch="X64",
            )

    def test_missing_and_nonfinite_confirmation_reports_fail_closed(self):
        self._write_initial({
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.06}
            }
        })
        plan = self._build_plan()
        plan_path = self._write_plan(plan)
        with self.assertRaises(FileNotFoundError):
            pcm_gate.build_confirmation_evidence(
                plan,
                plan_path,
                self.confirmation,
                runner_os="Linux",
                runner_arch="X64",
            )

        self._write_confirmation(plan)
        path = self.confirmation / plan["confirmation_populations"][0]["output"]
        document = json.loads(path.read_text(encoding="utf-8"))
        document["results"][0]["baseline_samples"][0]["user_cpu_seconds"] = (
            float("inf")
        )
        self._write_json(path, document)
        with self.assertRaisesRegex(ValueError, "non-finite"):
            pcm_gate.build_confirmation_evidence(
                plan,
                plan_path,
                self.confirmation,
                runner_os="Linux",
                runner_arch="X64",
            )

    def test_confirmation_runner_invokes_only_requested_population(self):
        self._write_initial({
            ("long", "default"): {
                "wav-stereo-resample-normalize": {"wall": 1.06}
            }
        })
        plan = self._build_plan()
        paths = {}
        executables = self.root / "executables"
        executables.mkdir()
        for name in ("paired", "baseline", "candidate", "ffmpeg"):
            paths[name] = executables / name
            paths[name].touch()
        with mock.patch.object(pcm_gate.subprocess, "run") as run:
            pcm_gate.run_confirmations(
                plan,
                paired_script=paths["paired"],
                baseline_forge=paths["baseline"],
                candidate_forge=paths["candidate"],
                ffmpeg=paths["ffmpeg"],
                work_dir=self.root / "work",
                confirmation_dir=self.confirmation,
            )
        self.assertEqual(run.call_count, 1)
        command = run.call_args.args[0]
        self.assertIn("600", command)
        self.assertIn("8", command)
        case_positions = [index for index, value in enumerate(command) if value == "--case"]
        self.assertEqual(len(case_positions), 1)
        self.assertEqual(
            command[case_positions[0] + 1], "wav-stereo-resample-normalize"
        )


if __name__ == "__main__":
    unittest.main()
