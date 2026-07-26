use forge_normalizer::normalize::Analysis;
use forge_normalizer::report::{self, AnalysisReport};
use forge_normalizer::wav::{ChannelRole, PcmKind};
use serde_json::Value;

#[test]
fn emitted_delivery_manifest_conforms_to_published_schema() {
    let analysis = Analysis {
        sample_rate: 48_000,
        channels: 2,
        channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
        frames: 480_000,
        kind: PcmKind::S16,
        lufs: -23.0,
        max_momentary_lufs: -22.5,
        max_short_term_lufs: -22.8,
        loudness_range_lu: 5.0,
        rms_db: -24.0,
        sample_peak: 0.2,
        true_peak: 0.25,
        loudness_blocks: vec![],
    };
    let reports = [AnalysisReport::new("programme.wav".as_ref(), &analysis)];
    let mut output = Vec::new();
    report::write_manifest(&mut output, &reports).expect("manifest serialization");
    let instance: Value = serde_json::from_slice(&output).expect("manifest JSON");
    let schema: Value =
        serde_json::from_str(include_str!("../schema/delivery-manifest-v1.schema.json"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}
