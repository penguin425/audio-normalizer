//! Crash-recoverable publication of a completely prepared set of outputs.
//!
//! The crate's internal `AtomicOutput` gives one pathname a strong
//! content/identity compare and
//! replace boundary.  A filesystem does not provide the same boundary for a
//! set of pathnames, so this module adds the missing coordination layer:
//! every member is staged first, a bounded JSON journal records the complete
//! evidence, existing destinations are moved to private siblings, and the
//! ordered publication is recoverable by inspecting those siblings.
//!
//! The journal is a recovery record, not an authenticated capability.  Its
//! state path and every destination parent must be protected by the caller's
//! trust boundary.  Portable filesystems can expose a prefix of a generation
//! while renames are in progress; the guarantee here is that a restart either
//! recognizes a complete generation or safely restores the previous one.

use crate::atomic::{AtomicOutput, DestinationPreimage};
use crate::output_plan::{physical_normalized_path, physical_route_key};
use crate::stable_input::{identity_from_open_file, StableFileIdentity};
use crate::state_lock::{
    read_regular_state_file, sibling_lock_path, StateFileLock, StateFileLockProbe,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

/// Persistent state schema for a multi-output generation publication.
pub const GENERATION_JOB_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/generation-job-v1";

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MEMBERS: usize = 100_000;
const MAX_GENERATOR_BYTES: usize = 256;
const HASH_BUFFER_BYTES: usize = 128 * 1024;
const STAGE_PREFIX: &str = ".forge-";
const BACKUP_PREFIX: &str = ".forge-generation-";

/// Durable lifecycle of one generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPhase {
    Ready,
    Publishing,
    Committed,
    RolledBack,
}

impl GenerationPhase {
    /// Stable wire spelling used by the journal and recovery CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Publishing => "publishing",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
        }
    }
}

impl fmt::Display for GenerationPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Durable lifecycle of one member's publication operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemberPublicationStep {
    Prepared,
    BackedUp,
    Published,
    Restored,
    Abandoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DestinationPreimageRecord {
    Missing,
    Present {
        identity: String,
        byte_len: u64,
        sha256: String,
    },
}

/// Identity, size, and complete digest of one regular file at a pathname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEvidence {
    path: String,
    identity: String,
    byte_len: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationMemberRecord {
    destination: String,
    destination_preimage: DestinationPreimageRecord,
    stage: FileEvidence,
    backup_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<FileEvidence>,
    publication: MemberPublicationStep,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationDocument {
    schema: String,
    generator: String,
    generation_id: String,
    revision: u64,
    phase: GenerationPhase,
    semantic_fingerprint: String,
    member_count: usize,
    members: Vec<GenerationMemberRecord>,
}

/// A prepared output handed to a generation transaction.
///
/// The contained internal `AtomicOutput` owns the private stage until the generation
/// has durably recorded `ready`.  Once that state is durable, ownership moves
/// to [`GenerationTransaction`] and the stage is deliberately retained across
/// a process restart.
///
/// External callers obtain this value from the staged normalization APIs,
/// such as [`crate::normalize::StagedNormalization::into_generation_parts`]
/// or [`crate::normalize::StagedAlbumNormalization::into_generation_parts`].
/// It cannot be constructed from an arbitrary pathname.
pub struct PreparedGenerationOutput {
    staged: AtomicOutput,
}

impl fmt::Debug for PreparedGenerationOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedGenerationOutput")
            .field("destination", &self.staged.destination_path())
            .field("stage", &self.staged.path())
            .finish()
    }
}

impl PreparedGenerationOutput {
    /// Wrap a complete, still-unpublished atomic output.
    pub(crate) fn new(staged: AtomicOutput) -> Result<Self, String> {
        staged.sync_stage()?;
        staged.generation_verify_stage()?;
        AtomicOutput::sync_generation_parent(staged.path())?;
        Ok(Self { staged })
    }

    /// Compatibility spelling useful to callers that already name their
    /// render result an atomic output.
    pub(crate) fn from_atomic(staged: AtomicOutput) -> Result<Self, String> {
        Self::new(staged)
    }

    /// Final destination that this member will replace on generation commit.
    pub fn destination(&self) -> &Path {
        self.staged.destination_path()
    }

    /// Complete private sibling file currently owned by this prepared member.
    pub fn staged_path(&self) -> &Path {
        self.staged.path()
    }

    pub(crate) fn into_atomic(self) -> AtomicOutput {
        self.staged
    }
}

/// Small, copyable view of a generation journal's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationStatus {
    state_path: PathBuf,
    generator: String,
    generation_id: String,
    semantic_fingerprint: String,
    phase: GenerationPhase,
    member_count: usize,
    publication_steps: Vec<String>,
    outputs: Vec<GenerationOutputEvidence>,
}

/// Opaque identity of a regular file at a generation destination.
///
/// The platform-specific device/inode or volume/file-index values remain an
/// implementation detail.  Callers can retain and compare identities through
/// [`PartialEq`] (or [`Self::same_file`]) without depending on those fields.
#[derive(Clone, PartialEq, Eq)]
pub struct FileIdentity(StableFileIdentity);

impl fmt::Debug for FileIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FileIdentity(..)")
    }
}

impl FileIdentity {
    /// Compare two identities without exposing platform-specific fields.
    pub fn same_file(&self, other: &Self) -> bool {
        self == other
    }
}

/// Destination, complete staged-byte digest, and regular-file identity
/// recorded by a generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationOutputEvidence {
    destination: PathBuf,
    byte_len: u64,
    sha256: String,
    identity: FileIdentity,
}

impl GenerationOutputEvidence {
    /// Final destination associated with these staged or committed bytes.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Number of bytes in the committed destination evidence.
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// Lowercase SHA-256 digest of the complete output bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Opaque identity of the committed destination file.
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    /// Alias that makes the file-identity meaning explicit at call sites.
    pub fn file_identity(&self) -> &FileIdentity {
        self.identity()
    }

    /// Verify the live destination against this committed-generation evidence.
    ///
    /// The destination is opened without following links, hashed through the
    /// opened handle, and re-opened to detect replacement while it is being
    /// inspected.  Its regular-file identity, byte length, and complete digest
    /// must all match the journal evidence; symlinks/reparse points and other
    /// non-regular paths are rejected by the same bounded capture used during
    /// generation recovery.
    pub fn verify_live_destination(&self) -> Result<(), String> {
        let observed = capture_required(&self.destination, "generation committed output")?;
        let observed_identity = parse_identity(&observed.identity)?;
        if observed_identity != self.identity.0
            || observed.byte_len != self.byte_len
            || observed.sha256 != self.sha256
        {
            return Err(format!(
                "generation committed output differs from evidence: {}",
                self.destination.display()
            ));
        }
        reject_hardlink_alias(&self.destination, "generation committed output", &observed)?;
        Ok(())
    }
}

impl GenerationStatus {
    /// Absolute path of the durable generation journal.
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Deterministic identifier bound to the state route and semantics.
    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    /// Forge version that created the journal.
    pub fn generator(&self) -> &str {
        &self.generator
    }

    /// Caller-supplied semantic fingerprint authorized for publication.
    pub fn semantic_fingerprint(&self) -> &str {
        &self.semantic_fingerprint
    }

    /// Durable phase observed in the journal.
    pub const fn phase(&self) -> GenerationPhase {
        self.phase
    }

    /// Number of generation members.
    pub const fn member_count(&self) -> usize {
        self.member_count
    }

    /// Per-member durable publication step in canonical destination order.
    pub fn publication_steps(&self) -> &[String] {
        &self.publication_steps
    }

    /// Evidence for each member's intended output bytes.
    ///
    /// In `Ready` and `Publishing`, identity refers to the private staged file
    /// and the destination may not contain these bytes yet.  In `Committed`,
    /// the same file identity has moved to the destination and callers may use
    /// [`GenerationOutputEvidence::verify_live_destination`].  A rolled-back
    /// status describes the abandoned staged bytes, not the restored preimage.
    pub fn outputs(&self) -> &[GenerationOutputEvidence] {
        &self.outputs
    }
}

/// Advisory lock observation made without creating a lock file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationLockState {
    Missing,
    Available,
    Active,
}

impl GenerationLockState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Available => "available",
            Self::Active => "active",
        }
    }
}

/// Side-effect-free prediction for an explicitly confirmed reclaim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationReclaimAction {
    AbandonReady,
    FinalizeCommitted,
    RollBack,
    CleanCommitted,
    Nothing,
    BlockedActive,
}

impl GenerationReclaimAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AbandonReady => "would_abandon_ready",
            Self::FinalizeCommitted => "would_finalize_committed",
            Self::RollBack => "would_roll_back",
            Self::CleanCommitted => "would_clean_committed",
            Self::Nothing => "nothing_to_reclaim",
            Self::BlockedActive => "blocked_active",
        }
    }
}

/// Read-only reclaim assessment, including lock and private-file evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationReclaimInspection {
    status: GenerationStatus,
    action: GenerationReclaimAction,
    lock_state: GenerationLockState,
    private_file_count: usize,
}

impl GenerationReclaimInspection {
    pub fn status(&self) -> &GenerationStatus {
        &self.status
    }

    pub const fn action(&self) -> GenerationReclaimAction {
        self.action
    }

    pub const fn lock_state(&self) -> GenerationLockState {
        self.lock_state
    }

    pub const fn private_file_count(&self) -> usize {
        self.private_file_count
    }
}

/// Result of opening a journal after a restart.
pub enum GenerationRecovery {
    Ready(GenerationTransaction),
    Committed(GenerationStatus),
    RolledBack(GenerationStatus),
}

impl fmt::Debug for GenerationRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready(transaction) => formatter
                .debug_tuple("Ready")
                .field(&transaction.status())
                .finish(),
            Self::Committed(status) => formatter.debug_tuple("Committed").field(status).finish(),
            Self::RolledBack(status) => formatter.debug_tuple("RolledBack").field(status).finish(),
        }
    }
}

/// A generation which owns its durable state lock, retained stages, and
/// private backups for the duration of publication/recovery.
pub struct GenerationTransaction {
    state_path: PathBuf,
    document: GenerationDocument,
    outputs: Vec<Option<AtomicOutput>>,
    _state_lock: StateFileLock,
    semantic_fingerprint_verified: bool,
    #[cfg(test)]
    failure_at_publication: Option<usize>,
}

impl fmt::Debug for GenerationTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenerationTransaction")
            .field("state_path", &self.state_path)
            .field("document", &self.document)
            .field(
                "stages_owned",
                &self
                    .outputs
                    .iter()
                    .filter(|output| output.is_some())
                    .count(),
            )
            .finish_non_exhaustive()
    }
}

impl GenerationTransaction {
    /// Create and durably prepare a generation from completely rendered
    /// stages.  `state_path` is explicit so recovery does not depend on a
    /// caller reconstructing an implicit checkpoint name.
    pub fn new(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
        outputs: Vec<PreparedGenerationOutput>,
    ) -> Result<Self, String> {
        Self::prepare(state_path, semantic_fingerprint, outputs)
    }

    /// Alias emphasizing that this call writes the durable `ready` journal.
    pub fn prepare(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
        outputs: Vec<PreparedGenerationOutput>,
    ) -> Result<Self, String> {
        Self::prepare_internal(state_path, semantic_fingerprint, outputs, false)
    }

    /// Prepare a new generation while explicitly replacing a validated
    /// terminal journal at the same path.
    ///
    /// This is intended for an authorized rebuild after a prior generation
    /// was committed or rolled back.  Ready and publishing journals are never
    /// replaced; they must be resumed or recovered first.  Private paths from
    /// the terminal journal must already be absent.
    pub fn prepare_replacing_terminal(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
        outputs: Vec<PreparedGenerationOutput>,
    ) -> Result<Self, String> {
        Self::prepare_internal(state_path, semantic_fingerprint, outputs, true)
    }

