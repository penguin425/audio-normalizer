//! Exact sample-time transforms for RIFF/WAVE metadata.
//!
//! This module intentionally operates on the complete encoded WAVE byte
//! stream.  [`transform_wave`] therefore never has to reconstruct a
//! `WaveChunk` from a lossy representation: chunk order, unknown chunks,
//! chunk headers, odd padding bytes, and bytes after the declared RIFF form
//! are copied verbatim.  Only fields whose sample-domain meaning is fixed by
//! the WAVE/BWF structures are edited.
//!
//! The transform uses the following explicit clock convention.  A source
//! sample position `p` is mapped to
//!
//! ```text
//! round_nearest_away_from_zero((p - crop_origin) * output_rate / source_rate)
//! ```
//!
//! where `crop_origin` is measured in source frames and the output frame at
//! index zero corresponds to that source position.  Durations use the same
//! rational ratio without subtracting `crop_origin`.  The rounding is exact
//! integer arithmetic; no `f32`/`f64` conversion is used.  This module uses
//! the crate-wide policy and sample-time types so all container adapters share
//! the same tie handling and publication semantics.

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fmt;

use crate::metadata_fidelity::sha256_hex;
pub use crate::metadata_fidelity::MetadataPolicy;
use crate::sample_time::{RoundingMode, SampleTimeError, SampleTimeTransform};

const RIFF_HEADER_BYTES: usize = 12;
const CHUNK_HEADER_BYTES: usize = 8;
const CUE_POINT_BYTES: usize = 24;
const LTXT_MIN_BYTES: usize = 20;
const SMPL_HEADER_BYTES: usize = 36;
const SMPL_LOOP_BYTES: usize = 24;

/// Maximum number of field/chunk decisions emitted by one timing transform.
/// This is aligned with the metadata registry's default entry budget so a
/// bounded raw metadata region cannot fan out into an unbounded report.
pub const MAX_TIMING_LEDGER_ENTRIES: usize = 100_000;
const MAX_DECLARED_TIMING_ITEMS: usize = MAX_TIMING_LEDGER_ENTRIES;
// A WAVE source with no sample-domain fields could otherwise still force the
// parser to retain one `ChunkSpan` per tiny physical chunk. Keep the framing
// inventory bounded by the same contract as the timing ledger.
const MAX_WAVE_CHUNKS: usize = MAX_TIMING_LEDGER_ENTRIES;

/// Four-byte WAVE chunk identifier.
pub type ChunkId = [u8; 4];

/// Deterministic action recorded for one metadata field or chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimingLedgerAction {
    /// The field was mapped to the output sample clock.
    Mapped,
    /// The source bytes were deliberately retained unchanged.
    Preserved,
    /// The explicitly selected chunk was removed.
    Dropped,
}

/// One auditable metadata-fidelity ledger entry.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TimingLedgerEntry {
    /// Zero-based index in the source ordered chunk list.
    pub chunk_index: usize,
    /// Chunk containing the field, or the selected chunk that was dropped.
    pub chunk_id: ChunkId,
    /// Stable field name such as `TimeReference`, `cue.dwPosition`, or
    /// `smpl.loop[0].dwStart`.
    pub field: String,
    /// Why a value was retained or changed.
    pub detail: String,
    /// The action taken for this entry.
    pub action: TimingLedgerAction,
    /// True when this is a Preserve-mode candidate requiring a policy review.
    pub candidate: bool,
    /// Numeric value before mapping, when the field was decoded successfully.
    /// A signed representation accommodates positions relative to a crop and
    /// the otherwise unsigned duration/period fields.
    pub before_value: Option<i128>,
    /// Numeric value after mapping, when one was written.
    pub after_value: Option<i128>,
    /// SHA-256 of the exact source field bytes, when this entry identifies a
    /// decoded fixed-width field.  Chunk-level opaque candidates may leave it
    /// absent because no field boundary was proven.
    pub source_field_sha256: Option<String>,
    /// SHA-256 of the exact destination field bytes written by the transform.
    /// It is absent for preserved candidates and dropped chunks.
    pub destination_field_sha256: Option<String>,
    /// Exact tie rule used for a mapped value.  It is absent for preserved,
    /// dropped, opaque, or malformed fields.
    pub rounding: Option<RoundingMode>,
}

impl TimingLedgerEntry {
    /// SHA-256 of the source field bytes, when a fixed-width field was
    /// identified by the timing adapter.
    pub fn source_field_sha256(&self) -> Option<&str> {
        self.source_field_sha256.as_deref()
    }

    /// SHA-256 of the destination field bytes, when the field was written by
    /// the timing adapter.
    pub fn destination_field_sha256(&self) -> Option<&str> {
        self.destination_field_sha256.as_deref()
    }

    /// Alias for callers that use the report vocabulary (`before_sha256`).
    pub fn before_sha256(&self) -> Option<&str> {
        self.source_field_sha256()
    }

    /// Alias for callers that use the report vocabulary (`after_sha256`).
    pub fn after_sha256(&self) -> Option<&str> {
        self.destination_field_sha256()
    }
}

/// Source/output clock and crop origin used by a metadata transform.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaveTimingTransformOptions {
    /// Source sample rate in Hz.
    pub source_rate_hz: u32,
    /// Output sample rate in Hz.
    pub output_rate_hz: u32,
    /// Source-frame index represented by output frame zero.  A negative value
    /// represents an output that starts before source frame zero.
    pub crop_origin_source_frames: i128,
    /// Metadata-fidelity behavior. `LegacyGeneric` intentionally follows
    /// Preserve semantics for this structured adapter; it is retained only
    /// as an explicit compatibility choice for callers using the historical
    /// generic-tag policy.
    pub policy: MetadataPolicy,
    /// Chunk IDs to remove when `policy` is [`MetadataPolicy::Strip`].
    pub strip_chunks: Vec<ChunkId>,
}

impl WaveTimingTransformOptions {
    /// Construct options with an explicit source/output rate and crop origin.
    pub fn new(
        source_rate_hz: u32,
        output_rate_hz: u32,
        crop_origin_source_frames: i128,
        policy: MetadataPolicy,
        strip_chunks: impl Into<Vec<ChunkId>>,
    ) -> Result<Self, WaveTimingError> {
        if source_rate_hz == 0 || output_rate_hz == 0 {
            return Err(WaveTimingError::InvalidRate {
                source_rate_hz,
                output_rate_hz,
            });
        }
        let strip_chunks = strip_chunks.into();
        if strip_chunks.len() > MAX_DECLARED_TIMING_ITEMS {
            return Err(WaveTimingError::StripLimitExceeded {
                requested: strip_chunks.len(),
                limit: MAX_DECLARED_TIMING_ITEMS,
            });
        }
        if !strip_chunks.is_empty() && policy != MetadataPolicy::Strip {
            return Err(WaveTimingError::InvalidStripScope { policy });
        }
        for id in &strip_chunks {
            if is_structural_chunk(*id) {
                return Err(WaveTimingError::InvalidStripChunk { id: *id });
            }
        }
        Ok(Self {
            source_rate_hz,
            output_rate_hz,
            crop_origin_source_frames,
            policy,
            strip_chunks,
        })
    }

    /// Construct Preserve-mode options without stripping any chunks.
    pub fn preserve(
        source_rate_hz: u32,
        output_rate_hz: u32,
        crop_origin_source_frames: i128,
    ) -> Result<Self, WaveTimingError> {
        Self::new(
            source_rate_hz,
            output_rate_hz,
            crop_origin_source_frames,
            MetadataPolicy::Preserve,
            Vec::new(),
        )
    }

    /// Construct Strict-mode options without stripping any chunks.
    pub fn strict(
        source_rate_hz: u32,
        output_rate_hz: u32,
        crop_origin_source_frames: i128,
    ) -> Result<Self, WaveTimingError> {
        Self::new(
            source_rate_hz,
            output_rate_hz,
            crop_origin_source_frames,
            MetadataPolicy::Strict,
            Vec::new(),
        )
    }

    /// Construct Strip-mode options for exactly the supplied chunk IDs.
    pub fn strip(
        source_rate_hz: u32,
        output_rate_hz: u32,
        crop_origin_source_frames: i128,
        strip_chunks: impl Into<Vec<ChunkId>>,
    ) -> Result<Self, WaveTimingError> {
        Self::new(
            source_rate_hz,
            output_rate_hz,
            crop_origin_source_frames,
            MetadataPolicy::Strip,
            strip_chunks,
        )
    }
}

/// Result of a byte-preserving WAVE metadata transform.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaveTimingTransformResult {
    /// Complete output WAVE bytes.
    pub bytes: Vec<u8>,
    /// Actions and Preserve candidates in deterministic chunk/field order.
    pub ledger: Vec<TimingLedgerEntry>,
    /// True when output bytes differ from the input bytes.
    pub changed: bool,
    /// XML-bearing timing containers intentionally left opaque.  In
    /// particular, ADM/S-ADM XML is not rewritten by this module.
    pub unsupported_xml_chunks: Vec<ChunkId>,
}

/// Errors that make a WAVE timing transform unsafe or impossible.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WaveTimingError {
    /// Input is not a RIFF/RF64/BW64 WAVE stream.
    NotWave,
    /// The container/chunk byte structure is malformed.
    Malformed {
        chunk_index: Option<usize>,
        chunk_id: Option<ChunkId>,
        detail: String,
    },
    /// A supported field's sample meaning cannot be established.
    Ambiguous {
        chunk_index: usize,
        chunk_id: ChunkId,
        field: String,
        detail: String,
    },
    /// A checked signed/unsigned rational operation overflowed or produced a
    /// value outside the destination field's representation.
    Overflow {
        chunk_index: usize,
        chunk_id: ChunkId,
        field: String,
        detail: String,
    },
    /// A caller supplied an invalid sample rate.
    InvalidRate {
        source_rate_hz: u32,
        output_rate_hz: u32,
    },
    /// A bounded metadata body could generate more timing ledger decisions
    /// than the report contract permits.
    LedgerLimitExceeded {
        chunk_index: usize,
        chunk_id: ChunkId,
        requested: usize,
        limit: usize,
        detail: String,
    },
    /// Structural WAVE chunks cannot be removed by the metadata Strip mode.
    InvalidStripChunk { id: ChunkId },
    /// A chunk removal list is only meaningful with the common Strip policy.
    InvalidStripScope { policy: MetadataPolicy },
    /// The caller supplied more strip identifiers than can be inspected
    /// within the bounded timing work budget.
    StripLimitExceeded { requested: usize, limit: usize },
}

impl fmt::Display for WaveTimingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotWave => formatter.write_str("not a RIFF/RF64/BW64 WAVE stream"),
            Self::Malformed {
                chunk_index,
                chunk_id,
                detail,
            } => write_chunk_error(formatter, "malformed", *chunk_index, *chunk_id, detail),
            Self::Ambiguous {
                chunk_index,
                chunk_id,
                field,
                detail,
            } => write!(
                formatter,
                "ambiguous {} at chunk {} {}: {}",
                field,
                chunk_index,
                id_text(*chunk_id),
                detail
            ),
            Self::Overflow {
                chunk_index,
                chunk_id,
                field,
                detail,
            } => write!(
                formatter,
                "overflow mapping {} at chunk {} {}: {}",
                field,
                chunk_index,
                id_text(*chunk_id),
                detail
            ),
            Self::InvalidRate {
                source_rate_hz,
                output_rate_hz,
            } => write!(
                formatter,
                "sample rates must be non-zero (source={source_rate_hz}, output={output_rate_hz})"
            ),
            Self::LedgerLimitExceeded {
                chunk_index,
                chunk_id,
                requested,
                limit,
                detail,
            } => write!(
                formatter,
                "timing ledger limit exceeded at chunk {chunk_index} {} ({requested}>{limit}): {detail}",
                id_text(*chunk_id)
            ),
            Self::InvalidStripChunk { id } => write!(
                formatter,
                "cannot strip structural WAVE chunk {}",
                id_text(*id)
            ),
            Self::InvalidStripScope { policy } => write!(
                formatter,
                "chunk strip scope requires metadata policy=strip (got {policy:?})"
            ),
            Self::StripLimitExceeded { requested, limit } => write!(
                formatter,
                "chunk strip scope contains {requested} identifiers; maximum is {limit}"
            ),
        }
    }
}

