//! Explicit, bounded ONNX Runtime adapter for the anomaly-provider contract.
//!
//! The adapter intentionally accepts a feature-frame sidecar rather than
//! decoding audio itself.  This keeps feature extraction (and any model-
//! specific preprocessing) reviewable and lets the same model boundary be
//! used by a future Demucs/dialogue front end.  No model, runtime library, or
//! network access is bundled with Forge.  Every invocation must provide the
//! model and the ONNX Runtime shared library explicitly.

use crate::anomaly_provider::{AnomalyKind, ProviderEvent, ProviderInput};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

pub const MODEL_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/onnx-anomaly-model-v1";
pub const FEATURE_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/onnx-feature-frames-v1";
pub const ADAPTER: &str = "forge-onnx-provider-1";
pub const SCHEMA_VERSION: u32 = 1;

const MAX_MANIFEST_BYTES: usize = 1 << 20;
const MAX_FEATURE_BYTES: usize = 512 << 20;
const MAX_MODEL_BYTES: u64 = 512 << 20;
const MAX_FEATURE_FRAMES: usize = 2_000_000;
const MAX_FEATURE_VALUES: usize = 128_000_000;
const MAX_EVENTS: usize = 100_000;
const MAX_SOURCE_DURATION_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
const MAX_TEXT_LENGTH: usize = 512;
const MAX_LABEL_LENGTH: usize = 128;

/// A model's input or output tensor contract.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TensorContract {
    /// Exact ONNX graph name.
    pub name: String,
    /// Rank-three shape `[batch, frames, width]`; `-1` denotes a dynamic
    /// dimension.
    pub shape: Vec<i64>,
}

/// Licensing and dataset evidence required before a model can be run.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseEvidence {
    /// SPDX identifier or the exact published non-SPDX license name.
    pub spdx: String,
    /// Public model licence/source URL.
    pub url: String,
    /// Dataset or corpus used to train/evaluate the model.
    pub dataset: String,
    /// Public dataset/provenance URL.
    pub dataset_url: String,
}

/// Calibration evidence bound to the model manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationEvidence {
    /// SHA-256 of the calibration report or notebook export.
    pub report_sha256: String,
    /// Public or repository URL for the calibration evidence.
    pub report_url: String,
}

/// One output class.  The model emits a confidence/severity pair for every
/// class, in this order, for each frame.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelClass {
    pub kind: AnomalyKind,
    pub confidence_threshold: f64,
    pub severity_threshold: f64,
    /// Optional short non-sensitive label copied to provider events.
    #[serde(default)]
    pub evidence_label: Option<String>,
}

/// Resource limits applied before and during model execution.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLimits {
    pub max_model_bytes: u64,
    pub max_feature_frames: usize,
    pub max_feature_values: usize,
    pub max_events: usize,
    pub max_inference_seconds: f64,
    pub intra_threads: usize,
}

/// v1 is deliberately fail-closed.  A missing/invalid runtime or model is an
/// error, never an empty passing result that could hide a detector failure.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    Reject,
}

/// Versioned model manifest.  The model file itself is not embedded in Forge;
/// its bytes must match `model_sha256` at invocation time.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnnxModelManifest {
    pub schema: String,
    pub schema_version: u32,
    pub adapter: String,
    pub provider: String,
    pub provider_version: String,
    pub model: String,
    pub model_version: String,
    /// Expected basename of the explicitly supplied `.onnx` file.
    pub model_file: String,
    pub model_sha256: String,
    pub license: LicenseEvidence,
    pub calibration: CalibrationEvidence,
    pub input: TensorContract,
    pub output: TensorContract,
    pub classes: Vec<ModelClass>,
    pub frame_hop_seconds: f64,
    pub limits: RuntimeLimits,
    pub fallback: FallbackPolicy,
}

