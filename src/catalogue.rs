//! Durable SQLite catalogue for measured and normalized audio assets.
//!
//! The catalogue is an opt-in index, not an analysis cache. Each row binds the
//! exact source and optional output byte streams to the measurement method,
//! selected profile, Forge version, measurements, and caller-supplied
//! provenance evidence.

use crate::analysis::Analysis;
use crate::analysis_cache::{ALGORITHM_REVISION, MEASUREMENT_STANDARD};
use crate::atomic::AtomicOutput;
use crate::channel_layout::ChannelLayoutDescriptor;
use crate::decoder::{ChannelLayoutProvenance, InputDescriptor, InputDescriptorOptions};
use crate::dsp::resample::ResampleQuality;
use crate::normalization_diff::{self, FileEvidence, MeasurementEvidence};
use crate::normalize::{Mode, Plan};
use crate::stable_input::{StableInput, StableInputOptions};
use crate::wav::{ChannelRole, PcmKind, WavContainer};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Schema URI used by exported catalogue provenance reports.
pub const CATALOGUE_REPORT_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/catalogue-report-v1";
/// Current schema URI used by exported catalogue provenance reports.
pub const CATALOGUE_REPORT_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/catalogue-report-v2";
/// Exact-layout schema URI used by current catalogue provenance reports.
pub const CATALOGUE_REPORT_SCHEMA_V3: &str =
    "https://penguin425.github.io/audio-normalizer/schema/catalogue-report-v3";
/// Canonical request identity embedded in catalogue v2 records.
pub const CATALOGUE_REQUEST_SCHEMA_V1: &str = "forge-catalogue-request-v1";
/// Canonical request identity carrying exact channel-layout evidence.
pub const CATALOGUE_REQUEST_SCHEMA_V2: &str = "forge-catalogue-request-v2";
/// SQLite schema version stored in `PRAGMA user_version`.
pub const CATALOGUE_DATABASE_VERSION: i32 = 2;
/// Maximum serialized provenance document accepted for one catalogue row.
pub const MAX_PROVENANCE_BYTES: usize = 1024 * 1024;
/// Maximum number of rows exported in one provenance report.
pub const MAX_REPORT_RECORDS: usize = 100_000;

const APPLICATION_ID: i32 = 0x464f_5247; // "FORG"
const MAX_TEXT_BYTES: usize = 4096;
const LEGACY_REQUEST_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Exact input-selection and processing identity used by catalogue v2.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueRequestEvidence {
    pub schema: &'static str,
    pub input_descriptor: CatalogueInputDescriptorEvidence,
    pub renderer: String,
    pub effective_plan: CataloguePlanEvidence,
}

/// Exact-layout input-selection and processing identity used by catalogue v3.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueRequestEvidenceV2 {
    pub schema: &'static str,
    pub input_descriptor: CatalogueInputDescriptorEvidenceV2,
    pub renderer: String,
    pub effective_plan: CataloguePlanEvidence,
}

/// Serializable subset of [`InputDescriptor`] that affects measured PCM.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueInputDescriptorEvidence {
    pub version: u32,
    pub decoder_route: String,
    pub container: String,
    pub codec: String,
    pub audio_track_index: u32,
    pub audio_track_id: u32,
    pub source_start_frame: u64,
    pub source_frames: Option<u64>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub channel_roles: Vec<String>,
    pub declared_layout_provenance: String,
    pub explicit_channel_roles: bool,
}

/// Serializable descriptor identity retaining container and effective layouts.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueInputDescriptorEvidenceV2 {
    pub version: u32,
    pub decoder_route: String,
    pub container: String,
    pub codec: String,
    pub audio_track_index: u32,
    pub audio_track_id: u32,
    pub source_start_frame: u64,
    pub source_frames: Option<u64>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub channel_roles: Vec<String>,
    pub declared_layout_provenance: String,
    pub explicit_channel_roles: bool,
    pub declared_channel_layout: ChannelLayoutDescriptor,
    pub effective_channel_layout: ChannelLayoutDescriptor,
    pub explicit_channel_layout: bool,
}

/// Complete resolved normalization plan used for a catalogue request.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CataloguePlanEvidence {
    pub mode: String,
    pub target_lufs: f64,
    pub target_peak_dbfs: f64,
    pub target_rms_dbfs: f64,
    pub ceiling_dbtp: f64,
    pub max_gain_db: Option<f64>,
    pub dither: bool,
    pub output_pcm_kind: Option<String>,
    pub mp3_bitrate_kbps: i32,
    pub mp3_quality: i32,
    pub limiter: Option<CatalogueLimiterEvidence>,
    pub wav_container: String,
    pub bwf: bool,
    pub output_sample_rate_hz: Option<u32>,
    pub resample_quality: String,
}

/// Effective look-ahead limiter settings bound into catalogue evidence.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueLimiterEvidence {
    pub lookahead_ms: f64,
    pub release_ms: f64,
}

impl CatalogueRequestEvidence {
    fn new(descriptor: &InputDescriptor, plan: &Plan, renderer: &str) -> Self {
        let info = descriptor.stream_info();
        Self {
            schema: CATALOGUE_REQUEST_SCHEMA_V1,
            input_descriptor: CatalogueInputDescriptorEvidence {
                version: descriptor.version(),
                decoder_route: descriptor.decoder_route_id(),
                container: descriptor.container().id().to_owned(),
                codec: descriptor.codec().id().to_owned(),
                audio_track_index: descriptor.track_index(),
                audio_track_id: descriptor.track_id(),
                source_start_frame: descriptor.source_range().start(),
                source_frames: descriptor.source_range().frames(),
                sample_rate_hz: info.sample_rate,
                channels: info.channels,
                channel_roles: info.channel_roles.iter().map(channel_role_id).collect(),
                declared_layout_provenance: layout_provenance_id(
                    descriptor.declared_layout_provenance(),
                )
                .to_owned(),
                explicit_channel_roles: descriptor.uses_explicit_channel_roles(),
            },
            renderer: renderer.to_owned(),
            effective_plan: catalogue_plan_evidence(plan),
        }
    }
}