impl Error for WaveTimingError {}

fn write_chunk_error(
    formatter: &mut fmt::Formatter<'_>,
    kind: &str,
    chunk_index: Option<usize>,
    chunk_id: Option<ChunkId>,
    detail: &str,
) -> fmt::Result {
    match (chunk_index, chunk_id) {
        (Some(index), Some(id)) => {
            write!(formatter, "{kind} chunk {index} {}: {detail}", id_text(id))
        }
        (Some(index), None) => write!(formatter, "{kind} chunk {index}: {detail}"),
        _ => write!(formatter, "{kind} WAVE: {detail}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerKind {
    Riff,
    Rf64,
    Bw64,
}

#[derive(Clone, Copy, Debug)]
struct ChunkSpan {
    id: ChunkId,
    header_start: usize,
    body_start: usize,
    body_end: usize,
    end: usize,
    /// Index of the ds64 table entry consumed for this chunk, if any.
    ds64_table_index: Option<usize>,
}

#[derive(Clone, Debug)]
struct ContainerInfo {
    kind: ContainerKind,
    declared_end: usize,
    riff_size: u64,
    ds64_index: Option<usize>,
    ds64: Option<Ds64Info>,
}

#[derive(Clone, Debug)]
struct Ds64TableEntry {
    id: ChunkId,
    size: u64,
}

#[derive(Clone, Debug)]
struct Ds64Info {
    riff_size: u64,
    data_size: u64,
    sample_count: u64,
    table: Vec<Ds64TableEntry>,
}

struct Ds64Resolver<'a> {
    info: &'a Ds64Info,
    table_used: Vec<bool>,
    table_by_id: HashMap<ChunkId, VecDeque<usize>>,
    data_used: bool,
}

impl<'a> Ds64Resolver<'a> {
    fn new(info: &'a Ds64Info) -> Self {
        let mut table_by_id = HashMap::<ChunkId, VecDeque<usize>>::new();
        for (index, entry) in info.table.iter().enumerate() {
            table_by_id.entry(entry.id).or_default().push_back(index);
        }
        Self {
            info,
            table_used: vec![false; info.table.len()],
            table_by_id,
            data_used: false,
        }
    }

    fn resolve(
        &mut self,
        raw_size: u32,
        id: ChunkId,
        chunk_index: usize,
    ) -> Result<(u64, Option<usize>), WaveTimingError> {
        if raw_size != u32::MAX {
            return Ok((u64::from(raw_size), None));
        }
        if id == *b"data" {
            if self.data_used {
                return Err(malformed(
                    Some(chunk_index),
                    Some(id),
                    "multiple sentinel data chunks cannot share ds64 dataSize",
                ));
            }
            self.data_used = true;
            return Ok((self.info.data_size, None));
        }
        let Some(table_index) = self.table_by_id.get_mut(&id).and_then(VecDeque::pop_front) else {
            return Err(malformed(
                Some(chunk_index),
                Some(id),
                "sentinel chunk size has no matching ds64 table entry",
            ));
        };
        self.table_used[table_index] = true;
        Ok((self.info.table[table_index].size, Some(table_index)))
    }

    fn finish(self) -> Result<(), WaveTimingError> {
        if let Some(index) = self.table_used.iter().position(|used| !used) {
            return Err(malformed(
                None,
                Some(self.info.table[index].id),
                "ds64 table contains an entry for no sentinel chunk",
            ));
        }
        Ok(())
    }
}

fn map_position_i128(
    mapper: &SampleTimeTransform,
    value: i128,
    crop_origin: i128,
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
) -> Result<i128, WaveTimingError> {
    let relative = value
        .checked_sub(crop_origin)
        .ok_or_else(|| overflow(chunk_index, chunk_id, field, "crop subtraction overflow"))?;
    mapper
        .map_signed_i128_with_rounding(relative, RoundingMode::HalfUp)
        .map_err(|error| sample_time_failure(chunk_index, chunk_id, field, error))
}

fn map_position_u64(
    mapper: &SampleTimeTransform,
    crop_origin: i128,
    value: u64,
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
) -> Result<u64, WaveTimingError> {
    let mapped = map_position_i128(
        mapper,
        i128::from(value),
        crop_origin,
        chunk_index,
        chunk_id,
        field,
    )?;
    u64::try_from(mapped).map_err(|_| {
        overflow(
            chunk_index,
            chunk_id,
            field,
            "mapped position is negative or does not fit u64",
        )
    })
}

/// Map a BWF absolute sample clock value. `TimeReference` is measured from
/// midnight, not from the first retained output sample, so a crop advances the
/// source clock before the source-to-output rate conversion is applied.
fn map_absolute_position_u64(
    mapper: &SampleTimeTransform,
    crop_origin: i128,
    value: u64,
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
) -> Result<u64, WaveTimingError> {
    let absolute = i128::from(value).checked_add(crop_origin).ok_or_else(|| {
        overflow(
            chunk_index,
            chunk_id,
            field,
            "absolute clock addition overflow",
        )
    })?;
    let mapped = mapper
        .map_signed_i128_with_rounding(absolute, RoundingMode::HalfUp)
        .map_err(|error| sample_time_failure(chunk_index, chunk_id, field, error))?;
    u64::try_from(mapped).map_err(|_| {
        overflow(
            chunk_index,
            chunk_id,
            field,
            "mapped absolute clock is negative or does not fit u64",
        )
    })
}

fn map_position_u32(
    mapper: &SampleTimeTransform,
    crop_origin: i128,
    value: u32,
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
) -> Result<u32, WaveTimingError> {
    let mapped = map_position_u64(
        mapper,
        crop_origin,
        u64::from(value),
        chunk_index,
        chunk_id,
        field,
    )?;
    u32::try_from(mapped).map_err(|_| {
        overflow(
            chunk_index,
            chunk_id,
            field,
            "mapped position does not fit u32",
        )
    })
}

fn map_duration_u32(
    mapper: &SampleTimeTransform,
    value: u32,
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
) -> Result<u32, WaveTimingError> {
    let mapped = mapper
        .map_sample_with_rounding(u64::from(value), RoundingMode::HalfUp)
        .map_err(|error| sample_time_failure(chunk_index, chunk_id, field, error))?;
    u32::try_from(mapped).map_err(|_| {
        overflow(
            chunk_index,
            chunk_id,
            field,
            "mapped duration does not fit u32",
        )
    })
}

fn map_period_ns_u32(
    mapper: &SampleTimeTransform,
    value: u32,
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
) -> Result<u32, WaveTimingError> {
    let mapped = mapper
        .map_sample_with_rounding(u64::from(value), RoundingMode::HalfUp)
        .map_err(|error| sample_time_failure(chunk_index, chunk_id, field, error))?;
    let mapped = u32::try_from(mapped).map_err(|_| {
        overflow(
            chunk_index,
            chunk_id,
            field,
            "mapped samplePeriod does not fit u32",
        )
    })?;
    if mapped == 0 {
        return Err(overflow(
            chunk_index,
            chunk_id,
            field,
            "mapped samplePeriod would be zero, which is not a valid WAVE sample period",
        ));
    }
    Ok(mapped)
}

fn sample_time_failure(
    chunk_index: usize,
    chunk_id: ChunkId,
    field: &str,
    error: SampleTimeError,
) -> WaveTimingError {
    overflow(chunk_index, chunk_id, field, &error.to_string())
}

fn sample_time_options_failure(
    options: &WaveTimingTransformOptions,
    error: SampleTimeError,
) -> WaveTimingError {
    match error {
        SampleTimeError::ZeroRate {
            source_rate,
            target_rate,
        } => WaveTimingError::InvalidRate {
            source_rate_hz: source_rate,
            output_rate_hz: target_rate,
        },
        other => overflow(
            usize::MAX,
            *b"WAVE",
            "sample-clock",
            &format!(
                "cannot construct sample clock for source={} output={}: {other}",
                options.source_rate_hz, options.output_rate_hz
            ),
        ),
    }
}

fn overflow(chunk_index: usize, chunk_id: ChunkId, field: &str, detail: &str) -> WaveTimingError {
    WaveTimingError::Overflow {
        chunk_index,
        chunk_id,
        field: field.into(),
        detail: detail.into(),
    }
}

fn ledger_limit_exceeded(
    chunk_index: usize,
    chunk_id: ChunkId,
    requested: usize,
    detail: impl Into<String>,
) -> WaveTimingError {
    WaveTimingError::LedgerLimitExceeded {
        chunk_index,
        chunk_id,
        requested,
        limit: MAX_TIMING_LEDGER_ENTRIES,
        detail: detail.into(),
    }
}

/// Conservative upper bound for entries that one chunk can append.  The
/// bound is deliberately an overestimate: rejecting a body before decoding
/// its declarations is preferable to transforming fields while silently
/// dropping ledger evidence after the report budget is exhausted.
fn chunk_ledger_upper_bound(
    source: &[u8],
    id: ChunkId,
    chunk_index: usize,
) -> Result<usize, WaveTimingError> {
    let requested = match id {
        id if id == *b"bext" => 1,
        id if id == *b"cue " => {
            if source.len() < 4 {
                1
            } else {
                let count = u32::from_le_bytes(source[..4].try_into().unwrap()) as usize;
                if count > MAX_DECLARED_TIMING_ITEMS {
                    return Err(ledger_limit_exceeded(
                        chunk_index,
                        id,
                        count,
                        "cue point count exceeds the bounded timing declaration limit",
                    ));
                }
                count.checked_mul(2).ok_or_else(|| {
                    ledger_limit_exceeded(
                        chunk_index,
                        id,
                        usize::MAX,
                        "cue ledger entry count overflows usize",
                    )
                })?
            }
        }
        id if id == *b"LIST" => {
            if source.len() < 4 {
                1
            } else if &source[..4] != b"adtl" {
                // Non-adtl LIST payloads are opaque to this adapter and emit
                // at most one candidate, regardless of their byte length.
                1
            } else {
                // Every nested subchunk occupies at least an eight-byte
                // header. This is a safe upper bound even for malformed
                // lengths; parse_subchunks remains responsible for the exact
                // structural error after the budget check.
                let count = source.len().saturating_sub(4) / CHUNK_HEADER_BYTES;
                if count > MAX_DECLARED_TIMING_ITEMS {
                    return Err(ledger_limit_exceeded(
                        chunk_index,
                        id,
                        count,
                        "LIST subchunk count exceeds the bounded timing declaration limit",
                    ));
                }
                count.max(1)
            }
        }
        id if id == *b"smpl" => {
            if source.len() < SMPL_HEADER_BYTES {
                1
            } else {
                let count = u32::from_le_bytes(source[28..32].try_into().unwrap()) as usize;
                if count > MAX_DECLARED_TIMING_ITEMS {
                    return Err(ledger_limit_exceeded(
                        chunk_index,
                        id,
                        count,
                        "smpl loop count exceeds the bounded timing declaration limit",
                    ));
                }
                1usize
                    .checked_add(count.checked_mul(2).ok_or_else(|| {
                        ledger_limit_exceeded(
                            chunk_index,
                            id,
                            usize::MAX,
                            "smpl ledger entry count overflows usize",
                        )
                    })?)
                    .ok_or_else(|| {
                        ledger_limit_exceeded(
                            chunk_index,
                            id,
                            usize::MAX,
                            "smpl ledger entry count overflows usize",
                        )
                    })?
            }
        }
        id if id == *b"axml" || id == *b"sxml" || id == *b"bxml" || id == *b"iXML" => 1,
        id if is_structural_chunk(id) || is_known_non_timing_chunk(id) => 0,
        _ => 1,
    };
    Ok(requested)
}

/// Transform sample-indexed BWF/WAVE metadata in a complete encoded stream.
pub fn transform_wave(
    input: &[u8],
    options: &WaveTimingTransformOptions,
) -> Result<WaveTimingTransformResult, WaveTimingError> {
    // Revalidate options here as callers may construct the public fields
    // directly instead of using `new`.
    let checked = WaveTimingTransformOptions::new(
        options.source_rate_hz,
        options.output_rate_hz,
        options.crop_origin_source_frames,
        options.policy,
        options.strip_chunks.clone(),
    )?;
    let (container, chunks) = parse_wave(input)?;
    let source_to_output = SampleTimeTransform::new(checked.source_rate_hz, checked.output_rate_hz)
        .map_err(|error| sample_time_options_failure(&checked, error))?;
    let output_to_source = SampleTimeTransform::new(checked.output_rate_hz, checked.source_rate_hz)
        .map_err(|error| sample_time_options_failure(&checked, error))?;
    let strip_chunks = checked.strip_chunks.iter().copied().collect::<HashSet<_>>();
    let duplicate_chunks = duplicate_transform_groups(input, &chunks);
    let single_data_chunk = chunks.iter().filter(|span| span.id == *b"data").count() == 1;
    let clock_changed =
        checked.source_rate_hz != checked.output_rate_hz || checked.crop_origin_source_frames != 0;
    let mut ledger = Vec::new();
    let mut unsupported_xml_chunks = Vec::new();
    let mut output = Vec::with_capacity(input.len());
    output.extend_from_slice(&input[..RIFF_HEADER_BYTES]);
    let mut removed_bytes = 0_u64;
    let mut output_ds64_body_start = None;
    let mut removed_ds64_table_indices = Vec::new();

    for (index, span) in chunks.iter().enumerate() {
        let body = &input[span.body_start..span.body_end];
        let chunk_upper_bound = if strip_chunks.contains(&span.id) {
            1
        } else {
            chunk_ledger_upper_bound(body, span.id, index)?
        };
        let requested_entries = ledger.len().checked_add(chunk_upper_bound).ok_or_else(|| {
            ledger_limit_exceeded(
                index,
                span.id,
                usize::MAX,
                "timing ledger entry count overflows usize",
            )
        })?;
        if requested_entries > MAX_TIMING_LEDGER_ENTRIES {
            return Err(ledger_limit_exceeded(
                index,
                span.id,
                requested_entries,
                "declared timing fields exceed the bounded report ledger",
            ));
        }
        if strip_chunks.contains(&span.id) {
            if span.id == *b"ds64" {
                // `new` rejects structural IDs, but retain this guard if the
                // option value was changed through a direct struct literal.
                return Err(WaveTimingError::InvalidStripChunk { id: span.id });
            }
            ledger.push(TimingLedgerEntry {
                chunk_index: index,
                chunk_id: span.id,
                field: "chunk".into(),
                detail: "explicitly selected by Strip policy".into(),
                action: TimingLedgerAction::Dropped,
                candidate: false,
                before_value: None,
                after_value: None,
                source_field_sha256: None,
                destination_field_sha256: None,
                rounding: None,
            });
            removed_bytes = removed_bytes
                .checked_add((span.end - span.header_start) as u64)
                .ok_or_else(|| {
                    malformed(
                        Some(index),
                        Some(span.id),
                        "removed-byte accounting overflow",
                    )
                })?;
            if let Some(table_index) = span.ds64_table_index {
                removed_ds64_table_indices.push(table_index);
            }
            continue;
        }

        let duplicate = duplicate_chunks.contains(&index);
        let transformed = match span.id {
            id if id == *b"bext" => transform_bext(
                body,
                &source_to_output,
                checked.crop_origin_source_frames,
                index,
                checked.policy,
                duplicate,
                clock_changed,
                &mut ledger,
            )?,
            id if id == *b"cue " => transform_cue(
                body,
                &source_to_output,
                checked.crop_origin_source_frames,
                index,
                checked.policy,
                duplicate,
                single_data_chunk,
                clock_changed,
                &mut ledger,
            )?,
            id if id == *b"LIST" => transform_list(
                body,
                &source_to_output,
                index,
                checked.policy,
                duplicate,
                clock_changed,
                &mut ledger,
            )?,
            id if id == *b"smpl" => transform_smpl(
                body,
                &source_to_output,
                &output_to_source,
                checked.crop_origin_source_frames,
                index,
                checked.policy,
                duplicate,
                &mut ledger,
            )?,
            id if id == *b"axml" || id == *b"sxml" || id == *b"bxml" || id == *b"iXML" => {
                if !unsupported_xml_chunks.contains(&id) {
                    unsupported_xml_chunks.push(id);
                }
                if policy_is_strict(checked.policy) && clock_changed {
                    return Err(WaveTimingError::Ambiguous {
                        chunk_index: index,
                        chunk_id: id,
                        field: "chunk".into(),
                        detail:
                            "XML-bearing timing metadata is opaque while the sample clock changes"
                                .into(),
                    });
                }
                if clock_changed {
                    ledger.push(candidate_entry(
                        index,
                        id,
                        "chunk",
                        "XML-bearing timing metadata is opaque and was preserved",
                    ));
                }
                None
            }
            id if clock_changed && !is_structural_chunk(id) && !is_known_non_timing_chunk(id) => {
                let detail =
                    "unregistered WAVE chunk may contain sample-domain timing and was preserved";
                if policy_is_strict(checked.policy) {
                    return Err(WaveTimingError::Ambiguous {
                        chunk_index: index,
                        chunk_id: id,
                        field: "chunk".into(),
                        detail: detail.into(),
                    });
                }
                ledger.push(candidate_entry(index, id, "chunk", detail));
                None
            }
            _ => None,
        };
        let new_body = transformed.as_deref().unwrap_or(body);
        output.extend_from_slice(&input[span.header_start..span.body_start]);
        if span.id == *b"ds64" {
            output_ds64_body_start = Some(output.len());
        }
        if new_body.len() != body.len() {
            // Every supported transform is fixed-width. Keep this guard so a
            // future extension cannot silently corrupt a chunk size field.
            return Err(malformed(
                Some(index),
                Some(span.id),
                "timing transform changed chunk body length",
            ));
        }
        output.extend_from_slice(new_body);
        output.extend_from_slice(&input[span.body_end..span.end]);
    }
    output.extend_from_slice(&input[container.declared_end..]);

    if removed_bytes > 0 || !removed_ds64_table_indices.is_empty() {
        let old_size = container.riff_size;
        let removed_table_bytes = u64::try_from(removed_ds64_table_indices.len())
            .ok()
            .and_then(|count| count.checked_mul(12))
            .ok_or_else(|| malformed(None, None, "removed ds64 table accounting overflow"))?;
        let total_removed = removed_bytes
            .checked_add(removed_table_bytes)
            .ok_or_else(|| malformed(None, None, "removed-byte accounting overflow"))?;
        let new_size = old_size.checked_sub(total_removed).ok_or_else(|| {
            malformed(
                None,
                None,
                "removed bytes exceed declared RIFF container size",
            )
        })?;
        match container.kind {
            ContainerKind::Riff => {
                let new_size = u32::try_from(new_size)
                    .map_err(|_| malformed(None, None, "updated RIFF size does not fit u32"))?;
                output[4..8].copy_from_slice(&new_size.to_le_bytes());
            }
            ContainerKind::Rf64 | ContainerKind::Bw64 => {
                let ds64_index = container.ds64_index.ok_or_else(|| {
                    malformed(None, None, "RF64/BW64 Strip requires a ds64 chunk")
                })?;
                let offset = output_ds64_body_start.ok_or_else(|| {
                    malformed(
                        Some(ds64_index),
                        Some(*b"ds64"),
                        "ds64 was not retained while applying Strip",
                    )
                })?;
                let ds64 = container.ds64.as_ref().ok_or_else(|| {
                    malformed(
                        Some(ds64_index),
                        Some(*b"ds64"),
                        "RF64/BW64 Strip requires parsed ds64 metadata",
                    )
                })?;
                rewrite_ds64(
                    &mut output,
                    offset,
                    ds64,
                    &removed_ds64_table_indices,
                    new_size,
                    ds64_index,
                )?;
            }
        }
    }

    Ok(WaveTimingTransformResult {
        changed: output != input,
        bytes: output,
        ledger,
        unsupported_xml_chunks,
    })
}

fn parse_wave(input: &[u8]) -> Result<(ContainerInfo, Vec<ChunkSpan>), WaveTimingError> {
    if input.len() < RIFF_HEADER_BYTES || &input[8..12] != b"WAVE" {
        return Err(WaveTimingError::NotWave);
    }
    let kind = match &input[..4] {
        b"RIFF" => ContainerKind::Riff,
        b"RF64" => ContainerKind::Rf64,
        b"BW64" => ContainerKind::Bw64,
        _ => return Err(WaveTimingError::NotWave),
    };
    let riff_size_32 = u32::from_le_bytes(input[4..8].try_into().unwrap());
    if kind == ContainerKind::Riff {
        if riff_size_32 < 4 {
            return Err(malformed(
                None,
                None,
                "RIFF size is smaller than WAVE form type",
            ));
        }
        let riff_size = usize::try_from(riff_size_32)
            .map_err(|_| malformed(None, None, "RIFF size does not fit usize"))?;
        let declared_end = 8_usize
            .checked_add(riff_size)
            .ok_or_else(|| malformed(None, None, "RIFF size overflows usize"))?;
        if declared_end > input.len() {
            return Err(malformed(
                None,
                None,
                "declared RIFF container extends beyond input",
            ));
        }
        let chunks = parse_chunk_spans(input, RIFF_HEADER_BYTES, declared_end, None)?;
        return Ok((
            ContainerInfo {
                kind,
                declared_end,
                riff_size: u64::from(riff_size_32),
                ds64_index: None,
                ds64: None,
            },
            chunks,
        ));
    }

    if riff_size_32 != u32::MAX {
        return Err(malformed(
            None,
            None,
            "RF64/BW64 RIFF size must be 0xffffffff",
        ));
    }
    // The 64-bit form size lives in ds64.  Find that chunk without assuming
    // that bytes after the declared form are themselves valid chunks; the
    // second scan then uses its declared form boundary so trailing bytes stay
    // opaque and byte-preserved.
    let (ds64_index_all, ds64_span) = find_ds64(input)?;
    let ds64 = parse_ds64(input, ds64_index_all, ds64_span)?;
    let riff_size = ds64.riff_size;
    let declared_end_u64 = 8_u64
        .checked_add(riff_size)
        .ok_or_else(|| malformed(None, None, "ds64 riffSize overflows u64"))?;
    let declared_end = usize::try_from(declared_end_u64)
        .map_err(|_| malformed(None, None, "ds64 riffSize does not fit usize"))?;
    if declared_end > input.len() || declared_end < RIFF_HEADER_BYTES {
        return Err(malformed(
            None,
            Some(*b"ds64"),
            "ds64 riffSize extends outside input",
        ));
    }
    let chunks = parse_chunk_spans(input, RIFF_HEADER_BYTES, declared_end, Some(&ds64))?;
    let ds64_indices = chunks
        .iter()
        .enumerate()
        .filter_map(|(index, span)| (span.id == *b"ds64").then_some(index))
        .collect::<Vec<_>>();
    if ds64_indices.len() != 1 {
        return Err(malformed(
            None,
            Some(*b"ds64"),
            "ds64 is not wholly inside the declared form",
        ));
    }
    Ok((
        ContainerInfo {
            kind,
            declared_end,
            riff_size,
            ds64_index: Some(ds64_indices[0]),
            ds64: Some(ds64),
        },
        chunks,
    ))
}

fn parse_ds64(
    input: &[u8],
    chunk_index: usize,
    span: ChunkSpan,
) -> Result<Ds64Info, WaveTimingError> {
    let body = input.get(span.body_start..span.body_end).ok_or_else(|| {
        malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "ds64 body is out of bounds",
        )
    })?;
    if body.len() < 28 {
        return Err(malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "ds64 body is shorter than the fixed header",
        ));
    }
    let riff_size = u64::from_le_bytes(body[..8].try_into().unwrap());
    let data_size = u64::from_le_bytes(body[8..16].try_into().unwrap());
    let sample_count = u64::from_le_bytes(body[16..24].try_into().unwrap());
    let table_length = usize::try_from(u32::from_le_bytes(body[24..28].try_into().unwrap()))
        .map_err(|_| {
            malformed(
                Some(chunk_index),
                Some(*b"ds64"),
                "ds64 tableLength does not fit usize",
            )
        })?;
    if table_length > MAX_DECLARED_TIMING_ITEMS {
        return Err(ledger_limit_exceeded(
            chunk_index,
            *b"ds64",
            table_length,
            "ds64 tableLength exceeds the bounded WAVE framing limit",
        ));
    }
    let table_bytes = table_length.checked_mul(12).ok_or_else(|| {
        malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "ds64 table length overflows body size",
        )
    })?;
    let expected = 28_usize.checked_add(table_bytes).ok_or_else(|| {
        malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "ds64 table size overflows body size",
        )
    })?;
    if expected != body.len() {
        return Err(malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            format!(
                "ds64 body has {} bytes, expected {expected} for tableLength {table_length}",
                body.len()
            ),
        ));
    }
    let mut table = Vec::with_capacity(table_length);
    for index in 0..table_length {
        let offset = 28 + index * 12;
        let id: ChunkId = body[offset..offset + 4].try_into().unwrap();
        if id == *b"data" {
            return Err(malformed(
                Some(chunk_index),
                Some(*b"ds64"),
                "ds64 table must not contain a data entry; use dataSize",
            ));
        }
        let size = u64::from_le_bytes(body[offset + 4..offset + 12].try_into().unwrap());
        table.push(Ds64TableEntry { id, size });
    }
    Ok(Ds64Info {
        riff_size,
        data_size,
        sample_count,
        table,
    })
}

