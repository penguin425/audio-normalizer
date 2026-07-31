//! Bounded protocol adapter for licensed/reference AC-4 decoders.
//!
//! Forge does not ship an AC-4 decoder. This module invokes an explicitly
//! selected adapter executable, binds the invocation to the input and adapter
//! bytes, validates presentation-level metadata, and independently measures
//! every rendered WAVE presentation with Forge's BS.1770 engine.

use crate::analysis;
use crate::decoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-ac4-reference-adapter-1";
pub const PART1_STANDARD: &str = "ETSI TS 103 190-1 V1.4.1 (2025-07)";
pub const PART2_STANDARD: &str = "ETSI TS 103 190-2 V1.3.1 (2025-07)";
pub const REQUEST_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-request-v1";
pub const RESPONSE_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-response-v1";
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-report-v1";

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PRESENTATIONS: usize = 256;
const HARD_MAX_DECODED_SAMPLES: u64 = 200_000_000;
const MAX_TIMEOUT_SECONDS: u64 = 3_600;
const TOOL_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AdapterOptions {
    pub input: PathBuf,
    pub adapter: PathBuf,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub dialnorm_tolerance_lu: f64,
    pub max_true_peak_dbtp: Option<f64>,
}

#[derive(Debug, Serialize)]
struct AdapterRequest {
    schema: &'static str,
    protocol_version: u32,
    input_path: String,
    input_sha256: String,
    input_bytes: u64,
    output_directory: String,
    requirements: AdapterRequirements,
}

