//! Bounded, policy-independent metadata discovery.
//!
//! This module deliberately stops at inventory.  It does not decide whether a
//! region should be copied, mapped, or stripped; those decisions belong to a
//! later metadata policy/copy planner.  Every discovered region retains its
//! physical order, container path, occurrence number, extents, size, and a
//! SHA-256 digest.  A bounded optional raw copy is retained when the configured
//! item and aggregate budgets allow it.

#[cfg(windows)]
use crate::stable_input::{identity_from_open_file, path_identity_if_exists};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub const METADATA_REGISTRY_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_MAX_METADATA_ITEM_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_MAX_METADATA_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_METADATA_ENTRIES: usize = 100_000;
pub const DEFAULT_MAX_OGG_PACKET_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_OGG_PAGE_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_NESTING_DEPTH: usize = 16;
/// Maximum encoded JSON accepted by [`MetadataInventory::from_json_slice`].
///
/// The cap includes JSON expansion of retained byte arrays as well as paths
/// and diagnostics. It is intentionally larger than the 16 MiB binary raw
/// retention budget while still bounding allocations before validation.
pub const MAX_METADATA_INVENTORY_JSON_BYTES: usize = 128 * 1024 * 1024;

// These bounds are part of the JSON boundary rather than discovery budgets.
// The collection limits below remain instance-configured because a JSON
// Schema cannot compare an array length with `limits.max_metadata_entries`.
const MAX_PATH_COMPONENTS: usize = 256;
const MAX_PATH_COMPONENT_CHARS: usize = 1024;
const MAX_FILE_IDENTITY_CHARS: usize = 256;
const MAX_ISSUE_CODE_CHARS: usize = 128;
const MAX_ISSUE_MESSAGE_CHARS: usize = 4096;

/// The bounded input and retention limits used by discovery.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryLimits {
    /// Maximum bytes charged to metadata entries.  Audio payload bytes are
    /// scanned but are not charged to this budget.
    pub max_metadata_item_bytes: u64,
    pub max_metadata_total_bytes: u64,
    /// Upper bound for retained metadata/structure entries.  The same bound
    /// also caps tracked Ogg serials and normal diagnostic issues; when a
    /// source exceeds either auxiliary cap, one aggregate budget issue is
    /// retained and scanning continues without growing those collections.
    pub max_metadata_entries: usize,
    /// Maximum logical Ogg packet size inspected by the packet assembler.
    pub max_ogg_packet_bytes: u64,
    /// Maximum Ogg page size accepted by the page scanner.
    pub max_ogg_page_bytes: u64,
    pub max_nesting_depth: usize,
    /// Maximum bytes copied into all `MetadataRegion::raw` values.  A zero
    /// value keeps digests/ranges but deliberately retains no raw bytes.
    pub max_retained_raw_bytes: u64,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_metadata_item_bytes: DEFAULT_MAX_METADATA_ITEM_BYTES,
            max_metadata_total_bytes: DEFAULT_MAX_METADATA_TOTAL_BYTES,
            max_metadata_entries: DEFAULT_MAX_METADATA_ENTRIES,
            max_ogg_packet_bytes: DEFAULT_MAX_OGG_PACKET_BYTES,
            max_ogg_page_bytes: DEFAULT_MAX_OGG_PAGE_BYTES,
            max_nesting_depth: DEFAULT_MAX_NESTING_DEPTH,
            max_retained_raw_bytes: DEFAULT_MAX_METADATA_TOTAL_BYTES,
        }
    }
}

/// The container family identified by the discovery signature and headers.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    Wave,
    Flac,
    Mp3,
    OggVorbis,
    OggOpus,
    Ogg,
    IsoBmff,
}

/// A logical metadata region can be physically non-contiguous (notably an Ogg
/// packet assembled from laced pages).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteExtent {
    pub offset: u64,
    pub length: u64,
}

impl ByteExtent {
    fn end(self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }
}

/// A physical or logical region kind.  FourCC/ID values are represented as
/// escaped ASCII strings so unknown values remain serializable and lossless at
/// the identifier level.
#[non_exhaustive]
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataKind {
    WaveChunk { id: String, audio_data: bool },
    FlacBlock { block_type: u8 },
    Id3v2Tag,
    Id3v2Frame { id: String },
    Id3v1Tag,
    Apev2Tag,
    Apev2Item { key: String },
    OggPage { serial: u32, sequence: u32 },
    VorbisComment { serial: u32, packet_index: u32 },
    OpusTags { serial: u32, packet_index: u32 },
    IsoBmffBox { id: String, metadata: bool },
}

/// Container framing and media regions are kept separately from metadata so
/// callers never mistake a large WAVE `data` payload for a copyable tag.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StructuralKind {
    WaveChunk { id: String, audio_data: bool },
    FlacBlock { block_type: u8 },
    OggPage { serial: u32, sequence: u32 },
    OggPacket { serial: u32, packet_index: u32 },
    IsoBmffBox { id: String },
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralRegion {
    pub ordinal: u64,
    pub path: Vec<String>,
    pub kind: StructuralKind,
    pub extents: Vec<ByteExtent>,
    pub size: u64,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Malformed,
    BudgetExceeded,
}

/// A non-fatal discovery issue.  The scanner keeps all safe regions found
/// before the issue and never hides malformed/budget failures.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataIssue {
    pub kind: IssueKind,
    pub code: String,
    pub message: String,
    pub offset: Option<u64>,
    pub length: Option<u64>,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawOmissionReason {
    Disabled,
    ItemLimit,
    AggregateLimit,
    OverlappingRegion,
}

/// One ordered metadata occurrence.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataRegion {
    /// Assigned after scanning by physical first extent, with stable discovery
    /// order as the tie breaker.  This is not a semantic key.
    pub ordinal: u64,
    /// Container-specific path from the enclosing container to this region.
    pub path: Vec<String>,
    pub kind: MetadataKind,
    pub extents: Vec<ByteExtent>,
    /// Sum of all extent lengths.  For an Ogg packet this is the logical
    /// packet payload length, excluding page headers and lacing tables.
    pub size: u64,
    pub raw_sha256: String,
    pub raw: Option<Vec<u8>>,
    pub raw_omitted: Option<RawOmissionReason>,
}

/// Content binding for the exact source observed by discovery.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub byte_len: u64,
    pub sha256: String,
    /// Platform file identity when discovery used a path.  In-memory sources
    /// intentionally leave this unset; content hash and length remain the
    /// portable binding evidence.
    pub file_identity: Option<String>,
}

/// A complete, policy-independent inventory.  `issues` is intentionally part
/// of the result so callers can choose strict/preserve/strip behavior later.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataInventory {
    pub schema_version: u32,
    pub container: ContainerKind,
    pub source: SourceBinding,
    pub regions: Vec<MetadataRegion>,
    pub structural_regions: Vec<StructuralRegion>,
    pub issues: Vec<MetadataIssue>,
    /// Sum of budget-charged region sizes.  This is a safety accounting value,
    /// not the source file size and does not include audio payload chunks.
    pub metadata_bytes: u64,
    pub retained_raw_bytes: u64,
    pub limits: DiscoveryLimits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataInventoryDocument {
    schema_version: u32,
    container: ContainerKind,
    source: SourceBinding,
    regions: Vec<MetadataRegion>,
    structural_regions: Vec<StructuralRegion>,
    issues: Vec<MetadataIssue>,
    metadata_bytes: u64,
    retained_raw_bytes: u64,
    limits: DiscoveryLimits,
}

impl From<MetadataInventoryDocument> for MetadataInventory {
    fn from(document: MetadataInventoryDocument) -> Self {
        Self {
            schema_version: document.schema_version,
            container: document.container,
            source: document.source,
            regions: document.regions,
            structural_regions: document.structural_regions,
            issues: document.issues,
            metadata_bytes: document.metadata_bytes,
            retained_raw_bytes: document.retained_raw_bytes,
            limits: document.limits,
        }
    }
}

#[non_exhaustive]
#[derive(Debug)]
pub enum MetadataRegistryError {
    Io(String),
    Unsupported(String),
    InvalidLimits(String),
    InvalidInventory(String),
}

impl fmt::Display for MetadataRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message)
            | Self::Unsupported(message)
            | Self::InvalidLimits(message)
            | Self::InvalidInventory(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for MetadataRegistryError {}

impl MetadataInventory {
    /// Decode and validate an inventory from a bounded JSON document.
    ///
    /// `MetadataInventory` deliberately does not implement unconstrained
    /// `Deserialize`: serde readers do not expose their total byte length, so
    /// collection allocation could otherwise precede the instance-relative
    /// limits in the document. API boundaries should retain a byte slice (or
    /// otherwise cap their reader) and use this entry point.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, MetadataRegistryError> {
        Self::from_json_slice_with_limit(bytes, MAX_METADATA_INVENTORY_JSON_BYTES)
    }

    fn from_json_slice_with_limit(
        bytes: &[u8],
        maximum_json_bytes: usize,
    ) -> Result<Self, MetadataRegistryError> {
        if bytes.len() > maximum_json_bytes {
            return Err(invalid_inventory(format!(
                "metadata inventory JSON contains {} bytes; maximum is {maximum_json_bytes}",
                bytes.len(),
            )));
        }
        let document: MetadataInventoryDocument =
            serde_json::from_slice(bytes).map_err(|error| {
                invalid_inventory(format!("decode metadata inventory JSON: {error}"))
            })?;
        let inventory = Self::from(document);
        inventory.validate()?;
        Ok(inventory)
    }

    /// Validate an inventory received across a serialization/API boundary.
    ///
    /// Discovery and [`Self::from_json_slice`] call this automatically. It is
    /// also available for callers that clone and edit a public inventory in
    /// memory. The check verifies relationships that JSON Schema cannot
    /// express: source-relative extents, deterministic ordinals, digest/raw
    /// consistency, and the bounded accounting ledgers.
    pub fn validate(&self) -> Result<(), MetadataRegistryError> {
        if self.schema_version != METADATA_REGISTRY_SCHEMA_VERSION {
            return Err(invalid_inventory(format!(
                "unsupported metadata inventory schema version {}; expected {}",
                self.schema_version, METADATA_REGISTRY_SCHEMA_VERSION
            )));
        }
        // Keep the existing limits error classification for callers that use
        // the same limits object for discovery and external validation.
        validate_limits(self.limits)?;

        if self.regions.len() > self.limits.max_metadata_entries {
            return Err(invalid_inventory(format!(
                "metadata region count {} exceeds configured entry limit {}",
                self.regions.len(),
                self.limits.max_metadata_entries
            )));
        }
        if self.structural_regions.len() > self.limits.max_metadata_entries {
            return Err(invalid_inventory(format!(
                "structural region count {} exceeds configured entry limit {}",
                self.structural_regions.len(),
                self.limits.max_metadata_entries
            )));
        }
        let maximum_issue_count = self.limits.max_metadata_entries.saturating_add(1);
        if self.issues.len() > maximum_issue_count {
            return Err(invalid_inventory(format!(
                "metadata issue count {} exceeds configured entry limit {} plus its aggregate marker",
                self.issues.len(), self.limits.max_metadata_entries
            )));
        }
        if self.issues.len() > self.limits.max_metadata_entries
            && !self.issues.iter().any(|issue| {
                issue.kind == IssueKind::BudgetExceeded && issue.code == "REGISTRY-MAX-ISSUES"
            })
        {
            return Err(invalid_inventory(
                "metadata issues exceed the configured entry limit without a REGISTRY-MAX-ISSUES aggregate marker".into(),
            ));
        }

        validate_source_binding(&self.source)?;
        for issue in &self.issues {
            validate_issue(issue)?;
        }

        let mut all_metadata_extents = ExtentLedger::default();
        let mut budgeted_metadata_extents = ExtentLedger::default();
        let mut retained_extents = ExtentLedger::default();
        let mut budgeted_metadata_bytes_seen = 0_u64;
        let mut budgeted_region_facts = Vec::new();
        let mut raw_byte_sum = 0_u64;
        let mut previous_first_offset = None;
        for (index, region) in self.regions.iter().enumerate() {
            let expected_ordinal = u64::try_from(index).map_err(|_| {
                invalid_inventory("metadata region ordinal does not fit in u64".into())
            })?;
            if region.ordinal != expected_ordinal {
                return Err(invalid_inventory(format!(
                    "metadata region ordinal {} is not the canonical ordinal {expected_ordinal}",
                    region.ordinal
                )));
            }
            let first_offset = validate_metadata_region(
                region,
                self.source.byte_len,
                self.limits.max_metadata_entries,
            )?;
            if previous_first_offset.is_some_and(|previous| first_offset < previous) {
                return Err(invalid_inventory(
                    "metadata regions are not ordered by their first extent".into(),
                ));
            }
            previous_first_offset = Some(first_offset);

            let _ = all_metadata_extents.add(&region.extents);
            if metadata_kind_retains_raw(&region.kind, &region.path) {
                let newly_accounted = budgeted_metadata_extents.add(&region.extents);
                budgeted_metadata_bytes_seen = budgeted_metadata_bytes_seen
                    .checked_add(newly_accounted)
                    .ok_or_else(|| {
                        invalid_inventory(
                            "budgeted metadata byte accounting overflows while validating regions"
                                .into(),
                        )
                    })?;
                budgeted_region_facts.push(BudgetedRegionFact {
                    issue_offset: region.extents[0].offset,
                    size: region.size,
                });
            }
            match (&region.raw, &region.raw_omitted) {
                (Some(raw), None) => {
                    if !metadata_kind_retains_raw(&region.kind, &region.path) {
                        return Err(invalid_inventory(format!(
                            "metadata region {} retains raw bytes although its kind is not raw-retainable",
                            region.ordinal
                        )));
                    }
                    if region.size > self.limits.max_metadata_item_bytes {
                        return Err(invalid_inventory(format!(
                            "metadata region {} retains {} raw bytes above configured item limit {}",
                            region.ordinal,
                            region.size,
                            self.limits.max_metadata_item_bytes
                        )));
                    }
                    let raw_len = u64::try_from(raw.len()).map_err(|_| {
                        invalid_inventory("retained metadata bytes do not fit in u64".into())
                    })?;
                    raw_byte_sum = raw_byte_sum.checked_add(raw_len).ok_or_else(|| {
                        invalid_inventory("retained metadata byte accounting overflows".into())
                    })?;
                    let newly_retained = retained_extents.add(&region.extents);
                    if newly_retained != region.size {
                        return Err(invalid_inventory(format!(
                            "metadata region {} retains overlapping or repeated extents",
                            region.ordinal
                        )));
                    }
                }
                (Some(_), Some(_)) => {
                    return Err(invalid_inventory(format!(
                        "metadata region {} has both raw bytes and a raw omission reason",
                        region.ordinal
                    )));
                }
                (None, Some(reason)) => {
                    validate_raw_omission(
                        region,
                        reason,
                        self.limits,
                        &retained_extents,
                        raw_byte_sum,
                    )?;
                }
                (None, None) if metadata_kind_retains_raw(&region.kind, &region.path) => {
                    return Err(invalid_inventory(format!(
                        "metadata region {} has neither raw bytes nor a raw omission reason",
                        region.ordinal
                    )));
                }
                (None, None) => {}
            }
        }

        validate_budget_issues(
            self.limits,
            budgeted_metadata_bytes_seen,
            &budgeted_region_facts,
            &self.issues,
        )?;

        previous_first_offset = None;
        for (index, region) in self.structural_regions.iter().enumerate() {
            let expected_ordinal = u64::try_from(index).map_err(|_| {
                invalid_inventory("structural region ordinal does not fit in u64".into())
            })?;
            if region.ordinal != expected_ordinal {
                return Err(invalid_inventory(format!(
                    "structural region ordinal {} is not the canonical ordinal {expected_ordinal}",
                    region.ordinal
                )));
            }
            let first_offset = validate_structural_region(
                region,
                self.source.byte_len,
                self.limits.max_metadata_entries,
            )?;
            if previous_first_offset.is_some_and(|previous| first_offset < previous) {
                return Err(invalid_inventory(
                    "structural regions are not ordered by their first extent".into(),
                ));
            }
            previous_first_offset = Some(first_offset);
        }

        let all_metadata_bytes = all_metadata_extents.covered_bytes().ok_or_else(|| {
            invalid_inventory("unique metadata extent accounting overflows".into())
        })?;
        let budgeted_metadata_bytes =
            budgeted_metadata_extents.covered_bytes().ok_or_else(|| {
                invalid_inventory("budgeted metadata extent accounting overflows".into())
            })?;
        if self.metadata_bytes != budgeted_metadata_bytes {
            return Err(invalid_inventory(format!(
                "metadata_bytes {} does not equal unique budgeted metadata extents ({budgeted_metadata_bytes})",
                self.metadata_bytes
            )));
        }
        if self.metadata_bytes > all_metadata_bytes {
            return Err(invalid_inventory(format!(
                "metadata_bytes {} exceeds unique discovered metadata extents ({all_metadata_bytes})",
                self.metadata_bytes
            )));
        }
        if self.retained_raw_bytes != raw_byte_sum {
            return Err(invalid_inventory(format!(
                "retained_raw_bytes {} does not equal retained raw length {raw_byte_sum}",
                self.retained_raw_bytes
            )));
        }
        let retained_unique_bytes = retained_extents.covered_bytes().ok_or_else(|| {
            invalid_inventory("unique retained extent accounting overflows".into())
        })?;
        if self.retained_raw_bytes != retained_unique_bytes {
            return Err(invalid_inventory(format!(
                "retained_raw_bytes {} does not equal unique retained extents {retained_unique_bytes}",
                self.retained_raw_bytes
            )));
        }
        if self.retained_raw_bytes > self.limits.max_retained_raw_bytes {
            return Err(invalid_inventory(format!(
                "retained_raw_bytes {} exceeds configured retention limit {}",
                self.retained_raw_bytes, self.limits.max_retained_raw_bytes
            )));
        }
        if self.retained_raw_bytes > self.metadata_bytes {
            return Err(invalid_inventory(format!(
                "retained_raw_bytes {} exceeds metadata_bytes {}",
                self.retained_raw_bytes, self.metadata_bytes
            )));
        }
        Ok(())
    }
}

fn invalid_inventory(message: String) -> MetadataRegistryError {
    MetadataRegistryError::InvalidInventory(message)
}

#[derive(Clone, Copy, Debug)]
struct BudgetedRegionFact {
    /// The builder reports the offset of the first supplied extent, rather
    /// than the minimum extent offset used for ordering.
    issue_offset: u64,
    size: u64,
}