fn find_ds64(input: &[u8]) -> Result<(usize, ChunkSpan), WaveTimingError> {
    let mut position = RIFF_HEADER_BYTES;
    let mut index = 0_usize;
    while position < input.len() {
        if index >= MAX_WAVE_CHUNKS {
            return Err(ledger_limit_exceeded(
                index,
                *b"WAVE",
                index.saturating_add(1),
                "RF64/BW64 chunk count exceeds the bounded WAVE framing limit before ds64",
            ));
        }
        if input.len() - position < CHUNK_HEADER_BYTES {
            return Err(malformed(
                None,
                None,
                "truncated RF64/BW64 chunk header before ds64",
            ));
        }
        let id: ChunkId = input[position..position + 4].try_into().unwrap();
        let raw_size = u32::from_le_bytes(input[position + 4..position + 8].try_into().unwrap());
        if raw_size == u32::MAX {
            return Err(malformed(
                Some(index),
                Some(id),
                "sentinel chunk size appears before ds64 and cannot be resolved",
            ));
        }
        let body_len = usize::try_from(raw_size)
            .map_err(|_| malformed(Some(index), Some(id), "chunk body size does not fit usize"))?;
        let body_start = position + CHUNK_HEADER_BYTES;
        let body_end = body_start
            .checked_add(body_len)
            .ok_or_else(|| malformed(Some(index), Some(id), "chunk body offset overflows usize"))?;
        let end = body_end.checked_add(body_len & 1).ok_or_else(|| {
            malformed(
                Some(index),
                Some(id),
                "chunk padding offset overflows usize",
            )
        })?;
        if end > input.len() {
            return Err(malformed(
                Some(index),
                Some(id),
                "chunk body or padding exceeds input before ds64",
            ));
        }
        let span = ChunkSpan {
            id,
            header_start: position,
            body_start,
            body_end,
            end,
            ds64_table_index: None,
        };
        if id == *b"ds64" {
            return Ok((index, span));
        }
        position = end;
        index = index
            .checked_add(1)
            .ok_or_else(|| malformed(None, None, "RF64/BW64 chunk index overflow"))?;
    }
    Err(malformed(
        None,
        Some(*b"ds64"),
        "RF64/BW64 requires a ds64 chunk",
    ))
}

