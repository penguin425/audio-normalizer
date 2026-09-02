//! ITU-R BS.2125-1 S-ADM frame and flow validation.

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const STANDARD: &str = "ITU-R BS.2125-1";
pub const VERSION: &str = "05/2022";
pub const VALIDATOR: &str = "forge-bs2125-1-flow-3";
const MAX_SADM_XML_DEPTH: usize = 64;
const MAX_SADM_XML_ELEMENTS: usize = 250_000;
const MAX_SADM_FRAME_FILES: usize = 100_000;
const MAX_SADM_XML_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SADM_TOTAL_XML_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SADM_XML_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SADM_XML_ATTRIBUTE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SADM_XML_ATTRIBUTES_PER_ELEMENT: usize = 4_096;
const MAX_SADM_NAMESPACE_URI_BYTES: usize = 4 * 1024;
const MAX_SADM_NAMESPACE_COUNT: usize = 1_024;
const MAX_SADM_NAMESPACE_BYTES: usize = 1024 * 1024;
const MAX_SADM_EXPANDED_NAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_SADM_FRAME_CANONICAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_SADM_TOTAL_CANONICAL_BYTES: u64 = 512 * 1024 * 1024;
const XML_NAMESPACE_URI: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Debug, Serialize)]
pub struct SadmAudit {
    pub standard: &'static str,
    pub standard_version: &'static str,
    pub validator: &'static str,
    pub frame_count: usize,
    pub flow_id: Option<String>,
    pub time_reference: String,
    pub passed: bool,
    pub flow_rules: Vec<SadmRule>,
    pub frames: Vec<SadmFrameAudit>,
}

#[derive(Debug, Serialize)]
pub struct SadmFrameAudit {
    pub index: usize,
    pub path: String,
    pub frame_format_id: Option<String>,
    pub frame_type: Option<String>,
    pub start: Option<String>,
    pub duration: Option<String>,
    pub passed: bool,
    pub rules: Vec<SadmRule>,
}

#[derive(Debug, Serialize)]
pub struct SadmRule {
    pub rule_id: &'static str,
    pub path: String,
    pub requirement: String,
    pub observed: String,
    pub passed: bool,
}

