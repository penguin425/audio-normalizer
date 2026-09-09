//! Durable state for resumable, content-bound normalization batches.
//!
//! A job state is committed atomically after every completed output. Inputs,
//! output paths, the operation descriptor, and completed outputs are bound by
//! SHA-256 so a resume never silently reuses work from different bytes or
//! settings.

use crate::atomic::AtomicOutput;
use crate::output_plan::physical_route_key;
use crate::stable_input::{
    identity_from_open_file, path_identity_if_exists, InputContentBinding, StableFileIdentity,
};
use crate::state_lock::{read_regular_state_file, sibling_lock_path, StateFileLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{Read, Write as IoWrite};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const BATCH_JOB_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-job-v1";
pub const BATCH_JOB_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-job-v2";
pub const BATCH_PROGRESS_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-progress-v1";
pub const BATCH_JOB_SCHEMA_V3: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-job-v3";
pub const BATCH_PROGRESS_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-progress-v2";
pub const BATCH_FAILURE_REPORT_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-failure-report-v1";
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ASSETS: usize = 100_000;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_GENERATOR_BYTES: usize = 256;
const MAX_BATCH_PATH_BYTES: usize = 4096;
const MAX_BATCH_PROGRESS_PHASE_BYTES: usize = 64;
pub const MAX_BATCH_FAILURES: usize = 4096;
pub const MAX_BATCH_FAILURE_ERROR_BYTES: usize = 16 * 1024;
pub const MAX_BATCH_FAILURE_REPORT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SEMANTIC_CONTEXT_DEPTH: usize = 64;
// One node is needed for every ordered output-format string in the supported
// 100,000-asset batch, in addition to the fixed semantic evidence envelope.
pub const MAX_SEMANTIC_CONTEXT_NODES: usize = MAX_ASSETS + 1_024;
pub const MAX_SEMANTIC_CONTEXT_BYTES: usize = 4 * 1024 * 1024;

/// Policy used when an independent batch asset fails.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchFailurePolicy {
    /// Stop after the first asset failure.
    #[default]
    FailFast,
    /// Continue independent assets and report failures at the end.
    KeepGoing,
}

/// One input/output pair included in a resumable job.
#[derive(Debug, Clone)]
pub struct BatchAssetSpec {
    input: PathBuf,
    output: PathBuf,
}