fn parse_chunk_spans(
    input: &[u8],
    mut position: usize,
    end: usize,
    ds64: Option<&Ds64Info>,
) -> Result<Vec<ChunkSpan>, WaveTimingError> {
    let mut chunks = Vec::new();
    let mut resolver = ds64.map(Ds64Resolver::new);
    while position < end {
        if chunks.len() >= MAX_WAVE_CHUNKS {
            return Err(ledger_limit_exceeded(
                chunks.len(),
                *b"WAVE",
                chunks.len().saturating_add(1),
                "WAVE chunk count exceeds the bounded WAVE framing limit",
            ));
        }
        let remaining = end - position;
        if remaining < CHUNK_HEADER_BYTES {
            return Err(malformed(None, None, "truncated WAVE chunk header"));
        }
        let id: ChunkId = input[position..position + 4].try_into().unwrap();
        let raw_size = u32::from_le_bytes(input[position + 4..position + 8].try_into().unwrap());
        let chunk_index = chunks.len();
        let (body_len_u64, ds64_table_index) = match resolver.as_mut() {
            Some(resolver) => resolver.resolve(raw_size, id, chunk_index)?,
            None => (u64::from(raw_size), None),
        };
        let body_len = usize::try_from(body_len_u64).map_err(|_| {
            malformed(
                Some(chunk_index),
                Some(id),
                "chunk body size does not fit usize",
            )
        })?;
        let body_start = position + CHUNK_HEADER_BYTES;
        let body_end = body_start
            .checked_add(body_len)
            .ok_or_else(|| malformed(None, Some(id), "chunk body offset overflows usize"))?;
        let end_with_padding = body_end
            .checked_add(body_len & 1)
            .ok_or_else(|| malformed(None, Some(id), "chunk padding offset overflows usize"))?;
        if end_with_padding > end {
            return Err(malformed(
                None,
                Some(id),
                "chunk body or padding exceeds form",
            ));
        }
        chunks.push(ChunkSpan {
            id,
            header_start: position,
            body_start,
            body_end,
            end: end_with_padding,
            ds64_table_index,
        });
        position = end_with_padding;
    }
    if position != end {
        return Err(malformed(None, None, "unaccounted bytes inside WAVE form"));
    }
    if let Some(resolver) = resolver {
        resolver.finish()?;
        if let Some(ds64) = ds64 {
            let data_spans = chunks
                .iter()
                .filter(|span| span.id == *b"data")
                .collect::<Vec<_>>();
            if data_spans.len() > 1 {
                return Err(malformed(
                    None,
                    Some(*b"data"),
                    "RF64/BW64 ds64 dataSize cannot identify multiple data chunks",
                ));
            }
            if let Some(data) = data_spans.first() {
                let data_size = u64::try_from(data.body_end - data.body_start).map_err(|_| {
                    malformed(None, Some(*b"data"), "data chunk size conversion overflow")
                })?;
                if data_size != ds64.data_size {
                    return Err(malformed(
                        None,
                        Some(*b"data"),
                        format!(
                            "data chunk size {data_size} does not match ds64 dataSize {}",
                            ds64.data_size
                        ),
                    ));
                }
            } else if ds64.data_size != 0 {
                return Err(malformed(
                    None,
                    Some(*b"ds64"),
                    "ds64 dataSize is non-zero but no data chunk is present",
                ));
            }
        }
    }
    Ok(chunks)
}

fn rewrite_ds64(
    output: &mut Vec<u8>,
    body_start: usize,
    info: &Ds64Info,
    removed_table_indices: &[usize],
    riff_size: u64,
    chunk_index: usize,
) -> Result<(), WaveTimingError> {
    let removed = removed_table_indices
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    if removed.iter().any(|index| *index >= info.table.len()) {
        return Err(malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "removed ds64 table index is out of range",
        ));
    }
    let kept = info
        .table
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .collect::<Vec<_>>();
    let body_len = 28_usize
        .checked_add(kept.len().checked_mul(12).ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(*b"ds64"),
                "updated ds64 table overflows",
            )
        })?)
        .ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(*b"ds64"),
                "updated ds64 body overflows",
            )
        })?;
    let body_len_u32 = u32::try_from(body_len).map_err(|_| {
        malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "updated ds64 body does not fit chunk size field",
        )
    })?;
    let original_table_bytes = info.table.len().checked_mul(12).ok_or_else(|| {
        malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "original ds64 table size overflows",
        )
    })?;
    let body_end = body_start
        .checked_add(28)
        .and_then(|offset| offset.checked_add(original_table_bytes))
        .ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(*b"ds64"),
                "ds64 body offset overflows",
            )
        })?;
    let header_start = body_start.checked_sub(CHUNK_HEADER_BYTES).ok_or_else(|| {
        malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "ds64 header offset underflows",
        )
    })?;
    if body_end > output.len() || header_start + CHUNK_HEADER_BYTES > output.len() {
        return Err(malformed(
            Some(chunk_index),
            Some(*b"ds64"),
            "ds64 body is truncated while updating Strip metadata",
        ));
    }
    let mut body = vec![0_u8; body_len];
    body[..8].copy_from_slice(&riff_size.to_le_bytes());
    body[8..16].copy_from_slice(&info.data_size.to_le_bytes());
    body[16..24].copy_from_slice(&info.sample_count.to_le_bytes());
    body[24..28].copy_from_slice(&(kept.len() as u32).to_le_bytes());
    for (new_index, (_, entry)) in kept.iter().enumerate() {
        let offset = 28 + new_index * 12;
        body[offset..offset + 4].copy_from_slice(&entry.id);
        body[offset + 4..offset + 12].copy_from_slice(&entry.size.to_le_bytes());
    }
    output[header_start + 4..header_start + 8].copy_from_slice(&body_len_u32.to_le_bytes());
    output.splice(body_start..body_end, body);
    Ok(())
}

