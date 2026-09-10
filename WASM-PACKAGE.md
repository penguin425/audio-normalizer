# Forge WebAssembly package

Forge also ships a browser-oriented WebAssembly package. It performs bounded
local analysis in an ES module; it does not normalize, encode, access the
network, or read the host filesystem.

The package name reserved by the release contract is
`@forge-normalizer/wasm`. The repository does not claim that this package has
already been published to npm. After the external npm package and trusted
publisher are configured, installation will be:

```sh
npm install @forge-normalizer/wasm
```

Until that configuration and a successful registry reconciliation exist, use
the versioned `forge-v<VERSION>-wasm-web.tar.gz` GitHub Release asset. Verify
its entry in the exact release manifest and `SHA256SUMS` before unpacking it.

## Browser use

The package exports the WebAssembly initializer as its default export and the
analysis helpers as named exports:

```js
import init, { analyzeWav, limits } from "@forge-normalizer/wasm";

await init();
const result = analyzeWav(new Uint8Array(await file.arrayBuffer()));
console.log(result.integratedLufs, result.truePeakDbtp, limits());
```

`analyzeWav` accepts bounded PCM or IEEE-float WAVE/RF64/BW64 data. Use
`analyzeWavWithLayout` when a multichannel input needs an explicit exact
speaker descriptor. `analyzeInterleaved` accepts mono or stereo decoded
`Float32Array` PCM; `analyzeInterleavedWithLayout` is the corresponding API for
multichannel data. See the package `README.md` and `index.d.ts` for the full
types, limits, and layout contract.

The generated npm tarball must contain only the package files declared by
`wasm/package/package.json`, with a version matching the tagged source. The
build produces the browser archive and npm tarball from the same generated
WASM bindings and checks both for deterministic bytes before publication.

## npm trusted publishing

The intended npm publication job is separate from build and attestation jobs.
It receives only the exact manifest-checked tarball, uses npm trusted
publishing through GitHub Actions OIDC, and does not receive an npm token. The
job must assert a supported Node/npm toolchain (npm trusted publishing
requires Node 22.14 or newer and npm 11.5.1 or newer), the public package
metadata, and the canonical GitHub repository identity before invoking
`npm publish --provenance`.

The package name/version is immutable on npm. A retry is successful only when
the npm registry reports the same name, version, integrity, and SHA-1 tarball
digest; the release manifest separately binds the local byte length and
SHA-256 digest. An existing version with different bytes or an unexpected
extra package file is a release blocker. OIDC publisher configuration, npm
scope ownership, and any first-package bootstrap are external prerequisites,
so this document does not assert that npm publication has occurred.

The v0.189.16 package metadata is `private`, so that tag cannot be used as an
exact npm seed. The one-time first-package exception is the explicitly named
`0.189.17-bootstrap.0` prerelease, built from the protected v0.189.17 source
and published under the non-default `bootstrap` dist-tag. It is not a stable
release; the manual bootstrap command omits `--provenance` and does not create
a trusted-OIDC attestation. After it exists, configure the `release.yml` /
`npm` trusted publisher with direct publishing allowed; the stable v0.189.17
package must still be published through OIDC. npm 11.15.0 or newer, account
2FA, and package write permission are required for that configuration.

## Evidence and unsupported release targets

The exact release manifest records the WASM archive and npm tarball separately,
including names, sizes, SHA-256 digests, artifact class, and per-artifact
SPDX/CycloneDX evidence. The SLSA subject set and checksum file must cover the
same bytes. A broad artifact glob must never add a PGO file, source tree, or
unlisted generated file to the public release.

Windows ARM64, OCI images/indexes, macOS notarization/stapling, and
Authenticode signatures are outside the v0.189.17 WASM/package contract. They
remain gated follow-up work and must not be inferred from an npm provenance
statement or a GitHub archive checksum.
