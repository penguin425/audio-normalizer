//! Restartable, single-file metadata-only transactions.
//!
//! A metadata mutation is first applied to a complete, persistent sibling
//! stage.  The live source is replaced exactly once, after the stage has been
//! verified and the source preimage has been checked again.  The journal is
//! intentionally small and JSON based so a caller can inspect it without
//! loading a Rust object.
//!
//! This is a crash-recovery and cooperative-concurrency boundary, not an
//! authenticated journal for mutually hostile writers.  The caller must keep
//! the state file, its containing directory, and the source's containing
//! directory under a trusted owner while a transaction is live.  An attacker
//! able to rewrite both the JSON state and its recorded stage can forge their
//! evidence, and portable pathname publication cannot exclude hostile renames
//! in the source directory after the final comparison.  Use an authenticated
//! higher-level protocol when those paths cross a trust boundary.

use crate::atomic::AtomicOutput;
use crate::stable_input::{
    identity_from_open_file, StableFileIdentity, StableInput, StableInputOptions,
};
use crate::state_lock::{read_regular_state_file, StateFileLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub use crate::metadata_fidelity::MetadataPolicy;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

/// Persistent state schema for a single-file metadata transaction.
pub const METADATA_JOB_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/metadata-job-v1";

/// Revision of the metadata field registry bound into each transaction.
pub const METADATA_REGISTRY_REVISION: &str = crate::metadata_fidelity::METADATA_REGISTRY_REVISION;

/// Compatibility alias for callers that named the registry revision as a
/// policy revision before the metadata-fidelity module was introduced.
pub const METADATA_POLICY_REVISION: &str = METADATA_REGISTRY_REVISION;

/// Default implementation identity for the stage writer boundary.
pub const METADATA_WRITER_REVISION: &str = "forge-metadata-writer-v1";

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OPERATION_BYTES: usize = 1024 * 1024;
const MAX_OPERATION_DEPTH: usize = 64;
const MAX_OPERATION_NODES: usize = 100_000;
const DEFAULT_MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;
const STAGE_PREFIX: &str = ".forge-";

/// Request identity and storage settings for a metadata-only transaction.
///
/// The operation value is intentionally opaque to this layer.  Container
/// registries and callers can define its fields, while this transaction layer
/// binds the canonical value, policy, registry revision, and writer revision
/// to the durable job fingerprint.
///
/// The state path and both its parent and the source parent must be protected
/// from hostile writers.  The state document is validated but is not signed or
/// MAC-authenticated.
#[derive(Clone, Debug)]
pub struct MetadataTransactionRequest {
    source: PathBuf,
    state: PathBuf,
    operation: Value,
    policy: MetadataPolicy,
    registry_revision: String,
    writer_revision: String,
    max_source_bytes: u64,
}

impl MetadataTransactionRequest {
    /// Create a metadata transaction request using the default policy and
    /// revision identifiers.
    pub fn new(source: impl Into<PathBuf>, state: impl Into<PathBuf>, operation: Value) -> Self {
        Self {
            source: source.into(),
            state: state.into(),
            operation,
            policy: MetadataPolicy::Preserve,
            registry_revision: METADATA_REGISTRY_REVISION.into(),
            writer_revision: METADATA_WRITER_REVISION.into(),
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
        }
    }

    pub fn with_policy(mut self, policy: MetadataPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_registry_revision(mut self, revision: impl Into<String>) -> Self {
        self.registry_revision = revision.into();
        self
    }

    pub fn with_writer_revision(mut self, revision: impl Into<String>) -> Self {
        self.writer_revision = revision.into();
        self
    }

    pub fn with_max_source_bytes(mut self, max_source_bytes: u64) -> Self {
        self.max_source_bytes = max_source_bytes;
        self
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn operation(&self) -> &Value {
        &self.operation
    }

    pub const fn policy(&self) -> MetadataPolicy {
        self.policy
    }

    pub fn registry_revision(&self) -> &str {
        &self.registry_revision
    }

    pub fn writer_revision(&self) -> &str {
        &self.writer_revision
    }

    pub const fn max_source_bytes(&self) -> u64 {
        self.max_source_bytes
    }
}

/// Durable lifecycle of one metadata transaction.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataTransactionPhase {
    Prepared,
    ReadyToCommit,
    Committed,
}

/// Public, stable receipt returned after one source-path publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataCommitReceipt {
    job_id: String,
    source: PathBuf,
    source_sha256: String,
    output_sha256: String,
    mutation: Option<Value>,
    verification: Option<Value>,
}

impl MetadataCommitReceipt {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    /// SHA-256 of the source preimage captured before the transaction.
    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }

    /// SHA-256 of the complete bytes published by the transaction.
    pub fn output_sha256(&self) -> &str {
        &self.output_sha256
    }

    pub fn mutation(&self) -> Option<&Value> {
        self.mutation.as_ref()
    }

    pub fn verification(&self) -> Option<&Value> {
        self.verification.as_ref()
    }
}

/// Result of opening a transaction after a process restart.
#[non_exhaustive]
#[derive(Debug)]
pub enum MetadataResume {
    /// No complete stage is available; the caller must run `stage` again.
    Prepared(MetadataTransaction),
    /// A verified persistent stage is ready for one CAS publication.
    Ready(ReadyMetadataCommit),
    /// The source already contains the recorded output bytes.
    Committed(MetadataCommitReceipt),
}

/// A prepared transaction owns the process lifetime state lock.
#[derive(Debug)]
pub struct MetadataTransaction {
    state_path: PathBuf,
    source: PathBuf,
    document: JobDocument,
    stable_input: Option<StableInput>,
    _state_lock: StateFileLock,
}

/// Capability for the private pathname owned by an active metadata
/// transaction.
///
/// Values of this type are created only by [`MetadataTransaction::stage`] and
/// are borrowed only for the duration of its mutation and verification
/// callbacks. Writer APIs that accept this capability therefore cannot be
/// called directly on a live source pathname.
pub struct MetadataStage {
    path: PathBuf,
}

