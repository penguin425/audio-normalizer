# Forge

Audio loudness normalization and delivery quality control in Rust.

Forge measures EBU R128 / ITU-R BS.1770-5 loudness and true peak. It can
normalize files, albums, and directory trees, or analyze audio without changing
it.

[Latest release](https://github.com/penguin425/audio-normalizer/releases/latest)
· [Documentation](DOCUMENTATION.md)
· [Changelog](CHANGELOG.md)
· [Roadmap](ROADMAP.md)

## Quick start

Download a platform archive from the
[latest GitHub Release](https://github.com/penguin425/audio-normalizer/releases/latest),
or build the default WAV/FLAC-capable version from source with Rust 1.89 or
newer:

```sh
cargo build --release
```

Normalize one file to -16 LUFS with a -1 dBTP ceiling:

```sh
forge input.wav -o output.wav --target=-16
```

Common workflows:

```sh
# Measure without writing audio
forge --analyze input.flac

# Normalize an album with one shared gain
forge --album album/*.flac -o normalized/

# Process a directory and preserve its layout with a recoverable generation
forge library/ --recursive -o normalized/ --job-state work/library-job.json

# Use a named delivery target and verify the encoded result
forge input.wav -o output.flac --preset spotify --verify

# Emit machine-readable analysis
forge --analyze input.wav --json

# Use the fixed-order reference meter and record its engine ID
forge --analyze input.wav --analysis-engine reference --json

# Select the second audio track in a multi-track container
forge --analyze programme.m4a --audio-track 1 --json
```

Run `forge --help` for the complete option list.

## Main features

- Integrated, momentary, and short-term loudness, LRA, sample peak, and true
  peak measurement.
- Track and duration-weighted album normalization.
- Configurable true-peak ceiling, optional look-ahead limiter, resampling, and
  integer PCM dither.
- Bounded recursive discovery and lock-protected, crash-resumable batch/watch
  processing. Multi-file normalization with `--job-state` publishes an
  all-or-nothing generation for the audio destination set only; catalogue
  records and auxiliary reports are separate post-audio writes. JSON, CSV, and
  NDJSON reports are available across the applicable workflows.
- Atomic output publication with no-clobber or unchanged-destination checks.
- Output re-verification, ReplayGain, native M4A/ALAC `tlou`/`alou`, BWF
  metadata, and delivery compliance profiles.
- Explicit metadata-fidelity policies, bounded registry-backed full-container
  inventory, exact sample-clock conversion for supported timing fields, and
  restartable single-file metadata-only transactions with field-level evidence.
- Bounded parsers and companion QC tools for broadcast, streaming, immersive,
  and packaged media.

## Formats

| Format | Read | Write |
| --- | --- | --- |
| WAV / RF64 / BW64 | Yes | Yes |
| FLAC | Yes | Yes |
| MP3 | Yes | With `mp3-encoding` and LAME |
| Ogg Opus | With `opus-encoding` | With `opus-encoding` |
| AAC / ALAC / Vorbis | Yes | With `ffmpeg-encoding` and FFmpeg |
| DSF / DSDIFF | Uncompressed input | No |

Release archives include Opus support and the FFmpeg adapter. AAC-LC, ALAC,
and Vorbis output still require `ffmpeg` on `PATH`. Output otherwise follows
the content-probed input codec when a compatible encoder is available, or
falls back to WAV. Forge checks the exact FFmpeg encoder and muxer before it
creates an output; lossless ALAC input therefore never defaults to lossy AAC.

## Optional source features

| Cargo feature | Adds |
| --- | --- |
| `mp3-encoding` | MP3 output through system `libmp3lame` |
| `opus-encoding` | Statically linked Ogg Opus input and output |
| `ffmpeg-encoding` | AAC-LC, ALAC, and Vorbis output through FFmpeg |
| `cuda-truepeak` | Optional NVIDIA true-peak worker on Linux and Windows |
| `clap-plugin`, `lv2-plugin` | Real-time plug-in targets |
| `grpc-service`, `onnx-provider` | Optional service and anomaly-provider APIs |

The default build does not require LAME, FFmpeg, CUDA, or a plug-in SDK.

## Companion tools

Release archives include focused commands such as `forge-live`,
`forge-container-qc`, `forge-streaming-qc`, `forge-compare`,
`forge-audio-compare`, and `forge-service`. Run `forge-doctor` to inspect the
current build's format, encoder, runtime, and CPU capabilities.
`forge-adm-presentation-qc` audits every ADM programme and complementary-object
render through the EBU reference renderer; `forge-adm-interactivity-qc` audits
bounded personalization ranges, and `forge-adm-semantics-qc` checks dialogue,
selection, importance, and tag semantics without claiming rendered-audio
compliance.
`forge-metadata-repair` can add
measured ISO-BMFF `ludt/tlou` and combined-album `alou` loudness metadata
without re-encoding media. Other commands cover IMF, AES31,
RTP/AES67/ST 2110, NMOS, codec adapters, remediation, and multi-delivery
workflows.

### Metadata fidelity

Metadata-bearing file workflows accept an explicit `--metadata-policy`
selection: `preserve` reports best-effort mappings and losses, `strict` blocks
publication on any loss, and `strip` requires an explicit all-fields or exact
locator scope. `legacy-generic` keeps the historical primary/first generic-tag
behavior available, and remains the default when no policy is supplied. The
field-level report uses the versioned
[metadata-fidelity contract](METADATA-FIDELITY.md); the bounded inventory
records repeated and unknown fields, physical extents, hashes, and structural
regions for supported WAVE, FLAC, MP3, Ogg, and ISO-BMFF containers.

When resampling, supported sample-indexed BWF/DAW timing is converted with exact
rational sample-clock arithmetic and an explicit rounding rule. XML-bearing
timing is retained as opaque evidence. `--metadata-job-state` persists and
resumes one metadata-only file transaction with staged verification and
compare-and-swap publication. Its state and source-parent directories must be
trusted against hostile writers; the journal is validated but not
cryptographically authenticated. The historical `--write-tags` path without a
job state writes requested tag families sequentially and does not provide that
transaction guarantee.

For multi-file normalization, `--job-state` uses a v3 semantic job identity and
the sibling `<job-state>.generation.json` journal. All pending audio outputs are
staged and verified before the generation is published; album shared-gain runs,
`--verify`, bounded `--keep-going`, and `forge recovery inspect/reclaim` are
documented in [BATCH-JOBS.md](BATCH-JOBS.md). Dry runs have no durable side
effects except explicit `--warm-cache` analysis-cache warming. Stages,
destinations, and private backups are sibling paths on one filesystem. The
no-clobber publication uses `renameat2(RENAME_NOREPLACE)` on Linux/Android,
`renamex_np(RENAME_EXCL)` on Apple platforms, and the corresponding
write-through Windows primitive; unsupported Unix targets fail closed.

See the [documentation map](DOCUMENTATION.md#command-line-tools) or run any
command with `--help`.

## APIs and integrations

Forge provides a Rust library, a versioned [C API](C-API.md),
[Python wheels](PYTHON-API.md), and a browser
[WebAssembly package](WASM-PACKAGE.md). Source integrations for FFmpeg,
GStreamer, VST3, and Audio Unit are documented in
[HOST-ADAPTERS.md](HOST-ADAPTERS.md),
[VST3-ADAPTER.md](VST3-ADAPTER.md), and [AU-ADAPTER.md](AU-ADAPTER.md).
Native C ABI archives retain their historical root-level library copies and
also provide the canonical `include/` and `lib/` layout. CMake consumers use
`find_package(ForgeNormalizer CONFIG REQUIRED)` and `Forge::Normalizer`; Linux
and macOS consumers may use `pkg-config --cflags --libs forge-normalizer`.
Only the dynamic library is shipped, and runtime loader-path configuration is
owned by the consumer. See [C-API.md](C-API.md) for the archive layout and
compatibility contract.

## Releases and verification

Tagged releases from v0.189.17 onward contain platform archives, Python wheels,
checksums, per-artifact SPDX and CycloneDX SBOMs, and SLSA provenance. Linux
and Apple Silicon release builds also pass independent reproducibility checks
before publication.
The official generic Linux native archive and wheel floor is x86-64 with glibc
2.34 or newer. Those generic artifacts use x86-64-v1 flags in a pinned
manylinux 2.28 build and ABI-stress environment; this is not a glibc 2.28
wheel-install claim. The supplemental x86-64-v3 CLI is built in the same
pinned environment with an explicit v3 target. The ordinary Linux archive is
built, ELF-inspected, and runtime-smoked at the official floor.

The v0.189.17 Linux ARM64 release contract adds
`forge-v<VERSION>-linux-aarch64.tar.gz` and
`forge_normalizer-<VERSION>-py3-none-manylinux_2_34_aarch64.whl`. These are
generic AArch64/ARMv8-A (mandatory NEON only) builds: they do not claim
CPU-specific extensions such as SVE or a cryptographic extension, and they
require glibc 2.34 or newer. The archive carries the same native C ABI layout
and relocation checks as the
other full native archives. `cargo-binstall` and the generated Homebrew
formula select this archive on `aarch64-unknown-linux-gnu`; the x86-64-v3
archive remains a CLI-only supplemental artifact.

Every public release file is selected by an exact, versioned manifest. The
manifest records the filename, byte length, SHA-256 digest, artifact class, and
the corresponding verification evidence. Each distributable has its own
SPDX/CycloneDX evidence and is included in the SLSA subject set; the aggregate
checksum and provenance files are not a substitute for an artifact entry.
Missing, extra, duplicate, symlinked, or digest-mismatched files fail before
the immutable GitHub Release is made public. Build-input evidence such as PGO
profiles is published only when it is explicitly listed, never because a
download glob happened to match it.

Use the checksums and attestation bundle shipped with each
[GitHub Release](https://github.com/penguin425/audio-normalizer/releases).

### Registry publication boundary

The repository prepares, but does not by itself claim, registry publication.
When the external publisher configuration is present, the v0.189.17 release
workflow will publish the exact verified artifacts with trusted OIDC
publishers: the Python wheel set to PyPI, the public browser package to npm,
and the Rust crate to crates.io. Build jobs never receive registry credentials;
the crates.io job uses the official `rust-lang/crates-io-auth-action`
short-lived OIDC token exchange and exposes it only as
`CARGO_REGISTRY_TOKEN` to the publish step.
Each registry job consumes only the manifest-checked artifact bundle. The
local manifest binds every byte length and SHA-256 digest; retry reconciliation
then checks the registry's exact package identity and integrity fields (PyPI
SHA-256 and size, npm integrity/SHA-1, or crates.io checksum). A missing
publisher, a first-release/bootstrap requirement, or a registry digest
mismatch blocks publication rather than being reported as a successful
release. Until those checks pass, install from the GitHub Release and do not
infer that a package exists on any registry.

Before creating a v0.189.17 tag, repository administrators must provide the
four protected GitHub environments `release`, `pypi`, `npm`, and `crates-io`.
The `release` environment must expose
`FORGE_RELEASE_POLICY_APP_CLIENT_ID` as a variable and
`FORGE_RELEASE_POLICY_APP_PRIVATE_KEY` as a secret for a GitHub App installed
only on this repository with repository Administration write permission.
GitHub exposes ruleset bypass actors only at that permission level; the
publisher uses the short-lived App token for GET requests only and fails closed
if the field is absent or nonempty. Release creation and uploads still use the
built-in contents-only token.

The three registry trusted publishers must name repository
`penguin425/audio-normalizer`, workflow `release.yml`, and their exact
environment (`pypi`, `npm`, or `crates-io`). PyPI may use a pending publisher
to create the project. npm and crates.io require the exact v0.189.16 package to
be bootstrapped manually before their trusted publishers can be registered;
the npm trusted-publisher entry must permit publishing, and the npm scope must
already be controlled by the project owner. These are deployment prerequisites,
not credentials accepted by build jobs.

Cargo's stable publisher cannot upload a pre-existing `.crate` path: it always
repackages before upload. The workflow therefore compares an independent
`cargo package` result with the attested asset before acquiring credentials,
runs the official `cargo publish`, compares Cargo's final local package again,
and requires the immutable crates.io checksum to equal the asset. This is the
strongest boundary available through the supported Cargo client, but the final
registry comparison necessarily detects a hypothetical mismatch only after
crates.io has accepted that non-replaceable version.

Windows ARM64 binaries, OCI images or multi-architecture indexes, macOS
notarization/stapling, and Authenticode signatures are deliberately outside
the v0.189.17 contract. They remain demand-, platform-verification-, and
credential-gated follow-up work; no current archive should be interpreted as
having any of those properties.

## Documentation

- [Documentation map and command index](DOCUMENTATION.md)
- [Browser WebAssembly package](WASM-PACKAGE.md)
- [JSON schemas and version registry](SCHEMA-REGISTRY.md)
- [Compatibility and deprecation policy](COMPATIBILITY.md)
- [Rust API stability policy](API-STABILITY.md)
- [Performance methodology](PERFORMANCE.md)
- [Implementation roadmap](ROADMAP.md)
- [Security policy](SECURITY.md)
- [Contributing](CONTRIBUTING.md)

## Scope and limitations

- File normalization is an offline two-pass workflow. `forge-live` provides
  bounded real-time gain control, not final integrated-LUFS normalization.
- Lossy encoding can move loudness and true peak; use `--verify` when delivery
  tolerances matter.
- Compliance and QC reports cover their documented checks. They are not a
  substitute for third-party certification or listening review.
- Some encoders, hardware acceleration, and host adapters require the optional
  dependencies listed above.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --no-default-features
```

Performance and conformance procedures are described in
[BENCHMARKS.md](BENCHMARKS.md) and the project workflows under
`.github/workflows/`.

## License

[MIT](LICENSE)
