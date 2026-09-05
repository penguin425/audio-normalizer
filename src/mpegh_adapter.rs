//! Bounded protocol adapter for licensed/reference MPEG-H decoders.
//!
//! Forge does not ship an MPEG-H decoder. This module invokes an explicitly
//! selected adapter executable, binds the invocation to the input and adapter
//! bytes, validates presentation-level metadata, and independently measures
//! every rendered WAVE presentation with Forge's BS.1770 engine.

use crate::analysis;
use crate::channel_layout::{ChannelLayoutDescriptor, RendererBinding};
use crate::decoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-mpegh-reference-adapter-1";
pub const CORE_STANDARD: &str = "ISO/IEC 23008-3:2026";
pub const REFERENCE_SOFTWARE_STANDARD: &str = "ISO/IEC 23008-6:2025";
pub const CONFORMANCE_STANDARD: &str = "ISO/IEC 23008-9:2023";
pub const REQUEST_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-request-v1";
pub const RESPONSE_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-response-v1";
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-report-v1";
pub const REPORT_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-report-v2";

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_GROUPS: usize = 128;
const MAX_SWITCH_GROUPS: usize = 32;
const MAX_PRESETS: usize = 32;
const MAX_PRESENTATIONS: usize = MAX_PRESETS + 1;
const MAX_MHAS_PACKETS: u64 = 1_000_000;
const MAX_CONFIG_BYTES: u64 = 4_096;
const HARD_MAX_DECODED_SAMPLES: u64 = 200_000_000;
const MAX_TIMEOUT_SECONDS: u64 = 3_600;
const TOOL_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AdapterOptions {
    pub input: PathBuf,
    pub adapter: PathBuf,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub loudness_tolerance_lu: f64,
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
    mhas_inventory: MhasInventory,
    requirements: AdapterRequirements,
}

