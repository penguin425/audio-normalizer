//! Bounded structural and interchange QC for AES31-3 EDML Audio Decision Lists.
//!
//! AES31-3 ADLs are plain-ASCII project documents. This module validates the
//! document envelope, core project fields, source and event identities,
//! source/track references, and sample-accurate edit timing without resolving
//! or fetching referenced media. It is not a substitute for the copyrighted
//! AES31-3 specification or a claim of complete normative conformance.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use url::Url;

pub const AES31_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/aes31-qc-v1";
const MAX_ADL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKENS: usize = 250_000;
const MAX_RECORD_VALUE_BYTES: usize = 64 * 1024;
const MAX_FINDING_DETAILS: usize = 100;

const CORE_SECTIONS: [&str; 5] = [
    "VERSION",
    "PROJECT",
    "SEQUENCE",
    "SOURCE_INDEX",
    "EVENT_LIST",
];
const KNOWN_SECTIONS: [&str; 11] = [
    "VERSION",
    "PROJECT",
    "SYSTEM",
    "SEQUENCE",
    "TRACKLIST",
    "TRACK_LIST",
    "SOURCE_INDEX",
    "EVENT_LIST",
    "GAIN_LIST",
    "PAN_LIST",
    "MUTE_LIST",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Serialize)]
pub struct Aes31Finding {
    pub rule_id: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct Aes31Audit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<Aes31Finding>,
    pub properties: Value,
}

#[derive(Clone, Debug)]
struct Record {
    keyword: String,
    value: String,
    line: usize,
}

#[derive(Clone, Debug, Default)]
struct Section {
    name: String,
    records: Vec<Record>,
}

#[derive(Clone, Debug, Default)]
struct Document {
    root_open_count: usize,
    root_close_count: usize,
    sections: Vec<Section>,
    syntax_errors: Vec<String>,
    record_count: usize,
}

#[derive(Clone, Debug)]
enum Lexeme {
    Tag {
        name: String,
        closing: bool,
        line: usize,
    },
    Record(Record),
}

#[derive(Clone, Copy, Debug)]
struct SequenceTiming {
    sample_rate: u64,
    frame_rate: FrameRate,
}

#[derive(Clone, Copy, Debug)]
struct FrameRate {
    value: f64,
    numerator: u64,
    denominator: u64,
    nominal: u64,
    drop_frame: bool,
}

#[derive(Clone, Copy, Debug)]
struct TimePoint {
    samples: f64,
}

#[derive(Clone, Debug)]
struct Source {
    index: u64,
    locator: Option<String>,
    usid: Option<String>,
    start: Option<TimePoint>,
    duration: Option<TimePoint>,
    line: usize,
}

#[derive(Clone, Debug)]
struct EventEdit {
    entry: u64,
    source: u64,
    source_tracks: Option<(u64, u64)>,
    destination_tracks: Option<(u64, u64)>,
    source_in: Option<TimePoint>,
    destination_in: Option<TimePoint>,
    destination_out: Option<TimePoint>,
    has_crossfade: bool,
    line: usize,
}

