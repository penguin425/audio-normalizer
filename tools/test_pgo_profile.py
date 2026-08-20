import importlib.util
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("canonicalize-pgo-profile.py")
SPEC = importlib.util.spec_from_file_location("forge_pgo_profile", MODULE_PATH)
profile = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(profile)


class PgoProfileTests(unittest.TestCase):
    def test_cold_functions_are_zeroed_without_changing_hot_functions(self):
        source = """# IR level Instrumentation Flag
:ir
cold
# Func Hash:
1
# Num Counters:
3
# Counter Values:
4
20
0

hot
# Func Hash:
2
# Num Counters:
3
# Counter Values:
4
10000
2

"""
        rendered, count, removed_value_profiles = profile.canonicalize(
            source, 10_000
        )
        self.assertEqual(count, 1)
        self.assertEqual(removed_value_profiles, 0)
        self.assertIn("cold\n# Func Hash:\n1\n# Num Counters:\n3\n# Counter Values:\n0\n0\n0\n", rendered)
        self.assertIn("hot\n# Func Hash:\n2\n# Num Counters:\n3\n# Counter Values:\n4\n10000\n2\n", rendered)

    def test_value_profile_payload_is_removed(self):
        source = """# IR level Instrumentation Flag
:ir
cold
# Func Hash:
1
# Num Counters:
1
# Counter Values:
3
# Num Value Kinds:
1
# ValueKind = IPVK_MemOPSize:
1
# NumValueSites:
1
1
8:3

next
# Func Hash:
2
# Num Counters:
1
# Counter Values:
10000

"""
        rendered, count, removed_value_profiles = profile.canonicalize(
            source, 10_000
        )
        self.assertEqual(count, 1)
        self.assertEqual(removed_value_profiles, 1)
        self.assertIn("# Counter Values:\n0\n", rendered)
        self.assertNotIn("# Num Value Kinds:", rendered)
        self.assertNotIn("# NumValueSites:", rendered)
        self.assertNotIn("8:3", rendered)
        self.assertIn("next\n# Func Hash:\n2\n", rendered)

    def test_rejects_non_ir_and_truncated_profiles(self):
        with self.assertRaises(ValueError):
            profile.canonicalize("not a profile\n", 10_000)
        with self.assertRaises(ValueError):
            profile.canonicalize(
                "# IR level Instrumentation Flag\n:ir\nname\n# Num Counters:\n1\n",
                10_000,
            )


if __name__ == "__main__":
    unittest.main()