    fn prepare_internal(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
        outputs: Vec<PreparedGenerationOutput>,
        replace_terminal: bool,
    ) -> Result<Self, String> {
        if outputs.is_empty() {
            return Err("a generation requires at least one prepared output".into());
        }
        if outputs.len() > MAX_MEMBERS {
            return Err(format!("generation exceeds the {MAX_MEMBERS}-member limit"));
        }
        let state_path = absolute_state_path(&state_path.into(), "generation state")?;
        let state_lock_path = sibling_lock_path(&state_path)?;
        let state_route_key = physical_route_key(&state_path)?;
        let state_lock_route_key = physical_route_key(&state_lock_path)?;
        let semantic_fingerprint = semantic_fingerprint.into();
        validate_semantic_fingerprint(&semantic_fingerprint)?;
        let generation_id = generation_id(&state_path, &semantic_fingerprint)?;
        for output in &outputs {
            let destination = absolute_normalized(output.destination(), "generation destination")?;
            let stage = absolute_normalized(output.staged_path(), "generation stage")?;
            let destination_route_key = physical_route_key(&destination)?;
            let stage_route_key = physical_route_key(&stage)?;
            if destination_route_key == state_route_key
                || destination_route_key == state_lock_route_key
                || stage_route_key == state_route_key
                || stage_route_key == state_lock_route_key
            {
                return Err(format!(
                    "generation state or lock path aliases a member path: {}",
                    destination.display()
                ));
            }
        }
        let state_lock = StateFileLock::acquire(&state_path, "generation state")?;
        if let Some(bytes) =
            read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
        {
            if !replace_terminal {
                return Err(format!(
                    "generation state already exists: {}",
                    state_path.display()
                ));
            }
            let prior: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "decode prior generation state {}: {error}",
                    state_path.display()
                )
            })?;
            validate_document(&prior, &state_path)?;
            if prior.semantic_fingerprint != semantic_fingerprint {
                return Err(
                    "replacement generation semantic fingerprint does not match terminal state"
                        .into(),
                );
            }
            if !matches!(
                prior.phase,
                GenerationPhase::Committed | GenerationPhase::RolledBack
            ) {
                return Err(format!(
                    "refuse to replace non-terminal generation in {} phase",
                    prior.phase
                ));
            }
            ensure_terminal_private_paths_absent(&prior)?;
        }

        let mut keyed_outputs = Vec::with_capacity(outputs.len());
        for output in outputs {
            let destination = absolute_normalized(output.destination(), "generation destination")?;
            keyed_outputs.push((
                physical_route_key(&destination)?,
                Some(output.into_atomic()),
            ));
        }
        // A canonical publication order prevents two overlapping generation
        // journals with different caller order from each backing up a
        // different shared destination before discovering the conflict.
        keyed_outputs.sort_by(|left, right| left.0.cmp(&right.0));
        let mut atomic_outputs = keyed_outputs
            .into_iter()
            .map(|(_, output)| output)
            .collect::<Vec<_>>();

        let mut members = Vec::with_capacity(atomic_outputs.len());
        let mut destination_keys = BTreeSet::new();
        let mut protected_keys = BTreeSet::new();
        for (index, output) in atomic_outputs.iter().enumerate() {
            let output = output.as_ref().expect("prepared output is present");
            let destination =
                absolute_normalized(output.destination_path(), "generation destination")?;
            let stage = absolute_normalized(output.path(), "generation stage")?;
            let destination_route_key = physical_route_key(&destination)?;
            let stage_route_key = physical_route_key(&stage)?;
            validate_private_stage_path(&destination, &stage)?;
            if !destination_keys.insert(destination_route_key.clone()) {
                return Err(format!(
                    "duplicate generation destination: {}",
                    destination.display()
                ));
            }
            if !protected_keys.insert(destination_route_key.clone()) {
                return Err(format!(
                    "duplicate generation protected path: {}",
                    destination.display()
                ));
            }
            if !protected_keys.insert(stage_route_key.clone()) {
                return Err(format!(
                    "generation stage collides with another protected path: {}",
                    stage.display()
                ));
            }
            if destination_route_key == state_route_key
                || destination_route_key == state_lock_route_key
                || stage_route_key == state_route_key
                || stage_route_key == state_lock_route_key
            {
                return Err(format!(
                    "generation state or lock path aliases a member path: {}",
                    destination.display(),
                ));
            }

            output.generation_verify_stage()?;
            output.generation_verify_destination()?;
            let stage_evidence = capture_required(&stage, "generation stage")?;
            reject_hardlink_alias(&stage, "generation stage", &stage_evidence)?;
            let destination_evidence = capture_optional(&destination, "generation destination")?;
            reject_optional_hardlink_alias(
                &destination,
                "generation destination",
                destination_evidence.as_ref(),
            )?;
            let preimage = destination_preimage_record(output.generation_expected_destination())?;
            if !preimage_matches(&preimage, destination_evidence.as_ref()) {
                return Err(format!(
                    "generation destination CAS preimage changed before ready: {}",
                    destination.display()
                ));
            }

            let backup = private_backup_path(&destination, &generation_id, index)?;
            let backup_route_key = physical_route_key(&backup)?;
            if backup_route_key == state_route_key
                || backup_route_key == state_lock_route_key
                || backup_route_key == stage_route_key
                || !protected_keys.insert(backup_route_key)
            {
                return Err(format!(
                    "generation private path collision or escape: {}",
                    backup.display()
                ));
            }
            if path_exists(&backup)? {
                return Err(format!(
                    "generation backup path already exists: {}",
                    backup.display()
                ));
            }

            members.push(GenerationMemberRecord {
                destination: path_text(&destination, "generation destination")?,
                destination_preimage: preimage,
                stage: stage_evidence,
                backup_path: path_text(&backup, "generation backup")?,
                backup: None,
                publication: MemberPublicationStep::Prepared,
            });
        }

        let document = GenerationDocument {
            schema: GENERATION_JOB_SCHEMA_V1.into(),
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            generation_id,
            revision: 1,
            phase: GenerationPhase::Ready,
            semantic_fingerprint,
            member_count: members.len(),
            members,
        };
        validate_document(&document, &state_path)?;
        let save_result = save_document(&state_path, &document);
        finish_ready_document_save(&state_path, &document, &mut atomic_outputs, save_result)?;
        Ok(Self {
            state_path,
            document,
            outputs: atomic_outputs,
            _state_lock: state_lock,
            semantic_fingerprint_verified: true,
            #[cfg(test)]
            failure_at_publication: None,
        })
    }

    /// Open a ready journal, or classify and recover a journal left in the
    /// middle of publication.
    pub fn open(state_path: impl Into<PathBuf>) -> Result<GenerationRecovery, String> {
        Self::resume(state_path)
    }

    /// Resume or recover a journal without authorizing publication of a ready
    /// generation.
    ///
    /// A returned ready transaction requires [`Self::commit_with_fingerprint`]
    /// before it can publish.
    pub fn resume(state_path: impl Into<PathBuf>) -> Result<GenerationRecovery, String> {
        Self::resume_internal(state_path, None)
    }

    /// Open a ready journal only when the caller supplies the semantic
    /// fingerprint it intends to publish.  Generic recovery can still inspect
    /// or roll back a journal without authorizing new bytes.
    pub fn open_with_fingerprint(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
    ) -> Result<GenerationRecovery, String> {
        Self::resume_with_fingerprint(state_path, semantic_fingerprint)
    }

    /// Resume or recover a journal while binding a caller-authorized semantic
    /// fingerprint.  A matching ready transaction can be committed directly.
    pub fn resume_with_fingerprint(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
    ) -> Result<GenerationRecovery, String> {
        let fingerprint = semantic_fingerprint.into();
        validate_semantic_fingerprint(&fingerprint)?;
        Self::resume_internal(state_path, Some(fingerprint))
    }

    fn resume_internal(
        state_path: impl Into<PathBuf>,
        expected_fingerprint: Option<String>,
    ) -> Result<GenerationRecovery, String> {
        let state_path = absolute_state_path(&state_path.into(), "generation state")?;
        let state_lock = StateFileLock::acquire(&state_path, "generation state")?;
        let bytes = read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
            .ok_or_else(|| format!("generation state does not exist: {}", state_path.display()))?;
        let document: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
            format!("decode generation state {}: {error}", state_path.display())
        })?;
        validate_document(&document, &state_path)?;
        let fingerprint_verified = expected_fingerprint
            .as_deref()
            .is_some_and(|expected| expected == document.semantic_fingerprint);
        if expected_fingerprint.is_some() && !fingerprint_verified {
            return Err(format!(
                "generation semantic fingerprint does not match state: {}",
                state_path.display()
            ));
        }

        match document.phase {
            GenerationPhase::Ready => {
                let transaction = Self::from_ready_document(
                    state_path,
                    document,
                    state_lock,
                    fingerprint_verified,
                )?;
                Ok(GenerationRecovery::Ready(transaction))
            }
            GenerationPhase::Publishing => recover_publishing(&state_path, document, state_lock),
            GenerationPhase::Committed => {
                validate_committed(&document)?;
                cleanup_committed_backups(&document)?;
                Ok(GenerationRecovery::Committed(status_from_document(
                    &state_path,
                    &document,
                )))
            }
            GenerationPhase::RolledBack => {
                validate_rolled_back(&document)?;
                Ok(GenerationRecovery::RolledBack(status_from_document(
                    &state_path,
                    &document,
                )))
            }
        }
    }

    /// Recover an in-progress journal and return its validated terminal/ready
    /// status.  A ready journal is intentionally left ready for the caller to
    /// reopen and publish; a publishing journal is rolled back unless every
    /// member's new bytes are already present.
    pub fn recover(state_path: impl Into<PathBuf>) -> Result<GenerationStatus, String> {
        match Self::resume(state_path)? {
            GenerationRecovery::Ready(transaction) => Ok(transaction.status()),
            GenerationRecovery::Committed(status) | GenerationRecovery::RolledBack(status) => {
                Ok(status)
            }
        }
    }

    /// Recover an in-progress journal after verifying the caller's semantic
    /// fingerprint, returning its validated current or terminal status.
    pub fn recover_with_fingerprint(
        state_path: impl Into<PathBuf>,
        semantic_fingerprint: impl Into<String>,
    ) -> Result<GenerationStatus, String> {
        match Self::resume_with_fingerprint(state_path, semantic_fingerprint)? {
            GenerationRecovery::Ready(transaction) => Ok(transaction.status()),
            GenerationRecovery::Committed(status) | GenerationRecovery::RolledBack(status) => {
                Ok(status)
            }
        }
    }

    /// Explicitly abandon a ready generation and reclaim only the private
    /// files proven to be owned by its journal.
    ///
    /// This never scans for files by name.  Destination preimages, stage
    /// identities, complete hashes, containment, and link counts are all
    /// revalidated while the state lock is held before any private file is
    /// removed.  Live destination bytes are left at their recorded preimage.
    pub fn abandon(mut self) -> Result<GenerationStatus, String> {
        if self.document.phase != GenerationPhase::Ready {
            return Err("only a ready generation can be explicitly abandoned".into());
        }
        validate_reclaimable_ready_document(&self.document)?;
        self.document.phase = GenerationPhase::Publishing;
        self.save_state()?;
        self.rollback_internal()?;
        validate_rolled_back(&self.document)?;
        Ok(self.status())
    }

    /// Recover an interrupted publication or explicitly abandon a valid
    /// ready journal, reclaiming only journal-bound private stages/backups.
    ///
    /// Callers should require an explicit confirmation before invoking this
    /// destructive operation.  Unknown `.forge-*` files are deliberately
    /// never considered reclaimable without a validated journal.
    pub fn reclaim(state_path: impl Into<PathBuf>) -> Result<GenerationStatus, String> {
        let state_path = absolute_state_path(&state_path.into(), "generation state")?;
        // Avoid creating a lock or parent directory for a nonexistent
        // journal. The state is read again after acquiring the lock below.
        if read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?.is_none() {
            return Err(format!(
                "generation state does not exist: {}",
                state_path.display()
            ));
        }
        let state_lock = StateFileLock::acquire(&state_path, "generation state")?;
        let bytes = read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
            .ok_or_else(|| {
                format!(
                    "generation state disappeared before reclaim: {}",
                    state_path.display()
                )
            })?;
        let document: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
            format!("decode generation state {}: {error}", state_path.display())
        })?;
        validate_document(&document, &state_path)?;
        match document.phase {
            GenerationPhase::Ready => {
                validate_reclaimable_ready_document(&document)?;
                let output_count = document.members.len();
                let mut transaction = Self {
                    state_path,
                    document,
                    outputs: (0..output_count).map(|_| None).collect(),
                    _state_lock: state_lock,
                    semantic_fingerprint_verified: false,
                    #[cfg(test)]
                    failure_at_publication: None,
                };
                transaction.document.phase = GenerationPhase::Publishing;
                transaction.save_state()?;
                transaction.rollback_internal()?;
                validate_rolled_back(&transaction.document)?;
                Ok(transaction.status())
            }
            GenerationPhase::Publishing => {
                match recover_publishing(&state_path, document, state_lock)? {
                    GenerationRecovery::Committed(status)
                    | GenerationRecovery::RolledBack(status) => Ok(status),
                    GenerationRecovery::Ready(_) => {
                        Err("publishing recovery unexpectedly returned ready".into())
                    }
                }
            }
            GenerationPhase::Committed => {
                validate_committed(&document)?;
                cleanup_committed_backups(&document)?;
                Ok(status_from_document(&state_path, &document))
            }
            GenerationPhase::RolledBack => {
                validate_rolled_back(&document)?;
                Ok(status_from_document(&state_path, &document))
            }
        }
    }

    /// Read and validate a journal without changing its phase.
    pub fn inspect(state_path: impl Into<PathBuf>) -> Result<GenerationStatus, String> {
        let state_path = absolute_state_path(&state_path.into(), "generation state")?;
        let bytes = read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
            .ok_or_else(|| format!("generation state does not exist: {}", state_path.display()))?;
        let document: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
            format!("decode generation state {}: {error}", state_path.display())
        })?;
        validate_document(&document, &state_path)?;
        match document.phase {
            GenerationPhase::Ready => validate_ready_document(&document)?,
            GenerationPhase::Committed => validate_committed(&document)?,
            GenerationPhase::RolledBack => validate_rolled_back(&document)?,
            GenerationPhase::Publishing => {
                // Publishing is expected to be transient; reject terminal
                // per-member states before recovery interprets live evidence.
                validate_publishing(&document)?;
            }
        }
        Ok(status_from_document(&state_path, &document))
    }

    /// Predict a confirmed [`Self::reclaim`] without creating a lock or
    /// changing any journal, destination, stage, or backup.
    pub fn inspect_reclaim(
        state_path: impl Into<PathBuf>,
    ) -> Result<GenerationReclaimInspection, String> {
        let state_path = absolute_state_path(&state_path.into(), "generation state")?;
        let initial = read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
            .ok_or_else(|| format!("generation state does not exist: {}", state_path.display()))?;
        let initial_document: GenerationDocument =
            serde_json::from_slice(&initial).map_err(|error| {
                format!("decode generation state {}: {error}", state_path.display())
            })?;
        validate_document(&initial_document, &state_path)?;
        if initial_document.phase == GenerationPhase::Publishing {
            validate_publishing(&initial_document)?;
        }

        let (lock_state, _held_lock) =
            match StateFileLock::probe_existing(&state_path, "generation state")? {
                StateFileLockProbe::Missing => (GenerationLockState::Missing, None),
                StateFileLockProbe::Available(lock) => (GenerationLockState::Available, Some(lock)),
                StateFileLockProbe::Active => {
                    return Ok(GenerationReclaimInspection {
                        status: status_from_document(&state_path, &initial_document),
                        action: GenerationReclaimAction::BlockedActive,
                        lock_state: GenerationLockState::Active,
                        private_file_count: 0,
                    });
                }
            };

        // Re-read after the lock probe. An available lock is held through all
        // dynamic evidence checks; a missing lock cannot be held by a
        // compliant generation process and is reported explicitly.
        let bytes = read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
            .ok_or_else(|| {
                format!(
                    "generation state disappeared during inspection: {}",
                    state_path.display()
                )
            })?;
        let document: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
            format!("decode generation state {}: {error}", state_path.display())
        })?;
        validate_document(&document, &state_path)?;
        if document.phase == GenerationPhase::Publishing {
            validate_publishing(&document)?;
        }
        let (action, private_file_count) = match document.phase {
            GenerationPhase::Ready => {
                validate_reclaimable_ready_document(&document)?;
                (
                    GenerationReclaimAction::AbandonReady,
                    count_existing_private_files(&document)?,
                )
            }
            GenerationPhase::Publishing => {
                if publishing_is_complete(&document)? {
                    (
                        GenerationReclaimAction::FinalizeCommitted,
                        count_existing_private_files(&document)?,
                    )
                } else {
                    classify_rollback(&document)?;
                    (
                        GenerationReclaimAction::RollBack,
                        count_existing_private_files(&document)?,
                    )
                }
            }
            GenerationPhase::Committed => {
                validate_committed(&document)?;
                let count = validate_committed_backups(&document)?;
                (
                    if count == 0 {
                        GenerationReclaimAction::Nothing
                    } else {
                        GenerationReclaimAction::CleanCommitted
                    },
                    count,
                )
            }
            GenerationPhase::RolledBack => {
                validate_rolled_back(&document)?;
                (GenerationReclaimAction::Nothing, 0)
            }
        };
        Ok(GenerationReclaimInspection {
            status: status_from_document(&state_path, &document),
            action,
            lock_state,
            private_file_count,
        })
    }

    /// Read only the structurally validated journal phase.
    ///
    /// Unlike [`Self::inspect`], this does not require terminal destination
    /// bytes to remain unchanged.  It lets an authorized rebuild distinguish
    /// a terminal record from a ready/publishing record without weakening the
    /// full recovery report.
    pub fn inspect_phase(state_path: impl Into<PathBuf>) -> Result<GenerationPhase, String> {
        let state_path = absolute_state_path(&state_path.into(), "generation state")?;
        let bytes = read_regular_state_file(&state_path, "generation state", MAX_STATE_BYTES)?
            .ok_or_else(|| format!("generation state does not exist: {}", state_path.display()))?;
        let document: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
            format!("decode generation state {}: {error}", state_path.display())
        })?;
        validate_document(&document, &state_path)?;
        if document.phase == GenerationPhase::Publishing {
            validate_publishing(&document)?;
        }
        Ok(document.phase)
    }

    fn from_ready_document(
        state_path: PathBuf,
        document: GenerationDocument,
        state_lock: StateFileLock,
        semantic_fingerprint_verified: bool,
    ) -> Result<Self, String> {
        validate_ready_document(&document)?;
        let mut outputs = Vec::with_capacity(document.members.len());
        for member in &document.members {
            let destination = PathBuf::from(&member.destination);
            let stage = PathBuf::from(&member.stage.path);
            let expected = destination_preimage_from_record(&member.destination_preimage)?;
            let output = AtomicOutput::from_existing_stage_with_destination_preimage(
                &destination,
                &stage,
                expected,
            )?;
            let observed = capture_required(&stage, "generation stage")?;
            ensure_stage_evidence(member, &observed)?;
            outputs.push(Some(output));
        }
        Ok(Self {
            state_path,
            document,
            outputs,
            _state_lock: state_lock,
            semantic_fingerprint_verified,
            #[cfg(test)]
            failure_at_publication: None,
        })
    }

    /// Current in-memory phase of the locked transaction.
    pub const fn phase(&self) -> GenerationPhase {
        self.document.phase
    }

    /// Absolute durable journal path held by this transaction.
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Deterministic identifier bound to the journal route and semantics.
    pub fn generation_id(&self) -> &str {
        &self.document.generation_id
    }

    /// Semantic fingerprint recorded in the journal.
    pub fn semantic_fingerprint(&self) -> &str {
        &self.document.semantic_fingerprint
    }

    /// Number of members owned by the transaction.
    pub fn member_count(&self) -> usize {
        self.document.member_count
    }

    /// Return a copyable snapshot of the transaction's durable state.
    pub fn status(&self) -> GenerationStatus {
        status_from_document(&self.state_path, &self.document)
    }

    /// Test-only deterministic failure hook.  The first publication is one.
    #[cfg(test)]
    fn fail_publication_at(mut self, one_based_member: usize) -> Self {
        self.failure_at_publication = Some(one_based_member);
        self
    }

    /// Publish every member, rolling back all already-published members if a
    /// later backup, rename, evidence check, or journal update fails.
    pub fn commit(self) -> Result<GenerationStatus, String> {
        if !self.semantic_fingerprint_verified {
            return Err(
                "generation resume requires caller semantic fingerprint; use commit_with_fingerprint"
                    .into(),
            );
        }
        self.publish()
    }

    /// Authorize and publish a ready transaction reopened without a semantic
    /// fingerprint.
    pub fn commit_with_fingerprint(
        mut self,
        semantic_fingerprint: impl AsRef<str>,
    ) -> Result<GenerationStatus, String> {
        if semantic_fingerprint.as_ref() != self.document.semantic_fingerprint {
            return Err("generation semantic fingerprint does not match state".into());
        }
        self.semantic_fingerprint_verified = true;
        self.publish()
    }

    fn publish(mut self) -> Result<GenerationStatus, String> {
        if self.document.phase != GenerationPhase::Ready {
            return Err("generation is not ready for publication".into());
        }
        validate_transaction_ready(&self)?;

        self.document.phase = GenerationPhase::Publishing;
        self.save_state()?;

        let publication_result = (|| {
            for index in 0..self.document.members.len() {
                self.backup_member(index)?;
                #[cfg(test)]
                if self.failure_at_publication == Some(index + 1) {
                    return Err(format!(
                        "injected generation publication failure at member {}",
                        index + 1
                    ));
                }
                self.publish_member(index)?;
            }
            Ok::<(), String>(())
        })();

        if let Err(error) = publication_result {
            let rollback = self.rollback_internal();
            return match rollback {
                Ok(()) => Err(format!(
                    "generation publication failed and rolled back: {error}"
                )),
                Err(rollback_error) => Err(format!(
                    "generation publication failed: {error}; rollback requires recovery: {rollback_error}"
                )),
            };
        }

        self.document.phase = GenerationPhase::Committed;
        if let Err(error) = self.save_state() {
            // The state remains publishing on disk.  A later resume sees all
            // new evidence and promotes it to committed.
            return Err(format!(
                "generation published but committed state could not be saved: {error}"
            ));
        }
        if let Err(error) = cleanup_committed_backups(&self.document) {
            return Err(format!(
                "generation committed but private-backup cleanup requires recovery: {error}"
            ));
        }
        Ok(self.status())
    }

    fn save_state(&mut self) -> Result<(), String> {
        self.document.revision = self
            .document
            .revision
            .checked_add(1)
            .ok_or("generation journal revision overflow")?;
        save_document(&self.state_path, &self.document)
    }

    fn backup_member(&mut self, index: usize) -> Result<(), String> {
        let member = self
            .document
            .members
            .get(index)
            .ok_or_else(|| format!("generation member index {index} is out of range"))?;
        if member.publication != MemberPublicationStep::Prepared {
            return Ok(());
        }
        let destination = PathBuf::from(&member.destination);
        let backup = PathBuf::from(&member.backup_path);
        if path_exists(&backup)? {
            return Err(format!(
                "generation backup path is unexpectedly occupied: {}",
                backup.display()
            ));
        }
        let destination_evidence = capture_optional(&destination, "generation destination")?;
        reject_optional_hardlink_alias(
            &destination,
            "generation destination",
            destination_evidence.as_ref(),
        )?;
        if !preimage_matches(&member.destination_preimage, destination_evidence.as_ref()) {
            return Err(format!(
                "generation destination CAS conflict before backup: {}",
                destination.display()
            ));
        }

        if let Some(destination_evidence) = destination_evidence.as_ref() {
            let expected_identity = parse_identity(&destination_evidence.identity)?;
            AtomicOutput::generation_move_without_replacing(
                &destination,
                &backup,
                &expected_identity,
            )
            .map_err(|error| {
                format!(
                    "backup generation destination {} to {}: {error}",
                    destination.display(),
                    backup.display()
                )
            })?;
            let backup_evidence = capture_required(&backup, "generation backup")?;
            reject_hardlink_alias(&backup, "generation backup", &backup_evidence)?;
            if !preimage_matches(&member.destination_preimage, Some(&backup_evidence)) {
                return Err(format!(
                    "generation backup evidence differs from destination preimage: {}",
                    backup.display()
                ));
            }
            let member = self
                .document
                .members
                .get_mut(index)
                .expect("member index checked above");
            member.backup = Some(backup_evidence);
        }
        let member = self
            .document
            .members
            .get_mut(index)
            .expect("member index checked above");
        member.publication = MemberPublicationStep::BackedUp;
        Ok(())
    }

    fn publish_member(&mut self, index: usize) -> Result<(), String> {
        let member = self
            .document
            .members
            .get(index)
            .ok_or_else(|| format!("generation member index {index} is out of range"))?;
        let destination = PathBuf::from(&member.destination);
        let stage = PathBuf::from(&member.stage.path);
        if path_exists(&destination)? {
            return Err(format!(
                "generation destination is not absent after backup: {}",
                destination.display()
            ));
        }
        let observed_stage = capture_required(&stage, "generation stage")?;
        ensure_stage_evidence(member, &observed_stage)?;
        reject_hardlink_alias(&stage, "generation stage", &observed_stage)?;

        let output = self.outputs[index]
            .take()
            .ok_or_else(|| format!("generation stage {} is no longer owned", stage.display()))?;
        output.generation_publish()?;
        let published = capture_required(&destination, "generation published output")?;
        reject_hardlink_alias(&destination, "generation published output", &published)?;
        if published.identity != member.stage.identity
            || published.byte_len != member.stage.byte_len
            || published.sha256 != member.stage.sha256
        {
            return Err(format!(
                "generation published output differs from stage evidence: {}",
                destination.display()
            ));
        }
        if path_exists(&stage)? {
            return Err(format!(
                "generation stage remained after publication: {}",
                stage.display()
            ));
        }
        let member = self
            .document
            .members
            .get_mut(index)
            .expect("member index checked above");
        member.publication = MemberPublicationStep::Published;
        Ok(())
    }

    fn rollback_internal(&mut self) -> Result<(), String> {
        let actions = classify_rollback(&self.document)?;
        for (index, action) in actions.into_iter().enumerate().rev() {
            rollback_member(&self.document.members[index], action)?;
            let member = self
                .document
                .members
                .get_mut(index)
                .expect("member index checked above");
            member.publication = if action == RollbackAction::UntouchedConflict {
                MemberPublicationStep::Abandoned
            } else {
                MemberPublicationStep::Restored
            };
        }
        self.document.phase = GenerationPhase::RolledBack;
        self.save_state()
    }
}

