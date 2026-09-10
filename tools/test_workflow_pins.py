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

    def test_wheel_build_lock_includes_the_windows_dependency_closure(self) -> None:
        lock = (TOOLS / "python-wheel-build-requirements.lock").read_text(
            encoding="utf-8"
        )
        requirements = {
            match.group(1).lower()
            for match in re.finditer(
                r"^([A-Za-z0-9_-]+)==[^ \\]+ \\$", lock, re.MULTILINE
            )
        }
        self.assertEqual(
            requirements,
            {
                "auditwheel",
                "build",
                "colorama",
                "packaging",
                "pyelftools",
                "pyproject-hooks",
                "setuptools",
                "tomli",
                "wheel",
            },
        )
        hashes = re.findall(r"--hash=sha256:([0-9a-f]{64})", lock)
        self.assertEqual(len(hashes), len(requirements))
        self.assertEqual(len(set(hashes)), len(requirements))


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

    def test_matrix_container_image_requires_every_include_row_to_be_pinned(self) -> None:
        digest = "a" * 64
        pinned = f"registry.example/forge@sha256:{digest}"
        fixture = f"""
jobs:
  build:
    strategy:
      matrix:
        include:
          - architecture: x86_64
            image: {pinned}
          - architecture: aarch64
            image: {pinned}
    container:
      image: ${{{{ matrix.image }}}}
"""
        _, images, violations = pins.scan_workflow_text(fixture)
        self.assertEqual(images, 2)
        self.assertEqual(violations, [])

        mutable = fixture.replace(pinned, "registry.example/forge:latest", 1)
        _, images, violations = pins.scan_workflow_text(mutable)
        self.assertEqual(images, 2)
        self.assertTrue(any("matrix image" in item for item in violations))

        missing = fixture.replace(f"            image: {pinned}\n", "", 1)
        _, _, violations = pins.scan_workflow_text(missing)
        self.assertTrue(any("resolve in every include row" in item for item in violations))

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
                "packaging/**",
                "src/**",
                "tests/fixtures/c_api_consumer.c",
                "tests/fixtures/native_package/**",
                "tools/check-linux-native-abi.py",
                "tools/manylinux-cmake-toolchain.cmake",
                "tools/native_package_metadata.py",
                "tools/publish-github-release.py",
                "tools/release_manifest.py",
                "tools/test-native-package.sh",
                "tools/test-native-package.ps1",
                "tools/test_native_package_metadata.py",
                "tools/test_publish_github_release.py",
                "tools/test_registry_artifacts.py",
                "tools/test_release_manifest.py",
                "tools/verify-registry-artifacts.py",
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

    def test_ci_bounds_target_growth_between_full_feature_test_sets(self) -> None:
        rust_job = self.ci["jobs"]["rust"]
        self.assertEqual(rust_job["env"]["CARGO_INCREMENTAL"], "0")
        steps = rust_job["steps"]
        positions = {
            step["name"]: index
            for index, step in enumerate(steps)
            if "name" in step
        }
        opus = positions["Run tests with static Opus support"]
        clean_before_ffmpeg = positions[
            "Reclaim target space before FFmpeg feature tests"
        ]
        ffmpeg = positions["Run tests with FFmpeg AAC, ALAC, and Vorbis support"]
        clean_before_mp3 = positions["Reclaim target space before MP3 feature tests"]
        mp3 = positions["Check and test MP3 encoding"]
        self.assertLess(opus, clean_before_ffmpeg)
        self.assertLess(clean_before_ffmpeg, ffmpeg)
        self.assertLess(ffmpeg, clean_before_mp3)
        self.assertLess(clean_before_mp3, mp3)
        self.assertEqual(steps[clean_before_ffmpeg]["run"], "cargo clean")
        self.assertEqual(steps[clean_before_mp3]["run"], "cargo clean")

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
        rows = {
            row["architecture"]: row
            for row in smoke["strategy"]["matrix"]["include"]
        }
        self.assertEqual(set(rows), {"x86_64", "aarch64"})
        self.assertEqual(
            rows["x86_64"]["qemu_deb_sha256"],
            "c3e3ba2bd87f8c5b9a5da5ef21b5a3b82d7c63b89dd448d9ddaa4eabc5b6e402",
        )
        self.assertEqual(
            rows["aarch64"]["qemu_deb_sha256"],
            "e420c417c12b42ca774795b62888d5388da0e08c1ef263dea1490e5ce4e32a0b",
        )
        self.assertEqual(
            rows["aarch64"]["manylinux_2_34"],
            "quay.io/pypa/manylinux_2_34_aarch64@sha256:"
            "effc0e17319a56b2c7eaff0cb5dd81a2d2c2850410841d3f017378f52ead6442",
        )
        self.assertEqual(rows["aarch64"]["qemu_cpu"], "cortex-a53")
        for row in rows.values():
            self.assertTrue(row["qemu_deb_url"].startswith(
                "https://snapshot.debian.org/archive/debian/20260526T203455Z/"
            ))
        commands = "\n".join(
            step.get("run", "") for step in smoke["steps"]
        )
        self.assertIn('--qemu-x86-64 "$qemu"', commands)
        self.assertIn('--qemu-aarch64 "$qemu"', commands)
        self.assertIn('--qemu-cpu "$QEMU_CPU"', commands)

    def test_release_workflow_passes_the_structural_pin_scan(self) -> None:
        _, _, violations = pins.scan_workflow_text(
            self.text, source="release.yml"
        )
        self.assertEqual(violations, [])

    def test_assembly_still_needs_every_linux_wheel_gate(self) -> None:
        needs = set(self.workflow["jobs"]["assemble-release"]["needs"])
        self.assertTrue(
            {
                "build-linux-wheel",
                "smoke-linux-wheel-floor",
                "reproducible-linux-wheel",
            }
            <= needs
        )

    def test_assembly_needs_every_portable_native_archive_gate(self) -> None:
        needs = set(self.workflow["jobs"]["assemble-release"]["needs"])
        self.assertTrue(
            {
                "build-linux",
                "smoke-linux-wheel-floor",
                "reproducible-linux",
                "reproducible-linux-aarch64",
            }
            <= needs
        )

    def test_generic_linux_archive_is_built_below_its_runtime_floor(self) -> None:
        x86_image = (
            "quay.io/pypa/manylinux_2_28_x86_64@sha256:"
            "53390351aeb4688114b02c36a23b3e6ce1166ee9b7afc5df1a4f776354fc764c"
        )
        arm_image = (
            "quay.io/pypa/manylinux_2_28_aarch64@sha256:"
            "ad74e53b713f3b07d8c889c526dc0c6500da9827b45e38739570875fef52e28f"
        )
        generic = self.workflow["jobs"]["build-linux"]
        self.assertEqual(generic["container"]["image"], "${{ matrix.image }}")
        rows = {
            row["architecture"]: row
            for row in generic["strategy"]["matrix"]["include"]
        }
        self.assertEqual(rows["x86_64"]["image"], x86_image)
        self.assertEqual(rows["aarch64"]["image"], arm_image)
        self.assertEqual(rows["x86_64"]["target_cpu"], "x86-64")
        self.assertEqual(rows["aarch64"]["target_cpu"], "generic")
        self.assertIn("-march=x86-64", rows["x86_64"]["cflags"])
        self.assertIn("-march=armv8-a", rows["aarch64"]["cflags"])
        for job_name, image in (
            ("build-linux-v3", x86_image),
            ("reproducible-linux", x86_image),
            ("reproducible-linux-aarch64", arm_image),
        ):
            with self.subTest(job=job_name):
                job = self.workflow["jobs"][job_name]
                self.assertEqual(job["container"]["image"], image)
                commands = "\n".join(
                    step.get("run", "") for step in job["steps"]
                )
                self.assertIn('glibc 2.28', commands)
                self.assertIn("RUSTUP_INIT_SHA256", commands)
        commands = "\n".join(
            step.get("run", "") for step in generic["steps"]
        )
        self.assertIn("matrix.target_cpu", generic["env"]["RUSTFLAGS"])
        self.assertIn("tools/check-linux-native-abi.py", commands)
        self.assertIn("tools/test-native-package.sh", commands)

    def test_oldest_supported_linux_runtime_exercises_both_artifacts(self) -> None:
        smoke = self.workflow["jobs"]["smoke-linux-wheel-floor"]
        self.assertTrue({"build-linux", "build-linux-wheel"} <= set(smoke["needs"]))
        commands = "\n".join(
            step.get("run", "") for step in smoke["steps"]
        )
        self.assertIn('glibc 2.34', commands)
        self.assertIn("/native/forge --version", commands)
        self.assertIn("/source/tools/test-native-package.sh", commands)
        self.assertIn("tools/check-linux-native-abi.py", commands)
        self.assertIn("--archive", commands)
        self.assertIn("--expected-root", commands)
        self.assertIn("/qemu-static", commands)
        self.assertIn("manylinux_2_34_${FORGE_ARCHITECTURE}", commands)

    def test_linux_wheel_builds_preserve_opus_analysis(self) -> None:
        builds = {
            "build-linux-wheel": "Build the dedicated generic Linux cdylib",
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

    def test_release_privileges_are_split_across_the_one_way_dag(self) -> None:
        jobs = self.workflow["jobs"]
        self.assertNotIn("permissions", jobs["assemble-release"])
        self.assertEqual(
            jobs["attest-release"]["permissions"],
            {
                "attestations": "write",
                "contents": "read",
                "id-token": "write",
            },
        )
        self.assertEqual(
            jobs["publish-github"]["permissions"],
            {"contents": "write"},
        )
        self.assertEqual(
            set(jobs["publish-github"]["needs"]),
            {"validate", "attest-release"},
        )
        self.assertEqual(
            set(jobs["attest-release"]["needs"]),
            {"validate", "assemble-release"},
        )
        concurrency = jobs["publish-github"]["concurrency"]
        self.assertEqual(concurrency["group"], "forge-release-publication")
        self.assertEqual(concurrency["queue"], "max")
        self.assertEqual(concurrency["cancel-in-progress"], "false")
        publisher_steps = jobs["publish-github"]["steps"]
        policy_steps = [
            step
            for step in publisher_steps
            if step.get("id") == "release-policy-token"
        ]
        self.assertEqual(len(policy_steps), 1)
        self.assertEqual(
            policy_steps[0]["uses"],
            "actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1",
        )
        self.assertEqual(
            policy_steps[0]["with"].get("permission-administration"), "write"
        )
        publish_steps = [
            step
            for step in publisher_steps
            if "tools/publish-github-release.py" in step.get("run", "")
        ]
        self.assertEqual(len(publish_steps), 1)
        self.assertEqual(
            publish_steps[0].get("env", {}).get("GITHUB_POLICY_TOKEN"),
            "${{ steps.release-policy-token.outputs.token }}",
        )

    def test_assembly_and_publication_use_an_exact_manifest_without_globs(self) -> None:
        forbidden = (
            "pattern: forge-*",
            "merge-multiple: true",
            "gh release create",
            "gh release edit",
            "dist/*",
        )
        for fragment in forbidden:
            with self.subTest(fragment=fragment):
                self.assertNotIn(fragment, self.text)
        assembly = self.workflow["jobs"]["assemble-release"]
        downloads = [
            step["with"]["name"]
            for step in assembly["steps"]
            if step.get("uses", "").startswith("actions/download-artifact@")
        ]
        self.assertEqual(
            set(downloads),
            {
                "forge-linux-x86_64",
                "forge-linux-aarch64",
                "forge-linux-x86_64-v3",
                "forge-windows-x86_64",
                "forge-macos-x86_64",
                "forge-macos-aarch64",
                "forge-python-linux-x86_64",
                "forge-python-linux-aarch64",
                "forge-wasm-packages",
                "forge-rust-crate",
            },
        )
        commands = "\n".join(
            step.get("run", "") for step in assembly["steps"]
        )
        self.assertIn("tools/release_manifest.py generate-sboms", commands)
        self.assertIn("tools/release_manifest.py finalize", commands)
        self.assertIn("tools/release_manifest.py verify", commands)
        self.assertIn("--state local", commands)
        self.assertNotIn("pgo-profile", "\n".join(downloads))

    def test_latest_is_decided_only_by_the_immutable_publisher(self) -> None:
        publisher_job = self.workflow["jobs"]["publish-github"]
        commands = "\n".join(
            step.get("run", "") for step in publisher_job["steps"]
        )
        self.assertIn("tools/publish-github-release.py", commands)
        self.assertNotIn("--latest", commands)
        helper = (TOOLS / "publish-github-release.py").read_text(encoding="utf-8")
        self.assertIn("decide_latest", helper)
        self.assertIn('"draft": False', helper)
        self.assertIn('"make_latest":', helper)

    def test_registry_publishers_are_isolated_oidc_tag_push_jobs(self) -> None:
        jobs = self.workflow["jobs"]
        expected = {
            "publish-pypi": "pypi",
            "publish-npm": "npm",
            "publish-crates": "crates-io",
        }
        for job_name, environment in expected.items():
            with self.subTest(job=job_name):
                job = jobs[job_name]
                self.assertEqual(
                    job["if"],
                    "github.event_name == 'push' && github.ref_type == 'tag'",
                )
                self.assertEqual(job["environment"], environment)
                self.assertEqual(
                    job["permissions"],
                    {"contents": "read", "id-token": "write"},
                )
                self.assertEqual(
                    set(job["needs"]), {"validate", "publish-github"}
                )
                self.assertEqual(job["concurrency"].get("queue"), "max")
                commands = "\n".join(
                    step.get("run", "") for step in job["steps"]
                )
                self.assertIn("tools/release_manifest.py verify", commands)
                self.assertIn("tools/verify-registry-artifacts.py", commands)
                self.assertIn("--state pre", commands)
                self.assertIn("--state post", commands)
        self.assertIn(
            "pypa/gh-action-pypi-publish@76f52bc884231f62b9a034ebfe128415bbaabdfc",
            self.text,
        )
        self.assertIn(
            "rust-lang/crates-io-auth-action@c6f97d42243bad5fab37ca0427f495c86d5b1a18",
            self.text,
        )
        crates_publish = [
            step
            for step in jobs["publish-crates"]["steps"]
            if step.get("name") == "Publish the previously absent crate"
        ]
        self.assertEqual(len(crates_publish), 1)
        self.assertIn("cargo publish --locked --no-verify", crates_publish[0]["run"])
        self.assertIn("cmp", crates_publish[0]["run"])
        self.assertIn("node-version: \"24.21.0\"", self.text)
        self.assertIn("npm --version", self.text)
        self.assertNotIn("NODE_AUTH_TOKEN", self.text)

    def test_arm64_is_built_reproduced_and_exercised_on_native_runners(self) -> None:
        release_jobs = self.workflow["jobs"]
        for job_name in (
            "build-linux",
            "build-linux-wheel",
            "reproducible-linux-wheel",
            "smoke-linux-wheel-floor",
        ):
            rows = release_jobs[job_name]["strategy"]["matrix"]["include"]
            arm = [row for row in rows if row["architecture"] == "aarch64"]
            self.assertEqual(len(arm), 1, job_name)
            self.assertEqual(arm[0]["runner"], "ubuntu-24.04-arm")
        self.assertEqual(
            release_jobs["reproducible-linux-aarch64"]["runs-on"],
            "ubuntu-24.04-arm",
        )
        ci_rows = self.ci["jobs"]["c-api"]["strategy"]["matrix"]["include"]
        self.assertTrue(
            any(
                row["os"] == "ubuntu-24.04-arm"
                and row["target"] == "aarch64-unknown-linux-gnu"
                for row in ci_rows
            )
        )


if __name__ == "__main__":
    unittest.main()