#[derive(Debug, Serialize)]
struct AdapterRequirements {
    enumerate_all_presentations: bool,
    rendered_format: &'static str,
    report_presentation_loudness_metadata: bool,
    ac4_part1_standard: &'static str,
    ac4_part2_standard: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterResponse {
    schema: String,
    protocol_version: u32,
    input_sha256: String,
    decoder: DecoderEvidence,
    ac4_part1_standard: String,
    ac4_part2_standard: String,
    presentation_count: usize,
    presentations: Vec<AdapterPresentation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecoderEvidence {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterPresentation {
    id: String,
    presentation_version: u8,
    rendered_path: PathBuf,
    output_layout: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    accessibility: Option<String>,
    loudness: Ac4LoudnessMetadata,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ac4LoudnessMetadata {
    pub dialnorm_bits: u8,
    pub dialnorm_lkfs: f64,
    pub dialnorm_source: DialnormSource,
    #[serde(default)]
    pub downmix_correction_db: Option<f64>,
    #[serde(default)]
    pub alternative_presentation_correction_db: Option<f64>,
    #[serde(default)]
    pub realtime_correction_db: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DialnormSource {
    PresentationSubstream,
    AssociatedBasicMetadata,
    MainOrDialogueSubstream,
}

#[derive(Debug, Serialize)]
pub struct Ac4AdapterReport {
    pub schema: &'static str,
    pub protocol_version: u32,
    pub validator: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub adapter_path: String,
    pub adapter_sha256: String,
    pub decoder: DecoderEvidence,
    pub ac4_part1_standard: &'static str,
    pub ac4_part2_standard: &'static str,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub dialnorm_tolerance_lu: f64,
    pub max_true_peak_dbtp: Option<f64>,
    pub presentation_count: usize,
    pub passed: bool,
    pub presentations: Vec<PresentationResult>,
}

#[derive(Debug, Serialize)]
pub struct PresentationResult {
    pub id: String,
    pub presentation_version: u8,
    pub output_layout: String,
    pub language: Option<String>,
    pub accessibility: Option<String>,
    pub loudness_metadata: Ac4LoudnessMetadata,
    pub rendered_sha256: String,
    pub rendered_bytes: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub measured_integrated_lufs: f64,
    pub measured_true_peak_dbtp: f64,
    pub dialnorm_drift_lu: f64,
    pub dialnorm_passed: bool,
    pub true_peak_passed: Option<bool>,
    pub passed: bool,
    pub checks: Vec<Ac4Check>,
}

#[derive(Debug, Serialize)]
pub struct Ac4Check {
    pub rule_id: &'static str,
    pub standard: &'static str,
    pub measured: f64,
    pub maximum: f64,
    pub unit: &'static str,
    pub passed: bool,
}

pub fn run(options: &AdapterOptions) -> Result<Ac4AdapterReport, String> {
    validate_options(options)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve AC-4 input {}: {error}", options.input.display()))?;
    let adapter = fs::canonicalize(&options.adapter).map_err(|error| {
        format!(
            "resolve AC-4 adapter {}: {error}",
            options.adapter.display()
        )
    })?;
    ensure_regular_file(&input, "AC-4 input")?;
    ensure_regular_file(&adapter, "AC-4 adapter")?;
    let (input_sha256, input_bytes) = sha256_file(&input)?;
    let (adapter_sha256, _) = sha256_file(&adapter)?;

    let work = tempfile::tempdir().map_err(|error| format!("create adapter workspace: {error}"))?;
    let renders = work.path().join("renders");
    fs::create_dir(&renders)
        .map_err(|error| format!("create adapter render directory: {error}"))?;
    let request_path = work.path().join("request.json");
    let response_path = work.path().join("response.json");
    let request = AdapterRequest {
        schema: REQUEST_SCHEMA,
        protocol_version: PROTOCOL_VERSION,
        input_path: input.to_string_lossy().into_owned(),
        input_sha256: input_sha256.clone(),
        input_bytes,
        output_directory: renders.to_string_lossy().into_owned(),
        requirements: AdapterRequirements {
            enumerate_all_presentations: true,
            rendered_format: "wave",
            report_presentation_loudness_metadata: true,
            ac4_part1_standard: PART1_STANDARD,
            ac4_part2_standard: PART2_STANDARD,
        },
    };
    let mut request_bytes = serde_json::to_vec_pretty(&request)
        .map_err(|error| format!("serialize AC-4 adapter request: {error}"))?;
    request_bytes.push(b'\n');
    fs::write(&request_path, request_bytes)
        .map_err(|error| format!("write AC-4 adapter request: {error}"))?;

    let tool = run_bounded(
        &adapter,
        &[
            "--request".into(),
            request_path.as_os_str().to_owned(),
            "--response".into(),
            response_path.as_os_str().to_owned(),
        ],
        Duration::from_secs(options.timeout_seconds),
    )?;
    if !tool.status.success() {
        return Err(format!(
            "AC-4 adapter failed ({}): {}",
            tool.status,
            String::from_utf8_lossy(&tool.stderr).trim()
        ));
    }
    let (adapter_after, _) = sha256_file(&adapter)?;
    if adapter_after != adapter_sha256 {
        return Err("AC-4 adapter executable changed while it was running".into());
    }
    let response_bytes = read_response(work.path(), &response_path)?;
    let response: AdapterResponse = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("parse AC-4 adapter response: {error}"))?;
    validate_response(&response, &input_sha256)?;

    let (input_after, bytes_after) = sha256_file(&input)?;
    if input_after != input_sha256 || bytes_after != input_bytes {
        return Err("AC-4 input changed while the decoder adapter was running".into());
    }

    let render_root = fs::canonicalize(&renders)
        .map_err(|error| format!("resolve adapter render directory: {error}"))?;
    let mut results = Vec::with_capacity(response.presentations.len());
    for presentation in response.presentations {
        let rendered = resolve_render(&render_root, &presentation.rendered_path)?;
        let (rendered_sha256, rendered_bytes) = sha256_file(&rendered)?;
        let buffer =
            decoder::decode_limited(&rendered, options.max_decoded_samples_per_presentation)?;
        let measured = analysis::analyze(&buffer);
        let (rendered_after, rendered_bytes_after) = sha256_file(&rendered)?;
        if rendered_after != rendered_sha256 || rendered_bytes_after != rendered_bytes {
            return Err(format!(
                "presentation {} render changed while it was being measured",
                presentation.id
            ));
        }
        if !measured.lufs.is_finite() || !measured.true_peak_db().is_finite() {
            return Err(format!(
                "presentation {} did not produce finite loudness and true-peak measurements",
                presentation.id
            ));
        }
        let drift = measured.lufs - presentation.loudness.dialnorm_lkfs;
        let dialnorm_passed = drift.abs() <= options.dialnorm_tolerance_lu;
        let true_peak_passed = options
            .max_true_peak_dbtp
            .map(|ceiling| measured.true_peak_db() <= ceiling);
        let passed = dialnorm_passed && true_peak_passed != Some(false);
        let mut checks = vec![Ac4Check {
            rule_id: "FORGE-AC4-DIALNORM-MATCH",
            standard: PART1_STANDARD,
            measured: drift.abs(),
            maximum: options.dialnorm_tolerance_lu,
            unit: "LU",
            passed: dialnorm_passed,
        }];
        if let Some(ceiling) = options.max_true_peak_dbtp {
            checks.push(Ac4Check {
                rule_id: "FORGE-AC4-TRUE-PEAK",
                standard: "ITU-R BS.1770-5",
                measured: measured.true_peak_db(),
                maximum: ceiling,
                unit: "dBTP",
                passed: true_peak_passed == Some(true),
            });
        }
        results.push(PresentationResult {
            id: presentation.id,
            presentation_version: presentation.presentation_version,
            output_layout: presentation.output_layout,
            language: presentation.language,
            accessibility: presentation.accessibility,
            loudness_metadata: presentation.loudness,
            rendered_sha256,
            rendered_bytes,
            sample_rate_hz: measured.sample_rate,
            channels: measured.channels,
            duration_seconds: measured.duration_secs(),
            measured_integrated_lufs: measured.lufs,
            measured_true_peak_dbtp: measured.true_peak_db(),
            dialnorm_drift_lu: drift,
            dialnorm_passed,
            true_peak_passed,
            passed,
            checks,
        });
    }
    let passed = results.iter().all(|value| value.passed);
    Ok(Ac4AdapterReport {
        schema: REPORT_SCHEMA,
        protocol_version: PROTOCOL_VERSION,
        validator: VALIDATOR,
        input_path: input.to_string_lossy().into_owned(),
        input_bytes,
        input_sha256,
        adapter_path: adapter.to_string_lossy().into_owned(),
        adapter_sha256,
        decoder: response.decoder,
        ac4_part1_standard: PART1_STANDARD,
        ac4_part2_standard: PART2_STANDARD,
        timeout_seconds: options.timeout_seconds,
        max_decoded_samples_per_presentation: options.max_decoded_samples_per_presentation,
        dialnorm_tolerance_lu: options.dialnorm_tolerance_lu,
        max_true_peak_dbtp: options.max_true_peak_dbtp,
        presentation_count: results.len(),
        passed,
        presentations: results,
    })
}

pub fn write_report(
    path: &Path,
    report: &Ac4AdapterReport,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "refusing to replace existing AC-4 report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize AC-4 adapter report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new(path)?;
    output.write_all(&bytes)?;
    output.commit()
}

fn validate_options(options: &AdapterOptions) -> Result<(), String> {
    if options.timeout_seconds == 0 || options.timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(format!(
            "adapter timeout must be 1..={MAX_TIMEOUT_SECONDS} seconds"
        ));
    }
    if options.max_decoded_samples_per_presentation == 0
        || options.max_decoded_samples_per_presentation > HARD_MAX_DECODED_SAMPLES
    {
        return Err(format!(
            "decoded sample limit must be 1..={HARD_MAX_DECODED_SAMPLES}"
        ));
    }
    if !options.dialnorm_tolerance_lu.is_finite()
        || !(0.0..=10.0).contains(&options.dialnorm_tolerance_lu)
    {
        return Err("dialnorm tolerance must be finite and between 0 and 10 LU".into());
    }
    if options
        .max_true_peak_dbtp
        .is_some_and(|value| !value.is_finite() || !(-100.0..=0.0).contains(&value))
    {
        return Err("true-peak ceiling must be finite and between -100 and 0 dBTP".into());
    }
    Ok(())
}

fn validate_response(response: &AdapterResponse, input_sha256: &str) -> Result<(), String> {
    if response.schema != RESPONSE_SCHEMA || response.protocol_version != PROTOCOL_VERSION {
        return Err("unsupported AC-4 adapter response schema or protocol version".into());
    }
    if !response.input_sha256.eq_ignore_ascii_case(input_sha256) {
        return Err("AC-4 adapter response is not bound to the requested input SHA-256".into());
    }
    if response.decoder.name.trim().is_empty() || response.decoder.version.trim().is_empty() {
        return Err("AC-4 decoder name and version are required".into());
    }
    if response.ac4_part1_standard != PART1_STANDARD
        || response.ac4_part2_standard != PART2_STANDARD
    {
        return Err("AC-4 adapter does not claim the required current ETSI standards".into());
    }
    if response.presentations.is_empty()
        || response.presentations.len() > MAX_PRESENTATIONS
        || response.presentation_count != response.presentations.len()
    {
        return Err(format!(
            "adapter must enumerate 1..={MAX_PRESENTATIONS} presentations and report the exact count"
        ));
    }
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    for item in &response.presentations {
        if !valid_id(&item.id) || !ids.insert(item.id.as_str()) {
            return Err(
                "presentation IDs must be unique 1..=64 character ASCII identifiers".into(),
            );
        }
        if item.presentation_version > 1 {
            return Err(format!("presentation {} has unsupported version", item.id));
        }
        if item.output_layout.trim().is_empty() || item.output_layout.len() > 64 {
            return Err(format!(
                "presentation {} has invalid output_layout",
                item.id
            ));
        }
        validate_relative_path(&item.rendered_path)?;
        if item
            .rendered_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("wav")
        {
            return Err(format!("presentation {} render must use .wav", item.id));
        }
        if !paths.insert(item.rendered_path.clone()) {
            return Err("each presentation must have a distinct rendered_path".into());
        }
        validate_loudness(item.presentation_version, &item.id, &item.loudness)?;
    }
    Ok(())
}

fn validate_loudness(
    presentation_version: u8,
    id: &str,
    value: &Ac4LoudnessMetadata,
) -> Result<(), String> {
    if value.dialnorm_bits > 127 {
        return Err(format!(
            "presentation {id} dialnorm_bits exceeds its 7-bit range"
        ));
    }
    let expected = -(f64::from(value.dialnorm_bits)) / 4.0;
    if !value.dialnorm_lkfs.is_finite() || (value.dialnorm_lkfs - expected).abs() > 1e-9 {
        return Err(format!(
            "presentation {id} dialnorm_lkfs does not equal -dialnorm_bits/4"
        ));
    }
    let source_valid = match presentation_version {
        1 => value.dialnorm_source == DialnormSource::PresentationSubstream,
        0 => matches!(
            value.dialnorm_source,
            DialnormSource::AssociatedBasicMetadata | DialnormSource::MainOrDialogueSubstream
        ),
        _ => false,
    };
    if !source_valid {
        return Err(format!(
            "presentation {id} dialnorm source is inconsistent with presentation_version"
        ));
    }
    for correction in [
        value.downmix_correction_db,
        value.alternative_presentation_correction_db,
        value.realtime_correction_db,
    ]
    .into_iter()
    .flatten()
    {
        if !correction.is_finite() || !(-64.0..=64.0).contains(&correction) {
            return Err(format!(
                "presentation {id} loudness correction is outside the bounded range"
            ));
        }
    }
    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("adapter rendered paths must be normalized relative paths".into());
    }
    Ok(())
}

fn resolve_render(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    validate_relative_path(relative)?;
    let path = fs::canonicalize(root.join(relative))
        .map_err(|error| format!("resolve adapter render {}: {error}", relative.display()))?;
    if !path.starts_with(root) {
        return Err(format!(
            "adapter render escapes its output directory: {}",
            relative.display()
        ));
    }
    ensure_regular_file(&path, "adapter render")?;
    Ok(path)
}

fn read_response(work: &Path, path: &Path) -> Result<Vec<u8>, String> {
    let resolved = fs::canonicalize(path)
        .map_err(|error| format!("resolve AC-4 adapter response: {error}"))?;
    let root =
        fs::canonicalize(work).map_err(|error| format!("resolve adapter workspace: {error}"))?;
    if !resolved.starts_with(root) {
        return Err("AC-4 adapter response escapes its workspace".into());
    }
    let metadata =
        fs::metadata(&resolved).map_err(|error| format!("stat AC-4 adapter response: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "AC-4 adapter response must be a regular file no larger than {MAX_RESPONSE_BYTES} bytes"
        ));
    }
    fs::read(&resolved).map_err(|error| format!("read AC-4 adapter response: {error}"))
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("stat {label}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{label} must be a regular file"));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "file length overflow".to_string())?;
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok((hex, bytes))
}