/// Feature frames supplied to the reference adapter.  Values are row-major
/// `[frames][feature_count]` float32 values and are bounded by the manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureFrames {
    pub schema: String,
    pub schema_version: u32,
    pub source_path: String,
    pub source_sha256: String,
    pub source_duration_seconds: f64,
    pub sample_rate_hz: u32,
    pub frame_hop_seconds: f64,
    pub feature_count: usize,
    pub frames: Vec<Vec<f32>>,
}

/// Load and validate an ONNX model manifest under the fixed byte limit.
pub fn load_manifest(path: &Path) -> Result<OnnxModelManifest, String> {
    let manifest: OnnxModelManifest = read_json(path, MAX_MANIFEST_BYTES, "model manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Validate all provenance, tensor, calibration, and resource fields in a
/// model manifest.
pub fn validate_manifest(manifest: &OnnxModelManifest) -> Result<(), String> {
    if manifest.schema != MODEL_SCHEMA {
        return Err(format!(
            "unsupported ONNX model schema {}; expected {MODEL_SCHEMA}",
            manifest.schema
        ));
    }
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported ONNX model schema version {}; expected {SCHEMA_VERSION}",
            manifest.schema_version
        ));
    }
    if manifest.adapter != ADAPTER {
        return Err(format!(
            "unsupported ONNX adapter {}; expected {ADAPTER}",
            manifest.adapter
        ));
    }
    for (label, value) in [
        ("provider", manifest.provider.as_str()),
        ("provider_version", manifest.provider_version.as_str()),
        ("model", manifest.model.as_str()),
        ("model_version", manifest.model_version.as_str()),
        ("model_file", manifest.model_file.as_str()),
        ("license.spdx", manifest.license.spdx.as_str()),
        ("license.dataset", manifest.license.dataset.as_str()),
    ] {
        validate_text(label, value, MAX_TEXT_LENGTH)?;
    }
    if manifest.model_file.contains('/')
        || manifest.model_file.contains('\\')
        || Path::new(&manifest.model_file).is_absolute()
        || manifest.model_file == "."
        || manifest.model_file == ".."
    {
        return Err("model_file must be a relative basename without path separators".into());
    }
    validate_sha256("model_sha256", &manifest.model_sha256)?;
    validate_url("license.url", &manifest.license.url)?;
    validate_url("license.dataset_url", &manifest.license.dataset_url)?;
    validate_sha256(
        "calibration.report_sha256",
        &manifest.calibration.report_sha256,
    )?;
    validate_url("calibration.report_url", &manifest.calibration.report_url)?;
    if manifest.fallback != FallbackPolicy::Reject {
        return Err("ONNX provider v1 only supports the fail-closed reject fallback".into());
    }

    validate_tensor_contract("input", &manifest.input)?;
    validate_tensor_contract("output", &manifest.output)?;
    if manifest.input.shape[0] != 1 || manifest.input.shape[1] != -1 {
        return Err("ONNX input shape must be [1, -1, feature_count]".into());
    }
    if manifest.output.shape[0] != 1 || manifest.output.shape[1] != -1 {
        return Err("ONNX output shape must be [1, -1, class_count * 2]".into());
    }
    let feature_count = positive_dimension(manifest.input.shape[2], "input feature count")?;
    let output_width = positive_dimension(manifest.output.shape[2], "output width")?;
    if manifest.classes.is_empty() || manifest.classes.len() > MAX_EVENTS {
        return Err("ONNX model must declare between 1 and 100000 output classes".into());
    }
    let expected_width = manifest
        .classes
        .len()
        .checked_mul(2)
        .ok_or_else(|| "ONNX output class count overflows output width".to_owned())?;
    if output_width != expected_width {
        return Err(format!(
            "ONNX output width is {output_width}; expected {expected_width} for {} classes",
            manifest.classes.len()
        ));
    }
    let mut kinds = BTreeMap::new();
    for (index, class) in manifest.classes.iter().enumerate() {
        validate_threshold(
            &format!("classes[{index}].confidence_threshold"),
            class.confidence_threshold,
        )?;
        validate_threshold(
            &format!("classes[{index}].severity_threshold"),
            class.severity_threshold,
        )?;
        if class.evidence_label.as_deref().is_some_and(|label| {
            label.is_empty()
                || label.chars().count() > MAX_LABEL_LENGTH
                || label.chars().any(char::is_control)
        }) {
            return Err(format!(
                "classes[{index}].evidence_label must be 1-{MAX_LABEL_LENGTH} printable characters"
            ));
        }
        if kinds.insert(class.kind, ()).is_some() {
            return Err(format!(
                "ONNX model declares duplicate output class {}",
                class.kind.as_str()
            ));
        }
    }
    if !manifest.frame_hop_seconds.is_finite()
        || manifest.frame_hop_seconds <= 0.0
        || manifest.frame_hop_seconds > 60.0
    {
        return Err("frame_hop_seconds must be finite, positive, and no more than 60".into());
    }
    if manifest.limits.max_model_bytes == 0 || manifest.limits.max_model_bytes > MAX_MODEL_BYTES {
        return Err(format!(
            "limits.max_model_bytes must be between 1 and {MAX_MODEL_BYTES}"
        ));
    }
    if manifest.limits.max_feature_frames == 0
        || manifest.limits.max_feature_frames > MAX_FEATURE_FRAMES
    {
        return Err(format!(
            "limits.max_feature_frames must be between 1 and {MAX_FEATURE_FRAMES}"
        ));
    }
    if manifest.limits.max_feature_values == 0
        || manifest.limits.max_feature_values > MAX_FEATURE_VALUES
        || manifest.limits.max_feature_values < feature_count
    {
        return Err(format!(
            "limits.max_feature_values must be at least {feature_count} and no more than {MAX_FEATURE_VALUES}"
        ));
    }
    if manifest.limits.max_events == 0 || manifest.limits.max_events > MAX_EVENTS {
        return Err(format!(
            "limits.max_events must be between 1 and {MAX_EVENTS}"
        ));
    }
    if !manifest.limits.max_inference_seconds.is_finite()
        || manifest.limits.max_inference_seconds <= 0.0
        || manifest.limits.max_inference_seconds > 3_600.0
    {
        return Err(
            "limits.max_inference_seconds must be finite, positive, and no more than 3600".into(),
        );
    }
    if manifest.limits.intra_threads == 0 || manifest.limits.intra_threads > 256 {
        return Err("limits.intra_threads must be between 1 and 256".into());
    }
    Ok(())
}

