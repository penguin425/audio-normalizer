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

## Explicit ONNX reference adapter

Forge 0.114 adds an opt-in CPU reference adapter behind the
`onnx-provider` Cargo feature. It does not download weights, load a default
runtime, decode audio, or make network requests. A caller supplies all four
inputs explicitly:

```text
forge-onnx-provider \
  --manifest quality-model.json \
  --model quality-model.onnx \
  --features programme.features.json \
  --runtime-library /opt/onnxruntime/lib/libonnxruntime.so \
  --output provider.json
```

`quality-model.json` must conform to
`schema/onnx-anomaly-model-v1.schema.json`. It binds the model basename and
SHA-256, provider/model versions, SPDX (or published) licence, training
dataset URL, calibration-report SHA-256/URL, exact input/output names and
shapes, class thresholds, and hard model/feature/event/thread/time limits.
The only v1 fallback is `reject`: a missing runtime, invalid model, timeout,
shape mismatch, or non-finite score is an error and cannot become an empty
passing audit.

The feature sidecar conforms to
`schema/onnx-feature-frames-v1.schema.json`. It is a bounded row-major
`[frames][feature_count]` float32 matrix carrying the source path, source
SHA-256, duration, sample rate, and frame hop. The model must expose one f32
input `[1, frames, feature_count]` and one f32 output
`[1, frames, class_count * 2]`; every class emits a confidence/severity pair.
Contiguous accepted frames of the same class are coalesced into time-bounded
`audio-anomaly-provider-v1` events. Run the ordinary audit command afterwards
to apply the review thresholds and create the manifest-compatible audit:

```text
forge-anomaly-provider provider.json \
  --confidence-threshold 0.60 \
  --severity-threshold 0.50 \
  --output anomaly-audit.json
```

This reference path is intentionally feature-extractor neutral. A Demucs-like
dialogue separator or a future audio front end must publish the same manifest,
feature provenance, calibration evidence, and fail-closed limits before its
results can cross the existing advisory `model_qc` boundary. The ONNX Runtime
shared library is process-global, so one runtime path is allowed per process.

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