#[derive(Debug, Default)]
struct ParsedFrame {
    roots: usize,
    frame_headers: usize,
    frame_formats: usize,
    transport_track_formats: usize,
    audio_format_extended: usize,
    direct_audio_format_extended: usize,
    nested_audio_format_extended: usize,
    core_metadata: usize,
    core_metadata_formats: usize,
    payload_path_valid: bool,
    version: Option<String>,
    attributes: HashMap<String, String>,
    changed_ids: usize,
    changed_declarations: Vec<ChangedDeclaration>,
    changed_shape_errors: Vec<String>,
    structural_shape_errors: Vec<String>,
    adm_elements: Vec<AdmElement>,
    state_errors: Vec<String>,
    canonical_bytes: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StateKey {
    kind: &'static str,
    id: String,
}

#[derive(Clone, Debug)]
struct ChangedDeclaration {
    key: Option<StateKey>,
    status: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AdmElement {
    key: StateKey,
    canonical: String,
    canonical_without_timing: String,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct XmlName {
    namespace: Option<Arc<str>>,
    local: String,
}

#[derive(Debug, Default)]
struct XmlNamePool {
    namespaces: HashSet<Arc<str>>,
    namespace_aliases: HashMap<String, Arc<str>>,
    namespace_bytes: usize,
    expanded_name_bytes: usize,
}

impl XmlNamePool {
    fn intern_namespace(&mut self, uri: &str) -> Result<Option<Arc<str>>, String> {
        if uri.is_empty() {
            return Ok(None);
        }
        if uri.len() > MAX_SADM_NAMESPACE_URI_BYTES {
            return Err(format!(
                "XML namespace URI exceeds {MAX_SADM_NAMESPACE_URI_BYTES} bytes"
            ));
        }
        if let Some(existing) = self.namespaces.get(uri) {
            return Ok(Some(existing.clone()));
        }
        if self.namespaces.len() >= MAX_SADM_NAMESPACE_COUNT {
            return Err(format!(
                "XML namespace count exceeds {MAX_SADM_NAMESPACE_COUNT}"
            ));
        }
        self.namespace_bytes = self
            .namespace_bytes
            .checked_add(uri.len())
            .ok_or_else(|| "XML namespace byte count overflow".to_string())?;
        if self.namespace_bytes > MAX_SADM_NAMESPACE_BYTES {
            return Err(format!(
                "XML namespace data exceeds {MAX_SADM_NAMESPACE_BYTES} bytes"
            ));
        }
        let shared = Arc::<str>::from(uri);
        self.namespaces.insert(shared.clone());
        Ok(Some(shared))
    }

    fn register_namespace(&mut self, raw: &str, normalized: &str) -> Result<(), String> {
        let Some(shared) = self.intern_namespace(normalized)? else {
            return Ok(());
        };
        if raw == normalized || self.namespace_aliases.contains_key(raw) {
            return Ok(());
        }
        if self.namespace_aliases.len() >= MAX_SADM_NAMESPACE_COUNT {
            return Err(format!(
                "XML namespace alias count exceeds {MAX_SADM_NAMESPACE_COUNT}"
            ));
        }
        self.namespace_bytes = self
            .namespace_bytes
            .checked_add(raw.len())
            .ok_or_else(|| "XML namespace byte count overflow".to_string())?;
        if self.namespace_bytes > MAX_SADM_NAMESPACE_BYTES {
            return Err(format!(
                "XML namespace data exceeds {MAX_SADM_NAMESPACE_BYTES} bytes"
            ));
        }
        self.namespace_aliases.insert(raw.to_owned(), shared);
        Ok(())
    }

    fn resolve_namespace(&mut self, raw: &str) -> Result<Option<Arc<str>>, String> {
        if let Some(shared) = self.namespace_aliases.get(raw) {
            return Ok(Some(shared.clone()));
        }
        self.intern_namespace(raw)
    }

    fn expanded_name(
        &mut self,
        namespace: Option<Arc<str>>,
        local: &str,
    ) -> Result<XmlName, String> {
        let bytes = local
            .len()
            .checked_add(namespace.as_ref().map_or(0, |uri| uri.len()))
            .ok_or_else(|| "XML expanded-name byte count overflow".to_string())?;
        self.expanded_name_bytes = self
            .expanded_name_bytes
            .checked_add(bytes)
            .ok_or_else(|| "XML expanded-name byte count overflow".to_string())?;
        if self.expanded_name_bytes > MAX_SADM_EXPANDED_NAME_BYTES {
            return Err(format!(
                "XML expanded-name data exceeds {MAX_SADM_EXPANDED_NAME_BYTES} bytes"
            ));
        }
        Ok(XmlName {
            namespace,
            local: local.to_owned(),
        })
    }
}

#[derive(Clone, Debug)]
struct XmlAttribute {
    name: XmlName,
    value: String,
}

#[derive(Clone, Debug)]
enum XmlContent {
    Child(usize),
    Text(String),
}

#[derive(Clone, Debug)]
struct XmlNode {
    name: XmlName,
    parent: Option<usize>,
    attributes: Vec<XmlAttribute>,
    content: Vec<XmlContent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameFormatId {
    base: u64,
    chunk: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExactTime {
    numerator: u128,
    denominator: u128,
}

impl ExactTime {
    fn new(numerator: u128, denominator: u128) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        let divisor = gcd(numerator, denominator);
        Some(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        let common = gcd(self.denominator, other.denominator);
        let left_scale = other.denominator / common;
        let right_scale = self.denominator / common;
        let numerator = self
            .numerator
            .checked_mul(left_scale)?
            .checked_add(other.numerator.checked_mul(right_scale)?)?;
        let denominator = self.denominator.checked_mul(left_scale)?;
        Self::new(numerator, denominator)
    }

    fn is_positive(self) -> bool {
        self.numerator > 0
    }
}

impl fmt::Display for ExactTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denominator == 1 {
            write!(formatter, "{}s", self.numerator)
        } else {
            write!(formatter, "{}/{}s", self.numerator, self.denominator)
        }
    }
}

#[derive(Debug)]
struct LogicalFrame {
    base: Option<u64>,
    chunked: bool,
    members: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowKind {
    FullFrame,
    IntermediateFrame,
    MixedFrame,
    DividedFrame,
    Indeterminate,
    Invalid,
}

impl FlowKind {
    fn name(self) -> &'static str {
        match self {
            Self::FullFrame => "full-frame",
            Self::IntermediateFrame => "intermediate-frame",
            Self::MixedFrame => "mixed-frame",
            Self::DividedFrame => "divided-frame",
            Self::Indeterminate => "indeterminate (single initial frame)",
            Self::Invalid => "invalid",
        }
    }
}

pub fn audit(paths: &[PathBuf]) -> Result<SadmAudit, String> {
    if paths.is_empty() {
        return Err("at least one S-ADM frame XML file is required".into());
    }
    if paths.len() > MAX_SADM_FRAME_FILES {
        return Err(format!(
            "S-ADM flow contains more than {MAX_SADM_FRAME_FILES} frame files"
        ));
    }
    let mut total_xml_bytes = 0_u64;
    for path in paths {
        let bytes = fs::metadata(path)
            .map_err(|error| format!("read {} metadata: {error}", path.display()))?
            .len();
        if bytes > MAX_SADM_XML_BYTES {
            return Err(format!(
                "read {}: XML size {bytes} exceeds {MAX_SADM_XML_BYTES} bytes",
                path.display()
            ));
        }
        total_xml_bytes = total_xml_bytes
            .checked_add(bytes)
            .ok_or_else(|| "S-ADM flow XML size overflow".to_string())?;
        if total_xml_bytes > MAX_SADM_TOTAL_XML_BYTES {
            return Err(format!(
                "S-ADM flow XML size exceeds {MAX_SADM_TOTAL_XML_BYTES} bytes"
            ));
        }
    }
    let mut frames = Vec::with_capacity(paths.len());
    let mut parsed_frames = Vec::with_capacity(paths.len());
    let mut actual_xml_bytes = 0_u64;
    let mut total_canonical_bytes = 0_u64;
    for (offset, path) in paths.iter().enumerate() {
        let file =
            fs::File::open(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let mut xml = Vec::new();
        file.take(MAX_SADM_XML_BYTES + 1)
            .read_to_end(&mut xml)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if u64::try_from(xml.len()).map_or(true, |bytes| bytes > MAX_SADM_XML_BYTES) {
            return Err(format!(
                "read {}: XML size exceeds {MAX_SADM_XML_BYTES} bytes",
                path.display()
            ));
        }
        actual_xml_bytes = actual_xml_bytes
            .checked_add(u64::try_from(xml.len()).map_err(|_| "S-ADM flow XML size overflow")?)
            .ok_or_else(|| "S-ADM flow XML size overflow".to_string())?;
        if actual_xml_bytes > MAX_SADM_TOTAL_XML_BYTES {
            return Err(format!(
                "S-ADM flow XML size exceeds {MAX_SADM_TOTAL_XML_BYTES} bytes"
            ));
        }
        let parsed =
            parse_frame(&xml).map_err(|error| format!("parse {}: {error}", path.display()))?;
        total_canonical_bytes = total_canonical_bytes
            .checked_add(
                u64::try_from(parsed.canonical_bytes)
                    .map_err(|_| "S-ADM canonical output size overflow")?,
            )
            .ok_or_else(|| "S-ADM canonical output size overflow".to_string())?;
        if total_canonical_bytes > MAX_SADM_TOTAL_CANONICAL_BYTES {
            return Err(format!(
                "S-ADM flow canonical output exceeds {MAX_SADM_TOTAL_CANONICAL_BYTES} bytes"
            ));
        }
        let frame = validate_frame(offset + 1, path, &parsed);
        frames.push(frame);
        parsed_frames.push(parsed);
    }

    let logical_frames = group_logical_frames(&parsed_frames);
    let logical_types = logical_frames
        .iter()
        .map(|logical| uniform_attribute(logical, &parsed_frames, "type"))
        .collect::<Vec<_>>();
    let flow_kind = classify_flow(&logical_types);
    let mut flow_rules = Vec::new();

    let first_type = logical_types.first().copied().flatten();
    flow_rules.push(rule(
        "BS2125-FIRST-FRAME",
        "/flow/frame[1]/frameHeader/frameFormat/@type",
        "the first frame type shall be header, full, all, or divided for a divided-frame flow",
        first_type.unwrap_or("missing or inconsistent"),
        matches!(first_type, Some("header" | "full" | "all" | "divided")),
    ));

    flow_rules.push(rule(
        "BS2125-FLOW-TYPE",
        "/flow/frame/frameHeader/frameFormat/@type",
        "the logical frame types shall form a Full-Frame, Intermediate-Frame, Mixed-Frame, or Divided-Frame flow",
        flow_kind.name(),
        flow_kind != FlowKind::Invalid,
    ));

    flow_rules.push(rule(
        "BS2125-LOGICAL-FRAME-COUNT",
        "/flow/frame/frameHeader/frameFormat/@frameFormatID",
        "divided metadata chunks with the same base frame index represent one logical frame",
        format!(
            "{} input frame document(s), {} logical frame(s)",
            parsed_frames.len(),
            logical_frames.len()
        ),
        true,
    ));

    let flow_ids = parsed_frames
        .iter()
        .filter_map(|frame| attribute(frame, "flowID"))
        .collect::<Vec<_>>();
    let flow_id = flow_ids.first().map(|value| (*value).to_owned());
    let flow_ids_fixed = flow_ids
        .iter()
        .all(|value| Some(*value) == flow_id.as_deref());
    flow_rules.push(rule(
        "BS2125-FLOW-ID-FIXED",
        "/flow/frame/frameHeader/frameFormat/@flowID",
        "flowID, when present, shall be a fixed RFC 4122 UUID for the flow",
        flow_id.as_deref().unwrap_or("not present"),
        flow_ids_fixed && flow_id.as_deref().is_none_or(valid_uuid),
    ));

    let time_reference = attribute(&parsed_frames[0], "timeReference").unwrap_or("total");
    let time_reference_fixed = parsed_frames
        .iter()
        .all(|frame| attribute(frame, "timeReference").unwrap_or("total") == time_reference);
    flow_rules.push(rule(
        "BS2125-TIME-REFERENCE-FIXED",
        "/flow/frame/frameHeader/frameFormat/@timeReference",
        "timeReference shall be total or local and fixed for the entire flow",
        time_reference,
        time_reference_fixed && matches!(time_reference, "total" | "local"),
    ));

    let logical_indices = logical_frames
        .iter()
        .map(|logical| logical.base)
        .collect::<Vec<_>>();
    let sequential = logical_indices
        .iter()
        .enumerate()
        .all(|(offset, value)| *value == Some((offset + 1) as u64));
    flow_rules.push(rule(
        "BS2125-LOGICAL-FRAME-SEQUENCE",
        "/flow/frame/frameHeader/frameFormat/@frameFormatID",
        "the hexadecimal base frame index shall start at 1 and increment once per logical frame",
        logical_indices
            .iter()
            .map(|value| value.map_or_else(|| "invalid".into(), |value| value.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        sequential,
    ));
    flow_rules.push(rule(
        "BS2125-FRAME-SEQUENCE",
        "/flow/frame/frameHeader/frameFormat/@frameFormatID",
        "the hexadecimal frame index shall start at 1 and increment by 1",
        logical_indices
            .iter()
            .map(|value| value.map_or_else(|| "invalid".into(), |value| value.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        sequential,
    ));

    let logical_times = logical_frames
        .iter()
        .map(|logical| uniform_frame_time(logical, &parsed_frames))
        .collect::<Vec<_>>();
    let group_times_fixed = logical_times.iter().all(Option::is_some);
    flow_rules.push(rule(
        "BS2125-DIVIDED-FRAME-TIME",
        "/flow/frame/frameHeader/frameFormat",
        "all chunks of a divided logical frame shall have identical start and duration values",
        if group_times_fixed {
            "all logical frame timings are internally consistent"
        } else {
            "a logical frame contains missing, invalid, or inconsistent timing"
        },
        group_times_fixed,
    ));

    let contiguous = logical_times
        .windows(2)
        .all(|pair| match (pair[0], pair[1]) {
            (Some((start, duration)), Some((next, _))) => {
                start.checked_add(duration).is_some_and(|end| end == next)
            }
            _ => false,
        });
    flow_rules.push(rule(
        "BS2125-LOGICAL-FRAME-CONTIGUITY",
        "/flow/frame/frameHeader/frameFormat",
        "logical S-ADM frames shall be non-overlapping and exactly contiguous",
        logical_times
            .iter()
            .map(|value| {
                value.map_or_else(
                    || "invalid or inconsistent".into(),
                    |(start, duration)| format!("{start}+{duration}"),
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
        group_times_fixed && contiguous,
    ));
    flow_rules.push(rule(
        "BS2125-FRAME-CONTIGUITY",
        "/flow/frame/frameHeader/frameFormat",
        "S-ADM frames shall be non-overlapping and contiguous",
        logical_times
            .iter()
            .map(|value| {
                value.map_or_else(
                    || "invalid or inconsistent".into(),
                    |(start, duration)| format!("{start}+{duration}"),
                )
            })
            .collect::<Vec<_>>()
            .join(", "),
        group_times_fixed && contiguous,
    ));

    append_divided_flow_rules(&mut flow_rules, &logical_frames, &parsed_frames);
    append_count_to_full_rule(
        &mut flow_rules,
        flow_kind,
        &logical_frames,
        &logical_types,
        &parsed_frames,
    );
    append_state_rules(&mut flow_rules, &logical_frames, &parsed_frames);

    let passed =
        frames.iter().all(|frame| frame.passed) && flow_rules.iter().all(|item| item.passed);
    Ok(SadmAudit {
        standard: STANDARD,
        standard_version: VERSION,
        validator: VALIDATOR,
        frame_count: frames.len(),
        flow_id,
        time_reference: time_reference.into(),
        passed,
        flow_rules,
        frames,
    })
}

fn append_divided_flow_rules(
    rules: &mut Vec<SadmRule>,
    logical_frames: &[LogicalFrame],
    parsed_frames: &[ParsedFrame],
) {
    let divided = parsed_frames
        .iter()
        .filter(|frame| attribute(frame, "type") == Some("divided"))
        .collect::<Vec<_>>();
    if divided.is_empty() {
        return;
    }

    let metadata_chunk_counts = divided
        .iter()
        .map(|frame| parse_decimal_u16(attribute(frame, "numMetadataChunks")))
        .collect::<Vec<_>>();
    let expected_count = metadata_chunk_counts.first().copied().flatten();
    let count_fixed = expected_count.is_some_and(|expected| {
        (2..=255).contains(&expected)
            && metadata_chunk_counts
                .iter()
                .all(|count| *count == Some(expected))
    });
    rules.push(rule(
        "BS2125-DIVIDED-CHUNK-COUNT-FIXED",
        "/flow/frame/frameHeader/frameFormat/@numMetadataChunks",
        "numMetadataChunks shall be at least 2 and fixed for every divided frame in the flow",
        metadata_chunk_counts
            .iter()
            .map(|count| count.map_or_else(|| "invalid".into(), |value| value.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        count_fixed,
    ));

    let groups_valid = expected_count.is_some_and(|expected| {
        logical_frames.iter().all(|logical| {
            let Some(kind) = uniform_attribute(logical, parsed_frames, "type") else {
                return false;
            };
            if kind != "divided" {
                return logical.members.len() == 1;
            }
            let chunks = logical
                .members
                .iter()
                .map(|index| {
                    attribute(&parsed_frames[*index], "frameFormatID")
                        .and_then(parse_frame_format_id)
                        .and_then(|id| id.chunk)
                })
                .collect::<Vec<_>>();
            chunks
                .iter()
                .all(|chunk| chunk.is_some_and(|chunk| u16::from(chunk) <= expected))
                && chunks.windows(2).all(|pair| pair[0] < pair[1])
                && chunks
                    .iter()
                    .any(|chunk| *chunk == u8::try_from(expected).ok())
        })
    });
    rules.push(rule(
        "BS2125-DIVIDED-CHUNK-SEQUENCE",
        "/flow/frame/frameHeader/frameFormat/@frameFormatID",
        "divided chunk indices shall be unique, ascending, within numMetadataChunks, and include the final chunk index reserved to carry dynamic metadata in every logical frame",
        describe_logical_chunks(logical_frames, parsed_frames),
        groups_valid,
    ));

    let member_chunks = parsed_frames
        .iter()
        .map(|frame| {
            attribute(frame, "frameFormatID")
                .and_then(parse_frame_format_id)
                .and_then(|id| id.chunk)
        })
        .collect::<Vec<_>>();
    let mut next_same_chunk_by_member = vec![None; parsed_frames.len()];
    let mut next_logical_by_chunk = [None; 256];
    for (logical_index, logical) in logical_frames.iter().enumerate().rev() {
        for member in &logical.members {
            if let Some(chunk) = member_chunks[*member] {
                next_same_chunk_by_member[*member] = next_logical_by_chunk[usize::from(chunk)];
            }
        }
        for member in &logical.members {
            if let Some(chunk) = member_chunks[*member] {
                next_logical_by_chunk[usize::from(chunk)] = Some(logical_index);
            }
        }
    }

    let mut recurrence_checked = 0_usize;
    let mut recurrence_valid = true;
    for (logical_index, logical) in logical_frames.iter().enumerate() {
        for member in &logical.members {
            let frame = &parsed_frames[*member];
            if attribute(frame, "type") != Some("divided") {
                continue;
            }
            let Some(count) = parse_decimal_u64(attribute(frame, "countToSameChunk")) else {
                continue;
            };
            if count == 0 {
                recurrence_valid = false;
                continue;
            }
            let Ok(distance) = usize::try_from(count) else {
                continue;
            };
            let next_occurrence = next_same_chunk_by_member[*member];
            if let Some(next_occurrence) = next_occurrence {
                recurrence_checked += 1;
                recurrence_valid &= next_occurrence - logical_index == distance;
            } else if logical_index
                .checked_add(distance)
                .is_some_and(|target| target < logical_frames.len())
            {
                recurrence_checked += 1;
                recurrence_valid = false;
            }
        }
    }
    rules.push(rule(
        "BS2125-DIVIDED-CHUNK-RECURRENCE",
        "/flow/frame/frameHeader/frameFormat/@countToSameChunk",
        "countToSameChunk, when its target is inside the supplied flow, shall point to the next frame carrying the same chunk",
        format!("{recurrence_checked} in-range recurrence declaration(s) checked"),
        recurrence_valid,
    ));
}

fn append_count_to_full_rule(
    rules: &mut Vec<SadmRule>,
    flow_kind: FlowKind,
    logical_frames: &[LogicalFrame],
    logical_types: &[Option<&str>],
    parsed_frames: &[ParsedFrame],
) {
    let mut checked = 0_usize;
    let mut valid = true;
    let mut next_full_by_logical = vec![None; logical_frames.len()];
    let mut next_full = None;
    for logical_index in (0..logical_frames.len()).rev() {
        next_full_by_logical[logical_index] = next_full;
        if logical_types[logical_index] == Some("full") {
            next_full = Some(logical_index);
        }
    }
    for (logical_index, logical) in logical_frames.iter().enumerate() {
        for member in &logical.members {
            let frame = &parsed_frames[*member];
            let value = attribute(frame, "countToFull");
            let parsed = parse_decimal_u64(value);
            if value.is_some() && parsed.is_none() {
                valid = false;
                continue;
            }
            match flow_kind {
                FlowKind::FullFrame => {
                    checked += 1;
                    valid &= parsed.unwrap_or(1) == 1;
                }
                FlowKind::IntermediateFrame => {
                    checked += 1;
                    valid &= parsed.unwrap_or(0) == 0;
                }
                FlowKind::MixedFrame => {
                    let Some(count) = parsed else {
                        continue;
                    };
                    checked += 1;
                    if count == 0 {
                        valid = false;
                        continue;
                    }
                    let Ok(distance) = usize::try_from(count) else {
                        continue;
                    };
                    let next_full = next_full_by_logical[logical_index];
                    if let Some(next_full) = next_full {
                        valid &= next_full - logical_index == distance;
                    } else if logical_index
                        .checked_add(distance)
                        .is_some_and(|target| target < logical_frames.len())
                    {
                        valid = false;
                    }
                }
                FlowKind::DividedFrame => {
                    if value.is_some() {
                        checked += 1;
                        valid = false;
                    }
                }
                FlowKind::Indeterminate | FlowKind::Invalid => {}
            }
        }
    }
    rules.push(rule(
        "BS2125-COUNT-TO-FULL",
        "/flow/frame/frameHeader/frameFormat/@countToFull",
        "countToFull shall be 1 in FF, 0 in IF, point to the next full frame in MF when verifiable, and be absent in DF",
        format!("{checked} countToFull value or default(s) checked"),
        valid,
    ));
}

fn append_state_rules(
    rules: &mut Vec<SadmRule>,
    logical_frames: &[LogicalFrame],
    parsed_frames: &[ParsedFrame],
) {
    let mut shape_valid = parsed_frames
        .iter()
        .all(|frame| frame.changed_ids <= 1 && frame.changed_shape_errors.is_empty());
    let mut reconstruction_valid = parsed_frames
        .iter()
        .all(|frame| frame.payload_path_valid && frame.state_errors.is_empty());
    let mut state_valid = true;
    let mut declarations_checked = 0_usize;
    let mut mismatches = Vec::new();
    for (frame_offset, frame) in parsed_frames.iter().enumerate() {
        for error in &frame.state_errors {
            push_state_message(
                &mut mismatches,
                format!("input frame {}: {error}", frame_offset + 1),
            );
        }
    }
    let mut previous = HashMap::<StateKey, &AdmElement>::new();
    let mut ever_seen = HashSet::<StateKey>::new();

    for (logical_offset, logical) in logical_frames.iter().enumerate() {
        let logical_number = logical_offset + 1;
        let mut incoming = HashMap::<StateKey, &AdmElement>::new();
        let mut declarations = Vec::<&ChangedDeclaration>::new();
        let mut declared = HashSet::<StateKey>::new();
        for member in &logical.members {
            let frame = &parsed_frames[*member];
            for element in &frame.adm_elements {
                if incoming.insert(element.key.clone(), element).is_some() {
                    reconstruction_valid = false;
                    push_state_message(
                        &mut mismatches,
                        format!(
                            "logical frame {logical_number} repeats {} {} across payload chunks",
                            element.key.kind, element.key.id
                        ),
                    );
                }
            }
            for declaration in &frame.changed_declarations {
                if declaration
                    .key
                    .as_ref()
                    .is_some_and(|key| !declared.insert(key.clone()))
                {
                    shape_valid = false;
                    push_state_message(
                        &mut mismatches,
                        format!(
                            "logical frame {logical_number} repeats changed ID {}",
                            declaration.key.as_ref().unwrap().id
                        ),
                    );
                }
                declarations.push(declaration);
            }
        }

        let frame_type = uniform_attribute(logical, parsed_frames, "type");
        let snapshot = matches!(frame_type, Some("header" | "full" | "all"));
        let patch = matches!(frame_type, Some("intermediate" | "divided"));
        if !snapshot && !patch {
            reconstruction_valid = false;
            push_state_message(
                &mut mismatches,
                format!("logical frame {logical_number} has no snapshot or patch frame type"),
            );
        }

        for declaration in &declarations {
            declarations_checked = declarations_checked.saturating_add(1);
            let (Some(key), Some(status)) =
                (declaration.key.as_ref(), declaration.status.as_deref())
            else {
                state_valid = false;
                continue;
            };
            let before = previous.get(key);
            let after = if snapshot {
                incoming.get(key)
            } else if let Some(element) = incoming.get(key) {
                Some(element)
            } else if patch && status == "expired" {
                None
            } else {
                previous.get(key)
            };
            let declaration_valid = match status {
                "new" => !ever_seen.contains(key) && after.is_some(),
                "changed" => before.zip(after).is_some_and(|(before, after)| {
                    before.canonical != after.canonical
                        && before.canonical_without_timing != after.canonical_without_timing
                }),
                "extended" => before.zip(after).is_some_and(|(before, after)| {
                    before.canonical != after.canonical
                        && before.canonical_without_timing == after.canonical_without_timing
                }),
                "expired" => before.is_some() && after.is_none(),
                _ => false,
            };
            if !declaration_valid {
                state_valid = false;
                push_state_message(
                    &mut mismatches,
                    format!(
                        "logical frame {logical_number}: {status} is false for {} {}",
                        key.kind, key.id
                    ),
                );
            }
        }
        ever_seen.extend(incoming.keys().cloned());
        if snapshot {
            previous = incoming;
        } else if patch {
            for declaration in declarations {
                if declaration.status.as_deref() == Some("expired") {
                    if let Some(key) = declaration.key.as_ref() {
                        if !incoming.contains_key(key) {
                            previous.remove(key);
                        }
                    }
                }
            }
            previous.extend(incoming);
        }
    }

    rules.push(rule(
        "BS2125-CHANGED-IDS-SHAPE",
        "/flow/frame/frameHeader/frameFormat/changedIDs",
        "changed ID declarations shall remain unique after divided chunks are combined into logical frames",
        if shape_valid {
            "all declarations are structurally valid and unique per logical frame".to_owned()
        } else {
            "a declaration is malformed or repeated in one logical frame".to_owned()
        },
        shape_valid,
    ));
    rules.push(rule(
        "BS2125-CHANGED-IDS-STATE",
        "/flow/frame/frameHeader/frameFormat/changedIDs/*",
        "declared new, changed, extended, and expired statuses shall agree with the reconstructed previous and current ADM states",
        if mismatches.is_empty() {
            format!("{declarations_checked} declaration(s) checked with no mismatch")
        } else {
            format!(
                "{declarations_checked} declaration(s) checked; {}",
                mismatches.join("; ")
            )
        },
        shape_valid && reconstruction_valid && state_valid,
    ));
    rules.push(rule(
        "BS2125-STATE-RECONSTRUCTION",
        "/flow/frame/audioFormatExtended",
        "header, full, and all frames shall form snapshots; intermediate and divided logical frames shall patch the prior ADM state",
        if mismatches.is_empty() {
            format!(
                "{} logical frame(s) reconstructed; {} ADM element(s) in final state",
                logical_frames.len(),
                previous.len()
            )
        } else {
            format!(
                "{} logical frame(s) reconstructed; {} ADM element(s) in final state; evidence: {}",
                logical_frames.len(),
                previous.len(),
                mismatches.join("; ")
            )
        },
        reconstruction_valid,
    ));
}

fn push_state_message(messages: &mut Vec<String>, message: String) {
    const MAX_REPORTED_STATE_MESSAGES: usize = 8;
    if messages.len() < MAX_REPORTED_STATE_MESSAGES {
        messages.push(message);
    }
}

fn validate_frame(index: usize, path: &Path, parsed: &ParsedFrame) -> SadmFrameAudit {
    let mut rules = vec![
        rule(
            "BS2125-FRAME-ROOT",
            "/frame",
            "exactly one frame root element",
            format!("{} frame element(s)", parsed.roots),
            parsed.roots == 1,
        ),
        rule(
            "BS2125-FRAME-VERSION",
            "/frame/@version",
            "version shall be exactly ITU-R_BS.2125-1",
            match parsed.version.as_deref() {
                Some(value) => value.to_owned(),
                None => "missing (interpreted as ITU-R BS.2125-0)".into(),
            },
            parsed.version.as_deref() == Some("ITU-R_BS.2125-1"),
        ),
        rule(
            "BS2125-FRAME-HEADER",
            "/frame/frameHeader",
            "exactly one frameHeader",
            format!("{} frameHeader element(s)", parsed.frame_headers),
            parsed.frame_headers == 1,
        ),
        rule(
            "BS2125-FRAME-FORMAT",
            "/frame/frameHeader/frameFormat",
            "exactly one frameFormat",
            format!("{} frameFormat element(s)", parsed.frame_formats),
            parsed.frame_formats == 1,
        ),
        rule(
            "BS2125-TRANSPORT-TRACK-FORMAT",
            "/frame/frameHeader/transportTrackFormat",
            "one or more transportTrackFormat elements",
            format!(
                "{} transportTrackFormat element(s)",
                parsed.transport_track_formats
            ),
            parsed.transport_track_formats >= 1,
        ),
        rule(
            "BS2125-AUDIO-FORMAT-EXTENDED",
            "/frame/audioFormatExtended | /frame/coreMetadata/format/audioFormatExtended",
            "exactly one direct audioFormatExtended, or exactly one coreMetadata containing exactly one direct format with exactly one audioFormatExtended, shall be used exclusively",
            format!(
                "{} direct, {} coreMetadata, {} format under coreMetadata, {} audioFormatExtended under coreMetadata/format ({} payload candidate(s))",
                parsed.direct_audio_format_extended,
                parsed.core_metadata,
                parsed.core_metadata_formats,
                parsed.nested_audio_format_extended,
                parsed.audio_format_extended,
            ),
            parsed.payload_path_valid,
        ),
        rule(
            "BS2125-STRUCTURAL-PATHS",
            "/frame",
            "known unqualified S-ADM structural elements shall occur only at their defined paths; namespace-qualified extension names remain distinct",
            if parsed.structural_shape_errors.is_empty() {
                "all known unqualified structural elements occur at defined paths".into()
            } else {
                parsed.structural_shape_errors.join("; ")
            },
            parsed.structural_shape_errors.is_empty(),
        ),
    ];
    let id = attribute(parsed, "frameFormatID");
    let parsed_id = id.and_then(parse_frame_format_id);
    rules.push(rule(
        "BS2125-FRAME-FORMAT-ID",
        "/frame/frameHeader/frameFormat/@frameFormatID",
        "FF_ plus an 8-digit (or legacy 11-digit) hexadecimal index and optional non-zero two-digit divided-frame chunk",
        id.unwrap_or("missing"),
        parsed_id.is_some(),
    ));
    let frame_type = attribute(parsed, "type");
    rules.push(rule(
        "BS2125-FRAME-TYPE",
        "/frame/frameHeader/frameFormat/@type",
        "header, full, divided, intermediate, or all",
        frame_type.unwrap_or("missing"),
        matches!(
            frame_type,
            Some("header" | "full" | "divided" | "intermediate" | "all")
        ),
    ));

    let divided_id_valid = match frame_type {
        Some("divided") => parsed_id.is_some_and(|value| value.chunk.is_some()),
        Some("header" | "full" | "intermediate" | "all") => {
            parsed_id.is_some_and(|value| value.chunk.is_none())
        }
        _ => false,
    };
    rules.push(rule(
        "BS2125-FRAME-ID-MODE",
        "/frame/frameHeader/frameFormat/@frameFormatID",
        "only divided frames shall use the _zz metadata chunk suffix, and every divided frame shall use it",
        id.unwrap_or("missing"),
        divided_id_valid,
    ));

    let metadata_chunks = parse_decimal_u16(attribute(parsed, "numMetadataChunks"));
    let count_to_same = parse_decimal_u64(attribute(parsed, "countToSameChunk"));
    let divided_attributes_valid = match frame_type {
        Some("divided") => {
            metadata_chunks.is_some_and(|count| {
                (2..=255).contains(&count)
                    && parsed_id
                        .and_then(|value| value.chunk)
                        .is_some_and(|chunk| u16::from(chunk) <= count)
            }) && attribute(parsed, "countToSameChunk")
                .is_none_or(|_| count_to_same.is_some_and(|count| count > 0))
                && attribute(parsed, "countToFull").is_none()
        }
        Some("header" | "full" | "intermediate" | "all") => {
            attribute(parsed, "numMetadataChunks").is_none()
                && attribute(parsed, "countToSameChunk").is_none()
        }
        _ => false,
    };
    rules.push(rule(
        "BS2125-DIVIDED-ATTRIBUTES",
        "/frame/frameHeader/frameFormat",
        "divided frames shall declare 2..255 metadata chunks and optional positive countToSameChunk; other frame types shall omit divided-only attributes",
        format!(
            "numMetadataChunks={}, countToSameChunk={}",
            attribute(parsed, "numMetadataChunks").unwrap_or("not present"),
            attribute(parsed, "countToSameChunk").unwrap_or("not present")
        ),
        divided_attributes_valid,
    ));

    for name in ["start", "duration"] {
        let value = attribute(parsed, name);
        let parsed_time = value.and_then(|value| {
            if name == "start" {
                parse_start_time(value)
            } else {
                parse_duration_time(value)
            }
        });
        rules.push(rule(
            if name == "start" {
                "BS2125-FRAME-START"
            } else {
                "BS2125-FRAME-DURATION"
            },
            format!("/frame/frameHeader/frameFormat/@{name}"),
            if name == "start" {
                "a valid non-negative BS.2125 time value with exact decimal or sample precision"
            } else {
                "a positive BS.2125 duration with exact decimal or sample precision"
            },
            value.unwrap_or("missing"),
            parsed_time.is_some_and(|time| name == "start" || time.is_positive()),
        ));
    }
    rules.push(rule(
        "BS2125-CHANGED-IDS-SHAPE",
        "/frame/frameHeader/frameFormat/changedIDs",
        "at most one changedIDs list shall contain only direct, unique, non-empty references of the eight permitted kinds with a required valid status",
        if parsed.changed_shape_errors.is_empty() {
            format!(
                "{} changedIDs list(s), {} valid declaration(s)",
                parsed.changed_ids,
                parsed.changed_declarations.len()
            )
        } else {
            parsed.changed_shape_errors.join("; ")
        },
        parsed.changed_ids <= 1 && parsed.changed_shape_errors.is_empty(),
    ));
    let invalid_statuses = parsed
        .changed_declarations
        .iter()
        .filter(|declaration| {
            !matches!(
                declaration.status.as_deref(),
                Some("new" | "changed" | "expired" | "extended")
            )
        })
        .count();
    rules.push(rule(
        "BS2125-CHANGED-IDS-STATUS",
        "/frame/frameHeader/frameFormat/changedIDs/*/@status",
        "each changed ID status shall be present and be new, changed, expired, or extended",
        if invalid_statuses == 0 {
            "all declared statuses valid".into()
        } else {
            format!("{invalid_statuses} missing or invalid status value(s)")
        },
        invalid_statuses == 0,
    ));
    SadmFrameAudit {
        index,
        path: path.to_string_lossy().into_owned(),
        frame_format_id: id.map(str::to_owned),
        frame_type: frame_type.map(str::to_owned),
        start: attribute(parsed, "start").map(str::to_owned),
        duration: attribute(parsed, "duration").map(str::to_owned),
        passed: rules.iter().all(|item| item.passed),
        rules,
    }
}

fn parse_frame(xml: &[u8]) -> Result<ParsedFrame, String> {
    let nodes = parse_xml_tree(xml)?;
    analyze_frame_tree(&nodes)
}

fn parse_xml_tree(xml: &[u8]) -> Result<Vec<XmlNode>, String> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    reader.config_mut().check_comments = true;
    let mut nodes = Vec::<XmlNode>::new();
    let mut stack = Vec::<usize>::new();
    let mut root_elements = 0_usize;
    let mut declaration_seen = false;
    let mut event_before_declaration = false;
    let mut text_bytes = 0_usize;
    let mut attribute_bytes = 0_usize;
    let mut name_pool = XmlNamePool::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if stack.len() >= MAX_SADM_XML_DEPTH {
                    return Err(format!("XML nesting depth exceeds {MAX_SADM_XML_DEPTH}"));
                }
                if stack.is_empty() {
                    root_elements = root_elements
                        .checked_add(1)
                        .ok_or_else(|| "XML root element count overflow".to_string())?;
                    if root_elements > 1 {
                        return Err("XML document contains more than one root element".into());
                    }
                }
                event_before_declaration = true;
                let index = push_xml_node(
                    &reader,
                    &element,
                    stack.last().copied(),
                    &mut nodes,
                    &mut attribute_bytes,
                    &mut name_pool,
                )?;
                stack.push(index);
            }
            Ok(Event::Empty(element)) => {
                if stack.len() >= MAX_SADM_XML_DEPTH {
                    return Err(format!("XML nesting depth exceeds {MAX_SADM_XML_DEPTH}"));
                }
                if stack.is_empty() {
                    root_elements = root_elements
                        .checked_add(1)
                        .ok_or_else(|| "XML root element count overflow".to_string())?;
                    if root_elements > 1 {
                        return Err("XML document contains more than one root element".into());
                    }
                }
                event_before_declaration = true;
                push_xml_node(
                    &reader,
                    &element,
                    stack.last().copied(),
                    &mut nodes,
                    &mut attribute_bytes,
                    &mut name_pool,
                )?;
            }
            Ok(Event::Text(text)) => {
                event_before_declaration = true;
                if text.as_ref().contains("]]>") {
                    return Err(
                        "XML character data shall not contain the literal ]]> sequence".into(),
                    );
                }
                let encoded = text.xml_content(XmlVersion::Implicit1_0);
                let value = quick_xml::escape::unescape(&encoded)
                    .map_err(|error| format!("XML entity: {error}"))?;
                validate_xml_chars(&value, "text")?;
                if let Some(parent) = stack.last().copied() {
                    add_xml_text_bytes(&mut text_bytes, value.len())?;
                    append_xml_text(&mut nodes[parent], &value);
                } else if !is_xml_space_only(&value) {
                    return Err("XML document contains non-whitespace text outside its root".into());
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(parent) = stack.last().copied() {
                    event_before_declaration = true;
                    let value = text.xml_content(XmlVersion::Implicit1_0);
                    validate_xml_chars(&value, "CDATA")?;
                    add_xml_text_bytes(&mut text_bytes, value.len())?;
                    append_xml_text(&mut nodes[parent], &value);
                } else {
                    return Err("XML document contains CDATA outside its root".into());
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if let Some(parent) = stack.last().copied() {
                    event_before_declaration = true;
                    let encoded = format!("&{};", reference.xml_content(XmlVersion::Implicit1_0));
                    let value = quick_xml::escape::unescape(&encoded)
                        .map_err(|error| format!("XML entity: {error}"))?;
                    validate_xml_chars(&value, "entity reference")?;
                    add_xml_text_bytes(&mut text_bytes, value.len())?;
                    append_xml_text(&mut nodes[parent], &value);
                } else {
                    return Err("XML document contains an entity reference outside its root".into());
                }
            }
            Ok(Event::Decl(declaration)) => {
                if declaration_seen || event_before_declaration || !stack.is_empty() {
                    return Err(
                        "XML declaration shall occur once, at the start of the document".into(),
                    );
                }
                validate_xml_declaration(&declaration)?;
                declaration_seen = true;
                event_before_declaration = true;
            }
            Ok(Event::DocType(_)) => {
                return Err("XML document types are not accepted".into());
            }
            Ok(Event::End(element)) => {
                event_before_declaration = true;
                let index = stack
                    .pop()
                    .ok_or_else(|| "closing element without an open element".to_string())?;
                validate_xml_qname(element.name().as_ref(), "closing element")?;
                let actual = resolve_element_name(&reader, element.name(), &mut name_pool)?;
                if nodes[index].name != actual {
                    return Err(format!(
                        "closing element {} does not match {}",
                        actual.local, nodes[index].name.local
                    ));
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("XML at byte {}: {error}", reader.error_position())),
            Ok(Event::Comment(comment)) => {
                let value = comment.xml_content(XmlVersion::Implicit1_0);
                validate_xml_chars(&value, "comment")?;
                add_xml_text_bytes(&mut text_bytes, value.len())?;
                event_before_declaration = true;
            }
            Ok(Event::PI(instruction)) => {
                validate_xml_name(instruction.target(), "processing-instruction target")?;
                if instruction.target().eq_ignore_ascii_case("xml") {
                    return Err("processing-instruction target shall not be XML".into());
                }
                validate_xml_chars(instruction.content(), "processing instruction")?;
                add_xml_text_bytes(&mut text_bytes, instruction.content().len())?;
                event_before_declaration = true;
            }
        }
    }
    if !stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    if root_elements != 1 {
        return Err(format!(
            "XML document shall contain exactly one root element, found {root_elements}"
        ));
    }
    Ok(nodes)
}

fn push_xml_node(
    reader: &NsReader<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    parent: Option<usize>,
    nodes: &mut Vec<XmlNode>,
    attribute_bytes: &mut usize,
    name_pool: &mut XmlNamePool,
) -> Result<usize, String> {
    if nodes.len() >= MAX_SADM_XML_ELEMENTS {
        return Err(format!("XML element count exceeds {MAX_SADM_XML_ELEMENTS}"));
    }
    validate_xml_qname(element.name().as_ref(), "element")?;
    register_namespace_declarations(element, name_pool)?;
    let name = resolve_element_name(reader, element.name(), name_pool)?;
    let mut attributes = Vec::new();
    let mut expanded_names = HashSet::new();
    for (offset, attribute) in element.attributes().enumerate() {
        if offset >= MAX_SADM_XML_ATTRIBUTES_PER_ELEMENT {
            return Err(format!(
                "XML element {} contains more than {MAX_SADM_XML_ATTRIBUTES_PER_ELEMENT} attributes",
                name.local
            ));
        }
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let raw_name = attribute.key.as_ref();
        validate_xml_qname(raw_name, "attribute")?;
        if raw_name == "xmlns" || raw_name.starts_with("xmlns:") {
            continue;
        }
        validate_xml_attribute_lexical_value(attribute.value.as_ref(), "attribute value")?;
        let name = resolve_attribute_name(reader, attribute.key, name_pool)?;
        if !expanded_names.insert(name.clone()) {
            return Err(format!("XML repeats attribute {}", name.local));
        }
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("XML attribute {}: {error}", name.local))?
            .into_owned();
        validate_xml_chars(&value, "attribute value")?;
        let expanded_bytes = name
            .local
            .len()
            .checked_add(name.namespace.as_ref().map_or(0, |uri| uri.len()))
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(|| "XML attribute byte count overflow".to_string())?;
        *attribute_bytes = attribute_bytes
            .checked_add(expanded_bytes)
            .ok_or_else(|| "XML attribute byte count overflow".to_string())?;
        if *attribute_bytes > MAX_SADM_XML_ATTRIBUTE_BYTES {
            return Err(format!(
                "XML attribute data exceeds {MAX_SADM_XML_ATTRIBUTE_BYTES} bytes"
            ));
        }
        attributes.push(XmlAttribute { name, value });
    }
    let index = nodes.len();
    nodes.push(XmlNode {
        name,
        parent,
        attributes,
        content: Vec::new(),
    });
    if let Some(parent) = parent {
        nodes[parent].content.push(XmlContent::Child(index));
    }
    Ok(index)
}

fn register_namespace_declarations(
    element: &quick_xml::events::BytesStart<'_>,
    name_pool: &mut XmlNamePool,
) -> Result<(), String> {
    for (offset, attribute) in element.attributes().enumerate() {
        if offset >= MAX_SADM_XML_ATTRIBUTES_PER_ELEMENT {
            return Err(format!(
                "XML element contains more than {MAX_SADM_XML_ATTRIBUTES_PER_ELEMENT} attributes"
            ));
        }
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let raw_name = attribute.key.as_ref();
        validate_xml_qname(raw_name, "attribute")?;
        let prefix = if raw_name == "xmlns" {
            Some(None)
        } else {
            raw_name.strip_prefix("xmlns:").map(Some)
        };
        let Some(prefix) = prefix else {
            continue;
        };
        validate_xml_attribute_lexical_value(attribute.value.as_ref(), "namespace declaration")?;
        let normalized = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("XML namespace declaration: {error}"))?;
        validate_xml_chars(&normalized, "namespace URI")?;
        validate_namespace_declaration(prefix, &normalized)?;
        name_pool.register_namespace(attribute.value.as_ref(), &normalized)?;
    }
    Ok(())
}

fn add_xml_text_bytes(total: &mut usize, bytes: usize) -> Result<(), String> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| "XML text byte count overflow".to_string())?;
    if *total > MAX_SADM_XML_TEXT_BYTES {
        return Err(format!(
            "XML text data exceeds {MAX_SADM_XML_TEXT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_xml_declaration(declaration: &quick_xml::events::BytesDecl<'_>) -> Result<(), String> {
    let mut stage = 0_u8;
    let pseudo_element =
        quick_xml::events::BytesStart::from_content(declaration.as_ref(), "xml".len());
    for attribute in pseudo_element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML declaration: {error}"))?;
        let name = attribute.key.as_ref();
        validate_xml_attribute_lexical_value(attribute.value.as_ref(), "XML declaration")?;
        if attribute.value.contains('&') {
            return Err(format!(
                "XML declaration {name} shall not contain an entity reference"
            ));
        }
        let value = attribute.value;
        match (stage, name) {
            (0, "version") if value == "1.0" => stage = 1,
            (0, "version") => return Err("XML declaration version shall be 1.0".into()),
            (1, "encoding") if value.eq_ignore_ascii_case("UTF-8") => stage = 2,
            (1 | 2, "encoding") => {
                return Err("XML declaration encoding shall be UTF-8 and occur once".into());
            }
            (1 | 2, "standalone") if matches!(value.as_ref(), "yes" | "no") => stage = 3,
            (1 | 2, "standalone") => {
                return Err("XML declaration standalone shall be yes or no".into());
            }
            _ => {
                return Err(format!(
                    "XML declaration contains unknown, repeated, or out-of-order attribute {name}"
                ));
            }
        }
    }
    if stage == 0 {
        return Err("XML declaration shall begin with version=\"1.0\"".into());
    }
    Ok(())
}

fn validate_xml_chars(value: &str, context: &str) -> Result<(), String> {
    if let Some(character) = value.chars().find(|character| !is_xml_10_char(*character)) {
        return Err(format!(
            "{context} contains invalid XML 1.0 character U+{:04X}",
            u32::from(character)
        ));
    }
    Ok(())
}

fn validate_xml_attribute_lexical_value(value: &str, context: &str) -> Result<(), String> {
    validate_xml_chars(value, context)?;
    if value.contains('<') {
        return Err(format!("{context} contains a literal < character"));
    }
    Ok(())
}

fn is_xml_10_char(value: char) -> bool {
    matches!(value, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&value)
        || ('\u{E000}'..='\u{FFFD}').contains(&value)
        || ('\u{10000}'..='\u{10FFFF}').contains(&value)
}

fn is_xml_space(value: char) -> bool {
    matches!(value, ' ' | '\t' | '\r' | '\n')
}

fn is_xml_space_only(value: &str) -> bool {
    value.chars().all(is_xml_space)
}

fn trim_xml_space(value: &str) -> &str {
    value.trim_matches(is_xml_space)
}

fn validate_xml_qname(value: &str, context: &str) -> Result<(), String> {
    let mut parts = value.split(':');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some()
        || !is_xml_ncname(first)
        || second.is_some_and(|local| !is_xml_ncname(local))
    {
        return Err(format!("{context} name {value:?} is not a valid XML QName"));
    }
    Ok(())
}

fn validate_xml_name(value: &str, context: &str) -> Result<(), String> {
    let mut characters = value.chars();
    if !characters.next().is_some_and(is_xml_name_start) || !characters.all(is_xml_name_char) {
        return Err(format!("{context} {value:?} is not a valid XML Name"));
    }
    Ok(())
}

fn is_xml_ncname(value: &str) -> bool {
    let mut characters = value.chars();
    characters.next().is_some_and(is_xml_ncname_start) && characters.all(is_xml_ncname_char)
}

fn is_xml_name_start(value: char) -> bool {
    value == ':' || is_xml_ncname_start(value)
}

fn is_xml_name_char(value: char) -> bool {
    value == ':' || is_xml_ncname_char(value)
}

fn is_xml_ncname_start(value: char) -> bool {
    value == '_'
        || value.is_ascii_alphabetic()
        || ('\u{C0}'..='\u{D6}').contains(&value)
        || ('\u{D8}'..='\u{F6}').contains(&value)
        || ('\u{F8}'..='\u{2FF}').contains(&value)
        || ('\u{370}'..='\u{37D}').contains(&value)
        || ('\u{37F}'..='\u{1FFF}').contains(&value)
        || ('\u{200C}'..='\u{200D}').contains(&value)
        || ('\u{2070}'..='\u{218F}').contains(&value)
        || ('\u{2C00}'..='\u{2FEF}').contains(&value)
        || ('\u{3001}'..='\u{D7FF}').contains(&value)
        || ('\u{F900}'..='\u{FDCF}').contains(&value)
        || ('\u{FDF0}'..='\u{FFFD}').contains(&value)
        || ('\u{10000}'..='\u{EFFFF}').contains(&value)
}

fn is_xml_ncname_char(value: char) -> bool {
    is_xml_ncname_start(value)
        || matches!(value, '-' | '.' | '\u{B7}')
        || value.is_ascii_digit()
        || ('\u{300}'..='\u{36F}').contains(&value)
        || ('\u{203F}'..='\u{2040}').contains(&value)
}

fn validate_namespace_declaration(prefix: Option<&str>, uri: &str) -> Result<(), String> {
    const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";
    if uri == XMLNS_NAMESPACE {
        return Err("the xmlns namespace URI shall not be declared".into());
    }
    match prefix {
        Some("xmlns") => return Err("the xmlns prefix shall not be declared".into()),
        Some("xml") if uri != XML_NAMESPACE_URI => {
            return Err("the xml prefix shall bind only the XML namespace URI".into());
        }
        Some("xml") => {}
        Some(_) if uri.is_empty() => {
            return Err("a namespace prefix shall not bind an empty URI".into());
        }
        _ if uri == XML_NAMESPACE_URI => {
            return Err("the XML namespace URI shall bind only the xml prefix".into());
        }
        _ => {}
    }
    Ok(())
}

fn resolve_element_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    name_pool: &mut XmlNamePool,
) -> Result<XmlName, String> {
    let (namespace, local) = reader.resolver().resolve_element(name);
    resolved_name(namespace, local.as_ref(), name_pool)
}

fn resolve_attribute_name(
    reader: &NsReader<&[u8]>,
    name: quick_xml::name::QName<'_>,
    name_pool: &mut XmlNamePool,
) -> Result<XmlName, String> {
    let (namespace, local) = reader.resolver().resolve_attribute(name);
    resolved_name(namespace, local.as_ref(), name_pool)
}

fn resolved_name(
    namespace: ResolveResult<'_>,
    local: &str,
    name_pool: &mut XmlNamePool,
) -> Result<XmlName, String> {
    let namespace = match namespace {
        ResolveResult::Unbound => None,
        ResolveResult::Bound(value) => name_pool.resolve_namespace(value.as_ref())?,
        ResolveResult::Unknown(prefix) => {
            return Err(format!("XML uses undeclared namespace prefix {prefix}"));
        }
    };
    name_pool.expanded_name(namespace, local)
}

fn append_xml_text(node: &mut XmlNode, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(XmlContent::Text(existing)) = node.content.last_mut() {
        existing.push_str(value);
    } else {
        node.content.push(XmlContent::Text(value.to_owned()));
    }
}

fn analyze_frame_tree(nodes: &[XmlNode]) -> Result<ParsedFrame, String> {
    let mut parsed = ParsedFrame::default();
    let roots = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.parent.is_none() && is_unqualified_element(node, "frame"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    parsed.roots = roots.len();

    let mut payloads = Vec::new();
    let mut frame_formats = Vec::new();
    let mut structural_elements = HashSet::new();
    for root in roots {
        structural_elements.insert(root);
        if parsed.version.is_none() {
            parsed.version = unqualified_attribute(&nodes[root], "version").map(str::to_owned);
        }
        for header in direct_children(nodes, root, "frameHeader") {
            structural_elements.insert(header);
            parsed.frame_headers = parsed
                .frame_headers
                .checked_add(1)
                .ok_or_else(|| "frameHeader count overflow".to_string())?;
            let formats = direct_children(nodes, header, "frameFormat");
            structural_elements.extend(formats.iter().copied());
            parsed.frame_formats = parsed
                .frame_formats
                .checked_add(formats.len())
                .ok_or_else(|| "frameFormat count overflow".to_string())?;
            frame_formats.extend(formats);
            let transport_track_formats = direct_children(nodes, header, "transportTrackFormat");
            structural_elements.extend(transport_track_formats.iter().copied());
            parsed.transport_track_formats = parsed
                .transport_track_formats
                .checked_add(transport_track_formats.len())
                .ok_or_else(|| "transportTrackFormat count overflow".to_string())?;
        }

        let direct = direct_children(nodes, root, "audioFormatExtended");
        structural_elements.extend(direct.iter().copied());
        parsed.direct_audio_format_extended = parsed
            .direct_audio_format_extended
            .checked_add(direct.len())
            .ok_or_else(|| "audioFormatExtended count overflow".to_string())?;
        payloads.extend(direct);
        let cores = direct_children(nodes, root, "coreMetadata");
        structural_elements.extend(cores.iter().copied());
        parsed.core_metadata = parsed
            .core_metadata
            .checked_add(cores.len())
            .ok_or_else(|| "coreMetadata count overflow".to_string())?;
        for core in cores {
            let formats = direct_children(nodes, core, "format");
            structural_elements.extend(formats.iter().copied());
            parsed.core_metadata_formats = parsed
                .core_metadata_formats
                .checked_add(formats.len())
                .ok_or_else(|| "coreMetadata format count overflow".to_string())?;
            for format in formats {
                let nested = direct_children(nodes, format, "audioFormatExtended");
                structural_elements.extend(nested.iter().copied());
                parsed.nested_audio_format_extended = parsed
                    .nested_audio_format_extended
                    .checked_add(nested.len())
                    .ok_or_else(|| "audioFormatExtended count overflow".to_string())?;
                payloads.extend(nested);
            }
        }
    }
    parsed.audio_format_extended = payloads.len();
    parsed.payload_path_valid = (parsed.direct_audio_format_extended == 1
        && parsed.core_metadata == 0
        && parsed.core_metadata_formats == 0
        && parsed.nested_audio_format_extended == 0)
        || (parsed.direct_audio_format_extended == 0
            && parsed.core_metadata == 1
            && parsed.core_metadata_formats == 1
            && parsed.nested_audio_format_extended == 1);
    if !parsed.payload_path_valid {
        push_frame_message(
            &mut parsed.state_errors,
            format!(
                "payload containers are ambiguous: {} direct audioFormatExtended, {} coreMetadata, {} direct format, {} nested audioFormatExtended",
                parsed.direct_audio_format_extended,
                parsed.core_metadata,
                parsed.core_metadata_formats,
                parsed.nested_audio_format_extended
            ),
        );
    }

    if let Some(format) = frame_formats.first().copied() {
        for attribute in &nodes[format].attributes {
            if attribute.name.namespace.is_none() {
                parsed
                    .attributes
                    .entry(attribute.name.local.clone())
                    .or_insert_with(|| attribute.value.clone());
            }
        }
    }

    let mut declared = HashSet::new();
    for format in frame_formats {
        for changed_ids in direct_children(nodes, format, "changedIDs") {
            structural_elements.insert(changed_ids);
            parsed.changed_ids = parsed
                .changed_ids
                .checked_add(1)
                .ok_or_else(|| "changedIDs count overflow".to_string())?;
            analyze_changed_ids(nodes, changed_ids, &mut declared, &mut parsed);
        }
    }
    if parsed.changed_ids > 1 {
        parsed.changed_shape_errors.push(format!(
            "{} changedIDs lists occur at the permitted path",
            parsed.changed_ids
        ));
    }

    for (index, node) in nodes.iter().enumerate() {
        if node.name.namespace.is_none()
            && is_sadm_structural_name(&node.name.local)
            && !structural_elements.contains(&index)
        {
            push_frame_message(
                &mut parsed.structural_shape_errors,
                format!(
                    "unqualified {} occurs outside its required S-ADM structural path",
                    node.name.local
                ),
            );
        }
    }
    if !parsed.structural_shape_errors.is_empty() {
        parsed.payload_path_valid = false;
        for error in &parsed.structural_shape_errors {
            push_frame_message(&mut parsed.state_errors, error.clone());
        }
    }

    if parsed.payload_path_valid {
        analyze_adm_payload(nodes, payloads[0], &mut parsed)?;
    } else {
        push_frame_message(
            &mut parsed.state_errors,
            format!(
                "state reconstruction requires one structurally valid payload, found {} candidate(s)",
                payloads.len()
            ),
        );
    }
    Ok(parsed)
}

fn is_sadm_structural_name(local: &str) -> bool {
    matches!(
        local,
        "frame"
            | "frameHeader"
            | "frameFormat"
            | "transportTrackFormat"
            | "changedIDs"
            | "coreMetadata"
            | "format"
            | "audioFormatExtended"
    )
}

fn direct_children(nodes: &[XmlNode], parent: usize, local: &str) -> Vec<usize> {
    nodes[parent]
        .content
        .iter()
        .filter_map(|content| match content {
            XmlContent::Child(index) if is_unqualified_element(&nodes[*index], local) => {
                Some(*index)
            }
            _ => None,
        })
        .collect()
}

fn is_unqualified_element(node: &XmlNode, local: &str) -> bool {
    node.name.namespace.is_none() && node.name.local == local
}

fn unqualified_attribute<'a>(node: &'a XmlNode, local: &str) -> Option<&'a str> {
    node.attributes
        .iter()
        .find(|attribute| attribute.name.namespace.is_none() && attribute.name.local == local)
        .map(|attribute| attribute.value.as_str())
}

fn analyze_changed_ids(
    nodes: &[XmlNode],
    changed_ids: usize,
    declared: &mut HashSet<StateKey>,
    parsed: &mut ParsedFrame,
) {
    for content in &nodes[changed_ids].content {
        match content {
            XmlContent::Text(value) if is_xml_space_only(value) => {}
            XmlContent::Text(_) => push_frame_message(
                &mut parsed.changed_shape_errors,
                "changedIDs contains non-whitespace text outside an IDRef".into(),
            ),
            XmlContent::Child(index) => {
                let node = &nodes[*index];
                let Some(kind) = changed_kind_for_ref(&node.name) else {
                    push_frame_message(
                        &mut parsed.changed_shape_errors,
                        format!(
                            "changedIDs contains unknown child {}",
                            display_expanded_name(&node.name)
                        ),
                    );
                    continue;
                };
                if node
                    .content
                    .iter()
                    .any(|content| matches!(content, XmlContent::Child(_)))
                {
                    push_frame_message(
                        &mut parsed.changed_shape_errors,
                        format!("{} must contain only an ID value", node.name.local),
                    );
                }
                let text = node_text(node);
                let id = trim_xml_space(&text).to_owned();
                let key = (!id.is_empty()).then_some(StateKey { kind, id });
                if key.is_none() {
                    push_frame_message(
                        &mut parsed.changed_shape_errors,
                        format!("{} contains an empty ID", node.name.local),
                    );
                }
                if key
                    .as_ref()
                    .is_some_and(|key| !declared.insert(key.clone()))
                {
                    push_frame_message(
                        &mut parsed.changed_shape_errors,
                        format!(
                            "duplicate changed ID declaration for {}",
                            key.as_ref().unwrap().id
                        ),
                    );
                }
                let status = unqualified_attribute(node, "status").map(str::to_owned);
                if !matches!(
                    status.as_deref(),
                    Some("new" | "changed" | "extended" | "expired")
                ) {
                    push_frame_message(
                        &mut parsed.changed_shape_errors,
                        format!(
                            "{} {} has a missing or invalid status",
                            node.name.local,
                            key.as_ref().map_or("(empty)", |key| key.id.as_str())
                        ),
                    );
                }
                parsed
                    .changed_declarations
                    .push(ChangedDeclaration { key, status });
            }
        }
    }
}

fn node_text(node: &XmlNode) -> String {
    node.content
        .iter()
        .filter_map(|content| match content {
            XmlContent::Text(value) => Some(value.as_str()),
            XmlContent::Child(_) => None,
        })
        .collect::<String>()
}

fn changed_kind_for_ref(name: &XmlName) -> Option<&'static str> {
    if name.namespace.is_some() {
        return None;
    }
    match name.local.as_str() {
        "audioProgrammeIDRef" => Some("audioProgramme"),
        "audioContentIDRef" => Some("audioContent"),
        "audioObjectIDRef" => Some("audioObject"),
        "audioPackFormatIDRef" => Some("audioPackFormat"),
        "audioChannelFormatIDRef" => Some("audioChannelFormat"),
        "audioTrackUIDRef" => Some("audioTrackUID"),
        "audioTrackFormatIDRef" => Some("audioTrackFormat"),
        "audioStreamFormatIDRef" => Some("audioStreamFormat"),
        _ => None,
    }
}

fn adm_local_definition(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "audioProgramme" => Some(("audioProgramme", "audioProgrammeID")),
        "audioContent" => Some(("audioContent", "audioContentID")),
        "audioObject" => Some(("audioObject", "audioObjectID")),
        "audioPackFormat" => Some(("audioPackFormat", "audioPackFormatID")),
        "audioChannelFormat" => Some(("audioChannelFormat", "audioChannelFormatID")),
        "audioTrackUID" => Some(("audioTrackUID", "UID")),
        "audioTrackFormat" => Some(("audioTrackFormat", "audioTrackFormatID")),
        "audioStreamFormat" => Some(("audioStreamFormat", "audioStreamFormatID")),
        _ => None,
    }
}

fn adm_element_definition(
    name: &XmlName,
    namespace: Option<&str>,
) -> Option<(&'static str, &'static str)> {
    (name.namespace.as_deref() == namespace)
        .then(|| adm_local_definition(&name.local))
        .flatten()
}

fn analyze_adm_payload(
    nodes: &[XmlNode],
    payload: usize,
    parsed: &mut ParsedFrame,
) -> Result<(), String> {
    let children = nodes[payload]
        .content
        .iter()
        .filter_map(|content| match content {
            XmlContent::Child(index) => Some(*index),
            XmlContent::Text(_) => None,
        })
        .collect::<Vec<_>>();
    let definition_namespace = nodes[payload].name.namespace.as_deref();

    let mut seen = HashSet::new();
    for child in children {
        let Some((kind, id_attribute)) =
            adm_element_definition(&nodes[child].name, definition_namespace)
        else {
            continue;
        };
        let Some(id) = unqualified_attribute(&nodes[child], id_attribute)
            .map(trim_xml_space)
            .filter(|id| !id.is_empty())
        else {
            push_frame_message(
                &mut parsed.state_errors,
                format!(
                    "{} is missing a non-empty {}",
                    display_expanded_name(&nodes[child].name),
                    id_attribute
                ),
            );
            continue;
        };
        let key = StateKey {
            kind,
            id: id.to_owned(),
        };
        if !seen.insert(key.clone()) {
            push_frame_message(
                &mut parsed.state_errors,
                format!("payload repeats {} {}", kind, key.id),
            );
            continue;
        }
        let remaining = MAX_SADM_FRAME_CANONICAL_BYTES
            .checked_sub(parsed.canonical_bytes)
            .ok_or_else(|| "S-ADM frame canonical output size overflow".to_string())?;
        let canonical = canonicalize_element(nodes, child, kind, false, remaining)?;
        parsed.canonical_bytes = parsed
            .canonical_bytes
            .checked_add(canonical.len())
            .ok_or_else(|| "S-ADM frame canonical output size overflow".to_string())?;
        let remaining = MAX_SADM_FRAME_CANONICAL_BYTES
            .checked_sub(parsed.canonical_bytes)
            .ok_or_else(|| "S-ADM frame canonical output size overflow".to_string())?;
        let canonical_without_timing = canonicalize_element(nodes, child, kind, true, remaining)?;
        parsed.canonical_bytes = parsed
            .canonical_bytes
            .checked_add(canonical_without_timing.len())
            .ok_or_else(|| "S-ADM frame canonical output size overflow".to_string())?;
        parsed.adm_elements.push(AdmElement {
            key,
            canonical,
            canonical_without_timing,
        });
    }
    Ok(())
}

fn display_expanded_name(name: &XmlName) -> String {
    name.namespace.as_ref().map_or_else(
        || name.local.clone(),
        |namespace| format!("{{{namespace}}}{}", name.local),
    )
}

fn push_frame_message(messages: &mut Vec<String>, message: String) {
    const MAX_REPORTED_FRAME_MESSAGES: usize = 32;
    if messages.len() < MAX_REPORTED_FRAME_MESSAGES {
        messages.push(message);
    }
}

struct CanonicalOutput {
    value: String,
    limit: usize,
}

struct CanonicalContext<'a> {
    nodes: &'a [XmlNode],
    root: usize,
    kind: &'static str,
    strip_timing: bool,
    namespaces: &'a [Arc<str>],
}

impl CanonicalOutput {
    fn new(limit: usize) -> Self {
        Self {
            value: String::new(),
            limit,
        }
    }

    fn push_char(&mut self, value: char) -> Result<(), String> {
        let mut encoded = [0_u8; 4];
        self.push_str(value.encode_utf8(&mut encoded))
    }

    fn push_str(&mut self, value: &str) -> Result<(), String> {
        let next = self
            .value
            .len()
            .checked_add(value.len())
            .ok_or_else(|| "S-ADM canonical output size overflow".to_string())?;
        if next > self.limit {
            return Err(format!(
                "S-ADM frame canonical output exceeds {MAX_SADM_FRAME_CANONICAL_BYTES} bytes"
            ));
        }
        self.value.push_str(value);
        Ok(())
    }

    fn push_component(&mut self, value: &str) -> Result<(), String> {
        self.push_str(&value.len().to_string())?;
        self.push_char(':')?;
        self.push_str(value)
    }
}

fn canonicalize_element(
    nodes: &[XmlNode],
    root: usize,
    kind: &'static str,
    strip_timing: bool,
    limit: usize,
) -> Result<String, String> {
    let namespaces = canonical_namespace_table(nodes, root);
    let mut output = CanonicalOutput::new(limit);
    output.push_char('S')?;
    output.push_component(&namespaces.len().to_string())?;
    for namespace in &namespaces {
        output.push_char('N')?;
        output.push_component(namespace)?;
    }
    output.push_char(';')?;
    let context = CanonicalContext {
        nodes,
        root,
        kind,
        strip_timing,
        namespaces: &namespaces,
    };
    append_canonical_node(&context, root, false, &mut output)?;
    Ok(output.value)
}

fn canonical_namespace_table(nodes: &[XmlNode], root: usize) -> Vec<Arc<str>> {
    let mut namespaces = HashSet::<Arc<str>>::new();
    let mut pending = vec![root];
    while let Some(index) = pending.pop() {
        let node = &nodes[index];
        if let Some(namespace) = &node.name.namespace {
            namespaces.insert(namespace.clone());
        }
        for attribute in &node.attributes {
            if let Some(namespace) = &attribute.name.namespace {
                namespaces.insert(namespace.clone());
            }
        }
        pending.extend(node.content.iter().filter_map(|content| match content {
            XmlContent::Child(child) => Some(*child),
            XmlContent::Text(_) => None,
        }));
    }
    let mut namespaces = namespaces.into_iter().collect::<Vec<_>>();
    namespaces.sort_unstable_by(|left, right| left.as_ref().cmp(right.as_ref()));
    namespaces
}

fn append_canonical_node(
    context: &CanonicalContext<'_>,
    index: usize,
    inherited_space_preserve: bool,
    output: &mut CanonicalOutput,
) -> Result<(), String> {
    let node = &context.nodes[index];
    let space_preserve = node
        .attributes
        .iter()
        .find(|attribute| {
            attribute.name.namespace.as_deref() == Some(XML_NAMESPACE_URI)
                && attribute.name.local == "space"
        })
        .map_or(inherited_space_preserve, |attribute| {
            match attribute.value.as_str() {
                "preserve" => true,
                "default" => false,
                _ => inherited_space_preserve,
            }
        });
    let has_child = node
        .content
        .iter()
        .any(|content| matches!(content, XmlContent::Child(_)));
    let has_non_space_text = node
        .content
        .iter()
        .any(|content| matches!(content, XmlContent::Text(value) if !is_xml_space_only(value)));
    let keep_space_only_text = space_preserve || !has_child || has_non_space_text;
    output.push_char('E')?;
    append_canonical_name(output, &node.name, context.namespaces)?;
    let mut attributes = node
        .attributes
        .iter()
        .filter(|attribute| {
            !context.strip_timing
                || !is_timing_attribute(context.nodes, index, context.root, context.kind, attribute)
        })
        .map(|attribute| {
            (
                canonical_attribute_name(
                    context.nodes,
                    index,
                    context.root,
                    context.kind,
                    attribute,
                ),
                canonical_attribute_value(
                    context.nodes,
                    index,
                    context.root,
                    context.kind,
                    attribute,
                ),
            )
        })
        .collect::<Vec<_>>();
    attributes.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (name, value) in attributes {
        output.push_char('A')?;
        append_canonical_name(output, &name, context.namespaces)?;
        output.push_component(&value)?;
    }
    output.push_char('>')?;
    for content in &node.content {
        match content {
            XmlContent::Child(child) => {
                append_canonical_node(context, *child, space_preserve, output)?
            }
            XmlContent::Text(value) if keep_space_only_text || !is_xml_space_only(value) => {
                output.push_char('T')?;
                output.push_component(value)?;
            }
            XmlContent::Text(_) => {}
        }
    }
    output.push_char('<')?;
    Ok(())
}

fn append_canonical_name(
    output: &mut CanonicalOutput,
    name: &XmlName,
    namespaces: &[Arc<str>],
) -> Result<(), String> {
    match &name.namespace {
        Some(namespace) => {
            let index = namespaces
                .binary_search_by(|candidate| candidate.as_ref().cmp(namespace.as_ref()))
                .map_err(|_| "S-ADM canonical namespace table is incomplete".to_string())?;
            output.push_char('N')?;
            output.push_component(&index.to_string())?;
        }
        None => output.push_char('U')?,
    }
    output.push_component(&name.local)
}

fn is_timing_attribute(
    nodes: &[XmlNode],
    index: usize,
    root: usize,
    kind: &'static str,
    attribute: &XmlAttribute,
) -> bool {
    if attribute.name.namespace.is_some() {
        return false;
    }
    let name = attribute.name.local.as_str();
    (index == root
        && ((kind == "audioProgramme" && matches!(name, "start" | "end"))
            || (kind == "audioObject" && matches!(name, "start" | "duration"))))
        || (is_normative_audio_block_format(nodes, index, root, kind)
            && matches!(
                name,
                "rtime" | "duration" | "lstart" | "lduration" | "ltime"
            ))
}

fn canonical_attribute_name(
    nodes: &[XmlNode],
    index: usize,
    root: usize,
    kind: &'static str,
    attribute: &XmlAttribute,
) -> XmlName {
    if attribute.name.namespace.is_none()
        && attribute.name.local == "ltime"
        && is_normative_audio_block_format(nodes, index, root, kind)
    {
        XmlName {
            namespace: None,
            local: "lstart".into(),
        }
    } else {
        attribute.name.clone()
    }
}

fn canonical_attribute_value(
    nodes: &[XmlNode],
    index: usize,
    root: usize,
    kind: &'static str,
    attribute: &XmlAttribute,
) -> String {
    if !is_timing_attribute(nodes, index, root, kind, attribute) {
        return attribute.value.clone();
    }
    if is_normative_audio_block_format(nodes, index, root, kind)
        && matches!(attribute.name.local.as_str(), "lstart" | "ltime")
    {
        return canonical_signed_local_time(&attribute.value)
            .unwrap_or_else(|| format!("R:{}", attribute.value));
    }
    let exact = if index == root && attribute.name.local != "duration" {
        parse_start_time(&attribute.value)
    } else {
        parse_duration_time(&attribute.value)
    };
    exact.map_or_else(
        || format!("R:{}", attribute.value),
        |time| format!("P:{}/{}", time.numerator, time.denominator),
    )
}

fn canonical_signed_local_time(value: &str) -> Option<String> {
    let (negative, magnitude) = value
        .strip_prefix('-')
        .map_or((false, value), |magnitude| (true, magnitude));
    let time = parse_duration_time(magnitude)?;
    let sign = if negative && time.numerator != 0 {
        "-"
    } else {
        ""
    };
    Some(format!("P:{sign}{}/{}", time.numerator, time.denominator))
}

fn is_normative_audio_block_format(
    nodes: &[XmlNode],
    index: usize,
    root: usize,
    kind: &'static str,
) -> bool {
    kind == "audioChannelFormat"
        && index != root
        && nodes[index].parent == Some(root)
        && nodes[index].name.local == "audioBlockFormat"
        && nodes[index].name.namespace == nodes[root].name.namespace
}

fn group_logical_frames(parsed_frames: &[ParsedFrame]) -> Vec<LogicalFrame> {
    let mut logical_frames: Vec<LogicalFrame> = Vec::new();
    for (index, frame) in parsed_frames.iter().enumerate() {
        let id = attribute(frame, "frameFormatID").and_then(parse_frame_format_id);
        let joins_previous = id.is_some_and(|id| {
            id.chunk.is_some()
                && logical_frames
                    .last()
                    .is_some_and(|logical| logical.base == Some(id.base) && logical.chunked)
        });
        if joins_previous {
            logical_frames.last_mut().unwrap().members.push(index);
        } else {
            logical_frames.push(LogicalFrame {
                base: id.map(|id| id.base),
                chunked: id.is_some_and(|id| id.chunk.is_some()),
                members: vec![index],
            });
        }
    }
    logical_frames
}

fn uniform_attribute<'a>(
    logical: &LogicalFrame,
    parsed_frames: &'a [ParsedFrame],
    name: &str,
) -> Option<&'a str> {
    let first = attribute(&parsed_frames[*logical.members.first()?], name)?;
    logical
        .members
        .iter()
        .all(|index| attribute(&parsed_frames[*index], name) == Some(first))
        .then_some(first)
}

fn uniform_frame_time(
    logical: &LogicalFrame,
    parsed_frames: &[ParsedFrame],
) -> Option<(ExactTime, ExactTime)> {
    let first = &parsed_frames[*logical.members.first()?];
    let start = attribute(first, "start").and_then(parse_start_time)?;
    let duration = attribute(first, "duration").and_then(parse_duration_time)?;
    logical
        .members
        .iter()
        .all(|index| {
            let frame = &parsed_frames[*index];
            attribute(frame, "start").and_then(parse_start_time) == Some(start)
                && attribute(frame, "duration").and_then(parse_duration_time) == Some(duration)
        })
        .then_some((start, duration))
}

fn classify_flow(types: &[Option<&str>]) -> FlowKind {
    let Some(first) = types.first().copied().flatten() else {
        return FlowKind::Invalid;
    };
    if first == "divided" {
        return if types.iter().all(|kind| *kind == Some("divided")) {
            FlowKind::DividedFrame
        } else {
            FlowKind::Invalid
        };
    }
    if !matches!(first, "header" | "full" | "all") {
        return FlowKind::Invalid;
    }
    let tail = &types[1..];
    if tail.is_empty() {
        return FlowKind::Indeterminate;
    }
    if tail.iter().all(|kind| *kind == Some("divided")) {
        return FlowKind::DividedFrame;
    }
    if tail.iter().all(|kind| *kind == Some("full")) {
        return FlowKind::FullFrame;
    }
    if tail.iter().all(|kind| *kind == Some("intermediate")) {
        return FlowKind::IntermediateFrame;
    }
    if tail
        .iter()
        .all(|kind| matches!(*kind, Some("full" | "intermediate")))
        && tail.contains(&Some("full"))
        && tail.contains(&Some("intermediate"))
    {
        return FlowKind::MixedFrame;
    }
    FlowKind::Invalid
}

fn describe_logical_chunks(
    logical_frames: &[LogicalFrame],
    parsed_frames: &[ParsedFrame],
) -> String {
    logical_frames
        .iter()
        .map(|logical| {
            let base = logical
                .base
                .map_or_else(|| "invalid".into(), |value| format!("{value:X}"));
            let chunks = logical
                .members
                .iter()
                .map(|index| {
                    attribute(&parsed_frames[*index], "frameFormatID")
                        .and_then(parse_frame_format_id)
                        .and_then(|id| id.chunk)
                        .map_or_else(|| "none".into(), |value| format!("{value:X}"))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{base}:[{chunks}]")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn attribute<'a>(frame: &'a ParsedFrame, name: &str) -> Option<&'a str> {
    frame.attributes.get(name).map(String::as_str)
}

fn rule(
    rule_id: &'static str,
    path: impl Into<String>,
    requirement: impl Into<String>,
    observed: impl Into<String>,
    passed: bool,
) -> SadmRule {
    SadmRule {
        rule_id,
        path: path.into(),
        requirement: requirement.into(),
        observed: observed.into(),
        passed,
    }
}

fn parse_frame_format_id(value: &str) -> Option<FrameFormatId> {
    let value = value.strip_prefix("FF_")?;
    let mut parts = value.split('_');
    let base = parts.next()?;
    if !matches!(base.len(), 8 | 11) || !base.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let base = u64::from_str_radix(base, 16).ok()?;
    let chunk = match parts.next() {
        Some(chunk) if chunk.len() == 2 && chunk.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            let chunk = u8::from_str_radix(chunk, 16).ok()?;
            (chunk > 0).then_some(chunk)?
        }
        Some(_) => return None,
        None => return Some(FrameFormatId { base, chunk: None }),
    };
    if parts.next().is_some() {
        return None;
    }
    Some(FrameFormatId {
        base,
        chunk: Some(chunk),
    })
}

fn valid_uuid(value: &str) -> bool {
    let lengths = [8, 4, 4, 4, 12];
    let parts = value.split('-').collect::<Vec<_>>();
    parts.len() == lengths.len()
        && parts.iter().zip(lengths).all(|(part, length)| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn parse_start_time(value: &str) -> Option<ExactTime> {
    if let Some((date, time)) = value.split_once('T') {
        let time = time.strip_suffix('Z')?;
        let day_offset = days_before_date(date)?.checked_mul(86_400)?;
        let parsed = parse_decimal_clock(time)?;
        if parsed.numerator >= 86_400_u128.checked_mul(parsed.denominator)? {
            return None;
        }
        return parsed.checked_add(ExactTime::new(day_offset, 1)?);
    }
    if value.ends_with('Z') {
        return None;
    }
    parse_time_of_day_or_samples(value, false)
}

fn parse_duration_time(value: &str) -> Option<ExactTime> {
    if value.contains('T') || value.ends_with('Z') {
        return None;
    }
    parse_time_of_day_or_samples(value, true)
}

fn parse_time_of_day_or_samples(value: &str, allow_short_decimal: bool) -> Option<ExactTime> {
    if let Some((time, rate_text)) = value.split_once('S') {
        if time.is_empty() || rate_text.contains('S') || !(5..=9).contains(&rate_text.len()) {
            return None;
        }
        let rate = parse_ascii_u128(rate_text)?;
        if rate == 0 {
            return None;
        }
        if time.contains(':') {
            let (whole, samples_text) = time.rsplit_once('.')?;
            if samples_text.len() != rate_text.len() || !(5..=9).contains(&samples_text.len()) {
                return None;
            }
            let samples = parse_ascii_u128(samples_text)?;
            if samples >= rate {
                return None;
            }
            let seconds = parse_whole_clock(whole)?;
            return ExactTime::new(seconds.checked_mul(rate)?.checked_add(samples)?, rate);
        }
        return ExactTime::new(parse_ascii_u128(time)?, rate);
    }
    if value.contains(':') {
        return parse_decimal_clock(value);
    }
    allow_short_decimal
        .then(|| parse_decimal_seconds(value, true))
        .flatten()
}

fn parse_decimal_clock(value: &str) -> Option<ExactTime> {
    let mut parts = value.split(':');
    let hours = parse_ascii_u128(parts.next()?)?;
    let minutes = parse_ascii_u128(parts.next()?)?;
    let seconds = parts.next()?;
    if parts.next().is_some() || minutes >= 60 {
        return None;
    }
    let seconds = parse_decimal_seconds(seconds, true)?;
    if seconds.numerator >= 60_u128.checked_mul(seconds.denominator)? {
        return None;
    }
    let whole = hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?;
    seconds.checked_add(ExactTime::new(whole, 1)?)
}

fn parse_whole_clock(value: &str) -> Option<u128> {
    let mut parts = value.split(':');
    let hours = parse_ascii_u128(parts.next()?)?;
    let minutes = parse_ascii_u128(parts.next()?)?;
    let seconds = parse_ascii_u128(parts.next()?)?;
    if parts.next().is_some() || minutes >= 60 || seconds >= 60 {
        return None;
    }
    hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)
}

fn parse_decimal_seconds(value: &str, require_fraction: bool) -> Option<ExactTime> {
    let (whole, fraction) = match value.split_once('.') {
        Some(parts) => parts,
        None if !require_fraction => return ExactTime::new(parse_ascii_u128(value)?, 1),
        None => return None,
    };
    if fraction.contains('.') || !(5..=9).contains(&fraction.len()) {
        return None;
    }
    let whole = parse_ascii_u128(whole)?;
    let fraction_value = parse_ascii_u128(fraction)?;
    let denominator = 10_u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    ExactTime::new(
        whole
            .checked_mul(denominator)?
            .checked_add(fraction_value)?,
        denominator,
    )
}

fn days_before_date(value: &str) -> Option<u128> {
    let mut parts = value.split('-');
    let year_text = parts.next()?;
    let month_text = parts.next()?;
    let day_text = parts.next()?;
    if parts.next().is_some()
        || year_text.len() != 4
        || month_text.len() != 2
        || day_text.len() != 2
    {
        return None;
    }
    let year = parse_ascii_u128(year_text)?;
    let month = parse_ascii_u128(month_text)?;
    let day = parse_ascii_u128(day_text)?;
    if year == 0 || !(1..=12).contains(&month) {
        return None;
    }
    let leap = is_leap_year(year);
    let month_lengths = [31_u128, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let month_index = usize::try_from(month - 1).ok()?;
    let days_in_month = month_lengths[month_index] + u128::from(leap && month == 2);
    if day == 0 || day > days_in_month {
        return None;
    }
    let previous_year = year - 1;
    let days_before_year = previous_year
        .checked_mul(365)?
        .checked_add(previous_year / 4)?
        .checked_sub(previous_year / 100)?
        .checked_add(previous_year / 400)?;
    let days_before_month = month_lengths
        .iter()
        .take(month_index)
        .try_fold(0_u128, |sum, days| sum.checked_add(*days))?
        .checked_add(u128::from(leap && month > 2))?;
    days_before_year
        .checked_add(days_before_month)?
        .checked_add(day - 1)
}

fn is_leap_year(year: u128) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn parse_ascii_u128(value: &str) -> Option<u128> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn parse_decimal_u64(value: Option<&str>) -> Option<u64> {
    value
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
}

fn parse_decimal_u16(value: Option<&str>) -> Option<u16> {
    value
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
}

fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_frame(
        directory: &Path,
        name: &str,
        id: &str,
        start: &str,
        duration: &str,
        kind: &str,
        extra_attributes: &str,
    ) -> PathBuf {
        let path = directory.join(name);
        fs::write(
            &path,
            format!(
                r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_{id}" start="{start}" duration="{duration}" flowID="12345678-abcd-4000-a000-112233445566" type="{kind}"{extra_attributes}/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#
            ),
        )
        .unwrap();
        path
    }

    fn flow_rule<'a>(audit: &'a SadmAudit, id: &str) -> &'a SadmRule {
        audit
            .flow_rules
            .iter()
            .find(|candidate| candidate.rule_id == id)
            .unwrap()
    }

    fn frame_rule<'a>(audit: &'a SadmAudit, frame: usize, id: &str) -> &'a SadmRule {
        audit.frames[frame]
            .rules
            .iter()
            .find(|candidate| candidate.rule_id == id)
            .unwrap()
    }

    fn write_xml(directory: &Path, name: &str, xml: impl AsRef<[u8]>) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, xml).unwrap();
        path
    }