impl BatchAssetSpec {
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AssetStatus {
    Pending,
    ReadyToPublish,
    Completed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JobAsset {
    input: String,
    output: String,
    input_sha256: String,
    status: AssetStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JobDocument {
    schema: String,
    generator: String,
    operation: Value,
    specification_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint_revision: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_context: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_policy: Option<BatchFailurePolicy>,
    asset_count: usize,
    completed_count: usize,
    assets: Vec<JobAsset>,
}

/// A validated resumable job backed by an atomically updated JSON document.
#[derive(Debug)]
pub struct BatchJob {
    path: PathBuf,
    document: JobDocument,
    _state_lock: StateFileLock,
}

impl BatchJob {
    /// Create a new state document or validate and resume an existing one.
    ///
    /// When `reset_changed_outputs` is true, missing or modified completed
    /// outputs are returned to `pending`; otherwise modified outputs are an
    /// error. Missing outputs are always returned to `pending`.
    pub fn open(
        path: impl Into<PathBuf>,
        assets: &[BatchAssetSpec],
        operation: &Value,
        reset_changed_outputs: bool,
    ) -> Result<Self, String> {
        if assets.is_empty() {
            return Err("a resumable batch requires at least one asset".into());
        }
        if assets.len() > MAX_ASSETS {
            return Err(format!(
                "resumable batch exceeds the {MAX_ASSETS}-asset limit"
            ));
        }
        let path = absolute_path(&path.into())?;
        let state_lock = StateFileLock::acquire(&path, "batch state")?;
        let expected_assets = build_assets(assets)?;
        reject_duplicate_paths(&expected_assets)?;
        let specification_sha256 = specification_hash(operation, &expected_assets)?;
        let generator = format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION"));

        let existing = read_regular_state_file(&path, "batch state", MAX_STATE_BYTES)?;
        let mut job = if let Some(bytes) = existing {
            let document: JobDocument = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode {}: {error}", path.display()))?;
            validate_document(
                &document,
                operation,
                &specification_sha256,
                &expected_assets,
            )?;
            Self {
                path,
                document,
                _state_lock: state_lock,
            }
        } else {
            let document = JobDocument {
                schema: BATCH_JOB_SCHEMA_V2.into(),
                generator,
                operation: operation.clone(),
                specification_sha256,
                fingerprint_revision: None,
                semantic_fingerprint: None,
                semantic_context: None,
                job_id: None,
                failure_policy: None,
                asset_count: expected_assets.len(),
                completed_count: 0,
                assets: expected_assets,
            };
            let job = Self {
                path,
                document,
                _state_lock: state_lock,
            };
            job.save()?;
            job
        };

        let mut changed = false;
        if job.document.schema == BATCH_JOB_SCHEMA_V1 {
            job.document.schema = BATCH_JOB_SCHEMA_V2.into();
            changed = true;
        }
        for asset in &mut job.document.assets {
            if asset.status == AssetStatus::Pending {
                continue;
            }
            let output = Path::new(&asset.output);
            if !output.is_file() {
                asset.status = AssetStatus::Pending;
                asset.output_sha256 = None;
                changed = true;
                continue;
            }
            let actual = hash_file(output)?;
            if asset.output_sha256.as_deref() != Some(actual.as_str()) {
                if !reset_changed_outputs {
                    return Err(format!(
                        "completed output changed since checkpoint: {} (use --overwrite to rebuild it)",
                        output.display()
                    ));
                }
                asset.status = AssetStatus::Pending;
                asset.output_sha256 = None;
                changed = true;
            } else if asset.status == AssetStatus::ReadyToPublish {
                // Publication completed but the process exited before it
                // could commit the final Completed checkpoint.
                asset.status = AssetStatus::Completed;
                changed = true;
            }
        }
        if changed {
            job.recount();
            job.save()?;
        }
        Ok(job)
    }

    /// Create or resume a semantic-fingerprint-bound v3 job.
    ///
    /// v3 deliberately refuses v1/v2 state documents.  A caller must provide
    /// the complete semantic context it used for preflight (for example the
    /// analysis revision, selected input track/decoder, and output writer
    /// identity).  The context is validated and canonically encoded before a
    /// state file or lock is created.
    pub fn open_v3(
        path: impl Into<PathBuf>,
        assets: &[BatchAssetSpec],
        operation: &Value,
        semantic_context: &Value,
        fingerprint_revision: u32,
        failure_policy: BatchFailurePolicy,
        reset_changed_outputs: bool,
    ) -> Result<Self, String> {
        validate_batch_assets_count(assets)?;
        let context_bytes = canonical_json_bytes(
            semantic_context,
            MAX_SEMANTIC_CONTEXT_DEPTH,
            MAX_SEMANTIC_CONTEXT_NODES,
            MAX_SEMANTIC_CONTEXT_BYTES,
            "semantic context",
        )?;
        let canonical_semantic_context: Value = serde_json::from_slice(&context_bytes)
            .map_err(|error| format!("decode canonical semantic context: {error}"))?;
        validate_normalization_semantic_context(&canonical_semantic_context)?;
        let semantic_fingerprint =
            semantic_fingerprint(&canonical_semantic_context, fingerprint_revision)?;
        let path = normalized_absolute_path(&path.into())?;
        let expected_assets = build_assets_v3(assets)?;
        reject_duplicate_v3_paths(&expected_assets)?;
        validate_v3_operation(operation)?;
        validate_v3_semantic_alignment(
            operation,
            &canonical_semantic_context,
            expected_assets.len(),
        )?;
        reject_v3_state_aliases(&path, &expected_assets)?;
        let specification_sha256 = specification_hash_v3(operation, &expected_assets)?;
        let job_id = job_id(
            operation,
            &expected_assets,
            &semantic_fingerprint,
            fingerprint_revision,
            failure_policy,
        )?;
        let generator = format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION"));
        let state_lock = StateFileLock::acquire(&path, "batch state")?;
        let existing = read_regular_state_file(&path, "batch state", MAX_STATE_BYTES)?;
        let mut job = if let Some(bytes) = existing {
            let document: JobDocument = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode {}: {error}", path.display()))?;
            if document.schema != BATCH_JOB_SCHEMA_V3 {
                return Err(format!(
                    "batch v3 refuses legacy state schema {}; create a new v3 state or explicitly migrate it",
                    document.schema
                ));
            }
            validate_document_v3(
                &document,
                operation,
                &specification_sha256,
                &expected_assets,
                &job_id,
                &semantic_fingerprint,
                &canonical_semantic_context,
                fingerprint_revision,
                failure_policy,
            )?;
            Self {
                path,
                document,
                _state_lock: state_lock,
            }
        } else {
            let document = JobDocument {
                schema: BATCH_JOB_SCHEMA_V3.into(),
                generator,
                operation: operation.clone(),
                specification_sha256,
                fingerprint_revision: Some(fingerprint_revision),
                semantic_fingerprint: Some(semantic_fingerprint),
                semantic_context: Some(canonical_semantic_context),
                job_id: Some(job_id),
                failure_policy: Some(failure_policy),
                asset_count: expected_assets.len(),
                completed_count: 0,
                assets: expected_assets,
            };
            let job = Self {
                path,
                document,
                _state_lock: state_lock,
            };
            job.save()?;
            job
        };

        let mut reset_generation = false;
        if job.document.completed_count == job.document.asset_count {
            for asset in &job.document.assets {
                let output = Path::new(&asset.output);
                let mismatch = if output.is_file() {
                    let actual = hash_file(output)?;
                    asset.output_sha256.as_deref() != Some(actual.as_str())
                } else {
                    true
                };
                if mismatch {
                    if !reset_changed_outputs {
                        return Err(format!(
                            "completed generation output changed or disappeared since checkpoint: {} (use --overwrite to rebuild the whole generation)",
                            output.display()
                        ));
                    }
                    reset_generation = true;
                }
            }
        }
        if reset_generation {
            for asset in &mut job.document.assets {
                asset.status = AssetStatus::Pending;
                asset.output_sha256 = None;
            }
            job.recount();
            job.save()?;
        }
        Ok(job)
    }

    pub fn asset_count(&self) -> usize {
        self.document.asset_count
    }

    pub fn completed_count(&self) -> usize {
        self.document.completed_count
    }

    /// Return the v3 whole-job identity, if this is a semantic-fingerprint
    /// state. Legacy v1/v2 jobs intentionally return `None`.
    pub fn job_id(&self) -> Option<&str> {
        self.document.job_id.as_deref()
    }

    pub fn semantic_fingerprint(&self) -> Option<&str> {
        self.document.semantic_fingerprint.as_deref()
    }

    pub fn fingerprint_revision(&self) -> Option<u32> {
        self.document.fingerprint_revision
    }

    pub fn semantic_context(&self) -> Option<&Value> {
        self.document.semantic_context.as_ref()
    }

    pub fn failure_policy(&self) -> Option<BatchFailurePolicy> {
        self.document.failure_policy
    }

    pub fn is_completed(&self, index: usize) -> bool {
        self.document
            .assets
            .get(index)
            .is_some_and(|asset| asset.status == AssetStatus::Completed)
    }

    /// Whether every asset in the job has been published and checkpointed.
    pub fn is_complete(&self) -> bool {
        self.document.completed_count == self.document.asset_count
    }

    /// Verify that an immutable input snapshot still belongs to this job's
    /// content-bound asset at `index`.
    pub fn verify_input_binding(
        &self,
        index: usize,
        binding: &InputContentBinding,
    ) -> Result<(), String> {
        let asset = self
            .document
            .assets
            .get(index)
            .ok_or_else(|| format!("batch asset index {index} is out of range"))?;
        if binding.sha256_hex() != asset.input_sha256 {
            return Err(format!(
                "batch input changed after the job fingerprint was captured: {}",
                asset.input
            ));
        }
        Ok(())
    }

    /// Commit a v3 checkpoint only from a committed generation whose identity
    /// and every live output match this batch job.
    pub fn mark_generation_completed_with_evidence(
        &mut self,
        status: &crate::generation::GenerationStatus,
    ) -> Result<(), String> {
        if self.document.schema != BATCH_JOB_SCHEMA_V3 {
            return Err("whole-generation completion requires a batch-job-v3 state".into());
        }
        if status.phase() != crate::generation::GenerationPhase::Committed {
            return Err("batch completion requires a committed generation journal".into());
        }
        if Some(status.semantic_fingerprint()) != self.document.job_id.as_deref() {
            return Err("generation identity does not match the batch job".into());
        }
        let evidence = status.outputs();
        if evidence.len() != self.document.assets.len() {
            return Err("generation evidence count does not match batch assets".into());
        }
        let mut expected = std::collections::BTreeMap::new();
        for item in evidence {
            let key = physical_route_key(item.destination())?;
            if expected.insert(key, item.sha256()).is_some() {
                return Err(format!(
                    "duplicate destination in generation evidence: {}",
                    item.destination().display()
                ));
            }
        }
        // Establish that the journal covers exactly the batch outputs before
        // inspecting any live destination evidence.
        for asset in &self.document.assets {
            let output = Path::new(&asset.output);
            let key = physical_route_key(output)?;
            if !expected.contains_key(&key) {
                return Err(format!(
                    "generation evidence does not contain batch output: {}",
                    output.display()
                ));
            }
        }
        for item in evidence {
            item.verify_live_destination()?;
        }
        let output_hashes = self
            .document
            .assets
            .iter()
            .map(|asset| {
                let output = Path::new(&asset.output);
                let key = physical_route_key(output)?;
                let expected_hash = expected.get(&key).ok_or_else(|| {
                    format!(
                        "generation evidence does not contain batch output: {}",
                        output.display()
                    )
                })?;
                let actual = hash_file(output)?;
                if actual != **expected_hash {
                    return Err(format!(
                        "published output differs from generation evidence: {}",
                        output.display()
                    ));
                }
                Ok(actual)
            })
            .collect::<Result<Vec<_>, String>>()?;
        if self
            .document
            .assets
            .iter()
            .zip(&output_hashes)
            .all(|(asset, output_sha256)| {
                asset.status == AssetStatus::Completed
                    && asset.output_sha256.as_deref() == Some(output_sha256.as_str())
            })
        {
            // A completed checkpoint may be reconciled again on a later
            // invocation to revalidate the generation's stronger live-file
            // evidence. Avoid rewriting an already identical durable state.
            return Ok(());
        }
        for (asset, output_sha256) in self.document.assets.iter_mut().zip(output_hashes) {
            asset.output_sha256 = Some(output_sha256);
            asset.status = AssetStatus::Completed;
        }
        self.recount();
        self.save()
    }

    /// Explicitly reset a fully completed v3 generation before an authorized
    /// whole-generation rebuild.  This is intentionally narrower than the
    /// ordinary `--overwrite` behavior: a partial job, a legacy job, or a job
    /// whose policy does not belong to v3 cannot be reset through this method.
    pub fn reset_completed_generation_for_rebuild(&mut self) -> Result<(), String> {
        if self.document.schema != BATCH_JOB_SCHEMA_V3 {
            return Err("completed-generation rebuild reset requires a batch-job-v3 state".into());
        }
        if self.document.failure_policy.is_none()
            || self.document.job_id.is_none()
            || self.document.semantic_fingerprint.is_none()
        {
            return Err("batch-job-v3 state is missing its generation identity or policy".into());
        }
        if !self.is_complete()
            || self
                .document
                .assets
                .iter()
                .any(|asset| asset.status != AssetStatus::Completed)
        {
            return Err(
                "completed-generation rebuild reset requires all v3 assets to be completed".into(),
            );
        }
        for asset in &mut self.document.assets {
            asset.status = AssetStatus::Pending;
            asset.output_sha256 = None;
        }
        self.recount();
        self.save()
    }

    /// Hash a fully rendered sibling stage and durably record that the next
    /// operation is publication of those exact bytes.
    pub fn mark_ready_to_publish(&mut self, index: usize, staged: &Path) -> Result<(), String> {
        if self.document.schema == BATCH_JOB_SCHEMA_V3 {
            return Err(
                "batch-job-v3 delegates readiness to its generation journal; per-asset readiness is forbidden"
                    .into(),
            );
        }
        let staged_sha256 = hash_file(staged)?;
        let asset = self
            .document
            .assets
            .get_mut(index)
            .ok_or_else(|| format!("batch asset index {index} is out of range"))?;
        if asset.status == AssetStatus::Completed {
            return Err(format!("batch asset {index} is already completed"));
        }
        asset.output_sha256 = Some(staged_sha256);
        asset.status = AssetStatus::ReadyToPublish;
        self.recount();
        self.save()
    }

    /// Hash the completed output and atomically commit the updated checkpoint.
    pub fn mark_completed(&mut self, index: usize) -> Result<(), String> {
        if self.document.schema == BATCH_JOB_SCHEMA_V3 {
            return Err("batch-job-v3 can only checkpoint a complete committed generation".into());
        }
        let asset = self
            .document
            .assets
            .get_mut(index)
            .ok_or_else(|| format!("batch asset index {index} is out of range"))?;
        let output = Path::new(&asset.output);
        if !output.is_file() {
            return Err(format!(
                "cannot checkpoint missing output: {}",
                output.display()
            ));
        }
        asset.output_sha256 = Some(hash_file(output)?);
        asset.status = AssetStatus::Completed;
        self.recount();
        self.save()
    }

    fn recount(&mut self) {
        self.document.completed_count = self
            .document
            .assets
            .iter()
            .filter(|asset| asset.status == AssetStatus::Completed)
            .count();
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let mut bytes = serde_json::to_vec_pretty(&self.document)
            .map_err(|error| format!("encode batch state: {error}"))?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(format!(
                "batch state exceeds the {MAX_STATE_BYTES}-byte limit"
            ));
        }
        let mut output = AtomicOutput::new(&self.path)?;
        output.write_all(&bytes)?;
        output.commit()
    }
}

fn build_assets(specs: &[BatchAssetSpec]) -> Result<Vec<JobAsset>, String> {
    specs
        .iter()
        .map(|spec| {
            let input = std::fs::canonicalize(&spec.input)
                .map_err(|error| format!("canonicalize {}: {error}", spec.input.display()))?;
            let output = absolute_path(&spec.output)?;
            let input_text = path_text(&input)?;
            let output_text = path_text(&output)?;
            Ok(JobAsset {
                input: input_text,
                output: output_text,
                input_sha256: hash_file(&input)?,
                status: AssetStatus::Pending,
                output_sha256: None,
            })
        })
        .collect()
}

/// Build v3 assets with the same lexical path meaning used by the generation
/// journal.  `std::path::absolute` intentionally preserves `.` and `..`,
/// while generation journals persist a normalized sibling path; retaining the
/// former here would make an otherwise matching generation look unrelated at
/// the evidence boundary.
fn build_assets_v3(specs: &[BatchAssetSpec]) -> Result<Vec<JobAsset>, String> {
    specs
        .iter()
        .map(|spec| {
            let input = std::fs::canonicalize(&spec.input)
                .map_err(|error| format!("canonicalize {}: {error}", spec.input.display()))?;
            let output = normalized_absolute_path(&spec.output)?;
            let input_text = path_text(&input)?;
            let output_text = path_text(&output)?;
            Ok(JobAsset {
                input: input_text,
                output: output_text,
                input_sha256: hash_file(&input)?,
                status: AssetStatus::Pending,
                output_sha256: None,
            })
        })
        .collect()
}

/// Validate the public normalization-semantic-context-v1 envelope before it
/// can participate in a v3 job identity.  The context is intentionally a
/// `serde_json::Value` at the API boundary, so relying on canonical encoding
/// alone would allow a caller to persist an incomplete or unknown evidence
/// shape that no normalizer invocation can produce.
fn validate_normalization_semantic_context(value: &Value) -> Result<(), String> {
    let object = require_semantic_object(
        value,
        "normalization semantic context",
        &[
            "schema",
            "revision",
            "bound_analysis_version",
            "measurement_algorithm_revision",
            "input_content_binding_version",
            "analysis_engine",
            "analysis_engine_id",
            "decoder",
            "input_descriptor_semantic_revision",
            "audio_track",
            "audio_track_selection",
            "normalization",
            "output_formats",
            "writers",
        ],
    )?;
    require_semantic_string(
        object,
        "schema",
        256,
        Some(crate::runtime_fingerprint::NORMALIZATION_SEMANTIC_CONTEXT_SCHEMA),
    )?;
    require_semantic_u32(object, "revision", 1, Some(1))?;
    require_semantic_u32(object, "bound_analysis_version", 1, None)?;
    require_semantic_string(object, "measurement_algorithm_revision", 128, None)?;
    require_semantic_u32(object, "input_content_binding_version", 1, None)?;

    let analysis_engine = require_semantic_object(
        object
            .get("analysis_engine")
            .expect("required semantic context field was checked"),
        "semantic context analysis_engine",
        &["id"],
    )?;
    let nested_engine_id = require_semantic_string(analysis_engine, "id", 128, None)?;
    require_known_engine_id(nested_engine_id)?;
    let engine_id = require_semantic_string(object, "analysis_engine_id", 128, None)?;
    require_known_engine_id(engine_id)?;
    if nested_engine_id != engine_id {
        return Err("semantic context analysis engine IDs do not match".into());
    }

    let decoder = require_semantic_object(
        object
            .get("decoder")
            .expect("required semantic context field was checked"),
        "semantic context decoder",
        &["semantic_revision", "input_descriptor_version"],
    )?;
    let decoder_revision = require_semantic_string(decoder, "semantic_revision", 128, None)?;
    let descriptor_revision =
        require_semantic_string(object, "input_descriptor_semantic_revision", 128, None)?;
    if decoder_revision != descriptor_revision {
        return Err("semantic context decoder revisions do not match".into());
    }
    require_semantic_u32(decoder, "input_descriptor_version", 1, None)?;

    let audio_track = require_optional_semantic_u32(object, "audio_track")?;
    let track_selection = require_semantic_object(
        object
            .get("audio_track_selection")
            .expect("required semantic context field was checked"),
        "semantic context audio_track_selection",
        &["kind", "index"],
    )?;
    let selection_kind = require_semantic_string(track_selection, "kind", 16, None)?;
    let selection_index = require_optional_semantic_u32(track_selection, "index")?;
    match selection_kind {
        "default" => {
            if selection_index.is_some() || audio_track.is_some() {
                return Err(
                    "semantic context default audio-track selection must use null indices".into(),
                );
            }
        }
        "index" => {
            if selection_index.is_none() || selection_index != audio_track {
                return Err(
                    "semantic context indexed audio-track selection does not match audio_track"
                        .into(),
                );
            }
        }
        _ => {
            return Err(format!(
                "unknown semantic context audio-track kind: {selection_kind}"
            ))
        }
    }

    let normalization = require_semantic_object(
        object
            .get("normalization")
            .expect("required semantic context field was checked"),
        "semantic context normalization",
        &["pipeline_revision"],
    )?;
    require_semantic_string(normalization, "pipeline_revision", 128, None)?;

    let output_formats = object
        .get("output_formats")
        .and_then(Value::as_array)
        .ok_or_else(|| "semantic context output_formats must be an array".to_string())?;
    if output_formats.len() > crate::runtime_fingerprint::MAX_NORMALIZATION_SEMANTIC_OUTPUTS {
        return Err(format!(
            "semantic context output_formats exceeds the {}-item limit",
            crate::runtime_fingerprint::MAX_NORMALIZATION_SEMANTIC_OUTPUTS
        ));
    }
    let mut distinct_formats = Vec::new();
    for format in output_formats {
        let format = format
            .as_str()
            .ok_or_else(|| "semantic context output_formats contains a non-string".to_string())?;
        require_known_output_format(format)?;
        if !distinct_formats.contains(&format) {
            distinct_formats.push(format);
        }
    }

    let writers = object
        .get("writers")
        .and_then(Value::as_array)
        .ok_or_else(|| "semantic context writers must be an array".to_string())?;
    if writers.len() > 7 {
        return Err("semantic context writers exceeds the seven-format limit".into());
    }
    let mut writer_formats = Vec::with_capacity(writers.len());
    for (index, writer) in writers.iter().enumerate() {
        if writers[..index].iter().any(|prior| prior == writer) {
            return Err(format!(
                "semantic context writers contains duplicate evidence at index {index}"
            ));
        }
        let writer = require_semantic_object(
            writer,
            "semantic context writer",
            &[
                "format",
                "implementation_id",
                "pipeline_revision",
                "capability_success",
                "runtime",
            ],
        )?;
        let format = require_semantic_string(writer, "format", 16, None)?;
        require_known_output_format(format)?;
        if writer_formats.contains(&format) {
            return Err(format!(
                "semantic context writers contains duplicate format: {format}"
            ));
        }
        writer_formats.push(format);
        require_semantic_string(writer, "implementation_id", 256, None)?;
        require_semantic_string(writer, "pipeline_revision", 128, None)?;
        require_semantic_true(writer, "capability_success")?;
        validate_semantic_runtime(
            writer
                .get("runtime")
                .expect("required semantic context field was checked"),
        )?;
    }
    if writer_formats != distinct_formats {
        return Err(
            "semantic context writers must match distinct output_formats in first-occurrence order"
                .into(),
        );
    }
    Ok(())
}

fn require_semantic_object<'a>(
    value: &'a Value,
    label: &str,
    fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    for key in object.keys() {
        if !fields.contains(&key.as_str()) {
            return Err(format!("{label} contains unknown field: {key}"));
        }
    }
    for field in fields {
        if !object.contains_key(*field) {
            return Err(format!("{label} is missing required field: {field}"));
        }
    }
    Ok(object)
}

fn require_semantic_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    maximum_chars: usize,
    expected: Option<&str>,
) -> Result<&'a str, String> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("semantic context field {field} must be a string"))?;
    if value.is_empty() || value.chars().count() > maximum_chars {
        return Err(format!(
            "semantic context field {field} must contain 1..={maximum_chars} characters"
        ));
    }
    if let Some(expected) = expected {
        if value != expected {
            return Err(format!(
                "semantic context field {field} has an unsupported value"
            ));
        }
    }
    Ok(value)
}

fn require_semantic_u32(
    object: &serde_json::Map<String, Value>,
    field: &str,
    minimum: u32,
    expected: Option<u32>,
) -> Result<u32, String> {
    let value = object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("semantic context field {field} must be a 32-bit integer"))?;
    if value < minimum || expected.is_some_and(|expected| value != expected) {
        return Err(format!(
            "semantic context field {field} has an out-of-range value"
        ));
    }
    Ok(value)
}

