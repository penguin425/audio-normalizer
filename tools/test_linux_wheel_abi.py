from __future__ import annotations

import argparse
import importlib.util
import os
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path


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


def elf_header(marker: bytes = b"") -> bytes:
    header = bytearray(64)
    header[:4] = b"\x7fELF"
    header[4] = 2
    header[5] = 1
    header[6] = 1
    header[16:18] = (3).to_bytes(2, "little")
    header[18:20] = (62).to_bytes(2, "little")
    return bytes(header) + marker


def make_wheel(
    directory: Path,
    *,
    filename_platform: str = abi.EXPECTED_PLATFORM,
    metadata_platform: str = abi.EXPECTED_PLATFORM,
    elves: tuple[str, ...] = ("forge_normalizer/lib/libforge_normalizer.so",),
) -> Path:
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
            archive.writestr(member, elf_header(str(index).encode("ascii")))
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


class WholeWheelTests(unittest.TestCase):
    def test_builder_can_only_emit_an_unclaimed_linux_tag(self) -> None:
        self.assertIn("linux_x86_64", builder.SUPPORTED_PLATFORMS)
        self.assertNotIn(abi.EXPECTED_PLATFORM, builder.SUPPORTED_PLATFORMS)

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


def _compile_static_elf(source: str, output: Path) -> None:
    assembly = output.with_suffix(".S")
    assembly.write_text(
        ".global _start\n.text\n_start:\n"
        f"{source}\n"
        '.section .note.GNU-stack,"",@progbits\n',
        encoding="utf-8",
    )
    subprocess.run(
        [
            "cc",
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


def parse_qemu_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run real-ELF controls for the release QEMU CPU model"
    )
    parser.add_argument("--qemu-x86-64", required=True)
    parser.add_argument("--qemu-cpu", required=True)
    return parser.parse_args()


if __name__ == "__main__":
    qemu_args = parse_qemu_args()
    run_cpu_emulation_controls(qemu_args.qemu_x86_64, qemu_args.qemu_cpu)
    print("QEMU CPUID and instruction controls passed")