struct ToolOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

fn run_bounded(
    executable: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
) -> Result<ToolOutput, String> {
    let mut stdout_file =
        tempfile::tempfile().map_err(|error| format!("create stdout spool: {error}"))?;
    let mut stderr_file =
        tempfile::tempfile().map_err(|error| format!("create stderr spool: {error}"))?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            stdout_file
                .try_clone()
                .map_err(|error| format!("clone stdout spool: {error}"))?,
        ))
        .stderr(Stdio::from(
            stderr_file
                .try_clone()
                .map_err(|error| format!("clone stderr spool: {error}"))?,
        ))
        .spawn()
        .map_err(|error| format!("start AC-4 adapter {}: {error}", executable.display()))?;
    let started = Instant::now();
    let status = loop {
        let stdout_len = stdout_file
            .metadata()
            .map_err(|error| format!("stat adapter stdout: {error}"))?
            .len();
        let stderr_len = stderr_file
            .metadata()
            .map_err(|error| format!("stat adapter stderr: {error}"))?
            .len();
        if stdout_len > TOOL_OUTPUT_LIMIT as u64 || stderr_len > TOOL_OUTPUT_LIMIT as u64 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("AC-4 adapter output exceeded its 1 MiB safety limit".into());
        }
        match child
            .try_wait()
            .map_err(|error| format!("wait for AC-4 adapter: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "AC-4 adapter exceeded the {} second timeout",
                    timeout.as_secs()
                ));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let _ = read_bounded(&mut stdout_file, TOOL_OUTPUT_LIMIT, "stdout")?;
    let stderr = read_bounded(&mut stderr_file, TOOL_OUTPUT_LIMIT, "stderr")?;
    Ok(ToolOutput { status, stderr })
}

