from __future__ import annotations

import argparse
import importlib.util
import os
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("check-linux-wheel-abi.py")
SPEC = importlib.util.spec_from_file_location("check_linux_wheel_abi", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
abi = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(abi)

BUILDER_SCRIPT = Path(__file__).with_name("build-python-wheel.py")
BUILDER_SPEC = importlib.util.spec_from_file_location(
    "build_python_wheel", BUILDER_SCRIPT
)
assert BUILDER_SPEC is not None and BUILDER_SPEC.loader is not None
builder = importlib.util.module_from_spec(BUILDER_SPEC)
BUILDER_SPEC.loader.exec_module(builder)

MANYLINUX_CMAKE_TOOLCHAIN = SCRIPT.with_name("manylinux-cmake-toolchain.cmake")


def elf_header(
    marker: bytes = b"",
    *,
    architecture: str = "x86_64",
    elf_type: int = abi.ET_DYN,
) -> bytes:
    header = bytearray(64)
    header[:4] = b"\x7fELF"
    header[4] = 2
    header[5] = 1
    header[6] = 1
    header[16:18] = elf_type.to_bytes(2, "little")
    machine = abi.contract_for_architecture(architecture).elf_machine
    header[18:20] = machine.to_bytes(2, "little")
    return bytes(header) + marker


def make_wheel(
    directory: Path,
    *,
    architecture: str = "x86_64",
    filename_platform: str | None = None,
    metadata_platform: str | None = None,
    elves: tuple[str, ...] = ("forge_normalizer/lib/libforge_normalizer.so",),
) -> Path:
    contract = abi.contract_for_architecture(architecture)
    if filename_platform is None:
        filename_platform = contract.platform
    if metadata_platform is None:
        metadata_platform = contract.platform
    wheel = directory / (
        f"forge_normalizer-0.189.11-py3-none-{filename_platform}.whl"
    )
    metadata = (
        "Wheel-Version: 1.0\n"
        "Root-Is-Purelib: false\n"
        f"Tag: py3-none-{metadata_platform}\n"
    )
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("forge_normalizer-0.189.11.dist-info/WHEEL", metadata)
        for index, member in enumerate(elves):
            archive.writestr(
                member,
                elf_header(
                    str(index).encode("ascii"),
                    architecture=architecture,
                ),
            )
    return wheel


def accept_auditwheel(_wheel: Path, _executable: str) -> None:
    return None


def accept_elf(_path: Path, _member: str, _readelf: str) -> None:
    return None


class ManylinuxCmakeLayoutTests(unittest.TestCase):
    def test_toolchain_forces_the_lib_directory_expected_by_audiopus_sys(self) -> None:
        commands = [
            line.strip()
            for line in MANYLINUX_CMAKE_TOOLCHAIN.read_text(
                encoding="utf-8"
            ).splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        ]
        self.assertEqual(
            commands,
            [
                'set(CMAKE_INSTALL_LIBDIR "lib" CACHE STRING '
                '"Install libraries under lib" FORCE)'
            ],
        )


class SymbolVersionTests(unittest.TestCase):
    def test_glibc_2_34_is_accepted(self) -> None:
        abi.validate_version_info(
            "0x0010: Name: GLIBC_2.34  Flags: none  Version: 7"
        )

    def test_glibc_2_35_is_rejected(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "GLIBC_2.35"):
            abi.validate_version_info(
                "0x0010: Name: GLIBC_2.35  Flags: none  Version: 7"
            )

    def test_glibc_private_is_rejected(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "GLIBC_PRIVATE"):
            abi.validate_version_info(
                "0x0010: Name: GLIBC_PRIVATE  Flags: none  Version: 7"
            )

    def test_non_numeric_glibc_namespace_is_rejected(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "GLIBC_ABI_DT_RELR"):
            abi.validate_version_info("Name: GLIBC_ABI_DT_RELR")

    def test_cxx_symbol_versions_are_rejected(self) -> None:
        for symbol in ("GLIBCXX_3.4.30", "CXXABI_1.3.13"):
            with self.subTest(symbol=symbol):
                with self.assertRaisesRegex(abi.WheelAbiError, symbol):
                    abi.validate_version_info(f"Name: {symbol}")


class DynamicAndIsaTests(unittest.TestCase):
    def test_unexpected_needed_library_is_rejected(self) -> None:
        output = """
         0x0000000000000001 (NEEDED) Shared library: [libc.so.6]
         0x0000000000000001 (NEEDED) Shared library: [libstdc++.so.6]
        """
        with self.assertRaisesRegex(abi.WheelAbiError, "libstdc\\+\\+.so.6"):
            abi.validate_dynamic_section(output)

    def test_x86_64_v2_requirement_is_rejected(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "x86-64-v2"):
            abi.validate_notes("Properties: x86 ISA needed: x86-64-v2")

    def test_dt_textrel_and_df_textrel_are_rejected(self) -> None:
        for dynamic in (
            "0x0000000000000016 (TEXTREL) 0x0",
            "0x000000000000001e (FLAGS) TEXTREL BIND_NOW",
        ):
            with self.subTest(dynamic=dynamic):
                with self.assertRaisesRegex(abi.WheelAbiError, "text relocations"):
                    abi.validate_dynamic_section(dynamic)

    def test_executable_or_missing_gnu_stack_is_rejected(self) -> None:
        abi.validate_program_headers(
            "GNU_STACK 0x0 0x0 0x0 0x0 0x0 RW 0x10"
        )
        with self.assertRaisesRegex(abi.WheelAbiError, "executable stack"):
            abi.validate_program_headers(
                "GNU_STACK 0x0 0x0 0x0 0x0 0x0 RWE 0x10"
            )
        with self.assertRaisesRegex(abi.WheelAbiError, "exactly one"):
            abi.validate_program_headers("LOAD 0x0 0x0 0x0 0x0 0x0 R E 0x1000")

    def test_interpreter_must_match_the_selected_architecture(self) -> None:
        abi.validate_interpreter(
            "[Requesting program interpreter: /lib/ld-linux-aarch64.so.1]",
            architecture="aarch64",
        )
        with self.assertRaisesRegex(abi.WheelAbiError, "ld-linux-aarch64"):
            abi.validate_interpreter(
                "[Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]",
                architecture="aarch64",
            )

    def test_auditwheel_success_without_policy_text_is_rejected(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "did not report"):
            abi.validate_auditwheel_output("The command completed successfully")

    def test_auditwheel_policy_at_or_below_2_34_is_accepted(self) -> None:
        for policy in ("manylinux_2_17_x86_64", "manylinux_2_34_x86_64"):
            with self.subTest(policy=policy):
                abi.validate_auditwheel_output(
                    "wheel is consistent with the following platform tag: "
                    f'"{policy}".'
                )

    def test_aarch64_contract_rejects_optional_features(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "BTI"):
            abi.validate_notes(
                "GNU properties: AArch64 feature: BTI",
                architecture="aarch64",
            )

    def test_wheel_header_rejects_static_executable(self) -> None:
        with self.assertRaisesRegex(abi.WheelAbiError, "ET_DYN"):
            abi.validate_elf_header(
                elf_header(elf_type=abi.ET_EXEC),
                member="fixture.so",
            )

    def test_aarch64_auditwheel_policy_is_architecture_specific(self) -> None:
        abi.validate_auditwheel_output(
            'wheel is consistent with the following platform tag: '
            '"manylinux_2_34_aarch64".',
            architecture="aarch64",
        )
        with self.assertRaisesRegex(abi.WheelAbiError, "aarch64"):
            abi.validate_auditwheel_output(
                'wheel is consistent with the following platform tag: '
                '"manylinux_2_34_x86_64".',
                architecture="aarch64",
            )


class WholeWheelTests(unittest.TestCase):
    def test_builder_can_only_emit_an_unclaimed_linux_tag(self) -> None:
        self.assertIn("linux_x86_64", builder.SUPPORTED_PLATFORMS)
        self.assertIn("linux_aarch64", builder.SUPPORTED_PLATFORMS)
        self.assertNotIn(abi.EXPECTED_PLATFORM, builder.SUPPORTED_PLATFORMS)

    def test_aarch64_wheel_uses_aarch64_machine_and_tag(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = make_wheel(Path(directory), architecture="aarch64")
            inspected = abi.verify_wheel(
                wheel,
                architecture="aarch64",
                inspector=accept_elf,
                auditwheel_checker=accept_auditwheel,
            )
            self.assertEqual(
                inspected,
                ["forge_normalizer/lib/libforge_normalizer.so"],
            )

    def test_aarch64_default_elf_inspector_receives_architecture(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            wheel = make_wheel(root, architecture="aarch64")
            fake_readelf = root / "fake-readelf"
            fake_readelf.write_text(
                "#!/bin/sh\nprintf '%s\\n' 'GNU_STACK 0x0 0x0 0x0 0x0 0x0 RW 0x10'\n",
                encoding="ascii",
            )
            fake_readelf.chmod(0o755)
            self.assertEqual(
                abi.verify_wheel(
                    wheel,
                    architecture="aarch64",
                    readelf=str(fake_readelf),
                    auditwheel_checker=accept_auditwheel,
                ),
                ["forge_normalizer/lib/libforge_normalizer.so"],
            )

    def test_aarch64_contract_rejects_x86_elf_payload(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = make_wheel(
                Path(directory),
                architecture="aarch64",
                elves=("forge_normalizer/lib/libforge_normalizer.so",),
            )
            # Replace the ARM fixture with a valid x86-64 ELF header while
            # preserving the aarch64 filename and WHEEL tag.
            with zipfile.ZipFile(wheel, "r") as source:
                members = {
                    name: source.read(name)
                    for name in source.namelist()
                }
            members["forge_normalizer/lib/libforge_normalizer.so"] = elf_header()
            wheel.unlink()
            with zipfile.ZipFile(wheel, "w") as target:
                for name, content in members.items():
                    target.writestr(name, content)
            with self.assertRaisesRegex(abi.WheelAbiError, "expected AArch64"):
                abi.verify_wheel(
                    wheel,
                    architecture="aarch64",
                    inspector=accept_elf,
                    auditwheel_checker=accept_auditwheel,
                )

    def test_every_elf_member_is_inspected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = make_wheel(
                Path(directory),
                elves=(
                    "forge_normalizer/lib/libforge_normalizer.so",
                    "forge_normalizer.libs/secondary.so",
                ),
            )
            inspected: list[str] = []

            def inspect(_path: Path, member: str, _readelf: str) -> None:
                inspected.append(member)
                if member.endswith("secondary.so"):
                    raise abi.WheelAbiError("secondary ELF rejected")

            with self.assertRaisesRegex(abi.WheelAbiError, "secondary ELF"):
                abi.verify_wheel(
                    wheel,
                    inspector=inspect,
                    auditwheel_checker=accept_auditwheel,
                )
            self.assertEqual(
                inspected,
                [
                    "forge_normalizer/lib/libforge_normalizer.so",
                    "forge_normalizer.libs/secondary.so",
                ],
            )

    def test_filename_and_wheel_tag_must_match(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = make_wheel(
                Path(directory), metadata_platform="linux_x86_64"
            )
            with self.assertRaisesRegex(abi.WheelAbiError, "WHEEL tags"):
                abi.verify_wheel(
                    wheel,
                    inspector=accept_elf,
                    auditwheel_checker=accept_auditwheel,
                )

    def test_filename_platform_must_be_the_release_contract(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            wheel = make_wheel(
                Path(directory),
                filename_platform="linux_x86_64",
                metadata_platform="linux_x86_64",
            )
            with self.assertRaisesRegex(abi.WheelAbiError, "filename platform"):
                abi.verify_wheel(
                    wheel,
                    inspector=accept_elf,
                    auditwheel_checker=accept_auditwheel,
                )


class BaselineCpuEmulationTests(unittest.TestCase):
    def test_x86_workflow_cpu_reaches_the_x86_fixture(self) -> None:
        class ReachedCompile(Exception):
            pass

        with tempfile.TemporaryDirectory() as directory:
            qemu = Path(directory) / "qemu-x86_64-static"
            qemu.write_bytes(b"fixture")
            qemu.chmod(0o755)
            with mock.patch(
                f"{__name__}._compile_static_elf",
                side_effect=ReachedCompile,
            ):
                with self.assertRaises(ReachedCompile):
                    run_cpu_emulation_controls(
                        str(qemu),
                        "qemu64,-lahf-lm,-pni,-ssse3,-sse4.1,-sse4.2,"
                        "-popcnt,-cx16,-avx,-avx2",
                    )

    def test_aarch64_control_requires_plain_cortex_a53(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            qemu = Path(directory) / "qemu-aarch64-static"
            qemu.write_bytes(b"fixture")
            qemu.chmod(0o755)
            with self.assertRaisesRegex(AssertionError, "plain cortex-a53"):
                run_aarch64_cpu_controls(str(qemu), "max")

    def test_aarch64_control_reaches_all_instruction_probes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            qemu = Path(directory) / "qemu-aarch64-static"
            qemu.write_bytes(b"fixture")
            qemu.chmod(0o755)

            def compile_fixture(_source: str, output: Path, **_kwargs: object) -> None:
                output.write_bytes(
                    elf_header(architecture="aarch64", elf_type=abi.ET_EXEC)
                )

            with (
                mock.patch(
                    f"{__name__}._compile_static_elf",
                    side_effect=compile_fixture,
                ) as compile_mock,
                mock.patch(
                    f"{__name__}._run_qemu",
                    side_effect=(0, 0, -4, 0),
                ) as qemu_mock,
                mock.patch.object(
                    abi,
                    "validate_elf_header",
                    wraps=abi.validate_elf_header,
                ) as header_mock,
            ):
                run_aarch64_cpu_controls(str(qemu), "cortex-a53")

            self.assertEqual(compile_mock.call_count, 3)
            self.assertEqual(header_mock.call_count, 3)
            self.assertEqual(qemu_mock.call_count, 4)

    @unittest.skipUnless(
        os.environ.get("FORGE_QEMU_X86_64")
        and os.environ.get("FORGE_QEMU_CPU"),
        "set FORGE_QEMU_X86_64 and FORGE_QEMU_CPU to run CPU fixtures",
    )
    def test_real_elf_cpu_controls(self) -> None:
        run_cpu_emulation_controls(
            os.environ["FORGE_QEMU_X86_64"],
            os.environ["FORGE_QEMU_CPU"],
        )


CPU_FEATURES = {
    # name: (QEMU feature, CPUID leaf, output register, bit, instruction)
    "lahf_sahf": (
        "lahf-lm",
        0x80000001,
        "ecx",
        0,
        "lahf\nsahf",
    ),
    "sse3": ("pni", 1, "ecx", 0, "addsubps %xmm0,%xmm0"),
    "ssse3": ("ssse3", 1, "ecx", 9, "pshufb %xmm0,%xmm0"),
    "cx16": (
        "cx16",
        1,
        "ecx",
        13,
        "xor %eax,%eax\nxor %edx,%edx\nxor %ebx,%ebx\n"
        "xor %ecx,%ecx\nlock cmpxchg16b (%rsp)",
    ),
    "sse4_1": ("sse4.1", 1, "ecx", 19, "ptest %xmm0,%xmm0"),
    "sse4_2": ("sse4.2", 1, "ecx", 20, "crc32 %eax,%eax"),
    "popcnt": ("popcnt", 1, "ecx", 23, "popcnt %rax,%rax"),
    # These are not part of x86-64-v2. They remain controls because Forge has
    # guarded AVX2/FMA fast paths and a property-only scan cannot distinguish
    # them from unguarded instructions.
    "avx": ("avx", 1, "ecx", 28, "vxorps %ymm0,%ymm0,%ymm0"),
    "avx2": ("avx2", 7, "ebx", 5, "vpbroadcastd %xmm0,%ymm0"),
}


def _compile_static_elf(
    source: str,
    output: Path,
    *,
    compiler: str = "cc",
    extra_args: tuple[str, ...] = (),
) -> None:
    assembly = output.with_suffix(".S")
    assembly.write_text(
        ".global _start\n.text\n_start:\n"
        f"{source}\n"
        '.section .note.GNU-stack,"",@progbits\n',
        encoding="utf-8",
    )
    subprocess.run(
        [
            compiler,
            *extra_args,
            "-nostdlib",
            "-static",
            "-no-pie",
            "-Wl,--build-id=none",
            "-Wl,-z,noexecstack",
            "-o",
            str(output),
            str(assembly),
        ],
        check=True,
    )
    notes = subprocess.run(
        ["readelf", "--notes", "--wide", str(output)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    if "x86 ISA needed" in notes:
        raise AssertionError(f"fixture unexpectedly declares an ISA: {output.name}")


def _cpuid_probe_source(leaf: int, register: str, bit: int) -> str:
    register32 = {"ebx": "%ebx", "ecx": "%ecx"}[register]
    return (
        f"mov ${leaf},%eax\n"
        "xor %ecx,%ecx\n"
        "cpuid\n"
        "xor %edi,%edi\n"
        f"bt ${bit},{register32}\n"
        "setc %dil\n"
        "mov $60,%eax\n"
        "syscall"
    )


def _instruction_probe_source(instruction: str) -> str:
    return f"{instruction}\nxor %edi,%edi\nmov $60,%eax\nsyscall"


def _feature_enabled_cpu(cpu: str, feature: str) -> str:
    # Requiring exactly one negative token makes removal of any individual
    # workflow mask a test failure. Replacing it with an explicit positive
    # token also avoids relying on QEMU model defaults for the sensitivity
    # control below.
    disabled = f"-{feature}"
    tokens = cpu.split(",")
    if tokens.count(disabled) != 1:
        raise AssertionError(
            f"QEMU CPU must disable {feature!r} exactly once: {cpu}"
        )
    enabled_tokens = [
        f"+{feature}" if token == disabled else token for token in tokens
    ]
    if feature == "avx":
        enabled_tokens.append("+xsave")
    elif feature == "avx2":
        enabled_tokens = [
            "+avx" if token == "-avx" else token for token in enabled_tokens
        ]
        enabled_tokens.append("+xsave")
    return ",".join(enabled_tokens)


def _run_qemu(qemu: str, cpu: str, executable: Path) -> int:
    return subprocess.run(
        [qemu, "-cpu", cpu, str(executable)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    ).returncode


def run_cpu_emulation_controls(qemu: str, cpu: str) -> None:
    """Prove that the supplied workflow CPU masks every required feature."""

    qemu_path = Path(qemu)
    if not qemu_path.is_file() or not os.access(qemu_path, os.X_OK):
        raise AssertionError(f"QEMU executable is unavailable: {qemu}")

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        baseline = root / "baseline"
        _compile_static_elf(
            "xor %edi,%edi\nmov $60,%eax\nsyscall",
            baseline,
        )
        if _run_qemu(qemu, cpu, baseline) != 0:
            raise AssertionError("baseline property-free ELF failed under QEMU")

        for name, (feature, leaf, register, bit, instruction) in CPU_FEATURES.items():
            enabled_cpu = _feature_enabled_cpu(cpu, feature)
            cpuid_probe = root / f"cpuid-{name}"
            instruction_probe = root / f"instruction-{name}"
            _compile_static_elf(
                _cpuid_probe_source(leaf, register, bit),
                cpuid_probe,
            )
            _compile_static_elf(
                _instruction_probe_source(instruction),
                instruction_probe,
            )

            disabled_cpuid = _run_qemu(qemu, cpu, cpuid_probe)
            if disabled_cpuid != 0:
                raise AssertionError(
                    f"CPUID still advertises disabled feature {feature}: "
                    f"exit {disabled_cpuid}"
                )
            disabled_instruction = _run_qemu(qemu, cpu, instruction_probe)
            if disabled_instruction not in (-4, 132):
                raise AssertionError(
                    f"{feature} instruction did not SIGILL while disabled: "
                    f"exit {disabled_instruction}"
                )

            enabled_cpuid = _run_qemu(qemu, enabled_cpu, cpuid_probe)
            if enabled_cpuid != 1:
                raise AssertionError(
                    f"CPUID positive control failed for {feature}: "
                    f"exit {enabled_cpuid}"
                )
            enabled_instruction = _run_qemu(
                qemu,
                enabled_cpu,
                instruction_probe,
            )
            if enabled_instruction != 0:
                raise AssertionError(
                    f"instruction positive control failed for {feature}: "
                    f"exit {enabled_instruction}"
                )


def run_aarch64_cpu_controls(
    qemu: str,
    cpu: str,
    *,
    compiler: str = "cc",
) -> None:
    """Prove that an AArch64 QEMU model enforces the Cortex-A53 floor.

    The release baseline includes ARMv8-A and mandatory ASIMD/NEON.  QEMU's
    Cortex-A53 model also exposes optional crypto instructions, so a later SVE
    instruction is the sensitivity control: it must trap on Cortex-A53 and
    execute on QEMU's ``max`` model.  Keeping the compiler explicit lets this
    run on a native ARM runner (``cc``) or an x86 runner with a pinned aarch64
    cross compiler.
    """

    qemu_path = Path(qemu)
    if not qemu_path.is_file() or not os.access(qemu_path, os.X_OK):
        raise AssertionError(f"QEMU executable is unavailable: {qemu}")
    if cpu != "cortex-a53":
        raise AssertionError(
            "AArch64 baseline QEMU CPU must be plain cortex-a53: "
            f"{cpu}"
        )

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        baseline = root / "baseline-aarch64"
        neon = root / "neon-aarch64"
        sve = root / "sve-aarch64"
        _compile_static_elf(
            "mov x0, #0\nmov x8, #93\nsvc #0",
            baseline,
            compiler=compiler,
            extra_args=("-march=armv8-a",),
        )
        _compile_static_elf(
            "movi v0.16b, #0\n"
            "add v0.16b, v0.16b, v0.16b\n"
            "mov x0, #0\nmov x8, #93\nsvc #0",
            neon,
            compiler=compiler,
            extra_args=("-march=armv8-a",),
        )
        _compile_static_elf(
            ".arch armv8.2-a+sve\n"
            "ptrue p0.b\n"
            "mov x0, #0\nmov x8, #93\nsvc #0",
            sve,
            compiler=compiler,
            extra_args=("-march=armv8.2-a+sve",),
        )
        for probe in (baseline, neon, sve):
            abi.validate_elf_header(
                probe.read_bytes()[:20],
                member=probe.name,
                architecture="aarch64",
                expected_elf_type=abi.ET_EXEC,
            )
        baseline_status = _run_qemu(qemu, cpu, baseline)
        if baseline_status != 0:
            raise AssertionError(
                f"AArch64 ARMv8 baseline ELF failed under QEMU: exit {baseline_status}"
            )
        neon_status = _run_qemu(qemu, cpu, neon)
        if neon_status != 0:
            raise AssertionError(
                "AArch64 mandatory NEON instruction failed under QEMU: "
                f"exit {neon_status}"
            )
        sve_status = _run_qemu(qemu, cpu, sve)
        if sve_status not in (-4, 132):
            raise AssertionError(
                "AArch64 SVE instruction was not trapped by the Cortex-A53 "
                f"QEMU CPU model: exit {sve_status}"
            )
        sve_positive_status = _run_qemu(qemu, "max", sve)
        if sve_positive_status != 0:
            raise AssertionError(
                "AArch64 SVE positive control failed under QEMU max: "
                f"exit {sve_positive_status}"
            )


def parse_qemu_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run real-ELF controls for the release QEMU CPU model"
    )
    parser.add_argument("--architecture", choices=abi.ARCHITECTURES, default="x86_64")
    parser.add_argument("--qemu-x86-64")
    parser.add_argument("--qemu-aarch64")
    parser.add_argument("--qemu-cpu")
    parser.add_argument("--cc", default="cc")
    return parser.parse_args()


if __name__ == "__main__":
    qemu_args = parse_qemu_args()
    if qemu_args.architecture == "x86_64":
        if not qemu_args.qemu_x86_64:
            raise SystemExit("--qemu-x86-64 is required for --architecture x86_64")
        if not qemu_args.qemu_cpu:
            raise SystemExit("--qemu-cpu is required for --architecture x86_64")
        run_cpu_emulation_controls(qemu_args.qemu_x86_64, qemu_args.qemu_cpu)
    else:
        if not qemu_args.qemu_aarch64:
            raise SystemExit("--qemu-aarch64 is required for --architecture aarch64")
        qemu_cpu = qemu_args.qemu_cpu or "cortex-a53"
        run_aarch64_cpu_controls(
            qemu_args.qemu_aarch64,
            qemu_cpu,
            compiler=qemu_args.cc,
        )
    print("QEMU CPU feature and instruction controls passed")
