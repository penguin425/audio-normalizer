//! Dry-run planning for conservative loudness remediation.
//!
//! The planner measures one source and describes the smallest static-gain and
//! dynamic-protection actions that could bring it inside the requested
//! true-peak and loudness-range limits.  It never writes or rewrites audio.
//! Dynamic processing is intentionally a projection only: a caller must render
//! the plan from the original source and remeasure the result before delivery.

use crate::decoder;
use crate::normalize;
use crate::wav::named_channel_layout;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-smart-remediation-1";
pub const DEFAULT_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_DECODED_SAMPLES: u64 = 24 * 1024 * 1024;
pub const DEFAULT_TRUE_PEAK_CEILING_DBTP: f64 = -1.0;
pub const DEFAULT_MAX_STATIC_GAIN_DB: f64 = 12.0;
pub const DEFAULT_MAX_DYNAMIC_REDUCTION_DB: f64 = 6.0;

/// Versioned, bounded input to the dry-run planner.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemediationSpec {
    pub schema_version: u32,
    pub source: PathBuf,
    /// Optional integrated-loudness target. When omitted, the planner keeps
    /// the current loudness and only evaluates the safety limits.
    #[serde(default)]
    pub target_lufs: Option<f64>,
    #[serde(default = "default_true_peak_ceiling")]
    pub true_peak_ceiling_dbtp: f64,
    /// Optional maximum EBU/ITU loudness range. Static gain cannot change LRA;
    /// an advisory compressor action is proposed when this is exceeded.
    #[serde(default)]
    pub max_loudness_range_lu: Option<f64>,
    #[serde(default = "default_max_static_gain")]
    pub max_static_gain_db: f64,
    #[serde(default = "default_max_dynamic_reduction")]
    pub max_dynamic_reduction_db: f64,
    /// Optional WAVE channel-order override for absent or ambiguous metadata.
    #[serde(default)]
    pub channel_layout: Option<String>,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: u64,
    #[serde(default = "default_max_decoded_samples")]
    pub max_decoded_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemediationReport {
    pub schema_version: u32,
    pub validator: &'static str,
    pub classification: &'static str,
    pub source: String,
    pub source_bytes: u64,
    pub source_sha256: String,
    /// Hash of the effective planner settings, excluding the source path and
    /// resource limits. It binds a plan to the policy that produced it.
    pub settings_sha256: String,
    pub limits: Limits,
    pub before: Measurement,
    pub plan: Plan,
    pub warnings: Vec<String>,
    pub feasible: bool,
    pub requires_audio_write: bool,
    pub manual_review_required: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub target_lufs: Option<f64>,
    pub true_peak_ceiling_dbtp: f64,
    pub max_loudness_range_lu: Option<f64>,
    pub max_static_gain_db: f64,
    pub max_dynamic_reduction_db: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Measurement {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: usize,
    pub duration_seconds: f64,
    pub integrated_lufs: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub sample_peak_dbfs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub lra_stable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// The static gain is always rendered from the original source if used.
    pub static_gain_db: f64,
    pub projected_after_static_gain: ProjectedMeasurement,
    pub projected_after_dynamic_actions: ProjectedMeasurement,
    pub target_loudness_passed: Option<bool>,
    pub true_peak_passed: bool,
    pub loudness_range_passed: Option<bool>,
    pub true_peak_excess_db: Option<f64>,
    pub loudness_range_excess_lu: Option<f64>,
    pub actions: Vec<Action>,
    pub infeasibility_reasons: Vec<String>,
    pub minimal_change: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectedMeasurement {
    /// Dynamic processing cannot be predicted exactly without rendering, so
    /// integrated loudness is null whenever an action changes dynamics.
    pub integrated_lufs: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    #[serde(rename = "kind")]
    pub kind: ActionKind,
    pub amount_db: Option<f64>,
    pub amount_lu: Option<f64>,
    pub rationale: String,
    pub requires_render_verification: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    StaticGain,
    TruePeakLimiter,
    LraCompressor,
}

/// Read a JSON/TOML request and produce a bounded dry-run report.
pub fn evaluate_file(path: &Path) -> Result<RemediationReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read remediation request {}: {error}", path.display()))?;
    let spec: RemediationSpec = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse remediation JSON: {error}"))?,
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("parse remediation TOML: {error}"))?
        }
        _ => return Err("remediation request must use .json or .toml".into()),
    };
    evaluate(path, spec)
}