impl MetadataStage {
    /// Return the private stage pathname for bounded readback or a
    /// transaction-aware writer.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for MetadataStage {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl std::ops::Deref for MetadataStage {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

/// A complete, verified stage which may replace its source exactly once.
pub struct ReadyMetadataCommit {
    state_path: PathBuf,
    source: PathBuf,
    document: JobDocument,
    staged: AtomicOutput,
    _state_lock: StateFileLock,
}

impl fmt::Debug for ReadyMetadataCommit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadyMetadataCommit")
            .field("state_path", &self.state_path)
            .field("source", &self.source)
            .field("document", &self.document)
            .field("staged_path", &self.staged.path())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEvidence {
    path: String,
    identity: String,
    byte_len: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageEvidence {
    /// A single filename relative to the source's canonical parent directory.
    path: String,
    identity: String,
    byte_len: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobDocument {
    schema: String,
    generator: String,
    job_id: String,
    revision: u64,
    phase: MetadataTransactionPhase,
    operation: Value,
    policy: MetadataPolicy,
    registry_revision: String,
    writer_revision: String,
    max_source_bytes: u64,
    semantic_fingerprint: String,
    source: FileEvidence,
    destination: FileEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<StageEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<FileEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mutation: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification: Option<Value>,
}

impl MetadataTransaction {
    /// Create or open a transaction journal and perform source/path preflight.
    ///
    /// Existing prepared and ready jobs are validated against the request.  A
    /// ready job is recovered to `committed` when its destination already has
    /// the recorded output hash, which closes the rename/state-write crash
    /// window without running a mutation twice.  This assumes the state and
    /// source-parent trust boundary documented on this module.
    pub fn prepare(request: MetadataTransactionRequest) -> Result<Self, String> {
        validate_request(&request)?;
        let source = canonical_source(&request.source)?;
        reject_multiply_linked(&source, "metadata source")?;
        let state_path = absolute_path(&request.state)?;
        reject_state_alias(&source, &state_path)?;

        let state_lock = StateFileLock::acquire(&state_path, "metadata transaction state")?;
        let existing =
            read_regular_state_file(&state_path, "metadata transaction state", MAX_STATE_BYTES)?;

        let Some(bytes) = existing else {
            let stable = capture_stable_input(&source, request.max_source_bytes)?;
            let source_evidence = stable_evidence(&stable, &source)?;
            let fingerprint = semantic_fingerprint(
                &source_evidence,
                &request.operation,
                request.policy,
                &request.registry_revision,
                &request.writer_revision,
                request.max_source_bytes,
            )?;
            let document = JobDocument {
                schema: METADATA_JOB_SCHEMA_V1.into(),
                generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
                job_id: fingerprint.clone(),
                revision: 0,
                phase: MetadataTransactionPhase::Prepared,
                operation: canonical_json(&request.operation),
                policy: request.policy,
                registry_revision: request.registry_revision,
                writer_revision: request.writer_revision,
                max_source_bytes: request.max_source_bytes,
                semantic_fingerprint: fingerprint,
                source: source_evidence.clone(),
                destination: source_evidence,
                stage: None,
                output: None,
                mutation: None,
                verification: None,
            };
            let mut transaction = Self {
                state_path,
                source,
                document,
                stable_input: Some(stable),
                _state_lock: state_lock,
            };
            transaction.save_state()?;
            return Ok(transaction);
        };

        let document: JobDocument = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "decode metadata transaction state {}: {error}",
                state_path.display()
            )
        })?;
        validate_document(&document, &source, &request)?;
        let mut transaction = Self {
            state_path,
            source,
            document,
            stable_input: None,
            _state_lock: state_lock,
        };

        match transaction.document.phase {
            MetadataTransactionPhase::Committed => {
                transaction.verify_committed_destination()?;
            }
            MetadataTransactionPhase::Prepared | MetadataTransactionPhase::ReadyToCommit => {
                let current = capture_file_evidence(
                    &transaction.source,
                    transaction.document.max_source_bytes,
                    "metadata transaction source",
                )?;
                if transaction.document.phase == MetadataTransactionPhase::ReadyToCommit
                    && transaction.document.stage.as_ref().is_some_and(|stage| {
                        current.identity == stage.identity
                            && current.byte_len == stage.byte_len
                            && current.sha256 == stage.sha256
                    })
                {
                    transaction.document.phase = MetadataTransactionPhase::Committed;
                    transaction.document.output = Some(current);
                    transaction.document.stage = None;
                    transaction.save_state()?;
                } else {
                    ensure_same_evidence(
                        &transaction.document.source,
                        &current,
                        "source preimage",
                    )?;
                    transaction.stable_input = Some(capture_stable_input(
                        &transaction.source,
                        transaction.document.max_source_bytes,
                    )?);
                }
            }
        }
        Ok(transaction)
    }

    /// Alias for callers that prefer an explicit open/resume naming style.
    pub fn open(request: MetadataTransactionRequest) -> Result<Self, String> {
        Self::prepare(request)
    }

    /// Open a job and classify the work needed after a process restart.
    pub fn resume(request: MetadataTransactionRequest) -> Result<MetadataResume, String> {
        let transaction = Self::prepare(request)?;
        match transaction.document.phase {
            MetadataTransactionPhase::Prepared => Ok(MetadataResume::Prepared(transaction)),
            MetadataTransactionPhase::Committed => {
                Ok(MetadataResume::Committed(transaction.receipt()?))
            }
            MetadataTransactionPhase::ReadyToCommit => {
                if transaction.stage_is_missing()? {
                    let mut transaction = transaction;
                    transaction.reset_to_prepared()?;
                    Ok(MetadataResume::Prepared(transaction))
                } else {
                    Ok(MetadataResume::Ready(transaction.into_ready()?))
                }
            }
        }
    }