impl Drop for GenerationTransaction {
    fn drop(&mut self) {
        // Once `ready` is durable, retained stages and any backup evidence are
        // owned by the journal, not by this Rust value.  In particular, a
        // caller may drop a ready/publishing value after a process error and
        // still reopen the state for recovery.  Terminal publication paths
        // perform their own explicit cleanup after the terminal state is
        // durable; inspection/reclaim remains a separate responsibility.
    }
}

fn status_from_document(state_path: &Path, document: &GenerationDocument) -> GenerationStatus {
    GenerationStatus {
        state_path: state_path.to_owned(),
        generator: document.generator.clone(),
        generation_id: document.generation_id.clone(),
        semantic_fingerprint: document.semantic_fingerprint.clone(),
        phase: document.phase,
        member_count: document.member_count,
        publication_steps: document
            .members
            .iter()
            .map(|member| match member.publication {
                MemberPublicationStep::Prepared => "prepared",
                MemberPublicationStep::BackedUp => "backed_up",
                MemberPublicationStep::Published => "published",
                MemberPublicationStep::Restored => "restored",
                MemberPublicationStep::Abandoned => "abandoned",
            })
            .map(str::to_owned)
            .collect(),
        outputs: document
            .members
            .iter()
            .map(|member| GenerationOutputEvidence {
                destination: PathBuf::from(&member.destination),
                byte_len: member.stage.byte_len,
                sha256: member.stage.sha256.clone(),
                identity: FileIdentity(
                    parse_identity(&member.stage.identity)
                        .expect("validated generation stage identity"),
                ),
            })
            .collect(),
    }
}

fn validate_semantic_fingerprint(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(
            "generation semantic fingerprint must contain 1..=256 non-control characters".into(),
        );
    }
    Ok(())
}

