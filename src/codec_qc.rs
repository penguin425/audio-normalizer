//! Automated codec-delivery metadata extraction and decoded-audio QC.

use crate::container_qc::ContainerAudit;
use crate::normalize::{self, Analysis, DialogueMeasurement};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PROBE_SCHEMA: &str = "ffprobe-json-v1";

#[derive(Debug, Clone)]
pub struct CodecProbe {
    pub tool: String,
    pub codec: String,
    pub profile: Option<String>,
    pub container: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub channel_layout: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub dialnorm_lkfs: Option<f64>,
    pub downmix_mode: Option<String>,
    pub drc_profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodecDeliveryQc {
    pub probe: CodecProbe,
    pub loudness_basis: &'static str,
    pub dialnorm_deviation_lu: Option<f64>,
    pub dialnorm_pass: Option<bool>,
    pub reference_path: Option<PathBuf>,
    pub loudness_drift_lu: Option<f64>,
    pub true_peak_drift_db: Option<f64>,
    pub duration_drift_seconds: Option<f64>,
    pub roundtrip_pass: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct XheDecodedMetadataQc {
    pub track_id: u64,
    pub tolerance_lu_db: f64,
    pub apple_basic_drc_profile_passed: bool,
    pub decoded_program_lufs: f64,
    pub metadata_program_lkfs: Option<f64>,
    pub program_deviation_lu: Option<f64>,
    pub program_pass: Option<bool>,
    pub decoded_anchor_lufs: Option<f64>,
    pub metadata_anchor_lkfs: Option<f64>,
    pub anchor_deviation_lu: Option<f64>,
    pub anchor_pass: Option<bool>,
    pub decoded_sample_peak_dbfs: f64,
    pub metadata_sample_peak_dbfs: Option<f64>,
    pub sample_peak_deviation_db: Option<f64>,
    pub sample_peak_pass: Option<bool>,
    pub decoded_true_peak_dbtp: f64,
    pub metadata_true_peak_dbtp: Option<f64>,
    pub true_peak_deviation_db: Option<f64>,
    pub true_peak_pass: Option<bool>,
    pub fully_reconciled: bool,
    pub passed: bool,
}

pub fn evaluate_xhe_decoded_metadata(
    audit: &ContainerAudit,
    decoded_program: &Analysis,
    decoded_anchor: Option<&Analysis>,
    tolerance: f64,
) -> Result<XheDecodedMetadataQc, String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("xHE-AAC metadata tolerance must be a finite non-negative number".into());
    }
    let tracks = audit.properties["tracks"]
        .as_array()
        .ok_or("container audit has no track inventory")?;
    let xhe_tracks = tracks
        .iter()
        .filter(|track| track["xhe_aac_usac_config"]["audio_object_type"].as_u64() == Some(42))
        .collect::<Vec<_>>();
    if xhe_tracks.len() != 1 {
        return Err(format!(
            "decoded-reference reconciliation requires exactly one xHE-AAC track, found {}",
            xhe_tracks.len()
        ));
    }
    let track = xhe_tracks[0];
    let track_id = track["track_id"]
        .as_u64()
        .ok_or("xHE-AAC track has no numeric track ID")?;
    let config = &track["xhe_aac_usac_config"];
    let apple_basic_drc_profile_passed = config["apple_basic_drc_profile"]["compliant"]
        .as_bool()
        .unwrap_or(false);
    let base = config["loudness_info_set"]["track"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| {
            entry["drc_set_id"].as_u64() == Some(0)
                && entry["eq_set_id"].as_u64() == Some(0)
                && entry["downmix_id"].as_u64() == Some(0)
        })
        .ok_or("xHE-AAC metadata has no base-layout loudnessInfo entry")?;
    let measurements = base["measurements"]
        .as_array()
        .ok_or("xHE-AAC base loudnessInfo has no measurements")?;
    let metadata_program_lkfs = loudness_measurement(measurements, 1);
    let metadata_anchor_lkfs = loudness_measurement(measurements, 2);
    let metadata_sample_peak_dbfs = base["sample_peak_level_dbfs"].as_f64();
    let metadata_true_peak_dbtp = (base["true_peak_measurement_system"].as_u64() == Some(2))
        .then(|| base["true_peak_level_dbtp"].as_f64())
        .flatten();

