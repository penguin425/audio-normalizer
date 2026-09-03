//! Conservative, copy-to-new-file metadata repair for delivery containers.
//!
//! The repairer deliberately mutates container bytes without re-encoding
//! audio.  ISO-BMFF loudness repair decodes a bounded reference only to derive
//! measurements; media payloads are copied and hash-verified verbatim.  Unknown
//! WAVE chunks and ADM XML are also preserved.  MXF is currently
//! validate-and-copy only because safe mutation requires a partition/index
//! table writer.

use crate::adm::{self, ProductionProfileMode, ProductionProfileResult};
use crate::container_qc::{self, ContainerAudit};
use crate::isobmff_loudness_repair::{self, DecodedLoudness, EncodedLoudness, TargetTrack};
use crate::metadata;
use crate::stable_input::{
    identity_from_open_file, BoundInput, StableFileIdentity, StableInput, StableInputOptions,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-metadata-repair-1";
pub const SCHEMA_VERSION_V2: u32 = 2;
pub const VALIDATOR_V2: &str = "forge-metadata-repair-2";
pub const DEFAULT_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_METADATA_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_XML_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_CHUNKS: u32 = 100_000;
pub const DEFAULT_MAX_DECODED_SAMPLES: u64 = 500_000_000;
pub const HARD_MAX_DECODED_SAMPLES: u64 = 4_000_000_000;
pub const DEFAULT_MAX_ALBUM_REFERENCES: u32 = 1_000;
pub const HARD_MAX_ALBUM_REFERENCES: u32 = 10_000;

type FileIdentity = StableFileIdentity;

enum FileSnapshotState {
    Bound(BoundInput),
    Captured(StableInput),
}

struct FileSnapshot {
    path: PathBuf,
    identity: FileIdentity,
    len: u64,
    sha256: String,
    state: FileSnapshotState,
}

impl FileSnapshot {
    fn capture(path: &Path, max_bytes: u64, description: &str) -> Result<Self, String> {
        Self::bind(path, max_bytes, description)?.snapshot_contents(max_bytes, description)
    }

    /// Bind a path, open identity, length, and content hash without copying
    /// the payload. Album entries that resolve to the already-snapshotted
    /// selected track use this form so that track bytes are not stored twice.
    fn bind(path: &Path, max_bytes: u64, description: &str) -> Result<Self, String> {
        let options = StableInputOptions::new(max_bytes)
            .map_err(|error| format!("configure {description}: {error}"))?;
        let bound = BoundInput::bind(path, &options)
            .map_err(|error| format!("bind {description} {}: {error}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            identity: bound.identity().clone(),
            len: bound.byte_len(),
            sha256: bound.binding().sha256_hex(),
            state: FileSnapshotState::Bound(bound),
        })
    }

    fn snapshot_contents(self, max_bytes: u64, description: &str) -> Result<Self, String> {
        if self.len > max_bytes {
            return Err(format!(
                "{description} {} is {} bytes, above the configured byte limit {max_bytes}",
                self.path.display(),
                self.len
            ));
        }
        let Self {
            path,
            identity,
            len,
            sha256,
            state,
        } = self;
        let stable = match state {
            FileSnapshotState::Bound(bound) => bound
                .snapshot()
                .map_err(|error| format!("snapshot {description} {}: {error}", path.display()))?,
            FileSnapshotState::Captured(stable) => stable,
        };
        Ok(Self {
            path,
            identity,
            len,
            sha256,
            state: FileSnapshotState::Captured(stable),
        })
    }

    fn verify(&self, max_bytes: u64, context: &str) -> Result<(), String> {
        if self.len > max_bytes {
            return Err(format!(
                "{context}: {} exceeds the configured byte limit {max_bytes}",
                self.path.display()
            ));
        }
        let result = match &self.state {
            FileSnapshotState::Bound(bound) => bound.verify_source(),
            FileSnapshotState::Captured(stable) => stable.verify_source(),
        };
        result.map_err(|error| format!("{context}: {}: {error}", self.path.display()))
    }

    fn stable_path(&self) -> &Path {
        match &self.state {
            FileSnapshotState::Captured(stable) => stable.stable_path(),
            FileSnapshotState::Bound(_) => {
                panic!("metadata input contents must be captured before decoding")
            }
        }
    }
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case)]
struct WindowsFileTime {
    dwLowDateTime: u32,
    dwHighDateTime: u32,
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case)]
struct WindowsByHandleFileInformation {
    dwFileAttributes: u32,
    ftCreationTime: WindowsFileTime,
    ftLastAccessTime: WindowsFileTime,
    ftLastWriteTime: WindowsFileTime,
    dwVolumeSerialNumber: u32,
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    nNumberOfLinks: u32,
    nFileIndexHigh: u32,
    nFileIndexLow: u32,
}

#[cfg(windows)]
fn windows_file_information(file: &File) -> Result<WindowsByHandleFileInformation, String> {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut c_void,
            information: *mut WindowsByHandleFileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<WindowsByHandleFileInformation>::uninit();
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    if succeeded == 0 {
        return Err(format!(
            "identify open file: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { information.assume_init() })
}

#[cfg(test)]
fn open_verified_file_snapshot(
    path: &Path,
    expected_identity: &FileIdentity,
    expected_len: u64,
) -> Result<File, String> {
    let file = File::open(path)
        .map_err(|error| format!("reopen decoded reference {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("restat decoded reference {}: {error}", path.display()))?;
    let identity = identity_from_open_file(&file, path).map_err(|error| error.to_string())?;
    if &identity != expected_identity || metadata.len() != expected_len {
        return Err(format!(
            "decoded reference changed while preparing album loudness: {}",
            path.display()
        ));
    }
    Ok(file)
}

#[cfg(test)]
fn ensure_file_snapshot(
    path: &Path,
    expected_identity: &FileIdentity,
    expected_len: u64,
) -> Result<(), String> {
    open_verified_file_snapshot(path, expected_identity, expected_len).map(drop)
}

#[cfg(test)]
fn ensure_matching_file_hash(
    path: &Path,
    before: &str,
    after: &str,
    context: &str,
) -> Result<(), String> {
    if before == after {
        Ok(())
    } else {
        Err(format!("{context}: {}", path.display()))
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RepairMode {
    Validate,
    #[default]
    Repair,
}

/// EBU R 128 loudness values stored in the five fixed BWF `bext` fields.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BwfLoudness {
    pub integrated_lufs: f64,
    pub loudness_range_lu: f64,
    pub true_peak_dbtp: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
}

/// Derive one ISO-BMFF track-level `ludt/tlou` value from decoded PCM.
///
/// When `decoded_reference` is absent the source file itself is decoded.  An
/// explicit reference is required for an fMP4 initialization segment that has
/// no media samples of its own; its path and SHA-256 are retained in evidence.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IsobmffLoudnessRepair {
    #[serde(default)]
    pub decoded_reference: Option<PathBuf>,
    #[serde(default = "default_max_decoded_samples")]
    pub max_decoded_samples: u64,
}

/// Schema-v2 ISO-BMFF loudness repair with optional album-level `alou` data.
///
/// `album_decoded_references`, when present, is the complete ordered album and
/// must include the selected track's decoded reference exactly once. The
/// decoded-sample and input-byte limits apply to the aggregate unique set.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IsobmffLoudnessRepairV2 {
    #[serde(default)]
    pub decoded_reference: Option<PathBuf>,
    #[serde(default)]
    pub album_decoded_references: Option<Vec<PathBuf>>,
    #[serde(default = "default_max_album_references")]
    pub max_album_references: u32,
    #[serde(default = "default_max_decoded_samples")]
    pub max_decoded_samples: u64,
}

/// Versioned and bounded metadata repair request.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataRepairSpec {
    pub schema_version: u32,
    pub source: PathBuf,
    pub destination: PathBuf,
    #[serde(default)]
    pub mode: RepairMode,
    /// Create/upgrade a WAVE `bext` chunk to the 602-byte BWF v2 layout.
    #[serde(default)]
    pub ensure_bwf_v2: bool,
    /// Replace the five BWF loudness values without changing audio bytes.
    #[serde(default)]
    pub bwf_loudness: Option<BwfLoudness>,
    /// Set the BS.2076 `audioFormatExtended/@version` value in an existing
    /// ADM `axml` chunk.  Only the published version supported by Forge is
    /// accepted; arbitrary XML declarations are not invented.
    #[serde(default)]
    pub adm_version: Option<String>,
    /// Permit replacing an existing destination.  The source is never
    /// replaced by this API.
    #[serde(default)]
    pub overwrite: bool,
    /// Write to a temporary file in the destination directory and rename it
    /// into place after all bytes have been flushed.
    #[serde(default)]
    pub atomic_replace: bool,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
    #[serde(default = "default_max_metadata_chunk_bytes")]
    pub max_metadata_chunk_bytes: u64,
    #[serde(default = "default_max_xml_bytes")]
    pub max_xml_bytes: u64,
    #[serde(default = "default_max_chunks")]
    pub max_chunks: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairLimits {
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
    pub max_metadata_chunk_bytes: u64,
    pub max_xml_bytes: u64,
    pub max_chunks: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepairAction {
    pub kind: &'static str,
    pub changed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IsobmffLoudnessEvidence {
    pub track_id: u32,
    pub codecs: Vec<String>,
    pub decoded_reference: String,
    pub decoded_reference_sha256: String,
    pub reference_is_source: bool,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: u64,
    pub decoded_samples: u64,
    pub max_decoded_samples: u64,
    pub measured_program_loudness_lufs: f64,
    pub encoded_program_loudness_lkfs: f64,
    pub program_quantization_error_lu: f64,
    pub measured_sample_peak_dbfs: f64,
    pub encoded_sample_peak_dbfs: f64,
    pub sample_peak_quantization_error_db: f64,
    pub measured_true_peak_dbtp: f64,
    pub encoded_true_peak_dbtp: f64,
    pub true_peak_quantization_error_db: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mdat_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_mdat_sha256: Option<String>,
    pub mdat_preserved: bool,
    pub replaced_existing_ludt: bool,
    pub moov_size_delta: i64,
    pub adjusted_chunk_offsets: u64,
    pub metadata_round_trip_passed: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IsobmffAlbumReferenceEvidence {
    pub path: String,
    pub sha256: String,
    pub track_reference: bool,
    pub input_bytes: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: u64,
    pub decoded_samples: u64,
    pub complete_gating_blocks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IsobmffAlbumLoudnessEvidence {
    pub references: Vec<IsobmffAlbumReferenceEvidence>,
    pub reference_count: u32,
    pub max_album_references: u32,
    pub total_reference_bytes: u64,
    pub total_decoded_samples: u64,
    pub max_decoded_samples: u64,
    pub complete_gating_blocks: u64,
    pub measured_program_loudness_lufs: f64,
    pub encoded_program_loudness_lkfs: f64,
    pub program_quantization_error_lu: f64,
    pub measured_sample_peak_dbfs: f64,
    pub encoded_sample_peak_dbfs: f64,
    pub sample_peak_quantization_error_db: f64,
    pub measured_true_peak_dbtp: f64,
    pub encoded_true_peak_dbtp: f64,
    pub true_peak_quantization_error_db: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetadataRepairReport {
    pub schema_version: u32,
    pub validator: &'static str,
    pub classification: &'static str,
    pub mode: RepairMode,
    pub source: String,
    pub destination: String,
    pub source_bytes: u64,
    pub output_bytes: u64,
    pub source_sha256: String,
    pub output_sha256: String,
    pub limits: RepairLimits,
    pub source_format: String,
    pub before: ContainerAudit,
    pub after: ContainerAudit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adm_before: Option<ProductionProfileResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adm_after: Option<ProductionProfileResult>,
    pub actions: Vec<RepairAction>,
    pub warnings: Vec<String>,
    pub passed: bool,
    pub changed: bool,
    pub unknown_bytes_preserved: bool,
    pub atomic_replace: bool,
}

/// Metadata-repair report with optional ISO-BMFF loudness evidence.
///
/// This additive wrapper keeps [`MetadataRepairReport`] source-compatible for
/// existing Rust callers while extending the command-line JSON contract.
#[derive(Debug, Clone, Serialize)]
pub struct ExtendedMetadataRepairReport {
    #[serde(flatten)]
    pub report: MetadataRepairReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isobmff_loudness: Option<IsobmffLoudnessEvidence>,
}

/// Schema-v2 ISO-BMFF evidence. The schema-v1 track evidence remains
/// byte-for-byte compatible and album evidence is added only in v2 output.
#[derive(Debug, Clone, Serialize)]
pub struct IsobmffLoudnessEvidenceV2 {
    #[serde(flatten)]
    pub track: IsobmffLoudnessEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_loudness: Option<IsobmffAlbumLoudnessEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtendedMetadataRepairReportV2 {
    #[serde(flatten)]
    pub report: MetadataRepairReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isobmff_loudness: Option<IsobmffLoudnessEvidenceV2>,
}

/// Versioned command-line report without changing the existing schema-v1 Rust
/// return type.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum VersionedMetadataRepairReport {
    V1(ExtendedMetadataRepairReport),
    V2(ExtendedMetadataRepairReportV2),
}

impl VersionedMetadataRepairReport {
    pub fn report(&self) -> &MetadataRepairReport {
        match self {
            Self::V1(value) => &value.report,
            Self::V2(value) => &value.report,
        }
    }
}

#[derive(Debug, Clone)]
struct WaveChunkInfo {
    id: [u8; 4],
    body_offset: u64,
    body_size: u64,
}

struct PreparedIsobmffLoudness {
    target: TargetTrack,
    reference: PathBuf,
    reference_snapshot: FileSnapshot,
    reference_is_source: bool,
    measured: DecodedLoudness,
    encoded: EncodedLoudness,
    album: Option<PreparedAlbumLoudness>,
    source_mdat_sha256: Option<String>,
    max_decoded_samples: u64,
}

struct PreparedAlbumLoudness {
    encoded: EncodedLoudness,
    evidence: IsobmffAlbumLoudnessEvidence,
    protected_inputs: Vec<FileSnapshot>,
}

fn protected_inputs<'a>(
    source: &'a FileSnapshot,
    prepared: Option<&'a PreparedIsobmffLoudness>,
) -> Vec<&'a FileSnapshot> {
    let mut identities = HashSet::new();
    let mut inputs = Vec::new();
    if identities.insert(source.identity.clone()) {
        inputs.push(source);
    }
    if let Some(prepared) = prepared {
        if identities.insert(prepared.reference_snapshot.identity.clone()) {
            inputs.push(&prepared.reference_snapshot);
        }
        if let Some(album) = &prepared.album {
            for input in &album.protected_inputs {
                if identities.insert(input.identity.clone()) {
                    inputs.push(input);
                }
            }
        }
    }
    inputs
}

#[derive(Debug, Clone)]
enum IsobmffOptions {
    V1(IsobmffLoudnessRepair),
    V2(IsobmffLoudnessRepairV2),
}

impl IsobmffOptions {
    fn decoded_reference(&self) -> Option<&PathBuf> {
        match self {
            Self::V1(value) => value.decoded_reference.as_ref(),
            Self::V2(value) => value.decoded_reference.as_ref(),
        }
    }

    fn max_decoded_samples(&self) -> u64 {
        match self {
            Self::V1(value) => value.max_decoded_samples,
            Self::V2(value) => value.max_decoded_samples,
        }
    }

    fn album_decoded_references(&self) -> Option<&[PathBuf]> {
        match self {
            Self::V1(_) => None,
            Self::V2(value) => value.album_decoded_references.as_deref(),
        }
    }

    fn max_album_references(&self) -> u32 {
        match self {
            Self::V1(_) => 1,
            Self::V2(value) => value.max_album_references,
        }
    }
}

struct InternalMetadataRepairReport {
    report: MetadataRepairReport,
    isobmff_loudness: Option<IsobmffLoudnessEvidence>,
    album_loudness: Option<IsobmffAlbumLoudnessEvidence>,
}

/// Read a JSON/TOML request and produce a bounded repair report.
pub fn evaluate_file(path: &Path) -> Result<MetadataRepairReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read metadata repair request {}: {error}", path.display()))?;
    let spec: MetadataRepairSpec = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse metadata repair JSON: {error}"))?,
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("parse metadata repair TOML: {error}"))?
        }
        _ => return Err("metadata repair request must use .json or .toml".into()),
    };
    evaluate(path, spec)
}

/// Read a JSON/TOML request, including the additive `isobmff_loudness`
/// operation used by the command-line tool.
pub fn evaluate_extended_file(path: &Path) -> Result<ExtendedMetadataRepairReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read metadata repair request {}: {error}", path.display()))?;
    let (spec, isobmff_loudness) = parse_extended_spec(path, &text)?;
    let options = isobmff_loudness.map(IsobmffOptions::V1);
    into_v1_report(evaluate_internal(path, spec, options)?)
}

/// Read a schema-v1 or schema-v2 request and retain the matching report
/// contract. The CLI uses this entry point so v1 consumers remain unchanged.
pub fn evaluate_versioned_file(path: &Path) -> Result<VersionedMetadataRepairReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read metadata repair request {}: {error}", path.display()))?;
    let (spec, isobmff_loudness) = parse_versioned_spec(path, &text)?;
    let schema_version = spec.schema_version;
    let report = evaluate_internal(path, spec, isobmff_loudness)?;
    match schema_version {
        SCHEMA_VERSION => into_v1_report(report).map(VersionedMetadataRepairReport::V1),
        SCHEMA_VERSION_V2 => Ok(VersionedMetadataRepairReport::V2(into_v2_report(report))),
        _ => unreachable!("validated schema version"),
    }
}

/// Evaluate one request.  The destination is always a separate path; this is
/// an intentional guard against accidental in-place audio replacement.
pub fn evaluate(
    request_path: &Path,
    spec: MetadataRepairSpec,
) -> Result<MetadataRepairReport, String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "evaluate supports schema_version {SCHEMA_VERSION}; use evaluate_versioned_file for schema_version {}",
            spec.schema_version
        ));
    }
    evaluate_internal(request_path, spec, None).map(|internal| internal.report)
}