fn duplicate_transform_groups(
    input: &[u8],
    chunks: &[ChunkSpan],
) -> std::collections::HashSet<usize> {
    let mut groups = HashMap::<[u8; 4], Vec<usize>>::new();
    for (index, span) in chunks.iter().enumerate() {
        let key = if span.id == *b"bext" || span.id == *b"cue " || span.id == *b"smpl" {
            Some(span.id)
        } else if span.id == *b"LIST"
            && span.body_end - span.body_start >= 4
            && &input[span.body_start..span.body_start + 4] == b"adtl"
        {
            Some(*b"LIST")
        } else {
            None
        };
        if let Some(key) = key {
            groups.entry(key).or_default().push(index);
        }
    }
    groups
        .into_values()
        .filter(|indices| indices.len() > 1)
        .flatten()
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn transform_bext(
    source: &[u8],
    mapper: &SampleTimeTransform,
    crop_origin: i128,
    chunk_index: usize,
    policy: MetadataPolicy,
    duplicate: bool,
    _clock_changed: bool,
    ledger: &mut Vec<TimingLedgerEntry>,
) -> Result<Option<Vec<u8>>, WaveTimingError> {
    let id = *b"bext";
    if duplicate {
        return duplicate_result(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "duplicate bext chunks are ambiguous",
        );
    }
    if source.len() < 346 {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "bext is shorter than the TimeReference field",
        );
    }
    let value = u64::from_le_bytes(source[338..346].try_into().unwrap());
    let mut output = source.to_vec();
    let mapped =
        map_absolute_position_u64(mapper, crop_origin, value, chunk_index, id, "TimeReference")
            .and_then(|mapped| {
                let max = u64::from(mapper.target_rate())
                    .checked_mul(86_400)
                    .ok_or_else(|| {
                        overflow(
                            chunk_index,
                            id,
                            "TimeReference",
                            "24-hour clock bound overflow",
                        )
                    })?;
                if mapped >= max {
                    return Err(overflow(
                        chunk_index,
                        id,
                        "TimeReference",
                        "mapped value is outside the BWF 24-hour clock",
                    ));
                }
                Ok(mapped)
            });
    match mapped {
        Ok(mapped) => {
            output[338..346].copy_from_slice(&mapped.to_le_bytes());
            ledger.push(mapped_value_entry(
                chunk_index,
                id,
                "TimeReference",
                Some(i128::from(value)),
                Some(i128::from(mapped)),
                &source[338..346],
                &output[338..346],
            ));
        }
        Err(error) => {
            if policy_is_strict(policy) {
                return Err(error);
            }
            ledger.push(candidate_value_entry(
                chunk_index,
                id,
                "TimeReference",
                error.to_string(),
                Some(i128::from(value)),
                Some(&source[338..346]),
            ));
        }
    }
    Ok(Some(output))
}

#[allow(clippy::too_many_arguments)]
fn transform_cue(
    source: &[u8],
    mapper: &SampleTimeTransform,
    crop_origin: i128,
    chunk_index: usize,
    policy: MetadataPolicy,
    duplicate: bool,
    single_data_chunk: bool,
    clock_changed: bool,
    ledger: &mut Vec<TimingLedgerEntry>,
) -> Result<Option<Vec<u8>>, WaveTimingError> {
    let id = *b"cue ";
    if duplicate {
        return duplicate_result(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "duplicate cue chunks are ambiguous",
        );
    }
    if source.len() < 4 {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "cue body has no point count",
        );
    }
    let count = u32::from_le_bytes(source[..4].try_into().unwrap()) as usize;
    let expected = count
        .checked_mul(CUE_POINT_BYTES)
        .and_then(|value| value.checked_add(4))
        .ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(id),
                "cue point count overflows body size",
            )
        })?;
    if expected != source.len() {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "cue body is not an exact sequence of 24-byte points",
        );
    }
    let mut output = source.to_vec();
    for point in 0..count {
        let offset = 4 + point * CUE_POINT_BYTES;
        let field_prefix = format!("cue[{point}]");
        // dwPosition is a sample index only when the cue point identifies the
        // waveform data chunk.  Byte offsets dwChunkStart/dwBlockStart stay
        // untouched deliberately.
        let fcc_chunk = &source[offset + 8..offset + 12];
        if fcc_chunk != b"data" {
            let detail = format!(
                "fccChunkID {} is not data; sample-domain meaning is unproven",
                String::from_utf8_lossy(fcc_chunk)
            );
            if policy_is_strict(policy) && clock_changed {
                return Err(WaveTimingError::Ambiguous {
                    chunk_index,
                    chunk_id: id,
                    field: field_prefix,
                    detail,
                });
            }
            if clock_changed {
                ledger.push(candidate_entry(chunk_index, id, field_prefix, detail));
            }
            continue;
        }
        let position = u32::from_le_bytes(source[offset + 4..offset + 8].try_into().unwrap());
        let chunk_start = u32::from_le_bytes(source[offset + 12..offset + 16].try_into().unwrap());
        let block_start = u32::from_le_bytes(source[offset + 16..offset + 20].try_into().unwrap());
        let sample_offset =
            u32::from_le_bytes(source[offset + 20..offset + 24].try_into().unwrap());

        let position_field = format!("{field_prefix}.dwPosition");
        match map_position_u32(
            mapper,
            crop_origin,
            position,
            chunk_index,
            id,
            &position_field,
        ) {
            Ok(mapped) => {
                output[offset + 4..offset + 8].copy_from_slice(&mapped.to_le_bytes());
                ledger.push(mapped_value_entry(
                    chunk_index,
                    id,
                    position_field,
                    Some(i128::from(position)),
                    Some(i128::from(mapped)),
                    &source[offset + 4..offset + 8],
                    &output[offset + 4..offset + 8],
                ));
            }
            Err(error) => {
                if policy_is_strict(policy) {
                    return Err(error);
                }
                ledger.push(candidate_value_entry(
                    chunk_index,
                    id,
                    position_field,
                    error.to_string(),
                    Some(i128::from(position)),
                    Some(&source[offset + 4..offset + 8]),
                ));
            }
        }

        let sample_offset_field = format!("{field_prefix}.dwSampleOffset");
        // RIFFNEW defines dwSampleOffset as the sample position relative to
        // the data chunk when one PCM data block is proven by zero
        // dwChunkStart/dwBlockStart.  It is a distinct field from
        // dwPosition, so applying the crop to both fields is not a double
        // application.  Without that single-data proof, a strict
        // clock-changing transform fails closed.
        let sample_offset_proven = single_data_chunk && chunk_start == 0 && block_start == 0;
        if clock_changed && !sample_offset_proven {
            let detail = format!(
                "dwSampleOffset is block-local but dwChunkStart={chunk_start}, dwBlockStart={block_start}, single_data_chunk={single_data_chunk} do not prove a global block"
            );
            if policy_is_strict(policy) {
                return Err(WaveTimingError::Ambiguous {
                    chunk_index,
                    chunk_id: id,
                    field: sample_offset_field,
                    detail,
                });
            }
            ledger.push(candidate_value_entry(
                chunk_index,
                id,
                sample_offset_field,
                detail,
                Some(i128::from(sample_offset)),
                Some(&source[offset + 20..offset + 24]),
            ));
            continue;
        }

        match map_position_u32(
            mapper,
            crop_origin,
            sample_offset,
            chunk_index,
            id,
            &sample_offset_field,
        ) {
            Ok(mapped) => {
                output[offset + 20..offset + 24].copy_from_slice(&mapped.to_le_bytes());
                ledger.push(mapped_value_entry(
                    chunk_index,
                    id,
                    sample_offset_field,
                    Some(i128::from(sample_offset)),
                    Some(i128::from(mapped)),
                    &source[offset + 20..offset + 24],
                    &output[offset + 20..offset + 24],
                ));
            }
            Err(error) => {
                if policy_is_strict(policy) {
                    return Err(error);
                }
                ledger.push(candidate_value_entry(
                    chunk_index,
                    id,
                    sample_offset_field,
                    error.to_string(),
                    Some(i128::from(sample_offset)),
                    Some(&source[offset + 20..offset + 24]),
                ));
            }
        }
    }
    Ok(Some(output))
}

fn transform_list(
    source: &[u8],
    mapper: &SampleTimeTransform,
    chunk_index: usize,
    policy: MetadataPolicy,
    duplicate: bool,
    clock_changed: bool,
    ledger: &mut Vec<TimingLedgerEntry>,
) -> Result<Option<Vec<u8>>, WaveTimingError> {
    let id = *b"LIST";
    if source.len() < 4 {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "LIST body has no list type",
        );
    }
    if &source[..4] != b"adtl" {
        if clock_changed {
            let detail = format!(
                "LIST type {} has no registered sample-time adapter",
                String::from_utf8_lossy(&source[..4])
            );
            if policy_is_strict(policy) {
                return Err(WaveTimingError::Ambiguous {
                    chunk_index,
                    chunk_id: id,
                    field: "LIST/type".into(),
                    detail,
                });
            }
            ledger.push(candidate_entry(chunk_index, id, "LIST/type", detail));
        }
        return Ok(None);
    }
    if duplicate {
        return duplicate_result(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "duplicate LIST/adtl chunks are ambiguous",
        );
    }
    let subchunks = match parse_subchunks(source, chunk_index, id) {
        Ok(value) => value,
        Err(error) => {
            if policy_is_strict(policy) {
                return Err(error);
            }
            ledger.push(candidate_entry(
                chunk_index,
                id,
                "LIST/adtl",
                error.to_string(),
            ));
            return Ok(Some(source.to_vec()));
        }
    };
    let mut output = source.to_vec();
    for (sub_index, sub) in subchunks.iter().enumerate() {
        if sub.id != *b"ltxt" {
            continue;
        }
        let field = format!("ltxt[{sub_index}].dwSampleLength");
        if sub.body_end - sub.body_start < LTXT_MIN_BYTES {
            let error = malformed(
                Some(chunk_index),
                Some(*b"ltxt"),
                "ltxt body is shorter than its fixed fields",
            );
            if policy_is_strict(policy) {
                return Err(error);
            }
            ledger.push(candidate_entry(chunk_index, id, field, error.to_string()));
            continue;
        }
        let value = u32::from_le_bytes(
            source[sub.body_start + 4..sub.body_start + 8]
                .try_into()
                .unwrap(),
        );
        match map_duration_u32(mapper, value, chunk_index, id, &field) {
            Ok(mapped) => {
                output[sub.body_start + 4..sub.body_start + 8]
                    .copy_from_slice(&mapped.to_le_bytes());
                ledger.push(mapped_value_entry(
                    chunk_index,
                    id,
                    field,
                    Some(i128::from(value)),
                    Some(i128::from(mapped)),
                    &source[sub.body_start + 4..sub.body_start + 8],
                    &output[sub.body_start + 4..sub.body_start + 8],
                ));
            }
            Err(error) => {
                if policy_is_strict(policy) {
                    return Err(error);
                }
                ledger.push(candidate_value_entry(
                    chunk_index,
                    id,
                    field,
                    error.to_string(),
                    Some(i128::from(value)),
                    Some(&source[sub.body_start + 4..sub.body_start + 8]),
                ));
            }
        }
    }
    Ok(Some(output))
}

