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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-metadata-repair-1";
pub const DEFAULT_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_METADATA_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_XML_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_CHUNKS: u32 = 100_000;
pub const DEFAULT_MAX_DECODED_SAMPLES: u64 = 500_000_000;
pub const HARD_MAX_DECODED_SAMPLES: u64 = 4_000_000_000;

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

#[derive(Debug, Clone)]
struct WaveChunkInfo {
    id: [u8; 4],
    body_offset: u64,
    body_size: u64,
}

struct PreparedIsobmffLoudness {
    target: TargetTrack,
    reference: PathBuf,
    reference_sha256: String,
    reference_is_source: bool,
    measured: DecodedLoudness,
    encoded: EncodedLoudness,
    source_mdat_sha256: Option<String>,
    max_decoded_samples: u64,
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
    evaluate_internal(path, spec, isobmff_loudness)
}

/// Evaluate one request.  The destination is always a separate path; this is
/// an intentional guard against accidental in-place audio replacement.
pub fn evaluate(
    request_path: &Path,
    spec: MetadataRepairSpec,
) -> Result<MetadataRepairReport, String> {
    evaluate_internal(request_path, spec, None).map(|extended| extended.report)
}

/// Evaluate one ISO-BMFF loudness repair without extending the shape of the
/// stable [`MetadataRepairSpec`] or [`MetadataRepairReport`] Rust types.
pub fn evaluate_isobmff_loudness(
    request_path: &Path,
    spec: MetadataRepairSpec,
    options: IsobmffLoudnessRepair,
) -> Result<ExtendedMetadataRepairReport, String> {
    evaluate_internal(request_path, spec, Some(options))
}