    pub const fn phase(&self) -> MetadataTransactionPhase {
        self.document.phase
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn job_id(&self) -> &str {
        &self.document.job_id
    }

    pub fn semantic_fingerprint(&self) -> &str {
        &self.document.semantic_fingerprint
    }

    /// Stage a complete source copy, apply a caller-owned mutation only to the
    /// stage pathname, then run the caller-owned readback verifier.
    ///
    /// The callbacks receive only a capability for the private stage path.
    /// They must not retain its pathname after this call and must never open
    /// or mutate the source path. The transaction itself never passes the live
    /// source to either callback.
    pub fn stage<M, V>(mut self, mutate: M, verify: V) -> Result<ReadyMetadataCommit, String>
    where
        M: FnOnce(&MetadataStage) -> Result<Value, String>,
        V: FnOnce(&MetadataStage) -> Result<Value, String>,
    {
        if self.document.phase == MetadataTransactionPhase::Committed {
            return Err("metadata transaction is already committed".into());
        }
        if self.document.phase == MetadataTransactionPhase::ReadyToCommit {
            return self.into_ready();
        }

        let stable = self
            .stable_input
            .take()
            .ok_or("metadata transaction has no prepared source snapshot")?;
        reject_multiply_linked(&self.source, "metadata source")?;
        verify_stable_matches_document(&stable, &self.source, &self.document.source)?;
        let source_permissions = fs::metadata(&self.source)
            .map_err(|error| format!("inspect metadata source permissions: {error}"))?
            .permissions();

        let mut staged = AtomicOutput::new_with_overwrite_and_limit(
            &self.source,
            true,
            self.document.max_source_bytes,
        )?;
        let expected = capture_atomic_expected(&staged, self.document.max_source_bytes)?;
        if identity_token(&expected.identity) != self.document.source.identity
            || expected.byte_len != self.document.source.byte_len
            || expected.sha256 != self.document.source.sha256
        {
            return Err("metadata transaction source changed before staging".into());
        }
        copy_snapshot_to_stage(&stable, &mut staged, self.document.max_source_bytes)?;

        let stage = MetadataStage {
            path: staged.path().to_owned(),
        };
        let mutation = mutate(&stage)?;
        if mutation.is_null() {
            return Err("metadata transaction mutation evidence must not be null".into());
        }
        staged.adopt_path_writer_output()?;
        reject_multiply_linked(staged.path(), "metadata transaction stage")?;
        let verification = verify(&stage)?;
        if verification.is_null() {
            return Err("metadata transaction verification evidence must not be null".into());
        }
        staged.adopt_path_writer_output()?;
        reject_multiply_linked(staged.path(), "metadata transaction stage")?;
        fs::set_permissions(staged.path(), source_permissions)
            .map_err(|error| format!("preserve metadata source permissions: {error}"))?;

        let stage_evidence = capture_file_evidence(
            staged.path(),
            self.document.max_source_bytes,
            "metadata transaction stage",
        )?;
        if stage_evidence.byte_len == 0 {
            return Err("metadata transaction stage is empty".into());
        }
        staged
            .file_mut()
            .sync_all()
            .map_err(|error| format!("sync metadata transaction stage: {error}"))?;
        sync_stage_parent_directory(&self.source)?;
        maybe_fail(FailurePoint::BeforeReadyState)?;
        self.document.phase = MetadataTransactionPhase::ReadyToCommit;
        self.document.stage = Some(StageEvidence {
            path: Path::new(&stage_evidence.path)
                .strip_prefix(
                    self.source
                        .parent()
                        .ok_or("metadata source has no parent directory")?,
                )
                .map_err(|_| "metadata transaction stage escaped source directory")?
                .to_str()
                .ok_or("metadata transaction stage path is not UTF-8")?
                .to_owned(),
            identity: stage_evidence.identity,
            byte_len: stage_evidence.byte_len,
            sha256: stage_evidence.sha256,
        });
        self.document.output = None;
        self.document.mutation = Some(mutation);
        self.document.verification = Some(verification);
        self.save_state()?;
        // Keep the stage only after the Ready state is durable. If state
        // publication fails, dropping the still-cleanup-enabled temp file
        // prevents an untracked orphan; if the process dies in this tiny
        // interval, resume will safely recreate the missing stage.
        staged.retain_stage();
        maybe_fail(FailurePoint::AfterReadyState)?;

        Ok(ReadyMetadataCommit {
            state_path: self.state_path,
            source: self.source,
            document: self.document,
            staged,
            _state_lock: self._state_lock,
        })
    }

    /// Return the receipt for a state already recorded as committed.
    pub fn receipt(&self) -> Result<MetadataCommitReceipt, String> {
        if self.document.phase != MetadataTransactionPhase::Committed {
            return Err("metadata transaction is not committed".into());
        }
        receipt_from_document(&self.document, &self.source)
    }

    fn into_ready(self) -> Result<ReadyMetadataCommit, String> {
        if self.document.phase != MetadataTransactionPhase::ReadyToCommit {
            return Err("metadata transaction has no ready stage".into());
        }
        let stage = self
            .document
            .stage
            .as_ref()
            .ok_or("ready metadata transaction has no stage evidence")?;
        let stage_path = resolve_stage_path(&self.source, &stage.path)?;
        reject_multiply_linked(&stage_path, "metadata transaction stage")?;
        let expected_identity = parse_identity(&self.document.source.identity)?;
        let expected_sha256 = parse_sha256(&self.document.source.sha256)?;
        let staged = AtomicOutput::from_existing_stage(
            &self.source,
            &stage_path,
            expected_identity,
            self.document.source.byte_len,
            expected_sha256,
        )?;
        let observed = capture_file_evidence(
            &stage_path,
            self.document.max_source_bytes,
            "metadata transaction stage",
        )?;
        if observed.path != stage_path.to_string_lossy()
            || observed.identity != stage.identity
            || observed.byte_len != stage.byte_len
            || observed.sha256 != stage.sha256
        {
            return Err(format!(
                "metadata transaction stage evidence changed: {}",
                stage_path.display()
            ));
        }
        Ok(ReadyMetadataCommit {
            state_path: self.state_path,
            source: self.source,
            document: self.document,
            staged,
            _state_lock: self._state_lock,
        })
    }

    fn stage_is_missing(&self) -> Result<bool, String> {
        let stage = self
            .document
            .stage
            .as_ref()
            .ok_or("ready metadata transaction has no stage evidence")?;
        let path = resolve_stage_path(&self.source, &stage.path)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(format!(
                "inspect metadata transaction stage {}: {error}",
                path.display()
            )),
        }
    }

    fn reset_to_prepared(&mut self) -> Result<(), String> {
        if self.document.phase != MetadataTransactionPhase::ReadyToCommit {
            return Ok(());
        }
        self.document.phase = MetadataTransactionPhase::Prepared;
        self.document.stage = None;
        self.document.output = None;
        self.document.mutation = None;
        self.document.verification = None;
        self.stable_input = Some(capture_stable_input(
            &self.source,
            self.document.max_source_bytes,
        )?);
        verify_stable_matches_document(
            self.stable_input
                .as_ref()
                .expect("reset captures stable input"),
            &self.source,
            &self.document.source,
        )?;
        self.save_state()
    }

    fn verify_committed_destination(&self) -> Result<(), String> {
        let output = self
            .document
            .output
            .as_ref()
            .ok_or("committed metadata transaction has no output evidence")?;
        let current = capture_file_evidence(
            &self.source,
            self.document.max_source_bytes,
            "committed metadata transaction output",
        )?;
        if current.path != output.path
            || current.identity != output.identity
            || current.byte_len != output.byte_len
            || current.sha256 != output.sha256
        {
            return Err(format!(
                "committed metadata transaction output changed: {}",
                self.source.display()
            ));
        }
        Ok(())
    }

    fn save_state(&mut self) -> Result<(), String> {
        self.document.revision = self
            .document
            .revision
            .checked_add(1)
            .ok_or("metadata transaction state revision overflow")?;
        save_document(&self.state_path, &self.document)
    }
}

impl ReadyMetadataCommit {
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn job_id(&self) -> &str {
        &self.document.job_id
    }

    pub fn semantic_fingerprint(&self) -> &str {
        &self.document.semantic_fingerprint
    }

    pub fn staged_path(&self) -> &Path {
        self.staged.path()
    }

    pub fn stage_sha256(&self) -> &str {
        self.document
            .stage
            .as_ref()
            .expect("ready metadata transaction retains stage evidence")
            .sha256
            .as_str()
    }

    /// Caller-owned verification evidence durably stored with the ready stage.
    pub fn verification(&self) -> Option<&Value> {
        self.document.verification.as_ref()
    }

    /// Caller-owned mutation evidence durably stored with the ready stage.
    pub fn mutation(&self) -> Option<&Value> {
        self.document.mutation.as_ref()
    }

    pub const fn phase(&self) -> MetadataTransactionPhase {
        MetadataTransactionPhase::ReadyToCommit
    }