/// Evaluate one ISO-BMFF loudness repair without extending the shape of the
/// stable [`MetadataRepairSpec`] or [`MetadataRepairReport`] Rust types.
pub fn evaluate_isobmff_loudness(
    request_path: &Path,
    spec: MetadataRepairSpec,
    options: IsobmffLoudnessRepair,
) -> Result<ExtendedMetadataRepairReport, String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "evaluate_isobmff_loudness supports schema_version {SCHEMA_VERSION}; use the schema-v2 API for schema_version {}",
            spec.schema_version
        ));
    }
    into_v1_report(evaluate_internal(
        request_path,
        spec,
        Some(IsobmffOptions::V1(options)),
    )?)
}

/// Evaluate schema-v2 track and optional album ISO-BMFF loudness repair.
pub fn evaluate_isobmff_loudness_v2(
    request_path: &Path,
    spec: MetadataRepairSpec,
    options: IsobmffLoudnessRepairV2,
) -> Result<ExtendedMetadataRepairReportV2, String> {
    if spec.schema_version != SCHEMA_VERSION_V2 {
        return Err(format!(
            "evaluate_isobmff_loudness_v2 requires schema_version {SCHEMA_VERSION_V2}; found {}",
            spec.schema_version
        ));
    }
    evaluate_internal(request_path, spec, Some(IsobmffOptions::V2(options))).map(into_v2_report)
}

fn evaluate_internal(
    request_path: &Path,
    spec: MetadataRepairSpec,
    isobmff_options: Option<IsobmffOptions>,
) -> Result<InternalMetadataRepairReport, String> {
    validate_spec(&spec, isobmff_options.as_ref())?;
    let base = request_path.parent().unwrap_or_else(|| Path::new("."));
    let source = resolve(base, &spec.source);
    let destination = resolve(base, &spec.destination);
    let source_snapshot = FileSnapshot::capture(&source, spec.max_input_bytes, "metadata source")?;
    let source_bytes = source_snapshot.len;
    reject_protected_destination(&destination, &[&source_snapshot])?;
    if destination.exists() && !spec.overwrite {
        return Err(format!(
            "metadata repair destination already exists: {} (pass overwrite=true)",
            destination.display()
        ));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(format!(
            "metadata repair destination directory does not exist: {}",
            parent.display()
        ));
    }

    source_snapshot.verify(
        spec.max_input_bytes,
        "metadata source changed before initial audit",
    )?;
    let before = container_qc::audit(source_snapshot.stable_path())?;
    source_snapshot.verify(
        spec.max_input_bytes,
        "metadata source changed during initial audit",
    )?;
    let source_format = before.format.clone();
    let adm_before = read_adm_profile(source_snapshot.stable_path(), &source_format)?;
    source_snapshot.verify(
        spec.max_input_bytes,
        "metadata source changed while reading its ADM profile",
    )?;
    let has_wave_mutation =
        spec.ensure_bwf_v2 || spec.bwf_loudness.is_some() || spec.adm_version.is_some();
    let has_isobmff_mutation = isobmff_options.is_some();
    let has_mutation = has_wave_mutation || has_isobmff_mutation;
    if has_mutation && spec.mode == RepairMode::Validate {
        return Err("mode=validate cannot request metadata mutations".into());
    }
    if has_wave_mutation && has_isobmff_mutation {
        return Err(
            "one metadata repair request cannot combine WAVE/ADM and ISO-BMFF mutations".into(),
        );
    }
    if has_wave_mutation && source_format != "wave" {
        return Err(format!(
            "{} WAVE/BWF metadata mutation is unsupported; MXF and other containers are validate-and-copy only",
            source_format
        ));
    }
    if has_isobmff_mutation && source_format != "isobmff" {
        return Err(format!(
            "{} does not support ISO-BMFF ludt metadata mutation",
            source_format
        ));
    }

    let prepared_isobmff = if let Some(options) = &isobmff_options {
        Some(prepare_isobmff_loudness(
            base,
            &source_snapshot,
            &before,
            &spec,
            options,
        )?)
    } else {
        None
    };

    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let mut isobmff_rewrite = None;
    let changed = {
        let protected = protected_inputs(&source_snapshot, prepared_isobmff.as_ref());
        reject_protected_destination(&destination, &protected)?;
        verify_protected_inputs(
            &protected,
            spec.max_input_bytes,
            "protected metadata input changed before output",
        )?;
        if has_wave_mutation {
            let result = write_output(
                &destination,
                spec.overwrite,
                spec.atomic_replace,
                spec.max_output_bytes,
                &protected,
                spec.max_input_bytes,
                |output| {
                    rewrite_wave(
                        source_snapshot.stable_path(),
                        output,
                        &spec,
                        &mut actions,
                        &mut warnings,
                    )
                },
            )?;
            let _ = result;
            actions.iter().any(|action| action.changed)
        } else if let Some(prepared) = &prepared_isobmff {
            write_output(
                &destination,
                spec.overwrite,
                spec.atomic_replace,
                spec.max_output_bytes,
                &protected,
                spec.max_input_bytes,
                |output| {
                    let result = isobmff_loudness_repair::rewrite(
                        source_snapshot.stable_path(),
                        output,
                        prepared.target.track_id,
                        &prepared.encoded,
                        prepared.album.as_ref().map(|album| &album.encoded),
                        isobmff_loudness_repair::RewriteLimits {
                            max_input_bytes: spec.max_input_bytes,
                            max_moov_bytes: spec.max_metadata_chunk_bytes,
                            max_boxes: spec.max_chunks,
                        },
                    )?;
                    let bytes = result.bytes_written;
                    isobmff_rewrite = Some(result);
                    Ok(bytes)
                },
            )?;
            let result = isobmff_rewrite
                .as_ref()
                .expect("ISO-BMFF write closure records rewrite evidence");
            actions.push(RepairAction {
                kind: "isobmff-loudness",
                changed: result.changed,
                detail: format!(
                    "{} ISO-BMFF {} for audio track {}; moov delta {:+} byte(s), adjusted {} chunk offset(s)",
                    if result.replaced_existing { "replaced" } else { "inserted" },
                    if prepared.album.is_some() { "ludt/tlou+alou" } else { "ludt/tlou" },
                    prepared.target.track_id,
                    result.moov_size_delta,
                    result.adjusted_chunk_offsets
                ),
            });
            result.changed
        } else {
            write_output(
                &destination,
                spec.overwrite,
                spec.atomic_replace,
                spec.max_output_bytes,
                &protected,
                spec.max_input_bytes,
                |output| copy_bounded(source_snapshot.stable_path(), output, spec.max_input_bytes),
            )?;
            actions.push(RepairAction {
                kind: if spec.mode == RepairMode::Validate {
                    "validate-and-copy"
                } else {
                    "copy-without-mutation"
                },
                changed: false,
                detail: "source bytes were copied without metadata mutation".into(),
            });
            if source_format == "mxf" {
                warnings.push(
                    "MXF KLV metadata and partition/index tables are preserved byte-for-byte; mutation is intentionally not attempted".into(),
                );
            }
            false
        }
    };

    verify_protected_inputs(
        &protected_inputs(&source_snapshot, prepared_isobmff.as_ref()),
        spec.max_input_bytes,
        "protected metadata input changed after output commit",
    )?;

    let output_bytes = fs::metadata(&destination)
        .map_err(|error| {
            format!(
                "stat metadata repair output {}: {error}",
                destination.display()
            )
        })?
        .len();
    if output_bytes > spec.max_output_bytes {
        return Err(format!(
            "metadata repair output is {output_bytes} bytes, above max_output_bytes {}",
            spec.max_output_bytes
        ));
    }
    let after = container_qc::audit(&destination)?;
    let adm_after = read_adm_profile(&destination, &after.format)?;
    verify_protected_inputs(
        &protected_inputs(&source_snapshot, prepared_isobmff.as_ref()),
        spec.max_input_bytes,
        "protected metadata input changed during output verification",
    )?;
    let (isobmff_loudness, album_loudness) = if let Some(prepared) = prepared_isobmff {
        let rewrite =
            isobmff_rewrite.expect("prepared ISO-BMFF loudness mutation has rewrite evidence");
        let output_mdat_sha256 = isobmff_loudness_repair::mdat_sha256(
            &destination,
            spec.max_output_bytes,
            spec.max_chunks,
        )?;
        let mdat_preserved = prepared.source_mdat_sha256 == output_mdat_sha256;
        let metadata_round_trip_passed = isobmff_loudness_repair::verify_round_trip(
            &after,
            prepared.target.track_id,
            &prepared.encoded,
            prepared.album.as_ref().map(|album| &album.encoded),
        );
        let passed = mdat_preserved && metadata_round_trip_passed;
        if !mdat_preserved {
            warnings.push(
                "post-repair mdat payload hash differs from the source; do not use the output"
                    .into(),
            );
        }
        if !metadata_round_trip_passed {
            warnings.push(
                "post-repair ISO-BMFF loudness metadata did not round-trip exactly; do not use the output"
                    .into(),
            );
        }
        prepared.reference_snapshot.verify(
            spec.max_input_bytes,
            "decoded reference changed before final evidence",
        )?;
        if let Some(album) = &prepared.album {
            for input in &album.protected_inputs {
                input.verify(
                    spec.max_input_bytes,
                    "album decoded reference changed before final evidence",
                )?;
            }
        }
        let evidence = IsobmffLoudnessEvidence {
            track_id: prepared.target.track_id,
            codecs: prepared.target.codecs,
            decoded_reference: prepared.reference.to_string_lossy().into_owned(),
            decoded_reference_sha256: prepared.reference_snapshot.sha256,
            reference_is_source: prepared.reference_is_source,
            sample_rate_hz: prepared.measured.sample_rate_hz,
            channels: prepared.measured.channels,
            frames: prepared.measured.frames,
            decoded_samples: prepared.measured.decoded_samples,
            max_decoded_samples: prepared.max_decoded_samples,
            measured_program_loudness_lufs: prepared.measured.integrated_lufs,
            encoded_program_loudness_lkfs: prepared.encoded.program_loudness_lkfs,
            program_quantization_error_lu: prepared.encoded.program_loudness_lkfs
                - prepared.measured.integrated_lufs,
            measured_sample_peak_dbfs: prepared.measured.sample_peak_dbfs,
            encoded_sample_peak_dbfs: prepared.encoded.sample_peak_dbfs,
            sample_peak_quantization_error_db: prepared.encoded.sample_peak_dbfs
                - prepared.measured.sample_peak_dbfs,
            measured_true_peak_dbtp: prepared.measured.true_peak_dbtp,
            encoded_true_peak_dbtp: prepared.encoded.true_peak_dbtp,
            true_peak_quantization_error_db: prepared.encoded.true_peak_dbtp
                - prepared.measured.true_peak_dbtp,
            source_mdat_sha256: prepared.source_mdat_sha256,
            output_mdat_sha256,
            mdat_preserved,
            replaced_existing_ludt: rewrite.replaced_existing,
            moov_size_delta: rewrite.moov_size_delta,
            adjusted_chunk_offsets: rewrite.adjusted_chunk_offsets,
            metadata_round_trip_passed,
            passed,
        };
        (Some(evidence), prepared.album.map(|album| album.evidence))
    } else {
        (None, None)
    };
    let passed = before.passed
        && after.passed
        && adm_before.as_ref().is_none_or(|profile| profile.passed)
        && adm_after.as_ref().is_none_or(|profile| profile.passed)
        && isobmff_loudness
            .as_ref()
            .is_none_or(|evidence| evidence.passed);
    if !after.passed {
        warnings.push("post-repair container QC failed; do not deliver the output".into());
    }
    if changed && source_format == "wave" {
        warnings.push(
            "audio data and unknown WAVE chunks were copied byte-for-byte; only explicitly requested metadata fields were changed".into(),
        );
    }
    if changed && source_format == "isobmff" {
        warnings.push(
            "ISO-BMFF media payloads were hash-verified byte-for-byte; only ludt loudness metadata, ancestor sizes, and required stco/co64 offsets changed".into(),
        );
    }
    let output_sha256 = sha256_file(&destination, spec.max_output_bytes)?;
    source_snapshot.verify(
        spec.max_input_bytes,
        "metadata source changed before final report",
    )?;
    let report = MetadataRepairReport {
        schema_version: spec.schema_version,
        validator: if spec.schema_version == SCHEMA_VERSION_V2 {
            VALIDATOR_V2
        } else {
            VALIDATOR
        },
        classification: "bounded metadata repair; delivery requires post-repair QC review",
        mode: spec.mode,
        source: source.to_string_lossy().into_owned(),
        destination: destination.to_string_lossy().into_owned(),
        source_bytes,
        output_bytes,
        source_sha256: source_snapshot.sha256,
        output_sha256,
        limits: RepairLimits {
            max_input_bytes: spec.max_input_bytes,
            max_output_bytes: spec.max_output_bytes,
            max_metadata_chunk_bytes: spec.max_metadata_chunk_bytes,
            max_xml_bytes: spec.max_xml_bytes,
            max_chunks: spec.max_chunks,
        },
        source_format,
        before,
        after,
        adm_before,
        adm_after,
        actions,
        warnings,
        passed,
        changed,
        unknown_bytes_preserved: true,
        atomic_replace: spec.atomic_replace,
    };
    Ok(InternalMetadataRepairReport {
        report,
        isobmff_loudness,
        album_loudness,
    })
}