fn evaluate_internal(
    request_path: &Path,
    spec: MetadataRepairSpec,
    isobmff_options: Option<IsobmffLoudnessRepair>,
) -> Result<ExtendedMetadataRepairReport, String> {
    validate_spec(&spec, isobmff_options.as_ref())?;
    let base = request_path.parent().unwrap_or_else(|| Path::new("."));
    let source = resolve(base, &spec.source);
    let destination = resolve(base, &spec.destination);
    let source_meta = fs::metadata(&source)
        .map_err(|error| format!("stat metadata source {}: {error}", source.display()))?;
    let source_bytes = source_meta.len();
    if source_bytes > spec.max_input_bytes {
        return Err(format!(
            "metadata source {} is {source_bytes} bytes, above max_input_bytes {}",
            source.display(),
            spec.max_input_bytes
        ));
    }
    if same_path(&source, &destination)? {
        return Err("metadata repair destination must differ from source".into());
    }
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

    let before = container_qc::audit(&source)?;
    let source_format = before.format.clone();
    let adm_before = read_adm_profile(&source, &source_format)?;
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
        let target = isobmff_loudness_repair::select_target(&before)?;
        let reference = options
            .decoded_reference
            .as_ref()
            .map(|path| resolve(base, path))
            .unwrap_or_else(|| source.clone());
        let reference_meta = fs::metadata(&reference).map_err(|error| {
            format!(
                "stat ISO-BMFF decoded reference {}: {error}",
                reference.display()
            )
        })?;
        if reference_meta.len() > spec.max_input_bytes {
            return Err(format!(
                "ISO-BMFF decoded reference {} is {} bytes, above max_input_bytes {}",
                reference.display(),
                reference_meta.len(),
                spec.max_input_bytes
            ));
        }
        let reference_is_source = same_path(&source, &reference)?;
        let measured =
            isobmff_loudness_repair::measure_reference(&reference, options.max_decoded_samples)?;
        isobmff_loudness_repair::validate_reference_geometry(&target, &measured)?;
        let encoded = isobmff_loudness_repair::encode_measurement(&measured)?;
        let reference_sha256 = sha256_file(&reference, spec.max_input_bytes)?;
        let source_mdat_sha256 =
            isobmff_loudness_repair::mdat_sha256(&source, spec.max_input_bytes, spec.max_chunks)?;
        Some(PreparedIsobmffLoudness {
            target,
            reference,
            reference_sha256,
            reference_is_source,
            measured,
            encoded,
            source_mdat_sha256,
            max_decoded_samples: options.max_decoded_samples,
        })
    } else {
        None
    };

    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let mut isobmff_rewrite = None;
    let changed = if has_wave_mutation {
        let result = write_output(
            &destination,
            spec.overwrite,
            spec.atomic_replace,
            spec.max_output_bytes,
            |output| rewrite_wave(&source, output, &spec, &mut actions, &mut warnings),
        )?;
        let _ = result;
        actions.iter().any(|action| action.changed)
    } else if let Some(prepared) = &prepared_isobmff {
        write_output(
            &destination,
            spec.overwrite,
            spec.atomic_replace,
            spec.max_output_bytes,
            |output| {
                let result = isobmff_loudness_repair::rewrite(
                    &source,
                    output,
                    prepared.target.track_id,
                    &prepared.encoded,
                    spec.max_input_bytes,
                    spec.max_metadata_chunk_bytes,
                    spec.max_chunks,
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
                "{} ISO-BMFF ludt/tlou for audio track {}; moov delta {:+} byte(s), adjusted {} chunk offset(s)",
                if result.replaced_existing { "replaced" } else { "inserted" },
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
            |output| copy_bounded(&source, output, spec.max_input_bytes),
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
    };

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
    let isobmff_loudness = if let Some(prepared) = prepared_isobmff {
        let rewrite =
            isobmff_rewrite.expect("prepared ISO-BMFF loudness mutation has rewrite evidence");
        let output_mdat_sha256 = isobmff_loudness_repair::mdat_sha256(
            &destination,
            spec.max_output_bytes,
            spec.max_chunks,
        )?;
        let mdat_preserved = prepared.source_mdat_sha256 == output_mdat_sha256;
        let metadata_round_trip_passed =
            verify_isobmff_loudness_round_trip(&after, prepared.target.track_id, &prepared.encoded);
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
        Some(IsobmffLoudnessEvidence {
            track_id: prepared.target.track_id,
            codecs: prepared.target.codecs,
            decoded_reference: prepared.reference.to_string_lossy().into_owned(),
            decoded_reference_sha256: prepared.reference_sha256,
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
        })
    } else {
        None
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
            "ISO-BMFF media payloads were hash-verified byte-for-byte; only ludt/tlou, ancestor sizes, and required stco/co64 offsets changed".into(),
        );
    }
    let report = MetadataRepairReport {
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        classification: "bounded metadata repair; delivery requires post-repair QC review",
        mode: spec.mode,
        source: source.to_string_lossy().into_owned(),
        destination: destination.to_string_lossy().into_owned(),
        source_bytes,
        output_bytes,
        source_sha256: sha256_file(&source, spec.max_input_bytes)?,
        output_sha256: sha256_file(&destination, spec.max_output_bytes)?,
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
    Ok(ExtendedMetadataRepairReport {
        report,
        isobmff_loudness,
    })
}

fn parse_extended_spec(
    path: &Path,
    text: &str,
) -> Result<(MetadataRepairSpec, Option<IsobmffLoudnessRepair>), String> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => {
            let mut value: serde_json::Value = serde_json::from_str(text)
                .map_err(|error| format!("parse metadata repair JSON: {error}"))?;
            let object = value
                .as_object_mut()
                .ok_or("metadata repair JSON request must be an object")?;
            let isobmff_loudness = object
                .remove("isobmff_loudness")
                .map(serde_json::from_value::<Option<IsobmffLoudnessRepair>>)
                .transpose()
                .map_err(|error| format!("parse metadata repair JSON isobmff_loudness: {error}"))?
                .flatten();
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
            let isobmff_loudness = table
                .remove("isobmff_loudness")
                .map(|value| value.try_into::<IsobmffLoudnessRepair>())
                .transpose()
                .map_err(|error| format!("parse metadata repair TOML isobmff_loudness: {error}"))?;
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
    isobmff_options: Option<&IsobmffLoudnessRepair>,
) -> Result<(), String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported metadata repair schema_version {}; expected {}",
            spec.schema_version, SCHEMA_VERSION
        ));
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
        if options.max_decoded_samples == 0
            || options.max_decoded_samples > HARD_MAX_DECODED_SAMPLES
        {
            return Err(format!(
                "metadata repair isobmff_loudness.max_decoded_samples must be 1..={HARD_MAX_DECODED_SAMPLES}"
            ));
        }
        if options
            .decoded_reference
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(
                "metadata repair isobmff_loudness.decoded_reference must not be empty".into(),
            );
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

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn same_path(source: &Path, destination: &Path) -> Result<bool, String> {
    if source == destination {
        return Ok(true);
    }
    let source = fs::canonicalize(source)
        .map_err(|error| format!("canonicalize metadata source {}: {error}", source.display()))?;
    let destination = if destination.exists() {
        fs::canonicalize(destination).map_err(|error| {
            format!(
                "canonicalize metadata destination {}: {error}",
                destination.display()
            )
        })?
    } else {
        destination.to_path_buf()
    };
    Ok(source == destination)
}

