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
[Python wheels](PYTHON-API.md), a browser WebAssembly package, and real-time
host adapters. Source integrations for FFmpeg, GStreamer, VST3, and Audio Unit
are documented in [HOST-ADAPTERS.md](HOST-ADAPTERS.md),
[VST3-ADAPTER.md](VST3-ADAPTER.md), and [AU-ADAPTER.md](AU-ADAPTER.md).

## Releases and verification

Tagged releases contain platform archives, Python wheels, checksums, SPDX and
CycloneDX SBOMs, and SLSA provenance. Linux and Apple Silicon release builds
also pass independent reproducibility checks before publication.

Use the checksums and attestation bundle shipped with each
[GitHub Release](https://github.com/penguin425/audio-normalizer/releases).

## Documentation

- [Documentation map and command index](DOCUMENTATION.md)
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