fn require_optional_semantic_u32(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<u32>, String> {
    let value = object
        .get(field)
        .ok_or_else(|| format!("semantic context is missing required field: {field}"))?;
    if value.is_null() {
        return Ok(None);
    }
    require_semantic_u32(object, field, 0, None).map(Some)
}

fn require_semantic_true(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(), String> {
    if object.get(field).and_then(Value::as_bool) != Some(true) {
        return Err(format!("semantic context field {field} must be true"));
    }
    Ok(())
}

fn require_known_engine_id(value: &str) -> Result<(), String> {
    if matches!(value, "forge-fast-bs1770-r4" | "forge-reference-bs1770-r1") {
        Ok(())
    } else {
        Err(format!("unknown semantic context analysis engine: {value}"))
    }
}

fn require_known_output_format(value: &str) -> Result<(), String> {
    if matches!(
        value,
        "wav" | "flac" | "mp3" | "opus" | "m4a" | "alac" | "vorbis"
    ) {
        Ok(())
    } else {
        Err(format!("unknown semantic context output format: {value}"))
    }
}

fn validate_semantic_runtime(value: &Value) -> Result<(), String> {
    let kind = value
        .as_object()
        .and_then(|object| object.get("kind"))
        .and_then(Value::as_str)
        .ok_or_else(|| "semantic context writer runtime must contain a kind".to_string())?;
    match kind {
        "native" => {
            let runtime = require_semantic_object(
                value,
                "semantic context native runtime",
                &["kind", "capability_success"],
            )?;
            require_semantic_string(runtime, "kind", 16, Some("native"))?;
            require_semantic_true(runtime, "capability_success")
        }
        "lame" => {
            let runtime = require_semantic_object(
                value,
                "semantic context lame runtime",
                &[
                    "kind",
                    "lame_get_version",
                    "lame_version",
                    "capability_success",
                ],
            )?;
            require_semantic_string(runtime, "kind", 16, Some("lame"))?;
            require_semantic_string(runtime, "lame_get_version", 256, None)?;
            require_semantic_string(runtime, "lame_version", 256, None)?;
            require_semantic_true(runtime, "capability_success")
        }
        "libopus" => {
            let runtime = require_semantic_object(
                value,
                "semantic context libopus runtime",
                &["kind", "opus_version", "container", "capability_success"],
            )?;
            require_semantic_string(runtime, "kind", 16, Some("libopus"))?;
            require_semantic_string(runtime, "opus_version", 256, None)?;
            require_semantic_string(runtime, "container", 16, Some("ogg"))?;
            require_semantic_true(runtime, "capability_success")
        }
        "ffmpeg" => {
            let runtime = require_semantic_object(
                value,
                "semantic context ffmpeg runtime",
                &[
                    "kind",
                    "executable_byte_len",
                    "executable_sha256",
                    "byte_len",
                    "sha256",
                    "encoder",
                    "muxer",
                    "capability_success",
                ],
            )?;
            require_semantic_string(runtime, "kind", 16, Some("ffmpeg"))?;
            let executable_byte_len =
                require_semantic_positive_u64(runtime, "executable_byte_len")?;
            let executable_sha256 = require_semantic_sha256(runtime, "executable_sha256")?;
            let byte_len = require_semantic_positive_u64(runtime, "byte_len")?;
            let sha256 = require_semantic_sha256(runtime, "sha256")?;
            if executable_byte_len != byte_len {
                return Err("semantic context ffmpeg runtime byte lengths must match".into());
            }
            if executable_sha256 != sha256 {
                return Err("semantic context ffmpeg runtime SHA-256 values must match".into());
            }
            require_semantic_string(runtime, "encoder", 128, None)?;
            require_semantic_string(runtime, "muxer", 128, None)?;
            require_semantic_true(runtime, "capability_success")
        }
        _ => Err(format!(
            "unknown semantic context writer runtime kind: {kind}"
        )),
    }
}

fn require_semantic_positive_u64(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<u64, String> {
    let value = object
        .get(field)
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("semantic context field {field} must be a positive integer"))?;
    Ok(value)
}

fn require_semantic_sha256<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("semantic context field {field} must be a SHA-256 string"))?;
    if !is_sha256(value) {
        return Err(format!(
            "semantic context field {field} must be a lower-case SHA-256"
        ));
    }
    Ok(value)
}

/// Validate the operation descriptor persisted inside a batch-job-v3 state.
///
/// The CLI currently constructs this object as a JSON value rather than a
/// dedicated Rust type.  Keep the public `open_v3` boundary just as strict as
/// the published schema so library callers cannot create a state document
/// whose operation has missing, unknown, or out-of-range fields.
fn validate_v3_operation(value: &Value) -> Result<(), String> {
    const REQUIRED: &[&str] = &[
        "schema",
        "mode",
        "target_lufs",
        "target_peak_dbfs",
        "target_rms_dbfs",
        "ceiling_dbtp",
        "max_gain_db",
        "dither",
        "output_bits",
        "bitrate_kbps",
        "encoder_quality",
        "limiter",
        "wav_container",
        "bwf",
        "output_sample_rate_hz",
        "resample_quality",
        "verify",
        "verify_tolerance",
        "verify_retries",
        "album",
        "analysis_engine",
        "audio_track",
        "channel_layout",
        "dual_mono",
        "formats",
    ];
    const OPTIONAL: &[&str] = &[
        "metadata_policy",
        "metadata_registry_revision",
        "metadata_timing_revision",
        "metadata_fidelity_schema_version",
    ];
    let object = strict_operation_object(value, "batch v3 operation", REQUIRED, OPTIONAL)?;

    operation_string(
        object,
        "schema",
        None,
        Some("forge-normalization-operation-v1"),
    )?;
    operation_enum(object, "mode", &["lufs", "peak", "rms"])?;
    operation_number(object, "target_lufs", None)?;
    operation_number(object, "target_peak_dbfs", None)?;
    operation_number(object, "target_rms_dbfs", None)?;
    operation_number(object, "ceiling_dbtp", None)?;
    if !operation_field(object, "max_gain_db")?.is_null() {
        operation_number(object, "max_gain_db", None)?;
    }
    operation_bool(object, "dither")?;

    match operation_field(object, "output_bits")? {
        Value::Null => {}
        Value::String(_) => {
            operation_enum(
                object,
                "output_bits",
                &["8", "16", "24", "32", "32f", "64f"],
            )?;
        }
        _ => return Err("batch v3 operation output_bits must be a string or null".into()),
    }
    operation_integer(object, "bitrate_kbps", None)?;
    operation_u64(object, "encoder_quality", 0, 9)?;

    match operation_field(object, "limiter")? {
        Value::Null => {}
        limiter => {
            let limiter = strict_operation_object(
                limiter,
                "batch v3 operation limiter",
                &["lookahead_ms", "release_ms"],
                &[],
            )?;
            operation_number(limiter, "lookahead_ms", Some(0.0))?;
            operation_number(limiter, "release_ms", Some(0.0))?;
        }
    }
    operation_enum(object, "wav_container", &["auto", "riff", "rf64", "bw64"])?;
    operation_bool(object, "bwf")?;

    match operation_field(object, "output_sample_rate_hz")? {
        Value::Null => {}
        _ => {
            operation_u64(object, "output_sample_rate_hz", 8_000, 384_000)?;
        }
    }
    operation_enum(object, "resample_quality", &["fast", "balanced", "best"])?;
    operation_bool(object, "verify")?;
    operation_nonnegative_number(object, "verify_tolerance")?;
    operation_nonnegative_integer(object, "verify_retries")?;
    operation_bool(object, "album")?;
    operation_string(object, "analysis_engine", None, None)?;
    match operation_field(object, "audio_track")? {
        Value::Null => {}
        _ => {
            operation_u64(object, "audio_track", 0, u32::MAX as u64)?;
        }
    }
    if !operation_field(object, "channel_layout")?.is_null() {
        operation_string(object, "channel_layout", None, None)?;
    }
    operation_bool(object, "dual_mono")?;

    let formats = operation_field(object, "formats")?
        .as_array()
        .ok_or_else(|| "batch v3 operation formats must be an array".to_string())?;
    if formats.is_empty() || formats.len() > MAX_ASSETS {
        return Err(format!(
            "batch v3 operation formats must contain 1..={MAX_ASSETS} items"
        ));
    }
    for (index, format) in formats.iter().enumerate() {
        let format = format.as_str().ok_or_else(|| {
            format!("batch v3 operation format at index {index} must be a string")
        })?;
        require_known_output_format(format)?;
    }

    if let Some(policy) = object.get("metadata_policy") {
        let policy = strict_operation_object(
            policy,
            "batch v3 operation metadata_policy",
            &["policy", "strip_scope", "strip_locators"],
            &[],
        )?;
        let policy_name = operation_enum(
            policy,
            "policy",
            &["preserve", "strict", "strip", "legacy_generic"],
        )?;
        let strip_scope = operation_enum(policy, "strip_scope", &["none", "all", "selected"])?;
        let locators = operation_field(policy, "strip_locators")?
            .as_array()
            .ok_or_else(|| "batch v3 operation strip_locators must be an array".to_string())?;
        if locators.len() > 1_000_000 {
            return Err("batch v3 operation strip_locators exceeds the 1000000-item limit".into());
        }
        for (index, locator) in locators.iter().enumerate() {
            let locator = locator.as_str().ok_or_else(|| {
                format!("batch v3 operation strip_locator at index {index} must be a string")
            })?;
            if locator.is_empty() || locator.chars().count() > 1024 {
                return Err(format!(
                    "batch v3 operation strip_locator at index {index} must contain 1..=1024 characters"
                ));
            }
        }
        if policy_name == "strip" {
            if strip_scope == "none" {
                return Err(
                    "batch v3 operation strip policy requires strip_scope=all or selected".into(),
                );
            }
        } else if strip_scope != "none" {
            return Err("batch v3 operation non-strip policy requires strip_scope=none".into());
        }
        if strip_scope == "selected" {
            if locators.is_empty() {
                return Err(
                    "batch v3 operation selected strip_scope requires strip_locators".into(),
                );
            }
        } else if !locators.is_empty() {
            return Err(
                "batch v3 operation strip_locators must be empty unless strip_scope=selected"
                    .into(),
            );
        }
    }
    if object.contains_key("metadata_registry_revision") {
        operation_string(object, "metadata_registry_revision", Some(128), None)?;
    }
    if object.contains_key("metadata_timing_revision") {
        operation_string(object, "metadata_timing_revision", Some(128), None)?;
    }
    if object.contains_key("metadata_fidelity_schema_version") {
        operation_u64(
            object,
            "metadata_fidelity_schema_version",
            1,
            u32::MAX as u64,
        )?;
    }
    Ok(())
}

fn strict_operation_object<'a>(
    value: &'a Value,
    label: &str,
    required: &[&str],
    optional: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    for key in object.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(format!("{label} contains unknown field: {key}"));
        }
    }
    for field in required {
        if !object.contains_key(*field) {
            return Err(format!("{label} is missing required field: {field}"));
        }
    }
    Ok(object)
}

fn operation_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a Value, String> {
    object
        .get(field)
        .ok_or_else(|| format!("batch v3 operation is missing field: {field}"))
}

fn operation_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    maximum_chars: Option<usize>,
    expected: Option<&str>,
) -> Result<&'a str, String> {
    let value = operation_field(object, field)?
        .as_str()
        .ok_or_else(|| format!("batch v3 operation field {field} must be a string"))?;
    if value.is_empty() || maximum_chars.is_some_and(|maximum| value.chars().count() > maximum) {
        return Err(format!(
            "batch v3 operation field {field} has an invalid length"
        ));
    }
    if expected.is_some_and(|expected| value != expected) {
        return Err(format!(
            "batch v3 operation field {field} has an unsupported value"
        ));
    }
    Ok(value)
}

fn operation_enum<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    values: &[&str],
) -> Result<&'a str, String> {
    let value = operation_string(object, field, None, None)?;
    if !values.contains(&value) {
        return Err(format!(
            "batch v3 operation field {field} has an unsupported value"
        ));
    }
    Ok(value)
}

fn operation_number(
    object: &serde_json::Map<String, Value>,
    field: &str,
    exclusive_minimum: Option<f64>,
) -> Result<f64, String> {
    let value = operation_field(object, field)?
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("batch v3 operation field {field} must be a number"))?;
    if exclusive_minimum.is_some_and(|minimum| value <= minimum) {
        return Err(format!(
            "batch v3 operation field {field} is below its exclusive minimum"
        ));
    }
    Ok(value)
}

