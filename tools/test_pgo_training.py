import importlib.util
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("train-pgo.py")
SPEC = importlib.util.spec_from_file_location("forge_pgo_training", MODULE_PATH)
training = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(training)


class PgoTrainingTests(unittest.TestCase):
    def test_profile_directory_must_be_empty(self):
        with tempfile.TemporaryDirectory() as directory:
            profile_dir = Path(directory) / "profiles"
            self.assertEqual(
                training.prepare_profile_directory(profile_dir),
                profile_dir.resolve(),
            )
            (profile_dir / "stale.profraw").write_bytes(b"stale")
            with self.assertRaises(ValueError):
                training.prepare_profile_directory(profile_dir)

    def test_training_plan_is_serial_and_covers_representative_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixtures = training.create_fixtures(root, 1, 2)
            plan = training.training_plan(
                Path("/tmp/forge"), root, fixtures, include_opus=True
            )
            labels = [case.label for case in plan]
            self.assertEqual(len(labels), len(set(labels)))
            self.assertIn("wav-96k-analyze", labels)
            self.assertIn("wav-96k-normalize", labels)
            self.assertIn("wav-verify", labels)
            self.assertIn("wav-resample", labels)
            self.assertIn("wav-dither", labels)
            self.assertIn("wav-limiter", labels)
            self.assertIn("wav-to-flac-verify", labels)
            self.assertIn("flac-normalize", labels)
            self.assertIn("surround-normalize", labels)
            self.assertIn("batch-normalize", labels)
            self.assertIn("album-normalize", labels)
            self.assertIn("cache-miss-normalize", labels)
            self.assertIn("cache-hit-normalize", labels)
            self.assertIn("wav-to-opus", labels)
            self.assertIn("opus-analyze", labels)
            self.assertIn("opus-normalize", labels)
            for case in plan:
                jobs_index = case.command.index("--jobs")
                self.assertEqual(case.command[jobs_index + 1], "1")

    def test_opus_training_is_opt_in(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixtures = training.create_fixtures(root, 1, 2)
            labels = [
                case.label
                for case in training.training_plan(Path("/tmp/forge"), root, fixtures)
            ]
            self.assertFalse(any("opus" in label for label in labels))

    def test_fixtures_are_bounded_and_deterministic(self):
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            first_fixtures = training.create_fixtures(Path(first), 1, 2)
            second_fixtures = training.create_fixtures(Path(second), 1, 2)
            self.assertEqual(
                first_fixtures["stereo"].read_bytes(),
                second_fixtures["stereo"].read_bytes(),
            )
            self.assertEqual(
                first_fixtures["stereo_high_rate"].read_bytes(),
                second_fixtures["stereo_high_rate"].read_bytes(),
            )
            self.assertEqual(first_fixtures["stereo"].stat().st_size, 192_044)
            self.assertEqual(
                first_fixtures["stereo_high_rate"].stat().st_size, 384_044
            )
            self.assertEqual(first_fixtures["surround"].stat().st_size, 768_044)
            self.assertEqual(len(first_fixtures["tracks"]), 2)


if __name__ == "__main__":
    unittest.main()