    fn write_state_frame(
        directory: &Path,
        name: &str,
        id: u64,
        start_half_seconds: u64,
        kind: &str,
        changed_ids: &str,
        payload: &str,
    ) -> PathBuf {
        let start_samples = start_half_seconds.checked_mul(24_000).unwrap();
        write_xml(
            directory,
            name,
            format!(
                r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_{id:08X}" start="{start_samples}S48000" duration="24000S48000" type="{kind}">{changed_ids}</frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended>{payload}</audioFormatExtended></frame>"#
            ),
        )
    }

    #[test]
    fn validates_a_contiguous_bs2125_flow() {
        let work = tempfile::tempdir().unwrap();
        let paths = [
            ("frame1.xml", "00000001", "00:00:00.00000", "header"),
            ("frame2.xml", "00000002", "00:00:00.50000", "full"),
        ]
        .into_iter()
        .map(|(name, id, start, kind)| {
            write_frame(work.path(), name, id, start, "00:00:00.50000", kind, "")
        })
        .collect::<Vec<_>>();
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert_eq!(audit.frame_count, 2);
        assert_eq!(flow_rule(&audit, "BS2125-FLOW-TYPE").observed, "full-frame");
    }

    #[test]
    fn validates_divided_chunks_as_logical_frames() {
        let work = tempfile::tempdir().unwrap();
        let inputs = [
            ("1-1.xml", "00000001_01", "0S48000"),
            ("1-2.xml", "00000001_02", "0S48000"),
            ("2-1.xml", "00000002_01", "00:00:00.50000"),
            ("2-2.xml", "00000002_02", "00:00:00.50000"),
            ("3-1.xml", "00000003_01", "48000S48000"),
            ("3-2.xml", "00000003_02", "48000S48000"),
        ];
        let paths = inputs
            .into_iter()
            .map(|(name, id, start)| {
                write_frame(
                    work.path(),
                    name,
                    id,
                    start,
                    "24000S48000",
                    "divided",
                    " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
                )
            })
            .collect::<Vec<_>>();
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert_eq!(audit.frame_count, 6);
        assert_eq!(
            flow_rule(&audit, "BS2125-FLOW-TYPE").observed,
            "divided-frame"
        );
        assert_eq!(
            flow_rule(&audit, "BS2125-LOGICAL-FRAME-COUNT").observed,
            "6 input frame document(s), 3 logical frame(s)"
        );
    }