fn operation_nonnegative_number(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<f64, String> {
    let value = operation_number(object, field, None)?;
    if value < 0.0 {
        return Err(format!(
            "batch v3 operation field {field} must be non-negative"
        ));
    }
    Ok(value)
}

fn operation_bool(object: &serde_json::Map<String, Value>, field: &str) -> Result<bool, String> {
    operation_field(object, field)?
        .as_bool()
        .ok_or_else(|| format!("batch v3 operation field {field} must be a boolean"))
}

fn operation_integer(
    object: &serde_json::Map<String, Value>,
    field: &str,
    minimum: Option<i64>,
) -> Result<(), String> {
    let value = operation_field(object, field)?;
    let is_integer = value
        .as_i64()
        .is_some_and(|value| minimum.is_none_or(|minimum| value >= minimum))
        || value
            .as_u64()
            .is_some_and(|value| minimum.is_none_or(|minimum| value >= minimum as u64))
        || value.as_f64().is_some_and(|value| {
            value.is_finite()
                && value.fract() == 0.0
                && minimum.is_none_or(|minimum| value >= minimum as f64)
        });
    if !is_integer {
        return Err(format!(
            "batch v3 operation field {field} must be an integer in range"
        ));
    }
    Ok(())
}

fn operation_nonnegative_integer(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(), String> {
    operation_integer(object, field, Some(0))
}

fn operation_u64(
    object: &serde_json::Map<String, Value>,
    field: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, String> {
    let value = operation_field(object, field)?;
    let parsed = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| {
            value.as_f64().and_then(|value| {
                (value.is_finite()
                    && value.fract() == 0.0
                    && value >= minimum as f64
                    && value <= maximum as f64)
                    .then_some(value as u64)
            })
        })
        .filter(|value| *value >= minimum && *value <= maximum)
        .ok_or_else(|| format!("batch v3 operation field {field} is out of range"))?;
    Ok(parsed)
}

/// Bind the v3 semantic context to the operation descriptor that the CLI
/// actually uses for this asset list.  Keeping this check before the state
/// lock is acquired prevents a semantically unrelated context from creating a
/// durable job that can never be resumed by the normalizer.
fn validate_v3_semantic_alignment(
    operation: &Value,
    semantic_context: &Value,
    asset_count: usize,
) -> Result<(), String> {
    let operation = operation
        .as_object()
        .ok_or_else(|| "batch v3 operation descriptor must be an object".to_string())?;
    let operation_engine = operation
        .get("analysis_engine")
        .and_then(Value::as_str)
        .ok_or_else(|| "batch v3 operation is missing analysis_engine".to_string())?;
    let context = semantic_context
        .as_object()
        .ok_or_else(|| "batch v3 semantic context must be an object".to_string())?;
    let context_engine = context
        .get("analysis_engine_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "batch v3 semantic context is missing analysis_engine_id".to_string())?;
    if operation_engine != context_engine {
        return Err("batch v3 operation analysis_engine does not match semantic context".into());
    }
    let operation_track = operation
        .get("audio_track")
        .ok_or_else(|| "batch v3 operation is missing audio_track".to_string())?;
    let context_track = context
        .get("audio_track")
        .ok_or_else(|| "batch v3 semantic context is missing audio_track".to_string())?;
    if operation_track != context_track {
        return Err("batch v3 operation audio_track does not match semantic context".into());
    }
    let operation_formats = operation
        .get("formats")
        .and_then(Value::as_array)
        .ok_or_else(|| "batch v3 operation is missing formats".to_string())?;
    let context_formats = context
        .get("output_formats")
        .and_then(Value::as_array)
        .ok_or_else(|| "batch v3 semantic context is missing output_formats".to_string())?;
    if operation_formats.len() != asset_count || context_formats.len() != asset_count {
        return Err(
            "batch v3 operation and semantic context format counts must match asset count".into(),
        );
    }
    for (index, (operation_format, context_format)) in
        operation_formats.iter().zip(context_formats).enumerate()
    {
        let operation_format = operation_format.as_str().ok_or_else(|| {
            format!("batch v3 operation format at index {index} must be a string")
        })?;
        let context_format = context_format.as_str().ok_or_else(|| {
            format!("batch v3 semantic context format at index {index} must be a string")
        })?;
        if !operation_format_matches_context(operation_format, context_format) {
            return Err(format!(
                "batch v3 operation format at index {index} does not match semantic context"
            ));
        }
    }
    Ok(())
}

fn operation_format_matches_context(operation: &str, context: &str) -> bool {
    operation == context
}

fn validate_batch_assets_count(assets: &[BatchAssetSpec]) -> Result<(), String> {
    if assets.is_empty() {
        return Err("a resumable batch requires at least one asset".into());
    }
    if assets.len() > MAX_ASSETS {
        return Err(format!(
            "resumable batch exceeds the {MAX_ASSETS}-asset limit"
        ));
    }
    Ok(())
}

/// Encode a JSON value with deterministic lexicographic object keys and
/// explicit depth/node/byte limits.  `serde_json::Value` normally preserves
/// the caller's map insertion order, so serializing it directly is not a
/// sufficient semantic fingerprint.
fn canonical_json_bytes(
    value: &Value,
    maximum_depth: usize,
    maximum_nodes: usize,
    maximum_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut writer = BoundedJsonWriter::new(maximum_bytes);
    let mut nodes = 0_usize;
    write_canonical_json(
        value,
        0,
        maximum_depth,
        &mut nodes,
        maximum_nodes,
        &mut writer,
    )
    .map_err(|error| format!("encode {label}: {error}"))?;
    Ok(writer.into_inner())
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    maximum_bytes: usize,
}

impl BoundedJsonWriter {
    fn new(maximum_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum_bytes,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl IoWrite for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "encoded size overflow")
        })?;
        if next > self.maximum_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded value exceeds its byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_canonical_json(
    value: &Value,
    depth: usize,
    maximum_depth: usize,
    nodes: &mut usize,
    maximum_nodes: usize,
    writer: &mut BoundedJsonWriter,
) -> Result<(), String> {
    *nodes = (*nodes)
        .checked_add(1)
        .ok_or_else(|| "JSON node count overflow".to_string())?;
    if *nodes > maximum_nodes {
        return Err(format!("JSON value exceeds the {maximum_nodes}-node limit"));
    }
    if depth > maximum_depth {
        return Err(format!(
            "JSON value exceeds the {maximum_depth}-level depth limit"
        ));
    }
    match value {
        Value::Null => writer
            .write_all(b"null")
            .map_err(|error| error.to_string())?,
        Value::Bool(value) => writer
            .write_all(if *value { b"true" } else { b"false" })
            .map_err(|error| error.to_string())?,
        Value::Number(value) => serde_json::to_writer(writer, value)
            .map_err(|error| format!("write JSON number: {error}"))?,
        Value::String(value) => serde_json::to_writer(writer, value)
            .map_err(|error| format!("write JSON string: {error}"))?,
        Value::Array(values) => {
            writer.write_all(b"[").map_err(|error| error.to_string())?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    writer.write_all(b",").map_err(|error| error.to_string())?;
                }
                write_canonical_json(
                    value,
                    depth + 1,
                    maximum_depth,
                    nodes,
                    maximum_nodes,
                    writer,
                )?;
            }
            writer.write_all(b"]").map_err(|error| error.to_string())?;
        }
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            writer.write_all(b"{").map_err(|error| error.to_string())?;
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    writer.write_all(b",").map_err(|error| error.to_string())?;
                }
                serde_json::to_writer(&mut *writer, key)
                    .map_err(|error| format!("write JSON object key: {error}"))?;
                writer.write_all(b":").map_err(|error| error.to_string())?;
                write_canonical_json(
                    values.get(key).expect("key came from the same object"),
                    depth + 1,
                    maximum_depth,
                    nodes,
                    maximum_nodes,
                    writer,
                )?;
            }
            writer.write_all(b"}").map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn semantic_fingerprint(
    semantic_context: &Value,
    fingerprint_revision: u32,
) -> Result<String, String> {
    if fingerprint_revision == 0 {
        return Err("batch fingerprint revision must be greater than zero".into());
    }
    // Validate the caller's context itself so the envelope bookkeeping does
    // not consume any of the public depth/node budget.
    let _context_bytes = canonical_json_bytes(
        semantic_context,
        MAX_SEMANTIC_CONTEXT_DEPTH,
        MAX_SEMANTIC_CONTEXT_NODES,
        MAX_SEMANTIC_CONTEXT_BYTES,
        "semantic context",
    )?;
    let envelope = serde_json::json!({
        "fingerprint_revision": fingerprint_revision,
        "semantic_context": semantic_context,
    });
    let bytes = canonical_json_bytes(
        &envelope,
        MAX_SEMANTIC_CONTEXT_DEPTH + 2,
        MAX_SEMANTIC_CONTEXT_NODES + 3,
        // The public context limit applies to the caller-provided value. The
        // digest envelope adds a fixed handful of bytes (revision and field
        // names), so a context exactly at the limit must still be hashable.
        MAX_SEMANTIC_CONTEXT_BYTES + 256,
        "semantic context",
    )?;
    Ok(hash_bytes(&bytes))
}

fn specification_hash_v3(operation: &Value, assets: &[JobAsset]) -> Result<String, String> {
    let specification = serde_json::json!({
        "operation": operation,
        "assets": assets
            .iter()
            .map(|asset| {
                serde_json::json!({
                    "input": asset.input,
                    "output": asset.output,
                    "input_sha256": asset.input_sha256,
                })
            })
            .collect::<Vec<_>>(),
    });
    let bytes = canonical_json_bytes(
        &specification,
        MAX_SEMANTIC_CONTEXT_DEPTH + 2,
        MAX_ASSETS.saturating_mul(8),
        MAX_STATE_BYTES as usize,
        "batch v3 specification",
    )?;
    Ok(hash_bytes(&bytes))
}

fn job_id(
    operation: &Value,
    assets: &[JobAsset],
    semantic_fingerprint: &str,
    fingerprint_revision: u32,
    failure_policy: BatchFailurePolicy,
) -> Result<String, String> {
    let identity = serde_json::json!({
        "operation": operation,
        "assets": assets
            .iter()
            .map(|asset| {
                serde_json::json!({
                    "input": asset.input,
                    "output": asset.output,
                    "input_sha256": asset.input_sha256,
                })
            })
            .collect::<Vec<_>>(),
        "semantic_fingerprint": semantic_fingerprint,
        "fingerprint_revision": fingerprint_revision,
        "failure_policy": failure_policy,
    });
    let bytes = canonical_json_bytes(
        &identity,
        MAX_SEMANTIC_CONTEXT_DEPTH + 2,
        MAX_ASSETS.saturating_mul(8),
        MAX_STATE_BYTES as usize,
        "batch v3 job identity",
    )?;
    Ok(hash_bytes(&bytes))
}

fn reject_duplicate_paths(assets: &[JobAsset]) -> Result<(), String> {
    let mut inputs = std::collections::BTreeSet::new();
    let mut outputs = std::collections::BTreeSet::new();
    for asset in assets {
        let input_key = normalized_path_key(Path::new(&asset.input))?;
        let output_key = normalized_path_key(Path::new(&asset.output))?;
        if !inputs.insert(input_key) {
            return Err(format!("duplicate batch input: {}", asset.input));
        }
        if !outputs.insert(output_key) {
            return Err(format!("duplicate batch output: {}", asset.output));
        }
    }
    Ok(())
}

/// Apply the same physical-route and live-file alias rules at the public v3
/// boundary that the CLI's output plan applies before it constructs a job.
/// A library caller must not be able to publish through a symlinked parent or
/// hard link which spells an input/output route differently.
fn reject_duplicate_v3_paths(assets: &[JobAsset]) -> Result<(), String> {
    let mut input_routes = std::collections::BTreeSet::new();
    let mut output_routes = std::collections::BTreeSet::new();
    let mut input_identities = std::collections::HashSet::<StableFileIdentity>::new();
    let mut output_identities = std::collections::HashSet::<StableFileIdentity>::new();

    for asset in assets {
        let input = Path::new(&asset.input);
        let input_route = physical_route_key(input)?;
        if !input_routes.insert(input_route) {
            return Err(format!("duplicate batch input route: {}", asset.input));
        }
        let identity = path_identity_if_exists(input)
            .map_err(|error| format!("identify batch input {}: {error}", input.display()))?
            .ok_or_else(|| format!("batch input disappeared: {}", input.display()))?;
        if !input_identities.insert(identity) {
            return Err(format!(
                "duplicate or hard-link-aliased batch input: {}",
                asset.input
            ));
        }
    }

    for asset in assets {
        let output = Path::new(&asset.output);
        let output_route = physical_route_key(output)?;
        if input_routes.contains(&output_route) {
            return Err(format!(
                "batch output aliases an input route: {}",
                asset.output
            ));
        }
        if !output_routes.insert(output_route) {
            return Err(format!("duplicate batch output route: {}", asset.output));
        }
        if let Some(identity) = path_identity_if_exists(output)
            .map_err(|error| format!("identify batch output {}: {error}", output.display()))?
        {
            if input_identities.contains(&identity) {
                return Err(format!(
                    "batch output hard-links an input: {}",
                    asset.output
                ));
            }
            if !output_identities.insert(identity) {
                return Err(format!(
                    "duplicate or hard-link-aliased batch output: {}",
                    asset.output
                ));
            }
        }
    }
    Ok(())
}