/// Check the instance-relative item and aggregate budget evidence which JSON
/// Schema cannot express.  Discovery deliberately keeps over-budget regions
/// in the inventory so callers can choose a strict/preserve/strip policy; an
/// over-budget inventory is therefore valid only when its builder-produced
/// budget issue is still present (or an aggregate issue explicitly records
/// that the specific budget issue was suppressed).
fn validate_budget_issues(
    limits: DiscoveryLimits,
    budgeted_metadata_bytes: u64,
    facts: &[BudgetedRegionFact],
    issues: &[MetadataIssue],
) -> Result<(), MetadataRegistryError> {
    for issue in issues {
        match issue.code.as_str() {
            "REGISTRY-MAX-ITEM-BYTES" | "REGISTRY-MAX-TOTAL-BYTES" => {
                if issue.kind != IssueKind::BudgetExceeded {
                    return Err(invalid_inventory(format!(
                        "{} must have kind budget_exceeded",
                        issue.code
                    )));
                }
            }
            "REGISTRY-MAX-ISSUES" => {
                if issue.kind != IssueKind::BudgetExceeded {
                    return Err(invalid_inventory(
                        "REGISTRY-MAX-ISSUES must have kind budget_exceeded".into(),
                    ));
                }
                if issues.len() <= limits.max_metadata_entries {
                    return Err(invalid_inventory(
                        "REGISTRY-MAX-ISSUES is present without exceeding the issue entry limit"
                            .into(),
                    ));
                }
            }
            _ => {}
        }
    }

    let aggregate_mentions_item = issues.iter().any(|issue| {
        issue.kind == IssueKind::BudgetExceeded
            && issue.code == "REGISTRY-MAX-ISSUES"
            && issue.message.contains("REGISTRY-MAX-ITEM-BYTES")
    });
    let aggregate_mentions_total = issues.iter().any(|issue| {
        issue.kind == IssueKind::BudgetExceeded
            && issue.code == "REGISTRY-MAX-ISSUES"
            && issue.message.contains("REGISTRY-MAX-TOTAL-BYTES")
    });

    validate_budget_issue_group(
        "REGISTRY-MAX-ITEM-BYTES",
        facts,
        issues,
        aggregate_mentions_item,
        |fact| fact.size > limits.max_metadata_item_bytes,
    )?;
    validate_total_budget_issue(
        "REGISTRY-MAX-TOTAL-BYTES",
        budgeted_metadata_bytes,
        limits.max_metadata_total_bytes,
        facts,
        issues,
        aggregate_mentions_total,
    )?;
    Ok(())
}

fn validate_budget_issue_group(
    code: &str,
    facts: &[BudgetedRegionFact],
    issues: &[MetadataIssue],
    aggregate_evidence: bool,
    exceeds: impl Fn(BudgetedRegionFact) -> bool,
) -> Result<(), MetadataRegistryError> {
    // Budget issues are keyed by the same offset/length pair emitted by the
    // builder.  Count occurrences instead of searching the region list for
    // every issue: repeated physical extents remain a multiset while the
    // complete validation stays O(regions + issues).
    let mut remaining = HashMap::<(u64, u64), usize>::new();
    for fact in facts.iter().copied().filter(|fact| exceeds(*fact)) {
        *remaining.entry((fact.issue_offset, fact.size)).or_default() += 1;
    }
    for issue in issues.iter().filter(|issue| issue.code == code) {
        let Some(offset) = issue.offset else {
            return Err(invalid_inventory(format!(
                "{code} must identify a matching over-budget metadata region"
            )));
        };
        let Some(length) = issue.length else {
            return Err(invalid_inventory(format!(
                "{code} must identify a matching over-budget metadata region"
            )));
        };
        let Some(count) = remaining.get_mut(&(offset, length)) else {
            return Err(invalid_inventory(format!(
                "{code} does not identify a matching over-budget metadata region"
            )));
        };
        if *count == 0 {
            return Err(invalid_inventory(format!(
                "{code} identifies an over-budget metadata region more than once"
            )));
        }
        *count -= 1;
    }

    if !aggregate_evidence && remaining.values().any(|count| *count != 0) {
        if let Some(((offset, _), _)) = remaining.iter().find(|(_, count)| **count != 0) {
            return Err(invalid_inventory(format!(
                "over-budget metadata region at offset {offset} lacks {code} evidence"
            )));
        }
    }
    Ok(())
}

fn validate_total_budget_issue(
    code: &str,
    budgeted_metadata_bytes: u64,
    maximum: u64,
    facts: &[BudgetedRegionFact],
    issues: &[MetadataIssue],
    aggregate_evidence: bool,
) -> Result<(), MetadataRegistryError> {
    let exceeds = budgeted_metadata_bytes > maximum;
    let mut remaining = HashMap::<(u64, u64), usize>::new();
    for fact in facts {
        *remaining.entry((fact.issue_offset, fact.size)).or_default() += 1;
    }
    for issue in issues.iter().filter(|issue| issue.code == code) {
        if !exceeds {
            return Err(invalid_inventory(format!(
                "{code} does not identify a matching aggregate over-budget metadata report"
            )));
        }
        let Some(offset) = issue.offset else {
            return Err(invalid_inventory(format!(
                "{code} must identify a matching aggregate over-budget metadata report"
            )));
        };
        let Some(length) = issue.length else {
            return Err(invalid_inventory(format!(
                "{code} must identify a matching aggregate over-budget metadata report"
            )));
        };
        let Some(count) = remaining.get_mut(&(offset, length)) else {
            return Err(invalid_inventory(format!(
                "{code} does not identify a matching aggregate over-budget metadata report"
            )));
        };
        if *count == 0 {
            return Err(invalid_inventory(format!(
                "{code} identifies an aggregate over-budget metadata report more than once"
            )));
        }
        *count -= 1;
    }
    if exceeds && !issues.iter().any(|issue| issue.code == code) && !aggregate_evidence {
        return Err(invalid_inventory(format!(
            "metadata_bytes {budgeted_metadata_bytes} exceeds configured aggregate limit {maximum} without {code} evidence"
        )));
    }
    Ok(())
}

fn validate_source_binding(source: &SourceBinding) -> Result<(), MetadataRegistryError> {
    validate_sha256("source.sha256", &source.sha256)?;
    if let Some(identity) = source.file_identity.as_deref() {
        validate_text("source.file_identity", identity, MAX_FILE_IDENTITY_CHARS)?;
    }
    Ok(())
}

fn validate_issue(issue: &MetadataIssue) -> Result<(), MetadataRegistryError> {
    validate_text("issue.code", &issue.code, MAX_ISSUE_CODE_CHARS)?;
    validate_text("issue.message", &issue.message, MAX_ISSUE_MESSAGE_CHARS)?;
    Ok(())
}

fn validate_text(field: &str, value: &str, maximum: usize) -> Result<(), MetadataRegistryError> {
    if value.is_empty()
        || value.chars().count() > maximum
        || value.chars().any(is_json_contract_control)
    {
        return Err(invalid_inventory(format!(
            "{field} must contain 1..={maximum} Unicode scalar values without C0/C1 controls or DEL"
        )));
    }
    Ok(())
}

fn is_json_contract_control(character: char) -> bool {
    character <= '\u{001f}' || ('\u{007f}'..='\u{009f}').contains(&character)
}

fn validate_sha256(field: &str, value: &str) -> Result<(), MetadataRegistryError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_inventory(format!(
            "{field} must be exactly 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn validate_path(path: &[String], field: &str) -> Result<(), MetadataRegistryError> {
    if path.is_empty() {
        return Err(invalid_inventory(format!("{field} must not be empty")));
    }
    if path.len() > MAX_PATH_COMPONENTS {
        return Err(invalid_inventory(format!(
            "{field} contains {}; maximum is {MAX_PATH_COMPONENTS}",
            path.len()
        )));
    }
    for (index, component) in path.iter().enumerate() {
        validate_text(
            &format!("{field}[{index}]"),
            component,
            MAX_PATH_COMPONENT_CHARS,
        )?;
    }
    Ok(())
}

fn validate_extent_list(
    extents: &[ByteExtent],
    size: u64,
    source_len: u64,
    maximum_extents: usize,
    field: &str,
) -> Result<u64, MetadataRegistryError> {
    if extents.is_empty() {
        return Err(invalid_inventory(format!("{field} must not be empty")));
    }
    if extents.len() > maximum_extents {
        return Err(invalid_inventory(format!(
            "{field} contains {} extents; configured maximum is {maximum_extents}",
            extents.len()
        )));
    }
    let mut extent_sum = 0_u64;
    let mut first_offset = u64::MAX;
    for (index, extent) in extents.iter().enumerate() {
        if extent.length == 0 {
            return Err(invalid_inventory(format!(
                "{field}[{index}] must have a positive length"
            )));
        }
        let end = extent.offset.checked_add(extent.length).ok_or_else(|| {
            invalid_inventory(format!("{field}[{index}] offset plus length overflows"))
        })?;
        if end > source_len {
            return Err(invalid_inventory(format!(
                "{field}[{index}] ends at {end}, beyond source length {source_len}"
            )));
        }
        extent_sum = extent_sum
            .checked_add(extent.length)
            .ok_or_else(|| invalid_inventory(format!("{field} length sum overflows")))?;
        first_offset = first_offset.min(extent.offset);
    }
    if extent_sum != size {
        return Err(invalid_inventory(format!(
            "{field} length sum {extent_sum} does not equal declared size {size}"
        )));
    }
    Ok(first_offset)
}

fn validate_metadata_region(
    region: &MetadataRegion,
    source_len: u64,
    maximum_extents: usize,
) -> Result<u64, MetadataRegistryError> {
    validate_path(&region.path, "metadata region path")?;
    validate_metadata_kind(&region.kind)?;
    if region.size == 0 {
        return Err(invalid_inventory(format!(
            "metadata region {} must have a positive size",
            region.ordinal
        )));
    }
    validate_sha256("metadata region raw_sha256", &region.raw_sha256)?;
    let first_offset = validate_extent_list(
        &region.extents,
        region.size,
        source_len,
        maximum_extents,
        "metadata region extents",
    )?;
    if let Some(raw) = region.raw.as_deref() {
        let raw_len = u64::try_from(raw.len()).map_err(|_| {
            invalid_inventory("metadata region raw length does not fit in u64".into())
        })?;
        if raw_len != region.size {
            return Err(invalid_inventory(format!(
                "metadata region {} raw length {raw_len} does not equal size {}",
                region.ordinal, region.size
            )));
        }
        if sha256_hex(raw) != region.raw_sha256 {
            return Err(invalid_inventory(format!(
                "metadata region {} raw bytes do not match raw_sha256",
                region.ordinal
            )));
        }
    }
    Ok(first_offset)
}

fn validate_structural_region(
    region: &StructuralRegion,
    source_len: u64,
    maximum_extents: usize,
) -> Result<u64, MetadataRegistryError> {
    validate_path(&region.path, "structural region path")?;
    validate_structural_kind(&region.kind)?;
    if region.size == 0 {
        return Err(invalid_inventory(format!(
            "structural region {} must have a positive size",
            region.ordinal
        )));
    }
    validate_extent_list(
        &region.extents,
        region.size,
        source_len,
        maximum_extents,
        "structural region extents",
    )
}

fn validate_metadata_kind(kind: &MetadataKind) -> Result<(), MetadataRegistryError> {
    match kind {
        MetadataKind::WaveChunk { id, audio_data } => {
            if *audio_data {
                return Err(invalid_inventory(
                    "metadata wave_chunk must not identify audio data".into(),
                ));
            }
            validate_text("metadata kind id", id, MAX_PATH_COMPONENT_CHARS)
        }
        MetadataKind::FlacBlock { block_type } => {
            if *block_type == 0 {
                return Err(invalid_inventory(
                    "metadata flac_block cannot be STREAMINFO block type 0".into(),
                ));
            }
            if *block_type > 127 {
                return Err(invalid_inventory(
                    "metadata flac_block type must be at most 127".into(),
                ));
            }
            Ok(())
        }
        MetadataKind::Id3v2Tag | MetadataKind::Id3v1Tag | MetadataKind::Apev2Tag => Ok(()),
        MetadataKind::Id3v2Frame { id } => {
            validate_text("metadata kind id", id, MAX_PATH_COMPONENT_CHARS)
        }
        MetadataKind::Apev2Item { key } => {
            validate_text("metadata kind key", key, MAX_PATH_COMPONENT_CHARS)
        }
        MetadataKind::OggPage { .. }
        | MetadataKind::VorbisComment { .. }
        | MetadataKind::OpusTags { .. } => Ok(()),
        MetadataKind::IsoBmffBox { id, metadata } => {
            if !*metadata {
                return Err(invalid_inventory(
                    "metadata iso_bmff_box must have metadata=true".into(),
                ));
            }
            validate_text("metadata kind id", id, MAX_PATH_COMPONENT_CHARS)
        }
    }
}

fn validate_structural_kind(kind: &StructuralKind) -> Result<(), MetadataRegistryError> {
    match kind {
        StructuralKind::WaveChunk { id, .. } | StructuralKind::IsoBmffBox { id } => {
            validate_text("structural kind id", id, MAX_PATH_COMPONENT_CHARS)
        }
        StructuralKind::FlacBlock { block_type } => {
            if *block_type != 0 {
                return Err(invalid_inventory(
                    "structural flac_block must be STREAMINFO block type 0".into(),
                ));
            }
            Ok(())
        }
        StructuralKind::OggPage { .. } | StructuralKind::OggPacket { .. } => Ok(()),
    }
}

fn metadata_kind_retains_raw(kind: &MetadataKind, path: &[String]) -> bool {
    match kind {
        MetadataKind::IsoBmffBox { id, .. } => {
            // Container boxes and user-item boxes are inventory structure, not
            // independently copyable metadata.  The parser marks those with
            // retain_raw=false; infer the same distinction at this boundary.
            !is_bmff_container_name(id)
                && !(path.iter().any(|component| component == "ilst")
                    && !matches!(id.as_str(), "data" | "mean" | "name"))
        }
        _ => true,
    }
}

fn validate_raw_omission(
    region: &MetadataRegion,
    reason: &RawOmissionReason,
    limits: DiscoveryLimits,
    retained_extents: &ExtentLedger,
    retained_raw_bytes: u64,
) -> Result<(), MetadataRegistryError> {
    if !metadata_kind_retains_raw(&region.kind, &region.path) {
        return Err(invalid_inventory(format!(
            "metadata region {} has an omission reason although raw retention is not applicable",
            region.ordinal
        )));
    }
    match reason {
        RawOmissionReason::Disabled => {
            if limits.max_retained_raw_bytes != 0 {
                return Err(invalid_inventory(format!(
                    "metadata region {} claims raw retention is disabled with a non-zero limit",
                    region.ordinal
                )));
            }
        }
        RawOmissionReason::ItemLimit => {
            if limits.max_retained_raw_bytes == 0 || region.size <= limits.max_metadata_item_bytes {
                return Err(invalid_inventory(format!(
                    "metadata region {} claims an item-limit omission for a region within the item limit",
                    region.ordinal
                )));
            }
        }
        RawOmissionReason::AggregateLimit => {
            if limits.max_retained_raw_bytes == 0 || region.size > limits.max_metadata_item_bytes {
                return Err(invalid_inventory(format!(
                    "metadata region {} has an omission reason inconsistent with its configured limits",
                    region.ordinal
                )));
            }
            if retained_raw_bytes
                .checked_add(region.size)
                .is_some_and(|total| total <= limits.max_retained_raw_bytes)
            {
                return Err(invalid_inventory(format!(
                    "metadata region {} claims an aggregate-limit omission while it still fits the retention budget",
                    region.ordinal
                )));
            }
        }
        RawOmissionReason::OverlappingRegion => {
            if limits.max_retained_raw_bytes == 0 || region.size > limits.max_metadata_item_bytes {
                return Err(invalid_inventory(format!(
                    "metadata region {} has an omission reason inconsistent with its configured limits",
                    region.ordinal
                )));
            }
            let mut local_extents = ExtentLedger::default();
            let internally_unique = local_extents.add(&region.extents);
            if !retained_extents.overlaps(&region.extents) && internally_unique == region.size {
                return Err(invalid_inventory(format!(
                    "metadata region {} claims an overlapping-region omission without overlapping retained bytes or repeated extents",
                    region.ordinal
                )));
            }
        }
    }
    Ok(())
}

fn is_bmff_container_name(id: &str) -> bool {
    matches!(
        id,
        "moov"
            | "trak"
            | "mdia"
            | "minf"
            | "stbl"
            | "mvex"
            | "moof"
            | "traf"
            | "edts"
            | "dinf"
            | "schi"
            | "sinf"
            | "udta"
            | "meta"
            | "ilst"
            | "ludt"
            | "keys"
            | "ipro"
    )
}

/// Discover a path using one opened source handle.  The returned binding is a
/// full SHA-256 of the exact handle contents, independent of the pathname.
pub fn discover_path(
    path: &Path,
    limits: DiscoveryLimits,
) -> Result<MetadataInventory, MetadataRegistryError> {
    validate_limits(limits)?;
    let (file, length, identity) = open_regular_path(path)?;
    let mut source = InputSource::File {
        file,
        length,
        identity,
    };
    discover_source(&mut source, limits)
}

fn open_regular_path(path: &Path) -> Result<(File, u64, Option<String>), MetadataRegistryError> {
    let link_metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if !link_metadata.is_file() {
        return Err(MetadataRegistryError::Io(format!(
            "{}: metadata discovery requires a regular file",
            path.display()
        )));
    }
    #[cfg(windows)]
    if link_metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(MetadataRegistryError::Io(format!(
            "{}: metadata discovery refuses a reparse point",
            path.display()
        )));
    }
    // `volume_serial_number` and `file_index` on Windows metadata are still
    // unstable (`windows_by_handle`). Capture the pre-open identity through
    // the stable handle-based helper instead, so the existing path/open race
    // check remains fail-closed on Rust 1.89 and newer.
    #[cfg(windows)]
    let link_identity = path_identity_if_exists(path)
        .map_err(|error| MetadataRegistryError::Io(format!("{}: {error}", path.display())))?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    let file = options.open(path).map_err(|error| io_error(path, error))?;
    let length = file
        .metadata()
        .map_err(|error| io_error(path, error))?
        .len();
    let metadata = file.metadata().map_err(|error| io_error(path, error))?;
    if !metadata.is_file() {
        return Err(MetadataRegistryError::Io(format!(
            "{}: metadata discovery target is not a regular file",
            path.display()
        )));
    }
    #[cfg(windows)]
    if metadata.file_attributes() & 0x0000_0400 != 0 {
        return Err(MetadataRegistryError::Io(format!(
            "{}: metadata discovery target is a reparse point",
            path.display()
        )));
    }
    #[cfg(unix)]
    if link_metadata.dev() != metadata.dev() || link_metadata.ino() != metadata.ino() {
        return Err(MetadataRegistryError::Io(format!(
            "{}: metadata source changed while it was opened",
            path.display()
        )));
    }
    #[cfg(windows)]
    {
        let opened_identity = identity_from_open_file(&file, path)
            .map_err(|error| MetadataRegistryError::Io(format!("{}: {error}", path.display())))?;
        if link_identity.as_ref() != Some(&opened_identity) {
            return Err(MetadataRegistryError::Io(format!(
                "{}: metadata source changed while it was opened",
                path.display()
            )));
        }
    }
    let identity = file_identity(&file, &metadata)?;
    Ok((file, length, identity))
}

