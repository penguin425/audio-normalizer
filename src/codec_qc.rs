//! Automated codec-delivery metadata extraction and decoded-audio QC.

use crate::normalize::{self, Analysis, DialogueMeasurement};
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
    use serde_json::json;

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
}
