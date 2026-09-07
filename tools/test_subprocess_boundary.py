from __future__ import annotations

import importlib.util
import re
import sys
import tempfile
import unittest
from pathlib import Path


TOOLS = Path(__file__).parent
CHECKER_PATH = TOOLS / "check-subprocess-boundary.py"
REPOSITORY = CHECKER_PATH.parents[1]
SPEC = importlib.util.spec_from_file_location("check_subprocess_boundary", CHECKER_PATH)
assert SPEC is not None and SPEC.loader is not None
checker = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = checker
SPEC.loader.exec_module(checker)


BUILD_SOURCE = """\
fn main() {
    let _ = std::process::Command::new("pkg-config");
}
"""

DECODER_SOURCE = """\
#[cfg(test)]
mod tests {
    fn encode_fixture() {
        let _ = std::process::Command::new("ffmpeg");
    }
}
"""

MXF_SOURCE = """\
#[cfg(test)]
mod tests {
    use std::process::Command;

    fn first() { let _ = Command::new("ffmpeg"); }
    fn second() { let _ = Command::new("ffmpeg"); }
    fn third() { let _ = Command::new("ffmpeg"); }
    fn fourth() { let _ = Command::new("ffmpeg"); }
}
"""


class SubprocessBoundaryCheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="forge-subprocess-boundary-")
        self.root = Path(self.temporary.name)
        (self.root / "src").mkdir()
        self.write("build.rs", BUILD_SOURCE)
        self.write("src/subprocess.rs", "use std::process::Command;\n")
        self.write("src/decoder.rs", DECODER_SOURCE)
        self.write("src/mxf_qc.rs", MXF_SOURCE)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, source: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding="utf-8")

    def check(self) -> tuple[object, ...]:
        return checker.check_repository(self.root)

    def assert_boundary_error(self, text: str) -> None:
        with self.assertRaises(checker.BoundaryError) as raised:
            self.check()
        self.assertIn(text, str(raised.exception))

    def test_exact_legacy_fixture_allowlist_is_accepted(self) -> None:
        scans = self.check()
        self.assertEqual(
            {
                scan.relative_path: len(scan.findings)
                for scan in scans
                if scan.relative_path in checker.ALLOWLIST
            },
            {"build.rs": 1, "src/decoder.rs": 1, "src/mxf_qc.rs": 5},
        )

    def test_allowlist_fingerprint_rejects_changed_constructor_argument(self) -> None:
        self.write("build.rs", BUILD_SOURCE.replace('"pkg-config"', '"curl"'))
        self.assert_boundary_error("fingerprint mismatch")

    def test_comments_and_literals_are_not_process_executions(self) -> None:
        self.write(
            "src/ordinary.rs",
            r'''
            // std::process::Command::new("comment")
            fn text() {
                let _ = "Command::new(std::process::Command)";
                let _ = r###"std::process::Command::new(\"raw\")"###;
            }
            ''',
        )
        self.check()

    def test_production_source_cannot_escape_to_direct_command(self) -> None:
        self.write(
            "src/ordinary.rs",
            'fn run() { let _ = std::process::Command::new("tool"); }\n',
        )
        self.assert_boundary_error("src/ordinary.rs:1:20")

    def test_test_only_allowlist_rejects_a_production_escape(self) -> None:
        self.write(
            "src/decoder.rs",
            DECODER_SOURCE
            + 'fn production() { let _ = std::process::Command::new("tool"); }\n',
        )
        self.assert_boundary_error("outside an exact #[cfg(test)] exception")

    def test_import_and_command_aliases_are_rejected(self) -> None:
        fixtures = {
            "import_alias.rs": (
                'use std::process::Command as ProcessCommand;\n'
                'fn run() { let _ = ProcessCommand::new("tool"); }\n'
            ),
            "module_alias.rs": (
                'use std::process as process;\n'
                'fn run() { let _ = process::Command::new("tool"); }\n'
            ),
            "root_alias.rs": (
                'use std as system;\n'
                'fn run() { let _ = system::process::Command::new("tool"); }\n'
            ),
            "extern_root_alias.rs": (
                'extern crate std as system;\n'
                'fn run() { let _ = system::process::Command::new("tool"); }\n'
            ),
            "grouped_alias.rs": (
                'use std::{process::{Command as ProcessCommand}};\n'
                'fn run() { let _ = ProcessCommand::new("tool"); }\n'
            ),
            "chained_alias.rs": (
                'use std as system;\n'
                'use system::process as process;\n'
                'fn run() { let _ = process::Command::new("tool"); }\n'
            ),
            "reverse_chained_alias.rs": (
                'use system::process as process;\n'
                'use std as system;\n'
                'fn run() { let _ = process::Command::new("tool"); }\n'
            ),
        }
        for name, source in fixtures.items():
            with self.subTest(name=name):
                self.write(f"src/{name}", source)
                self.assert_boundary_error(f"src/{name}")
                (self.root / "src" / name).unlink()

    def test_raw_identifiers_and_relative_aliases_are_rejected(self) -> None:
        fixtures = {
            "raw_identifier.rs": (
                'fn run() { let _ = r#std::r#process::r#Command::new("tool"); }\n'
            ),
            "raw_alias.rs": (
                'use r#std as r#system;\n'
                'use self::r#system::r#process as r#process_mod;\n'
                'fn run() { let _ = self::r#process_mod::r#Command::new("tool"); }\n'
            ),
            "crate_alias.rs": (
                'use std as system;\n'
                'use crate::system as local_system;\n'
                'fn run() { let _ = crate::local_system::process::Command::new("tool"); }\n'
            ),
            "super_alias.rs": (
                'use std as system;\n'
                'use super::system as parent_system;\n'
                'fn run() { let _ = super::parent_system::process::Command::new("tool"); }\n'
            ),
        }
        for name, source in fixtures.items():
            with self.subTest(name=name):
                self.write(f"src/{name}", source)
                self.assert_boundary_error(f"src/{name}")
                (self.root / "src" / name).unlink()

    def test_macro_path_interpolation_and_ufcs_are_rejected(self) -> None:
        self.write(
            "src/macro_and_ufcs.rs",
            """
            use std::process as process;
            use std::process::Command as C;
            macro_rules! process_path {
                ($p:ident) => { std::$p::Command::new("tool") };
            }
            macro_rules! command_path {
                ($c:ident) => { std::process::$c::new("tool") };
            }
            fn run() {
                let _ = process_path!(process);
                let _ = command_path!(C);
                let _ = <std::process::Command>::new("tool");
            }
            """,
        )
        with self.assertRaises(checker.BoundaryError) as raised:
            self.check()
        message = str(raised.exception)
        self.assertIn("macro-process-command-new", message)
        self.assertIn("macro-command-new", message)
        self.assertIn("ufcs-command-new", message)

        (self.root / "src" / "macro_and_ufcs.rs").write_text(
            """
            macro_rules! process_path {
                ($p:ident) => { std::$p::Command::new("tool") };
            }
            macro_rules! command_path {
                ($c:ident) => { std::process::$c::new("tool") };
            }
            fn run() {
                let _ = process_path!(other_process);
                let _ = command_path!(OtherCommand);
                let _ = <OtherCommand>::new("tool");
            }
            """,
            encoding="utf-8",
        )
        self.check()

    def test_macro_root_parameter_is_rejected_when_called_with_std(self) -> None:
        self.write(
            "src/macro_root.rs",
            """
            macro_rules! spawn {
                ($p:ident) => { $p::process::Command::new("tool") };
            }
            fn run() { let _ = spawn!(std); }
            """,
        )
        self.assert_boundary_error("macro-qualified-command-new")

    def test_macro_command_parameter_is_rejected_but_unrelated_path_is_allowed(self) -> None:
        self.write(
            "src/macro_command.rs",
            """
            use std::process::Command;
            macro_rules! make {
                ($c:path) => { $c::new("tool") };
            }
            fn run() { let _ = make!(Command); }
            """,
        )
        self.assert_boundary_error("macro-command-new")

        (self.root / "src" / "macro_command.rs").write_text(
            """
            macro_rules! make {
                ($c:path) => { $c::new("tool") };
            }
            fn run() { let _ = make!(Widget); }
            """,
            encoding="utf-8",
        )
        self.check()

    def test_allowlist_is_stale_when_an_expected_site_disappears(self) -> None:
        self.write(
            "src/mxf_qc.rs",
            MXF_SOURCE.replace('fn fourth() { let _ = Command::new("ffmpeg"); }\n', ""),
        )
        self.assert_boundary_error("stale subprocess exception allowlist")

    def test_allowlist_is_stale_when_an_exception_file_disappears(self) -> None:
        (self.root / "src" / "mxf_qc.rs").unlink()
        self.assert_boundary_error("missing source file(s): src/mxf_qc.rs")

    def test_allowlist_rejects_a_broad_or_unknown_entry(self) -> None:
        broad = dict(checker.ALLOWLIST)
        broad["src/ordinary.rs"] = checker.AllowRule(
            expected_kinds=(("qualified-command-new", 1),),
            test_only=False,
        )
        with self.assertRaisesRegex(checker.BoundaryError, "too broad"):
            checker.check_repository(self.root, allowlist=broad)

    def test_cfg_test_requires_the_exact_attribute(self) -> None:
        self.write(
            "src/decoder.rs",
            DECODER_SOURCE.replace("#[cfg(test)]", "#[cfg(any(test, feature = \"fixtures\"))]"),
        )
        self.assert_boundary_error("outside an exact #[cfg(test)] exception")

    def test_cli_root_option_reports_success_for_a_fixture(self) -> None:
        self.assertEqual(checker.main(["--root", str(self.root)]), 0)

    def test_release_workflow_tracks_boundary_and_readiness_contract(self) -> None:
        workflow = (REPOSITORY / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        readiness = (REPOSITORY / "tools/check-release-readiness.sh").read_text(
            encoding="utf-8"
        )
        for path in (
            "tools/check-subprocess-boundary.py",
            "tools/test_subprocess_boundary.py",
            "tools/check-release-readiness.sh",
        ):
            self.assertRegex(
                workflow,
                re.compile(rf'^\s*-\s+"?{re.escape(path)}"?\s*$', re.MULTILINE),
                msg=f"release pull-request paths omit {path}",
            )
        self.assertIn(
            'run: tools/check-release-readiness.sh "${{ steps.version.outputs.version }}"',
            workflow,
        )
        self.assertRegex(
            readiness,
            re.compile(r"^python3 tools/check-subprocess-boundary\.py\s*$", re.MULTILINE),
        )
        self.assertRegex(
            readiness,
            re.compile(
                r"^python3 -m unittest tools/test_subprocess_boundary\.py\s*$",
                re.MULTILINE,
            ),
        )


if __name__ == "__main__":
    unittest.main()