fn into_v1_report(
    report: InternalMetadataRepairReport,
) -> Result<ExtendedMetadataRepairReport, String> {
    if report.album_loudness.is_some() {
        return Err("schema_version 1 cannot emit album loudness evidence".into());
    }
    Ok(ExtendedMetadataRepairReport {
        report: report.report,
        isobmff_loudness: report.isobmff_loudness,
    })
}

fn into_v2_report(report: InternalMetadataRepairReport) -> ExtendedMetadataRepairReportV2 {
    let album_loudness = report.album_loudness;
    ExtendedMetadataRepairReportV2 {
        report: report.report,
        isobmff_loudness: report
            .isobmff_loudness
            .map(|track| IsobmffLoudnessEvidenceV2 {
                track,
                album_loudness,
            }),
    }
}

fn prepare_isobmff_loudness(
    base: &Path,
    source: &FileSnapshot,
    before: &ContainerAudit,
    spec: &MetadataRepairSpec,
    options: &IsobmffOptions,
) -> Result<PreparedIsobmffLoudness, String> {
    let target = isobmff_loudness_repair::select_target(before)?;
    let reference = options
        .decoded_reference()
        .map(|path| resolve(base, path))
        .unwrap_or_else(|| source.path.clone());
    let reference_snapshot = FileSnapshot::capture(
        &reference,
        spec.max_input_bytes,
        "ISO-BMFF decoded reference",
    )?;
    let reference_is_source = reference_snapshot.identity == source.identity;
    reference_snapshot.verify(
        spec.max_input_bytes,
        "decoded reference changed before measurement",
    )?;
    let analyzed = isobmff_loudness_repair::analyze_reference(
        reference_snapshot.stable_path(),
        options.max_decoded_samples(),
    )?;
    reference_snapshot.verify(
        spec.max_input_bytes,
        "decoded reference changed while it was measured",
    )?;
    isobmff_loudness_repair::validate_encodable_measurement(&reference, &analyzed.loudness)?;
    isobmff_loudness_repair::validate_reference_geometry(&target, &analyzed.loudness)?;
    let measured = analyzed.loudness.clone();
    let encoded = isobmff_loudness_repair::encode_measurement(&measured)?;
    let album = if let Some(album_references) = options.album_decoded_references() {
        Some(prepare_album_loudness(
            base,
            &reference_snapshot,
            analyzed,
            album_references,
            options.max_album_references(),
            options.max_decoded_samples(),
            spec.max_input_bytes,
        )?)
    } else {
        None
    };
    source.verify(
        spec.max_input_bytes,
        "metadata source changed before hashing media payloads",
    )?;
    let source_mdat_sha256 = isobmff_loudness_repair::mdat_sha256(
        source.stable_path(),
        spec.max_input_bytes,
        spec.max_chunks,
    )?;
    source.verify(
        spec.max_input_bytes,
        "metadata source changed while hashing media payloads",
    )?;
    Ok(PreparedIsobmffLoudness {
        target,
        reference,
        reference_snapshot,
        reference_is_source,
        measured,
        encoded,
        album,
        source_mdat_sha256,
        max_decoded_samples: options.max_decoded_samples(),
    })
}

#[allow(clippy::too_many_arguments)]
fn prepare_album_loudness(
    base: &Path,
    track_snapshot: &FileSnapshot,
    track_analysis: isobmff_loudness_repair::ReferenceMeasurement,
    album_references: &[PathBuf],
    max_album_references: u32,
    max_decoded_samples: u64,
    max_input_bytes: u64,
) -> Result<PreparedAlbumLoudness, String> {
    let mut seen = HashSet::with_capacity(album_references.len());
    let mut resolved = Vec::with_capacity(album_references.len());
    let mut total_reference_bytes = 0_u64;
    let mut track_matches = 0_u32;
    for path in album_references {
        let path = resolve(base, path);
        // Pass the remaining aggregate budget into capture. Its opened-handle
        // metadata check runs before creating/copying a private snapshot, so a
        // long reference list cannot consume references * max_input_bytes of
        // temporary storage before the aggregate check fires.
        let remaining_reference_bytes = max_input_bytes
            .checked_sub(total_reference_bytes)
            .ok_or("album decoded references exceed aggregate max_input_bytes")?;
        let snapshot = FileSnapshot::bind(
            &path,
            remaining_reference_bytes,
            "album decoded reference (remaining aggregate budget)",
        )?;
        if !seen.insert(snapshot.identity.clone()) {
            return Err(format!(
                "album_decoded_references contains duplicate file {}",
                path.display()
            ));
        }
        total_reference_bytes = total_reference_bytes
            .checked_add(snapshot.len)
            .ok_or("album decoded reference byte count overflow")?;
        let is_track = snapshot.identity == track_snapshot.identity;
        let snapshot = if is_track {
            snapshot
        } else {
            snapshot.snapshot_contents(
                remaining_reference_bytes,
                "album decoded reference (remaining aggregate budget)",
            )?
        };
        track_matches += u32::from(is_track);
        resolved.push((path, snapshot, is_track));
    }
    if track_matches != 1 {
        return Err(format!(
            "album_decoded_references must contain the selected track decoded reference exactly once; found {track_matches}"
        ));
    }
    debug_assert!(total_reference_bytes <= max_input_bytes);

    let mut total_decoded_samples = track_analysis.loudness.decoded_samples;
    let mut track_analysis = Some(track_analysis);
    let mut gating_blocks = Vec::new();
    let mut sample_peak_dbfs = f64::NEG_INFINITY;
    let mut true_peak_dbtp = f64::NEG_INFINITY;
    let mut evidence = Vec::with_capacity(resolved.len());
    let mut protected_inputs = Vec::with_capacity(resolved.len());
    for (path, snapshot, is_track) in resolved {
        snapshot.verify(
            max_input_bytes,
            "album decoded reference changed before measurement",
        )?;
        if is_track && snapshot.sha256 != track_snapshot.sha256 {
            return Err(format!(
                "selected track decoded reference changed before album measurement: {}",
                path.display()
            ));
        }
        let analysis = if is_track {
            track_analysis
                .take()
                .expect("validated album list contains the track reference once")
        } else {
            let remaining = max_decoded_samples
                .checked_sub(total_decoded_samples)
                .filter(|remaining| *remaining != 0)
                .ok_or_else(|| {
                    format!(
                        "album decoded references exceed aggregate max_decoded_samples ({max_decoded_samples})"
                    )
                })?;
            isobmff_loudness_repair::analyze_reference(snapshot.stable_path(), remaining).map_err(
                |error| {
                    format!(
                        "album decoded references exceed aggregate max_decoded_samples ({max_decoded_samples}) or could not be measured: {error}"
                    )
                },
            )?
        };
        snapshot.verify(
            max_input_bytes,
            "album decoded reference changed while it was measured",
        )?;
        if !is_track {
            total_decoded_samples = total_decoded_samples
                .checked_add(analysis.loudness.decoded_samples)
                .ok_or("album decoded sample count overflow")?;
        }
        if total_decoded_samples > max_decoded_samples {
            return Err(format!(
                "album decoded references total {total_decoded_samples} samples, above aggregate max_decoded_samples {max_decoded_samples}"
            ));
        }
        let block_count = analysis.gating_blocks.len();
        let combined_blocks = gating_blocks
            .len()
            .checked_add(block_count)
            .ok_or("album gating block count overflow")?;
        if combined_blocks > crate::dsp::lufs::MAX_LOUDNESS_BLOCKS {
            return Err(format!(
                "album loudness exceeds the {} complete-gating-block limit",
                crate::dsp::lufs::MAX_LOUDNESS_BLOCKS
            ));
        }
        sample_peak_dbfs = sample_peak_dbfs.max(analysis.loudness.sample_peak_dbfs);
        true_peak_dbtp = true_peak_dbtp.max(analysis.loudness.true_peak_dbtp);
        gating_blocks.extend(analysis.gating_blocks);
        evidence.push(IsobmffAlbumReferenceEvidence {
            path: path.to_string_lossy().into_owned(),
            sha256: snapshot.sha256.clone(),
            track_reference: is_track,
            input_bytes: if is_track {
                track_snapshot.len
            } else {
                snapshot.len
            },
            sample_rate_hz: analysis.loudness.sample_rate_hz,
            channels: analysis.loudness.channels,
            frames: analysis.loudness.frames,
            decoded_samples: analysis.loudness.decoded_samples,
            complete_gating_blocks: u64::try_from(block_count)
                .map_err(|_| "album gating block count exceeds u64")?,
        });
        protected_inputs.push(snapshot);
    }
    let integrated_lufs = crate::dsp::lufs::gated_lufs(&gating_blocks);
    if !integrated_lufs.is_finite() || !sample_peak_dbfs.is_finite() || !true_peak_dbtp.is_finite()
    {
        return Err(
            "album decoded references do not contain finite loudness and peak measurements".into(),
        );
    }
    let encoded =
        isobmff_loudness_repair::encode_values(integrated_lufs, sample_peak_dbfs, true_peak_dbtp)?;
    let complete_gating_blocks =
        u64::try_from(gating_blocks.len()).map_err(|_| "album gating block count exceeds u64")?;
    let reference_count =
        u32::try_from(evidence.len()).map_err(|_| "album reference count exceeds u32")?;
    debug_assert!(reference_count <= max_album_references);
    Ok(PreparedAlbumLoudness {
        evidence: IsobmffAlbumLoudnessEvidence {
            references: evidence,
            reference_count,
            max_album_references,
            total_reference_bytes,
            total_decoded_samples,
            max_decoded_samples,
            complete_gating_blocks,
            measured_program_loudness_lufs: integrated_lufs,
            encoded_program_loudness_lkfs: encoded.program_loudness_lkfs,
            program_quantization_error_lu: encoded.program_loudness_lkfs - integrated_lufs,
            measured_sample_peak_dbfs: sample_peak_dbfs,
            encoded_sample_peak_dbfs: encoded.sample_peak_dbfs,
            sample_peak_quantization_error_db: encoded.sample_peak_dbfs - sample_peak_dbfs,
            measured_true_peak_dbtp: true_peak_dbtp,
            encoded_true_peak_dbtp: encoded.true_peak_dbtp,
            true_peak_quantization_error_db: encoded.true_peak_dbtp - true_peak_dbtp,
        },
        encoded,
        protected_inputs,
    })
}