fn read_adm_profile(path: &Path, format: &str) -> Result<Option<ProductionProfileResult>, String> {
    if format != "wave" || metadata::read_wave_chunk(path, *b"axml")?.is_none() {
        return Ok(None);
    }
    adm::validate_production_profile(path, ProductionProfileMode::Read).map(Some)
}

fn verify_isobmff_loudness_round_trip(
    audit: &ContainerAudit,
    track_id: u32,
    expected: &EncodedLoudness,
) -> bool {
    let Some(tracks) = audit.properties["tracks"].as_array() else {
        return false;
    };
    let matching = tracks
        .iter()
        .filter(|track| track["track_id"].as_u64() == Some(u64::from(track_id)))
        .collect::<Vec<_>>();
    if matching.len() != 1 || matching[0]["loudness_box_count"].as_u64() != Some(1) {
        return false;
    }
    let Some(entries) = matching[0]["loudness"].as_array() else {
        return false;
    };
    let track_entries = entries
        .iter()
        .filter(|entry| entry["scope"].as_str() == Some("track"))
        .collect::<Vec<_>>();
    if track_entries.len() != 1 {
        return false;
    }
    let entry = track_entries[0];
    if entry["version"].as_u64() != Some(0)
        || !entry["eq_set_id"].is_null()
        || entry["downmix_id"].as_u64() != Some(0)
        || entry["drc_set_id"].as_u64() != Some(0)
        || entry["sample_peak_code"].as_i64() != Some(i64::from(expected.sample_peak_code))
        || entry["true_peak_code"].as_i64() != Some(i64::from(expected.true_peak_code))
        || entry["true_peak_measurement_system"].as_u64() != Some(2)
        || entry["true_peak_reliability"].as_u64() != Some(3)
    {
        return false;
    }
    let Some(measurements) = entry["measurements"].as_array() else {
        return false;
    };
    measurements.len() == 1
        && measurements[0]["method_definition"].as_u64() == Some(1)
        && measurements[0]["method_value"].as_u64() == Some(u64::from(expected.program_code))
        && measurements[0]["measurement_system"].as_u64() == Some(2)
        && measurements[0]["reliability"].as_u64() == Some(3)
        && measurements[0]["value_lkfs"]
            .as_f64()
            .is_some_and(|value| (value - expected.program_loudness_lkfs).abs() < 1e-12)
}

