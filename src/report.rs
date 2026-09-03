//! Stable machine-readable analysis reports.

use crate::analysis::AnalysisEngine;
use crate::anomaly_provider::ProviderAuditDocument;
use crate::bound_analysis::MEASUREMENT_ALGORITHM_REVISION;
use crate::dsp::lufs::LoudnessTimelinePoint;
use crate::normalize::{Analysis, DialogueMeasurement, DialogueSource};
use crate::qc::{QcResult, QC_SCHEMA};
use crate::{container_qc, container_qc::ContainerAudit};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodecMetadata {
    pub codec: String,
    pub dialnorm_lkfs: Option<f64>,
    pub encoded_loudness_lufs: Option<f64>,
    pub downmix_mode: Option<String>,
    pub tolerance_lu: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CodecQcResult {
    pub metadata: CodecMetadata,
    pub loudness_basis: &'static str,
    pub dialnorm_deviation_lu: Option<f64>,
    pub dialnorm_pass: Option<bool>,
    pub encoded_loudness_deviation_lu: Option<f64>,
    pub encoded_loudness_pass: Option<bool>,
}

impl CodecMetadata {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("read codec metadata {}: {error}", path.display()))?;
        let metadata: Self = match path.extension().and_then(|value| value.to_str()) {
            Some("json") => serde_json::from_str(&text)
                .map_err(|error| format!("parse codec metadata {}: {error}", path.display()))?,
            Some("toml") => toml::from_str(&text)
                .map_err(|error| format!("parse codec metadata {}: {error}", path.display()))?,
            _ => return Err("codec metadata must use a .json or .toml extension".into()),
        };
        metadata.validate()?;
        Ok(metadata)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.codec.trim().is_empty() {
            return Err("codec metadata requires a non-empty codec".into());
        }
        if self
            .dialnorm_lkfs
            .is_some_and(|value| !value.is_finite() || !(-31.0..=-1.0).contains(&value))
        {
            return Err("dialnorm_lkfs must be between -31 and -1".into());
        }
        if self
            .encoded_loudness_lufs
            .is_some_and(|value| !value.is_finite())
        {
            return Err("encoded_loudness_lufs must be finite".into());
        }
        if self
            .tolerance_lu
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err("tolerance_lu must be a finite non-negative number".into());
        }
        if self.dialnorm_lkfs.is_none() && self.encoded_loudness_lufs.is_none() {
            return Err("codec metadata must define dialnorm or encoded loudness".into());
        }
        Ok(())
    }

    pub fn evaluate(
        &self,
        analysis: &Analysis,
        dialogue: Option<&DialogueMeasurement>,
    ) -> CodecQcResult {
        let tolerance = self.tolerance_lu.unwrap_or(1.0);
        let (basis, measured) = dialogue
            .map(|value| ("dialogue", value.lufs))
            .unwrap_or(("programme", analysis.lufs));
        let dialnorm_deviation_lu = self.dialnorm_lkfs.map(|value| measured - value);
        let encoded_loudness_deviation_lu = self
            .encoded_loudness_lufs
            .map(|value| analysis.lufs - value);
        CodecQcResult {
            metadata: self.clone(),
            loudness_basis: basis,
            dialnorm_deviation_lu,
            dialnorm_pass: dialnorm_deviation_lu.map(|value| value.abs() <= tolerance),
            encoded_loudness_deviation_lu,
            encoded_loudness_pass: encoded_loudness_deviation_lu
                .map(|value| value.abs() <= tolerance),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComplianceProfile {
    pub name: String,
    #[serde(default)]
    pub standard: Option<String>,
    #[serde(default)]
    pub standard_version: Option<String>,
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
    pub min_peak_to_loudness_ratio_lu: Option<f64>,
    pub max_peak_to_loudness_ratio_lu: Option<f64>,
    #[serde(default)]
    pub peak_to_loudness_ratio_max_exclusive: bool,
    #[serde(default)]
    pub max_loudness_to_dialogue_ratio_lu: Option<f64>,
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
            "ebu-r128" => {
                Self::symmetric("ebu-r128", -23.0, 0.2, -1.0).sourced("EBU R 128", "5.0 (2023)")
            }
            "ebu-r128-live" => {
                Self::symmetric(name, -23.0, 1.0, -1.0).sourced("EBU R 128", "5.0 (2023)")
            }
            "ebu-r128-creative" => {
                Self::upper_bounded(name, -23.0, 0.2, -1.0).sourced("EBU R 128", "5.0 (2023)")
            }
            "radio-ebu" | "ebu-r128-s3-radio" => {
                Self::symmetric(name, -23.0, 0.2, -1.0).sourced("EBU R 128 s3", "2023")
            }
            "ebu-r128-s2-streaming" => {
                Self::symmetric(name, -23.0, 0.2, -1.0).sourced("EBU R 128 s2", "3.0 (2023)")
            }
            "ebu-r128-s2-streaming-adapted" => {
                Self::symmetric(name, -18.0, 2.0, -1.0).sourced("EBU R 128 s2", "3.0 (2023)")
            }
            "ebu-r128-s2-music-low-plr" => Self {
                max_peak_to_loudness_ratio_lu: Some(15.0),
                peak_to_loudness_ratio_max_exclusive: true,
                ..Self::symmetric(name, -16.0, 0.2, -1.0).sourced("EBU R 128 s2", "3.0 (2023)")
            },
            "streaming-music" => Self::symmetric("streaming-music", -14.0, 1.0, -1.0),
            "streaming-speech-stereo" => {
                Self::symmetric("streaming-speech-stereo", -16.0, 1.0, -1.0)
            }
            "streaming-speech-mono" => Self::symmetric("streaming-speech-mono", -19.0, 1.0, -1.0),
            "ebu-r128-short" => Self {
                max_short_term_lufs: Some(-18.0),
                ..Self::symmetric("ebu-r128-short", -23.0, 0.2, -1.0)
                    .sourced("EBU R 128 s1", "2023")
            },
            "ebu-r128-cinematic" => Self {
                max_loudness_to_dialogue_ratio_lu: Some(5.0),
                ..Self::symmetric("ebu-r128-cinematic", -23.0, 0.2, -1.0)
                    .sourced("EBU R 128 s4", "2023")
            },
            "itu-h872-game" => {
                Self::true_peak_only(name, -1.0).sourced("ITU-T H.872 clause 9.3.1", "10/2024")
            }
            "itu-h872-handheld" => Self::true_peak_only(name, -1.0)
                .sourced("ITU-T H.872 clause 9.3.1 Note 4", "10/2024"),
            "atsc-a85-short" => {
                Self::symmetric("atsc-a85-short", -24.0, 2.0, -2.0).sourced("ATSC A/85", "2026-07")
            }
            "atsc-a85-long" => Self {
                loudness_basis: LoudnessBasis::Dialogue,
                ..Self::symmetric("atsc-a85-long", -24.0, 2.0, -2.0).sourced("ATSC A/85", "2026-07")
            },
            "arib-tr-b32" => {
                Self::symmetric(name, -24.0, 1.0, -1.0).sourced("ARIB TR-B32", "1.6 (2025)")
            }
            "arib-tr-b32-creative" => {
                Self::upper_bounded(name, -24.0, 1.0, -1.0).sourced("ARIB TR-B32", "1.6 (2025)")
            }
            "aes77-assorted" => Self {
                name: "aes77-assorted".into(),
                standard: Some("AES77".into()),
                standard_version: Some("2023".into()),
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
                min_peak_to_loudness_ratio_lu: None,
                max_peak_to_loudness_ratio_lu: None,
                peak_to_loudness_ratio_max_exclusive: false,
                max_loudness_to_dialogue_ratio_lu: None,
            },
            "aes77-music-track" => {
                Self::upper_bounded("aes77-music-track", -16.0, 0.2, -1.0).sourced("AES77", "2023")
            }
            "aes77-interstitial" => {
                Self::upper_bounded("aes77-interstitial", -18.0, 0.2, -1.0).sourced("AES77", "2023")
            }
            _ => return None,
        };
        Some(profile)
    }

    fn symmetric(name: &str, target: f64, tolerance: f64, true_peak: f64) -> Self {
        Self {
            name: name.into(),
            standard: None,
            standard_version: None,
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
            min_peak_to_loudness_ratio_lu: None,
            max_peak_to_loudness_ratio_lu: None,
            peak_to_loudness_ratio_max_exclusive: false,
            max_loudness_to_dialogue_ratio_lu: None,
        }
    }

    fn upper_bounded(name: &str, target: f64, tolerance: f64, true_peak: f64) -> Self {
        Self {
            name: name.into(),
            standard: None,
            standard_version: None,
            loudness_basis: LoudnessBasis::Programme,
            target_lufs: Some(target),
            loudness_tolerance_lu: None,
            lower_tolerance_lu: None,
            upper_tolerance_lu: Some(tolerance),
            max_true_peak_dbtp: Some(true_peak),
            max_short_term_lufs: None,
            max_momentary_lufs: None,
            min_loudness_range_lu: None,
            max_loudness_range_lu: None,
            min_peak_to_loudness_ratio_lu: None,
            max_peak_to_loudness_ratio_lu: None,
            peak_to_loudness_ratio_max_exclusive: false,
            max_loudness_to_dialogue_ratio_lu: None,
        }
    }

    fn true_peak_only(name: &str, true_peak: f64) -> Self {
        Self {
            name: name.into(),
            standard: None,
            standard_version: None,
            loudness_basis: LoudnessBasis::Programme,
            target_lufs: None,
            loudness_tolerance_lu: None,
            lower_tolerance_lu: None,
            upper_tolerance_lu: None,
            max_true_peak_dbtp: Some(true_peak),
            max_short_term_lufs: None,
            max_momentary_lufs: None,
            min_loudness_range_lu: None,
            max_loudness_range_lu: None,
            min_peak_to_loudness_ratio_lu: None,
            max_peak_to_loudness_ratio_lu: None,
            peak_to_loudness_ratio_max_exclusive: false,
            max_loudness_to_dialogue_ratio_lu: None,
        }
    }

    fn sourced(mut self, standard: &str, version: &str) -> Self {
        self.standard = Some(standard.into());
        self.standard_version = Some(version.into());
        self
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
            self.min_peak_to_loudness_ratio_lu,
            self.max_peak_to_loudness_ratio_lu,
            self.max_loudness_to_dialogue_ratio_lu,
        ];
        if values.into_iter().flatten().any(|value| !value.is_finite()) {
            return Err("compliance profile values must be finite".into());
        }
        if [
            self.loudness_tolerance_lu,
            self.lower_tolerance_lu,
            self.upper_tolerance_lu,
            self.min_loudness_range_lu,
            self.max_loudness_range_lu,
            self.min_peak_to_loudness_ratio_lu,
            self.max_peak_to_loudness_ratio_lu,
            self.max_loudness_to_dialogue_ratio_lu,
        ]
        .into_iter()
        .flatten()
        .any(|value| value < 0.0)
        {
            return Err(
                "loudness tolerances, LRA, PLR, and LDR limits must be non-negative".into(),
            );
        }
        if self.target_lufs.is_none()
            && self.max_true_peak_dbtp.is_none()
            && self.max_short_term_lufs.is_none()
            && self.max_momentary_lufs.is_none()
            && self.min_loudness_range_lu.is_none()
            && self.max_loudness_range_lu.is_none()
            && self.min_peak_to_loudness_ratio_lu.is_none()
            && self.max_peak_to_loudness_ratio_lu.is_none()
            && self.max_loudness_to_dialogue_ratio_lu.is_none()
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
        if self
            .min_peak_to_loudness_ratio_lu
            .zip(self.max_peak_to_loudness_ratio_lu)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err("minimum peak-to-loudness ratio exceeds maximum".into());
        }
        if self.peak_to_loudness_ratio_max_exclusive && self.max_peak_to_loudness_ratio_lu.is_none()
        {
            return Err(
                "peak_to_loudness_ratio_max_exclusive requires max_peak_to_loudness_ratio_lu"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn evaluate(&self, analysis: &Analysis) -> Result<ComplianceResult, String> {
        self.evaluate_with_dialogue(analysis, None)
    }

    pub fn requires_dialogue(&self) -> bool {
        (self.loudness_basis == LoudnessBasis::Dialogue && self.target_lufs.is_some())
            || self.max_loudness_to_dialogue_ratio_lu.is_some()
    }

    pub fn evaluate_with_dialogue(
        &self,
        analysis: &Analysis,
        dialogue_lufs: Option<f64>,
    ) -> Result<ComplianceResult, String> {
        let mut rules = Vec::new();
        if let Some(target) = h872_target(self) {
            add_h872_rules(&mut rules, analysis, target)?;
        }
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
        if self.min_peak_to_loudness_ratio_lu.is_some()
            || self.max_peak_to_loudness_ratio_lu.is_some()
        {
            rules.push(ComplianceRuleResult::bounded(
                "peak_to_loudness_ratio_lu",
                analysis.peak_to_loudness_ratio_lu(),
                self.min_peak_to_loudness_ratio_lu
                    .unwrap_or(f64::NEG_INFINITY),
                self.max_peak_to_loudness_ratio_lu.unwrap_or(f64::INFINITY),
                true,
                !self.peak_to_loudness_ratio_max_exclusive,
            ));
        }
        if let Some(maximum) = self.max_loudness_to_dialogue_ratio_lu {
            let dialogue = dialogue_lufs.ok_or_else(|| {
                format!(
                    "compliance profile {} requires --dialogue-ranges",
                    self.name
                )
            })?;
            rules.push(ComplianceRuleResult::maximum(
                "loudness_to_dialogue_ratio_lu",
                analysis.lufs - dialogue,
                maximum,
            ));
        }
        Ok(ComplianceResult {
            profile: self.name.clone(),
            standard: self.standard.clone(),
            standard_version: self.standard_version.clone(),
            passed: rules.iter().all(|rule| rule.passed),
            rules,
        })
    }
}

fn h872_target(profile: &ComplianceProfile) -> Option<f64> {
    match (
        profile.name.as_str(),
        profile.standard.as_deref(),
        profile.standard_version.as_deref(),
    ) {
        ("itu-h872-game", Some("ITU-T H.872 clause 9.3.1"), Some("10/2024")) => Some(-23.0),
        ("itu-h872-handheld", Some("ITU-T H.872 clause 9.3.1 Note 4"), Some("10/2024")) => {
            Some(-18.0)
        }
        _ => None,
    }
}

fn add_h872_rules(
    rules: &mut Vec<ComplianceRuleResult>,
    analysis: &Analysis,
    target_lufs: f64,
) -> Result<(), String> {
    const WINDOW_SECONDS: usize = 30 * 60;
    rules.push(ComplianceRuleResult::range(
        "capture_duration_seconds",
        analysis.duration_secs(),
        WINDOW_SECONDS as f64,
        f64::INFINITY,
    ));
    if analysis.duration_secs() < WINDOW_SECONDS as f64 {
        return Ok(());
    }

    let sample_rate = analysis.sample_rate as usize;
    let momentary_frames = (0.4 * sample_rate as f64).round() as usize;
    let hop_frames = (0.1 * sample_rate as f64).round() as usize;
    let window_frames = WINDOW_SECONDS
        .checked_mul(sample_rate)
        .ok_or_else(|| "ITU-T H.872 window size overflow".to_string())?;
    if momentary_frames == 0 || hop_frames == 0 || window_frames < momentary_frames {
        return Err("ITU-T H.872 requires a valid BS.1770 gating block geometry".into());
    }
    let blocks_per_window = (window_frames - momentary_frames) / hop_frames + 1;
    let expected_blocks = if analysis.frames < momentary_frames {
        0
    } else {
        (analysis.frames - momentary_frames) / hop_frames + 1
    };
    if analysis.loudness_blocks.len() != expected_blocks {
        return Err(format!(
            "ITU-T H.872 requires all {expected_blocks} retained BS.1770 blocks; analysis retained {}",
            analysis.loudness_blocks.len()
        ));
    }
    let (minimum, maximum) = crate::dsp::lufs::rolling_gated_loudness_extrema(
        &analysis.loudness_blocks,
        blocks_per_window,
    )
    .ok_or_else(|| "ITU-T H.872 requires at least one complete 30-minute window".to_string())?;
    let lower = target_lufs - 2.0;
    let upper = target_lufs + 2.0;
    rules.push(ComplianceRuleResult::range(
        "minimum_rolling_30m_integrated_lufs",
        minimum,
        lower,
        upper,
    ));
    rules.push(ComplianceRuleResult::range(
        "maximum_rolling_30m_integrated_lufs",
        maximum,
        lower,
        upper,
    ));
    Ok(())
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
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub measured: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub minimum_inclusive: Option<bool>,
    pub maximum_inclusive: Option<bool>,
    pub passed: bool,
}

impl ComplianceRuleResult {
    fn range(metric: &'static str, measured: f64, minimum: f64, maximum: f64) -> Self {
        Self::bounded(metric, measured, minimum, maximum, true, true)
    }

    fn bounded(
        metric: &'static str,
        measured: f64,
        minimum: f64,
        maximum: f64,
        minimum_inclusive: bool,
        maximum_inclusive: bool,
    ) -> Self {
        let minimum_passed = if minimum_inclusive {
            measured >= minimum
        } else {
            measured > minimum
        };
        let maximum_passed = if maximum_inclusive {
            measured <= maximum
        } else {
            measured < maximum
        };
        Self {
            metric,
            measured,
            minimum: minimum.is_finite().then_some(minimum),
            maximum: maximum.is_finite().then_some(maximum),
            minimum_inclusive: minimum.is_finite().then_some(minimum_inclusive),
            maximum_inclusive: maximum.is_finite().then_some(maximum_inclusive),
            passed: minimum_passed && maximum_passed,
        }
    }

    fn maximum(metric: &'static str, measured: f64, maximum: f64) -> Self {
        Self::range(metric, measured, f64::NEG_INFINITY, maximum)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComplianceResult {
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_version: Option<String>,
    pub rules: Vec<ComplianceRuleResult>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisReport {
    pub path: String,
    pub duration_seconds: f64,
    pub source_start_seconds: f64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub sample_format: &'static str,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub integrated_lufs: f64,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub dialogue_lufs: Option<f64>,
    pub dialogue_duration_seconds: Option<f64>,
    pub dialogue_range_count: Option<usize>,
    pub dialogue_measurement_standard: Option<&'static str>,
    pub dialogue_measurement_method: Option<&'static str>,
    pub dialogue_source: Option<DialogueSource>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub loudness_to_dialogue_ratio_lu: Option<f64>,
    pub dialogue_detector: Option<&'static str>,
    pub dialogue_detector_version: Option<&'static str>,
    pub dialogue_detection_threshold: Option<f64>,
    pub dialogue_detection_ranges_json: Option<String>,
    pub dialogue_detection_frames_json: Option<String>,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub max_momentary_lufs: f64,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub loudness_range_stable: bool,
    pub loudness_range_stable_after_seconds: f64,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub rms_dbfs: f64,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub sample_peak_dbfs: f64,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub true_peak_dbtp: f64,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub peak_to_loudness_ratio_lu: f64,
    /// Complete EBU QC result set encoded as JSON so CSV remains flat.
    pub ebu_qc_results_json: Option<String>,
    pub ebu_qc_passed: Option<bool>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub downmix_integrated_lufs: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub downmix_true_peak_dbtp: Option<f64>,
    pub downmix_method: Option<&'static str>,
    pub codec: Option<String>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub codec_dialnorm_lkfs: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub codec_encoded_loudness_lufs: Option<f64>,
    pub codec_downmix_mode: Option<String>,
    pub codec_loudness_basis: Option<&'static str>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub codec_dialnorm_deviation_lu: Option<f64>,
    pub codec_dialnorm_pass: Option<bool>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub codec_encoded_loudness_deviation_lu: Option<f64>,
    pub codec_encoded_loudness_pass: Option<bool>,
    pub codec_qc_tolerance_lu: Option<f64>,
    pub codec_probe_tool: Option<String>,
    pub codec_probe_schema: Option<&'static str>,
    pub codec_profile: Option<String>,
    pub codec_container: Option<String>,
    pub codec_sample_rate_hz: Option<u32>,
    pub codec_channels: Option<u16>,
    pub codec_channel_layout: Option<String>,
    pub codec_bitrate_bps: Option<u64>,
    pub codec_drc_profile: Option<String>,
    pub codec_reference_path: Option<String>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub codec_loudness_drift_lu: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub codec_true_peak_drift_db: Option<f64>,
    pub codec_duration_drift_seconds: Option<f64>,
    pub codec_roundtrip_pass: Option<bool>,
    pub adm_axml_present: Option<bool>,
    pub adm_chna_present: Option<bool>,
    pub adm_presentations_json: Option<String>,
    pub adm_qc_passed: Option<bool>,
    pub adm_model_standard: Option<&'static str>,
    pub adm_model_version: Option<&'static str>,
    pub adm_production_profile_standard: Option<&'static str>,
    pub adm_production_profile_version: Option<&'static str>,
    pub adm_production_profile_level: Option<&'static str>,
    pub adm_production_profile_mode: Option<crate::adm::ProductionProfileMode>,
    pub adm_production_profile_validator: Option<&'static str>,
    pub adm_production_profile_rules_json: Option<String>,
    pub adm_production_profile_passed: Option<bool>,
    pub adm_render_renderer: Option<String>,
    pub adm_render_standard: Option<&'static str>,
    pub adm_render_profile: Option<&'static str>,
    pub adm_render_profile_level: Option<u8>,
    pub adm_render_layout: Option<String>,
    pub adm_render_validation_passed: Option<bool>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub adm_render_integrated_lufs: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub adm_render_true_peak_dbtp: Option<f64>,
    pub adm_render_channels: Option<u16>,
    pub adm_render_output_path: Option<String>,
    pub compliance_profile: Option<String>,
    pub compliance_standard: Option<String>,
    pub compliance_standard_version: Option<String>,
    pub compliance_loudness_basis: Option<LoudnessBasis>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub compliance_target_lufs: Option<f64>,
    pub compliance_loudness_tolerance_lu: Option<f64>,
    pub compliance_lower_tolerance_lu: Option<f64>,
    pub compliance_upper_tolerance_lu: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub compliance_max_true_peak_dbtp: Option<f64>,
    pub compliance_loudness_pass: Option<bool>,
    pub compliance_true_peak_pass: Option<bool>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub compliance_max_short_term_lufs: Option<f64>,
    pub compliance_short_term_pass: Option<bool>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub compliance_max_momentary_lufs: Option<f64>,
    pub compliance_momentary_pass: Option<bool>,
    pub compliance_min_loudness_range_lu: Option<f64>,
    pub compliance_max_loudness_range_lu: Option<f64>,
    pub compliance_loudness_range_pass: Option<bool>,
    pub compliance_min_peak_to_loudness_ratio_lu: Option<f64>,
    pub compliance_max_peak_to_loudness_ratio_lu: Option<f64>,
    pub compliance_peak_to_loudness_ratio_max_exclusive: Option<bool>,
    pub compliance_peak_to_loudness_ratio_pass: Option<bool>,
    pub compliance_max_loudness_to_dialogue_ratio_lu: Option<f64>,
    pub compliance_loudness_to_dialogue_ratio_pass: Option<bool>,
    /// Complete evaluated rule set, encoded as JSON so CSV remains flat.
    pub compliance_rules_json: Option<String>,
    pub compliance_passed: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineReport {
    pub path: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub momentary_lufs: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_optional_db")]
    pub short_term_lufs: Option<f64>,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
    pub sample_peak_dbfs: f64,
    #[serde(serialize_with = "crate::db_value::serialize_db")]
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
            dialogue_source: dialogue.map(|value| value.source),
            loudness_to_dialogue_ratio_lu: dialogue.map(|value| analysis.lufs - value.lufs),
            dialogue_detector: None,
            dialogue_detector_version: None,
            dialogue_detection_threshold: None,
            dialogue_detection_ranges_json: None,
            dialogue_detection_frames_json: None,
            max_momentary_lufs: analysis.max_momentary_lufs,
            max_short_term_lufs: analysis.max_short_term_lufs,
            loudness_range_lu: analysis.loudness_range_lu,
            loudness_range_stable: analysis.loudness_range_stable(),
            loudness_range_stable_after_seconds: Analysis::LRA_STABLE_AFTER_SECONDS,
            rms_dbfs: analysis.rms_db,
            sample_peak_dbfs: analysis.sample_peak_db(),
            true_peak_dbtp: analysis.true_peak_db(),
            peak_to_loudness_ratio_lu: analysis.peak_to_loudness_ratio_lu(),
            ebu_qc_results_json: None,
            ebu_qc_passed: None,
            downmix_integrated_lufs: None,
            downmix_true_peak_dbtp: None,
            downmix_method: None,
            codec: None,
            codec_dialnorm_lkfs: None,
            codec_encoded_loudness_lufs: None,
            codec_downmix_mode: None,
            codec_loudness_basis: None,
            codec_dialnorm_deviation_lu: None,
            codec_dialnorm_pass: None,
            codec_encoded_loudness_deviation_lu: None,
            codec_encoded_loudness_pass: None,
            codec_qc_tolerance_lu: None,
            codec_probe_tool: None,
            codec_probe_schema: None,
            codec_profile: None,
            codec_container: None,
            codec_sample_rate_hz: None,
            codec_channels: None,
            codec_channel_layout: None,
            codec_bitrate_bps: None,
            codec_drc_profile: None,
            codec_reference_path: None,
            codec_loudness_drift_lu: None,
            codec_true_peak_drift_db: None,
            codec_duration_drift_seconds: None,
            codec_roundtrip_pass: None,
            adm_axml_present: None,
            adm_chna_present: None,
            adm_presentations_json: None,
            adm_qc_passed: None,
            adm_model_standard: None,
            adm_model_version: None,
            adm_production_profile_standard: None,
            adm_production_profile_version: None,
            adm_production_profile_level: None,
            adm_production_profile_mode: None,
            adm_production_profile_validator: None,
            adm_production_profile_rules_json: None,
            adm_production_profile_passed: None,
            adm_render_renderer: None,
            adm_render_standard: None,
            adm_render_profile: None,
            adm_render_profile_level: None,
            adm_render_layout: None,
            adm_render_validation_passed: None,
            adm_render_integrated_lufs: None,
            adm_render_true_peak_dbtp: None,
            adm_render_channels: None,
            adm_render_output_path: None,
            compliance_profile: compliance.as_ref().map(|result| result.profile.clone()),
            compliance_standard: profile.and_then(|value| value.standard.clone()),
            compliance_standard_version: profile.and_then(|value| value.standard_version.clone()),
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
            compliance_min_peak_to_loudness_ratio_lu: profile
                .and_then(|value| value.min_peak_to_loudness_ratio_lu),
            compliance_max_peak_to_loudness_ratio_lu: profile
                .and_then(|value| value.max_peak_to_loudness_ratio_lu),
            compliance_peak_to_loudness_ratio_max_exclusive: profile.and_then(|value| {
                value
                    .max_peak_to_loudness_ratio_lu
                    .map(|_| value.peak_to_loudness_ratio_max_exclusive)
            }),
            compliance_peak_to_loudness_ratio_pass: rule_pass(
                &compliance,
                "peak_to_loudness_ratio_lu",
            ),
            compliance_max_loudness_to_dialogue_ratio_lu: profile
                .and_then(|value| value.max_loudness_to_dialogue_ratio_lu),
            compliance_loudness_to_dialogue_ratio_pass: rule_pass(
                &compliance,
                "loudness_to_dialogue_ratio_lu",
            ),
            compliance_rules_json: compliance.as_ref().map(|result| {
                serde_json::to_string(&result.rules).expect("compliance rules are serializable")
            }),
            compliance_passed: compliance.as_ref().map(|result| result.passed),
        })
    }

    fn canonicalize_for_engine(&mut self, engine: AnalysisEngine) {
        if engine == AnalysisEngine::Reference {
            let canonical = |value: f64| {
                if value.is_finite() {
                    let quantum = crate::dsp::lufs::REFERENCE_DB_QUANTUM;
                    let rounded = (value / quantum).round() * quantum;
                    if rounded == 0.0 {
                        0.0
                    } else {
                        rounded
                    }
                } else {
                    value
                }
            };
            self.integrated_lufs = canonical(self.integrated_lufs);
            self.max_momentary_lufs = canonical(self.max_momentary_lufs);
            self.max_short_term_lufs = canonical(self.max_short_term_lufs);
            self.loudness_range_lu = canonical(self.loudness_range_lu);
            self.rms_dbfs = canonical(self.rms_dbfs);
            self.sample_peak_dbfs = canonical(self.sample_peak_dbfs);
            self.true_peak_dbtp = canonical(self.true_peak_dbtp);
            self.peak_to_loudness_ratio_lu = canonical(self.true_peak_dbtp - self.integrated_lufs);
        }
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

#[derive(Debug, Serialize)]
pub(crate) struct AnalysisReportWire<'a> {
    analysis_engine_id: &'static str,
    measurement_algorithm_revision: &'static str,
    #[serde(flatten)]
    report: Cow<'a, AnalysisReport>,
}

impl<'a> AnalysisReportWire<'a> {
    pub(crate) fn new(report: &'a AnalysisReport, engine: AnalysisEngine) -> Self {
        let report = if engine == AnalysisEngine::Reference {
            let mut report = report.clone();
            report.canonicalize_for_engine(engine);
            Cow::Owned(report)
        } else {
            Cow::Borrowed(report)
        };
        Self {
            analysis_engine_id: engine.id(),
            measurement_algorithm_revision: MEASUREMENT_ALGORITHM_REVISION,
            report,
        }
    }
}

pub fn write_json<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    write_json_with_engine(writer, reports, AnalysisEngine::Fast)
}

/// Write analysis JSON with explicit measurement-engine provenance.
pub fn write_json_with_engine<W: Write>(
    writer: W,
    reports: &[AnalysisReport],
    engine: AnalysisEngine,
) -> Result<(), String> {
    let reports = reports
        .iter()
        .map(|report| AnalysisReportWire::new(report, engine))
        .collect::<Vec<_>>();
    serde_json::to_writer_pretty(writer, &reports).map_err(|error| format!("write JSON: {error}"))
}

#[derive(Serialize)]
struct DeliveryManifest<'a> {
    schema: &'static str,
    generator: &'static str,
    asset_count: usize,
    passed_count: usize,
    failed_count: usize,
    assets: Vec<DeliveryAsset<'a>>,
}

#[derive(Serialize)]
struct DeliveryAsset<'a> {
    analysis_engine_id: &'static str,
    measurement_algorithm_revision: &'static str,
    #[serde(flatten)]
    report: Cow<'a, AnalysisReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qc: Option<QcEnvelope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container_qc: Option<ContainerAudit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_qc: Option<ModelQcEnvelope>,
}

