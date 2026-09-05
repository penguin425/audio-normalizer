#!/usr/bin/env python3
"""Run Docker only with an immutable, content-addressed container image."""

from __future__ import annotations

import os
import re
import sys
from collections.abc import Sequence


PINNED_IMAGE = re.compile(
    r"^[A-Za-z0-9][A-Za-z0-9._:/-]*@sha256:[0-9a-f]{64}$"
)


class PinnedContainerError(ValueError):
    """The requested operation is not an immutable container invocation."""


def image_is_pinned(image: str) -> bool:
    return PINNED_IMAGE.fullmatch(image) is not None


def docker_command(arguments: Sequence[str]) -> list[str]:
    if len(arguments) < 2:
        raise PinnedContainerError(
            "usage: run-pinned-container.py (pull IMAGE | "
            "run IMAGE [DOCKER_OPTIONS] -- [COMMAND ...])"
        )

    operation, image, *remainder = arguments
    if not image_is_pinned(image):
        raise PinnedContainerError(
            f"container image must use a full sha256 digest: {image!r}"
        )

    if operation == "pull":
        if remainder:
            raise PinnedContainerError("pull accepts exactly one image")
        return ["docker", "pull", image]

    if operation != "run":
        raise PinnedContainerError(f"unsupported container operation: {operation}")
    try:
        delimiter = remainder.index("--")
    except ValueError as error:
        raise PinnedContainerError(
            "run requires -- between Docker options and the container command"
        ) from error

    docker_options = remainder[:delimiter]
    container_command = remainder[delimiter + 1 :]
    if not container_command:
        raise PinnedContainerError("run requires a container command after --")
    return ["docker", "run", *docker_options, image, *container_command]


def main() -> int:
    try:
        command = docker_command(sys.argv[1:])
    except PinnedContainerError as error:
        print(f"pinned container invocation rejected: {error}", file=sys.stderr)
        return 2
    try:
        os.execvp(command[0], command)
    except OSError as error:
        print(f"cannot execute Docker: {error}", file=sys.stderr)
        return 127


if __name__ == "__main__":
    raise SystemExit(main())