/// Discover an in-memory source without taking ownership or cloning the input.
pub fn discover_bytes(
    bytes: &[u8],
    limits: DiscoveryLimits,
) -> Result<MetadataInventory, MetadataRegistryError> {
    validate_limits(limits)?;
    let mut source = InputSource::Bytes(bytes);
    discover_source(&mut source, limits)
}

fn validate_limits(limits: DiscoveryLimits) -> Result<(), MetadataRegistryError> {
    if limits.max_metadata_item_bytes == 0
        || limits.max_metadata_total_bytes == 0
        || limits.max_metadata_entries == 0
        || limits.max_ogg_packet_bytes == 0
        || limits.max_ogg_page_bytes == 0
        || limits.max_nesting_depth == 0
    {
        return Err(MetadataRegistryError::InvalidLimits(
            "metadata discovery limits must be positive".into(),
        ));
    }
    if limits.max_metadata_item_bytes > DEFAULT_MAX_METADATA_TOTAL_BYTES
        || limits.max_metadata_total_bytes > DEFAULT_MAX_METADATA_TOTAL_BYTES
        || limits.max_metadata_entries > DEFAULT_MAX_METADATA_ENTRIES
        || limits.max_ogg_packet_bytes > DEFAULT_MAX_OGG_PACKET_BYTES
        || limits.max_ogg_page_bytes > DEFAULT_MAX_OGG_PAGE_BYTES
        || limits.max_retained_raw_bytes > DEFAULT_MAX_METADATA_TOTAL_BYTES
    {
        return Err(MetadataRegistryError::InvalidLimits(
            "metadata discovery limits exceed the registry's absolute safety bounds".into(),
        ));
    }
    if limits.max_nesting_depth >= MAX_PATH_COMPONENTS {
        return Err(MetadataRegistryError::InvalidLimits(format!(
            "metadata nesting depth must be less than {MAX_PATH_COMPONENTS}"
        )));
    }
    Ok(())
}

fn io_error(path: &Path, error: io::Error) -> MetadataRegistryError {
    MetadataRegistryError::Io(format!("{}: {error}", path.display()))
}

enum InputSource<'a> {
    Bytes(&'a [u8]),
    File {
        file: File,
        length: u64,
        identity: Option<String>,
    },
}

impl InputSource<'_> {
    fn len(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File { length, .. } => *length,
        }
    }

    fn read_exact_at(
        &mut self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), MetadataRegistryError> {
        let end = offset
            .checked_add(destination.len() as u64)
            .ok_or_else(|| MetadataRegistryError::Io("metadata source offset overflow".into()))?;
        if end > self.len() {
            return Err(MetadataRegistryError::Io(
                "metadata source read exceeds source length".into(),
            ));
        }
        match self {
            Self::Bytes(bytes) => {
                let start = usize::try_from(offset).map_err(|_| {
                    MetadataRegistryError::Io("metadata source offset does not fit usize".into())
                })?;
                destination.copy_from_slice(&bytes[start..start + destination.len()]);
                Ok(())
            }
            Self::File { file, .. } => {
                file.seek(SeekFrom::Start(offset))
                    .map_err(|error| MetadataRegistryError::Io(error.to_string()))?;
                file.read_exact(destination)
                    .map_err(|error| MetadataRegistryError::Io(error.to_string()))
            }
        }
    }

    fn read_u8(&mut self, offset: u64) -> Result<u8, MetadataRegistryError> {
        let mut value = [0];
        self.read_exact_at(offset, &mut value)?;
        Ok(value[0])
    }

    fn read_extents(
        &mut self,
        extents: &[ByteExtent],
        maximum: u64,
    ) -> Result<Vec<u8>, MetadataRegistryError> {
        let total = extents.iter().try_fold(0_u64, |total, extent| {
            total
                .checked_add(extent.length)
                .ok_or_else(|| MetadataRegistryError::Io("metadata extent size overflow".into()))
        })?;
        if total > maximum {
            return Err(MetadataRegistryError::Io(
                "metadata extent read exceeds configured item limit".into(),
            ));
        }
        let capacity = usize::try_from(total)
            .map_err(|_| MetadataRegistryError::Io("metadata extents do not fit usize".into()))?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut buffer = [0_u8; 32 * 1024];
        for extent in extents {
            let mut offset = extent.offset;
            let mut remaining = extent.length;
            while remaining != 0 {
                let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
                self.read_exact_at(offset, &mut buffer[..count])?;
                bytes.extend_from_slice(&buffer[..count]);
                offset = offset.checked_add(count as u64).ok_or_else(|| {
                    MetadataRegistryError::Io("metadata extent offset overflow".into())
                })?;
                remaining -= count as u64;
            }
        }
        Ok(bytes)
    }

    fn hash_extents(&mut self, extents: &[ByteExtent]) -> Result<String, MetadataRegistryError> {
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 32 * 1024];
        for extent in extents {
            let mut offset = extent.offset;
            let mut remaining = extent.length;
            while remaining != 0 {
                let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
                self.read_exact_at(offset, &mut buffer[..count])?;
                digest.update(&buffer[..count]);
                offset = offset.checked_add(count as u64).ok_or_else(|| {
                    MetadataRegistryError::Io("metadata extent offset overflow".into())
                })?;
                remaining -= count as u64;
            }
        }
        Ok(hex_digest(digest.finalize()))
    }

    fn hash_all(&mut self) -> Result<String, MetadataRegistryError> {
        let mut digest = Sha256::new();
        let mut offset = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        while offset < self.len() {
            let count = usize::try_from((self.len() - offset).min(buffer.len() as u64)).unwrap();
            self.read_exact_at(offset, &mut buffer[..count])?;
            digest.update(&buffer[..count]);
            offset = offset.checked_add(count as u64).ok_or_else(|| {
                MetadataRegistryError::Io("metadata source length overflow".into())
            })?;
        }
        Ok(hex_digest(digest.finalize()))
    }

    fn current_file_binding(&self) -> Result<Option<(u64, Option<String>)>, MetadataRegistryError> {
        let Self::File { file, .. } = self else {
            return Ok(None);
        };
        let metadata = file
            .metadata()
            .map_err(|error| MetadataRegistryError::Io(error.to_string()))?;
        Ok(Some((metadata.len(), file_identity(file, &metadata)?)))
    }

    fn initial_file_identity(&self) -> Option<String> {
        match self {
            Self::Bytes(_) => None,
            Self::File { identity, .. } => identity.clone(),
        }
    }
}

#[derive(Clone, Default)]
struct ExtentLedger {
    ranges: BTreeMap<u64, u64>,
}

impl ExtentLedger {
    /// Add ranges and return the number of bytes not already covered.  The
    /// ledger is shared by nested metadata regions so a tag and its frames (or
    /// a page and a packet) are charged once for aggregate accounting.
    fn add(&mut self, extents: &[ByteExtent]) -> u64 {
        let mut newly_covered = 0_u64;
        for extent in extents {
            let Some(original_end) = extent.end() else {
                continue;
            };
            let original_start = extent.offset;
            if original_end <= original_start {
                continue;
            }
            let mut already_covered = 0_u64;
            let mut merged_start = original_start;
            let mut merged_end = original_end;

            // The map invariant keeps ranges disjoint and merges adjacency.
            // At most one predecessor can therefore touch this extent. Start
            // with it instead of scanning every earlier range: an input with
            // the maximum number of disjoint metadata items must remain
            // O(N log N), not O(N^2).
            let predecessor = self
                .ranges
                .range(..=original_start)
                .next_back()
                .map(|(&start, &end)| (start, end));
            if let Some((range_start, range_end)) =
                predecessor.filter(|(_, range_end)| *range_end >= original_start)
            {
                already_covered = already_covered.saturating_add(intersection_length(
                    original_start,
                    original_end,
                    range_start,
                    range_end,
                ));
                merged_start = merged_start.min(range_start);
                merged_end = merged_end.max(range_end);
                self.ranges.remove(&range_start);
            }

            // Removing one successor at a time avoids retaining an attacker-
            // sized temporary vector. Extending merged_end can make the next
            // range adjacent, so continue until the inclusive range query is
            // empty.
            while let Some((range_start, range_end)) = self
                .ranges
                .range(merged_start..=merged_end)
                .next()
                .map(|(&start, &end)| (start, end))
            {
                already_covered = already_covered.saturating_add(intersection_length(
                    original_start,
                    original_end,
                    range_start,
                    range_end,
                ));
                merged_end = merged_end.max(range_end);
                self.ranges.remove(&range_start);
            }
            newly_covered = newly_covered
                .saturating_add((original_end - original_start).saturating_sub(already_covered));
            self.ranges.insert(merged_start, merged_end);
        }
        newly_covered
    }

    fn overlaps(&self, extents: &[ByteExtent]) -> bool {
        extents.iter().any(|extent| {
            let Some(end) = extent.end() else {
                return true;
            };
            // Ranges are half-open.  A region beginning exactly where a
            // retained range ends is adjacent, not overlapping; using
            // `..=end` here would incorrectly suppress raw retention for
            // every pair of neighbouring chunks.
            self.ranges
                .range(..end)
                .next_back()
                .is_some_and(|(_, range_end)| *range_end > extent.offset)
        })
    }

    /// Return the number of bytes that would be newly covered without
    /// changing this ledger.  This is used for raw-retention accounting when
    /// an adapter supplies overlapping or repeated extents of its own.
    fn newly_covered(&self, extents: &[ByteExtent]) -> u64 {
        // Normalize only the candidate extents, then subtract the portions
        // already covered by this ledger. Cloning the complete ledger here
        // makes a sequence of disjoint retained regions quadratic in both
        // allocations and tree-node copies.
        let mut candidate = ExtentLedger::default();
        let candidate_bytes = candidate.add(extents);
        let already_covered = candidate
            .ranges
            .iter()
            .fold(0_u64, |covered, (&start, &end)| {
                covered.saturating_add(self.covered_between(start, end))
            });
        candidate_bytes.saturating_sub(already_covered)
    }

    fn covered_between(&self, start: u64, end: u64) -> u64 {
        if end <= start {
            return 0;
        }
        let predecessor = self
            .ranges
            .range(..=start)
            .next_back()
            .map(|(&range_start, &range_end)| (range_start, range_end));
        let mut covered =
            predecessor.map_or(0, |(_, range_end)| range_end.min(end).saturating_sub(start));
        for (&range_start, &range_end) in self.ranges.range(start..end) {
            // The inclusive predecessor lookup also returns a range starting
            // exactly at `start`; do not count that range twice.
            if predecessor.is_some_and(|(predecessor_start, _)| predecessor_start == range_start) {
                continue;
            }
            covered = covered.saturating_add(range_end.min(end).saturating_sub(range_start));
        }
        covered
    }

    fn covered_bytes(&self) -> Option<u64> {
        self.ranges.iter().try_fold(0_u64, |total, (start, end)| {
            end.checked_sub(*start)
                .and_then(|length| total.checked_add(length))
        })
    }
}

