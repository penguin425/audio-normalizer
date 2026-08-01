# Next-generation audio normalization plan

This plan turns the research proposal into independently reviewable,
release-sized work.  Forge already has deterministic BS.1770/EBU measurement,
baseband anomaly rules, external dialogue-provider and immersive-renderer
boundaries, bounded DASH observation, LV2/CLAP, and MXF/BWF/ADM QC.  The work
below fills the remaining gaps without making experimental behaviour part of a
standards-compliance result.

## Delivery order

| Stage | PR-sized deliverable | Dependencies and release gate |
| --- | --- | --- |
| 1 | External audio anomaly provider v1: provenance-bound noise/pop/dropout/lip-noise/phase-cancellation findings and bounded audit | No model runtime; JSON schema, negative fixtures, fuzz/resource limits. First feature release. |
| 2 | Anomaly evidence bridge: import the audit into delivery manifests and `forge-report explain` as a separate `model-qc` layer | Stage 1; stable category/rule IDs and explicit “non-normative model evidence” wording. |
| 3 | Optional ONNX provider SDK and reference adapter (CPU first; Demucs-like dialogue separation only through the same boundary) | Stage 1; pinned model SHA/licence, calibration report, deterministic fallback, CPU/memory/time limits. Never a default dependency. |
| 4 | Bounded remote range reader for explicitly allow-listed S3/GCS/HTTPS objects, then `forge-remote-qc` for header/stream analysis without a full local download | Reuse DASH observation security rules; Range/redirect/request/byte/time caps, redacted fetch manifest, no credentials in reports. |
| 5 | Stateless REST/gRPC service mode with job/request schemas, concurrency and decoded-sample limits, cancellation, and authentication hooks | Stage 4; service is an optional binary/feature and must not alter the library default. |
| 6 | Prometheus text exporter and OpenTelemetry span/attribute bridge | Stage 5; bounded labels (no paths, model payloads, or source identifiers), opt-in endpoint/exporter, deterministic counters. |
| 7 | FFmpeg and GStreamer integration adapters over the existing C ABI/live processor | Define frame/latency/flush semantics first; ABI tests and Linux package smoke tests. |
| 8 | VST3 and Audio Unit wrappers | Separate platform PRs, host lifecycle/latency tests, licensing and signing documentation; keep LV2/CLAP parity gates. |
| 9 | Immersive downmix profiles for stereo/5.1/7.1.4 plus loudness/true-peak delta and clip-risk reports | Reuse existing WAVE-order downmix and external presentation QC; coefficients and channel mapping must be explicit and fixture-backed. |
| 10 | Binaural verification through an explicitly selected renderer, with renderer/model/version/hash evidence | No bundled proprietary renderer; compare duration, loudness, true peak, channel/layout and reference drift. |
| 11 | Smart remediation dry-run planner for true peak/LRA, producing a minimal-change plan before writing audio | Never rewrite in place by default; preserve original, settings hash, before/after evidence, and infeasibility reasons. |
| 12 | BWF/MXF/ADM metadata repair, first as copy-to-new-file and then optional atomic replacement | Standards-specific validators must run before and after; preserve unknown chunks/XML, timecode provenance, and byte/count limits. |

## Scope decisions

AI quality detection is an advisory layer.  It may flag audio for review but
must not redefine LUFS targets, infer SPL from digital level, or silently apply
restoration.  Dialogue separation, preference learning, hearing profiles,
environmental adaptation, perceptual metrics, and GPU/NPU acceleration remain
opt-in research features until their licence, calibration, privacy, and
failure behaviour are documented.

Cloud and service work must preserve Forge's current trust boundary: explicit
origin allow-lists, redirect reauthorization, request/body/time caps, bounded
decoded samples, redacted evidence, and no implicit network access in local
commands.  Plugin work must preserve callback safety and bit-exact parity with
the shared live processor.

## PR and release policy

Each stage is a small PR with its own schema/tests/docs.  Merge only after the
required `rust`, EBU, and ITU checks are green; release archives must include
new binaries/schemas and pass checksum/provenance verification.  User-visible
capabilities use the next minor version; documentation-only or internal CI
changes use a patch version.  A release note must identify whether a feature is
normative, engineering QC, or experimental and list its safety limits.

## Acceptance checklist

Every stage must record its normative source or non-normative method, units and
edge behaviour, clean and defective fixtures, stable JSON rule identifiers,
resource limits, cross-platform impact, and rollback/dry-run behaviour.  The
default build must remain free of optional native/model dependencies, and
existing EBU/ITU conformance, fuzz, API, C ABI, Python, WASM, and plugin parity
jobs must stay green where applicable.