    #[test]
    fn groups_many_same_base_chunks_without_rescanning_prior_members() {
        let mut frames = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            let mut frame = ParsedFrame::default();
            frame
                .attributes
                .insert("frameFormatID".into(), "FF_00000001_01".into());
            frames.push(frame);
        }
        let logical = group_logical_frames(&frames);
        assert_eq!(logical.len(), 1);
        assert!(logical[0].chunked);
        assert_eq!(logical[0].members.len(), frames.len());
    }

    #[test]
    fn validates_header_started_divided_flow() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_frame(
                work.path(),
                "header.xml",
                "00000001",
                "00:00:00.00000",
                "00:00:00.50000",
                "header",
                "",
            ),
            write_frame(
                work.path(),
                "chunk-1.xml",
                "00000002_01",
                "00:00:00.50000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\"",
            ),
            write_frame(
                work.path(),
                "chunk-2.xml",
                "00000002_02",
                "00:00:00.50000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\"",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert_eq!(
            flow_rule(&audit, "BS2125-FLOW-TYPE").observed,
            "divided-frame"
        );
    }

    #[test]
    fn validates_sparse_divided_chunks_from_the_recommendation_pattern() {
        let work = tempfile::tempdir().unwrap();
        let logical_frames = [
            (
                1_u64,
                "00:00:00.00000",
                vec![(1_u8, 1_u64), (2, 2), (3, 3), (4, 1)],
            ),
            (2, "00:00:01.50000", vec![(1, 3), (4, 1)]),
            (3, "00:00:03.00000", vec![(2, 3), (4, 1)]),
            (4, "00:00:04.50000", vec![(3, 3), (4, 1)]),
            (5, "00:00:06.00000", vec![(1, 3), (4, 1)]),
        ];
        let mut paths = Vec::new();
        for (base, start, chunks) in logical_frames {
            for (chunk, count) in chunks {
                paths.push(write_frame(
                    work.path(),
                    &format!("{base}-{chunk}.xml"),
                    &format!("{base:08X}_{chunk:02X}"),
                    start,
                    "00:00:01.50000",
                    "divided",
                    &format!(" numMetadataChunks=\"4\" countToSameChunk=\"{count}\""),
                ));
            }
        }
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert_eq!(
            flow_rule(&audit, "BS2125-LOGICAL-FRAME-SEQUENCE").observed,
            "1, 2, 3, 4, 5"
        );
    }