/// Evaluate an already parsed request. No output audio is created.
pub fn evaluate(path: &Path, spec: RemediationSpec) -> Result<RemediationReport, String> {
    validate_spec(&spec)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let source_path = resolve(base, &spec.source);
    let source_bytes = fs::metadata(&source_path)
        .map_err(|error| format!("stat remediation source {}: {error}", source_path.display()))?
        .len();
    if source_bytes > spec.max_input_bytes {
        return Err(format!(
            "remediation source {} is {source_bytes} bytes, above max_input_bytes {}",
            source_path.display(),
            spec.max_input_bytes
        ));
    }
    let source_sha256 = sha256_file_bounded(&source_path, spec.max_input_bytes)?;
    let (mut audio, layout_provenance) =
        decoder::decode_limited_with_layout(&source_path, spec.max_decoded_samples).map_err(
            |error| {
                format!(
                    "decode remediation source {}: {error}",
                    source_path.display()
                )
            },
        )?;
    let role_override = spec
        .channel_layout
        .as_deref()
        .map(|layout| {
            named_channel_layout(layout)
                .ok_or_else(|| format!("unsupported channel layout {layout}"))
        })
        .transpose()?;
    audio.channel_roles = normalize::resolve_decoded_channel_roles(
        &source_path,
        audio.channels,
        &audio.channel_roles,
        layout_provenance,
        role_override.as_deref(),
    )?;
    let analysis = normalize::analyze(&audio);
    let settings_sha256 = settings_sha256(&spec)?;
    let before = measurement(&analysis);
    let result = plan(&analysis, &spec);
    Ok(RemediationReport {
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        classification: "engineering-qc-dry-run; non-normative until re-rendered and verified",
        source: source_path.to_string_lossy().into_owned(),
        source_bytes,
        source_sha256,
        settings_sha256,
        limits: Limits {
            target_lufs: spec.target_lufs,
            true_peak_ceiling_dbtp: spec.true_peak_ceiling_dbtp,
            max_loudness_range_lu: spec.max_loudness_range_lu,
            max_static_gain_db: spec.max_static_gain_db,
            max_dynamic_reduction_db: spec.max_dynamic_reduction_db,
        },
        before,
        plan: result.plan,
        warnings: result.warnings,
        feasible: result.feasible,
        requires_audio_write: result.requires_audio_write,
        manual_review_required: result.manual_review_required,
    })
}

struct PlanResult {
    plan: Plan,
    warnings: Vec<String>,
    feasible: bool,
    requires_audio_write: bool,
    manual_review_required: bool,
}

