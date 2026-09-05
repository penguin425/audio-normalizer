use forge_normalizer::{ac4_adapter, dts_adapter, mpegh_adapter};
use std::path::Path;

#[test]
fn legacy_adapter_entry_points_and_schemas_remain_v1() {
    let _: fn(&ac4_adapter::AdapterOptions) -> Result<ac4_adapter::Ac4AdapterReport, String> =
        ac4_adapter::run;
    let _: fn(&mpegh_adapter::AdapterOptions) -> Result<mpegh_adapter::MpeghAdapterReport, String> =
        mpegh_adapter::run;
    let _: fn(&dts_adapter::AdapterOptions) -> Result<dts_adapter::DtsAdapterReport, String> =
        dts_adapter::run;
    let _: fn(&Path, &ac4_adapter::Ac4AdapterReport, bool, bool) -> Result<(), String> =
        ac4_adapter::write_report;
    let _: fn(&Path, &mpegh_adapter::MpeghAdapterReport, bool, bool) -> Result<(), String> =
        mpegh_adapter::write_report;
    let _: fn(&Path, &dts_adapter::DtsAdapterReport, bool, bool) -> Result<(), String> =
        dts_adapter::write_report;

    assert_eq!(
        ac4_adapter::REPORT_SCHEMA,
        "https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-report-v1"
    );
    assert_eq!(
        mpegh_adapter::REPORT_SCHEMA,
        "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-report-v1"
    );
    assert_eq!(
        dts_adapter::REPORT_SCHEMA,
        "https://penguin425.github.io/audio-normalizer/schema/dts-adapter-report-v1"
    );
    assert_eq!(
        ac4_adapter::REPORT_SCHEMA_V2,
        "https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-report-v2"
    );
    assert_eq!(
        mpegh_adapter::REPORT_SCHEMA_V2,
        "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-report-v2"
    );
    assert_eq!(
        dts_adapter::REPORT_SCHEMA_V2,
        "https://penguin425.github.io/audio-normalizer/schema/dts-adapter-report-v2"
    );
}

#[test]
fn legacy_presentation_results_remain_constructible_without_layout_fields() {
    let ac4 = ac4_adapter::PresentationResult {
        id: "main".into(),
        presentation_version: 1,
        output_layout: "stereo".into(),
        language: None,
        accessibility: None,
        loudness_metadata: ac4_adapter::Ac4LoudnessMetadata {
            dialnorm_bits: 96,
            dialnorm_lkfs: -24.0,
            dialnorm_source: ac4_adapter::DialnormSource::PresentationSubstream,
            downmix_correction_db: None,
            alternative_presentation_correction_db: None,
            realtime_correction_db: None,
        },
        rendered_sha256: "0".repeat(64),
        rendered_bytes: 1,
        sample_rate_hz: 48_000,
        channels: 2,
        duration_seconds: 1.0,
        measured_integrated_lufs: -24.0,
        measured_true_peak_dbtp: -1.0,
        dialnorm_drift_lu: 0.0,
        dialnorm_passed: true,
        true_peak_passed: None,
        passed: true,
        checks: Vec::new(),
    };
    let mpegh = mpegh_adapter::PresentationResult {
        id: "main".into(),
        preset_id: None,
        output_layout: "stereo".into(),
        language: None,
        accessibility: None,
        loudness_metadata: mpegh_adapter::MpeghLoudnessMetadata {
            loudness_info_type: 0,
            mae_group_id: None,
            mae_group_preset_id: None,
            method_definition: 1,
            program_loudness_lkfs: -24.0,
            drc_set_id: 0,
            downmix_id: 0,
            measurement_system: None,
        },
        rendered_sha256: "0".repeat(64),
        rendered_bytes: 1,
        sample_rate_hz: 48_000,
        channels: 2,
        duration_seconds: 1.0,
        measured_integrated_lufs: -24.0,
        measured_true_peak_dbtp: -1.0,
        loudness_drift_lu: 0.0,
        loudness_passed: true,
        true_peak_passed: None,
        passed: true,
        checks: Vec::new(),
    };
    let dts = dts_adapter::PresentationResult {
        id: "main".into(),
        asset_ids: vec!["core".into()],
        output_layout: "stereo".into(),
        language: None,
        accessibility: None,
        declared_sample_rate_hz: 48_000,
        declared_channels: 2,
        rendered_sha256: "0".repeat(64),
        rendered_bytes: 1,
        sample_rate_hz: 48_000,
        channels: 2,
        duration_seconds: 1.0,
        measured_integrated_lufs: -24.0,
        measured_true_peak_dbtp: -1.0,
        sample_rate_passed: true,
        channels_passed: true,
        true_peak_passed: None,
        passed: true,
        checks: Vec::new(),
    };

    for value in [
        serde_json::to_value(ac4).unwrap(),
        serde_json::to_value(mpegh).unwrap(),
        serde_json::to_value(dts).unwrap(),
    ] {
        assert!(value.get("channel_layout").is_none());
    }
}
