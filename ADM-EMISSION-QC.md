# ADM emission-profile QC

`forge-adm-emission-qc` audits an AXML-carried file-based ADM/BW64 delivery
against the selected Level 0, 1, or 2 requirements in Sections 2 and 3 of
[ITU-R BS.2168-0](https://www.itu.int/rec/R-REC-BS.2168-0-202502-I/en).

```sh
forge-adm-emission-qc programme.bw64 \
  --level 1 \
  --output emission.json
```

The audit checks the profile declaration, allowed ADM structure, identifiers
and references, complementary and independent object groups, Matrix and
Objects pack/channel relationships, exact file-based block timing, and the
relationship between `audioTrackUID`, `chna`, and PCM essence. Level 1 and 2
also apply their numerical occurrence limits. Level 0 removes those ceilings;
it does not disable the other profile rules.

## Evidence and limits

The versioned
[`adm-emission-report-v1` schema](schema/adm-emission-report-v1.schema.json)
records the input SHA-256, selected level and resource limits, derived counts,
and stable rule-level evidence. XML bytes, CHNA bytes, nesting depth, element
count, attributes, text, report items, and retained evidence are bounded.

Exit status is 0 for PASS, 1 for a completed profile failure, and 2 for invalid
input, a safety-limit violation, or an output error. Existing reports are not
replaced unless `--overwrite` is supplied.

## Scope

This is a bounded metadata-and-essence preflight, not certification. It does
not render an ADM presentation and therefore makes no rendered loudness,
true-peak, audibility, or perceptual-equivalence claim. Use
`forge-adm-presentation-qc` when renderer-backed evidence is required, and
`forge-sadm-qc` for S-ADM frame and flow validation.

This release accepts one uncompressed `axml` carrier. A `bxml` chunk, including
a file that contains both `axml` and `bxml`, is reported as unsupported input
with exit status 2 rather than as a BS.2168 profile failure.