    #[test]
    fn classifies_intermediate_and_mixed_flows() {
        let work = tempfile::tempdir().unwrap();
        let intermediate = vec![
            write_frame(
                work.path(),
                "if-1.xml",
                "00000001",
                "00:00:00.00000",
                "00:00:00.50000",
                "full",
                " countToFull=\"0\"",
            ),
            write_frame(
                work.path(),
                "if-2.xml",
                "00000002",
                "00:00:00.50000",
                "00:00:00.50000",
                "intermediate",
                " countToFull=\"0\"",
            ),
        ];
        let intermediate_audit = audit(&intermediate).unwrap();
        assert!(
            intermediate_audit.passed,
            "{:#?}",
            intermediate_audit.flow_rules
        );
        assert_eq!(
            flow_rule(&intermediate_audit, "BS2125-FLOW-TYPE").observed,
            "intermediate-frame"
        );

        let mixed = vec![
            write_frame(
                work.path(),
                "mf-1.xml",
                "00000001",
                "00:00:00.00000",
                "00:00:00.50000",
                "header",
                " countToFull=\"2\"",
            ),
            write_frame(
                work.path(),
                "mf-2.xml",
                "00000002",
                "00:00:00.50000",
                "00:00:00.50000",
                "intermediate",
                " countToFull=\"1\"",
            ),
            write_frame(
                work.path(),
                "mf-3.xml",
                "00000003",
                "00:00:01.00000",
                "00:00:00.50000",
                "full",
                " countToFull=\"2\"",
            ),
            write_frame(
                work.path(),
                "mf-4.xml",
                "00000004",
                "00:00:01.50000",
                "00:00:00.50000",
                "intermediate",
                " countToFull=\"1\"",
            ),
            write_frame(
                work.path(),
                "mf-5.xml",
                "00000005",
                "00:00:02.00000",
                "00:00:00.50000",
                "full",
                "",
            ),
        ];
        let mixed_audit = audit(&mixed).unwrap();
        assert!(mixed_audit.passed, "{:#?}", mixed_audit.flow_rules);
        assert_eq!(
            flow_rule(&mixed_audit, "BS2125-FLOW-TYPE").observed,
            "mixed-frame"
        );
    }