fn catalogue_plan_evidence(plan: &Plan) -> CataloguePlanEvidence {
    CataloguePlanEvidence {
        mode: mode_id(plan.mode).to_owned(),
        target_lufs: plan.target_lufs,
        target_peak_dbfs: plan.target_peak_db,
        target_rms_dbfs: plan.target_rms_db,
        ceiling_dbtp: plan.ceiling_db,
        max_gain_db: plan.max_gain_db,
        dither: plan.dither,
        output_pcm_kind: plan.output_kind.map(pcm_kind_id).map(ToOwned::to_owned),
        mp3_bitrate_kbps: plan.mp3_bitrate,
        mp3_quality: plan.mp3_quality,
        limiter: plan.limiter.map(|limiter| CatalogueLimiterEvidence {
            lookahead_ms: limiter.lookahead_ms,
            release_ms: limiter.release_ms,
        }),
        wav_container: wav_container_id(plan.wav_container).to_owned(),
        bwf: plan.bwf,
        output_sample_rate_hz: plan.output_sample_rate,
        resample_quality: resample_quality_id(plan.resample_quality).to_owned(),
    }
}

impl CatalogueRequestEvidenceV2 {
    fn new(descriptor: &InputDescriptor, plan: &Plan, renderer: &str) -> Self {
        let info = descriptor.stream_info();
        Self {
            schema: CATALOGUE_REQUEST_SCHEMA_V2,
            input_descriptor: CatalogueInputDescriptorEvidenceV2 {
                version: descriptor.version(),
                decoder_route: descriptor.decoder_route_id(),
                container: descriptor.container().id().to_owned(),
                codec: descriptor.codec().id().to_owned(),
                audio_track_index: descriptor.track_index(),
                audio_track_id: descriptor.track_id(),
                source_start_frame: descriptor.source_range().start(),
                source_frames: descriptor.source_range().frames(),
                sample_rate_hz: info.sample_rate,
                channels: info.channels,
                channel_roles: info.channel_roles.iter().map(channel_role_id).collect(),
                declared_layout_provenance: layout_provenance_id(
                    descriptor.declared_layout_provenance(),
                )
                .to_owned(),
                explicit_channel_roles: descriptor.uses_explicit_channel_roles(),
                declared_channel_layout: descriptor.declared_channel_layout().clone(),
                effective_channel_layout: descriptor.channel_layout().clone(),
                explicit_channel_layout: descriptor.uses_explicit_channel_layout(),
            },
            renderer: renderer.to_owned(),
            effective_plan: catalogue_plan_evidence(plan),
        }
    }
}

/// One asset to hash and add to a [`Catalogue`].
#[derive(Debug)]
pub struct CatalogueAsset<'a> {
    /// Source audio path.
    pub source: &'a Path,
    /// SHA-256 captured before the associated measurement began.
    pub expected_source_sha256: &'a str,
    /// Successfully created output, when this is a normalization record.
    pub output: Option<&'a Path>,
    /// Measurement associated with the source audio.
    pub measurement: &'a Analysis,
    /// Stable operation identifier such as `analysis` or `normalization`.
    pub operation: &'a str,
    /// User-visible preset, compliance profile, or explicit custom profile.
    pub profile: &'a str,
    /// Structured evidence describing the invocation and profile selection.
    pub provenance: Value,
}

/// Self-contained evidence returned after a catalogue row is committed.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueRecord {
    /// Stable row identifier within the SQLite database.
    pub id: i64,
    /// Milliseconds since the Unix epoch at commit time.
    pub recorded_unix_ms: i64,
    /// Operation that created the row.
    pub operation: String,
    /// Exact source file evidence.
    pub source: FileEvidence,
    /// Exact output file evidence for normalization rows.
    pub output: Option<FileEvidence>,
    /// Measurement standard applied to the source.
    pub measurement_standard: String,
    /// Forge measurement implementation revision.
    pub measurement_version: String,
    /// Selected preset, compliance profile, or custom profile.
    pub profile: String,
    /// Forge package version.
    pub tool_version: String,
    /// Source loudness and peak measurements.
    pub measurement: MeasurementEvidence,
    /// Structured invocation and profile provenance.
    pub provenance: Value,
}

/// Descriptor-bound catalogue evidence returned by the v2 recording API.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueRecordV2 {
    /// Stable row identifier within the SQLite database.
    pub id: i64,
    /// Milliseconds since the Unix epoch at commit time.
    pub recorded_unix_ms: i64,
    /// Operation that created the row.
    pub operation: String,
    /// Exact source file evidence.
    pub source: FileEvidence,
    /// Exact output file evidence for normalization rows.
    pub output: Option<FileEvidence>,
    /// Measurement standard applied to the source.
    pub measurement_standard: String,
    /// Forge measurement implementation revision.
    pub measurement_version: String,
    /// Selected preset, compliance profile, or custom profile.
    pub profile: String,
    /// Forge package version.
    pub tool_version: String,
    /// SHA-256 of the canonical input selection, renderer, and effective plan.
    pub request_sha256: String,
    /// Exact request evidence covered by `request_sha256`.
    pub request: Value,
    /// Source loudness and peak measurements.
    pub measurement: MeasurementEvidence,
    /// Structured invocation and profile provenance.
    pub provenance: Value,
}

impl CatalogueRecordV2 {
    fn into_legacy(self) -> CatalogueRecord {
        CatalogueRecord {
            id: self.id,
            recorded_unix_ms: self.recorded_unix_ms,
            operation: self.operation,
            source: self.source,
            output: self.output,
            measurement_standard: self.measurement_standard,
            measurement_version: self.measurement_version,
            profile: self.profile,
            tool_version: self.tool_version,
            measurement: self.measurement,
            provenance: self.provenance,
        }
    }
}

/// Legacy v1 report containing rows committed by an invocation.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueReport {
    /// Stable report schema URI.
    pub schema: &'static str,
    /// Forge version that generated the report.
    pub generator: String,
    /// User-selected catalogue path.
    pub catalogue: String,
    /// Committed catalogue records.
    pub records: Vec<CatalogueRecord>,
}

/// Descriptor-bound v2 report containing rows committed by an invocation.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueReportV2 {
    /// Stable report schema URI.
    pub schema: &'static str,
    /// Forge version that generated the report.
    pub generator: String,
    /// User-selected catalogue path.
    pub catalogue: String,
    /// Committed descriptor-bound catalogue records.
    pub records: Vec<CatalogueRecordV2>,
}

/// Exact-layout v3 report containing rows committed by an invocation.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct CatalogueReportV3 {
    /// Stable report schema URI.
    pub schema: &'static str,
    /// Forge version that generated the report.
    pub generator: String,
    /// User-selected catalogue path.
    pub catalogue: String,
    /// Committed exact-layout catalogue records.
    pub records: Vec<CatalogueRecordV2>,
}