fn parse_extended_spec(
    path: &Path,
    text: &str,
) -> Result<(MetadataRepairSpec, Option<IsobmffLoudnessRepair>), String> {
    let (spec, options) = parse_versioned_spec(path, text)?;
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "evaluate_extended_file supports schema_version {SCHEMA_VERSION}; found {}",
            spec.schema_version
        ));
    }
    let options = match options {
        None => None,
        Some(IsobmffOptions::V1(value)) => Some(value),
        Some(IsobmffOptions::V2(_)) => unreachable!("schema-v1 parser selected v1 options"),
    };
    Ok((spec, options))
}

fn parse_versioned_spec(
    path: &Path,
    text: &str,
) -> Result<(MetadataRepairSpec, Option<IsobmffOptions>), String> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => {
            let mut value: serde_json::Value = serde_json::from_str(text)
                .map_err(|error| format!("parse metadata repair JSON: {error}"))?;
            let object = value
                .as_object_mut()
                .ok_or("metadata repair JSON request must be an object")?;
            let schema_version = object
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u32::try_from(version).ok())
                .ok_or("metadata repair JSON schema_version must be an unsigned integer")?;
            let raw_options = object.remove("isobmff_loudness");
            let isobmff_loudness = match (schema_version, raw_options) {
                (_, None) => None,
                (SCHEMA_VERSION, Some(raw)) => serde_json::from_value::<
                    Option<IsobmffLoudnessRepair>,
                >(raw)
                .map(|value| value.map(IsobmffOptions::V1))
                .map_err(|error| format!("parse metadata repair JSON isobmff_loudness: {error}"))?,
                (SCHEMA_VERSION_V2, Some(raw)) => serde_json::from_value::<
                    Option<IsobmffLoudnessRepairV2>,
                >(raw)
                .map(|value| value.map(IsobmffOptions::V2))
                .map_err(|error| format!("parse metadata repair JSON isobmff_loudness: {error}"))?,
                (version, Some(_)) => {
                    return Err(format!(
                        "unsupported metadata repair schema_version {version}; expected {SCHEMA_VERSION} or {SCHEMA_VERSION_V2}"
                    ));
                }
            };
            let spec = serde_json::from_value(value)
                .map_err(|error| format!("parse metadata repair JSON: {error}"))?;
            Ok((spec, isobmff_loudness))
        }
        Some("toml") => {
            let mut value: toml::Value = toml::from_str(text)
                .map_err(|error| format!("parse metadata repair TOML: {error}"))?;
            let table = value
                .as_table_mut()
                .ok_or("metadata repair TOML request must be a table")?;
            let schema_version = table
                .get("schema_version")
                .and_then(toml::Value::as_integer)
                .and_then(|version| u32::try_from(version).ok())
                .ok_or("metadata repair TOML schema_version must be an unsigned integer")?;
            let raw_options = table.remove("isobmff_loudness");
            let isobmff_loudness = match (schema_version, raw_options) {
                (_, None) => None,
                (SCHEMA_VERSION, Some(raw)) => Some(IsobmffOptions::V1(
                    raw.try_into::<IsobmffLoudnessRepair>().map_err(|error| {
                        format!("parse metadata repair TOML isobmff_loudness: {error}")
                    })?,
                )),
                (SCHEMA_VERSION_V2, Some(raw)) => Some(IsobmffOptions::V2(
                    raw.try_into::<IsobmffLoudnessRepairV2>().map_err(|error| {
                        format!("parse metadata repair TOML isobmff_loudness: {error}")
                    })?,
                )),
                (version, Some(_)) => {
                    return Err(format!(
                        "unsupported metadata repair schema_version {version}; expected {SCHEMA_VERSION} or {SCHEMA_VERSION_V2}"
                    ));
                }
            };
            let spec = value
                .try_into::<MetadataRepairSpec>()
                .map_err(|error| format!("parse metadata repair TOML: {error}"))?;
            Ok((spec, isobmff_loudness))
        }
        _ => Err("metadata repair request must use .json or .toml".into()),
    }
}

fn validate_spec(
    spec: &MetadataRepairSpec,
    isobmff_options: Option<&IsobmffOptions>,
) -> Result<(), String> {
    if !matches!(spec.schema_version, SCHEMA_VERSION | SCHEMA_VERSION_V2) {
        return Err(format!(
            "unsupported metadata repair schema_version {}; expected {SCHEMA_VERSION} or {SCHEMA_VERSION_V2}",
            spec.schema_version
        ));
    }
    if matches!(isobmff_options, Some(IsobmffOptions::V1(_)))
        && spec.schema_version != SCHEMA_VERSION
        || matches!(isobmff_options, Some(IsobmffOptions::V2(_)))
            && spec.schema_version != SCHEMA_VERSION_V2
    {
        return Err(
            "metadata repair schema_version does not match isobmff_loudness contract".into(),
        );
    }
    if spec.source.as_os_str().is_empty() || spec.destination.as_os_str().is_empty() {
        return Err("metadata repair source and destination must not be empty".into());
    }
    for (name, value) in [
        ("max_input_bytes", spec.max_input_bytes),
        ("max_output_bytes", spec.max_output_bytes),
        ("max_metadata_chunk_bytes", spec.max_metadata_chunk_bytes),
        ("max_xml_bytes", spec.max_xml_bytes),
    ] {
        if value == 0 {
            return Err(format!("metadata repair {name} must be positive"));
        }
    }
    if spec.max_chunks == 0 {
        return Err("metadata repair max_chunks must be positive".into());
    }
    if let Some(options) = isobmff_options {
        if options.max_decoded_samples() == 0
            || options.max_decoded_samples() > HARD_MAX_DECODED_SAMPLES
        {
            return Err(format!(
                "metadata repair isobmff_loudness.max_decoded_samples must be 1..={HARD_MAX_DECODED_SAMPLES}"
            ));
        }
        if options
            .decoded_reference()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(
                "metadata repair isobmff_loudness.decoded_reference must not be empty".into(),
            );
        }
        if let IsobmffOptions::V2(options) = options {
            if options.max_album_references == 0
                || options.max_album_references > HARD_MAX_ALBUM_REFERENCES
            {
                return Err(format!(
                    "metadata repair isobmff_loudness.max_album_references must be 1..={HARD_MAX_ALBUM_REFERENCES}"
                ));
            }
            if let Some(references) = &options.album_decoded_references {
                if references.is_empty() {
                    return Err(
                        "metadata repair isobmff_loudness.album_decoded_references must not be empty when present".into(),
                    );
                }
                let count = u32::try_from(references.len())
                    .map_err(|_| "album decoded reference count exceeds u32")?;
                if count > options.max_album_references {
                    return Err(format!(
                        "metadata repair album_decoded_references has {count} entries, above max_album_references {}",
                        options.max_album_references
                    ));
                }
                if references.iter().any(|path| path.as_os_str().is_empty()) {
                    return Err(
                        "metadata repair album_decoded_references paths must not be empty".into(),
                    );
                }
            }
        }
    }
    if (spec.ensure_bwf_v2 || spec.bwf_loudness.is_some()) && spec.max_metadata_chunk_bytes < 602 {
        return Err(
            "metadata repair max_metadata_chunk_bytes must be at least 602 for BWF v2".into(),
        );
    }
    if let Some(loudness) = &spec.bwf_loudness {
        for (name, value) in [
            ("integrated_lufs", loudness.integrated_lufs),
            ("true_peak_dbtp", loudness.true_peak_dbtp),
            ("max_momentary_lufs", loudness.max_momentary_lufs),
            ("max_short_term_lufs", loudness.max_short_term_lufs),
        ] {
            if !value.is_finite() || !(..=6.0).contains(&value) || value < -120.0 {
                return Err(format!(
                    "metadata repair {name} is outside the supported range -120..=6"
                ));
            }
        }
        if !loudness.loudness_range_lu.is_finite()
            || !(0.0..=50.0).contains(&loudness.loudness_range_lu)
        {
            return Err(
                "metadata repair loudness_range_lu is outside the supported range 0..=50".into(),
            );
        }
    }
    if let Some(version) = &spec.adm_version {
        if version != adm::ADM_VERSION {
            return Err(format!(
                "metadata repair adm_version must be {}",
                adm::ADM_VERSION
            ));
        }
    }
    Ok(())
}

fn default_max_input_bytes() -> u64 {
    DEFAULT_MAX_INPUT_BYTES
}

fn default_max_output_bytes() -> u64 {
    DEFAULT_MAX_OUTPUT_BYTES
}

fn default_max_metadata_chunk_bytes() -> u64 {
    DEFAULT_MAX_METADATA_CHUNK_BYTES
}

fn default_max_xml_bytes() -> u64 {
    DEFAULT_MAX_XML_BYTES
}

fn default_max_chunks() -> u32 {
    DEFAULT_MAX_CHUNKS
}

fn default_max_decoded_samples() -> u64 {
    DEFAULT_MAX_DECODED_SAMPLES
}

fn default_max_album_references() -> u32 {
    DEFAULT_MAX_ALBUM_REFERENCES
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn verify_protected_inputs(
    protected: &[&FileSnapshot],
    max_input_bytes: u64,
    context: &str,
) -> Result<(), String> {
    for input in protected {
        input.verify(max_input_bytes, context)?;
    }
    Ok(())
}

fn opened_file_identity(file: &File, path: &Path) -> Result<FileIdentity, String> {
    identity_from_open_file(file, path).map_err(|error| error.to_string())
}

fn reject_open_protected_destination(
    destination: &Path,
    file: &File,
    protected: &[&FileSnapshot],
) -> Result<(), String> {
    let identity = opened_file_identity(file, destination)?;
    if protected.iter().any(|input| input.identity == identity) {
        return Err(format!(
            "metadata repair destination resolves to a protected source or decoded reference: {}",
            destination.display()
        ));
    }
    Ok(())
}

fn reject_protected_destination(
    destination: &Path,
    protected: &[&FileSnapshot],
) -> Result<(), String> {
    let file = match File::open(destination) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "open metadata repair destination {}: {error}",
                destination.display()
            ))
        }
    };
    reject_open_protected_destination(destination, &file, protected)
}

fn open_non_atomic_destination(destination: &Path, overwrite: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    if !overwrite {
        options.create_new(true);
    }
    // Opening without truncation lets us bind checks to the actual handle.
    // O_NOFOLLOW closes the final-component symlink exchange window on Unix.
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    // FILE_FLAG_OPEN_REPARSE_POINT opens the link itself rather than its
    // target; the opened-handle attribute check below then rejects it.
    #[cfg(windows)]
    options.custom_flags(0x0020_0000);

    let output = options.open(destination).map_err(|error| {
        format!(
            "create metadata repair output {}: {error}",
            destination.display()
        )
    })?;
    let metadata = output.metadata().map_err(|error| {
        format!(
            "stat opened metadata repair output {}: {error}",
            destination.display()
        )
    })?;
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(format!(
            "metadata repair destination must not be a reparse point: {}",
            destination.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "metadata repair destination is not a regular file: {}",
            destination.display()
        ));
    }
    Ok(output)
}

fn reject_multiply_linked_destination(destination: &Path, file: &File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "stat opened metadata repair output {}: {error}",
                destination.display()
            )
        })?;
        if metadata.nlink() > 1 {
            return Err(format!(
                "non-atomic metadata repair refuses a multiply-linked destination: {}",
                destination.display()
            ));
        }
    }
    #[cfg(windows)]
    if windows_file_information(file)?.nNumberOfLinks > 1 {
        return Err(format!(
            "non-atomic metadata repair refuses a multiply-linked destination: {}",
            destination.display()
        ));
    }
    Ok(())
}

fn verify_non_atomic_destination_binding(
    destination: &Path,
    output: &File,
    protected: &[&FileSnapshot],
) -> Result<(), String> {
    let opened_identity = opened_file_identity(output, destination)?;
    let path_metadata = fs::symlink_metadata(destination).map_err(|error| {
        format!(
            "restat metadata repair destination {}: {error}",
            destination.display()
        )
    })?;
    if path_metadata.file_type().is_symlink() {
        return Err(format!(
            "metadata repair destination changed to a symbolic link while writing: {}",
            destination.display()
        ));
    }
    let current = File::open(destination).map_err(|error| {
        format!(
            "reopen metadata repair destination {}: {error}",
            destination.display()
        )
    })?;
    reject_open_protected_destination(destination, &current, protected)?;
    if opened_file_identity(&current, destination)? != opened_identity {
        return Err(format!(
            "metadata repair destination changed while it was being written: {}",
            destination.display()
        ));
    }
    Ok(())
}

fn read_adm_profile(path: &Path, format: &str) -> Result<Option<ProductionProfileResult>, String> {
    if format != "wave" || metadata::read_wave_chunk(path, *b"axml")?.is_none() {
        return Ok(None);
    }
    adm::validate_production_profile(path, ProductionProfileMode::Read).map(Some)
}