pub fn audit(path: &Path) -> Result<Aes31Audit, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("read metadata {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_ADL_BYTES {
        return Err(format!(
            "{} exceeds the {} byte AES31 ADL safety limit",
            path.display(),
            MAX_ADL_BYTES
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let invalid_ascii = bytes
        .iter()
        .enumerate()
        .filter_map(|(offset, byte)| {
            (!byte.is_ascii() || (*byte < 0x20 && !matches!(*byte, b'\t' | b'\n' | b'\r')))
                .then_some(offset)
        })
        .take(MAX_FINDING_DETAILS)
        .collect::<Vec<_>>();
    let text = String::from_utf8_lossy(&bytes);
    let (lexemes, mut lexical_errors) = lex(&text)?;
    let mut document = parse_document(lexemes);
    document.syntax_errors.append(&mut lexical_errors);

    let mut findings = Vec::new();
    finding(
        &mut findings,
        "FORGE-AES31-ASCII",
        Severity::Error,
        invalid_ascii.is_empty(),
        "ADL uses the bounded plain-ASCII EDML character repertoire",
        Some(json!({"invalid_byte_offsets": invalid_ascii})),
    );
    finding(
        &mut findings,
        "FORGE-AES31-SYNTAX",
        Severity::Error,
        document.syntax_errors.is_empty(),
        "EDML tags, records, quoting, and section nesting are structurally balanced",
        Some(json!({"errors": truncate(document.syntax_errors.clone())})),
    );
    finding(
        &mut findings,
        "FORGE-AES31-ROOT",
        Severity::Error,
        document.root_open_count == 1 && document.root_close_count == 1,
        "Document has exactly one balanced ADL root",
        Some(json!({
            "open_count": document.root_open_count,
            "close_count": document.root_close_count
        })),
    );

    let section_counts = count_sections(&document);
    let missing_sections = CORE_SECTIONS
        .iter()
        .filter(|name| section_counts.get(**name).copied().unwrap_or(0) == 0)
        .copied()
        .collect::<Vec<_>>();
    let duplicate_core_sections = CORE_SECTIONS
        .iter()
        .filter(|name| section_counts.get(**name).copied().unwrap_or(0) > 1)
        .copied()
        .collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-SECTIONS",
        Severity::Error,
        missing_sections.is_empty() && duplicate_core_sections.is_empty(),
        "Core VERSION, PROJECT, SEQUENCE, SOURCE_INDEX, and EVENT_LIST sections occur once",
        Some(json!({
            "counts": section_counts,
            "missing": missing_sections,
            "duplicate": duplicate_core_sections
        })),
    );
    let section_order = document
        .sections
        .iter()
        .map(|section| section.name.as_str())
        .collect::<Vec<_>>();
    let core_order_valid = in_declared_order(&section_order, &CORE_SECTIONS);
    finding(
        &mut findings,
        "FORGE-AES31-SECTION-ORDER",
        Severity::Error,
        core_order_valid,
        "Core ADL sections occur in interchange order",
        Some(json!({"section_order": section_order})),
    );

    let version = first_section(&document, "VERSION");
    let project = first_section(&document, "PROJECT");
    let sequence = first_section(&document, "SEQUENCE");
    let required_fields = [
        (version, "ADL_UID"),
        (version, "ADL_ID"),
        (version, "VER_ADL_VERSION"),
        (version, "VER_CREATOR"),
        (version, "VER_CRTR"),
        (project, "PROJ_TITLE"),
        (project, "PROJ_ORIGINATOR"),
        (project, "PROJ_CREATE_DATE"),
        (project, "PROJ_NOTES"),
        (project, "PROJ_CLIENT_DATA"),
        (sequence, "SEQ_SAMPLE_RATE"),
        (sequence, "SEQ_FRAME_RATE"),
        (sequence, "SEQ_ADL_LEVEL"),
        (sequence, "SEQ_CLEAN"),
        (sequence, "SEQ_DEST_START"),
    ];
    let missing_fields = required_fields
        .iter()
        .filter_map(|(section, keyword)| {
            (!section.is_some_and(|value| has_record(value, keyword))).then_some(*keyword)
        })
        .collect::<Vec<_>>();
    let duplicate_singletons = document
        .sections
        .iter()
        .filter(|section| matches!(section.name.as_str(), "VERSION" | "PROJECT" | "SEQUENCE"))
        .flat_map(|section| {
            let counts = count_keywords(section);
            counts
                .into_iter()
                .filter(|(_, count)| *count > 1)
                .map(move |(keyword, _)| format!("{}:{keyword}", section.name))
        })
        .collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-REQUIRED-FIELDS",
        Severity::Error,
        missing_fields.is_empty() && duplicate_singletons.is_empty(),
        "Core header fields are present once",
        Some(json!({
            "missing": missing_fields,
            "duplicate_singletons": duplicate_singletons
        })),
    );

    let uid = record_value(version, "ADL_UID");
    let adl_version = record_value(version, "VER_ADL_VERSION");
    let uid_valid = uid.is_some_and(valid_uid);
    let version_valid = adl_version.is_some_and(valid_adl_version);
    finding(
        &mut findings,
        "FORGE-AES31-VERSION",
        Severity::Error,
        uid_valid && version_valid,
        "ADL identity and 01.xx EDML version fields are recognizable",
        Some(json!({"adl_uid": uid, "adl_version": adl_version})),
    );

    let sample_rate_value = record_value(sequence, "SEQ_SAMPLE_RATE");
    let frame_rate_value = record_value(sequence, "SEQ_FRAME_RATE");
    let level_value = record_value(sequence, "SEQ_ADL_LEVEL");
    let sample_rate = sample_rate_value.and_then(parse_sample_rate);
    let frame_rate = frame_rate_value.and_then(parse_frame_rate);
    let adl_level = level_value.and_then(|value| unquote(value).parse::<u8>().ok());
    let timing = sample_rate
        .zip(frame_rate)
        .map(|(sample_rate, frame_rate)| SequenceTiming {
            sample_rate,
            frame_rate,
        });
    let destination_start = record_value(sequence, "SEQ_DEST_START")
        .and_then(|value| timing.and_then(|config| parse_time(value, config).ok()));
    let sequence_valid = sample_rate.is_some()
        && frame_rate.is_some()
        && adl_level.is_some_and(|level| (1..=3).contains(&level))
        && destination_start.is_some();
    finding(
        &mut findings,
        "FORGE-AES31-SEQUENCE",
        Severity::Error,
        sequence_valid,
        "Sequence sample rate, frame rate, ADL level, and destination start are valid",
        Some(json!({
            "sample_rate_hz": sample_rate,
            "frame_rate": frame_rate.map(|rate| rate.value),
            "drop_frame": frame_rate.map(|rate| rate.drop_frame),
            "adl_level": adl_level,
            "destination_start_samples": destination_start.map(|time| time.samples)
        })),
    );

    let tracks = parse_tracks(first_section_any(&document, &["TRACKLIST", "TRACK_LIST"]));
    let track_list_present = first_section_any(&document, &["TRACKLIST", "TRACK_LIST"]).is_some();
    finding(
        &mut findings,
        "FORGE-AES31-TRACK-LIST",
        Severity::Error,
        !track_list_present || contiguous_unique(&tracks),
        "An optional track list is unique and contiguous from one",
        Some(json!({"present": track_list_present, "tracks": tracks})),
    );
    let (sources, source_errors, source_locators) =
        parse_sources(first_section(&document, "SOURCE_INDEX"), timing);
    let source_indices = sources
        .iter()
        .map(|source| source.index)
        .collect::<Vec<_>>();
    let source_sequence_valid = contiguous_unique(&source_indices);
    finding(
        &mut findings,
        "FORGE-AES31-SOURCE-INDEX",
        Severity::Error,
        source_errors.is_empty() && source_sequence_valid && !sources.is_empty(),
        "Source entries are parseable, unique, and contiguous from one",
        Some(json!({
            "source_count": sources.len(),
            "indices": truncate(source_indices),
            "errors": truncate(source_errors)
        })),
    );
    let invalid_locators = sources
        .iter()
        .filter_map(|source| {
            source
                .locator
                .as_deref()
                .filter(|locator| !valid_source_locator(locator))
                .map(|locator| json!({"index": source.index, "line": source.line, "locator": locator}))
        })
        .collect::<Vec<_>>();
    let unique_locator_count = source_locators.iter().collect::<HashSet<_>>().len();
    finding(
        &mut findings,
        "FORGE-AES31-SOURCE-LOCATOR",
        Severity::Error,
        invalid_locators.is_empty() && source_locators.len() == sources.len(),
        "Every source has a syntactically valid URL resource locator",
        Some(json!({
            "locator_count": source_locators.len(),
            "unique_locator_count": unique_locator_count,
            "invalid": truncate(invalid_locators)
        })),
    );
    let usids = sources
        .iter()
        .filter_map(|source| source.usid.as_deref())
        .filter(|usid| *usid != "_")
        .collect::<Vec<_>>();
    let unique_usid_count = usids.iter().copied().collect::<HashSet<_>>().len();
    finding(
        &mut findings,
        "FORGE-AES31-SOURCE-IDENTITY",
        Severity::Warning,
        unique_locator_count == source_locators.len() && unique_usid_count == usids.len(),
        "Source locators and supplied source identifiers are unique",
        Some(json!({
            "unique_locator_count": unique_locator_count,
            "locator_count": source_locators.len(),
            "unique_usid_count": unique_usid_count,
            "supplied_usid_count": usids.len()
        })),
    );

    let (events, event_errors) = parse_events(first_section(&document, "EVENT_LIST"), timing);
    let event_indices = events.iter().map(|event| event.entry).collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-EVENT-LIST",
        Severity::Error,
        event_errors.is_empty() && contiguous_unique(&event_indices) && !events.is_empty(),
        "Event entries are parseable, unique, and contiguous from one",
        Some(json!({
            "event_count": events.len(),
            "indices": truncate(event_indices),
            "errors": truncate(event_errors)
        })),
    );
    let known_sources = sources
        .iter()
        .map(|source| source.index)
        .collect::<HashSet<_>>();
    let missing_source_references = events
        .iter()
        .filter(|event| !known_sources.contains(&event.source))
        .map(|event| json!({"entry": event.entry, "source": event.source, "line": event.line}))
        .collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-EVENT-SOURCE-REFERENCE",
        Severity::Error,
        missing_source_references.is_empty(),
        "Every edit references a declared source",
        Some(json!({"missing_references": truncate(missing_source_references)})),
    );

    let declared_tracks = tracks.iter().copied().collect::<HashSet<_>>();
    let track_errors = events
        .iter()
        .filter_map(|event| {
            let (source_tracks, destination_tracks) =
                event.source_tracks.zip(event.destination_tracks)?;
            let source_width = source_tracks.1 - source_tracks.0 + 1;
            let destination_width = destination_tracks.1 - destination_tracks.0 + 1;
            let missing = !declared_tracks.is_empty()
                && (destination_tracks.0..=destination_tracks.1)
                    .any(|track| !declared_tracks.contains(&track));
            (source_width != destination_width || missing).then_some(json!({
                "entry": event.entry,
                "source_tracks": [source_tracks.0, source_tracks.1],
                "destination_tracks": [destination_tracks.0, destination_tracks.1],
                "undeclared_destination_track": missing
            }))
        })
        .collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-EVENT-TRACK-REFERENCE",
        Severity::Error,
        track_errors.is_empty(),
        "Edit source/destination channel widths match and destination tracks are declared when a track list exists",
        Some(json!({"declared_tracks": tracks, "errors": truncate(track_errors)})),
    );

    let timing_errors = events
        .iter()
        .filter_map(|event| {
            let (source_in, destination_in, destination_out) = event
                .source_in
                .zip(event.destination_in)
                .zip(event.destination_out)
                .map(|((source_in, destination_in), destination_out)| {
                    (source_in, destination_in, destination_out)
                })?;
            (source_in.samples < 0.0
                || destination_in.samples < 0.0
                || destination_out.samples <= destination_in.samples)
                .then_some(json!({"entry": event.entry, "line": event.line}))
        })
        .collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-EVENT-TIMING",
        Severity::Error,
        events.iter().all(complete_event_timing) && timing_errors.is_empty(),
        "Every edit has non-negative source/destination in points and a positive destination duration",
        Some(json!({"errors": truncate(timing_errors)})),
    );

    let sources_by_index = sources
        .iter()
        .map(|source| (source.index, source))
        .collect::<HashMap<_, _>>();
    let source_bound_errors = events
        .iter()
        .filter_map(|event| {
            let source = sources_by_index.get(&event.source)?;
            let (source_start, source_duration, source_in, destination_in, destination_out) =
                source
                    .start
                    .zip(source.duration)
                    .zip(event.source_in)
                    .zip(event.destination_in)
                    .zip(event.destination_out)
                    .map(
                        |(
                            (((source_start, source_duration), source_in), destination_in),
                            destination_out,
                        )| {
                            (
                                source_start,
                                source_duration,
                                source_in,
                                destination_in,
                                destination_out,
                            )
                        },
                    )?;
            let source_out = source_in.samples + destination_out.samples - destination_in.samples;
            let outside = source_in.samples + 0.5 < source_start.samples
                || source_out > source_start.samples + source_duration.samples + 0.5;
            outside.then_some(json!({
                "entry": event.entry,
                "source": event.source,
                "source_in_samples": source_in.samples,
                "source_out_samples": source_out,
                "declared_start_samples": source_start.samples,
                "declared_end_samples": source_start.samples + source_duration.samples
            }))
        })
        .collect::<Vec<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-SOURCE-BOUNDS",
        Severity::Error,
        source_bound_errors.is_empty(),
        "Edits with declared source timing stay inside source bounds",
        Some(json!({"errors": truncate(source_bound_errors)})),
    );

    let (overlap_count, uncovered_overlap_count) = count_overlaps(&events);
    finding(
        &mut findings,
        "FORGE-AES31-OVERLAPS",
        Severity::Warning,
        uncovered_overlap_count == 0,
        "Overlapping destination edits have explicit crossfade evidence",
        Some(json!({
            "overlap_count": overlap_count,
            "without_crossfade_evidence": uncovered_overlap_count
        })),
    );

    let (automation_counts, automation_errors) =
        validate_automation(&document, timing, &declared_tracks);
    finding(
        &mut findings,
        "FORGE-AES31-AUTOMATION-TIMING",
        Severity::Error,
        automation_errors.is_empty(),
        "Pan, gain, mute, and marker records carry valid bounded timeline positions",
        Some(json!({
            "record_counts": automation_counts,
            "errors": truncate(automation_errors)
        })),
    );

    let unsupported_sections = document
        .sections
        .iter()
        .map(|section| section.name.as_str())
        .filter(|name| {
            !KNOWN_SECTIONS.contains(name)
                && !matches!(*name, "MARK_LIST" | "REFERENCE_LIST" | "REF_LIST")
        })
        .collect::<BTreeSet<_>>();
    finding(
        &mut findings,
        "FORGE-AES31-EXTENSIONS",
        Severity::Warning,
        unsupported_sections.is_empty(),
        "No unrecognized extension sections require producer-specific interpretation",
        Some(json!({"unrecognized_sections": unsupported_sections})),
    );

    let destination_extent = events
        .iter()
        .filter_map(|event| event.destination_out)
        .fold(None::<f64>, |maximum, point| {
            Some(maximum.map_or(point.samples, |value| value.max(point.samples)))
        });
    let warning_count = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Warning && !finding.passed)
        .count();
    let passed = findings
        .iter()
        .all(|finding| finding.severity != Severity::Error || finding.passed);
    Ok(Aes31Audit {
        schema: AES31_QC_SCHEMA,
        generator: "forge-aes31-qc",
        path: path.display().to_string(),
        passed,
        warning_count,
        properties: json!({
            "method": "forge-aes31-3-edml-structural-v1",
            "scope": "bounded AES31-3 EDML structural, identity, reference, channel-map, and sample-accurate timing QC; referenced media are not fetched or decoded; not complete normative certification",
            "document_bytes": bytes.len(),
            "record_count": document.record_count,
            "section_count": document.sections.len(),
            "adl_version": adl_version,
            "creator": record_value(version, "VER_CREATOR").map(unquote),
            "sample_rate_hz": sample_rate,
            "frame_rate": frame_rate.map(|rate| rate.value),
            "drop_frame": frame_rate.map(|rate| rate.drop_frame),
            "adl_level": adl_level,
            "track_count": tracks.len(),
            "source_count": sources.len(),
            "event_count": events.len(),
            "overlap_count": overlap_count,
            "overlap_without_crossfade_evidence_count": uncovered_overlap_count,
            "destination_extent_samples": destination_extent,
            "automation_record_counts": automation_counts,
            "limits": {
                "max_document_bytes": MAX_ADL_BYTES,
                "max_tokens": MAX_TOKENS,
                "max_record_value_bytes": MAX_RECORD_VALUE_BYTES,
                "max_reported_details_per_finding": MAX_FINDING_DETAILS
            }
        }),
        findings,
    })
}

