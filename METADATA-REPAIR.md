# Metadata repair

`forge-metadata-repair` validates a delivery file, copies it to a separate
destination, and optionally performs conservative byte-preserving repairs.
Audio essence is never decoded or re-encoded.  Unknown WAVE chunks, padding,
ADM XML outside the explicitly selected attribute, and all MXF bytes are
preserved.

```bash
forge-metadata-repair metadata-repair.json --output metadata-repair-report.json
```

The request is JSON or TOML and must contain `schema_version`, `source`, and a
different `destination`.  `mode: "validate"` performs a standards audit and
exact copy.  `mode: "repair"` (the default) supports:

* BWF `bext` v2 normalization and replacement of the five EBU R 128 loudness
  fields, only for RIFF/WAVE.  A missing `bext` is inserted before `data` when
  `ensure_bwf_v2` or `bwf_loudness` is requested.
* Updating an existing ADM `axml` `audioFormatExtended/@version` to the
  published `ITU-R_BS.2076-3` value, followed by the ADM profile validator.
* MXF and other containers are validate-and-copy only.  A mutation request for
  them fails closed rather than risking KLV partition/index corruption.

`container_qc::audit` and the ADM production-profile validator run before and
after every copy.  Reports include source/output SHA-256 values, action IDs,
pass/fail state, and explicit byte/count/XML limits.  `atomic_replace: true`
uses a temporary file in the destination directory and renames it only after
the output has been flushed.  The source is never replaced by this API;
`overwrite: true` is required to replace an existing destination.

Exit status 0 means both audits passed, status 1 emits a report requiring
review, and status 2 denotes a malformed request, unsupported mutation, or
resource/I/O error.  The contract is versioned by
[`metadata-repair-request-v1.schema.json`](schema/metadata-repair-request-v1.schema.json)
and
[`metadata-repair-report-v1.schema.json`](schema/metadata-repair-report-v1.schema.json).
