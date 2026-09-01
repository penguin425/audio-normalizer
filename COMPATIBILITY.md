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

The Python package wraps the versioned native library. Documented Python names
and call signatures are additive within a major package version. Platform wheel
availability is a release property, not an API guarantee.

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
