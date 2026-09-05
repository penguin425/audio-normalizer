import importlib.util
import json
import struct
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).with_name("benchmark.py")
SPEC = importlib.util.spec_from_file_location("forge_benchmark", MODULE_PATH)
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)
sys.modules["benchmark"] = benchmark

PAIRED_MODULE_PATH = Path(__file__).with_name("paired_benchmark.py")
PAIRED_SPEC = importlib.util.spec_from_file_location(
    "forge_paired_benchmark", PAIRED_MODULE_PATH
)
paired_benchmark = importlib.util.module_from_spec(PAIRED_SPEC)
assert PAIRED_SPEC.loader is not None
PAIRED_SPEC.loader.exec_module(paired_benchmark)


class BenchmarkTests(unittest.TestCase):
    def test_paired_schedule_balances_each_small_block_and_starting_order(self):
        schedule = paired_benchmark.alternating_schedule(4)
        self.assertEqual(len(schedule), 16)
        self.assertEqual(schedule.count(paired_benchmark.BASELINE), 8)
        self.assertEqual(schedule.count(paired_benchmark.CANDIDATE), 8)
        self.assertEqual(
            schedule[:4],
            [
                paired_benchmark.BASELINE,
                paired_benchmark.CANDIDATE,
                paired_benchmark.CANDIDATE,
                paired_benchmark.BASELINE,
            ],
        )
        self.assertEqual(
            schedule[4:8],
            [
                paired_benchmark.CANDIDATE,
                paired_benchmark.BASELINE,
                paired_benchmark.BASELINE,
                paired_benchmark.CANDIDATE,
            ],
        )
        self.assertEqual(
            paired_benchmark.alternating_schedule(1, inverted=True),
            schedule[4:8],
        )

    def test_paired_case_uses_two_balanced_unmeasured_warmup_blocks(self):
        binaries = {
            paired_benchmark.BASELINE: Path("/baseline-forge"),
            paired_benchmark.CANDIDATE: Path("/candidate-forge"),
        }
        labels_by_binary = {str(path): label for label, path in binaries.items()}
        self.assertEqual(paired_benchmark.WARMUP_ROUNDS, 2)

        for case_index, case_id in enumerate(paired_benchmark.CASES):
            with self.subTest(case_id=case_id):
                inverted = case_index % 2 == 1
                expected_warmup = paired_benchmark.alternating_schedule(
                    2, inverted=inverted
                )
                expected_measured = paired_benchmark.alternating_schedule(
                    1, inverted=inverted
                )
                warmup_labels = []
                measured_labels = []

                def record_warmup(command, _output_path, _timeout_seconds):
                    warmup_labels.append(command[0])

                def record_measurement(
                    command, _timeout_seconds, _stdout_path, _stderr_path
                ):
                    measured_labels.append(command[0])
                    return {"exit_code": 0}

                with tempfile.TemporaryDirectory() as directory:
                    with mock.patch.object(
                        paired_benchmark, "run_warmup", side_effect=record_warmup
                    ), mock.patch.object(
                        paired_benchmark.benchmark,
                        "run_measured",
                        side_effect=record_measurement,
                    ):
                        result = paired_benchmark.run_case(
                            case_id,
                            case_index,
                            Path(directory),
                            binaries,
                            Path(directory) / "input.wav",
                            Path(directory) / "input.flac",
                            1,
                            60,
                        )

                self.assertEqual(len(warmup_labels), 8)
                self.assertEqual(
                    [labels_by_binary[path] for path in warmup_labels], expected_warmup
                )
                self.assertEqual(
                    [labels_by_binary[path] for path in measured_labels], expected_measured
                )
                self.assertEqual(result["schedule"], expected_measured)
                self.assertEqual(len(result["baseline_samples"]), 2)
                self.assertEqual(len(result["candidate_samples"]), 2)

    def test_paired_change_reduces_each_balanced_block_before_comparing(self):
        result = {
            "schedule": paired_benchmark.alternating_schedule(2),
            "baseline_samples": [
                {"wall_seconds": value} for value in (10.0, 10.0, 100.0, 100.0)
            ],
            "candidate_samples": [
                {"wall_seconds": value} for value in (11.0, 11.0, 101.0, 101.0)
            ],
        }
        changes = paired_benchmark.paired_round_changes(
            result, lambda sample: sample["wall_seconds"]
        )
        self.assertEqual(len(changes), 2)
        self.assertAlmostEqual(changes[0], 10.0)
        self.assertAlmostEqual(changes[1], 1.0)
        self.assertAlmostEqual(
            paired_benchmark.paired_median_change_percent(
                result, lambda sample: sample["wall_seconds"]
            ),
            5.5,
        )

    def test_paired_change_rejects_an_unbalanced_block(self):
        result = {
            "schedule": [
                paired_benchmark.BASELINE,
                paired_benchmark.BASELINE,
                paired_benchmark.BASELINE,
                paired_benchmark.CANDIDATE,
            ],
            "baseline_samples": [{"value": 1.0}] * 3,
            "candidate_samples": [{"value": 1.0}],
        }
        with self.assertRaisesRegex(ValueError, "not balanced"):
            paired_benchmark.paired_round_changes(
                result, lambda sample: sample["value"]
            )

    def test_repeated_measurements_use_medians_and_maximum_rss(self):
        summary = benchmark.aggregate_measurements([
            {
                "wall_seconds": 3.0, "user_cpu_seconds": 2.7,
                "system_cpu_seconds": 0.3, "cpu_percent": 100.0,
                "peak_rss_bytes": 90,
            },
            {
                "wall_seconds": 1.0, "user_cpu_seconds": 0.8,
                "system_cpu_seconds": 0.2, "cpu_percent": 100.0,
                "peak_rss_bytes": 120,
            },
            {
                "wall_seconds": 2.0, "user_cpu_seconds": 1.7,
                "system_cpu_seconds": 0.3, "cpu_percent": 100.0,
                "peak_rss_bytes": 100,
            },
        ])
        self.assertEqual(summary["wall_seconds"], 2.0)
        self.assertEqual(summary["user_cpu_seconds"], 1.7)
        self.assertEqual(summary["system_cpu_seconds"], 0.3)
        self.assertEqual(summary["cpu_percent"], 100.0)
        self.assertEqual(summary["peak_rss_bytes"], 120)

    def test_pcm_wave_size_and_header(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.wav"
            size = benchmark.write_pcm16_wave(path, 1, 8_000, 2)
            self.assertEqual(size, 44 + 8_000 * 2 * 2)
            self.assertEqual(path.read_bytes()[:12], b"RIFF$}\x00\x00WAVE")

    def test_active_limiter_wave_has_a_deterministic_full_scale_transient(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "active.wav"
            benchmark.write_pcm16_wave(
                path,
                2,
                48_000,
                2,
                full_scale_transient=True,
            )
            transient_offset = 44 + 48_000 * 2 * 2
            samples = struct.unpack("<hh", path.read_bytes()[transient_offset:transient_offset + 4])
            self.assertEqual(samples, (32_767, 32_767))

    def test_dsd_fixtures_have_bounded_deterministic_wrappers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dsf = root / "fixture.dsf"
            dff = root / "fixture.dff"
            dsf_size = benchmark.write_dsf(dsf, 1, 2)
            dff_size = benchmark.write_dsdiff(dff, 1, 2)
            self.assertEqual(dsf.read_bytes()[:4], b"DSD ")
            self.assertEqual(dff.read_bytes()[:4], b"FRM8")
            self.assertEqual(dsf_size, dsf.stat().st_size)
            self.assertEqual(dff_size, dff.stat().st_size)
            self.assertLess(dsf_size, 1_000_000)
            self.assertLess(dff_size, 1_000_000)

    def test_pathological_wave_is_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pathological.wav"
            size = benchmark.write_pathological_wave(path, 3)
            self.assertEqual(size, 68)
            self.assertEqual(path.read_bytes().count(b"JUNK"), 3)
        with self.assertRaises(ValueError):
            benchmark.write_pathological_wave(Path("unused"), 100_002)

    def test_baseline_comparison(self):
        result = {
            "system": {
                "os": "Linux", "architecture": "x86_64", "cpu_model": "CPU",
                "cpu_count": 8,
            },
            "configuration": {
                "duration_seconds": 1, "sample_rate_hz": 48_000,
                "pathological_chunks": 100_001, "iterations": 3,
                "cases": ["case"],
            },
            "results": [{
                "id": "case", "wall_seconds": 1.1, "peak_rss_bytes": 110,
                "regression": None,
            }],
            "error": None,
            "passed": True,
        }
        baseline = json.loads(json.dumps(result))
        baseline["results"][0]["wall_seconds"] = 1.0
        baseline["results"][0]["peak_rss_bytes"] = 100
        self.assertTrue(benchmark.compare_baseline(result, baseline, 15, 15))
        self.assertTrue(result["results"][0]["regression"]["passed"])

    def test_measured_timeout_kills_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaises(TimeoutError):
                benchmark.run_measured(
                    [sys.executable, "-c", "import time; time.sleep(5)"],
                    1,
                    root / "stdout.log",
                    root / "stderr.log",
                )

    def test_schema_is_valid_json_with_stable_id(self):
        schema_path = MODULE_PATH.parent.parent / "schema" / "performance-benchmark-v1.schema.json"
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        self.assertEqual(schema["$id"], benchmark.SCHEMA)
        schema_cases = schema["properties"]["configuration"]["properties"]["cases"]["items"]["enum"]
        self.assertEqual(schema_cases, list(benchmark.ALL_CASES))
        for case_id in benchmark.ALL_CASES:
            self.assertTrue(benchmark.sanitized_command(case_id))
            self.assertEqual(len(benchmark.case_spec(case_id)), 4)


if __name__ == "__main__":
    unittest.main()