/// Connection to a versioned Forge SQLite catalogue.
#[derive(Debug)]
pub struct Catalogue {
    path: PathBuf,
    connection: Connection,
}

impl Catalogue {
    /// Open or create a Forge catalogue and validate its application/schema IDs.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        validate_catalogue_path(&path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let connection = Connection::open(&path)
            .map_err(|error| format!("open catalogue {}: {error}", path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("configure catalogue busy timeout: {error}"))?;
        initialize_schema(&connection)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA trusted_schema = OFF;
                 PRAGMA synchronous = FULL;
                 PRAGMA journal_mode = WAL;",
            )
            .map_err(|error| format!("configure catalogue {}: {error}", path.display()))?;
        Ok(Self { path, connection })
    }

    /// Path used to open this catalogue.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hash and atomically insert or refresh one compatibility row.
    ///
    /// New callers should use [`Self::record_bound`] or
    /// [`Self::record_bound_path`]. This method cannot identify a selected
    /// track or source range and therefore records an explicit legacy request.
    pub fn record(&mut self, asset: CatalogueAsset<'_>) -> Result<CatalogueRecord, String> {
        let source = normalization_diff::inspect_file(asset.source)?;
        if source.sha256 != asset.expected_source_sha256 {
            return Err(format!(
                "{} changed between pre-measurement hashing and catalogue commit",
                asset.source.display()
            ));
        }
        let request = serde_json::json!({
            "schema": CATALOGUE_REQUEST_SCHEMA_V1,
            "legacy": true
        });
        let request_sha256 = LEGACY_REQUEST_SHA256.to_owned();
        self.record_prepared(asset, source, request_sha256, request, None)
            .map(CatalogueRecordV2::into_legacy)
    }

    /// Record an asset using the exact descriptor that produced its
    /// measurement and render.
    pub fn record_bound(
        &mut self,
        asset: CatalogueAsset<'_>,
        descriptor: &InputDescriptor,
        plan: &Plan,
        renderer: &str,
    ) -> Result<CatalogueRecordV2, String> {
        self.record_bound_with_schema(asset, descriptor, plan, renderer, false)
    }

    /// Record an asset with the exact declared and effective channel layouts.
    pub fn record_bound_v3(
        &mut self,
        asset: CatalogueAsset<'_>,
        descriptor: &InputDescriptor,
        plan: &Plan,
        renderer: &str,
    ) -> Result<CatalogueRecordV2, String> {
        self.record_bound_with_schema(asset, descriptor, plan, renderer, true)
    }

    fn record_bound_with_schema(
        &mut self,
        asset: CatalogueAsset<'_>,
        descriptor: &InputDescriptor,
        plan: &Plan,
        renderer: &str,
        exact_layout: bool,
    ) -> Result<CatalogueRecordV2, String> {
        plan.validate()?;
        validate_label("renderer", renderer, 512)?;
        let source_path = descriptor.stable_input().source_path().ok_or_else(|| {
            "catalogue path records require a path-backed input descriptor".to_string()
        })?;
        if comparison_path(source_path)? != comparison_path(asset.source)? {
            return Err("catalogue input descriptor belongs to a different source path".into());
        }
        descriptor
            .stable_input()
            .verify_source()
            .map_err(|error| error.to_string())?;
        let binding = descriptor.stable_input().binding();
        let source_sha256 = binding.sha256_hex();
        if source_sha256 != asset.expected_source_sha256 {
            return Err(format!(
                "{} changed between pre-measurement hashing and catalogue commit",
                asset.source.display()
            ));
        }
        let source = FileEvidence {
            path: asset.source.to_string_lossy().into_owned(),
            bytes: binding.byte_len(),
            sha256: source_sha256,
        };
        let request = if exact_layout {
            serde_json::to_value(CatalogueRequestEvidenceV2::new(descriptor, plan, renderer))
        } else {
            serde_json::to_value(CatalogueRequestEvidence::new(descriptor, plan, renderer))
        }
        .map_err(|error| format!("encode catalogue request: {error}"))?;
        let request_sha256 = hash_request(&request)?;
        self.record_prepared(
            asset,
            source,
            request_sha256,
            request,
            Some(descriptor.stable_input()),
        )
    }

    /// Capture a path as one immutable descriptor and record its complete v2
    /// request identity.
    pub fn record_bound_path(
        &mut self,
        asset: CatalogueAsset<'_>,
        descriptor_options: InputDescriptorOptions,
        plan: &Plan,
        renderer: &str,
    ) -> Result<CatalogueRecordV2, String> {
        let stable_options = StableInputOptions::new(u64::MAX)
            .map_err(|error| error.to_string())?
            .with_source_name_hint(asset.source);
        let descriptor =
            InputDescriptor::from_path(asset.source, &stable_options, descriptor_options)?;
        self.record_bound(asset, &descriptor, plan, renderer)
    }

    /// Capture a path and record its current exact-layout request identity.
    pub fn record_bound_path_v3(
        &mut self,
        asset: CatalogueAsset<'_>,
        descriptor_options: InputDescriptorOptions,
        plan: &Plan,
        renderer: &str,
    ) -> Result<CatalogueRecordV2, String> {
        let stable_options = StableInputOptions::new(u64::MAX)
            .map_err(|error| error.to_string())?
            .with_source_name_hint(asset.source);
        let descriptor =
            InputDescriptor::from_path(asset.source, &stable_options, descriptor_options)?;
        self.record_bound_v3(asset, &descriptor, plan, renderer)
    }

    fn record_prepared(
        &mut self,
        asset: CatalogueAsset<'_>,
        source: FileEvidence,
        request_sha256: String,
        request: Value,
        stable_input: Option<&StableInput>,
    ) -> Result<CatalogueRecordV2, String> {
        validate_label("operation", asset.operation, 256)?;
        if !matches!(asset.operation, "analysis" | "normalization") {
            return Err("catalogue operation must be `analysis` or `normalization`".into());
        }
        validate_label("profile", asset.profile, MAX_TEXT_BYTES)?;
        validate_provenance(&asset.provenance)?;
        validate_request(&request_sha256, &request)?;
        let output = asset.output.map(inspect_stable_file).transpose()?;
        let measurement = MeasurementEvidence::from(asset.measurement);
        validate_measurement(&measurement)?;
        let recorded_unix_ms = unix_milliseconds()?;
        let tool_version = env!("CARGO_PKG_VERSION").to_string();
        let provenance_json = serde_json::to_string(&asset.provenance)
            .map_err(|error| format!("encode catalogue provenance: {error}"))?;
        let request_json = serde_json::to_string(&request)
            .map_err(|error| format!("encode catalogue request: {error}"))?;
        if let Some(input) = stable_input {
            input.verify_source().map_err(|error| {
                format!(
                    "{} changed before catalogue commit: {error}",
                    asset.source.display()
                )
            })?;
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("begin catalogue transaction: {error}"))?;
        let id = transaction
            .query_row(
                "INSERT INTO catalogue_entries (
                    recorded_unix_ms, operation,
                    source_path, source_bytes, source_sha256,
                    output_path, output_bytes, output_sha256,
                    request_sha256, request_json,
                    measurement_standard, measurement_version, profile, tool_version,
                    sample_rate_hz, channels, frames, duration_seconds,
                    integrated_lufs, loudness_range_lu, rms_dbfs,
                    sample_peak_dbfs, true_peak_dbtp, provenance_json
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
                 )
                 ON CONFLICT DO UPDATE SET
                    recorded_unix_ms = excluded.recorded_unix_ms,
                    source_path = excluded.source_path,
                    source_bytes = excluded.source_bytes,
                    output_path = excluded.output_path,
                    output_bytes = excluded.output_bytes,
                    request_json = excluded.request_json,
                    sample_rate_hz = excluded.sample_rate_hz,
                    channels = excluded.channels,
                    frames = excluded.frames,
                    duration_seconds = excluded.duration_seconds,
                    integrated_lufs = excluded.integrated_lufs,
                    loudness_range_lu = excluded.loudness_range_lu,
                    rms_dbfs = excluded.rms_dbfs,
                    sample_peak_dbfs = excluded.sample_peak_dbfs,
                    true_peak_dbtp = excluded.true_peak_dbtp,
                    provenance_json = excluded.provenance_json
                 RETURNING id",
                params![
                    recorded_unix_ms,
                    asset.operation,
                    source.path,
                    to_i64("source byte count", source.bytes)?,
                    source.sha256,
                    output.as_ref().map(|value| value.path.as_str()),
                    output
                        .as_ref()
                        .map(|value| to_i64("output byte count", value.bytes))
                        .transpose()?,
                    output.as_ref().map(|value| value.sha256.as_str()),
                    request_sha256,
                    request_json,
                    MEASUREMENT_STANDARD,
                    ALGORITHM_REVISION,
                    asset.profile,
                    tool_version,
                    i64::from(measurement.sample_rate_hz),
                    i64::from(measurement.channels),
                    to_i64(
                        "frame count",
                        u64::try_from(measurement.frames)
                            .map_err(|_| "frame count exceeds u64 range".to_string())?,
                    )?,
                    measurement.duration_seconds,
                    measurement.integrated_lufs,
                    measurement.loudness_range_lu,
                    measurement.rms_dbfs,
                    measurement.sample_peak_dbfs,
                    measurement.true_peak_dbtp,
                    provenance_json,
                ],
                |row| row.get(0),
            )
            .map_err(|error| format!("write catalogue row: {error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("commit catalogue row: {error}"))?;
        Ok(CatalogueRecordV2 {
            id,
            recorded_unix_ms,
            operation: asset.operation.to_string(),
            source,
            output,
            measurement_standard: MEASUREMENT_STANDARD.to_string(),
            measurement_version: ALGORITHM_REVISION.to_string(),
            profile: asset.profile.to_string(),
            tool_version,
            request_sha256,
            request,
            measurement,
            provenance: asset.provenance,
        })
    }

    /// Return the number of rows currently stored.
    pub fn len(&self) -> Result<u64, String> {
        self.connection
            .query_row("SELECT COUNT(*) FROM catalogue_entries", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|error| format!("count catalogue rows: {error}"))
            .and_then(|count| {
                u64::try_from(count).map_err(|_| "catalogue row count is negative".to_string())
            })
    }

    /// Return whether the catalogue contains no rows.
    pub fn is_empty(&self) -> Result<bool, String> {
        self.len().map(|length| length == 0)
    }

    /// Export compatibility records as an atomic catalogue-report-v1 document.
    pub fn write_report(&self, path: &Path, records: Vec<CatalogueRecord>) -> Result<(), String> {
        self.write_report_with_overwrite(path, records, true)
    }

    /// Atomically export a catalogue-report-v1 document with a replacement policy.
    pub fn write_report_with_overwrite(
        &self,
        path: &Path,
        records: Vec<CatalogueRecord>,
        overwrite: bool,
    ) -> Result<(), String> {
        if records.len() > MAX_REPORT_RECORDS {
            return Err(format!(
                "catalogue report contains {} records; limit is {MAX_REPORT_RECORDS}",
                records.len()
            ));
        }
        if comparison_path(path)? == comparison_path(&self.path)? {
            return Err("catalogue report must not overwrite the SQLite catalogue".into());
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let report = CatalogueReport {
            schema: CATALOGUE_REPORT_SCHEMA_V1,
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            catalogue: self.path.to_string_lossy().into_owned(),
            records,
        };
        let mut bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("encode catalogue report: {error}"))?;
        bytes.push(b'\n');
        let mut output = AtomicOutput::new_with_overwrite(path, overwrite)?;
        output.write_all(&bytes)?;
        output.commit()
    }

    /// Export descriptor-bound records as an atomic catalogue-report-v2 document.
    pub fn write_report_v2(
        &self,
        path: &Path,
        records: Vec<CatalogueRecordV2>,
    ) -> Result<(), String> {
        self.write_report_v2_with_overwrite(path, records, true)
    }

    /// Atomically export a catalogue-report-v2 document with a replacement policy.
    pub fn write_report_v2_with_overwrite(
        &self,
        path: &Path,
        records: Vec<CatalogueRecordV2>,
        overwrite: bool,
    ) -> Result<(), String> {
        if records.len() > MAX_REPORT_RECORDS {
            return Err(format!(
                "catalogue report contains {} records; limit is {MAX_REPORT_RECORDS}",
                records.len()
            ));
        }
        if comparison_path(path)? == comparison_path(&self.path)? {
            return Err("catalogue report must not overwrite the SQLite catalogue".into());
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let report = CatalogueReportV2 {
            schema: CATALOGUE_REPORT_SCHEMA_V2,
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            catalogue: self.path.to_string_lossy().into_owned(),
            records,
        };
        let mut bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("encode catalogue report: {error}"))?;
        bytes.push(b'\n');
        let mut output = AtomicOutput::new_with_overwrite(path, overwrite)?;
        output.write_all(&bytes)?;
        output.commit()
    }

    /// Export exact-layout records as an atomic catalogue-report-v3 document.
    pub fn write_report_v3(
        &self,
        path: &Path,
        records: Vec<CatalogueRecordV2>,
    ) -> Result<(), String> {
        self.write_report_v3_with_overwrite(path, records, true)
    }

    /// Atomically export catalogue-report-v3 with a replacement policy.
    pub fn write_report_v3_with_overwrite(
        &self,
        path: &Path,
        records: Vec<CatalogueRecordV2>,
        overwrite: bool,
    ) -> Result<(), String> {
        if records.len() > MAX_REPORT_RECORDS {
            return Err(format!(
                "catalogue report contains {} records; limit is {MAX_REPORT_RECORDS}",
                records.len()
            ));
        }
        if comparison_path(path)? == comparison_path(&self.path)? {
            return Err("catalogue report must not overwrite the SQLite catalogue".into());
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let report = CatalogueReportV3 {
            schema: CATALOGUE_REPORT_SCHEMA_V3,
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            catalogue: self.path.to_string_lossy().into_owned(),
            records,
        };
        let mut bytes = serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("encode catalogue report: {error}"))?;
        bytes.push(b'\n');
        let mut output = AtomicOutput::new_with_overwrite(path, overwrite)?;
        output.write_all(&bytes)?;
        output.commit()
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), String> {
    let application_id: i32 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(|error| format!("read catalogue application ID: {error}"))?;
    let user_version: i32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| format!("read catalogue schema version: {error}"))?;
    if application_id != 0 && application_id != APPLICATION_ID {
        return Err(format!(
            "SQLite file belongs to application ID {application_id}, not Forge"
        ));
    }
    if user_version > CATALOGUE_DATABASE_VERSION {
        return Err(format!(
            "catalogue schema version {user_version} is newer than supported version {CATALOGUE_DATABASE_VERSION}"
        ));
    }
    if user_version == 0 {
        let existing_table: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("inspect catalogue schema: {error}"))?;
        if existing_table.is_some() {
            return Err("refusing to initialize an unrecognized non-empty SQLite database".into());
        }
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE catalogue_entries (
                    id INTEGER PRIMARY KEY,
                    recorded_unix_ms INTEGER NOT NULL CHECK(recorded_unix_ms >= 0),
                    operation TEXT NOT NULL CHECK(length(operation) BETWEEN 1 AND 256),
                    source_path TEXT NOT NULL CHECK(length(CAST(source_path AS BLOB)) BETWEEN 1 AND 4096),
                    source_bytes INTEGER NOT NULL CHECK(source_bytes >= 0),
                    source_sha256 TEXT NOT NULL CHECK(length(source_sha256) = 64),
                    output_path TEXT CHECK(output_path IS NULL OR length(CAST(output_path AS BLOB)) BETWEEN 1 AND 4096),
                    output_bytes INTEGER CHECK(output_bytes IS NULL OR output_bytes >= 0),
                    output_sha256 TEXT CHECK(output_sha256 IS NULL OR length(output_sha256) = 64),
                    request_sha256 TEXT NOT NULL CHECK(length(request_sha256) = 64),
                    request_json TEXT NOT NULL CHECK(json_valid(request_json) AND length(CAST(request_json AS BLOB)) <= 1048576),
                    measurement_standard TEXT NOT NULL,
                    measurement_version TEXT NOT NULL,
                    profile TEXT NOT NULL CHECK(length(CAST(profile AS BLOB)) BETWEEN 1 AND 4096),
                    tool_version TEXT NOT NULL,
                    sample_rate_hz INTEGER NOT NULL CHECK(sample_rate_hz BETWEEN 1 AND 384000),
                    channels INTEGER NOT NULL CHECK(channels BETWEEN 1 AND 1024),
                    frames INTEGER NOT NULL CHECK(frames >= 0),
                    duration_seconds REAL NOT NULL CHECK(duration_seconds >= 0),
                    integrated_lufs REAL,
                    loudness_range_lu REAL,
                    rms_dbfs REAL,
                    sample_peak_dbfs REAL,
                    true_peak_dbtp REAL,
                    provenance_json TEXT NOT NULL CHECK(json_valid(provenance_json) AND length(CAST(provenance_json AS BLOB)) <= 1048576),
                    CHECK((output_path IS NULL) = (output_bytes IS NULL)),
                    CHECK((output_path IS NULL) = (output_sha256 IS NULL))
                 );
                 CREATE UNIQUE INDEX catalogue_identity_v2
                 ON catalogue_entries (
                    source_sha256, IFNULL(output_sha256, ''), operation,
                    request_sha256, measurement_standard, measurement_version,
                    profile, tool_version
                 );
                 CREATE INDEX catalogue_source_sha256_v2
                 ON catalogue_entries(source_sha256);
                 CREATE INDEX catalogue_output_sha256_v2
                 ON catalogue_entries(output_sha256) WHERE output_sha256 IS NOT NULL;
                 PRAGMA application_id = 1179603527;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )
            .map_err(|error| format!("initialize catalogue schema: {error}"))?;
    } else if application_id != APPLICATION_ID {
        return Err("catalogue schema has no Forge application ID".into());
    } else if user_version == 1 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE catalogue_entries ADD COLUMN request_sha256 TEXT NOT NULL
                    DEFAULT '0000000000000000000000000000000000000000000000000000000000000000'
                    CHECK(length(request_sha256) = 64);
                 ALTER TABLE catalogue_entries ADD COLUMN request_json TEXT NOT NULL
                    DEFAULT '{\"schema\":\"forge-catalogue-request-v1\",\"legacy\":true}'
                    CHECK(json_valid(request_json) AND length(CAST(request_json AS BLOB)) <= 1048576);
                 DROP INDEX catalogue_identity_v1;
                 DROP INDEX catalogue_source_sha256_v1;
                 DROP INDEX catalogue_output_sha256_v1;
                 CREATE UNIQUE INDEX catalogue_identity_v2
                 ON catalogue_entries (
                    source_sha256, IFNULL(output_sha256, ''), operation,
                    request_sha256, measurement_standard, measurement_version,
                    profile, tool_version
                 );
                 CREATE INDEX catalogue_source_sha256_v2
                 ON catalogue_entries(source_sha256);
                 CREATE INDEX catalogue_output_sha256_v2
                 ON catalogue_entries(output_sha256) WHERE output_sha256 IS NOT NULL;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )
            .map_err(|error| format!("migrate catalogue schema from v1 to v2: {error}"))?;
    }
    connection
        .query_row(
            "SELECT COUNT(*) FROM catalogue_entries LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("validate catalogue schema: {error}"))?;
    Ok(())
}

