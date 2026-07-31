# AC-4 reference-decoder adapter

`forge-ac4-qc` audits every presentation exposed by an explicitly selected
licensed or reference AC-4 decoder. Forge does not include an AC-4 decoder and
does not claim that a third-party renderer is normative. It binds the input and
adapter executable with SHA-256, validates the decoder's metadata response,
then independently measures every rendered WAVE output with Forge's
ITU-R BS.1770-5 engine.

The current contract tracks ETSI TS 103 190-1 V1.4.1 and ETSI
TS 103 190-2 V1.3.1 (both 2025-07). Part 1 defines the seven-bit dialnorm value
in quarter-dB steps. Part 2 places dialnorm in the presentation substream for
`presentation_version = 1`, and in selected substream/basic metadata for
version 0. It also defines downmix, alternative-presentation, and real-time
loudness correction stages. Forge records these values; it does not attempt to
reimplement the patented decoder.

## Invocation

```sh
forge-ac4-qc programme.ac4 \
  --adapter /opt/vendor/bin/forge-ac4-adapter \
  --output ac4-qc.json \
  --dialnorm-tolerance-lu 1.0 \
  --max-true-peak-dbtp -1.0
```

The adapter is invoked without a shell:

```text
forge-ac4-adapter --request REQUEST.json --response RESPONSE.json
```

The request follows `schema/ac4-adapter-request-v1.schema.json`. It supplies
the canonical input path, input SHA-256 and byte length, a private output
directory, the exact ETSI versions, and the requirement to enumerate every
presentation. The adapter must render a distinct WAVE file for every
presentation inside that directory and atomically write a response conforming
to `schema/ac4-adapter-response-v1.schema.json`.

The response reports the decoder name/version, the input hash it consumed,
the exact number of available presentations, and for each presentation:

- a unique stable ID, presentation version, output layout, and relative WAVE
  path;
- optional language and accessibility labels;
- `dialnorm_bits`, its interpreted LKFS value, and its normative metadata
  source;
- optional downmix, alternative-presentation, and real-time correction gains.

For presentation version 1, `dialnorm_source` must be
`presentation-substream`. Version 0 accepts `associated-basic-metadata` or
`main-or-dialogue-substream`. `dialnorm_lkfs` must equal
`-dialnorm_bits / 4` exactly.

## Bounded execution and evidence

The default process timeout is 300 seconds and cannot exceed one hour.
Captured stdout and stderr are limited to 1 MiB each, the response to 4 MiB,
and presentation enumeration to 256 entries. Each render has a default limit
of 50,000,000 interleaved decoded samples and a hard maximum of 200,000,000.
Absolute paths, `..`, symlink escapes, duplicate IDs, duplicate render paths,
unsupported presentation versions, stale input hashes, and changed input bytes
are rejected.

The report follows `schema/ac4-adapter-report-v1.schema.json` and records the
input, adapter, and render SHA-256 values; decoder and ETSI versions; decoded
geometry; measured integrated loudness and true peak; dialnorm drift; optional
true-peak gating; and per-presentation plus aggregate pass/fail results.
Exit status is 0 for PASS, 1 for completed QC failure, and 2 for invalid input,
adapter/protocol failure, or unsafe output.

## Adapter trust boundary

SHA-256 evidence proves which executable and input were used; it does not grant
the executable trust or certify its licence. Operators remain responsible for
installing an authorized implementation, validating its provenance, and
mapping vendor-specific presentation selection and PCM export into this narrow
protocol. Forge never sends the input over a network and never loads adapter
code into its own process.
