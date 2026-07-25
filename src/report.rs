//! Stable machine-readable analysis reports.

use crate::dsp::lufs::LoudnessTimelinePoint;
use crate::normalize::{Analysis, DialogueMeasurement};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComplianceProfile {
    pub name: String,
    #[serde(default)]
    pub loudness_basis: LoudnessBasis,
    pub target_lufs: Option<f64>,
    pub loudness_tolerance_lu: Option<f64>,
    pub lower_tolerance_lu: Option<f64>,
    pub upper_tolerance_lu: Option<f64>,
    pub max_true_peak_dbtp: Option<f64>,
    pub max_short_term_lufs: Option<f64>,
    pub max_momentary_lufs: Option<f64>,
    pub min_loudness_range_lu: Option<f64>,
    pub max_loudness_range_lu: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoudnessBasis {
    #[default]
    Programme,
    Dialogue,
}

impl ComplianceProfile {
    pub fn builtin(name: &str) -> Option<Self> {
        let profile = match name {
            "ebu-r128" => Self::symmetric("ebu-r128", -23.0, 0.2, -1.0),
            "ebu-r128-short" => Self {
                max_short_term_lufs: Some(-18.0),
                ..Self::symmetric("ebu-r128-short", -23.0, 0.2, -1.0)
            },
            "atsc-a85-short" => Self::symmetric("atsc-a85-short", -24.0, 2.0, -2.0),
            "atsc-a85-long" => Self {
                loudness_basis: LoudnessBasis::Dialogue,
                ..Self::symmetric("atsc-a85-long", -24.0, 2.0, -2.0)
            },
            "aes77-assorted" => Self {
                name: "aes77-assorted".into(),
                loudness_basis: LoudnessBasis::Programme,
                target_lufs: Some(-18.0),
                loudness_tolerance_lu: None,
                lower_tolerance_lu: None,
                upper_tolerance_lu: Some(2.0),
                max_true_peak_dbtp: Some(-1.0),
                max_short_term_lufs: None,
                max_momentary_lufs: None,
                min_loudness_range_lu: None,
                max_loudness_range_lu: None,
            },
            "aes77-music-track" => Self::symmetric("aes77-music-track", -16.0, 0.2, -1.0),
            "aes77-interstitial" => Self::symmetric("aes77-interstitial", -18.0, 0.2, -1.0),
            _ => return None,
        };
        Some(profile)
    }

    fn symmetric(name: &str, target: f64, tolerance: f64, true_peak: f64) -> Self {
        Self {
            name: name.into(),
            loudness_basis: LoudnessBasis::Programme,
            target_lufs: Some(target),
            loudness_tolerance_lu: Some(tolerance),
            lower_tolerance_lu: None,
            upper_tolerance_lu: None,
            max_true_peak_dbtp: Some(true_peak),
            max_short_term_lufs: None,
            max_momentary_lufs: None,
            min_loudness_range_lu: None,
            max_loudness_range_lu: None,
        }
    }

    pub fn load(name_or_path: &str) -> Result<Self, String> {
        if let Some(profile) = Self::builtin(name_or_path) {
            return Ok(profile);
        }
        let path = Path::new(name_or_path);
        let text = fs::read_to_string(path)
            .map_err(|error| format!("read compliance profile {}: {error}", path.display()))?;
        let profile: Self = match path.extension().and_then(|value| value.to_str()) {
            Some("json") => serde_json::from_str(&text)
                .map_err(|error| format!("parse JSON profile {}: {error}", path.display()))?,
            Some("toml") => toml::from_str(&text)
                .map_err(|error| format!("parse TOML profile {}: {error}", path.display()))?,
            _ => {
                return Err("custom compliance profiles must use a .json or .toml extension".into())
            }
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("compliance profile name cannot be empty".into());
        }
        if self.loudness_tolerance_lu.is_some()
            && (self.lower_tolerance_lu.is_some() || self.upper_tolerance_lu.is_some())
        {
            return Err(
                "use either loudness_tolerance_lu or asymmetric lower/upper tolerances".into(),
            );
        }
        if self.target_lufs.is_some()
            && self.loudness_tolerance_lu.is_none()
            && self.lower_tolerance_lu.is_none()
            && self.upper_tolerance_lu.is_none()
        {
            return Err("target_lufs requires at least one loudness tolerance".into());
        }
        let values = [
            self.target_lufs,
            self.loudness_tolerance_lu,
            self.lower_tolerance_lu,
            self.upper_tolerance_lu,
            self.max_true_peak_dbtp,
            self.max_short_term_lufs,
            self.max_momentary_lufs,
            self.min_loudness_range_lu,
            self.max_loudness_range_lu,
        ];
        if values.into_iter().flatten().any(|value| !value.is_finite()) {
            return Err("compliance profile values must be finite".into());
        }
        if [
            self.loudness_tolerance_lu,
            self.lower_tolerance_lu,
            self.upper_tolerance_lu,
        ]
        .into_iter()
        .flatten()
        .any(|value| value < 0.0)
        {
            return Err("loudness tolerances must be non-negative".into());
        }
        if self.target_lufs.is_none()
            && self.max_true_peak_dbtp.is_none()
            && self.max_short_term_lufs.is_none()
            && self.max_momentary_lufs.is_none()
            && self.min_loudness_range_lu.is_none()
            && self.max_loudness_range_lu.is_none()
        {
            return Err("compliance profile must define at least one rule".into());
        }
        if self
            .min_loudness_range_lu
            .zip(self.max_loudness_range_lu)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err("minimum loudness range exceeds maximum".into());
        }
        Ok(())
    }

    pub fn evaluate(&self, analysis: &Analysis) -> Result<ComplianceResult, String> {
        self.evaluate_with_dialogue(analysis, None)
    }

    pub fn requires_dialogue(&self) -> bool {
        self.loudness_basis == LoudnessBasis::Dialogue && self.target_lufs.is_some()
    }

    pub fn evaluate_with_dialogue(
        &self,
        analysis: &Analysis,
        dialogue_lufs: Option<f64>,
    ) -> Result<ComplianceResult, String> {
        let mut rules = Vec::new();
        if let Some(target) = self.target_lufs {
            let (metric, measured) = match self.loudness_basis {
                LoudnessBasis::Programme => ("integrated_lufs", analysis.lufs),
                LoudnessBasis::Dialogue => (
                    "dialogue_lufs",
                    dialogue_lufs.ok_or_else(|| {
                        format!(
                            "compliance profile {} requires --dialogue-ranges",
                            self.name
                        )
                    })?,
                ),
            };
            let lower = target
                - self
                    .loudness_tolerance_lu
                    .or(self.lower_tolerance_lu)
                    .unwrap_or(f64::INFINITY);
            let upper = target
                + self
                    .loudness_tolerance_lu
                    .or(self.upper_tolerance_lu)
                    .unwrap_or(f64::INFINITY);
            rules.push(ComplianceRuleResult::range(metric, measured, lower, upper));
        }
        add_max_rule(
            &mut rules,
            "true_peak_dbtp",
            analysis.true_peak_db(),
            self.max_true_peak_dbtp,
        );
        add_max_rule(
            &mut rules,
            "max_short_term_lufs",
            analysis.max_short_term_lufs,
            self.max_short_term_lufs,
        );
        add_max_rule(
            &mut rules,
            "max_momentary_lufs",
            analysis.max_momentary_lufs,
            self.max_momentary_lufs,
        );
        if self.min_loudness_range_lu.is_some() || self.max_loudness_range_lu.is_some() {
            rules.push(ComplianceRuleResult::range(
                "loudness_range_lu",
                analysis.loudness_range_lu,
                self.min_loudness_range_lu.unwrap_or(f64::NEG_INFINITY),
                self.max_loudness_range_lu.unwrap_or(f64::INFINITY),
            ));
        }
        Ok(ComplianceResult {
            profile: self.name.clone(),
            passed: rules.iter().all(|rule| rule.passed),
            rules,
        })
    }
}

