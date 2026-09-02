# ADM interactivity QC

`forge-adm-interactivity-qc` audits the personalization envelope declared by
ADM `audioObjectInteraction` metadata. It follows the gain semantics in
[ITU-R BS.2076-3](https://www.itu.int/rec/R-REC-BS.2076-3-202502-I/en) and can
also apply the interactivity subset of the
[ITU-R BS.2168-0 emission profile](https://www.itu.int/rec/R-REC-BS.2168-0-202502-I/en).

```sh
forge-adm-interactivity-qc programme.bw64 \
  --profile bs2168-emission-ranges \
  --output interactivity.json
```

The audit resolves the parent `audioObject` configuration and every direct
`alternativeValueSet`. It checks:

- explicit minimum and finite maximum gain bounds whenever gain interaction is
  enabled;
- bound ordering and whether the default gain is inside the effective range;
- complete position min/max pairs and consistent polar or Cartesian
  coordinates;
- `onOffInteract`, gain, and position constraints from the BS.2168 emission
  profile when requested.

BS.2076 intentionally treats an interactive object with no
`audioObjectInteraction` as enabling on/off interaction plus unrestricted gain
and position interaction. Forge reports that case as a safety failure because
no finite upper envelope can be established.

## Evidence and limits

The input, `axml` size, selected limits, normalized linear/dB gains, every rule,
and inherited `alternativeValueSet` state are recorded in the versioned
[`adm-interactivity-report-v1` schema](schema/adm-interactivity-report-v1.schema.json).
Input bytes are bound by SHA-256 before and after parsing. Object count,
expanded parent/alternative configuration count, `axml` bytes, and XML nodes
have configurable limits with hard ceilings.

Exit status is 0 for PASS, 1 for a completed QC failure, and 2 for invalid
input, a safety-limit violation, or an output error.

## Scope

This is a metadata-range audit, not a renderer. Every report fixes
`continuous_audio_compliance_verified` to `false`. A bounded renderer adapter
must still produce the relevant personalization cases and Forge must measure
their loudness and true peak before audio compliance can be asserted. In
particular, integrated loudness gating makes a general continuous-range claim
unsafe to infer from metadata alone. Nested object gains also combine along the
rendering graph; this object-level report deliberately does not present those
individual bounds as a programme-wide envelope.
