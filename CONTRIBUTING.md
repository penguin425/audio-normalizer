# Contributing to Forge

Thank you for improving Forge. Changes should preserve deterministic loudness
results, bounded processing, and observable standards evidence.

## Before changing code

Use an issue or draft pull request for a new public contract, codec, normative
interpretation, or large dependency. Security reports belong in the private
channel described in [SECURITY.md](SECURITY.md).

Keep normative behavior separate from optional perceptual or model-based
features. A standards change should cite the exact public specification and
include both a conforming fixture and a failing fixture.

## Local checks

Forge requires Rust 1.89 or newer. The main CI toolchain is pinned separately
so that lint changes are reviewed deliberately.

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --no-default-features -- -D warnings
cargo test --locked --no-default-features
```

Run the feature-specific tests affected by the change. For example:

```sh
cargo test --locked --features opus-encoding
cargo test --locked --features ffmpeg-encoding
cargo test --locked --features mp3-encoding
```

Optional native dependencies are documented in the README. Do not run
`cargo clean` routinely: build artifacts are reusable and can be expensive.
When investigating performance, follow [BENCHMARKS.md](BENCHMARKS.md) and
compare against the pull request's base commit.

## Pull requests

- Keep generated files, lockfiles, schemas, tests, and documentation consistent
  with the implementation.
- Preserve unrelated work and avoid drive-by formatting or dependency changes.
- Add a changelog entry for a user-visible change. Maintainers assign the
  release version when a batch is ready.
- Do not weaken byte, sample, recursion, process, or timeout bounds to make a
  fixture pass.
- Describe the commands used for verification and any platform or optional
  dependency that was not available locally.

The protected `main` branch requires Rust, EBU, and ITU conformance checks and
linear history. Pull requests are squash-merged after those checks pass.

## Compatibility

Public Rust, C, Python, CLI, and JSON changes must follow
[COMPATIBILITY.md](COMPATIBILITY.md). Intentional breaking changes require a
migration note and the corresponding major schema, ABI, or package version.