fn add_max_rule(
    rules: &mut Vec<ComplianceRuleResult>,
    metric: &'static str,
    measured: f64,
    maximum: Option<f64>,
) {
    if let Some(maximum) = maximum {
        rules.push(ComplianceRuleResult::maximum(metric, measured, maximum));
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceRuleResult {
    pub metric: &'static str,
    pub measured: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub passed: bool,
}

impl ComplianceRuleResult {
    fn range(metric: &'static str, measured: f64, minimum: f64, maximum: f64) -> Self {
        Self {
            metric,
            measured,
            minimum: minimum.is_finite().then_some(minimum),
            maximum: maximum.is_finite().then_some(maximum),
            passed: measured >= minimum && measured <= maximum,
        }
    }

    fn maximum(metric: &'static str, measured: f64, maximum: f64) -> Self {
        Self::range(metric, measured, f64::NEG_INFINITY, maximum)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceResult {
    pub profile: String,
    pub rules: Vec<ComplianceRuleResult>,
    pub passed: bool,
}

#[derive(Debug, Serialize)]
pub struct AnalysisReport {
    pub path: String,
    pub duration_seconds: f64,
    pub source_start_seconds: f64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: &'static str,
    pub integrated_lufs: f64,
    pub dialogue_lufs: Option<f64>,
    pub dialogue_duration_seconds: Option<f64>,
    pub dialogue_range_count: Option<usize>,
    pub dialogue_measurement_standard: Option<&'static str>,
    pub dialogue_measurement_method: Option<&'static str>,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub loudness_range_stable: bool,
    pub loudness_range_stable_after_seconds: f64,
    pub rms_dbfs: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
    pub peak_to_loudness_ratio_lu: f64,
    pub compliance_profile: Option<String>,
    pub compliance_loudness_basis: Option<LoudnessBasis>,
    pub compliance_target_lufs: Option<f64>,
    pub compliance_loudness_tolerance_lu: Option<f64>,
    pub compliance_lower_tolerance_lu: Option<f64>,
    pub compliance_upper_tolerance_lu: Option<f64>,
    pub compliance_max_true_peak_dbtp: Option<f64>,
    pub compliance_loudness_pass: Option<bool>,
    pub compliance_true_peak_pass: Option<bool>,
    pub compliance_max_short_term_lufs: Option<f64>,
    pub compliance_short_term_pass: Option<bool>,
    pub compliance_max_momentary_lufs: Option<f64>,
    pub compliance_momentary_pass: Option<bool>,
    pub compliance_min_loudness_range_lu: Option<f64>,
    pub compliance_max_loudness_range_lu: Option<f64>,
    pub compliance_loudness_range_pass: Option<bool>,
    /// Complete evaluated rule set, encoded as JSON so CSV remains flat.
    pub compliance_rules_json: Option<String>,
    pub compliance_passed: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineReport {
    pub path: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub momentary_lufs: Option<f64>,
    pub short_term_lufs: Option<f64>,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
    pub violations: Vec<&'static str>,
}

impl TimelineReport {
    pub fn from_points(
        path: &Path,
        points: &[LoudnessTimelinePoint],
        profile: Option<&ComplianceProfile>,
    ) -> Vec<Self> {
        points
            .iter()
            .map(|point| {
                let mut violations = Vec::new();
                if profile
                    .and_then(|value| value.max_momentary_lufs)
                    .zip(point.momentary_lufs)
                    .is_some_and(|(maximum, measured)| measured > maximum)
                {
                    violations.push("max_momentary_lufs");
                }
                if profile
                    .and_then(|value| value.max_short_term_lufs)
                    .zip(point.short_term_lufs)
                    .is_some_and(|(maximum, measured)| measured > maximum)
                {
                    violations.push("max_short_term_lufs");
                }
                if profile
                    .and_then(|value| value.max_true_peak_dbtp)
                    .is_some_and(|maximum| point.true_peak_dbtp > maximum)
                {
                    violations.push("max_true_peak_dbtp");
                }
                Self {
                    path: path.to_string_lossy().into_owned(),
                    start_seconds: point.start_seconds,
                    end_seconds: point.end_seconds,
                    momentary_lufs: point.momentary_lufs,
                    short_term_lufs: point.short_term_lufs,
                    sample_peak_dbfs: point.sample_peak_dbfs,
                    true_peak_dbtp: point.true_peak_dbtp,
                    violations,
                }
            })
            .collect()
    }
}

impl AnalysisReport {
    pub fn new(path: &Path, analysis: &Analysis) -> Self {
        Self::with_compliance(path, analysis, None)
    }

    pub fn with_compliance(
        path: &Path,
        analysis: &Analysis,
        profile: Option<&ComplianceProfile>,
    ) -> Self {
        Self::with_compliance_at(path, analysis, profile, 0.0)
    }

    pub fn with_compliance_at(
        path: &Path,
        analysis: &Analysis,
        profile: Option<&ComplianceProfile>,
        source_start_seconds: f64,
    ) -> Self {
        if profile.is_some_and(ComplianceProfile::requires_dialogue) {
            return Self::with_measurements_at(path, analysis, None, None, source_start_seconds)
                .expect("a report without compliance cannot fail");
        }
        Self::with_measurements_at(path, analysis, None, profile, source_start_seconds)
            .expect("programme compliance evaluation cannot fail")
    }

    pub fn with_measurements_at(
        path: &Path,
        analysis: &Analysis,
        dialogue: Option<&DialogueMeasurement>,
        profile: Option<&ComplianceProfile>,
        source_start_seconds: f64,
    ) -> Result<Self, String> {
        let compliance = profile
            .map(|profile| {
                profile.evaluate_with_dialogue(analysis, dialogue.map(|value| value.lufs))
            })
            .transpose()?;
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            duration_seconds: analysis.duration_secs(),
            source_start_seconds,
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
            dialogue_lufs: dialogue.map(|value| value.lufs),
            dialogue_duration_seconds: dialogue.map(|value| value.duration_seconds),
            dialogue_range_count: dialogue.map(|value| value.range_count),
            dialogue_measurement_standard: dialogue.map(|value| value.standard),
            dialogue_measurement_method: dialogue.map(|value| value.method),
            max_momentary_lufs: analysis.max_momentary_lufs,
            max_short_term_lufs: analysis.max_short_term_lufs,
            loudness_range_lu: analysis.loudness_range_lu,
            loudness_range_stable: analysis.loudness_range_stable(),
            loudness_range_stable_after_seconds: Analysis::LRA_STABLE_AFTER_SECONDS,
            rms_dbfs: analysis.rms_db,
            sample_peak_dbfs: analysis.sample_peak_db(),
            true_peak_dbtp: analysis.true_peak_db(),
            peak_to_loudness_ratio_lu: analysis.peak_to_loudness_ratio_lu(),
            compliance_profile: compliance.as_ref().map(|result| result.profile.clone()),
            compliance_loudness_basis: profile.map(|value| value.loudness_basis),
            compliance_target_lufs: profile.and_then(|value| value.target_lufs),
            compliance_loudness_tolerance_lu: profile.and_then(|value| value.loudness_tolerance_lu),
            compliance_lower_tolerance_lu: profile.and_then(|value| value.lower_tolerance_lu),
            compliance_upper_tolerance_lu: profile.and_then(|value| value.upper_tolerance_lu),
            compliance_max_true_peak_dbtp: profile.and_then(|value| value.max_true_peak_dbtp),
            compliance_loudness_pass: rule_pass(&compliance, "integrated_lufs")
                .or_else(|| rule_pass(&compliance, "dialogue_lufs")),
            compliance_true_peak_pass: rule_pass(&compliance, "true_peak_dbtp"),
            compliance_max_short_term_lufs: profile.and_then(|value| value.max_short_term_lufs),
            compliance_short_term_pass: rule_pass(&compliance, "max_short_term_lufs"),
            compliance_max_momentary_lufs: profile.and_then(|value| value.max_momentary_lufs),
            compliance_momentary_pass: rule_pass(&compliance, "max_momentary_lufs"),
            compliance_min_loudness_range_lu: profile.and_then(|value| value.min_loudness_range_lu),
            compliance_max_loudness_range_lu: profile.and_then(|value| value.max_loudness_range_lu),
            compliance_loudness_range_pass: rule_pass(&compliance, "loudness_range_lu"),
            compliance_rules_json: compliance.as_ref().map(|result| {
                serde_json::to_string(&result.rules).expect("compliance rules are serializable")
            }),
            compliance_passed: compliance.as_ref().map(|result| result.passed),
        })
    }
}

fn rule_pass(compliance: &Option<ComplianceResult>, metric: &str) -> Option<bool> {
    compliance.as_ref().and_then(|result| {
        result
            .rules
            .iter()
            .find(|rule| rule.metric == metric)
            .map(|rule| rule.passed)
    })
}

pub fn write_json<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    serde_json::to_writer_pretty(writer, reports).map_err(|error| format!("write JSON: {error}"))
}

pub fn write_ndjson<W: Write>(mut writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    for report in reports {
        serde_json::to_writer(&mut writer, report)
            .map_err(|error| format!("write NDJSON: {error}"))?;
        writer
            .write_all(b"\n")
            .map_err(|error| format!("write NDJSON newline: {error}"))?;
    }
    Ok(())
}

pub fn write_csv<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    let mut csv = csv::Writer::from_writer(writer);
    for report in reports {
        csv.serialize(report)
            .map_err(|error| format!("write CSV: {error}"))?;
    }
    csv.flush().map_err(|error| format!("flush CSV: {error}"))
}

pub fn write_timeline_json<W: Write>(writer: W, reports: &[TimelineReport]) -> Result<(), String> {
    serde_json::to_writer_pretty(writer, reports)
        .map_err(|error| format!("write timeline JSON: {error}"))
}

pub fn write_timeline_ndjson<W: Write>(
    mut writer: W,
    reports: &[TimelineReport],
) -> Result<(), String> {
    for report in reports {
        serde_json::to_writer(&mut writer, report)
            .map_err(|error| format!("write timeline NDJSON: {error}"))?;
        writer
            .write_all(b"\n")
            .map_err(|error| format!("write timeline NDJSON newline: {error}"))?;
    }
    Ok(())
}

pub fn write_timeline_csv<W: Write>(writer: W, reports: &[TimelineReport]) -> Result<(), String> {
    let mut csv = csv::Writer::from_writer(writer);
    csv.write_record([
        "path",
        "start_seconds",
        "end_seconds",
        "momentary_lufs",
        "short_term_lufs",
        "sample_peak_dbfs",
        "true_peak_dbtp",
        "violations_json",
    ])
    .map_err(|error| format!("write timeline CSV header: {error}"))?;
    for report in reports {
        let start = report.start_seconds.to_string();
        let end = report.end_seconds.to_string();
        let momentary = report.momentary_lufs.map(|value| value.to_string());
        let short_term = report.short_term_lufs.map(|value| value.to_string());
        let sample_peak = report.sample_peak_dbfs.to_string();
        let true_peak = report.true_peak_dbtp.to_string();
        let violations = serde_json::to_string(&report.violations)
            .map_err(|error| format!("encode timeline violations: {error}"))?;
        csv.write_record([
            report.path.as_str(),
            &start,
            &end,
            momentary.as_deref().unwrap_or(""),
            short_term.as_deref().unwrap_or(""),
            &sample_peak,
            &true_peak,
            &violations,
        ])
        .map_err(|error| format!("write timeline CSV: {error}"))?;
    }
    csv.flush()
        .map_err(|error| format!("flush timeline CSV: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> AnalysisReport {
        AnalysisReport {
            path: "album/track, one.wav".into(),
            duration_seconds: 12.5,
            source_start_seconds: 0.0,
            sample_rate_hz: 48_000,
            channels: 2,
            sample_format: "s24",
            integrated_lufs: -23.0,
            dialogue_lufs: None,
            dialogue_duration_seconds: None,
            dialogue_range_count: None,
            dialogue_measurement_standard: None,
            dialogue_measurement_method: None,
            max_momentary_lufs: -20.0,
            max_short_term_lufs: -21.0,
            loudness_range_lu: 8.0,
            loudness_range_stable: false,
            loudness_range_stable_after_seconds: 60.0,
            rms_dbfs: -25.0,
            sample_peak_dbfs: -3.0,
            true_peak_dbtp: -2.8,
            peak_to_loudness_ratio_lu: 20.2,
            compliance_profile: None,
            compliance_loudness_basis: None,
            compliance_target_lufs: None,
            compliance_loudness_tolerance_lu: None,
            compliance_lower_tolerance_lu: None,
            compliance_upper_tolerance_lu: None,
            compliance_max_true_peak_dbtp: None,
            compliance_loudness_pass: None,
            compliance_true_peak_pass: None,
            compliance_max_short_term_lufs: None,
            compliance_short_term_pass: None,
            compliance_max_momentary_lufs: None,
            compliance_momentary_pass: None,
            compliance_min_loudness_range_lu: None,
            compliance_max_loudness_range_lu: None,
            compliance_loudness_range_pass: None,
            compliance_rules_json: None,
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
        assert_eq!(value[0]["loudness_range_stable"], false);
        assert_eq!(value[0]["loudness_range_stable_after_seconds"], 60.0);
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
    fn ndjson_writes_one_object_per_line() {
        let mut output = Vec::new();
        write_ndjson(&mut output, &[sample_report(), sample_report()]).unwrap();
        let lines: Vec<_> = output.split(|byte| *byte == b'\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(lines[0]).unwrap()["channels"],
            2
        );
        assert!(lines[2].is_empty());
    }

    #[test]
    fn timeline_marks_profile_violations_at_their_time_ranges() {
        let points = vec![LoudnessTimelinePoint {
            start_seconds: 1.0,
            end_seconds: 1.1,
            momentary_lufs: Some(-17.0),
            short_term_lufs: Some(-20.0),
            sample_peak_dbfs: -0.5,
            true_peak_dbtp: -0.4,
        }];
        let profile = ComplianceProfile {
            name: "timeline".into(),
            loudness_basis: LoudnessBasis::Programme,
            target_lufs: None,
            loudness_tolerance_lu: None,
            lower_tolerance_lu: None,
            upper_tolerance_lu: None,
            max_true_peak_dbtp: Some(-1.0),
            max_short_term_lufs: Some(-18.0),
            max_momentary_lufs: Some(-18.0),
            min_loudness_range_lu: None,
            max_loudness_range_lu: None,
        };
        let reports =
            TimelineReport::from_points(Path::new("programme.wav"), &points, Some(&profile));
        assert_eq!(reports[0].start_seconds, 1.0);
        assert_eq!(
            reports[0].violations,
            vec!["max_momentary_lufs", "max_true_peak_dbtp"]
        );
        let mut csv = Vec::new();
        write_timeline_csv(&mut csv, &reports).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.starts_with("path,start_seconds,end_seconds,"));
        assert!(csv.contains("max_momentary_lufs"));
    }

    #[test]
    fn ebu_compliance_checks_loudness_and_true_peak() {
        let mut analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
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
        let profile = ComplianceProfile::builtin("ebu-r128").unwrap();
        assert!(profile.evaluate(&analysis).unwrap().passed);
        analysis.true_peak = 1.0;
        let result = profile.evaluate(&analysis).unwrap();
        assert!(
            result
                .rules
                .iter()
                .find(|rule| rule.metric == "integrated_lufs")
                .unwrap()
                .passed
        );
        assert!(
            !result
                .rules
                .iter()
                .find(|rule| rule.metric == "true_peak_dbtp")
                .unwrap()
                .passed
        );
        assert!(!result.passed);
    }

    #[test]
    fn atsc_long_form_checks_dialogue_instead_of_programme_loudness() {
        let analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000,
            kind: crate::wav::PcmKind::S24,
            lufs: -12.0,
            max_momentary_lufs: -10.0,
            max_short_term_lufs: -11.0,
            loudness_range_lu: 4.0,
            rms_db: -14.0,
            sample_peak: 0.5,
            true_peak: 0.5,
            loudness_blocks: Vec::new(),
        };
        let profile = ComplianceProfile::builtin("atsc-a85-long").unwrap();
        assert!(profile.requires_dialogue());
        assert!(profile.evaluate_with_dialogue(&analysis, None).is_err());
        let result = profile
            .evaluate_with_dialogue(&analysis, Some(-24.0))
            .unwrap();
        assert!(result.passed);
        assert_eq!(result.rules[0].metric, "dialogue_lufs");
    }

    #[test]
    fn short_form_profile_checks_short_term_loudness() {
        let mut report = sample_report();
        let analysis = crate::normalize::Analysis {
            sample_rate: report.sample_rate_hz,
            channels: report.channels,
            channel_roles: crate::wav::default_channel_roles(report.channels),
            frames: 48_000,
            kind: crate::wav::PcmKind::S24,
            lufs: -23.0,
            max_momentary_lufs: report.max_momentary_lufs,
            max_short_term_lufs: -17.9,
            loudness_range_lu: report.loudness_range_lu,
            rms_db: report.rms_dbfs,
            sample_peak: 0.5,
            true_peak: 0.5,
            loudness_blocks: Vec::new(),
        };
        let profile = ComplianceProfile::builtin("ebu-r128-short").unwrap();
        let result = profile.evaluate(&analysis).unwrap();
        assert!(!result.passed);
        report.compliance_passed = Some(result.passed);
        assert_eq!(report.compliance_passed, Some(false));
    }

    #[test]
    fn custom_profile_validation_rejects_conflicting_tolerances() {
        let profile = ComplianceProfile {
            name: "custom".into(),
            loudness_basis: LoudnessBasis::Programme,
            target_lufs: Some(-20.0),
            loudness_tolerance_lu: Some(1.0),
            lower_tolerance_lu: None,
            upper_tolerance_lu: Some(2.0),
            max_true_peak_dbtp: None,
            max_short_term_lufs: None,
            max_momentary_lufs: None,
            min_loudness_range_lu: None,
            max_loudness_range_lu: None,
        };
        assert!(profile.validate().is_err());
    }

    #[test]
    fn custom_json_and_toml_profiles_load() {
        let json_path = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        std::fs::write(
            json_path.path(),
            r#"{
                "name": "station-json",
                "target_lufs": -20.0,
                "loudness_tolerance_lu": 1.0,
                "lower_tolerance_lu": null,
                "upper_tolerance_lu": null,
                "max_true_peak_dbtp": -1.0,
                "max_short_term_lufs": null,
                "max_momentary_lufs": null,
                "min_loudness_range_lu": null,
                "max_loudness_range_lu": null
            }"#,
        )
        .unwrap();
        assert_eq!(
            ComplianceProfile::load(json_path.path().to_str().unwrap())
                .unwrap()
                .name,
            "station-json"
        );

        let toml_path = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
        std::fs::write(
            toml_path.path(),
            r#"
                name = "station-toml"
                target_lufs = -18.0
                lower_tolerance_lu = 1.0
                upper_tolerance_lu = 2.0
                max_true_peak_dbtp = -1.0
            "#,
        )
        .unwrap();
        assert_eq!(
            ComplianceProfile::load(toml_path.path().to_str().unwrap())
                .unwrap()
                .upper_tolerance_lu,
            Some(2.0)
        );
    }
}