fn hash_request(request: &Value) -> Result<String, String> {
    let bytes = serde_json::to_vec(request)
        .map_err(|error| format!("encode canonical catalogue request: {error}"))?;
    let mut digest = Sha256::new();
    digest.update(b"forge-catalogue-request-v1\0");
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
    Ok(hex_digest(digest.finalize()))
}

fn validate_request(request_sha256: &str, request: &Value) -> Result<(), String> {
    let legacy = request
        .as_object()
        .is_some_and(|object| object.get("legacy") == Some(&Value::Bool(true)));
    if legacy && request_sha256 == LEGACY_REQUEST_SHA256 {
        return Ok(());
    }
    if request_sha256.len() != 64
        || !request_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || request_sha256 != hash_request(request)?
    {
        return Err("catalogue request SHA-256 does not match its canonical evidence".into());
    }
    if !request.is_object() {
        return Err("catalogue request must be a JSON object".into());
    }
    let bytes = serde_json::to_vec(request)
        .map_err(|error| format!("encode catalogue request: {error}"))?;
    if bytes.len() > MAX_PROVENANCE_BYTES {
        return Err(format!(
            "catalogue request is {} bytes; limit is {MAX_PROVENANCE_BYTES}",
            bytes.len()
        ));
    }
    Ok(())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;

    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn channel_role_id(role: &ChannelRole) -> String {
    match *role {
        ChannelRole::Main => "main".into(),
        ChannelRole::Surround => "surround".into(),
        ChannelRole::DualMono => "dual-mono".into(),
        ChannelRole::Positioned {
            azimuth_degrees,
            elevation_degrees,
        } => format!("positioned:{azimuth_degrees}:{elevation_degrees}"),
        ChannelRole::Lfe => "lfe".into(),
    }
}

const fn layout_provenance_id(value: ChannelLayoutProvenance) -> &'static str {
    match value {
        ChannelLayoutProvenance::KnownSpeakers => "known-speakers",
        ChannelLayoutProvenance::Unknown => "unknown",
        ChannelLayoutProvenance::SceneBased => "scene-based",
    }
}

