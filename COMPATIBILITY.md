# Compatibility and deprecation policy

Forge uses separate compatibility boundaries for its library, command-line
tools, native ABI, language binding, and machine-readable reports. A version
number in one boundary does not silently redefine another.

## Rust library and Cargo features

The documented Rust API and public Cargo feature names follow
[API-STABILITY.md](API-STABILITY.md). Forge applies that source-compatibility
contract across pre-1.0 releases. The minimum supported Rust version is stated
in `Cargo.toml`; an increase is announced in the changelog.

## C and Python APIs

The C ABI uses explicitly versioned symbols and structures as documented in
[C-API.md](C-API.md). An incompatible ABI requires a new ABI version; existing
versioned entry points remain available for their documented lifetime.

Native archives also expose the relocatable CMake package
`find_package(ForgeNormalizer CONFIG REQUIRED)` with the `Forge::Normalizer`
target, and the `forge-normalizer` pkg-config module on Linux and macOS. These
metadata names and the canonical `include/`/`lib/` layout are stable within the
C ABI major version. Archives provide the dynamic library only; runtime loader
paths are configured by the consumer, and a Windows `.lib` is an import library
for the DLL.

The Python package wraps the versioned native library. Documented Python names
and call signatures are additive within a major package version. Platform wheel
availability is a release property, not an API guarantee. The official generic
Linux archive and wheel target x86-64 with glibc 2.34 or newer; those artifacts
use x86-64-v1 flags in the pinned manylinux 2.28 build and ABI-stress
environment, which is not a glibc 2.28 wheel compatibility claim. The
v0.189.17 ARM64 pair uses the `aarch64`/`manylinux_2_34_aarch64` names, a
generic ARMv8-A (mandatory NEON only) baseline, and the same glibc 2.34 runtime
floor. The supplemental x86-64-v3 CLI is built separately with its explicit
ISA target.

### Distribution evidence and registry boundary

The release manifest is the authority for public distribution. It enumerates
each archive, wheel, WASM package, and registry payload by exact name, byte
length, SHA-256 digest, and artifact class. The corresponding per-artifact
SPDX/CycloneDX evidence and SLSA subject are checked against that entry before
immutable publication. A checksum file or aggregate SBOM does not authorize an
unlisted file. Registry retries must reconcile the exact remote package and
the integrity data exposed by that registry; the manifest separately binds
the local size and SHA-256 digest.

The v0.189.17 workflow targets trusted OIDC publishing for PyPI, npm, and
crates.io. It is a release mechanism, not a statement that those registries
already contain Forge: publisher configuration and any required first-release
bootstrap are external prerequisites. Build and verification jobs receive no
registry credentials, and a missing publisher or digest mismatch fails closed.
Cargo's stable publisher always repackages its input, so the crate job compares
both the pre-publish and final local packages with the attested `.crate`, then
requires crates.io's immutable checksum to match. A hypothetical final mismatch
is detectable but cannot be rolled back after registry acceptance.

Windows ARM64, OCI images/indexes, macOS notarization or stapling, and
Authenticode signatures are not compatibility claims for v0.189.17. Each is a
demand- and credential-gated follow-up that requires a real platform build,
signature/registry verification, and corresponding release-manifest entries.

## JSON, TOML, XML, and protobuf contracts

Every stable machine-readable request or report identifies its schema or
protocol version. Within one schema version, Forge may add optional fields and
new symbolic values only where the schema already permits them. It does not
remove required fields, change units, reuse a rule identifier for different
semantics, or reinterpret an existing enum value.

An incompatible contract gets a new schema ID or protocol version. Readers
should ignore optional fields only when the referenced schema permits that
behavior and must reject an unsupported required version.

## Command-line interface

Documented command names, option names, option value syntax, exit-status
meanings, and machine-readable stdout modes are compatibility surfaces. Human
diagnostic wording, progress display, help layout, and ordering of independent
warnings are not stable interfaces.

An option scheduled for removal is first documented as deprecated and retains
its behavior for at least two subsequent feature releases. The changelog names
the replacement and earliest removal version. Immediate removal is reserved
for a security issue, a standards violation, or behavior that can corrupt or
overwrite data; such a change is called out prominently in release notes.

## Behavioral corrections

Standards corrections, tighter validation, bounded-resource enforcement, and
post-encode measurement may change numeric output without changing an API
shape. Each correction must identify its measurement basis and add regression
evidence. Forge never treats a compatibility promise as permission to retain a
known incorrect loudness or true-peak result.
