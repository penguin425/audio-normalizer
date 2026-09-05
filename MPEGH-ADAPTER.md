# MPEG-H MHAS and conforming-decoder adapter

`forge-mpegh-qc` structurally audits a raw MPEG-H Audio Stream (MHAS), then
audits every presentation exposed by an explicitly selected conforming or
reference decoder. Forge does not include an MPEG-H decoder and does not claim
that a third-party renderer is normative.

The contract tracks the current
[ISO/IEC 23008-3:2026](https://www.iso.org/standard/90199.html) 3D Audio
standard and [ISO/IEC 23008-6:2025](https://www.iso.org/standard/90200.html)
reference software. Decoder conformance evidence identifies the current
[ISO/IEC 23008-9:2023](https://www.iso.org/standard/86274.html) test standard.

## Invocation

```sh
forge-mpegh-qc programme.mhas \
  --adapter /opt/vendor/bin/forge-mpegh-adapter \
  --output mpegh-qc.json \
  --loudness-tolerance-lu 1.0 \
  --max-true-peak-dbtp -1.0
```

The input must be a raw MHAS elementary stream. ISO-BMFF extraction remains a
container-demuxing responsibility; Forge does not scan arbitrary bytes for a
plausible packet boundary.

## Forge-owned MHAS validation

Before invoking external code, Forge walks the complete stream with the
normative three-tier escaped-value widths:

- packet type: 3, 8, and 8 bits;
- packet label: 2, 8, and 32 bits;
- packet length: 11, 24, and 24 bits.

Every declared byte range must end exactly within the file. Forge records
packet-type counts, labels, payload bytes, configuration/frame/Audio Scene
Information/loudness/SYNC counts, and every distinct profile-level indication.
It requires configuration before a frame of the same label, validates the
one-byte `0xA5` SYNC payload and label, bounds configuration payloads to 4096
bytes, and maps Main, High, Low Complexity, and Baseline levels 1 through 5.
The total packet count is limited to 1,000,000.

These checks cover MHAS framing and selected externally visible fields. Forge
does not claim to parse or validate the complete coded audio, MAE, MPEG-D DRC,
or Audio Scene Information payload syntax; those require the selected decoder.

## Adapter protocol

The adapter is invoked directly, without a shell:

```text
forge-mpegh-adapter --request REQUEST.json --response RESPONSE.json
```

The request follows `schema/mpegh-adapter-request-v1.schema.json`. It provides
the canonical input path, byte length, SHA-256, private output directory,
Forge's MHAS inventory, exact standards, and these requirements:

- enumerate the complete audio scene and every selectable preset;
- render every presentation to a distinct WAVE file;
- disable loudness normalization and dynamic-range control while rendering;
- report programme loudness for `drcSetId = 0` and `downmixId = 0` using
  `methodDefinition` 1 or 2, including `loudnessInfoType` and its matching MAE
  group-preset reference when type 3 is used.

The response follows `schema/mpegh-adapter-response-v1.schema.json`. It reports
the decoder name/version, matching input hash and profile-level indication,
then the complete scene:

- channel, object, or HOA groups, default/on-off state, language/content labels
  and permitted gain or position interactivity;
- switch-group membership and default selection;
- every preset, its kind, and its member groups;
- one render of the default scene plus one rendered presentation per preset.

Forge rejects duplicate or dangling IDs, inconsistent counts, incomplete
preset coverage, unsafe render paths, profile mismatch with the native MHAS
inventory, non-programme loudness entries, and changed input/adapter/render
bytes. It independently decodes each WAVE render and measures integrated
loudness and true peak using its ITU-R BS.1770-5 engine.

## Bounded execution and evidence

The default process timeout is 300 seconds and cannot exceed one hour.
Captured stdout and stderr are limited to 1 MiB each, the response to 4 MiB,
scene enumeration to 128 groups, 32 switch groups and 32 presets,
presentation enumeration to 33 entries (default plus 32 presets), and each
render to 50,000,000
interleaved decoded samples by default with a hard maximum of 200,000,000.
Absolute paths, `..`, symlink escapes, duplicate render paths and non-WAVE
renders are rejected.

The atomic report follows `schema/mpegh-adapter-report-v2.schema.json` and
contains input, adapter, and render SHA-256 values; native MHAS evidence;
decoder and standards evidence; validated scene metadata; decoded geometry;
measured loudness and true peak; an exact renderer-bound output-layout
descriptor; and stable rule IDs. The descriptor binds the decoder,
configuration, adapter executable, and exact adapter response. Report v1
remains available for historical validation. Exit status is 0 for
PASS, 1 for completed QC failure, and 2 for invalid input, adapter/protocol
failure, or unsafe output.

## Trust boundary

SHA-256 evidence proves which executable and input were used; it does not grant
the executable trust or establish ISO conformance. Operators remain responsible
for selecting an authorized implementation and validating it with applicable
ISO/IEC 23008-9 test material. Forge never sends the input over a network and
never loads adapter code into its own process.
