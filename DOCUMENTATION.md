# Forge documentation

The [README](README.md) covers installation and common commands. This page is
an index for the focused references kept in the repository. The CLI remains the
authoritative option reference:

```sh
forge --help
forge-container-qc --help
```

## Core workflows

- [Resumable batch and album generations](BATCH-JOBS.md)
- [Watch folders](WATCH-FOLDERS.md)
- [Content-addressed analysis cache](ANALYSIS-CACHE.md)
- [SQLite catalogue](CATALOGUE.md)
- [Multi-delivery optimization](MULTI-DELIVERY.md)
- [Segment-aware normalization](SEGMENT-NORMALIZATION.md)
- [Remediation planning](REMEDIATION.md)
- [Metadata repair](METADATA-REPAIR.md)
- [Metadata fidelity, inventory, and single-file transactions](METADATA-FIDELITY.md)

Metadata-bearing file workflows use an explicit `preserve`, `strict`, or
`strip` policy; `legacy-generic` remains available for compatibility with the
historical primary/first generic-tag default. The field ledger reports every
preserved, mapped, recomputed, or dropped field. A bounded registry-backed
inventory covers repeated and unknown metadata regions and structural regions
in supported WAVE, FLAC, MP3, Ogg, and ISO-BMFF containers. Sample-indexed
BWF/DAW timing is converted with exact rational sample-clock arithmetic after
resampling, while XML-bearing timing remains opaque. A metadata-only job state
can restart one file transaction from its verified stage and publish with a
compare-and-swap check. Its state and source-parent directories are a trusted
boundary rather than a cryptographically authenticated adversarial store; the
historical `--write-tags` path without a job state remains sequential.
Multi-file `--job-state` now uses `batch-job-v3`, a semantic runtime context
and job ID, and the sibling `<job-state>.generation.json` journal to stage and
publish all audio outputs as one recoverable generation. See
[BATCH-JOBS.md](BATCH-JOBS.md) for album, verification, bounded
`--keep-going`, progress v2, and recovery commands. Dry runs remain
side-effect-free apart from explicit cache warming. The machine-readable contracts are
[`metadata-fidelity-report-v1`](schema/metadata-fidelity-report-v1.schema.json),
[`metadata-inventory-v1`](schema/metadata-inventory-v1.schema.json), and
[`metadata-job-v1`](schema/metadata-job-v1.schema.json), together with
[`batch-job-v3`](schema/batch-job-v3.schema.json),
[`batch-progress-v2`](schema/batch-progress-v2.schema.json),
[`batch-failure-report-v1`](schema/batch-failure-report-v1.schema.json),
[`generation-job-v1`](schema/generation-job-v1.schema.json),
[`generation-recovery-report-v1`](schema/generation-recovery-report-v1.schema.json),
and [`normalization-semantic-context-v1`](schema/normalization-semantic-context-v1.schema.json).

## Quality control and codec adapters

- [EBU QC Scenario 1 XML reports](EBU-QC-SCENARIO1.md)
- [ATSC A/85 streaming-service QC](ATSC-A85-SERVICE-QC.md)
- [ADM programme and complementary-presentation QC](ADM-PRESENTATION-QC.md)
- [ADM personalization-range QC](ADM-INTERACTIVITY-QC.md)
- [ADM content and presentation semantics QC](ADM-SEMANTICS-QC.md)
- [Binaural renderer QC](BINAURAL-QC.md)
- [Immersive downmix QC](IMMERSIVE-DOWNMIX.md)
- [External anomaly-provider protocol](ANOMALY-ADAPTER.md)
- [AC-4 reference-decoder adapter](AC4-ADAPTER.md)
- [DTS reference-decoder adapter](DTS-ADAPTER.md)
- [MPEG-H adapter](MPEGH-ADAPTER.md)

Machine-readable contracts are under [`schema/`](schema/), with lifecycle and
ownership recorded in the [JSON contract registry](SCHEMA-REGISTRY.md). Each
report names its schema version and the bounded checks it performs.

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
| Immersive and codecs | `forge-adm-presentation-qc`, `forge-adm-interactivity-qc`, `forge-adm-semantics-qc`, `forge-presentation-qc`, `forge-downmix-qc`, `forge-binaural-qc`, `forge-sadm-qc`, `forge-ac4-qc`, `forge-dts-qc`, `forge-mpegh-qc` |
| Automation | `forge-multi-delivery`, `forge-segment-normalize`, `forge-remediate`, `forge-metadata-repair`, `forge-report`, `forge-service` |
| Providers | `forge-dialogue-provider`, `forge-anomaly-provider`, `forge-onnx-provider` |