/// Load and validate feature frames against a model manifest's bounds.
pub fn load_features(path: &Path, manifest: &OnnxModelManifest) -> Result<FeatureFrames, String> {
    validate_manifest(manifest)?;
    let features: FeatureFrames = read_json(path, MAX_FEATURE_BYTES, "feature frames")?;
    validate_features(&features, manifest)?;
    Ok(features)
}

/// Validate feature provenance, shape, finite values, and decoded-resource
/// bounds before allocating an ONNX tensor.
pub fn validate_features(
    features: &FeatureFrames,
    manifest: &OnnxModelManifest,
) -> Result<(), String> {
    validate_manifest(manifest)?;
    if features.schema != FEATURE_SCHEMA {
        return Err(format!(
            "unsupported feature schema {}; expected {FEATURE_SCHEMA}",
            features.schema
        ));
    }
    if features.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported feature schema version {}; expected {SCHEMA_VERSION}",
            features.schema_version
        ));
    }
    validate_text("source_path", &features.source_path, MAX_TEXT_LENGTH * 2)?;
    validate_sha256("source_sha256", &features.source_sha256)?;
    if !features.source_duration_seconds.is_finite()
        || features.source_duration_seconds <= 0.0
        || features.source_duration_seconds > MAX_SOURCE_DURATION_SECONDS
    {
        return Err(format!(
            "source_duration_seconds must be finite, positive, and no more than {MAX_SOURCE_DURATION_SECONDS}"
        ));
    }
    if features.sample_rate_hz == 0 {
        return Err("sample_rate_hz must be positive".into());
    }
    if !features.frame_hop_seconds.is_finite() || features.frame_hop_seconds <= 0.0 {
        return Err("feature frame_hop_seconds must be finite and positive".into());
    }
    let hop_tolerance = 1e-9_f64.max(manifest.frame_hop_seconds.abs() * 1e-9);
    if (features.frame_hop_seconds - manifest.frame_hop_seconds).abs() > hop_tolerance {
        return Err(format!(
            "feature frame hop {} does not match manifest {}",
            features.frame_hop_seconds, manifest.frame_hop_seconds
        ));
    }
    let expected_feature_count =
        positive_dimension(manifest.input.shape[2], "input feature count")?;
    if features.feature_count != expected_feature_count {
        return Err(format!(
            "feature_count is {}; manifest requires {expected_feature_count}",
            features.feature_count
        ));
    }
    if features.frames.is_empty() {
        return Err("feature frames must not be empty".into());
    }
    if features.frames.len() > manifest.limits.max_feature_frames {
        return Err(format!(
            "feature frames contain {}; maximum is {}",
            features.frames.len(),
            manifest.limits.max_feature_frames
        ));
    }
    let values = features
        .frames
        .len()
        .checked_mul(features.feature_count)
        .ok_or_else(|| "feature value count overflows usize".to_owned())?;
    if values > manifest.limits.max_feature_values {
        return Err(format!(
            "feature frames contain {values} values; maximum is {}",
            manifest.limits.max_feature_values
        ));
    }
    let last_start = (features.frames.len() - 1) as f64 * features.frame_hop_seconds;
    if !last_start.is_finite() || last_start >= features.source_duration_seconds {
        return Err("last feature frame starts at or after source duration".into());
    }
    for (index, frame) in features.frames.iter().enumerate() {
        if frame.len() != features.feature_count {
            return Err(format!(
                "feature frame {} contains {}; expected {} values",
                index + 1,
                frame.len(),
                features.feature_count
            ));
        }
        if frame.iter().any(|value| !value.is_finite()) {
            return Err(format!(
                "feature frame {} contains a non-finite value",
                index + 1
            ));
        }
    }
    Ok(())
}