fn intersection_length(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> u64 {
    left_end
        .min(right_end)
        .saturating_sub(left_start.max(right_start))
}

struct InventoryBuilder<'source, 'input> {
    source: &'source mut InputSource<'input>,
    limits: DiscoveryLimits,
    container: ContainerKind,
    regions: Vec<(usize, MetadataRegion)>,
    structural_regions: Vec<(usize, StructuralRegion)>,
    issues: Vec<MetadataIssue>,
    metadata_bytes: u64,
    retained_raw_bytes: u64,
    sequence: usize,
    entry_budget_exhausted: bool,
    structural_budget_exhausted: bool,
    issue_budget_exhausted: bool,
    suppressed_issue_count: u64,
    first_suppressed_issue_code: Option<String>,
    metadata_ledger: ExtentLedger,
    retained_ledger: ExtentLedger,
    region_extent_keys: HashSet<(u64, u64)>,
}

impl<'source, 'input> InventoryBuilder<'source, 'input> {
    fn new(
        source: &'source mut InputSource<'input>,
        limits: DiscoveryLimits,
        container: ContainerKind,
    ) -> Self {
        Self {
            source,
            limits,
            container,
            regions: Vec::new(),
            structural_regions: Vec::new(),
            issues: Vec::new(),
            metadata_bytes: 0,
            retained_raw_bytes: 0,
            sequence: 0,
            entry_budget_exhausted: false,
            structural_budget_exhausted: false,
            issue_budget_exhausted: false,
            suppressed_issue_count: 0,
            first_suppressed_issue_code: None,
            metadata_ledger: ExtentLedger::default(),
            retained_ledger: ExtentLedger::default(),
            region_extent_keys: HashSet::new(),
        }
    }

    fn issue(
        &mut self,
        kind: IssueKind,
        code: impl Into<String>,
        message: impl Into<String>,
        offset: Option<u64>,
        length: Option<u64>,
    ) {
        let code = code.into();
        let message = message.into();
        let is_budget_evidence = matches!(
            code.as_str(),
            "REGISTRY-MAX-ITEM-BYTES" | "REGISTRY-MAX-TOTAL-BYTES"
        );
        if self.issues.len() < self.limits.max_metadata_entries {
            self.issues.push(MetadataIssue {
                kind,
                code,
                message,
                offset,
                length,
            });
            return;
        }

        // An adversarial stream can otherwise append one issue per malformed
        // page/frame forever.  Keep the configured entry budget as the normal
        // issue bound, plus one aggregate marker that makes suppression
        // observable without retaining all attacker-controlled diagnostics.
        self.suppressed_issue_count = self.suppressed_issue_count.saturating_add(1);
        if self.first_suppressed_issue_code.is_none() {
            self.first_suppressed_issue_code = Some(code.clone());
        }
        let first_suppressed_issue_code = self
            .first_suppressed_issue_code
            .as_deref()
            .unwrap_or("unknown");
        if !self.issue_budget_exhausted {
            self.issues.push(MetadataIssue {
                kind: IssueKind::BudgetExceeded,
                code: "REGISTRY-MAX-ISSUES".into(),
                message: format!(
                    "metadata issue count exceeds configured entry limit {}; additional issues are aggregated (first suppressed code: {first_suppressed_issue_code})",
                    self.limits.max_metadata_entries,
                ),
                offset,
                length,
            });
            self.issue_budget_exhausted = true;
        } else if let Some(issue) = self.issues.last_mut() {
            issue.message = format!(
                "metadata issue count exceeds configured entry limit {}; suppressed {} additional issue(s) (first suppressed code: {first_suppressed_issue_code})",
                self.limits.max_metadata_entries,
                self.suppressed_issue_count,
            );
        }
        if is_budget_evidence {
            // The aggregate marker is the only retained evidence when a
            // per-region budget issue arrives after the diagnostic budget has
            // filled. Keep the suppressed budget code in its bounded message
            // so external validation can distinguish an aggregate budget
            // report from an unrelated issue-count marker.
            if let Some(issue) = self.issues.last_mut() {
                if !issue.message.contains(&code) {
                    issue.message.push_str("; suppressed budget code: ");
                    issue.message.push_str(&code);
                }
            }
        }
    }

    fn set_container(&mut self, container: ContainerKind) {
        self.container = container;
    }

    fn malformed(
        &mut self,
        code: impl Into<String>,
        message: impl Into<String>,
        offset: Option<u64>,
        length: Option<u64>,
    ) {
        self.issue(IssueKind::Malformed, code, message, offset, length);
    }

    fn has_region_extent(&self, offset: u64, length: u64) -> bool {
        self.region_extent_keys.contains(&(offset, length))
    }

    fn add_region(
        &mut self,
        path: Vec<String>,
        kind: MetadataKind,
        extents: Vec<ByteExtent>,
        budgeted: bool,
        retain_raw: bool,
    ) -> Result<(), MetadataRegistryError> {
        if extents.is_empty() {
            return Ok(());
        }
        let mut size = 0_u64;
        for extent in &extents {
            if extent.length == 0 {
                continue;
            }
            if extent.end().is_none_or(|end| end > self.source.len()) {
                self.malformed(
                    "REGISTRY-REGION-BOUNDS",
                    "metadata region extent exceeds source bounds",
                    Some(extent.offset),
                    Some(extent.length),
                );
                return Ok(());
            }
            size = size
                .checked_add(extent.length)
                .ok_or_else(|| MetadataRegistryError::Io("metadata region size overflow".into()))?;
        }
        if size == 0 {
            return Ok(());
        }
        if self.regions.len() >= self.limits.max_metadata_entries {
            if !self.entry_budget_exhausted {
                self.issue(
                    IssueKind::BudgetExceeded,
                    "REGISTRY-MAX-ENTRIES",
                    format!(
                        "metadata region count exceeds configured limit {}",
                        self.limits.max_metadata_entries
                    ),
                    extents.first().map(|extent| extent.offset),
                    Some(size),
                );
                self.entry_budget_exhausted = true;
            }
            return Ok(());
        }
        if budgeted {
            if size > self.limits.max_metadata_item_bytes {
                self.issue(
                    IssueKind::BudgetExceeded,
                    "REGISTRY-MAX-ITEM-BYTES",
                    format!(
                        "metadata region contains {size} bytes, above configured item limit {}",
                        self.limits.max_metadata_item_bytes
                    ),
                    extents.first().map(|extent| extent.offset),
                    Some(size),
                );
            }
            let newly_accounted = self.metadata_ledger.add(&extents);
            self.metadata_bytes = self.metadata_bytes.saturating_add(newly_accounted);
            if self.metadata_bytes > self.limits.max_metadata_total_bytes {
                self.issue(
                    IssueKind::BudgetExceeded,
                    "REGISTRY-MAX-TOTAL-BYTES",
                    format!(
                        "metadata regions contain {} bytes, above configured aggregate limit {}",
                        self.metadata_bytes, self.limits.max_metadata_total_bytes
                    ),
                    extents.first().map(|extent| extent.offset),
                    Some(size),
                );
            }
        }
        let raw_sha256 = self.source.hash_extents(&extents)?;
        let mut raw = None;
        let mut raw_omitted = None;
        if retain_raw {
            if self.limits.max_retained_raw_bytes == 0 {
                raw_omitted = Some(RawOmissionReason::Disabled);
            } else if size > self.limits.max_metadata_item_bytes {
                raw_omitted = Some(RawOmissionReason::ItemLimit);
            } else if self.retained_ledger.overlaps(&extents) {
                // Do not allocate a second copy of bytes already retained by
                // an enclosing/preceding region.  The enclosing raw region
                // still gives callers an exact source copy.
                raw_omitted = Some(RawOmissionReason::OverlappingRegion);
            } else {
                let newly_retained = self.retained_ledger.newly_covered(&extents);
                // Repeated extents within one logical region would make the
                // concatenated raw value larger than the unique source bytes
                // represented by the retention budget.  Keep the digest and
                // ranges, but avoid an under-accounted allocation.
                if newly_retained != size {
                    raw_omitted = Some(RawOmissionReason::OverlappingRegion);
                } else if self
                    .retained_raw_bytes
                    .checked_add(newly_retained)
                    .is_none_or(|total| total > self.limits.max_retained_raw_bytes)
                {
                    raw_omitted = Some(RawOmissionReason::AggregateLimit);
                } else {
                    raw = Some(self.source.read_extents(&extents, size)?);
                    let newly_retained = self.retained_ledger.add(&extents);
                    self.retained_raw_bytes = self
                        .retained_raw_bytes
                        .checked_add(newly_retained)
                        .ok_or_else(|| {
                        MetadataRegistryError::Io("retained metadata size overflow".into())
                    })?;
                }
            }
        }
        let region_extent_key =
            (extents.len() == 1).then(|| (extents[0].offset, extents[0].length));
        self.regions.push((
            self.sequence,
            MetadataRegion {
                ordinal: 0,
                path,
                kind,
                extents,
                size,
                raw_sha256,
                raw,
                raw_omitted,
            },
        ));
        if let Some(key) = region_extent_key {
            self.region_extent_keys.insert(key);
        }
        self.sequence += 1;
        Ok(())
    }

    fn add_structural(
        &mut self,
        path: Vec<String>,
        kind: StructuralKind,
        extents: Vec<ByteExtent>,
    ) {
        if extents.is_empty() {
            return;
        }
        if self.structural_regions.len() >= self.limits.max_metadata_entries {
            if !self.structural_budget_exhausted {
                self.issue(
                    IssueKind::BudgetExceeded,
                    "REGISTRY-MAX-STRUCTURAL-ENTRIES",
                    format!(
                        "structural region count exceeds configured limit {}",
                        self.limits.max_metadata_entries
                    ),
                    extents.first().map(|extent| extent.offset),
                    None,
                );
                self.structural_budget_exhausted = true;
            }
            return;
        }
        let size = extents
            .iter()
            .try_fold(0_u64, |size, extent| size.checked_add(extent.length));
        let Some(size) = size else {
            self.malformed(
                "REGISTRY-STRUCTURAL-SIZE",
                "structural region size overflows",
                extents.first().map(|extent| extent.offset),
                None,
            );
            return;
        };
        self.structural_regions.push((
            self.sequence,
            StructuralRegion {
                ordinal: 0,
                path,
                kind,
                extents,
                size,
            },
        ));
        self.sequence += 1;
    }

    fn finish(mut self, source: SourceBinding) -> MetadataInventory {
        self.regions.sort_by_key(|(sequence, region)| {
            (
                region
                    .extents
                    .iter()
                    .map(|extent| extent.offset)
                    .min()
                    .unwrap_or(u64::MAX),
                *sequence,
            )
        });
        self.structural_regions.sort_by_key(|(sequence, region)| {
            (
                region
                    .extents
                    .iter()
                    .map(|extent| extent.offset)
                    .min()
                    .unwrap_or(u64::MAX),
                *sequence,
            )
        });
        let regions = self
            .regions
            .into_iter()
            .enumerate()
            .map(|(ordinal, (_, mut region))| {
                region.ordinal = ordinal as u64;
                region
            })
            .collect();
        let structural_regions = self
            .structural_regions
            .into_iter()
            .enumerate()
            .map(|(ordinal, (_, mut region))| {
                region.ordinal = ordinal as u64;
                region
            })
            .collect();
        MetadataInventory {
            schema_version: METADATA_REGISTRY_SCHEMA_VERSION,
            container: self.container,
            source,
            regions,
            structural_regions,
            issues: self.issues,
            metadata_bytes: self.metadata_bytes,
            retained_raw_bytes: self.retained_raw_bytes,
            limits: self.limits,
        }
    }
}

fn discover_source(
    source: &mut InputSource<'_>,
    limits: DiscoveryLimits,
) -> Result<MetadataInventory, MetadataRegistryError> {
    let length = source.len();
    let initial_file_identity = source.initial_file_identity();
    let source_hash_before = source.hash_all()?;
    let prefix_len = usize::try_from(length.min(16)).unwrap();
    let mut prefix = vec![0_u8; prefix_len];
    source.read_exact_at(0, &mut prefix)?;
    let mut inventory = if is_wave_signature(&prefix) {
        let mut builder = InventoryBuilder::new(source, limits, ContainerKind::Wave);
        discover_wave(&mut builder)?;
        builder.finish(SourceBinding {
            byte_len: length,
            sha256: source_hash_before.clone(),
            file_identity: None,
        })
    } else if prefix.starts_with(b"fLaC") {
        let mut builder = InventoryBuilder::new(source, limits, ContainerKind::Flac);
        discover_flac(&mut builder)?;
        builder.finish(SourceBinding {
            byte_len: length,
            sha256: source_hash_before.clone(),
            file_identity: None,
        })
    } else if prefix.starts_with(b"OggS") {
        let mut builder = InventoryBuilder::new(source, limits, ContainerKind::Ogg);
        discover_ogg(&mut builder)?;
        builder.finish(SourceBinding {
            byte_len: length,
            sha256: source_hash_before.clone(),
            file_identity: None,
        })
    } else if is_isobmff_signature(&prefix, length) {
        let mut builder = InventoryBuilder::new(source, limits, ContainerKind::IsoBmff);
        discover_isobmff(&mut builder)?;
        builder.finish(SourceBinding {
            byte_len: length,
            sha256: source_hash_before.clone(),
            file_identity: None,
        })
    } else if is_mpeg_or_tag_signature(&prefix) {
        let mut builder = InventoryBuilder::new(source, limits, ContainerKind::Mp3);
        discover_mp3(&mut builder)?;
        builder.finish(SourceBinding {
            byte_len: length,
            sha256: source_hash_before.clone(),
            file_identity: None,
        })
    } else {
        return Err(MetadataRegistryError::Unsupported(
            "metadata registry has no bounded adapter for this source signature".into(),
        ));
    };
    let source_hash_after = source.hash_all()?;
    let current_binding = source.current_file_binding()?;
    if source_hash_before != source_hash_after
        || current_binding
            .as_ref()
            .is_some_and(|(current_length, _)| *current_length != length)
        || current_binding
            .as_ref()
            .is_some_and(|(_, identity)| initial_file_identity.as_ref() != identity.as_ref())
    {
        return Err(MetadataRegistryError::Io(
            "metadata source changed while it was being discovered".into(),
        ));
    }
    inventory.source.sha256 = source_hash_after;
    inventory.source.file_identity = current_binding.and_then(|(_, identity)| identity);
    inventory.validate()?;
    Ok(inventory)
}

fn is_wave_signature(prefix: &[u8]) -> bool {
    prefix.len() >= 12
        && matches!(&prefix[..4], b"RIFF" | b"RF64" | b"BW64")
        && &prefix[8..12] == b"WAVE"
}

fn is_isobmff_signature(prefix: &[u8], length: u64) -> bool {
    if prefix.len() < 8 || &prefix[4..8] != b"ftyp" {
        return false;
    }
    length >= 16
}

fn is_mpeg_or_tag_signature(prefix: &[u8]) -> bool {
    prefix.starts_with(b"ID3")
        || prefix.starts_with(b"APETAGEX")
        || prefix.first().is_some_and(|byte| *byte == 0xff)
}

fn fourcc(bytes: [u8; 4]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte).to_string()
            } else {
                format!("\\x{byte:02x}")
            }
        })
        .collect()
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex_digest(digest.finalize())
}

fn file_identity(
    file: &File,
    metadata: &fs::Metadata,
) -> Result<Option<String>, MetadataRegistryError> {
    #[cfg(unix)]
    {
        let _ = file;
        Ok(Some(format!("unix:{}:{}", metadata.dev(), metadata.ino())))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        let (volume, index) = crate::stable_input::windows_file_identity(file)
            .map_err(|error| MetadataRegistryError::Io(format!("identify open file: {error}")))?;
        Ok(Some(format!("windows:{volume}:{index}")))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, metadata);
        Ok(None)
    }
}

fn discover_wave(builder: &mut InventoryBuilder<'_, '_>) -> Result<(), MetadataRegistryError> {
    let source_len = builder.source.len();
    let mut riff_header = [0_u8; 12];
    builder.source.read_exact_at(0, &mut riff_header)?;
    let container_id: [u8; 4] = riff_header[..4].try_into().unwrap();
    let declared_riff_size = u32::from_le_bytes(riff_header[4..8].try_into().unwrap()) as u64;
    let is_rf64 = matches!(&container_id, b"RF64" | b"BW64");
    let mut declared_container_end = if !is_rf64 && declared_riff_size != u64::from(u32::MAX) {
        8_u64.checked_add(declared_riff_size)
    } else {
        None
    };
    if is_rf64 {
        if declared_riff_size != u64::from(u32::MAX) {
            builder.malformed(
                "REGISTRY-WAVE-RF64-RIFF-SIZE",
                "RF64/BW64 RIFF size field must be 0xffffffff",
                Some(4),
                Some(4),
            );
        }
    } else if declared_riff_size == u64::from(u32::MAX) {
        builder.malformed(
            "REGISTRY-WAVE-RIFF-SIZE",
            "RIFF/WAVE size field cannot be 0xffffffff",
            Some(4),
            Some(4),
        );
    } else if let Some(end) = declared_container_end {
        if end != source_len {
            builder.malformed(
                "REGISTRY-WAVE-RIFF-BOUNDS",
                "RIFF/WAVE declared container size differs from the source length",
                Some(4),
                Some(declared_riff_size),
            );
        }
    } else {
        builder.malformed(
            "REGISTRY-WAVE-RIFF-SIZE",
            "RIFF/WAVE declared container size overflows",
            Some(4),
            Some(declared_riff_size),
        );
    }
    let mut offset = 12_u64;
    let mut ds64_data_size = None;
    let mut ds64_riff_size = None;
    let mut ds64_table = HashMap::<[u8; 4], Vec<u64>>::new();
    let mut ds64_table_uses = HashMap::<[u8; 4], usize>::new();
    let mut saw_ds64 = false;
    let mut first_chunk = true;
    let mut structural_counts = HashMap::<[u8; 4], usize>::new();
    let mut saw_data = false;
    while offset < source_len {
        if source_len - offset < 8 {
            builder.malformed(
                "REGISTRY-WAVE-CHUNK-HEADER",
                "truncated WAVE chunk header",
                Some(offset),
                Some(source_len - offset),
            );
            break;
        }
        let mut header = [0_u8; 8];
        builder.source.read_exact_at(offset, &mut header)?;
        let id: [u8; 4] = header[..4].try_into().unwrap();
        let declared_size = u32::from_le_bytes(header[4..].try_into().unwrap()) as u64;
        if first_chunk && is_rf64 && id != *b"ds64" {
            builder.malformed(
                "REGISTRY-WAVE-RF64-DS64-FIRST",
                "RF64/BW64 must place ds64 immediately after the WAVE header",
                Some(offset),
                Some(8),
            );
        }
        first_chunk = false;
        let body_offset = offset + 8;
        let body_size = if declared_size == u64::from(u32::MAX) {
            let resolved = if id == *b"data" {
                ds64_data_size
            } else {
                let used = ds64_table_uses.entry(id).or_default();
                let value = ds64_table.get(&id).and_then(|values| values.get(*used));
                if value.is_some() {
                    *used += 1;
                }
                value.copied()
            };
            match resolved {
                Some(size) => size,
                None => {
                    builder.malformed(
                        "REGISTRY-WAVE-RF64-SIZE-TABLE",
                        "WAVE chunk uses 0xffffffff without a matching ds64 size entry",
                        Some(offset),
                        Some(8),
                    );
                    break;
                }
            }
        } else {
            declared_size
        };
        let padding = body_size & 1;
        let end = match body_offset
            .checked_add(body_size)
            .and_then(|value| value.checked_add(padding))
        {
            Some(end) if end <= source_len => end,
            _ => {
                builder.malformed(
                    "REGISTRY-WAVE-CHUNK-BOUNDS",
                    "WAVE chunk body or padding exceeds the source",
                    Some(offset),
                    Some(body_size),
                );
                break;
            }
        };
        if matches!(&id, b"fmt " | b"fact" | b"data" | b"ds64") {
            let count = structural_counts.entry(id).or_default();
            *count += 1;
            if *count > 1 {
                builder.malformed(
                    "REGISTRY-WAVE-DUPLICATE-STRUCTURAL",
                    "WAVE structural chunk occurs more than once",
                    Some(offset),
                    Some(end - offset),
                );
            }
        }
        if id == *b"ds64" && is_rf64 {
            saw_ds64 = true;
            if body_size < 28 {
                builder.malformed(
                    "REGISTRY-WAVE-DS64-SIZE",
                    "RF64/BW64 ds64 chunk is shorter than its fixed fields",
                    Some(body_offset),
                    Some(body_size),
                );
            } else {
                let mut riff_size = [0_u8; 8];
                let mut data_size = [0_u8; 8];
                let mut table_length = [0_u8; 4];
                builder.source.read_exact_at(body_offset, &mut riff_size)?;
                builder
                    .source
                    .read_exact_at(body_offset + 8, &mut data_size)?;
                builder
                    .source
                    .read_exact_at(body_offset + 24, &mut table_length)?;
                ds64_riff_size = Some(u64::from_le_bytes(riff_size));
                ds64_data_size = Some(u64::from_le_bytes(data_size));
                let table_count = u64::from(u32::from_le_bytes(table_length));
                let required = table_count
                    .checked_mul(12)
                    .and_then(|value| value.checked_add(28));
                if required.is_none_or(|required| required > body_size) {
                    builder.malformed(
                        "REGISTRY-WAVE-DS64-TABLE",
                        "RF64/BW64 ds64 size table exceeds its chunk",
                        Some(body_offset + 24),
                        Some(table_count),
                    );
                } else if table_count > builder.limits.max_metadata_entries as u64 {
                    builder.issue(
                        IssueKind::BudgetExceeded,
                        "REGISTRY-WAVE-DS64-TABLE",
                        format!(
                            "RF64/BW64 ds64 table has {table_count} entries, above configured limit {}",
                            builder.limits.max_metadata_entries
                        ),
                        Some(body_offset + 24),
                        Some(table_count),
                    );
                } else {
                    if required != Some(body_size) {
                        builder.malformed(
                            "REGISTRY-WAVE-DS64-TRAILING",
                            "RF64/BW64 ds64 chunk has bytes beyond its declared size table",
                            Some(body_offset + required.unwrap_or(body_size)),
                            Some(body_size.saturating_sub(required.unwrap_or(body_size))),
                        );
                    }
                    for index in 0..table_count {
                        let entry_offset = body_offset + 28 + index * 12;
                        let mut entry = [0_u8; 12];
                        builder.source.read_exact_at(entry_offset, &mut entry)?;
                        ds64_table
                            .entry(entry[..4].try_into().unwrap())
                            .or_default()
                            .push(u64::from_le_bytes(entry[4..12].try_into().unwrap()));
                    }
                }
                // RF64's 64-bit riffSize governs all chunks after ds64.  Set
                // the bound as soon as the mandatory first chunk is parsed so
                // a later chunk cannot silently escape it.
                declared_container_end = ds64_riff_size.and_then(|size| size.checked_add(8));
            }
        }
        if let Some(container_end) = declared_container_end {
            if end > container_end {
                builder.malformed(
                    "REGISTRY-WAVE-RIFF-CHUNK-BOUNDS",
                    "WAVE chunk extends beyond the declared RIFF container",
                    Some(offset),
                    Some(end - offset),
                );
            }
        }
        let audio_data = id == *b"data";
        let physical_size = end - offset;
        let path = vec!["RIFF".into(), "WAVE".into(), fourcc(id)];
        let extents = vec![ByteExtent {
            offset,
            length: physical_size,
        }];
        if matches!(&id, b"fmt " | b"fact" | b"data" | b"ds64") {
            builder.add_structural(
                path,
                StructuralKind::WaveChunk {
                    id: fourcc(id),
                    audio_data,
                },
                extents,
            );
        } else {
            builder.add_region(
                path,
                MetadataKind::WaveChunk {
                    id: fourcc(id),
                    audio_data: false,
                },
                extents,
                true,
                true,
            )?;
        }
        saw_data |= audio_data;
        offset = end;
    }
    if !saw_data {
        builder.malformed(
            "REGISTRY-WAVE-MISSING-DATA",
            "WAVE source has no data chunk",
            None,
            None,
        );
    }
    if is_rf64 && !saw_ds64 {
        builder.malformed(
            "REGISTRY-WAVE-RF64-MISSING-DS64",
            "RF64/BW64 source has no ds64 chunk",
            None,
            None,
        );
    }
    if let Some(riff_size) = ds64_riff_size {
        match riff_size.checked_add(8) {
            Some(end) if end == source_len => {}
            Some(end) => builder.malformed(
                "REGISTRY-WAVE-DS64-RIFF-BOUNDS",
                "ds64 riffSize differs from the source length",
                Some(12),
                Some(end),
            ),
            None => builder.malformed(
                "REGISTRY-WAVE-DS64-RIFF-BOUNDS",
                "ds64 riffSize overflows",
                Some(12),
                Some(riff_size),
            ),
        }
    }
    Ok(())
}

