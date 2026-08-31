# Immersive downmix QC

`forge-downmix-qc` provides a bounded, deterministic simulation of selected
channel-based downmixes. It is intended for delivery and presentation QC when
the source is decoded PCM in the WAVE channel order. The command reports the
matrix that was applied, integrated loudness, loudness/peak deltas, sample
clipping, and true-peak ceiling risk for every requested profile.

This is an engineering QC profile, not a normative Dolby, MPEG-H, IAMF, or
ITU renderer. Object metadata, renderer-specific downmix metadata, speaker
calibration, HRTF/binaural rendering, and codec decoding remain outside this
command. Use [`forge-presentation-qc`](DOCUMENTATION.md#command-line-tools)
to audit an externally rendered presentation, and provide its renderer and
model/version evidence separately.

## WAVE order and profiles

The input layout is explicit and is never inferred from a file's channel
count. The supported WAVE orders are:

| Layout | Channels (in order) |
| --- | --- |
| `mono` | `M` |
| `stereo` | `FL FR` |
| `5.1` | `FL FR FC LFE BL BR` |
| `6.1` | `FL FR FC LFE BC SL SR` |
| `7.1` | `FL FR FC LFE BL BR SL SR` |
| `5.1.4` | `FL FR FC LFE BL BR TFL TFR TBL TBR` |
| `7.1.4` | `FL FR FC LFE BL BR SL SR TFL TFR TBL TBR` |

`-3.01 dB` means the equal-power coefficient
`1/sqrt(2) = 0.7071067812`. The matrices are versioned by their method string
in the report.

* `stereo` keeps `FL`/`FR` at unity, sends `FC` and each present left/right
  surround or height channel to its corresponding output at `-3.01 dB`, and
  omits `LFE`. A mono source is duplicated to both outputs. `BC` is split to
  both outputs when present.
* `5.1` is a downmix target, never an upmix. `5.1.4` adds each top channel to
  its matching base channel at `-3.01 dB`; `7.1.4` additionally folds `SL`/`SR`
  into `BL`/`BR` at `-3.01 dB`; `7.1` folds `SL`/`SR` into `BL`/`BR`; and `6.1`
  splits `BC` to `BL`/`BR` at `-3.01 dB` while retaining `SL`/`SR` at unity.
  `FC` and `LFE` stay in their 5.1 positions at unity.
* `7.1.4` is an identity verification profile. It accepts only a `7.1.4`
  source, so the tool cannot silently upmix a smaller layout.

## Request and report

The JSON and TOML request is versioned by `schema_version: 1`:

```json
{
  "schema_version": 1,
  "source": "programme-7.1.4.wav",
  "input_layout": "7.1.4",
  "profiles": ["stereo", "5.1", "7.1.4"],
  "true_peak_ceiling_dbtp": -1.0,
  "max_loudness_delta_lu": 1.0,
  "max_true_peak_delta_db": 1.0,
  "max_clipped_samples": 0
}
```

`source` is resolved relative to the request file. The input and decoded
sample limits default to 512 MiB and 24 million samples and can be lowered or
raised explicitly within the bounded request. The request schemas are
[`downmix-qc-request-v1.schema.json`](schema/downmix-qc-request-v1.schema.json)
and the report contract is
[`downmix-qc-report-v1.schema.json`](schema/downmix-qc-report-v1.schema.json).

Run it in a pipeline as follows:

```bash
forge-downmix-qc downmix.json --output downmix-qc.json
forge-downmix-qc downmix.toml --compact > downmix-qc.json
```

Exit status `0` means every profile passed, `1` means a report was produced
but at least one profile failed, and `2` means the request could not be
validated or decoded. A report is emitted even for a QC failure. The
`clip_risk` field distinguishes `sample-clipping`, a post-matrix sample above
full scale, from `true-peak-ceiling`, a finite true peak above the requested
ceiling. Loudness and true-peak deltas are measured against the decoded source
and are `null` when the corresponding measurement is unavailable (for
example, complete loudness gating blocks are absent).

The existing `forge --downmix-qc` option remains the legacy stereo measurement
used by delivery manifests. The new command is the explicit multi-profile
matrix and evidence contract; it does not replace externally rendered
presentation QC.