/// Convert a validated model output tensor (flattened row-major
/// `[1][frames][classes * 2]`) into bounded anomaly events.  This function is
/// public so a separately reviewed feature extractor can test the exact event
/// conversion without loading ONNX Runtime.
pub fn events_from_scores(
    features: &FeatureFrames,
    manifest: &OnnxModelManifest,
    scores: &[f32],
) -> Result<Vec<ProviderEvent>, String> {
    validate_features(features, manifest)?;
    let width = manifest
        .classes
        .len()
        .checked_mul(2)
        .ok_or_else(|| "ONNX output width overflows usize".to_owned())?;
    let expected = features
        .frames
        .len()
        .checked_mul(width)
        .ok_or_else(|| "ONNX output element count overflows usize".to_owned())?;
    if scores.len() != expected {
        return Err(format!(
            "ONNX output contains {}; expected {expected} values",
            scores.len()
        ));
    }

    let mut events: Vec<ProviderEvent> = Vec::new();
    let mut last_by_kind: BTreeMap<AnomalyKind, usize> = BTreeMap::new();
    for frame_index in 0..features.frames.len() {
        let start = frame_index as f64 * features.frame_hop_seconds;
        let end = features
            .source_duration_seconds
            .min(start + features.frame_hop_seconds);
        if end <= start {
            continue;
        }
        let row = &scores[frame_index * width..(frame_index + 1) * width];
        for (class_index, class) in manifest.classes.iter().enumerate() {
            let confidence = f64::from(row[class_index * 2]);
            let severity = f64::from(row[class_index * 2 + 1]);
            if !confidence.is_finite() || !severity.is_finite() {
                return Err(format!(
                    "ONNX output frame {} class {} contains a non-finite score",
                    frame_index + 1,
                    class.kind.as_str()
                ));
            }
            if !(0.0..=1.0).contains(&confidence) || !(0.0..=1.0).contains(&severity) {
                return Err(format!(
                    "ONNX output frame {} class {} scores must be between 0 and 1",
                    frame_index + 1,
                    class.kind.as_str()
                ));
            }
            if confidence < class.confidence_threshold || severity < class.severity_threshold {
                continue;
            }
            if let Some(previous_index) = last_by_kind.get(&class.kind).copied() {
                let previous = &mut events[previous_index];
                let contiguous = (previous.end_seconds - start).abs()
                    <= 1e-9_f64.max(features.frame_hop_seconds.abs() * 1e-9);
                if contiguous && previous.evidence_label == class.evidence_label {
                    previous.end_seconds = end;
                    previous.confidence = previous.confidence.max(confidence);
                    previous.severity = previous.severity.max(severity);
                    continue;
                }
            }
            if events.len() >= manifest.limits.max_events {
                return Err(format!(
                    "ONNX output would emit more than {} events",
                    manifest.limits.max_events
                ));
            }
            let index = events.len();
            events.push(ProviderEvent {
                kind: class.kind,
                start_seconds: start,
                end_seconds: end,
                confidence,
                severity,
                channel: None,
                related_channel: None,
                evidence_label: class.evidence_label.clone(),
            });
            last_by_kind.insert(class.kind, index);
        }
    }
    // Coalescing updates an earlier event after another class may already
    // have been appended at the same start time.  Restore the provider
    // contract's `(start_seconds, end_seconds)` ordering before returning.
    events.sort_by(|left, right| {
        left.start_seconds
            .total_cmp(&right.start_seconds)
            .then_with(|| left.end_seconds.total_cmp(&right.end_seconds))
    });
    Ok(events)
}