    /// Verify the source preimage and publish the retained stage once.
    ///
    /// If the process fails after the rename and before the state write, the
    /// state remains `ready_to_commit`; a later `resume` observes the output
    /// hash and promotes that state to `committed` without running callbacks.
    pub fn commit(self) -> Result<MetadataCommitReceipt, String> {
        let Self {
            state_path,
            source,
            mut document,
            staged,
            _state_lock,
        } = self;
        reject_multiply_linked(&source, "metadata transaction source")?;
        let source_evidence = capture_file_evidence(
            &source,
            document.max_source_bytes,
            "metadata transaction source before commit",
        )?;
        ensure_same_evidence(
            &document.source,
            &source_evidence,
            "source preimage before commit",
        )?;

        let stage = document
            .stage
            .as_ref()
            .ok_or("ready metadata transaction has no stage evidence")?;
        let stage_path = resolve_stage_path(&source, &stage.path)?;
        reject_multiply_linked(&stage_path, "metadata transaction stage")?;
        let observed_stage = capture_file_evidence(
            &stage_path,
            document.max_source_bytes,
            "metadata transaction stage before commit",
        )?;
        if observed_stage.identity != stage.identity
            || observed_stage.byte_len != stage.byte_len
            || observed_stage.sha256 != stage.sha256
        {
            return Err(format!(
                "metadata transaction stage changed before commit: {}",
                stage_path.display()
            ));
        }
        maybe_fail(FailurePoint::BeforePublish)?;
        staged.commit_with_destination_check(|path| {
            reject_multiply_linked(
                path,
                "metadata transaction source immediately before publish",
            )
        })?;
        maybe_fail(FailurePoint::AfterPublishBeforeState)?;

        let output = capture_file_evidence(
            &source,
            document.max_source_bytes,
            "metadata transaction committed output",
        )?;
        if output.sha256 != stage.sha256 || output.byte_len != stage.byte_len {
            return Err(format!(
                "metadata transaction output differs from staged bytes: {}",
                source.display()
            ));
        }
        document.phase = MetadataTransactionPhase::Committed;
        document.output = Some(output);
        document.stage = None;
        document.revision = document
            .revision
            .checked_add(1)
            .ok_or("metadata transaction state revision overflow")?;
        maybe_fail(FailurePoint::BeforeCommittedState)?;
        save_document(&state_path, &document)?;
        receipt_from_document(&document, &source)
    }
}

fn validate_request(request: &MetadataTransactionRequest) -> Result<(), String> {
    if request.operation.is_null() {
        return Err("metadata transaction operation must not be null".into());
    }
    validate_operation_shape(&request.operation)?;
    let operation_bytes = serde_json::to_vec(&request.operation)
        .map_err(|error| format!("encode metadata transaction operation: {error}"))?;
    if operation_bytes.len() > MAX_OPERATION_BYTES {
        return Err(format!(
            "metadata transaction operation exceeds {MAX_OPERATION_BYTES} bytes"
        ));
    }
    if request.source.as_os_str().is_empty() || request.state.as_os_str().is_empty() {
        return Err("metadata transaction source and state paths must not be empty".into());
    }
    if request.max_source_bytes == 0 {
        return Err("metadata transaction source byte limit must be greater than zero".into());
    }
    for (name, revision) in [
        ("registry", request.registry_revision.as_str()),
        ("writer", request.writer_revision.as_str()),
    ] {
        if revision.trim().is_empty()
            || revision.len() > 256
            || revision.chars().any(char::is_control)
        {
            return Err(format!(
                "metadata transaction {name} revision must contain 1..=256 non-control bytes"
            ));
        }
    }
    Ok(())
}

fn canonical_source(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect metadata source {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "metadata source must not be a symbolic link: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "metadata source is not a regular file: {}",
            path.display()
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("canonicalize metadata source {}: {error}", path.display()))?;
    let canonical_metadata = fs::symlink_metadata(&canonical).map_err(|error| {
        format!(
            "inspect canonical metadata source {}: {error}",
            canonical.display()
        )
    })?;
    if canonical_metadata.file_type().is_symlink() || !canonical_metadata.is_file() {
        return Err(format!(
            "metadata source resolved to a non-regular file: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| {
        format!(
            "resolve metadata transaction path {}: {error}",
            path.display()
        )
    })
}

fn reject_state_alias(source: &Path, state: &Path) -> Result<(), String> {
    if source == state {
        return Err("metadata transaction state path must differ from its source".into());
    }
    if let Ok(metadata) = fs::symlink_metadata(state) {
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "metadata transaction state must not be a symbolic link: {}",
                state.display()
            ));
        }
        if !metadata.is_file() {
            return Err(format!(
                "metadata transaction state is not a regular file: {}",
                state.display()
            ));
        }
        if crate::stable_input::paths_alias_if_existing(source, state)
            .map_err(|error| format!("compare metadata source and state paths: {error}"))?
        {
            return Err("metadata transaction state aliases its source".into());
        }
        reject_multiply_linked(state, "metadata transaction state")?;
    }
    Ok(())
}

fn validate_document(
    document: &JobDocument,
    source: &Path,
    request: &MetadataTransactionRequest,
) -> Result<(), String> {
    if document.schema != METADATA_JOB_SCHEMA_V1 {
        return Err(format!(
            "unsupported metadata transaction state schema {}",
            document.schema
        ));
    }
    if !valid_generator(&document.generator) {
        return Err("metadata transaction state contains an invalid generator".into());
    }
    if document.source.path != path_text(source)? || document.destination.path != path_text(source)?
    {
        return Err("metadata transaction source path does not match the state".into());
    }
    if document.operation != canonical_json(&request.operation)
        || document.policy != request.policy
        || document.registry_revision != request.registry_revision
        || document.writer_revision != request.writer_revision
        || document.max_source_bytes != request.max_source_bytes
    {
        return Err("metadata transaction request does not match the existing state".into());
    }
    if !is_sha256(&document.source.sha256)
        || !is_sha256(&document.destination.sha256)
        || document.source.byte_len != document.destination.byte_len
        || document.source.sha256 != document.destination.sha256
        || document.source.identity != document.destination.identity
    {
        return Err("metadata transaction state contains invalid source evidence".into());
    }
    let expected_fingerprint = semantic_fingerprint(
        &document.source,
        &document.operation,
        document.policy,
        &document.registry_revision,
        &document.writer_revision,
        document.max_source_bytes,
    )?;
    if document.semantic_fingerprint != expected_fingerprint
        || document.job_id != expected_fingerprint
    {
        return Err("metadata transaction semantic fingerprint is invalid".into());
    }
    parse_identity(&document.source.identity)?;
    match document.phase {
        MetadataTransactionPhase::Prepared => {
            if document.stage.is_some()
                || document.output.is_some()
                || document.mutation.is_some()
                || document.verification.is_some()
            {
                return Err("prepared metadata transaction contains publication evidence".into());
            }
        }
        MetadataTransactionPhase::ReadyToCommit => {
            let stage = document
                .stage
                .as_ref()
                .ok_or("ready metadata transaction has no stage evidence")?;
            validate_stage_evidence(stage, source, document.max_source_bytes)?;
            if document.output.is_some() {
                return Err("ready metadata transaction already contains output evidence".into());
            }
            require_evidence(&document.mutation, "ready metadata transaction mutation")?;
            require_evidence(
                &document.verification,
                "ready metadata transaction verification",
            )?;
            if !is_sha256(&stage.sha256) || stage.byte_len == 0 {
                return Err("ready metadata transaction contains invalid stage evidence".into());
            }
        }
        MetadataTransactionPhase::Committed => {
            let output = document
                .output
                .as_ref()
                .ok_or("committed metadata transaction has no output evidence")?;
            if output.path != path_text(source)?
                || !is_sha256(&output.sha256)
                || output.byte_len == 0
            {
                return Err(
                    "committed metadata transaction contains invalid output evidence".into(),
                );
            }
            parse_identity(&output.identity)?;
            if document.stage.is_some() {
                return Err("committed metadata transaction retains a stage".into());
            }
            require_evidence(
                &document.mutation,
                "committed metadata transaction mutation",
            )?;
            require_evidence(
                &document.verification,
                "committed metadata transaction verification",
            )?;
        }
    }
    if document.revision == 0 {
        return Err("metadata transaction state revision must be non-zero".into());
    }
    Ok(())
}