fn reject_v3_state_aliases(state_path: &Path, assets: &[JobAsset]) -> Result<(), String> {
    let state_key = physical_route_key(state_path)?;
    let lock_path = sibling_lock_path(state_path)?;
    let lock_key = physical_route_key(&lock_path)?;
    let state_identity = path_identity_if_exists(state_path)
        .map_err(|error| format!("identify batch state {}: {error}", state_path.display()))?;
    let lock_identity = path_identity_if_exists(&lock_path)
        .map_err(|error| format!("identify batch state lock {}: {error}", lock_path.display()))?;
    for asset in assets {
        for (kind, path) in [
            ("input", asset.input.as_str()),
            ("output", asset.output.as_str()),
        ] {
            let path = Path::new(path);
            let key = physical_route_key(path)?;
            if key == state_key || key == lock_key {
                return Err(format!(
                    "batch v3 {kind} aliases its state or lock path: {}",
                    path.display()
                ));
            }
            if let Some(identity) = path_identity_if_exists(path)
                .map_err(|error| format!("identify batch v3 {kind} {}: {error}", path.display()))?
            {
                if state_identity.as_ref() == Some(&identity)
                    || lock_identity.as_ref() == Some(&identity)
                {
                    return Err(format!(
                        "batch v3 {kind} hard-links its state or lock path: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn specification_hash(operation: &Value, assets: &[JobAsset]) -> Result<String, String> {
    #[derive(Serialize)]
    struct Specification<'a> {
        operation: &'a Value,
        assets: Vec<SpecificationAsset<'a>>,
    }
    #[derive(Serialize)]
    struct SpecificationAsset<'a> {
        input: &'a str,
        output: &'a str,
        input_sha256: &'a str,
    }
    let specification = Specification {
        operation,
        assets: assets
            .iter()
            .map(|asset| SpecificationAsset {
                input: &asset.input,
                output: &asset.output,
                input_sha256: &asset.input_sha256,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&specification)
        .map_err(|error| format!("encode batch specification: {error}"))?;
    Ok(hash_bytes(&bytes))
}

fn validate_document(
    document: &JobDocument,
    operation: &Value,
    specification_sha256: &str,
    expected_assets: &[JobAsset],
) -> Result<(), String> {
    if document.schema != BATCH_JOB_SCHEMA_V1 && document.schema != BATCH_JOB_SCHEMA_V2 {
        return Err(format!(
            "unsupported batch state schema {}",
            document.schema
        ));
    }
    if document.schema == BATCH_JOB_SCHEMA_V1
        && document
            .assets
            .iter()
            .any(|asset| asset.status == AssetStatus::ReadyToPublish)
    {
        return Err("batch-job-v1 cannot contain a ready-to-publish asset".into());
    }
    if document.fingerprint_revision.is_some()
        || document.semantic_fingerprint.is_some()
        || document.semantic_context.is_some()
        || document.job_id.is_some()
        || document.failure_policy.is_some()
    {
        return Err("legacy batch state contains v3 fingerprint fields".into());
    }
    if document.generator.is_empty()
        || document.generator.len() > MAX_GENERATOR_BYTES
        || !document.generator.starts_with("forge-normalizer/")
    {
        return Err(format!(
            "batch state generator provenance must contain 1..={MAX_GENERATOR_BYTES} bytes and use the forge-normalizer prefix"
        ));
    }
    if &document.operation != operation {
        return Err(
            "batch state operation does not match the current normalization settings".into(),
        );
    }
    if document.specification_sha256 != specification_sha256 {
        return Err(
            "batch state does not match the current inputs, outputs, or normalization settings"
                .into(),
        );
    }
    if document.asset_count != expected_assets.len()
        || document.assets.len() != expected_assets.len()
        || document.asset_count > MAX_ASSETS
    {
        return Err("batch state asset counts are inconsistent".into());
    }
    let completed = document
        .assets
        .iter()
        .filter(|asset| asset.status == AssetStatus::Completed)
        .count();
    if document.completed_count != completed {
        return Err("batch state completed count is inconsistent".into());
    }
    for (stored, expected) in document.assets.iter().zip(expected_assets) {
        if stored.input != expected.input
            || stored.output != expected.output
            || stored.input_sha256 != expected.input_sha256
        {
            return Err("batch state asset list does not match the current job".into());
        }
        if !is_sha256(&stored.input_sha256)
            || stored
                .output_sha256
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
            || (stored.status != AssetStatus::Pending && stored.output_sha256.is_none())
            || (stored.status == AssetStatus::Pending && stored.output_sha256.is_some())
        {
            return Err("batch state contains invalid hash or status evidence".into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_document_v3(
    document: &JobDocument,
    operation: &Value,
    specification_sha256: &str,
    expected_assets: &[JobAsset],
    job_id: &str,
    expected_semantic_fingerprint: &str,
    semantic_context: &Value,
    fingerprint_revision: u32,
    failure_policy: BatchFailurePolicy,
) -> Result<(), String> {
    if document.schema != BATCH_JOB_SCHEMA_V3 {
        return Err(format!(
            "unsupported batch v3 state schema {}",
            document.schema
        ));
    }
    if !valid_generator(&document.generator) {
        return Err("batch v3 state contains an invalid generator".into());
    }
    if &document.operation != operation {
        return Err(
            "batch v3 state operation does not match the current normalization settings".into(),
        );
    }
    if document.specification_sha256 != specification_sha256 {
        return Err(
            "batch v3 state does not match the current inputs, outputs, or normalization settings"
                .into(),
        );
    }
    if document.fingerprint_revision != Some(fingerprint_revision)
        || document.semantic_fingerprint.as_deref() != Some(expected_semantic_fingerprint)
        || document.job_id.as_deref() != Some(job_id)
        || document.failure_policy != Some(failure_policy)
    {
        return Err("batch v3 state semantic fingerprint or failure policy does not match".into());
    }
    let stored_context = document
        .semantic_context
        .as_ref()
        .ok_or_else(|| "batch v3 state has no semantic context".to_string())?;
    if stored_context != semantic_context {
        return Err("batch v3 state semantic context does not match".into());
    }
    let stored_context_bytes = canonical_json_bytes(
        stored_context,
        MAX_SEMANTIC_CONTEXT_DEPTH,
        MAX_SEMANTIC_CONTEXT_NODES,
        MAX_SEMANTIC_CONTEXT_BYTES,
        "stored semantic context",
    )?;
    let stored_canonical: Value = serde_json::from_slice(&stored_context_bytes)
        .map_err(|error| format!("decode stored canonical semantic context: {error}"))?;
    validate_normalization_semantic_context(stored_context)?;
    if stored_canonical != *stored_context
        || serde_json::to_vec(stored_context)
            .map_err(|error| format!("encode stored semantic context: {error}"))?
            != stored_context_bytes
    {
        return Err("batch v3 state semantic context is not canonical".into());
    }
    let stored_fingerprint = semantic_fingerprint(stored_context, fingerprint_revision)?;
    if stored_fingerprint != expected_semantic_fingerprint {
        return Err("batch v3 state semantic context digest does not match".into());
    }
    validate_document_assets(
        &document.assets,
        document.asset_count,
        document.completed_count,
        expected_assets,
    )?;
    if document.completed_count != 0 && document.completed_count != document.asset_count {
        return Err("batch v3 state cannot checkpoint a partially published generation".into());
    }
    if document
        .assets
        .iter()
        .any(|asset| asset.status == AssetStatus::ReadyToPublish)
    {
        return Err(
            "batch v3 state delegates publication readiness to the generation journal".into(),
        );
    }
    Ok(())
}

fn validate_document_assets(
    stored_assets: &[JobAsset],
    asset_count: usize,
    completed_count: usize,
    expected_assets: &[JobAsset],
) -> Result<(), String> {
    if asset_count != expected_assets.len()
        || stored_assets.len() != expected_assets.len()
        || asset_count > MAX_ASSETS
    {
        return Err("batch state asset counts are inconsistent".into());
    }
    let completed = stored_assets
        .iter()
        .filter(|asset| asset.status == AssetStatus::Completed)
        .count();
    if completed_count != completed {
        return Err("batch state completed count is inconsistent".into());
    }
    for (stored, expected) in stored_assets.iter().zip(expected_assets) {
        if stored.input != expected.input
            || stored.output != expected.output
            || stored.input_sha256 != expected.input_sha256
        {
            return Err("batch state asset list does not match the current job".into());
        }
        if !is_sha256(&stored.input_sha256)
            || stored
                .output_sha256
                .as_deref()
                .is_some_and(|value| !is_sha256(value))
            || (stored.status != AssetStatus::Pending && stored.output_sha256.is_none())
            || (stored.status == AssetStatus::Pending && stored.output_sha256.is_some())
        {
            return Err("batch state contains invalid hash or status evidence".into());
        }
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| format!("resolve {}: {error}", path.display()))
}

/// Lexically normalize an absolute path without consulting the filesystem.
/// This mirrors generation's path handling so `foo/../bar` and `bar` have the
/// same persisted meaning while still allowing a missing destination.
fn normalized_absolute_path(path: &Path) -> Result<PathBuf, String> {
    let absolute = absolute_path(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR))
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let _ = normalized.pop();
            }
            std::path::Component::Normal(value) => normalized.push(value),
        }
    }
    if !normalized.is_absolute() {
        return Err(format!("path is not absolute: {}", path.display()));
    }
    Ok(normalized)
}

/// Return a platform-appropriate comparison key after lexical normalization.
/// Windows generation path keys are case-insensitive; Unix keys preserve case.
fn normalized_path_key(path: &Path) -> Result<String, String> {
    let normalized = normalized_absolute_path(path)?;
    let key = path_text(&normalized)?;
    #[cfg(windows)]
    {
        let mut key = key;
        key.make_ascii_lowercase();
        Ok(key)
    }
    #[cfg(not(windows))]
    {
        Ok(key)
    }
}

fn path_text(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("resumable batch paths must be UTF-8: {}", path.display()))?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(format!(
            "resumable batch paths must be non-empty and contain no control characters: {}",
            path.display()
        ));
    }
    Ok(value.to_owned())
}

fn hash_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "refuse to hash a non-regular or symlink batch file: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse to hash a reparse-point batch file: {}",
            path.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);
    let mut file = options
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let before = file
        .metadata()
        .map_err(|error| format!("inspect opened {}: {error}", path.display()))?;
    #[cfg(windows)]
    if before.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "refuse to hash a reparse-point batch file: {}",
            path.display()
        ));
    }
    if !before.is_file() {
        return Err(format!("batch file is not regular: {}", path.display()));
    }
    let identity = identity_from_open_file(&file, path)
        .map_err(|error| format!("identify {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|error| format!("reinspect opened {}: {error}", path.display()))?;
    if before.len() != after.len() {
        return Err(format!(
            "batch file changed while hashing: {}",
            path.display()
        ));
    }
    let confirmation = options
        .open(path)
        .map_err(|error| format!("reopen {} after hashing: {error}", path.display()))?;
    let confirmation_identity = identity_from_open_file(&confirmation, path)
        .map_err(|error| format!("identify {} after hashing: {error}", path.display()))?;
    if confirmation_identity != identity {
        return Err(format!(
            "batch file changed while hashing: {}",
            path.display()
        ));
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes).as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_generator(generator: &str) -> bool {
    if generator.is_empty() || generator.len() > MAX_GENERATOR_BYTES {
        return false;
    }
    let Some(version) = generator.strip_prefix("forge-normalizer/") else {
        return false;
    };
    let mut components = version.split('.');
    let numeric = |component: Option<&str>| {
        component.is_some_and(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    };
    numeric(components.next())
        && numeric(components.next())
        && numeric(components.next())
        && components.next().is_none()
}

/// One stable, machine-readable lifecycle event emitted by a normalization job.
#[derive(Debug, Serialize)]
pub struct BatchProgressEvent<'a> {
    pub schema: &'static str,
    pub generator: String,
    pub sequence: u64,
    pub event: &'a str,
    pub completed: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl<'a> BatchProgressEvent<'a> {
    pub fn new(sequence: u64, event: &'a str, completed: usize, total: usize) -> Self {
        Self {
            schema: BATCH_PROGRESS_SCHEMA_V1,
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            sequence,
            event,
            completed,
            total,
            index: None,
            input: None,
            output: None,
            error: None,
        }
    }

    /// Construct the additive v2 event shape without changing the v1 wire
    /// format used by existing callers.
    pub fn new_v2(
        sequence: u64,
        event: &'a str,
        completed: usize,
        total: usize,
        job_id: impl Into<String>,
        generation: u64,
        phase: impl Into<String>,
    ) -> BatchProgressEventV2<'a> {
        BatchProgressEventV2::new(sequence, event, completed, total, job_id, generation, phase)
    }
}

/// The additive v2 lifecycle event.  The v1 event remains unchanged for
/// consumers that intentionally request the historical stream.
#[derive(Debug, Clone)]
pub struct BatchProgressEventV2<'a> {
    pub schema: &'static str,
    pub generator: String,
    pub sequence: u64,
    pub event: &'a str,
    pub job_id: String,
    pub generation: u64,
    pub phase: String,
    pub completed: usize,
    pub total: usize,
    pub index: Option<usize>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
struct BatchProgressEventV2Wire<'a> {
    schema: &'a str,
    generator: &'a str,
    sequence: u64,
    event: &'a str,
    job_id: &'a str,
    generation: u64,
    phase: &'a str,
    completed: usize,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

impl Serialize for BatchProgressEventV2<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        BatchProgressEventV2Wire {
            schema: self.schema,
            generator: &self.generator,
            sequence: self.sequence,
            event: self.event,
            job_id: &self.job_id,
            generation: self.generation,
            phase: &self.phase,
            completed: self.completed,
            total: self.total,
            index: self.index,
            input: self.input.as_deref(),
            output: self.output.as_deref(),
            error: self.error.as_deref(),
        }
        .serialize(serializer)
    }
}

impl<'a> BatchProgressEventV2<'a> {
    pub fn new(
        sequence: u64,
        event: &'a str,
        completed: usize,
        total: usize,
        job_id: impl Into<String>,
        generation: u64,
        phase: impl Into<String>,
    ) -> Self {
        Self {
            schema: BATCH_PROGRESS_SCHEMA_V2,
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            sequence,
            event,
            job_id: job_id.into(),
            generation,
            phase: phase.into(),
            completed,
            total,
            index: None,
            input: None,
            output: None,
            error: None,
        }
    }

    /// Validate lifecycle fields before handing the event to a writer.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != BATCH_PROGRESS_SCHEMA_V2 {
            return Err("unsupported batch progress v2 schema".into());
        }
        if !valid_generator(&self.generator) {
            return Err("batch progress v2 contains an invalid generator".into());
        }
        if !is_sha256(&self.job_id) {
            return Err("batch progress v2 job_id must be a lower-case SHA-256".into());
        }
        if self.phase.is_empty() || self.phase.len() > MAX_BATCH_PROGRESS_PHASE_BYTES {
            return Err(format!(
                "batch progress v2 phase must contain 1..={MAX_BATCH_PROGRESS_PHASE_BYTES} bytes"
            ));
        }
        if !matches!(self.phase.as_str(), "rendering" | "committed" | "failed") {
            return Err(format!("unknown batch progress v2 phase {}", self.phase));
        }
        if self.total == 0 || self.total > MAX_ASSETS || self.completed > self.total {
            return Err("batch progress v2 counts are out of range".into());
        }
        let asset_event = matches!(
            self.event,
            "asset_started" | "asset_completed" | "asset_skipped" | "asset_failed"
        );
        let job_event = matches!(self.event, "job_started" | "job_completed" | "job_failed");
        if !asset_event && !job_event {
            return Err(format!("unknown batch progress v2 event {}", self.event));
        }
        let expected_phase = match self.event {
            "job_started" | "asset_started" => "rendering",
            "asset_completed" | "asset_skipped" | "job_completed" => "committed",
            "asset_failed" | "job_failed" => "failed",
            _ => unreachable!("event was checked above"),
        };
        if self.phase != expected_phase {
            return Err(format!(
                "batch progress v2 event {} requires phase {}",
                self.event, expected_phase
            ));
        }
        if asset_event {
            if self.index.is_none() || self.input.is_none() || self.output.is_none() {
                return Err("asset progress v2 events require index, input, and output".into());
            }
            if self.index.is_some_and(|index| index >= self.total)
                || self.input.as_deref().is_some_and(str::is_empty)
                || self.output.as_deref().is_some_and(str::is_empty)
            {
                return Err("asset progress v2 fields are out of range".into());
            }
            if self
                .input
                .as_deref()
                .is_some_and(|value| value.len() > MAX_BATCH_PATH_BYTES)
                || self
                    .output
                    .as_deref()
                    .is_some_and(|value| value.len() > MAX_BATCH_PATH_BYTES)
            {
                return Err(format!(
                    "batch progress v2 input and output exceed the {MAX_BATCH_PATH_BYTES}-byte limit"
                ));
            }
        } else if self.index.is_some() || self.input.is_some() || self.output.is_some() {
            return Err("job progress v2 events cannot contain asset fields".into());
        }
        if matches!(self.event, "asset_failed" | "job_failed") {
            if self.error.as_deref().is_none_or(str::is_empty) {
                return Err(format!("{} requires an error", self.event));
            }
            if self
                .error
                .as_deref()
                .is_some_and(|error| error.len() > MAX_BATCH_FAILURE_ERROR_BYTES)
            {
                return Err(format!(
                    "batch progress v2 error exceeds the {MAX_BATCH_FAILURE_ERROR_BYTES}-byte limit"
                ));
            }
        } else if self.error.is_some() {
            return Err(format!("{} cannot contain an error", self.event));
        }
        Ok(())
    }
}

/// One bounded failure entry in a batch failure report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchFailure {
    pub index: usize,
    pub input: String,
    pub output: String,
    pub error: String,
}

#[derive(Serialize)]
struct BatchFailureWire<'a> {
    index: usize,
    input: &'a str,
    output: &'a str,
    error: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchFailureDocument {
    index: usize,
    input: String,
    output: String,
    error: String,
}

impl Serialize for BatchFailure {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.validate_fields().map_err(serde::ser::Error::custom)?;
        BatchFailureWire {
            index: self.index,
            input: &self.input,
            output: &self.output,
            error: &self.error,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BatchFailure {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let document = BatchFailureDocument::deserialize(deserializer)?;
        let failure = Self {
            index: document.index,
            input: document.input,
            output: document.output,
            error: document.error,
        };
        failure
            .validate_fields()
            .map_err(serde::de::Error::custom)?;
        Ok(failure)
    }
}

impl BatchFailure {
    pub fn new(
        index: usize,
        input: impl Into<String>,
        output: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            index,
            input: input.into(),
            output: output.into(),
            error: error.into(),
        }
    }