fn discover_flac(builder: &mut InventoryBuilder<'_, '_>) -> Result<(), MetadataRegistryError> {
    let source_len = builder.source.len();
    let mut offset = 4_u64;
    let mut saw_last = false;
    while offset < source_len {
        if source_len - offset < 4 {
            builder.malformed(
                "REGISTRY-FLAC-BLOCK-HEADER",
                "truncated FLAC metadata block header",
                Some(offset),
                Some(source_len - offset),
            );
            break;
        }
        let mut header = [0_u8; 4];
        builder.source.read_exact_at(offset, &mut header)?;
        let last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let body_size = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        let body_offset = offset + 4;
        let end = match body_offset.checked_add(body_size) {
            Some(end) if end <= source_len => end,
            _ => {
                builder.malformed(
                    "REGISTRY-FLAC-BLOCK-BOUNDS",
                    "FLAC metadata block exceeds the source",
                    Some(offset),
                    Some(body_size),
                );
                break;
            }
        };
        if block_type == 127 {
            builder.malformed(
                "REGISTRY-FLAC-RESERVED-BLOCK",
                "FLAC metadata block type 127 is reserved",
                Some(offset),
                Some(end - offset),
            );
        }
        if block_type == 0 && body_size != 34 {
            builder.malformed(
                "REGISTRY-FLAC-STREAMINFO-SIZE",
                "FLAC STREAMINFO block must contain exactly 34 bytes",
                Some(body_offset),
                Some(body_size),
            );
        }
        let path = vec!["fLaC".into(), format!("block:{block_type}")];
        let extents = vec![ByteExtent {
            offset,
            length: end - offset,
        }];
        if block_type == 0 {
            builder.add_structural(path, StructuralKind::FlacBlock { block_type }, extents);
        } else {
            builder.add_region(
                path,
                MetadataKind::FlacBlock { block_type },
                extents,
                true,
                true,
            )?;
        }
        offset = end;
        if last {
            saw_last = true;
            break;
        }
    }
    if !saw_last {
        builder.malformed(
            "REGISTRY-FLAC-MISSING-LAST",
            "FLAC metadata chain has no last-block flag",
            None,
            None,
        );
    } else if offset < source_len {
        // The bytes after the last metadata block are the encoded audio frame
        // stream and are intentionally not inventory regions.
        let _ = source_len - offset;
    }
    Ok(())
}

fn discover_mp3(builder: &mut InventoryBuilder<'_, '_>) -> Result<(), MetadataRegistryError> {
    let source_len = builder.source.len();
    let mut cursor = 0_u64;
    // Some encoders put an APEv2 header before ID3v2, and concatenated ID3
    // tags are legal in the wild.  Walk only the unambiguously contiguous
    // leading tag chain; once an audio byte is encountered, do not scan its
    // payload for incidental signatures.
    loop {
        let old_cursor = cursor;
        if source_len - cursor >= 8 && source_starts_with(builder.source, cursor, b"APETAGEX")? {
            if let Some(end) = parse_ape_header_at(builder, cursor)? {
                cursor = end;
            }
        } else if source_len - cursor >= 3 && source_starts_with(builder.source, cursor, b"ID3")? {
            if let Some(end) = parse_id3v2_at(builder, cursor)? {
                cursor = end;
            }
        }
        if cursor == old_cursor {
            break;
        }
    }

    let id3v1_start =
        if source_len >= 128 && source_starts_with(builder.source, source_len - 128, b"TAG")? {
            Some(source_len - 128)
        } else {
            None
        };
    if let Some(start) = id3v1_start {
        builder.add_region(
            vec!["MPEG".into(), "ID3v1".into()],
            MetadataKind::Id3v1Tag,
            vec![ByteExtent {
                offset: start,
                length: 128,
            }],
            true,
            true,
        )?;
    }

    // APEv2 footers are discovered from the end, allowing ID3v1 immediately
    // after the APE tag and also exposing multiple adjacent APE occurrences.
    let mut tail_end = id3v1_start.unwrap_or(source_len);
    while tail_end >= 32 {
        let footer_start = tail_end - 32;
        if !source_starts_with(builder.source, footer_start, b"APETAGEX")? {
            break;
        }
        let Some(tag_start) = parse_ape_footer_at(builder, footer_start, tail_end)? else {
            break;
        };
        if tag_start >= tail_end {
            break;
        }
        tail_end = tag_start;
    }
    Ok(())
}

fn source_starts_with(
    source: &mut InputSource<'_>,
    offset: u64,
    expected: &[u8],
) -> Result<bool, MetadataRegistryError> {
    if expected.len() as u64 > source.len().saturating_sub(offset) {
        return Ok(false);
    }
    let mut bytes = vec![0_u8; expected.len()];
    source.read_exact_at(offset, &mut bytes)?;
    Ok(bytes == expected)
}

fn parse_id3v2_at(
    builder: &mut InventoryBuilder<'_, '_>,
    start: u64,
) -> Result<Option<u64>, MetadataRegistryError> {
    let source_len = builder.source.len();
    if source_len - start < 10 {
        builder.malformed(
            "REGISTRY-ID3-HEADER",
            "truncated ID3v2 header",
            Some(start),
            Some(source_len - start),
        );
        return Ok(None);
    }
    let mut header = [0_u8; 10];
    builder.source.read_exact_at(start, &mut header)?;
    if &header[..3] != b"ID3" {
        return Ok(None);
    }
    let major = header[3];
    let revision = header[4];
    let flags = header[5];
    if !(2..=4).contains(&major) || revision == 0xff {
        builder.malformed(
            "REGISTRY-ID3-VERSION",
            "unsupported ID3v2 version",
            Some(start),
            Some(10),
        );
        return Ok(None);
    }
    let Some(body_size) = syncsafe(header[6..10].try_into().unwrap()) else {
        builder.malformed(
            "REGISTRY-ID3-SYNCHSAFE",
            "ID3v2 size is not a valid syncsafe integer",
            Some(start + 6),
            Some(4),
        );
        return Ok(None);
    };
    let footer_size = u64::from(major == 4 && flags & 0x10 != 0) * 10;
    let total_size = match 10_u64
        .checked_add(body_size)
        .and_then(|value| value.checked_add(footer_size))
    {
        Some(value) => value,
        None => {
            builder.malformed(
                "REGISTRY-ID3-SIZE",
                "ID3v2 size overflows the source address space",
                Some(start),
                None,
            );
            return Ok(None);
        }
    };
    let end = match start.checked_add(total_size) {
        Some(end) if end <= source_len => end,
        _ => {
            builder.malformed(
                "REGISTRY-ID3-BOUNDS",
                "ID3v2 tag exceeds the source",
                Some(start),
                Some(total_size),
            );
            return Ok(None);
        }
    };
    if footer_size != 0 {
        let mut footer = [0_u8; 10];
        builder.source.read_exact_at(end - 10, &mut footer)?;
        if &footer[..3] != b"3DI" || footer[3..] != header[3..] {
            builder.malformed(
                "REGISTRY-ID3-FOOTER",
                "ID3v2 footer does not match its header",
                Some(end - 10),
                Some(10),
            );
        }
    }
    builder.add_region(
        vec!["MPEG".into(), format!("ID3v2.{major}.{revision}")],
        MetadataKind::Id3v2Tag,
        vec![ByteExtent {
            offset: start,
            length: total_size,
        }],
        true,
        true,
    )?;

    let body_start = start + 10;
    let body_end = body_start + body_size;
    let mut frame_offset = body_start;
    if flags & 0x40 != 0 {
        let extended_size = match major {
            3 if body_end - frame_offset >= 4 => {
                let mut value = [0_u8; 4];
                builder.source.read_exact_at(frame_offset, &mut value)?;
                u64::from(u32::from_be_bytes(value)).checked_add(4)
            }
            4 if body_end - frame_offset >= 4 => {
                let mut value = [0_u8; 4];
                builder.source.read_exact_at(frame_offset, &mut value)?;
                syncsafe(value)
            }
            _ => None,
        };
        if let Some(size) = extended_size {
            if size <= body_end - frame_offset {
                frame_offset += size;
            } else {
                builder.malformed(
                    "REGISTRY-ID3-EXTENDED-HEADER",
                    "ID3v2 extended header exceeds the tag body",
                    Some(frame_offset),
                    Some(size),
                );
                return Ok(Some(end));
            }
        } else {
            builder.malformed(
                "REGISTRY-ID3-EXTENDED-HEADER",
                "ID3v2 extended header is truncated or invalid",
                Some(frame_offset),
                None,
            );
            return Ok(Some(end));
        }
    }
    // A compressed v2.2 tag cannot be safely split into physical frames from
    // its header alone.  The complete tag region remains inventoried.
    if major == 2 && flags & 0x40 != 0 {
        builder.malformed(
            "REGISTRY-ID3-FRAMES-SKIPPED",
            "ID3v2.2 compressed tag prevents bounded frame discovery",
            Some(frame_offset),
            Some(body_end - frame_offset),
        );
        return Ok(Some(end));
    }
    let frame_header_size = if major == 2 { 6_u64 } else { 10_u64 };
    while frame_offset < body_end {
        if body_end - frame_offset < frame_header_size {
            if !region_is_zero(builder.source, frame_offset, body_end)? {
                builder.malformed(
                    "REGISTRY-ID3-PADDING",
                    "ID3v2 trailing bytes are not valid zero padding",
                    Some(frame_offset),
                    Some(body_end - frame_offset),
                );
            }
            break;
        }
        let mut frame_header = [0_u8; 10];
        builder.source.read_exact_at(
            frame_offset,
            &mut frame_header[..frame_header_size as usize],
        )?;
        if frame_header[..frame_header_size as usize]
            .iter()
            .all(|byte| *byte == 0)
        {
            if !region_is_zero(builder.source, frame_offset, body_end)? {
                builder.malformed(
                    "REGISTRY-ID3-PADDING",
                    "ID3v2 frame padding contains non-zero bytes",
                    Some(frame_offset),
                    Some(body_end - frame_offset),
                );
            }
            break;
        }
        let id_len = if major == 2 { 3 } else { 4 };
        if !frame_header[..id_len]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            builder.malformed(
                "REGISTRY-ID3-FRAME-ID",
                "ID3v2 frame identifier is invalid",
                Some(frame_offset),
                Some(frame_header_size),
            );
            break;
        }
        let frame_size = if major == 2 {
            u64::from(u32::from_be_bytes([
                0,
                frame_header[3],
                frame_header[4],
                frame_header[5],
            ]))
        } else if major == 4 {
            let Some(size) = syncsafe(frame_header[4..8].try_into().unwrap()) else {
                builder.malformed(
                    "REGISTRY-ID3-FRAME-SIZE",
                    "ID3v2.4 frame size is not syncsafe",
                    Some(frame_offset + 4),
                    Some(4),
                );
                break;
            };
            size
        } else {
            u64::from(u32::from_be_bytes(frame_header[4..8].try_into().unwrap()))
        };
        let next = match frame_offset
            .checked_add(frame_header_size)
            .and_then(|value| value.checked_add(frame_size))
        {
            Some(next) if next <= body_end && frame_size != 0 => next,
            _ => {
                builder.malformed(
                    "REGISTRY-ID3-FRAME-BOUNDS",
                    "ID3v2 frame exceeds its tag body or has zero size",
                    Some(frame_offset),
                    Some(frame_size),
                );
                break;
            }
        };
        let frame_id = bytes_identifier(&frame_header[..id_len]);
        builder.add_region(
            vec![
                "MPEG".into(),
                format!("ID3v2.{major}.{revision}"),
                frame_id.clone(),
            ],
            MetadataKind::Id3v2Frame { id: frame_id },
            vec![ByteExtent {
                offset: frame_offset,
                length: next - frame_offset,
            }],
            true,
            true,
        )?;
        frame_offset = next;
    }
    Ok(Some(end))
}

fn parse_ape_header_at(
    builder: &mut InventoryBuilder<'_, '_>,
    start: u64,
) -> Result<Option<u64>, MetadataRegistryError> {
    let source_len = builder.source.len();
    if source_len - start < 32 {
        builder.malformed(
            "REGISTRY-APE-HEADER",
            "truncated APEv2 header",
            Some(start),
            Some(source_len - start),
        );
        return Ok(None);
    }
    let descriptor = read_ape_descriptor(builder.source, start)?;
    if descriptor.version != 2000 || !descriptor.is_header {
        builder.malformed(
            "REGISTRY-APE-HEADER",
            "leading APE descriptor is not a version 2 header",
            Some(start),
            Some(32),
        );
        return Ok(None);
    }
    let Some(total) = descriptor.declared_size.checked_add(32) else {
        builder.malformed(
            "REGISTRY-APE-SIZE",
            "APEv2 header size overflows",
            Some(start),
            None,
        );
        return Ok(None);
    };
    let Some(end) = start.checked_add(total) else {
        builder.malformed(
            "REGISTRY-APE-SIZE",
            "APEv2 header end overflows",
            Some(start),
            Some(total),
        );
        return Ok(None);
    };
    if end > source_len || descriptor.declared_size < 32 {
        builder.malformed(
            "REGISTRY-APE-BOUNDS",
            "APEv2 header/footer exceeds the source",
            Some(start),
            Some(total),
        );
        return Ok(None);
    }
    let footer_start = end - 32;
    let footer = read_ape_descriptor(builder.source, footer_start)?;
    if !ape_descriptors_match(descriptor, footer) || footer.is_header {
        builder.malformed(
            "REGISTRY-APE-HEADER-FOOTER",
            "APEv2 header/footer descriptors do not match",
            Some(footer_start),
            Some(32),
        );
    }
    add_ape_region_and_items(builder, start, end, footer_start, descriptor, true)?;
    Ok(Some(end))
}

fn parse_ape_footer_at(
    builder: &mut InventoryBuilder<'_, '_>,
    footer_start: u64,
    footer_end: u64,
) -> Result<Option<u64>, MetadataRegistryError> {
    let footer = read_ape_descriptor(builder.source, footer_start)?;
    if footer.version != 1000 && footer.version != 2000 {
        builder.malformed(
            "REGISTRY-APE-VERSION",
            "trailing APE descriptor has an unsupported version",
            Some(footer_start),
            Some(32),
        );
        return Ok(None);
    }
    if footer.is_header {
        builder.malformed(
            "REGISTRY-APE-FOOTER",
            "trailing APE descriptor is marked as a header",
            Some(footer_start),
            Some(32),
        );
        return Ok(None);
    }
    if footer.declared_size < 32 {
        builder.malformed(
            "REGISTRY-APE-SIZE",
            "APEv2 footer declares a size smaller than its descriptor",
            Some(footer_start),
            Some(footer.declared_size),
        );
        return Ok(None);
    }
    let header_bytes = u64::from(footer.version == 2000 && footer.has_header);
    let physical_size = match footer.declared_size.checked_add(header_bytes * 32) {
        Some(size) => size,
        None => {
            builder.malformed(
                "REGISTRY-APE-SIZE",
                "APEv2 footer size overflows",
                Some(footer_start),
                None,
            );
            return Ok(None);
        }
    };
    let Some(tag_start) = footer_end.checked_sub(physical_size) else {
        builder.malformed(
            "REGISTRY-APE-BOUNDS",
            "APEv2 footer points before the source",
            Some(footer_start),
            Some(physical_size),
        );
        return Ok(None);
    };
    if tag_start > footer_start {
        builder.malformed(
            "REGISTRY-APE-BOUNDS",
            "APEv2 item area is outside the footer",
            Some(tag_start),
            Some(physical_size),
        );
        return Ok(None);
    }
    let descriptor = if header_bytes != 0 {
        let header = read_ape_descriptor(builder.source, tag_start)?;
        if !header.is_header || !ape_descriptors_match(header, footer) {
            builder.malformed(
                "REGISTRY-APE-HEADER-FOOTER",
                "APEv2 header/footer descriptors do not match",
                Some(tag_start),
                Some(32),
            );
        }
        header
    } else {
        footer
    };
    add_ape_region_and_items(
        builder,
        tag_start,
        footer_end,
        footer_start,
        descriptor,
        header_bytes != 0,
    )?;
    Ok(Some(tag_start))
}

#[derive(Clone, Copy)]
struct ApeDescriptor {
    version: u32,
    declared_size: u64,
    item_count: u32,
    has_header: bool,
    has_footer: bool,
    is_header: bool,
    flags: u32,
}

fn read_ape_descriptor(
    source: &mut InputSource<'_>,
    offset: u64,
) -> Result<ApeDescriptor, MetadataRegistryError> {
    let mut raw = [0_u8; 32];
    source.read_exact_at(offset, &mut raw)?;
    let version = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    let flags = u32::from_le_bytes(raw[20..24].try_into().unwrap());
    Ok(ApeDescriptor {
        version,
        declared_size: u64::from(u32::from_le_bytes(raw[12..16].try_into().unwrap())),
        item_count: u32::from_le_bytes(raw[16..20].try_into().unwrap()),
        has_header: version == 2000 && flags & 0x8000_0000 != 0,
        has_footer: version == 2000 && flags & 0x4000_0000 != 0,
        is_header: version == 2000 && flags & 0x2000_0000 != 0,
        flags,
    })
}

fn ape_descriptors_match(left: ApeDescriptor, right: ApeDescriptor) -> bool {
    left.version == right.version
        && left.declared_size == right.declared_size
        && left.item_count == right.item_count
        && left.has_header == right.has_header
        && left.has_footer == right.has_footer
        && left.is_header != right.is_header
        && (left.flags & 0xd000_0000) == (right.flags & 0xd000_0000)
}

fn add_ape_region_and_items(
    builder: &mut InventoryBuilder<'_, '_>,
    tag_start: u64,
    tag_end: u64,
    footer_start: u64,
    descriptor: ApeDescriptor,
    has_header: bool,
) -> Result<(), MetadataRegistryError> {
    // A valid header+footer tag encountered by both the leading and trailing
    // walks is one physical occurrence, not two logical discoveries.  Avoid
    // duplicating its tag and item regions while still preserving genuinely
    // repeated tags at different offsets.
    if builder.has_region_extent(tag_start, tag_end - tag_start) {
        return Ok(());
    }
    if descriptor.item_count as usize > builder.limits.max_metadata_entries {
        builder.issue(
            IssueKind::BudgetExceeded,
            "REGISTRY-APE-ITEM-COUNT",
            format!(
                "APEv2 item count {} exceeds configured entry limit {}",
                descriptor.item_count, builder.limits.max_metadata_entries
            ),
            Some(tag_start),
            Some(tag_end - tag_start),
        );
    }
    builder.add_region(
        vec!["MPEG".into(), "APEv2".into()],
        MetadataKind::Apev2Tag,
        vec![ByteExtent {
            offset: tag_start,
            length: tag_end - tag_start,
        }],
        true,
        true,
    )?;
    let mut cursor = if has_header {
        tag_start + 32
    } else {
        tag_start
    };
    let item_count = usize::try_from(descriptor.item_count).unwrap_or(usize::MAX);
    let item_count = item_count.min(builder.limits.max_metadata_entries);
    for _ in 0..item_count {
        if cursor > footer_start || footer_start - cursor < 8 {
            builder.malformed(
                "REGISTRY-APE-ITEM-HEADER",
                "APEv2 item header is truncated",
                Some(cursor),
                Some(footer_start.saturating_sub(cursor)),
            );
            return Ok(());
        }
        let mut fields = [0_u8; 8];
        builder.source.read_exact_at(cursor, &mut fields)?;
        let value_size = u64::from(u32::from_le_bytes(fields[..4].try_into().unwrap()));
        let key_start = cursor + 8;
        let mut key_end = key_start;
        while key_end < footer_start
            && key_end - key_start <= 255
            && builder.source.read_u8(key_end)? != 0
        {
            key_end += 1;
        }
        if key_end >= footer_start || key_end - key_start < 2 || key_end - key_start > 255 {
            builder.malformed(
                "REGISTRY-APE-ITEM-KEY",
                "APEv2 item key is missing, too short, or unterminated",
                Some(key_start),
                Some(footer_start.saturating_sub(key_start)),
            );
            return Ok(());
        }
        let value_start = key_end + 1;
        let value_end = match value_start.checked_add(value_size) {
            Some(end) if end <= footer_start => end,
            _ => {
                builder.malformed(
                    "REGISTRY-APE-ITEM-VALUE",
                    "APEv2 item value exceeds the item table",
                    Some(value_start),
                    Some(value_size),
                );
                return Ok(());
            }
        };
        let mut key = Vec::with_capacity((key_end - key_start) as usize);
        let mut key_offset = key_start;
        while key_offset < key_end {
            key.push(builder.source.read_u8(key_offset)?);
            key_offset += 1;
        }
        let key = bytes_identifier(&key);
        builder.add_region(
            vec!["MPEG".into(), "APEv2".into(), key.clone()],
            MetadataKind::Apev2Item { key },
            vec![ByteExtent {
                offset: cursor,
                length: value_end - cursor,
            }],
            true,
            true,
        )?;
        cursor = value_end;
    }
    if cursor != footer_start {
        builder.malformed(
            "REGISTRY-APE-ITEM-TABLE",
            "APEv2 item table does not exactly reach its footer",
            Some(cursor),
            Some(footer_start.saturating_sub(cursor)),
        );
    }
    Ok(())
}