/// Run the explicitly selected ONNX Runtime model and return the existing
/// `audio-anomaly-provider-v1` input document.  Runtime loading is native-only
/// and process-global; callers must use one runtime library per process.
pub fn run(
    manifest_path: &Path,
    model_path: &Path,
    feature_path: &Path,
    runtime_library: &Path,
) -> Result<ProviderInput, String> {
    let manifest = load_manifest(manifest_path)?;
    let features = load_features(feature_path, &manifest)?;
    let model_name = model_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "ONNX model path must have a UTF-8 basename".to_owned())?;
    if model_name != manifest.model_file {
        return Err(format!(
            "ONNX model basename {model_name} does not match manifest model_file {}",
            manifest.model_file
        ));
    }
    let model_sha256 = hash_bounded_file(model_path, manifest.limits.max_model_bytes)?;
    if !model_sha256.eq_ignore_ascii_case(&manifest.model_sha256) {
        return Err(format!(
            "ONNX model SHA-256 {model_sha256} does not match manifest {}",
            manifest.model_sha256
        ));
    }
    let scores = run_model(&manifest, &features, model_path, runtime_library)?;
    let events = events_from_scores(&features, &manifest, &scores)?;
    Ok(ProviderInput {
        schema_version: crate::anomaly_provider::SCHEMA_VERSION,
        provider: manifest.provider,
        provider_version: manifest.provider_version,
        model: manifest.model,
        model_version: manifest.model_version,
        model_sha256: manifest.model_sha256.to_ascii_lowercase(),
        source_sha256: features.source_sha256.to_ascii_lowercase(),
        source_duration_seconds: features.source_duration_seconds,
        sample_rate_hz: Some(features.sample_rate_hz),
        events,
    })
}

#[cfg(target_arch = "wasm32")]
fn run_model(
    _manifest: &OnnxModelManifest,
    _features: &FeatureFrames,
    _model_path: &Path,
    _runtime_library: &Path,
) -> Result<Vec<f32>, String> {
    Err("ONNX provider is not available on wasm32; use a native adapter".into())
}

