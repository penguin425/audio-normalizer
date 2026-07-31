//! Content-addressed, bounded analysis cache.
//!
//! Cache identity is derived from the complete input byte stream and every
//! option that changes the measured signal. Entries are versioned JSON
//! evidence documents and are committed atomically.

use crate::analysis::Analysis;
use crate::atomic::AtomicOutput;
use crate::dsp::lufs::LoudnessTimelinePoint;
use crate::dsp::resample::ResampleQuality;
use crate::normalize::{self, Plan, TimedAnalysis};
use crate::wav::{ChannelRole, PcmKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ANALYSIS_CACHE_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/analysis-cache-v1";
pub const MEASUREMENT_STANDARD: &str = "ITU-R BS.1770-5 / EBU R 128";
pub const ALGORITHM_REVISION: &str = "forge-bs1770-5-r1";

const LAYOUT_VERSION: &str = "v1";
const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHANNELS: usize = 1024;
const MAX_SCAN_ENTRIES: usize = 100_000;

/// Storage and mutation policy for an [`AnalysisCache`].
#[derive(Debug, Clone, Copy)]
pub struct AnalysisCachePolicy {
    /// Do not create, replace, or evict cache entries.
    pub read_only: bool,
    /// Maximum bytes occupied by recognized cache entry files.
    pub max_bytes: u64,
}

impl Default for AnalysisCachePolicy {
    fn default() -> Self {
        Self {
            read_only: false,
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// Observable result of a cache lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDisposition {
    Hit,
    Stored,
    Repaired,
    ReadOnlyMiss,
    ReadOnlyInvalid,
    TooLarge,
}

/// A measured value and the cache action that produced it.
#[derive(Debug, Clone)]
pub struct Cached<T> {
    pub value: T,
    pub disposition: CacheDisposition,
    /// Validation detail for an ignored invalid entry.
    pub warning: Option<String>,
}

/// A content-addressed cache rooted at a caller-selected directory.
#[derive(Debug, Clone)]
pub struct AnalysisCache {
    root: PathBuf,
    policy: AnalysisCachePolicy,
}

impl AnalysisCache {
    pub fn new(root: impl Into<PathBuf>, policy: AnalysisCachePolicy) -> Result<Self, String> {
        if policy.max_bytes == 0 {
            return Err("analysis cache size limit must be greater than zero".into());
        }
        let root = root.into();
        if root.exists() && !root.is_dir() {
            return Err(format!(
                "analysis cache path is not a directory: {}",
                root.display()
            ));
        }
        Ok(Self { root, policy })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Analyze a complete source stream with an optional explicit channel map.
    pub fn analyze_file(
        &self,
        path: &Path,
        channel_roles: Option<&[ChannelRole]>,
    ) -> Result<Cached<Analysis>, String> {
        self.analyze_range(path, channel_roles, 0.0, None, None)
            .map(|cached| Cached {
                value: cached.value.analysis,
                disposition: cached.disposition,
                warning: cached.warning,
            })
    }

    /// Analyze a range and optionally retain its loudness timeline.
    pub fn analyze_range(
        &self,
        path: &Path,
        channel_roles: Option<&[ChannelRole]>,
        start_seconds: f64,
        duration_seconds: Option<f64>,
        timeline_interval_ms: Option<f64>,
    ) -> Result<Cached<TimedAnalysis>, String> {
        let request = RequestRecord::Range {
            channel_roles: channel_roles.map(roles_to_records),
            start_seconds,
            duration_seconds,
            timeline_interval_ms,
        };
        validate_request(&request)?;
        self.lookup_or_compute(path, request, || {
            normalize::analyze_file_range_with_roles(
                path,
                channel_roles,
                start_seconds,
                duration_seconds,
                timeline_interval_ms,
            )
        })
    }

    /// Analyze the exact output-domain signal selected by a normalization plan.
    pub fn analyze_for_plan(
        &self,
        path: &Path,
        channel_roles: Option<&[ChannelRole]>,
        plan: &Plan,
    ) -> Result<Cached<Analysis>, String> {
        let request = RequestRecord::OutputDomain {
            channel_roles: channel_roles.map(roles_to_records),
            output_sample_rate_hz: plan.output_sample_rate,
            resample_quality: plan.resample_quality,
        };
        validate_request(&request)?;
        self.lookup_or_compute(path, request, || {
            normalize::analyze_file_for_plan(path, channel_roles, plan).map(|analysis| {
                TimedAnalysis {
                    analysis,
                    timeline: Vec::new(),
                }
            })
        })
        .map(|cached| Cached {
            value: cached.value.analysis,
            disposition: cached.disposition,
            warning: cached.warning,
        })
    }

    fn lookup_or_compute(
        &self,
        input: &Path,
        request: RequestRecord,
        compute: impl FnOnce() -> Result<TimedAnalysis, String>,
    ) -> Result<Cached<TimedAnalysis>, String> {
        let input_sha256 = hash_file(input)?;
        let request_bytes = serde_json::to_vec(&request)
            .map_err(|error| format!("encode analysis cache request: {error}"))?;
        let request_sha256 = hash_bytes(&request_bytes);
        let path = self.entry_path(&input_sha256, &request_sha256);
        let invalid = match self.load(&path, &input_sha256, &request_sha256, &request)? {
            LoadResult::Hit(value) => {
                return Ok(Cached {
                    value,
                    disposition: CacheDisposition::Hit,
                    warning: None,
                });
            }
            LoadResult::Missing => None,
            LoadResult::Invalid(reason) => Some(reason),
        };

        let value = compute()?;
        validate_timed_analysis(&value)?;
        let hash_after_analysis = hash_file(input)?;
        if hash_after_analysis != input_sha256 {
            return Err(format!(
                "{} changed while its analysis cache entry was being measured",
                input.display()
            ));
        }
        if self.policy.read_only {
            return Ok(Cached {
                value,
                disposition: if invalid.is_some() {
                    CacheDisposition::ReadOnlyInvalid
                } else {
                    CacheDisposition::ReadOnlyMiss
                },
                warning: invalid,
            });
        }

        let result = TimedAnalysisRecord::from_analysis(&value);
        let result_sha256 = hash_bytes(
            &serde_json::to_vec(&result)
                .map_err(|error| format!("encode analysis cache result: {error}"))?,
        );
        let document = CacheDocument {
            schema: ANALYSIS_CACHE_SCHEMA_V1.to_owned(),
            generator: format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
            measurement_standard: MEASUREMENT_STANDARD.to_owned(),
            algorithm_revision: ALGORITHM_REVISION.to_owned(),
            created_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("read system clock: {error}"))?
                .as_secs(),
            input_sha256,
            request_sha256,
            request,
            result_sha256,
            result,
        };
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("encode analysis cache entry: {error}"))?;
        if bytes.len() as u64 > self.policy.max_bytes || bytes.len() as u64 > MAX_ENTRY_BYTES {
            let size_warning = format!(
                "entry requires {} bytes but the effective entry/cache limit is {} bytes",
                bytes.len(),
                self.policy.max_bytes.min(MAX_ENTRY_BYTES)
            );
            return Ok(Cached {
                value,
                disposition: CacheDisposition::TooLarge,
                warning: Some(
                    invalid
                        .map(|invalid| format!("{invalid}; {size_warning}"))
                        .unwrap_or(size_warning),
                ),
            });
        }
        self.store(&path, &bytes)?;
        self.prune(&path)?;
        Ok(Cached {
            value,
            disposition: if invalid.is_some() {
                CacheDisposition::Repaired
            } else {
                CacheDisposition::Stored
            },
            warning: invalid,
        })
    }

    fn entry_path(&self, input_hash: &str, request_hash: &str) -> PathBuf {
        self.root
            .join(LAYOUT_VERSION)
            .join(&input_hash[..2])
            .join(input_hash)
            .join(format!("{request_hash}.json"))
    }

    fn load(
        &self,
        path: &Path,
        input_hash: &str,
        request_hash: &str,
        request: &RequestRecord,
    ) -> Result<LoadResult, String> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadResult::Missing);
            }
            Err(error) => {
                return Err(format!(
                    "inspect analysis cache {}: {error}",
                    path.display()
                ));
            }
        };
        if !metadata.is_file() {
            return Ok(LoadResult::Invalid(
                "cache entry is not a regular file".into(),
            ));
        }
        if metadata.len() > MAX_ENTRY_BYTES {
            return Ok(LoadResult::Invalid(format!(
                "cache entry exceeds the {MAX_ENTRY_BYTES}-byte limit"
            )));
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadResult::Missing);
            }
            Err(error) => {
                return Err(format!("read analysis cache {}: {error}", path.display()));
            }
        };
        let document: CacheDocument = match serde_json::from_slice(&bytes) {
            Ok(document) => document,
            Err(error) => {
                return Ok(LoadResult::Invalid(format!(
                    "cache entry is not valid v1 JSON: {error}"
                )));
            }
        };
        if let Err(reason) = validate_document(&document, input_hash, request_hash, request) {
            return Ok(LoadResult::Invalid(reason));
        }
        match document.result.into_analysis() {
            Ok(value) => Ok(LoadResult::Hit(value)),
            Err(reason) => Ok(LoadResult::Invalid(reason)),
        }
    }

    fn store(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        let parent = path
            .parent()
            .expect("content-addressed cache entries always have a parent");
        fs::create_dir_all(parent)
            .map_err(|error| format!("create analysis cache {}: {error}", parent.display()))?;
        let mut output = AtomicOutput::new(path)?;
        output.write_all(bytes)?;
        output.commit()
    }

    fn prune(&self, newest: &Path) -> Result<(), String> {
        let mut entries = self.recognized_entries()?;
        let mut total = entries
            .iter()
            .try_fold(0_u64, |total, entry| total.checked_add(entry.bytes))
            .ok_or_else(|| "analysis cache byte count overflow".to_string())?;
        if total <= self.policy.max_bytes {
            return Ok(());
        }
        entries.sort_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        for entry in entries {
            if total <= self.policy.max_bytes {
                break;
            }
            if entry.path == newest {
                continue;
            }
            match fs::remove_file(&entry.path) {
                Ok(()) => total = total.saturating_sub(entry.bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "evict analysis cache entry {}: {error}",
                        entry.path.display()
                    ));
                }
            }
        }
        if total > self.policy.max_bytes {
            return Err(format!(
                "analysis cache remains above its {}-byte limit",
                self.policy.max_bytes
            ));
        }
        Ok(())
    }

    fn recognized_entries(&self) -> Result<Vec<CacheFile>, String> {
        let base = self.root.join(LAYOUT_VERSION);
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for prefix in read_dir_or_error(&base)? {
            let prefix = prefix
                .map_err(|error| format!("scan analysis cache {}: {error}", base.display()))?;
            if !prefix
                .file_type()
                .map_err(|error| format!("inspect {}: {error}", prefix.path().display()))?
                .is_dir()
                || !is_lower_hex(&prefix.file_name().to_string_lossy(), 2)
            {
                continue;
            }
            for input in read_dir_or_error(&prefix.path())? {
                let input = input.map_err(|error| {
                    format!("scan analysis cache {}: {error}", prefix.path().display())
                })?;
                if !input
                    .file_type()
                    .map_err(|error| format!("inspect {}: {error}", input.path().display()))?
                    .is_dir()
                    || !is_lower_hex(&input.file_name().to_string_lossy(), 64)
                {
                    continue;
                }
                for entry in read_dir_or_error(&input.path())? {
                    let entry = entry.map_err(|error| {
                        format!("scan analysis cache {}: {error}", input.path().display())
                    })?;
                    if entries.len() == MAX_SCAN_ENTRIES {
                        return Err(format!(
                            "analysis cache exceeds the {MAX_SCAN_ENTRIES}-entry scan limit"
                        ));
                    }
                    if !entry
                        .file_type()
                        .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?
                        .is_file()
                        || !is_cache_filename(&entry.file_name().to_string_lossy())
                    {
                        continue;
                    }
                    let metadata = entry
                        .metadata()
                        .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
                    entries.push(CacheFile {
                        path: entry.path(),
                        bytes: metadata.len(),
                        modified: metadata.modified().unwrap_or(UNIX_EPOCH),
                    });
                }
            }
        }
        Ok(entries)
    }
}