fn valid_generator(value: &str) -> bool {
    if value.len() > MAX_GENERATOR_BYTES {
        return false;
    }
    let Some(version) = value.strip_prefix("forge-normalizer/") else {
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

fn generation_id(state_path: &Path, semantic_fingerprint: &str) -> Result<String, String> {
    let physical_state_path = physical_normalized_path(state_path)?;
    #[cfg(any(
        windows,
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    if physical_state_path.to_str().is_none() {
        return Err(format!(
            "generation state path must be valid Unicode on a case-insensitive platform: {}",
            physical_state_path.display()
        ));
    }
    let mut digest = Sha256::new();
    digest.update(b"forge-generation-job-v1\0");
    #[cfg(all(
        unix,
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "visionos"
        ))
    ))]
    {
        use std::os::unix::ffi::OsStrExt;
        digest.update(b"unix\0");
        digest.update(physical_state_path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        digest.update(b"windows\0");
        digest.update(physical_route_key(&physical_state_path)?.as_bytes());
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    {
        digest.update(b"apple\0");
        digest.update(physical_route_key(&physical_state_path)?.as_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    {
        digest.update(b"fallback\0");
        digest.update(physical_state_path.to_string_lossy().as_bytes());
    }
    digest.update([0]);
    digest.update(semantic_fingerprint.as_bytes());
    Ok(hex_digest(digest.finalize()))
}

fn validate_document(document: &GenerationDocument, state_path: &Path) -> Result<(), String> {
    let state_lock_path = sibling_lock_path(state_path)?;
    let state_route_key = physical_route_key(state_path)?;
    let state_lock_route_key = physical_route_key(&state_lock_path)?;
    if document.schema != GENERATION_JOB_SCHEMA_V1 {
        return Err(format!(
            "unsupported generation state schema: {}",
            document.schema
        ));
    }
    if !valid_generator(&document.generator) {
        return Err(format!(
            "generation state has an invalid generator provenance: {}",
            document.generator
        ));
    }
    if document.revision == 0 {
        return Err("generation journal revision must be greater than zero".into());
    }
    if document.members.is_empty() || document.members.len() > MAX_MEMBERS {
        return Err(format!(
            "generation member count must be in 1..={MAX_MEMBERS}"
        ));
    }
    if document.member_count != document.members.len() {
        return Err("generation member_count does not match members".into());
    }
    validate_semantic_fingerprint(&document.semantic_fingerprint)?;
    if !is_sha256(&document.generation_id) {
        return Err("generation_id must be a lower-case SHA-256 value".into());
    }
    if document.generation_id != generation_id(state_path, &document.semantic_fingerprint)? {
        return Err("generation_id does not match state path and semantic fingerprint".into());
    }

    let mut destinations = BTreeSet::new();
    let mut private_paths = BTreeSet::new();
    for (index, member) in document.members.iter().enumerate() {
        let destination = parse_absolute_path(&member.destination, "generation destination")?;
        let stage = parse_absolute_path(&member.stage.path, "generation stage")?;
        let backup = parse_absolute_path(&member.backup_path, "generation backup")?;
        let destination_route_key = physical_route_key(&destination)?;
        let stage_route_key = physical_route_key(&stage)?;
        let backup_route_key = physical_route_key(&backup)?;
        validate_private_stage_path(&destination, &stage)?;
        let expected_backup = private_backup_path(&destination, &document.generation_id, index)?;
        if backup_route_key != physical_route_key(&expected_backup)? {
            return Err(format!(
                "generation backup path escapes its destination sibling: {}",
                backup.display()
            ));
        }
        if !destinations.insert(destination_route_key.clone()) {
            return Err(format!(
                "duplicate generation destination in journal: {}",
                destination.display()
            ));
        }
        if destinations.contains(&stage_route_key)
            || destinations.contains(&backup_route_key)
            || !private_paths.insert(stage_route_key.clone())
            || !private_paths.insert(backup_route_key.clone())
        {
            return Err(format!(
                "duplicate generation private path in journal: member {index}"
            ));
        }
        if destination_route_key == state_route_key
            || destination_route_key == state_lock_route_key
            || stage_route_key == state_route_key
            || stage_route_key == state_lock_route_key
            || backup_route_key == state_route_key
            || backup_route_key == state_lock_route_key
        {
            return Err("generation member path aliases its state or lock path".into());
        }
        validate_file_evidence(&member.stage, &stage, "generation stage")?;
        validate_destination_record(&member.destination_preimage)?;
        if let Some(backup_evidence) = &member.backup {
            validate_file_evidence(backup_evidence, &backup, "generation backup")?;
            if !preimage_matches(&member.destination_preimage, Some(backup_evidence)) {
                return Err(format!(
                    "generation backup evidence does not match destination preimage: {}",
                    backup.display()
                ));
            }
        }
        if member.destination != destination.to_string_lossy()
            || member.stage.path != stage.to_string_lossy()
            || member.backup_path != backup.to_string_lossy()
        {
            return Err("generation journal path is not normalized".into());
        }
    }
    if destinations
        .iter()
        .any(|destination| private_paths.contains(destination))
    {
        return Err("generation destination collides with a private path".into());
    }
    Ok(())
}

fn validate_ready_document(document: &GenerationDocument) -> Result<(), String> {
    for member in &document.members {
        if member.publication != MemberPublicationStep::Prepared || member.backup.is_some() {
            return Err("ready generation journal has a non-prepared member".into());
        }
        let stage = Path::new(&member.stage.path);
        let observed = capture_required(stage, "generation ready stage")?;
        ensure_stage_evidence(member, &observed)?;
        reject_hardlink_alias(stage, "generation ready stage", &observed)?;
        let destination = Path::new(&member.destination);
        let observed_destination = capture_optional(destination, "generation ready destination")?;
        reject_optional_hardlink_alias(
            destination,
            "generation ready destination",
            observed_destination.as_ref(),
        )?;
        if !preimage_matches(&member.destination_preimage, observed_destination.as_ref()) {
            return Err(format!(
                "generation ready destination CAS preimage changed: {}",
                destination.display()
            ));
        }
    }
    Ok(())
}

fn validate_publishing(document: &GenerationDocument) -> Result<(), String> {
    for member in &document.members {
        if matches!(
            member.publication,
            MemberPublicationStep::Restored | MemberPublicationStep::Abandoned
        ) {
            return Err("publishing generation has a terminal member step".into());
        }
    }
    Ok(())
}

fn validate_transaction_ready(transaction: &GenerationTransaction) -> Result<(), String> {
    validate_ready_document(&transaction.document)?;
    for (index, output) in transaction.outputs.iter().enumerate() {
        let output = output
            .as_ref()
            .ok_or_else(|| format!("generation member {index} has no retained stage"))?;
        output.generation_verify_stage()?;
        output.generation_verify_destination()?;
    }
    Ok(())
}

fn validate_reclaimable_ready_document(document: &GenerationDocument) -> Result<(), String> {
    if document.phase != GenerationPhase::Ready {
        return Err("generation journal is not ready for abandonment".into());
    }
    for member in &document.members {
        if member.publication != MemberPublicationStep::Prepared || member.backup.is_some() {
            return Err("ready generation journal has a non-prepared member".into());
        }
        let stage = Path::new(&member.stage.path);
        let observed = capture_required(stage, "generation reclaim stage")?;
        ensure_stage_evidence(member, &observed)?;
        reject_hardlink_alias(stage, "generation reclaim stage", &observed)?;
        let backup = Path::new(&member.backup_path);
        if path_exists(backup)? {
            return Err(format!(
                "ready generation unexpectedly has a private backup: {}",
                backup.display()
            ));
        }
    }
    Ok(())
}

fn validate_committed(document: &GenerationDocument) -> Result<(), String> {
    for member in &document.members {
        if member.publication != MemberPublicationStep::Published {
            return Err("committed generation has a non-published member".into());
        }
        let destination = Path::new(&member.destination);
        let observed = capture_required(destination, "generation committed output")?;
        if observed.identity != member.stage.identity
            || observed.byte_len != member.stage.byte_len
            || observed.sha256 != member.stage.sha256
        {
            return Err(format!(
                "committed generation output changed: {}",
                destination.display()
            ));
        }
        reject_hardlink_alias(destination, "generation committed output", &observed)?;
        let stage = Path::new(&member.stage.path);
        if path_exists(stage)? {
            return Err(format!(
                "committed generation retains its private stage: {}",
                stage.display()
            ));
        }
    }
    Ok(())
}

fn validate_rolled_back(document: &GenerationDocument) -> Result<(), String> {
    for member in &document.members {
        match member.publication {
            MemberPublicationStep::Restored => {
                let destination = Path::new(&member.destination);
                let observed = capture_optional(destination, "generation rolled-back destination")?;
                if !preimage_matches(&member.destination_preimage, observed.as_ref()) {
                    return Err(format!(
                        "rolled-back generation destination differs from preimage: {}",
                        destination.display()
                    ));
                }
            }
            MemberPublicationStep::Abandoned => {
                // This member never moved its destination, but another
                // publisher changed that path. Only the absence of this
                // journal's private files is owned by the rollback.
            }
            _ => return Err("rolled-back generation has an unfinished member".into()),
        }
        let stage = Path::new(&member.stage.path);
        let backup = Path::new(&member.backup_path);
        if path_exists(stage)? || path_exists(backup)? {
            return Err("rolled-back generation retains a private file".into());
        }
    }
    Ok(())
}

fn ensure_terminal_private_paths_absent(document: &GenerationDocument) -> Result<(), String> {
    for member in &document.members {
        for path in [&member.stage.path, &member.backup_path] {
            let path = Path::new(path);
            if path_exists(path)? {
                return Err(format!(
                    "terminal generation retains a private path; recover or reclaim it first: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn retain_ready_stages(outputs: &mut [Option<AtomicOutput>]) {
    for output in outputs {
        output
            .as_mut()
            .expect("prepared output is present")
            .retain_stage();
    }
}

fn live_document_matches(state_path: &Path, expected: &GenerationDocument) -> Result<bool, String> {
    let Some(bytes) = read_regular_state_file(state_path, "generation state", MAX_STATE_BYTES)?
    else {
        return Ok(false);
    };
    let observed: GenerationDocument = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "decode generation state {} after save failure: {error}",
            state_path.display()
        )
    })?;
    validate_document(&observed, state_path)?;
    Ok(observed == *expected)
}

fn finish_ready_document_save(
    state_path: &Path,
    document: &GenerationDocument,
    outputs: &mut [Option<AtomicOutput>],
    save_result: Result<(), String>,
) -> Result<(), String> {
    match save_result {
        Ok(()) => {
            // The durable ready record now owns every stage. Before this point
            // normal AtomicOutput drop cleanup intentionally removes them when
            // validation or a pre-publication state write fails.
            retain_ready_stages(outputs);
            Ok(())
        }
        Err(save_error) => match live_document_matches(state_path, document) {
            Ok(true) => {
                // Atomic state publication can succeed before its parent
                // directory fsync reports an error. The visible journal owns
                // these exact stages, so dropping them would leave a Ready
                // record that can never resume or reclaim.
                retain_ready_stages(outputs);
                Err(format!(
                    "{save_error}; the ready journal is visible, so its stages were retained for recovery"
                ))
            }
            Ok(false) => Err(save_error),
            Err(inspection_error) => {
                // If the post-error state cannot be classified, retain rather
                // than risk destroying stages that a published journal names.
                // An absent or valid different journal is classified above and
                // still receives normal drop cleanup.
                retain_ready_stages(outputs);
                Err(format!(
                    "{save_error}; could not classify the ready journal after the save error, so stages were retained: {inspection_error}"
                ))
            }
        },
    }
}

fn save_document(path: &Path, document: &GenerationDocument) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| format!("encode generation state: {error}"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(format!(
            "generation state exceeds the {MAX_STATE_BYTES}-byte limit"
        ));
    }
    let mut output = AtomicOutput::new_with_overwrite_and_limit(path, true, MAX_STATE_BYTES)?;
    output.write_all(&bytes)?;
    output.commit()
}

fn validate_file_evidence(
    evidence: &FileEvidence,
    expected_path: &Path,
    description: &str,
) -> Result<(), String> {
    let path = parse_absolute_path(&evidence.path, description)?;
    if physical_route_key(&path)? != physical_route_key(expected_path)? {
        return Err(format!(
            "{description} evidence path differs from journal path: {}",
            expected_path.display()
        ));
    }
    parse_identity(&evidence.identity)
        .map_err(|_| format!("{description} evidence has an invalid identity"))?;
    if !is_sha256(&evidence.sha256) {
        return Err(format!("{description} evidence has an invalid SHA-256"));
    }
    Ok(())
}

fn validate_destination_record(record: &DestinationPreimageRecord) -> Result<(), String> {
    if let DestinationPreimageRecord::Present {
        identity, sha256, ..
    } = record
    {
        if parse_identity(identity).is_err() || !is_sha256(sha256) {
            return Err("generation destination preimage evidence is invalid".into());
        }
    }
    Ok(())
}

fn destination_preimage_record(
    preimage: &DestinationPreimage,
) -> Result<DestinationPreimageRecord, String> {
    match preimage.generation_present_parts() {
        None => Ok(DestinationPreimageRecord::Missing),
        Some((identity, byte_len, sha256)) => Ok(DestinationPreimageRecord::Present {
            identity: identity_token(identity),
            byte_len,
            sha256: hex_digest(sha256),
        }),
    }
}

fn destination_preimage_from_record(
    record: &DestinationPreimageRecord,
) -> Result<DestinationPreimage, String> {
    match record {
        DestinationPreimageRecord::Missing => Ok(DestinationPreimage::Missing),
        DestinationPreimageRecord::Present {
            identity,
            byte_len,
            sha256,
        } => Ok(DestinationPreimage::Present {
            identity: parse_identity(identity)?,
            byte_len: *byte_len,
            sha256: parse_sha256(sha256)?,
        }),
    }
}

fn preimage_matches(expected: &DestinationPreimageRecord, observed: Option<&FileEvidence>) -> bool {
    match (expected, observed) {
        (DestinationPreimageRecord::Missing, None) => true,
        (
            DestinationPreimageRecord::Present {
                identity,
                byte_len,
                sha256,
            },
            Some(observed),
        ) => {
            observed.identity == *identity
                && observed.byte_len == *byte_len
                && observed.sha256 == *sha256
        }
        _ => false,
    }
}

fn ensure_stage_evidence(
    member: &GenerationMemberRecord,
    observed: &FileEvidence,
) -> Result<(), String> {
    if observed.path != member.stage.path
        || observed.identity != member.stage.identity
        || observed.byte_len != member.stage.byte_len
        || observed.sha256 != member.stage.sha256
    {
        return Err(format!(
            "generation stage evidence changed or was tampered: {}",
            member.stage.path
        ));
    }
    Ok(())
}

fn backup_evidence_for_member(member: &GenerationMemberRecord) -> Result<FileEvidence, String> {
    if let Some(backup) = &member.backup {
        if backup.path != member.backup_path {
            return Err(format!(
                "generation backup evidence path does not match its journal path: {}",
                member.backup_path
            ));
        }
        return Ok(backup.clone());
    }

    // A process may stop after the atomic destination-to-backup rename but
    // before the member step is saved. The destination preimage already binds
    // that backup's complete identity and bytes, so recovery can reconstruct
    // the expected path-qualified evidence without trusting the live file.
    match &member.destination_preimage {
        DestinationPreimageRecord::Present {
            identity,
            byte_len,
            sha256,
        } => Ok(FileEvidence {
            path: member.backup_path.clone(),
            identity: identity.clone(),
            byte_len: *byte_len,
            sha256: sha256.clone(),
        }),
        DestinationPreimageRecord::Missing => Err(format!(
            "generation member with a missing preimage cannot have a backup: {}",
            member.destination
        )),
    }
}

/// Compare the identity and complete bytes of evidence after an atomic move.
/// The recorded pathname necessarily changes from source to destination, so
/// path equality is checked before the move and deliberately omitted here.
fn ensure_file_content_evidence_matches(
    expected: &FileEvidence,
    observed: &FileEvidence,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    if expected.identity != observed.identity
        || expected.byte_len != observed.byte_len
        || expected.sha256 != observed.sha256
    {
        return Err(format!(
            "{description} evidence changed or was tampered: {}",
            path.display()
        ));
    }
    Ok(())
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
            return Err(format!("invalid Unix generation identity: {value}"));
        }
        let device = parts
            .next()
            .ok_or_else(|| format!("invalid Unix generation identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Unix generation identity: {value}"))?;
        let inode = parts
            .next()
            .ok_or_else(|| format!("invalid Unix generation identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Unix generation identity: {value}"))?;
        if parts.next().is_some() {
            return Err(format!("invalid Unix generation identity: {value}"));
        }
        Ok(StableFileIdentity::Unix { device, inode })
    }
    #[cfg(windows)]
    {
        let mut parts = value.split(':');
        if parts.next() != Some("windows") {
            return Err(format!("invalid Windows generation identity: {value}"));
        }
        let volume = parts
            .next()
            .ok_or_else(|| format!("invalid Windows generation identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Windows generation identity: {value}"))?;
        let index = parts
            .next()
            .ok_or_else(|| format!("invalid Windows generation identity: {value}"))?
            .parse()
            .map_err(|_| format!("invalid Windows generation identity: {value}"))?;
        if parts.next().is_some() {
            return Err(format!("invalid Windows generation identity: {value}"));
        }
        Ok(StableFileIdentity::Windows { volume, index })
    }
    #[cfg(not(any(unix, windows)))]
    {
        value
            .strip_prefix("canonical:")
            .map(|path| StableFileIdentity::Canonical(PathBuf::from(path)))
            .ok_or_else(|| format!("invalid generation identity: {value}"))
    }
}

fn parse_sha256(value: &str) -> Result<[u8; 32], String> {
    if !is_sha256(value) {
        return Err(format!("invalid generation SHA-256: {value}"));
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
        _ => Err(format!("invalid hexadecimal generation digit: {value}")),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut text = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
    }
    text
}

/// Resolve every ancestor using filesystem component semantics while leaving
/// the final state-file component unresolved. State I/O deliberately opens
/// that final component with no-follow flags; resolving it here would turn a
/// caller-supplied symlink into its target before those checks can run.
fn absolute_state_path(path: &Path, description: &str) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolve {description} {}: {error}", path.display()))?
            .join(path)
    };
    let mut components = absolute.components().collect::<Vec<_>>();
    let name = match components.pop() {
        Some(std::path::Component::Normal(name)) => name.to_owned(),
        _ => {
            return Err(format!(
                "{description} has no final file component: {}",
                path.display()
            ));
        }
    };
    let mut parent = PathBuf::new();
    for component in components {
        parent.push(component.as_os_str());
    }
    let parent = physical_normalized_path(&parent)
        .map_err(|error| format!("resolve {description} {}: {error}", path.display()))?;
    let normalized = parent.join(name);
    if !normalized.is_absolute() {
        return Err(format!("{description} is not absolute: {}", path.display()));
    }
    match fs::symlink_metadata(&normalized) {
        Ok(metadata) => {
            #[cfg(windows)]
            let is_link =
                metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0;
            #[cfg(not(windows))]
            let is_link = metadata.file_type().is_symlink();
            if is_link {
                return Err(format!(
                    "refuse symbolic-link or reparse-point {description}: {}",
                    normalized.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "inspect {description} {} without following links: {error}",
                normalized.display()
            ));
        }
    }
    Ok(normalized)
}

fn absolute_normalized(path: &Path, description: &str) -> Result<PathBuf, String> {
    let normalized = physical_normalized_path(path)
        .map_err(|error| format!("resolve {description} {}: {error}", path.display()))?;
    if !normalized.is_absolute() {
        return Err(format!("{description} is not absolute: {}", path.display()));
    }
    Ok(normalized)
}

fn parse_absolute_path(value: &str, description: &str) -> Result<PathBuf, String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(format!(
            "{description} path is empty or contains control characters"
        ));
    }
    let path = PathBuf::from(value);
    let normalized = absolute_normalized(&path, description)?;
    if normalized.to_string_lossy() != value {
        return Err(format!("{description} path is not normalized: {value}"));
    }
    Ok(normalized)
}