#[derive(Debug, Serialize)]
struct AdapterRequirements {
    enumerate_all_presentations: bool,
    enumerate_audio_scene: bool,
    rendered_format: &'static str,
    report_presentation_loudness_metadata: bool,
    loudness_normalization: bool,
    dynamic_range_control: bool,
    core_standard: &'static str,
    reference_software_standard: &'static str,
    conformance_standard: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterResponse {
    schema: String,
    protocol_version: u32,
    input_sha256: String,
    decoder: DecoderEvidence,
    core_standard: String,
    reference_software_standard: String,
    conformance_standard: String,
    mpegh3da_profile_level_indication: u8,
    scene: SceneMetadata,
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
    #[serde(default)]
    preset_id: Option<u8>,
    rendered_path: PathBuf,
    output_layout: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    accessibility: Option<String>,
    loudness: MpeghLoudnessMetadata,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MpeghLoudnessMetadata {
    pub loudness_info_type: u8,
    #[serde(default)]
    pub mae_group_id: Option<u8>,
    #[serde(default)]
    pub mae_group_preset_id: Option<u8>,
    pub method_definition: u8,
    pub program_loudness_lkfs: f64,
    pub drc_set_id: u8,
    pub downmix_id: u8,
    #[serde(default)]
    pub measurement_system: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SceneMetadata {
    pub group_count: usize,
    pub groups: Vec<SceneGroup>,
    pub switch_group_count: usize,
    pub switch_groups: Vec<SwitchGroup>,
    pub preset_count: usize,
    pub presets: Vec<ScenePreset>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SceneGroup {
    pub id: u8,
    pub signal_kind: SignalKind,
    pub allow_on_off: bool,
    pub default_on: bool,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub content_kind: Option<String>,
    pub allow_gain_interactivity: bool,
    #[serde(default)]
    pub min_gain_db: Option<f64>,
    #[serde(default)]
    pub max_gain_db: Option<f64>,
    pub allow_position_interactivity: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SignalKind {
    Channels,
    Objects,
    Hoa,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchGroup {
    pub id: u8,
    pub group_ids: Vec<u8>,
    pub default_group_id: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScenePreset {
    pub id: u8,
    #[serde(default)]
    pub name: Option<String>,
    pub kind: u8,
    pub group_ids: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MhasInventory {
    pub packet_count: u64,
    pub payload_bytes: u64,
    pub packet_types: Vec<MhasPacketTypeCount>,
    pub labels: Vec<u64>,
    pub configuration_count: u64,
    pub frame_count: u64,
    pub audio_scene_info_count: u64,
    pub loudness_drc_count: u64,
    pub loudness_count: u64,
    pub sync_count: u64,
    pub profile_levels: Vec<ProfileLevel>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MhasPacketTypeCount {
    pub packet_type: u64,
    pub name: &'static str,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProfileLevel {
    pub indication: u8,
    pub profile: &'static str,
    pub level: u8,
}

#[derive(Debug, Serialize)]
pub struct MpeghAdapterReport {
    pub schema: &'static str,
    pub protocol_version: u32,
    pub validator: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub adapter_path: String,
    pub adapter_sha256: String,
    pub decoder: DecoderEvidence,
    pub core_standard: &'static str,
    pub reference_software_standard: &'static str,
    pub conformance_standard: &'static str,
    pub mhas_inventory: MhasInventory,
    pub profile_level: ProfileLevel,
    pub scene: SceneMetadata,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub loudness_tolerance_lu: f64,
    pub max_true_peak_dbtp: Option<f64>,
    pub presentation_count: usize,
    pub passed: bool,
    pub presentations: Vec<PresentationResult>,
}

#[derive(Debug, Serialize)]
pub struct PresentationResult {
    pub id: String,
    pub preset_id: Option<u8>,
    pub output_layout: String,
    pub language: Option<String>,
    pub accessibility: Option<String>,
    pub loudness_metadata: MpeghLoudnessMetadata,
    pub rendered_sha256: String,
    pub rendered_bytes: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub measured_integrated_lufs: f64,
    pub measured_true_peak_dbtp: f64,
    pub loudness_drift_lu: f64,
    pub loudness_passed: bool,
    pub true_peak_passed: Option<bool>,
    pub passed: bool,
    pub checks: Vec<MpeghCheck>,
}

#[non_exhaustive]
#[derive(Debug, Serialize)]
pub struct MpeghAdapterReportV2 {
    pub schema: &'static str,
    pub protocol_version: u32,
    pub validator: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub adapter_path: String,
    pub adapter_sha256: String,
    pub decoder: DecoderEvidence,
    pub core_standard: &'static str,
    pub reference_software_standard: &'static str,
    pub conformance_standard: &'static str,
    pub mhas_inventory: MhasInventory,
    pub profile_level: ProfileLevel,
    pub scene: SceneMetadata,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub loudness_tolerance_lu: f64,
    pub max_true_peak_dbtp: Option<f64>,
    pub presentation_count: usize,
    pub passed: bool,
    pub presentations: Vec<PresentationResultV2>,
}

#[non_exhaustive]
#[derive(Debug, Serialize)]
pub struct PresentationResultV2 {
    pub id: String,
    pub preset_id: Option<u8>,
    pub output_layout: String,
    pub channel_layout: ChannelLayoutDescriptor,
    pub language: Option<String>,
    pub accessibility: Option<String>,
    pub loudness_metadata: MpeghLoudnessMetadata,
    pub rendered_sha256: String,
    pub rendered_bytes: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub measured_integrated_lufs: f64,
    pub measured_true_peak_dbtp: f64,
    pub loudness_drift_lu: f64,
    pub loudness_passed: bool,
    pub true_peak_passed: Option<bool>,
    pub passed: bool,
    pub checks: Vec<MpeghCheck>,
}

impl From<PresentationResultV2> for PresentationResult {
    fn from(value: PresentationResultV2) -> Self {
        Self {
            id: value.id,
            preset_id: value.preset_id,
            output_layout: value.output_layout,
            language: value.language,
            accessibility: value.accessibility,
            loudness_metadata: value.loudness_metadata,
            rendered_sha256: value.rendered_sha256,
            rendered_bytes: value.rendered_bytes,
            sample_rate_hz: value.sample_rate_hz,
            channels: value.channels,
            duration_seconds: value.duration_seconds,
            measured_integrated_lufs: value.measured_integrated_lufs,
            measured_true_peak_dbtp: value.measured_true_peak_dbtp,
            loudness_drift_lu: value.loudness_drift_lu,
            loudness_passed: value.loudness_passed,
            true_peak_passed: value.true_peak_passed,
            passed: value.passed,
            checks: value.checks,
        }
    }
}

impl From<MpeghAdapterReportV2> for MpeghAdapterReport {
    fn from(value: MpeghAdapterReportV2) -> Self {
        Self {
            schema: REPORT_SCHEMA,
            protocol_version: value.protocol_version,
            validator: value.validator,
            input_path: value.input_path,
            input_bytes: value.input_bytes,
            input_sha256: value.input_sha256,
            adapter_path: value.adapter_path,
            adapter_sha256: value.adapter_sha256,
            decoder: value.decoder,
            core_standard: value.core_standard,
            reference_software_standard: value.reference_software_standard,
            conformance_standard: value.conformance_standard,
            mhas_inventory: value.mhas_inventory,
            profile_level: value.profile_level,
            scene: value.scene,
            timeout_seconds: value.timeout_seconds,
            max_decoded_samples_per_presentation: value.max_decoded_samples_per_presentation,
            loudness_tolerance_lu: value.loudness_tolerance_lu,
            max_true_peak_dbtp: value.max_true_peak_dbtp,
            presentation_count: value.presentation_count,
            passed: value.passed,
            presentations: value.presentations.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MpeghCheck {
    pub rule_id: &'static str,
    pub standard: &'static str,
    pub measured: f64,
    pub maximum: f64,
    pub unit: &'static str,
    pub passed: bool,
}

pub fn run(options: &AdapterOptions) -> Result<MpeghAdapterReport, String> {
    run_v2(options).map(Into::into)
}

pub fn run_v2(options: &AdapterOptions) -> Result<MpeghAdapterReportV2, String> {
    validate_options(options)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve MPEG-H input {}: {error}", options.input.display()))?;
    let adapter = fs::canonicalize(&options.adapter).map_err(|error| {
        format!(
            "resolve MPEG-H adapter {}: {error}",
            options.adapter.display()
        )
    })?;
    ensure_regular_file(&input, "MPEG-H input")?;
    ensure_regular_file(&adapter, "MPEG-H adapter")?;
    let (input_sha256, input_bytes) = sha256_file(&input)?;
    let (adapter_sha256, _) = sha256_file(&adapter)?;
    let mhas_inventory = inspect_mhas(&input)?;

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
        mhas_inventory: mhas_inventory.clone(),
        requirements: AdapterRequirements {
            enumerate_all_presentations: true,
            enumerate_audio_scene: true,
            rendered_format: "wave",
            report_presentation_loudness_metadata: true,
            loudness_normalization: false,
            dynamic_range_control: false,
            core_standard: CORE_STANDARD,
            reference_software_standard: REFERENCE_SOFTWARE_STANDARD,
            conformance_standard: CONFORMANCE_STANDARD,
        },
    };
    let mut request_bytes = serde_json::to_vec_pretty(&request)
        .map_err(|error| format!("serialize MPEG-H adapter request: {error}"))?;
    request_bytes.push(b'\n');
    fs::write(&request_path, request_bytes)
        .map_err(|error| format!("write MPEG-H adapter request: {error}"))?;

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
            "MPEG-H adapter failed ({}): {}",
            tool.status,
            String::from_utf8_lossy(&tool.stderr).trim()
        ));
    }
    let (adapter_after, _) = sha256_file(&adapter)?;
    if adapter_after != adapter_sha256 {
        return Err("MPEG-H adapter executable changed while it was running".into());
    }
    let response_bytes = read_response(work.path(), &response_path)?;
    let settings_sha256 = sha256_bytes(&response_bytes);
    let response: AdapterResponse = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("parse MPEG-H adapter response: {error}"))?;
    validate_response(&response, &input_sha256, &mhas_inventory)?;
    let profile_level = profile_level(response.mpegh3da_profile_level_indication)?;

    let (input_after, bytes_after) = sha256_file(&input)?;
    if input_after != input_sha256 || bytes_after != input_bytes {
        return Err("MPEG-H input changed while the decoder adapter was running".into());
    }

    let render_root = fs::canonicalize(&renders)
        .map_err(|error| format!("resolve adapter render directory: {error}"))?;
    let mut results = Vec::with_capacity(response.presentations.len());
    for presentation in response.presentations {
        let rendered = resolve_render(&render_root, &presentation.rendered_path)?;
        let (rendered_sha256, rendered_bytes) = sha256_file(&rendered)?;
        let (mut buffer, decoded_layout) = decoder::decode_limited_with_channel_layout(
            &rendered,
            options.max_decoded_samples_per_presentation,
        )?;
        apply_output_layout(
            &rendered,
            &presentation.id,
            &presentation.output_layout,
            &mut buffer,
            decoded_layout.provenance(),
        )?;
        let renderer = RendererBinding::new(
            &response.decoder.name,
            &response.decoder.version,
            &presentation.output_layout,
            &adapter_sha256,
            &settings_sha256,
        )?;
        let assignments =
            match decoded_layout.assignments_compatible_with_roles(&buffer.channel_roles) {
                Some(assignments) => assignments,
                None => ChannelLayoutDescriptor::from_channel_roles(buffer.channel_roles.clone())?
                    .assignments()
                    .to_vec(),
            };
        let channel_layout = ChannelLayoutDescriptor::rendered(assignments, renderer)?;
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
        let drift = measured.lufs - presentation.loudness.program_loudness_lkfs;
        let loudness_passed = drift.abs() <= options.loudness_tolerance_lu;
        let true_peak_passed = options
            .max_true_peak_dbtp
            .map(|ceiling| measured.true_peak_db() <= ceiling);
        let passed = loudness_passed && true_peak_passed != Some(false);
        let mut checks = vec![MpeghCheck {
            rule_id: "FORGE-MPEGH-PROGRAM-LOUDNESS-MATCH",
            standard: CORE_STANDARD,
            measured: drift.abs(),
            maximum: options.loudness_tolerance_lu,
            unit: "LU",
            passed: loudness_passed,
        }];
        if let Some(ceiling) = options.max_true_peak_dbtp {
            checks.push(MpeghCheck {
                rule_id: "FORGE-MPEGH-TRUE-PEAK",
                standard: "ITU-R BS.1770-5",
                measured: measured.true_peak_db(),
                maximum: ceiling,
                unit: "dBTP",
                passed: true_peak_passed == Some(true),
            });
        }
        results.push(PresentationResultV2 {
            id: presentation.id,
            preset_id: presentation.preset_id,
            output_layout: presentation.output_layout,
            channel_layout,
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
            loudness_drift_lu: drift,
            loudness_passed,
            true_peak_passed,
            passed,
            checks,
        });
    }
    let passed = results.iter().all(|value| value.passed);
    Ok(MpeghAdapterReportV2 {
        schema: REPORT_SCHEMA_V2,
        protocol_version: PROTOCOL_VERSION,
        validator: VALIDATOR,
        input_path: input.to_string_lossy().into_owned(),
        input_bytes,
        input_sha256,
        adapter_path: adapter.to_string_lossy().into_owned(),
        adapter_sha256,
        decoder: response.decoder,
        core_standard: CORE_STANDARD,
        reference_software_standard: REFERENCE_SOFTWARE_STANDARD,
        conformance_standard: CONFORMANCE_STANDARD,
        mhas_inventory,
        profile_level,
        scene: response.scene,
        timeout_seconds: options.timeout_seconds,
        max_decoded_samples_per_presentation: options.max_decoded_samples_per_presentation,
        loudness_tolerance_lu: options.loudness_tolerance_lu,
        max_true_peak_dbtp: options.max_true_peak_dbtp,
        presentation_count: results.len(),
        passed,
        presentations: results,
    })
}

fn apply_output_layout(
    rendered: &Path,
    presentation_id: &str,
    output_layout: &str,
    buffer: &mut crate::wav::AudioBuffer,
    layout_provenance: decoder::ChannelLayoutProvenance,
) -> Result<(), String> {
    let decoded_channels = usize::from(buffer.channels);
    if buffer.data.len() != decoded_channels || buffer.channel_roles.len() != decoded_channels {
        return Err(format!(
            "presentation {presentation_id} render has inconsistent decoded channel metadata"
        ));
    }

    let declared_roles = crate::wav::named_channel_layout(output_layout);
    if let Some(roles) = declared_roles.as_deref() {
        if roles.len() != decoded_channels {
            return Err(format!(
                "presentation {presentation_id} output_layout {output_layout} declares {} channels but the render decoded as {}",
                roles.len(),
                buffer.channels
            ));
        }
        if layout_provenance == decoder::ChannelLayoutProvenance::KnownSpeakers {
            let decoded_form = crate::wav::writer::persisted_channel_roles(roles).map_err(|error| {
                format!(
                    "presentation {presentation_id} output_layout {output_layout} cannot be represented as WAVE: {error}"
                )
            })?;
            if buffer.channel_roles != decoded_form {
                return Err(format!(
                    "presentation {presentation_id} output_layout {output_layout} conflicts with the render's declared speaker layout"
                ));
            }
        }
    }

    buffer.channel_roles = crate::normalize::resolve_decoded_channel_roles(
        rendered,
        buffer.channels,
        &buffer.channel_roles,
        layout_provenance,
        declared_roles.as_deref(),
    )?;
    Ok(())
}

pub fn write_report(
    path: &Path,
    report: &MpeghAdapterReport,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    write_report_value(path, report, compact, overwrite)
}

pub fn write_report_v2(
    path: &Path,
    report: &MpeghAdapterReportV2,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    write_report_value(path, report, compact, overwrite)
}

fn write_report_value<T: Serialize>(
    path: &Path,
    report: &T,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "refusing to replace existing MPEG-H report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize MPEG-H adapter report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new_with_overwrite(path, overwrite)?;
    output.write_all(&bytes)?;
    output.commit()
}

/// Parse and structurally validate a raw MPEG-H Audio Stream (MHAS).
///
/// Forge owns this framing check. The external decoder is only trusted for
/// normative payload interpretation and rendering.
pub fn inspect_mhas(path: &Path) -> Result<MhasInventory, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open MHAS stream {}: {error}", path.display()))?;
    let file_bytes = file
        .metadata()
        .map_err(|error| format!("stat MHAS stream {}: {error}", path.display()))?
        .len();
    if file_bytes == 0 {
        return Err("MHAS stream is empty".into());
    }
    let mut reader = BitFileReader::new(&mut file);
    let mut packet_types = BTreeMap::<u64, u64>::new();
    let mut labels = BTreeSet::new();
    let mut configured_labels = BTreeSet::new();
    let mut profile_indications = BTreeSet::new();
    let mut packet_count = 0_u64;
    let mut payload_bytes = 0_u64;

    while reader.byte_position()? < file_bytes {
        packet_count += 1;
        if packet_count > MAX_MHAS_PACKETS {
            return Err(format!(
                "MHAS packet count exceeds the {MAX_MHAS_PACKETS} packet safety limit"
            ));
        }
        let packet_type = reader.read_escaped(3, 8, 8, packet_count, "type")?;
        let label = reader.read_escaped(2, 8, 32, packet_count, "label")?;
        let length = reader.read_escaped(11, 24, 24, packet_count, "length")?;
        if !reader.is_byte_aligned() {
            return Err(format!(
                "MHAS packet {packet_count} header is not byte-aligned"
            ));
        }
        let payload_offset = reader.byte_position()?;
        let payload_end = payload_offset
            .checked_add(length)
            .ok_or_else(|| format!("MHAS packet {packet_count} payload offset overflow"))?;
        if payload_end > file_bytes {
            return Err(format!(
                "MHAS packet {packet_count} type {packet_type} declares {length} payload bytes, but only {} remain",
                file_bytes.saturating_sub(payload_offset)
            ));
        }

        *packet_types.entry(packet_type).or_default() += 1;
        labels.insert(label);
        payload_bytes = payload_bytes
            .checked_add(length)
            .ok_or_else(|| "MHAS payload byte count overflow".to_string())?;

        match packet_type {
            1 => {
                if label == 0 {
                    return Err(format!(
                        "MHAS configuration packet {packet_count} must use a non-zero label"
                    ));
                }
                if length == 0 || length > MAX_CONFIG_BYTES {
                    return Err(format!(
                        "MHAS configuration packet {packet_count} must contain 1..={MAX_CONFIG_BYTES} bytes"
                    ));
                }
                let indication = reader.read_payload_byte(packet_count)?;
                profile_level(indication)?;
                profile_indications.insert(indication);
                configured_labels.insert(label);
                reader.skip_payload(length - 1, packet_count)?;
            }
            2 => {
                if length == 0 {
                    return Err(format!("MHAS audio frame packet {packet_count} is empty"));
                }
                if !configured_labels.contains(&label) {
                    return Err(format!(
                        "MHAS audio frame packet {packet_count} label {label} has no preceding configuration"
                    ));
                }
                reader.skip_payload(length, packet_count)?;
            }
            6 => {
                if label != 0 || length != 1 || reader.read_payload_byte(packet_count)? != 0xA5 {
                    return Err(format!(
                        "MHAS SYNC packet {packet_count} must have label 0 and the one-byte sync word 0xA5"
                    ));
                }
            }
            _ => reader.skip_payload(length, packet_count)?,
        }
    }
    if !reader.is_byte_aligned() {
        return Err("MHAS stream ends in a partial header byte".into());
    }
    let configuration_count = packet_types.get(&1).copied().unwrap_or(0);
    let frame_count = packet_types.get(&2).copied().unwrap_or(0);
    if configuration_count == 0 || frame_count == 0 {
        return Err("MHAS stream must contain configuration and audio-frame packets".into());
    }
    let profile_levels = profile_indications
        .into_iter()
        .map(profile_level)
        .collect::<Result<Vec<_>, _>>()?;
    let packet_types: Vec<_> = packet_types
        .into_iter()
        .map(|(packet_type, count)| MhasPacketTypeCount {
            packet_type,
            name: mhas_packet_type_name(packet_type),
            count,
        })
        .collect();
    Ok(MhasInventory {
        packet_count,
        payload_bytes,
        audio_scene_info_count: packet_count_for(&packet_types, 3),
        loudness_drc_count: packet_count_for(&packet_types, 13),
        loudness_count: packet_count_for(&packet_types, 22),
        sync_count: packet_count_for(&packet_types, 6),
        packet_types,
        labels: labels.into_iter().collect(),
        configuration_count,
        frame_count,
        profile_levels,
    })
}

fn packet_count_for(types: &[MhasPacketTypeCount], packet_type: u64) -> u64 {
    types
        .iter()
        .find(|entry| entry.packet_type == packet_type)
        .map_or(0, |entry| entry.count)
}

fn profile_level(indication: u8) -> Result<ProfileLevel, String> {
    let (profile, level) = match indication {
        0x01..=0x05 => ("main", indication),
        0x06..=0x0A => ("high", indication - 0x05),
        0x0B..=0x0F => ("low-complexity", indication - 0x0A),
        0x10..=0x14 => ("baseline", indication - 0x0F),
        _ => {
            return Err(format!(
                "reserved or unsupported mpegh3daProfileLevelIndication 0x{indication:02X}"
            ));
        }
    };
    Ok(ProfileLevel {
        indication,
        profile,
        level,
    })
}

fn mhas_packet_type_name(packet_type: u64) -> &'static str {
    match packet_type {
        0 => "fill-data",
        1 => "mpeg-h-3da-config",
        2 => "mpeg-h-3da-frame",
        3 => "audio-scene-info",
        4 | 5 | 18 => "reserved",
        6 => "sync",
        7 => "sync-gap",
        8 => "marker",
        9 => "crc16",
        10 => "crc32",
        11 => "descriptor",
        12 => "user-interaction",
        13 => "loudness-drc",
        14 => "buffer-info",
        15 => "global-crc16",
        16 => "global-crc32",
        17 => "audio-truncation",
        19 => "earcon",
        20 => "pcm-config",
        21 => "pcm-data",
        22 => "loudness",
        129 => "frame-length",
        _ => "unknown",
    }
}

struct BitFileReader<'a> {
    file: &'a mut File,
    current: u8,
    remaining: u8,
}

impl<'a> BitFileReader<'a> {
    fn new(file: &'a mut File) -> Self {
        Self {
            file,
            current: 0,
            remaining: 0,
        }
    }

    fn is_byte_aligned(&self) -> bool {
        self.remaining == 0
    }

    fn byte_position(&mut self) -> Result<u64, String> {
        if !self.is_byte_aligned() {
            return Err("internal MHAS parser position requested mid-byte".into());
        }
        self.file
            .stream_position()
            .map_err(|error| format!("read MHAS stream position: {error}"))
    }

    fn read_escaped(
        &mut self,
        first_bits: u8,
        second_bits: u8,
        third_bits: u8,
        packet: u64,
        field: &str,
    ) -> Result<u64, String> {
        let first = self.read_bits(first_bits, packet, field)?;
        let first_max = (1_u64 << first_bits) - 1;
        if first != first_max {
            return Ok(first);
        }
        let second = self.read_bits(second_bits, packet, field)?;
        let second_max = (1_u64 << second_bits) - 1;
        if second != second_max {
            return Ok(first + second);
        }
        Ok(first + second + self.read_bits(third_bits, packet, field)?)
    }

    fn read_bits(&mut self, count: u8, packet: u64, field: &str) -> Result<u64, String> {
        let mut value = 0_u64;
        for _ in 0..count {
            if self.remaining == 0 {
                let mut byte = [0_u8; 1];
                self.file.read_exact(&mut byte).map_err(|error| {
                    format!("read MHAS packet {packet} {field}: truncated header: {error}")
                })?;
                self.current = byte[0];
                self.remaining = 8;
            }
            self.remaining -= 1;
            value = (value << 1) | u64::from((self.current >> self.remaining) & 1);
        }
        Ok(value)
    }

    fn read_payload_byte(&mut self, packet: u64) -> Result<u8, String> {
        if !self.is_byte_aligned() {
            return Err("internal MHAS parser payload read is not byte-aligned".into());
        }
        let mut byte = [0_u8; 1];
        self.file
            .read_exact(&mut byte)
            .map_err(|error| format!("read MHAS packet {packet} payload: {error}"))?;
        Ok(byte[0])
    }

    fn skip_payload(&mut self, bytes: u64, packet: u64) -> Result<(), String> {
        if !self.is_byte_aligned() {
            return Err("internal MHAS parser payload skip is not byte-aligned".into());
        }
        let distance = i64::try_from(bytes)
            .map_err(|_| format!("MHAS packet {packet} payload is too large to seek"))?;
        self.file
            .seek(SeekFrom::Current(distance))
            .map_err(|error| format!("skip MHAS packet {packet} payload: {error}"))?;
        Ok(())
    }
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
    if !options.loudness_tolerance_lu.is_finite()
        || !(0.0..=10.0).contains(&options.loudness_tolerance_lu)
    {
        return Err("loudness tolerance must be finite and between 0 and 10 LU".into());
    }
    if options
        .max_true_peak_dbtp
        .is_some_and(|value| !value.is_finite() || !(-100.0..=0.0).contains(&value))
    {
        return Err("true-peak ceiling must be finite and between -100 and 0 dBTP".into());
    }
    Ok(())
}

fn validate_response(
    response: &AdapterResponse,
    input_sha256: &str,
    inventory: &MhasInventory,
) -> Result<(), String> {
    if response.schema != RESPONSE_SCHEMA || response.protocol_version != PROTOCOL_VERSION {
        return Err("unsupported MPEG-H adapter response schema or protocol version".into());
    }
    if !response.input_sha256.eq_ignore_ascii_case(input_sha256) {
        return Err("MPEG-H adapter response is not bound to the requested input SHA-256".into());
    }
    if !valid_text(&response.decoder.name, 128) || !valid_text(&response.decoder.version, 128) {
        return Err("MPEG-H decoder name and version are required".into());
    }
    if response.core_standard != CORE_STANDARD
        || response.reference_software_standard != REFERENCE_SOFTWARE_STANDARD
        || response.conformance_standard != CONFORMANCE_STANDARD
    {
        return Err("MPEG-H adapter does not claim the required current ISO standards".into());
    }
    if !inventory
        .profile_levels
        .iter()
        .any(|value| value.indication == response.mpegh3da_profile_level_indication)
    {
        return Err(
            "adapter profile-level indication does not match an MHAS configuration packet".into(),
        );
    }
    if response.presentations.is_empty()
        || response.presentations.len() > MAX_PRESENTATIONS
        || response.presentation_count != response.presentations.len()
    {
        return Err(format!(
            "adapter must enumerate 1..={MAX_PRESENTATIONS} presentations and report the exact count"
        ));
    }
    validate_scene(&response.scene)?;
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let preset_ids: HashSet<u8> = response
        .scene
        .presets
        .iter()
        .map(|value| value.id)
        .collect();
    let mut rendered_presets = HashSet::new();
    let mut default_presentations = 0_usize;
    for item in &response.presentations {
        if !valid_id(&item.id) || !ids.insert(item.id.as_str()) {
            return Err(
                "presentation IDs must be unique 1..=64 character ASCII identifiers".into(),
            );
        }
        match item.preset_id {
            Some(id) if !preset_ids.contains(&id) => {
                return Err(format!(
                    "presentation {} references unknown preset {id}",
                    item.id
                ));
            }
            Some(id) if !rendered_presets.insert(id) => {
                return Err(format!("preset {id} is rendered more than once"));
            }
            Some(_) => {}
            None => default_presentations += 1,
        }
        if !valid_text(&item.output_layout, 64)
            || item
                .language
                .as_deref()
                .is_some_and(|text| !valid_text(text, 35))
            || item
                .accessibility
                .as_deref()
                .is_some_and(|text| !valid_text(text, 64))
        {
            return Err(format!(
                "presentation {} has invalid layout, language, or accessibility metadata",
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
        validate_loudness(&item.id, item.preset_id, &item.loudness)?;
    }
    if default_presentations != 1 || rendered_presets != preset_ids {
        return Err(
            "adapter must render the default scene and every scene preset exactly once".into(),
        );
    }
    Ok(())
}

fn validate_loudness(
    id: &str,
    preset_id: Option<u8>,
    value: &MpeghLoudnessMetadata,
) -> Result<(), String> {
    match value.loudness_info_type {
        0 if value.mae_group_id.is_none() && value.mae_group_preset_id.is_none() => {}
        3 if preset_id.is_some()
            && value.mae_group_id.is_none()
            && value.mae_group_preset_id == preset_id => {}
        _ => {
            return Err(format!(
                "presentation {id} loudnessInfoType and MAE reference do not describe the rendered programme or preset"
            ));
        }
    }
    if !matches!(value.method_definition, 1 | 2) {
        return Err(format!(
            "presentation {id} loudness method_definition must be 1 or 2"
        ));
    }
    if !value.program_loudness_lkfs.is_finite()
        || !(-70.0..=0.0).contains(&value.program_loudness_lkfs)
    {
        return Err(format!(
            "presentation {id} program loudness must be finite and between -70 and 0 LKFS"
        ));
    }
    if value.drc_set_id != 0 || value.downmix_id != 0 {
        return Err(format!(
            "presentation {id} must report the programme loudness entry for drcSetId=0 and downmixId=0"
        ));
    }
    if value
        .measurement_system
        .as_deref()
        .is_some_and(|text| !valid_text(text, 64))
    {
        return Err(format!("presentation {id} has invalid measurement_system"));
    }
    Ok(())
}

fn validate_scene(scene: &SceneMetadata) -> Result<(), String> {
    if scene.group_count != scene.groups.len()
        || scene.switch_group_count != scene.switch_groups.len()
        || scene.preset_count != scene.presets.len()
    {
        return Err("scene item counts do not match their arrays".into());
    }
    if scene.groups.len() > MAX_GROUPS
        || scene.switch_groups.len() > MAX_SWITCH_GROUPS
        || scene.presets.len() > MAX_PRESETS
    {
        return Err(format!(
            "scene exceeds the {MAX_GROUPS} group, {MAX_SWITCH_GROUPS} switch-group, or {MAX_PRESETS} preset limit"
        ));
    }
    let mut group_ids = HashSet::new();
    for group in &scene.groups {
        if group.id > 127 || !group_ids.insert(group.id) {
            return Err(format!("duplicate scene group ID {}", group.id));
        }
        if group
            .language
            .as_deref()
            .is_some_and(|text| !valid_text(text, 35))
            || group
                .content_kind
                .as_deref()
                .is_some_and(|text| !valid_text(text, 64))
        {
            return Err(format!(
                "scene group {} has invalid text metadata",
                group.id
            ));
        }
        match (group.min_gain_db, group.max_gain_db) {
            (Some(minimum), Some(maximum))
                if group.allow_gain_interactivity
                    && minimum.is_finite()
                    && maximum.is_finite()
                    && (-128.0..=0.0).contains(&minimum)
                    && (0.0..=128.0).contains(&maximum) => {}
            (None, None) if !group.allow_gain_interactivity => {}
            _ => {
                return Err(format!(
                    "scene group {} has inconsistent gain-interactivity bounds",
                    group.id
                ));
            }
        }
    }
    let mut switch_ids = HashSet::new();
    for switch in &scene.switch_groups {
        if switch.id > 31
            || !switch_ids.insert(switch.id)
            || switch.group_ids.is_empty()
            || switch.group_ids.len() > MAX_GROUPS
            || switch.group_ids.iter().any(|id| !group_ids.contains(id))
            || !switch.group_ids.contains(&switch.default_group_id)
        {
            return Err(format!("switch group {} has invalid references", switch.id));
        }
        let unique: HashSet<_> = switch.group_ids.iter().collect();
        if unique.len() != switch.group_ids.len() {
            return Err(format!("switch group {} repeats a group ID", switch.id));
        }
    }
    let mut preset_ids = HashSet::new();
    for preset in &scene.presets {
        if preset.id > 31
            || preset.kind > 31
            || !preset_ids.insert(preset.id)
            || preset.group_ids.is_empty()
            || preset.group_ids.len() > MAX_GROUPS
            || preset.group_ids.iter().any(|id| !group_ids.contains(id))
            || preset
                .name
                .as_deref()
                .is_some_and(|text| !valid_text(text, 128))
        {
            return Err(format!("scene preset {} is invalid", preset.id));
        }
        let unique: HashSet<_> = preset.group_ids.iter().collect();
        if unique.len() != preset.group_ids.len() {
            return Err(format!("scene preset {} repeats a group ID", preset.id));
        }
    }
    Ok(())
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
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
        .map_err(|error| format!("resolve MPEG-H adapter response: {error}"))?;
    let root =
        fs::canonicalize(work).map_err(|error| format!("resolve adapter workspace: {error}"))?;
    if !resolved.starts_with(root) {
        return Err("MPEG-H adapter response escapes its workspace".into());
    }
    let metadata = fs::metadata(&resolved)
        .map_err(|error| format!("stat MPEG-H adapter response: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "MPEG-H adapter response must be a regular file no larger than {MAX_RESPONSE_BYTES} bytes"
        ));
    }
    fs::read(&resolved).map_err(|error| format!("read MPEG-H adapter response: {error}"))
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("stat {label}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{label} must be a regular file"));
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
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
        .map_err(|error| format!("start MPEG-H adapter {}: {error}", executable.display()))?;
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
            return Err("MPEG-H adapter output exceeded its 1 MiB safety limit".into());
        }
        match child
            .try_wait()
            .map_err(|error| format!("wait for MPEG-H adapter: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "MPEG-H adapter exceeded the {} second timeout",
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
        return Err(format!("MPEG-H adapter {label} exceeded its safety limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{
        default_channel_roles, named_channel_layout, AudioBuffer, ChannelRole, PcmKind,
    };

    fn rendered_buffer(roles: Vec<ChannelRole>) -> AudioBuffer {
        let channels = u16::try_from(roles.len()).unwrap();
        AudioBuffer {
            sample_rate: 48_000,
            channels,
            frames: 4,
            data: vec![vec![0.0; 4]; usize::from(channels)],
            channel_roles: roles,
            source_kind: PcmKind::S16,
        }
    }

    #[test]
    fn declared_5_1_layout_resolves_a_maskless_render() {
        let mut buffer = rendered_buffer(default_channel_roles(6));
        apply_output_layout(
            Path::new("render.wav"),
            "default",
            "5.1",
            &mut buffer,
            decoder::ChannelLayoutProvenance::Unknown,
        )
        .unwrap();
        assert_eq!(buffer.channel_roles, named_channel_layout("5.1").unwrap());
    }

    #[test]
    fn output_layout_must_match_decoded_channel_count() {
        let mut buffer = rendered_buffer(default_channel_roles(2));
        let error = apply_output_layout(
            Path::new("render.wav"),
            "default",
            "5.1",
            &mut buffer,
            decoder::ChannelLayoutProvenance::KnownSpeakers,
        )
        .unwrap_err();
        assert!(error.contains("declares 6 channels"));
        assert!(error.contains("decoded as 2"));
    }

    #[test]
    fn unknown_output_layout_requires_known_decoder_speakers() {
        for provenance in [
            decoder::ChannelLayoutProvenance::Unknown,
            decoder::ChannelLayoutProvenance::SceneBased,
        ] {
            let mut buffer = rendered_buffer(default_channel_roles(6));
            assert!(apply_output_layout(
                Path::new("render.wav"),
                "default",
                "vendor-layout",
                &mut buffer,
                provenance,
            )
            .is_err());
        }
    }

    #[test]
    fn stereo_layout_accepts_matching_known_decoder_speakers() {
        let mut buffer = rendered_buffer(default_channel_roles(2));
        apply_output_layout(
            Path::new("render.wav"),
            "default",
            "stereo",
            &mut buffer,
            decoder::ChannelLayoutProvenance::KnownSpeakers,
        )
        .unwrap();
        assert_eq!(buffer.channel_roles, default_channel_roles(2));
    }

    #[test]
    fn known_decoder_speakers_must_match_declared_layout() {
        let mut roles = default_channel_roles(6);
        roles[3] = ChannelRole::Main;
        let mut buffer = rendered_buffer(roles);
        let error = apply_output_layout(
            Path::new("render.wav"),
            "default",
            "5.1",
            &mut buffer,
            decoder::ChannelLayoutProvenance::KnownSpeakers,
        )
        .unwrap_err();
        assert!(error.contains("conflicts with the render's declared speaker layout"));
    }

    #[test]
    fn immersive_wave_positions_match_the_named_layout() {
        let declared = named_channel_layout("5.1.4").unwrap();
        let decoded = crate::wav::writer::persisted_channel_roles(&declared).unwrap();
        let mut buffer = rendered_buffer(decoded);
        apply_output_layout(
            Path::new("render.wav"),
            "default",
            "5.1.4",
            &mut buffer,
            decoder::ChannelLayoutProvenance::KnownSpeakers,
        )
        .unwrap();
        assert_eq!(buffer.channel_roles, declared);
    }

    fn inventory() -> MhasInventory {
        MhasInventory {
            packet_count: 3,
            payload_bytes: 4,
            packet_types: vec![
                MhasPacketTypeCount {
                    packet_type: 1,
                    name: "mpeg-h-3da-config",
                    count: 1,
                },
                MhasPacketTypeCount {
                    packet_type: 2,
                    name: "mpeg-h-3da-frame",
                    count: 1,
                },
                MhasPacketTypeCount {
                    packet_type: 3,
                    name: "audio-scene-info",
                    count: 1,
                },
            ],
            labels: vec![1],
            configuration_count: 1,
            frame_count: 1,
            audio_scene_info_count: 1,
            loudness_drc_count: 0,
            loudness_count: 0,
            sync_count: 0,
            profile_levels: vec![profile_level(0x0D).unwrap()],
        }
    }

    fn response() -> AdapterResponse {
        AdapterResponse {
            schema: RESPONSE_SCHEMA.into(),
            protocol_version: 1,
            input_sha256: "a".repeat(64),
            decoder: DecoderEvidence {
                name: "licensed-reference".into(),
                version: "1.0".into(),
            },
            core_standard: CORE_STANDARD.into(),
            reference_software_standard: REFERENCE_SOFTWARE_STANDARD.into(),
            conformance_standard: CONFORMANCE_STANDARD.into(),
            mpegh3da_profile_level_indication: 0x0D,
            scene: SceneMetadata {
                group_count: 1,
                groups: vec![SceneGroup {
                    id: 1,
                    signal_kind: SignalKind::Objects,
                    allow_on_off: true,
                    default_on: true,
                    language: Some("en".into()),
                    content_kind: Some("dialogue".into()),
                    allow_gain_interactivity: true,
                    min_gain_db: Some(-12.0),
                    max_gain_db: Some(6.0),
                    allow_position_interactivity: false,
                }],
                switch_group_count: 0,
                switch_groups: Vec::new(),
                preset_count: 1,
                presets: vec![ScenePreset {
                    id: 7,
                    name: Some("English".into()),
                    kind: 0,
                    group_ids: vec![1],
                }],
            },
            presentation_count: 2,
            presentations: vec![
                AdapterPresentation {
                    id: "default".into(),
                    preset_id: None,
                    rendered_path: "default.wav".into(),
                    output_layout: "stereo".into(),
                    language: Some("en".into()),
                    accessibility: None,
                    loudness: MpeghLoudnessMetadata {
                        loudness_info_type: 0,
                        mae_group_id: None,
                        mae_group_preset_id: None,
                        method_definition: 1,
                        program_loudness_lkfs: -23.0,
                        drc_set_id: 0,
                        downmix_id: 0,
                        measurement_system: Some("ITU-R BS.1770-5".into()),
                    },
                },
                AdapterPresentation {
                    id: "main-en".into(),
                    preset_id: Some(7),
                    rendered_path: "main.wav".into(),
                    output_layout: "stereo".into(),
                    language: Some("en".into()),
                    accessibility: None,
                    loudness: MpeghLoudnessMetadata {
                        loudness_info_type: 3,
                        mae_group_id: None,
                        mae_group_preset_id: Some(7),
                        method_definition: 1,
                        program_loudness_lkfs: -23.0,
                        drc_set_id: 0,
                        downmix_id: 0,
                        measurement_system: Some("ITU-R BS.1770-5".into()),
                    },
                },
            ],
        }
    }

    #[test]
    fn validates_current_mpegh_presentation_metadata() {
        assert!(validate_response(&response(), &"a".repeat(64), &inventory()).is_ok());
    }

    #[test]
    fn rejects_non_programme_loudness_entry() {
        let mut value = response();
        value.presentations[0].loudness.drc_set_id = 1;
        assert!(validate_response(&value, &"a".repeat(64), &inventory())
            .unwrap_err()
            .contains("drcSetId=0"));
    }

    #[test]
    fn rejects_profile_level_not_present_in_mhas() {
        let mut value = response();
        value.mpegh3da_profile_level_indication = 0x10;
        assert!(validate_response(&value, &"a".repeat(64), &inventory())
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn rejects_incomplete_or_duplicate_presentation_enumeration() {
        let mut value = response();
        value.presentation_count = 3;
        assert!(validate_response(&value, &"a".repeat(64), &inventory()).is_err());
        let mut value = response();
        let mut duplicate = value.presentations[0].clone();
        duplicate.rendered_path = "other.wav".into();
        value.presentations.push(duplicate);
        value.presentation_count = 2;
        assert!(validate_response(&value, &"a".repeat(64), &inventory()).is_err());
    }

    #[test]
    fn rejects_render_path_traversal() {
        let mut value = response();
        value.presentations[0].rendered_path = "../outside.wav".into();
        assert!(validate_response(&value, &"a".repeat(64), &inventory()).is_err());
    }

    #[test]
    fn accepts_single_default_presentation_without_scene_presets() {
        let mut value = response();
        value.scene.preset_count = 0;
        value.scene.presets.clear();
        value.presentation_count = 1;
        value.presentations.truncate(1);
        assert!(validate_response(&value, &"a".repeat(64), &inventory()).is_ok());
    }

    #[test]
    fn enforces_normative_profile_and_scene_id_ranges() {
        assert_eq!(profile_level(0x01).unwrap().profile, "main");
        assert_eq!(profile_level(0x0A).unwrap().profile, "high");
        assert_eq!(profile_level(0x0F).unwrap().profile, "low-complexity");
        assert_eq!(profile_level(0x14).unwrap().profile, "baseline");
        assert!(profile_level(0).is_err());
        assert!(profile_level(0x15).is_err());
        let mut value = response();
        value.scene.presets[0].id = 32;
        value.presentations[1].preset_id = Some(32);
        value.presentations[1].loudness.mae_group_preset_id = Some(32);
        assert!(validate_response(&value, &"a".repeat(64), &inventory()).is_err());
    }

    #[test]
    fn parses_mhas_packet_inventory_and_profile_level() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("fixture.mhas");
        let mut bytes = Vec::new();
        append_packet(&mut bytes, 6, 0, &[0xA5]);
        append_packet(&mut bytes, 1, 1, &[0x0D, 0]);
        append_packet(&mut bytes, 3, 1, &[0]);
        append_packet(&mut bytes, 13, 1, &[0]);
        append_packet(&mut bytes, 22, 1, &[0]);
        append_packet(&mut bytes, 2, 1, &[0]);
        fs::write(&path, bytes).unwrap();
        let value = inspect_mhas(&path).unwrap();
        assert_eq!(value.packet_count, 6);
        assert_eq!(value.audio_scene_info_count, 1);
        assert_eq!(value.loudness_drc_count, 1);
        assert_eq!(value.loudness_count, 1);
        assert_eq!(value.sync_count, 1);
        assert_eq!(value.profile_levels[0].profile, "low-complexity");
        assert_eq!(value.profile_levels[0].level, 3);
    }

    #[test]
    fn rejects_truncated_or_unconfigured_mhas_frames() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("bad.mhas");
        let mut bytes = Vec::new();
        append_packet(&mut bytes, 2, 1, &[0]);
        fs::write(&path, &bytes).unwrap();
        assert!(inspect_mhas(&path)
            .unwrap_err()
            .contains("no preceding configuration"));
        bytes.pop();
        fs::write(&path, bytes).unwrap();
        assert!(inspect_mhas(&path).unwrap_err().contains("declares"));
    }

    fn append_packet(output: &mut Vec<u8>, packet_type: u64, label: u64, payload: &[u8]) {
        let mut bits = Vec::new();
        write_escaped(&mut bits, packet_type, 3, 8, 8);
        write_escaped(&mut bits, label, 2, 8, 32);
        write_escaped(&mut bits, payload.len() as u64, 11, 24, 24);
        assert_eq!(bits.len() % 8, 0);
        for chunk in bits.chunks(8) {
            output.push(chunk.iter().fold(0, |byte, bit| (byte << 1) | bit));
        }
        output.extend_from_slice(payload);
    }

    fn write_escaped(bits: &mut Vec<u8>, value: u64, n1: u8, n2: u8, n3: u8) {
        let max1 = (1_u64 << n1) - 1;
        let first = value.min(max1);
        write_bits(bits, first, n1);
        if first == max1 {
            let rest = value - max1;
            let max2 = (1_u64 << n2) - 1;
            let second = rest.min(max2);
            write_bits(bits, second, n2);
            if second == max2 {
                write_bits(bits, rest - max2, n3);
            }
        }
    }

    fn write_bits(bits: &mut Vec<u8>, value: u64, count: u8) {
        for shift in (0..count).rev() {
            bits.push(((value >> shift) & 1) as u8);
        }
    }
}
