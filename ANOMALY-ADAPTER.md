# External audio anomaly provider protocol v1

Forge's standards-based loudness and EBU QC paths remain deterministic.  An
optional AI/ML detector can supply separate quality findings through the
`audio-anomaly-provider-v1` JSON contract.  Forge validates the result and
emits an `anomaly-provider-audit-v1` report; it does not load model weights,
send source audio to a service, or silently change a compliance result.

## Invocation

```text
forge-anomaly-provider provider.json \
  --confidence-threshold 0.60 \
  --severity-threshold 0.50 \
  --output anomaly-audit.json
```

The input must conform to
`schema/audio-anomaly-provider-v1.schema.json`.  The output conforms to
`schema/anomaly-provider-audit-v1.schema.json` and records the exact provider,
model, model SHA-256, source SHA-256, thresholds, every finding, and whether a
finding passed both review thresholds.

Supported v1 finding kinds are `noise`, `pop`, `dropout`, `lip-noise`,
`phase-cancellation`, `clipping`, and `other`.  Findings are time-bounded,
sorted by `(start_seconds, end_seconds)`, and may overlap when different
detectors report separate phenomena.  `channel` and `related_channel` are
one-based when present.  `evidence_label` is deliberately limited to a short,
printable, non-sensitive feature label; transcripts and raw model payloads do
not belong in this contract.

## Trust boundary and limits

The provider is an external process and is not trusted as a standards
measurement.  The input is rejected when:

- either the source or model SHA-256 is absent or malformed;
- the source duration is non-finite, non-positive, or longer than seven days;
- a finding is outside the source duration, has invalid confidence/severity,
  uses zero-based channels, or is not sorted; or
- more than 100,000 findings are supplied.

An empty finding list is a valid, passing anomaly audit.  A non-empty audit is
reported as `passed: false` only when at least one event meets both configured
thresholds.  This status is intentionally separate from EBU/ITU compliance;
downstream systems must decide whether a model finding is actionable.

The v1 contract is an integration boundary, not a claim that any particular
model is accurate.  A future ONNX/Demucs adapter must publish its model
licence, dataset/provenance, calibration evidence, deterministic fallback, and
false-positive/false-negative fixtures before it can be enabled by default.

## Delivery-manifest bridge

An audit produced by this command can be attached to a batch manifest without
installing a model runtime:

```text
forge programme.wav --analyze --manifest delivery.json \
  --anomaly-audit anomaly-audit.json
```

For multiple inputs, repeat `--anomaly-audit` once per input in the same order.
Forge revalidates the complete audit, including selected flags, event counts,
thresholds, sorted time bounds, and duration totals. The manifest stores the
result under `assets[].model_qc` with the `model-qc-v1` schema, the explicit
`model-qc` layer, and the classification
`non-normative-model-evidence`. Its `passed` field is advisory and is excluded
from the manifest's normative `passed_count` and `failed_count` totals.

`forge-report explain` turns selected events into stable
`FORGE-MODEL-ANOMALY-<KIND>` findings. Explanations include the provider/model
identity, SHA-256 provenance, thresholds, exact time location, and the explicit
statement that model evidence does not change EBU/ITU compliance. The bridge
does not authenticate a provider or claim that a model finding is correct.
