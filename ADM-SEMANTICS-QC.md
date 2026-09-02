# ADM semantics QC

`forge-adm-semantics-qc` audits content and presentation metadata that can be
checked without rendering audio. The normative rules come from
[ITU-R BS.2076-3](https://www.itu.int/rec/R-REC-BS.2076-3-202502-I/en); the
presentation and importance guidance comes from the current
[Report ITU-R BS.2388-7](https://www.itu.int/pub/R-REP-BS.2388-7-2026).

```sh
forge-adm-semantics-qc programme.bw64 \
  --expected-default-programme APR_1001 \
  --renderer-object-limit 64 \
  --output semantics.json
```

The audit checks:

- `dialogue` values, their value-dependent kind attributes, and every defined
  content-kind enumerator;
- `alternativeValueSetIDRef` syntax, local ownership, and the one-reference-
  per-object rule in each programme or content;
- deterministic lowest-ID fallback programme selection;
- fixed-programme versus interactive complementary-object authoring intent;
- integer importance values from 0 through 10 and their distinct object,
  pack, and block meanings;
- `tagList` structure and local references while keeping tags
  non-authoritative for ADM parsing.

`--presentation-intent auto` only reports the inferred pattern. `fixed` and
`interactive` turn the selected BS.2388-7 authoring pattern into an enforced
operator policy. `--expected-default-programme` likewise enforces an explicit
expectation rather than guessing from language or tag text.

## Importance planning

With `--renderer-object-limit N`, Forge evaluates thresholds 0 through 10 and
reports the first one whose metadata object count is at most `N`. It never
lists importance-10, missing-importance, or invalid-importance objects as
automatic discard candidates. An unattainable limit is a completed policy
failure and requires renderer-aware merging, downmixing, or another explicit
decision.

The plan applies only to `audioObject` count. `audioPackFormat` importance
describes a format/spatial-quality compromise. `audioBlockFormat` importance
is reported as informational, following BS.2388-7 guidance that pack
importance should take precedence.

## Evidence, limits, and exit status

The versioned
[`adm-semantics-report-v1` schema](schema/adm-semantics-report-v1.schema.json)
records the input SHA-256, selected limits, inventories, every rule, and
whether a rule is normative, guidance, policy, or informational. Input bytes
are hashed before and after parsing. Programme, content, object, expanded
report-item, `axml` byte, and XML-node limits are configurable and have hard
ceilings.

Exit status is 0 for PASS, 1 for a completed normative or explicitly requested
policy failure, and 2 for invalid input, a safety-limit violation, or an output
error.

## Scope

The report fixes `rendered_audio_verified`, `renderer_capacity_verified`, and
`tag_semantics_authoritative` to `false`. Metadata object count is not a proof
of a renderer's actual track/object capacity, and no loudness or true-peak
claim is made. Use `forge-adm-presentation-qc` for bounded rendered variants
and `forge-adm-interactivity-qc` for gain and position personalization ranges.
