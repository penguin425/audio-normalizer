# ADM presentation QC

`forge-adm-presentation-qc` renders every `audioProgramme` and every Cartesian
combination of its complementary-object groups. Rendering is delegated to the
[EBU ADM Renderer](https://github.com/ebu/ebu_adm_renderer); Forge independently
measures each resulting WAVE file with its ITU-R BS.1770-5 engine.

```sh
forge-adm-presentation-qc programme.bw64 \
  --renderer /path/to/ear-render \
  --layout 0+5+0 \
  --output adm-presentations.json
```

The input must contain `axml` and `chna`. Forge applies the EBU Tech 3393:2025
writing-profile audit, resolves the programme/content/object graph, and invokes
`ear-render` with `--programme` and one `--comp-object` selection per relevant
group. A group includes its root object and every
`audioComplementaryObjectIDRef`; no-selection defaults are therefore measured
explicitly rather than assumed.

If programme loudness metadata is present, `loudnessMethod` must identify
ITU-R BS.1770-5 or later and `integratedLoudness` must be present. Measured
integrated loudness and optional `maxTruePeak` are compared with explicit
tolerances.

## Safety and evidence

- The total expansion is calculated before the renderer starts. The default
  maximum is 256 presentations and the hard maximum is 4096.
- Each renderer process has a timeout and a 1 MiB limit for each diagnostic
  stream. The sample-count limit also derives a WAVE byte limit that is checked
  while the renderer is running, before bounded decoding.
- The input, renderer executable, and each output are bound by SHA-256. Input
  and renderer hashes are checked again after all renders.
- Reports and optionally retained renders are written through sibling temporary
  files and committed only after a complete copy.
- The versioned report contract is
  [`schema/adm-presentation-report-v1.schema.json`](schema/adm-presentation-report-v1.schema.json).

Use `--retain-renders DIR` to retain numbered WAVE outputs. Existing reports or
renders are not replaced unless `--overwrite` is supplied.

Exit status is 0 for PASS, 1 for a completed QC failure, and 2 for invalid
input, a safety-limit violation, or a renderer/runtime error.

## Scope

The command enumerates discrete programmes and complementary-object choices.
Programme/content references to `alternativeValueSet` are applied by the
reference renderer, but Forge does not sample continuous gain or position
interactivity. Those ranges require a separate safety analysis rather than an
unbounded set of renders.