fn plan(analysis: &crate::analysis::Analysis, spec: &RemediationSpec) -> PlanResult {
    let mut reasons = Vec::new();
    let mut warnings = Vec::new();
    let requested_gain_db = spec
        .target_lufs
        .zip(finite(analysis.lufs))
        .map_or(0.0, |(target, current)| target - current);
    let static_gain_db = requested_gain_db.clamp(-spec.max_static_gain_db, spec.max_static_gain_db);
    let target_loudness_passed = match (spec.target_lufs, finite(analysis.lufs)) {
        (None, _) => Some(true),
        (Some(target), Some(current)) => Some(
            requested_gain_db.abs() <= spec.max_static_gain_db + 1.0e-9
                && (current + static_gain_db - target).abs() <= 1.0e-9,
        ),
        (Some(_), None) => Some(false),
    };
    if target_loudness_passed == Some(false) {
        if spec.target_lufs.is_some() && !analysis.lufs.is_finite() {
            reasons.push(
                "target loudness cannot be computed from a non-finite source measurement".into(),
            );
        } else {
            reasons.push(format!(
                "target loudness requires {requested_gain_db:.3} dB static gain, above max_static_gain_db {:.3}",
                spec.max_static_gain_db
            ));
        }
    }

    let projected_lufs = add_db(finite(analysis.lufs), static_gain_db);
    let projected_true_peak = add_db(finite(analysis.true_peak_db()), static_gain_db);
    let projected_after_static_gain = ProjectedMeasurement {
        integrated_lufs: projected_lufs,
        loudness_range_lu: finite(analysis.loudness_range_lu),
        true_peak_dbtp: projected_true_peak,
    };
    let true_peak_excess_db = projected_true_peak
        .filter(|value| *value > spec.true_peak_ceiling_dbtp)
        .map(|value| value - spec.true_peak_ceiling_dbtp);
    let true_peak_limiter_needed = true_peak_excess_db.is_some_and(|value| value > 1.0e-9);
    if true_peak_limiter_needed
        && true_peak_excess_db.expect("checked above") > spec.max_dynamic_reduction_db
    {
        reasons.push(format!(
            "true-peak remediation requires {:.3} dB reduction, above max_dynamic_reduction_db {:.3}",
            true_peak_excess_db.expect("checked above"),
            spec.max_dynamic_reduction_db
        ));
    }
    let true_peak_passed = !true_peak_limiter_needed
        || true_peak_excess_db.expect("checked above") <= spec.max_dynamic_reduction_db;

    let (loudness_range_passed, loudness_range_excess_lu) = match (
        spec.max_loudness_range_lu,
        finite(analysis.loudness_range_lu),
    ) {
        (None, _) => (Some(true), None),
        (Some(_), None) => (Some(false), None),
        (Some(limit), Some(value)) if !analysis.loudness_range_stable() => {
            warnings.push(format!(
                "LRA is not stable before {:.0} seconds; render and review manually",
                crate::analysis::Analysis::LRA_STABLE_AFTER_SECONDS
            ));
            (None, (value > limit).then_some(value - limit))
        }
        (Some(limit), Some(value)) => {
            let excess = (value > limit).then_some(value - limit);
            let passed =
                excess.is_none_or(|amount| amount <= spec.max_dynamic_reduction_db + 1.0e-9);
            (Some(passed), excess)
        }
    };
    if loudness_range_passed == Some(false) && loudness_range_excess_lu.is_none() {
        reasons.push("loudness range is not finite and cannot be bounded".into());
    }
    if let Some(excess) = loudness_range_excess_lu {
        if excess > spec.max_dynamic_reduction_db {
            reasons.push(format!(
                "LRA remediation requires {:.3} LU dynamic reduction, above max_dynamic_reduction_db {:.3}",
                excess, spec.max_dynamic_reduction_db
            ));
        }
    }
    let lra_action_needed = loudness_range_excess_lu.is_some_and(|value| value > 1.0e-9);
    let mut actions = Vec::new();
    if static_gain_db.abs() > 1.0e-9 {
        actions.push(Action {
            kind: ActionKind::StaticGain,
            amount_db: Some(static_gain_db),
            amount_lu: None,
            rationale: spec.target_lufs.map_or(
                "reduce static gain to satisfy the requested safety policy".into(),
                |target| format!("move integrated loudness toward target {target:.3} LUFS"),
            ),
            requires_render_verification: false,
        });
    }
    if true_peak_limiter_needed {
        actions.push(Action {
            kind: ActionKind::TruePeakLimiter,
            amount_db: true_peak_excess_db,
            amount_lu: None,
            rationale: format!(
                "limit the minimum {:.3} dB true-peak excess at {:.3} dBTP",
                true_peak_excess_db.expect("checked above"),
                spec.true_peak_ceiling_dbtp
            ),
            requires_render_verification: true,
        });
        warnings.push(
            "true-peak limiting changes the waveform; re-decode and verify loudness after rendering".into(),
        );
    }
    if lra_action_needed {
        actions.push(Action {
            kind: ActionKind::LraCompressor,
            amount_db: None,
            amount_lu: loudness_range_excess_lu,
            rationale: format!(
                "use the least-aggressive dynamics profile that targets {:.3} LU LRA",
                spec.max_loudness_range_lu
                    .expect("LRA action requires a limit")
            ),
            requires_render_verification: true,
        });
        warnings.push(
            "LRA compression is only a recommendation; Forge does not render a compressor in this dry-run".into(),
        );
    }
    let projected_after_dynamic_actions = ProjectedMeasurement {
        integrated_lufs: if true_peak_limiter_needed || lra_action_needed {
            None
        } else {
            projected_lufs
        },
        loudness_range_lu: if lra_action_needed {
            spec.max_loudness_range_lu
        } else {
            finite(analysis.loudness_range_lu)
        },
        true_peak_dbtp: if true_peak_limiter_needed {
            Some(spec.true_peak_ceiling_dbtp)
        } else {
            projected_true_peak
        },
    };
    let lra_stability_unverified =
        spec.max_loudness_range_lu.is_some() && !analysis.loudness_range_stable();
    let manual_review_required =
        lra_stability_unverified || true_peak_limiter_needed || lra_action_needed;
    let feasible = reasons.is_empty()
        && target_loudness_passed != Some(false)
        && true_peak_passed
        && loudness_range_passed != Some(false)
        && !lra_stability_unverified;
    let requires_audio_write = !actions.is_empty();
    PlanResult {
        plan: Plan {
            static_gain_db,
            projected_after_static_gain,
            projected_after_dynamic_actions,
            target_loudness_passed,
            true_peak_passed,
            loudness_range_passed,
            true_peak_excess_db,
            loudness_range_excess_lu,
            actions,
            infeasibility_reasons: reasons,
            minimal_change: true,
        },
        warnings,
        feasible,
        requires_audio_write,
        manual_review_required,
    }
}

