# Forge Rust API stability

Forge provides a source-compatibility contract for the `forge_normalizer`
Rust library starting with release v0.94.0.

## Contract

- Code that compiles against a documented public item in v0.94.0 or later
  should continue to compile after upgrading Forge.
- The contract applies to documented public items exposed by optional Cargo
  features when the same features are enabled.
- Removing or renaming a public Cargo feature is a compatibility break.
- Forge applies this policy across its pre-1.0 `0.x` releases. In other words,
  a `0.x` minor version is not permission to break the documented Rust API.
- The minimum supported Rust version is 1.89. A future MSRV change is announced
  separately and is not inferred from the library API check.

The contract covers source compatibility, not bit-for-bit behavioural
identity. Standards corrections, security fixes, more precise measurements,
and newly rejected invalid inputs may change results without changing the
public Rust shape. Versioned JSON schemas and report rule identifiers retain
their own explicit compatibility contracts.

## Exclusions

The following are not stable Rust API:

- items marked `#[doc(hidden)]`;
- private modules and implementation details;
- plugin C ABIs exported for CLAP or LV2 hosts;
- command-line text intended for humans;
- undocumented effects of native codecs or third-party tools.

An optional feature may require its documented native/runtime dependency.
Enabling `mp3-encoding`, for example, still requires LAME.

## Enforcement

The `Rust API compatibility` CI job:

1. selects the highest semantic release tag reachable from the pull request;
2. compares that tag with the proposed all-feature API;
3. forces patch-level compatibility even when Cargo would normally permit a
   breaking change between pre-1.0 minor versions; and
4. fails the pull request when `cargo-semver-checks` reports a denied change.

`tests/public_api.rs` is compiled as a downstream crate and exercises the
main analysis, stable-input/bound-analysis, normalization, WAV, preset, and
real-time entry points. It complements structural API inspection with ordinary
consumer code.

No static checker can recognize every Rust or behavioural compatibility
break. Reviewers must still assess public type changes, generic and lifetime
changes, feature subsets, documented semantics, and downstream behaviour.

## Intentional breaking changes

An intentional breaking change requires all of the following:

1. a dedicated proposal describing affected downstream code and migration;
2. a major-version release (1.x to 2.x once Forge is 1.0 or later);
3. updated contract tests, documentation, and release notes; and
4. an explicit, reviewed CI-policy change in the same release series.

Suppressing a semver lint solely to make a pull request pass is not an
acceptable migration.

The separately versioned Forge C ABI is governed by
[`C-API.md`](C-API.md), not by Rust source compatibility.