fn write_output(
    destination: &Path,
    overwrite: bool,
    atomic_replace: bool,
    max_output_bytes: u64,
    mut write: impl FnMut(&mut dyn Write) -> Result<u64, String>,
) -> Result<u64, String> {
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
        if destination.exists() && !overwrite {
            return Err(format!(
                "metadata repair destination already exists: {} (pass overwrite=true)",
                destination.display()
            ));
        }
        temp.persist(destination).map_err(|error| {
            format!(
                "atomically replace metadata repair destination {}: {}",
                destination.display(),
                error.error
            )
        })?;
        Ok(bytes)
    } else {
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        if overwrite {
            options.truncate(true);
        } else {
            options.create_new(true);
        }
        let mut output = options.open(destination).map_err(|error| {
            format!(
                "create metadata repair output {}: {error}",
                destination.display()
            )
        })?;
        let bytes = write(&mut output)?;
        if bytes > max_output_bytes {
            drop(output);
            let _ = fs::remove_file(destination);
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
    let mut input =
        File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
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
    let mut position = 12_u64;
    let mut chunks = Vec::new();
    while position < file_size {
        if chunks.len() >= max_chunks as usize {
            return Err(format!("WAVE contains more than max_chunks ({max_chunks})"));
        }
        if file_size - position < 8 {
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
        if next > file_size {
            return Err("WAVE chunk extends beyond end of file".into());
        }
        chunks.push(WaveChunkInfo {
            id,
            body_offset,
            body_size: size,
        });
        position = next;
    }
    if position != file_size {
        return Err("WAVE contains unaccounted trailing bytes".into());
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
        const SAMPLE_RATE: u32 = 48_000;
        const FRAMES: u32 = 48_000;
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
                    FRAMES.to_be_bytes(),
                    1_u32.to_be_bytes(),
                ]
                .concat(),
            ),
        );
        let stsz = mp4_box(
            b"stsz",
            mp4_full_box(0, [4_u32.to_be_bytes(), FRAMES.to_be_bytes()].concat()),
        );
        let stsc = mp4_box(
            b"stsc",
            mp4_full_box(
                0,
                [
                    1_u32.to_be_bytes(),
                    1_u32.to_be_bytes(),
                    FRAMES.to_be_bytes(),
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
            tkhd[20..24].copy_from_slice(&FRAMES.to_be_bytes());
            let tkhd = mp4_box(b"tkhd", tkhd);
            let mdhd = mp4_box(
                b"mdhd",
                mp4_full_box(
                    0,
                    [
                        vec![0; 8],
                        SAMPLE_RATE.to_be_bytes().to_vec(),
                        FRAMES.to_be_bytes().to_vec(),
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
                        FRAMES.to_be_bytes().to_vec(),
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
        let mut pcm = Vec::with_capacity(FRAMES as usize * 4);
        for frame in 0..FRAMES {
            let phase = std::f32::consts::TAU * 440.0 * frame as f32 / SAMPLE_RATE as f32;
            let sample = (phase.sin() * 0.1 * i16::MAX as f32).round() as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        fs::write(path, [ftyp, moov, mp4_box(b"mdat", pcm)].concat()).unwrap();
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
        let destination = directory.path().join("destination.m4a");
        pcm_mp4(&source);
        let before = container_qc::audit(&source).unwrap();
        assert!(before.passed, "{before:#?}");
        let request_json = serde_json::json!({
            "schema_version": 1,
            "source": source,
            "destination": destination,
            "isobmff_loudness": {"max_decoded_samples": 200_000},
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
        assert_eq!(parsed_options.unwrap().max_decoded_samples, 200_000);
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
                decoded_reference: None,
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

    #[test]
    fn isobmff_decoded_sample_limit_is_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.m4a");
        pcm_mp4(&source);
        let error = isobmff_loudness_repair::measure_reference(&source, 100).unwrap_err();
        assert!(error.contains("exceeds max_decoded_samples (100)"));
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