#[cfg(not(target_arch = "wasm32"))]
fn run_model(
    manifest: &OnnxModelManifest,
    features: &FeatureFrames,
    model_path: &Path,
    runtime_library: &Path,
) -> Result<Vec<f32>, String> {
    use ndarray::Array3;
    use ort::session::{RunOptions, Session};
    use ort::value::TensorRef;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    let environment = ort::init_from(runtime_library)
        .map_err(|error| format!("load ONNX Runtime {}: {error}", runtime_library.display()))?;
    if !environment.commit() {
        return Err(
            "ONNX Runtime was already initialized in this process; use one runtime library per process"
                .into(),
        );
    }
    let mut builder =
        Session::builder().map_err(|error| format!("create ONNX session: {error}"))?;
    builder = builder
        .with_intra_threads(manifest.limits.intra_threads)
        .map_err(|error| format!("configure ONNX intra-op threads: {error}"))?;
    builder = builder
        .with_inter_threads(1)
        .map_err(|error| format!("configure ONNX inter-op threads: {error}"))?;
    let mut session = builder
        .commit_from_file(model_path)
        .map_err(|error| format!("load ONNX model {}: {error}", model_path.display()))?;
    if session.inputs().len() != 1 || session.outputs().len() != 1 {
        return Err(format!(
            "ONNX model must expose exactly one input and one output (got {} and {})",
            session.inputs().len(),
            session.outputs().len()
        ));
    }
    validate_runtime_tensor(
        "input",
        session.inputs()[0].name(),
        session.inputs()[0].dtype(),
        &manifest.input,
    )?;
    validate_runtime_tensor(
        "output",
        session.outputs()[0].name(),
        session.outputs()[0].dtype(),
        &manifest.output,
    )?;

    let flat: Vec<f32> = features
        .frames
        .iter()
        .flat_map(|frame| frame.iter().copied())
        .collect();
    let input = Array3::from_shape_vec((1, features.frames.len(), features.feature_count), flat)
        .map_err(|error| format!("construct ONNX input tensor: {error}"))?;
    let input = TensorRef::from_array_view(&input)
        .map_err(|error| format!("construct ONNX input value: {error}"))?;
    let options =
        Arc::new(RunOptions::new().map_err(|error| format!("create ONNX run options: {error}"))?);
    let (stop_sender, stop_receiver) = mpsc::channel();
    let timer_options = Arc::clone(&options);
    let timeout = Duration::from_secs_f64(manifest.limits.max_inference_seconds);
    let timer = thread::spawn(move || {
        if stop_receiver.recv_timeout(timeout).is_err() {
            let _ = timer_options.terminate();
        }
    });
    let started = Instant::now();
    let result = session.run_with_options(
        ort::inputs![manifest.input.name.as_str() => input],
        options.as_ref(),
    );
    let _ = stop_sender.send(());
    let _ = timer.join();
    let outputs = result.map_err(|error| format!("run ONNX model: {error}"))?;
    if started.elapsed().as_secs_f64() > manifest.limits.max_inference_seconds {
        return Err(format!(
            "ONNX inference exceeded {:.3}s limit",
            manifest.limits.max_inference_seconds
        ));
    }
    let output = outputs
        .get(&manifest.output.name)
        .ok_or_else(|| format!("ONNX output {} was not returned", manifest.output.name))?;
    let (shape, data) = output
        .try_extract_tensor::<f32>()
        .map_err(|error| format!("extract ONNX output tensor: {error}"))?;
    let expected_width = manifest.classes.len() * 2;
    if shape.len() != 3
        || shape[0] != 1
        || shape[1] != features.frames.len() as i64
        || shape[2] != expected_width as i64
    {
        return Err(format!(
            "ONNX output shape {shape} does not match [1, {}, {expected_width}]",
            features.frames.len()
        ));
    }
    Ok(data.to_vec())
}

