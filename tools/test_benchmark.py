import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("benchmark.py")
SPEC = importlib.util.spec_from_file_location("forge_benchmark", MODULE_PATH)
benchmark = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(benchmark)


class BenchmarkTests(unittest.TestCase):
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
        self.assertEqual(schema_cases, list(benchmark.DEFAULT_CASES))
        for case_id in benchmark.DEFAULT_CASES:
            self.assertTrue(benchmark.sanitized_command(case_id))
            self.assertEqual(len(benchmark.case_spec(case_id)), 4)


if __name__ == "__main__":
    unittest.main()
