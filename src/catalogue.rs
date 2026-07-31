//! Durable SQLite catalogue for measured and normalized audio assets.
//!
//! The catalogue is an opt-in index, not an analysis cache. Each row binds the
//! exact source and optional output byte streams to the measurement method,
//! selected profile, Forge version, measurements, and caller-supplied
//! provenance evidence.

use crate::analysis::Analysis;
use crate::analysis_cache::{ALGORITHM_REVISION, MEASUREMENT_STANDARD};
use crate::atomic::AtomicOutput;
use crate::normalization_diff::{self, FileEvidence, MeasurementEvidence};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Schema URI used by exported catalogue provenance reports.
pub const CATALOGUE_REPORT_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/catalogue-report-v1";
/// SQLite schema version stored in `PRAGMA user_version`.
pub const CATALOGUE_DATABASE_VERSION: i32 = 1;
/// Maximum serialized provenance document accepted for one catalogue row.
pub const MAX_PROVENANCE_BYTES: usize = 1024 * 1024;
/// Maximum number of rows exported in one provenance report.
pub const MAX_REPORT_RECORDS: usize = 100_000;

const APPLICATION_ID: i32 = 0x464f_5247; // "FORG"
const MAX_TEXT_BYTES: usize = 4096;

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

/// Versioned report containing rows committed by an invocation.
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

    /// Hash and atomically insert or refresh one content-addressed row.
    pub fn record(&mut self, asset: CatalogueAsset<'_>) -> Result<CatalogueRecord, String> {
        validate_label("operation", asset.operation, 256)?;
        validate_label("profile", asset.profile, MAX_TEXT_BYTES)?;
        validate_provenance(&asset.provenance)?;
        let source = normalization_diff::inspect_file(asset.source)?;
        if source.sha256 != asset.expected_source_sha256 {
            return Err(format!(
                "{} changed between pre-measurement hashing and catalogue commit",
                asset.source.display()
            ));
        }
        let output = asset.output.map(inspect_stable_file).transpose()?;
        let measurement = MeasurementEvidence::from(asset.measurement);
        validate_measurement(&measurement)?;
        let recorded_unix_ms = unix_milliseconds()?;
        let tool_version = env!("CARGO_PKG_VERSION").to_string();
        let provenance_json = serde_json::to_string(&asset.provenance)
            .map_err(|error| format!("encode catalogue provenance: {error}"))?;
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
                    measurement_standard, measurement_version, profile, tool_version,
                    sample_rate_hz, channels, frames, duration_seconds,
                    integrated_lufs, loudness_range_lu, rms_dbfs,
                    sample_peak_dbfs, true_peak_dbtp, provenance_json
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22
                 )
                 ON CONFLICT DO UPDATE SET
                    recorded_unix_ms = excluded.recorded_unix_ms,
                    source_path = excluded.source_path,
                    source_bytes = excluded.source_bytes,
                    output_path = excluded.output_path,
                    output_bytes = excluded.output_bytes,
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
        Ok(CatalogueRecord {
            id,
            recorded_unix_ms,
            operation: asset.operation.to_string(),
            source,
            output,
            measurement_standard: MEASUREMENT_STANDARD.to_string(),
            measurement_version: ALGORITHM_REVISION.to_string(),
            profile: asset.profile.to_string(),
            tool_version,
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

    /// Export records committed by the caller as an atomic JSON report.
    pub fn write_report(&self, path: &Path, records: Vec<CatalogueRecord>) -> Result<(), String> {
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
        let mut output = AtomicOutput::new(path)?;
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
                 CREATE UNIQUE INDEX catalogue_identity_v1
                 ON catalogue_entries (
                    source_sha256, IFNULL(output_sha256, ''), operation,
                    measurement_standard, measurement_version, profile, tool_version
                 );
                 CREATE INDEX catalogue_source_sha256_v1
                 ON catalogue_entries(source_sha256);
                 CREATE INDEX catalogue_output_sha256_v1
                 ON catalogue_entries(output_sha256) WHERE output_sha256 IS NOT NULL;
                 PRAGMA application_id = 1179603527;
                 PRAGMA user_version = 1;
                 COMMIT;",
            )
            .map_err(|error| format!("initialize catalogue schema: {error}"))?;
    } else if application_id != APPLICATION_ID {
        return Err("catalogue schema has no Forge application ID".into());
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
    fn exports_an_atomic_versioned_report() {
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
            .execute_batch("PRAGMA application_id = 1179603527; PRAGMA user_version = 2;")
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