    let program_deviation_lu =
        metadata_program_lkfs.map(|metadata| decoded_program.lufs - metadata);
    let program_pass = program_deviation_lu.map(|deviation| deviation.abs() <= tolerance);
    let decoded_anchor_lufs = decoded_anchor.map(|analysis| analysis.lufs);
    let anchor_deviation_lu = decoded_anchor_lufs
        .zip(metadata_anchor_lkfs)
        .map(|(decoded, metadata)| decoded - metadata);
    let anchor_pass = anchor_deviation_lu.map(|deviation| deviation.abs() <= tolerance);
    let sample_peak_deviation_db =
        metadata_sample_peak_dbfs.map(|metadata| decoded_program.sample_peak_db() - metadata);
    let sample_peak_pass = sample_peak_deviation_db.map(|deviation| deviation.abs() <= tolerance);
    let true_peak_deviation_db =
        metadata_true_peak_dbtp.map(|metadata| decoded_program.true_peak_db() - metadata);
    let true_peak_pass = true_peak_deviation_db.map(|deviation| deviation.abs() <= tolerance);
    let peak_reconciled = sample_peak_pass == Some(true) || true_peak_pass == Some(true);
    let fully_reconciled = anchor_pass.is_some()
        && peak_reconciled
        && program_pass != Some(false)
        && sample_peak_pass != Some(false)
        && true_peak_pass != Some(false);
    let passed = apple_basic_drc_profile_passed
        && fully_reconciled
        && anchor_pass == Some(true)
        && program_pass != Some(false);

    Ok(XheDecodedMetadataQc {
        track_id,
        tolerance_lu_db: tolerance,
        apple_basic_drc_profile_passed,
        decoded_program_lufs: decoded_program.lufs,
        metadata_program_lkfs,
        program_deviation_lu,
        program_pass,
        decoded_anchor_lufs,
        metadata_anchor_lkfs,
        anchor_deviation_lu,
        anchor_pass,
        decoded_sample_peak_dbfs: decoded_program.sample_peak_db(),
        metadata_sample_peak_dbfs,
        sample_peak_deviation_db,
        sample_peak_pass,
        decoded_true_peak_dbtp: decoded_program.true_peak_db(),
        metadata_true_peak_dbtp,
        true_peak_deviation_db,
        true_peak_pass,
        fully_reconciled,
        passed,
    })
}

fn loudness_measurement(measurements: &[Value], method_definition: u64) -> Option<f64> {
    measurements
        .iter()
        .find(|measurement| {
            measurement["method_definition"].as_u64() == Some(method_definition)
                && measurement["measurement_system"].as_u64() == Some(2)
        })
        .and_then(|measurement| measurement["value"].as_f64())
}

pub fn probe_and_evaluate(
    input: &Path,
    command: &Path,
    analysis: &Analysis,
    dialogue: Option<&DialogueMeasurement>,
    reference: Option<&Path>,
    tolerance: f64,
) -> Result<CodecDeliveryQc, String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("codec QC tolerance must be a finite non-negative number".into());
    }
    let probe = probe(input, command)?;
    let (loudness_basis, measured) = dialogue
        .map(|value| ("dialogue", value.lufs))
        .unwrap_or(("programme", analysis.lufs));
    let dialnorm_deviation_lu = probe.dialnorm_lkfs.map(|value| measured - value);
    let reference_analysis = reference.map(normalize::analyze_file).transpose()?;
    let loudness_drift_lu = reference_analysis
        .as_ref()
        .map(|reference| analysis.lufs - reference.lufs);
    let true_peak_drift_db = reference_analysis
        .as_ref()
        .map(|reference| analysis.true_peak_db() - reference.true_peak_db());
    let duration_drift_seconds = reference_analysis
        .as_ref()
        .map(|reference| analysis.duration_secs() - reference.duration_secs());
    let duration_tolerance = if analysis.sample_rate == 0 {
        0.0
    } else {
        1.0 / analysis.sample_rate as f64
    };
    let roundtrip_pass = reference_analysis.as_ref().map(|_| {
        loudness_drift_lu.is_some_and(|value| value.abs() <= tolerance)
            && true_peak_drift_db.is_some_and(|value| value.abs() <= tolerance)
            && duration_drift_seconds.is_some_and(|value| value.abs() <= duration_tolerance)
    });
    Ok(CodecDeliveryQc {
        probe,
        loudness_basis,
        dialnorm_deviation_lu,
        dialnorm_pass: dialnorm_deviation_lu.map(|value| value.abs() <= tolerance),
        reference_path: reference.map(Path::to_path_buf),
        loudness_drift_lu,
        true_peak_drift_db,
        duration_drift_seconds,
        roundtrip_pass,
    })
}