fn measurement(analysis: &crate::analysis::Analysis) -> Measurement {
    Measurement {
        sample_rate_hz: analysis.sample_rate,
        channels: analysis.channels,
        frames: analysis.frames,
        duration_seconds: analysis.duration_secs(),
        integrated_lufs: finite(analysis.lufs),
        loudness_range_lu: finite(analysis.loudness_range_lu),
        sample_peak_dbfs: finite(analysis.sample_peak_db()),
        true_peak_dbtp: finite(analysis.true_peak_db()),
        lra_stable: analysis.loudness_range_stable(),
    }
}

fn validate_spec(spec: &RemediationSpec) -> Result<(), String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported remediation schema {}; expected {SCHEMA_VERSION}",
            spec.schema_version
        ));
    }
    if spec.source.as_os_str().is_empty() {
        return Err("remediation source path is required".into());
    }
    if let Some(target) = spec.target_lufs {
        if !target.is_finite() || !(-120.0..=6.0).contains(&target) {
            return Err("target_lufs must be finite and between -120 and 6 LUFS".into());
        }
    }
    if !spec.true_peak_ceiling_dbtp.is_finite()
        || !(-120.0..=6.0).contains(&spec.true_peak_ceiling_dbtp)
    {
        return Err("true_peak_ceiling_dbtp must be finite and between -120 and 6 dBTP".into());
    }
    if let Some(limit) = spec.max_loudness_range_lu {
        if !limit.is_finite() || !(0.0..=50.0).contains(&limit) {
            return Err("max_loudness_range_lu must be finite and between 0 and 50 LU".into());
        }
    }
    for (name, value) in [
        ("max_static_gain_db", spec.max_static_gain_db),
        ("max_dynamic_reduction_db", spec.max_dynamic_reduction_db),
    ] {
        if !value.is_finite() || !(0.0..=60.0).contains(&value) {
            return Err(format!("{name} must be finite and between 0 and 60 dB"));
        }
    }
    if let Some(layout) = spec.channel_layout.as_deref() {
        if named_channel_layout(layout).is_none() {
            return Err(format!("unsupported channel layout {layout}"));
        }
    }
    if spec.max_input_bytes == 0 || spec.max_decoded_samples == 0 {
        return Err("remediation input and decoded-sample limits must be greater than zero".into());
    }
    Ok(())
}