fn validate_stage_evidence(
    stage: &StageEvidence,
    source: &Path,
    max_bytes: u64,
) -> Result<(), String> {
    let path = resolve_stage_path(source, &stage.path)?;
    if stage.path.is_empty() || !stage.path.starts_with(STAGE_PREFIX) {
        return Err("metadata transaction stage path has an invalid prefix".into());
    }
    if stage.byte_len > max_bytes || !is_sha256(&stage.sha256) {
        return Err("metadata transaction stage exceeds its bound or has an invalid hash".into());
    }
    parse_identity(&stage.identity)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        // A missing stage is a recoverable interruption. `resume` will move
        // the job back to Prepared so the caller can create a fresh stage.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "inspect metadata transaction stage {}: {error}",
                path.display()
            ))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "metadata transaction stage is not a regular file: {}",
            path.display()
        ));
    }
    reject_multiply_linked(&path, "metadata transaction stage")?;
    Ok(())
}

fn require_evidence(evidence: &Option<Value>, description: &str) -> Result<(), String> {
    match evidence {
        Some(value) if !value.is_null() => Ok(()),
        Some(_) => Err(format!("{description} must not be null")),
        None => Err(format!("{description} is missing")),
    }
}

fn resolve_stage_path(source: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().count() != 1
        || !matches!(
            relative_path.components().next(),
            Some(Component::Normal(_))
        )
        || !relative.starts_with(STAGE_PREFIX)
        || relative.len() == STAGE_PREFIX.len()
    {
        return Err(format!(
            "metadata transaction stage path must be one .forge- filename: {relative}"
        ));
    }
    let parent = source
        .parent()
        .ok_or("metadata source has no parent directory")?;
    Ok(parent.join(relative_path))
}

fn capture_stable_input(source: &Path, max_bytes: u64) -> Result<StableInput, String> {
    let options = StableInputOptions::new(max_bytes)
        .map_err(|error| format!("configure metadata source snapshot: {error}"))?;
    StableInput::from_path(source, &options).map_err(|error| {
        format!(
            "capture metadata source snapshot {}: {error}",
            source.display()
        )
    })
}

fn stable_evidence(stable: &StableInput, source: &Path) -> Result<FileEvidence, String> {
    reject_reparse_point_path(source, "metadata transaction stable source")?;
    let identity = stable
        .source_identity()
        .ok_or("metadata source snapshot has no live source identity")?;
    Ok(FileEvidence {
        path: path_text(source)?,
        identity: identity_token(identity),
        byte_len: stable.byte_len(),
        sha256: stable.binding().sha256_hex(),
    })
}

fn verify_stable_matches_document(
    stable: &StableInput,
    source: &Path,
    expected: &FileEvidence,
) -> Result<(), String> {
    let actual = stable_evidence(stable, source)?;
    ensure_same_evidence(expected, &actual, "metadata source snapshot")
}

fn capture_file_evidence(
    path: &Path,
    max_bytes: u64,
    description: &str,
) -> Result<FileEvidence, String> {
    let (identity, byte_len, sha256) = hash_regular_file(path, max_bytes, description)?;
    Ok(FileEvidence {
        path: path_text(path)?,
        identity: identity_token(&identity),
        byte_len,
        sha256: hex_digest(&sha256),
    })
}

fn hash_regular_file(
    path: &Path,
    max_bytes: u64,
    description: &str,
) -> Result<(StableFileIdentity, u64, [u8; 32]), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);
    let mut file = options
        .open(path)
        .map_err(|error| format!("open {description} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
    reject_reparse_point_metadata(&metadata, path, description)?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{description} {} exceeds the {max_bytes}-byte limit",
            path.display()
        ));
    }
    let identity = identity_from_open_file(&file, path)
        .map_err(|error| format!("identify {description} {}: {error}", path.display()))?;
    let before_len = metadata.len();
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {description} {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| format!("{description} byte length overflow: {}", path.display()))?;
        if total > max_bytes {
            return Err(format!(
                "{description} {} exceeds the {max_bytes}-byte limit",
                path.display()
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let after_len = file
        .metadata()
        .map_err(|error| format!("reinspect {description} {}: {error}", path.display()))?
        .len();
    if before_len != after_len || before_len != total {
        return Err(format!(
            "{description} changed while it was hashed: {}",
            path.display()
        ));
    }
    let mut confirmation_options = OpenOptions::new();
    confirmation_options.read(true);
    #[cfg(unix)]
    confirmation_options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    confirmation_options.custom_flags(0x0020_0000);
    let confirmation = confirmation_options
        .open(path)
        .map_err(|error| format!("reopen {description} {}: {error}", path.display()))?;
    let confirmation_identity = identity_from_open_file(&confirmation, path)
        .map_err(|error| format!("reidentify {description} {}: {error}", path.display()))?;
    if confirmation_identity != identity {
        return Err(format!(
            "{description} changed while it was hashed: {}",
            path.display()
        ));
    }
    Ok((identity, total, hasher.finalize().into()))
}

fn copy_snapshot_to_stage(
    stable: &StableInput,
    staged: &mut AtomicOutput,
    max_bytes: u64,
) -> Result<(), String> {
    let mut input = File::open(stable.stable_path())
        .map_err(|error| format!("open metadata source snapshot: {error}"))?;
    let mut total = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| format!("read metadata source snapshot: {error}"))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or("metadata source snapshot length overflow")?;
        if total > max_bytes {
            return Err("metadata source snapshot exceeds its byte limit".into());
        }
        staged
            .file_mut()
            .write_all(&buffer[..count])
            .map_err(|error| format!("write metadata transaction stage: {error}"))?;
    }
    if total != stable.byte_len() {
        return Err("metadata source snapshot changed while staging".into());
    }
    staged
        .file_mut()
        .flush()
        .map_err(|error| format!("flush metadata transaction stage: {error}"))
}

#[derive(Clone, Debug)]
struct AtomicExpected {
    identity: StableFileIdentity,
    byte_len: u64,
    sha256: String,
}

fn capture_atomic_expected(
    staged: &AtomicOutput,
    max_bytes: u64,
) -> Result<AtomicExpected, String> {
    let evidence = capture_file_evidence(
        staged.destination_path(),
        max_bytes,
        "metadata transaction destination",
    )?;
    Ok(AtomicExpected {
        identity: parse_identity(&evidence.identity)?,
        byte_len: evidence.byte_len,
        sha256: evidence.sha256,
    })
}

fn save_document(path: &Path, document: &JobDocument) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| format!("encode metadata transaction state: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(format!(
            "metadata transaction state exceeds the {MAX_STATE_BYTES}-byte limit"
        ));
    }
    let mut output = AtomicOutput::new_with_overwrite_and_limit(path, true, MAX_STATE_BYTES)?;
    output.write_all(&bytes)?;
    output.commit()
}

#[cfg(unix)]
fn sync_stage_parent_directory(source: &Path) -> Result<(), String> {
    let parent = source
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            format!(
                "sync metadata transaction stage directory {}: {error}",
                parent.display()
            )
        })
}

#[cfg(not(unix))]
fn sync_stage_parent_directory(_source: &Path) -> Result<(), String> {
    // Windows relies on the durable file handle and publication semantics in
    // AtomicOutput; directory handles are not portably fsync-able there.
    Ok(())
}