#[derive(Serialize)]
struct QcEnvelope {
    schema: &'static str,
    results: Vec<QcResult>,
}

#[derive(Serialize)]
struct ModelQcEnvelope {
    schema: &'static str,
    layer: &'static str,
    classification: &'static str,
    passed: bool,
    audit: ProviderAuditDocument,
}

pub const MODEL_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/model-qc-v1";

pub fn write_manifest<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    write_manifest_with_anomaly_audits(writer, reports, &[])
}

/// Write a delivery manifest and optionally attach one validated external
/// anomaly audit to each asset. `model_qc` is an advisory layer: its `passed`
/// value is reported for review but never changes the manifest's normative
/// `passed_count`/`failed_count` totals.
pub fn write_manifest_with_anomaly_audits<W: Write>(
    writer: W,
    reports: &[AnalysisReport],
    anomaly_audits: &[Option<ProviderAuditDocument>],
) -> Result<(), String> {
    write_manifest_with_engine_and_anomaly_audits(
        writer,
        reports,
        anomaly_audits,
        AnalysisEngine::Fast,
    )
}

/// Write a delivery manifest with explicit primary measurement-engine
/// provenance and optional validated external anomaly evidence.
pub fn write_manifest_with_engine_and_anomaly_audits<W: Write>(
    writer: W,
    reports: &[AnalysisReport],
    anomaly_audits: &[Option<ProviderAuditDocument>],
    engine: AnalysisEngine,
) -> Result<(), String> {
    if !anomaly_audits.is_empty() && anomaly_audits.len() != reports.len() {
        return Err(format!(
            "anomaly audit count ({}) must match asset count ({})",
            anomaly_audits.len(),
            reports.len()
        ));
    }
    let assets = reports
        .iter()
        .enumerate()
        .map(|(index, report)| {
            let qc = report
                .ebu_qc_results_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| format!("decode EBU QC results: {error}"))?
                .map(|results| QcEnvelope {
                    schema: QC_SCHEMA,
                    results,
                });
            let path = Path::new(&report.path);
            let container_qc = if path.is_file() {
                container_qc::audit_if_supported(path)?
            } else {
                None
            };
            let report = AnalysisReportWire::new(report, engine);
            Ok(DeliveryAsset {
                analysis_engine_id: report.analysis_engine_id,
                measurement_algorithm_revision: report.measurement_algorithm_revision,
                report: report.report,
                qc,
                container_qc,
                model_qc: anomaly_audits
                    .get(index)
                    .and_then(Option::as_ref)
                    .map(|audit| ModelQcEnvelope {
                        schema: MODEL_QC_SCHEMA,
                        layer: "model-qc",
                        classification: "non-normative-model-evidence",
                        passed: audit.passed,
                        audit: audit.clone(),
                    }),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let passed_count = assets
        .iter()
        .filter(|asset| {
            let report = asset.report.as_ref();
            report.compliance_passed != Some(false)
                && report.codec_dialnorm_pass != Some(false)
                && report.codec_encoded_loudness_pass != Some(false)
                && report.codec_roundtrip_pass != Some(false)
                && report.adm_qc_passed != Some(false)
                && report.adm_production_profile_passed != Some(false)
                && report.ebu_qc_passed != Some(false)
                && asset.container_qc.as_ref().is_none_or(|audit| audit.passed)
        })
        .count();
    let manifest = DeliveryManifest {
        schema: "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v4",
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        asset_count: reports.len(),
        passed_count,
        failed_count: reports.len() - passed_count,
        assets,
    };
    serde_json::to_writer_pretty(writer, &manifest)
        .map_err(|error| format!("write delivery manifest: {error}"))
}

pub fn write_ndjson<W: Write>(mut writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    write_ndjson_with_engine(&mut writer, reports, AnalysisEngine::Fast)
}

/// Write analysis NDJSON with explicit measurement-engine provenance.
pub fn write_ndjson_with_engine<W: Write>(
    mut writer: W,
    reports: &[AnalysisReport],
    engine: AnalysisEngine,
) -> Result<(), String> {
    for report in reports {
        serde_json::to_writer(&mut writer, &AnalysisReportWire::new(report, engine))
            .map_err(|error| format!("write NDJSON: {error}"))?;
        writer
            .write_all(b"\n")
            .map_err(|error| format!("write NDJSON newline: {error}"))?;
    }
    Ok(())
}

pub fn write_csv<W: Write>(writer: W, reports: &[AnalysisReport]) -> Result<(), String> {
    write_csv_with_engine(writer, reports, AnalysisEngine::Fast)
}

/// Write analysis CSV with explicit measurement-engine provenance.
pub fn write_csv_with_engine<W: Write>(
    writer: W,
    reports: &[AnalysisReport],
    engine: AnalysisEngine,
) -> Result<(), String> {
    let mut csv = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(writer);
    let mut expected_headers = None;
    for report in reports {
        let report = AnalysisReportWire::new(report, engine);
        let (headers, record) = analysis_report_csv_record(report.report.as_ref())?;
        if let Some(expected) = &expected_headers {
            if expected != &headers {
                return Err("analysis CSV fields changed between records".into());
            }
        } else {
            let mut prefixed = csv::StringRecord::new();
            prefixed.push_field("analysis_engine_id");
            prefixed.push_field("measurement_algorithm_revision");
            prefixed.extend(headers.iter());
            csv.write_record(&prefixed)
                .map_err(|error| format!("write CSV header: {error}"))?;
            expected_headers = Some(headers);
        }
        let mut prefixed = csv::StringRecord::new();
        prefixed.push_field(report.analysis_engine_id);
        prefixed.push_field(report.measurement_algorithm_revision);
        prefixed.extend(record.iter());
        csv.write_record(&prefixed)
            .map_err(|error| format!("write CSV: {error}"))?;
    }
    csv.flush().map_err(|error| format!("flush CSV: {error}"))
}

fn analysis_report_csv_record(
    report: &AnalysisReport,
) -> Result<(csv::StringRecord, csv::StringRecord), String> {
    let mut encoded = csv::Writer::from_writer(Vec::new());
    encoded
        .serialize(report)
        .map_err(|error| format!("encode CSV record: {error}"))?;
    let encoded = encoded
        .into_inner()
        .map_err(|error| format!("finish CSV record: {}", error.error()))?;
    let mut reader = csv::Reader::from_reader(encoded.as_slice());
    let headers = reader
        .headers()
        .map_err(|error| format!("read CSV header: {error}"))?
        .clone();
    let record = reader
        .records()
        .next()
        .ok_or_else(|| "analysis CSV serializer emitted no record".to_string())?
        .map_err(|error| format!("read CSV record: {error}"))?;
    Ok((headers, record))
}

pub fn write_timeline_json<W: Write>(writer: W, reports: &[TimelineReport]) -> Result<(), String> {
    write_timeline_json_with_engine(writer, reports, AnalysisEngine::Fast)
}

#[derive(Serialize)]
struct TimelineReportWire<'a> {
    analysis_engine_id: &'static str,
    measurement_algorithm_revision: &'static str,
    #[serde(flatten)]
    report: &'a TimelineReport,
}

impl<'a> TimelineReportWire<'a> {
    fn new(report: &'a TimelineReport, engine: AnalysisEngine) -> Self {
        Self {
            analysis_engine_id: engine.id(),
            measurement_algorithm_revision: MEASUREMENT_ALGORITHM_REVISION,
            report,
        }
    }
}

/// Write timeline JSON with explicit measurement-engine provenance.
pub fn write_timeline_json_with_engine<W: Write>(
    writer: W,
    reports: &[TimelineReport],
    engine: AnalysisEngine,
) -> Result<(), String> {
    let reports = reports
        .iter()
        .map(|report| TimelineReportWire::new(report, engine))
        .collect::<Vec<_>>();
    serde_json::to_writer_pretty(writer, &reports)
        .map_err(|error| format!("write timeline JSON: {error}"))
}

pub fn write_timeline_ndjson<W: Write>(
    mut writer: W,
    reports: &[TimelineReport],
) -> Result<(), String> {
    write_timeline_ndjson_with_engine(&mut writer, reports, AnalysisEngine::Fast)
}

/// Write timeline NDJSON with explicit measurement-engine provenance.
pub fn write_timeline_ndjson_with_engine<W: Write>(
    mut writer: W,
    reports: &[TimelineReport],
    engine: AnalysisEngine,
) -> Result<(), String> {
    for report in reports {
        serde_json::to_writer(&mut writer, &TimelineReportWire::new(report, engine))
            .map_err(|error| format!("write timeline NDJSON: {error}"))?;
        writer
            .write_all(b"\n")
            .map_err(|error| format!("write timeline NDJSON newline: {error}"))?;
    }
    Ok(())
}

pub fn write_timeline_csv<W: Write>(writer: W, reports: &[TimelineReport]) -> Result<(), String> {
    write_timeline_csv_with_engine(writer, reports, AnalysisEngine::Fast)
}

/// Write timeline CSV with explicit measurement-engine provenance.
pub fn write_timeline_csv_with_engine<W: Write>(
    writer: W,
    reports: &[TimelineReport],
    engine: AnalysisEngine,
) -> Result<(), String> {
    let mut csv = csv::Writer::from_writer(writer);
    csv.write_record([
        "analysis_engine_id",
        "measurement_algorithm_revision",
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
            engine.id(),
            MEASUREMENT_ALGORITHM_REVISION,
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
            dialogue_source: None,
            loudness_to_dialogue_ratio_lu: None,
            dialogue_detector: None,
            dialogue_detector_version: None,
            dialogue_detection_threshold: None,
            dialogue_detection_ranges_json: None,
            dialogue_detection_frames_json: None,
            max_momentary_lufs: -20.0,
            max_short_term_lufs: -21.0,
            loudness_range_lu: 8.0,
            loudness_range_stable: false,
            loudness_range_stable_after_seconds: 60.0,
            rms_dbfs: -25.0,
            sample_peak_dbfs: -3.0,
            true_peak_dbtp: -2.8,
            peak_to_loudness_ratio_lu: 20.2,
            ebu_qc_results_json: None,
            ebu_qc_passed: None,
            downmix_integrated_lufs: None,
            downmix_true_peak_dbtp: None,
            downmix_method: None,
            codec: None,
            codec_dialnorm_lkfs: None,
            codec_encoded_loudness_lufs: None,
            codec_downmix_mode: None,
            codec_loudness_basis: None,
            codec_dialnorm_deviation_lu: None,
            codec_dialnorm_pass: None,
            codec_encoded_loudness_deviation_lu: None,
            codec_encoded_loudness_pass: None,
            codec_qc_tolerance_lu: None,
            codec_probe_tool: None,
            codec_probe_schema: None,
            codec_profile: None,
            codec_container: None,
            codec_sample_rate_hz: None,
            codec_channels: None,
            codec_channel_layout: None,
            codec_bitrate_bps: None,
            codec_drc_profile: None,
            codec_reference_path: None,
            codec_loudness_drift_lu: None,
            codec_true_peak_drift_db: None,
            codec_duration_drift_seconds: None,
            codec_roundtrip_pass: None,
            adm_axml_present: None,
            adm_chna_present: None,
            adm_presentations_json: None,
            adm_qc_passed: None,
            adm_model_standard: None,
            adm_model_version: None,
            adm_production_profile_standard: None,
            adm_production_profile_version: None,
            adm_production_profile_level: None,
            adm_production_profile_mode: None,
            adm_production_profile_validator: None,
            adm_production_profile_rules_json: None,
            adm_production_profile_passed: None,
            adm_render_renderer: None,
            adm_render_standard: None,
            adm_render_profile: None,
            adm_render_profile_level: None,
            adm_render_layout: None,
            adm_render_validation_passed: None,
            adm_render_integrated_lufs: None,
            adm_render_true_peak_dbtp: None,
            adm_render_channels: None,
            adm_render_output_path: None,
            compliance_profile: None,
            compliance_standard: None,
            compliance_standard_version: None,
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
            compliance_min_peak_to_loudness_ratio_lu: None,
            compliance_max_peak_to_loudness_ratio_lu: None,
            compliance_peak_to_loudness_ratio_max_exclusive: None,
            compliance_peak_to_loudness_ratio_pass: None,
            compliance_max_loudness_to_dialogue_ratio_lu: None,
            compliance_loudness_to_dialogue_ratio_pass: None,
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
    fn delivery_manifest_summarizes_assets() {
        let passing = sample_report();
        let mut failing = sample_report();
        failing.path = "failed.wav".into();
        failing.compliance_passed = Some(false);
        let mut output = Vec::new();
        write_manifest(&mut output, &[passing, failing]).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["asset_count"], 2);
        assert_eq!(value["passed_count"], 1);
        assert_eq!(value["failed_count"], 1);
        assert_eq!(value["assets"][1]["path"], "failed.wav");
        assert!(value["schema"]
            .as_str()
            .unwrap()
            .ends_with("delivery-manifest-v4"));
    }

    #[test]
    fn delivery_manifest_counts_container_qc_failures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("damaged.wav");
        let audio = crate::wav::AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 16,
            data: vec![vec![0.0; 16]],
            channel_roles: vec![crate::wav::ChannelRole::Main],
            source_kind: crate::wav::PcmKind::S16,
        };
        crate::wav::WavWriter::write(&path, &audio, crate::wav::PcmKind::S16, false).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let mut report = sample_report();
        report.path = path.to_string_lossy().into_owned();
        let mut output = Vec::new();
        write_manifest(&mut output, &[report]).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["passed_count"], 0);
        assert_eq!(value["failed_count"], 1);
        assert_eq!(value["assets"][0]["container_qc"]["passed"], false);
    }

    #[test]
    fn purpose_based_delivery_profiles_are_available() {
        assert_eq!(
            ComplianceProfile::builtin("streaming-music")
                .unwrap()
                .target_lufs,
            Some(-14.0)
        );
        assert_eq!(
            ComplianceProfile::builtin("streaming-speech-mono")
                .unwrap()
                .target_lufs,
            Some(-19.0)
        );
        assert_eq!(
            ComplianceProfile::builtin("radio-ebu").unwrap().target_lufs,
            Some(-23.0)
        );
    }

    fn profile_analysis(lufs: f64) -> crate::normalize::Analysis {
        crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000,
            kind: crate::wav::PcmKind::S24,
            lufs,
            max_momentary_lufs: lufs,
            max_short_term_lufs: lufs,
            loudness_range_lu: 0.0,
            rms_db: lufs,
            sample_peak: 0.5,
            true_peak: 0.5,
            loudness_blocks: Vec::new(),
        }
    }

    fn profile_accepts_loudness(profile: &ComplianceProfile, lufs: f64) -> bool {
        profile.evaluate(&profile_analysis(lufs)).unwrap().passed
    }

    #[test]
    fn versioned_ebu_distribution_profiles_preserve_their_source() {
        let unchanged = ComplianceProfile::builtin("ebu-r128-s2-streaming").unwrap();
        assert_eq!(unchanged.target_lufs, Some(-23.0));
        assert_eq!(unchanged.standard.as_deref(), Some("EBU R 128 s2"));
        assert_eq!(unchanged.standard_version.as_deref(), Some("3.0 (2023)"));

        let adapted = ComplianceProfile::builtin("ebu-r128-s2-streaming-adapted").unwrap();
        assert_eq!(adapted.target_lufs, Some(-18.0));
        assert_eq!(adapted.loudness_tolerance_lu, Some(2.0));
        let music = ComplianceProfile::builtin("ebu-r128-s2-music-low-plr").unwrap();
        assert_eq!(music.target_lufs, Some(-16.0));
        assert_eq!(music.max_peak_to_loudness_ratio_lu, Some(15.0));
        assert!(music.peak_to_loudness_ratio_max_exclusive);

        let radio = ComplianceProfile::builtin("ebu-r128-s3-radio").unwrap();
        assert_eq!(radio.target_lufs, Some(-23.0));
        assert_eq!(radio.standard_version.as_deref(), Some("2023"));
    }

    #[test]
    fn adapted_ebu_streaming_profile_accepts_only_minus_20_to_minus_16() {
        let profile = ComplianceProfile::builtin("ebu-r128-s2-streaming-adapted").unwrap();
        let lower = -20.0_f64;
        let upper = -16.0_f64;

        assert!(profile_accepts_loudness(&profile, -18.0));
        assert!(profile_accepts_loudness(&profile, lower));
        assert!(profile_accepts_loudness(&profile, lower.next_up()));
        assert!(!profile_accepts_loudness(&profile, lower.next_down()));
        assert!(profile_accepts_loudness(&profile, upper));
        assert!(profile_accepts_loudness(&profile, upper.next_down()));
        assert!(!profile_accepts_loudness(&profile, upper.next_up()));
    }

    #[test]
    fn aes77_music_and_interstitial_have_only_an_upper_tolerance() {
        for name in ["aes77-music-track", "aes77-interstitial"] {
            let profile = ComplianceProfile::builtin(name).unwrap();
            assert_eq!(profile.loudness_tolerance_lu, None, "{name}");
            assert_eq!(profile.lower_tolerance_lu, None, "{name}");
            assert_eq!(profile.upper_tolerance_lu, Some(0.2), "{name}");

            let target = profile.target_lufs.unwrap();
            let upper = target + profile.upper_tolerance_lu.unwrap();
            assert!(profile_accepts_loudness(&profile, target), "{name}");
            assert!(profile_accepts_loudness(&profile, upper), "{name}");
            assert!(
                profile_accepts_loudness(&profile, upper.next_down()),
                "{name}"
            );
            assert!(
                !profile_accepts_loudness(&profile, upper.next_up()),
                "{name}"
            );
            assert!(
                profile_accepts_loudness(&profile, target - 20.0),
                "{name} must not invent a lower loudness bound"
            );
        }
    }

    #[test]
    fn low_plr_music_profile_enforces_the_exclusive_ebu_boundary() {
        let profile = ComplianceProfile::builtin("ebu-r128-s2-music-low-plr").unwrap();
        let mut analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000 * 60,
            kind: crate::wav::PcmKind::S24,
            lufs: -16.0,
            max_momentary_lufs: -14.0,
            max_short_term_lufs: -15.0,
            loudness_range_lu: 4.0,
            rms_db: -18.0,
            sample_peak: 0.8,
            true_peak: 10.0_f32.powf(-1.1 / 20.0),
            loudness_blocks: Vec::new(),
        };
        let result = profile.evaluate(&analysis).unwrap();
        let plr = result
            .rules
            .iter()
            .find(|rule| rule.metric == "peak_to_loudness_ratio_lu")
            .unwrap();
        assert!(plr.passed);
        assert_eq!(plr.maximum, Some(15.0));
        assert_eq!(plr.maximum_inclusive, Some(false));

        analysis.true_peak = 10.0_f32.powf(-0.9 / 20.0);
        assert!(!profile.evaluate(&analysis).unwrap().passed);

        let boundary = ComplianceRuleResult::bounded(
            "peak_to_loudness_ratio_lu",
            15.0,
            0.0,
            15.0,
            true,
            false,
        );
        assert!(
            !boundary.passed,
            "EBU R 128 s2 requires PLR lower than 15 dB"
        );
    }

    #[test]
    fn custom_profile_checks_inclusive_lra_and_plr_ranges() {
        let profile = ComplianceProfile {
            name: "content-class".into(),
            standard: None,
            standard_version: None,
            loudness_basis: LoudnessBasis::Programme,
            target_lufs: None,
            loudness_tolerance_lu: None,
            lower_tolerance_lu: None,
            upper_tolerance_lu: None,
            max_true_peak_dbtp: None,
            max_short_term_lufs: None,
            max_momentary_lufs: None,
            min_loudness_range_lu: Some(3.0),
            max_loudness_range_lu: Some(18.0),
            min_peak_to_loudness_ratio_lu: Some(8.0),
            max_peak_to_loudness_ratio_lu: Some(20.0),
            peak_to_loudness_ratio_max_exclusive: false,
            max_loudness_to_dialogue_ratio_lu: None,
        };
        profile.validate().unwrap();
        let analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000 * 60,
            kind: crate::wav::PcmKind::S24,
            lufs: -20.0,
            max_momentary_lufs: -15.0,
            max_short_term_lufs: -17.0,
            loudness_range_lu: 3.0,
            rms_db: -22.0,
            sample_peak: 0.8,
            true_peak: 1.0,
            loudness_blocks: Vec::new(),
        };
        let result = profile.evaluate(&analysis).unwrap();
        assert!(result.passed);
        assert_eq!(result.rules.len(), 2);
        assert!(result.rules.iter().all(
            |rule| rule.minimum_inclusive == Some(true) && rule.maximum_inclusive == Some(true)
        ));
    }

    #[test]
    fn csv_has_headers_and_quotes_paths() {
        let mut output = Vec::new();
        write_csv(&mut output, &[sample_report()]).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with(
            "analysis_engine_id,measurement_algorithm_revision,path,duration_seconds,"
        ));
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
    fn reports_distinguish_silence_from_an_undefined_measurement() {
        let mut report = sample_report();
        report.integrated_lufs = f64::NEG_INFINITY;
        report.max_momentary_lufs = f64::NEG_INFINITY;
        report.rms_dbfs = f64::NEG_INFINITY;
        report.sample_peak_dbfs = f64::NEG_INFINITY;
        report.true_peak_dbtp = f64::NEG_INFINITY;
        report.peak_to_loudness_ratio_lu = f64::NAN;
        report.downmix_integrated_lufs = Some(f64::NEG_INFINITY);
        report.adm_render_integrated_lufs = None;

        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["integrated_lufs"], "-inf");
        assert_eq!(value["rms_dbfs"], "-inf");
        assert_eq!(value["true_peak_dbtp"], "-inf");
        assert_eq!(value["peak_to_loudness_ratio_lu"], serde_json::Value::Null);
        assert_eq!(value["downmix_integrated_lufs"], "-inf");
        assert_eq!(value["adm_render_integrated_lufs"], serde_json::Value::Null);

        let mut csv = Vec::new();
        write_csv(&mut csv, &[report]).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.contains("-inf"));
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
            standard: None,
            standard_version: None,
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
            min_peak_to_loudness_ratio_lu: None,
            max_peak_to_loudness_ratio_lu: None,
            peak_to_loudness_ratio_max_exclusive: false,
            max_loudness_to_dialogue_ratio_lu: None,
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
        assert!(csv.starts_with(
            "analysis_engine_id,measurement_algorithm_revision,path,start_seconds,end_seconds,"
        ));
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

    fn h872_analysis(lufs: f64, seconds: usize) -> crate::normalize::Analysis {
        let sample_rate = 48_000;
        let frames = sample_rate as usize * seconds;
        let momentary = (0.4 * sample_rate as f64).round() as usize;
        let hop = (0.1 * sample_rate as f64).round() as usize;
        let blocks = if frames < momentary {
            0
        } else {
            (frames - momentary) / hop + 1
        };
        crate::normalize::Analysis {
            sample_rate,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames,
            kind: crate::wav::PcmKind::S24,
            lufs,
            max_momentary_lufs: lufs,
            max_short_term_lufs: lufs,
            loudness_range_lu: 0.0,
            rms_db: lufs,
            sample_peak: 0.5,
            true_peak: 10.0_f32.powf(-1.1 / 20.0),
            loudness_blocks: vec![10.0_f64.powf((lufs + 0.691) / 10.0); blocks],
        }
    }

    #[test]
    fn itu_h872_profiles_scan_complete_thirty_minute_windows() {
        let standard = ComplianceProfile::builtin("itu-h872-game").unwrap();
        let result = standard.evaluate(&h872_analysis(-23.0, 30 * 60)).unwrap();
        assert!(result.passed, "{:#?}", result.rules);
        assert_eq!(result.standard.as_deref(), Some("ITU-T H.872 clause 9.3.1"));
        assert!(result
            .rules
            .iter()
            .any(|rule| rule.metric == "minimum_rolling_30m_integrated_lufs"));

        let loud = standard.evaluate(&h872_analysis(-20.0, 30 * 60)).unwrap();
        assert!(!loud.passed);
        assert!(
            !loud
                .rules
                .iter()
                .find(|rule| rule.metric == "maximum_rolling_30m_integrated_lufs")
                .unwrap()
                .passed
        );

        let handheld = ComplianceProfile::builtin("itu-h872-handheld").unwrap();
        assert!(
            handheld
                .evaluate(&h872_analysis(-18.0, 30 * 60))
                .unwrap()
                .passed
        );
    }

    #[test]
    fn itu_h872_rejects_a_capture_without_a_complete_window() {
        let profile = ComplianceProfile::builtin("itu-h872-game").unwrap();
        let result = profile.evaluate(&h872_analysis(-23.0, 29 * 60)).unwrap();
        assert!(!result.passed);
        let duration = result
            .rules
            .iter()
            .find(|rule| rule.metric == "capture_duration_seconds")
            .unwrap();
        assert!(!duration.passed);
        assert_eq!(result.rules.len(), 2, "duration and true-peak rules only");
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
    fn broadcast_exception_profiles_keep_the_upper_limit() {
        let mut analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000,
            kind: crate::wav::PcmKind::S24,
            lufs: -30.0,
            max_momentary_lufs: -28.0,
            max_short_term_lufs: -29.0,
            loudness_range_lu: 4.0,
            rms_db: -32.0,
            sample_peak: 0.1,
            true_peak: 0.2,
            loudness_blocks: Vec::new(),
        };
        for name in ["arib-tr-b32-creative", "ebu-r128-creative"] {
            let profile = ComplianceProfile::builtin(name).unwrap();
            assert!(profile.evaluate(&analysis).unwrap().passed);
        }
        analysis.lufs = -22.7;
        assert!(
            !ComplianceProfile::builtin("arib-tr-b32-creative")
                .unwrap()
                .evaluate(&analysis)
                .unwrap()
                .passed
        );
        assert!(
            !ComplianceProfile::builtin("ebu-r128-creative")
                .unwrap()
                .evaluate(&analysis)
                .unwrap()
                .passed
        );
        let live = ComplianceProfile::builtin("ebu-r128-live").unwrap();
        analysis.lufs = -22.0;
        assert!(live.evaluate(&analysis).unwrap().passed);
        assert_eq!(
            ComplianceProfile::builtin("arib-tr-b32")
                .unwrap()
                .standard_version
                .as_deref(),
            Some("1.6 (2025)")
        );
        assert_eq!(
            ComplianceProfile::builtin("atsc-a85-short")
                .unwrap()
                .standard_version
                .as_deref(),
            Some("2026-07")
        );
    }

    #[test]
    fn cinematic_profile_limits_loudness_to_dialogue_ratio() {
        let mut analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000 * 60,
            kind: crate::wav::PcmKind::S24,
            lufs: -23.0,
            max_momentary_lufs: -20.0,
            max_short_term_lufs: -21.0,
            loudness_range_lu: 8.0,
            rms_db: -25.0,
            sample_peak: 0.5,
            true_peak: 0.5,
            loudness_blocks: Vec::new(),
        };
        let profile = ComplianceProfile::builtin("ebu-r128-cinematic").unwrap();
        assert!(profile.requires_dialogue());
        assert!(
            profile
                .evaluate_with_dialogue(&analysis, Some(-28.0))
                .unwrap()
                .passed
        );
        analysis.lufs = -22.9;
        let result = profile
            .evaluate_with_dialogue(&analysis, Some(-28.0))
            .unwrap();
        let ldr = result
            .rules
            .iter()
            .find(|rule| rule.metric == "loudness_to_dialogue_ratio_lu")
            .unwrap();
        assert!((ldr.measured - 5.1).abs() < 1e-12);
        assert!(!ldr.passed);
        assert!(!result.passed);
    }

    #[test]
    fn codec_metadata_checks_dialnorm_against_dialogue() {
        let analysis = crate::normalize::Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000,
            kind: crate::wav::PcmKind::S24,
            lufs: -23.0,
            max_momentary_lufs: -20.0,
            max_short_term_lufs: -21.0,
            loudness_range_lu: 4.0,
            rms_db: -25.0,
            sample_peak: 0.5,
            true_peak: 0.5,
            loudness_blocks: Vec::new(),
        };
        let dialogue = DialogueMeasurement {
            lufs: -27.5,
            duration_seconds: 10.0,
            range_count: 1,
            standard: "EBU R 128 s4",
            method: "test",
            source: DialogueSource::Mix,
        };
        let metadata = CodecMetadata {
            codec: "eac3".into(),
            dialnorm_lkfs: Some(-27.0),
            encoded_loudness_lufs: Some(-24.5),
            downmix_mode: Some("loro".into()),
            tolerance_lu: Some(1.0),
        };
        let result = metadata.evaluate(&analysis, Some(&dialogue));
        assert_eq!(result.loudness_basis, "dialogue");
        assert_eq!(result.dialnorm_pass, Some(true));
        assert_eq!(result.encoded_loudness_pass, Some(false));
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
        let mut profile = ComplianceProfile {
            name: "custom".into(),
            standard: None,
            standard_version: None,
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
            min_peak_to_loudness_ratio_lu: None,
            max_peak_to_loudness_ratio_lu: None,
            peak_to_loudness_ratio_max_exclusive: false,
            max_loudness_to_dialogue_ratio_lu: None,
        };
        assert!(profile.validate().is_err());
        profile.target_lufs = None;
        profile.loudness_tolerance_lu = None;
        profile.upper_tolerance_lu = None;
        profile.peak_to_loudness_ratio_max_exclusive = true;
        assert!(profile.validate().is_err());
        profile.max_peak_to_loudness_ratio_lu = Some(10.0);
        profile.min_peak_to_loudness_ratio_lu = Some(11.0);
        assert!(profile.validate().is_err());
        profile.min_peak_to_loudness_ratio_lu = Some(-1.0);
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
                "max_loudness_range_lu": null,
                "min_peak_to_loudness_ratio_lu": 8.0,
                "max_peak_to_loudness_ratio_lu": 15.0,
                "peak_to_loudness_ratio_max_exclusive": true
            }"#,
        )
        .unwrap();
        let loaded = ComplianceProfile::load(json_path.path().to_str().unwrap()).unwrap();
        assert_eq!(loaded.name, "station-json");
        assert_eq!(loaded.min_peak_to_loudness_ratio_lu, Some(8.0));
        assert!(loaded.peak_to_loudness_ratio_max_exclusive);

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
