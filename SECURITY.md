# Security policy

## Supported versions

Security fixes are made on `main` and released in the newest stable Forge
version. Older releases are not maintained as separate security branches.
Before reporting a problem, reproduce it with the latest release or the current
`main` branch when practical.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's
[private vulnerability reporting](https://github.com/penguin425/audio-normalizer/security/advisories/new)
so that details and a fix can be coordinated without exposing users.

Include the affected Forge version or commit, operating system, enabled Cargo
features, a minimal reproducer, expected impact, and any relevant resource
limits. Remove private audio or credentials from the reproducer. If a media
file is required, provide the smallest synthetic file that demonstrates the
problem.

Relevant reports include, but are not limited to:

- memory-safety or parser-boundary failures in untrusted media;
- path traversal, unintended overwrite, or command execution;
- authentication or isolation failures in the optional service;
- denial of service that bypasses documented byte, sample, count, or time
  limits; and
- release, dependency, or provenance weaknesses that could distribute
  untrusted binaries.

Ordinary loudness disagreements, unsupported formats, and feature requests are
not security reports unless they create a concrete safety or trust-boundary
failure.

## Release verification

Official GitHub Releases include SHA-256 checksums, SPDX and CycloneDX SBOMs,
and GitHub build-provenance attestations. Verify those artifacts before using
Forge in a trusted delivery pipeline. Release archives are built only from an
annotated semantic-version tag after the protected CI checks pass.