    #[test]
    fn rejects_invalid_divided_chunk_sequences_and_recurrence() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_frame(
                work.path(),
                "1-2.xml",
                "00000001_02",
                "00:00:00.00000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
            ),
            write_frame(
                work.path(),
                "1-1.xml",
                "00000001_01",
                "00:00:00.00000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
            ),
            write_frame(
                work.path(),
                "2-2.xml",
                "00000002_02",
                "00:00:00.50000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(!audit.passed);
        assert!(!flow_rule(&audit, "BS2125-DIVIDED-CHUNK-SEQUENCE").passed);
        assert!(!flow_rule(&audit, "BS2125-DIVIDED-CHUNK-RECURRENCE").passed);
    }

    #[test]
    fn rejects_recurrence_that_skips_an_observed_same_chunk() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_frame(
                work.path(),
                "1-1.xml",
                "00000001_01",
                "00:00:00.00000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"99\"",
            ),
            write_frame(
                work.path(),
                "1-2.xml",
                "00000001_02",
                "00:00:00.00000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
            ),
            write_frame(
                work.path(),
                "2-1.xml",
                "00000002_01",
                "00:00:00.50000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
            ),
            write_frame(
                work.path(),
                "2-2.xml",
                "00000002_02",
                "00:00:00.50000",
                "00:00:00.50000",
                "divided",
                " numMetadataChunks=\"2\" countToSameChunk=\"1\"",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(!audit.passed);
        assert!(!flow_rule(&audit, "BS2125-DIVIDED-CHUNK-RECURRENCE").passed);
    }

    #[test]
    fn rejects_count_to_full_that_skips_an_observed_full_frame() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_frame(
                work.path(),
                "first.xml",
                "00000001",
                "00:00:00.00000",
                "00:00:00.50000",
                "header",
                " countToFull=\"99\"",
            ),
            write_frame(
                work.path(),
                "second.xml",
                "00000002",
                "00:00:00.50000",
                "00:00:00.50000",
                "intermediate",
                " countToFull=\"1\"",
            ),
            write_frame(
                work.path(),
                "third.xml",
                "00000003",
                "00:00:01.00000",
                "00:00:00.50000",
                "full",
                "",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(!audit.passed);
        assert!(!flow_rule(&audit, "BS2125-COUNT-TO-FULL").passed);
    }

    #[test]
    fn rejects_a_one_nanosecond_gap_without_float_tolerance() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_frame(
                work.path(),
                "first.xml",
                "00000001",
                "00:00:00.000000000",
                "00:00:00.500000000",
                "header",
                "",
            ),
            write_frame(
                work.path(),
                "second.xml",
                "00000002",
                "00:00:00.500000001",
                "00:00:00.500000000",
                "full",
                "",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(!audit.passed);
        assert!(!flow_rule(&audit, "BS2125-LOGICAL-FRAME-CONTIGUITY").passed);
    }

    #[test]
    fn compares_legacy_dates_across_midnight_exactly() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_frame(
                work.path(),
                "first.xml",
                "00000001",
                "2026-09-02T23:59:59.50000Z",
                "00:00:00.50000",
                "header",
                "",
            ),
            write_frame(
                work.path(),
                "second.xml",
                "00000002",
                "2026-09-03T00:00:00.00000Z",
                "00:00:00.50000",
                "full",
                "",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
    }

    #[test]
    fn rejects_invalid_exact_time_forms() {
        for value in [
            "NaN",
            "-1S48000",
            "1e3S48000",
            "00:00:00.1200S48000",
            "00:00:00.48000S48000",
            "00:00:00.1",
            "2026-02-29T00:00:00.00000Z",
            "2026-09-02T24:00:00.00000Z",
            "2026-09-02T0S48000Z",
        ] {
            assert!(parse_start_time(value).is_none(), "accepted {value}");
        }
        assert_eq!(
            parse_start_time("00:00:00.50000"),
            parse_start_time("24000S48000")
        );
        assert_eq!(
            parse_duration_time("00.50000"),
            parse_duration_time("00:00:00.50000")
        );
    }