fn lex(text: &str) -> Result<(Vec<Lexeme>, Vec<String>), String> {
    let bytes = text.as_bytes();
    let mut lexemes = Vec::new();
    let mut errors = Vec::new();
    let mut cursor = 0usize;
    let mut line = 1usize;
    let mut pending_record: Option<(String, usize, usize)> = None;
    let mut quote = false;
    let mut escaped = false;

    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'\n' {
            line += 1;
        }
        if quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quote = false;
            }
            cursor += 1;
            continue;
        }
        if byte == b'"' {
            quote = true;
            cursor += 1;
            continue;
        }
        if byte == b'<' || byte == b'(' {
            if let Some((keyword, value_start, record_line)) = pending_record.take() {
                let value = text[value_start..cursor].trim().to_owned();
                if value.len() > MAX_RECORD_VALUE_BYTES {
                    return Err(format!(
                        "record {keyword} on line {record_line} exceeds the {MAX_RECORD_VALUE_BYTES} byte value limit"
                    ));
                }
                lexemes.push(Lexeme::Record(Record {
                    keyword,
                    value,
                    line: record_line,
                }));
            } else {
                let stray = text[..cursor]
                    .rsplit_once(['>', ')'])
                    .map_or(&text[..cursor], |(_, tail)| tail)
                    .trim();
                if !stray.is_empty() {
                    errors.push(format!("line {line}: text occurs outside an EDML record"));
                }
            }
            let closing_byte = if byte == b'<' { b'>' } else { b')' };
            let Some(relative_end) = bytes[cursor + 1..]
                .iter()
                .position(|candidate| *candidate == closing_byte)
            else {
                errors.push(format!("line {line}: unterminated EDML marker"));
                break;
            };
            let end = cursor + 1 + relative_end;
            let raw = text[cursor + 1..end].trim();
            if byte == b'<' {
                let (closing, name) = raw
                    .strip_prefix('/')
                    .map_or((false, raw), |name| (true, name.trim()));
                if valid_name(name) {
                    lexemes.push(Lexeme::Tag {
                        name: name.to_ascii_uppercase(),
                        closing,
                        line,
                    });
                } else {
                    errors.push(format!("line {line}: invalid EDML tag <{raw}>"));
                }
            } else if valid_name(raw) {
                pending_record = Some((raw.to_ascii_uppercase(), end + 1, line));
            } else {
                errors.push(format!("line {line}: invalid EDML keyword ({raw})"));
            }
            cursor = end + 1;
            if lexemes.len() >= MAX_TOKENS {
                return Err(format!("ADL exceeds the {MAX_TOKENS} token safety limit"));
            }
            continue;
        }
        cursor += 1;
    }
    if let Some((keyword, value_start, record_line)) = pending_record {
        let value = text[value_start..].trim().to_owned();
        if value.len() > MAX_RECORD_VALUE_BYTES {
            return Err(format!(
                "record {keyword} on line {record_line} exceeds the {MAX_RECORD_VALUE_BYTES} byte value limit"
            ));
        }
        lexemes.push(Lexeme::Record(Record {
            keyword,
            value,
            line: record_line,
        }));
    }
    if quote {
        errors.push(format!("line {line}: unterminated quoted string"));
    }
    Ok((lexemes, errors))
}