fn syncsafe(bytes: [u8; 4]) -> Option<u64> {
    bytes.iter().all(|byte| byte & 0x80 == 0).then(|| {
        bytes
            .into_iter()
            .fold(0_u64, |value, byte| (value << 7) | u64::from(byte))
    })
}

fn region_is_zero(
    source: &mut InputSource<'_>,
    start: u64,
    end: u64,
) -> Result<bool, MetadataRegistryError> {
    let mut offset = start;
    let mut buffer = [0_u8; 32 * 1024];
    while offset < end {
        let count = usize::try_from((end - offset).min(buffer.len() as u64)).unwrap();
        source.read_exact_at(offset, &mut buffer[..count])?;
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        offset += count as u64;
    }
    Ok(true)
}

fn bytes_identifier(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte).to_string()
            } else {
                format!("\\x{byte:02x}")
            }
        })
        .collect()
}

#[derive(Default)]
struct OggStreamState {
    packet_index: u32,
    pending: Vec<ByteExtent>,
    pending_size: u64,
    packet_discarded: bool,
    next_sequence: Option<u32>,
    saw_eos: bool,
    codec: Option<ContainerKind>,
}

struct OggCompletedPacket {
    serial: u32,
    packet_index: u32,
    extents: Vec<ByteExtent>,
}

fn discover_ogg(builder: &mut InventoryBuilder<'_, '_>) -> Result<(), MetadataRegistryError> {
    let source_len = builder.source.len();
    let mut offset = 0_u64;
    let mut streams = HashMap::<u32, OggStreamState>::new();
    let mut codec_set = Vec::<ContainerKind>::new();
    let mut page_count = 0_usize;
    let mut page_budget_exhausted = false;
    let mut stream_budget_exhausted = false;
    while offset < source_len {
        if source_len - offset < 27 {
            builder.malformed(
                "REGISTRY-OGG-PAGE-HEADER",
                "truncated Ogg page header",
                Some(offset),
                Some(source_len - offset),
            );
            break;
        }
        let mut header = [0_u8; 27];
        builder.source.read_exact_at(offset, &mut header)?;
        if &header[..4] != b"OggS" {
            builder.malformed(
                "REGISTRY-OGG-CAPTURE",
                "Ogg page capture pattern is invalid",
                Some(offset),
                Some(4),
            );
            break;
        }
        let invalid_version = header[4] != 0;
        let header_type = header[5];
        let serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        let sequence = u32::from_le_bytes(header[18..22].try_into().unwrap());
        let segment_count = u64::from(header[26]);
        let lacing_offset = offset + 27;
        let body_offset = lacing_offset + segment_count;
        if source_len < body_offset {
            builder.malformed(
                "REGISTRY-OGG-LACING",
                "Ogg page lacing table exceeds the source",
                Some(lacing_offset),
                Some(segment_count),
            );
            break;
        }
        let mut lacing = vec![0_u8; segment_count as usize];
        builder.source.read_exact_at(lacing_offset, &mut lacing)?;
        let body_size = lacing
            .iter()
            .try_fold(0_u64, |size, segment| size.checked_add(u64::from(*segment)));
        let Some(body_size) = body_size else {
            builder.malformed(
                "REGISTRY-OGG-PAGE-SIZE",
                "Ogg page body size overflows",
                Some(offset),
                None,
            );
            break;
        };
        let page_size = match 27_u64
            .checked_add(segment_count)
            .and_then(|value| value.checked_add(body_size))
        {
            Some(size) => size,
            None => {
                builder.malformed(
                    "REGISTRY-OGG-PAGE-SIZE",
                    "Ogg page size overflows",
                    Some(offset),
                    None,
                );
                break;
            }
        };
        let page_end = match offset.checked_add(page_size) {
            Some(end) if end <= source_len => end,
            _ => {
                builder.malformed(
                    "REGISTRY-OGG-PAGE-BOUNDS",
                    "Ogg page body exceeds the source",
                    Some(offset),
                    Some(page_size),
                );
                break;
            }
        };
        if page_size > builder.limits.max_ogg_page_bytes {
            builder.issue(
                IssueKind::BudgetExceeded,
                "REGISTRY-OGG-MAX-PAGE",
                format!(
                    "Ogg page contains {page_size} bytes, above configured limit {}",
                    builder.limits.max_ogg_page_bytes
                ),
                Some(offset),
                Some(page_size),
            );
        }
        if page_count < builder.limits.max_metadata_entries {
            builder.add_structural(
                vec![
                    "Ogg".into(),
                    format!("serial:{serial}"),
                    format!("page:{sequence}"),
                ],
                StructuralKind::OggPage { serial, sequence },
                vec![ByteExtent {
                    offset,
                    length: page_size,
                }],
            );
            page_count += 1;
        } else {
            if !page_budget_exhausted {
                builder.issue(
                    IssueKind::BudgetExceeded,
                    "REGISTRY-OGG-MAX-PAGES",
                    format!(
                        "Ogg page count exceeds configured entry limit {}; later page structure is aggregated",
                        builder.limits.max_metadata_entries
                    ),
                    Some(offset),
                    Some(page_size),
                );
                page_budget_exhausted = true;
            }
            // Page bounds and lacing have already been checked.  Continue
            // walking physical pages without allocating stream state or
            // packet extents beyond the bounded inventory window.
            offset = page_end;
            continue;
        }
        if invalid_version {
            builder.malformed(
                "REGISTRY-OGG-VERSION",
                "Ogg page version is not zero",
                Some(offset + 4),
                Some(1),
            );
        }

        if header_type & 0x02 != 0 {
            // A serial can be reused for a later sequential logical stream;
            // BOS starts a fresh occurrence and must not inherit lacing state.
            if streams.get(&serial).is_some_and(|state| {
                state.packet_index != 0
                    || !state.pending.is_empty()
                    || state.packet_discarded
                    || state.saw_eos
            }) {
                streams.insert(serial, OggStreamState::default());
            }
        }
        if !streams.contains_key(&serial) && streams.len() >= builder.limits.max_metadata_entries {
            if !stream_budget_exhausted {
                builder.issue(
                    IssueKind::BudgetExceeded,
                    "REGISTRY-OGG-MAX-STREAMS",
                    format!(
                        "Ogg logical stream count exceeds configured entry limit {}; later serials are aggregated",
                        builder.limits.max_metadata_entries
                    ),
                    Some(offset + 14),
                    Some(4),
                );
                stream_budget_exhausted = true;
            }
            offset = page_end;
            continue;
        }
        let state = streams.entry(serial).or_default();
        if let Some(expected) = state.next_sequence {
            if expected != sequence {
                builder.malformed(
                    "REGISTRY-OGG-SEQUENCE",
                    "Ogg page sequence number is not contiguous for its serial",
                    Some(offset + 18),
                    Some(4),
                );
            }
        }
        state.next_sequence = Some(sequence.wrapping_add(1));
        let continued = header_type & 0x01 != 0;
        if continued && state.pending.is_empty() && !state.packet_discarded {
            builder.malformed(
                "REGISTRY-OGG-CONTINUATION",
                "Ogg continuation page has no pending packet",
                Some(offset),
                Some(page_size),
            );
        } else if !continued && (!state.pending.is_empty() || state.packet_discarded) {
            builder.malformed(
                "REGISTRY-OGG-CONTINUATION",
                "Ogg page starts a new packet while the previous packet is incomplete",
                Some(offset),
                Some(page_size),
            );
            state.pending.clear();
            state.pending_size = 0;
            state.packet_discarded = false;
        }
        let mut body_cursor = body_offset;
        let mut completed = Vec::new();
        for segment in lacing {
            let segment_length = u64::from(segment);
            // Once a packet has exceeded the configured bound, retain only
            // its completion state. Keeping one extent per later lacing
            // segment would let an unterminated packet grow `pending` with
            // the entire source despite the packet limit.
            if !state.packet_discarded && segment_length != 0 {
                if state.pending.len() >= builder.limits.max_metadata_entries {
                    builder.issue(
                        IssueKind::BudgetExceeded,
                        "REGISTRY-OGG-MAX-PACKET-EXTENTS",
                        format!(
                            "Ogg packet extent count exceeds configured entry limit {}",
                            builder.limits.max_metadata_entries
                        ),
                        Some(body_cursor),
                        Some(segment_length),
                    );
                    state.pending.clear();
                    state.packet_discarded = true;
                } else {
                    state.pending.push(ByteExtent {
                        offset: body_cursor,
                        length: segment_length,
                    });
                }
            }
            body_cursor += segment_length;
            state.pending_size = state.pending_size.saturating_add(segment_length);
            if state.pending_size > builder.limits.max_ogg_packet_bytes {
                if !state.packet_discarded {
                    builder.issue(
                        IssueKind::BudgetExceeded,
                        "REGISTRY-OGG-MAX-PACKET",
                        format!(
                            "Ogg packet exceeds configured limit {}",
                            builder.limits.max_ogg_packet_bytes
                        ),
                        Some(state.pending.first().map_or(offset, |extent| extent.offset)),
                        Some(state.pending_size),
                    );
                }
                state.pending.clear();
                state.packet_discarded = true;
            }
            if segment != 255 {
                let packet_extents = std::mem::take(&mut state.pending);
                let packet_size = state.pending_size;
                state.pending_size = 0;
                let packet_discarded = std::mem::replace(&mut state.packet_discarded, false);
                if !packet_discarded && (!packet_extents.is_empty() || packet_size == 0) {
                    completed.push(OggCompletedPacket {
                        serial,
                        packet_index: state.packet_index,
                        extents: packet_extents,
                    });
                    state.packet_index = state.packet_index.saturating_add(1);
                } else if packet_discarded {
                    // Preserve packet numbering after a bounded discard so
                    // later comment/header packets retain their physical
                    // occurrence identity.
                    state.packet_index = state.packet_index.saturating_add(1);
                }
            }
        }
        if header_type & 0x04 != 0 {
            state.saw_eos = true;
            if !state.pending.is_empty() || state.packet_discarded {
                builder.malformed(
                    "REGISTRY-OGG-EOS",
                    "Ogg EOS page ends with an incomplete packet",
                    Some(offset),
                    Some(page_size),
                );
                state.pending.clear();
                state.pending_size = 0;
                state.packet_discarded = false;
            }
        }
        // Drop the mutable state borrow before reading packet payloads and
        // adding ordered regions to the inventory builder.
        for packet in completed {
            let Some(state) = streams.get_mut(&packet.serial) else {
                continue;
            };
            process_ogg_packet(builder, state, packet)?;
            if let Some(codec) = state.codec {
                if !codec_set.contains(&codec) {
                    codec_set.push(codec);
                }
            }
        }
        offset = page_end;
    }
    for state in streams.values() {
        if !state.pending.is_empty() || state.packet_discarded {
            builder.malformed(
                "REGISTRY-OGG-TRAILING-PACKET",
                "Ogg source ends with an incomplete packet",
                None,
                Some(state.pending_size),
            );
        }
        if state.packet_index != 0 && !state.saw_eos {
            builder.malformed(
                "REGISTRY-OGG-MISSING-EOS",
                "Ogg logical stream has no EOS page",
                None,
                None,
            );
        }
    }
    match codec_set.as_slice() {
        [ContainerKind::OggVorbis] => builder.set_container(ContainerKind::OggVorbis),
        [ContainerKind::OggOpus] => builder.set_container(ContainerKind::OggOpus),
        [] => builder.malformed(
            "REGISTRY-OGG-CODEC",
            "Ogg source has no recognized Vorbis or Opus stream",
            None,
            None,
        ),
        _ => builder.set_container(ContainerKind::Ogg),
    }
    Ok(())
}

fn process_ogg_packet(
    builder: &mut InventoryBuilder<'_, '_>,
    state: &mut OggStreamState,
    packet: OggCompletedPacket,
) -> Result<(), MetadataRegistryError> {
    if packet.extents.is_empty() {
        builder.malformed(
            "REGISTRY-OGG-EMPTY-PACKET",
            "Ogg packet has no payload bytes",
            None,
            Some(0),
        );
        return Ok(());
    }
    let data = builder
        .source
        .read_extents(&packet.extents, builder.limits.max_ogg_packet_bytes)?;
    let packet_path = vec![
        "Ogg".into(),
        format!("serial:{}", packet.serial),
        format!("packet:{}", packet.packet_index),
    ];
    builder.add_structural(
        packet_path.clone(),
        StructuralKind::OggPacket {
            serial: packet.serial,
            packet_index: packet.packet_index,
        },
        packet.extents.clone(),
    );
    // Generic packet bytes are structural provenance only. The codec comment
    // packet below is added as the canonical metadata payload when it is
    // recognized as OpusTags/VorbisComment.
    if packet.packet_index == 0 {
        state.codec = if data.starts_with(b"OpusHead") {
            Some(ContainerKind::OggOpus)
        } else if data.starts_with(b"\x01vorbis") {
            Some(ContainerKind::OggVorbis)
        } else {
            builder.malformed(
                "REGISTRY-OGG-CODEC-HEADER",
                "first Ogg packet is neither OpusHead nor Vorbis identification",
                packet.extents.first().map(|extent| extent.offset),
                Some(data.len() as u64),
            );
            None
        };
    } else if packet.packet_index == 1 {
        match state.codec {
            Some(ContainerKind::OggOpus) if data.starts_with(b"OpusTags") => {
                builder.add_region(
                    packet_path,
                    MetadataKind::OpusTags {
                        serial: packet.serial,
                        packet_index: packet.packet_index,
                    },
                    packet.extents,
                    true,
                    true,
                )?;
                validate_codec_comments(builder, &data, true, packet.serial, packet.packet_index);
            }
            Some(ContainerKind::OggVorbis) if data.starts_with(b"\x03vorbis") => {
                builder.add_region(
                    packet_path,
                    MetadataKind::VorbisComment {
                        serial: packet.serial,
                        packet_index: packet.packet_index,
                    },
                    packet.extents,
                    true,
                    true,
                )?;
                validate_codec_comments(builder, &data, false, packet.serial, packet.packet_index);
            }
            _ => builder.malformed(
                "REGISTRY-OGG-COMMENT-HEADER",
                "second Ogg packet does not match the stream codec comment header",
                packet.extents.first().map(|extent| extent.offset),
                Some(data.len() as u64),
            ),
        }
    }
    Ok(())
}

fn validate_codec_comments(
    builder: &mut InventoryBuilder<'_, '_>,
    data: &[u8],
    opus: bool,
    serial: u32,
    packet_index: u32,
) {
    let prefix_len = if opus { 8 } else { 7 };
    if data.len() < prefix_len + 8 {
        builder.malformed(
            "REGISTRY-OGG-COMMENT-SIZE",
            "codec comment packet is shorter than its vendor/count fields",
            None,
            Some(data.len() as u64),
        );
        return;
    }
    let mut cursor = prefix_len;
    let vendor_size_u64 = u64::from(u32::from_le_bytes(
        data[cursor..cursor + 4].try_into().unwrap(),
    ));
    cursor += 4;
    let Ok(vendor_size) = usize::try_from(vendor_size_u64) else {
        builder.malformed(
            "REGISTRY-OGG-COMMENT-SIZE",
            "codec comment vendor size does not fit the host address space",
            None,
            Some(vendor_size_u64),
        );
        return;
    };
    let Some(vendor_end) = cursor.checked_add(vendor_size) else {
        builder.malformed(
            "REGISTRY-OGG-COMMENT-SIZE",
            "codec comment vendor size overflows",
            None,
            Some(vendor_size_u64),
        );
        return;
    };
    if vendor_end > data.len() {
        builder.malformed(
            "REGISTRY-OGG-COMMENT-VENDOR",
            "codec comment vendor exceeds packet bounds",
            None,
            Some(vendor_size_u64),
        );
        return;
    }
    if std::str::from_utf8(&data[cursor..vendor_end]).is_err() {
        builder.malformed(
            "REGISTRY-OGG-COMMENT-UTF8",
            "codec comment vendor is not UTF-8",
            None,
            Some(vendor_size_u64),
        );
    }
    cursor = vendor_end;
    if cursor + 4 > data.len() {
        builder.malformed(
            "REGISTRY-OGG-COMMENT-COUNT",
            "codec comment lacks its comment count",
            None,
            Some((data.len() - cursor) as u64),
        );
        return;
    }
    let count = u32::from_le_bytes(data[cursor..cursor + 4].try_into().unwrap());
    cursor += 4;
    let mut r128_track = false;
    let mut r128_album = false;
    for _ in 0..count {
        if cursor + 4 > data.len() {
            builder.malformed(
                "REGISTRY-OGG-COMMENT-LENGTH",
                "codec comment lacks a field length",
                None,
                Some((data.len() - cursor) as u64),
            );
            return;
        }
        let field_length_u64 = u64::from(u32::from_le_bytes(
            data[cursor..cursor + 4].try_into().unwrap(),
        ));
        cursor += 4;
        let Ok(length) = usize::try_from(field_length_u64) else {
            builder.malformed(
                "REGISTRY-OGG-COMMENT-LENGTH",
                "codec comment field length does not fit the host address space",
                None,
                Some(field_length_u64),
            );
            return;
        };
        let Some(end) = cursor.checked_add(length) else {
            builder.malformed(
                "REGISTRY-OGG-COMMENT-LENGTH",
                "codec comment field length overflows",
                None,
                Some(field_length_u64),
            );
            return;
        };
        if end > data.len() {
            builder.malformed(
                "REGISTRY-OGG-COMMENT-BOUNDS",
                "codec comment field exceeds packet bounds",
                None,
                Some(field_length_u64),
            );
            return;
        }
        let field = &data[cursor..end];
        if std::str::from_utf8(field).is_err() {
            builder.malformed(
                "REGISTRY-OGG-COMMENT-UTF8",
                "codec comment field is not UTF-8",
                None,
                Some(field_length_u64),
            );
        }
        if let Some(separator) = field.iter().position(|byte| *byte == b'=') {
            let key = &field[..separator];
            if key.is_empty() || key.iter().any(|byte| *byte < 0x20 || *byte > 0x7e) {
                builder.malformed(
                    "REGISTRY-OGG-COMMENT-KEY",
                    "codec comment field name is invalid",
                    None,
                    Some(field_length_u64),
                );
            }
            if opus && key.eq_ignore_ascii_case(b"R128_TRACK_GAIN") {
                if r128_track {
                    builder.malformed(
                        "REGISTRY-OGG-R128-DUPLICATE",
                        "Opus R128_TRACK_GAIN occurs more than once",
                        None,
                        Some(field_length_u64),
                    );
                }
                r128_track = true;
            }
            if opus && key.eq_ignore_ascii_case(b"R128_ALBUM_GAIN") {
                if r128_album {
                    builder.malformed(
                        "REGISTRY-OGG-R128-DUPLICATE",
                        "Opus R128_ALBUM_GAIN occurs more than once",
                        None,
                        Some(field_length_u64),
                    );
                }
                r128_album = true;
            }
        }
        cursor = end;
    }
    if !opus {
        if cursor >= data.len() || data[cursor] != 1 {
            builder.malformed(
                "REGISTRY-VORBIS-FRAMING",
                "Vorbis comment packet has no valid framing bit",
                None,
                Some(1),
            );
        } else if cursor + 1 != data.len() {
            builder.malformed(
                "REGISTRY-VORBIS-FRAMING",
                "Vorbis comment packet has bytes after its framing bit",
                None,
                Some((data.len() - cursor - 1) as u64),
            );
        }
    } else if cursor != data.len() {
        builder.malformed(
            "REGISTRY-OPUS-COMMENT-TRAILING",
            "OpusTags packet has trailing bytes after its comment vector",
            None,
            Some((data.len() - cursor) as u64),
        );
    }
    let _ = (serial, packet_index);
}

