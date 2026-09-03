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
  Schema v2 can also set `album_decoded_references` to the complete ordered
  album. It derives `alou` from the combined population of complete 400 ms
  BS.1770 gating blocks, never from an equal-weight average of track values.
  Every decoded reference must carry an authoritative speaker layout;
  ambiguous or scene-based channel order is rejected because this request
  schema has no speaker-layout override.
* MXF and unsupported containers are validate-and-copy only.  A mutation
  request fails closed rather than risking partition or offset corruption.

The ISO-BMFF writer supports exactly one audio track per destination. It
buffers only `moov`, hashes every `mdat` payload before and after, and adjusts
unfragmented `stco/co64` entries when `moov` grows. It refuses
APAC, xHE-AAC and presentation codecs, media files containing `moof`, unknown
`saio/iloc/tfra` offset mechanisms, and 32-bit offset overflow.  Those cases
need a presentation-aware muxer rather than an implicit metadata patch.

For schema v2, the album list must contain the selected track reference exactly
once after canonical path resolution; aliases and duplicates are rejected. The
default `max_album_references` is 1000 (hard maximum 10000), while
`max_input_bytes` and `max_decoded_samples` are aggregate limits over the
unique reference set. Combined analysis is also capped at one million complete
gating blocks. This follows the complete 400 ms / 75%-overlap gating population
defined by [ITU-R BS.1770-5](https://www.itu.int/rec/R-REC-BS.1770-5-202311-I)
and the album-metadata guidance in
[EBU R 128 s2](https://tech.ebu.ch/publications/r128s2).

`container_qc::audit` and the ADM production-profile validator run before and
after every copy.  Reports include source/output SHA-256 values, action IDs,
pass/fail state, and explicit byte/count/XML limits.  `atomic_replace: true`
uses a temporary file in the destination directory and renames it only after
the output has been flushed.  The source is never replaced by this API;
`overwrite: true` is required to replace an existing destination.

When `--output` is used, the report destination is preflighted before the
repair begins and checked again immediately before publication. It must not
resolve to the request, source, repair destination, or any explicit decoded
reference through path normalization, a hard link, or a symlink. Its final
component must be a regular file or not yet exist; report-output symlinks are
always rejected, as are Windows reparse points. The report is written to a
sibling temporary file and atomically published. Failures detected before the
final rename preserve an existing report's bytes; successful publication
retains Unix mode bits or the Windows read-only state unless the operating
system reports an error. ACLs and extended attributes are not copied. The
containing output directory remains a trust boundary against hostile concurrent
renames.

Exit status 0 means both audits passed, status 1 emits a report requiring
review, and status 2 denotes a malformed request, unsupported mutation, or
resource/I/O error.  The contract is versioned by
[`metadata-repair-request-v1.schema.json`](schema/metadata-repair-request-v1.schema.json)
and
[`metadata-repair-report-v1.schema.json`](schema/metadata-repair-report-v1.schema.json).
Schema v2 adds bounded album references and structured album measurement
evidence in
[`metadata-repair-request-v2.schema.json`](schema/metadata-repair-request-v2.schema.json)
and
[`metadata-repair-report-v2.schema.json`](schema/metadata-repair-report-v2.schema.json);
schema v1 remains accepted unchanged.