fn settings_sha256(spec: &RemediationSpec) -> Result<String, String> {
    let settings = serde_json::json!({
        "schema_version": spec.schema_version,
        "target_lufs": spec.target_lufs,
        "true_peak_ceiling_dbtp": spec.true_peak_ceiling_dbtp,
        "max_loudness_range_lu": spec.max_loudness_range_lu,
        "max_static_gain_db": spec.max_static_gain_db,
        "max_dynamic_reduction_db": spec.max_dynamic_reduction_db,
        "channel_layout": spec.channel_layout,
    });
    let bytes = serde_json::to_vec(&settings)
        .map_err(|error| format!("serialize remediation settings: {error}"))?;
    Ok(hash_bytes(&bytes))
}

fn sha256_file_bounded(path: &Path, max_bytes: u64) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open {} for SHA-256: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {} for SHA-256: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return Err(format!(
                "{} exceeds max_input_bytes {} while hashing",
                path.display(),
                max_bytes
            ));
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn add_db(value: Option<f64>, gain_db: f64) -> Option<f64> {
    value.and_then(|value| finite(value + gain_db))
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn default_true_peak_ceiling() -> f64 {
    DEFAULT_TRUE_PEAK_CEILING_DBTP
}

fn default_max_static_gain() -> f64 {
    DEFAULT_MAX_STATIC_GAIN_DB
}

fn default_max_dynamic_reduction() -> f64 {
    DEFAULT_MAX_DYNAMIC_REDUCTION_DB
}

fn default_max_input_bytes() -> u64 {
    DEFAULT_MAX_INPUT_BYTES
}

fn default_max_decoded_samples() -> u64 {
    DEFAULT_MAX_DECODED_SAMPLES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Analysis;
    use crate::wav::{default_channel_roles, PcmKind};

    fn analysis(lufs: f64, lra: f64, true_peak: f32, frames: usize) -> Analysis {
        Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: default_channel_roles(2),
            frames,
            kind: PcmKind::F32,
            lufs,
            max_momentary_lufs: lufs,
            max_short_term_lufs: lufs,
            loudness_range_lu: lra,
            rms_db: lufs,
            sample_peak: true_peak,
            true_peak,
            loudness_blocks: Vec::new(),
        }
    }

    fn spec() -> RemediationSpec {
        RemediationSpec {
            schema_version: 1,
            source: "input.wav".into(),
            target_lufs: Some(-16.0),
            true_peak_ceiling_dbtp: -1.0,
            max_loudness_range_lu: Some(12.0),
            max_static_gain_db: 12.0,
            max_dynamic_reduction_db: 6.0,
            channel_layout: None,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_decoded_samples: DEFAULT_MAX_DECODED_SAMPLES,
        }
    }

    #[test]
    fn planner_uses_static_gain_then_minimal_dynamic_actions() {
        let result = plan(&analysis(-20.0, 15.0, 1.0, 48_000 * 60), &spec());
        assert!((result.plan.static_gain_db - 4.0).abs() < 1e-9);
        assert_eq!(result.plan.actions.len(), 3);
        assert_eq!(result.plan.actions[0].kind, ActionKind::StaticGain);
        assert_eq!(result.plan.actions[1].kind, ActionKind::TruePeakLimiter);
        assert_eq!(result.plan.actions[2].kind, ActionKind::LraCompressor);
        assert!(result.feasible);
        assert!(result.manual_review_required);
    }

    #[test]
    fn compliant_source_has_no_audio_action() {
        let mut request = spec();
        request.target_lufs = None;
        request.max_loudness_range_lu = Some(20.0);
        let result = plan(&analysis(-16.0, 10.0, 0.5, 48_000 * 60), &request);
        assert!(result.feasible);
        assert!(result.plan.actions.is_empty());
        assert!(!result.requires_audio_write);
    }
}