pub fn probe(input: &Path, command: &Path) -> Result<CodecProbe, String> {
    let output = Command::new(command)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_streams",
            "-show_format",
            "-show_frames",
            "-read_intervals",
            "%+#1",
            "-of",
            "json",
        ])
        .arg(input)
        .output()
        .map_err(|error| {
            format!(
                "run codec metadata prober {}: {error}; install ffprobe or pass --codec-prober PATH",
                command.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "codec metadata prober {} failed for {}: {}",
            command.display(),
            input.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("parse codec metadata prober JSON: {error}"))?;
    parse_probe(command, &value)
}

fn parse_probe(command: &Path, root: &Value) -> Result<CodecProbe, String> {
    let stream = root
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .ok_or_else(|| "codec metadata prober returned no audio stream".to_string())?;
    let format = root.get("format").unwrap_or(&Value::Null);
    let codec = text(stream, "codec_name")
        .ok_or_else(|| "codec metadata prober did not report codec_name".to_string())?;
    let dialnorm_lkfs = find_number(root, &["dialnorm", "dialnorm_lkfs"])
        .map(|value| if value > 0.0 { -value } else { value })
        .filter(|value| (-31.0..=-1.0).contains(value));
    Ok(CodecProbe {
        tool: command.to_string_lossy().into_owned(),
        codec,
        profile: text(stream, "profile"),
        container: text(format, "format_name"),
        sample_rate_hz: integer(stream, "sample_rate").and_then(|value| value.try_into().ok()),
        channels: integer(stream, "channels").and_then(|value| value.try_into().ok()),
        channel_layout: text(stream, "channel_layout"),
        bitrate_bps: integer(stream, "bit_rate").or_else(|| integer(format, "bit_rate")),
        dialnorm_lkfs,
        downmix_mode: find_text(root, &["downmix_mode", "preferred_downmix_type"]),
        drc_profile: find_text(
            root,
            &[
                "drc_profile",
                "dynamic_range_profile",
                "compression_profile",
            ],
        ),
    })
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|item| match item {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

fn integer(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|item| match item {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    })
}

fn find_text(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if keys
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate))
                {
                    if let Some(text) = item.as_str().filter(|text| !text.trim().is_empty()) {
                        return Some(text.to_string());
                    }
                }
                if let Some(found) = find_text(item, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|item| find_text(item, keys)),
        _ => None,
    }
}

fn find_number(value: &Value, keys: &[&str]) -> Option<f64> {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if keys
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate))
                {
                    let parsed = item
                        .as_f64()
                        .or_else(|| item.as_str().and_then(|text| text.parse().ok()));
                    if parsed.is_some() {
                        return parsed;
                    }
                }
                if let Some(found) = find_number(item, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|item| find_number(item, keys)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_qc::ContainerAudit;
    use crate::wav::PcmKind;
    use serde_json::json;

    fn analysis(lufs: f64) -> Analysis {
        let minus_one_db = 10_f32.powf(-1.0 / 20.0);
        Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: Vec::new(),
            frames: 48_000,
            kind: PcmKind::F32,
            lufs,
            max_momentary_lufs: lufs,
            max_short_term_lufs: lufs,
            loudness_range_lu: 0.0,
            rms_db: lufs,
            sample_peak: minus_one_db,
            true_peak: minus_one_db,
            loudness_blocks: Vec::new(),
        }
    }

    #[test]
    fn parses_stream_and_nested_delivery_metadata() {
        let value = json!({
            "streams": [{
                "codec_name": "eac3",
                "profile": "E-AC-3",
                "sample_rate": "48000",
                "channels": 6,
                "channel_layout": "5.1(side)",
                "bit_rate": "384000",
                "side_data_list": [{
                    "dialnorm": 24,
                    "downmix_mode": "loro",
                    "drc_profile": "film_standard"
                }]
            }],
            "format": {"format_name": "eac3"}
        });
        let probe = parse_probe(Path::new("ffprobe"), &value).unwrap();
        assert_eq!(probe.codec, "eac3");
        assert_eq!(probe.sample_rate_hz, Some(48_000));
        assert_eq!(probe.channels, Some(6));
        assert_eq!(probe.dialnorm_lkfs, Some(-24.0));
        assert_eq!(probe.downmix_mode.as_deref(), Some("loro"));
        assert_eq!(probe.drc_profile.as_deref(), Some("film_standard"));
    }

    #[test]
    fn rejects_probe_without_audio_stream() {
        assert!(parse_probe(Path::new("ffprobe"), &json!({})).is_err());
    }

    #[test]
    fn reconciles_xhe_metadata_with_independent_pcm_measurements() {
        let audit = ContainerAudit {
            schema: "test",
            generator: "test",
            path: "input.mp4".into(),
            format: "isobmff".into(),
            passed: true,
            layers: Vec::new(),
            properties: json!({
                "tracks": [{
                    "track_id": 1,
                    "xhe_aac_usac_config": {
                        "audio_object_type": 42,
                        "apple_basic_drc_profile": {"compliant": true},
                        "loudness_info_set": {"track": [{
                            "drc_set_id": 0,
                            "eq_set_id": 0,
                            "downmix_id": 0,
                            "sample_peak_level_dbfs": -1.0,
                            "true_peak_level_dbtp": -1.0,
                            "true_peak_measurement_system": 2,
                            "measurements": [
                                {"method_definition": 1, "measurement_system": 2, "value": -24.0},
                                {"method_definition": 2, "measurement_system": 2, "value": -24.0}
                            ]
                        }]}
                    }
                }]
            }),
        };
        let programme = analysis(-24.0);
        let anchor = analysis(-24.0);
        let qc = evaluate_xhe_decoded_metadata(&audit, &programme, Some(&anchor), 0.01).unwrap();
        assert!(qc.fully_reconciled);
        assert!(qc.passed);

        let incomplete = evaluate_xhe_decoded_metadata(&audit, &programme, None, 0.01).unwrap();
        assert!(!incomplete.fully_reconciled);
        assert!(!incomplete.passed);
    }
}