const fn mode_id(value: Mode) -> &'static str {
    match value {
        Mode::Lufs => "lufs",
        Mode::Peak => "peak",
        Mode::Rms => "rms",
    }
}

const fn pcm_kind_id(value: PcmKind) -> &'static str {
    match value {
        PcmKind::U8 => "u8",
        PcmKind::S16 => "s16",
        PcmKind::S24 => "s24",
        PcmKind::S32 => "s32",
        PcmKind::F32 => "f32",
        PcmKind::F64 => "f64",
    }
}

const fn wav_container_id(value: WavContainer) -> &'static str {
    match value {
        WavContainer::Auto => "auto",
        WavContainer::Riff => "riff",
        WavContainer::Rf64 => "rf64",
        WavContainer::Bw64 => "bw64",
    }
}

const fn resample_quality_id(value: ResampleQuality) -> &'static str {
    match value {
        ResampleQuality::Fast => "fast",
        ResampleQuality::Balanced => "balanced",
        ResampleQuality::Best => "best",
    }
}

fn validate_catalogue_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("catalogue path must not be empty".into());
    }
    if path.to_string_lossy().len() > MAX_TEXT_BYTES {
        return Err(format!(
            "catalogue path exceeds the {MAX_TEXT_BYTES}-byte limit"
        ));
    }
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!(
            "catalogue path must not be a symbolic link: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_label(name: &str, value: &str, maximum: usize) -> Result<(), String> {
    let length = value.len();
    if length == 0 || length > maximum {
        return Err(format!(
            "catalogue {name} must contain 1..={maximum} UTF-8 bytes"
        ));
    }
    Ok(())
}