#[cfg(not(target_arch = "wasm32"))]
fn validate_runtime_tensor(
    direction: &str,
    actual_name: &str,
    actual_type: &ort::value::ValueType,
    expected: &TensorContract,
) -> Result<(), String> {
    use ort::value::ValueType;

    if actual_name != expected.name {
        return Err(format!(
            "ONNX {direction} name is {actual_name}; manifest requires {}",
            expected.name
        ));
    }
    let ValueType::Tensor {
        ty: ort::value::TensorElementType::Float32,
        shape,
        ..
    } = actual_type
    else {
        return Err(format!(
            "ONNX {direction} {} must be an f32 tensor",
            expected.name
        ));
    };
    if shape.len() != expected.shape.len()
        || expected
            .shape
            .iter()
            .zip(shape.iter())
            .any(|(wanted, actual)| *wanted >= 0 && *actual >= 0 && wanted != actual)
    {
        return Err(format!(
            "ONNX {direction} shape {shape} does not match manifest {:?}",
            expected.shape
        ));
    }
    Ok(())
}

fn validate_tensor_contract(label: &str, tensor: &TensorContract) -> Result<(), String> {
    validate_text(&format!("{label}.name"), &tensor.name, MAX_TEXT_LENGTH)?;
    if tensor.shape.len() != 3 {
        return Err(format!("{label}.shape must have rank 3"));
    }
    if tensor.shape[0] < -1 || tensor.shape[1] < -1 || tensor.shape[2] <= 0 {
        return Err(format!("{label}.shape dimensions must be -1 or positive"));
    }
    Ok(())
}

fn positive_dimension(value: i64, label: &str) -> Result<usize, String> {
    if value <= 0 {
        return Err(format!("{label} must be a positive fixed dimension"));
    }
    usize::try_from(value).map_err(|_| format!("{label} does not fit in usize"))
}

fn validate_threshold(label: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("{label} must be between 0 and 1"));
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.trim().is_empty()
        || value.chars().count() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(format!("{label} must be 1-{maximum} printable characters"));
    }
    Ok(())
}

fn validate_url(label: &str, value: &str) -> Result<(), String> {
    validate_text(label, value, MAX_TEXT_LENGTH)?;
    if !value.starts_with("https://") {
        return Err(format!("{label} must use an https URL"));
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "{label} must contain exactly 64 hexadecimal digits"
        ));
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(
    path: &Path,
    maximum_bytes: usize,
    description: &str,
) -> Result<T, String> {
    let bytes = fs::metadata(path)
        .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?
        .len();
    if bytes > maximum_bytes as u64 {
        return Err(format!(
            "{description} {} is {bytes} bytes; maximum is {maximum_bytes}",
            path.display()
        ));
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("read {description} {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {description} {}: {error}", path.display()))
}

