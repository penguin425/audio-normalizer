//! ITU-R BS.2125-1 S-ADM frame and flow validation.

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const STANDARD: &str = "ITU-R BS.2125-1";
pub const VERSION: &str = "05/2022";
pub const VALIDATOR: &str = "forge-bs2125-1-flow-2";

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
    attributes: HashMap<String, String>,
    changed_statuses: Vec<String>,
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
    let mut frames = Vec::with_capacity(paths.len());
    let mut parsed_frames = Vec::with_capacity(paths.len());
    for (offset, path) in paths.iter().enumerate() {
        let xml = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let parsed =
            parse_frame(&xml).map_err(|error| format!("parse {}: {error}", path.display()))?;
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
            let chunk = attribute(frame, "frameFormatID")
                .and_then(parse_frame_format_id)
                .and_then(|id| id.chunk);
            let next_occurrence = chunk.and_then(|expected| {
                logical_frames
                    .iter()
                    .enumerate()
                    .skip(logical_index + 1)
                    .find(|(_, candidate)| {
                        logical_contains_chunk(candidate, parsed_frames, expected)
                    })
                    .map(|(index, _)| index)
            });
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
                    let next_full = logical_types
                        .iter()
                        .enumerate()
                        .skip(logical_index + 1)
                        .find(|(_, kind)| **kind == Some("full"))
                        .map(|(index, _)| index);
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
            "/frame/audioFormatExtended",
            "exactly one audioFormatExtended payload",
            format!(
                "{} audioFormatExtended element(s)",
                parsed.audio_format_extended
            ),
            parsed.audio_format_extended == 1,
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
    let invalid_statuses = parsed
        .changed_statuses
        .iter()
        .filter(|status| !matches!(status.as_str(), "new" | "changed" | "expired" | "extended"))
        .cloned()
        .collect::<Vec<_>>();
    rules.push(rule(
        "BS2125-CHANGED-IDS-STATUS",
        "/frame/frameHeader/frameFormat/changedIDs/*/@status",
        "changed ID status shall be new, changed, expired, or extended",
        if invalid_statuses.is_empty() {
            "all statuses valid"
        } else {
            "invalid status present"
        },
        invalid_statuses.is_empty(),
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
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedFrame::default();
    let mut depth = 0_usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                observe_element(&element, depth, &mut parsed)?;
                depth += 1;
            }
            Ok(Event::Empty(element)) => {
                observe_element(&element, depth, &mut parsed)?;
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("XML: {error}")),
            _ => {}
        }
    }
    Ok(parsed)
}

fn observe_element(
    element: &quick_xml::events::BytesStart<'_>,
    depth: usize,
    parsed: &mut ParsedFrame,
) -> Result<(), String> {
    let name = local_name(element.name().as_ref());
    if name == "frame" && depth == 0 {
        parsed.roots += 1;
    } else if name == "frameHeader" {
        parsed.frame_headers += 1;
    } else if name == "frameFormat" {
        parsed.frame_formats += 1;
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
            let key = local_name(attribute.key.as_ref());
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|error| format!("XML attribute value: {error}"))?
                .into_owned();
            parsed.attributes.insert(key, value);
        }
    } else if name == "transportTrackFormat" {
        parsed.transport_track_formats += 1;
    } else if name == "audioFormatExtended" {
        parsed.audio_format_extended += 1;
    } else if name.ends_with("IDRef") {
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
            if local_name(attribute.key.as_ref()) == "status" {
                parsed.changed_statuses.push(
                    attribute
                        .normalized_value(XmlVersion::Implicit1_0)
                        .map_err(|error| format!("XML status: {error}"))?
                        .into_owned(),
                );
            }
        }
    }
    Ok(())
}

fn group_logical_frames(parsed_frames: &[ParsedFrame]) -> Vec<LogicalFrame> {
    let mut logical_frames: Vec<LogicalFrame> = Vec::new();
    for (index, frame) in parsed_frames.iter().enumerate() {
        let id = attribute(frame, "frameFormatID").and_then(parse_frame_format_id);
        let joins_previous = id.is_some_and(|id| {
            id.chunk.is_some()
                && logical_frames.last().is_some_and(|logical| {
                    logical.base == Some(id.base)
                        && logical.members.iter().all(|member| {
                            attribute(&parsed_frames[*member], "frameFormatID")
                                .and_then(parse_frame_format_id)
                                .is_some_and(|candidate| candidate.chunk.is_some())
                        })
                })
        });
        if joins_previous {
            logical_frames.last_mut().unwrap().members.push(index);
        } else {
            logical_frames.push(LogicalFrame {
                base: id.map(|id| id.base),
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

fn logical_contains_chunk(
    logical: &LogicalFrame,
    parsed_frames: &[ParsedFrame],
    expected: u8,
) -> bool {
    logical.members.iter().any(|candidate| {
        attribute(&parsed_frames[*candidate], "frameFormatID")
            .and_then(parse_frame_format_id)
            .and_then(|id| id.chunk)
            == Some(expected)
    })
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

fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_owned()
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
        fs::write(&first, r#"<frame><frameHeader><frameFormat frameFormatID="FF_00000001" start="0S48000" duration="24000S48000" type="header"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#).unwrap();
        fs::write(&second, r#"<frame><frameHeader><frameFormat frameFormatID="FF_00000003" start="30000S48000" duration="24000S48000" type="intermediate"><changedIDs><audioObjectIDRef status="bad">AO_1001</audioObjectIDRef></changedIDs></frameFormat><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#).unwrap();
        let audit = audit(&[first, second]).unwrap();
        assert!(!audit.passed);
    }
}