fn validate_provenance(provenance: &Value) -> Result<(), String> {
    if !provenance.is_object() {
        return Err("catalogue provenance must be a JSON object".into());
    }
    let bytes = serde_json::to_vec(provenance)
        .map_err(|error| format!("encode catalogue provenance: {error}"))?;
    if bytes.len() > MAX_PROVENANCE_BYTES {
        return Err(format!(
            "catalogue provenance is {} bytes; limit is {MAX_PROVENANCE_BYTES}",
            bytes.len()
        ));
    }
    Ok(())
}

fn validate_measurement(measurement: &MeasurementEvidence) -> Result<(), String> {
    if measurement.sample_rate_hz == 0
        || measurement.sample_rate_hz > 384_000
        || measurement.channels == 0
        || measurement.channels > 1024
        || !measurement.duration_seconds.is_finite()
        || measurement.duration_seconds < 0.0
    {
        return Err("catalogue measurement is outside supported bounds".into());
    }
    Ok(())
}

fn inspect_stable_file(path: &Path) -> Result<FileEvidence, String> {
    let first = normalization_diff::inspect_file(path)?;
    let second = normalization_diff::inspect_file(path)?;
    if first.bytes != second.bytes || first.sha256 != second.sha256 {
        return Err(format!(
            "{} changed while catalogue evidence was being hashed",
            path.display()
        ));
    }
    Ok(second)
}

fn unix_milliseconds() -> Result<i64, String> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system clock: {error}"))?
        .as_millis();
    i64::try_from(value).map_err(|_| "system clock exceeds SQLite timestamp range".into())
}

fn to_i64(name: &str, value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{name} exceeds SQLite integer range"))
}

