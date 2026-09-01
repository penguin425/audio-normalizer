# Metadata repair

`forge-metadata-repair` validates a delivery file, copies it to a separate
destination, and optionally performs conservative byte-preserving repairs.
Audio essence is never re-encoded.  ISO-BMFF loudness repair decodes a bounded
reference only for measurement; the original media payload remains unchanged.
Unknown WAVE chunks, padding, ADM XML outside the explicitly selected
attribute, ISO-BMFF media payloads, and all MXF bytes are preserved.

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
* `isobmff_loudness: {}` measures the single audio track's decoded PCM and
  creates or replaces ISO/IEC 14496-12 `ludt/tlou` Program Loudness, sample
  peak, and true-peak metadata.  Values use MPEG-D's 0.25 LU and 1/32 dB
  quantization with BS.1770/accurate provenance.  For an fMP4 initialization
  segment, set `decoded_reference` to a bounded PCM or complete encoded render.
* MXF and unsupported containers are validate-and-copy only.  A mutation
  request fails closed rather than risking partition or offset corruption.

The ISO-BMFF writer supports exactly one audio track.  It buffers only `moov`,
preserves existing album loudness, hashes every `mdat` payload before and after,
and adjusts unfragmented `stco/co64` entries when `moov` grows.  It refuses
APAC, xHE-AAC and presentation codecs, media files containing `moof`, unknown
`saio/iloc/tfra` offset mechanisms, and 32-bit offset overflow.  Those cases
need a presentation-aware muxer rather than an implicit metadata patch.

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
