# Binaural renderer QC

`forge-binaural-qc` verifies a binaural file produced by an explicitly
selected external renderer. Forge does not ship an HRTF, object renderer, or
proprietary model, and this command never synthesizes a binaural signal from
channel-based PCM. The renderer and model identity are recorded in the report
with required SHA-256 evidence.

## Request

The request is JSON or TOML and must name a multichannel source layout, the
stereo renderer output, and the renderer evidence:

```json
{
  "schema_version": 1,
  "source": "master-7.1.4.wav",
  "rendered": "headphones.wav",
  "reference": "reference-headphones.wav",
  "input_layout": "7.1.4",
  "renderer": {
    "name": "reference-hrtf",
    "version": "1.2.3",
    "renderer_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "model": "studio-hrtf",
    "model_version": "2026.1",
    "model_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  }
}
```

`reference` is optional. When supplied, the rendered output is compared with
the trusted reference for loudness, true peak, and duration drift. Without a
reference, the report still verifies source/output duration, stereo layout,
true-peak ceiling, and sample clipping. Limits and decoded-sample/input-byte
caps are bounded and can be overridden in the request.

```sh
forge-binaural-qc binaural.json --output binaural-report.json
```

Exit status `0` means all configured gates passed, `1` means a QC gate failed
and JSON evidence was emitted, and `2` means the request, input, or renderer
evidence was invalid. The versioned schemas are
[`schema/binaural-qc-request-v1.schema.json`](schema/binaural-qc-request-v1.schema.json)
and [`schema/binaural-qc-report-v1.schema.json`](schema/binaural-qc-report-v1.schema.json).

This is an engineering verification profile, not a normative HRTF or spatial
audio conformance renderer. Renderer licensing, perceptual quality, head
tracking, headphone calibration, and individualized hearing compensation stay
outside Forge's trust boundary and must be documented by the renderer owner.
