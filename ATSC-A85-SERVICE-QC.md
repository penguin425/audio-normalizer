# ATSC A/85 streaming-service QC

`forge-streaming-qc --profile atsc-a85-service` checks a bounded set of decoded
service assets against ATSC A/85:2026 Annex L and the applicable Annex M
measurement and true-peak requirements.

```sh
forge-streaming-qc service.json --profile atsc-a85-service -o report.json
```

The request is JSON or TOML and uses the
[`atsc-a85-service-request-v1`](schema/atsc-a85-service-request-v1.schema.json)
contract. Relative audio paths resolve from the request directory.

```json
{
  "schema": "https://penguin425.github.io/audio-normalizer/schema/atsc-a85-service-request-v1",
  "service_id": "example-service",
  "target_lkfs": -24,
  "assets": [
    {
      "id": "programme",
      "path": "programme.wav",
      "programme_kind": "long_form",
      "delivery_codec": "ac3",
      "declaration_source": "traffic-system dialnorm export",
      "declared_loudness_lkfs": -24,
      "dialogue_ranges": [
        { "start_seconds": 12.5, "duration_seconds": 18.0 }
      ]
    },
    {
      "id": "promo",
      "path": "promo.wav",
      "programme_kind": "short_form",
      "delivery_codec": "aac",
      "declaration_source": "packager codec report",
      "accompanies": "programme",
      "inserted": true
    }
  ]
}
```

Long-form assets require explicit dialogue ranges unless they are explicitly
marked `dialogue_free`. Short-form assets use full-programme loudness and must
name an accompanying long-form asset. The default target is -24 LKFS; targets
outside the recommended -27 to -23 LKFS range require
`"target_authority": "prior_arrangement"`.

The report checks codec-metadata agreement, non-metadata target agreement,
inserted-content level, short-versus-long normalized playback level, mixed-mode
consistency, and a -2 dBTP maximum. Codec and encoded-loudness values are
operator declarations bound by the request SHA-256. Forge independently
measures the decoded renders; it does not claim native metadata extraction for
every licensed codec.