fn ensure_same_evidence(
    expected: &FileEvidence,
    actual: &FileEvidence,
    context: &str,
) -> Result<(), String> {
    if expected.path != actual.path
        || expected.identity != actual.identity
        || expected.byte_len != actual.byte_len
        || expected.sha256 != actual.sha256
    {
        return Err(format!("{context} changed"));
    }
    Ok(())
}

fn receipt_from_document(
    document: &JobDocument,
    source: &Path,
) -> Result<MetadataCommitReceipt, String> {
    let output = document
        .output
        .as_ref()
        .ok_or("metadata transaction has no output receipt")?;
    Ok(MetadataCommitReceipt {
        job_id: document.job_id.clone(),
        source: source.to_owned(),
        source_sha256: document.source.sha256.clone(),
        output_sha256: output.sha256.clone(),
        mutation: document.mutation.clone(),
        verification: document.verification.clone(),
    })
}

fn semantic_fingerprint(
    source: &FileEvidence,
    operation: &Value,
    policy: MetadataPolicy,
    registry_revision: &str,
    writer_revision: &str,
    max_source_bytes: u64,
) -> Result<String, String> {
    let payload = json!({
        "source": {
            "path": source.path,
            "byte_len": source.byte_len,
            "sha256": source.sha256,
        },
        "operation": canonical_json(operation),
        "policy": policy,
        "registry_revision": registry_revision,
        "writer_revision": writer_revision,
        "max_source_bytes": max_source_bytes,
    });
    let bytes = serde_json::to_vec(&canonical_json(&payload))
        .map_err(|error| format!("encode metadata transaction fingerprint: {error}"))?;
    Ok(hex_digest(&Sha256::digest(bytes)))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut sorted = Map::new();
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&values[key]));
            }
            Value::Object(sorted)
        }
        scalar => scalar.clone(),
    }
}

