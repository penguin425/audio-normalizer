//! Stable machine-readable analysis reports.

use crate::normalize::Analysis;
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub enum ComplianceProfile {
    EbuR128,
}

impl ComplianceProfile {
    pub fn name(self) -> &'static str {
        match self {
            Self::EbuR128 => "ebu-r128",
        }
    }

    pub fn target_lufs(self) -> f64 {
        match self {
            Self::EbuR128 => -23.0,
        }
    }

    pub fn loudness_tolerance_lu(self) -> f64 {
        match self {
            Self::EbuR128 => 0.2,
        }
    }

    pub fn max_true_peak_dbtp(self) -> f64 {
        match self {
            Self::EbuR128 => -1.0,
        }
    }

    pub fn evaluate(self, analysis: &Analysis) -> ComplianceResult {
        let loudness_pass =
            (analysis.lufs - self.target_lufs()).abs() <= self.loudness_tolerance_lu();
        let true_peak_pass = analysis.true_peak_db() <= self.max_true_peak_dbtp();
        ComplianceResult {
            profile: self.name(),
            target_lufs: self.target_lufs(),
            loudness_tolerance_lu: self.loudness_tolerance_lu(),
            max_true_peak_dbtp: self.max_true_peak_dbtp(),
            loudness_pass,
            true_peak_pass,
            passed: loudness_pass && true_peak_pass,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceResult {
    pub profile: &'static str,
    pub target_lufs: f64,
    pub loudness_tolerance_lu: f64,
    pub max_true_peak_dbtp: f64,
    pub loudness_pass: bool,
    pub true_peak_pass: bool,
    pub passed: bool,
}

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
    pub compliance_profile: Option<&'static str>,
    pub compliance_target_lufs: Option<f64>,
    pub compliance_loudness_tolerance_lu: Option<f64>,
    pub compliance_max_true_peak_dbtp: Option<f64>,
    pub compliance_loudness_pass: Option<bool>,
    pub compliance_true_peak_pass: Option<bool>,
    pub compliance_passed: Option<bool>,
}

impl AnalysisReport {
    pub fn new(path: &Path, analysis: &Analysis) -> Self {
        Self::with_compliance(path, analysis, None)
    }

    pub fn with_compliance(
        path: &Path,
        analysis: &Analysis,
        profile: Option<ComplianceProfile>,
    ) -> Self {
        let compliance = profile.map(|profile| profile.evaluate(analysis));
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
            compliance_profile: compliance.as_ref().map(|result| result.profile),
            compliance_target_lufs: compliance.as_ref().map(|result| result.target_lufs),
            compliance_loudness_tolerance_lu: compliance
                .as_ref()
                .map(|result| result.loudness_tolerance_lu),
            compliance_max_true_peak_dbtp: compliance
                .as_ref()
                .map(|result| result.max_true_peak_dbtp),
            compliance_loudness_pass: compliance.as_ref().map(|result| result.loudness_pass),
            compliance_true_peak_pass: compliance.as_ref().map(|result| result.true_peak_pass),
            compliance_passed: compliance.as_ref().map(|result| result.passed),
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
            compliance_profile: None,
            compliance_target_lufs: None,
            compliance_loudness_tolerance_lu: None,
            compliance_max_true_peak_dbtp: None,
            compliance_loudness_pass: None,
            compliance_true_peak_pass: None,
            compliance_passed: None,
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

    #[test]
    fn ebu_compliance_checks_loudness_and_true_peak() {
        let mut analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            kind: crate::wav::PcmKind::S24,
            lufs: -23.1,
            max_momentary_lufs: -20.0,
            max_short_term_lufs: -21.0,
            loudness_range_lu: 4.0,
            rms_db: -25.0,
            sample_peak: 0.5,
            true_peak: 0.8,
            loudness_blocks: Vec::new(),
        };
        assert!(ComplianceProfile::EbuR128.evaluate(&analysis).passed);
        analysis.true_peak = 1.0;
        let result = ComplianceProfile::EbuR128.evaluate(&analysis);
        assert!(result.loudness_pass);
        assert!(!result.true_peak_pass);
        assert!(!result.passed);
    }
}