fn hash_bounded_file(path: &Path, maximum_bytes: u64) -> Result<String, String> {
    let bytes = fs::metadata(path)
        .map_err(|error| format!("inspect ONNX model {}: {error}", path.display()))?
        .len();
    if bytes == 0 || bytes > maximum_bytes || bytes > MAX_MODEL_BYTES {
        return Err(format!(
            "ONNX model is {bytes} bytes; allowed range is 1..={maximum_bytes}"
        ));
    }
    let mut file =
        File::open(path).map_err(|error| format!("open ONNX model {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("hash ONNX model {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let mut result = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut result, "{byte:02x}").expect("writing a digest cannot fail");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> OnnxModelManifest {
        OnnxModelManifest {
            schema: MODEL_SCHEMA.into(),
            schema_version: SCHEMA_VERSION,
            adapter: ADAPTER.into(),
            provider: "reference-onnx".into(),
            provider_version: "1.0".into(),
            model: "quality-model".into(),
            model_version: "2026-08".into(),
            model_file: "quality.onnx".into(),
            model_sha256: "a".repeat(64),
            license: LicenseEvidence {
                spdx: "MIT".into(),
                url: "https://example.test/model-license".into(),
                dataset: "reviewed-audio-v1".into(),
                dataset_url: "https://example.test/dataset".into(),
            },
            calibration: CalibrationEvidence {
                report_sha256: "b".repeat(64),
                report_url: "https://example.test/calibration".into(),
            },
            input: TensorContract {
                name: "features".into(),
                shape: vec![1, -1, 2],
            },
            output: TensorContract {
                name: "scores".into(),
                shape: vec![1, -1, 4],
            },
            classes: vec![
                ModelClass {
                    kind: AnomalyKind::Pop,
                    confidence_threshold: 0.6,
                    severity_threshold: 0.5,
                    evidence_label: Some("impulse-spectrum".into()),
                },
                ModelClass {
                    kind: AnomalyKind::Noise,
                    confidence_threshold: 0.7,
                    severity_threshold: 0.5,
                    evidence_label: None,
                },
            ],
            frame_hop_seconds: 0.1,
            limits: RuntimeLimits {
                max_model_bytes: 16 * 1024 * 1024,
                max_feature_frames: 100,
                max_feature_values: 10_000,
                max_events: 100,
                max_inference_seconds: 10.0,
                intra_threads: 1,
            },
            fallback: FallbackPolicy::Reject,
        }
    }

    fn features() -> FeatureFrames {
        FeatureFrames {
            schema: FEATURE_SCHEMA.into(),
            schema_version: SCHEMA_VERSION,
            source_path: "programme.wav".into(),
            source_sha256: "c".repeat(64),
            source_duration_seconds: 1.0,
            sample_rate_hz: 48_000,
            frame_hop_seconds: 0.1,
            feature_count: 2,
            frames: vec![vec![0.0, 1.0], vec![0.1, 0.9], vec![0.0, 0.0]],
        }
    }

    #[test]
    fn validates_provenance_and_shape_contract() {
        let value = manifest();
        validate_manifest(&value).unwrap();
        let mut invalid = value;
        invalid.output.shape[2] = 2;
        assert!(validate_manifest(&invalid).is_err());
    }

    #[test]
    fn validates_feature_bounds_before_tensor_creation() {
        let value = manifest();
        validate_features(&features(), &value).unwrap();
        let mut invalid = features();
        invalid.frames[1].push(0.0);
        assert!(validate_features(&invalid, &value).is_err());
    }

    #[test]
    fn converts_scores_to_coalesced_bounded_events() {
        let value = manifest();
        let scores = vec![
            0.9, 0.8, 0.1, 0.1, // pop, noise
            0.95, 0.7, 0.1, 0.2, // pop continues
            0.0, 0.0, 0.0, 0.0,
        ];
        let events = events_from_scores(&features(), &value, &scores).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, AnomalyKind::Pop);
        assert_eq!(events[0].start_seconds, 0.0);
        assert_eq!(events[0].end_seconds, 0.2);
        assert!((events[0].confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn rejects_non_finite_model_scores() {
        let value = manifest();
        let mut scores = vec![0.0; 12];
        scores[0] = f32::NAN;
        assert!(events_from_scores(&features(), &value, &scores).is_err());
    }

    #[test]
    fn sorts_overlapping_classes_after_one_class_is_coalesced() {
        let value = manifest();
        let scores = vec![
            0.9, 0.8, 0.8, 0.8, // pop and noise at frame zero
            0.95, 0.7, 0.0, 0.0, // only pop continues
            0.0, 0.0, 0.0, 0.0,
        ];
        let events = events_from_scores(&features(), &value, &scores).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].start_seconds <= events[1].start_seconds);
        assert!(events[0].end_seconds <= events[1].end_seconds);
        let input = ProviderInput {
            schema_version: crate::anomaly_provider::SCHEMA_VERSION,
            provider: value.provider,
            provider_version: value.provider_version,
            model: value.model,
            model_version: value.model_version,
            model_sha256: value.model_sha256,
            source_sha256: features().source_sha256,
            source_duration_seconds: features().source_duration_seconds,
            sample_rate_hz: Some(features().sample_rate_hz),
            events,
        };
        let audit = crate::anomaly_provider::audit(Path::new("programme.wav"), input, 0.6, 0.5);
        assert!(audit.is_ok());
    }

    #[test]
    fn manifest_round_trips_without_losing_evidence() {
        let value = manifest();
        let json = serde_json::to_value(&value).unwrap();
        let decoded: OnnxModelManifest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.calibration.report_sha256, "b".repeat(64));
        validate_manifest(&decoded).unwrap();
    }
}
