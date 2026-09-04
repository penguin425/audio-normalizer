# EBU QC Scenario 1

Forge can write an EBU QC 2026-04 XML report for the broadcaster pass/fail
workflow described by the EBU Scenario 1 guidance.

```sh
forge programme.wav --analyze --ebu-qc \
  --ebu-qc-xml programme.qc.xml
```

The report contains an overall result, Forge tool identity, sample edit-unit
timing, a one-to-one `Item`/`ItemResult` mapping, and no embedded
`ItemDefinition` children. Published EBU Items use their catalogue-specific
Input and Output names. Forge-only signal-health rules remain in the JSON
analysis output and are not presented as EBU catalogue Items.

Every generated report is checked before publication with Forge's bounded
2026-04 semantic validator. To validate an existing report:

```sh
forge-report ebu-qc-validate programme.qc.xml
forge-report ebu-qc-validate --profile data-model another-report.xml
```

The default applies the additional Scenario 1 rules. `--profile data-model`
checks the general cross-element rules only. Both modes reject DTDs, excessive
resource use, mismatched Item identities, inconsistent overall results,
report-mode `CheckResult`, invalid timing locators, and the obsolete
`Output/Name=CheckResult` representation.

Formal XSD validation is separate. The release ships the core, timing, and
Catalogue API schemas fixed to EBU tag `2026-04` commit
`c9b04821831a38b91f650449b09a17a8e6092757`, their SHA-256 manifest, and a
combined validation wrapper under `schema/ebu-qc-2026-04/`.

## Catalogue pinning

Report generation is offline and deterministic. It does not query the live EBU
catalogue. Forge pins the ID, version, name, mode, and source-response SHA-256
for every supported definition in
[`schema/ebu-qc-catalogue-v2-pins.json`](schema/ebu-qc-catalogue-v2-pins.json).
The pins use Catalogue API v2 (`tag:qc.ebu.ch,2026-01`), which is the API used
by the current EBU report implementation resources, while reports use the
2026-04 data-model namespace.

The EBU 2026-04 release specifies Catalogue API v3, but the production
`/api/v3` endpoint was still unavailable on 2026-09-04 and the EBU tracks its
rollout in issue #7. Forge therefore does not claim production Catalogue API
v3 conformance and will not fabricate v3 Item definitions or hashes. A later
release will replace the v2 snapshot after the published/withdrawn v3
definitions are available and verified.

Catalogue identity drift is a hard error. Updating an Item version therefore
requires an explicit code and pin-manifest change rather than silently changing
the meaning of a report.

## Check and report modes

- Defect and threshold Items are emitted as `check`; their `CheckResult` values
  contribute to the report-wide logical `AND`.
- Loudness and true peak are emitted as `report`; their ItemResults contain
  measurements and correctly omit `CheckResult`.
- Audio duration is a `report` when no expected duration was supplied and a
  `check` when `--expected-duration` is present.

Forge captures a loudness timeline automatically for XML output. The default
interval is 40 ms; if `--timeline` is also requested, its selected interval is
used and declared in the XML. Catalogue Items that cannot produce a meaningful
measurement are omitted as not applicable: short or ungated content without a
complete short-term loudness measurement is not labelled as Item 0010B, true
peak requires a finite measurement, and Item 0095B is included only when an LFE
channel is declared.

## References

- [EBU QC Data Model 2026-04](https://tech.ebu.ch/publications/ebu_qc_dm)
- [EBU QC 2026-04 release](https://github.com/ebu/qc/releases/tag/2026-04)
- [Scenario 1 best-practice guidance](https://github.com/ebu/qc/blob/2026-04/qc-reports/qc-reports-best-practice-guidance-1.md)
- [EBU QC report compliance checklist](https://github.com/ebu/qc/blob/2026-04/qc-reports/qc-reports-compliance-checklist.md)
- [Catalogue API v3 rollout issue](https://github.com/ebu/qc/issues/7)
- [EBU QC Catalogue API](https://qc.ebu.io/help/api)