fn write_output(
    destination: &Path,
    overwrite: bool,
    atomic_replace: bool,
    max_output_bytes: u64,
    protected: &[&FileSnapshot],
    max_input_bytes: u64,
    mut write: impl FnMut(&mut dyn Write) -> Result<u64, String>,
) -> Result<u64, String> {
    verify_protected_inputs(
        protected,
        max_input_bytes,
        "protected metadata input changed before writing output",
    )?;
    if atomic_replace {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("create atomic metadata repair temporary file: {error}"))?;
        let bytes = write(temp.as_file_mut())?;
        if bytes > max_output_bytes {
            return Err(format!(
                "metadata repair output is {bytes} bytes, above max_output_bytes {max_output_bytes}"
            ));
        }
        temp.as_file_mut()
            .sync_all()
            .map_err(|error| format!("sync metadata repair temporary file: {error}"))?;
        verify_protected_inputs(
            protected,
            max_input_bytes,
            "protected metadata input changed while writing output",
        )?;
        reject_protected_destination(destination, protected)?;
        if overwrite {
            temp.persist(destination).map_err(|error| {
                format!(
                    "atomically replace metadata repair destination {}: {}",
                    destination.display(),
                    error.error
                )
            })?;
        } else {
            temp.persist_noclobber(destination).map_err(|error| {
                format!(
                    "atomically create metadata repair destination without overwrite {}: {}",
                    destination.display(),
                    error.error
                )
            })?;
        }
        verify_protected_inputs(
            protected,
            max_input_bytes,
            "protected metadata input changed during output commit",
        )?;
        Ok(bytes)
    } else {
        let mut output = open_non_atomic_destination(destination, overwrite)?;
        reject_open_protected_destination(destination, &output, protected)?;
        if overwrite {
            reject_multiply_linked_destination(destination, &output)?;
        }
        verify_protected_inputs(
            protected,
            max_input_bytes,
            "protected metadata input changed before truncating output",
        )?;
        if overwrite {
            output.set_len(0).map_err(|error| {
                format!(
                    "truncate metadata repair output {}: {error}",
                    destination.display()
                )
            })?;
            output.seek(SeekFrom::Start(0)).map_err(|error| {
                format!(
                    "seek metadata repair output {}: {error}",
                    destination.display()
                )
            })?;
        }
        let bytes = write(&mut output)?;
        if bytes > max_output_bytes {
            return Err(format!(
                "metadata repair output is {bytes} bytes, above max_output_bytes {max_output_bytes}"
            ));
        }
        output.flush().map_err(|error| {
            format!(
                "flush metadata repair output {}: {error}",
                destination.display()
            )
        })?;
        verify_protected_inputs(
            protected,
            max_input_bytes,
            "protected metadata input changed while writing output",
        )?;
        verify_non_atomic_destination_binding(destination, &output, protected)?;
        Ok(bytes)
    }
}

fn copy_bounded(source: &Path, output: &mut dyn Write, max_bytes: u64) -> Result<u64, String> {
    let mut input =
        File::open(source).map_err(|error| format!("open {}: {error}", source.display()))?;
    let mut remaining = max_bytes;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", source.display()))?;
        if read == 0 {
            break;
        }
        remaining = remaining
            .checked_sub(read as u64)
            .ok_or_else(|| format!("{} exceeds the configured byte limit", source.display()))?;
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("write metadata repair output: {error}"))?;
        copied += read as u64;
    }
    Ok(copied)
}

fn sha256_file(path: &Path, max_bytes: u64) -> Result<String, String> {
    let input = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    sha256_open_file(&input, path, max_bytes)
}

fn sha256_open_file(file: &File, path: &Path, max_bytes: u64) -> Result<String, String> {
    let mut input = file
        .try_clone()
        .map_err(|error| format!("clone open file {}: {error}", path.display()))?;
    input
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| format!("{} size overflow", path.display()))?;
        if total > max_bytes {
            return Err(format!(
                "{} exceeds the configured byte limit",
                path.display()
            ));
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn rewrite_wave(
    source: &Path,
    output: &mut dyn Write,
    spec: &MetadataRepairSpec,
    actions: &mut Vec<RepairAction>,
    warnings: &mut Vec<String>,
) -> Result<u64, String> {
    let mut input =
        File::open(source).map_err(|error| format!("open {}: {error}", source.display()))?;
    let file_size = input
        .metadata()
        .map_err(|error| format!("stat {}: {error}", source.display()))?
        .len();
    let (chunks, original_header) = scan_riff_chunks(
        &mut input,
        file_size,
        spec.max_metadata_chunk_bytes,
        spec.max_chunks,
    )?;
    let mut replacements: Vec<Option<Vec<u8>>> = vec![None; chunks.len()];
    let mut insert_bext = None;
    let bext_indices = chunks
        .iter()
        .enumerate()
        .filter_map(|(index, chunk)| (chunk.id == *b"bext").then_some(index))
        .collect::<Vec<_>>();
    if spec.ensure_bwf_v2 || spec.bwf_loudness.is_some() {
        if bext_indices.len() > 1 {
            return Err("WAVE contains duplicate bext chunks; refusing ambiguous repair".into());
        }
        let body = if let Some(&index) = bext_indices.first() {
            read_chunk_body(&mut input, &chunks[index], spec.max_metadata_chunk_bytes)?
        } else {
            metadata::blank_bext()
        };
        let mut body = body;
        let before_len = body.len();
        body.resize(body.len().max(602), 0);
        body[346..348].copy_from_slice(&2_u16.to_le_bytes());
        if let Some(loudness) = &spec.bwf_loudness {
            for (offset, value) in [
                (412, loudness.integrated_lufs),
                (414, loudness.loudness_range_lu),
                (416, loudness.true_peak_dbtp),
                (418, loudness.max_momentary_lufs),
                (420, loudness.max_short_term_lufs),
            ] {
                body[offset..offset + 2].copy_from_slice(&bwf_value(value).to_le_bytes());
            }
        }
        if let Some(&index) = bext_indices.first() {
            let changed =
                body != read_chunk_body(&mut input, &chunks[index], spec.max_metadata_chunk_bytes)?;
            replacements[index] = Some(body);
            actions.push(RepairAction {
                kind: "bwf-bext-v2",
                changed,
                detail: format!(
                    "normalized existing bext from {before_len} to {} bytes and set version 2",
                    replacements[index].as_ref().unwrap().len()
                ),
            });
        } else {
            actions.push(RepairAction {
                kind: "bwf-bext-v2",
                changed: true,
                detail: "inserted one 602-byte BWF v2 bext chunk before data".into(),
            });
            insert_bext = Some((first_data_index(&chunks), body));
        }
    }
    if let Some(target) = &spec.adm_version {
        let indices = chunks
            .iter()
            .enumerate()
            .filter_map(|(index, chunk)| (chunk.id == *b"axml").then_some(index))
            .collect::<Vec<_>>();
        if indices.len() != 1 {
            return Err(format!(
                "ADM version repair requires exactly one axml chunk; found {}",
                indices.len()
            ));
        }
        let index = indices[0];
        let body = read_chunk_body(&mut input, &chunks[index], spec.max_xml_bytes)?;
        let replacement = set_adm_version(&body, target, spec.max_xml_bytes)?;
        let changed = replacement != body;
        replacements[index] = Some(replacement);
        actions.push(RepairAction {
            kind: "adm-version",
            changed,
            detail: format!("set audioFormatExtended/@version to {target}"),
        });
    }

    let inserted_size = insert_bext
        .as_ref()
        .map(|(_, body)| 8_u64 + body.len() as u64 + (body.len() as u64 & 1))
        .unwrap_or(0);
    let new_chunks_size = chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            replacements[index]
                .as_ref()
                .map(|body| 8 + body.len() as u64 + (body.len() as u64 & 1))
                .unwrap_or(8 + chunk.body_size + (chunk.body_size & 1))
        })
        .sum::<u64>();
    let new_payload = 4_u64
        .checked_add(inserted_size)
        .and_then(|value| value.checked_add(new_chunks_size))
        .ok_or_else(|| "WAVE RIFF size overflow".to_string())?;
    if new_payload > u32::MAX as u64 {
        return Err(
            "metadata repair would exceed RIFF's 4 GiB size limit; use RF64/BW64 source".into(),
        );
    }
    let mut header = original_header;
    header[4..8].copy_from_slice(&(new_payload as u32).to_le_bytes());
    output
        .write_all(&header)
        .map_err(|error| format!("write WAVE header: {error}"))?;
    let mut written = 12_u64;
    for (index, chunk) in chunks.iter().enumerate() {
        if insert_bext.as_ref().is_some_and(|(at, _)| *at == index) {
            let body = &insert_bext.as_ref().unwrap().1;
            write_wave_chunk(output, *b"bext", body)?;
            written += 8 + body.len() as u64 + (body.len() as u64 & 1);
        }
        if let Some(body) = &replacements[index] {
            write_wave_chunk(output, chunk.id, body)?;
            written += 8 + body.len() as u64 + (body.len() as u64 & 1);
        } else {
            copy_wave_chunk(&mut input, output, chunk)?;
            written += 8 + chunk.body_size + (chunk.body_size & 1);
        }
    }
    if insert_bext
        .as_ref()
        .is_some_and(|(at, _)| *at == chunks.len())
    {
        let body = &insert_bext.as_ref().unwrap().1;
        write_wave_chunk(output, *b"bext", body)?;
        written += 8 + body.len() as u64 + (body.len() as u64 & 1);
    }
    if written != new_payload + 8 {
        warnings.push(format!(
            "WAVE byte accounting differs from declared size (wrote {written}, expected {})",
            new_payload + 8
        ));
    }
    Ok(written)
}

fn scan_riff_chunks(
    input: &mut File,
    file_size: u64,
    max_metadata_chunk_bytes: u64,
    max_chunks: u32,
) -> Result<(Vec<WaveChunkInfo>, [u8; 12]), String> {
    input
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek WAVE header: {error}"))?;
    let mut riff_header = [0_u8; 12];
    input
        .read_exact(&mut riff_header)
        .map_err(|error| format!("read WAVE header: {error}"))?;
    if &riff_header[..4] != b"RIFF" || &riff_header[8..] != b"WAVE" {
        return Err("metadata mutation currently supports RIFF/WAVE only; RF64/BW64 is validate-and-copy only".into());
    }
    let declared_size = u64::from(u32::from_le_bytes(
        riff_header[4..8].try_into().expect("four-byte RIFF size"),
    ));
    if declared_size < 4 {
        return Err("RIFF size is smaller than the WAVE form type".into());
    }
    let scan_end = 8_u64
        .checked_add(declared_size)
        .ok_or_else(|| "RIFF size overflows file offset".to_string())?;
    if scan_end > file_size {
        return Err("declared RIFF container extends beyond end of file".into());
    }
    if scan_end < file_size {
        // Mutation promises to preserve every unknown source byte. Silently
        // dropping data outside the declared RIFF form would violate that
        // contract, so repair is stricter than the decoder (which ignores it).
        return Err("WAVE contains bytes outside the declared RIFF container".into());
    }
    let mut position = 12_u64;
    let mut chunks = Vec::new();
    while position < scan_end {
        if chunks.len() >= max_chunks as usize {
            return Err(format!("WAVE contains more than max_chunks ({max_chunks})"));
        }
        if scan_end - position < 8 {
            return Err("truncated WAVE chunk header".into());
        }
        input
            .seek(SeekFrom::Start(position))
            .map_err(|error| format!("seek WAVE chunk: {error}"))?;
        let mut header = [0_u8; 8];
        input
            .read_exact(&mut header)
            .map_err(|error| format!("read WAVE chunk header: {error}"))?;
        let id: [u8; 4] = header[..4].try_into().unwrap();
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as u64;
        if id != *b"data" && size > max_metadata_chunk_bytes {
            return Err(format!(
                "WAVE chunk {:?} is {size} bytes, above max_metadata_chunk_bytes {}",
                String::from_utf8_lossy(&id),
                max_metadata_chunk_bytes
            ));
        }
        let body_offset = position + 8;
        let next = body_offset
            .checked_add(size)
            .and_then(|value| value.checked_add(size & 1))
            .ok_or_else(|| "WAVE chunk offset overflow".to_string())?;
        if next > scan_end {
            return Err("WAVE chunk or padding extends beyond the declared RIFF container".into());
        }
        chunks.push(WaveChunkInfo {
            id,
            body_offset,
            body_size: size,
        });
        position = next;
    }
    if position != scan_end {
        return Err("WAVE contains unaccounted bytes inside the declared RIFF container".into());
    }
    Ok((chunks, riff_header))
}

fn first_data_index(chunks: &[WaveChunkInfo]) -> usize {
    chunks
        .iter()
        .position(|chunk| chunk.id == *b"data")
        .unwrap_or(chunks.len())
}

fn read_chunk_body(
    input: &mut File,
    chunk: &WaveChunkInfo,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    if chunk.body_size > max_bytes {
        return Err(format!(
            "WAVE metadata chunk is {} bytes, above configured limit {max_bytes}",
            chunk.body_size
        ));
    }
    let size =
        usize::try_from(chunk.body_size).map_err(|_| "WAVE chunk is too large".to_string())?;
    let mut body = vec![0_u8; size];
    input
        .seek(SeekFrom::Start(chunk.body_offset))
        .map_err(|error| format!("seek WAVE chunk body: {error}"))?;
    input
        .read_exact(&mut body)
        .map_err(|error| format!("read WAVE chunk body: {error}"))?;
    Ok(body)
}

fn copy_wave_chunk(
    input: &mut File,
    output: &mut dyn Write,
    chunk: &WaveChunkInfo,
) -> Result<(), String> {
    output
        .write_all(&chunk.id)
        .map_err(|error| format!("write WAVE chunk id: {error}"))?;
    output
        .write_all(&(chunk.body_size as u32).to_le_bytes())
        .map_err(|error| format!("write WAVE chunk size: {error}"))?;
    input
        .seek(SeekFrom::Start(chunk.body_offset))
        .map_err(|error| format!("seek WAVE chunk body: {error}"))?;
    copy_exact(input, output, chunk.body_size)?;
    if chunk.body_size & 1 != 0 {
        output
            .write_all(&[0])
            .map_err(|error| format!("write WAVE chunk padding: {error}"))?;
    }
    Ok(())
}

fn copy_exact(input: &mut File, output: &mut dyn Write, mut bytes: u64) -> Result<(), String> {
    let mut buffer = [0_u8; 128 * 1024];
    while bytes > 0 {
        let want = usize::try_from(bytes.min(buffer.len() as u64)).unwrap();
        input
            .read_exact(&mut buffer[..want])
            .map_err(|error| format!("read WAVE bytes: {error}"))?;
        output
            .write_all(&buffer[..want])
            .map_err(|error| format!("write WAVE bytes: {error}"))?;
        bytes -= want as u64;
    }
    Ok(())
}

fn write_wave_chunk(output: &mut dyn Write, id: [u8; 4], body: &[u8]) -> Result<(), String> {
    let size =
        u32::try_from(body.len()).map_err(|_| "WAVE metadata chunk exceeds 4 GiB".to_string())?;
    output
        .write_all(&id)
        .and_then(|_| output.write_all(&size.to_le_bytes()))
        .and_then(|_| output.write_all(body))
        .map_err(|error| format!("write WAVE metadata chunk: {error}"))?;
    if body.len() & 1 != 0 {
        output
            .write_all(&[0])
            .map_err(|error| format!("write WAVE metadata padding: {error}"))?;
    }
    Ok(())
}