fn validate_operation_shape(value: &Value) -> Result<(), String> {
    let mut stack = vec![(value, 1_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_OPERATION_DEPTH {
            return Err(format!(
                "metadata transaction operation exceeds nesting depth {MAX_OPERATION_DEPTH}"
            ));
        }
        nodes = nodes
            .checked_add(1)
            .ok_or("metadata transaction operation node count overflow")?;
        if nodes > MAX_OPERATION_NODES {
            return Err(format!(
                "metadata transaction operation exceeds {MAX_OPERATION_NODES} JSON values"
            ));
        }
        match value {
            Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                stack.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn valid_generator(generator: &str) -> bool {
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

fn path_text(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "metadata transaction paths must be UTF-8: {}",
            path.display()
        )
    })
}

fn identity_token(identity: &StableFileIdentity) -> String {
    match identity {
        #[cfg(unix)]
        StableFileIdentity::Unix { device, inode } => format!("unix:{device}:{inode}"),
        #[cfg(windows)]
        StableFileIdentity::Windows { volume, index } => format!("windows:{volume}:{index}"),
        #[cfg(not(any(unix, windows)))]
        StableFileIdentity::Canonical(path) => format!("canonical:{}", path.display()),
    }
}

fn parse_identity(value: &str) -> Result<StableFileIdentity, String> {
    #[cfg(unix)]
    {
        let mut parts = value.split(':');
        if parts.next() != Some("unix") {
            return Err(format!(
                "invalid Unix metadata transaction identity: {value}"
            ));
        }
        let device = parts
            .next()
            .ok_or_else(|| format!("invalid Unix metadata transaction identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Unix metadata transaction identity: {value}"))?;
        let inode = parts
            .next()
            .ok_or_else(|| format!("invalid Unix metadata transaction identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Unix metadata transaction identity: {value}"))?;
        if parts.next().is_some() {
            return Err(format!(
                "invalid Unix metadata transaction identity: {value}"
            ));
        }
        Ok(StableFileIdentity::Unix { device, inode })
    }
    #[cfg(windows)]
    {
        let mut parts = value.split(':');
        if parts.next() != Some("windows") {
            return Err(format!(
                "invalid Windows metadata transaction identity: {value}"
            ));
        }
        let volume = parts
            .next()
            .ok_or_else(|| format!("invalid Windows metadata transaction identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Windows metadata transaction identity: {value}"))?;
        let index = parts
            .next()
            .ok_or_else(|| format!("invalid Windows metadata transaction identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Windows metadata transaction identity: {value}"))?;
        if parts.next().is_some() {
            return Err(format!(
                "invalid Windows metadata transaction identity: {value}"
            ));
        }
        Ok(StableFileIdentity::Windows { volume, index })
    }
    #[cfg(not(any(unix, windows)))]
    {
        value
            .strip_prefix("canonical:")
            .map(|path| StableFileIdentity::Canonical(PathBuf::from(path)))
            .ok_or_else(|| format!("invalid metadata transaction identity: {value}"))
    }
}

fn parse_sha256(value: &str) -> Result<[u8; 32], String> {
    if !is_sha256(value) {
        return Err(format!("invalid metadata transaction SHA-256: {value}"));
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(bytes)
}

fn hex_nibble(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("invalid hexadecimal digit".into()),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(windows)]
fn reject_reparse_point_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!(
            "{description} is a reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn reject_reparse_point_metadata(
    _metadata: &fs::Metadata,
    _path: &Path,
    _description: &str,
) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn reject_reparse_point_path(path: &Path, description: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
    reject_reparse_point_metadata(&metadata, path, description)
}

#[cfg(not(windows))]
fn reject_reparse_point_path(_path: &Path, _description: &str) -> Result<(), String> {
    Ok(())
}

fn reject_multiply_linked(path: &Path, description: &str) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
    #[cfg(unix)]
    if metadata.nlink() > 1 {
        return Err(format!(
            "{description} must not have hard-link aliases: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    if metadata.number_of_links().is_some_and(|links| links > 1) {
        return Err(format!(
            "{description} must not have hard-link aliases: {}",
            path.display()
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailurePoint {
    BeforeReadyState,
    AfterReadyState,
    BeforePublish,
    AfterPublishBeforeState,
    BeforeCommittedState,
}

#[cfg(test)]
fn maybe_fail(point: FailurePoint) -> Result<(), String> {
    FAILURE_POINT.with(|failure| {
        if failure.get() == Some(point) {
            failure.set(None);
            Err(format!(
                "injected metadata transaction failure at {point:?}"
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(not(test))]
fn maybe_fail(_point: FailurePoint) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
fn inject_failure(point: FailurePoint) {
    FAILURE_POINT.with(|failure| failure.set(Some(point)));
}

#[cfg(test)]
thread_local! {
    static FAILURE_POINT: std::cell::Cell<Option<FailurePoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn request(directory: &Path) -> MetadataTransactionRequest {
        MetadataTransactionRequest::new(
            directory.join("library.wav"),
            directory.join("library.metadata-job.json"),
            json!({"fields": ["replaygain_track_gain"], "value": "-2.00 dB"}),
        )
    }

    fn stage_transaction(
        directory: &Path,
    ) -> (MetadataTransactionRequest, ReadyMetadataCommit, Vec<u8>) {
        let request = request(directory);
        let original = b"source bytes".to_vec();
        fs::write(request.source(), &original).unwrap();
        let transaction = MetadataTransaction::prepare(request.clone()).unwrap();
        let ready = transaction
            .stage(
                |stage| {
                    fs::write(stage, b"source bytes with metadata").unwrap();
                    Ok(json!({"changed": true}))
                },
                |stage| {
                    assert_eq!(fs::read(stage).unwrap(), b"source bytes with metadata");
                    Ok(json!({"round_trip": true}))
                },
            )
            .unwrap();
        (request, ready, original)
    }

    #[test]
    fn stages_only_and_commits_one_complete_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, original) = stage_transaction(directory.path());
        assert_eq!(fs::read(request.source()).unwrap(), original);
        assert_eq!(ready.phase(), MetadataTransactionPhase::ReadyToCommit);
        let stage_path = ready.staged_path().to_owned();
        let receipt = ready.commit().unwrap();
        assert_eq!(
            fs::read(request.source()).unwrap(),
            b"source bytes with metadata"
        );
        assert!(!stage_path.exists());
        assert_ne!(receipt.source_sha256(), receipt.output_sha256());
        let resumed = MetadataTransaction::resume(request).unwrap();
        assert!(matches!(resumed, MetadataResume::Committed(_)));
    }

    #[test]
    fn no_op_ready_stage_is_not_mistaken_for_a_published_output() {
        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source bytes").unwrap();
        let transaction = MetadataTransaction::prepare(request.clone()).unwrap();
        let ready = transaction
            .stage(
                |stage| {
                    fs::write(stage, b"source bytes").unwrap();
                    Ok(json!({"changed": false}))
                },
                |_| Ok(json!({"round_trip": true})),
            )
            .unwrap();
        drop(ready);

        let resumed = MetadataTransaction::resume(request.clone()).unwrap();
        let MetadataResume::Ready(ready) = resumed else {
            panic!("an uncommitted no-op stage was treated as committed")
        };
        ready.commit().unwrap();
        assert!(matches!(
            MetadataTransaction::resume(request).unwrap(),
            MetadataResume::Committed(_)
        ));
    }

    #[test]
    fn path_replacement_by_writer_is_adopted() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, _) = stage_transaction(directory.path());
        let expected = fs::read(ready.staged_path()).unwrap();
        assert_eq!(expected, b"source bytes with metadata");
        drop(ready);
        let resumed = MetadataTransaction::resume(request).unwrap();
        let MetadataResume::Ready(ready) = resumed else {
            panic!("expected ready stage")
        };
        assert_eq!(fs::read(ready.staged_path()).unwrap(), expected);
    }

    #[test]
    fn crash_after_publish_converges_on_resume() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, _) = stage_transaction(directory.path());
        inject_failure(FailurePoint::AfterPublishBeforeState);
        assert!(ready.commit().is_err());
        assert_eq!(
            fs::read(request.source()).unwrap(),
            b"source bytes with metadata"
        );
        let resumed = MetadataTransaction::resume(request).unwrap();
        let MetadataResume::Committed(receipt) = resumed else {
            panic!("published output was not recovered")
        };
        assert_eq!(receipt.output_sha256().len(), 64);
    }

    #[test]
    fn state_failure_after_publish_converges_on_resume() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, _) = stage_transaction(directory.path());
        inject_failure(FailurePoint::BeforeCommittedState);
        assert!(ready.commit().is_err());
        let MetadataResume::Committed(receipt) = MetadataTransaction::resume(request).unwrap()
        else {
            panic!("published output was not recovered after state failure")
        };
        assert_eq!(receipt.output_sha256().len(), 64);
    }

    #[test]
    fn missing_stage_is_requeued_without_touching_source() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, original) = stage_transaction(directory.path());
        let stage_path = ready.staged_path().to_owned();
        drop(ready);
        fs::remove_file(stage_path).unwrap();
        let resumed = MetadataTransaction::resume(request).unwrap();
        let MetadataResume::Prepared(transaction) = resumed else {
            panic!("missing stage was not requeued")
        };
        assert_eq!(transaction.phase(), MetadataTransactionPhase::Prepared);
        assert_eq!(fs::read(transaction.source()).unwrap(), original);
    }

    #[test]
    fn changed_stage_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, original) = stage_transaction(directory.path());
        fs::write(ready.staged_path(), b"tampered").unwrap();
        drop(ready);
        let error = MetadataTransaction::resume(request).unwrap_err();
        assert!(error.contains("stage"), "{error}");
        assert_eq!(
            fs::read(directory.path().join("library.wav")).unwrap(),
            original
        );
    }

    #[test]
    fn source_conflict_never_publishes_stage() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, _) = stage_transaction(directory.path());
        fs::write(request.source(), b"external change").unwrap();
        assert!(ready.commit().is_err());
        assert_eq!(fs::read(request.source()).unwrap(), b"external change");
    }

    #[test]
    fn state_fingerprint_rejects_operation_change() {
        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source").unwrap();
        MetadataTransaction::prepare(request.clone()).unwrap();
        let changed = MetadataTransactionRequest::new(
            request.source(),
            request.state(),
            json!({"fields": ["different"]}),
        );
        let error = MetadataTransaction::prepare(changed).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn request_revisions_match_the_published_state_contract() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("library.wav");
        let state = directory.path().join("library.metadata-job.json");
        fs::write(&source, b"source").unwrap();
        for request in [
            MetadataTransactionRequest::new(&source, &state, json!({}))
                .with_registry_revision("x".repeat(257)),
            MetadataTransactionRequest::new(&source, &state, json!({}))
                .with_writer_revision("writer\nrevision"),
        ] {
            let error = MetadataTransaction::prepare(request).unwrap_err();
            assert!(error.contains("revision must contain"), "{error}");
        }
        assert!(!state.exists());
    }

    #[test]
    fn operation_shape_is_bounded_before_recursive_canonicalization() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("library.wav");
        let state = directory.path().join("library.metadata-job.json");
        fs::write(&source, b"source").unwrap();

        let mut deeply_nested = json!(0);
        for _ in 0..MAX_OPERATION_DEPTH {
            deeply_nested = Value::Array(vec![deeply_nested]);
        }
        let error = MetadataTransaction::prepare(MetadataTransactionRequest::new(
            &source,
            &state,
            deeply_nested,
        ))
        .unwrap_err();
        assert!(error.contains("nesting depth"), "{error}");
        assert!(!state.exists());

        let too_many = Value::Array(vec![Value::Bool(false); MAX_OPERATION_NODES]);
        let error = MetadataTransaction::prepare(MetadataTransactionRequest::new(
            &source, &state, too_many,
        ))
        .unwrap_err();
        assert!(error.contains("JSON values"), "{error}");
        assert!(!state.exists());
    }

    #[test]
    fn state_generator_and_stage_name_match_the_schema_boundary() {
        assert!(valid_generator("forge-normalizer/0.189.14"));
        assert!(!valid_generator("forge-normalizer/not-a-version"));
        assert!(!valid_generator("forge-normalizer/0.189.14-extra"));
        assert!(!valid_generator("other/0.189.14"));

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("library.wav");
        fs::write(&source, b"source").unwrap();
        assert!(resolve_stage_path(&source, ".forge-a").is_ok());
        assert!(resolve_stage_path(&source, ".forge-").is_err());
    }

    #[test]
    fn state_corruption_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source").unwrap();
        MetadataTransaction::prepare(request.clone()).unwrap();
        fs::write(request.state(), b"not json").unwrap();
        let error = MetadataTransaction::prepare(request).unwrap_err();
        assert!(error.contains("decode"), "{error}");
    }

    #[test]
    fn failure_before_ready_leaves_source_unchanged_and_state_prepared() {
        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source").unwrap();
        let transaction = MetadataTransaction::prepare(request.clone()).unwrap();
        inject_failure(FailurePoint::BeforeReadyState);
        assert!(transaction
            .stage(
                |stage| {
                    fs::write(stage, b"new").unwrap();
                    Ok(json!({}))
                },
                |_| Ok(json!({})),
            )
            .is_err());
        assert_eq!(fs::read(request.source()).unwrap(), b"source");
        assert_eq!(
            MetadataTransaction::prepare(request).unwrap().phase(),
            MetadataTransactionPhase::Prepared
        );
    }

    #[test]
    fn failure_after_ready_leaves_a_resumable_stage() {
        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source").unwrap();
        let transaction = MetadataTransaction::prepare(request.clone()).unwrap();
        inject_failure(FailurePoint::AfterReadyState);
        assert!(transaction
            .stage(
                |stage| {
                    fs::write(stage, b"new").unwrap();
                    Ok(json!({}))
                },
                |_| Ok(json!({})),
            )
            .is_err());
        let resumed = MetadataTransaction::resume(request).unwrap();
        assert!(matches!(resumed, MetadataResume::Ready(_)));
    }

    #[test]
    fn phase_evidence_is_required_and_forbidden() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, _) = stage_transaction(directory.path());
        drop(ready);

        let mut document =
            serde_json::from_slice::<JobDocument>(&fs::read(request.state()).unwrap()).unwrap();
        let ready_stage = document.stage.clone();

        document.phase = MetadataTransactionPhase::Prepared;
        save_document(request.state(), &document).unwrap();
        let error = MetadataTransaction::prepare(request.clone()).unwrap_err();
        assert!(error.contains("publication evidence"), "{error}");

        document.phase = MetadataTransactionPhase::ReadyToCommit;
        document.stage = None;
        save_document(request.state(), &document).unwrap();
        let error = MetadataTransaction::prepare(request.clone()).unwrap_err();
        assert!(error.contains("no stage evidence"), "{error}");

        document.stage = ready_stage.clone();
        document.mutation = Some(Value::Null);
        document.verification = Some(json!({"round_trip": true}));
        let source = canonical_source(request.source()).unwrap();
        let error = validate_document(&document, &source, &request).unwrap_err();
        assert!(error.contains("mutation must not be null"), "{error}");

        document.mutation = Some(json!({"changed": true}));
        document.verification = Some(Value::Null);
        let error = validate_document(&document, &source, &request).unwrap_err();
        assert!(error.contains("verification must not be null"), "{error}");

        document.verification = Some(json!({"round_trip": true}));
        document.mutation = Some(json!({"changed": true}));
        document.output = Some(document.source.clone());
        save_document(request.state(), &document).unwrap();
        let error = MetadataTransaction::prepare(request.clone()).unwrap_err();
        assert!(error.contains("output evidence"), "{error}");

        document.output = None;
        document.mutation = None;
        save_document(request.state(), &document).unwrap();
        let error = MetadataTransaction::prepare(request.clone()).unwrap_err();
        assert!(error.contains("mutation is missing"), "{error}");

        document.phase = MetadataTransactionPhase::Committed;
        document.stage = None;
        document.output = None;
        document.mutation = Some(json!({"changed": true}));
        document.verification = Some(json!({"round_trip": true}));
        save_document(request.state(), &document).unwrap();
        let error = MetadataTransaction::prepare(request.clone()).unwrap_err();
        assert!(error.contains("no output evidence"), "{error}");

        document.output = Some(document.source.clone());
        document.mutation = Some(Value::Null);
        let error = validate_document(&document, &source, &request).unwrap_err();
        assert!(error.contains("mutation must not be null"), "{error}");

        document.mutation = Some(json!({"changed": true}));
        document.verification = Some(Value::Null);
        let error = validate_document(&document, &source, &request).unwrap_err();
        assert!(error.contains("verification must not be null"), "{error}");

        document.verification = Some(json!({"round_trip": true}));
        document.stage = ready_stage;
        save_document(request.state(), &document).unwrap();
        let error = MetadataTransaction::prepare(request).unwrap_err();
        assert!(error.contains("retains a stage"), "{error}");
    }

    #[test]
    fn json_null_metadata_evidence_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source").unwrap();
        let transaction = MetadataTransaction::prepare(request.clone()).unwrap();
        let error = transaction
            .stage(
                |stage| {
                    fs::write(stage, b"updated").unwrap();
                    Ok(Value::Null)
                },
                |_| Ok(json!({"round_trip": true})),
            )
            .unwrap_err();
        assert!(error.contains("mutation evidence"), "{error}");

        let transaction = MetadataTransaction::prepare(request).unwrap();
        let error = transaction
            .stage(
                |stage| {
                    fs::write(stage, b"updated").unwrap();
                    Ok(json!({"changed": true}))
                },
                |_| Ok(Value::Null),
            )
            .unwrap_err();
        assert!(error.contains("verification evidence"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn source_permissions_are_preserved_on_metadata_commit() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let request = request(directory.path());
        fs::write(request.source(), b"source").unwrap();
        fs::set_permissions(request.source(), fs::Permissions::from_mode(0o640)).unwrap();
        let transaction = MetadataTransaction::prepare(request.clone()).unwrap();
        let ready = transaction
            .stage(
                |stage| {
                    fs::write(stage, b"updated").unwrap();
                    Ok(json!({"changed": true}))
                },
                |_| Ok(json!({"round_trip": true})),
            )
            .unwrap();
        assert_eq!(
            fs::metadata(ready.staged_path()).unwrap().mode() & 0o777,
            0o640
        );
        ready.commit().unwrap();
        assert_eq!(
            fs::metadata(request.source()).unwrap().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_aliases_are_rejected_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let (request, ready, original) = stage_transaction(directory.path());
        let alias = directory.path().join("stage-alias");
        fs::hard_link(ready.staged_path(), &alias).unwrap();
        drop(ready);
        let error = MetadataTransaction::resume(request).unwrap_err();
        assert!(error.contains("hard-link"), "{error}");
        assert_eq!(
            fs::read(directory.path().join("library.wav")).unwrap(),
            original
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_symlink_and_hardlink_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("library.wav");
        fs::write(&source, b"source").unwrap();
        let symlink = directory.path().join("link.wav");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        let symlink_request = MetadataTransactionRequest::new(
            symlink,
            directory.path().join("symlink-state.json"),
            json!({"x": 1}),
        );
        assert!(MetadataTransaction::prepare(symlink_request).is_err());

        let alias = directory.path().join("alias.wav");
        fs::hard_link(&source, &alias).unwrap();
        let hardlink_request = MetadataTransactionRequest::new(
            source,
            directory.path().join("hardlink-state.json"),
            json!({"x": 1}),
        );
        assert!(MetadataTransaction::prepare(hardlink_request).is_err());
    }
}
