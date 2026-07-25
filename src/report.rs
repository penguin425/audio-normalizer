//! Stable machine-readable analysis reports.

use crate::normalize::Analysis;
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct AnalysisReport {
    pub path: String,
    pub duration_seconds: f64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: &'static str,
    pub integrated_lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub rms_dbfs: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

impl AnalysisReport {
    pub fn new(path: &Path, analysis: &Analysis) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            duration_seconds: analysis.duration_secs(),
            sample_rate_hz: analysis.sample_rate,
            channels: analysis.channels,
            sample_format: match analysis.kind {
                crate::wav::PcmKind::U8 => "u8",
                crate::wav::PcmKind::S16 => "s16",
                crate::wav::PcmKind::S24 => "s24",
                crate::wav::PcmKind::S32 => "s32",
                crate::wav::PcmKind::F32 => "f32",
                crate::wav::PcmKind::F64 => "f64",
            },
            integrated_lufs: analysis.lufs,
            max_momentary_lufs: analysis.max_momentary_lufs,
            max_short_term_lufs: analysis.max_short_term_lufs,
            loudness_range_lu: analysis.loudness_range_lu,
            rms_dbfs: analysis.rms_db,
            sample_peak_dbfs: analysis.sample_peak_db(),
            true_peak_dbtp: analysis.true_peak_db(),
        }
    }
}

pub fn write_json<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    serde_json::to_writer_pretty(writer, reports).map_err(|error| format!("write JSON: {error}"))
}

pub fn write_csv<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    let mut csv = csv::Writer::from_writer(writer);
    for report in reports {
        csv.serialize(report)
            .map_err(|error| format!("write CSV: {error}"))?;
    }
    csv.flush().map_err(|error| format!("flush CSV: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> AnalysisReport {
        AnalysisReport {
            path: "album/track, one.wav".into(),
            duration_seconds: 12.5,
            sample_rate_hz: 48_000,
            channels: 2,
            sample_format: "s24",
            integrated_lufs: -23.0,
            max_momentary_lufs: -20.0,
            max_short_term_lufs: -21.0,
            loudness_range_lu: 8.0,
            rms_dbfs: -25.0,
            sample_peak_dbfs: -3.0,
            true_peak_dbtp: -2.8,
        }
    }

    #[test]
    fn json_is_an_array_with_named_fields() {
        let mut output = Vec::new();
        write_json(&mut output, &[sample_report()]).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value[0]["integrated_lufs"], -23.0);
        assert_eq!(value[0]["path"], "album/track, one.wav");
    }

    #[test]
    fn csv_has_headers_and_quotes_paths() {
        let mut output = Vec::new();
        write_csv(&mut output, &[sample_report()]).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("path,duration_seconds,"));
        assert!(text.contains("\"album/track, one.wav\""));
    }
}