fn bwf_value(value: f64) -> i16 {
    (value * 100.0)
        .round()
        .clamp(i16::MIN as f64, (i16::MAX - 1) as f64) as i16
}

fn set_adm_version(body: &[u8], target: &str, max_xml_bytes: u64) -> Result<Vec<u8>, String> {
    if body.len() as u64 > max_xml_bytes {
        return Err("ADM axml exceeds the default XML safety limit".into());
    }
    let mut roots = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = body[cursor..]
        .windows(b"audioFormatExtended".len())
        .position(|window| window == b"audioFormatExtended")
    {
        let start = cursor + relative;
        let before = body.get(start.wrapping_sub(1)).copied();
        let after = body.get(start + b"audioFormatExtended".len()).copied();
        if before == Some(b'<')
            && after.is_some_and(|byte| byte.is_ascii_whitespace() || byte == b'>' || byte == b'/')
        {
            roots.push(start);
        }
        cursor = start + b"audioFormatExtended".len();
    }
    if roots.len() != 1 {
        return Err(format!(
            "ADM axml must contain exactly one audioFormatExtended root; found {}",
            roots.len()
        ));
    }
    let name_start = roots[0];
    let tag_end = body[name_start..]
        .iter()
        .position(|byte| *byte == b'>')
        .map(|value| name_start + value)
        .ok_or_else(|| "ADM audioFormatExtended start tag is unterminated".to_string())?;
    let tag = &body[name_start + b"audioFormatExtended".len()..tag_end];
    if let Some(relative) = find_xml_attribute(tag, b"version") {
        let value_start = name_start + b"audioFormatExtended".len() + relative.0;
        let value_end = name_start + b"audioFormatExtended".len() + relative.1;
        let mut result = Vec::with_capacity(body.len() + target.len());
        result.extend_from_slice(&body[..value_start]);
        result.extend_from_slice(target.as_bytes());
        result.extend_from_slice(&body[value_end..]);
        if result.len() as u64 > max_xml_bytes {
            return Err("ADM axml repair exceeds max_xml_bytes".into());
        }
        Ok(result)
    } else {
        let mut insert = tag_end;
        while insert > name_start && body[insert - 1].is_ascii_whitespace() {
            insert -= 1;
        }
        if insert > name_start && body[insert - 1] == b'/' {
            insert -= 1;
        }
        let mut result = Vec::with_capacity(body.len() + target.len() + 11);
        result.extend_from_slice(&body[..insert]);
        result.extend_from_slice(b" version=\"");
        result.extend_from_slice(target.as_bytes());
        result.extend_from_slice(b"\"");
        result.extend_from_slice(&body[insert..]);
        if result.len() as u64 > max_xml_bytes {
            return Err("ADM axml repair exceeds max_xml_bytes".into());
        }
        Ok(result)
    }
}