    #[test]
    fn rejects_gaps_and_invalid_changed_id_status() {
        let work = tempfile::tempdir().unwrap();
        let first = work.path().join("first.xml");
        let second = work.path().join("second.xml");
        fs::write(&first, r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#).unwrap();
        fs::write(&second, r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000003" start="30000S48000" duration="24000S48000" type="intermediate"><changedIDs><audioObjectIDRef status="bad">AO_1001</audioObjectIDRef></changedIDs></frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#).unwrap();
        let audit = audit(&[first, second]).unwrap();
        assert!(!audit.passed);
    }

    #[test]
    fn counts_only_elements_at_the_normative_paths_and_allows_extensions() {
        let work = tempfile::tempdir().unwrap();
        let path = write_xml(
            work.path(),
            "path-aware.xml",
            r#"<frame xmlns:v="urn:vendor" version="ITU-R_BS.2125-1">
  <frameHeader>
    <frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"><v:changedIDs><v:audioObjectIDRef status="bad">AO_FAKE</v:audioObjectIDRef></v:changedIDs></frameFormat>
    <transportTrackFormat/>
    <v:extension><v:frameFormat/><v:transportTrackFormat/></v:extension>
  </frameHeader>
  <v:extension>
    <v:frameHeader/><v:audioFormatExtended/>
    <v:changedIDs><v:audioObjectIDRef status="bad">AO_FAKE</v:audioObjectIDRef></v:changedIDs>
  </v:extension>
  <audioFormatExtended><audioObject audioObjectID="AO_REAL"/><v:audioObject audioObjectID="AO_REAL"/><v:metadata/></audioFormatExtended>
  <v:coreMetadata><v:format><v:audioFormatExtended/></v:format></v:coreMetadata>
</frame>"#,
        );
        let audit = audit(&[path]).unwrap();
        assert!(audit.passed, "{:#?}", audit.frames[0].rules);
        for id in [
            "BS2125-FRAME-ROOT",
            "BS2125-FRAME-HEADER",
            "BS2125-FRAME-FORMAT",
            "BS2125-TRANSPORT-TRACK-FORMAT",
            "BS2125-AUDIO-FORMAT-EXTENDED",
            "BS2125-CHANGED-IDS-SHAPE",
        ] {
            assert!(frame_rule(&audit, 0, id).passed, "{id}");
        }
    }

    #[test]
    fn rejects_foreign_namespace_spoofs_of_normative_elements() {
        let work = tempfile::tempdir().unwrap();
        let foreign_root = write_xml(
            work.path(),
            "foreign-root.xml",
            r#"<v:frame xmlns:v="urn:vendor" version="ITU-R_BS.2125-1"><v:frameHeader><v:frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><v:transportTrackFormat/></v:frameHeader><v:audioFormatExtended/></v:frame>"#,
        );
        let root_audit = audit(&[foreign_root]).unwrap();
        assert!(!root_audit.passed);
        assert!(!frame_rule(&root_audit, 0, "BS2125-FRAME-ROOT").passed);

        let foreign_ref = write_xml(
            work.path(),
            "foreign-ref.xml",
            r#"<frame xmlns:v="urn:vendor" version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"><changedIDs><v:audioObjectIDRef status="new">AO_1</v:audioObjectIDRef></changedIDs></frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended><audioObject audioObjectID="AO_1"/></audioFormatExtended></frame>"#,
        );
        let ref_audit = audit(&[foreign_ref]).unwrap();
        let shape = frame_rule(&ref_audit, 0, "BS2125-CHANGED-IDS-SHAPE");
        assert!(!shape.passed);
        assert!(shape.observed.contains("{urn:vendor}audioObjectIDRef"));

        let foreign_payload = write_xml(
            work.path(),
            "foreign-payload.xml",
            r#"<frame xmlns:v="urn:vendor" version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"><changedIDs><audioObjectIDRef status="new">AO_1</audioObjectIDRef></changedIDs></frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended><v:audioObject audioObjectID="AO_1"/></audioFormatExtended></frame>"#,
        );
        let payload_audit = audit(&[foreign_payload]).unwrap();
        let state = flow_rule(&payload_audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!state.passed);
        assert!(state.observed.contains("new is false"));
    }

    #[test]
    fn rejects_xml_that_is_not_one_well_formed_document() {
        let valid = r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#;
        let cases = [
            ("multiple roots", format!("{valid}<extra/>")),
            ("leading text", format!("not-xml{valid}")),
            ("trailing text", format!("{valid}not-xml")),
            ("leading NBSP", format!("\u{a0}{valid}")),
            ("leading CDATA", format!("<![CDATA[text]]>{valid}")),
            ("trailing entity", format!("{valid}&amp;")),
            (
                "raw control character",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended>bad\u{1}</audioFormatExtended>",
                ),
            ),
            (
                "numeric reference to control character",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended>&#1;</audioFormatExtended>",
                ),
            ),
            (
                "numeric control character in attribute",
                valid.replace(
                    "<audioFormatExtended/>",
                    r#"<audioFormatExtended x="&#1;"/>"#,
                ),
            ),
            (
                "numeric control character in namespace declaration",
                valid.replace(
                    "<audioFormatExtended/>",
                    r#"<audioFormatExtended xmlns:v="urn:&#1;"/>"#,
                ),
            ),
            (
                "control character in CDATA",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended><![CDATA[bad\u{1}]]></audioFormatExtended>",
                ),
            ),
            (
                "invalid comment",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended><!-- bad--comment --></audioFormatExtended>",
                ),
            ),
            (
                "control character in comment",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended><!-- bad\u{1} --></audioFormatExtended>",
                ),
            ),
            (
                "invalid element QName",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended><1bad/></audioFormatExtended>",
                ),
            ),
            (
                "invalid closing element QName",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended></:audioFormatExtended>",
                ),
            ),
            (
                "invalid processing instruction target",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended><?1bad foo?></audioFormatExtended>",
                ),
            ),
            (
                "control character in processing instruction",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended><?valid bad\u{1}?></audioFormatExtended>",
                ),
            ),
            (
                "literal CDATA terminator in character data",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended>bad]]>text</audioFormatExtended>",
                ),
            ),
            (
                "literal less-than in attribute",
                valid.replace(
                    "<audioFormatExtended/>",
                    r#"<audioFormatExtended x="a<b"/>"#,
                ),
            ),
            (
                "literal less-than in namespace declaration",
                valid.replace(
                    "<audioFormatExtended/>",
                    r#"<audioFormatExtended xmlns:v="urn:a<b"/>"#,
                ),
            ),
            (
                "late declaration",
                format!(r#"<!--prolog--><?xml version="1.0"?>{valid}"#),
            ),
            (
                "second declaration",
                format!(r#"<?xml version="1.0"?>{valid}<?xml version="1.0"?>"#),
            ),
            (
                "declaration missing version",
                format!(r#"<?xml foo="bar"?>{valid}"#),
            ),
            (
                "unsupported declaration version",
                format!(r#"<?xml version="2.0"?>{valid}"#),
            ),
            (
                "entity in declaration value",
                format!(r#"<?xml version="1.&#48;"?>{valid}"#),
            ),
            (
                "unsupported declaration encoding",
                format!(r#"<?xml version="1.0" encoding="ISO-8859-1"?>{valid}"#),
            ),
            (
                "invalid declaration standalone",
                format!(r#"<?xml version="1.0" standalone="maybe"?>{valid}"#),
            ),
            (
                "unknown declaration attribute",
                format!(r#"<?xml version="1.0" foo="bar"?>{valid}"#),
            ),
            (
                "out-of-order declaration attribute",
                format!(r#"<?xml version="1.0" standalone="yes" encoding="UTF-8"?>{valid}"#),
            ),
            (
                "literal less-than in declaration attribute",
                format!(r#"<?xml version="1.<0"?>{valid}"#),
            ),
        ];
        for (name, xml) in cases {
            assert!(parse_xml_tree(xml.as_bytes()).is_err(), "accepted {name}");
        }

        for (name, xml) in [
            (
                "strict XML declaration",
                format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>{valid}"#),
            ),
            (
                "escaped CDATA terminator",
                valid.replace(
                    "<audioFormatExtended/>",
                    "<audioFormatExtended>bad]]&gt;text</audioFormatExtended>",
                ),
            ),
            (
                "escaped less-than in attribute",
                valid.replace(
                    "<audioFormatExtended/>",
                    r#"<audioFormatExtended x="a&lt;b"/>"#,
                ),
            ),
            (
                "escaped less-than in namespace declaration",
                valid.replace(
                    "<audioFormatExtended/>",
                    r#"<audioFormatExtended xmlns:v="urn:a&lt;b"/>"#,
                ),
            ),
        ] {
            assert!(
                parse_xml_tree(xml.as_bytes()).is_ok(),
                "rejected legal {name}"
            );
        }

        assert!(validate_xml_attribute_lexical_value("a<b", "test").is_err());
        assert!(validate_xml_attribute_lexical_value("a&lt;b", "test").is_ok());
    }

    #[test]
    fn enforces_flow_file_and_decoded_text_safety_limits() {
        let too_many = vec![PathBuf::new(); MAX_SADM_FRAME_FILES + 1];
        assert!(audit(&too_many)
            .unwrap_err()
            .contains("more than 100000 frame files"));

        let mut text_bytes = MAX_SADM_XML_TEXT_BYTES;
        assert!(add_xml_text_bytes(&mut text_bytes, 1)
            .unwrap_err()
            .contains("XML text data exceeds"));

        let work = tempfile::tempdir().unwrap();
        let oversized = work.path().join("oversized.xml");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_SADM_XML_BYTES + 1)
            .unwrap();
        assert!(audit(&[oversized])
            .unwrap_err()
            .contains("XML size 67108865 exceeds"));

        let noisy_changed_ids = "\u{a0}<v:extension/>".repeat(100);
        let noisy_xml = format!(
            r#"<frame xmlns:v="urn:vendor" version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"><changedIDs>{noisy_changed_ids}</changedIDs></frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#
        );
        let noisy = parse_frame(noisy_xml.as_bytes()).unwrap();
        assert_eq!(noisy.changed_shape_errors.len(), 32);
    }

    #[test]
    fn interns_namespace_uris_and_canonicalizes_them_once_per_element() {
        let namespace = format!("urn:shared:{}", "n".repeat(3_000));
        let children = r#"<v:label v:mode="same"/>"#.repeat(512);
        let xml = format!(
            r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader><audioFormatExtended><audioObject xmlns:v="{namespace}" audioObjectID="AO_1">{children}</audioObject></audioFormatExtended></frame>"#
        );
        let nodes = parse_xml_tree(xml.as_bytes()).unwrap();
        let shared_namespaces = nodes
            .iter()
            .flat_map(|node| {
                std::iter::once(&node.name)
                    .chain(node.attributes.iter().map(|attribute| &attribute.name))
            })
            .filter_map(|name| name.namespace.as_ref())
            .filter(|uri| uri.as_ref() == namespace)
            .collect::<Vec<_>>();
        assert!(shared_namespaces.len() > 1_000);
        let first = shared_namespaces[0];
        assert!(shared_namespaces
            .iter()
            .all(|namespace| Arc::ptr_eq(first, namespace)));

        let parsed = parse_frame(xml.as_bytes()).unwrap();
        assert_eq!(parsed.adm_elements.len(), 1);
        let adm = &parsed.adm_elements[0];
        assert_eq!(adm.canonical.matches(&namespace).count(), 1);
        assert_eq!(adm.canonical_without_timing.matches(&namespace).count(), 1);

        let root = nodes
            .iter()
            .position(|node| is_unqualified_element(node, "audioObject"))
            .unwrap();
        assert!(canonicalize_element(&nodes, root, "audioObject", false, 16)
            .unwrap_err()
            .contains("canonical output exceeds"));
    }

    #[test]
    fn bounds_namespace_uri_and_expanded_name_work() {
        let oversized_namespace = format!("urn:{}", "n".repeat(MAX_SADM_NAMESPACE_URI_BYTES));
        let oversized_xml = format!(r#"<frame xmlns:v="{oversized_namespace}"><v:x/></frame>"#);
        assert!(parse_xml_tree(oversized_xml.as_bytes())
            .unwrap_err()
            .contains("namespace URI exceeds"));

        let namespace = format!(
            "urn:{}",
            "n".repeat(MAX_SADM_NAMESPACE_URI_BYTES - "urn:".len())
        );
        let children = "<v:x/>".repeat(16_384);
        let xml = format!(r#"<frame xmlns:v="{namespace}">{children}</frame>"#);
        assert!(parse_xml_tree(xml.as_bytes())
            .unwrap_err()
            .contains("expanded-name data exceeds"));
    }

    #[test]
    fn accepts_nested_payload_but_rejects_malformed_payload_container_cardinality() {
        let work = tempfile::tempdir().unwrap();
        let header = r#"<frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader>"#;
        let nested = write_xml(
            work.path(),
            "nested.xml",
            format!(
                r#"<frame version="ITU-R_BS.2125-1">{header}<coreMetadata><format><audioFormatExtended/></format></coreMetadata></frame>"#
            ),
        );
        let nested_audit = audit(&[nested]).unwrap();
        assert!(nested_audit.passed, "{:#?}", nested_audit.frames[0].rules);

        let malformed = [
            (
                "both-complete.xml",
                "<audioFormatExtended/><coreMetadata><format><audioFormatExtended/></format></coreMetadata>",
            ),
            (
                "direct-plus-empty-core.xml",
                "<audioFormatExtended/><coreMetadata/>",
            ),
            (
                "two-core.xml",
                "<coreMetadata/><coreMetadata><format><audioFormatExtended/></format></coreMetadata>",
            ),
            (
                "two-format.xml",
                "<coreMetadata><format/><format><audioFormatExtended/></format></coreMetadata>",
            ),
            ("missing-format.xml", "<coreMetadata/>"),
        ];
        for (name, payload) in malformed {
            let path = write_xml(
                work.path(),
                name,
                format!(r#"<frame version="ITU-R_BS.2125-1">{header}{payload}</frame>"#),
            );
            let malformed_audit = audit(&[path]).unwrap();
            assert!(!malformed_audit.passed, "accepted {name}");
            assert!(
                !frame_rule(&malformed_audit, 0, "BS2125-AUDIO-FORMAT-EXTENDED").passed,
                "accepted payload shape for {name}"
            );
            let state = flow_rule(&malformed_audit, "BS2125-STATE-RECONSTRUCTION");
            assert!(!state.passed, "reconstructed malformed payload for {name}");
            assert!(state.observed.contains("payload containers are ambiguous"));
        }
    }

    #[test]
    fn rejects_misplaced_unqualified_structural_names_but_allows_namespaced_extensions() {
        let work = tempfile::tempdir().unwrap();
        let valid_header = r#"<frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader>"#;
        for (name, misplaced) in [
            ("frame", "<frame/>"),
            ("frame-header", "<frameHeader/>"),
            ("frame-format", "<frameFormat/>"),
            ("transport", "<transportTrackFormat/>"),
            ("changed-ids", "<changedIDs/>"),
            ("core", "<coreMetadata/>"),
            ("format", "<format/>"),
            ("afe", "<audioFormatExtended/>"),
        ] {
            let path = write_xml(
                work.path(),
                &format!("misplaced-{name}.xml"),
                format!(
                    r#"<frame version="ITU-R_BS.2125-1">{valid_header}<audioFormatExtended>{misplaced}</audioFormatExtended></frame>"#
                ),
            );
            let audit = audit(&[path]).unwrap();
            assert!(
                !frame_rule(&audit, 0, "BS2125-STRUCTURAL-PATHS").passed,
                "accepted misplaced {name}"
            );
            assert!(
                !flow_rule(&audit, "BS2125-STATE-RECONSTRUCTION").passed,
                "reconstructed state with misplaced {name}"
            );
        }

        let extra_core_child = write_xml(
            work.path(),
            "core-direct-afe.xml",
            format!(
                r#"<frame version="ITU-R_BS.2125-1">{valid_header}<coreMetadata><audioFormatExtended/><format><audioFormatExtended/></format></coreMetadata></frame>"#
            ),
        );
        let extra_audit = audit(&[extra_core_child]).unwrap();
        assert!(!frame_rule(&extra_audit, 0, "BS2125-STRUCTURAL-PATHS").passed);
        assert!(!frame_rule(&extra_audit, 0, "BS2125-AUDIO-FORMAT-EXTENDED").passed);

        let foreign_extensions = write_xml(
            work.path(),
            "foreign-extensions.xml",
            format!(
                r#"<frame version="ITU-R_BS.2125-1">{valid_header}<audioFormatExtended xmlns:v="urn:vendor"><v:frame><v:frameHeader><v:frameFormat/><v:transportTrackFormat/><v:changedIDs/><v:coreMetadata/><v:format/><v:audioFormatExtended/></v:frameHeader></v:frame></audioFormatExtended></frame>"#
            ),
        );
        let foreign_audit = audit(&[foreign_extensions]).unwrap();
        assert!(
            frame_rule(&foreign_audit, 0, "BS2125-STRUCTURAL-PATHS").passed,
            "{:#?}",
            foreign_audit.frames[0].rules
        );
        assert!(foreign_audit.passed, "{:#?}", foreign_audit.flow_rules);
    }

    #[test]
    fn requires_the_exact_bs2125_1_root_version() {
        let work = tempfile::tempdir().unwrap();
        for (name, version, observed) in [
            (
                "missing.xml",
                "",
                "missing (interpreted as ITU-R BS.2125-0)",
            ),
            (
                "wrong.xml",
                " version=\"ITU-R_BS.2125-0\"",
                "ITU-R_BS.2125-0",
            ),
        ] {
            let path = write_xml(
                work.path(),
                name,
                format!(
                    r#"<frame{version}><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#
                ),
            );
            let audit = audit(&[path]).unwrap();
            let version_rule = frame_rule(&audit, 0, "BS2125-FRAME-VERSION");
            assert!(!version_rule.passed);
            assert_eq!(version_rule.observed, observed);
        }
    }

    #[test]
    fn accepts_all_eight_changed_id_reference_kinds() {
        let work = tempfile::tempdir().unwrap();
        let changed = r#"<changedIDs>
  <audioProgrammeIDRef status="new">APR_1001</audioProgrammeIDRef>
  <audioContentIDRef status="new">ACO_1001</audioContentIDRef>
  <audioObjectIDRef status="new">AO_1001</audioObjectIDRef>
  <audioPackFormatIDRef status="new">AP_00031001</audioPackFormatIDRef>
  <audioChannelFormatIDRef status="new">AC_00031001</audioChannelFormatIDRef>
  <audioTrackUIDRef status="new">ATU_00000001</audioTrackUIDRef>
  <audioTrackFormatIDRef status="new">AT_00031001_01</audioTrackFormatIDRef>
  <audioStreamFormatIDRef status="new">AS_00031001</audioStreamFormatIDRef>
</changedIDs>"#;
        let payload = r#"
  <audioProgramme audioProgrammeID="APR_1001"/>
  <audioContent audioContentID="ACO_1001"/>
  <audioObject audioObjectID="AO_1001"/>
  <audioPackFormat audioPackFormatID="AP_00031001"/>
  <audioChannelFormat audioChannelFormatID="AC_00031001"/>
  <audioTrackUID UID="ATU_00000001"/>
  <audioTrackFormat audioTrackFormatID="AT_00031001_01"/>
  <audioStreamFormat audioStreamFormatID="AS_00031001"/>
"#;
        let path = write_state_frame(
            work.path(),
            "all-kinds.xml",
            1,
            0,
            "header",
            changed,
            payload,
        );
        let audit = audit(&[path]).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert!(frame_rule(&audit, 0, "BS2125-CHANGED-IDS-SHAPE").passed);
        assert!(flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed);
        assert!(flow_rule(&audit, "BS2125-CHANGED-IDS-STATE")
            .observed
            .starts_with("8 declaration(s) checked"));
    }

    #[test]
    fn rejects_malformed_changed_id_lists() {
        let cases = [
            (
                "missing-status",
                "<changedIDs><audioObjectIDRef>AO_1001</audioObjectIDRef></changedIDs>",
            ),
            (
                "bad-status",
                "<changedIDs><audioObjectIDRef status=\"updated\">AO_1001</audioObjectIDRef></changedIDs>",
            ),
            (
                "empty-id",
                "<changedIDs><audioObjectIDRef status=\"new\">  </audioObjectIDRef></changedIDs>",
            ),
            (
                "duplicate",
                "<changedIDs><audioObjectIDRef status=\"new\">AO_1001</audioObjectIDRef><audioObjectIDRef status=\"new\">AO_1001</audioObjectIDRef></changedIDs>",
            ),
            (
                "unknown-child",
                "<changedIDs><audioBlockFormatIDRef status=\"new\">AB_1</audioBlockFormatIDRef></changedIDs>",
            ),
            (
                "nested-ref",
                "<changedIDs><wrapper><audioObjectIDRef status=\"new\">AO_1001</audioObjectIDRef></wrapper></changedIDs>",
            ),
            (
                "non-xml-whitespace",
                "<changedIDs>\u{a0}<audioObjectIDRef status=\"new\">AO_1001</audioObjectIDRef></changedIDs>",
            ),
        ];
        for (name, changed) in cases {
            let work = tempfile::tempdir().unwrap();
            let path = write_state_frame(
                work.path(),
                &format!("{name}.xml"),
                1,
                0,
                "header",
                changed,
                r#"<audioObject audioObjectID="AO_1001"/>"#,
            );
            let audit = audit(&[path]).unwrap();
            assert!(
                !frame_rule(&audit, 0, "BS2125-CHANGED-IDS-SHAPE").passed,
                "accepted {name}"
            );
        }
    }

    #[test]
    fn validates_all_four_statuses_against_snapshot_state() {
        let work = tempfile::tempdir().unwrap();
        let first = write_state_frame(
            work.path(),
            "first.xml",
            1,
            0,
            "header",
            "",
            r#"
  <audioObject audioObjectID="AO_CHANGE" gain="1"/>
  <audioProgramme audioProgrammeID="APR_EXPIRE"/>
  <audioChannelFormat audioChannelFormatID="AC_EXTEND"><audioBlockFormat audioBlockFormatID="AB_1" duration="1"/></audioChannelFormat>
"#,
        );
        let second = write_state_frame(
            work.path(),
            "second.xml",
            2,
            1,
            "full",
            r#"<changedIDs>
  <audioObjectIDRef status="new">AO_NEW</audioObjectIDRef>
  <audioObjectIDRef status="changed">AO_CHANGE</audioObjectIDRef>
  <audioProgrammeIDRef status="expired">APR_EXPIRE</audioProgrammeIDRef>
  <audioChannelFormatIDRef status="extended">AC_EXTEND</audioChannelFormatIDRef>
</changedIDs>"#,
            r#"
  <audioObject audioObjectID="AO_NEW"/>
  <audioObject gain="2" audioObjectID="AO_CHANGE"/>
  <audioChannelFormat audioChannelFormatID="AC_EXTEND"><audioBlockFormat duration="2" audioBlockFormatID="AB_1"/></audioChannelFormat>
"#,
        );
        let audit = audit(&[first, second]).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert!(flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed);
        assert!(flow_rule(&audit, "BS2125-STATE-RECONSTRUCTION").passed);
    }

    #[test]
    fn canonicalization_ignores_prefix_attribute_order_layout_and_entity_spelling() {
        let work = tempfile::tempdir().unwrap();
        let first = write_state_frame(
            work.path(),
            "canonical-1.xml",
            1,
            0,
            "header",
            "",
            r#"<audioObject xmlns:a="urn:adm&amp;extension" a:mode="same" audioObjectName="A&amp;B" audioObjectID="AO_1001"><a:label>A&amp;B</a:label></audioObject>"#,
        );
        let second = write_state_frame(
            work.path(),
            "canonical-2.xml",
            2,
            1,
            "full",
            r#"<changedIDs><audioObjectIDRef status="changed">AO_1001</audioObjectIDRef></changedIDs>"#,
            r#"<audioObject audioObjectID="AO_1001" xmlns:b="urn:adm&#38;extension" audioObjectName="A&#38;B" b:mode="same">
  <?vendor ignored?><b:label><![CDATA[A&B]]><!-- ignored --></b:label>
</audioObject>"#,
        );
        let audit = audit(&[first, second]).unwrap();
        assert!(!audit.passed);
        let state = flow_rule(&audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!state.passed);
        assert!(state.observed.contains("changed is false"));
    }

    #[test]
    fn canonical_timing_is_exact_domain_separated_and_status_exclusive() {
        let equivalent_work = tempfile::tempdir().unwrap();
        let equivalent = vec![
            write_state_frame(
                equivalent_work.path(),
                "equivalent-first.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1" duration="24000S48000"/>"#,
            ),
            write_state_frame(
                equivalent_work.path(),
                "equivalent-second.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject duration="0.50000" audioObjectID="AO_1"/>"#,
            ),
        ];
        let equivalent_audit = audit(&equivalent).unwrap();
        let equivalent_state = flow_rule(&equivalent_audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!equivalent_state.passed);
        assert!(equivalent_state.observed.contains("changed is false"));

        let timing_only_work = tempfile::tempdir().unwrap();
        let timing_only = vec![
            write_state_frame(
                timing_only_work.path(),
                "timing-first.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1" duration="1"/>"#,
            ),
            write_state_frame(
                timing_only_work.path(),
                "timing-second.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1" duration="2"/>"#,
            ),
        ];
        let timing_only_audit = audit(&timing_only).unwrap();
        let timing_only_state = flow_rule(&timing_only_audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!timing_only_state.passed);
        assert!(timing_only_state.observed.contains("changed is false"));

        let domain_work = tempfile::tempdir().unwrap();
        let domain_separated = vec![
            write_state_frame(
                domain_work.path(),
                "domain-first.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1" duration="P:1/2"/>"#,
            ),
            write_state_frame(
                domain_work.path(),
                "domain-second.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="extended">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1" duration="0.50000"/>"#,
            ),
        ];
        let domain_audit = audit(&domain_separated).unwrap();
        assert!(
            flow_rule(&domain_audit, "BS2125-CHANGED-IDS-STATE").passed,
            "{:#?}",
            domain_audit.flow_rules
        );
    }

    #[test]
    fn canonicalization_preserves_non_xml_whitespace() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_state_frame(
                work.path(),
                "empty-label.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1"><label/></audioObject>"#,
            ),
            write_state_frame(
                work.path(),
                "nbsp-label.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                "<audioObject audioObjectID=\"AO_1\"><label>\u{a0}</label></audioObject>",
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(
            flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed,
            "{:#?}",
            audit.flow_rules
        );
    }

    #[test]
    fn canonicalization_preserves_leaf_and_inherited_xml_space_text() {
        let leaf_work = tempfile::tempdir().unwrap();
        let leaf_paths = vec![
            write_state_frame(
                leaf_work.path(),
                "leaf-one-space.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1"><label> </label></audioObject>"#,
            ),
            write_state_frame(
                leaf_work.path(),
                "leaf-two-spaces.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1"><label>  </label></audioObject>"#,
            ),
        ];
        let leaf_audit = audit(&leaf_paths).unwrap();
        assert!(
            flow_rule(&leaf_audit, "BS2125-CHANGED-IDS-STATE").passed,
            "{:#?}",
            leaf_audit.flow_rules
        );

        let inherited_work = tempfile::tempdir().unwrap();
        let inherited_paths = vec![
            write_state_frame(
                inherited_work.path(),
                "preserve-compact.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1" xml:space="preserve"><wrapper><label/></wrapper></audioObject>"#,
            ),
            write_state_frame(
                inherited_work.path(),
                "preserve-space.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1" xml:space="preserve"><wrapper> <label/></wrapper></audioObject>"#,
            ),
        ];
        let inherited_audit = audit(&inherited_paths).unwrap();
        assert!(
            flow_rule(&inherited_audit, "BS2125-CHANGED-IDS-STATE").passed,
            "{:#?}",
            inherited_audit.flow_rules
        );
    }

    #[test]
    fn recognizes_every_normative_extended_timing_attribute() {
        let simple_cases = [
            ("audioProgramme", "audioProgrammeID", "APR_1", "start"),
            ("audioProgramme", "audioProgrammeID", "APR_1", "end"),
            ("audioObject", "audioObjectID", "AO_1", "start"),
            ("audioObject", "audioObjectID", "AO_1", "duration"),
        ];
        for (kind, id_attribute, id, timing) in simple_cases {
            let work = tempfile::tempdir().unwrap();
            let id_ref = if kind == "audioProgramme" {
                "audioProgrammeIDRef"
            } else {
                "audioObjectIDRef"
            };
            let first_payload = format!(r#"<{kind} {id_attribute}="{id}" {timing}="1"/>"#);
            let second_payload = format!(r#"<{kind} {timing}="2" {id_attribute}="{id}"/>"#);
            let paths = vec![
                write_state_frame(work.path(), "first.xml", 1, 0, "header", "", &first_payload),
                write_state_frame(
                    work.path(),
                    "second.xml",
                    2,
                    1,
                    "full",
                    &format!(
                        r#"<changedIDs><{id_ref} status="extended">{id}</{id_ref}></changedIDs>"#
                    ),
                    &second_payload,
                ),
            ];
            let audit = audit(&paths).unwrap();
            assert!(
                flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed,
                "did not recognize {kind}/@{timing}: {:#?}",
                audit.flow_rules
            );
        }

        for (kind, id_attribute, id, id_ref, non_timing) in [
            (
                "audioProgramme",
                "audioProgrammeID",
                "APR_1",
                "audioProgrammeIDRef",
                "duration",
            ),
            (
                "audioObject",
                "audioObjectID",
                "AO_1",
                "audioObjectIDRef",
                "end",
            ),
        ] {
            let work = tempfile::tempdir().unwrap();
            let first_payload = format!(r#"<{kind} {id_attribute}="{id}" {non_timing}="1"/>"#);
            let second_payload = format!(r#"<{kind} {id_attribute}="{id}" {non_timing}="2"/>"#);
            let paths = vec![
                write_state_frame(work.path(), "first.xml", 1, 0, "header", "", &first_payload),
                write_state_frame(
                    work.path(),
                    "second.xml",
                    2,
                    1,
                    "full",
                    &format!(
                        r#"<changedIDs><{id_ref} status="extended">{id}</{id_ref}></changedIDs>"#
                    ),
                    &second_payload,
                ),
            ];
            let audit = audit(&paths).unwrap();
            assert!(
                !flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed,
                "treated {kind}/@{non_timing} as timing: {:#?}",
                audit.flow_rules
            );
        }

        for timing in ["rtime", "duration", "lstart", "lduration", "ltime"] {
            let work = tempfile::tempdir().unwrap();
            let payload = |value| {
                format!(
                    r#"<audioChannelFormat audioChannelFormatID="AC_1"><audioBlockFormat audioBlockFormatID="AB_1" {timing}="{value}"/></audioChannelFormat>"#
                )
            };
            let paths = vec![
                write_state_frame(work.path(), "first.xml", 1, 0, "header", "", &payload(1)),
                write_state_frame(
                    work.path(),
                    "second.xml",
                    2,
                    1,
                    "full",
                    r#"<changedIDs><audioChannelFormatIDRef status="extended">AC_1</audioChannelFormatIDRef></changedIDs>"#,
                    &payload(2),
                ),
            ];
            let audit = audit(&paths).unwrap();
            assert!(
                flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed,
                "did not recognize audioBlockFormat/@{timing}: {:#?}",
                audit.flow_rules
            );
        }
    }

    #[test]
    fn treats_legacy_ltime_as_lstart_and_only_strips_normative_block_timing() {
        let work = tempfile::tempdir().unwrap();
        let alias_paths = vec![
            write_state_frame(
                work.path(),
                "alias-first.xml",
                1,
                0,
                "header",
                "",
                r#"<audioChannelFormat audioChannelFormatID="AC_1"><audioBlockFormat audioBlockFormatID="AB_1" ltime="24000S48000"/></audioChannelFormat>"#,
            ),
            write_state_frame(
                work.path(),
                "alias-second.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioChannelFormatIDRef status="extended">AC_1</audioChannelFormatIDRef></changedIDs>"#,
                r#"<audioChannelFormat audioChannelFormatID="AC_1"><audioBlockFormat audioBlockFormatID="AB_1" lstart="0.50000"/></audioChannelFormat>"#,
            ),
        ];
        let alias_audit = audit(&alias_paths).unwrap();
        let alias_state = flow_rule(&alias_audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!alias_state.passed);
        assert!(alias_state.observed.contains("extended is false"));

        let negative_alias_paths = vec![
            write_state_frame(
                work.path(),
                "negative-alias-first.xml",
                1,
                0,
                "header",
                "",
                r#"<audioChannelFormat audioChannelFormatID="AC_1"><audioBlockFormat audioBlockFormatID="AB_1" ltime="-24000S48000"/></audioChannelFormat>"#,
            ),
            write_state_frame(
                work.path(),
                "negative-alias-second.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioChannelFormatIDRef status="extended">AC_1</audioChannelFormatIDRef></changedIDs>"#,
                r#"<audioChannelFormat audioChannelFormatID="AC_1"><audioBlockFormat audioBlockFormatID="AB_1" lstart="-0.50000"/></audioChannelFormat>"#,
            ),
        ];
        let negative_alias_audit = audit(&negative_alias_paths).unwrap();
        let negative_alias_state = flow_rule(&negative_alias_audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!negative_alias_state.passed);
        assert!(negative_alias_state.observed.contains("extended is false"));

        for (name, block) in [
            (
                "same-local-prefix",
                r#"<audioBlockFormatVendor duration="2"/>"#,
            ),
            (
                "foreign-namespace",
                r#"<v:audioBlockFormat xmlns:v="urn:vendor" duration="2"/>"#,
            ),
            (
                "wrong-path",
                r#"<wrapper><audioBlockFormat duration="2"/></wrapper>"#,
            ),
        ] {
            let case_work = tempfile::tempdir().unwrap();
            let first_payload = match name {
                "same-local-prefix" => {
                    r#"<audioChannelFormat audioChannelFormatID="AC_1"><audioBlockFormatVendor duration="1"/></audioChannelFormat>"#
                }
                "foreign-namespace" => {
                    r#"<audioChannelFormat audioChannelFormatID="AC_1"><v:audioBlockFormat xmlns:v="urn:vendor" duration="1"/></audioChannelFormat>"#
                }
                _ => {
                    r#"<audioChannelFormat audioChannelFormatID="AC_1"><wrapper><audioBlockFormat duration="1"/></wrapper></audioChannelFormat>"#
                }
            };
            let paths = vec![
                write_state_frame(
                    case_work.path(),
                    "first.xml",
                    1,
                    0,
                    "header",
                    "",
                    first_payload,
                ),
                write_state_frame(
                    case_work.path(),
                    "second.xml",
                    2,
                    1,
                    "full",
                    r#"<changedIDs><audioChannelFormatIDRef status="extended">AC_1</audioChannelFormatIDRef></changedIDs>"#,
                    &format!(
                        r#"<audioChannelFormat audioChannelFormatID="AC_1">{block}</audioChannelFormat>"#
                    ),
                ),
            ];
            let audit = audit(&paths).unwrap();
            assert!(
                !flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed,
                "treated {name} as normative audioBlockFormat timing"
            );
        }
    }

    #[test]
    fn rejects_new_when_an_id_reappears_after_expiry() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_state_frame(
                work.path(),
                "first.xml",
                1,
                0,
                "header",
                r#"<changedIDs><audioObjectIDRef status="new">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1"/>"#,
            ),
            write_state_frame(
                work.path(),
                "second.xml",
                2,
                1,
                "full",
                r#"<changedIDs><audioObjectIDRef status="expired">AO_1</audioObjectIDRef></changedIDs>"#,
                "",
            ),
            write_state_frame(
                work.path(),
                "third.xml",
                3,
                2,
                "full",
                r#"<changedIDs><audioObjectIDRef status="new">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1"/>"#,
            ),
        ];
        let audit = audit(&paths).unwrap();
        let state = flow_rule(&audit, "BS2125-CHANGED-IDS-STATE");
        assert!(!state.passed);
        assert!(state.observed.contains("logical frame 3: new is false"));
    }

    #[test]
    fn rejects_false_status_claims_but_does_not_require_declarations() {
        let false_claims = [
            (
                "new-existing",
                r#"<audioObject audioObjectID="AO_1"/>"#,
                "new",
                r#"<audioObject audioObjectID="AO_1"/>"#,
            ),
            (
                "changed-identical",
                r#"<audioObject audioObjectID="AO_1"/>"#,
                "changed",
                r#"<audioObject audioObjectID="AO_1"/>"#,
            ),
            (
                "extended-non-timing",
                r#"<audioObject audioObjectID="AO_1" gain="1"/>"#,
                "extended",
                r#"<audioObject audioObjectID="AO_1" gain="2"/>"#,
            ),
            (
                "expired-still-present",
                r#"<audioObject audioObjectID="AO_1"/>"#,
                "expired",
                r#"<audioObject audioObjectID="AO_1"/>"#,
            ),
            ("expired-never-present", "", "expired", ""),
        ];
        for (name, before, status, after) in false_claims {
            let work = tempfile::tempdir().unwrap();
            let paths = vec![
                write_state_frame(work.path(), "first.xml", 1, 0, "header", "", before),
                write_state_frame(
                    work.path(),
                    "second.xml",
                    2,
                    1,
                    "full",
                    &format!(
                        r#"<changedIDs><audioObjectIDRef status="{status}">AO_1</audioObjectIDRef></changedIDs>"#
                    ),
                    after,
                ),
            ];
            let audit = audit(&paths).unwrap();
            assert!(
                !flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").passed,
                "accepted false claim {name}"
            );
        }

        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_state_frame(
                work.path(),
                "optional-1.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1" gain="1"/>"#,
            ),
            write_state_frame(
                work.path(),
                "optional-2.xml",
                2,
                1,
                "full",
                "",
                r#"<audioObject audioObjectID="AO_1" gain="2"/>"#,
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert_eq!(
            flow_rule(&audit, "BS2125-CHANGED-IDS-STATE").observed,
            "0 declaration(s) checked with no mismatch"
        );
    }

    #[test]
    fn intermediate_frames_patch_and_retain_omitted_state() {
        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            write_state_frame(
                work.path(),
                "first.xml",
                1,
                0,
                "header",
                "",
                r#"<audioObject audioObjectID="AO_1" gain="1"/>"#,
            ),
            write_state_frame(work.path(), "second.xml", 2, 1, "intermediate", "", ""),
            write_state_frame(
                work.path(),
                "third.xml",
                3,
                2,
                "intermediate",
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1" gain="2"/>"#,
            ),
        ];
        let audit = audit(&paths).unwrap();
        assert!(audit.passed, "{:#?}", audit.flow_rules);
        assert!(flow_rule(&audit, "BS2125-STATE-RECONSTRUCTION").passed);
        assert!(flow_rule(&audit, "BS2125-STATE-RECONSTRUCTION")
            .observed
            .ends_with("1 ADM element(s) in final state"));
    }

    #[test]
    fn combines_divided_chunks_before_validating_state_and_shape() {
        fn divided(
            directory: &Path,
            name: &str,
            base: u64,
            chunk: u8,
            start: u64,
            changed: &str,
            payload: &str,
        ) -> PathBuf {
            write_xml(
                directory,
                name,
                format!(
                    r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_{base:08X}_{chunk:02X}" start="{start}S48000" duration="24000S48000" type="divided" numMetadataChunks="2">{changed}</frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended>{payload}</audioFormatExtended></frame>"#
                ),
            )
        }

        let work = tempfile::tempdir().unwrap();
        let paths = vec![
            divided(
                work.path(),
                "1-1.xml",
                1,
                1,
                0,
                r#"<changedIDs><audioObjectIDRef status="new">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1" gain="1"/>"#,
            ),
            divided(work.path(), "1-2.xml", 1, 2, 0, "", ""),
            divided(
                work.path(),
                "2-1.xml",
                2,
                1,
                24_000,
                r#"<changedIDs><audioObjectIDRef status="changed">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1" gain="2"/>"#,
            ),
            divided(work.path(), "2-2.xml", 2, 2, 24_000, "", ""),
        ];
        let divided_audit = audit(&paths).unwrap();
        assert!(divided_audit.passed, "{:#?}", divided_audit.flow_rules);
        assert!(flow_rule(&divided_audit, "BS2125-CHANGED-IDS-STATE").passed);

        let duplicate_work = tempfile::tempdir().unwrap();
        let duplicate = vec![
            divided(
                duplicate_work.path(),
                "1-1.xml",
                1,
                1,
                0,
                r#"<changedIDs><audioObjectIDRef status="new">AO_1</audioObjectIDRef></changedIDs>"#,
                r#"<audioObject audioObjectID="AO_1"/>"#,
            ),
            divided(
                duplicate_work.path(),
                "1-2.xml",
                1,
                2,
                0,
                r#"<changedIDs><audioObjectIDRef status="new">AO_1</audioObjectIDRef></changedIDs>"#,
                "",
            ),
        ];
        let duplicate_audit = audit(&duplicate).unwrap();
        assert!(!flow_rule(&duplicate_audit, "BS2125-CHANGED-IDS-SHAPE").passed);
    }
}