fn parse_document(lexemes: Vec<Lexeme>) -> Document {
    let mut document = Document::default();
    let mut current: Option<Section> = None;
    let mut root_open = false;
    let mut root_closed = false;
    for lexeme in lexemes {
        match lexeme {
            Lexeme::Tag {
                name,
                closing,
                line,
            } if name == "ADL" => {
                if closing {
                    document.root_close_count += 1;
                    if !root_open || root_closed || current.is_some() {
                        document
                            .syntax_errors
                            .push(format!("line {line}: misplaced </ADL>"));
                    }
                    root_closed = true;
                } else {
                    document.root_open_count += 1;
                    if root_open || root_closed || current.is_some() {
                        document
                            .syntax_errors
                            .push(format!("line {line}: misplaced <ADL>"));
                    }
                    root_open = true;
                }
            }
            Lexeme::Tag {
                name,
                closing: false,
                line,
            } => {
                if !root_open || root_closed {
                    document
                        .syntax_errors
                        .push(format!("line {line}: <{name}> occurs outside ADL"));
                }
                if let Some(section) = current.take() {
                    document.syntax_errors.push(format!(
                        "line {line}: <{}> begins before <{}> closes",
                        name, section.name
                    ));
                    document.sections.push(section);
                }
                current = Some(Section {
                    name,
                    records: Vec::new(),
                });
            }
            Lexeme::Tag {
                name,
                closing: true,
                line,
            } => match current.take() {
                Some(section) if section.name == name => document.sections.push(section),
                Some(section) => {
                    document
                        .syntax_errors
                        .push(format!("line {line}: </{name}> closes <{}>", section.name));
                    document.sections.push(section);
                }
                None => document
                    .syntax_errors
                    .push(format!("line {line}: unmatched </{name}>")),
            },
            Lexeme::Record(record) => {
                document.record_count += 1;
                if let Some(section) = current.as_mut() {
                    section.records.push(record);
                } else {
                    document.syntax_errors.push(format!(
                        "line {}: ({}) occurs outside a section",
                        record.line, record.keyword
                    ));
                }
            }
        }
    }
    if let Some(section) = current {
        document
            .syntax_errors
            .push(format!("unclosed <{}> section", section.name));
        document.sections.push(section);
    }
    if root_open && !root_closed {
        document
            .syntax_errors
            .push("unclosed <ADL> root".to_owned());
    }
    document
}