fn path_text(path: &Path, description: &str) -> Result<String, String> {
    path.to_str()
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| format!("{description} path must be UTF-8: {}", path.display()))
}

fn private_backup_path(
    destination: &Path,
    generation_id: &str,
    index: usize,
) -> Result<PathBuf, String> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "generation destination has no parent directory: {}",
            destination.display()
        )
    })?;
    let name = format!("{BACKUP_PREFIX}{generation_id}-{index}.bak");
    let backup = parent.join(name);
    let normalized = absolute_normalized(&backup, "generation backup")?;
    let normalized_parent = normalized.parent().ok_or_else(|| {
        format!(
            "generation backup path has no parent directory: {}",
            normalized.display()
        )
    })?;
    if normalized.parent() != Some(parent)
        || physical_route_key(normalized_parent)? != physical_route_key(parent)?
    {
        return Err(format!(
            "generation backup path escapes destination parent: {}",
            normalized.display()
        ));
    }
    Ok(normalized)
}

fn validate_private_stage_path(destination: &Path, stage: &Path) -> Result<(), String> {
    let destination_parent = destination.parent().ok_or_else(|| {
        format!(
            "generation destination has no parent directory: {}",
            destination.display()
        )
    })?;
    let stage_parent = stage.parent().ok_or_else(|| {
        format!(
            "generation stage has no parent directory: {}",
            stage.display()
        )
    })?;
    if physical_route_key(destination_parent)? != physical_route_key(stage_parent)? {
        return Err(format!(
            "generation stage path escapes destination parent: {}",
            stage.display()
        ));
    }
    let name = stage
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !name.starts_with(STAGE_PREFIX) || name == STAGE_PREFIX || name.contains('/') {
        return Err(format!(
            "generation stage is not a private sibling path: {}",
            stage.display()
        ));
    }
    if physical_route_key(destination)? == physical_route_key(stage)? {
        return Err(format!(
            "generation stage aliases destination: {}",
            destination.display()
        ));
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect private path {}: {error}", path.display())),
    }
}

fn capture_required(path: &Path, description: &str) -> Result<FileEvidence, String> {
    capture_optional(path, description)?.ok_or_else(|| {
        format!(
            "{description} is missing at the required path: {}",
            path.display()
        )
    })
}

fn capture_optional(path: &Path, description: &str) -> Result<Option<FileEvidence>, String> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {description} {}: {error}", path.display())),
    };
    if link_metadata.file_type().is_symlink() {
        return Err(format!(
            "{description} must not be a symbolic link: {}",
            path.display()
        ));
    }
    if !link_metadata.is_file() {
        return Err(format!(
            "{description} must be a regular file: {}",
            path.display()
        ));
    }

    let file = open_regular(path, description)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect opened {description} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} is not a regular file: {}",
            path.display()
        ));
    }
    let identity = identity_from_open_file(&file, path)
        .map_err(|error| format!("identify {description} {}: {error}", path.display()))?;
    let mut reader = file
        .try_clone()
        .map_err(|error| format!("clone {description} {}: {error}", path.display()))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {description} {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("hash {description} {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = file
        .metadata()
        .map_err(|error| format!("reinspect {description} {}: {error}", path.display()))?;
    let confirmation = open_regular(path, description)?;
    let confirmation_identity = identity_from_open_file(&confirmation, path)
        .map_err(|error| format!("reidentify {description} {}: {error}", path.display()))?;
    if metadata.len() != after.len() || identity != confirmation_identity {
        return Err(format!(
            "{description} changed while it was inspected: {}",
            path.display()
        ));
    }
    Ok(Some(FileEvidence {
        path: path_text(path, description)?,
        identity: identity_token(&identity),
        byte_len: after.len(),
        sha256: hex_digest(hasher.finalize()),
    }))
}

fn open_regular(path: &Path, description: &str) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options
        .share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004)
        .custom_flags(0x0020_0000);
    let file = options.open(path).map_err(|error| {
        format!(
            "open {description} {} without following links: {error}",
            path.display()
        )
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect opened {description} {}: {error}", path.display()))?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "{description} must not be a reparse point: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "{description} must be a regular file: {}",
            path.display()
        ));
    }
    Ok(file)
}

fn reject_optional_hardlink_alias(
    path: &Path,
    description: &str,
    evidence: Option<&FileEvidence>,
) -> Result<(), String> {
    if let Some(evidence) = evidence {
        reject_hardlink_alias(path, description, evidence)?;
    }
    Ok(())
}