    pub fn validate(&self, total: usize) -> Result<(), String> {
        if self.index >= total {
            return Err(format!(
                "batch failure index {} is outside the {total}-asset job",
                self.index
            ));
        }
        self.validate_fields()
    }

    fn validate_fields(&self) -> Result<(), String> {
        if self.index >= MAX_ASSETS {
            return Err(format!(
                "batch failure index {} exceeds the maximum supported asset index {}",
                self.index,
                MAX_ASSETS - 1
            ));
        }
        if self.input.is_empty() || self.output.is_empty() {
            return Err("batch failure input and output must be non-empty".into());
        }
        if self.input.len() > MAX_BATCH_PATH_BYTES || self.output.len() > MAX_BATCH_PATH_BYTES {
            return Err(format!(
                "batch failure input and output exceed the {MAX_BATCH_PATH_BYTES}-byte limit"
            ));
        }
        if self.error.is_empty() {
            return Err("batch failure error must be non-empty".into());
        }
        if self.error.len() > MAX_BATCH_FAILURE_ERROR_BYTES {
            return Err(format!(
                "batch failure error exceeds the {MAX_BATCH_FAILURE_ERROR_BYTES}-byte limit"
            ));
        }
        Ok(())
    }
}

/// A deterministic, bounded summary of independent batch failures.
#[derive(Debug, Clone)]
pub struct BatchFailureReport {
    pub schema: String,
    pub generator: String,
    pub job_id: String,
    pub semantic_fingerprint: String,
    pub fingerprint_revision: u32,
    pub failure_policy: BatchFailurePolicy,
    pub total: usize,
    pub succeeded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub failures: Vec<BatchFailure>,
    pub truncated: bool,
    pub dropped_count: usize,
    seen_failure_indexes: BTreeSet<usize>,
}

#[derive(Serialize)]
struct BatchFailureReportWire<'a> {
    schema: &'a str,
    generator: &'a str,
    job_id: &'a str,
    semantic_fingerprint: &'a str,
    fingerprint_revision: u32,
    failure_policy: BatchFailurePolicy,
    total: usize,
    succeeded: usize,
    skipped: usize,
    failed: usize,
    failures: &'a [BatchFailure],
    truncated: bool,
    dropped_count: usize,
}

impl<'a> From<&'a BatchFailureReport> for BatchFailureReportWire<'a> {
    fn from(report: &'a BatchFailureReport) -> Self {
        Self {
            schema: &report.schema,
            generator: &report.generator,
            job_id: &report.job_id,
            semantic_fingerprint: &report.semantic_fingerprint,
            fingerprint_revision: report.fingerprint_revision,
            failure_policy: report.failure_policy,
            total: report.total,
            succeeded: report.succeeded,
            skipped: report.skipped,
            failed: report.failed,
            failures: &report.failures,
            truncated: report.truncated,
            dropped_count: report.dropped_count,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchFailureReportDocument {
    schema: String,
    generator: String,
    job_id: String,
    semantic_fingerprint: String,
    fingerprint_revision: u32,
    failure_policy: BatchFailurePolicy,
    total: usize,
    succeeded: usize,
    skipped: usize,
    failed: usize,
    failures: Vec<BatchFailure>,
    truncated: bool,
    dropped_count: usize,
}

impl<'de> Deserialize<'de> for BatchFailureReport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let document = BatchFailureReportDocument::deserialize(deserializer)?;
        let report = Self {
            schema: document.schema,
            generator: document.generator,
            job_id: document.job_id,
            semantic_fingerprint: document.semantic_fingerprint,
            fingerprint_revision: document.fingerprint_revision,
            failure_policy: document.failure_policy,
            total: document.total,
            succeeded: document.succeeded,
            skipped: document.skipped,
            failed: document.failed,
            failures: document.failures,
            truncated: document.truncated,
            dropped_count: document.dropped_count,
            seen_failure_indexes: BTreeSet::new(),
        };
        report.validate().map_err(serde::de::Error::custom)?;
        Ok(report)
    }
}

impl BatchFailureReport {
    pub fn new(
        job_id: impl Into<String>,
        semantic_fingerprint: impl Into<String>,
        fingerprint_revision: u32,
        failure_policy: BatchFailurePolicy,
        total: usize,
    ) -> Self {
        Self {
            schema: BATCH_FAILURE_REPORT_SCHEMA_V1.into(),
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            job_id: job_id.into(),
            semantic_fingerprint: semantic_fingerprint.into(),
            fingerprint_revision,
            failure_policy,
            total,
            succeeded: 0,
            skipped: 0,
            failed: 0,
            failures: Vec::new(),
            truncated: false,
            dropped_count: 0,
            seen_failure_indexes: BTreeSet::new(),
        }
    }

    /// Update non-failure counts before encoding the final report.
    pub fn set_completed_counts(&mut self, succeeded: usize, skipped: usize) {
        self.succeeded = succeeded;
        self.skipped = skipped;
    }

    /// Record one failure in input-index order.  Once the retained-entry cap
    /// is reached, later entries are counted but omitted deterministically.
    /// Oversized UTF-8 errors are truncated at a character boundary and set
    /// the report-level `truncated` marker. A decoded report that already
    /// dropped entries is sealed because their indexes are intentionally not
    /// present on the wire and cannot be safely reconstructed for appending.
    pub fn add_failure(&mut self, mut failure: BatchFailure) -> Result<bool, String> {
        if self.total == 0 || self.total > MAX_ASSETS {
            return Err("batch failure report total is out of range".into());
        }
        if failure.index >= self.total {
            return Err(format!(
                "batch failure index {} is outside the {}-asset job",
                failure.index, self.total
            ));
        }
        if failure.input.is_empty() || failure.output.is_empty() || failure.error.is_empty() {
            return Err("batch failure input, output, and error must be non-empty".into());
        }
        if failure.input.len() > MAX_BATCH_PATH_BYTES || failure.output.len() > MAX_BATCH_PATH_BYTES
        {
            return Err(format!(
                "batch failure input and output exceed the {MAX_BATCH_PATH_BYTES}-byte limit"
            ));
        }
        if self.seen_failure_indexes.is_empty() && self.failed != 0 {
            self.validate()?;
            if self.dropped_count != 0 {
                return Err(
                    "cannot append to a decoded batch failure report with dropped entries".into(),
                );
            }
            self.seen_failure_indexes
                .extend(self.failures.iter().map(|entry| entry.index));
        }
        if self.seen_failure_indexes.contains(&failure.index) {
            return Err(format!("duplicate batch failure index {}", failure.index));
        }
        let failed = self
            .failed
            .checked_add(1)
            .ok_or_else(|| "batch failure count overflow".to_string())?;
        if failed > self.total {
            return Err("batch failure count exceeds the report total".into());
        }
        let error_truncated = truncate_utf8(&mut failure.error, MAX_BATCH_FAILURE_ERROR_BYTES);
        if self.failures.len() >= MAX_BATCH_FAILURES {
            let dropped_count = self
                .dropped_count
                .checked_add(1)
                .ok_or_else(|| "batch failure dropped count overflow".to_string())?;
            self.seen_failure_indexes.insert(failure.index);
            self.failed = failed;
            self.dropped_count = dropped_count;
            self.truncated = true;
            return Ok(false);
        }
        let index = self
            .failures
            .binary_search_by_key(&failure.index, |entry| entry.index)
            .unwrap_or_else(|index| index);
        self.seen_failure_indexes.insert(failure.index);
        self.failed = failed;
        self.truncated |= error_truncated;
        self.failures.insert(index, failure);
        Ok(true)
    }

    /// Validate all report invariants and return canonical, bounded JSON
    /// bytes suitable for the caller's atomic staging/output primitive.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let encoded = encode_bounded_json(
            &BatchFailureReportWire::from(self),
            MAX_BATCH_FAILURE_REPORT_BYTES,
            "batch failure report",
        );
        if encoded.is_ok() {
            return encoded;
        }

        // A report may be structurally valid while its retained entries still
        // exceed the aggregate wire-size budget.  Keep the deterministic input
        // order and drop only a suffix, accounting for every omitted entry in
        // `dropped_count`.  The encoded size is monotone for prefixes, so a
        // binary search avoids repeatedly walking all possible retained sizes.
        let retained = self.failures.len();
        if retained == 0 {
            return encoded;
        }
        let original = self.failures.clone();
        let mut candidate = self.clone();
        let mut low = 0_usize;
        let mut high = retained;
        let mut best = None;
        while low <= high {
            let keep = low + (high - low) / 2;
            candidate.failures = original[..keep].to_vec();
            candidate.dropped_count = self
                .dropped_count
                .checked_add(retained - keep)
                .ok_or_else(|| "batch failure dropped count overflow".to_string())?;
            candidate.truncated = self.truncated || keep < retained;
            match encode_bounded_json(
                &BatchFailureReportWire::from(&candidate),
                MAX_BATCH_FAILURE_REPORT_BYTES,
                "batch failure report",
            ) {
                Ok(bytes) => {
                    best = Some(bytes);
                    low = keep.saturating_add(1);
                }
                Err(_) => {
                    if keep == 0 {
                        break;
                    }
                    high = keep - 1;
                }
            }
        }
        best.ok_or_else(|| {
            "batch failure report exceeds its byte limit even after dropping retained failures"
                .into()
        })
    }

    pub fn encoded_bytes(&self) -> Result<Vec<u8>, String> {
        self.to_bytes()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != BATCH_FAILURE_REPORT_SCHEMA_V1 {
            return Err("unsupported batch failure report schema".into());
        }
        if !valid_generator(&self.generator) {
            return Err("batch failure report contains an invalid generator".into());
        }
        if !is_sha256(&self.job_id) || !is_sha256(&self.semantic_fingerprint) {
            return Err("batch failure report contains an invalid fingerprint".into());
        }
        if self.fingerprint_revision == 0 {
            return Err("batch failure report fingerprint revision is zero".into());
        }
        if self.total == 0 || self.total > MAX_ASSETS {
            return Err("batch failure report total is out of range".into());
        }
        if self.succeeded > self.total
            || self.skipped > self.total
            || self.failed > self.total
            || self
                .succeeded
                .checked_add(self.skipped)
                .and_then(|count| count.checked_add(self.failed))
                .is_none_or(|count| count > self.total)
        {
            return Err("batch failure report counts are inconsistent".into());
        }
        if self.failures.len() > MAX_BATCH_FAILURES {
            return Err("batch failure report retains too many failures".into());
        }
        if self.dropped_count > self.failed
            || self.failures.len().checked_add(self.dropped_count) != Some(self.failed)
        {
            return Err("batch failure report failure counts are inconsistent".into());
        }
        if self.dropped_count != 0 && !self.truncated {
            return Err("dropped failures must set truncated".into());
        }
        let mut previous = None;
        for failure in &self.failures {
            failure.validate(self.total)?;
            if previous.is_some_and(|index| index >= failure.index) {
                return Err("batch failure entries are not in input order".into());
            }
            previous = Some(failure.index);
        }
        Ok(())
    }
}