enum LoadResult {
    Hit(TimedAnalysis),
    Missing,
    Invalid(String),
}

struct CacheFile {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheDocument {
    schema: String,
    generator: String,
    measurement_standard: String,
    algorithm_revision: String,
    created_unix_seconds: u64,
    input_sha256: String,
    request_sha256: String,
    request: RequestRecord,
    result_sha256: String,
    result: TimedAnalysisRecord,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RequestRecord {
    Range {
        channel_roles: Option<Vec<ChannelRoleRecord>>,
        start_seconds: f64,
        duration_seconds: Option<f64>,
        timeline_interval_ms: Option<f64>,
    },
    OutputDomain {
        channel_roles: Option<Vec<ChannelRoleRecord>>,
        output_sample_rate_hz: Option<u32>,
        resample_quality: ResampleQuality,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
enum ChannelRoleRecord {
    Main,
    Surround,
    DualMono,
    Positioned {
        azimuth_degrees: i16,
        elevation_degrees: i16,
    },
    Lfe,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TimedAnalysisRecord {
    sample_rate_hz: u32,
    channels: u16,
    channel_roles: Vec<ChannelRoleRecord>,
    frames: u64,
    pcm_kind: String,
    integrated_lufs: DbRecord,
    max_momentary_lufs: DbRecord,
    max_short_term_lufs: DbRecord,
    loudness_range_lu: f64,
    rms_dbfs: DbRecord,
    sample_peak_linear: f32,
    true_peak_linear: f32,
    gating_block_mean_squares: Vec<f64>,
    timeline: Vec<TimelineRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TimelineRecord {
    start_seconds: f64,
    end_seconds: f64,
    momentary_lufs: Option<DbRecord>,
    short_term_lufs: Option<DbRecord>,
    sample_peak_dbfs: DbRecord,
    true_peak_dbtp: DbRecord,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum DbRecord {
    Finite(f64),
    NegativeInfinity(String),
}

impl TimedAnalysisRecord {
    fn from_analysis(value: &TimedAnalysis) -> Self {
        let analysis = &value.analysis;
        Self {
            sample_rate_hz: analysis.sample_rate,
            channels: analysis.channels,
            channel_roles: roles_to_records(&analysis.channel_roles),
            frames: analysis.frames as u64,
            pcm_kind: pcm_kind_name(analysis.kind).to_owned(),
            integrated_lufs: DbRecord::from_db(analysis.lufs),
            max_momentary_lufs: DbRecord::from_db(analysis.max_momentary_lufs),
            max_short_term_lufs: DbRecord::from_db(analysis.max_short_term_lufs),
            loudness_range_lu: analysis.loudness_range_lu,
            rms_dbfs: DbRecord::from_db(analysis.rms_db),
            sample_peak_linear: analysis.sample_peak,
            true_peak_linear: analysis.true_peak,
            gating_block_mean_squares: analysis.loudness_blocks.clone(),
            timeline: value
                .timeline
                .iter()
                .map(|point| TimelineRecord {
                    start_seconds: point.start_seconds,
                    end_seconds: point.end_seconds,
                    momentary_lufs: point.momentary_lufs.map(DbRecord::from_db),
                    short_term_lufs: point.short_term_lufs.map(DbRecord::from_db),
                    sample_peak_dbfs: DbRecord::from_db(point.sample_peak_dbfs),
                    true_peak_dbtp: DbRecord::from_db(point.true_peak_dbtp),
                })
                .collect(),
        }
    }

    fn into_analysis(self) -> Result<TimedAnalysis, String> {
        if self.sample_rate_hz == 0 {
            return Err("cached sample rate must be positive".into());
        }
        if self.channels == 0 || self.channels as usize > MAX_CHANNELS {
            return Err(format!(
                "cached channel count must be within 1..={MAX_CHANNELS}"
            ));
        }
        if self.channel_roles.len() != self.channels as usize {
            return Err("cached channel-role count does not match channel count".into());
        }
        if self.frames > usize::MAX as u64 {
            return Err("cached frame count exceeds this platform's limit".into());
        }
        if !self.loudness_range_lu.is_finite() || self.loudness_range_lu < 0.0 {
            return Err("cached loudness range must be finite and non-negative".into());
        }
        if !self.sample_peak_linear.is_finite() || self.sample_peak_linear < 0.0 {
            return Err("cached sample peak must be finite and non-negative".into());
        }
        if !self.true_peak_linear.is_finite() || self.true_peak_linear < 0.0 {
            return Err("cached true peak must be finite and non-negative".into());
        }
        if self.gating_block_mean_squares.len() > crate::dsp::lufs::MAX_LOUDNESS_BLOCKS
            || self
                .gating_block_mean_squares
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(format!(
                "cached gating blocks must contain at most {} finite non-negative values",
                crate::dsp::lufs::MAX_LOUDNESS_BLOCKS
            ));
        }
        if self.timeline.len() > crate::dsp::lufs::MAX_LOUDNESS_TIMELINE_POINTS {
            return Err(format!(
                "cached timeline exceeds the {}-point limit",
                crate::dsp::lufs::MAX_LOUDNESS_TIMELINE_POINTS
            ));
        }
        let mut timeline = Vec::with_capacity(self.timeline.len());
        let mut previous_start = 0.0;
        for point in self.timeline {
            if !point.start_seconds.is_finite()
                || !point.end_seconds.is_finite()
                || point.start_seconds < previous_start
                || point.end_seconds < point.start_seconds
            {
                return Err("cached timeline contains invalid or unordered times".into());
            }
            previous_start = point.start_seconds;
            timeline.push(LoudnessTimelinePoint {
                start_seconds: point.start_seconds,
                end_seconds: point.end_seconds,
                momentary_lufs: decode_optional_db(point.momentary_lufs, "momentary loudness")?,
                short_term_lufs: decode_optional_db(point.short_term_lufs, "short-term loudness")?,
                sample_peak_dbfs: point.sample_peak_dbfs.into_db("timeline sample peak")?,
                true_peak_dbtp: point.true_peak_dbtp.into_db("timeline true peak")?,
            });
        }
        Ok(TimedAnalysis {
            analysis: Analysis {
                sample_rate: self.sample_rate_hz,
                channels: self.channels,
                channel_roles: self
                    .channel_roles
                    .into_iter()
                    .map(ChannelRoleRecord::into_role)
                    .collect(),
                frames: self.frames as usize,
                kind: parse_pcm_kind(&self.pcm_kind)?,
                lufs: self.integrated_lufs.into_db("integrated loudness")?,
                max_momentary_lufs: self
                    .max_momentary_lufs
                    .into_db("maximum momentary loudness")?,
                max_short_term_lufs: self
                    .max_short_term_lufs
                    .into_db("maximum short-term loudness")?,
                loudness_range_lu: self.loudness_range_lu,
                rms_db: self.rms_dbfs.into_db("RMS level")?,
                sample_peak: self.sample_peak_linear,
                true_peak: self.true_peak_linear,
                loudness_blocks: self.gating_block_mean_squares,
            },
            timeline,
        })
    }
}

impl ChannelRoleRecord {
    fn into_role(self) -> ChannelRole {
        match self {
            Self::Main => ChannelRole::Main,
            Self::Surround => ChannelRole::Surround,
            Self::DualMono => ChannelRole::DualMono,
            Self::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => ChannelRole::positioned(azimuth_degrees, elevation_degrees),
            Self::Lfe => ChannelRole::Lfe,
        }
    }
}

fn validate_document(
    document: &CacheDocument,
    input_hash: &str,
    request_hash: &str,
    request: &RequestRecord,
) -> Result<(), String> {
    if document.schema != ANALYSIS_CACHE_SCHEMA_V1 {
        return Err("cache entry has an unsupported schema".into());
    }
    if document.generator.is_empty() || document.generator.len() > 256 {
        return Err("cache entry generator provenance must contain 1..=256 bytes".into());
    }
    if document.measurement_standard != MEASUREMENT_STANDARD {
        return Err("cache entry measurement standard does not match".into());
    }
    if document.algorithm_revision != ALGORITHM_REVISION {
        return Err("cache entry algorithm revision does not match".into());
    }
    if document.input_sha256 != input_hash || document.request_sha256 != request_hash {
        return Err("cache entry content binding does not match its address".into());
    }
    if &document.request != request {
        return Err("cache entry request descriptor does not match".into());
    }
    let encoded_result = serde_json::to_vec(&document.result)
        .map_err(|error| format!("re-encode cached result for integrity check: {error}"))?;
    if document.result_sha256 != hash_bytes(&encoded_result) {
        return Err("cache entry result SHA-256 does not match".into());
    }
    validate_request(&document.request)
}

fn validate_request(request: &RequestRecord) -> Result<(), String> {
    let roles = match request {
        RequestRecord::Range {
            channel_roles,
            start_seconds,
            duration_seconds,
            timeline_interval_ms,
        } => {
            if !start_seconds.is_finite() || *start_seconds < 0.0 {
                return Err("analysis cache start must be finite and non-negative".into());
            }
            if duration_seconds.is_some_and(|value| !value.is_finite() || value <= 0.0) {
                return Err("analysis cache duration must be finite and positive".into());
            }
            if timeline_interval_ms.is_some_and(|value| !value.is_finite() || value <= 0.0) {
                return Err("analysis cache timeline interval must be finite and positive".into());
            }
            channel_roles
        }
        RequestRecord::OutputDomain {
            channel_roles,
            output_sample_rate_hz,
            ..
        } => {
            if output_sample_rate_hz == &Some(0) {
                return Err("analysis cache output sample rate must be positive".into());
            }
            channel_roles
        }
    };
    if roles
        .as_ref()
        .is_some_and(|roles| roles.is_empty() || roles.len() > MAX_CHANNELS)
    {
        return Err(format!(
            "analysis cache channel roles must contain 1..={MAX_CHANNELS} entries"
        ));
    }
    Ok(())
}

fn validate_timed_analysis(value: &TimedAnalysis) -> Result<(), String> {
    TimedAnalysisRecord::from_analysis(value)
        .into_analysis()
        .map(|_| ())
}

fn roles_to_records(roles: &[ChannelRole]) -> Vec<ChannelRoleRecord> {
    roles
        .iter()
        .map(|role| match *role {
            ChannelRole::Main => ChannelRoleRecord::Main,
            ChannelRole::Surround => ChannelRoleRecord::Surround,
            ChannelRole::DualMono => ChannelRoleRecord::DualMono,
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => ChannelRoleRecord::Positioned {
                azimuth_degrees,
                elevation_degrees,
            },
            ChannelRole::Lfe => ChannelRoleRecord::Lfe,
        })
        .collect()
}

impl DbRecord {
    fn from_db(value: f64) -> Self {
        if value == f64::NEG_INFINITY {
            Self::NegativeInfinity("-inf".into())
        } else {
            Self::Finite(value)
        }
    }

    fn into_db(self, label: &str) -> Result<f64, String> {
        match self {
            Self::Finite(value) if value.is_finite() => Ok(value),
            Self::Finite(_) => Err(format!("cached {label} must be finite or \"-inf\"")),
            Self::NegativeInfinity(value) if value == "-inf" => Ok(f64::NEG_INFINITY),
            Self::NegativeInfinity(_) => {
                Err(format!("cached {label} uses an unsupported symbolic value"))
            }
        }
    }
}

fn decode_optional_db(value: Option<DbRecord>, label: &str) -> Result<Option<f64>, String> {
    match value {
        Some(value) => value.into_db(label).map(Some),
        None => Ok(None),
    }
}

fn pcm_kind_name(kind: PcmKind) -> &'static str {
    match kind {
        PcmKind::U8 => "u8",
        PcmKind::S16 => "s16",
        PcmKind::S24 => "s24",
        PcmKind::S32 => "s32",
        PcmKind::F32 => "f32",
        PcmKind::F64 => "f64",
    }
}

fn parse_pcm_kind(value: &str) -> Result<PcmKind, String> {
    match value {
        "u8" => Ok(PcmKind::U8),
        "s16" => Ok(PcmKind::S16),
        "s24" => Ok(PcmKind::S24),
        "s32" => Ok(PcmKind::S32),
        "f32" => Ok(PcmKind::F32),
        "f64" => Ok(PcmKind::F64),
        _ => Err(format!("cached PCM kind is unsupported: {value}")),
    }
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("open {} for content hash: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

fn read_dir_or_error(path: &Path) -> Result<fs::ReadDir, String> {
    fs::read_dir(path).map_err(|error| format!("scan analysis cache {}: {error}", path.display()))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_cache_filename(value: &str) -> bool {
    value
        .strip_suffix(".json")
        .is_some_and(|hash| is_lower_hex(hash, 64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, WavWriter};

    fn wav(path: &Path, amplitude: f32) {
        let frames = 48_000;
        let mut samples = (0..2)
            .map(|_| Vec::with_capacity(frames))
            .collect::<Vec<_>>();
        for frame in 0..frames {
            let sample =
                amplitude * (std::f32::consts::TAU * 440.0 * frame as f32 / 48_000.0).sin();
            samples[0].push(sample);
            samples[1].push(sample);
        }
        WavWriter::write(
            path,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 2,
                frames,
                data: samples,
                channel_roles: vec![ChannelRole::Main; 2],
                source_kind: PcmKind::S16,
            },
            PcmKind::S16,
            false,
        )
        .unwrap();
    }

    #[test]
    fn stores_hits_and_content_changes_miss() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("tone.wav");
        let cache_root = directory.path().join("cache");
        wav(&input, 0.1);
        let cache = AnalysisCache::new(&cache_root, AnalysisCachePolicy::default()).unwrap();

        let first = cache.analyze_file(&input, None).unwrap();
        assert_eq!(first.disposition, CacheDisposition::Stored);
        let second = cache.analyze_file(&input, None).unwrap();
        assert_eq!(second.disposition, CacheDisposition::Hit);
        assert_eq!(first.value.frames, second.value.frames);
        assert_eq!(first.value.lufs, second.value.lufs);

        wav(&input, 0.2);
        let changed = cache.analyze_file(&input, None).unwrap();
        assert_eq!(changed.disposition, CacheDisposition::Stored);
        assert_ne!(first.value.lufs, changed.value.lufs);
    }

    #[test]
    fn request_options_are_part_of_the_address() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("tone.wav");
        wav(&input, 0.1);
        let cache = AnalysisCache::new(
            directory.path().join("cache"),
            AnalysisCachePolicy::default(),
        )
        .unwrap();
        assert_eq!(
            cache
                .analyze_range(&input, None, 0.0, None, None)
                .unwrap()
                .disposition,
            CacheDisposition::Stored
        );
        assert_eq!(
            cache
                .analyze_range(&input, None, 0.0, Some(0.5), None)
                .unwrap()
                .disposition,
            CacheDisposition::Stored
        );
        assert_eq!(
            cache
                .analyze_range(&input, None, 0.0, Some(0.5), None)
                .unwrap()
                .disposition,
            CacheDisposition::Hit
        );
    }

    #[test]
    fn corrupt_entry_is_repaired_or_observed_in_read_only_mode() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("tone.wav");
        let cache_root = directory.path().join("cache");
        wav(&input, 0.1);
        let cache = AnalysisCache::new(&cache_root, AnalysisCachePolicy::default()).unwrap();
        cache.analyze_file(&input, None).unwrap();
        let entry = cache.recognized_entries().unwrap().pop().unwrap().path;
        fs::write(&entry, br#"{"broken":"#).unwrap();

        let read_only = AnalysisCache::new(
            &cache_root,
            AnalysisCachePolicy {
                read_only: true,
                ..AnalysisCachePolicy::default()
            },
        )
        .unwrap();
        let ignored = read_only.analyze_file(&input, None).unwrap();
        assert_eq!(ignored.disposition, CacheDisposition::ReadOnlyInvalid);
        assert!(ignored.warning.is_some());
        assert_eq!(fs::read(&entry).unwrap(), br#"{"broken":"#);

        let repaired = cache.analyze_file(&input, None).unwrap();
        assert_eq!(repaired.disposition, CacheDisposition::Repaired);
        assert!(repaired.warning.is_some());
        let mut valid_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
        valid_json["result"]["sample_peak_linear"] = serde_json::json!(0.123);
        fs::write(&entry, serde_json::to_vec_pretty(&valid_json).unwrap()).unwrap();
        let payload_repaired = cache.analyze_file(&input, None).unwrap();
        assert_eq!(payload_repaired.disposition, CacheDisposition::Repaired);
        assert!(payload_repaired
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("result SHA-256")));
    }

    #[test]
    fn silence_and_incomplete_timeline_windows_remain_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("silence.wav");
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 1,
                frames: 48_000,
                data: vec![vec![0.0; 48_000]],
                channel_roles: vec![ChannelRole::Main],
                source_kind: PcmKind::S16,
            },
            PcmKind::S16,
            false,
        )
        .unwrap();
        let cache = AnalysisCache::new(
            directory.path().join("cache"),
            AnalysisCachePolicy::default(),
        )
        .unwrap();
        let stored = cache
            .analyze_range(&input, None, 0.0, None, Some(100.0))
            .unwrap();
        let hit = cache
            .analyze_range(&input, None, 0.0, None, Some(100.0))
            .unwrap();
        assert_eq!(hit.disposition, CacheDisposition::Hit);
        assert_eq!(stored.value.analysis.lufs, f64::NEG_INFINITY);
        assert_eq!(hit.value.analysis.lufs, f64::NEG_INFINITY);
        assert!(stored
            .value
            .timeline
            .iter()
            .any(|point| point.momentary_lufs.is_none()));
        assert_eq!(
            stored
                .value
                .timeline
                .iter()
                .map(|point| point.momentary_lufs)
                .collect::<Vec<_>>(),
            hit.value
                .timeline
                .iter()
                .map(|point| point.momentary_lufs)
                .collect::<Vec<_>>()
        );

        let explicit = TimedAnalysis {
            analysis: stored.value.analysis,
            timeline: vec![
                LoudnessTimelinePoint {
                    start_seconds: 0.0,
                    end_seconds: 0.1,
                    momentary_lufs: None,
                    short_term_lufs: None,
                    sample_peak_dbfs: f64::NEG_INFINITY,
                    true_peak_dbtp: f64::NEG_INFINITY,
                },
                LoudnessTimelinePoint {
                    start_seconds: 0.1,
                    end_seconds: 0.2,
                    momentary_lufs: Some(f64::NEG_INFINITY),
                    short_term_lufs: Some(f64::NEG_INFINITY),
                    sample_peak_dbfs: f64::NEG_INFINITY,
                    true_peak_dbtp: f64::NEG_INFINITY,
                },
            ],
        };
        let bytes = serde_json::to_vec(&TimedAnalysisRecord::from_analysis(&explicit)).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("\"-inf\""));
        let round_trip = serde_json::from_slice::<TimedAnalysisRecord>(&bytes)
            .unwrap()
            .into_analysis()
            .unwrap();
        assert_eq!(round_trip.timeline[0].momentary_lufs, None);
        assert_eq!(
            round_trip.timeline[1].momentary_lufs,
            Some(f64::NEG_INFINITY)
        );
    }

    #[test]
    fn size_limit_evicts_older_entries() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        wav(&first, 0.1);
        wav(&second, 0.2);
        let cache_root = directory.path().join("cache");
        let cache = AnalysisCache::new(&cache_root, AnalysisCachePolicy::default()).unwrap();
        cache.analyze_file(&first, None).unwrap();
        let first_size = cache.recognized_entries().unwrap()[0].bytes;
        let cache = AnalysisCache::new(
            &cache_root,
            AnalysisCachePolicy {
                read_only: false,
                max_bytes: first_size + 1024,
            },
        )
        .unwrap();
        cache.analyze_file(&second, None).unwrap();
        let entries = cache.recognized_entries().unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn emitted_entry_matches_the_published_schema() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("tone.wav");
        wav(&input, 0.1);
        let cache = AnalysisCache::new(
            directory.path().join("cache"),
            AnalysisCachePolicy::default(),
        )
        .unwrap();
        cache
            .analyze_range(&input, None, 0.0, None, Some(100.0))
            .unwrap();
        let entry = cache.recognized_entries().unwrap().pop().unwrap().path;
        let instance: serde_json::Value =
            serde_json::from_slice(&fs::read(entry).unwrap()).unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/analysis-cache-v1.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.validate(&instance).is_ok());

        let mut defective = instance;
        defective["result"]["sample_peak_linear"] = serde_json::json!(-1.0);
        assert!(validator.validate(&defective).is_err());
    }

    #[test]
    fn input_changed_during_measurement_is_not_cached() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("tone.wav");
        wav(&input, 0.1);
        let cache = AnalysisCache::new(
            directory.path().join("cache"),
            AnalysisCachePolicy::default(),
        )
        .unwrap();
        let request = RequestRecord::Range {
            channel_roles: None,
            start_seconds: 0.0,
            duration_seconds: None,
            timeline_interval_ms: None,
        };
        let error = cache
            .lookup_or_compute(&input, request, || {
                let measured =
                    normalize::analyze_file_range_with_roles(&input, None, 0.0, None, None)?;
                fs::write(&input, b"changed during analysis").unwrap();
                Ok(measured)
            })
            .unwrap_err();
        assert!(error.contains("changed while"));
        assert!(cache.recognized_entries().unwrap().is_empty());
    }
}