fn reject_hardlink_alias(
    path: &Path,
    description: &str,
    _evidence: &FileEvidence,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
        if metadata.nlink() > 1 {
            return Err(format!(
                "{description} must not have hard-link aliases: {}",
                path.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        let file = open_regular(path, description)?;
        let links = crate::stable_input::windows_file_link_count(&file)
            .map_err(|error| format!("inspect {description} {}: {error}", path.display()))?;
        if links > 1 {
            return Err(format!(
                "{description} must not have hard-link aliases: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn remove_private_if_present(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(format!(
                    "refuse to reclaim non-regular private path: {}",
                    path.display()
                ));
            }
            #[cfg(windows)]
            if metadata.permissions().readonly() {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(false);
                fs::set_permissions(path, permissions).map_err(|error| {
                    format!(
                        "make journal-owned private path removable {}: {error}",
                        path.display()
                    )
                })?;
            }
            fs::remove_file(path)
                .map_err(|error| format!("remove private path {}: {error}", path.display()))?;
            AtomicOutput::sync_generation_parent(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            AtomicOutput::sync_generation_parent(path)
        }
        Err(error) => Err(format!("inspect private path {}: {error}", path.display())),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RollbackAction {
    Untouched,
    UntouchedConflict,
    BackedUp,
    PublishedExisting,
    PublishedMissing,
    AlreadyRestored,
}

fn classify_rollback(document: &GenerationDocument) -> Result<Vec<RollbackAction>, String> {
    let mut actions = Vec::with_capacity(document.members.len());
    for member in &document.members {
        let destination = Path::new(&member.destination);
        let stage = Path::new(&member.stage.path);
        let backup = Path::new(&member.backup_path);
        let observed_destination =
            capture_optional(destination, "generation rollback destination")?;
        let observed_stage = capture_optional(stage, "generation rollback stage")?;
        let observed_backup = capture_optional(backup, "generation rollback backup")?;

        if let Some(evidence) = observed_stage.as_ref() {
            ensure_stage_evidence(member, evidence)?;
            reject_hardlink_alias(stage, "generation rollback stage", evidence)?;
        }
        if let Some(evidence) = observed_backup.as_ref() {
            reject_hardlink_alias(backup, "generation rollback backup", evidence)?;
            if !preimage_matches(&member.destination_preimage, Some(evidence)) {
                return Err(format!(
                    "generation rollback backup evidence changed: {}",
                    backup.display()
                ));
            }
        }
        reject_optional_hardlink_alias(
            destination,
            "generation rollback destination",
            observed_destination.as_ref(),
        )?;

        if member.publication == MemberPublicationStep::Restored {
            if observed_stage.is_some()
                || observed_backup.is_some()
                || !preimage_matches(&member.destination_preimage, observed_destination.as_ref())
            {
                return Err(format!(
                    "generation restored member has inconsistent evidence: {}",
                    destination.display()
                ));
            }
            actions.push(RollbackAction::AlreadyRestored);
            continue;
        }

        let destination_is_preimage =
            preimage_matches(&member.destination_preimage, observed_destination.as_ref());
        let destination_is_new = observed_destination.as_ref().is_some_and(|evidence| {
            evidence.identity == member.stage.identity
                && evidence.byte_len == member.stage.byte_len
                && evidence.sha256 == member.stage.sha256
        });
        let backup_is_preimage = observed_backup
            .as_ref()
            .is_some_and(|evidence| preimage_matches(&member.destination_preimage, Some(evidence)));
        let stage_is_new = observed_stage.as_ref().is_some_and(|evidence| {
            evidence.identity == member.stage.identity
                && evidence.byte_len == member.stage.byte_len
                && evidence.sha256 == member.stage.sha256
        });

        // A rollback may have completed its filesystem work and then lost the
        // terminal journal write.  The absence of both private files together
        // with the exact destination preimage is a safe, already-restored
        // state even if the per-member step is still stale in the journal.
        if observed_stage.is_none() && observed_backup.is_none() && destination_is_preimage {
            actions.push(RollbackAction::AlreadyRestored);
            continue;
        }

        // An untouched-conflict rollback removes only this journal's stage.
        // If the process stops after that unlink but before the terminal
        // journal write, the durable step is still `prepared` while the live
        // destination belongs to the competing publisher. No private path
        // remains to restore, so repeat the conflict action without touching
        // the live destination.
        if observed_stage.is_none()
            && observed_backup.is_none()
            && member.publication == MemberPublicationStep::Prepared
            && !destination_is_preimage
            && !destination_is_new
        {
            actions.push(RollbackAction::UntouchedConflict);
            continue;
        }

        // An intact stage with no generation backup means this member never
        // moved its destination: every supported move is one atomic rename.
        // It is therefore safe to abandon the private stage even when a
        // concurrent publisher has changed the live destination.
        if stage_is_new
            && observed_backup.is_none()
            && member.publication == MemberPublicationStep::Prepared
        {
            actions.push(if destination_is_preimage {
                RollbackAction::Untouched
            } else {
                RollbackAction::UntouchedConflict
            });
            continue;
        }
        if stage_is_new
            && observed_destination.is_none()
            && (backup_is_preimage
                || (observed_backup.is_none()
                    && matches!(
                        member.destination_preimage,
                        DestinationPreimageRecord::Missing
                    )))
        {
            actions.push(if backup_is_preimage {
                RollbackAction::BackedUp
            } else {
                RollbackAction::Untouched
            });
            continue;
        }
        if observed_stage.is_none() && destination_is_new {
            if backup_is_preimage {
                actions.push(RollbackAction::PublishedExisting);
                continue;
            }
            if observed_backup.is_none()
                && matches!(
                    member.destination_preimage,
                    DestinationPreimageRecord::Missing
                )
            {
                actions.push(RollbackAction::PublishedMissing);
                continue;
            }
        }

        return Err(format!(
            "generation rollback has ambiguous or tampered filesystem evidence: {}",
            destination.display()
        ));
    }
    Ok(actions)
}

fn rollback_member(member: &GenerationMemberRecord, action: RollbackAction) -> Result<(), String> {
    let destination = Path::new(&member.destination);
    let stage = Path::new(&member.stage.path);
    let backup = Path::new(&member.backup_path);
    match action {
        RollbackAction::AlreadyRestored => Ok(()),
        RollbackAction::Untouched => {
            if path_exists(destination)? {
                let observed = capture_required(destination, "generation rollback destination")?;
                if !preimage_matches(&member.destination_preimage, Some(&observed)) {
                    return Err(format!(
                        "generation rollback destination changed: {}",
                        destination.display()
                    ));
                }
            }
            remove_private_if_present(stage)
        }
        RollbackAction::UntouchedConflict => remove_private_if_present(stage),
        RollbackAction::BackedUp => {
            if path_exists(destination)? {
                return Err(format!(
                    "generation rollback destination unexpectedly exists: {}",
                    destination.display()
                ));
            }
            let expected_backup = backup_evidence_for_member(member)?;
            restore_without_replacing(backup, destination, &expected_backup, "generation backup")?;
            remove_private_if_present(stage)
        }
        RollbackAction::PublishedExisting => {
            move_private_without_replacing(
                destination,
                stage,
                &member.stage,
                "generation published output",
            )?;
            let expected_backup = backup_evidence_for_member(member)?;
            restore_without_replacing(backup, destination, &expected_backup, "generation backup")?;
            remove_private_if_present(stage)
        }
        RollbackAction::PublishedMissing => {
            move_private_without_replacing(
                destination,
                stage,
                &member.stage,
                "generation published output",
            )?;
            remove_private_if_present(stage)
        }
    }
}

/// Move a private regular file without replacing a path that appeared during
/// rollback.  The platform-specific no-clobber/WRITE_THROUGH implementation
/// lives beside the existing AtomicOutput publication primitive.
fn move_private_without_replacing(
    source: &Path,
    destination: &Path,
    expected: &FileEvidence,
    description: &str,
) -> Result<(), String> {
    let observed = capture_required(source, description)?;
    // The journal evidence follows the file from stage to destination (or
    // from destination preimage to backup), so its original pathname differs
    // during a rollback move. The caller supplies the validated journal path;
    // bind the movable object by identity, length, and complete digest here.
    ensure_file_content_evidence_matches(expected, &observed, source, description)?;
    reject_hardlink_alias(source, description, &observed)?;
    let expected_identity = parse_identity(&expected.identity)?;
    AtomicOutput::generation_move_without_replacing(source, destination, &expected_identity)?;
    let moved = capture_required(destination, description)?;
    if let Err(evidence_error) =
        ensure_file_content_evidence_matches(expected, &moved, destination, description)
    {
        return Err(restore_moved_after_evidence_mismatch(
            source,
            destination,
            &moved,
            description,
            evidence_error,
        ));
    }
    reject_hardlink_alias(destination, description, &moved)?;
    Ok(())
}

/// If the source changed in place after the pre-move evidence check, the
/// no-clobber move can still succeed because its inode identity is unchanged.
/// Keep the changed bytes recoverable: move them back to the original live
/// pathname using their observed identity, and never unlink the private path
/// when that restoration loses a race.
fn restore_moved_after_evidence_mismatch(
    source: &Path,
    destination: &Path,
    moved: &FileEvidence,
    description: &str,
    evidence_error: String,
) -> String {
    let observed_identity = match parse_identity(&moved.identity) {
        Ok(identity) => identity,
        Err(error) => {
            return format!(
                "{evidence_error}; moved {description} retained at {} because its identity could not be rebound: {error}",
                destination.display()
            )
        }
    };
    match AtomicOutput::generation_move_without_replacing(
        destination,
        source,
        &observed_identity,
    ) {
        Ok(()) => match capture_required(source, description) {
            Ok(restored) => match ensure_file_content_evidence_matches(
                moved,
                &restored,
                source,
                description,
            ) {
                Ok(()) => format!(
                    "{evidence_error}; moved {description} was restored to {}",
                    source.display()
                ),
                Err(restore_error) => format!(
                    "{evidence_error}; moved {description} was restored to {}, but restoration verification failed: {restore_error}",
                    source.display()
                ),
            },
            Err(restore_error) => format!(
                "{evidence_error}; moved {description} was restored to {}, but restoration verification failed: {restore_error}",
                source.display()
            ),
        },
        Err(restore_error) => format!(
            "{evidence_error}; moved {description} retained at {} because no-clobber restoration to {} failed: {restore_error}",
            destination.display(),
            source.display()
        ),
    }
}

fn restore_without_replacing(
    backup: &Path,
    destination: &Path,
    expected: &FileEvidence,
    description: &str,
) -> Result<(), String> {
    move_private_without_replacing(backup, destination, expected, description)
}

fn recover_publishing(
    state_path: &Path,
    mut document: GenerationDocument,
    state_lock: StateFileLock,
) -> Result<GenerationRecovery, String> {
    validate_publishing(&document)?;
    let all_published = publishing_is_complete(&document)?;
    if all_published {
        for member in &mut document.members {
            member.publication = MemberPublicationStep::Published;
        }
        document.phase = GenerationPhase::Committed;
        document.revision = document
            .revision
            .checked_add(1)
            .ok_or("generation journal revision overflow")?;
        save_document(state_path, &document)?;
        cleanup_committed_backups(&document)?;
        drop(state_lock);
        return Ok(GenerationRecovery::Committed(status_from_document(
            state_path, &document,
        )));
    }

    let actions = classify_rollback(&document)?;
    for (index, action) in actions.into_iter().enumerate().rev() {
        rollback_member(&document.members[index], action)?;
        document.members[index].publication = if action == RollbackAction::UntouchedConflict {
            MemberPublicationStep::Abandoned
        } else {
            MemberPublicationStep::Restored
        };
    }
    document.phase = GenerationPhase::RolledBack;
    document.revision = document
        .revision
        .checked_add(1)
        .ok_or("generation journal revision overflow")?;
    save_document(state_path, &document)?;
    drop(state_lock);
    Ok(GenerationRecovery::RolledBack(status_from_document(
        state_path, &document,
    )))
}

fn publishing_is_complete(document: &GenerationDocument) -> Result<bool, String> {
    for member in &document.members {
        let destination = Path::new(&member.destination);
        let stage = Path::new(&member.stage.path);
        let backup = Path::new(&member.backup_path);
        if path_exists(stage)? {
            return Ok(false);
        }
        let destination_evidence =
            capture_optional(destination, "generation recovery destination")?;
        let Some(destination_evidence) = destination_evidence else {
            return Ok(false);
        };
        if destination_evidence.identity != member.stage.identity
            || destination_evidence.byte_len != member.stage.byte_len
            || destination_evidence.sha256 != member.stage.sha256
        {
            return Ok(false);
        }
        reject_hardlink_alias(
            destination,
            "generation recovery destination",
            &destination_evidence,
        )?;
        match &member.destination_preimage {
            DestinationPreimageRecord::Present { .. } => {
                let backup_evidence = capture_optional(backup, "generation recovery backup")?;
                let Some(backup_evidence) = backup_evidence else {
                    return Ok(false);
                };
                reject_hardlink_alias(backup, "generation recovery backup", &backup_evidence)?;
                if !preimage_matches(&member.destination_preimage, Some(&backup_evidence)) {
                    return Ok(false);
                }
            }
            DestinationPreimageRecord::Missing => {
                if path_exists(backup)? {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn cleanup_committed_backups(document: &GenerationDocument) -> Result<(), String> {
    if document.phase != GenerationPhase::Committed {
        return Err("private-backup cleanup requires a committed generation".into());
    }
    for member in &document.members {
        let stage = Path::new(&member.stage.path);
        if path_exists(stage)? {
            return Err(format!(
                "committed generation unexpectedly retains its stage: {}",
                stage.display()
            ));
        }
        let backup = Path::new(&member.backup_path);
        if validated_committed_backup(member)?.is_none() {
            continue;
        }
        remove_private_if_present(backup)?;
    }
    Ok(())
}

fn count_existing_private_files(document: &GenerationDocument) -> Result<usize, String> {
    let mut count = 0_usize;
    for member in &document.members {
        for path in [&member.stage.path, &member.backup_path] {
            if path_exists(Path::new(path))? {
                count = count
                    .checked_add(1)
                    .ok_or("generation private-file count overflow")?;
            }
        }
    }
    Ok(count)
}

fn validate_committed_backups(document: &GenerationDocument) -> Result<usize, String> {
    if document.phase != GenerationPhase::Committed {
        return Err("private-backup validation requires a committed generation".into());
    }
    let mut count = 0_usize;
    for member in &document.members {
        if validated_committed_backup(member)?.is_some() {
            count = count
                .checked_add(1)
                .ok_or("generation private-file count overflow")?;
        }
    }
    Ok(count)
}

fn validated_committed_backup(
    member: &GenerationMemberRecord,
) -> Result<Option<FileEvidence>, String> {
    let backup = Path::new(&member.backup_path);
    let Some(observed) = capture_optional(backup, "generation committed backup")? else {
        return Ok(None);
    };
    reject_hardlink_alias(backup, "generation committed backup", &observed)?;
    if !preimage_matches(&member.destination_preimage, Some(&observed)) {
        return Err(format!(
            "generation committed backup evidence changed: {}",
            backup.display()
        ));
    }
    if let Some(recorded) = &member.backup {
        if recorded != &observed {
            return Err(format!(
                "generation committed backup differs from recorded evidence: {}",
                backup.display()
            ));
        }
    }
    Ok(Some(observed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic::AtomicOutput;
    use std::fs;

    fn prepared(directory: &Path, name: &str, bytes: &[u8]) -> (PreparedGenerationOutput, PathBuf) {
        let destination = directory.join(name);
        let mut output = AtomicOutput::new_with_overwrite(&destination, true).unwrap();
        output.write_all(bytes).unwrap();
        (PreparedGenerationOutput::new(output).unwrap(), destination)
    }

    fn make_generation(
        directory: &Path,
        first_destination: Option<&[u8]>,
        second_destination: Option<&[u8]>,
    ) -> (GenerationTransaction, PathBuf, PathBuf, PathBuf) {
        let first = directory.join("first.wav");
        let second = directory.join("second.wav");
        if let Some(bytes) = first_destination {
            fs::write(&first, bytes).unwrap();
        }
        if let Some(bytes) = second_destination {
            fs::write(&second, bytes).unwrap();
        }
        let (first_output, _) = prepared(directory, "first.wav", b"new-first");
        let (second_output, _) = prepared(directory, "second.wav", b"new-second");
        let state = directory.join("generation.json");
        let transaction = GenerationTransaction::prepare(
            &state,
            "semantic-generation-test",
            vec![first_output, second_output],
        )
        .unwrap();
        (transaction, state, first, second)
    }

    #[test]
    fn ready_save_error_retains_stages_when_the_new_journal_is_visible() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) = make_generation(directory.path(), None, None);
        let (prepared, _) = prepared(directory.path(), "retained.wav", b"retained-stage");
        let stage = prepared.staged_path().to_owned();
        let mut outputs = vec![Some(prepared.into_atomic())];

        let error = finish_ready_document_save(
            &state,
            &transaction.document,
            &mut outputs,
            Err("injected post-publication directory sync failure".into()),
        )
        .unwrap_err();
        drop(outputs);

        assert!(error.contains("stages were retained"), "{error}");
        assert!(stage.exists());
    }

    #[test]
    fn ready_save_error_cleans_stages_when_no_new_journal_exists() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, _, _, _) = make_generation(directory.path(), None, None);
        let (prepared, _) = prepared(directory.path(), "discarded.wav", b"discarded-stage");
        let stage = prepared.staged_path().to_owned();
        let mut outputs = vec![Some(prepared.into_atomic())];

        let error = finish_ready_document_save(
            &directory.path().join("missing-generation.json"),
            &transaction.document,
            &mut outputs,
            Err("injected pre-publication state failure".into()),
        )
        .unwrap_err();
        drop(outputs);

        assert_eq!(error, "injected pre-publication state failure");
        assert!(!stage.exists());
    }

    #[cfg(all(
        unix,
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "visionos"
        ))
    ))]
    #[test]
    fn generation_identity_distinguishes_non_utf8_state_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(OsString::from_vec(b"state-\x80.json".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"state-\x81.json".to_vec()));
        assert_ne!(
            generation_id(&first, "semantic").unwrap(),
            generation_id(&second, "semantic").unwrap()
        );
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    #[test]
    fn generation_identity_rejects_non_utf8_state_paths_on_apple_platforms() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(b"state-\x80.json".to_vec()));
        let error = generation_id(&path, "semantic").unwrap_err();
        assert!(error.contains("valid Unicode"), "{error}");
    }

    #[cfg(any(
        windows,
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    #[test]
    fn generation_identity_uses_case_folded_physical_state_route() {
        assert_eq!(
            generation_id(Path::new("Generation-State.JSON"), "semantic").unwrap(),
            generation_id(Path::new("generation-state.json"), "semantic").unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_final_symlink_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("victim.json");
        let state = directory.path().join("generation.json");
        fs::write(&target, b"victim-state").unwrap();
        symlink(&target, &state).unwrap();
        let (prepared, destination) = prepared(directory.path(), "result.wav", b"new");

        let error =
            GenerationTransaction::prepare(&state, "semantic-state-symlink-test", vec![prepared])
                .unwrap_err();
        assert!(
            error.contains("symbolic-link") || error.contains("reparse-point"),
            "{error}"
        );
        assert_eq!(fs::read(target).unwrap(), b"victim-state");
        assert!(!destination.exists());
        assert!(!sibling_lock_path(&state).unwrap().exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn state_hard_link_alias_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) = make_generation(directory.path(), None, None);
        drop(transaction);
        let alias = directory.path().join("generation-alias.json");
        fs::hard_link(&state, &alias).unwrap();

        let error = GenerationTransaction::inspect(&alias).unwrap_err();
        assert!(error.contains("hard-link"), "{error}");
    }

    #[test]
    fn commits_existing_and_missing_destinations_as_one_generation() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, first, second) =
            make_generation(directory.path(), Some(b"old-first"), None);
        let status = transaction.commit().unwrap();
        assert_eq!(status.phase(), GenerationPhase::Committed);
        assert_eq!(fs::read(first).unwrap(), b"new-first");
        assert_eq!(fs::read(second).unwrap(), b"new-second");
        assert_eq!(
            GenerationTransaction::inspect(&state).unwrap().phase(),
            GenerationPhase::Committed
        );
    }

    #[test]
    fn committed_status_exposes_and_verifies_complete_output_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, _state, first, _) = make_generation(directory.path(), None, None);
        let status = transaction.commit().unwrap();
        let evidence = status
            .outputs()
            .iter()
            .find(|evidence| evidence.destination() == first)
            .expect("committed status contains the first destination");

        assert_eq!(evidence.byte_len(), b"new-first".len() as u64);
        assert!(evidence.verify_live_destination().is_ok());
        assert!(evidence.identity().same_file(evidence.file_identity()));

        // Replacing the destination with the same bytes must still be rejected
        // because the committed evidence binds the original regular-file
        // identity, not only its content digest.
        let replacement = directory.path().join("replacement.wav");
        fs::write(&replacement, b"new-first").unwrap();
        fs::remove_file(&first).unwrap();
        fs::rename(replacement, &first).unwrap();
        let error = evidence.verify_live_destination().unwrap_err();
        assert!(error.contains("differs from evidence"), "{error}");

        fs::remove_file(&first).unwrap();
        fs::create_dir(&first).unwrap();
        let error = evidence.verify_live_destination().unwrap_err();
        assert!(error.contains("regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn committed_evidence_rejects_a_symlink_destination() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let (transaction, _state, first, _) = make_generation(directory.path(), None, None);
        let status = transaction.commit().unwrap();
        let evidence = status
            .outputs()
            .iter()
            .find(|evidence| evidence.destination() == first)
            .expect("committed status contains the first destination");
        let target = directory.path().join("target.wav");
        fs::write(&target, b"new-first").unwrap();
        fs::remove_file(&first).unwrap();
        symlink(target, &first).unwrap();

        let error = evidence.verify_live_destination().unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn generator_provenance_is_bounded_to_256_bytes() {
        assert!(valid_generator("forge-normalizer/1.2.3"));
        let oversized = format!("forge-normalizer/1.2.{}", "0".repeat(MAX_GENERATOR_BYTES));
        assert!(oversized.len() > MAX_GENERATOR_BYTES);
        assert!(!valid_generator(&oversized));

        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) = make_generation(directory.path(), None, None);
        drop(transaction);
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        document["generator"] = oversized.into();
        fs::write(&state, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        let error = GenerationTransaction::inspect(&state).unwrap_err();
        assert!(error.contains("invalid generator provenance"), "{error}");
    }

    #[test]
    fn second_publication_failure_restores_existing_and_missing_destinations() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, first, second) =
            make_generation(directory.path(), Some(b"old-first"), None);
        let error = transaction.fail_publication_at(2).publish().unwrap_err();
        assert!(error.contains("rolled back"), "{error}");
        assert_eq!(fs::read(first).unwrap(), b"old-first");
        assert!(!second.exists());
        let status = GenerationTransaction::inspect(&state).unwrap();
        assert_eq!(status.phase(), GenerationPhase::RolledBack);
        assert!(fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| {
                let name = entry.file_name();
                name == "first.wav" || name == "generation.json" || name == "generation.json.lock"
            }));
    }

    #[test]
    fn destination_cas_conflict_is_rejected_before_ready() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        fs::write(&destination, b"old").unwrap();
        let mut output = AtomicOutput::new_with_overwrite(&destination, true).unwrap();
        output.write_all(b"new").unwrap();
        fs::write(&destination, b"bad").unwrap();
        let prepared = PreparedGenerationOutput::new(output).unwrap();
        let error = GenerationTransaction::prepare(
            directory.path().join("generation.json"),
            "semantic-cas-test",
            vec![prepared],
        )
        .unwrap_err();
        assert!(
            error.contains("changed") || error.contains("CAS"),
            "{error}"
        );
        assert_eq!(fs::read(destination).unwrap(), b"bad");
    }

    #[test]
    fn stage_tamper_is_rejected_and_ready_journal_is_recoverable() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("result.wav");
        let (prepared, _) = prepared(directory.path(), "result.wav", b"new");
        let stage = prepared.staged_path().to_owned();
        let state = directory.path().join("generation.json");
        let transaction =
            GenerationTransaction::prepare(&state, "semantic-stage-test", vec![prepared]).unwrap();
        fs::write(&stage, b"tampered").unwrap();
        let error = transaction.commit().unwrap_err();
        assert!(
            error.contains("stage") || error.contains("tamper"),
            "{error}"
        );
        assert!(!destination.exists());
        assert!(stage.exists());
        assert!(GenerationTransaction::inspect(&state)
            .unwrap_err()
            .contains("stage"));
    }

    #[test]
    fn publishing_journal_recovery_rolls_back_filesystem_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let (mut transaction, state, first, second) =
            make_generation(directory.path(), Some(b"old-first"), None);
        transaction.document.phase = GenerationPhase::Publishing;
        transaction.save_state().unwrap();
        transaction.backup_member(0).unwrap();
        transaction.publish_member(0).unwrap();
        drop(transaction);

        let recovery = GenerationTransaction::resume(&state).unwrap();
        let GenerationRecovery::RolledBack(status) = recovery else {
            panic!("expected rollback recovery");
        };
        assert_eq!(status.phase(), GenerationPhase::RolledBack);
        assert_eq!(fs::read(first).unwrap(), b"old-first");
        assert!(!second.exists());
    }

    #[test]
    fn rollback_rechecks_published_source_evidence_after_classification() {
        let directory = tempfile::tempdir().unwrap();
        let (mut transaction, _state, first, _) = make_generation(directory.path(), None, None);
        transaction.document.phase = GenerationPhase::Publishing;
        transaction.save_state().unwrap();
        transaction.backup_member(0).unwrap();
        transaction.publish_member(0).unwrap();

        let actions = classify_rollback(&transaction.document).unwrap();
        assert_eq!(actions[0], RollbackAction::PublishedMissing);

        // Model a competing publisher replacing the destination in the
        // classify -> move window.  Rollback must refuse the moved bytes and
        // leave the competing output in place.
        fs::write(&first, b"competing-generation").unwrap();
        let error = rollback_member(&transaction.document.members[0], actions[0]).unwrap_err();
        assert!(error.contains("evidence changed"), "{error}");
        assert_eq!(fs::read(first).unwrap(), b"competing-generation");
    }

    #[test]
    fn rollback_rechecks_published_source_identity_after_competing_rename() {
        let directory = tempfile::tempdir().unwrap();
        let (mut transaction, _state, first, _) = make_generation(directory.path(), None, None);
        transaction.document.phase = GenerationPhase::Publishing;
        transaction.save_state().unwrap();
        transaction.backup_member(0).unwrap();
        transaction.publish_member(0).unwrap();

        let actions = classify_rollback(&transaction.document).unwrap();
        assert_eq!(actions[0], RollbackAction::PublishedMissing);

        // Model a competing generation's rename replacement: unlike the
        // in-place rewrite test above, this gives the competing bytes a new
        // regular-file identity before replacing the live destination.
        let competing = directory.path().join("competing.wav");
        fs::write(&competing, b"competing-renamed-generation").unwrap();
        fs::remove_file(&first).unwrap();
        fs::rename(&competing, &first).unwrap();
        let competing_evidence = capture_required(&first, "competing destination").unwrap();

        let error = rollback_member(&transaction.document.members[0], actions[0]).unwrap_err();
        assert!(error.contains("evidence changed"), "{error}");
        assert_eq!(
            capture_required(&first, "competing destination").unwrap(),
            competing_evidence
        );
        assert_eq!(fs::read(first).unwrap(), b"competing-renamed-generation");
        assert!(!Path::new(&transaction.document.members[0].stage.path).exists());
    }

    #[test]
    fn post_move_evidence_failure_restores_bytes_or_retains_private_path() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("live.wav");
        let private = directory.path().join(".forge-stage.wav");
        fs::write(&private, b"moved-but-unexpected").unwrap();
        let moved = capture_required(&private, "generation published output").unwrap();

        let error = restore_moved_after_evidence_mismatch(
            &source,
            &private,
            &moved,
            "generation published output",
            "forced full-evidence mismatch".into(),
        );
        assert!(error.contains("was restored"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), b"moved-but-unexpected");
        assert!(!private.exists());

        // If a competing path appears before restoration, the no-clobber
        // retry fails and the unexpected bytes remain at the journal-owned
        // private path for a later recovery attempt.
        fs::write(&source, b"competing-live").unwrap();
        fs::write(&private, b"retained-private-bytes").unwrap();
        let moved = capture_required(&private, "generation published output").unwrap();
        let error = restore_moved_after_evidence_mismatch(
            &source,
            &private,
            &moved,
            "generation published output",
            "forced full-evidence mismatch".into(),
        );
        assert!(error.contains("retained at"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), b"competing-live");
        assert_eq!(fs::read(&private).unwrap(), b"retained-private-bytes");
    }

    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    #[test]
    fn canonical_order_lets_overlapping_generations_abandon_the_loser() {
        let directory = tempfile::tempdir().unwrap();
        let (a_z, z) = prepared(directory.path(), "z.wav", b"a-z");
        let (a_x, x) = prepared(directory.path(), "x.wav", b"a-x");
        let (b_z, _) = prepared(directory.path(), "z.wav", b"b-z");
        let (b_y, y) = prepared(directory.path(), "y.wav", b"b-y");
        let mut a = GenerationTransaction::prepare(
            directory.path().join("a-generation.json"),
            "semantic-overlap-a",
            vec![a_z, a_x],
        )
        .unwrap();
        let mut b = GenerationTransaction::prepare(
            directory.path().join("b-generation.json"),
            "semantic-overlap-b",
            vec![b_z, b_y],
        )
        .unwrap();
        assert!(a.document.members[0].destination.ends_with("x.wav"));
        assert!(b.document.members[0].destination.ends_with("y.wav"));

        for transaction in [&mut a, &mut b] {
            transaction.document.phase = GenerationPhase::Publishing;
            transaction.save_state().unwrap();
        }
        a.backup_member(0).unwrap();
        a.publish_member(0).unwrap();
        b.backup_member(0).unwrap();
        b.publish_member(0).unwrap();
        a.backup_member(1).unwrap();
        a.publish_member(1).unwrap();
        assert!(b.backup_member(1).is_err());

        b.rollback_internal().unwrap();
        validate_rolled_back(&b.document).unwrap();
        assert!(b
            .document
            .members
            .iter()
            .any(|member| member.publication == MemberPublicationStep::Abandoned));
        a.document.phase = GenerationPhase::Committed;
        a.save_state().unwrap();
        cleanup_committed_backups(&a.document).unwrap();

        assert_eq!(fs::read(x).unwrap(), b"a-x");
        assert_eq!(fs::read(z).unwrap(), b"a-z");
        assert!(!y.exists());
    }

    #[cfg(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    #[test]
    fn recovery_finishes_an_interrupted_untouched_conflict_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let (a_z, z) = prepared(directory.path(), "z.wav", b"a-z");
        let (a_x, x) = prepared(directory.path(), "x.wav", b"a-x");
        let (b_z, _) = prepared(directory.path(), "z.wav", b"b-z");
        let (b_y, y) = prepared(directory.path(), "y.wav", b"b-y");
        let mut a = GenerationTransaction::prepare(
            directory.path().join("a-generation.json"),
            "semantic-overlap-crash-a",
            vec![a_z, a_x],
        )
        .unwrap();
        let b_state = directory.path().join("b-generation.json");
        let mut b =
            GenerationTransaction::prepare(&b_state, "semantic-overlap-crash-b", vec![b_z, b_y])
                .unwrap();

        for transaction in [&mut a, &mut b] {
            transaction.document.phase = GenerationPhase::Publishing;
            transaction.save_state().unwrap();
        }
        a.backup_member(0).unwrap();
        a.publish_member(0).unwrap();
        b.backup_member(0).unwrap();
        b.publish_member(0).unwrap();
        a.backup_member(1).unwrap();
        a.publish_member(1).unwrap();
        assert!(b.backup_member(1).is_err());

        // Simulate a stop after the conflict member's stage was removed but
        // before any rollback step was written to the journal.
        let actions = classify_rollback(&b.document).unwrap();
        assert_eq!(actions[1], RollbackAction::UntouchedConflict);
        rollback_member(&b.document.members[1], actions[1]).unwrap();
        drop(b);

        let GenerationRecovery::RolledBack(status) =
            GenerationTransaction::resume(&b_state).unwrap()
        else {
            panic!("expected interrupted conflict rollback to finish");
        };
        assert_eq!(status.phase(), GenerationPhase::RolledBack);
        assert_eq!(fs::read(&x).unwrap(), b"a-x");
        assert_eq!(fs::read(&z).unwrap(), b"a-z");
        assert!(!y.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_destination_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.wav");
        let destination = directory.path().join("result.wav");
        fs::write(&target, b"target").unwrap();
        symlink(&target, &destination).unwrap();
        let output = AtomicOutput::new_with_overwrite(&destination, true)
            .err()
            .expect("symbolic link must be rejected");
        assert!(output.contains("destination") || output.contains("symbolic"));

        let real = directory.path().join("real.wav");
        let alias = directory.path().join("alias.wav");
        fs::write(&real, b"real").unwrap();
        fs::hard_link(&real, &alias).unwrap();
        let (prepared, _) = prepared(directory.path(), "alias.wav", b"new-alias");
        let error = GenerationTransaction::prepare(
            directory.path().join("hardlink-generation.json"),
            "semantic-hardlink-test",
            vec![prepared],
        )
        .unwrap_err();
        assert!(error.contains("hard-link"), "{error}");
    }

    #[test]
    fn duplicate_destination_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let (first, _) = prepared(directory.path(), "same.wav", b"first");
        let (second, _) = prepared(directory.path(), "same.wav", b"second");
        let error = GenerationTransaction::prepare(
            directory.path().join("duplicate-generation.json"),
            "semantic-duplicate-test",
            vec![first, second],
        )
        .unwrap_err();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[test]
    fn dropped_ready_transaction_keeps_stage_and_requires_fingerprint_to_resume() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, destination, _) = make_generation(directory.path(), None, None);
        let fingerprint = transaction.semantic_fingerprint().to_owned();
        let stage = transaction.document.members[0].stage.path.clone();
        drop(transaction);
        assert!(Path::new(&stage).exists());
        let GenerationRecovery::Ready(transaction) = GenerationTransaction::resume(&state).unwrap()
        else {
            panic!("expected ready transaction");
        };
        assert!(transaction.commit().is_err());
        let GenerationRecovery::Ready(transaction) =
            GenerationTransaction::resume_with_fingerprint(&state, fingerprint).unwrap()
        else {
            panic!("expected ready transaction with matching fingerprint");
        };
        transaction.commit().unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"new-first");
    }

    #[test]
    fn explicit_reclaim_abandons_ready_generation_without_touching_destinations() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, first, second) =
            make_generation(directory.path(), Some(b"old-first"), None);
        let stages = transaction
            .document
            .members
            .iter()
            .map(|member| PathBuf::from(&member.stage.path))
            .collect::<Vec<_>>();
        drop(transaction);

        let status = GenerationTransaction::reclaim(&state).unwrap();
        assert_eq!(status.phase(), GenerationPhase::RolledBack);
        assert_eq!(fs::read(first).unwrap(), b"old-first");
        assert!(!second.exists());
        assert!(stages.iter().all(|stage| !stage.exists()));
    }

    #[test]
    fn reclaim_abandons_an_unpublished_member_after_destination_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, first, _) =
            make_generation(directory.path(), Some(b"old-first"), None);
        let stages = transaction
            .document
            .members
            .iter()
            .map(|member| PathBuf::from(&member.stage.path))
            .collect::<Vec<_>>();
        drop(transaction);
        fs::write(&first, b"concurrent-generation").unwrap();

        let status = GenerationTransaction::reclaim(&state).unwrap();
        assert_eq!(status.phase(), GenerationPhase::RolledBack);
        assert!(status
            .publication_steps()
            .iter()
            .any(|step| step == "abandoned"));
        assert_eq!(fs::read(first).unwrap(), b"concurrent-generation");
        assert!(stages.iter().all(|stage| !stage.exists()));
    }

    #[test]
    fn side_effect_free_inspection_does_not_recreate_a_missing_lock() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) = make_generation(directory.path(), None, None);
        drop(transaction);
        let lock = state.with_file_name("generation.json.lock");
        fs::remove_file(&lock).unwrap();

        let status = GenerationTransaction::inspect(&state).unwrap();
        assert_eq!(status.phase(), GenerationPhase::Ready);
        assert!(!lock.exists());

        GenerationTransaction::reclaim(&state).unwrap();
    }

    #[test]
    fn reclaim_inspection_reports_active_and_missing_locks_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) = make_generation(directory.path(), None, None);
        let active = GenerationTransaction::inspect_reclaim(&state).unwrap();
        assert_eq!(active.action(), GenerationReclaimAction::BlockedActive);
        assert_eq!(active.lock_state(), GenerationLockState::Active);
        drop(transaction);

        let lock = sibling_lock_path(&state).unwrap();
        fs::remove_file(&lock).unwrap();
        let missing = GenerationTransaction::inspect_reclaim(&state).unwrap();
        assert_eq!(missing.action(), GenerationReclaimAction::AbandonReady);
        assert_eq!(missing.lock_state(), GenerationLockState::Missing);
        assert_eq!(missing.private_file_count(), 2);
        assert!(!lock.exists());
        GenerationTransaction::reclaim(&state).unwrap();
    }

    #[test]
    fn reclaim_of_a_missing_journal_creates_no_lock_or_parent() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("missing-parent");
        let state = parent.join("generation.json");
        let error = GenerationTransaction::reclaim(&state).unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
        assert!(!parent.exists());
    }

    #[test]
    fn nested_unknown_fields_and_committed_step_tamper_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) =
            make_generation(directory.path(), Some(b"old-first"), None);
        drop(transaction);
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        document["members"][0]["destination_preimage"]["unknown"] = 1.into();
        fs::write(&state, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        assert!(GenerationTransaction::inspect(&state).is_err());

        let other = tempfile::tempdir().unwrap();
        let (transaction, state, _, _) = make_generation(other.path(), None, None);
        transaction.commit().unwrap();
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
        document["members"][0]["publication"] = "prepared".into();
        fs::write(&state, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        let error = GenerationTransaction::inspect(&state).unwrap_err();
        assert!(error.contains("non-published"), "{error}");
    }

    #[test]
    fn resume_reclaims_a_journal_bound_backup_left_after_commit() {
        let directory = tempfile::tempdir().unwrap();
        let (mut transaction, state, first, _) =
            make_generation(directory.path(), Some(b"old-first"), None);
        transaction.document.phase = GenerationPhase::Publishing;
        transaction.save_state().unwrap();
        for index in 0..transaction.document.members.len() {
            transaction.backup_member(index).unwrap();
            transaction.publish_member(index).unwrap();
        }
        let backup = PathBuf::from(&transaction.document.members[0].backup_path);
        assert!(backup.exists());
        transaction.document.phase = GenerationPhase::Committed;
        transaction.save_state().unwrap();
        drop(transaction);

        let GenerationRecovery::Committed(status) = GenerationTransaction::resume(&state).unwrap()
        else {
            panic!("expected committed recovery");
        };
        assert_eq!(status.phase(), GenerationPhase::Committed);
        assert_eq!(fs::read(first).unwrap(), b"new-first");
        assert!(!backup.exists());
    }

    #[test]
    fn member_cannot_alias_the_generation_lock_path() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("generation.json");
        let lock = sibling_lock_path(&state).unwrap();
        let mut output = AtomicOutput::new_with_overwrite(&lock, true).unwrap();
        output.write_all(b"not-a-lock").unwrap();
        let prepared = PreparedGenerationOutput::new(output).unwrap();

        let error =
            GenerationTransaction::prepare(&state, "semantic-lock-alias-test", vec![prepared])
                .unwrap_err();
        assert!(error.contains("lock path aliases"), "{error}");
        assert!(!state.exists());
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn member_cannot_physically_alias_state_or_lock_through_a_symlink_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real_parent = directory.path().join("real");
        let alias_parent = directory.path().join("alias");
        fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &alias_parent).unwrap();

        let state = real_parent.join("generation.json");
        let lock = sibling_lock_path(&state).unwrap();
        for (name, label) in [
            ("generation.json", "state"),
            ("generation.json.lock", "lock"),
        ] {
            let destination = alias_parent.join(name);
            let mut output = AtomicOutput::new_with_overwrite(&destination, true).unwrap();
            output.write_all(label.as_bytes()).unwrap();
            let prepared = PreparedGenerationOutput::new(output).unwrap();

            let error = GenerationTransaction::prepare(
                &state,
                format!("physical-route-{label}"),
                vec![prepared],
            )
            .unwrap_err();
            assert!(error.contains("state or lock path aliases"), "{error}");
            assert!(!state.exists());
            assert!(!lock.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn member_stage_cannot_physically_alias_generation_lock_through_a_symlink_parent() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real_parent = directory.path().join("real");
        let alias_parent = directory.path().join("alias");
        fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &alias_parent).unwrap();

        // Choose a state basename whose lock is itself a valid private-stage
        // basename. The stage file uses the alias spelling but occupies the
        // lock's physical inode before prepare can create or open a lock.
        let state = real_parent.join(".forge-generation-stage");
        let lock = sibling_lock_path(&state).unwrap();
        let destination = alias_parent.join("target.wav");
        let stage = alias_parent.join(".forge-generation-stage.lock");
        fs::write(&stage, b"staged-through-alias").unwrap();
        let output = AtomicOutput::from_existing_stage_with_destination_preimage(
            &destination,
            &stage,
            DestinationPreimage::Missing,
        )
        .unwrap();
        let prepared = PreparedGenerationOutput::new(output).unwrap();

        let error = GenerationTransaction::prepare(&state, "physical-stage-route", vec![prepared])
            .unwrap_err();
        assert!(error.contains("state or lock path aliases"), "{error}");
        assert!(!state.exists());
        assert_eq!(fs::read(&lock).unwrap(), b"staged-through-alias");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_parent_before_dotdot_keeps_publication_and_evidence_on_one_route() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real_parent = directory.path().join("real");
        let nested = real_parent.join("nested");
        let alias = directory.path().join("alias");
        fs::create_dir_all(&nested).unwrap();
        symlink(&nested, &alias).unwrap();

        // Filesystem traversal resolves `alias` first, so `..` selects
        // `real`, not the lexical temp-directory parent.
        let destination = alias.join("..").join("output.wav");
        let mut output = AtomicOutput::new_with_overwrite(&destination, true).unwrap();
        output.write_all(b"new-output").unwrap();
        let prepared = PreparedGenerationOutput::new(output).unwrap();
        let state = directory.path().join("generation.json");

        let status = GenerationTransaction::prepare(&state, "physical-dotdot", vec![prepared])
            .unwrap()
            .commit()
            .unwrap();
        let physical_destination = real_parent.join("output.wav");
        assert_eq!(fs::read(&physical_destination).unwrap(), b"new-output");
        assert!(!directory.path().join("output.wav").exists());
        assert_eq!(status.outputs()[0].destination(), physical_destination);
        status.outputs()[0].verify_live_destination().unwrap();
    }

    #[test]
    fn explicitly_replaces_a_terminal_journal_for_an_authorized_rebuild() {
        let directory = tempfile::tempdir().unwrap();
        let (transaction, state, first, _) = make_generation(directory.path(), None, None);
        let fingerprint = transaction.semantic_fingerprint().to_owned();
        transaction.commit().unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"new-first");

        let (replacement, _) = prepared(directory.path(), "first.wav", b"newer-first");
        let transaction = GenerationTransaction::prepare_replacing_terminal(
            &state,
            &fingerprint,
            vec![replacement],
        )
        .unwrap();
        transaction.commit().unwrap();
        assert_eq!(fs::read(first).unwrap(), b"newer-first");
    }
}
