# Smart remediation dry-run

`forge-remediate` creates a bounded, auditable plan for bringing an audio
source toward a target loudness and true-peak/LRA policy. It is deliberately a
planner: it never rewrites the source or creates an output audio file.

```bash
forge-remediate remediation.json --output remediation-report.json
```

The request may be JSON or TOML. A minimal request is:

```json
{
  "schema_version": 1,
  "source": "master.wav",
  "target_lufs": -16,
  "true_peak_ceiling_dbtp": -1,
  "max_loudness_range_lu": 12
}
```

The report records the source SHA-256, an effective-settings SHA-256, the
measured input, a projected static gain, and the smallest actions needed by the
policy:

* `static-gain` is safe to apply only to a fresh render of the original source.
* `true-peak-limiter` is the minimum projected reduction needed to meet the
  ceiling. Its effect on integrated loudness must be remeasured.
* `lra-compressor` is an advisory dynamics action. Forge does not select or
  render a compressor in this command.

`max_static_gain_db` and `max_dynamic_reduction_db` are bounded safety caps.
An over-cap request returns exit status 1 with a JSON report and explicit
`infeasibility_reasons`; malformed requests or decode/resource failures return
exit status 2. Exit status 0 means the plan is feasible, not that audio has
already been corrected. The report's `manual_review_required` and
`requires_render_verification` fields must be respected before delivery.

The LRA gate is marked unverified for sources shorter than the 60-second
stability interval described by EBU Tech 3341. The versioned contracts are
[`remediation-request-v1.schema.json`](schema/remediation-request-v1.schema.json)
and
[`remediation-report-v1.schema.json`](schema/remediation-report-v1.schema.json).
