//! Durable state for resumable, content-bound normalization batches.
//!
//! A job state is committed atomically after every completed output. Inputs,
//! output paths, the operation descriptor, and completed outputs are bound by
//! SHA-256 so a resume never silently reuses work from different bytes or
//! settings.

use crate::atomic::AtomicOutput;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const BATCH_JOB_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-job-v1";
pub const BATCH_PROGRESS_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/batch-progress-v1";
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ASSETS: usize = 100_000;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

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
    asset_count: usize,
    completed_count: usize,
    assets: Vec<JobAsset>,
}

/// A validated resumable job backed by an atomically updated JSON document.
#[derive(Debug)]
pub struct BatchJob {
    path: PathBuf,
    document: JobDocument,
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
        let expected_assets = build_assets(assets)?;
        reject_duplicate_paths(&expected_assets)?;
        let specification_sha256 = specification_hash(operation, &expected_assets)?;
        let generator = format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION"));

        let mut job = if path.exists() {
            let metadata = std::fs::metadata(&path)
                .map_err(|error| format!("inspect {}: {error}", path.display()))?;
            if metadata.len() > MAX_STATE_BYTES {
                return Err(format!(
                    "{} exceeds the {}-byte batch state limit",
                    path.display(),
                    MAX_STATE_BYTES
                ));
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let document: JobDocument = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode {}: {error}", path.display()))?;
            validate_document(
                &document,
                operation,
                &specification_sha256,
                &expected_assets,
            )?;
            Self { path, document }
        } else {
            let document = JobDocument {
                schema: BATCH_JOB_SCHEMA_V1.into(),
                generator,
                operation: operation.clone(),
                specification_sha256,
                asset_count: expected_assets.len(),
                completed_count: 0,
                assets: expected_assets,
            };
            let job = Self { path, document };
            job.save()?;
            job
        };

        let mut changed = false;
        for asset in &mut job.document.assets {
            if asset.status != AssetStatus::Completed {
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
            }
        }
        if changed {
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

    pub fn is_completed(&self, index: usize) -> bool {
        self.document
            .assets
            .get(index)
            .is_some_and(|asset| asset.status == AssetStatus::Completed)
    }

    /// Hash the completed output and atomically commit the updated checkpoint.
    pub fn mark_completed(&mut self, index: usize) -> Result<(), String> {
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

fn reject_duplicate_paths(assets: &[JobAsset]) -> Result<(), String> {
    let mut inputs = std::collections::BTreeSet::new();
    let mut outputs = std::collections::BTreeSet::new();
    for asset in assets {
        if !inputs.insert(&asset.input) {
            return Err(format!("duplicate batch input: {}", asset.input));
        }
        if !outputs.insert(&asset.output) {
            return Err(format!("duplicate batch output: {}", asset.output));
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
    if document.schema != BATCH_JOB_SCHEMA_V1 {
        return Err(format!(
            "unsupported batch state schema {}",
            document.schema
        ));
    }
    if !document.generator.starts_with("forge-normalizer/") {
        return Err("batch state contains an invalid generator".into());
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
            || (stored.status == AssetStatus::Completed && stored.output_sha256.is_none())
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

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("resumable batch paths must be UTF-8: {}", path.display()))
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
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
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
}