fn comparison_path(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        fs::canonicalize(path).map_err(|error| format!("canonicalize {}: {error}", path.display()))
    } else {
        std::path::absolute(path).map_err(|error| format!("resolve {}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, ChannelRole, PcmKind, WavWriter};

    fn plan() -> Plan {
        Plan {
            mode: Mode::Lufs,
            target_lufs: -16.0,
            target_peak_db: -1.0,
            target_rms_db: -18.0,
            ceiling_db: -1.0,
            max_gain_db: Some(12.0),
            dither: false,
            output_kind: Some(PcmKind::S16),
            mp3_bitrate: 192,
            mp3_quality: 2,
            limiter: None,
            wav_container: WavContainer::Auto,
            bwf: false,
            output_sample_rate: None,
            resample_quality: ResampleQuality::Balanced,
        }
    }

    fn fixture(path: &Path) -> Analysis {
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            channel_roles: vec![ChannelRole::Main],
            frames: 48_000,
            data: vec![vec![0.1; 48_000]],
            source_kind: PcmKind::S16,
        };
        WavWriter::write(path, &buffer, PcmKind::S16, false).unwrap();
        crate::analysis::analyze(&buffer)
    }

    #[test]
    fn records_hash_bound_evidence_and_deduplicates() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let output_path = directory.path().join("output.wav");
        let measurement = fixture(&source_path);
        fs::copy(&source_path, &output_path).unwrap();
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let source_sha256 = normalization_diff::inspect_file(&source_path)
            .unwrap()
            .sha256;
        let asset = || CatalogueAsset {
            source: &source_path,
            expected_source_sha256: &source_sha256,
            output: Some(&output_path),
            measurement: &measurement,
            operation: "normalization",
            profile: "custom-lufs--16",
            provenance: serde_json::json!({"target_lufs": -16.0}),
        };
        let first = catalogue.record(asset()).unwrap();
        let second = catalogue.record(asset()).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(catalogue.len().unwrap(), 1);
        assert_eq!(first.source.sha256.len(), 64);
        assert_eq!(first.output.unwrap().sha256.len(), 64);
    }

    #[test]
    fn v2_identity_includes_descriptor_renderer_and_effective_plan() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        fixture(&source_path);
        let stable_options = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor = InputDescriptor::from_path(
            &source_path,
            &stable_options,
            InputDescriptorOptions::default(),
        )
        .unwrap();
        let measurement = crate::normalize::analyze_input_descriptor_for_plan(&descriptor, &plan())
            .unwrap()
            .analysis()
            .clone();
        let source_sha256 = descriptor.stable_input().binding().sha256_hex();
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let asset = || CatalogueAsset {
            source: &source_path,
            expected_source_sha256: &source_sha256,
            output: None,
            measurement: &measurement,
            operation: "analysis",
            profile: "custom-lufs--16",
            provenance: serde_json::json!({"target_lufs": -16.0}),
        };

        let first = catalogue
            .record_bound(asset(), &descriptor, &plan(), "forge-analysis:fast")
            .unwrap();
        let duplicate = catalogue
            .record_bound(asset(), &descriptor, &plan(), "forge-analysis:fast")
            .unwrap();
        assert_eq!(first.id, duplicate.id);
        assert_eq!(first.request["input_descriptor"]["audio_track_index"], 0);
        assert_eq!(first.request["renderer"], "forge-analysis:fast");

        let other_renderer = catalogue
            .record_bound(asset(), &descriptor, &plan(), "forge-analysis:reference")
            .unwrap();
        assert_ne!(first.id, other_renderer.id);
        let mut other_plan = plan();
        other_plan.target_lufs = -14.0;
        let other_plan = catalogue
            .record_bound(asset(), &descriptor, &other_plan, "forge-analysis:fast")
            .unwrap();
        assert_ne!(first.id, other_plan.id);

        let ranged = InputDescriptor::from_path(
            &source_path,
            &stable_options,
            InputDescriptorOptions::default().with_time_range(0.0, Some(0.5)),
        )
        .unwrap();
        let ranged_measurement =
            crate::normalize::analyze_input_descriptor_for_plan(&ranged, &plan())
                .unwrap()
                .analysis()
                .clone();
        let ranged_asset = CatalogueAsset {
            source: &source_path,
            expected_source_sha256: &source_sha256,
            output: None,
            measurement: &ranged_measurement,
            operation: "analysis",
            profile: "custom-lufs--16",
            provenance: serde_json::json!({"target_lufs": -16.0}),
        };
        let ranged_record = catalogue
            .record_bound(ranged_asset, &ranged, &plan(), "forge-analysis:fast")
            .unwrap();
        assert_ne!(first.id, ranged_record.id);
        assert_eq!(catalogue.len().unwrap(), 4);
    }

    #[test]
    fn migrates_v1_catalogues_without_losing_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalogue.sqlite");
        let source = directory.path().join("source.wav");
        let measurement = fixture(&source);
        let source_hash = normalization_diff::inspect_file(&source).unwrap().sha256;
        {
            let mut catalogue = Catalogue::open(&path).unwrap();
            catalogue
                .record(CatalogueAsset {
                    source: &source,
                    expected_source_sha256: &source_hash,
                    output: None,
                    measurement: &measurement,
                    operation: "analysis",
                    profile: "migration-fixture",
                    provenance: serde_json::json!({"fixture": true}),
                })
                .unwrap();
            assert_eq!(catalogue.len().unwrap(), 1);
        }
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP INDEX catalogue_identity_v2;
                 DROP INDEX catalogue_source_sha256_v2;
                 DROP INDEX catalogue_output_sha256_v2;
                 ALTER TABLE catalogue_entries DROP COLUMN request_json;
                 ALTER TABLE catalogue_entries DROP COLUMN request_sha256;
                 CREATE UNIQUE INDEX catalogue_identity_v1
                 ON catalogue_entries (
                    source_sha256, IFNULL(output_sha256, ''), operation,
                    measurement_standard, measurement_version, profile, tool_version
                 );
                 CREATE INDEX catalogue_source_sha256_v1
                 ON catalogue_entries(source_sha256);
                 CREATE INDEX catalogue_output_sha256_v1
                 ON catalogue_entries(output_sha256) WHERE output_sha256 IS NOT NULL;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let catalogue = Catalogue::open(&path).unwrap();
        assert_eq!(catalogue.len().unwrap(), 1);
        let version: i32 = catalogue
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CATALOGUE_DATABASE_VERSION);
        let columns: i64 = catalogue
            .connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('catalogue_entries')
                 WHERE name IN ('request_sha256', 'request_json')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 2);
        let (request_sha256, request_json): (String, String) = catalogue
            .connection
            .query_row(
                "SELECT request_sha256, request_json FROM catalogue_entries",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(request_sha256, LEGACY_REQUEST_SHA256);
        assert_eq!(
            serde_json::from_str::<Value>(&request_json).unwrap(),
            serde_json::json!({
                "schema": CATALOGUE_REQUEST_SCHEMA_V1,
                "legacy": true
            })
        );
    }

    #[test]
    fn compatibility_api_exports_an_atomic_v1_report() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let measurement = fixture(&source_path);
        let source_sha256 = normalization_diff::inspect_file(&source_path)
            .unwrap()
            .sha256;
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let record = catalogue
            .record(CatalogueAsset {
                source: &source_path,
                expected_source_sha256: &source_sha256,
                output: None,
                measurement: &measurement,
                operation: "analysis",
                profile: "measurement-only",
                provenance: serde_json::json!({"mode": "analysis"}),
            })
            .unwrap();
        let report_path = directory.path().join("report.json");
        catalogue.write_report(&report_path, vec![record]).unwrap();
        let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["schema"], CATALOGUE_REPORT_SCHEMA_V1);
        assert_eq!(report["records"].as_array().unwrap().len(), 1);
        assert!(report["records"][0].get("request").is_none());
        assert!(report["records"][0].get("request_sha256").is_none());
    }

    #[test]
    fn descriptor_bound_api_exports_an_atomic_v2_report() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        fixture(&source_path);
        let stable_options = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor = InputDescriptor::from_path(
            &source_path,
            &stable_options,
            InputDescriptorOptions::default(),
        )
        .unwrap();
        let measurement = crate::normalize::analyze_input_descriptor_for_plan(&descriptor, &plan())
            .unwrap()
            .analysis()
            .clone();
        let source_sha256 = descriptor.stable_input().binding().sha256_hex();
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let record = catalogue
            .record_bound(
                CatalogueAsset {
                    source: &source_path,
                    expected_source_sha256: &source_sha256,
                    output: None,
                    measurement: &measurement,
                    operation: "analysis",
                    profile: "measurement-only",
                    provenance: serde_json::json!({"mode": "analysis"}),
                },
                &descriptor,
                &plan(),
                "forge-analysis:fast",
            )
            .unwrap();
        let report_path = directory.path().join("report-v2.json");
        catalogue
            .write_report_v2(&report_path, vec![record])
            .unwrap();
        let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["schema"], CATALOGUE_REPORT_SCHEMA_V2);
        assert_eq!(report["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            report["records"][0]["request"]["renderer"],
            "forge-analysis:fast"
        );
        assert_eq!(
            report["records"][0]["request_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn exact_layout_api_exports_an_atomic_v3_report() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        fixture(&source_path);
        let stable_options = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor = InputDescriptor::from_path(
            &source_path,
            &stable_options,
            InputDescriptorOptions::default(),
        )
        .unwrap();
        let measurement = crate::normalize::analyze_input_descriptor_for_plan(&descriptor, &plan())
            .unwrap()
            .analysis()
            .clone();
        let source_sha256 = descriptor.stable_input().binding().sha256_hex();
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let record = catalogue
            .record_bound_v3(
                CatalogueAsset {
                    source: &source_path,
                    expected_source_sha256: &source_sha256,
                    output: None,
                    measurement: &measurement,
                    operation: "analysis",
                    profile: "measurement-only",
                    provenance: serde_json::json!({"mode": "analysis"}),
                },
                &descriptor,
                &plan(),
                "forge-analysis:fast",
            )
            .unwrap();
        let report_path = directory.path().join("report-v3.json");
        catalogue
            .write_report_v3(&report_path, vec![record])
            .unwrap();
        let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["schema"], CATALOGUE_REPORT_SCHEMA_V3);
        let request = &report["records"][0]["request"];
        assert_eq!(request["schema"], CATALOGUE_REQUEST_SCHEMA_V2);
        assert_eq!(
            request["input_descriptor"]["declared_channel_layout"]["version"],
            1
        );
        assert_eq!(
            request["input_descriptor"]["effective_channel_layout"]["origin"],
            "wave"
        );
        assert_eq!(
            request["input_descriptor"]["explicit_channel_layout"],
            false
        );
    }

    #[test]
    fn rejects_unrecognized_and_newer_databases() {
        let directory = tempfile::tempdir().unwrap();
        let unrelated = directory.path().join("unrelated.sqlite");
        let connection = Connection::open(&unrelated).unwrap();
        connection
            .execute("CREATE TABLE other (id INTEGER)", [])
            .unwrap();
        let journal_before: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        drop(connection);
        assert!(Catalogue::open(&unrelated)
            .unwrap_err()
            .contains("unrecognized non-empty"));
        let connection = Connection::open(&unrelated).unwrap();
        let journal_after: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_after, journal_before);
        drop(connection);

        let newer = directory.path().join("newer.sqlite");
        let connection = Connection::open(&newer).unwrap();
        connection
            .execute_batch("PRAGMA application_id = 1179603527; PRAGMA user_version = 3;")
            .unwrap();
        drop(connection);
        assert!(Catalogue::open(&newer)
            .unwrap_err()
            .contains("newer than supported"));
    }

    #[test]
    fn rejects_unbounded_or_non_object_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let measurement = fixture(&source_path);
        let source_sha256 = normalization_diff::inspect_file(&source_path)
            .unwrap()
            .sha256;
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let error = catalogue
            .record(CatalogueAsset {
                source: &source_path,
                expected_source_sha256: &source_sha256,
                output: None,
                measurement: &measurement,
                operation: "analysis",
                profile: "measurement-only",
                provenance: Value::String("not an object".into()),
            })
            .unwrap_err();
        assert!(error.contains("JSON object"));
    }

    #[test]
    fn rejects_a_source_changed_after_pre_measurement_hashing() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.wav");
        let measurement = fixture(&source_path);
        let source_sha256 = normalization_diff::inspect_file(&source_path)
            .unwrap()
            .sha256;
        fs::write(&source_path, b"changed after measurement").unwrap();
        let mut catalogue = Catalogue::open(directory.path().join("catalogue.sqlite")).unwrap();
        let error = catalogue
            .record(CatalogueAsset {
                source: &source_path,
                expected_source_sha256: &source_sha256,
                output: None,
                measurement: &measurement,
                operation: "analysis",
                profile: "measurement-only",
                provenance: serde_json::json!({"mode": "analysis"}),
            })
            .unwrap_err();
        assert!(error.contains("changed between pre-measurement"));
        assert!(catalogue.is_empty().unwrap());
    }
}
