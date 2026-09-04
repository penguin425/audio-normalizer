//! Two-pass, segment-aware catalogue normalization with continuous boundaries.
//!
//! This is a non-normative engineering workflow. Pass one binds ordered input
//! bytes to measurements and a deterministic gain envelope. Pass two verifies
//! those bindings, renders one segment at a time, re-decodes the encoded bytes,
//! and emits bounded machine-readable evidence.

use crate::atomic::AtomicOutput;
use crate::decoder;
use crate::dsp::resample::ResampleQuality;
use crate::dsp::simd;
use crate::normalization_diff::{self, FileEvidence, MeasurementEvidence};
use crate::normalize::{self, Analysis, Mode, OutputFormat, Plan};
use crate::stable_input::{paths_alias_if_existing, StableInput, StableInputOptions};
use crate::wav::{
    AudioBuffer, ChannelRole, WavContainer, MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub const REQUEST_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/segment-normalization-request-v1";
pub const PLAN_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/segment-normalization-plan-v2";
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/segment-normalization-report-v2";
pub const METHOD_ID: &str = "forge-segment-normalization-v2";
pub const ALGORITHM_REVISION: &str = "smoothstep-db-boundary-layout-provenance-v2";

const MAX_REQUEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_AUDIO_INPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_SEGMENTS: usize = 4096;
const MAX_CHANNELS: usize = 32;
const DEFAULT_MAX_DECODED_SAMPLES: u64 = 50_000_000;
const HARD_MAX_DECODED_SAMPLES: u64 = 200_000_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentNormalizationRequest {
    pub schema: String,
    #[serde(default = "default_target_lufs")]
    pub target_lufs: f64,
    #[serde(default = "default_ceiling_dbtp")]
    pub ceiling_dbtp: f64,
    pub max_gain_db: Option<f64>,
    #[serde(default = "default_smoothing_ms")]
    pub smoothing_ms: f64,
    #[serde(default = "default_verification_tolerance")]
    pub verification_tolerance_lu_db: f64,
    #[serde(default = "default_duration_tolerance_ms")]
    pub duration_tolerance_ms: f64,
    #[serde(default = "default_review_threshold_db")]
    pub boundary_review_threshold_db: f64,
    #[serde(default = "default_max_decoded_samples")]
    pub max_decoded_samples_per_segment: u64,
    pub format: String,
    #[serde(default = "default_mp3_bitrate")]
    pub mp3_bitrate_kbps: i32,
    #[serde(default = "default_mp3_quality")]
    pub mp3_quality: i32,
    pub segments: Vec<SegmentRequest>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentRequest {
    pub id: String,
    pub input: PathBuf,
    pub output: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentNormalizationPlan {
    pub schema: String,
    pub generator: String,
    pub method: PlanMethodEvidence,
    pub request: FileEvidence,
    pub settings: PlanSettings,
    pub layout: LayoutEvidence,
    pub segments: Vec<PlannedSegment>,
    pub manual_review_recommended: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanMethodEvidence {
    pub id: String,
    pub classification: String,
    pub algorithm_revision: String,
    pub boundary_rule: String,
    pub smoothing_curve: String,
    pub processing_bound: String,
    pub maximum_segments: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSettings {
    pub target_lufs: f64,
    pub ceiling_dbtp: f64,
    pub max_gain_db: Option<f64>,
    pub smoothing_ms: f64,
    pub verification_tolerance_lu_db: f64,
    pub duration_tolerance_ms: f64,
    pub boundary_review_threshold_db: f64,
    pub max_decoded_samples_per_segment: u64,
    pub format: String,
    pub mp3_bitrate_kbps: i32,
    pub mp3_quality: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LayoutEvidence {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRoleEvidence>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChannelRoleEvidence {
    Main,
    Surround,
    DualMono,
    Lfe,
    Positioned {
        azimuth_degrees: i16,
        elevation_degrees: i16,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedSegment {
    pub index: usize,
    pub id: String,
    pub input: FileEvidence,
    pub output_path: String,
    pub source: MeasurementEvidence,
    pub desired_gain_db: f64,
    pub maximum_safe_gain_db: f64,
    pub start_gain_db: f64,
    pub end_gain_db: f64,
    pub ramp_frames: usize,
    pub adjacent_desired_gain_delta_db: Option<f64>,
    pub manual_review_recommended: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentNormalizationReport {
    pub schema: &'static str,
    pub generator: &'static str,
    pub method: RenderMethodEvidence,
    pub plan: FileEvidence,
    pub settings: PlanSettings,
    pub layout: LayoutEvidence,
    pub segments: Vec<RenderedSegment>,
    pub published_segments: usize,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderMethodEvidence {
    pub id: &'static str,
    pub classification: &'static str,
    pub algorithm_revision: &'static str,
    pub input_binding: &'static str,
    pub publication: &'static str,
    pub verification: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderedSegment {
    pub index: usize,
    pub id: String,
    pub input: FileEvidence,
    pub output: FileEvidence,
    pub source: MeasurementEvidence,
    pub intended_pre_codec: MeasurementEvidence,
    pub decoded_output: MeasurementEvidence,
    pub target_loudness_headroom_lu: f64,
    pub codec_loudness_deviation_lu: f64,
    pub duration_deviation_ms: f64,
    pub desired_gain_db: f64,
    pub maximum_safe_gain_db: f64,
    pub start_gain_db: f64,
    pub end_gain_db: f64,
    pub ramp_frames: usize,
    pub input_binding_passed: bool,
    pub codec_loudness_passed: bool,
    pub true_peak_passed: bool,
    pub duration_passed: bool,
    pub published: bool,
    pub passed: bool,
}

fn default_target_lufs() -> f64 {
    -16.0
}

fn default_ceiling_dbtp() -> f64 {
    -1.0
}

fn default_smoothing_ms() -> f64 {
    500.0
}

fn default_verification_tolerance() -> f64 {
    0.5
}

fn default_duration_tolerance_ms() -> f64 {
    100.0
}

fn default_review_threshold_db() -> f64 {
    6.0
}

fn default_max_decoded_samples() -> u64 {
    DEFAULT_MAX_DECODED_SAMPLES
}

fn default_mp3_bitrate() -> i32 {
    320
}

fn default_mp3_quality() -> i32 {
    2
}

pub fn create_plan(
    request_path: &Path,
    manifest_path: &Path,
    overwrite: bool,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<SegmentNormalizationPlan, String> {
    let request_path = resolved_path(request_path)?;
    let manifest_path = resolved_path(manifest_path)?;
    validate_json_path(&manifest_path, "segment plan")?;
    if path_key(&request_path) == path_key(&manifest_path) {
        return Err("segment plan aliases its request file".into());
    }
    if paths_alias_if_existing(&request_path, &manifest_path).map_err(|error| error.to_string())? {
        return Err("segment plan aliases its request file".into());
    }
    if manifest_path.exists() && !overwrite {
        return Err(format!(
            "{} already exists (use --overwrite to replace it)",
            manifest_path.display()
        ));
    }
    let request_input = capture_stable_input(&request_path, MAX_REQUEST_BYTES, "segment request")?;
    let request_binding = stable_file_evidence(&request_input, &request_path);
    let request = load_bounded::<SegmentNormalizationRequest>(
        request_input.stable_path(),
        MAX_REQUEST_BYTES,
    )?;
    validate_request(&request)?;
    let format = parse_format(&request.format)?;
    let plan_settings = PlanSettings::from(&request);
    // Planning measures and records the requested operation but does not
    // encode. Keep manifests portable to a render host that has the optional
    // codec while still rejecting malformed format settings up front.
    normalization_plan(&plan_settings).validate_format_request(format)?;
    let base = request_path.parent().unwrap_or_else(|| Path::new("."));
    let mut inputs = Vec::with_capacity(request.segments.len());
    let mut outputs = Vec::with_capacity(request.segments.len());
    for segment in &request.segments {
        inputs.push(resolved_path(&resolve_from(base, &segment.input))?);
        outputs.push(resolved_path(&resolve_from(base, &segment.output))?);
    }
    validate_plan_paths(&request_path, &manifest_path, &inputs, &outputs, format)?;

    let mut measured = Vec::with_capacity(inputs.len());
    let mut layout = None;
    for input in &inputs {
        let stable = capture_stable_input(input, MAX_AUDIO_INPUT_BYTES, "segment audio input")?;
        let binding = stable_file_evidence(&stable, input);
        let buffer = decode_segment(
            stable.stable_path(),
            request.max_decoded_samples_per_segment,
            channel_roles,
        )?;
        let current_layout = LayoutEvidence {
            sample_rate_hz: buffer.sample_rate,
            channels: buffer.channels,
            channel_roles: buffer
                .channel_roles
                .iter()
                .copied()
                .map(ChannelRoleEvidence::from)
                .collect(),
        };
        if let Some(expected) = &layout {
            if expected != &current_layout {
                return Err(format!(
                    "segment layout differs from the first input: {}",
                    input.display()
                ));
            }
        } else {
            layout = Some(current_layout);
        }
        let analysis = normalize::analyze(&buffer);
        if !analysis.lufs.is_finite() {
            return Err(format!(
                "segment has no finite integrated loudness: {}",
                input.display()
            ));
        }
        let maximum_safe_gain_db = maximum_safe_gain_db(&analysis, &request);
        let desired_gain_db = (request.target_lufs - analysis.lufs).min(maximum_safe_gain_db);
        stable.verify_source().map_err(|error| {
            format!(
                "{} changed while the pass-one plan was measured: {error}",
                input.display()
            )
        })?;
        measured.push((
            binding,
            MeasurementEvidence::from(&analysis),
            desired_gain_db,
            maximum_safe_gain_db,
            buffer.frames,
        ));
    }
    let layout = layout.ok_or_else(|| "segment request is empty".to_string())?;
    let desired = measured.iter().map(|item| item.2).collect::<Vec<_>>();
    let safe = measured.iter().map(|item| item.3).collect::<Vec<_>>();
    let boundaries = boundary_gains(&desired, &safe);
    let mut segments = Vec::with_capacity(measured.len());
    for (index, ((((request_segment, output), measured), desired_gain), safe_gain)) in request
        .segments
        .iter()
        .zip(&outputs)
        .zip(&measured)
        .zip(&desired)
        .zip(&safe)
        .enumerate()
    {
        let previous_boundary = index.checked_sub(1).and_then(|value| boundaries.get(value));
        let next_boundary = boundaries.get(index);
        let start_gain_db = previous_boundary.copied().unwrap_or(*desired_gain);
        let end_gain_db = next_boundary.copied().unwrap_or(*desired_gain);
        let adjacent_delta = desired
            .get(index + 1)
            .map(|next| (next - desired_gain).abs());
        let review =
            adjacent_delta.is_some_and(|delta| delta > request.boundary_review_threshold_db);
        segments.push(PlannedSegment {
            index,
            id: request_segment.id.clone(),
            input: measured.0.clone(),
            output_path: output.to_string_lossy().into_owned(),
            source: measured.1.clone(),
            desired_gain_db: *desired_gain,
            maximum_safe_gain_db: *safe_gain,
            start_gain_db,
            end_gain_db,
            ramp_frames: ramp_frames(measured.4, layout.sample_rate_hz, request.smoothing_ms),
            adjacent_desired_gain_delta_db: adjacent_delta,
            manual_review_recommended: review,
        });
    }
    request_input.verify_source().map_err(|error| {
        format!(
            "{} changed while the pass-one plan was created: {error}",
            request_path.display()
        )
    })?;
    for segment in &segments {
        verify_file_binding(Path::new(&segment.input.path), &segment.input)?;
    }
    let plan = SegmentNormalizationPlan {
        schema: PLAN_SCHEMA.into(),
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")).into(),
        method: PlanMethodEvidence {
            id: METHOD_ID.into(),
            classification: "non-normative deterministic engineering workflow; not a streaming-platform compliance certification".into(),
            algorithm_revision: ALGORITHM_REVISION.into(),
            boundary_rule: "the dB midpoint of adjacent desired gains, capped by both segments' maximum safe gain; both sides store the identical boundary gain".into(),
            smoothing_curve: "cubic smoothstep in the dB domain from the shared boundary gain to each segment's desired gain".into(),
            processing_bound: "inputs are decoded and rendered one segment at a time under the manifest's per-segment decoded-sample limit".into(),
            maximum_segments: MAX_SEGMENTS,
        },
        request: request_binding,
        settings: plan_settings,
        layout,
        manual_review_recommended: segments
            .iter()
            .any(|segment| segment.manual_review_recommended),
        segments,
    };
    validate_plan(&plan)?;
    request_input.verify_source().map_err(|error| {
        format!(
            "{} changed before the pass-one plan was published: {error}",
            request_path.display()
        )
    })?;
    for segment in &plan.segments {
        verify_file_binding(Path::new(&segment.input.path), &segment.input)?;
    }
    if let Some(parent) = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    write_json_atomic(&manifest_path, &plan, overwrite)?;
    Ok(plan)
}

pub fn render_plan(
    manifest_path: &Path,
    report_path: &Path,
    overwrite: bool,
) -> Result<SegmentNormalizationReport, String> {
    let manifest_path = resolved_path(manifest_path)?;
    let report_path = resolved_path(report_path)?;
    validate_json_path(&manifest_path, "segment plan")?;
    validate_json_path(&report_path, "segment report")?;
    let manifest_input = capture_stable_input(&manifest_path, MAX_MANIFEST_BYTES, "segment plan")?;
    let plan =
        load_bounded::<SegmentNormalizationPlan>(manifest_input.stable_path(), MAX_MANIFEST_BYTES)?;
    validate_plan(&plan)?;
    let format = parse_format(&plan.settings.format)?;
    validate_render_paths(&manifest_path, &report_path, &plan, format, overwrite)?;
    let render_plan = normalization_plan(&plan.settings);
    render_plan.validate_for_format(format)?;
    let request_input = verify_request_binding_and_intent(&plan)?;
    manifest_input.verify_source().map_err(|error| {
        format!(
            "segment plan changed before rendering {}: {error}",
            manifest_path.display()
        )
    })?;
    // Capture and hash every source before publishing anything. Individual
    // renders take another immutable snapshot and recheck the live source at
    // their own publication boundary.
    for segment in &plan.segments {
        let input = capture_stable_input(
            Path::new(&segment.input.path),
            MAX_AUDIO_INPUT_BYTES,
            "segment audio input",
        )?;
        verify_stable_file_binding(&input, &segment.input)?;
    }
    if let Some(parent) = report_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    for segment in &plan.segments {
        let output = Path::new(&segment.output_path);
        if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
    }

    let roles = plan
        .layout
        .channel_roles
        .iter()
        .cloned()
        .map(ChannelRole::from)
        .collect::<Vec<_>>();
    let mut rendered = Vec::with_capacity(plan.segments.len());
    for segment in &plan.segments {
        let input = capture_stable_input(
            Path::new(&segment.input.path),
            MAX_AUDIO_INPUT_BYTES,
            "segment audio input",
        )?;
        verify_stable_file_binding(&input, &segment.input)?;
        rendered.push(render_segment(
            segment,
            &input,
            &plan.settings,
            &roles,
            &render_plan,
            format,
            overwrite,
        )?);
    }
    let published_segments = rendered.iter().filter(|segment| segment.published).count();
    let report = SegmentNormalizationReport {
        schema: REPORT_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        method: RenderMethodEvidence {
            id: METHOD_ID,
            classification: "non-normative deterministic engineering workflow; decoded-output checks are not platform certification",
            algorithm_revision: ALGORITHM_REVISION,
            input_binding: "SHA-256 and byte length are checked before publication and again around each decoded source",
            publication: "each output is staged beside its destination and atomically replaced only after its own verification; the ordered set is not a filesystem transaction",
            verification: "re-decoded output loudness is compared with the exact smoothed pre-codec signal; decoded true peak and duration are bounded",
        },
        plan: stable_file_evidence(&manifest_input, &manifest_path),
        settings: plan.settings,
        layout: plan.layout,
        passed: rendered.iter().all(|segment| segment.passed),
        segments: rendered,
        published_segments,
    };
    manifest_input.verify_source().map_err(|error| {
        format!(
            "segment plan changed before report publication {}: {error}",
            manifest_path.display()
        )
    })?;
    request_input
        .verify_source()
        .map_err(|error| format!("segment request changed before report publication: {error}"))?;
    write_json_atomic(&report_path, &report, overwrite)?;
    Ok(report)
}

fn render_segment(
    segment: &PlannedSegment,
    input: &StableInput,
    settings: &PlanSettings,
    roles: &[ChannelRole],
    plan: &Plan,
    format: OutputFormat,
    overwrite: bool,
) -> Result<RenderedSegment, String> {
    let input_path = Path::new(&segment.input.path);
    let output_path = Path::new(&segment.output_path);
    verify_stable_file_binding(input, &segment.input)?;
    let mut buffer = decode_segment(
        input.stable_path(),
        settings.max_decoded_samples_per_segment,
        Some(roles),
    )?;
    let source = normalize::analyze(&buffer);
    verify_source_measurement(segment, &source)?;
    apply_smoothed_gain(
        &mut buffer,
        segment.start_gain_db,
        segment.desired_gain_db,
        segment.end_gain_db,
        segment.ramp_frames,
        settings.ceiling_dbtp,
    )?;
    let intended = normalize::analyze(&buffer);
    input.verify_source().map_err(|error| {
        format!(
            "{} changed while segment {} was decoded: {error}",
            input_path.display(),
            segment.id
        )
    })?;

    let mut staged = AtomicOutput::new_with_overwrite(output_path, overwrite)?;
    normalize::write(&buffer, staged.path(), plan, format)?;
    staged.adopt_path_writer_output()?;
    let padding = u64::from(intended.sample_rate)
        .checked_mul(u64::from(intended.channels))
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| "decoded output padding limit overflow".to_string())?;
    drop(buffer);
    let output_limit = settings
        .max_decoded_samples_per_segment
        .checked_add(padding)
        .ok_or_else(|| "decoded output sample limit overflow".to_string())?;
    let decoded = decode_segment(staged.path(), output_limit, Some(roles))?;
    let decoded_analysis = normalize::analyze(&decoded);
    let codec_loudness_deviation_lu = (decoded_analysis.lufs - intended.lufs).abs();
    let duration_deviation_ms = ((decoded_analysis.frames as f64
        / decoded_analysis.sample_rate as f64)
        - (intended.frames as f64 / intended.sample_rate as f64))
        .abs()
        * 1000.0;
    let codec_loudness_passed =
        codec_loudness_deviation_lu <= settings.verification_tolerance_lu_db;
    let true_peak_passed =
        decoded_true_peak_passed(decoded_analysis.true_peak_db(), settings.ceiling_dbtp);
    let duration_passed = duration_deviation_ms <= settings.duration_tolerance_ms;
    let passed = codec_loudness_passed && true_peak_passed && duration_passed;
    let staged_file = normalization_diff::inspect_file(staged.path())?;
    let output = FileEvidence {
        path: output_path.to_string_lossy().into_owned(),
        bytes: staged_file.bytes,
        sha256: staged_file.sha256,
    };
    if passed {
        input.verify_source().map_err(|error| {
            format!(
                "{} changed before segment {} publication: {error}",
                input_path.display(),
                segment.id
            )
        })?;
        staged.commit()?;
    }
    Ok(RenderedSegment {
        index: segment.index,
        id: segment.id.clone(),
        input: segment.input.clone(),
        output,
        source: MeasurementEvidence::from(&source),
        intended_pre_codec: MeasurementEvidence::from(&intended),
        decoded_output: MeasurementEvidence::from(&decoded_analysis),
        target_loudness_headroom_lu: settings.target_lufs - decoded_analysis.lufs,
        codec_loudness_deviation_lu,
        duration_deviation_ms,
        desired_gain_db: segment.desired_gain_db,
        maximum_safe_gain_db: segment.maximum_safe_gain_db,
        start_gain_db: segment.start_gain_db,
        end_gain_db: segment.end_gain_db,
        ramp_frames: segment.ramp_frames,
        input_binding_passed: true,
        codec_loudness_passed,
        true_peak_passed,
        duration_passed,
        published: passed,
        passed,
    })
}

fn decoded_true_peak_passed(true_peak_dbtp: f64, ceiling_dbtp: f64) -> bool {
    normalize::true_peak_within_ceiling(true_peak_dbtp, ceiling_dbtp)
}

fn apply_smoothed_gain(
    buffer: &mut AudioBuffer,
    start_gain_db: f64,
    desired_gain_db: f64,
    end_gain_db: f64,
    ramp_frames: usize,
    ceiling_dbtp: f64,
) -> Result<(), String> {
    if buffer.frames == 0
        || buffer
            .data
            .iter()
            .any(|channel| channel.len() != buffer.frames)
    {
        return Err("segment buffer has inconsistent frame geometry".into());
    }
    if ramp_frames > buffer.frames / 2 {
        return Err("segment smoothing ramp exceeds half the segment".into());
    }
    let desired_linear = db_to_linear(desired_gain_db)?;
    for channel in &mut buffer.data {
        simd::apply_gain(channel, desired_linear);
    }
    if ramp_frames > 0 {
        for frame in 0..ramp_frames {
            let t = unit_position(frame, ramp_frames);
            let envelope_db = interpolate_db(start_gain_db, desired_gain_db, t);
            let ratio = db_to_linear(envelope_db - desired_gain_db)?;
            for channel in &mut buffer.data {
                channel[frame] *= ratio;
            }
        }
        let start = buffer.frames - ramp_frames;
        for offset in 0..ramp_frames {
            let t = end_ramp_position(offset, ramp_frames);
            let envelope_db = interpolate_db(desired_gain_db, end_gain_db, t);
            let ratio = db_to_linear(envelope_db - desired_gain_db)?;
            for channel in &mut buffer.data {
                channel[start + offset] *= ratio;
            }
        }
    }
    let ceiling = 10.0_f64.powf(ceiling_dbtp / 20.0) as f32;
    for channel in &mut buffer.data {
        simd::hard_clip(channel, ceiling);
    }
    Ok(())
}

fn unit_position(index: usize, count: usize) -> f64 {
    if count <= 1 {
        0.0
    } else {
        index as f64 / (count - 1) as f64
    }
}

fn end_ramp_position(index: usize, count: usize) -> f64 {
    if count <= 1 {
        1.0
    } else {
        unit_position(index, count)
    }
}

fn interpolate_db(from: f64, to: f64, t: f64) -> f64 {
    let smooth = t * t * (3.0 - 2.0 * t);
    from + (to - from) * smooth
}

fn db_to_linear(db: f64) -> Result<f32, String> {
    let linear = 10.0_f64.powf(db / 20.0);
    if !linear.is_finite() || linear <= 0.0 || linear > f32::MAX as f64 {
        return Err("segment gain is outside the representable range".into());
    }
    Ok(linear as f32)
}

fn ramp_frames(frames: usize, sample_rate: u32, smoothing_ms: f64) -> usize {
    let requested = (smoothing_ms * sample_rate as f64 / 1000.0).round() as usize;
    requested.min(frames / 2)
}

fn boundary_gains(desired: &[f64], safe: &[f64]) -> Vec<f64> {
    desired
        .windows(2)
        .zip(safe.windows(2))
        .map(|(desired, safe)| ((desired[0] + desired[1]) * 0.5).min(safe[0]).min(safe[1]))
        .collect()
}

fn maximum_safe_gain_db(analysis: &Analysis, request: &SegmentNormalizationRequest) -> f64 {
    let peak_limit = request.ceiling_dbtp - analysis.true_peak_db();
    request
        .max_gain_db
        .map_or(peak_limit, |limit| peak_limit.min(limit))
}

fn normalization_plan(settings: &PlanSettings) -> Plan {
    Plan {
        mode: Mode::Lufs,
        target_lufs: settings.target_lufs,
        target_peak_db: -0.1,
        target_rms_db: -18.0,
        ceiling_db: settings.ceiling_dbtp,
        max_gain_db: settings.max_gain_db,
        dither: false,
        output_kind: None,
        mp3_bitrate: settings.mp3_bitrate_kbps,
        mp3_quality: settings.mp3_quality,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: ResampleQuality::Balanced,
    }
}

fn validate_request(request: &SegmentNormalizationRequest) -> Result<(), String> {
    if request.schema != REQUEST_SCHEMA {
        return Err(format!(
            "unsupported segment normalization request schema: {}",
            request.schema
        ));
    }
    if !(2..=MAX_SEGMENTS).contains(&request.segments.len()) {
        return Err(format!(
            "segment normalization requires 2..={MAX_SEGMENTS} ordered segments"
        ));
    }
    finite_range("target_lufs", request.target_lufs, -100.0, 0.0)?;
    finite_range("ceiling_dbtp", request.ceiling_dbtp, -20.0, 0.0)?;
    if let Some(max_gain) = request.max_gain_db {
        finite_range("max_gain_db", max_gain, -120.0, 60.0)?;
    }
    finite_range("smoothing_ms", request.smoothing_ms, 1.0, 10_000.0)?;
    finite_range(
        "verification_tolerance_lu_db",
        request.verification_tolerance_lu_db,
        0.0,
        5.0,
    )?;
    finite_range(
        "duration_tolerance_ms",
        request.duration_tolerance_ms,
        0.0,
        10_000.0,
    )?;
    finite_range(
        "boundary_review_threshold_db",
        request.boundary_review_threshold_db,
        0.0,
        60.0,
    )?;
    if request.max_decoded_samples_per_segment == 0
        || request.max_decoded_samples_per_segment > HARD_MAX_DECODED_SAMPLES
    {
        return Err(format!(
            "max_decoded_samples_per_segment must be 1..={HARD_MAX_DECODED_SAMPLES}"
        ));
    }
    if !(8..=320).contains(&request.mp3_bitrate_kbps) {
        return Err("mp3_bitrate_kbps must be 8..=320".into());
    }
    if !(0..=9).contains(&request.mp3_quality) {
        return Err("mp3_quality must be 0..=9".into());
    }
    parse_format(&request.format)?;
    let mut ids = HashSet::new();
    for segment in &request.segments {
        validate_id(&segment.id)?;
        if !ids.insert(segment.id.clone()) {
            return Err(format!("duplicate segment id: {}", segment.id));
        }
        if segment.input.as_os_str().is_empty() || segment.output.as_os_str().is_empty() {
            return Err(format!("segment {} has an empty path", segment.id));
        }
    }
    Ok(())
}

fn validate_plan(plan: &SegmentNormalizationPlan) -> Result<(), String> {
    if plan.schema != PLAN_SCHEMA
        || plan.method.id != METHOD_ID
        || plan.method.algorithm_revision != ALGORITHM_REVISION
    {
        return Err("unsupported segment normalization plan method or schema".into());
    }
    if plan.method.maximum_segments != MAX_SEGMENTS {
        return Err("segment plan has an unexpected maximum-segment bound".into());
    }
    if !plan.generator.starts_with("forge-normalizer/") {
        return Err("segment plan has an invalid generator".into());
    }
    if !(2..=MAX_SEGMENTS).contains(&plan.segments.len()) {
        return Err("segment plan has an invalid segment count".into());
    }
    if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ)
        .contains(&plan.layout.sample_rate_hz)
        || plan.layout.channels == 0
        || plan.layout.channels as usize > MAX_CHANNELS
        || plan.layout.channel_roles.len() != plan.layout.channels as usize
    {
        return Err("segment plan has invalid channel layout evidence".into());
    }
    validate_settings(&plan.settings)?;
    let request_path = Path::new(&plan.request.path);
    if !request_path.is_absolute()
        || plan.request.bytes == 0
        || plan.request.sha256.len() != 64
        || !plan
            .request
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("segment plan has an invalid request binding".into());
    }
    let request_key = path_key(request_path);
    let mut ids = HashSet::new();
    let mut inputs = HashSet::new();
    let mut outputs = HashSet::new();
    for (index, segment) in plan.segments.iter().enumerate() {
        if segment.index != index {
            return Err("segment plan indices are not contiguous and ordered".into());
        }
        validate_id(&segment.id)?;
        if !ids.insert(segment.id.clone()) {
            return Err(format!("duplicate segment id: {}", segment.id));
        }
        let input_path = Path::new(&segment.input.path);
        let output_path = Path::new(&segment.output_path);
        if !input_path.is_absolute() || !output_path.is_absolute() {
            return Err(format!(
                "segment {} plan paths must be absolute",
                segment.id
            ));
        }
        if segment.input.bytes == 0
            || segment.input.sha256.len() != 64
            || !segment
                .input
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !inputs.insert(path_key(input_path))
            || !outputs.insert(path_key(output_path))
        {
            return Err(format!(
                "segment {} has an invalid file binding",
                segment.id
            ));
        }
        if path_key(input_path) == path_key(output_path) {
            return Err(format!("segment {} input aliases its output", segment.id));
        }
        if path_key(input_path) == request_key || path_key(output_path) == request_key {
            return Err(format!("segment {} aliases the request file", segment.id));
        }
        for value in [
            segment.desired_gain_db,
            segment.maximum_safe_gain_db,
            segment.start_gain_db,
            segment.end_gain_db,
        ] {
            if !value.is_finite() {
                return Err(format!("segment {} has a non-finite gain", segment.id));
            }
        }
        if segment.desired_gain_db > segment.maximum_safe_gain_db + 1.0e-9
            || segment.start_gain_db > segment.maximum_safe_gain_db + 1.0e-9
            || segment.end_gain_db > segment.maximum_safe_gain_db + 1.0e-9
        {
            return Err(format!("segment {} exceeds its safe gain", segment.id));
        }
        if segment.source.sample_rate_hz != plan.layout.sample_rate_hz
            || segment.source.channels != plan.layout.channels
            || segment.source.frames == 0
        {
            return Err(format!(
                "segment {} has inconsistent source geometry",
                segment.id
            ));
        }
        let source_lufs = segment
            .source
            .integrated_lufs
            .filter(|value| value.is_finite())
            .ok_or_else(|| format!("segment {} has no finite source loudness", segment.id))?;
        let source_true_peak = segment
            .source
            .true_peak_dbtp
            .filter(|value| value.is_finite())
            .ok_or_else(|| format!("segment {} has no finite source true peak", segment.id))?;
        let expected_safe = plan
            .settings
            .max_gain_db
            .map_or(plan.settings.ceiling_dbtp - source_true_peak, |maximum| {
                maximum.min(plan.settings.ceiling_dbtp - source_true_peak)
            });
        let expected_desired = (plan.settings.target_lufs - source_lufs).min(expected_safe);
        if (segment.maximum_safe_gain_db - expected_safe).abs() > 1.0e-9
            || (segment.desired_gain_db - expected_desired).abs() > 1.0e-9
        {
            return Err(format!("segment {} gain plan is inconsistent", segment.id));
        }
        let frames = segment.source.frames;
        let expected_ramp = ramp_frames(
            frames,
            plan.layout.sample_rate_hz,
            plan.settings.smoothing_ms,
        );
        if segment.ramp_frames != expected_ramp {
            return Err(format!("segment {} has an invalid ramp length", segment.id));
        }
        validate_output_extension(output_path, parse_format(&plan.settings.format)?)?;
    }
    let desired = plan
        .segments
        .iter()
        .map(|segment| segment.desired_gain_db)
        .collect::<Vec<_>>();
    let safe = plan
        .segments
        .iter()
        .map(|segment| segment.maximum_safe_gain_db)
        .collect::<Vec<_>>();
    let expected = boundary_gains(&desired, &safe);
    for (index, boundary) in expected.iter().enumerate() {
        if (plan.segments[index].end_gain_db - boundary).abs() > 1.0e-9
            || (plan.segments[index + 1].start_gain_db - boundary).abs() > 1.0e-9
        {
            return Err(format!("segment boundary {index} is not continuous"));
        }
        let expected_delta = (desired[index + 1] - desired[index]).abs();
        if plan.segments[index]
            .adjacent_desired_gain_delta_db
            .is_none_or(|value| (value - expected_delta).abs() > 1.0e-9)
        {
            return Err(format!("segment boundary {index} delta is inconsistent"));
        }
        let expected_review = expected_delta > plan.settings.boundary_review_threshold_db;
        if plan.segments[index].manual_review_recommended != expected_review {
            return Err(format!(
                "segment boundary {index} review flag is inconsistent"
            ));
        }
    }
    if plan
        .segments
        .last()
        .is_some_and(|segment| segment.adjacent_desired_gain_delta_db.is_some())
        || plan
            .segments
            .last()
            .is_some_and(|segment| segment.manual_review_recommended)
        || (plan.segments[0].start_gain_db - plan.segments[0].desired_gain_db).abs() > 1.0e-9
        || (plan.segments.last().unwrap().end_gain_db
            - plan.segments.last().unwrap().desired_gain_db)
            .abs()
            > 1.0e-9
    {
        return Err("segment plan endpoint evidence is inconsistent".into());
    }
    let expected_review = plan
        .segments
        .iter()
        .any(|segment| segment.manual_review_recommended);
    if plan.manual_review_recommended != expected_review {
        return Err("segment plan manual-review summary is inconsistent".into());
    }
    Ok(())
}

fn validate_settings(settings: &PlanSettings) -> Result<(), String> {
    let request = SegmentNormalizationRequest {
        schema: REQUEST_SCHEMA.into(),
        target_lufs: settings.target_lufs,
        ceiling_dbtp: settings.ceiling_dbtp,
        max_gain_db: settings.max_gain_db,
        smoothing_ms: settings.smoothing_ms,
        verification_tolerance_lu_db: settings.verification_tolerance_lu_db,
        duration_tolerance_ms: settings.duration_tolerance_ms,
        boundary_review_threshold_db: settings.boundary_review_threshold_db,
        max_decoded_samples_per_segment: settings.max_decoded_samples_per_segment,
        format: settings.format.clone(),
        mp3_bitrate_kbps: settings.mp3_bitrate_kbps,
        mp3_quality: settings.mp3_quality,
        segments: vec![
            SegmentRequest {
                id: "validation-a".into(),
                input: "a.wav".into(),
                output: "a.wav".into(),
            },
            SegmentRequest {
                id: "validation-b".into(),
                input: "b.wav".into(),
                output: "b.wav".into(),
            },
        ],
    };
    validate_request(&request)
}

fn validate_plan_paths(
    request: &Path,
    manifest: &Path,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    format: OutputFormat,
) -> Result<(), String> {
    let request_key = path_key(request);
    let manifest_key = path_key(manifest);
    let mut input_keys = HashSet::new();
    let mut input_paths: Vec<PathBuf> = Vec::with_capacity(inputs.len());
    for input in inputs {
        if !input.is_file() {
            return Err(format!("segment input is not a file: {}", input.display()));
        }
        let key = path_key(input);
        let aliases_control = paths_alias_if_existing(input, request)
            .map_err(|error| error.to_string())?
            || paths_alias_if_existing(input, manifest).map_err(|error| error.to_string())?;
        let aliases_input = input_paths.iter().try_fold(false, |found, previous| {
            if found {
                Ok(true)
            } else {
                paths_alias_if_existing(previous, input).map_err(|error| error.to_string())
            }
        })?;
        if key == request_key
            || key == manifest_key
            || aliases_control
            || aliases_input
            || !input_keys.insert(key)
        {
            return Err(format!("segment input path collision: {}", input.display()));
        }
        input_paths.push(input.to_owned());
    }
    let mut output_keys = HashSet::new();
    let mut output_paths: Vec<PathBuf> = Vec::with_capacity(outputs.len());
    for output in outputs {
        validate_output_extension(output, format)?;
        let key = path_key(output);
        let aliases_control = paths_alias_if_existing(output, request)
            .map_err(|error| error.to_string())?
            || paths_alias_if_existing(output, manifest).map_err(|error| error.to_string())?;
        let aliases_input = input_paths.iter().try_fold(false, |found, input| {
            if found {
                Ok(true)
            } else {
                paths_alias_if_existing(input, output).map_err(|error| error.to_string())
            }
        })?;
        let aliases_output = output_paths.iter().try_fold(false, |found, previous| {
            if found {
                Ok(true)
            } else {
                paths_alias_if_existing(previous, output).map_err(|error| error.to_string())
            }
        })?;
        if key == request_key
            || key == manifest_key
            || input_keys.contains(&key)
            || aliases_control
            || aliases_input
            || aliases_output
            || !output_keys.insert(key)
        {
            return Err(format!(
                "segment output path collision: {}",
                output.display()
            ));
        }
        output_paths.push(output.to_owned());
    }
    Ok(())
}

fn validate_render_paths(
    manifest: &Path,
    report: &Path,
    plan: &SegmentNormalizationPlan,
    format: OutputFormat,
    overwrite: bool,
) -> Result<(), String> {
    let manifest_key = path_key(manifest);
    let report_key = path_key(report);
    let request_key = path_key(&resolved_path(Path::new(&plan.request.path))?);
    if manifest_key == report_key {
        return Err("segment report aliases its plan manifest".into());
    }
    if manifest_key == request_key {
        return Err("segment plan aliases the pass-one request file".into());
    }
    if report_key == request_key {
        return Err("segment report aliases the pass-one request file".into());
    }
    if report.exists() && !overwrite {
        return Err(format!(
            "{} already exists (use --overwrite to replace it)",
            report.display()
        ));
    }
    let mut input_keys = HashSet::new();
    let mut input_paths: Vec<PathBuf> = Vec::with_capacity(plan.segments.len());
    for segment in &plan.segments {
        let input = resolved_path(Path::new(&segment.input.path))?;
        let key = path_key(&input);
        let aliases_control = paths_alias_if_existing(&input, manifest)
            .map_err(|error| error.to_string())?
            || paths_alias_if_existing(&input, report).map_err(|error| error.to_string())?
            || paths_alias_if_existing(&input, Path::new(&plan.request.path))
                .map_err(|error| error.to_string())?;
        let aliases_input = input_paths.iter().try_fold(false, |found, previous| {
            if found {
                Ok(true)
            } else {
                paths_alias_if_existing(previous, &input).map_err(|error| error.to_string())
            }
        })?;
        if key == manifest_key
            || key == report_key
            || key == request_key
            || aliases_control
            || aliases_input
            || !input_keys.insert(key)
        {
            return Err(format!("segment input path collision: {}", input.display()));
        }
        input_paths.push(input);
    }
    let mut output_keys = HashSet::new();
    let mut output_paths: Vec<PathBuf> = Vec::with_capacity(plan.segments.len());
    for segment in &plan.segments {
        let output = resolved_path(Path::new(&segment.output_path))?;
        validate_output_extension(&output, format)?;
        let key = path_key(&output);
        let aliases_control = paths_alias_if_existing(&output, manifest)
            .map_err(|error| error.to_string())?
            || paths_alias_if_existing(&output, report).map_err(|error| error.to_string())?
            || paths_alias_if_existing(&output, Path::new(&plan.request.path))
                .map_err(|error| error.to_string())?;
        let aliases_input = input_paths.iter().try_fold(false, |found, input| {
            if found {
                Ok(true)
            } else {
                paths_alias_if_existing(input, &output).map_err(|error| error.to_string())
            }
        })?;
        let aliases_output = output_paths.iter().try_fold(false, |found, previous| {
            if found {
                Ok(true)
            } else {
                paths_alias_if_existing(previous, &output).map_err(|error| error.to_string())
            }
        })?;
        if key == manifest_key
            || key == report_key
            || key == request_key
            || input_keys.contains(&key)
            || aliases_control
            || aliases_input
            || aliases_output
            || !output_keys.insert(key)
        {
            return Err(format!(
                "segment output path collision: {}",
                output.display()
            ));
        }
        output_paths.push(output.clone());
        if output.exists() && !overwrite {
            return Err(format!(
                "{} already exists (use --overwrite to replace it)",
                output.display()
            ));
        }
    }
    Ok(())
}

fn verify_request_binding_and_intent(
    plan: &SegmentNormalizationPlan,
) -> Result<StableInput, String> {
    let request_path = resolved_path(Path::new(&plan.request.path))?;
    let request_input = capture_stable_input(&request_path, MAX_REQUEST_BYTES, "segment request")?;
    verify_stable_file_binding(&request_input, &plan.request)?;
    let request = load_bounded::<SegmentNormalizationRequest>(
        request_input.stable_path(),
        MAX_REQUEST_BYTES,
    )?;
    validate_request(&request)?;
    if PlanSettings::from(&request) != plan.settings {
        return Err("segment plan settings do not match the bound request".into());
    }
    if request.segments.len() != plan.segments.len() {
        return Err("segment plan count does not match the bound request".into());
    }
    let base = request_path.parent().unwrap_or_else(|| Path::new("."));
    for (requested, planned) in request.segments.iter().zip(&plan.segments) {
        let input = resolved_path(&resolve_from(base, &requested.input))?;
        let output = resolved_path(&resolve_from(base, &requested.output))?;
        if requested.id != planned.id
            || path_key(&input) != path_key(Path::new(&planned.input.path))
            || path_key(&output) != path_key(Path::new(&planned.output_path))
        {
            return Err(format!(
                "segment {} plan paths or identity do not match the bound request",
                planned.id
            ));
        }
    }
    request_input.verify_source().map_err(|error| {
        format!("segment request changed while its intent was verified: {error}")
    })?;
    Ok(request_input)
}

fn verify_source_measurement(segment: &PlannedSegment, analysis: &Analysis) -> Result<(), String> {
    let source_lufs = segment
        .source
        .integrated_lufs
        .ok_or_else(|| format!("segment {} plan has no source loudness", segment.id))?;
    if segment.source.sample_rate_hz != analysis.sample_rate
        || segment.source.channels != analysis.channels
        || segment.source.frames != analysis.frames
        || (source_lufs - analysis.lufs).abs() > 1.0e-9
        || segment
            .source
            .true_peak_dbtp
            .is_none_or(|value| (value - analysis.true_peak_db()).abs() > 1.0e-9)
    {
        return Err(format!(
            "segment {} decoded measurement does not match its plan",
            segment.id
        ));
    }
    Ok(())
}

fn verify_file_binding(path: &Path, expected: &FileEvidence) -> Result<FileEvidence, String> {
    let input = capture_stable_input(path, MAX_AUDIO_INPUT_BYTES, "segment audio input")?;
    verify_stable_file_binding(&input, expected)?;
    Ok(stable_file_evidence(&input, path))
}

fn capture_stable_input(
    path: &Path,
    max_input_bytes: u64,
    description: &str,
) -> Result<StableInput, String> {
    let options = StableInputOptions::new(max_input_bytes)
        .map_err(|error| format!("configure {description}: {error}"))?;
    StableInput::from_path(path, &options)
        .map_err(|error| format!("capture {description} {}: {error}", path.display()))
}

fn stable_file_evidence(input: &StableInput, display_path: &Path) -> FileEvidence {
    FileEvidence {
        path: display_path.to_string_lossy().into_owned(),
        bytes: input.byte_len(),
        sha256: input.binding().sha256_hex(),
    }
}

fn verify_stable_file_binding(input: &StableInput, expected: &FileEvidence) -> Result<(), String> {
    if input.byte_len() != expected.bytes || input.binding().sha256_hex() != expected.sha256 {
        return Err(format!(
            "segment input does not match the plan binding: {}",
            expected.path
        ));
    }
    input
        .verify_source()
        .map_err(|error| format!("segment input changed: {}: {error}", expected.path))
}

fn apply_channel_roles(
    buffer: &mut AudioBuffer,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<(), String> {
    if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&buffer.sample_rate)
        || buffer.channels == 0
        || buffer.channels as usize > MAX_CHANNELS
        || buffer.data.len() != buffer.channels as usize
        || buffer.frames == 0
        || buffer
            .data
            .iter()
            .any(|channel| channel.len() != buffer.frames)
    {
        return Err(format!(
            "segment audio geometry exceeds the {MAX_CHANNELS}-channel/{MIN_DECODE_SAMPLE_RATE_HZ}..={MAX_DECODE_SAMPLE_RATE_HZ}-Hz bounds"
        ));
    }
    if let Some(roles) = channel_roles {
        if roles.len() != buffer.channels as usize {
            return Err(format!(
                "channel layout has {} roles for {} channels",
                roles.len(),
                buffer.channels
            ));
        }
        buffer.channel_roles = roles.to_vec();
    }
    Ok(())
}

fn decode_segment(
    path: &Path,
    max_decoded_samples: u64,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<AudioBuffer, String> {
    let (mut buffer, layout_provenance) =
        decoder::decode_limited_with_layout(path, max_decoded_samples)?;
    let roles = normalize::resolve_decoded_channel_roles(
        path,
        buffer.channels,
        &buffer.channel_roles,
        layout_provenance,
        channel_roles,
    )?;
    apply_channel_roles(&mut buffer, Some(&roles))?;
    Ok(buffer)
}

fn finite_range(name: &str, value: f64, minimum: f64, maximum: f64) -> Result<(), String> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        return Err(format!(
            "{name} must be a finite value in {minimum}..={maximum}"
        ));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!("invalid segment id: {id}"));
    }
    Ok(())
}

fn validate_json_path(path: &Path, description: &str) -> Result<(), String> {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("json"))
    {
        Ok(())
    } else {
        Err(format!("{description} must use a .json extension"))
    }
}

fn load_bounded<T: for<'de> Deserialize<'de>>(path: &Path, limit: u64) -> Result<T, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("manifest is not a file: {}", path.display()));
    }
    if metadata.len() > limit {
        return Err(format!("{} exceeds the {limit} byte limit", path.display()));
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if path.extension().and_then(|value| value.to_str()) == Some("toml") {
        toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
    } else {
        serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize, overwrite: bool) -> Result<(), String> {
    let staged = AtomicOutput::new_with_overwrite(path, overwrite)?;
    let mut file = File::create(staged.path())
        .map_err(|error| format!("create {}: {error}", staged.path().display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    drop(file);
    staged.commit()
}

fn resolve_from(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

#[cfg(any(windows, target_os = "macos"))]
type PathKey = String;
#[cfg(not(any(windows, target_os = "macos")))]
type PathKey = PathBuf;

#[cfg(any(windows, target_os = "macos"))]
fn path_key(path: &Path) -> PathKey {
    path.to_string_lossy().to_lowercase()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn path_key(path: &Path) -> PathKey {
    path.to_owned()
}

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

fn parse_format(value: &str) -> Result<OutputFormat, String> {
    match value {
        "wav" => Ok(OutputFormat::Wav),
        "flac" => Ok(OutputFormat::Flac),
        "mp3" => Ok(OutputFormat::Mp3),
        "opus" => Ok(OutputFormat::Opus),
        "m4a" => Ok(OutputFormat::M4a),
        "alac" => Ok(OutputFormat::Alac),
        "vorbis" => Ok(OutputFormat::Vorbis),
        _ => Err(format!("unsupported segment output format: {value}")),
    }
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
            "output extension does not match format: {}",
            path.display()
        ))
    }
}

impl From<&SegmentNormalizationRequest> for PlanSettings {
    fn from(request: &SegmentNormalizationRequest) -> Self {
        Self {
            target_lufs: request.target_lufs,
            ceiling_dbtp: request.ceiling_dbtp,
            max_gain_db: request.max_gain_db,
            smoothing_ms: request.smoothing_ms,
            verification_tolerance_lu_db: request.verification_tolerance_lu_db,
            duration_tolerance_ms: request.duration_tolerance_ms,
            boundary_review_threshold_db: request.boundary_review_threshold_db,
            max_decoded_samples_per_segment: request.max_decoded_samples_per_segment,
            format: request.format.clone(),
            mp3_bitrate_kbps: request.mp3_bitrate_kbps,
            mp3_quality: request.mp3_quality,
        }
    }
}

impl From<ChannelRole> for ChannelRoleEvidence {
    fn from(role: ChannelRole) -> Self {
        match role {
            ChannelRole::Main => Self::Main,
            ChannelRole::Surround => Self::Surround,
            ChannelRole::DualMono => Self::DualMono,
            ChannelRole::Lfe => Self::Lfe,
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => Self::Positioned {
                azimuth_degrees,
                elevation_degrees,
            },
        }
    }
}

impl From<ChannelRoleEvidence> for ChannelRole {
    fn from(role: ChannelRoleEvidence) -> Self {
        match role {
            ChannelRoleEvidence::Main => Self::Main,
            ChannelRoleEvidence::Surround => Self::Surround,
            ChannelRoleEvidence::DualMono => Self::DualMono,
            ChannelRoleEvidence::Lfe => Self::Lfe,
            ChannelRoleEvidence::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => Self::Positioned {
                azimuth_degrees,
                elevation_degrees,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_are_shared_and_capped_by_both_segments() {
        let desired = [0.0, 8.0, -4.0];
        let safe = [3.0, 6.0, 10.0];
        let boundaries = boundary_gains(&desired, &safe);
        assert_eq!(boundaries, vec![3.0, 2.0]);
    }

    #[test]
    fn smoothstep_reaches_both_endpoints_with_zero_endpoint_slope() {
        assert_eq!(interpolate_db(-6.0, 0.0, 0.0), -6.0);
        assert_eq!(interpolate_db(-6.0, 0.0, 1.0), 0.0);
        let near_start = interpolate_db(-6.0, 0.0, 1.0e-6);
        let near_end = interpolate_db(-6.0, 0.0, 1.0 - 1.0e-6);
        assert!((near_start + 6.0).abs() < 1.0e-9);
        assert!(near_end.abs() < 1.0e-9);
    }

    #[test]
    fn ramp_is_bounded_to_half_of_short_segments() {
        assert_eq!(ramp_frames(100, 48_000, 500.0), 50);
        assert_eq!(ramp_frames(48_000, 48_000, 500.0), 24_000);
    }

    #[test]
    fn one_frame_ramps_keep_both_shared_boundary_endpoints() {
        let mut buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 2,
            data: vec![vec![1.0, 1.0]],
            channel_roles: vec![ChannelRole::Main],
            source_kind: crate::wav::PcmKind::F32,
        };
        apply_smoothed_gain(&mut buffer, -6.0, -3.0, -9.0, 1, 0.0).unwrap();
        assert!((buffer.data[0][0] - db_to_linear(-6.0).unwrap()).abs() < 1.0e-6);
        assert!((buffer.data[0][1] - db_to_linear(-9.0).unwrap()).abs() < 1.0e-6);
    }

    #[test]
    fn request_count_and_resource_bounds_are_enforced_before_io() {
        let mut request = SegmentNormalizationRequest {
            schema: REQUEST_SCHEMA.into(),
            target_lufs: -16.0,
            ceiling_dbtp: -1.0,
            max_gain_db: None,
            smoothing_ms: 500.0,
            verification_tolerance_lu_db: 0.5,
            duration_tolerance_ms: 100.0,
            boundary_review_threshold_db: 6.0,
            max_decoded_samples_per_segment: DEFAULT_MAX_DECODED_SAMPLES,
            format: "wav".into(),
            mp3_bitrate_kbps: 320,
            mp3_quality: 2,
            segments: vec![SegmentRequest {
                id: "one".into(),
                input: "one.wav".into(),
                output: "one-out.wav".into(),
            }],
        };
        assert!(validate_request(&request).unwrap_err().contains("2..=4096"));
        request.segments.push(SegmentRequest {
            id: "two".into(),
            input: "two.wav".into(),
            output: "two-out.wav".into(),
        });
        request.max_decoded_samples_per_segment = HARD_MAX_DECODED_SAMPLES + 1;
        assert!(validate_request(&request)
            .unwrap_err()
            .contains("max_decoded_samples_per_segment"));
    }

    #[test]
    fn decoded_true_peak_never_uses_the_loudness_tolerance() {
        assert!(decoded_true_peak_passed(-1.0, -1.0));
        assert!(!decoded_true_peak_passed(-0.75, -1.0));
    }
}