#[derive(Clone, Copy)]
struct BmffBox {
    id: [u8; 4],
    start: u64,
    body_start: u64,
    end: u64,
}

fn discover_isobmff(builder: &mut InventoryBuilder<'_, '_>) -> Result<(), MetadataRegistryError> {
    let source_len = builder.source.len();
    let mut offset = 0_u64;
    let mut saw_ftyp = false;
    while offset < source_len {
        let Some(item) = read_bmff_box(builder, offset, source_len)? else {
            break;
        };
        if item.id == *b"ftyp" {
            saw_ftyp = true;
        }
        let path = vec![fourcc(item.id)];
        walk_bmff_region(builder, item, 0, path, false, false)?;
        offset = item.end;
    }
    if !saw_ftyp {
        builder.malformed(
            "REGISTRY-ISOBMFF-FTYP",
            "ISO-BMFF source has no ftyp box",
            None,
            None,
        );
    }
    if offset != source_len {
        builder.malformed(
            "REGISTRY-ISOBMFF-TOP-LEVEL",
            "ISO-BMFF top-level boxes do not cover the source",
            Some(offset),
            Some(source_len.saturating_sub(offset)),
        );
    }
    Ok(())
}

fn read_bmff_box(
    builder: &mut InventoryBuilder<'_, '_>,
    start: u64,
    parent_end: u64,
) -> Result<Option<BmffBox>, MetadataRegistryError> {
    if start >= parent_end {
        return Ok(None);
    }
    if parent_end - start < 8 {
        builder.malformed(
            "REGISTRY-ISOBMFF-HEADER",
            "truncated ISO-BMFF box header",
            Some(start),
            Some(parent_end - start),
        );
        return Ok(None);
    }
    let mut header = [0_u8; 16];
    builder.source.read_exact_at(start, &mut header[..8])?;
    let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
    let id: [u8; 4] = header[4..8].try_into().unwrap();
    let (size, header_size) = match size32 {
        0 => (parent_end - start, 8_u64),
        1 => {
            if parent_end - start < 16 {
                builder.malformed(
                    "REGISTRY-ISOBMFF-EXTENDED-HEADER",
                    "truncated ISO-BMFF extended-size box header",
                    Some(start),
                    Some(parent_end - start),
                );
                return Ok(None);
            }
            builder
                .source
                .read_exact_at(start + 8, &mut header[8..16])?;
            (
                u64::from_be_bytes(header[8..16].try_into().unwrap()),
                16_u64,
            )
        }
        size => (u64::from(size), 8_u64),
    };
    if size < header_size {
        builder.malformed(
            "REGISTRY-ISOBMFF-SIZE",
            "ISO-BMFF box is smaller than its header",
            Some(start),
            Some(size),
        );
        return Ok(None);
    }
    let Some(end) = start.checked_add(size) else {
        builder.malformed(
            "REGISTRY-ISOBMFF-SIZE",
            "ISO-BMFF box size overflows",
            Some(start),
            Some(size),
        );
        return Ok(None);
    };
    if end > parent_end || end <= start {
        builder.malformed(
            "REGISTRY-ISOBMFF-BOUNDS",
            "ISO-BMFF box exceeds its parent",
            Some(start),
            Some(size),
        );
        return Ok(None);
    }
    Ok(Some(BmffBox {
        id,
        start,
        body_start: start + header_size,
        end,
    }))
}

fn walk_bmff_region(
    builder: &mut InventoryBuilder<'_, '_>,
    parent: BmffBox,
    depth: usize,
    parent_path: Vec<String>,
    metadata_context: bool,
    under_ilst: bool,
) -> Result<(), MetadataRegistryError> {
    if depth > builder.limits.max_nesting_depth {
        builder.issue(
            IssueKind::BudgetExceeded,
            "REGISTRY-ISOBMFF-MAX-DEPTH",
            format!(
                "ISO-BMFF nesting exceeds configured depth {}",
                builder.limits.max_nesting_depth
            ),
            Some(parent.start),
            Some(parent.end - parent.start),
        );
        return Ok(());
    }
    let is_meta_box =
        metadata_context || matches!(&parent.id, b"udta" | b"meta" | b"ilst") || under_ilst;
    let metadata_leaf = matches!(&parent.id, b"data" | b"mean" | b"name");
    let known_container = is_bmff_container(parent.id) || (under_ilst && !metadata_leaf);
    if is_meta_box {
        builder.add_region(
            parent_path.clone(),
            MetadataKind::IsoBmffBox {
                id: fourcc(parent.id),
                metadata: true,
            },
            vec![ByteExtent {
                offset: parent.start,
                length: parent.end - parent.start,
            }],
            !known_container,
            !known_container,
        )?;
    } else {
        builder.add_structural(
            parent_path.clone(),
            StructuralKind::IsoBmffBox {
                id: fourcc(parent.id),
            },
            vec![ByteExtent {
                offset: parent.start,
                length: parent.end - parent.start,
            }],
        );
    }

    let mut child_start = parent.body_start;
    if parent.id == *b"meta" {
        if parent.end - child_start < 4 {
            builder.malformed(
                "REGISTRY-ISOBMFF-META-FULLBOX",
                "ISO-BMFF meta box lacks its FullBox version/flags",
                Some(child_start),
                Some(parent.end - child_start),
            );
            return Ok(());
        }
        child_start += 4;
    }
    if !known_container {
        return Ok(());
    }
    while child_start < parent.end {
        let Some(child) = read_bmff_box(builder, child_start, parent.end)? else {
            break;
        };
        let child_name = fourcc(child.id);
        let mut child_path = parent_path.clone();
        child_path.push(child_name);
        let child_metadata =
            is_meta_box || matches!(&child.id, b"udta" | b"meta" | b"ilst") || under_ilst;
        // A direct child of ilst is a user-defined item box.  Its payload is
        // itself a nested data/mean/name tree even when its fourcc is unknown.
        let child_under_ilst = under_ilst || parent.id == *b"ilst";
        walk_bmff_region(
            builder,
            child,
            depth + 1,
            child_path,
            child_metadata,
            child_under_ilst,
        )?;
        child_start = child.end;
    }
    if child_start != parent.end {
        builder.malformed(
            "REGISTRY-ISOBMFF-CHILDREN",
            "ISO-BMFF child boxes do not cover their parent",
            Some(child_start),
            Some(parent.end.saturating_sub(child_start)),
        );
    }
    Ok(())
}