Some binaries require the Cargo features listed in [`Cargo.toml`](Cargo.toml).
Use `<command> --help` for its inputs, limits, output schemas, and exit codes.

### Service transport and authentication

`forge-service` accepts plaintext traffic only on a loopback listener by
default. A non-loopback REST or gRPC listener requires both a bearer token and
an explicitly declared TLS-terminating proxy. Each `--trusted-proxy-ip IP`
value is matched against the TCP peer address exactly (with IPv4-mapped IPv6
normalized), and every admitted request must contain exactly one
`x-forwarded-proto: https` header. The proxy must remove any client-supplied
copy of that header and inject its own value after completing TLS; forwarding
an untrusted value defeats the deployment boundary.

The legacy token in `FORGE_SERVICE_BEARER_TOKEN` grants all endpoints for
compatibility. Repeatable `--auth-scoped-token-env SCOPES=ENV` options load
digest-only tokens with any combination of `analyze`, `cancel`, `health`, and
`metrics`. REST analysis routes use `analyze`, health/readiness use `health`,
and `/metrics` uses `metrics`; gRPC Analyze, Health, Metrics, and Cancel use
`analyze`, `health`, `metrics`, and `cancel`, respectively. A cancel token can
cancel any currently active request ID, so issue that scope only to trusted
operators. For example:

```sh
export FORGE_ANALYZE_TOKEN='replace-with-a-high-entropy-secret'
forge-service \
  --bind 0.0.0.0:8080 \
  --trusted-proxy-ip 10.0.0.10 \
  --auth-scoped-token-env analyze=FORGE_ANALYZE_TOKEN
```

Forge does not yet terminate TLS itself and does not claim mTLS or OIDC
support. Do not expose the listener directly to an untrusted network; bind it
on a private path reachable only by the declared proxy.

The existing library entry points without a `ServiceSecurity` argument remain
available for source compatibility, but they now fail closed for every
non-loopback listener even when `ServiceConfig::bearer_token` is set. Library
callers that intentionally deploy behind a terminating proxy must construct a
`ServiceSecurity`, call `validate_for_config`, and use the corresponding
`run_with_security*` or `serve_with_security*` REST/gRPC entry point. The
legacy `bearer_token`, when present, is merged as an all-scope token and is
then discarded from the long-lived runtime configuration.

```rust
use forge_normalizer::service::{
    self, ScopedServiceToken, ServiceConfig, ServiceScope, ServiceSecurity,
};

let config = ServiceConfig {
    bind: "0.0.0.0:8080".parse().expect("valid bind address"),
    ..ServiceConfig::default()
};
let token = ScopedServiceToken::new(
    std::env::var("FORGE_ANALYZE_TOKEN").expect("token environment variable"),
    [ServiceScope::Analyze],
)
.expect("valid scoped token");
let security = ServiceSecurity::new()
    .with_trusted_proxy_ip("10.0.0.10".parse().expect("valid proxy IP"))
    .with_token(token)
    .expect("valid security policy");
security
    .validate_for_config(&config)
    .expect("valid service boundary");
service::run_with_security(config, security).expect("service failed");
```

### Service resource controls