#[allow(clippy::too_many_arguments)]
fn transform_smpl(
    source: &[u8],
    position_mapper: &SampleTimeTransform,
    period_mapper: &SampleTimeTransform,
    crop_origin: i128,
    chunk_index: usize,
    policy: MetadataPolicy,
    duplicate: bool,
    ledger: &mut Vec<TimingLedgerEntry>,
) -> Result<Option<Vec<u8>>, WaveTimingError> {
    let id = *b"smpl";
    if duplicate {
        return duplicate_result(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "duplicate smpl chunks are ambiguous",
        );
    }
    if source.len() < SMPL_HEADER_BYTES {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "smpl body is shorter than its fixed header",
        );
    }
    let sample_period = u32::from_le_bytes(source[8..12].try_into().unwrap());
    let loop_count = u32::from_le_bytes(source[28..32].try_into().unwrap()) as usize;
    let loop_bytes = loop_count
        .checked_mul(SMPL_LOOP_BYTES)
        .and_then(|value| value.checked_add(SMPL_HEADER_BYTES))
        .ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(id),
                "smpl loop count overflows body size",
            )
        })?;
    if loop_bytes > source.len() {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "smpl loop table exceeds body",
        );
    }
    let declared_sampler_data = u32::from_le_bytes(source[32..36].try_into().unwrap()) as usize;
    let actual_sampler_data = source.len() - loop_bytes;
    if declared_sampler_data != actual_sampler_data {
        return malformed_or_preserve(
            source,
            chunk_index,
            id,
            policy,
            ledger,
            "smpl samplerData length does not match the bytes after the loop table",
        );
    }
    if sample_period == 0 {
        let error = malformed(
            Some(chunk_index),
            Some(id),
            "smpl samplePeriod must be non-zero",
        );
        if policy_is_strict(policy) {
            return Err(error);
        }
        ledger.push(candidate_value_entry(
            chunk_index,
            id,
            "samplePeriod",
            error.to_string(),
            Some(0),
            Some(&source[8..12]),
        ));
    }
    let mut output = source.to_vec();
    if sample_period != 0 {
        match map_period_ns_u32(
            period_mapper,
            sample_period,
            chunk_index,
            id,
            "samplePeriod",
        ) {
            Ok(mapped) => {
                output[8..12].copy_from_slice(&mapped.to_le_bytes());
                ledger.push(mapped_value_entry(
                    chunk_index,
                    id,
                    "samplePeriod",
                    Some(i128::from(sample_period)),
                    Some(i128::from(mapped)),
                    &source[8..12],
                    &output[8..12],
                ));
            }
            Err(error) => {
                if policy_is_strict(policy) {
                    return Err(error);
                }
                ledger.push(candidate_value_entry(
                    chunk_index,
                    id,
                    "samplePeriod",
                    error.to_string(),
                    Some(i128::from(sample_period)),
                    Some(&source[8..12]),
                ));
            }
        }
    }
    for loop_index in 0..loop_count {
        let offset = SMPL_HEADER_BYTES + loop_index * SMPL_LOOP_BYTES;
        let start = u32::from_le_bytes(source[offset + 8..offset + 12].try_into().unwrap());
        let end = u32::from_le_bytes(source[offset + 12..offset + 16].try_into().unwrap());
        if end < start {
            let error = malformed(
                Some(chunk_index),
                Some(id),
                format!("smpl loop[{loop_index}] end precedes start"),
            );
            if policy_is_strict(policy) {
                return Err(error);
            }
            ledger.push(candidate_entry(
                chunk_index,
                id,
                format!("loop[{loop_index}]"),
                error.to_string(),
            ));
            continue;
        }
        let start_field = format!("loop[{loop_index}].dwStart");
        let end_field = format!("loop[{loop_index}].dwEnd");
        let mapped_start = map_position_u32(
            position_mapper,
            crop_origin,
            start,
            chunk_index,
            id,
            &start_field,
        );
        let mapped_end = map_position_u32(
            position_mapper,
            crop_origin,
            end,
            chunk_index,
            id,
            &end_field,
        );
        match (mapped_start, mapped_end) {
            (Ok(mapped_start), Ok(mapped_end)) if mapped_end >= mapped_start => {
                output[offset + 8..offset + 12].copy_from_slice(&mapped_start.to_le_bytes());
                output[offset + 12..offset + 16].copy_from_slice(&mapped_end.to_le_bytes());
                ledger.push(mapped_value_entry(
                    chunk_index,
                    id,
                    start_field,
                    Some(i128::from(start)),
                    Some(i128::from(mapped_start)),
                    &source[offset + 8..offset + 12],
                    &output[offset + 8..offset + 12],
                ));
                ledger.push(mapped_value_entry(
                    chunk_index,
                    id,
                    end_field,
                    Some(i128::from(end)),
                    Some(i128::from(mapped_end)),
                    &source[offset + 12..offset + 16],
                    &output[offset + 12..offset + 16],
                ));
            }
            (mapped_start, mapped_end) => {
                let error = match (&mapped_start, &mapped_end) {
                    (Err(error), _) => error.clone(),
                    (_, Err(error)) => error.clone(),
                    (Ok(_), Ok(_)) => overflow(
                        chunk_index,
                        id,
                        &format!("loop[{loop_index}]"),
                        "mapped end precedes mapped start",
                    ),
                };
                if policy_is_strict(policy) {
                    return Err(error);
                }
                let detail = format!(
                    "loop endpoints were retained as one pair because their mapping failed: {error}"
                );
                ledger.push(candidate_value_entry(
                    chunk_index,
                    id,
                    start_field,
                    detail.clone(),
                    Some(i128::from(start)),
                    Some(&source[offset + 8..offset + 12]),
                ));
                ledger.push(candidate_value_entry(
                    chunk_index,
                    id,
                    end_field,
                    detail,
                    Some(i128::from(end)),
                    Some(&source[offset + 12..offset + 16]),
                ));
            }
        }
    }
    Ok(Some(output))
}

#[derive(Clone, Copy, Debug)]
struct SubchunkSpan {
    id: ChunkId,
    body_start: usize,
    body_end: usize,
}

fn parse_subchunks(
    source: &[u8],
    chunk_index: usize,
    chunk_id: ChunkId,
) -> Result<Vec<SubchunkSpan>, WaveTimingError> {
    let mut position = 4_usize;
    let mut output = Vec::new();
    while position < source.len() {
        if source.len() - position < CHUNK_HEADER_BYTES {
            return Err(malformed(
                Some(chunk_index),
                Some(chunk_id),
                "LIST subchunk header is truncated",
            ));
        }
        let id: ChunkId = source[position..position + 4].try_into().unwrap();
        let size =
            u32::from_le_bytes(source[position + 4..position + 8].try_into().unwrap()) as usize;
        let body_start = position + CHUNK_HEADER_BYTES;
        let body_end = body_start.checked_add(size).ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(chunk_id),
                "LIST subchunk size overflows usize",
            )
        })?;
        let next = body_end.checked_add(size & 1).ok_or_else(|| {
            malformed(
                Some(chunk_index),
                Some(chunk_id),
                "LIST subchunk padding overflows usize",
            )
        })?;
        if next > source.len() {
            return Err(malformed(
                Some(chunk_index),
                Some(chunk_id),
                "LIST subchunk body or padding exceeds LIST body",
            ));
        }
        output.push(SubchunkSpan {
            id,
            body_start,
            body_end,
        });
        position = next;
    }
    Ok(output)
}

fn malformed(
    chunk_index: Option<usize>,
    chunk_id: Option<ChunkId>,
    detail: impl Into<String>,
) -> WaveTimingError {
    WaveTimingError::Malformed {
        chunk_index,
        chunk_id,
        detail: detail.into(),
    }
}

fn malformed_or_preserve(
    source: &[u8],
    chunk_index: usize,
    chunk_id: ChunkId,
    policy: MetadataPolicy,
    ledger: &mut Vec<TimingLedgerEntry>,
    detail: &str,
) -> Result<Option<Vec<u8>>, WaveTimingError> {
    let error = malformed(Some(chunk_index), Some(chunk_id), detail);
    if policy_is_strict(policy) {
        Err(error)
    } else {
        ledger.push(candidate_entry(chunk_index, chunk_id, "chunk", detail));
        Ok(Some(source.to_vec()))
    }
}

fn duplicate_result(
    source: &[u8],
    chunk_index: usize,
    chunk_id: ChunkId,
    policy: MetadataPolicy,
    ledger: &mut Vec<TimingLedgerEntry>,
    detail: &str,
) -> Result<Option<Vec<u8>>, WaveTimingError> {
    if policy_is_strict(policy) {
        return Err(WaveTimingError::Ambiguous {
            chunk_index,
            chunk_id,
            field: "chunk".into(),
            detail: detail.into(),
        });
    }
    ledger.push(candidate_entry(chunk_index, chunk_id, "chunk", detail));
    Ok(Some(source.to_vec()))
}

fn mapped_value_entry(
    chunk_index: usize,
    chunk_id: ChunkId,
    field: impl Into<String>,
    before_value: Option<i128>,
    after_value: Option<i128>,
    source_bytes: &[u8],
    destination_bytes: &[u8],
) -> TimingLedgerEntry {
    TimingLedgerEntry {
        chunk_index,
        chunk_id,
        field: field.into(),
        detail: "mapped with checked exact rational arithmetic (rounding=half-up)".into(),
        action: TimingLedgerAction::Mapped,
        candidate: false,
        before_value,
        after_value,
        source_field_sha256: Some(sha256_hex(source_bytes)),
        destination_field_sha256: Some(sha256_hex(destination_bytes)),
        rounding: Some(RoundingMode::HalfUp),
    }
}

fn candidate_entry(
    chunk_index: usize,
    chunk_id: ChunkId,
    field: impl Into<String>,
    detail: impl Into<String>,
) -> TimingLedgerEntry {
    candidate_value_entry(chunk_index, chunk_id, field, detail, None, None)
}

fn candidate_value_entry(
    chunk_index: usize,
    chunk_id: ChunkId,
    field: impl Into<String>,
    detail: impl Into<String>,
    before_value: Option<i128>,
    source_bytes: Option<&[u8]>,
) -> TimingLedgerEntry {
    TimingLedgerEntry {
        chunk_index,
        chunk_id,
        field: field.into(),
        detail: detail.into(),
        action: TimingLedgerAction::Preserved,
        candidate: true,
        before_value,
        after_value: None,
        source_field_sha256: source_bytes.map(sha256_hex),
        destination_field_sha256: None,
        rounding: None,
    }
}

fn is_structural_chunk(id: ChunkId) -> bool {
    id == *b"fmt " || id == *b"fact" || id == *b"data" || id == *b"ds64"
}

/// Chunks whose RIFF contract is explicitly padding or channel-association
/// metadata, rather than a sample-coordinate field.  Other unregistered IDs
/// remain timing candidates whenever the sample clock changes.
fn is_known_non_timing_chunk(id: ChunkId) -> bool {
    id == *b"JUNK" || id == *b"PAD " || id == *b"chna"
}

/// Strip mode is strict for chunks it retains: only the explicitly selected
/// metadata IDs are dropped, while malformed or ambiguous unselected timing
/// fields still fail closed.  Keeping this policy test in one place avoids a
/// subtle divergence between the field-specific transforms.
fn policy_is_strict(policy: MetadataPolicy) -> bool {
    matches!(policy, MetadataPolicy::Strict | MetadataPolicy::Strip)
}