fn is_bmff_container(id: [u8; 4]) -> bool {
    matches!(
        &id,
        b"moov"
            | b"trak"
            | b"mdia"
            | b"minf"
            | b"stbl"
            | b"mvex"
            | b"moof"
            | b"traf"
            | b"edts"
            | b"dinf"
            | b"schi"
            | b"sinf"
            | b"udta"
            | b"meta"
            | b"ilst"
            | b"ludt"
            | b"keys"
            | b"ipro"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory_from_value(
        value: &serde_json::Value,
    ) -> Result<MetadataInventory, MetadataRegistryError> {
        MetadataInventory::from_json_slice(&serde_json::to_vec(value).unwrap())
    }

    fn le32(value: u32) -> [u8; 4] {
        value.to_le_bytes()
    }

    fn wave_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::with_capacity(8 + body.len() + body.len() % 2);
        chunk.extend_from_slice(id);
        chunk.extend_from_slice(&le32(body.len() as u32));
        chunk.extend_from_slice(body);
        if body.len() & 1 != 0 {
            chunk.push(0);
        }
        chunk
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let payload_len = chunks.iter().map(Vec::len).sum::<usize>();
        let mut bytes = Vec::with_capacity(12 + payload_len);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&le32((4 + payload_len) as u32));
        bytes.extend_from_slice(b"WAVE");
        for chunk in chunks {
            bytes.extend_from_slice(chunk);
        }
        bytes
    }

    fn rf64_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::with_capacity(8 + body.len() + body.len() % 2);
        chunk.extend_from_slice(id);
        chunk.extend_from_slice(&u32::MAX.to_le_bytes());
        chunk.extend_from_slice(body);
        if body.len() & 1 != 0 {
            chunk.push(0);
        }
        chunk
    }

    fn rf64() -> Vec<u8> {
        let fmt = wave_chunk(b"fmt ", &[1, 0, 2, 0]);
        let data = rf64_chunk(b"data", &[0xaa, 0xbb, 0xcc, 0xdd]);
        let junk = rf64_chunk(b"JUNK", b"tail");
        let ds64_len = 28 + 12;
        let source_len = 12 + 8 + ds64_len + ds64_len % 2 + fmt.len() + data.len() + junk.len();
        let mut ds64_body = Vec::with_capacity(ds64_len);
        ds64_body.extend_from_slice(&((source_len - 8) as u64).to_le_bytes());
        ds64_body.extend_from_slice(&(4_u64).to_le_bytes());
        ds64_body.extend_from_slice(&0_u64.to_le_bytes());
        ds64_body.extend_from_slice(&1_u32.to_le_bytes());
        ds64_body.extend_from_slice(b"JUNK");
        ds64_body.extend_from_slice(&(4_u64).to_le_bytes());
        let ds64 = wave_chunk(b"ds64", &ds64_body);
        assert_eq!(ds64.len(), 8 + ds64_len);
        let mut source = Vec::with_capacity(source_len);
        source.extend_from_slice(b"RF64");
        source.extend_from_slice(&u32::MAX.to_le_bytes());
        source.extend_from_slice(b"WAVE");
        source.extend_from_slice(&ds64);
        source.extend_from_slice(&fmt);
        source.extend_from_slice(&data);
        source.extend_from_slice(&junk);
        assert_eq!(source.len(), source_len);
        source
    }

    #[test]
    fn wave_inventories_chunks_after_audio_without_retaining_audio() {
        let source = riff(&[
            wave_chunk(b"fmt ", &[1, 0, 2, 0]),
            wave_chunk(b"data", &[0xaa, 0xbb, 0xcc, 0xdd]),
            wave_chunk(b"JUNK", b"after-data"),
            wave_chunk(b"LIST", b"tail"),
        ]);
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(inventory.container, ContainerKind::Wave);
        assert!(inventory.regions.iter().all(|region| {
            !matches!(
                region.kind,
                MetadataKind::WaveChunk {
                    audio_data: true,
                    ..
                }
            )
        }));
        let data = inventory.structural_regions.iter().find(|region| {
            matches!(
                region.kind,
                StructuralKind::WaveChunk {
                    audio_data: true,
                    ..
                }
            )
        });
        assert!(data.is_some());
        let tail_ids = inventory
            .regions
            .iter()
            .filter_map(|region| match &region.kind {
                MetadataKind::WaveChunk { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tail_ids, ["JUNK", "LIST"]);
        assert_eq!(
            inventory.metadata_bytes,
            inventory.regions[0].size + inventory.regions[1].size
        );
        assert!(inventory.regions.iter().all(|region| {
            region.raw.is_some() || region.raw_omitted == Some(RawOmissionReason::Disabled)
        }));
    }

    #[test]
    fn rf64_resolves_ds64_data_and_table_sizes_without_classifying_data_as_metadata() {
        let source = rf64();
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(inventory.container, ContainerKind::Wave);
        assert!(
            inventory.issues.is_empty(),
            "unexpected RF64 issues: {:?}",
            inventory.issues
        );
        assert!(inventory.structural_regions.iter().any(|region| {
            matches!(&region.kind, StructuralKind::WaveChunk { id, audio_data: true } if id == "data")
        }));
        assert!(inventory.regions.iter().any(|region| {
            matches!(&region.kind, MetadataKind::WaveChunk { id, audio_data: false } if id == "JUNK")
        }));
        assert!(!inventory.regions.iter().any(|region| {
            matches!(&region.kind, MetadataKind::WaveChunk { id, .. } if id == "data")
        }));
    }

    #[test]
    fn rf64_reports_ds64_table_and_riff_bounds_issues() {
        let mut source = rf64();
        // ds64 tableLength is at the fixed-field offset 24 in the body; make
        // it require an entry that does not fit this ds64 chunk.
        source[12 + 8 + 24..12 + 8 + 28].copy_from_slice(&2_u32.to_le_bytes());
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-WAVE-DS64-TABLE"));

        let mut source = rf64();
        // ds64 riffSize is at the beginning of the ds64 payload.
        let declared = (source.len() as u64 - 9).to_le_bytes();
        source[20..28].copy_from_slice(&declared);
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-WAVE-DS64-RIFF-BOUNDS"));
    }

    #[test]
    fn duplicate_extents_are_charged_once_by_the_ledger() {
        let mut ledger = ExtentLedger::default();
        assert_eq!(
            ledger.add(&[
                ByteExtent {
                    offset: 10,
                    length: 5
                },
                ByteExtent {
                    offset: 10,
                    length: 5
                },
                ByteExtent {
                    offset: 12,
                    length: 5
                },
            ]),
            7
        );
        assert_eq!(
            ledger.newly_covered(&[ByteExtent {
                offset: 10,
                length: 5
            }]),
            0
        );

        let mut adjacent = ExtentLedger::default();
        assert_eq!(
            adjacent.add(&[ByteExtent {
                offset: 20,
                length: 10,
            }]),
            10
        );
        assert_eq!(
            adjacent.add(&[ByteExtent {
                offset: 10,
                length: 10,
            }]),
            10
        );
        assert_eq!(adjacent.covered_bytes(), Some(20));
        assert_eq!(
            adjacent.newly_covered(&[ByteExtent {
                offset: 10,
                length: 20,
            }]),
            0
        );

        let mut disjoint = ExtentLedger::default();
        for index in (0_u64..10_000).rev() {
            assert_eq!(
                disjoint.add(&[ByteExtent {
                    offset: index * 4,
                    length: 1,
                }]),
                1
            );
            assert_eq!(
                disjoint.newly_covered(&[ByteExtent {
                    offset: index * 4,
                    length: 1,
                }]),
                0
            );
        }
        assert_eq!(disjoint.covered_bytes(), Some(10_000));
        assert!(!disjoint.overlaps(&[ByteExtent {
            offset: 20_001,
            length: 2,
        }]));
        assert!(disjoint.overlaps(&[ByteExtent {
            offset: 20_000,
            length: 2,
        }]));
        assert_eq!(
            disjoint.add(&[ByteExtent {
                offset: 0,
                length: 40_000,
            }]),
            30_000
        );
        assert_eq!(disjoint.covered_bytes(), Some(40_000));
    }

    #[test]
    fn wave_reports_declared_bounds_and_duplicate_structural_chunks() {
        let mut source = riff(&[
            wave_chunk(b"fmt ", &[1]),
            wave_chunk(b"fmt ", &[2]),
            wave_chunk(b"data", &[0]),
        ]);
        // Keep a valid source-level size but make the first RIFF declaration
        // end before the final chunk; discovery remains bounded and reports it.
        let declared_size = source.len() as u32 - 8 - 10;
        source[4..8].copy_from_slice(&le32(declared_size));
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-WAVE-RIFF-CHUNK-BOUNDS"));
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-WAVE-DUPLICATE-STRUCTURAL"));
    }

    #[test]
    fn flac_inventories_streaminfo_as_structure_and_other_blocks_as_metadata() {
        let mut source = b"fLaC".to_vec();
        source.extend_from_slice(&[0, 0, 0, 1, 0]);
        source.extend_from_slice(&[0x84, 0, 0, 3]);
        source.extend_from_slice(b"tag");
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(inventory.container, ContainerKind::Flac);
        assert!(inventory
            .structural_regions
            .iter()
            .any(|region| { matches!(region.kind, StructuralKind::FlacBlock { block_type: 0 }) }));
        assert!(inventory
            .regions
            .iter()
            .any(|region| { matches!(region.kind, MetadataKind::FlacBlock { block_type: 4 }) }));
    }

    #[test]
    fn flac_block_kinds_match_published_type_ranges() {
        assert!(validate_metadata_kind(&MetadataKind::FlacBlock { block_type: 1 }).is_ok());
        assert!(validate_metadata_kind(&MetadataKind::FlacBlock { block_type: 127 }).is_ok());
        assert!(validate_metadata_kind(&MetadataKind::FlacBlock { block_type: 128 }).is_err());
        assert!(validate_structural_kind(&StructuralKind::FlacBlock { block_type: 0 }).is_ok());
        assert!(validate_structural_kind(&StructuralKind::FlacBlock { block_type: 1 }).is_err());
    }

    fn syncsafe(value: usize) -> [u8; 4] {
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    #[test]
    fn mp3_discovers_id3_frames_id3v1_and_ape_footer() {
        let mut source = b"ID3\x04\x00\x00".to_vec();
        let frame_body = b"\x00title";
        source.extend_from_slice(&syncsafe(10 + frame_body.len()));
        source.extend_from_slice(b"TIT2");
        source.extend_from_slice(&(frame_body.len() as u32).to_be_bytes());
        source.extend_from_slice(&[0, 0]);
        source.extend_from_slice(frame_body);
        source.extend_from_slice(b"APETAGEX");
        source.extend_from_slice(&2000_u32.to_le_bytes());
        source.extend_from_slice(&32_u32.to_le_bytes());
        source.extend_from_slice(&0_u32.to_le_bytes());
        source.extend_from_slice(&0x4000_0000_u32.to_le_bytes());
        source.extend_from_slice(&[0; 8]);
        source.extend_from_slice(&[0; 128]);
        let id3v1 = source.len() - 128;
        source[id3v1..id3v1 + 3].copy_from_slice(b"TAG");
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert!(inventory
            .regions
            .iter()
            .any(|region| matches!(region.kind, MetadataKind::Id3v2Frame { .. })));
        assert!(inventory
            .regions
            .iter()
            .any(|region| matches!(region.kind, MetadataKind::Id3v1Tag)));
        assert!(inventory
            .regions
            .iter()
            .any(|region| matches!(region.kind, MetadataKind::Apev2Tag)));
    }

    #[test]
    fn many_empty_ape_tags_do_not_require_a_quadratic_duplicate_scan() {
        const TAG_COUNT: usize = 10_000;
        let mut source = Vec::with_capacity(4 + TAG_COUNT * 32);
        // Keep the leading bytes outside the APE footer walk so the MP3
        // adapter does not also interpret the first footer as a malformed
        // leading APE header.
        source.extend_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
        for _ in 0..TAG_COUNT {
            // A footer-only APEv2 tag with no items occupies exactly its
            // 32-byte descriptor. The reverse footer walk discovers these as
            // distinct physical occurrences.
            source.extend_from_slice(b"APETAGEX");
            source.extend_from_slice(&2000_u32.to_le_bytes());
            source.extend_from_slice(&32_u32.to_le_bytes());
            source.extend_from_slice(&0_u32.to_le_bytes());
            source.extend_from_slice(&0_u32.to_le_bytes());
            source.extend_from_slice(&[0; 8]);
        }

        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(
            inventory
                .regions
                .iter()
                .filter(|region| matches!(region.kind, MetadataKind::Apev2Tag))
                .count(),
            TAG_COUNT
        );
        assert!(inventory.issues.is_empty());
    }

    fn ogg_page(
        serial: u32,
        sequence: u32,
        header_type: u8,
        body: &[u8],
        lacing: &[u8],
    ) -> Vec<u8> {
        assert_eq!(
            body.len(),
            lacing.iter().map(|length| *length as usize).sum::<usize>()
        );
        let mut page = Vec::with_capacity(27 + lacing.len() + body.len());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(header_type);
        page.extend_from_slice(&0_u64.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&0_u32.to_le_bytes());
        page.push(lacing.len() as u8);
        page.extend_from_slice(lacing);
        page.extend_from_slice(body);
        page
    }

    #[test]
    fn ogg_comment_packet_keeps_noncontiguous_extents() {
        let mut source = Vec::new();
        let opus_head =
            b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        source.extend_from_slice(&ogg_page(7, 0, 0x02, opus_head, &[opus_head.len() as u8]));
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&300_u32.to_le_bytes());
        tags.extend_from_slice(&[b'v'; 300]);
        tags.extend_from_slice(&0_u32.to_le_bytes());
        let first = tags[..255].to_vec();
        let second = tags[255..].to_vec();
        source.extend_from_slice(&ogg_page(7, 1, 0, &first, &[255]));
        source.extend_from_slice(&ogg_page(7, 2, 0x05, &second, &[second.len() as u8]));
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(inventory.container, ContainerKind::OggOpus);
        let tags_region = inventory
            .regions
            .iter()
            .find(|region| matches!(region.kind, MetadataKind::OpusTags { .. }))
            .unwrap();
        assert_eq!(inventory.regions.len(), 1);
        assert_eq!(tags_region.extents.len(), 2);
        assert_eq!(tags_region.raw.as_ref().unwrap(), &tags);
    }

    #[test]
    fn oversized_ogg_continuations_do_not_retain_discarded_extents() {
        let serial = 11;
        let opus_head =
            b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let mut source = Vec::new();
        source.extend_from_slice(&ogg_page(
            serial,
            0,
            0x02,
            opus_head,
            &[opus_head.len() as u8],
        ));

        // Start a packet whose first segment is just below the configured
        // limit, then continue it over many pages without completing it.
        // The scanner must track only the discard state until the final
        // lacing segment, rather than retaining one extent per page.
        source.extend_from_slice(&ogg_page(serial, 1, 0, &[0_u8; 255], &[255]));
        let continuation_count = 2048_u32;
        for sequence in 2..=continuation_count + 1 {
            let terminating = sequence == continuation_count + 1;
            let body = if terminating {
                vec![0_u8]
            } else {
                vec![0_u8; 255]
            };
            let lacing = if terminating {
                vec![1_u8]
            } else {
                vec![255_u8]
            };
            source.extend_from_slice(&ogg_page(serial, sequence, 0x01, &body, &lacing));
        }

        // A fresh packet after the discarded one must still be assembled and
        // assigned the next packet index. Mark it EOS so stream finalization
        // has a complete logical stream to validate.
        source.extend_from_slice(&ogg_page(serial, continuation_count + 2, 0x04, b"OK", &[2]));

        let inventory = discover_bytes(
            &source,
            DiscoveryLimits {
                max_ogg_packet_bytes: 256,
                ..DiscoveryLimits::default()
            },
        )
        .unwrap();
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-OGG-MAX-PACKET"));
        assert!(inventory.structural_regions.iter().any(|region| {
            matches!(
                region.kind,
                StructuralKind::OggPacket {
                    serial: 11,
                    packet_index: 2
                }
            )
        }));
    }

    #[test]
    fn ogg_packet_extent_count_is_bounded_independently_of_bytes() {
        let serial = 17;
        let opus_head =
            b"OpusHead\x01\x02\x00\x00\x80\xbb\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let mut source = ogg_page(serial, 0, 0x02, opus_head, &[opus_head.len() as u8]);
        source.extend_from_slice(&ogg_page(
            serial,
            1,
            0x04,
            &vec![0_u8; 766],
            &[255, 255, 255, 1],
        ));

        let inventory = discover_bytes(
            &source,
            DiscoveryLimits {
                max_metadata_entries: 2,
                max_ogg_packet_bytes: 4096,
                ..DiscoveryLimits::default()
            },
        )
        .unwrap();
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-OGG-MAX-PACKET-EXTENTS"));
        assert!(inventory.structural_regions.iter().all(|region| !matches!(
            region.kind,
            StructuralKind::OggPacket {
                serial: 17,
                packet_index: 1
            }
        )));
    }

    #[test]
    fn ogg_limits_bound_page_and_issue_accumulation() {
        let mut source = Vec::new();
        for serial in 0..12_u32 {
            let mut page = ogg_page(serial + 1, 0, 0x02, &[], &[]);
            page[4] = 1; // malformed version on every page
            source.extend_from_slice(&page);
        }
        let limits = DiscoveryLimits {
            max_metadata_entries: 2,
            ..DiscoveryLimits::default()
        };
        let inventory = discover_bytes(&source, limits).unwrap();
        assert_eq!(inventory.structural_regions.len(), 2);
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.code == "REGISTRY-MAX-ISSUES"));
        assert!(inventory.issues.iter().any(|issue| {
            issue.code == "REGISTRY-MAX-ISSUES" && issue.message.contains("REGISTRY-OGG-MAX-PAGES")
        }));
        assert!(inventory.issues.len() <= limits.max_metadata_entries + 1);
    }

    fn bmff_box(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut value = Vec::with_capacity(8 + body.len());
        value.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        value.extend_from_slice(id);
        value.extend_from_slice(body);
        value
    }

    #[test]
    fn isobmff_walks_metadata_box_tree() {
        let data = bmff_box(b"data", &[0, 0, 0, 1, b'x']);
        let item = bmff_box(b"titl", &data);
        let ilst = bmff_box(b"ilst", &item);
        let mut meta_body = vec![0, 0, 0, 0];
        meta_body.extend_from_slice(&ilst);
        let meta = bmff_box(b"meta", &meta_body);
        let udta = bmff_box(b"udta", &meta);
        let moov = bmff_box(b"moov", &udta);
        let mut source = bmff_box(b"ftyp", b"isom\0\0\0\0");
        source.extend_from_slice(&moov);
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(inventory.container, ContainerKind::IsoBmff);
        assert!(inventory.regions.iter().any(|region| {
            matches!(&region.kind, MetadataKind::IsoBmffBox { id, .. } if id == "data")
        }));
        assert!(inventory.regions.iter().any(|region| {
            matches!(&region.kind, MetadataKind::IsoBmffBox { id, .. } if id == "titl")
        }));
    }

    #[test]
    fn limits_report_without_allocating_large_raw_region() {
        let source = riff(&[
            wave_chunk(b"fmt ", &[1]),
            wave_chunk(b"JUNK", &[0xaa; 16]),
            wave_chunk(b"data", &[0]),
        ]);
        let limits = DiscoveryLimits {
            max_metadata_item_bytes: 4,
            max_retained_raw_bytes: 4,
            ..DiscoveryLimits::default()
        };
        let inventory = discover_bytes(&source, limits).unwrap();
        let junk = inventory
            .regions
            .iter()
            .find(
                |region| matches!(&region.kind, MetadataKind::WaveChunk { id, .. } if id == "JUNK"),
            )
            .unwrap();
        assert!(junk.raw.is_none());
        assert_eq!(junk.raw_omitted, Some(RawOmissionReason::ItemLimit));
        assert!(inventory
            .issues
            .iter()
            .any(|issue| issue.kind == IssueKind::BudgetExceeded));
    }

    #[test]
    fn validation_requires_matching_item_and_total_budget_evidence() {
        let source = riff(&[
            wave_chunk(b"fmt ", &[1, 0, 2, 0]),
            wave_chunk(b"JUNK", &[0xaa, 0xbb]),
            wave_chunk(b"data", &[0, 1]),
        ]);
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        let region_index = inventory
            .regions
            .iter()
            .position(
                |region| matches!(&region.kind, MetadataKind::WaveChunk { id, .. } if id == "JUNK"),
            )
            .unwrap();
        let region = &inventory.regions[region_index];
        let issue_offset = region.extents[0].offset;
        let size = region.size;
        let mut forged = serde_json::to_value(&inventory).unwrap();
        forged["limits"]["max_metadata_item_bytes"] = serde_json::Value::from(1_u64);
        forged["limits"]["max_metadata_total_bytes"] = serde_json::Value::from(1_u64);
        forged["limits"]["max_retained_raw_bytes"] = serde_json::Value::from(0_u64);
        forged["regions"][region_index]["raw"] = serde_json::Value::Null;
        forged["regions"][region_index]["raw_omitted"] = serde_json::Value::from("disabled");
        forged["retained_raw_bytes"] = serde_json::Value::from(0_u64);
        forged["issues"] = serde_json::Value::Array(Vec::new());

        // Matching extents and hashes are insufficient when the serialized
        // limits are tighter than the declared accounting values.
        assert!(inventory_from_value(&forged).is_err());

        forged["issues"] = serde_json::json!([
            {
                "kind": "budget_exceeded",
                "code": "REGISTRY-MAX-ITEM-BYTES",
                "message": "metadata region exceeds the configured item limit",
                "offset": issue_offset,
                "length": size
            },
            {
                "kind": "budget_exceeded",
                "code": "REGISTRY-MAX-TOTAL-BYTES",
                "message": "metadata regions exceed the configured aggregate limit",
                "offset": issue_offset,
                "length": size
            }
        ]);
        inventory_from_value(&forged).unwrap();

        // Raw retention also obeys the per-item limit; a forged retained copy
        // must not turn an over-limit region into a valid inventory.
        forged["regions"][region_index]["raw"] =
            serde_json::to_value(&inventory.regions[region_index].raw).unwrap();
        forged["regions"][region_index]["raw_omitted"] = serde_json::Value::Null;
        forged["retained_raw_bytes"] = serde_json::Value::from(size);
        assert!(inventory_from_value(&forged).is_err());
    }

    #[test]
    fn budget_issue_validation_scales_with_many_unique_regions() {
        const COUNT: usize = 10_000;
        const REGION_SIZE: u64 = 8;
        let facts: Vec<_> = (0..COUNT)
            .map(|index| BudgetedRegionFact {
                issue_offset: (index as u64) * 16,
                size: REGION_SIZE,
            })
            .collect();
        let mut issues = Vec::with_capacity(COUNT * 2);
        for fact in facts.iter().rev() {
            issues.push(MetadataIssue {
                kind: IssueKind::BudgetExceeded,
                code: "REGISTRY-MAX-ITEM-BYTES".into(),
                message: "metadata region exceeds item limit".into(),
                offset: Some(fact.issue_offset),
                length: Some(fact.size),
            });
        }
        for fact in facts.iter().rev() {
            issues.push(MetadataIssue {
                kind: IssueKind::BudgetExceeded,
                code: "REGISTRY-MAX-TOTAL-BYTES".into(),
                message: "metadata regions exceed aggregate limit".into(),
                offset: Some(fact.issue_offset),
                length: Some(fact.size),
            });
        }

        let total_bytes = (COUNT as u64) * REGION_SIZE;
        let limits = DiscoveryLimits {
            max_metadata_item_bytes: REGION_SIZE - 1,
            max_metadata_total_bytes: total_bytes - 1,
            ..DiscoveryLimits::default()
        };
        validate_budget_issues(limits, total_bytes, &facts, &issues).unwrap();
    }

    #[test]
    fn path_binding_uses_the_open_file_identity_and_matches_bytes_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.wav");
        let source = riff(&[wave_chunk(b"data", &[1, 2, 3, 4])]);
        std::fs::write(&path, &source).unwrap();
        let path_inventory = discover_path(&path, DiscoveryLimits::default()).unwrap();
        let bytes_inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        assert_eq!(path_inventory.source.byte_len, source.len() as u64);
        assert_eq!(path_inventory.source.sha256, bytes_inventory.source.sha256);
        assert!(path_inventory.source.file_identity.is_some());
        assert!(bytes_inventory.source.file_identity.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn path_discovery_rejects_a_final_symlink_before_reading_it() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.wav");
        let link = directory.path().join("link.wav");
        std::fs::write(&target, riff(&[wave_chunk(b"data", &[0])])).unwrap();
        symlink(&target, &link).unwrap();
        let error = discover_path(&link, DiscoveryLimits::default()).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn generated_inventory_validates_and_rejects_boundary_tampering() {
        let source = riff(&[
            wave_chunk(b"fmt ", &[1, 0, 2, 0]),
            wave_chunk(b"JUNK", &[0xaa, 0xbb]),
            wave_chunk(b"data", &[0, 1]),
        ]);
        let inventory = discover_bytes(&source, DiscoveryLimits::default()).unwrap();
        inventory.validate().unwrap();

        let value = serde_json::to_value(&inventory).unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/metadata-inventory-v1.schema.json"))
                .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        if !validator.is_valid(&value) {
            for error in validator.iter_errors(&value) {
                eprintln!("schema error: {error}");
            }
            panic!("generated inventory does not satisfy its schema");
        }

        let mut unknown = value.clone();
        unknown["limits"]["unexpected"] = serde_json::Value::Bool(true);
        assert!(inventory_from_value(&unknown).is_err());

        let round_trip = inventory_from_value(&value).unwrap();
        assert_eq!(round_trip, inventory);
        let encoded = serde_json::to_vec(&value).unwrap();
        assert!(
            MetadataInventory::from_json_slice_with_limit(&encoded, encoded.len() - 1)
                .unwrap_err()
                .to_string()
                .contains("maximum")
        );

        let mut tampered_size = value.clone();
        tampered_size["regions"][0]["size"] = serde_json::Value::from(1_u64);
        assert!(inventory_from_value(&tampered_size).is_err());

        let mut fabricated_omission = value.clone();
        fabricated_omission["regions"][0]["raw"] = serde_json::Value::Null;
        fabricated_omission["regions"][0]["raw_omitted"] =
            serde_json::Value::from("aggregate_limit");
        fabricated_omission["retained_raw_bytes"] = serde_json::Value::from(0_u64);
        assert!(inventory_from_value(&fabricated_omission).is_err());

        let mut tampered_accounting = value.clone();
        let metadata_bytes = tampered_accounting["metadata_bytes"].as_u64().unwrap();
        tampered_accounting["metadata_bytes"] = serde_json::Value::from(metadata_bytes + 1);
        assert!(inventory_from_value(&tampered_accounting).is_err());

        let mut tampered_raw = value;
        tampered_raw["regions"][0]["raw"][0] = serde_json::Value::from(0_u8);
        assert!(inventory_from_value(&tampered_raw).is_err());
    }

    #[test]
    fn configured_limits_cannot_exceed_absolute_registry_bounds() {
        let source = riff(&[wave_chunk(b"data", &[0, 1])]);
        let limits = DiscoveryLimits {
            max_metadata_entries: DEFAULT_MAX_METADATA_ENTRIES + 1,
            ..DiscoveryLimits::default()
        };
        assert!(matches!(
            discover_bytes(&source, limits),
            Err(MetadataRegistryError::InvalidLimits(_))
        ));
    }

    #[test]
    fn serialized_text_and_extent_bounds_match_the_json_contract() {
        // JSON Schema maxLength counts Unicode scalar values, not UTF-8
        // bytes. The Rust boundary uses the same count and C0/C1 range.
        assert!(validate_text("test", &"\u{100}".repeat(1024), 1024).is_ok());
        assert!(validate_text("test", &"\u{100}".repeat(1025), 1024).is_err());
        assert!(validate_text("test", "allowed\u{100}", 1024).is_ok());
        assert!(validate_text("test", "forbidden\u{1f}", 1024).is_err());
        assert!(validate_text("test", "forbidden\u{7f}", 1024).is_err());
        assert!(validate_text("test", "forbidden\u{80}", 1024).is_err());
        assert!(validate_text("test", "forbidden\u{9f}", 1024).is_err());

        let extents = [
            ByteExtent {
                offset: 0,
                length: 1,
            },
            ByteExtent {
                offset: 1,
                length: 1,
            },
        ];
        assert!(validate_extent_list(&extents, 2, 2, 1, "test extents").is_err());
        assert!(validate_extent_list(&extents, 2, 2, 2, "test extents").is_ok());
    }
}