fn read_bounded(file: &mut File, limit: usize, label: &str) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek adapter {label}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read adapter {label}: {error}"))?;
    if bytes.len() > limit {
        return Err(format!("AC-4 adapter {label} exceeded its safety limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> AdapterResponse {
        AdapterResponse {
            schema: RESPONSE_SCHEMA.into(),
            protocol_version: 1,
            input_sha256: "a".repeat(64),
            decoder: DecoderEvidence {
                name: "licensed-reference".into(),
                version: "1.0".into(),
            },
            ac4_part1_standard: PART1_STANDARD.into(),
            ac4_part2_standard: PART2_STANDARD.into(),
            presentation_count: 1,
            presentations: vec![AdapterPresentation {
                id: "main-en".into(),
                presentation_version: 1,
                rendered_path: "main.wav".into(),
                output_layout: "stereo".into(),
                language: Some("en".into()),
                accessibility: None,
                loudness: Ac4LoudnessMetadata {
                    dialnorm_bits: 64,
                    dialnorm_lkfs: -16.0,
                    dialnorm_source: DialnormSource::PresentationSubstream,
                    downmix_correction_db: Some(0.0),
                    alternative_presentation_correction_db: None,
                    realtime_correction_db: None,
                },
            }],
        }
    }

    #[test]
    fn validates_current_ac4_presentation_metadata() {
        assert!(validate_response(&response(), &"a".repeat(64)).is_ok());
    }

    #[test]
    fn rejects_wrong_dialnorm_source_for_presentation_version() {
        let mut value = response();
        value.presentations[0].loudness.dialnorm_source = DialnormSource::AssociatedBasicMetadata;
        assert!(validate_response(&value, &"a".repeat(64))
            .unwrap_err()
            .contains("dialnorm source"));
    }

    #[test]
    fn rejects_inconsistent_dialnorm_code_and_level() {
        let mut value = response();
        value.presentations[0].loudness.dialnorm_lkfs = -24.0;
        assert!(validate_response(&value, &"a".repeat(64))
            .unwrap_err()
            .contains("dialnorm_lkfs"));
    }

    #[test]
    fn rejects_incomplete_or_duplicate_presentation_enumeration() {
        let mut value = response();
        value.presentation_count = 2;
        assert!(validate_response(&value, &"a".repeat(64)).is_err());
        let mut value = response();
        let mut duplicate = value.presentations[0].clone();
        duplicate.rendered_path = "other.wav".into();
        value.presentations.push(duplicate);
        value.presentation_count = 2;
        assert!(validate_response(&value, &"a".repeat(64)).is_err());
    }

    #[test]
    fn rejects_render_path_traversal() {
        let mut value = response();
        value.presentations[0].rendered_path = "../outside.wav".into();
        assert!(validate_response(&value, &"a".repeat(64)).is_err());
    }
}
