# Forge service metrics

`forge-service` exposes bounded process metrics only when observability is
explicitly enabled. The registry contains no paths, filenames, request IDs,
model payloads, or user-controlled labels.

## Prometheus

Start REST mode with `--metrics`:

```bash
forge-service --bind 127.0.0.1:8080 --metrics
curl --fail http://127.0.0.1:8080/metrics
```

The endpoint uses the Prometheus text exposition format. In gRPC mode, the
same text is returned by the `ForgeMetrics/Metrics` RPC in the optional
`forge.service.v1` protocol. The RPC is available only when the
`grpc-service` feature is enabled and `--metrics` (or `--otel-jsonl`) was
selected.

The following names are stable for the v1 service contract:

| Metric | Type | Meaning |
| --- | --- | --- |
| `forge_service_requests_total` | counter | Completed requests |
| `forge_service_request_success_total` | counter | 2xx responses |
| `forge_service_request_client_errors_total` | counter | 4xx responses |
| `forge_service_request_server_errors_total` | counter | 5xx responses |
| `forge_service_request_busy_total` | counter | Worker-limit rejections |
| `forge_service_request_timeout_total` | counter | 408/504 outcomes |
| `forge_service_request_cancelled_total` | counter | Cooperative/disconnect cancellations (499) |
| `forge_service_in_flight_requests` | gauge | Requests currently active |
| `forge_service_request_duration_seconds` | histogram | End-to-end request duration |
| `forge_service_analysis_total` | counter | Successfully measured uploads |
| `forge_service_analysis_bytes_received_total` | counter | Bytes in successful uploads |
| `forge_service_analysis_decoded_samples_total` | counter | Decoded samples in successful uploads |
| `forge_service_analysis_loudness_lufs_mean` | gauge | Mean integrated LUFS of successful uploads |

The duration histogram has fixed upper bounds of 5 ms, 25 ms, 100 ms, 500 ms,
1 s, 5 s, 30 s, and 120 s, plus `+Inf`. There are no labels other than the
standard `le` histogram bucket label. Values are process-local and reset on
restart; counters saturate instead of wrapping.

The metrics endpoint inherits the service's bearer-token policy. Put TLS,
network policy, and any scrape authorization at the deployment gateway before
binding outside loopback.

## OpenTelemetry-compatible span bridge

`--otel-jsonl PATH` appends one bounded JSON object per request:

```bash
forge-service --metrics --otel-jsonl /var/log/forge/service-spans.jsonl
```

Records contain a fixed server-span name (`forge.service.request`), protocol,
status, status code, duration, request byte count, and optional decoded-sample
and integrated-LUFS measurements. A valid W3C `traceparent` header contributes
only its trace and parent span IDs. Invalid or overlong metadata is ignored.

The file is an adapter boundary, not an OTLP exporter: a deployment may tail
the JSONL records into its OpenTelemetry SDK or collector. Writes are
serialized, flushed after each record, and ignored if the destination becomes
unavailable so an observability failure cannot change an analysis response.
Keep the file on a protected local volume and rotate it outside the service;
the reference binary does not perform unbounded buffering or remote export.