fn parse_tracks(section: Option<&Section>) -> Vec<u64> {
    section
        .into_iter()
        .flat_map(|section| &section.records)
        .filter(|record| record.keyword == "TRACK")
        .filter_map(|record| split_fields(&record.value).first()?.parse().ok())
        .collect()
}

fn parse_sources(
    section: Option<&Section>,
    timing: Option<SequenceTiming>,
) -> (Vec<Source>, Vec<String>, Vec<String>) {
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    let mut locators = Vec::new();
    let mut pending: Option<Source> = None;
    for record in section.into_iter().flat_map(|section| &section.records) {
        match record.keyword.as_str() {
            "INDEX" => {
                if let Some(source) = pending.take() {
                    if source.locator.is_none() {
                        errors.push(format!(
                            "line {}: source {} has no file record",
                            source.line, source.index
                        ));
                    }
                    sources.push(source);
                }
                let fields = split_fields(&record.value);
                let Some(index) = fields.first().and_then(|value| value.parse::<u64>().ok()) else {
                    errors.push(format!("line {}: invalid source Index", record.line));
                    continue;
                };
                pending = Some(Source {
                    index,
                    locator: None,
                    usid: None,
                    start: None,
                    duration: None,
                    line: record.line,
                });
            }
            "F" => {
                let Some(source) = pending.as_mut() else {
                    errors.push(format!("line {}: file record precedes Index", record.line));
                    continue;
                };
                if source.locator.is_some() {
                    errors.push(format!(
                        "line {}: source {} has duplicate file records",
                        record.line, source.index
                    ));
                    continue;
                }
                let fields = split_fields(&record.value);
                if let Some(locator) = fields.first() {
                    let locator = unquote(locator).to_owned();
                    locators.push(locator.clone());
                    source.locator = Some(locator);
                } else {
                    errors.push(format!("line {}: empty source file record", record.line));
                }
                source.usid = fields.get(1).map(|value| unquote(value).to_owned());
                if let Some(config) = timing {
                    let time_fields = fields
                        .iter()
                        .filter_map(|value| parse_time(value, config).ok())
                        .take(2)
                        .collect::<Vec<_>>();
                    source.start = time_fields.first().copied();
                    source.duration = time_fields.get(1).copied();
                    if time_fields.len() == 1 {
                        errors.push(format!(
                            "line {}: source {} supplies only one of start/duration timing",
                            record.line, source.index
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(source) = pending {
        if source.locator.is_none() {
            errors.push(format!(
                "line {}: source {} has no file record",
                source.line, source.index
            ));
        }
        sources.push(source);
    }
    (sources, errors, locators)
}

fn parse_events(
    section: Option<&Section>,
    timing: Option<SequenceTiming>,
) -> (Vec<EventEdit>, Vec<String>) {
    let mut events: Vec<EventEdit> = Vec::new();
    let mut errors = Vec::new();
    let mut entry: Option<(u64, usize)> = None;
    let mut has_crossfade = false;
    for record in section.into_iter().flat_map(|section| &section.records) {
        match record.keyword.as_str() {
            "ENTRY" => {
                if let Some((_, prior_line)) = entry {
                    errors.push(format!(
                        "line {}: event Entry has no Cut record",
                        prior_line
                    ));
                }
                let fields = split_fields(&record.value);
                entry = fields
                    .first()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|number| (number, record.line));
                if entry.is_none() {
                    errors.push(format!("line {}: invalid event Entry", record.line));
                }
                has_crossfade = false;
            }
            "XFADE" => {
                has_crossfade = true;
                if entry.is_none() {
                    if let Some(event) = events.last_mut() {
                        event.has_crossfade = true;
                    }
                }
            }
            "CUT" => {
                let Some((entry_number, entry_line)) = entry.take() else {
                    errors.push(format!("line {}: Cut precedes event Entry", record.line));
                    continue;
                };
                let fields = split_fields(&record.value);
                if fields.len() < 7 {
                    errors.push(format!(
                        "line {}: event {} Cut has fewer than seven fields",
                        record.line, entry_number
                    ));
                    continue;
                }
                let source = fields.get(1).and_then(|value| value.parse::<u64>().ok());
                let source_tracks = fields.get(2).and_then(|value| parse_track_range(value));
                let destination_tracks = fields.get(3).and_then(|value| parse_track_range(value));
                let times = timing.map(|config| {
                    fields
                        .iter()
                        .skip(4)
                        .filter_map(|value| parse_time(value, config).ok())
                        .take(3)
                        .collect::<Vec<_>>()
                });
                let Some(source) = source else {
                    errors.push(format!(
                        "line {}: event {} has invalid source reference",
                        record.line, entry_number
                    ));
                    continue;
                };
                if source_tracks.is_none() || destination_tracks.is_none() {
                    errors.push(format!(
                        "line {}: event {} has invalid track mapping",
                        record.line, entry_number
                    ));
                }
                let times = times.unwrap_or_default();
                if times.len() != 3 {
                    errors.push(format!(
                        "line {}: event {} lacks three valid time values",
                        record.line, entry_number
                    ));
                }
                events.push(EventEdit {
                    entry: entry_number,
                    source,
                    source_tracks,
                    destination_tracks,
                    source_in: times.first().copied(),
                    destination_in: times.get(1).copied(),
                    destination_out: times.get(2).copied(),
                    has_crossfade,
                    line: entry_line,
                });
            }
            _ => {}
        }
    }
    if let Some((_, line)) = entry {
        errors.push(format!("line {line}: event Entry has no Cut record"));
    }
    (events, errors)
}

fn validate_automation(
    document: &Document,
    timing: Option<SequenceTiming>,
    declared_tracks: &HashSet<u64>,
) -> (BTreeMap<String, usize>, Vec<String>) {
    let mut counts = BTreeMap::new();
    let mut errors = Vec::new();
    for section in &document.sections {
        if !matches!(
            section.name.as_str(),
            "PAN_LIST" | "GAIN_LIST" | "MUTE_LIST" | "MARK_LIST"
        ) {
            continue;
        }
        counts.insert(section.name.clone(), section.records.len());
        for record in &section.records {
            let fields = split_fields(&record.value);
            let time_valid = timing
                .is_some_and(|config| fields.iter().any(|field| parse_time(field, config).is_ok()));
            if !time_valid {
                errors.push(format!(
                    "line {}: {} ({}) lacks a valid time value",
                    record.line, section.name, record.keyword
                ));
            }
            if section.name != "MARK_LIST" && !declared_tracks.is_empty() {
                let track = fields.first().and_then(|field| field.parse::<u64>().ok());
                if track.is_some_and(|track| !declared_tracks.contains(&track)) {
                    errors.push(format!(
                        "line {}: {} references undeclared track {}",
                        record.line,
                        section.name,
                        track.unwrap()
                    ));
                }
            }
        }
    }
    (counts, errors)
}

fn count_overlaps(events: &[EventEdit]) -> (usize, usize) {
    let mut count = 0;
    let mut uncovered = 0;
    for (index, left) in events.iter().enumerate() {
        let Some((left_tracks, left_in, left_out)) = left
            .destination_tracks
            .zip(left.destination_in)
            .zip(left.destination_out)
            .map(|((tracks, start), end)| (tracks, start.samples, end.samples))
        else {
            continue;
        };
        for right in &events[index + 1..] {
            let Some((right_tracks, right_in, right_out)) = right
                .destination_tracks
                .zip(right.destination_in)
                .zip(right.destination_out)
                .map(|((tracks, start), end)| (tracks, start.samples, end.samples))
            else {
                continue;
            };
            let tracks_overlap = left_tracks.0 <= right_tracks.1 && right_tracks.0 <= left_tracks.1;
            if tracks_overlap && left_in < right_out && right_in < left_out {
                count += 1;
                if !left.has_crossfade && !right.has_crossfade {
                    uncovered += 1;
                }
            }
        }
    }
    (count, uncovered)
}

fn parse_sample_rate(value: &str) -> Option<u64> {
    let value = unquote(value).trim().trim_start_matches(['S', 's']);
    let rate = value.parse::<u64>().ok()?;
    (8_000..=768_000).contains(&rate).then_some(rate)
}

fn parse_frame_rate(value: &str) -> Option<FrameRate> {
    let normalized = unquote(value)
        .trim()
        .trim_start_matches(['F', 'f'])
        .to_ascii_uppercase();
    let drop_frame = normalized.ends_with("DF") && !normalized.ends_with("NDF");
    let normalized = normalized
        .strip_suffix("NDF")
        .or_else(|| normalized.strip_suffix("DF"))
        .unwrap_or(&normalized);
    let rate = normalized.parse::<f64>().ok()?;
    let (numerator, denominator, nominal) = if f64::abs(rate - 23.976) < 0.001 {
        (24_000, 1_001, 24)
    } else if f64::abs(rate - 29.97) < 0.001 {
        (30_000, 1_001, 30)
    } else if f64::abs(rate - 59.94) < 0.001 {
        (60_000, 1_001, 60)
    } else {
        let integer = rate.round() as u64;
        if ![24, 25, 30, 48, 50, 60].contains(&integer) || f64::abs(rate - integer as f64) >= 0.001
        {
            return None;
        }
        (integer, 1, integer)
    };
    if drop_frame && !matches!((numerator, denominator), (30_000, 1_001) | (60_000, 1_001)) {
        return None;
    }
    Some(FrameRate {
        value: rate,
        numerator,
        denominator,
        nominal,
        drop_frame,
    })
}

fn parse_time(value: &str, timing: SequenceTiming) -> Result<TimePoint, ()> {
    let value = unquote(value);
    if value
        .chars()
        .any(|character| character.is_ascii_alphabetic())
    {
        return Err(());
    }
    let parts = digit_runs(value);
    if parts.len() != 5 {
        return Err(());
    }
    let hours = parts[0];
    let minutes = parts[1];
    let seconds = parts[2];
    let frames = parts[3];
    let remainder = parts[4];
    let frame_rate = timing.frame_rate;
    let samples_per_frame =
        timing.sample_rate as f64 * frame_rate.denominator as f64 / frame_rate.numerator as f64;
    if minutes >= 60
        || seconds >= 60
        || frames >= frame_rate.nominal
        || remainder as f64 >= samples_per_frame.ceil()
    {
        return Err(());
    }
    let total_minutes = hours * 60 + minutes;
    let mut frame_number = ((hours * 3_600 + minutes * 60 + seconds) * frame_rate.nominal) + frames;
    let drop_frame = frame_rate.drop_frame || value.contains(';');
    if drop_frame {
        if !matches!(
            (frame_rate.numerator, frame_rate.denominator),
            (30_000, 1_001) | (60_000, 1_001)
        ) {
            return Err(());
        }
        let dropped_per_minute = if frame_rate.nominal == 60 { 4 } else { 2 };
        if !minutes.is_multiple_of(10) && seconds == 0 && frames < dropped_per_minute {
            return Err(());
        }
        let dropped = dropped_per_minute * (total_minutes - total_minutes / 10);
        frame_number = frame_number.checked_sub(dropped).ok_or(())?;
    }
    let samples = frame_number as f64 * samples_per_frame + remainder as f64;
    Ok(TimePoint { samples })
}

fn digit_runs(value: &str) -> Vec<u64> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

fn parse_track_range(value: &str) -> Option<(u64, u64)> {
    let parts = value
        .split('~')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let (start, end) = match parts.as_slice() {
        [single] => (*single, *single),
        [start, end] => (*start, *end),
        _ => return None,
    };
    (start > 0 && start <= end).then_some((start, end))
}

fn split_fields(value: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quote = false;
    let mut escaped = false;
    for character in value.chars() {
        if quote {
            current.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quote = false;
            }
        } else if character == '"' {
            quote = true;
            current.push(character);
        } else if character.is_whitespace() {
            if !current.is_empty() {
                fields.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

fn valid_source_locator(locator: &str) -> bool {
    let Some(value) = locator.strip_prefix("URL:") else {
        return false;
    };
    Url::parse(value).is_ok_and(|url| !url.scheme().is_empty())
}

fn valid_uid(value: &str) -> bool {
    let value = unquote(value);
    let groups = value.split('-').map(str::len).collect::<Vec<_>>();
    groups == [8, 4, 4, 4, 12]
        && value
            .chars()
            .filter(|character| *character != '-')
            .all(|character| character.is_ascii_hexdigit())
}

fn valid_adl_version(value: &str) -> bool {
    let value = unquote(value);
    let Some((major, minor)) = value.split_once('.') else {
        return false;
    };
    major == "01" && minor.len() == 2 && minor.chars().all(|character| character.is_ascii_digit())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn complete_event_timing(event: &EventEdit) -> bool {
    event.source_in.is_some() && event.destination_in.is_some() && event.destination_out.is_some()
}

fn contiguous_unique(values: &[u64]) -> bool {
    values
        .iter()
        .copied()
        .eq(1..=u64::try_from(values.len()).unwrap_or(u64::MAX))
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

fn count_sections(document: &Document) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for section in &document.sections {
        *counts.entry(section.name.clone()).or_insert(0) += 1;
    }
    counts
}

fn count_keywords(section: &Section) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for record in &section.records {
        *counts.entry(record.keyword.clone()).or_insert(0) += 1;
    }
    counts
}

fn first_section<'a>(document: &'a Document, name: &str) -> Option<&'a Section> {
    document
        .sections
        .iter()
        .find(|section| section.name == name)
}

fn first_section_any<'a>(document: &'a Document, names: &[&str]) -> Option<&'a Section> {
    document
        .sections
        .iter()
        .find(|section| names.contains(&section.name.as_str()))
}

fn has_record(section: &Section, keyword: &str) -> bool {
    section
        .records
        .iter()
        .any(|record| record.keyword == keyword)
}

fn record_value<'a>(section: Option<&'a Section>, keyword: &str) -> Option<&'a str> {
    section?
        .records
        .iter()
        .find(|record| record.keyword == keyword)
        .map(|record| record.value.as_str())
}

fn in_declared_order(actual: &[&str], expected: &[&str]) -> bool {
    let mut last = None;
    for name in expected {
        let Some(position) = actual.iter().position(|candidate| candidate == name) else {
            continue;
        };
        if last.is_some_and(|last| position <= last) {
            return false;
        }
        last = Some(position);
    }
    true
}

fn truncate<T>(mut values: Vec<T>) -> Vec<T> {
    values.truncate(MAX_FINDING_DETAILS);
    values
}

fn finding(
    findings: &mut Vec<Aes31Finding>,
    rule_id: &'static str,
    severity: Severity,
    passed: bool,
    message: impl Into<String>,
    observed: Option<Value>,
) {
    findings.push(Aes31Finding {
        rule_id,
        severity,
        passed,
        message: message.into(),
        observed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const VALID_ADL: &str = r#"<ADL>
<VERSION>
(ADL_ID) "06,64,43,52,01,01,01,04,01,02,03,04,"
(ADL_UID) 12345678-1234-4234-8234-123456789abc
(VER_ADL_VERSION) 01.02
(VER_CREATOR) "Forge fixture"
(VER_CRTR) 01.00
</VERSION>
<PROJECT>
(PROJ_TITLE) "Bounded test"
(PROJ_ORIGINATOR) "Forge"
(PROJ_CREATE_DATE) 2026-07-29T12:00:00Z
(PROJ_NOTES) ""
(PROJ_CLIENT_DATA) ""
</PROJECT>
<SEQUENCE>
(SEQ_SAMPLE_RATE) S48000
(SEQ_FRAME_RATE) 25
(SEQ_ADL_LEVEL) 1
(SEQ_CLEAN) TRUE
(SEQ_DEST_START) 00:00:00:00/0000
</SEQUENCE>
<TRACKLIST>
(Track) 1 "Left"
(Track) 2 "Right"
</TRACKLIST>
<SOURCE_INDEX>
(Index) 1 (F) "URL:file://localhost/audio/one.wav" USID-ONE
 00:00:00:00/0000 00:00:10:00/0000 "One" N
(Index) 2
(F) "URL:file://localhost/audio/two.wav" USID-TWO 00|00|00.00*0000
 00|00|10.00*0000 "Two" N
</SOURCE_INDEX>
<EVENT_LIST>
(Entry) 1 (Cut) I 1 1~2 1~2 00:00:00:00/0000 00:00:00:00/0000 00:00:05:00/0000 _
(Rem) NAME "First"
(Entry) 2
(Cut) I 2 1~2 1~2 00|00|00.00*0000 00|00|05.00*0000 00|00|10.00*0000 _
</EVENT_LIST>
<PAN_LIST>
(PP) 1 00:00:00:00/0000 -100.0 0.0
(PP) 2 00:00:00:00/0000 100.0 0.0
</PAN_LIST>
<MARK_LIST>
(MK-PQ-START) 0 00:00:00:00/0000 _ "Start"
</MARK_LIST>
</ADL>
"#;

    #[test]
    fn valid_multiline_and_compact_records_pass() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("project.adl");
        fs::write(&path, VALID_ADL).unwrap();
        let audit = audit(&path).unwrap();
        assert!(audit.passed, "{:#?}", audit.findings);
        assert_eq!(audit.properties["source_count"], 2);
        assert_eq!(audit.properties["event_count"], 2);
        assert_eq!(
            audit.properties["method"],
            "forge-aes31-3-edml-structural-v1"
        );
    }

    #[test]
    fn broken_references_tracks_and_bounds_fail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken.adl");
        let broken = VALID_ADL
            .replace("(Cut) I 2 1~2 1~2", "(Cut) I 9 1~2 2~4")
            .replace(
                "00|00|05.00*0000 00|00|10.00*0000",
                "00|00|09.00*0000 00|00|12.00*0000",
            );
        fs::write(&path, broken).unwrap();
        let audit = audit(&path).unwrap();
        assert!(!audit.passed);
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-AES31-EVENT-SOURCE-REFERENCE" && !finding.passed
        }));
        assert!(audit.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-AES31-EVENT-TRACK-REFERENCE" && !finding.passed
        }));
    }

    #[test]
    fn malformed_edml_is_reported_without_panicking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("malformed.adl");
        fs::write(&path, "<ADL><VERSION>(ADL_UID) \"unterminated</VERSION>").unwrap();
        let audit = audit(&path).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .findings
            .iter()
            .any(|finding| finding.rule_id == "FORGE-AES31-SYNTAX" && !finding.passed));
    }

    #[test]
    fn fractional_drop_frame_time_is_exact_and_rejects_skipped_labels() {
        let rate = parse_frame_rate("29.97DF").unwrap();
        let timing = SequenceTiming {
            sample_rate: 48_000,
            frame_rate: rate,
        };
        let ten_minutes = parse_time("00:10:00:00/0000", timing).unwrap();
        assert!((ten_minutes.samples - 28_799_971.2).abs() < 0.01);
        assert!(parse_time("00:01:00:00/0000", timing).is_err());
        assert!(parse_time("00:01:00:02/0000", timing).is_ok());
    }
}
