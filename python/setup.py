"""Wheel tagging for the ABI-independent Python wrapper and bundled library."""

from __future__ import annotations

import os
import re

from setuptools import Distribution, setup
from setuptools.command.bdist_wheel import bdist_wheel


class ForgeBinaryDistribution(Distribution):
    """Treat the bundled native library as a platform payload."""

    def has_ext_modules(self) -> bool:
        return True


class ForgePlatformWheel(bdist_wheel):
    """Emit a py3-none platform wheel whose payload lives in platlib."""

    def finalize_options(self) -> None:
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self) -> tuple[str, str, str]:
        platform_tag = os.environ.get("FORGE_WHEEL_PLATFORM", "")
        if not re.fullmatch(r"[A-Za-z0-9_.]+", platform_tag):
            raise RuntimeError("FORGE_WHEEL_PLATFORM must be a valid wheel platform tag")
        return ("py3", "none", platform_tag)


setup(
    cmdclass={"bdist_wheel": ForgePlatformWheel},
    distclass=ForgeBinaryDistribution,
)