`forge-service` requires fixed `Content-Length` framing for REST analysis and
streams the body into a bounded private replay spool in 64 KiB reads. The
process-wide `--temp-quota-mib` admission remains charged until the final
immutable-input clone is dropped. `--memory-quota-mib` accounts for accepted
gRPC messages and applies a conservative working-set admission charge through
streaming decode, DSP, and report serialization. That charge covers the input
as a worst-case demux packet, two eight-byte PCM representations, serial decoder
scratch, maximum loudness windows/block vectors, per-channel DSP state, and the
bounded report/layout response. Controlled service decoding uses serial FLAC
and serial, 4 KiB-read DSD conversion; DSD FIR work checks control at most every
256 source bytes. Declared WAVE, DSD, and Symphonia geometries are checked before
their major PCM allocations, with decoded output checked again after each
codec packet. The service rejects decoded geometries outside 1..=64 channels
before creating the analyzer; offline library and CLI decoding retain their
existing limits.
Before a service request enters a third-party demuxer, Forge scans the immutable
snapshot without materializing payloads. Ogg lacing is checked page by page and
continued packets are capped at 16 MiB; Vorbis/Opus comments and Ogg-FLAC
Vorbis-comment/picture fields are checked incrementally across page boundaries
against a 1 MiB encoded/wire-item bound and a file-wide 16 MiB encoded-metadata
aggregate. Chained Ogg logical streams share that aggregate and item count.
Matroska is limited to 16 nesting levels, 100,000 elements, 16 MiB
Block/SimpleBlock payloads, and the same per-item/file-wide bounds for retained
binary or string metadata leaves. Native FLAC metadata and
both trailing APE anchors on FLAC/MPEG inputs are bounded before Symphonia sees
them; leading APEv2 tags in its 1 MiB supplemental probe range use the same
checked item parser. Native FLAC and supplemental APE tags share one file-wide
metadata budget; skip-only FLAC PADDING and unknown blocks may exceed 1 MiB but
remain subject to the 16 MiB aggregate and 100,000-block limits.
Raw MPEG audio and ADTS inputs are checked as a complete, same-class frame
chain; only leading checked ID3v2/APEv2, a trailing ID3v1 record beginning with
`TAG`, and checked APEv2 immediately before EOF or that ID3v1 record are
excluded. Inter-frame junk, metadata, or another container marker is rejected.
The resulting container class and exact half-open audio byte range select a
service-only Symphonia probe containing one format reader and no supplemental
metadata readers. Controlled reads and seeks cannot expose bytes beyond that
range. Ordinary library and CLI probes retain Symphonia's full format registry.
Non-media ISO-BMFF top-level boxes are limited to 16 MiB, and nested
`udta/meta/ilst` entries are capped at 100,000, `data`/`name`/`mean` items at
1 MiB, and physical metadata leaves at 16 MiB in aggregate. Controlled Symphonia
reads/seeks and direct Opus reads check the absolute request control and split
actual reads at 32 KiB. ISO-BMFF `stsz`/`stz2` and fragmented
`trex`/`tfhd`/`trun` sample sizes are validated before demux, and Symphonia
tag/visual allocations are explicitly capped at 1 MiB. PCM packets use codec
width, channel count, and encoded length for a pre-decode sample bound; a
compressed packet with no trustworthy nonzero duration bound is rejected before
codec output allocation.
These encoded limits bound parser inputs and retained encoded values; they do
not claim that later character-set conversion uses exactly the same heap size.
Library users can clone `ServiceRuntimeLimits` across REST and gRPC listeners to
share the same counters. The memory counter is not a hard RSS cap: third-party
codec internals, allocator fragmentation, thread stacks, and transport state
are outside it. Temporary-storage charges are active in-process reservations,
not durable filesystem accounting; deployments still need filesystem quotas
and startup scavenging for crash or unlink-failure residue.

The absolute request deadline and cancellation state are checked during upload,
bounded WAVE chunk-table scans, codec packets, PCM validation/peak scans, and
DSP frame loops (at most 1024 DSP frames between polls). Dropping a gRPC RPC
future or calling `Cancel` stops detached blocking work at the next checkpoint.
Request IDs must be unique only while active and may be reused after completion,
matching the original v1/v3 contract. Internal request identity prevents an old
worker's cleanup from deleting a newer registration. Because the existing
Cancel wire message contains no generation, a delayed Cancel that arrives after
intentional ID reuse can still address the then-active request; clients that
need stronger ABA protection should generate process-unique request IDs.

