from __future__ import annotations

import importlib.util
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path

import yaml


TOOLS = Path(__file__).parent
REPOSITORY = TOOLS.parent


def load_script(module_name: str, filename: str):
    specification = importlib.util.spec_from_file_location(
        module_name,
        TOOLS / filename,
    )
    assert specification is not None and specification.loader is not None
    module = importlib.util.module_from_spec(specification)
    sys.modules[module_name] = module
    specification.loader.exec_module(module)
    return module


pins = load_script("check_workflow_pins", "check-workflow-pins.py")
runner = load_script("run_pinned_container", "run-pinned-container.py")


class WorkflowCheckerLockTests(unittest.TestCase):
    def test_checker_requires_the_locked_pyyaml_version(self) -> None:
        self.assertEqual(pins.REQUIRED_PYYAML_VERSION, "6.0.3")

    def test_lock_is_one_dependency_with_cross_platform_cp312_hashes(self) -> None:
        lock = (TOOLS / "workflow-check-requirements.lock").read_text(
            encoding="utf-8"
        )
        requirements = [
            line for line in lock.splitlines() if line and not line.startswith(("#", " "))
        ]
        self.assertEqual(requirements, ["PyYAML==6.0.3 \\"])
        hashes = re.findall(r"--hash=sha256:([0-9a-f]{64})", lock)
        self.assertEqual(len(hashes), 10)
        self.assertEqual(len(set(hashes)), 10)

    def test_missing_pyyaml_reports_the_hash_lock_without_traceback(self) -> None:
        environment = os.environ.copy()
        environment.pop("PYTHONPATH", None)
        completed = subprocess.run(
            [sys.executable, "-S", str(TOOLS / "check-workflow-pins.py")],
            capture_output=True,
            text=True,
            check=False,
            env=environment,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("workflow-check-requirements.lock", completed.stderr)
        self.assertNotIn("Traceback", completed.stderr)


class WorkflowPinTests(unittest.TestCase):
    def test_container_image_requires_sha256_digest(self) -> None:
        self.assertFalse(
            pins.container_image_is_pinned(
                "quay.io/pypa/manylinux_2_34_x86_64:latest"
            )
        )
        self.assertFalse(
            pins.container_image_is_pinned(
                "quay.io/pypa/manylinux_2_34_x86_64@sha256:short"
            )
        )

    def test_container_image_accepts_full_sha256_digest(self) -> None:
        digest = "9" * 64
        self.assertTrue(
            pins.container_image_is_pinned(
                f"quay.io/pypa/manylinux_2_34_x86_64@sha256:{digest}"
            )
        )

    def test_flow_mappings_at_all_workflow_levels_are_inspected(self) -> None:
        fixture = """
jobs: {test: {container: {image: ubuntu:24.04}, services: {database:
  {image: postgres:16}}, steps: [{uses: actions/checkout@v7}]}}
"""
        dependencies, images, violations = pins.scan_workflow_text(
            fixture, source="flow.yml"
        )
        self.assertEqual(dependencies, 1)
        self.assertEqual(images, 2)
        self.assertEqual(len(violations), 3)
        self.assertTrue(all(item.startswith("flow.yml:") for item in violations))

    def test_quoted_and_escaped_keys_cannot_bypass_structure_scan(self) -> None:
        fixture = r'''
"jobs":
  test:
    "cont\u0061iner": {"im\u0061ge": "ubuntu:24.04"}
    "serv\u0069ces": {db: {'image': 'postgres:16'}}
    steps:
      - "\u0075ses": "actions/checkout@v7"
'''
        dependencies, images, violations = pins.scan_workflow_text(
            fixture, source="escaped.yml"
        )
        self.assertEqual(dependencies, 1)
        self.assertEqual(images, 2)
        self.assertEqual(len(violations), 3)

    def test_pinned_structural_references_are_accepted(self) -> None:
        digest = "a" * 64
        commit = "b" * 40
        fixture = f"""
jobs:
  test:
    container: registry.example/forge@sha256:{digest}
    services:
      database:
        image: registry.example/database@sha256:{digest}
    steps:
      - uses: owner/action@{commit}
      - uses: docker://registry.example/action@sha256:{digest}
      - uses: ./local-action
"""
        dependencies, images, violations = pins.scan_workflow_text(fixture)
        self.assertEqual(dependencies, 2)
        self.assertEqual(images, 2)
        self.assertEqual(violations, [])

    def test_duplicate_security_keys_are_rejected(self) -> None:
        fixture = """
jobs:
  test:
    container: registry.example/first@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    container: ubuntu:24.04
"""
        _, _, violations = pins.scan_workflow_text(fixture)
        self.assertTrue(any("duplicate" in item for item in violations))

    def test_raw_shell_container_engine_cannot_hide_behind_image_name(self) -> None:
        for engine in (
            "docker",
            "docker-compose",
            "Docker",
            "DOCKER.EXE",
            "podman",
            "podman-compose",
            "Podman",
            "nerdctl",
            "NERDCTL",
        ):
            with self.subTest(engine=engine):
                fixture = f"""
jobs:
  test:
    steps:
      - run: {engine} run "$RENAMED_OR_COMPUTED_IMAGE" true
"""
                _, _, violations = pins.scan_workflow_text(fixture)
                self.assertTrue(any(f"raw {engine}" in item for item in violations))

    def test_pinned_container_helper_is_allowed_in_shell(self) -> None:
        fixture = """
jobs:
  test:
    steps:
      - run: python3 tools/run-pinned-container.py pull "$IMAGE"
"""
        _, _, violations = pins.scan_workflow_text(fixture)
        self.assertEqual(violations, [])

    def test_docker_action_image_requires_a_remote_digest(self) -> None:
        digest = "d" * 64
        for image in ("docker://ubuntu:latest", "Dockerfile", "./Dockerfile"):
            with self.subTest(image=image):
                fixture = f"runs: {{using: docker, image: {image}}}\n"
                _, images, violations = pins.scan_workflow_text(fixture)
                self.assertEqual(images, 1)
                self.assertEqual(len(violations), 1)

        fixture = (
            "runs:\n"
            "  using: docker\n"
            f"  image: docker://registry.example/action@sha256:{digest}\n"
        )
        _, images, violations = pins.scan_workflow_text(fixture)
        self.assertEqual(images, 1)
        self.assertEqual(violations, [])

    def test_action_requires_commit_or_image_digest(self) -> None:
        self.assertFalse(pins.dependency_is_pinned("actions/checkout@v7"))
        self.assertTrue(pins.dependency_is_pinned(f"actions/checkout@{'a' * 40}"))
        self.assertTrue(
            pins.dependency_is_pinned(
                f"docker://quay.io/example/tool@sha256:{'b' * 64}"
            )
        )


class PinnedContainerRunnerTests(unittest.TestCase):
    IMAGE = f"registry.example/forge@sha256:{'c' * 64}"

    def test_pull_requires_and_preserves_the_digest(self) -> None:
        self.assertEqual(
            runner.docker_command(["pull", self.IMAGE]),
            ["docker", "pull", self.IMAGE],
        )
        with self.assertRaises(runner.PinnedContainerError):
            runner.docker_command(["pull", "registry.example/forge:latest"])

    def test_run_places_options_before_the_verified_image(self) -> None:
        self.assertEqual(
            runner.docker_command(
                [
                    "run",
                    self.IMAGE,
                    "--network",
                    "none",
                    "--read-only",
                    "--",
                    "/bin/true",
                ]
            ),
            [
                "docker",
                "run",
                "--network",
                "none",
                "--read-only",
                self.IMAGE,
                "/bin/true",
            ],
        )

    def test_run_requires_an_explicit_command_delimiter(self) -> None:
        with self.assertRaises(runner.PinnedContainerError):
            runner.docker_command(["run", self.IMAGE, "/bin/true"])


class ReleaseWorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.text = (REPOSITORY / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        cls.workflow = yaml.load(cls.text, Loader=yaml.BaseLoader)
        cls.supply_chain_text = (
            REPOSITORY / ".github/workflows/supply-chain.yml"
        ).read_text(encoding="utf-8")
        cls.supply_chain = yaml.load(
            cls.supply_chain_text,
            Loader=yaml.BaseLoader,
        )
        cls.ci_text = (REPOSITORY / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        cls.ci = yaml.load(cls.ci_text, Loader=yaml.BaseLoader)

    def test_rust_and_build_changes_trigger_the_release_dry_run(self) -> None:
        paths = set(self.workflow["on"]["pull_request"]["paths"])
        self.assertTrue(
            {
                "Cargo.toml",
                "Cargo.lock",
                "build.rs",
                "src/**",
                "tools/manylinux-cmake-toolchain.cmake",
                "tools/workflow-check-requirements.lock",
            }
            <= paths
        )

    def test_security_checker_changes_trigger_supply_chain_validation(self) -> None:
        paths = set(self.supply_chain["on"]["pull_request"]["paths"])
        self.assertTrue(
            {
                "tools/check-workflow-pins.py",
                "tools/run-pinned-container.py",
                "tools/test_workflow_pins.py",
                "tools/workflow-check-requirements.lock",
            }
            <= paths
        )

    def test_workflows_install_the_dedicated_hash_lock(self) -> None:
        release_commands = "\n".join(
            step.get("run", "")
            for step in self.workflow["jobs"]["validate"]["steps"]
        )
        supply_commands = "\n".join(
            step.get("run", "")
            for step in self.supply_chain["jobs"]["workflow-integrity"]["steps"]
        )
        ci_commands = "\n".join(
            step.get("run", "") for step in self.ci["jobs"]["rust"]["steps"]
        )
        for commands in (release_commands, supply_commands, ci_commands):
            with self.subTest(workflow=commands[:40]):
                self.assertIn("--require-hashes", commands)
                self.assertIn("--only-binary=:all:", commands)
                self.assertIn(
                    "-r tools/workflow-check-requirements.lock",
                    commands,
                )
                self.assertIn("FORGE_WORKFLOW_CHECK_PYTHON=", commands)
                self.assertIn('>> "$GITHUB_ENV"', commands)
        supply_job = self.supply_chain["jobs"]["workflow-integrity"]
        self.assertNotIn(
            "FORGE_WORKFLOW_CHECK_PYTHON",
            supply_job.get("env", {}),
        )
        self.assertIn(
            '"$FORGE_WORKFLOW_CHECK_PYTHON" tools/check-workflow-pins.py',
            supply_commands,
        )

    def test_every_release_readiness_caller_has_the_locked_python(self) -> None:
        callers = 0
        for path in sorted((REPOSITORY / ".github/workflows").glob("*.y*ml")):
            workflow = yaml.load(
                path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader
            )
            for job in workflow.get("jobs", {}).values():
                commands = "\n".join(
                    step.get("run", "") for step in job.get("steps", [])
                )
                if "tools/check-release-readiness.sh" not in commands:
                    continue
                callers += 1
                self.assertNotIn(
                    "FORGE_WORKFLOW_CHECK_PYTHON",
                    job.get("env", {}),
                )
                self.assertIn("FORGE_WORKFLOW_CHECK_PYTHON=", commands)
                self.assertIn('>> "$GITHUB_ENV"', commands)
                self.assertIn(
                    "-r tools/workflow-check-requirements.lock",
                    commands,
                )
        self.assertEqual(callers, 2)

    def test_cpu_model_is_passed_from_workflow_to_real_elf_controls(self) -> None:
        smoke = self.workflow["jobs"]["smoke-linux-wheel-floor"]
        self.assertTrue(smoke["env"]["QEMU_CPU"])
        commands = "\n".join(
            step.get("run", "") for step in smoke["steps"]
        )
        self.assertIn('--qemu-x86-64 "$qemu"', commands)
        self.assertIn('--qemu-cpu "$QEMU_CPU"', commands)

    def test_release_workflow_passes_the_structural_pin_scan(self) -> None:
        _, _, violations = pins.scan_workflow_text(
            self.text, source="release.yml"
        )
        self.assertEqual(violations, [])

    def test_publish_still_needs_every_linux_wheel_gate(self) -> None:
        needs = set(self.workflow["jobs"]["publish"]["needs"])
        self.assertTrue(
            {
                "build-linux-wheel",
                "smoke-linux-wheel-floor",
                "reproducible-linux-wheel",
            }
            <= needs
        )

    def test_linux_wheel_builds_preserve_opus_analysis(self) -> None:
        builds = {
            "build-linux-wheel": "Build the dedicated generic x86-64 cdylib",
            "reproducible-linux-wheel": "Rebuild and repair independently",
        }
        toolchain = (
            "${{ github.workspace }}/tools/manylinux-cmake-toolchain.cmake"
        )
        for job_name, step_name in builds.items():
            job = self.workflow["jobs"][job_name]
            steps = [
                step for step in job["steps"] if step.get("name") == step_name
            ]
            with self.subTest(job=job_name):
                self.assertEqual(len(steps), 1)
                step = steps[0]
                commands = step.get("run", "")
                self.assertIn("--no-default-features", commands)
                self.assertIn("--features opus-encoding", commands)
                self.assertEqual(
                    job["env"].get("CMAKE_POLICY_VERSION_MINIMUM"),
                    "3.5",
                )
                self.assertEqual(
                    step.get("env", {}).get("CMAKE_TOOLCHAIN_FILE"),
                    toolchain,
                )


if __name__ == "__main__":
    unittest.main()
