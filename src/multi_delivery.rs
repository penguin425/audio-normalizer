//! Conservative one-master/many-codec delivery workflow.

use crate::atomic::AtomicOutput;
use crate::dsp::resample::ResampleQuality;
use crate::normalization_diff::{self, FileEvidence, MeasurementEvidence};
use crate::normalize::{self, Mode, OutputFormat, Plan};
use crate::preset::Preset;
use crate::wav::{ChannelRole, WavContainer};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub const REQUEST_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/multi-delivery-request-v1";
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/multi-delivery-report-v1";
const MAX_REQUEST_BYTES: u64 = 1_048_576;
const MAX_DELIVERIES: usize = 32;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiDeliveryRequest {
    pub schema: String,
    #[serde(default = "default_tolerance")]
    pub verify_tolerance_lu_db: f64,
    #[serde(default = "default_retries")]
    pub verify_retries: usize,
    pub max_gain_db: Option<f64>,
    #[serde(default = "default_mp3_bitrate")]
    pub mp3_bitrate_kbps: i32,
    #[serde(default = "default_mp3_quality")]
    pub mp3_quality: i32,
    pub deliveries: Vec<DeliveryRequest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryRequest {
    pub id: String,
    pub output: PathBuf,
    pub format: String,
    pub preset: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MultiDeliveryReport {
    pub schema: &'static str,
    pub generator: &'static str,
    pub method: MethodEvidence,
    pub source: FileEvidence,
    pub source_measurement: MeasurementEvidence,
    pub common: CommonEvidence,
    pub deliveries: Vec<DeliveryEvidence>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MethodEvidence {
    pub id: &'static str,
    pub classification: &'static str,
    pub common_target_rule: &'static str,
    pub common_ceiling_rule: &'static str,
    pub correction_rule: &'static str,
    pub maximum_deliveries: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommonEvidence {
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
    pub verification_target_lufs: f64,
    pub final_pre_codec_lufs: f64,
    pub shared_gain_db: f64,
    pub verification_tolerance_lu_db: f64,
    pub encoding_passes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryEvidence {
    pub id: String,
    pub format: String,
    pub output: FileEvidence,
    pub profile: ProfileEvidence,
    pub decoded: MeasurementEvidence,
    pub verification_target_lufs: f64,
    pub final_pre_codec_lufs: f64,
    pub level_deviation_lu: f64,
    pub profile_loudness_headroom_lu: f64,
    pub profile_true_peak_headroom_db: Option<f64>,
    pub level_passed: bool,
    pub true_peak_passed: bool,
    pub conservative_profile_bounds_passed: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileEvidence {
    pub requested: String,
    pub resolved: String,
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
    pub evidence: Option<&'static str>,
    pub source_url: Option<&'static str>,
    pub checked_on: Option<&'static str>,
    pub caveat: Option<&'static str>,
}

fn default_tolerance() -> f64 {
    0.5
}

fn default_retries() -> usize {
    2
}

fn default_mp3_bitrate() -> i32 {
    320
}

fn default_mp3_quality() -> i32 {
    2
}

pub fn load_request(path: &Path) -> Result<MultiDeliveryRequest, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "multi-delivery request is not a file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "multi-delivery request exceeds the {} byte limit",
            MAX_REQUEST_BYTES
        ));
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let request = match path.extension().and_then(|value| value.to_str()) {
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?
        }
        _ => serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?,
    };
    validate_request(&request)?;
    Ok(request)
}

pub fn run(
    input: &Path,
    request_path: &Path,
    report_path: &Path,
    overwrite: bool,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<MultiDeliveryReport, String> {
    let request = load_request(request_path)?;
    let request_parent = request_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let outputs = request
        .deliveries
        .iter()
        .map(|delivery| {
            if delivery.output.is_absolute() {
                delivery.output.clone()
            } else {
                request_parent.join(&delivery.output)
            }
        })
        .collect::<Vec<_>>();
    validate_paths(input, request_path, report_path, &outputs, overwrite)?;
    for output in &outputs {
        if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
    }
    if let Some(parent) = report_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let presets = request
        .deliveries
        .iter()
        .map(|delivery| {
            Preset::named(&delivery.preset)
                .ok_or_else(|| format!("unknown multi-delivery preset: {}", delivery.preset))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let formats = request
        .deliveries
        .iter()
        .map(|delivery| parse_format(&delivery.format))
        .collect::<Result<Vec<_>, _>>()?;
    let target_lufs = presets
        .iter()
        .map(|preset| preset.target_lufs)
        .fold(f64::INFINITY, f64::min);
    let ceiling_db = presets
        .iter()
        .map(|preset| preset.ceiling_db)
        .fold(f64::INFINITY, f64::min);
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs,
        target_peak_db: -0.1,
        target_rms_db: -18.0,
        ceiling_db,
        max_gain_db: request.max_gain_db,
        dither: false,
        output_kind: None,
        mp3_bitrate: request.mp3_bitrate_kbps,
        mp3_quality: request.mp3_quality,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: ResampleQuality::Balanced,
    };
    let input_snapshot = normalize::capture_stable_input(input)?;
    let source_file = FileEvidence {
        path: input.to_string_lossy().into_owned(),
        bytes: input_snapshot.byte_len(),
        sha256: input_snapshot.binding().sha256_hex(),
    };
    let result = normalize::normalize_multi_delivery_corrected_with_roles(
        &input_snapshot,
        &outputs,
        &plan,
        &formats,
        request.verify_tolerance_lu_db,
        request.verify_retries,
        channel_roles,
    )?;
    if !result.source.lufs.is_finite()
        || !result.expected_level.is_finite()
        || result
            .renders
            .iter()
            .any(|render| !render.intended.lufs.is_finite())
    {
        return Err("multi-delivery requires finite source and intended loudness".into());
    }
    let deliveries = request
        .deliveries
        .iter()
        .zip(&outputs)
        .zip(&formats)
        .zip(&presets)
        .zip(&result.verifications)
        .enumerate()
        .map(
            |(index, ((((delivery, output), format), preset), verification))| {
                let true_peak = verification.output.true_peak_db();
                let profile_peak_headroom = true_peak
                    .is_finite()
                    .then_some(preset.ceiling_db - true_peak);
                let conservative_profile_bounds_passed = profile_bounds_passed(
                    verification.output.lufs,
                    true_peak,
                    preset.target_lufs,
                    preset.ceiling_db,
                    request.verify_tolerance_lu_db,
                );
                Ok(DeliveryEvidence {
                    id: delivery.id.clone(),
                    format: format_name(*format).into(),
                    output: normalization_diff::inspect_file(output)?,
                    profile: profile_evidence(delivery, *preset),
                    decoded: MeasurementEvidence::from(&verification.output),
                    verification_target_lufs: verification.expected_level,
                    final_pre_codec_lufs: result.renders[index].intended.lufs,
                    level_deviation_lu: verification.deviation,
                    profile_loudness_headroom_lu: preset.target_lufs - verification.output.lufs,
                    profile_true_peak_headroom_db: profile_peak_headroom,
                    level_passed: verification.level_ok,
                    true_peak_passed: verification.true_peak_ok,
                    conservative_profile_bounds_passed,
                    passed: verification.passed() && conservative_profile_bounds_passed,
                })
            },
        )
        .collect::<Result<Vec<_>, String>>()?;
    let report = MultiDeliveryReport {
        schema: REPORT_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        method: MethodEvidence {
            id: "forge-multi-delivery-v1",
            classification: "non-normative deterministic engineering optimization over versioned delivery profiles",
            common_target_rule: "minimum integrated-loudness target across all selected profiles",
            common_ceiling_rule: "minimum true-peak ceiling across all selected profiles",
            correction_rule: "quietest point in the intersection of every decoded codec level-tolerance interval and true-peak upper bound",
            maximum_deliveries: MAX_DELIVERIES,
        },
        source: source_file,
        source_measurement: MeasurementEvidence::from(&result.source),
        common: CommonEvidence {
            target_lufs,
            ceiling_dbtp: ceiling_db,
            verification_target_lufs: result.expected_level,
            final_pre_codec_lufs: result.renders[0].intended.lufs,
            shared_gain_db: 20.0 * f64::from(result.gain).log10(),
            verification_tolerance_lu_db: request.verify_tolerance_lu_db,
            encoding_passes: result.attempts,
        },
        passed: deliveries.iter().all(|delivery| delivery.passed),
        deliveries,
    };
    write_report(report_path, &report)?;
    Ok(report)
}

fn validate_request(request: &MultiDeliveryRequest) -> Result<(), String> {
    if request.schema != REQUEST_SCHEMA {
        return Err(format!(
            "unsupported multi-delivery request schema: {}",
            request.schema
        ));
    }
    if !(2..=MAX_DELIVERIES).contains(&request.deliveries.len()) {
        return Err(format!(
            "multi-delivery requires 2..={MAX_DELIVERIES} deliveries"
        ));
    }
    if !request.verify_tolerance_lu_db.is_finite() || request.verify_tolerance_lu_db < 0.0 {
        return Err("verify_tolerance_lu_db must be a finite non-negative number".into());
    }
    if request.verify_retries > 10 {
        return Err("verify_retries must not exceed 10".into());
    }
    if request.max_gain_db.is_some_and(|value| !value.is_finite()) {
        return Err("max_gain_db must be finite".into());
    }
    if !(8..=320).contains(&request.mp3_bitrate_kbps) {
        return Err("mp3_bitrate_kbps must be 8..=320".into());
    }
    if !(0..=9).contains(&request.mp3_quality) {
        return Err("mp3_quality must be 0..=9".into());
    }
    let mut ids = HashSet::new();
    let mut outputs = HashSet::new();
    for delivery in &request.deliveries {
        if delivery.id.is_empty()
            || delivery.id.len() > 64
            || !delivery
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(format!("invalid delivery id: {}", delivery.id));
        }
        if !ids.insert(delivery.id.clone()) {
            return Err(format!("duplicate delivery id: {}", delivery.id));
        }
        if delivery.output.as_os_str().is_empty() || !outputs.insert(delivery.output.clone()) {
            return Err(format!(
                "empty or duplicate delivery output: {}",
                delivery.output.display()
            ));
        }
        let format = parse_format(&delivery.format)?;
        validate_output_extension(&delivery.output, format)?;
        if Preset::named(&delivery.preset).is_none() {
            return Err(format!(
                "unknown multi-delivery preset: {}",
                delivery.preset
            ));
        }
    }
    Ok(())
}

fn validate_paths(
    input: &Path,
    request: &Path,
    report: &Path,
    outputs: &[PathBuf],
    overwrite: bool,
) -> Result<(), String> {
    let input = resolved_path(input)?;
    let request = resolved_path(request)?;
    let report = resolved_path(report)?;
    let input_key = path_key(&input);
    let request_key = path_key(&request);
    let report_key = path_key(&report);
    if report_key == input_key {
        return Err("multi-delivery report aliases the audio input".into());
    }
    if report_key == request_key {
        return Err("multi-delivery report aliases its request file".into());
    }
    if report.exists() && !overwrite {
        return Err(format!(
            "{} already exists (use --overwrite to replace it)",
            report.display()
        ));
    }
    let mut seen = HashSet::new();
    for output in outputs {
        let output = resolved_path(output)?;
        let output_key = path_key(&output);
        if output_key == input_key
            || output_key == request_key
            || output_key == report_key
            || !seen.insert(output_key)
        {
            return Err(format!(
                "multi-delivery path collision: {}",
                output.display()
            ));
        }
        if output.exists() && !overwrite {
            return Err(format!(
                "{} already exists (use --overwrite to replace it)",
                output.display()
            ));
        }
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
type PathKey = String;
#[cfg(not(any(windows, target_os = "macos")))]
type PathKey = PathBuf;

#[cfg(any(windows, target_os = "macos"))]
fn path_key(path: &Path) -> PathKey {
    // Default Windows and macOS filesystems are case-insensitive. Rejecting a
    // case-only distinction is conservative even on a case-sensitive volume.
    path.to_string_lossy().to_lowercase()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn path_key(path: &Path) -> PathKey {
    path.to_owned()
}

/// Resolve lexical aliases and symlinked existing ancestors without requiring
/// the final output to exist yet.
fn resolved_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve {}: {error}", path.display()))?
            .join(path)
    };
    let mut lexical = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                lexical.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !lexical.pop() {
                    return Err(format!(
                        "path escapes its filesystem root: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    if lexical.exists() {
        return fs::canonicalize(&lexical)
            .map_err(|error| format!("resolve {}: {error}", path.display()));
    }
    let mut ancestor = lexical.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| format!("cannot resolve path: {}", path.display()))?;
        suffix.push(name.to_owned());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| format!("cannot resolve path: {}", path.display()))?;
    }
    let mut resolved = fs::canonicalize(ancestor)
        .map_err(|error| format!("resolve {}: {error}", path.display()))?;
    for name in suffix.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn validate_output_extension(path: &Path, format: OutputFormat) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    let matches = matches!(
        (format, extension.as_deref()),
        (OutputFormat::Wav, Some("wav"))
            | (OutputFormat::Flac, Some("flac"))
            | (OutputFormat::Mp3, Some("mp3"))
            | (OutputFormat::Opus, Some("opus"))
            | (OutputFormat::M4a | OutputFormat::Alac, Some("m4a" | "mp4"))
            | (OutputFormat::Vorbis, Some("ogg" | "oga"))
    );
    if matches {
        Ok(())
    } else {
        Err(format!(
            "output extension does not match format {}: {}",
            format_name(format),
            path.display()
        ))
    }
}

fn parse_format(value: &str) -> Result<OutputFormat, String> {
    match value {
        "wav" => Ok(OutputFormat::Wav),
        "flac" => Ok(OutputFormat::Flac),
        "mp3" => Ok(OutputFormat::Mp3),
        "opus" => Ok(OutputFormat::Opus),
        "m4a" => Ok(OutputFormat::M4a),
        "alac" => Ok(OutputFormat::Alac),
        "vorbis" => Ok(OutputFormat::Vorbis),
        _ => Err(format!("unsupported multi-delivery format: {value}")),
    }
}

fn format_name(value: OutputFormat) -> &'static str {
    match value {
        OutputFormat::Wav => "wav",
        OutputFormat::Flac => "flac",
        OutputFormat::Mp3 => "mp3",
        OutputFormat::Opus => "opus",
        OutputFormat::M4a => "m4a",
        OutputFormat::Alac => "alac",
        OutputFormat::Vorbis => "vorbis",
    }
}

fn profile_evidence(delivery: &DeliveryRequest, preset: Preset) -> ProfileEvidence {
    let provenance = preset.provenance;
    ProfileEvidence {
        requested: delivery.preset.clone(),
        resolved: preset.name.into(),
        target_lufs: preset.target_lufs,
        ceiling_dbtp: preset.ceiling_db,
        evidence: provenance.map(|value| value.evidence.as_str()),
        source_url: provenance.map(|value| value.source_url),
        checked_on: provenance.map(|value| value.checked_on),
        caveat: provenance.map(|value| value.caveat),
    }
}

fn profile_bounds_passed(
    loudness_lufs: f64,
    true_peak_dbtp: f64,
    target_lufs: f64,
    ceiling_dbtp: f64,
    loudness_tolerance: f64,
) -> bool {
    loudness_lufs <= target_lufs + loudness_tolerance
        && normalize::true_peak_within_ceiling(true_peak_dbtp, ceiling_dbtp)
}

fn write_report(path: &Path, report: &MultiDeliveryReport) -> Result<(), String> {
    let staged = AtomicOutput::new(path)?;
    let mut file = File::create(staged.path())
        .map_err(|error| format!("create {}: {error}", staged.path().display()))?;
    serde_json::to_writer_pretty(&mut file, report)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    drop(file);
    staged.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_bounds_never_apply_loudness_tolerance_to_true_peak() {
        assert!(profile_bounds_passed(-14.4, -1.0, -14.0, -1.0, 0.5));
        assert!(!profile_bounds_passed(-14.4, -0.75, -14.0, -1.0, 0.5));
    }
}