fn id_text(id: ChunkId) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: ChunkId, body: &[u8], pad: Option<u8>) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&id);
        output.extend_from_slice(&(body.len() as u32).to_le_bytes());
        output.extend_from_slice(body);
        if body.len() & 1 != 0 {
            output.push(pad.unwrap_or(0));
        }
        output
    }

    fn riff(chunks: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
        let payload = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        output.extend_from_slice(b"RIFF");
        output.extend_from_slice(&(u32::try_from(4 + payload.len()).unwrap()).to_le_bytes());
        output.extend_from_slice(b"WAVE");
        output.extend_from_slice(&payload);
        output.extend_from_slice(trailing);
        output
    }

    fn rf64(chunks: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
        let payload = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        output.extend_from_slice(b"RF64");
        output.extend_from_slice(&u32::MAX.to_le_bytes());
        output.extend_from_slice(b"WAVE");
        output.extend_from_slice(&payload);
        output.extend_from_slice(trailing);
        output
    }

    fn ds64(riff_size: u64) -> Vec<u8> {
        let mut body = vec![0_u8; 28];
        body[..8].copy_from_slice(&riff_size.to_le_bytes());
        body
    }

    fn ds64_with_table(riff_size: u64, data_size: u64, table: &[(ChunkId, u64)]) -> Vec<u8> {
        let mut body = vec![0_u8; 28 + table.len() * 12];
        body[..8].copy_from_slice(&riff_size.to_le_bytes());
        body[8..16].copy_from_slice(&data_size.to_le_bytes());
        body[24..28].copy_from_slice(&(table.len() as u32).to_le_bytes());
        for (index, (id, size)) in table.iter().enumerate() {
            let offset = 28 + index * 12;
            body[offset..offset + 4].copy_from_slice(id);
            body[offset + 4..offset + 12].copy_from_slice(&size.to_le_bytes());
        }
        body
    }

    fn sentinel_chunk(id: ChunkId, body: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&id);
        output.extend_from_slice(&u32::MAX.to_le_bytes());
        output.extend_from_slice(body);
        if body.len() & 1 != 0 {
            output.push(0);
        }
        output
    }

    fn bext(time_reference: u64) -> Vec<u8> {
        let mut body = vec![0_u8; 602];
        body[338..346].copy_from_slice(&time_reference.to_le_bytes());
        body
    }

    fn cue(points: &[(ChunkId, u32, u32, u8)]) -> Vec<u8> {
        let mut body = Vec::with_capacity(4 + points.len() * CUE_POINT_BYTES);
        body.extend_from_slice(&(points.len() as u32).to_le_bytes());
        for (fcc, position, sample_offset, fill) in points {
            let mut point = [0_u8; CUE_POINT_BYTES];
            point[0..4].copy_from_slice(&1_u32.to_le_bytes());
            point[4..8].copy_from_slice(&position.to_le_bytes());
            point[8..12].copy_from_slice(fcc);
            point[12..24].fill(*fill);
            // A zero chunk/block start proves that dwSampleOffset is local to
            // the single synthetic data block used by the timing tests.
            point[12..20].fill(0);
            point[20..24].copy_from_slice(&sample_offset.to_le_bytes());
            body.extend_from_slice(&point);
        }
        body
    }

    fn ltxt(sample_length: u32, extra: &[u8]) -> Vec<u8> {
        let mut body = vec![0_u8; LTXT_MIN_BYTES];
        body[4..8].copy_from_slice(&sample_length.to_le_bytes());
        body.extend_from_slice(extra);
        body
    }

    fn adtl(subchunks: &[Vec<u8>]) -> Vec<u8> {
        let mut body = b"adtl".to_vec();
        for subchunk in subchunks {
            body.extend_from_slice(subchunk);
        }
        body
    }

    fn smpl(period: u32, loops: &[(u32, u32)], sampler_data: &[u8]) -> Vec<u8> {
        let mut body = vec![0_u8; SMPL_HEADER_BYTES];
        body[8..12].copy_from_slice(&period.to_le_bytes());
        body[28..32].copy_from_slice(&(loops.len() as u32).to_le_bytes());
        body[32..36].copy_from_slice(&(sampler_data.len() as u32).to_le_bytes());
        for (start, end) in loops {
            let offset = body.len();
            body.resize(offset + SMPL_LOOP_BYTES, 0);
            body[offset + 8..offset + 12].copy_from_slice(&start.to_le_bytes());
            body[offset + 12..offset + 16].copy_from_slice(&end.to_le_bytes());
        }
        body.extend_from_slice(sampler_data);
        body
    }

    fn parse_top_chunks(bytes: &[u8]) -> Vec<(ChunkId, Vec<u8>, Vec<u8>)> {
        let (_, chunks) = parse_wave(bytes).unwrap();
        chunks
            .iter()
            .map(|span| {
                (
                    span.id,
                    bytes[span.body_start..span.body_end].to_vec(),
                    bytes[span.body_end..span.end].to_vec(),
                )
            })
            .collect()
    }

    #[test]
    fn maps_bext_cue_ltxt_and_smpl_with_exact_ratio() {
        let source = riff(
            &[
                chunk(*b"JUNK", b"unknown", Some(0xa5)),
                chunk(*b"bext", &bext(48_000), None),
                chunk(*b"cue ", &cue(&[(*b"data", 48_000, 12_000, 0x33)]), None),
                chunk(
                    *b"LIST",
                    &adtl(&[
                        chunk(*b"labl", b"label", Some(0x91)),
                        chunk(*b"ltxt", &ltxt(24_000, b"extension"), None),
                    ]),
                    None,
                ),
                chunk(
                    *b"smpl",
                    &smpl(20_833, &[(24_000, 48_000)], b"opaque"),
                    None,
                ),
                chunk(*b"data", b"pcm", Some(0x7e)),
                chunk(*b"JUNK", b"after-data", Some(0x68)),
            ],
            b"outside-form",
        );
        let options = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        let chunks = parse_top_chunks(&result.bytes);
        assert_eq!(chunks[0].1, b"unknown");
        assert_eq!(chunks[0].2, [0xa5]);
        assert_eq!(
            u64::from_le_bytes(chunks[1].1[338..346].try_into().unwrap()),
            44_100
        );
        let cue_body = &chunks[2].1;
        assert_eq!(
            u32::from_le_bytes(cue_body[8..12].try_into().unwrap()),
            44_100
        );
        assert_eq!(
            u32::from_le_bytes(cue_body[24..28].try_into().unwrap()),
            11_025
        );
        let list_body = &chunks[3].1;
        let ltxt_body = 4 + 8 + 5 + 1 + 8; // adtl + labl header/body/pad + ltxt header
        assert_eq!(
            u32::from_le_bytes(list_body[ltxt_body + 4..ltxt_body + 8].try_into().unwrap()),
            22_050
        );
        let smpl_body = &chunks[4].1;
        assert_eq!(
            u32::from_le_bytes(smpl_body[8..12].try_into().unwrap()),
            22_675
        );
        assert_eq!(
            u32::from_le_bytes(smpl_body[44..48].try_into().unwrap()),
            22_050
        );
        assert_eq!(
            u32::from_le_bytes(smpl_body[48..52].try_into().unwrap()),
            44_100
        );
        assert_eq!(&chunks[5].1, b"pcm");
        assert_eq!(chunks[5].2, [0x7e]);
        assert_eq!(&result.bytes[result.bytes.len() - 12..], b"outside-form");
        assert!(result.changed);
        assert!(result.ledger.iter().all(|entry| !entry.candidate));
    }

    #[test]
    fn no_op_preserves_every_byte_and_unknown_order() {
        let source = riff(
            &[
                chunk(*b"JUNK", b"x", Some(0x99)),
                chunk(*b"data", b"pcm", Some(0x77)),
                chunk(*b"cue ", &cue(&[(*b"data", 7, 3, 0)]), None),
            ],
            b"tail",
        );
        let options = WaveTimingTransformOptions::strict(48_000, 48_000, 0).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        assert_eq!(result.bytes, source);
        assert!(!result.changed);
    }

    #[test]
    fn crop_origin_maps_positions_signed_and_preserves_unrepresentable_values() {
        let source = riff(
            &[
                chunk(*b"bext", &bext(100), None),
                chunk(*b"cue ", &cue(&[(*b"data", 50, 75, 0)]), None),
                chunk(*b"data", b"", None),
            ],
            &[],
        );
        let options = WaveTimingTransformOptions::strict(48_000, 48_000, 50).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        let chunks = parse_top_chunks(&result.bytes);
        assert_eq!(
            u64::from_le_bytes(chunks[0].1[338..346].try_into().unwrap()),
            150
        );
        assert_eq!(
            u32::from_le_bytes(chunks[1].1[8..12].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_le_bytes(chunks[1].1[24..28].try_into().unwrap()),
            25
        );

        let negative = riff(
            &[chunk(*b"cue ", &cue(&[(*b"data", 10, 10, 0)]), None)],
            &[],
        );
        let preserve = WaveTimingTransformOptions::preserve(48_000, 48_000, 20).unwrap();
        let preserved = transform_wave(&negative, &preserve).unwrap();
        assert_eq!(preserved.bytes, negative);
        assert!(preserved.ledger.iter().any(|entry| entry.candidate));
        let strict = WaveTimingTransformOptions::strict(48_000, 48_000, 20).unwrap();
        assert!(matches!(
            transform_wave(&negative, &strict),
            Err(WaveTimingError::Overflow { .. })
        ));
    }

    #[test]
    fn cue_non_data_semantics_are_ambiguous_but_preserve_is_auditable() {
        let source = riff(
            &[chunk(*b"cue ", &cue(&[(*b"JUNK", 10, 20, 0)]), None)],
            &[],
        );
        let preserve = WaveTimingTransformOptions::preserve(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&source, &preserve).unwrap();
        assert_eq!(result.bytes, source);
        assert_eq!(result.ledger.len(), 1);
        assert!(result.ledger[0].candidate);
        let strict = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        assert!(matches!(
            transform_wave(&source, &strict),
            Err(WaveTimingError::Ambiguous { .. })
        ));
    }

    #[test]
    fn unknown_chunks_are_timing_candidates_when_the_clock_changes() {
        let source = riff(&[chunk(*b"zzzz", b"opaque", None)], &[]);
        let preserve = WaveTimingTransformOptions::preserve(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&source, &preserve).unwrap();
        assert_eq!(result.bytes, source);
        assert_eq!(result.ledger.len(), 1);
        assert!(result.ledger[0].candidate);
        let strict = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        assert!(matches!(
            transform_wave(&source, &strict),
            Err(WaveTimingError::Ambiguous { .. })
        ));
    }

    #[test]
    fn declared_timing_counts_cannot_exhaust_the_ledger_budget() {
        let mut cue_body = vec![0_u8; 4];
        cue_body[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let source = riff(&[chunk(*b"cue ", &cue_body, None)], &[]);
        for policy in [MetadataPolicy::Preserve, MetadataPolicy::Strict] {
            let options =
                WaveTimingTransformOptions::new(48_000, 44_100, 0, policy, Vec::new()).unwrap();
            assert!(matches!(
                transform_wave(&source, &options),
                Err(WaveTimingError::LedgerLimitExceeded { .. })
            ));
        }
    }

    #[test]
    fn rf64_ds64_table_length_is_bounded_before_allocation() {
        let mut body = ds64_with_table(0, 0, &[]);
        body[24..28].copy_from_slice(
            &(u32::try_from(MAX_DECLARED_TIMING_ITEMS + 1).unwrap()).to_le_bytes(),
        );
        let source = rf64(&[chunk(*b"ds64", &body, None)], &[]);
        assert!(matches!(
            parse_wave(&source),
            Err(WaveTimingError::LedgerLimitExceeded {
                chunk_id: id,
                ..
            }) if id == *b"ds64"
        ));
    }

    #[test]
    fn ds64_resolver_consumes_repeated_ids_in_linear_table_order() {
        let table = (0..MAX_DECLARED_TIMING_ITEMS)
            .map(|index| Ds64TableEntry {
                id: *b"JUNK",
                size: index as u64,
            })
            .collect::<Vec<_>>();
        let info = Ds64Info {
            riff_size: 0,
            data_size: 0,
            sample_count: 0,
            table,
        };
        let mut resolver = Ds64Resolver::new(&info);
        for index in 0..MAX_DECLARED_TIMING_ITEMS {
            assert_eq!(
                resolver.resolve(u32::MAX, *b"JUNK", index).unwrap(),
                (index as u64, Some(index))
            );
        }
        resolver.finish().unwrap();
    }

    #[test]
    fn wave_chunk_count_is_bounded_for_riff_and_rf64_ds64_search() {
        let chunk_count = MAX_WAVE_CHUNKS + 1;
        let riff_size = u32::try_from(4 + chunk_count * CHUNK_HEADER_BYTES).unwrap();
        let mut riff_source =
            Vec::with_capacity(RIFF_HEADER_BYTES + chunk_count * CHUNK_HEADER_BYTES);
        riff_source.extend_from_slice(b"RIFF");
        riff_source.extend_from_slice(&riff_size.to_le_bytes());
        riff_source.extend_from_slice(b"WAVE");
        for _ in 0..chunk_count {
            riff_source.extend_from_slice(b"JUNK");
            riff_source.extend_from_slice(&0_u32.to_le_bytes());
        }
        assert!(matches!(
            parse_wave(&riff_source),
            Err(WaveTimingError::LedgerLimitExceeded { .. })
        ));

        let mut rf64_source =
            Vec::with_capacity(RIFF_HEADER_BYTES + (chunk_count + 1) * CHUNK_HEADER_BYTES + 28);
        rf64_source.extend_from_slice(b"RF64");
        rf64_source.extend_from_slice(&u32::MAX.to_le_bytes());
        rf64_source.extend_from_slice(b"WAVE");
        for _ in 0..chunk_count {
            rf64_source.extend_from_slice(b"JUNK");
            rf64_source.extend_from_slice(&0_u32.to_le_bytes());
        }
        rf64_source.extend_from_slice(&chunk(*b"ds64", &ds64(0), None));
        assert!(matches!(
            parse_wave(&rf64_source),
            Err(WaveTimingError::LedgerLimitExceeded { .. })
        ));
    }

    #[test]
    fn duplicate_and_malformed_metadata_follow_policy() {
        let duplicate = riff(
            &[
                chunk(*b"bext", &bext(1), None),
                chunk(*b"bext", &bext(2), None),
            ],
            &[],
        );
        let preserve = WaveTimingTransformOptions::preserve(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&duplicate, &preserve).unwrap();
        assert_eq!(result.bytes, duplicate);
        assert_eq!(result.ledger.len(), 2);
        let strict = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        assert!(matches!(
            transform_wave(&duplicate, &strict),
            Err(WaveTimingError::Ambiguous { .. })
        ));

        let malformed = riff(&[chunk(*b"smpl", &[0; 35], None)], &[]);
        let result = transform_wave(&malformed, &preserve).unwrap();
        assert_eq!(result.bytes, malformed);
        assert!(result.ledger[0].candidate);
        assert!(matches!(
            transform_wave(&malformed, &strict),
            Err(WaveTimingError::Malformed { .. })
        ));
    }

    #[test]
    fn strips_only_requested_metadata_and_updates_riff_size() {
        let source = riff(
            &[
                chunk(*b"JUNK", b"keep", Some(0xa1)),
                chunk(*b"cue ", &cue(&[(*b"data", 10, 10, 0)]), None),
                chunk(*b"data", b"pcm", Some(0xc3)),
            ],
            b"opaque-tail",
        );
        let options = WaveTimingTransformOptions::strip(48_000, 44_100, 0, vec![*b"cue "]).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        let chunks = parse_top_chunks(&result.bytes);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, *b"JUNK");
        assert_eq!(chunks[1].0, *b"data");
        assert!(chunks[0].2.is_empty());
        assert_eq!(chunks[1].2, [0xc3]);
        let (_, source_spans) = parse_wave(&source).unwrap();
        let cue_bytes = u32::try_from(source_spans[1].end - source_spans[1].header_start).unwrap();
        assert_eq!(
            u32::from_le_bytes(result.bytes[4..8].try_into().unwrap()),
            u32::from_le_bytes(source[4..8].try_into().unwrap()) - cue_bytes
        );
        assert_eq!(&result.bytes[result.bytes.len() - 11..], b"opaque-tail");
        assert!(result
            .ledger
            .iter()
            .any(|entry| entry.action == TimingLedgerAction::Dropped));
    }

    #[test]
    fn fact_is_structural_and_cannot_be_stripped() {
        assert!(matches!(
            WaveTimingTransformOptions::strip(48_000, 44_100, 0, vec![*b"fact"]),
            Err(WaveTimingError::InvalidStripChunk { id }) if id == *b"fact"
        ));
    }

    #[test]
    fn rf64_strip_updates_ds64_after_an_earlier_removed_chunk() {
        let placeholder = chunk(*b"ds64", &ds64(0), None);
        let other = chunk(*b"JUNK", b"remove", Some(0x91));
        let cue_chunk = chunk(*b"cue ", &cue(&[(*b"data", 10, 10, 0)]), None);
        let initial = rf64(
            &[
                other.clone(),
                placeholder,
                cue_chunk,
                chunk(*b"data", b"", None),
            ],
            b"opaque-tail",
        );
        let initial_size = (initial.len() - 8 - b"opaque-tail".len()) as u64;
        let mut source = initial;
        // The ds64 body is after the first JUNK chunk in this deliberately
        // unusual but parseable ordering.
        let ds64_body = 12 + other.len() + 8;
        source[ds64_body..ds64_body + 8].copy_from_slice(&initial_size.to_le_bytes());

        let options = WaveTimingTransformOptions::strip(48_000, 44_100, 0, vec![*b"JUNK"]).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        let (_, spans) = parse_wave(&result.bytes).unwrap();
        assert_eq!(spans[0].id, *b"ds64");
        let new_size = u64::from_le_bytes(
            result.bytes[spans[0].body_start..spans[0].body_start + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(new_size, initial_size - other.len() as u64);
        assert_eq!(&result.bytes[result.bytes.len() - 11..], b"opaque-tail");
    }

    #[test]
    fn rf64_resolves_sentinel_sizes_and_removes_the_matching_ds64_entry() {
        let sentinel_body = b"drop";
        let data_body = b"pcm";
        let ds64_placeholder = chunk(
            *b"ds64",
            &ds64_with_table(
                0,
                data_body.len() as u64,
                &[(*b"JUNK", sentinel_body.len() as u64)],
            ),
            None,
        );
        let sentinel = sentinel_chunk(*b"JUNK", sentinel_body);
        let data = sentinel_chunk(*b"data", data_body);
        let initial = rf64(&[ds64_placeholder, sentinel.clone(), data], &[]);
        let riff_size = (initial.len() - 8) as u64;
        let mut source = initial;
        source[20..28].copy_from_slice(&riff_size.to_le_bytes());

        let parsed = parse_top_chunks(&source);
        assert_eq!(parsed[0].0, *b"ds64");
        assert_eq!(parsed[1].0, *b"JUNK");
        assert_eq!(parsed[1].1, sentinel_body);
        assert_eq!(parsed[2].0, *b"data");
        assert_eq!(parsed[2].1, data_body);

        let options = WaveTimingTransformOptions::strip(48_000, 48_000, 0, vec![*b"JUNK"]).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        let chunks = parse_top_chunks(&result.bytes);
        assert_eq!(
            chunks.iter().map(|chunk| chunk.0).collect::<Vec<_>>(),
            vec![*b"ds64", *b"data"]
        );
        assert_eq!(chunks[1].1, data_body);
        let ds64_body = &chunks[0].1;
        assert_eq!(u32::from_le_bytes(ds64_body[24..28].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(ds64_body[..8].try_into().unwrap()),
            riff_size - sentinel.len() as u64 - 12
        );
        assert_eq!(
            u64::from_le_bytes(ds64_body[8..16].try_into().unwrap()),
            data_body.len() as u64
        );
    }

    #[test]
    fn malformed_nested_padding_is_preserved_or_rejected() {
        let mut body = b"adtl".to_vec();
        body.extend_from_slice(b"ltxt");
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(0x55);
        // Missing the required odd pad byte and fixed ltxt fields.
        let source = riff(&[chunk(*b"LIST", &body, None)], &[]);
        let preserve = WaveTimingTransformOptions::preserve(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&source, &preserve).unwrap();
        assert_eq!(result.bytes, source);
        assert!(result.ledger[0].candidate);
        let strict = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        assert!(matches!(
            transform_wave(&source, &strict),
            Err(WaveTimingError::Malformed { .. })
        ));
    }

    #[test]
    fn xml_chunks_are_explicitly_opaque() {
        let source = riff(&[chunk(*b"sxml", b"opaque S-ADM", None)], &[]);
        let options = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        assert!(matches!(
            transform_wave(&source, &options),
            Err(WaveTimingError::Ambiguous { .. })
        ));
        let preserve = WaveTimingTransformOptions::preserve(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&source, &preserve).unwrap();
        assert_eq!(result.bytes, source);
        assert_eq!(result.unsupported_xml_chunks, vec![*b"sxml"]);
    }

    #[test]
    fn checked_mapping_rejects_zero_rates_and_destination_overflow() {
        assert!(matches!(
            WaveTimingTransformOptions::strict(0, 48_000, 0),
            Err(WaveTimingError::InvalidRate { .. })
        ));
        let source = riff(
            &[chunk(*b"cue ", &cue(&[(*b"data", u32::MAX, 0, 0)]), None)],
            &[],
        );
        let options = WaveTimingTransformOptions::strict(1, u32::MAX, 0).unwrap();
        assert!(matches!(
            transform_wave(&source, &options),
            Err(WaveTimingError::Overflow { .. })
        ));
    }

    #[test]
    fn positive_sample_period_is_not_rewritten_to_zero() {
        let source = riff(&[chunk(*b"smpl", &smpl(1, &[], &[]), None)], &[]);
        let preserve =
            WaveTimingTransformOptions::preserve(1_000_000_000, 4_000_000_000, 0).unwrap();
        let result = transform_wave(&source, &preserve).unwrap();
        assert_eq!(result.bytes, source);
        assert!(result
            .ledger
            .iter()
            .any(|entry| entry.field == "samplePeriod" && entry.candidate));

        let strict = WaveTimingTransformOptions::strict(1_000_000_000, 4_000_000_000, 0).unwrap();
        assert!(matches!(
            transform_wave(&source, &strict),
            Err(WaveTimingError::Overflow { .. })
        ));
    }

    #[test]
    fn common_legacy_generic_policy_is_explicit_preserve_compatibility() {
        let source = riff(
            &[chunk(*b"cue ", &cue(&[(*b"JUNK", 10, 20, 0)]), None)],
            &[],
        );
        let options = WaveTimingTransformOptions::new(
            48_000,
            44_100,
            0,
            MetadataPolicy::LegacyGeneric,
            Vec::new(),
        )
        .unwrap();
        let result = transform_wave(&source, &options).unwrap();
        assert_eq!(result.bytes, source);
        assert!(result.ledger.iter().any(|entry| entry.candidate));
    }

    #[test]
    fn strip_mode_remains_strict_for_unselected_malformed_chunks() {
        let source = riff(
            &[
                chunk(*b"JUNK", b"drop", None),
                chunk(*b"smpl", &[0; 35], None),
            ],
            &[],
        );
        let options = WaveTimingTransformOptions::strip(48_000, 44_100, 0, vec![*b"JUNK"]).unwrap();
        assert!(matches!(
            transform_wave(&source, &options),
            Err(WaveTimingError::Malformed { .. })
        ));
    }

    #[test]
    fn ledger_records_numeric_values_and_rounding_rule() {
        let source = riff(&[chunk(*b"bext", &bext(48_000), None)], &[]);
        let options = WaveTimingTransformOptions::strict(48_000, 44_100, 0).unwrap();
        let result = transform_wave(&source, &options).unwrap();
        let entry = &result.ledger[0];
        assert_eq!(entry.before_value, Some(48_000));
        assert_eq!(entry.after_value, Some(44_100));
        assert_eq!(
            entry.source_field_sha256(),
            Some(sha256_hex(&48_000_u64.to_le_bytes()).as_str())
        );
        assert_eq!(
            entry.destination_field_sha256(),
            Some(sha256_hex(&44_100_u64.to_le_bytes()).as_str())
        );
        assert_eq!(entry.rounding, Some(RoundingMode::HalfUp));
    }
}