/// Return the byte range of an XML attribute's value relative to `tag`.
fn find_xml_attribute(tag: &[u8], wanted: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index + wanted.len() < tag.len() {
        if &tag[index..index + wanted.len()] == wanted
            && (index == 0 || tag[index - 1].is_ascii_whitespace())
        {
            let mut cursor = index + wanted.len();
            while cursor < tag.len() && tag[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if tag.get(cursor) != Some(&b'=') {
                index += wanted.len();
                continue;
            }
            cursor += 1;
            while cursor < tag.len() && tag[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let quote = *tag.get(cursor)?;
            if quote != b'\'' && quote != b'"' {
                return None;
            }
            let start = cursor + 1;
            let end = tag[start..].iter().position(|byte| *byte == quote)? + start;
            return Some((start, end));
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, PcmKind, WavWriter, WaveChunk};

    fn mp4_box(kind: &[u8; 4], body: Vec<u8>) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&(u32::try_from(body.len() + 8).unwrap()).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(&body);
        output
    }

    fn mp4_full_box(version: u8, payload: Vec<u8>) -> Vec<u8> {
        [vec![version, 0, 0, 0], payload].concat()
    }

    fn pcm_mp4(path: &Path) {
        pcm_mp4_with(path, 48_000, 0.1);
    }

    fn pcm_mp4_with(path: &Path, frames: u32, amplitude: f32) {
        const SAMPLE_RATE: u32 = 48_000;
        let ftyp = mp4_box(
            b"ftyp",
            [b"M4A ".as_slice(), &[0, 0, 0, 0], b"isom"].concat(),
        );
        let mut sample_entry = vec![0_u8; 28];
        sample_entry[6..8].copy_from_slice(&1_u16.to_be_bytes());
        sample_entry[16..18].copy_from_slice(&2_u16.to_be_bytes());
        sample_entry[18..20].copy_from_slice(&16_u16.to_be_bytes());
        sample_entry[24..28].copy_from_slice(&(SAMPLE_RATE << 16).to_be_bytes());
        let stsd = mp4_box(
            b"stsd",
            mp4_full_box(
                0,
                [1_u32.to_be_bytes().to_vec(), mp4_box(b"sowt", sample_entry)].concat(),
            ),
        );
        let stts = mp4_box(
            b"stts",
            mp4_full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    frames.to_be_bytes(),
                    1_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let stsz = mp4_box(
            b"stsz",
            mp4_full_box(0, [4_u32.to_be_bytes(), frames.to_be_bytes()].concat()),
        );
        let stsc = mp4_box(
            b"stsc",
            mp4_full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    frames.to_be_bytes(),
                    1_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let make_moov = |chunk_offset: u32| {
            let stco = mp4_box(
                b"stco",
                mp4_full_box(
                    0,
                    [1_u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat(),
                ),
            );
            let stbl = mp4_box(
                b"stbl",
                [stsd.clone(), stts.clone(), stsz.clone(), stsc.clone(), stco].concat(),
            );
            let mut tkhd = vec![0_u8; 84];
            tkhd[12..16].copy_from_slice(&1_u32.to_be_bytes());
            tkhd[20..24].copy_from_slice(&frames.to_be_bytes());
            let tkhd = mp4_box(b"tkhd", tkhd);
            let mdhd = mp4_box(
                b"mdhd",
                mp4_full_box(
                    0,
                    [
                        vec![0; 8],
                        SAMPLE_RATE.to_be_bytes().to_vec(),
                        frames.to_be_bytes().to_vec(),
                        vec![0; 4],
                    ]
                    .concat(),
                ),
            );
            let hdlr = mp4_box(
                b"hdlr",
                mp4_full_box(0, [vec![0; 4], b"soun".to_vec(), vec![0; 12]].concat()),
            );
            let mdia = mp4_box(b"mdia", [mdhd, hdlr, mp4_box(b"minf", stbl)].concat());
            let mvhd = mp4_box(
                b"mvhd",
                mp4_full_box(
                    0,
                    [
                        vec![0; 8],
                        SAMPLE_RATE.to_be_bytes().to_vec(),
                        frames.to_be_bytes().to_vec(),
                        vec![0; 80],
                    ]
                    .concat(),
                ),
            );
            mp4_box(
                b"moov",
                [mvhd, mp4_box(b"trak", [tkhd, mdia].concat())].concat(),
            )
        };
        let placeholder = make_moov(0);
        let chunk_offset = u32::try_from(ftyp.len() + placeholder.len() + 8).unwrap();
        let moov = make_moov(chunk_offset);
        let mut pcm = Vec::with_capacity(frames as usize * 4);
        for frame in 0..frames {
            let phase = std::f32::consts::TAU * 440.0 * frame as f32 / SAMPLE_RATE as f32;
            let sample = (phase.sin() * amplitude * i16::MAX as f32).round() as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(path, [ftyp, moov, mp4_box(b"mdat", pcm)].concat()).unwrap();
    }

    fn classic_stereo_wave(path: &Path) {
        classic_stereo_wave_with(path, 48_000, 0.1);
    }

    fn classic_stereo_wave_with(path: &Path, frames: u32, amplitude: f32) {
        const SAMPLE_RATE: u32 = 48_000;
        let frames = usize::try_from(frames).unwrap();
        let samples = (0..frames)
            .map(|frame| {
                let phase = std::f32::consts::TAU * 440.0 * frame as f32 / SAMPLE_RATE as f32;
                phase.sin() * amplitude
            })
            .collect::<Vec<_>>();
        let audio = AudioBuffer {
            sample_rate: SAMPLE_RATE,
            channels: 2,
            frames,
            data: vec![samples.clone(), samples],
            channel_roles: crate::wav::default_channel_roles(2),
            source_kind: PcmKind::S16,
        };
        WavWriter::write(path, &audio, PcmKind::S16, false).unwrap();
    }

    fn fixture(path: &Path, chunks: Vec<WaveChunk>) {
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 32,
            data: vec![vec![0.1; 32]],
            channel_roles: crate::wav::default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
        WavWriter::write_with_metadata(
            path,
            &audio,
            PcmKind::F32,
            false,
            crate::wav::WavContainer::Riff,
            &chunks,
        )
        .unwrap();
    }

    fn repair_spec(
        source: PathBuf,
        destination: PathBuf,
        schema_version: u32,
    ) -> MetadataRepairSpec {
        MetadataRepairSpec {
            schema_version,
            source,
            destination,
            mode: RepairMode::Repair,
            ensure_bwf_v2: false,
            bwf_loudness: None,
            adm_version: None,
            overwrite: false,
            atomic_replace: true,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_metadata_chunk_bytes: DEFAULT_MAX_METADATA_CHUNK_BYTES,
            max_xml_bytes: DEFAULT_MAX_XML_BYTES,
            max_chunks: DEFAULT_MAX_CHUNKS,
        }
    }

    fn scan_test_wave(path: &Path) -> Result<(Vec<WaveChunkInfo>, [u8; 12]), String> {
        let mut file = File::open(path).unwrap();
        let file_size = file.metadata().unwrap().len();
        scan_riff_chunks(
            &mut file,
            file_size,
            DEFAULT_MAX_METADATA_CHUNK_BYTES,
            DEFAULT_MAX_CHUNKS,
        )
    }

    #[test]
    fn metadata_mutation_is_bounded_by_the_declared_riff_container() {
        let directory = tempfile::tempdir().unwrap();
        let valid_path = directory.path().join("valid.wav");
        classic_stereo_wave_with(&valid_path, 8, 0.1);
        let valid = fs::read(&valid_path).unwrap();
        assert!(scan_test_wave(&valid_path).is_ok());

        let smaller_than_form = directory.path().join("small-riff-size.wav");
        let mut bytes = valid.clone();
        bytes[4..8].copy_from_slice(&3_u32.to_le_bytes());
        fs::write(&smaller_than_form, bytes).unwrap();
        let error = scan_test_wave(&smaller_than_form).unwrap_err();
        assert!(error.contains("smaller than the WAVE form type"), "{error}");

        let beyond_eof = directory.path().join("declared-beyond-eof.wav");
        let mut bytes = valid.clone();
        let oversized = u32::try_from(bytes.len() - 8 + 1).unwrap();
        bytes[4..8].copy_from_slice(&oversized.to_le_bytes());
        fs::write(&beyond_eof, bytes).unwrap();
        let error = scan_test_wave(&beyond_eof).unwrap_err();
        assert!(error.contains("extends beyond end of file"), "{error}");

        let chunk_crossing = directory.path().join("chunk-crossing.wav");
        let mut bytes = valid.clone();
        bytes.pop();
        let shortened = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&shortened.to_le_bytes());
        fs::write(&chunk_crossing, bytes).unwrap();
        let error = scan_test_wave(&chunk_crossing).unwrap_err();
        assert!(
            error.contains("extends beyond the declared RIFF"),
            "{error}"
        );

        let padding_crossing = directory.path().join("padding-crossing.wav");
        let mut bytes = b"RIFF\0\0\0\0WAVEJUNK\x01\0\0\0x".to_vec();
        let riff_size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        fs::write(&padding_crossing, bytes).unwrap();
        let error = scan_test_wave(&padding_crossing).unwrap_err();
        assert!(error.contains("padding extends beyond"), "{error}");

        let outside = directory.path().join("outside-declaration.wav");
        let mut bytes = valid;
        bytes.extend_from_slice(b"JUNK\0\0\0\0");
        fs::write(&outside, bytes).unwrap();
        let error = scan_test_wave(&outside).unwrap_err();
        assert!(error.contains("outside the declared RIFF"), "{error}");
    }

    #[test]
    fn content_hash_detects_same_identity_same_length_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reference.bin");
        fs::write(&path, [0x11; 64]).unwrap();
        let handle = File::open(&path).unwrap();
        let canonical = fs::canonicalize(&path).unwrap();
        let identity = identity_from_open_file(&handle, &canonical).unwrap();
        let before = sha256_file(&canonical, 64).unwrap();

        fs::write(&canonical, [0x22; 64]).unwrap();
        ensure_file_snapshot(&canonical, &identity, 64).unwrap();
        let after = sha256_file(&canonical, 64).unwrap();
        let error = ensure_matching_file_hash(
            &path,
            &before,
            &after,
            "decoded reference content changed while it was measured",
        )
        .unwrap_err();
        assert!(error.contains("content changed"), "{error}");
    }

    #[test]
    fn file_snapshot_binds_processing_bytes_to_the_opened_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let original = directory.path().join("original.wav");
        let replacement = directory.path().join("replacement.wav");
        fs::write(&source, [0x11; 64]).unwrap();
        fs::write(&replacement, [0x22; 64]).unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        fs::rename(&source, &original).unwrap();
        fs::rename(&replacement, &source).unwrap();
        let error = snapshot
            .verify(64, "metadata source changed during processing")
            .unwrap_err();
        assert!(error.contains("source changed"), "{error}");
        assert_eq!(fs::read(snapshot.stable_path()).unwrap(), [0x11; 64]);
        assert_eq!(fs::read(&original).unwrap(), [0x11; 64]);
        assert_eq!(fs::read(&source).unwrap(), [0x22; 64]);
    }

    #[cfg(unix)]
    #[test]
    fn file_snapshot_detects_symlink_retargeting() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let original = directory.path().join("original.wav");
        let replacement = directory.path().join("replacement.wav");
        fs::write(&original, [0x11; 64]).unwrap();
        fs::write(&replacement, [0x22; 64]).unwrap();
        symlink(&original, &source).unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        fs::remove_file(&source).unwrap();
        symlink(&replacement, &source).unwrap();
        let error = snapshot
            .verify(64, "metadata source changed during processing")
            .unwrap_err();
        assert!(error.contains("source changed"), "{error}");
        assert_eq!(fs::read(snapshot.stable_path()).unwrap(), [0x11; 64]);
        assert_eq!(fs::read(&original).unwrap(), [0x11; 64]);
        assert_eq!(fs::read(&replacement).unwrap(), [0x22; 64]);
    }

    #[test]
    fn atomic_output_rejects_source_replacement_before_publish() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let original = directory.path().join("original.bin");
        let replacement = directory.path().join("replacement.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"original source").unwrap();
        fs::write(&replacement, b"replacement src").unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        let error = write_output(&destination, true, true, 64, &[&snapshot], 64, |output| {
            output.write_all(b"generated").unwrap();
            fs::rename(&source, &original).unwrap();
            fs::rename(&replacement, &source).unwrap();
            Ok(9)
        })
        .unwrap_err();
        assert!(error.contains("changed while writing output"), "{error}");
        assert!(!destination.exists());
        assert_eq!(fs::read(&original).unwrap(), b"original source");
        assert_eq!(fs::read(&source).unwrap(), b"replacement src");
    }

    #[test]
    fn atomic_no_overwrite_is_race_free() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"source").unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        let error = write_output(&destination, false, true, 64, &[&snapshot], 64, |output| {
            output.write_all(b"generated").unwrap();
            fs::write(&destination, b"competitor").unwrap();
            Ok(9)
        })
        .unwrap_err();
        assert!(error.contains("without overwrite"), "{error}");
        assert_eq!(fs::read(&destination).unwrap(), b"competitor");
        assert_eq!(fs::read(&source).unwrap(), b"source");
    }

    #[test]
    fn album_snapshot_budget_is_applied_before_copying_each_reference() {
        let directory = tempfile::tempdir().unwrap();
        let track = directory.path().join("track.bin");
        let other = directory.path().join("other.bin");
        fs::write(&track, b"123456").unwrap();
        fs::write(&other, b"abcde").unwrap();
        let track_snapshot = FileSnapshot::capture(&track, 10, "selected track").unwrap();
        let track_analysis = isobmff_loudness_repair::ReferenceMeasurement {
            loudness: DecodedLoudness {
                sample_rate_hz: 48_000,
                channels: 1,
                frames: 1,
                decoded_samples: 1,
                integrated_lufs: -23.0,
                sample_peak_dbfs: -20.0,
                true_peak_dbtp: -20.0,
            },
            gating_blocks: vec![0.01],
        };

        let error = match prepare_album_loudness(
            directory.path(),
            &track_snapshot,
            track_analysis,
            &[track.clone(), other],
            2,
            16,
            10,
        ) {
            Ok(_) => panic!("aggregate snapshot budget unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(error.contains("remaining aggregate budget"), "{error}");
        assert!(error.contains("byte limit 4"), "{error}");
    }

    #[test]
    fn non_atomic_overwrite_writes_a_regular_single_link_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"old destination").unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        let bytes = write_output(&destination, true, false, 64, &[&snapshot], 64, |output| {
            output.write_all(b"new").unwrap();
            Ok(3)
        })
        .unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert_eq!(fs::read(&source).unwrap(), b"source");
    }

    #[cfg(unix)]
    #[test]
    fn non_atomic_overwrite_never_follows_a_destination_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let victim = directory.path().join("victim.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"source").unwrap();
        fs::write(&victim, b"unrelated victim").unwrap();
        symlink(&victim, &destination).unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        assert!(
            write_output(&destination, true, false, 64, &[&snapshot], 64, |output| {
                output.write_all(b"replacement").unwrap();
                Ok(11)
            })
            .is_err()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"unrelated victim");
        assert!(fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_atomic_overwrite_refuses_unprotected_hardlink_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        let alias = directory.path().join("alias.bin");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"shared destination").unwrap();
        fs::hard_link(&destination, &alias).unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        let error = write_output(&destination, true, false, 64, &[&snapshot], 64, |output| {
            output.write_all(b"replacement").unwrap();
            Ok(11)
        })
        .unwrap_err();
        assert!(error.contains("multiply-linked"), "{error}");
        assert_eq!(fs::read(&destination).unwrap(), b"shared destination");
        assert_eq!(fs::read(&alias).unwrap(), b"shared destination");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_atomic_overwrite_checks_the_opened_destination_before_truncating() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"protected source bytes").unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        // This simulates the destination being exchanged for a source hardlink
        // after an earlier path-level check but before the output open.
        fs::hard_link(&source, &destination).unwrap();
        let before = fs::read(&source).unwrap();
        let error = write_output(&destination, true, false, 64, &[&snapshot], 64, |output| {
            output.write_all(b"replacement").unwrap();
            Ok(11)
        })
        .unwrap_err();
        assert!(error.contains("protected source"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), before);
        assert_eq!(fs::read(&destination).unwrap(), before);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_atomic_limit_error_never_unlinks_a_raced_protected_path() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        let displaced_output = directory.path().join("displaced-output.bin");
        fs::write(&source, b"protected source bytes").unwrap();
        fs::write(&destination, b"old destination").unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        let error = write_output(&destination, true, false, 4, &[&snapshot], 64, |output| {
            output.write_all(b"oversized").unwrap();
            fs::rename(&destination, &displaced_output).unwrap();
            fs::rename(&source, &destination).unwrap();
            Ok(9)
        })
        .unwrap_err();
        assert!(error.contains("above max_output_bytes"), "{error}");
        assert_eq!(fs::read(&destination).unwrap(), b"protected source bytes");
        assert_eq!(fs::read(&displaced_output).unwrap(), b"oversized");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_atomic_success_rejects_a_destination_exchanged_for_protected_input() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        let displaced_output = directory.path().join("displaced-output.bin");
        fs::write(&source, b"protected source bytes").unwrap();
        fs::write(&destination, b"old destination").unwrap();
        let snapshot = FileSnapshot::capture(&source, 64, "test source").unwrap();

        let error = write_output(&destination, true, false, 64, &[&snapshot], 64, |output| {
            output.write_all(b"generated").unwrap();
            fs::rename(&destination, &displaced_output).unwrap();
            fs::hard_link(&source, &destination).unwrap();
            Ok(9)
        })
        .unwrap_err();
        assert!(error.contains("protected source"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), b"protected source bytes");
        assert_eq!(fs::read(&destination).unwrap(), b"protected source bytes");
        assert_eq!(fs::read(&displaced_output).unwrap(), b"generated");
    }

    #[test]
    fn adm_version_patch_preserves_xml_bytes() {
        let source = br#"<audioFormatExtended><audioProgramme/></audioFormatExtended>"#;
        let output = set_adm_version(source, adm::ADM_VERSION, DEFAULT_MAX_XML_BYTES).unwrap();
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains("version=\"ITU-R_BS.2076-3\""));
        let source = br#"<audioFormatExtended version='old' />tail"#;
        let output = set_adm_version(source, adm::ADM_VERSION, DEFAULT_MAX_XML_BYTES).unwrap();
        assert_eq!(
            std::str::from_utf8(&output).unwrap(),
            "<audioFormatExtended version='ITU-R_BS.2076-3' />tail"
        );
    }

    #[test]
    fn repair_changes_bext_and_keeps_unknown_chunk() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.wav");
        let destination = directory.path().join("destination.wav");
        fixture(
            &source,
            vec![
                WaveChunk {
                    id: *b"JUNK",
                    body: vec![1, 2, 3],
                },
                WaveChunk {
                    id: *b"bext",
                    body: metadata::blank_bext(),
                },
            ],
        );
        let spec = MetadataRepairSpec {
            schema_version: 1,
            source: source.clone(),
            destination: destination.clone(),
            mode: RepairMode::Repair,
            ensure_bwf_v2: true,
            bwf_loudness: Some(BwfLoudness {
                integrated_lufs: -23.0,
                loudness_range_lu: 4.0,
                true_peak_dbtp: -1.0,
                max_momentary_lufs: -12.0,
                max_short_term_lufs: -16.0,
            }),
            adm_version: None,
            overwrite: false,
            atomic_replace: true,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_metadata_chunk_bytes: DEFAULT_MAX_METADATA_CHUNK_BYTES,
            max_xml_bytes: DEFAULT_MAX_XML_BYTES,
            max_chunks: DEFAULT_MAX_CHUNKS,
        };
        let report = evaluate(directory.path().join("request.json").as_path(), spec).unwrap();
        assert!(report.passed);
        assert!(report.changed);
        assert_eq!(
            metadata::read_wave_chunk(&destination, *b"JUNK").unwrap(),
            Some(vec![1, 2, 3])
        );
        let bext = metadata::read_bext(&destination).unwrap().unwrap();
        assert_eq!(i16::from_le_bytes([bext[412], bext[413]]), -2300);
    }

    #[test]
    fn repairs_isobmff_loudness_from_its_decoded_audio() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.m4a");
        let reference = directory.path().join("reference.wav");
        let destination = directory.path().join("destination.m4a");
        pcm_mp4(&source);
        classic_stereo_wave(&reference);
        let before = container_qc::audit(&source).unwrap();
        assert!(before.passed, "{before:#?}");
        let request_json = serde_json::json!({
            "schema_version": 1,
            "source": source,
            "destination": destination,
            "isobmff_loudness": {
                "decoded_reference": "reference.wav",
                "max_decoded_samples": 200_000
            },
            "atomic_replace": true
        });
        let request_schema: serde_json::Value = serde_json::from_str(include_str!(
            "../schema/metadata-repair-request-v1.schema.json"
        ))
        .unwrap();
        assert!(jsonschema::validator_for(&request_schema)
            .unwrap()
            .is_valid(&request_json));
        let (_, parsed_options) =
            parse_extended_spec(Path::new("request.json"), &request_json.to_string()).unwrap();
        let parsed_options = parsed_options.unwrap();
        assert_eq!(
            parsed_options.decoded_reference.as_deref(),
            Some(Path::new("reference.wav"))
        );
        assert_eq!(parsed_options.max_decoded_samples, 200_000);
        let spec = MetadataRepairSpec {
            schema_version: 1,
            source: source.clone(),
            destination: destination.clone(),
            mode: RepairMode::Repair,
            ensure_bwf_v2: false,
            bwf_loudness: None,
            adm_version: None,
            overwrite: false,
            atomic_replace: true,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_metadata_chunk_bytes: DEFAULT_MAX_METADATA_CHUNK_BYTES,
            max_xml_bytes: DEFAULT_MAX_XML_BYTES,
            max_chunks: DEFAULT_MAX_CHUNKS,
        };
        let report = evaluate_isobmff_loudness(
            directory.path().join("request.json").as_path(),
            spec,
            IsobmffLoudnessRepair {
                decoded_reference: Some(reference),
                max_decoded_samples: 200_000,
            },
        )
        .unwrap();
        assert!(report.report.passed, "{report:#?}");
        assert!(report.report.changed);
        let report_json = serde_json::to_value(&report).unwrap();
        let report_schema: serde_json::Value = serde_json::from_str(include_str!(
            "../schema/metadata-repair-report-v1.schema.json"
        ))
        .unwrap();
        assert!(jsonschema::validator_for(&report_schema)
            .unwrap()
            .is_valid(&report_json));
        let evidence = report.isobmff_loudness.unwrap();
        assert!(evidence.mdat_preserved);
        assert!(evidence.metadata_round_trip_passed);
        assert_eq!(evidence.max_decoded_samples, 200_000);
        assert_eq!(evidence.source_mdat_sha256, evidence.output_mdat_sha256);
        assert_eq!(
            report.report.after.properties["tracks"][0]["loudness_box_count"],
            1
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn destinations_cannot_replace_source_or_decoded_reference_identities() {
        let directory = tempfile::tempdir().unwrap();
        let request = directory.path().join("request.json");

        let wave_source = directory.path().join("source.wav");
        let source_alias = directory.path().join("source-alias.wav");
        classic_stereo_wave(&wave_source);
        fs::hard_link(&wave_source, &source_alias).unwrap();
        let source_before = fs::read(&wave_source).unwrap();
        let mut source_spec =
            repair_spec(wave_source.clone(), source_alias.clone(), SCHEMA_VERSION);
        source_spec.overwrite = true;
        source_spec.atomic_replace = false;
        let error = evaluate(&request, source_spec).unwrap_err();
        assert!(error.contains("protected source"), "{error}");
        assert_eq!(fs::read(&wave_source).unwrap(), source_before);
        assert_eq!(fs::read(&source_alias).unwrap(), source_before);

        let source = directory.path().join("source.m4a");
        let track = directory.path().join("track.wav");
        pcm_mp4(&source);
        classic_stereo_wave(&track);
        let source_before = fs::read(&source).unwrap();
        let track_before = fs::read(&track).unwrap();
        let mut track_spec = repair_spec(source.clone(), track.clone(), SCHEMA_VERSION);
        track_spec.overwrite = true;
        let error = evaluate_isobmff_loudness(
            &request,
            track_spec,
            IsobmffLoudnessRepair {
                decoded_reference: Some(track.clone()),
                max_decoded_samples: 200_000,
            },
        )
        .unwrap_err();
        assert!(error.contains("decoded reference"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(&track).unwrap(), track_before);

        let companion = directory.path().join("companion.wav");
        let companion_alias = directory.path().join("companion-alias.wav");
        classic_stereo_wave_with(&companion, 48_000, 0.2);
        fs::hard_link(&companion, &companion_alias).unwrap();
        let companion_before = fs::read(&companion).unwrap();
        let mut album_spec =
            repair_spec(source.clone(), companion_alias.clone(), SCHEMA_VERSION_V2);
        album_spec.overwrite = true;
        album_spec.atomic_replace = false;
        let error = evaluate_isobmff_loudness_v2(
            &request,
            album_spec,
            IsobmffLoudnessRepairV2 {
                decoded_reference: Some(track.clone()),
                album_decoded_references: Some(vec![track.clone(), companion.clone()]),
                max_album_references: 2,
                max_decoded_samples: 300_000,
            },
        )
        .unwrap_err();
        assert!(error.contains("decoded reference"), "{error}");
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(&track).unwrap(), track_before);
        assert_eq!(fs::read(&companion).unwrap(), companion_before);
        assert_eq!(fs::read(&companion_alias).unwrap(), companion_before);
    }

    #[test]
    fn schema_v2_repairs_album_loudness_from_combined_complete_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("quiet.m4a");
        let quiet_reference = directory.path().join("quiet.wav");
        let loud_reference = directory.path().join("loud.wav");
        let destination = directory.path().join("destination.m4a");
        let request = directory.path().join("request.json");
        pcm_mp4_with(&source, 192_000, 0.01);
        classic_stereo_wave_with(&quiet_reference, 192_000, 0.01);
        classic_stereo_wave_with(&loud_reference, 48_000, 0.5);

        let quiet_analysis =
            isobmff_loudness_repair::analyze_reference(&quiet_reference, 1_000_000).unwrap();
        let loud_analysis =
            isobmff_loudness_repair::analyze_reference(&loud_reference, 1_000_000).unwrap();
        let mut combined_blocks = quiet_analysis.gating_blocks.clone();
        combined_blocks.extend_from_slice(&loud_analysis.gating_blocks);
        let expected_lufs = crate::dsp::lufs::gated_lufs(&combined_blocks);
        let equal_track_mean_square = [
            quiet_analysis.loudness.integrated_lufs,
            loud_analysis.loudness.integrated_lufs,
        ]
        .into_iter()
        .map(|value| 10.0_f64.powf((value + 0.691) / 10.0))
        .sum::<f64>()
            / 2.0;
        let equal_track_lufs = -0.691 + 10.0 * equal_track_mean_square.log10();
        assert!((expected_lufs - equal_track_lufs).abs() > 2.0);

        let request_json = serde_json::json!({
            "schema_version": 2,
            "source": "quiet.m4a",
            "destination": "destination.m4a",
            "atomic_replace": true,
            "isobmff_loudness": {
                "decoded_reference": "quiet.wav",
                "album_decoded_references": ["quiet.wav", "loud.wav"],
                "max_album_references": 2,
                "max_decoded_samples": 600_000
            }
        });
        let request_schema: serde_json::Value = serde_json::from_str(include_str!(
            "../schema/metadata-repair-request-v2.schema.json"
        ))
        .unwrap();
        assert!(jsonschema::validator_for(&request_schema)
            .unwrap()
            .is_valid(&request_json));
        fs::write(&request, serde_json::to_vec_pretty(&request_json).unwrap()).unwrap();

        let report = evaluate_versioned_file(&request).unwrap();
        let VersionedMetadataRepairReport::V2(report) = report else {
            panic!("schema v2 request returned a v1 report");
        };
        assert!(report.report.passed, "{report:#?}");
        assert!(report.report.changed);
        assert_eq!(report.report.schema_version, 2);
        assert_eq!(report.report.validator, VALIDATOR_V2);
        let report_json = serde_json::to_value(&report).unwrap();
        let report_schema: serde_json::Value = serde_json::from_str(include_str!(
            "../schema/metadata-repair-report-v2.schema.json"
        ))
        .unwrap();
        assert!(
            jsonschema::validator_for(&report_schema)
                .unwrap()
                .is_valid(&report_json),
            "{report_json:#}"
        );
        let evidence = report.isobmff_loudness.unwrap();
        assert!(evidence.track.mdat_preserved);
        assert!(evidence.track.metadata_round_trip_passed);
        assert_eq!(
            evidence.track.source_mdat_sha256,
            evidence.track.output_mdat_sha256
        );
        let album = evidence.album_loudness.unwrap();
        assert_eq!(album.reference_count, 2);
        assert_eq!(album.max_album_references, 2);
        assert_eq!(
            album
                .references
                .iter()
                .filter(|item| item.track_reference)
                .count(),
            1
        );
        assert_eq!(
            album.complete_gating_blocks,
            u64::try_from(combined_blocks.len()).unwrap()
        );
        assert!((album.measured_program_loudness_lufs - expected_lufs).abs() < 1e-12);
        let expected_encoded = isobmff_loudness_repair::encode_values(
            expected_lufs,
            quiet_analysis
                .loudness
                .sample_peak_dbfs
                .max(loud_analysis.loudness.sample_peak_dbfs),
            quiet_analysis
                .loudness
                .true_peak_dbtp
                .max(loud_analysis.loudness.true_peak_dbtp),
        )
        .unwrap();
        assert_eq!(
            album.encoded_program_loudness_lkfs,
            expected_encoded.program_loudness_lkfs
        );
        let scopes = report.report.after.properties["tracks"][0]["loudness"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["scope"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(scopes, vec!["track", "album"]);
        assert!(destination.exists());
    }

    #[test]
    fn schema_v1_rejects_album_fields_and_v2_bounds_album_references() {
        let v1 = serde_json::json!({
            "schema_version": 1,
            "source": "source.m4a",
            "destination": "destination.m4a",
            "isobmff_loudness": {
                "album_decoded_references": ["source.m4a"]
            }
        });
        let v1_schema: serde_json::Value = serde_json::from_str(include_str!(
            "../schema/metadata-repair-request-v1.schema.json"
        ))
        .unwrap();
        assert!(!jsonschema::validator_for(&v1_schema).unwrap().is_valid(&v1));
        let error = parse_versioned_spec(Path::new("request.json"), &v1.to_string()).unwrap_err();
        assert!(error.contains("unknown field"), "{error}");

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.m4a");
        let source_reference = directory.path().join("source.wav");
        let companion_reference = directory.path().join("companion.wav");
        pcm_mp4_with(&source, 24_000, 0.1);
        classic_stereo_wave_with(&source_reference, 24_000, 0.1);
        classic_stereo_wave_with(&companion_reference, 24_000, 0.2);
        let request_path = directory.path().join("request.json");

        let duplicate = evaluate_isobmff_loudness_v2(
            &request_path,
            repair_spec(
                source.clone(),
                directory.path().join("duplicate.m4a"),
                SCHEMA_VERSION_V2,
            ),
            IsobmffLoudnessRepairV2 {
                decoded_reference: Some(source_reference.clone()),
                album_decoded_references: Some(vec![
                    source_reference.clone(),
                    source_reference.clone(),
                ]),
                max_album_references: 2,
                max_decoded_samples: 200_000,
            },
        )
        .unwrap_err();
        assert!(duplicate.contains("duplicate file"), "{duplicate}");

        #[cfg(any(unix, windows))]
        {
            let hardlink = directory.path().join("source-hardlink.wav");
            fs::hard_link(&source_reference, &hardlink).unwrap();
            let duplicate_identity = evaluate_isobmff_loudness_v2(
                &request_path,
                repair_spec(
                    source.clone(),
                    directory.path().join("duplicate-hardlink.m4a"),
                    SCHEMA_VERSION_V2,
                ),
                IsobmffLoudnessRepairV2 {
                    decoded_reference: Some(source_reference.clone()),
                    album_decoded_references: Some(vec![
                        source_reference.clone(),
                        hardlink.clone(),
                    ]),
                    max_album_references: 2,
                    max_decoded_samples: 200_000,
                },
            )
            .unwrap_err();
            assert!(
                duplicate_identity.contains("duplicate file"),
                "{duplicate_identity}"
            );

            let alias_result = evaluate_isobmff_loudness_v2(
                &request_path,
                repair_spec(
                    source.clone(),
                    directory.path().join("hardlink-track.m4a"),
                    SCHEMA_VERSION_V2,
                ),
                IsobmffLoudnessRepairV2 {
                    decoded_reference: Some(source_reference.clone()),
                    album_decoded_references: Some(vec![hardlink, companion_reference.clone()]),
                    max_album_references: 2,
                    max_decoded_samples: 200_000,
                },
            )
            .unwrap();
            let album = alias_result
                .isobmff_loudness
                .unwrap()
                .album_loudness
                .unwrap();
            assert!(album.references[0].track_reference);
            assert!(!album.references[1].track_reference);
        }

        let missing = evaluate_isobmff_loudness_v2(
            &request_path,
            repair_spec(
                source.clone(),
                directory.path().join("missing.m4a"),
                SCHEMA_VERSION_V2,
            ),
            IsobmffLoudnessRepairV2 {
                decoded_reference: Some(source_reference.clone()),
                album_decoded_references: Some(vec![companion_reference.clone()]),
                max_album_references: 2,
                max_decoded_samples: 200_000,
            },
        )
        .unwrap_err();
        assert!(missing.contains("exactly once; found 0"), "{missing}");

        let too_many = validate_spec(
            &repair_spec(
                source.clone(),
                directory.path().join("too-many.m4a"),
                SCHEMA_VERSION_V2,
            ),
            Some(&IsobmffOptions::V2(IsobmffLoudnessRepairV2 {
                decoded_reference: Some(source_reference.clone()),
                album_decoded_references: Some(vec![
                    source_reference.clone(),
                    companion_reference.clone(),
                ]),
                max_album_references: 1,
                max_decoded_samples: 200_000,
            })),
        )
        .unwrap_err();
        assert!(
            too_many.contains("above max_album_references 1"),
            "{too_many}"
        );

        let source_bytes = fs::metadata(&source).unwrap().len();
        let source_reference_bytes = fs::metadata(&source_reference).unwrap().len();
        let companion_reference_bytes = fs::metadata(&companion_reference).unwrap().len();
        let reference_bytes = source_reference_bytes
            .checked_add(companion_reference_bytes)
            .unwrap();
        let largest_input = source_bytes
            .max(source_reference_bytes)
            .max(companion_reference_bytes);
        assert!(largest_input < reference_bytes);
        let mut byte_limited_spec = repair_spec(
            source.clone(),
            directory.path().join("byte-limited.m4a"),
            SCHEMA_VERSION_V2,
        );
        byte_limited_spec.max_input_bytes = largest_input + (reference_bytes - largest_input) / 2;
        assert!(byte_limited_spec.max_input_bytes > largest_input);
        assert!(byte_limited_spec.max_input_bytes < reference_bytes);
        let byte_limit = evaluate_isobmff_loudness_v2(
            &request_path,
            byte_limited_spec,
            IsobmffLoudnessRepairV2 {
                decoded_reference: Some(source_reference.clone()),
                album_decoded_references: Some(vec![
                    source_reference.clone(),
                    companion_reference.clone(),
                ]),
                max_album_references: 2,
                max_decoded_samples: 200_000,
            },
        )
        .unwrap_err();
        assert!(
            byte_limit.contains("remaining aggregate budget"),
            "{byte_limit}"
        );

        let sample_limit = evaluate_isobmff_loudness_v2(
            &request_path,
            repair_spec(
                source.clone(),
                directory.path().join("sample-limited.m4a"),
                SCHEMA_VERSION_V2,
            ),
            IsobmffLoudnessRepairV2 {
                decoded_reference: Some(source_reference.clone()),
                album_decoded_references: Some(vec![source_reference, companion_reference]),
                max_album_references: 2,
                max_decoded_samples: 60_000,
            },
        )
        .unwrap_err();
        assert!(
            sample_limit.contains("aggregate max_decoded_samples (60000)"),
            "{sample_limit}"
        );
    }

    #[test]
    fn isobmff_decoded_sample_limit_is_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("long.wav");
        classic_stereo_wave(&source);
        let error = isobmff_loudness_repair::analyze_reference(&source, 100).unwrap_err();
        assert!(error.contains("exceeds max_decoded_samples (100)"));
    }

    #[test]
    fn sowt_m4a_without_decoded_reference_rejects_ambiguous_layout() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.m4a");
        let destination = directory.path().join("destination.m4a");
        pcm_mp4(&source);

        let error = evaluate_isobmff_loudness(
            &directory.path().join("request.json"),
            repair_spec(source, destination, SCHEMA_VERSION),
            IsobmffLoudnessRepair {
                decoded_reference: None,
                max_decoded_samples: 200_000,
            },
        )
        .unwrap_err();
        assert!(error.contains("ambiguous 2-channel layout"), "{error}");
    }

    #[test]
    fn isobmff_target_rejects_conflicting_loudness_systems() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.m4a");
        pcm_mp4(&source);
        let mut audit = container_qc::audit(&source).unwrap();

        audit.properties["tracks"][0]["codecs"] = serde_json::json!(["apac"]);
        let error = isobmff_loudness_repair::select_target(&audit).unwrap_err();
        assert!(error.contains("presentation-aware"));

        audit.properties["tracks"][0]["codecs"] = serde_json::json!(["sowt"]);
        audit.properties["tracks"][0]["xhe_aac_usac_config"] = serde_json::json!({});
        let error = isobmff_loudness_repair::select_target(&audit).unwrap_err();
        assert!(error.contains("xHE-AAC"));
    }

    #[test]
    fn parses_extended_toml_request_without_changing_legacy_spec() {
        let text = r#"
schema_version = 1
source = "source.m4a"
destination = "destination.m4a"

[isobmff_loudness]
decoded_reference = "reference.wav"
max_decoded_samples = 12345
"#;
        let (spec, options) = parse_extended_spec(Path::new("request.toml"), text).unwrap();
        assert_eq!(spec.source, Path::new("source.m4a"));
        let options = options.unwrap();
        assert_eq!(
            options.decoded_reference.as_deref(),
            Some(Path::new("reference.wav"))
        );
        assert_eq!(options.max_decoded_samples, 12_345);
    }
}