For every unary gRPC method, a shared outer layer validates bearer metadata and
advances the body only until it can parse the five-byte gRPC frame prefix,
before tonic's generated protobuf decoder. An HTTP/2 DATA frame may include
some payload alongside that prefix; the fixed receive windows below bound this
pre-admission transport buffer. Immediately after route and authentication
checks, the layer acquires a process-wide semaphore (`workers` for Analyze and
four reserved control slots), before waiting for that prefix. Once its declared
length is known, it applies the method-specific cap and admitted-byte memory
lease before protobuf decoding. The complete Analyze protobuf message,
including audio and string fields, has a checked size cap and each string also
has a semantic limit. The listener accepts at most `workers + 1` connections
and advertises at most `workers + 4` HTTP/2 streams per connection, but both the
stream and connection receive windows are fixed at 64 KiB; receive credit
therefore does not multiply by the streams on a connection. An accepted socket
must complete the HTTP/2 preface and first request headers within the configured
request timeout even if it sends bytes continuously. Established connections
have a 120-second IO-idle deadline and a hard IO close after the 30-minute base
age plus one request-timeout allowance. Forge does not advertise GOAWAY-based
draining on tonic 0.14.6 because that release's max-age graceful path can panic
while an RPC is in flight. The same absolute request deadline
starts before frame-prefix reading and releases admission on timeout or future
drop. Kernel socket buffers, the listen backlog, HTTP/2 implementation overhead,
and tonic's bounded per-message bookkeeping remain transport/runtime resources
rather than memory-quota charges. A future client-streaming RPC would reduce
per-request latency and copying but is not required for these unary bounds.
Admission starts the gRPC metric timer before authentication/body reads and
transfers it once to the decoded handler. Serialized report/layout bytes hold a
checked memory lease until the actual tonic body is drained or dropped. REST
response writes remain inside the same absolute request deadline, except that
legacy `read_timeout`/`incomplete_body` failures get one fixed 100 ms
best-effort write grace; temporary-spool IO failures retain the existing v1
`temporary_file` discriminator.
Existing errors retain `service-error-v1`; newly introduced resource-limit,
quota, and cancellation failures identify the additive `service-error-v2`
contract.

### External process boundary

Runtime FFmpeg/ffprobe operations, ADM renderers, AC-4/DTS/MPEG-H adapters,
provenance verification, and capability probes share one subprocess broker.
It resolves and hashes a canonical executable before launch, rechecks that
identity at launch, applies an explicit minimal environment, closes or bounds
each standard stream, enforces a finite wall-clock deadline, and reaps the
leader plus ordinary descendants on timeout, cancellation, output overflow,
I/O failure, normal completion, or caller drop. Unix uses a dedicated process
group and Windows uses a kill-on-close Job Object; failure to establish the
platform containment primitive fails the launch.

The broker runs explicitly selected, trusted tools; it is not an OS sandbox.
It does not yet provide filesystem/syscall isolation, network denial, or
privilege dropping. Deploy third-party renderers with ordinary host-level
filesystem quotas and isolation, and treat the later strict sandbox profile as
a separate security control.

The minimal child environment contains only a fixed system `PATH` and locale
on Unix, or the Windows system root and system directories; user `PATH`, loader,
language-runtime, temporary-directory, licence, and vendor variables are not
inherited. Select helpers by an explicit executable path and pass configuration
as explicit arguments. A native self-contained launcher is required when a
vendor runtime cannot operate under that contract. On supported Unix systems,
Forge executes the opened executable through `/proc/self/fd` or `/dev/fd` to
bind pathname replacement. Consequently a shebang script observes an fd path
as `$0`/`argv[0]`; scripts that locate sibling resources through their own path
are unsupported. The executable check is not an immutability or sandbox
guarantee against a hostile same-user writer, a separately replaced script
interpreter, a double-forked daemon, or adversarial renaming of monitored
directories; use OS isolation for those threat models.

`forge-report ebu-qc-validate` validates EBU QC 2026-04 report structure and
cross-element semantics; it uses Scenario 1 constraints by default and accepts
`--profile data-model` for the general rules. The release also contains the
hash-pinned official XSDs for independent schema validation.
`forge-sadm-qc` accepts S-ADM XML frame documents in transport order; divided
chunks with a shared base `frameFormatID` must be adjacent and ordered by chunk
index. It validates the normative frame paths and version declaration, then
reconstructs logical ADM state and checks any declared `changedIDs` status
transitions after all chunks for a logical frame have been combined. XML
parsing and flow reconstruction use fixed file, byte, depth, element,
attribute, text, namespace-expansion, and canonical-state limits. Non-document
or malformed XML, namespace lookalikes, and known S-ADM elements at invalid
paths are rejected before a QC report is produced.
