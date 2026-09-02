# Forge documentation

The [README](README.md) covers installation and common commands. This page is
an index for the focused references kept in the repository. The CLI remains the
authoritative option reference:

```sh
forge --help
forge-container-qc --help
```

## Core workflows

- [Resumable batch jobs](BATCH-JOBS.md)
- [Watch folders](WATCH-FOLDERS.md)
- [Content-addressed analysis cache](ANALYSIS-CACHE.md)
- [SQLite catalogue](CATALOGUE.md)
- [Multi-delivery optimization](MULTI-DELIVERY.md)
- [Segment-aware normalization](SEGMENT-NORMALIZATION.md)
- [Remediation planning](REMEDIATION.md)
- [Metadata repair](METADATA-REPAIR.md)

## Quality control and codec adapters

- [EBU QC Scenario 1 XML reports](EBU-QC-SCENARIO1.md)
- [ATSC A/85 streaming-service QC](ATSC-A85-SERVICE-QC.md)
- [ADM programme and complementary-presentation QC](ADM-PRESENTATION-QC.md)
- [ADM personalization-range QC](ADM-INTERACTIVITY-QC.md)
- [ADM content and presentation semantics QC](ADM-SEMANTICS-QC.md)
- [ADM emission-profile QC](ADM-EMISSION-QC.md)
- [Binaural renderer QC](BINAURAL-QC.md)
- [Immersive downmix QC](IMMERSIVE-DOWNMIX.md)
- [External anomaly-provider protocol](ANOMALY-ADAPTER.md)
- [AC-4 reference-decoder adapter](AC4-ADAPTER.md)
- [DTS reference-decoder adapter](DTS-ADAPTER.md)
- [MPEG-H adapter](MPEGH-ADAPTER.md)

Machine-readable contracts are under [`schema/`](schema/). Each report names
its schema version and the bounded checks it performs.

## APIs and host integration

- [C API](C-API.md)
- [Python API](PYTHON-API.md)
- [FFmpeg and GStreamer adapters](HOST-ADAPTERS.md)
- [VST3 adapter](VST3-ADAPTER.md)
- [Audio Unit adapter](AU-ADAPTER.md)
- [Compatibility and deprecation policy](COMPATIBILITY.md)
- [Rust API stability policy](API-STABILITY.md)

## Operations and engineering

- [Service metrics](SERVICE-METRICS.md)
- [Performance plan](PERFORMANCE.md)
- [Benchmark harness](BENCHMARKS.md)
- [Implementation roadmap](ROADMAP.md)
- [Next-generation plan](NEXT-GENERATION-PLAN.md)
- [Changelog](CHANGELOG.md)
- [Security policy](SECURITY.md)
- [Contributing](CONTRIBUTING.md)

## Command-line tools

The release contains the main `forge` normalizer plus focused binaries:

| Area | Commands |
| --- | --- |
| Diagnostics | `forge-doctor` |
| Streaming and comparison | `forge-live`, `forge-compare`, `forge-audio-compare` |
| Containers and packages | `forge-container-qc`, `forge-streaming-qc`, `forge-imf-qc`, `forge-aes31-qc`, `forge-provenance-qc` |
| Network delivery | `forge-rtp-qc`, `forge-st2022-7-qc`, `forge-nmos-qc`, `forge-remote-qc` |
| Immersive and codecs | `forge-adm-presentation-qc`, `forge-adm-interactivity-qc`, `forge-adm-semantics-qc`, `forge-adm-emission-qc`, `forge-presentation-qc`, `forge-downmix-qc`, `forge-binaural-qc`, `forge-sadm-qc`, `forge-ac4-qc`, `forge-dts-qc`, `forge-mpegh-qc` |
| Automation | `forge-multi-delivery`, `forge-segment-normalize`, `forge-remediate`, `forge-metadata-repair`, `forge-report`, `forge-service` |
| Providers | `forge-dialogue-provider`, `forge-anomaly-provider`, `forge-onnx-provider` |

Some binaries require the Cargo features listed in [`Cargo.toml`](Cargo.toml).
Use `<command> --help` for its inputs, limits, output schemas, and exit codes.
`forge-sadm-qc` accepts S-ADM XML frame documents in transport order; divided
chunks with a shared base `frameFormatID` must be adjacent and ordered by chunk
index. It validates the normative frame paths and version declaration, then
reconstructs logical ADM state and checks any declared `changedIDs` status
transitions after all chunks for a logical frame have been combined. XML
parsing and flow reconstruction use fixed file, byte, depth, element,
attribute, text, namespace-expansion, and canonical-state limits. Non-document
or malformed XML, namespace lookalikes, and known S-ADM elements at invalid
paths are rejected before a QC report is produced.