fn truncate_utf8(value: &mut String, maximum_bytes: usize) -> bool {
    if value.len() <= maximum_bytes {
        return false;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    true
}

fn encode_bounded_json<T: Serialize>(
    value: &T,
    maximum_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut writer = BoundedJsonWriter::new(maximum_bytes);
    serde_json::to_writer_pretty(&mut writer, value)
        .map_err(|error| format!("encode {label}: {error}"))?;
    writer
        .write_all(b"\n")
        .map_err(|error| format!("encode {label}: {error}"))?;
    Ok(writer.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resumes_only_hash_verified_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let first_input = directory.path().join("one.wav");
        let second_input = directory.path().join("two.wav");
        let first_output = directory.path().join("one-out.wav");
        let second_output = directory.path().join("two-out.wav");
        let state = directory.path().join("job.json");
        std::fs::write(&first_input, b"one").unwrap();
        std::fs::write(&second_input, b"two").unwrap();
        let assets = [
            BatchAssetSpec::new(&first_input, &first_output),
            BatchAssetSpec::new(&second_input, &second_output),
        ];
        let operation = json!({"mode": "lufs", "target": -16.0});

        let mut job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        assert_eq!(job.asset_count(), 2);
        assert_eq!(job.completed_count(), 0);
        std::fs::write(&first_output, b"normalized").unwrap();
        job.mark_completed(0).unwrap();
        drop(job);

        let job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        assert!(job.is_completed(0));
        assert!(!job.is_completed(1));
        drop(job);

        std::fs::write(&first_output, b"changed").unwrap();
        assert!(BatchJob::open(&state, &assets, &operation, false)
            .unwrap_err()
            .contains("completed output changed"));
        let job = BatchJob::open(&state, &assets, &operation, true).unwrap();
        assert!(!job.is_completed(0));
        assert_eq!(job.completed_count(), 0);
    }

    #[test]
    fn input_or_operation_changes_cannot_reuse_a_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job.json");
        std::fs::write(&input, b"first").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        BatchJob::open(&state, &assets, &json!({"target": -16}), false).unwrap();

        assert!(
            BatchJob::open(&state, &assets, &json!({"target": -23}), false)
                .unwrap_err()
                .contains("does not match")
        );
        std::fs::write(&input, b"second").unwrap();
        assert!(
            BatchJob::open(&state, &assets, &json!({"target": -16}), false)
                .unwrap_err()
                .contains("does not match")
        );
    }

    #[test]
    fn a_batch_state_cannot_be_opened_twice() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job.json");
        std::fs::write(&input, b"input").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        let operation = json!({"target": -16});

        let job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        let error = BatchJob::open(&state, &assets, &operation, false).unwrap_err();
        assert!(error.contains("already open"), "{error}");
        drop(job);
        BatchJob::open(&state, &assets, &operation, false).unwrap();
    }

    #[test]
    fn valid_v1_state_is_migrated_to_v2() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job.json");
        std::fs::write(&input, b"input").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        let operation = json!({"target": -16});

        drop(BatchJob::open(&state, &assets, &operation, false).unwrap());
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        document["schema"] = BATCH_JOB_SCHEMA_V1.into();
        std::fs::write(&state, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

        drop(BatchJob::open(&state, &assets, &operation, false).unwrap());
        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        assert_eq!(migrated["schema"], BATCH_JOB_SCHEMA_V2);
    }

    #[test]
    fn ready_checkpoint_recovers_a_published_output() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let staged = directory.path().join("staged.wav");
        let state = directory.path().join("job.json");
        std::fs::write(&input, b"input").unwrap();
        std::fs::write(&staged, b"normalized").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        let operation = json!({"target": -16});

        let mut job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        job.mark_ready_to_publish(0, &staged).unwrap();
        std::fs::rename(&staged, &output).unwrap();
        drop(job);

        let job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        assert!(job.is_completed(0));
        assert_eq!(job.completed_count(), 1);
    }

    #[test]
    fn ready_checkpoint_without_publication_is_requeued() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let staged = directory.path().join("staged.wav");
        let state = directory.path().join("job.json");
        std::fs::write(&input, b"input").unwrap();
        std::fs::write(&staged, b"normalized").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        let operation = json!({"target": -16});

        let mut job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        job.mark_ready_to_publish(0, &staged).unwrap();
        drop(job);
        std::fs::remove_file(&staged).unwrap();

        let job = BatchJob::open(&state, &assets, &operation, false).unwrap();
        assert!(!job.is_completed(0));
        assert_eq!(job.completed_count(), 0);
    }

    fn v3_context() -> Value {
        v3_context_for_asset_count(1)
    }

    fn v3_context_for_asset_count(asset_count: usize) -> Value {
        crate::runtime_fingerprint::normalization_semantic_context(
            crate::analysis::AnalysisEngine::Fast,
            None,
            &vec![crate::normalize::OutputFormat::Wav; asset_count],
        )
        .unwrap()
    }

    fn v3_operation(asset_count: usize) -> Value {
        json!({
            "schema": "forge-normalization-operation-v1",
            "mode": "lufs",
            "target_lufs": -16.0,
            "target_peak_dbfs": -1.0,
            "target_rms_dbfs": -18.0,
            "ceiling_dbtp": -1.0,
            "max_gain_db": null,
            "dither": false,
            "output_bits": null,
            "bitrate_kbps": 192,
            "encoder_quality": 5,
            "limiter": null,
            "wav_container": "auto",
            "bwf": false,
            "output_sample_rate_hz": null,
            "resample_quality": "balanced",
            "verify": false,
            "verify_tolerance": 0.1,
            "verify_retries": 0,
            "album": false,
            "analysis_engine": "forge-fast-bs1770-r4",
            "audio_track": null,
            "channel_layout": null,
            "dual_mono": false,
            "formats": vec!["wav"; asset_count],
        })
    }

    fn open_test_v3(
        directory: &std::path::Path,
        context: &Value,
        revision: u32,
        policy: BatchFailurePolicy,
    ) -> Result<BatchJob, String> {
        let input = directory.join("input.wav");
        let output = directory.join("output.wav");
        let state = directory.join("job-v3.json");
        if !input.exists() {
            std::fs::write(&input, b"input").unwrap();
        }
        BatchJob::open_v3(
            state,
            &[BatchAssetSpec::new(input, output)],
            &v3_operation(1),
            context,
            revision,
            policy,
            false,
        )
    }

    #[test]
    fn v3_persists_and_exposes_semantic_context_and_identity() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        let job =
            open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::KeepGoing).unwrap();
        let job_id = job.job_id().unwrap().to_owned();
        let semantic_fingerprint = job.semantic_fingerprint().unwrap().to_owned();
        assert_eq!(job.fingerprint_revision(), Some(1));
        assert_eq!(job.failure_policy(), Some(BatchFailurePolicy::KeepGoing));
        assert_eq!(job.semantic_context(), Some(&context));
        drop(job);
        let state: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("job-v3.json")).unwrap())
                .unwrap();
        assert_eq!(state["schema"], BATCH_JOB_SCHEMA_V3);
        assert_eq!(state["job_id"], job_id);
        assert_eq!(state["semantic_fingerprint"], semantic_fingerprint);
        assert_eq!(state["semantic_context"], context);
    }

    #[test]
    fn v3_checkpoints_only_a_complete_published_generation() {
        let directory = tempfile::tempdir().unwrap();
        let first_input = directory.path().join("first.wav");
        let second_input = directory.path().join("second.wav");
        let first_output = directory.path().join("first-out.wav");
        let second_output = directory.path().join("second-out.wav");
        let state = directory.path().join("job-v3.json");
        std::fs::write(&first_input, b"first").unwrap();
        std::fs::write(&second_input, b"second").unwrap();
        let assets = [
            BatchAssetSpec::new(&first_input, &first_output),
            BatchAssetSpec::new(&second_input, &second_output),
        ];
        let mut job = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(2),
            &v3_context_for_asset_count(2),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap();

        let mut first_stage =
            crate::atomic::AtomicOutput::new_with_overwrite(&first_output, true).unwrap();
        first_stage.write_all(b"normalized-first").unwrap();
        let mut second_stage =
            crate::atomic::AtomicOutput::new_with_overwrite(&second_output, true).unwrap();
        second_stage.write_all(b"normalized-second").unwrap();
        let generation_path = directory.path().join("generation.json");
        let transaction = crate::generation::GenerationTransaction::prepare(
            &generation_path,
            job.job_id().unwrap(),
            vec![
                crate::generation::PreparedGenerationOutput::from_atomic(first_stage).unwrap(),
                crate::generation::PreparedGenerationOutput::from_atomic(second_stage).unwrap(),
            ],
        )
        .unwrap();
        let ready = crate::generation::GenerationTransaction::inspect(&generation_path).unwrap();
        let state_before_rejected_checkpoint = std::fs::read(&state).unwrap();
        let error = job
            .mark_generation_completed_with_evidence(&ready)
            .unwrap_err();
        assert!(error.contains("committed"), "{error}");
        assert_eq!(
            std::fs::read(&state).unwrap(),
            state_before_rejected_checkpoint
        );
        assert_eq!(job.completed_count(), 0);

        let committed = transaction.commit().unwrap();
        job.mark_generation_completed_with_evidence(&committed)
            .unwrap();
        assert!(job.is_complete());
        assert_eq!(std::fs::read(&first_output).unwrap(), b"normalized-first");
        assert_eq!(std::fs::read(&second_output).unwrap(), b"normalized-second");
        let completed_state = std::fs::read(&state).unwrap();
        job.mark_generation_completed_with_evidence(&committed)
            .unwrap();
        assert_eq!(std::fs::read(&state).unwrap(), completed_state);
        drop(job);

        let reopened = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(2),
            &v3_context_for_asset_count(2),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap();
        assert!(reopened.is_complete());
    }

    #[test]
    fn v3_rejects_per_asset_checkpoints_without_mutating_state() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let staged = directory.path().join("staged.wav");
        std::fs::write(&input, b"input").unwrap();
        std::fs::write(&staged, b"staged").unwrap();
        let state = directory.path().join("job-v3.json");
        let assets = [BatchAssetSpec::new(&input, &output)];
        let mut job = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap();
        let before = std::fs::read(&state).unwrap();

        let error = job.mark_ready_to_publish(0, &staged).unwrap_err();
        assert!(
            error.contains("generation") || error.contains("readiness"),
            "{error}"
        );
        assert_eq!(std::fs::read(&state).unwrap(), before);
        assert_eq!(job.completed_count(), 0);
        assert!(!job.is_completed(0));

        let error = job.mark_completed(0).unwrap_err();
        assert!(
            error.contains("generation") || error.contains("checkpoint"),
            "{error}"
        );
        assert_eq!(std::fs::read(&state).unwrap(), before);
        assert_eq!(job.completed_count(), 0);
        assert!(!job.is_completed(0));
    }

    #[test]
    fn v3_verifies_different_input_snapshot_binding_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        let job =
            open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let options = crate::stable_input::StableInputOptions::new(1024).unwrap();
        let matching = crate::stable_input::StableInput::from_bytes(b"input", &options).unwrap();
        let different =
            crate::stable_input::StableInput::from_bytes(b"different", &options).unwrap();
        assert!(job.verify_input_binding(0, matching.binding()).is_ok());
        let error = job
            .verify_input_binding(0, different.binding())
            .unwrap_err();
        assert!(
            error.contains("changed") || error.contains("fingerprint"),
            "{error}"
        );
    }

    #[test]
    fn v3_generation_evidence_uses_lexically_normalized_output_paths() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let input = directory.path().join("input.wav");
        let output_alias = nested.join("..").join("output.wav");
        std::fs::write(&input, b"input").unwrap();
        let state = directory.path().join("job-v3.json");
        let assets = [BatchAssetSpec::new(&input, &output_alias)];
        let mut job = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap();
        let state_value: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        let normalized_output = directory.path().join("output.wav");
        assert_eq!(
            state_value["assets"][0]["output"],
            normalized_output.to_string_lossy().as_ref()
        );

        let mut stage =
            crate::atomic::AtomicOutput::new_with_overwrite(&output_alias, true).unwrap();
        stage.write_all(b"normalized").unwrap();
        let generation_path = directory.path().join("generation.json");
        let transaction = crate::generation::GenerationTransaction::prepare(
            &generation_path,
            job.job_id().unwrap(),
            vec![crate::generation::PreparedGenerationOutput::from_atomic(stage).unwrap()],
        )
        .unwrap();
        let status = transaction.commit().unwrap();
        job.mark_generation_completed_with_evidence(&status)
            .unwrap();
        assert!(job.is_complete());
    }

    #[test]
    fn v3_completed_generation_reset_requires_all_assets_and_clears_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job-v3.json");
        let generation_path = directory.path().join("generation.json");
        std::fs::write(&input, b"input").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        let context = v3_context();
        let mut job = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(1),
            &context,
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap();
        assert!(job
            .reset_completed_generation_for_rebuild()
            .unwrap_err()
            .contains("completed"));

        let mut stage = crate::atomic::AtomicOutput::new_with_overwrite(&output, true).unwrap();
        stage.write_all(b"normalized").unwrap();
        let transaction = crate::generation::GenerationTransaction::prepare(
            &generation_path,
            job.job_id().unwrap(),
            vec![crate::generation::PreparedGenerationOutput::from_atomic(stage).unwrap()],
        )
        .unwrap();
        let status = transaction.commit().unwrap();
        job.mark_generation_completed_with_evidence(&status)
            .unwrap();
        assert!(job.is_complete());
        job.reset_completed_generation_for_rebuild().unwrap();
        assert_eq!(job.completed_count(), 0);
        assert!(!job.is_complete());
        assert!(!job.is_completed(0));
        drop(job);

        let reopened = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(1),
            &context,
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap();
        assert_eq!(reopened.completed_count(), 0);
        assert!(!reopened.is_complete());
    }

    #[test]
    fn v3_semantic_context_track_and_writer_changes_cannot_resume() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let mut track_context = context.clone();
        track_context["audio_track"] = 1.into();
        track_context["audio_track_selection"]["kind"] = "index".into();
        track_context["audio_track_selection"]["index"] = 1.into();
        drop(
            open_test_v3(
                directory.path(),
                &track_context,
                1,
                BatchFailurePolicy::FailFast,
            )
            .unwrap_err(),
        );
        let mut writer_context = context;
        writer_context["writers"][0]["pipeline_revision"] = "forge-wav-writer-v2".into();
        drop(
            open_test_v3(
                directory.path(),
                &writer_context,
                1,
                BatchFailurePolicy::FailFast,
            )
            .unwrap_err(),
        );
    }

    #[test]
    fn v3_fingerprint_revision_change_cannot_resume() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let error =
            open_test_v3(directory.path(), &context, 2, BatchFailurePolicy::FailFast).unwrap_err();
        assert!(
            error.contains("fingerprint") || error.contains("semantic"),
            "{error}"
        );
    }

    #[test]
    fn v3_canonicalizes_semantic_context_object_key_order() {
        let directory = tempfile::tempdir().unwrap();
        let first = v3_context();
        let second = {
            let object = first.as_object().unwrap();
            let mut reversed = serde_json::Map::new();
            let entries = object.iter().collect::<Vec<_>>();
            for (key, value) in entries.into_iter().rev() {
                reversed.insert(key.clone(), value.clone());
            }
            Value::Object(reversed)
        };
        let first_job =
            open_test_v3(directory.path(), &first, 1, BatchFailurePolicy::FailFast).unwrap();
        let first_fingerprint = first_job.semantic_fingerprint().unwrap().to_owned();
        drop(first_job);
        let second_job =
            open_test_v3(directory.path(), &second, 1, BatchFailurePolicy::FailFast).unwrap();
        assert_eq!(
            second_job.semantic_fingerprint(),
            Some(first_fingerprint.as_str())
        );
    }

    #[test]
    fn v3_rejects_deep_or_oversized_semantic_context_before_state_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut deep = Value::Null;
        for _ in 0..=MAX_SEMANTIC_CONTEXT_DEPTH {
            deep = Value::Array(vec![deep]);
        }
        let error =
            open_test_v3(directory.path(), &deep, 1, BatchFailurePolicy::FailFast).unwrap_err();
        assert!(error.contains("depth"), "{error}");
        assert!(!directory.path().join("job-v3.json").exists());

        let oversized = Value::String("x".repeat(MAX_SEMANTIC_CONTEXT_BYTES + 1));
        let error = open_test_v3(
            directory.path(),
            &oversized,
            1,
            BatchFailurePolicy::FailFast,
        )
        .unwrap_err();
        assert!(error.contains("byte") || error.contains("size"), "{error}");
    }

    #[test]
    fn v3_refuses_v2_state_without_implicit_migration() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job-v3.json");
        std::fs::write(&input, b"input").unwrap();
        let assets = [BatchAssetSpec::new(&input, &output)];
        BatchJob::open(&state, &assets, &json!({"target": -16}), false).unwrap();
        let error = BatchJob::open_v3(
            &state,
            &assets,
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(error.contains("legacy") || error.contains("v2"), "{error}");
    }

    #[test]
    fn v3_rejects_tampered_context_and_generator_suffix() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let state_path = directory.path().join("job-v3.json");
        let mut state: Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        state["semantic_context"]["writers"][0]["implementation_id"] = "writer-tampered".into();
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let error =
            open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap_err();
        assert!(
            error.contains("context") || error.contains("fingerprint"),
            "{error}"
        );

        let directory = tempfile::tempdir().unwrap();
        open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let state_path = directory.path().join("job-v3.json");
        let mut state: Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        state["generator"] = "forge-normalizer/0.189.15-suffix".into();
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let error =
            open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap_err();
        assert!(error.contains("generator"), "{error}");
    }

    #[test]
    fn v3_rejects_incomplete_or_unknown_semantic_context_before_state_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut incomplete = v3_context();
        incomplete.as_object_mut().unwrap().remove("writers");
        let error = open_test_v3(
            directory.path(),
            &incomplete,
            1,
            BatchFailurePolicy::FailFast,
        )
        .unwrap_err();
        assert!(
            error.contains("missing") || error.contains("context"),
            "{error}"
        );
        assert!(!directory.path().join("job-v3.json").exists());
        assert!(!directory.path().join("job-v3.json.lock").exists());

        let directory = tempfile::tempdir().unwrap();
        let mut unknown = v3_context();
        unknown["unexpected"] = true.into();
        let error =
            open_test_v3(directory.path(), &unknown, 1, BatchFailurePolicy::FailFast).unwrap_err();
        assert!(
            error.contains("unknown") || error.contains("context"),
            "{error}"
        );
        assert!(!directory.path().join("job-v3.json").exists());
        assert!(!directory.path().join("job-v3.json.lock").exists());

        let directory = tempfile::tempdir().unwrap();
        let mut nested_unknown = v3_context();
        nested_unknown["writers"][0]["runtime"]["unexpected"] = true.into();
        let error = open_test_v3(
            directory.path(),
            &nested_unknown,
            1,
            BatchFailurePolicy::FailFast,
        )
        .unwrap_err();
        assert!(
            error.contains("unknown") || error.contains("runtime"),
            "{error}"
        );
        assert!(!directory.path().join("job-v3.json").exists());
    }

    #[test]
    fn v3_rejects_control_characters_in_asset_paths_before_state_creation() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        std::fs::write(&input, b"input").unwrap();
        let state = directory.path().join("job-v3.json");
        let output = directory.path().join("out\nput.wav");
        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, output)],
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("control") || error.contains("path"),
            "{error}"
        );
        assert!(!state.exists());
        assert!(!state.with_file_name("job-v3.json.lock").exists());
    }

    #[test]
    fn v3_rejects_operation_and_context_format_or_track_mismatch_before_state_creation() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job-v3.json");
        std::fs::write(&input, b"input").unwrap();
        let mut operation = v3_operation(1);
        operation["formats"][0] = "flac".into();
        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, &output)],
            &operation,
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("format") || error.contains("context"),
            "{error}"
        );
        assert!(!state.exists());
        assert!(!state.with_file_name("job-v3.json.lock").exists());

        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job-v3.json");
        std::fs::write(&input, b"input").unwrap();
        let mut context = v3_context();
        context["output_formats"] = Value::Array(Vec::new());
        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, &output)],
            &v3_operation(1),
            &context,
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("format") || error.contains("asset"),
            "{error}"
        );
        assert!(!state.exists());

        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job-v3.json");
        std::fs::write(&input, b"input").unwrap();
        let mut operation = v3_operation(1);
        operation["audio_track"] = 2.into();
        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, &output)],
            &operation,
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("audio_track") || error.contains("context"),
            "{error}"
        );
        assert!(!state.exists());
    }

    #[test]
    fn v3_normalizes_state_path_and_rejects_asset_state_aliases_before_lock_creation() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("sub").join("..").join("output.wav");
        std::fs::write(&input, b"input").unwrap();
        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, &output)],
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("aliases") || error.contains("state"),
            "{error}"
        );
        assert!(!directory.path().join("output.wav").exists());
        assert!(!directory.path().join("sub").exists());
    }

    #[cfg(unix)]
    #[test]
    fn v3_rejects_a_state_alias_through_an_existing_symlink_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let input = directory.path().join("input.wav");
        std::fs::write(&input, b"input").unwrap();
        let state = real.join("job-v3.json");
        let output = alias.join("job-v3.json");

        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, &output)],
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("aliases") || error.contains("state"),
            "{error}"
        );
        assert!(!state.exists());
        assert!(!sibling_lock_path(&state).unwrap().exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn v3_rejects_an_output_hard_linked_to_its_input() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let output = directory.path().join("output.wav");
        let state = directory.path().join("job-v3.json");
        std::fs::write(&input, b"input").unwrap();
        std::fs::hard_link(&input, &output).unwrap();

        let error = BatchJob::open_v3(
            &state,
            &[BatchAssetSpec::new(&input, &output)],
            &v3_operation(1),
            &v3_context(),
            1,
            BatchFailurePolicy::FailFast,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("hard-link") || error.contains("aliases"),
            "{error}"
        );
        assert!(!state.exists());
        assert!(!sibling_lock_path(&state).unwrap().exists());
    }

    #[test]
    fn v3_rejects_generator_provenance_over_256_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let state_path = directory.path().join("job-v3.json");
        let mut state: Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        state["generator"] = format!("forge-normalizer/{}.1.1", "1".repeat(240)).into();
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let error =
            open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap_err();
        assert!(error.contains("generator"), "{error}");
    }

    #[test]
    fn v3_context_limit_leaves_room_under_state_limit() {
        let directory = tempfile::tempdir().unwrap();
        let context = v3_context();
        open_test_v3(directory.path(), &context, 1, BatchFailurePolicy::FailFast).unwrap();
        let state_len = std::fs::metadata(directory.path().join("job-v3.json"))
            .unwrap()
            .len();
        assert!(state_len <= MAX_STATE_BYTES);
    }

    #[test]
    fn failure_report_is_bounded_truncated_and_input_ordered() {
        let hash = "a".repeat(64);
        let mut report = BatchFailureReport::new(
            &hash,
            &hash,
            1,
            BatchFailurePolicy::KeepGoing,
            MAX_BATCH_FAILURES + 4,
        );
        report
            .add_failure(BatchFailure::new(2, "in-2", "out-2", "second"))
            .unwrap();
        report
            .add_failure(BatchFailure::new(0, "in-0", "out-0", "first"))
            .unwrap();
        for index in 1..(MAX_BATCH_FAILURES + 4) {
            if index == 2 {
                continue;
            }
            report
                .add_failure(BatchFailure::new(
                    index,
                    format!("in-{index}"),
                    format!("out-{index}"),
                    "failure",
                ))
                .unwrap();
        }
        assert_eq!(report.failures.len(), MAX_BATCH_FAILURES);
        assert_eq!(report.dropped_count, 4);
        assert!(report.truncated);
        assert_eq!(report.failures[0].index, 0);
        assert_eq!(report.failures[1].index, 1);
        assert!(report
            .add_failure(BatchFailure::new(
                MAX_BATCH_FAILURES + 3,
                "duplicate-input",
                "duplicate-output",
                "duplicate",
            ))
            .unwrap_err()
            .contains("duplicate"));
        let bytes = report.to_bytes().unwrap();
        assert!(bytes.len() <= MAX_BATCH_FAILURE_REPORT_BYTES);
        let mut decoded: BatchFailureReport = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded
            .add_failure(BatchFailure::new(0, "input", "output", "error"))
            .unwrap_err()
            .contains("decoded"));

        let mut short = BatchFailureReport::new(&hash, &hash, 1, BatchFailurePolicy::KeepGoing, 1);
        short
            .add_failure(BatchFailure::new(
                0,
                "input",
                "output",
                "あ".repeat(MAX_BATCH_FAILURE_ERROR_BYTES),
            ))
            .unwrap();
        assert!(short.truncated);
        assert!(short.failures[0].error.len() <= MAX_BATCH_FAILURE_ERROR_BYTES);
        assert!(short.to_bytes().is_ok());
    }

    #[test]
    fn failure_report_validation_rejects_unbounded_direct_values() {
        let hash = "b".repeat(64);
        assert!(
            serde_json::to_value(BatchFailure::new(MAX_ASSETS, "input", "output", "error"))
                .is_err()
        );
        assert!(serde_json::from_value::<BatchFailure>(serde_json::json!({
            "index": MAX_ASSETS,
            "input": "input",
            "output": "output",
            "error": "error",
        }))
        .is_err());

        let mut report = BatchFailureReport::new(&hash, &hash, 1, BatchFailurePolicy::FailFast, 1);
        report.failures.push(BatchFailure::new(
            0,
            "input",
            "output",
            "e".repeat(MAX_BATCH_FAILURE_ERROR_BYTES + 1),
        ));
        report.failed = 1;
        assert!(report.to_bytes().unwrap_err().contains("exceeds"));

        let invalid = serde_json::json!({
            "schema": BATCH_FAILURE_REPORT_SCHEMA_V1,
            "generator": format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            "job_id": hash,
            "semantic_fingerprint": "b".repeat(64),
            "fingerprint_revision": 1,
            "failure_policy": "fail_fast",
            "total": 1,
            "succeeded": 0,
            "skipped": 0,
            "failed": 1,
            "failures": [{
                "index": 0,
                "input": "input",
                "output": "output",
                "error": "e".repeat(MAX_BATCH_FAILURE_ERROR_BYTES + 1),
            }],
            "truncated": false,
            "dropped_count": 0,
        });
        assert!(serde_json::from_value::<BatchFailureReport>(invalid).is_err());
    }

    #[test]
    fn failure_report_truncates_retained_entries_to_aggregate_wire_limit() {
        let hash = "f".repeat(64);
        let total = 512;
        let input = "i".repeat(MAX_BATCH_PATH_BYTES);
        let output = "o".repeat(MAX_BATCH_PATH_BYTES);
        let error = "e".repeat(MAX_BATCH_FAILURE_ERROR_BYTES);
        let mut report =
            BatchFailureReport::new(&hash, &hash, 1, BatchFailurePolicy::KeepGoing, total);
        for index in 0..total {
            report
                .add_failure(BatchFailure::new(
                    index,
                    input.clone(),
                    output.clone(),
                    error.clone(),
                ))
                .unwrap();
        }

        let bytes = report.to_bytes().unwrap();
        assert!(bytes.len() <= MAX_BATCH_FAILURE_REPORT_BYTES);
        let encoded: BatchFailureReport = serde_json::from_slice(&bytes).unwrap();
        assert!(encoded.truncated);
        assert!(encoded.dropped_count > 0);
        assert!(encoded.failures.len() < total);
        assert_eq!(encoded.failures.first().map(|entry| entry.index), Some(0));
        assert_eq!(
            encoded.failures.last().map(|entry| entry.index),
            encoded.failures.len().checked_sub(1)
        );
        encoded.validate().unwrap();
    }

    #[test]
    fn failure_entries_reject_overlong_paths_at_validation_and_insert() {
        let hash = "e".repeat(64);
        let overlong = "p".repeat(MAX_BATCH_PATH_BYTES + 1);
        let failure = BatchFailure::new(0, &overlong, "output", "error");
        assert!(failure.validate(1).unwrap_err().contains("input"));
        assert!(serde_json::to_vec(&failure).is_err());

        let mut report = BatchFailureReport::new(&hash, &hash, 1, BatchFailurePolicy::KeepGoing, 1);
        assert!(report
            .add_failure(BatchFailure::new(0, "input", &overlong, "error"))
            .unwrap_err()
            .contains("output"));
    }

    #[test]
    fn progress_v2_contains_job_generation_phase_and_job_failed() {
        let hash = "c".repeat(64);
        let mut event = BatchProgressEventV2::new(4, "job_failed", 1, 2, &hash, 7, "failed");
        event.error = Some("one asset failed\0".into());
        event.validate().unwrap();
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["schema"], BATCH_PROGRESS_SCHEMA_V2);
        assert_eq!(value["job_id"], hash);
        assert_eq!(value["generation"], 7);
        assert_eq!(value["phase"], "failed");
        assert_eq!(value["event"], "job_failed");
        assert!(value["error"].as_str().unwrap().contains("failed"));
    }

    #[test]
    fn progress_v2_rejects_invalid_identity_phase_and_bounded_fields() {
        let hash = "d".repeat(64);
        let mut invalid_identity =
            BatchProgressEventV2::new(0, "job_started", 0, 1, "not-a-hash", 1, "rendering");
        assert!(invalid_identity.validate().unwrap_err().contains("job_id"));
        assert!(serde_json::to_vec(&invalid_identity).is_err());

        invalid_identity.job_id = hash.clone();
        invalid_identity.phase = "unknown".into();
        assert!(invalid_identity.validate().unwrap_err().contains("phase"));

        let mut mismatched_phase =
            BatchProgressEventV2::new(0, "asset_started", 0, 1, &hash, 1, "committed");
        mismatched_phase.index = Some(0);
        mismatched_phase.input = Some("input".into());
        mismatched_phase.output = Some("output".into());
        assert!(mismatched_phase.validate().unwrap_err().contains("phase"));

        let mut long_phase =
            BatchProgressEventV2::new(0, "job_started", 0, 1, &hash, 1, "rendering");
        long_phase.phase = "x".repeat(MAX_BATCH_PROGRESS_PHASE_BYTES + 1);
        assert!(long_phase.validate().unwrap_err().contains("phase"));

        let mut long_path =
            BatchProgressEventV2::new(0, "asset_started", 0, 1, &hash, 1, "rendering");
        long_path.index = Some(0);
        long_path.input = Some("i".repeat(MAX_BATCH_PATH_BYTES + 1));
        long_path.output = Some("output".into());
        assert!(long_path.validate().unwrap_err().contains("input"));

        let mut long_error = BatchProgressEventV2::new(0, "job_failed", 0, 1, &hash, 1, "failed");
        long_error.error = Some("e".repeat(MAX_BATCH_FAILURE_ERROR_BYTES + 1));
        assert!(long_error.validate().unwrap_err().contains("error"));
    }

    #[test]
    fn fingerprint_accepts_the_maximum_ordered_normalization_formats() {
        use crate::analysis::AnalysisEngine;
        use crate::normalize::OutputFormat;
        use crate::runtime_fingerprint::{
            normalization_semantic_context, MAX_NORMALIZATION_SEMANTIC_OUTPUTS,
        };

        let formats = vec![OutputFormat::Wav; MAX_NORMALIZATION_SEMANTIC_OUTPUTS];
        let context = normalization_semantic_context(AnalysisEngine::Fast, None, &formats).unwrap();
        assert!(semantic_fingerprint(&context, 1).is_ok());
    }
}
