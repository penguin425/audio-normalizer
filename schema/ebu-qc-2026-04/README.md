# EBU QC 2026-04 schemas

These files are copied from the EBU QC `2026-04` release at commit
`c9b04821831a38b91f650449b09a17a8e6092757`:

- `qc-data-model/qc.xsd`
- `qc-data-model/TimingExtensionMediaPlaybackEditUnits.xsd`
- `qc-catalogue-api/qc-catalogue-api-schema.xsd`
- `qc-reports/qc-report-generic-sample.xml`

The timing XSD and sample lacked a terminal line feed upstream; the vendored
text adds one. Their upstream SHA-256 values are respectively
`82a998dbb6f8c47b75ee56bfa8b9f911e6b9a2e5cd7b79ddc4431e215da77d81`
and `426f37fbfaed397b6dc6cdc55ede9a2b3fd7eef510ecc37c2952dfc6b681079a`.
`SHA256SUMS` records the actual packaged bytes. The core and Catalogue XSDs
are byte-identical to upstream.

The EBU source is licensed under CC BY 4.0; see `LICENSE.md`. The local
`forge-validation.xsd` wrapper is an MIT-licensed Forge file that imports both
the core data-model schema and the timing-extension schema so generated reports
can be validated in one command.

Catalogue API v3 is specified by this release but was not available from the
production EBU endpoint on 2026-09-04. Forge therefore continues to identify
its separately pinned Catalogue API v2 definitions and does not claim that
these files prove v3 catalogue-item conformance.

Verify the pinned files with:

```sh
(cd schema/ebu-qc-2026-04 && sha256sum -c SHA256SUMS)
```
