//! RFC 8216 and Apple HLS package validation with local CMAF/MPEG-TS cross-checks.

use crate::container_qc;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

pub const HLS_QC_SCHEMA: &str = "https://penguin425.github.io/audio-normalizer/schema/hls-qc-v1";
const MAX_PLAYLIST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REFERENCED_PLAYLISTS: usize = 4_096;
const MAX_IPF_SAMPLE_BYTES: u64 = 16 * 1024 * 1024;
const MPEG_PTS_MODULUS: u64 = 1_u64 << 33;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HlsProfile {
    Rfc8216,
    AppleHls,
    LlHls,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Serialize)]
pub struct HlsFinding {
    pub rule_id: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct HlsAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub profile: HlsProfile,
    pub kind: String,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<HlsFinding>,
    pub properties: Value,
}

#[derive(Default)]
struct Playlist {
    path: PathBuf,
    kind: &'static str,
    target_duration: Option<u64>,
    total_duration: f64,
    segment_durations: Vec<f64>,
    segment_uris: Vec<String>,
    segment_discontinuities: Vec<bool>,
    map_uri: Option<String>,
    referenced_playlists: Vec<String>,
    media_renditions: Vec<MediaRendition>,
    has_endlist: bool,
    playlist_type: Option<String>,
    version: Option<u64>,
    media_sequence: Option<u64>,
    discontinuity_sequence: Option<u64>,
    is_fmp4: bool,
    part_target: Option<f64>,
    parts: Vec<PartialSegment>,
    server_control: Option<ServerControl>,
    skipped_segments: Option<u64>,
    has_recently_removed_dateranges: bool,
    preload_hints: Vec<PreloadHint>,
    rendition_reports: Vec<RenditionReport>,
    program_date_time_count: usize,
    has_i_frames_only: bool,
    has_independent_segments: bool,
}

#[derive(Clone, Debug)]
struct XheTrackInit {
    track_id: u64,
    audio_preroll: Option<crate::aac_qc::UsacAudioPrerollConfig>,
}

#[derive(Clone, Debug)]
struct MediaRendition {
    kind: String,
    group_id: String,
    name: String,
    language: Option<String>,
    uri: Option<String>,
}

#[derive(Clone, Debug)]
struct PartialSegment {
    uri: String,
    duration: f64,
    independent: bool,
    gap: bool,
    parent: usize,
    discontinuity_sequence: u64,
    byterange: Option<(u64, Option<u64>)>,
}

#[derive(Clone, Debug, Default)]
struct ServerControl {
    can_skip_until: Option<f64>,
    can_skip_dateranges: bool,
    hold_back: Option<f64>,
    part_hold_back: Option<f64>,
    can_block_reload: bool,
}

#[derive(Clone, Debug)]
struct PreloadHint {
    kind: String,
    uri: String,
}

#[derive(Clone, Debug)]
struct RenditionReport {
    uri: String,
    last_msn: Option<u64>,
    last_part: Option<u64>,
}

#[derive(Debug, Eq, PartialEq)]
enum LocalReference {
    External,
    Invalid(&'static str),
    Path(PathBuf),
}

type InvalidReferenceSet = HashSet<(PathBuf, String)>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TsAudioStream {
    program: u64,
    pid: u64,
    stream_type: u64,
    codec: String,
    language: Option<String>,
    first_pts: Option<u64>,
    last_pts: Option<u64>,
}

impl TsAudioStream {
    fn configuration(&self) -> (u64, u64, u64, &str, Option<&str>) {
        (
            self.program,
            self.pid,
            self.stream_type,
            &self.codec,
            self.language.as_deref(),
        )
    }
}

/// Audits an HLS playlist and the local package resources it references.
///
/// # Input stability and security
///
/// The caller must keep the entry playlist, its package root (the playlist's parent directory),
/// and every referenced playlist and resource quiescent for the entire call. This function
/// performs canonical containment checks when it resolves local references, but those checks do
/// not provide a filesystem snapshot or isolation against concurrent replacement or mutation.
/// Callers must materialize untrusted input into a caller-owned, unshared snapshot before calling
/// this function.
pub fn audit(path: &Path, profile: HlsProfile) -> Result<HlsAudit, String> {
    let mut findings = Vec::new();
    let mut invalid_references = InvalidReferenceSet::new();
    let package_root = package_root(path)?;
    let root = parse_playlist(path, profile, &mut findings)?;
    audit_reference_safety(&package_root, &root, &mut invalid_references, &mut findings);
    let root_path = root.path.clone();
    let root_independent_segments = root.has_independent_segments;
    let media_renditions = root.media_renditions.clone();
    let mut media = Vec::new();
    if root.kind == "multivariant" {
        let mut seen = HashSet::new();
        for uri in &root.referenced_playlists {
            if media.len() == MAX_REFERENCED_PLAYLISTS {
                findings.push(finding(
                    "FORGE-HLS-PLAYLIST-LIMIT",
                    Severity::Error,
                    false,
                    "referenced playlist count exceeds the safety limit",
                    Some(json!(MAX_REFERENCED_PLAYLISTS)),
                ));
                break;
            }
            let reference = match checked_local_reference(
                &package_root,
                &root.path,
                "playlist",
                uri,
                &mut invalid_references,
                &mut findings,
            ) {
                LocalReference::Path(path) => path,
                LocalReference::External => {
                    findings.push(finding(
                        "FORGE-HLS-REMOTE-REFERENCE",
                        Severity::Warning,
                        false,
                        format!("remote playlist was not fetched: {uri}"),
                        Some(json!(uri)),
                    ));
                    continue;
                }
                LocalReference::Invalid(_) => continue,
            };
            if !seen.insert(reference.clone()) {
                continue;
            }
            match parse_playlist(&reference, profile, &mut findings) {
                Ok(playlist) => {
                    audit_reference_safety(
                        &package_root,
                        &playlist,
                        &mut invalid_references,
                        &mut findings,
                    );
                    if playlist.kind == "media" {
                        media.push(playlist);
                    } else {
                        findings.push(finding(
                            "FORGE-HLS-RENDITION-KIND",
                            Severity::Error,
                            false,
                            format!(
                                "referenced playlist is not a Media Playlist: {}",
                                reference.display()
                            ),
                            None,
                        ));
                    }
                }
                Err(error) => findings.push(finding(
                    "FORGE-HLS-RENDITION-READ",
                    Severity::Error,
                    false,
                    error,
                    Some(json!(reference)),
                )),
            }
        }
        findings.push(finding(
            "FORGE-HLS-RENDITION-SET",
            Severity::Warning,
            !media.is_empty(),
            if media.is_empty() {
                "no local Media Playlist could be validated"
            } else {
                "referenced local Media Playlists were loaded"
            },
            Some(json!(media.len())),
        ));
        if profile == HlsProfile::LlHls {
            let expected = root
                .referenced_playlists
                .iter()
                .collect::<HashSet<_>>()
                .len();
            findings.push(finding(
                "FORGE-LL-HLS-LOCAL-RENDITIONS",
                Severity::Error,
                media.len() == expected,
                "every referenced Low-Latency Media Playlist is available for local validation",
                Some(json!({"expected": expected, "loaded": media.len()})),
            ));
        }
    } else {
        media.push(root);
    }

    for playlist in &media {
        audit_media_files(
            &package_root,
            playlist,
            profile,
            root_independent_segments,
            &mut invalid_references,
            &mut findings,
        );
    }
    cross_check_renditions(
        &package_root,
        &media,
        profile,
        &mut invalid_references,
        &mut findings,
    );
    cross_check_cmaf_audio_renditions(
        &package_root,
        &root_path,
        &media_renditions,
        &media,
        &mut invalid_references,
        &mut findings,
    );

    let passed = findings
        .iter()
        .all(|item| item.severity != Severity::Error || item.passed);
    let warning_count = findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    let kind = if media.len() == 1 && media[0].path == path {
        "media"
    } else {
        "multivariant"
    };
    Ok(HlsAudit {
        schema: HLS_QC_SCHEMA,
        generator: "forge-streaming-qc",
        path: path.display().to_string(),
        profile,
        kind: kind.into(),
        passed,
        warning_count,
        findings,
        properties: json!({
            "media_playlists": media.iter().map(|item| json!({
                "path": item.path,
                "target_duration": item.target_duration,
                "total_duration": item.total_duration,
                "segments": item.segment_durations.len(),
                "map_uri": item.map_uri,
                "playlist_type": item.playlist_type,
                "version": item.version,
                "media_sequence": item.media_sequence,
                "discontinuity_sequence": item.discontinuity_sequence,
                "discontinuities": item.segment_discontinuities.iter().filter(|value| **value).count(),
                "fmp4": item.is_fmp4,
                "part_target": item.part_target,
                "parts": item.parts.len(),
                "part_uris": item.parts.iter().map(|part| &part.uri).collect::<Vec<_>>(),
                "skipped_segments": item.skipped_segments,
                "preload_hints": item.preload_hints.len(),
                "rendition_reports": item.rendition_reports.len(),
                "can_block_reload": item.server_control.as_ref().is_some_and(|control| control.can_block_reload),
                "independent_segments": item.has_independent_segments || root_independent_segments
            })).collect::<Vec<_>>()
        }),
    })
}

fn parse_playlist(
    path: &Path,
    profile: HlsProfile,
    findings: &mut Vec<HlsFinding>,
) -> Result<Playlist, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.len() > MAX_PLAYLIST_BYTES {
        return Err(format!(
            "{} exceeds the {}-byte playlist safety limit",
            path.display(),
            MAX_PLAYLIST_BYTES
        ));
    }
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let lines: Vec<_> = text.lines().map(str::trim).collect();
    findings.push(finding(
        "FORGE-HLS-EXTM3U",
        Severity::Error,
        lines.first() == Some(&"#EXTM3U"),
        "EXTM3U is the first playlist line",
        lines.first().map(|line| json!(line)),
    ));
    let multivariant = lines
        .iter()
        .any(|line| line.starts_with("#EXT-X-STREAM-INF:") || line.starts_with("#EXT-X-MEDIA:"));
    let media = lines.iter().any(|line| {
        line.starts_with("#EXTINF:")
            || line.starts_with("#EXT-X-TARGETDURATION:")
            || *line == "#EXT-X-ENDLIST"
    });
    findings.push(finding(
        "FORGE-HLS-PLAYLIST-KIND",
        Severity::Error,
        multivariant ^ media,
        "playlist contains exactly one class of Multivariant or Media tags",
        Some(json!({"multivariant_tags": multivariant, "media_tags": media})),
    ));
    let mut playlist = Playlist {
        path: path.to_path_buf(),
        kind: if multivariant {
            "multivariant"
        } else {
            "media"
        },
        ..Playlist::default()
    };
    let mut singleton = HashSet::new();
    let mut pending_stream = false;
    let mut pending_duration = None;
    let mut pending_discontinuity = false;
    let mut map_count = 0_usize;
    let mut discontinuity_state = 0_u64;
    for (index, line) in lines.iter().enumerate().skip(1) {
        if line.is_empty() {
            continue;
        }
        if pending_stream && !line.starts_with('#') {
            playlist.referenced_playlists.push((*line).into());
            pending_stream = false;
            continue;
        }
        if let Some(duration) = pending_duration {
            if !line.starts_with('#') {
                playlist.segment_durations.push(duration);
                playlist.segment_uris.push((*line).into());
                playlist.segment_discontinuities.push(pending_discontinuity);
                playlist.total_duration += duration;
                pending_duration = None;
                pending_discontinuity = false;
                continue;
            }
        }
        if let Some(value) = line.strip_prefix("#EXT-X-VERSION:") {
            singleton_tag(&mut singleton, "EXT-X-VERSION", findings);
            playlist.version = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            singleton_tag(&mut singleton, "EXT-X-MEDIA-SEQUENCE", findings);
            playlist.media_sequence =
                parse_sequence_tag("EXT-X-MEDIA-SEQUENCE", value, index, findings);
        } else if let Some(value) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            singleton_tag(&mut singleton, "EXT-X-DISCONTINUITY-SEQUENCE", findings);
            let before_segments = playlist.segment_uris.is_empty()
                && playlist.parts.is_empty()
                && !pending_discontinuity;
            findings.push(finding(
                "FORGE-HLS-DISCONTINUITY-SEQUENCE-ORDER",
                Severity::Error,
                before_segments,
                "EXT-X-DISCONTINUITY-SEQUENCE precedes every Media Segment",
                Some(json!({"line": index + 1})),
            ));
            playlist.discontinuity_sequence =
                parse_sequence_tag("EXT-X-DISCONTINUITY-SEQUENCE", value, index, findings);
            discontinuity_state = playlist.discontinuity_sequence.unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            singleton_tag(&mut singleton, "EXT-X-TARGETDURATION", findings);
            playlist.target_duration = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("#EXT-X-PART-INF:") {
            singleton_tag(&mut singleton, "EXT-X-PART-INF", findings);
            match attributes(value) {
                Ok(values) => {
                    playlist.part_target = values
                        .get("PART-TARGET")
                        .and_then(|value| positive_float(value));
                    findings.push(finding(
                        "FORGE-HLS-PART-INF",
                        Severity::Error,
                        playlist.part_target.is_some(),
                        "EXT-X-PART-INF declares a positive PART-TARGET",
                        Some(json!(&values)),
                    ));
                }
                Err(error) => attribute_error(index, error, findings),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-SERVER-CONTROL:") {
            singleton_tag(&mut singleton, "EXT-X-SERVER-CONTROL", findings);
            match attributes(value) {
                Ok(values) => {
                    let control = ServerControl {
                        can_skip_until: values
                            .get("CAN-SKIP-UNTIL")
                            .and_then(|value| positive_float(value)),
                        can_skip_dateranges: values
                            .get("CAN-SKIP-DATERANGES")
                            .is_some_and(|value| value == "YES"),
                        hold_back: values
                            .get("HOLD-BACK")
                            .and_then(|value| positive_float(value)),
                        part_hold_back: values
                            .get("PART-HOLD-BACK")
                            .and_then(|value| positive_float(value)),
                        can_block_reload: values
                            .get("CAN-BLOCK-RELOAD")
                            .is_some_and(|value| value == "YES"),
                    };
                    let enums_valid = ["CAN-SKIP-DATERANGES", "CAN-BLOCK-RELOAD"]
                        .iter()
                        .all(|name| values.get(*name).is_none_or(|value| value == "YES"));
                    let numbers_valid = ["CAN-SKIP-UNTIL", "HOLD-BACK", "PART-HOLD-BACK"]
                        .iter()
                        .all(|name| {
                            values
                                .get(*name)
                                .is_none_or(|value| positive_float(value).is_some())
                        });
                    findings.push(finding(
                        "FORGE-HLS-SERVER-CONTROL",
                        Severity::Error,
                        enums_valid && numbers_valid,
                        "EXT-X-SERVER-CONTROL attributes have valid types and values",
                        Some(json!(&values)),
                    ));
                    playlist.server_control = Some(control);
                }
                Err(error) => attribute_error(index, error, findings),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            singleton_tag(&mut singleton, "EXT-X-PLAYLIST-TYPE", findings);
            playlist.playlist_type = Some(value.into());
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let number = value.split(',').next().unwrap_or_default();
            pending_duration = number.parse::<f64>().ok();
            if pending_duration.is_none_or(|duration| !duration.is_finite() || duration <= 0.0) {
                findings.push(finding(
                    "FORGE-HLS-EXTINF",
                    Severity::Error,
                    false,
                    format!("invalid EXTINF duration at line {}", index + 1),
                    Some(json!(number)),
                ));
                pending_duration = None;
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-PART:") {
            let placement_valid = pending_duration.is_none();
            match attributes(value) {
                Ok(values) => {
                    let uri = values.get("URI").filter(|value| !value.is_empty()).cloned();
                    let duration = values
                        .get("DURATION")
                        .and_then(|value| positive_float(value));
                    let enums_valid = ["INDEPENDENT", "GAP"]
                        .iter()
                        .all(|name| values.get(*name).is_none_or(|value| value == "YES"));
                    let byterange = values.get("BYTERANGE").and_then(|range| {
                        parse_byterange(range).filter(|_| attribute_is_quoted(value, "BYTERANGE"))
                    });
                    let byterange_valid = !values.contains_key("BYTERANGE") || byterange.is_some();
                    let uri_quoted = attribute_is_quoted(value, "URI");
                    let implicit_range_valid = match (&uri, byterange) {
                        (Some(uri), Some((_, None))) => playlist.parts.last().is_some_and(|part| {
                            part.parent == playlist.segment_durations.len()
                                && part.uri == *uri
                                && part.byterange.is_some()
                        }),
                        _ => true,
                    };
                    let valid = placement_valid
                        && uri.is_some()
                        && uri_quoted
                        && duration.is_some()
                        && enums_valid
                        && byterange_valid
                        && implicit_range_valid;
                    findings.push(finding(
                        "FORGE-HLS-PART",
                        Severity::Error,
                        valid,
                        "EXT-X-PART has a URI, positive duration, valid attributes, and precedes its EXTINF",
                        Some(json!({"line": index + 1, "attributes": &values})),
                    ));
                    if valid {
                        let (Some(uri), Some(duration)) = (uri, duration) else {
                            unreachable!("valid PART has URI and duration");
                        };
                        playlist.parts.push(PartialSegment {
                            uri,
                            duration,
                            independent: values
                                .get("INDEPENDENT")
                                .is_some_and(|value| value == "YES"),
                            gap: values.get("GAP").is_some_and(|value| value == "YES"),
                            parent: playlist.segment_durations.len(),
                            discontinuity_sequence: discontinuity_state,
                            byterange,
                        });
                    }
                }
                Err(error) => attribute_error(index, error, findings),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-SKIP:") {
            singleton_tag(&mut singleton, "EXT-X-SKIP", findings);
            match attributes(value) {
                Ok(values) => {
                    playlist.skipped_segments = values
                        .get("SKIPPED-SEGMENTS")
                        .and_then(|value| value.parse().ok());
                    playlist.has_recently_removed_dateranges =
                        values.contains_key("RECENTLY-REMOVED-DATERANGES");
                    let removed_valid = !playlist.has_recently_removed_dateranges
                        || attribute_is_quoted(value, "RECENTLY-REMOVED-DATERANGES");
                    findings.push(finding(
                        "FORGE-HLS-DELTA-UPDATE",
                        Severity::Error,
                        playlist.skipped_segments.is_some() && removed_valid,
                        "EXT-X-SKIP declares a non-negative SKIPPED-SEGMENTS count",
                        Some(json!(&values)),
                    ));
                }
                Err(error) => attribute_error(index, error, findings),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-PRELOAD-HINT:") {
            match attributes(value) {
                Ok(values) => {
                    let kind = values
                        .get("TYPE")
                        .filter(|value| !value.is_empty())
                        .cloned();
                    let uri = values.get("URI").filter(|value| !value.is_empty()).cloned();
                    let start_valid = values
                        .get("BYTERANGE-START")
                        .is_none_or(|value| value.parse::<u64>().is_ok());
                    let length_valid = values
                        .get("BYTERANGE-LENGTH")
                        .is_none_or(|value| value.parse::<u64>().is_ok_and(|length| length > 0));
                    let valid = kind.is_some()
                        && uri.is_some()
                        && attribute_is_quoted(value, "URI")
                        && start_valid
                        && length_valid;
                    findings.push(finding(
                        "FORGE-HLS-PRELOAD-HINT",
                        Severity::Error,
                        valid,
                        "EXT-X-PRELOAD-HINT declares a PART or MAP URI and a valid byte range",
                        Some(json!({"line": index + 1, "attributes": &values})),
                    ));
                    findings.push(finding(
                        "FORGE-HLS-PRELOAD-HINT-TYPE",
                        Severity::Warning,
                        kind.as_deref()
                            .is_some_and(|kind| matches!(kind, "PART" | "MAP")),
                        "preload hint TYPE is recognized as PART or MAP",
                        kind.clone().map(Value::from),
                    ));
                    if valid {
                        let (Some(kind), Some(uri)) = (kind, uri) else {
                            unreachable!("valid preload hint has TYPE and URI");
                        };
                        playlist.preload_hints.push(PreloadHint { kind, uri });
                    }
                }
                Err(error) => attribute_error(index, error, findings),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-RENDITION-REPORT:") {
            match attributes(value) {
                Ok(values) => {
                    let uri = values
                        .get("URI")
                        .filter(|value| relative_uri(value))
                        .cloned();
                    let last_msn = values.get("LAST-MSN").and_then(|value| value.parse().ok());
                    let last_part = values.get("LAST-PART").and_then(|value| value.parse().ok());
                    let integers_valid = values
                        .get("LAST-MSN")
                        .is_none_or(|value| value.parse::<u64>().is_ok())
                        && values
                            .get("LAST-PART")
                            .is_none_or(|value| value.parse::<u64>().is_ok());
                    findings.push(finding(
                        "FORGE-HLS-RENDITION-REPORT",
                        Severity::Error,
                        uri.is_some() && attribute_is_quoted(value, "URI") && integers_valid,
                        "EXT-X-RENDITION-REPORT has a relative URI and valid sequence fields",
                        Some(json!({"line": index + 1, "attributes": &values})),
                    ));
                    if integers_valid && attribute_is_quoted(value, "URI") {
                        let Some(uri) = uri else {
                            continue;
                        };
                        playlist.rendition_reports.push(RenditionReport {
                            uri,
                            last_msn,
                            last_part,
                        });
                    }
                }
                Err(error) => attribute_error(index, error, findings),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            let valid = valid_iso8601_datetime(value);
            findings.push(finding(
                "FORGE-HLS-PROGRAM-DATE-TIME",
                Severity::Error,
                valid,
                "EXT-X-PROGRAM-DATE-TIME contains an ISO 8601 date-time",
                Some(json!({"line": index + 1, "value": value})),
            ));
            findings.push(finding(
                "FORGE-HLS-PART-TAG-ORDER",
                Severity::Error,
                !current_parent_has_parts(&playlist),
                "PROGRAM-DATE-TIME precedes the first Partial Segment of its Parent Segment",
                Some(json!({"line": index + 1})),
            ));
            findings.push(finding(
                "FORGE-HLS-PROGRAM-DATE-TIME-PRECISION",
                Severity::Warning,
                has_datetime_precision(value),
                "program date-time includes a time zone and millisecond precision",
                Some(json!({"line": index + 1, "value": value})),
            ));
            if valid {
                playlist.program_date_time_count += 1;
            }
        } else if *line == "#EXT-X-I-FRAMES-ONLY" {
            playlist.has_i_frames_only = true;
        } else if *line == "#EXT-X-INDEPENDENT-SEGMENTS" {
            singleton_tag(&mut singleton, "EXT-X-INDEPENDENT-SEGMENTS", findings);
            playlist.has_independent_segments = true;
        } else if line.starts_with("#EXT-X-KEY:") {
            findings.push(finding(
                "FORGE-HLS-PART-TAG-ORDER",
                Severity::Error,
                !current_parent_has_parts(&playlist),
                "EXT-X-KEY precedes the first Partial Segment of its Parent Segment",
                Some(json!({"line": index + 1})),
            ));
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            findings.push(finding(
                "FORGE-HLS-PART-TAG-ORDER",
                Severity::Error,
                !current_parent_has_parts(&playlist),
                "EXT-X-MAP precedes the first Partial Segment of its Parent Segment",
                Some(json!({"line": index + 1})),
            ));
            map_count += 1;
            match attributes(value) {
                Ok(values) => playlist.map_uri = values.get("URI").cloned(),
                Err(error) => findings.push(finding(
                    "FORGE-HLS-ATTRIBUTES",
                    Severity::Error,
                    false,
                    format!("line {}: {error}", index + 1),
                    None,
                )),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_stream = true;
            match attributes(value) {
                Ok(values) => {
                    findings.push(finding(
                        "FORGE-HLS-STREAM-INF",
                        Severity::Error,
                        values
                            .get("BANDWIDTH")
                            .and_then(|value| value.parse::<u64>().ok())
                            .is_some_and(|value| value > 0),
                        "EXT-X-STREAM-INF declares a positive BANDWIDTH",
                        values.get("BANDWIDTH").cloned().map(Value::from),
                    ));
                    let has_codecs = values.contains_key("CODECS");
                    findings.push(finding(
                        "FORGE-APPLE-HLS-CODECS",
                        Severity::Error,
                        profile != HlsProfile::AppleHls || has_codecs,
                        "Apple Multivariant streams declare CODECS",
                        None,
                    ));
                }
                Err(error) => findings.push(finding(
                    "FORGE-HLS-ATTRIBUTES",
                    Severity::Error,
                    false,
                    format!("line {}: {error}", index + 1),
                    None,
                )),
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
            match attributes(value) {
                Ok(values) => {
                    let media_type = values.get("TYPE").map(String::as_str);
                    let required = media_type.is_some_and(|value| {
                        matches!(value, "AUDIO" | "VIDEO" | "SUBTITLES" | "CLOSED-CAPTIONS")
                    }) && values.contains_key("GROUP-ID")
                        && values.contains_key("NAME");
                    let uri_valid = if media_type == Some("CLOSED-CAPTIONS") {
                        !values.contains_key("URI")
                    } else if media_type == Some("SUBTITLES") {
                        values.contains_key("URI")
                    } else {
                        true
                    };
                    let default_valid = values.get("DEFAULT").map(String::as_str) != Some("YES")
                        || values.get("AUTOSELECT").map(String::as_str) == Some("YES");
                    findings.push(finding(
                        "FORGE-HLS-MEDIA-RENDITION",
                        Severity::Error,
                        required && uri_valid && default_valid,
                        "EXT-X-MEDIA has required attributes and a valid URI/default relationship",
                        Some(json!(&values)),
                    ));
                    if required && uri_valid && default_valid {
                        let (Some(kind), Some(group_id), Some(name)) = (
                            values.get("TYPE"),
                            values.get("GROUP-ID"),
                            values.get("NAME"),
                        ) else {
                            unreachable!("valid EXT-X-MEDIA has required attributes");
                        };
                        playlist.media_renditions.push(MediaRendition {
                            kind: kind.clone(),
                            group_id: group_id.clone(),
                            name: name.clone(),
                            language: values.get("LANGUAGE").cloned(),
                            uri: values.get("URI").cloned(),
                        });
                    }
                    if let Some(uri) = values.get("URI") {
                        playlist.referenced_playlists.push(uri.clone());
                    }
                }
                Err(error) => findings.push(finding(
                    "FORGE-HLS-ATTRIBUTES",
                    Severity::Error,
                    false,
                    format!("line {}: {error}", index + 1),
                    None,
                )),
            }
        } else if *line == "#EXT-X-DISCONTINUITY" {
            let valid = pending_duration.is_none()
                && !pending_discontinuity
                && !current_parent_has_parts(&playlist);
            findings.push(finding(
                "FORGE-HLS-DISCONTINUITY-PLACEMENT",
                Severity::Error,
                valid,
                "EXT-X-DISCONTINUITY appears once between Media Segments",
                Some(json!({"line": index + 1})),
            ));
            pending_discontinuity = true;
            discontinuity_state = discontinuity_state.saturating_add(1);
        } else if *line == "#EXT-X-ENDLIST" {
            singleton_tag(&mut singleton, "EXT-X-ENDLIST", findings);
            playlist.has_endlist = true;
        } else if !line.starts_with('#') {
            findings.push(finding(
                "FORGE-HLS-ORPHAN-URI",
                Severity::Error,
                false,
                format!("URI at line {} has no preceding URI-bearing tag", index + 1),
                Some(json!(line)),
            ));
        }
    }
    findings.push(finding(
        "FORGE-HLS-DANGLING-TAG",
        Severity::Error,
        !pending_stream && pending_duration.is_none() && !pending_discontinuity,
        "every URI-bearing tag is followed by its URI",
        None,
    ));
    if playlist.kind == "media" {
        let durations_valid = playlist.target_duration.is_some_and(|target| {
            playlist
                .segment_durations
                .iter()
                .all(|duration| duration.round() <= target as f64)
        });
        findings.push(finding(
            "FORGE-HLS-TARGET-DURATION",
            Severity::Error,
            playlist.target_duration.is_some_and(|value| value > 0) && durations_valid,
            "target duration is positive and covers every rounded EXTINF duration",
            Some(json!({
                "target": playlist.target_duration,
                "maximum": playlist.segment_durations.iter().copied().fold(0.0_f64, f64::max)
            })),
        ));
        findings.push(finding(
            "FORGE-HLS-SEGMENTS",
            Severity::Error,
            !playlist.segment_durations.is_empty(),
            "Media Playlist contains complete EXTINF/URI segment pairs",
            Some(json!(playlist.segment_durations.len())),
        ));
        let fmp4 = playlist.map_uri.is_some()
            || playlist
                .segment_uris
                .iter()
                .any(|uri| uri.ends_with(".m4s") || uri.ends_with(".mp4") || uri.ends_with(".m4a"));
        playlist.is_fmp4 = fmp4;
        findings.push(finding(
            "FORGE-HLS-FMP4-MAP",
            Severity::Error,
            !fmp4 || (map_count > 0 && playlist.map_uri.is_some()),
            "fMP4 Media Playlists declare EXT-X-MAP with URI",
            Some(json!({"fmp4": fmp4, "map_tags": map_count})),
        ));
        findings.push(finding(
            "FORGE-HLS-PROTOCOL-VERSION",
            Severity::Error,
            !fmp4 || playlist.version.is_some_and(|version| version >= 6),
            "fMP4 Media Playlists declare protocol version 6 or later",
            playlist.version.map(Value::from),
        ));
        findings.push(finding(
            "FORGE-HLS-PLAYLIST-TYPE",
            Severity::Error,
            playlist
                .playlist_type
                .as_deref()
                .is_none_or(|value| matches!(value, "VOD" | "EVENT"))
                && (playlist.playlist_type.as_deref() != Some("VOD") || playlist.has_endlist),
            "playlist type is valid and declared VOD playlists end with ENDLIST",
            playlist.playlist_type.clone().map(Value::from),
        ));
        if profile == HlsProfile::AppleHls {
            findings.push(finding(
                "FORGE-APPLE-HLS-SIX-SECOND-TARGET",
                Severity::Warning,
                playlist.target_duration == Some(6),
                "Apple recommends a six-second target duration",
                playlist.target_duration.map(Value::from),
            ));
            let within_half_second = playlist.target_duration.is_some_and(|target| {
                playlist
                    .segment_durations
                    .iter()
                    .all(|duration| *duration <= target as f64 + 0.5)
            });
            findings.push(finding(
                "FORGE-APPLE-HLS-SEGMENT-DURATION",
                Severity::Error,
                within_half_second,
                "segments do not exceed target duration by more than 0.5 seconds",
                None,
            ));
        }
        audit_low_latency_tags(&playlist, profile, findings);
    }
    Ok(playlist)
}

fn audit_low_latency_tags(
    playlist: &Playlist,
    profile: HlsProfile,
    findings: &mut Vec<HlsFinding>,
) {
    let uses_low_latency = !playlist.parts.is_empty()
        || playlist.part_target.is_some()
        || playlist.server_control.is_some()
        || playlist.skipped_segments.is_some()
        || !playlist.preload_hints.is_empty()
        || !playlist.rendition_reports.is_empty();
    if !uses_low_latency && profile != HlsProfile::LlHls {
        return;
    }

    findings.push(finding(
        "FORGE-HLS-PART-INF",
        Severity::Error,
        playlist.parts.is_empty() || playlist.part_target.is_some(),
        "a Playlist containing EXT-X-PART declares EXT-X-PART-INF",
        Some(json!({
            "parts": playlist.parts.len(),
            "part_target": playlist.part_target
        })),
    ));

    let durations_valid = playlist.part_target.is_some_and(|target| {
        playlist.parts.iter().enumerate().all(|(index, part)| {
            if part.duration > target {
                return false;
            }
            let next = playlist.parts.get(index + 1);
            let final_for_parent = part.parent < playlist.segment_uris.len()
                && next.is_none_or(|next| next.parent != part.parent);
            let before_gap = next.is_some_and(|next| next.parent == part.parent && next.gap);
            part.duration >= target * 0.85
                || part.independent
                || part.gap
                || before_gap
                || final_for_parent
        })
    });
    findings.push(finding(
        "FORGE-HLS-PART-DURATION",
        Severity::Error,
        playlist.parts.is_empty() || durations_valid,
        "Partial Segment durations satisfy the Part Target bounds and exceptions",
        Some(json!({
            "part_target": playlist.part_target,
            "durations": playlist.parts.iter().map(|part| part.duration).collect::<Vec<_>>()
        })),
    ));

    let control = playlist.server_control.as_ref();
    let target = playlist.target_duration.map(|value| value as f64);
    let skip_valid = control
        .and_then(|value| value.can_skip_until)
        .zip(target)
        .is_none_or(|(skip, target)| skip >= target * 6.0);
    let dateranges_valid =
        control.is_none_or(|value| !value.can_skip_dateranges || value.can_skip_until.is_some());
    let hold_back_valid = control
        .and_then(|value| value.hold_back)
        .zip(target)
        .is_none_or(|(hold_back, target)| hold_back >= target * 3.0);
    let part_hold_back_valid = playlist.part_target.is_none_or(|part_target| {
        control
            .and_then(|value| value.part_hold_back)
            .is_some_and(|hold_back| hold_back >= part_target * 2.0)
    });
    findings.push(finding(
        "FORGE-HLS-SERVER-CONTROL-RELATIONSHIPS",
        Severity::Error,
        skip_valid && dateranges_valid && hold_back_valid && part_hold_back_valid,
        "server-control skip and hold-back values satisfy HLS duration relationships",
        Some(json!({
            "target_duration": playlist.target_duration,
            "part_target": playlist.part_target,
            "can_skip_until": control.and_then(|value| value.can_skip_until),
            "hold_back": control.and_then(|value| value.hold_back),
            "part_hold_back": control.and_then(|value| value.part_hold_back)
        })),
    ));

    let unique_hint_types = playlist
        .preload_hints
        .iter()
        .map(|hint| hint.kind.as_str())
        .collect::<HashSet<_>>()
        .len()
        == playlist.preload_hints.len();
    findings.push(finding(
        "FORGE-HLS-PRELOAD-HINT-SET",
        Severity::Warning,
        unique_hint_types,
        "at most one preload hint of each TYPE is present",
        Some(json!(playlist
            .preload_hints
            .iter()
            .map(|hint| { json!({"type": hint.kind, "uri": hint.uri}) })
            .collect::<Vec<_>>())),
    ));
    findings.push(finding(
        "FORGE-HLS-PRELOAD-ENDLIST",
        Severity::Error,
        !playlist.has_endlist || playlist.preload_hints.is_empty(),
        "an ended Playlist does not contain EXT-X-PRELOAD-HINT",
        None,
    ));

    if playlist.skipped_segments.is_some() {
        let minimum_version = if playlist.has_recently_removed_dateranges {
            10
        } else {
            9
        };
        findings.push(finding(
            "FORGE-HLS-DELTA-VERSION",
            Severity::Error,
            playlist
                .version
                .is_some_and(|version| version >= minimum_version),
            "Playlist Delta Updates declare a compatible protocol version",
            Some(json!({
                "version": playlist.version,
                "minimum": minimum_version
            })),
        ));
    }

    if profile == HlsProfile::LlHls {
        findings.push(finding(
            "FORGE-LL-HLS-PARTS",
            Severity::Error,
            !playlist.parts.is_empty() && playlist.part_target.is_some(),
            "the Low-Latency profile contains Partial Segments and a Part Target",
            Some(json!(playlist.parts.len())),
        ));
        let live_with_parts = !playlist.parts.is_empty() && !playlist.has_endlist;
        let has_part_hint = playlist
            .preload_hints
            .iter()
            .any(|hint| hint.kind == "PART");
        findings.push(finding(
            "FORGE-LL-HLS-PRELOAD",
            Severity::Error,
            !live_with_parts || has_part_hint,
            "an active Partial-Segment Playlist hints the next Partial Segment",
            None,
        ));
        findings.push(finding(
            "FORGE-LL-HLS-PROGRAM-DATE-TIME",
            Severity::Error,
            playlist.program_date_time_count > 0,
            "the Low-Latency profile includes EXT-X-PROGRAM-DATE-TIME",
            Some(json!(playlist.program_date_time_count)),
        ));
        findings.push(finding(
            "FORGE-LL-HLS-BLOCKING-RELOAD",
            Severity::Error,
            control.is_some_and(|value| value.can_block_reload),
            "the Low-Latency profile advertises blocking playlist reload",
            None,
        ));
        findings.push(finding(
            "FORGE-LL-HLS-PART-HOLD-BACK",
            Severity::Warning,
            playlist.part_target.is_some_and(|part_target| {
                control
                    .and_then(|value| value.part_hold_back)
                    .is_some_and(|hold_back| hold_back >= part_target * 3.0)
            }),
            "PART-HOLD-BACK is at least three Part Target Durations",
            None,
        ));
        findings.push(finding(
            "FORGE-LL-HLS-I-FRAMES",
            Severity::Warning,
            !playlist.has_i_frames_only || playlist.parts.is_empty(),
            "I-frame-only Playlists do not use Partial Segments",
            None,
        ));
    }
}

fn current_parent_has_parts(playlist: &Playlist) -> bool {
    playlist
        .parts
        .last()
        .is_some_and(|part| part.parent == playlist.segment_uris.len())
}

fn audit_media_files(
    package_root: &Path,
    playlist: &Playlist,
    profile: HlsProfile,
    inherited_independent_segments: bool,
    invalid_references: &mut InvalidReferenceSet,
    findings: &mut Vec<HlsFinding>,
) {
    let mut xhe_tracks = Vec::new();
    if let Some(uri) = &playlist.map_uri {
        if let LocalReference::Path(path) = checked_local_reference(
            package_root,
            &playlist.path,
            "initialization resource",
            uri,
            invalid_references,
            findings,
        ) {
            let exists = path.is_file();
            findings.push(finding(
                "FORGE-HLS-LOCAL-RESOURCE",
                Severity::Error,
                exists,
                format!("initialization resource exists: {}", path.display()),
                None,
            ));
            if exists && playlist.is_fmp4 {
                xhe_tracks = audit_isobmff(&path, true, profile, findings);
            }
        }
    }
    let mut previous_sequences = None;
    let mut last_decode: HashMap<u64, u64> = HashMap::new();
    let mut previous_ts_streams: Option<Vec<TsAudioStream>> = None;
    let mut last_timed_id3_pts: HashMap<u64, u64> = HashMap::new();
    let mut last_emsg_time = None;
    let mut xhe_ipf_results = Vec::new();
    for (segment_index, uri) in playlist.segment_uris.iter().enumerate() {
        let discontinuity = playlist
            .segment_discontinuities
            .get(segment_index)
            .copied()
            .unwrap_or(false);
        if discontinuity {
            previous_sequences = None;
            last_decode.clear();
            previous_ts_streams = None;
            last_timed_id3_pts.clear();
            last_emsg_time = None;
        }
        let path = match checked_local_reference(
            package_root,
            &playlist.path,
            "media segment",
            uri,
            invalid_references,
            findings,
        ) {
            LocalReference::Path(path) => path,
            LocalReference::External => {
                findings.push(finding(
                    "FORGE-HLS-REMOTE-REFERENCE",
                    Severity::Warning,
                    false,
                    format!("remote segment was not fetched: {uri}"),
                    Some(json!(uri)),
                ));
                continue;
            }
            LocalReference::Invalid(_) => continue,
        };
        let exists = path.is_file();
        findings.push(finding(
            "FORGE-HLS-LOCAL-RESOURCE",
            Severity::Error,
            exists,
            format!("segment resource exists: {}", path.display()),
            None,
        ));
        if !exists {
            continue;
        }
        if !playlist.is_fmp4 {
            audit_transport_segment(
                playlist,
                segment_index,
                &path,
                &mut previous_ts_streams,
                &mut last_timed_id3_pts,
                findings,
            );
            continue;
        }
        match container_qc::audit(&path) {
            Ok(audit) => {
                findings.push(finding(
                    "FORGE-HLS-SEGMENT-CONTAINER",
                    Severity::Error,
                    audit.passed && audit.format == "isobmff",
                    format!("segment container audited: {}", path.display()),
                    Some(json!({"passed": audit.passed, "format": audit.format})),
                ));
                findings.push(finding(
                    "FORGE-HLS-MOVIE-FRAGMENT-RELATIVE",
                    Severity::Error,
                    audit.properties["fragment_movie_relative"] == true,
                    "fMP4 segments use movie-fragment-relative addressing",
                    audit.properties.get("fragment_movie_relative").cloned(),
                ));
                let sequences: Vec<u64> = audit.properties["fragment_sequences"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
                    .collect();
                if let (Some(previous), Some(first)) =
                    (previous_sequences, sequences.first().copied())
                {
                    findings.push(finding(
                        "FORGE-HLS-FRAGMENT-SEQUENCE",
                        Severity::Error,
                        first == previous + 1,
                        "fragment sequence continues across segment boundaries",
                        Some(json!({"previous": previous, "current": first})),
                    ));
                }
                previous_sequences = sequences.last().copied().or(previous_sequences);
                if let Some(times) = audit.properties["fragment_decode_times"].as_array() {
                    for item in times {
                        let Some(track) = item["track_id"].as_u64() else {
                            continue;
                        };
                        let Some(time) = item["time"].as_u64() else {
                            continue;
                        };
                        let monotonic = last_decode
                            .insert(track, time)
                            .is_none_or(|old| time >= old);
                        findings.push(finding(
                            "FORGE-HLS-FRAGMENT-TIMELINE",
                            Severity::Error,
                            monotonic,
                            "fragment decode time is monotonic across segments",
                            Some(json!({"track_id": track, "time": time})),
                        ));
                    }
                }
                audit_cmaf_timed_id3(
                    segment_index,
                    &audit.properties,
                    &mut last_emsg_time,
                    findings,
                );
                for track in &xhe_tracks {
                    let first_sample = audit.properties["fragment_first_samples"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|sample| {
                            sample["fragment_index"].as_u64() == Some(0)
                                && sample["track_id"].as_u64() == Some(track.track_id)
                        });
                    let result = inspect_xhe_segment_ipf(&path, track, first_sample);
                    let passed = result.as_ref().is_ok_and(|inspection| inspection.valid_ipf);
                    xhe_ipf_results.push(passed);
                    findings.push(finding(
                        "FORGE-APPLE-HLS-XHE-AAC-IPF",
                        Severity::Warning,
                        passed,
                        "Apple recommends that each xHE-AAC segment begin with an Immediate Playout Frame",
                        Some(match result {
                            Ok(inspection) => json!({
                                "segment": segment_index,
                                "track_id": track.track_id,
                                "inspection": inspection
                            }),
                            Err(error) => json!({
                                "segment": segment_index,
                                "track_id": track.track_id,
                                "error": error
                            }),
                        }),
                    ));
                }
            }
            Err(error) => findings.push(finding(
                "FORGE-HLS-SEGMENT-READ",
                Severity::Error,
                false,
                error,
                Some(json!(path)),
            )),
        }
    }
    if !xhe_tracks.is_empty() {
        let all_segments_are_ipf = !xhe_ipf_results.is_empty()
            && xhe_ipf_results.len() == playlist.segment_uris.len() * xhe_tracks.len()
            && xhe_ipf_results.iter().all(|passed| *passed);
        let independent_segments =
            inherited_independent_segments || playlist.has_independent_segments;
        findings.push(finding(
            "FORGE-HLS-XHE-AAC-INDEPENDENT-SEGMENTS-TRUTH",
            Severity::Error,
            !independent_segments || all_segments_are_ipf,
            "EXT-X-INDEPENDENT-SEGMENTS is used only when every local xHE-AAC segment begins with an IPF",
            Some(json!({
                "declared": independent_segments,
                "segments_checked": xhe_ipf_results.len(),
                "all_segments_are_ipf": all_segments_are_ipf
            })),
        ));
        findings.push(finding(
            "FORGE-APPLE-HLS-XHE-AAC-INDEPENDENT-SEGMENTS",
            Severity::Warning,
            !all_segments_are_ipf || independent_segments,
            "Apple recommends EXT-X-INDEPENDENT-SEGMENTS when every xHE-AAC segment starts with an IPF",
            Some(json!({
                "declared": independent_segments,
                "all_segments_are_ipf": all_segments_are_ipf
            })),
        ));
    }
}

fn inspect_xhe_segment_ipf(
    path: &Path,
    track: &XheTrackInit,
    first_sample: Option<&Value>,
) -> Result<crate::aac_qc::UsacIpfInspection, String> {
    let preroll = track
        .audio_preroll
        .as_ref()
        .ok_or("xHE-AAC configuration has no AudioPreRoll extension element")?;
    let sample =
        first_sample.ok_or("segment has no resolved first sample for the xHE-AAC track")?;
    if sample["is_sync_sample"].as_bool() != Some(true) {
        return Err("segment first xHE-AAC sample is marked non-sync".into());
    }
    let offset = sample["offset"]
        .as_u64()
        .ok_or("segment first-sample offset is missing")?;
    let size = sample["size"]
        .as_u64()
        .ok_or("segment first-sample size is missing")?;
    if size == 0 || size > MAX_IPF_SAMPLE_BYTES {
        return Err(format!(
            "segment first-sample size {size} is outside 1..={MAX_IPF_SAMPLE_BYTES}"
        ));
    }
    let end = offset
        .checked_add(size)
        .ok_or("segment first-sample range overflow")?;
    let file_size = fs::metadata(path)
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    if end > file_size {
        return Err("segment first-sample range exceeds the file".into());
    }
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut bytes = vec![0_u8; usize::try_from(size).unwrap()];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read {} xHE-AAC sample: {error}", path.display()))?;
    crate::aac_qc::inspect_usac_ipf(&bytes, preroll)
}

fn audit_transport_segment(
    playlist: &Playlist,
    segment_index: usize,
    path: &Path,
    previous_streams: &mut Option<Vec<TsAudioStream>>,
    last_timed_id3_pts: &mut HashMap<u64, u64>,
    findings: &mut Vec<HlsFinding>,
) {
    let expected_transport = path.extension().is_some_and(|extension| {
        matches!(
            extension.to_string_lossy().to_ascii_lowercase().as_str(),
            "ts" | "m2ts" | "mts"
        )
    });
    let audit = match container_qc::audit_if_supported(path) {
        Ok(Some(audit)) => audit,
        Ok(None) if !expected_transport => return,
        Ok(None) => {
            findings.push(finding(
                "FORGE-HLS-SEGMENT-CONTAINER",
                Severity::Error,
                false,
                format!(
                    "MPEG-TS segment container is not recognized: {}",
                    path.display()
                ),
                None,
            ));
            return;
        }
        Err(error) => {
            findings.push(finding(
                "FORGE-HLS-SEGMENT-READ",
                Severity::Error,
                false,
                error,
                Some(json!(path)),
            ));
            return;
        }
    };
    let transport = matches!(audit.format.as_str(), "mpegts" | "m2ts");
    if !transport && !expected_transport {
        return;
    }
    findings.push(finding(
        "FORGE-HLS-SEGMENT-CONTAINER",
        Severity::Error,
        audit.passed && transport,
        format!("MPEG-TS segment container audited: {}", path.display()),
        Some(json!({"passed": audit.passed, "format": audit.format})),
    ));
    if !transport {
        return;
    }
    audit_ts_timed_id3(
        segment_index,
        &audit.properties,
        last_timed_id3_pts,
        findings,
    );
    let streams = transport_streams(&audit.properties);
    findings.push(finding(
        "FORGE-HLS-TS-AUDIO-STREAMS",
        Severity::Error,
        !streams.is_empty(),
        "MPEG-TS segment exposes recognized audio stream timing",
        Some(json!({"segment": segment_index, "streams": streams.len()})),
    ));
    if let Some(previous) = previous_streams.as_ref() {
        let previous_configuration = previous
            .iter()
            .map(TsAudioStream::configuration)
            .collect::<Vec<_>>();
        let configuration = streams
            .iter()
            .map(TsAudioStream::configuration)
            .collect::<Vec<_>>();
        findings.push(finding(
            "FORGE-HLS-TS-PROGRAM-CONTINUITY",
            Severity::Error,
            configuration == previous_configuration,
            "MPEG-TS programme, PID, codec, and language configuration is stable across segments",
            Some(json!({
                "segment": segment_index,
                "previous": previous_configuration,
                "current": configuration
            })),
        ));
        let maximum_gap = playlist
            .target_duration
            .map_or(7.0, |duration| duration as f64 + 1.0);
        for old in previous {
            let Some(current) = streams.iter().find(|stream| stream.pid == old.pid) else {
                continue;
            };
            let (Some(last), Some(first)) = (old.last_pts, current.first_pts) else {
                continue;
            };
            let delta = pts_forward_delta(last, first);
            let delta_seconds = delta as f64 / 90_000.0;
            findings.push(finding(
                "FORGE-HLS-TS-PTS-CONTINUITY",
                Severity::Error,
                delta > 0 && delta_seconds <= maximum_gap,
                "audio PTS advances across the MPEG-TS segment boundary",
                Some(json!({
                    "segment": segment_index,
                    "pid": old.pid,
                    "previous_last_pts_90khz": last,
                    "current_first_pts_90khz": first,
                    "forward_delta_90khz": delta,
                    "maximum_gap_seconds": maximum_gap
                })),
            ));
        }
    }
    *previous_streams = Some(streams);
}

fn audit_ts_timed_id3(
    segment_index: usize,
    properties: &Value,
    last_pts: &mut HashMap<u64, u64>,
    findings: &mut Vec<HlsFinding>,
) {
    let streams = properties["timed_id3_streams"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let stream_count = streams.len();
    if stream_count == 0 {
        return;
    }
    let descriptor_errors = streams
        .iter()
        .filter(|stream| {
            !stream["descriptor_error"].is_null() || !stream["id3_descriptor"].is_object()
        })
        .count();
    let errors = properties["timed_id3_error_count"]
        .as_u64()
        .unwrap_or_else(|| {
            properties["timed_id3_errors"]
                .as_array()
                .map_or(0, |items| items.len() as u64)
        });
    let tags = properties["timed_id3"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    findings.push(finding(
        "FORGE-HLS-TIMED-ID3",
        Severity::Error,
        errors == 0 && descriptor_errors == 0,
        "MPEG-TS timed metadata streams have ID3 PMT descriptors and carry structurally valid ID3v2 tags",
        Some(json!({
            "segment": segment_index,
            "streams": stream_count,
            "tags": tags.len(),
            "descriptor_errors": descriptor_errors,
            "payload_errors": errors
        })),
    ));
    let mut monotonic = true;
    let mut timed = 0_usize;
    let mut relative_volume = 0_usize;
    for item in &tags {
        relative_volume += item["tag"]["relative_volume_adjustments"]
            .as_array()
            .map_or(0, Vec::len);
        let (Some(pid), Some(pts)) = (item["pid"].as_u64(), item["pts_90khz"].as_u64()) else {
            continue;
        };
        timed += 1;
        if let Some(previous) = last_pts.insert(pid, pts) {
            let delta = pts_forward_delta(previous, pts);
            monotonic &= delta < MPEG_PTS_MODULUS / 2;
        }
    }
    findings.push(finding(
        "FORGE-HLS-TIMED-ID3-PTS",
        Severity::Error,
        tags.is_empty() || (timed == tags.len() && monotonic),
        "timed-ID3 PES packets have present, monotonic 90 kHz presentation timestamps",
        Some(json!({
            "segment": segment_index,
            "tags": tags.len(),
            "timestamped": timed
        })),
    ));
    if relative_volume > 0 {
        findings.push(finding(
            "FORGE-HLS-TIMED-ID3-LOUDNESS",
            Severity::Error,
            true,
            "timed ID3v2.4 RVA2 relative-volume metadata is structurally valid",
            Some(json!({
                "segment": segment_index,
                "relative_volume_adjustments": relative_volume
            })),
        ));
    }
}

fn audit_cmaf_timed_id3(
    segment_index: usize,
    properties: &Value,
    last_time: &mut Option<(u64, u64)>,
    findings: &mut Vec<HlsFinding>,
) {
    let events = properties["timed_id3"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if events.is_empty() {
        return;
    }
    let mut monotonic = true;
    let mut relative_volume = 0_usize;
    for event in &events {
        relative_volume += event["tag"]["relative_volume_adjustments"]
            .as_array()
            .map_or(0, Vec::len);
        let (Some(timescale), Some(presentation)) = (
            event["timescale"].as_u64(),
            event["presentation_time"].as_u64(),
        ) else {
            monotonic = false;
            continue;
        };
        if timescale == 0 {
            monotonic = false;
            continue;
        }
        monotonic &= last_time.is_none_or(|(previous_time, previous_scale)| {
            u128::from(presentation) * u128::from(previous_scale)
                >= u128::from(previous_time) * u128::from(timescale)
        });
        *last_time = Some((presentation, timescale));
    }
    findings.push(finding(
        "FORGE-HLS-CMAF-TIMED-ID3",
        Severity::Error,
        monotonic,
        "CMAF aid3 EventMessageBox metadata uses version 1, complete ID3v2.4, and monotonic presentation times",
        Some(json!({
            "segment": segment_index,
            "events": events.len(),
            "relative_volume_adjustments": relative_volume
        })),
    ));
    findings.push(finding(
        "FORGE-HLS-CMAF-TIMED-ID3-BRAND",
        Severity::Warning,
        properties["timed_id3_aid3_compatible_brand"] == true,
        "CMAF timed-ID3 media lists the recommended aid3 compatible brand",
        properties.get("timed_id3_aid3_compatible_brand").cloned(),
    ));
    if relative_volume > 0 {
        findings.push(finding(
            "FORGE-HLS-TIMED-ID3-LOUDNESS",
            Severity::Error,
            true,
            "CMAF timed ID3v2.4 RVA2 relative-volume metadata is structurally valid",
            Some(json!({
                "segment": segment_index,
                "relative_volume_adjustments": relative_volume
            })),
        ));
    }
}

fn transport_streams(properties: &Value) -> Vec<TsAudioStream> {
    properties["audio_streams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|stream| {
            Some(TsAudioStream {
                program: stream["program"].as_u64()?,
                pid: stream["pid"].as_u64()?,
                stream_type: stream["stream_type"].as_u64()?,
                codec: stream["codec"].as_str()?.into(),
                language: stream["language"].as_str().map(str::to_owned),
                first_pts: stream["first_pts_90khz"].as_u64(),
                last_pts: stream["last_pts_90khz"].as_u64(),
            })
        })
        .collect()
}

fn pts_forward_delta(previous: u64, current: u64) -> u64 {
    (current + MPEG_PTS_MODULUS - previous) % MPEG_PTS_MODULUS
}

fn audit_isobmff(
    path: &Path,
    initialization: bool,
    profile: HlsProfile,
    findings: &mut Vec<HlsFinding>,
) -> Vec<XheTrackInit> {
    match container_qc::audit(path) {
        Ok(audit) => {
            let xhe_tracks = xhe_tracks_from_init_properties(&audit.properties);
            findings.push(finding(
                "FORGE-HLS-INITIALIZATION-CONTAINER",
                Severity::Error,
                audit.passed && audit.format == "isobmff",
                format!("initialization container audited: {}", path.display()),
                Some(json!({"passed": audit.passed, "format": audit.format})),
            ));
            if initialization {
                let movie_zero = audit.properties["movie_duration"].as_u64() == Some(0);
                let tracks_zero = audit.properties["track_header_durations"]
                    .as_array()
                    .is_some_and(|values| {
                        !values.is_empty() && values.iter().all(|value| value.as_u64() == Some(0))
                    });
                findings.push(finding(
                    "FORGE-HLS-INITIALIZATION-DURATIONS",
                    Severity::Error,
                    movie_zero && tracks_zero,
                    "fMP4 initialization movie and track header durations are zero",
                    Some(json!({
                        "movie": audit.properties["movie_duration"],
                        "tracks": audit.properties["track_header_durations"]
                    })),
                ));
                findings.push(finding(
                    "FORGE-HLS-MVEX-ORDER",
                    Severity::Error,
                    audit.properties["mvex_after_tracks"] == true,
                    "MovieExtendsBox follows the final TrackBox",
                    audit.properties.get("mvex_after_tracks").cloned(),
                ));
            }
            if initialization && profile == HlsProfile::AppleHls {
                audit_apple_isobmff_loudness(&audit.properties, findings);
            }
            xhe_tracks
        }
        Err(error) => {
            findings.push(finding(
                "FORGE-HLS-INITIALIZATION-READ",
                Severity::Error,
                false,
                error,
                Some(json!(path)),
            ));
            Vec::new()
        }
    }
}

fn audit_apple_isobmff_loudness(properties: &Value, findings: &mut Vec<HlsFinding>) {
    let tracks = properties["tracks"].as_array().into_iter().flatten();
    for track in tracks.filter(|track| track["handler"].as_str() == Some("soun")) {
        let codecs = track["codecs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let apac = codecs.contains(&"apac");
        let loudness_box_count = track["loudness_box_count"].as_u64().unwrap_or(0);
        let observed = Some(json!({
            "track_id": track["track_id"],
            "codecs": codecs,
            "loudness_box_count": loudness_box_count
        }));
        if apac {
            findings.push(finding(
                "FORGE-APPLE-HLS-APAC-LOUDNESS-BOX",
                Severity::Error,
                loudness_box_count == 0,
                "APAC fMP4 does not use ludt; loudness and DRC metadata belong in the audio stream",
                observed,
            ));
        } else {
            findings.push(finding(
                "FORGE-APPLE-HLS-LOUDNESS-BOX",
                Severity::Warning,
                loudness_box_count > 0,
                "Apple recommends ludt loudness metadata for each non-APAC fMP4 audio track",
                observed,
            ));
        }
    }
}

fn xhe_tracks_from_init_properties(properties: &Value) -> Vec<XheTrackInit> {
    properties["tracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|track| {
            let config = track.get("xhe_aac_usac_config")?.as_object()?;
            (config.get("audio_object_type")?.as_u64() == Some(42)).then_some(())?;
            let track_id = track.get("track_id")?.as_u64()?;
            let audio_preroll = config
                .get("audio_preroll")
                .and_then(Value::as_object)
                .and_then(|preroll| {
                    Some(crate::aac_qc::UsacAudioPrerollConfig {
                        element_index: u16::try_from(preroll.get("element_index")?.as_u64()?)
                            .ok()?,
                        default_payload_length_bytes: preroll
                            .get("default_payload_length_bytes")?
                            .as_u64(),
                        payload_fragmentation: preroll.get("payload_fragmentation")?.as_bool()?,
                    })
                });
            Some(XheTrackInit {
                track_id,
                audio_preroll,
            })
        })
        .collect()
}

fn cross_check_renditions(
    package_root: &Path,
    media: &[Playlist],
    profile: HlsProfile,
    invalid_references: &mut InvalidReferenceSet,
    findings: &mut Vec<HlsFinding>,
) {
    if media.len() < 2 {
        return;
    }
    let targets: HashSet<_> = media
        .iter()
        .filter_map(|item| item.target_duration)
        .collect();
    findings.push(finding(
        "FORGE-APPLE-HLS-COMMON-TARGET",
        Severity::Error,
        profile != HlsProfile::AppleHls || targets.len() == 1,
        "Apple audio/video Media Playlists use one target duration",
        Some(json!(targets)),
    ));
    let minimum = media
        .iter()
        .map(|item| item.total_duration)
        .fold(f64::INFINITY, f64::min);
    let maximum = media
        .iter()
        .map(|item| item.total_duration)
        .fold(0.0_f64, f64::max);
    findings.push(finding(
        "FORGE-APPLE-HLS-COMMON-DURATION",
        Severity::Error,
        profile != HlsProfile::AppleHls || maximum - minimum <= 0.05,
        "Apple audio/video Media Playlists cover the same content duration",
        Some(json!({"minimum": minimum, "maximum": maximum})),
    ));
    let aligned = media.windows(2).all(|pair| {
        let left = cumulative(&pair[0].segment_durations);
        let right = cumulative(&pair[1].segment_durations);
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(left, right)| (left - right).abs() <= 0.05)
    });
    findings.push(finding(
        "FORGE-APPLE-HLS-BOUNDARY-ALIGNMENT",
        Severity::Warning,
        profile != HlsProfile::AppleHls || aligned,
        "Apple recommends aligned segment boundaries across renditions",
        None,
    ));
    if profile == HlsProfile::LlHls {
        let maximum_part_target = media
            .iter()
            .filter_map(|playlist| playlist.part_target)
            .fold(0.0_f64, f64::max);
        findings.push(finding(
            "FORGE-LL-HLS-COMMON-PART-HOLD-BACK",
            Severity::Warning,
            maximum_part_target > 0.0
                && media.iter().all(|playlist| {
                    playlist
                        .server_control
                        .as_ref()
                        .and_then(|control| control.part_hold_back)
                        .is_some_and(|hold_back| hold_back >= maximum_part_target * 3.0)
                }),
            "every Rendition PART-HOLD-BACK covers three times the maximum Part Target",
            Some(json!({"maximum_part_target": maximum_part_target})),
        ));
        cross_check_low_latency_renditions(package_root, media, invalid_references, findings);
    }
}

fn cross_check_cmaf_audio_renditions(
    package_root: &Path,
    root_path: &Path,
    renditions: &[MediaRendition],
    media: &[Playlist],
    invalid_references: &mut InvalidReferenceSet,
    findings: &mut Vec<HlsFinding>,
) {
    let mut declared_groups = HashMap::<&str, Vec<&MediaRendition>>::new();
    for rendition in renditions
        .iter()
        .filter(|rendition| rendition.kind == "AUDIO")
    {
        declared_groups
            .entry(rendition.group_id.as_str())
            .or_default()
            .push(rendition);
    }

    for (group_id, declared) in declared_groups
        .into_iter()
        .filter(|(_, group)| group.len() > 1)
    {
        let mut group = Vec::new();
        for rendition in &declared {
            let Some(uri) = rendition.uri.as_deref() else {
                continue;
            };
            let LocalReference::Path(path) = checked_local_reference(
                package_root,
                root_path,
                "playlist",
                uri,
                invalid_references,
                findings,
            ) else {
                continue;
            };
            if let Some(playlist) = media
                .iter()
                .find(|playlist| paths_equal(&playlist.path, &path))
            {
                group.push((*rendition, playlist));
            }
        }
        if !group.iter().any(|(_, item)| item.is_fmp4) {
            continue;
        }
        let observed = || {
            json!({
                "group_id": group_id,
                "renditions": group.iter().map(|(rendition, playlist)| json!({
                    "name": rendition.name,
                    "language": rendition.language,
                    "path": playlist.path,
                    "fmp4": playlist.is_fmp4,
                    "duration": playlist.total_duration,
                    "segments": playlist.segment_durations.len(),
                    "discontinuity_sequence": playlist.discontinuity_sequence
                })).collect::<Vec<_>>()
            })
        };
        let expected_local = declared
            .iter()
            .filter(|rendition| rendition.uri.is_some())
            .count();
        findings.push(finding(
            "FORGE-HLS-CMAF-AUDIO-GROUP-EVIDENCE",
            Severity::Warning,
            group.len() == expected_local,
            format!(
                "CMAF audio Rendition Group {group_id} has every URI-bearing language rendition available for local alignment checks"
            ),
            Some(json!({
                "group_id": group_id,
                "expected": expected_local,
                "loaded": group.len()
            })),
        ));
        if group.len() < 2 {
            continue;
        }
        findings.push(finding(
            "FORGE-HLS-CMAF-AUDIO-GROUP-FORMAT",
            Severity::Error,
            group.iter().all(|(_, playlist)| playlist.is_fmp4),
            format!(
                "CMAF audio Rendition Group {group_id} uses fMP4 in every local language rendition"
            ),
            Some(observed()),
        ));

        let reference = &group[0].1;
        let durations_match = group.iter().skip(1).all(|(_, playlist)| {
            (playlist.total_duration - reference.total_duration).abs() <= 0.001
        });
        findings.push(finding(
            "FORGE-HLS-CMAF-AUDIO-GROUP-DURATION",
            Severity::Error,
            durations_match,
            format!(
                "CMAF audio Rendition Group {group_id} covers the same duration in every local language rendition"
            ),
            Some(observed()),
        ));

        let reference_boundaries = cumulative(&reference.segment_durations);
        let boundaries_match = group.iter().skip(1).all(|(_, playlist)| {
            let boundaries = cumulative(&playlist.segment_durations);
            boundaries.len() == reference_boundaries.len()
                && boundaries
                    .iter()
                    .zip(&reference_boundaries)
                    .all(|(observed, expected)| (observed - expected).abs() <= 0.001)
        });
        findings.push(finding(
            "FORGE-HLS-CMAF-AUDIO-GROUP-ALIGNMENT",
            Severity::Error,
            boundaries_match,
            format!(
                "CMAF audio Rendition Group {group_id} has aligned segment boundaries across local languages"
            ),
            Some(observed()),
        ));

        let discontinuities_match = group.iter().skip(1).all(|(_, playlist)| {
            playlist.discontinuity_sequence == reference.discontinuity_sequence
                && playlist.segment_discontinuities == reference.segment_discontinuities
        });
        findings.push(finding(
            "FORGE-HLS-CMAF-AUDIO-GROUP-DISCONTINUITY",
            Severity::Error,
            discontinuities_match,
            format!(
                "CMAF audio Rendition Group {group_id} has identical discontinuity state across local languages"
            ),
            Some(observed()),
        ));
    }
}

fn cross_check_low_latency_renditions(
    package_root: &Path,
    media: &[Playlist],
    invalid_references: &mut InvalidReferenceSet,
    findings: &mut Vec<HlsFinding>,
) {
    for source in media {
        let source_edge = playlist_edge(source);
        let mut reported = HashSet::new();
        let mut values_match = true;
        for report in &source.rendition_reports {
            let path = match checked_local_reference(
                package_root,
                &source.path,
                "rendition report",
                &report.uri,
                invalid_references,
                findings,
            ) {
                LocalReference::Path(path) => path,
                LocalReference::External | LocalReference::Invalid(_) => {
                    values_match = false;
                    continue;
                }
            };
            let Some(target) = media
                .iter()
                .find(|playlist| paths_equal(&playlist.path, &path))
            else {
                values_match = false;
                continue;
            };
            if target.path == source.path {
                values_match = false;
                continue;
            }
            reported.insert(target.path.clone());
            let target_edge = playlist_edge(target);
            let effective_msn = report.last_msn.or(source_edge.map(|edge| edge.0));
            let effective_part = report.last_part.or(source_edge.and_then(|edge| edge.1));
            values_match &= effective_msn == target_edge.map(|edge| edge.0)
                && effective_part == target_edge.and_then(|edge| edge.1);
        }
        let expected = media
            .iter()
            .filter(|target| target.path != source.path && !target.has_i_frames_only)
            .map(|target| target.path.clone())
            .collect::<HashSet<_>>();
        findings.push(finding(
            "FORGE-LL-HLS-RENDITION-REPORT-SET",
            Severity::Error,
            reported == expected,
            "each Low-Latency Media Playlist reports every other non-I-frame Rendition",
            Some(json!({
                "playlist": source.path,
                "expected": expected,
                "reported": reported
            })),
        ));
        findings.push(finding(
            "FORGE-LL-HLS-RENDITION-REPORT-EDGE",
            Severity::Error,
            values_match,
            "Rendition Reports identify each referenced Playlist live edge",
            Some(json!({"playlist": source.path})),
        ));
    }

    let discontinuities_match = media.windows(2).all(|pair| {
        let left = discontinuity_timeline(&pair[0]);
        let right = discontinuity_timeline(&pair[1])
            .into_iter()
            .collect::<HashMap<_, _>>();
        let common = left
            .iter()
            .filter(|(msn, _)| right.contains_key(msn))
            .count();
        common > 0
            && left
                .iter()
                .all(|(msn, state)| right.get(msn).is_none_or(|other| other == state))
    });
    findings.push(finding(
        "FORGE-LL-HLS-DISCONTINUITY-STATE",
        Severity::Error,
        discontinuities_match,
        "Discontinuity Sequence state is aligned across Low-Latency Renditions",
        Some(json!(media
            .iter()
            .map(|playlist| json!({
                "path": playlist.path,
                "states": discontinuity_timeline(playlist)
            }))
            .collect::<Vec<_>>())),
    ));
}

fn playlist_edge(playlist: &Playlist) -> Option<(u64, Option<u64>)> {
    let first = playlist
        .media_sequence
        .unwrap_or(0)
        .saturating_add(playlist.skipped_segments.unwrap_or(0));
    if let Some(last_part) = playlist.parts.last() {
        let part_msn = first.saturating_add(last_part.parent as u64);
        let segment_msn = (!playlist.segment_uris.is_empty())
            .then(|| first.saturating_add(playlist.segment_uris.len() as u64 - 1));
        if segment_msn.is_none_or(|segment_msn| part_msn >= segment_msn) {
            let part_index = playlist
                .parts
                .iter()
                .rev()
                .take_while(|part| part.parent == last_part.parent)
                .count() as u64
                - 1;
            return Some((part_msn, Some(part_index)));
        }
    }
    (!playlist.segment_uris.is_empty()).then(|| {
        (
            first.saturating_add(playlist.segment_uris.len() as u64 - 1),
            None,
        )
    })
}

fn discontinuity_timeline(playlist: &Playlist) -> Vec<(u64, u64)> {
    let mut state = playlist.discontinuity_sequence.unwrap_or(0);
    let first = playlist
        .media_sequence
        .unwrap_or(0)
        .saturating_add(playlist.skipped_segments.unwrap_or(0));
    let mut timeline = playlist
        .segment_discontinuities
        .iter()
        .enumerate()
        .map(|(index, discontinuity)| {
            if *discontinuity {
                state = state.saturating_add(1);
            }
            (first.saturating_add(index as u64), state)
        })
        .collect::<Vec<_>>();
    for part in &playlist.parts {
        let msn = first.saturating_add(part.parent as u64);
        if !timeline.iter().any(|(existing, _)| *existing == msn) {
            timeline.push((msn, part.discontinuity_sequence));
        }
    }
    timeline
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn cumulative(values: &[f64]) -> Vec<f64> {
    let mut sum = 0.0;
    values
        .iter()
        .map(|value| {
            sum += value;
            sum
        })
        .collect()
}

fn singleton_tag(
    seen: &mut HashSet<&'static str>,
    name: &'static str,
    findings: &mut Vec<HlsFinding>,
) {
    findings.push(finding(
        "FORGE-HLS-SINGLETON-TAG",
        Severity::Error,
        seen.insert(name),
        format!("{name} appears at most once"),
        Some(json!(name)),
    ));
}

fn parse_sequence_tag(
    name: &'static str,
    value: &str,
    line_index: usize,
    findings: &mut Vec<HlsFinding>,
) -> Option<u64> {
    let parsed = value.parse::<u64>().ok();
    findings.push(finding(
        "FORGE-HLS-SEQUENCE-NUMBER",
        Severity::Error,
        parsed.is_some(),
        format!("{name} is a non-negative decimal integer"),
        Some(json!({"line": line_index + 1, "value": value})),
    ));
    parsed
}

fn attribute_error(line_index: usize, error: String, findings: &mut Vec<HlsFinding>) {
    findings.push(finding(
        "FORGE-HLS-ATTRIBUTES",
        Severity::Error,
        false,
        format!("line {}: {error}", line_index + 1),
        None,
    ));
}

fn positive_float(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn parse_byterange(value: &str) -> Option<(u64, Option<u64>)> {
    let (length, offset) = value
        .split_once('@')
        .map_or((value, None), |(length, offset)| (length, Some(offset)));
    let length = length.parse::<u64>().ok().filter(|length| *length > 0)?;
    let offset = offset.map(str::parse).transpose().ok()?;
    Some((length, offset))
}

fn valid_iso8601_datetime(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let Some(separator) = value.find(['T', 't']) else {
        return false;
    };
    let (date, time_with_separator) = value.split_at(separator);
    let time = &time_with_separator[1..];
    let mut date_fields = date.split('-');
    let (Some(year), Some(month), Some(day), None) = (
        date_fields.next(),
        date_fields.next(),
        date_fields.next(),
        date_fields.next(),
    ) else {
        return false;
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<u32>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > maximum_day {
        return false;
    }

    let (clock, zone_valid) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, true)
    } else if time.len() >= 6 && matches!(time.as_bytes().get(time.len() - 6), Some(b'+' | b'-')) {
        let zone_start = time.len() - 6;
        let (clock, zone) = time.split_at(zone_start);
        let bytes = zone.as_bytes();
        let valid = matches!(bytes.first(), Some(b'+' | b'-'))
            && bytes.get(3) == Some(&b':')
            && zone[1..3].parse::<u32>().is_ok_and(|hours| hours <= 23)
            && zone[4..6].parse::<u32>().is_ok_and(|minutes| minutes <= 59);
        (clock, valid)
    } else {
        (time, true)
    };
    if !zone_valid {
        return false;
    }
    let mut clock_fields = clock.split(':');
    let (Some(hour), Some(minute), Some(second), None) = (
        clock_fields.next(),
        clock_fields.next(),
        clock_fields.next(),
        clock_fields.next(),
    ) else {
        return false;
    };
    if hour.len() != 2 || minute.len() != 2 {
        return false;
    }
    let (seconds, fraction) = second.find(['.', ',']).map_or((second, None), |separator| {
        (&second[..separator], Some(&second[separator + 1..]))
    });
    seconds.len() == 2
        && hour.parse::<u32>().is_ok_and(|hour| hour <= 23)
        && minute.parse::<u32>().is_ok_and(|minute| minute <= 59)
        && seconds.parse::<u32>().is_ok_and(|seconds| seconds <= 60)
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn has_datetime_precision(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let Some(separator) = value.find(['T', 't']) else {
        return false;
    };
    let time = &value[separator + 1..];
    let clock = if let Some(clock) = time.strip_suffix('Z') {
        clock
    } else if time.len() >= 6 && matches!(time.as_bytes().get(time.len() - 6), Some(b'+' | b'-')) {
        &time[..time.len() - 6]
    } else {
        return false;
    };
    clock.find(['.', ',']).is_some_and(|separator| {
        let fraction = &clock[separator + 1..];
        fraction.len() >= 3 && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn relative_uri(uri: &str) -> bool {
    !uri.is_empty()
        && !uri.starts_with("//")
        && !uri.contains("://")
        && !uri.to_ascii_lowercase().starts_with("data:")
}

fn attribute_is_quoted(list: &str, expected_name: &str) -> bool {
    let bytes = list.as_bytes();
    let mut start = 0_usize;
    let mut quoted = false;
    for index in 0..=bytes.len() {
        if index < bytes.len() && bytes[index] == b'"' {
            quoted = !quoted;
        }
        if index == bytes.len() || (bytes[index] == b',' && !quoted) {
            let item = &list[start..index];
            if let Some((name, raw)) = item.split_once('=') {
                if name == expected_name {
                    return raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"');
                }
            }
            start = index + 1;
        }
    }
    false
}

fn attributes(value: &str) -> Result<HashMap<String, String>, String> {
    let mut values = HashMap::new();
    let mut start = 0_usize;
    let mut quoted = false;
    let bytes = value.as_bytes();
    for index in 0..=bytes.len() {
        if index < bytes.len() && bytes[index] == b'"' {
            quoted = !quoted;
        }
        if index == bytes.len() || (bytes[index] == b',' && !quoted) {
            let item = &value[start..index];
            let (name, raw) = item
                .split_once('=')
                .ok_or_else(|| format!("invalid attribute: {item}"))?;
            if name.is_empty() || values.contains_key(name) {
                return Err(format!("empty or duplicate attribute: {name}"));
            }
            let parsed = if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
                &raw[1..raw.len() - 1]
            } else {
                raw
            };
            values.insert(name.into(), parsed.into());
            start = index + 1;
        }
    }
    if quoted {
        return Err("unterminated quoted attribute".into());
    }
    Ok(values)
}

fn package_root(playlist: &Path) -> Result<PathBuf, String> {
    let parent = playlist
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(parent).map_err(|error| {
        format!(
            "canonicalize HLS package root {}: {error}",
            parent.display()
        )
    })
}

fn audit_reference_safety(
    package_root: &Path,
    playlist: &Playlist,
    invalid_references: &mut InvalidReferenceSet,
    findings: &mut Vec<HlsFinding>,
) {
    let mut validate = |kind: &'static str, uri: &str| {
        checked_local_reference(
            package_root,
            &playlist.path,
            kind,
            uri,
            invalid_references,
            findings,
        );
    };

    for uri in &playlist.referenced_playlists {
        validate("playlist", uri);
    }
    if let Some(uri) = &playlist.map_uri {
        validate("initialization resource", uri);
    }
    for uri in &playlist.segment_uris {
        validate("media segment", uri);
    }
    for part in &playlist.parts {
        validate("partial segment", &part.uri);
    }
    for hint in &playlist.preload_hints {
        validate("preload hint", &hint.uri);
    }
    for report in &playlist.rendition_reports {
        validate("rendition report", &report.uri);
    }
}

fn checked_local_reference(
    package_root: &Path,
    playlist: &Path,
    reference_kind: &'static str,
    uri: &str,
    invalid_references: &mut InvalidReferenceSet,
    findings: &mut Vec<HlsFinding>,
) -> LocalReference {
    // Deliberately resolve again at each use site instead of caching a successful path. A path
    // that became an outside symlink is therefore rejected before use; the set only suppresses
    // duplicate diagnostics for references that were already reported invalid.
    let reference = local_reference(package_root, playlist, uri);
    if let LocalReference::Invalid(reason) = &reference {
        let reason = *reason;
        if invalid_references.insert((playlist.to_path_buf(), uri.to_owned())) {
            findings.push(finding(
                "FORGE-HLS-LOCAL-REFERENCE-CONTAINMENT",
                Severity::Error,
                false,
                format!("unsafe {reference_kind} URI was not resolved as a local file: {reason}"),
                Some(json!({
                    "playlist": playlist,
                    "reference_kind": reference_kind,
                    "uri": uri,
                    "reason": reason
                })),
            ));
        }
    }
    reference
}

fn local_reference(package_root: &Path, playlist: &Path, uri: &str) -> LocalReference {
    if uri.is_empty() {
        return LocalReference::Invalid("URI is empty");
    }
    // The query and fragment are URI metadata, not filesystem path bytes. Only the path is
    // decoded and mapped to the package after this split.
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    if uri
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return LocalReference::Invalid("URI contains whitespace or a control character");
    }
    if path.as_bytes().contains(&b'\\') {
        return LocalReference::Invalid(
            "URI contains a raw backslash, which is a path separator on Windows",
        );
    }
    if !valid_percent_encoding(uri) {
        return LocalReference::Invalid("URI contains an invalid percent escape");
    }
    if uri.starts_with("//") {
        if uri.starts_with("///") {
            return LocalReference::Invalid("URI has an absolute path without a network authority");
        }
        return match url::Url::parse(&format!("https:{uri}")) {
            Ok(url) if url.host_str().is_some() => LocalReference::External,
            _ => LocalReference::Invalid("URI has an invalid network-path reference"),
        };
    }

    if windows_drive_prefix(path) {
        return LocalReference::Invalid("URI has a Windows drive prefix");
    }
    if let Some(scheme) = uri_scheme(path) {
        if scheme.eq_ignore_ascii_case("file") {
            return LocalReference::Invalid("file URI is not an allowed HLS package reference");
        }
        return match url::Url::parse(uri) {
            Ok(url) if !matches!(url.scheme(), "http" | "https") || url.host_str().is_some() => {
                LocalReference::External
            }
            _ => LocalReference::Invalid("URI has an invalid absolute reference"),
        };
    }
    if path.is_empty() {
        return LocalReference::Invalid("URI has no local path");
    }
    if path.starts_with('/') {
        return LocalReference::Invalid("URI has an absolute local path");
    }

    let parent = playlist
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = match fs::canonicalize(parent) {
        Ok(parent) if parent.starts_with(package_root) => parent,
        Ok(_) => {
            return LocalReference::Invalid("playlist parent resolves outside the HLS package root")
        }
        Err(_) => return LocalReference::Invalid("playlist parent could not be canonicalized"),
    };
    let mut candidate = parent
        .strip_prefix(package_root)
        .ok()
        .filter(|relative| {
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        })
        .map_or(canonical_parent, |relative| package_root.join(relative));

    // RFC 3986 dot segments are applied to the playlist's logical parent. The package root is
    // the bottom of this stack, so `../shared/...` is valid from a nested playlist while an
    // additional `..` that would pop the root is rejected.
    for encoded_segment in path.split('/') {
        let segment = match decode_uri_path_segment(encoded_segment) {
            Ok(segment) => segment,
            Err(reason) => return LocalReference::Invalid(reason),
        };
        match segment.as_str() {
            "" | "." => {}
            ".." => {
                if candidate == package_root {
                    return LocalReference::Invalid("URI dot segments escape the HLS package root");
                }
                if !candidate.pop() || !candidate.starts_with(package_root) {
                    return LocalReference::Invalid("URI dot segments escape the HLS package root");
                }
            }
            _ => {
                if windows_drive_prefix(&segment) {
                    return LocalReference::Invalid("URI path segment has a Windows drive prefix");
                }
                candidate.push(segment);
            }
        }
    }
    contained_local_path(package_root, &candidate)
}

fn decode_uri_path_segment(segment: &str) -> Result<String, &'static str> {
    let encoded = segment.as_bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'%' {
            let Some(hex) = encoded.get(index + 1..index + 3) else {
                return Err("URI path contains an invalid percent escape");
            };
            let (Some(high), Some(low)) = (hex_value(hex[0]), hex_value(hex[1])) else {
                return Err("URI path contains an invalid percent escape");
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(encoded[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded)
        .map_err(|_| "URI path is not valid UTF-8 after percent-decoding")?;
    if decoded.chars().any(char::is_control) {
        return Err("URI path contains a decoded NUL or control character");
    }
    if decoded.contains('/') {
        return Err("URI path segment contains a decoded slash");
    }
    if decoded.contains('\\') {
        return Err("URI path segment contains a decoded backslash");
    }
    Ok(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn contained_local_path(package_root: &Path, candidate: &Path) -> LocalReference {
    match fs::symlink_metadata(candidate) {
        Ok(_) => {
            let canonical = match fs::canonicalize(candidate) {
                Ok(canonical) => canonical,
                Err(_) => {
                    return LocalReference::Invalid(
                        "existing local reference could not be canonicalized",
                    )
                }
            };
            if !canonical.starts_with(package_root) {
                return LocalReference::Invalid(
                    "existing local reference resolves outside the HLS package root",
                );
            }
            // Auditing assumes a quiescent package. The canonical target proves containment,
            // while the logical candidate preserves RFC 3986 resolution for a symlink alias.
            // Every later use resolves and checks again, but a writer can still replace the path
            // between this check and open. Fully eliminating that TOCTOU window requires
            // handle-relative I/O on every platform.
            return LocalReference::Path(candidate.to_path_buf());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return LocalReference::Invalid("local reference could not be inspected safely"),
    }

    // A missing final resource is allowed so the normal LOCAL-RESOURCE finding can report it.
    // Canonicalizing its nearest existing ancestor also rejects an existing directory symlink
    // that would otherwise redirect the candidate outside the package.
    let mut ancestor = candidate.parent();
    while let Some(path) = ancestor {
        match fs::canonicalize(path) {
            Ok(canonical) => {
                return if canonical.starts_with(package_root) {
                    LocalReference::Path(candidate.to_path_buf())
                } else {
                    LocalReference::Invalid(
                        "local reference has an ancestor outside the HLS package root",
                    )
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = path.parent();
            }
            Err(_) => {
                return LocalReference::Invalid(
                    "local reference ancestor could not be canonicalized",
                )
            }
        }
    }
    LocalReference::Invalid("local reference has no resolvable package ancestor")
}

fn windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn uri_scheme(uri: &str) -> Option<&str> {
    let colon = uri.find(':')?;
    let scheme = &uri[..colon];
    let mut characters = scheme.chars();
    characters.next()?.is_ascii_alphabetic().then_some(())?;
    characters
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
        .then_some(scheme)
}

fn valid_percent_encoding(uri: &str) -> bool {
    let bytes = uri.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if bytes
                .get(index + 1..index + 3)
                .is_none_or(|hex| !hex.iter().all(u8::is_ascii_hexdigit))
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn finding(
    rule_id: &'static str,
    severity: Severity,
    passed: bool,
    message: impl Into<String>,
    observed: Option<Value>,
) -> HlsFinding {
    HlsFinding {
        rule_id,
        severity,
        passed,
        message: message.into(),
        observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipf_sample() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut position = 0_usize;
        let mut write = |value: u64, width: usize| {
            for shift in (0..width).rev() {
                if position.is_multiple_of(8) {
                    bytes.push(0);
                }
                let index = position / 8;
                bytes[index] |= (((value >> shift) & 1) as u8) << (7 - position % 8);
                position += 1;
            }
        };
        write(1, 1); // usacIndependencyFlag
        write(1, 1); // AudioPreRoll present
        write(0, 1); // explicit payload length
        write(5, 8);
        write(1, 4); // one-byte replacement config
        write(0, 8);
        write(0, 1); // no crossfade
        write(0, 1); // reserved
        write(1, 2); // one pre-roll AU
        write(1, 16);
        write(0x80, 8);
        bytes
    }

    fn boxed(kind: &[u8; 4], body: Vec<u8>) -> Vec<u8> {
        [
            u32::try_from(body.len() + 8)
                .unwrap()
                .to_be_bytes()
                .as_slice(),
            kind,
            &body,
        ]
        .concat()
    }

    fn full_box(version: u8, payload: Vec<u8>) -> Vec<u8> {
        [vec![version, 0, 0, 0], payload].concat()
    }

    #[test]
    fn applies_apple_loudness_rules_per_audio_track_and_exempts_apac() {
        let properties = json!({
            "tracks": [
                {"track_id": 1, "handler": "soun", "codecs": ["apac"], "loudness_box_count": 0},
                {"track_id": 2, "handler": "soun", "codecs": ["mp4a"], "loudness_box_count": 1},
                {"track_id": 3, "handler": "soun", "codecs": ["alac"], "loudness_box_count": 0},
                {"track_id": 4, "handler": "vide", "codecs": ["apac"], "loudness_box_count": 1}
            ]
        });
        let mut findings = Vec::new();
        audit_apple_isobmff_loudness(&properties, &mut findings);

        let apac = findings
            .iter()
            .filter(|item| item.rule_id == "FORGE-APPLE-HLS-APAC-LOUDNESS-BOX")
            .collect::<Vec<_>>();
        assert_eq!(apac.len(), 1);
        assert!(apac[0].passed);
        assert_eq!(apac[0].severity, Severity::Error);
        assert_eq!(apac[0].observed.as_ref().unwrap()["track_id"], 1);

        let ordinary = findings
            .iter()
            .filter(|item| item.rule_id == "FORGE-APPLE-HLS-LOUDNESS-BOX")
            .collect::<Vec<_>>();
        assert_eq!(ordinary.len(), 2);
        assert!(ordinary
            .iter()
            .any(|item| item.passed && item.observed.as_ref().unwrap()["track_id"] == 2));
        assert!(ordinary
            .iter()
            .any(|item| !item.passed && item.observed.as_ref().unwrap()["track_id"] == 3));
    }

    #[test]
    fn rejects_ludt_on_apple_apac_audio() {
        let properties = json!({
            "tracks": [{
                "track_id": 9,
                "handler": "soun",
                "codecs": ["apac"],
                "loudness_box_count": 1
            }]
        });
        let mut findings = Vec::new();
        audit_apple_isobmff_loudness(&properties, &mut findings);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, "FORGE-APPLE-HLS-APAC-LOUDNESS-BOX");
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(!findings[0].passed);
    }

    fn media_segment(sequence: u32, decode_time: u64) -> Vec<u8> {
        let styp = boxed(
            b"styp",
            [b"msdh".as_slice(), &[0, 0, 0, 0], b"msdh"].concat(),
        );
        let mfhd = boxed(b"mfhd", full_box(0, sequence.to_be_bytes().to_vec()));
        let tfhd = boxed(
            b"tfhd",
            [vec![0, 2, 0, 0], 1_u32.to_be_bytes().to_vec()].concat(),
        );
        let tfdt = boxed(b"tfdt", full_box(1, decode_time.to_be_bytes().to_vec()));
        let trun = boxed(b"trun", full_box(0, 1_u32.to_be_bytes().to_vec()));
        let moof = boxed(
            b"moof",
            [mfhd, boxed(b"traf", [tfhd, tfdt, trun].concat())].concat(),
        );
        [styp, moof, boxed(b"mdat", vec![1, 2, 3, 4])].concat()
    }

    #[test]
    fn reports_ts_and_cmaf_timed_id3_loudness_timing() {
        let ts = json!({
            "timed_id3_streams": [{
                "program": 1,
                "pid": 257,
                "stream_type": 21,
                "id3_descriptor": {"metadata_service_id": 0},
                "descriptor_error": null
            }],
            "timed_id3_errors": [],
            "timed_id3": [{
                "pid": 257,
                "pts_90khz": 90000,
                "tag": {"relative_volume_adjustments": [{"identification": "track"}]}
            }]
        });
        let mut findings = Vec::new();
        let mut pts = HashMap::new();
        audit_ts_timed_id3(0, &ts, &mut pts, &mut findings);
        assert!(findings
            .iter()
            .any(|finding| finding.rule_id == "FORGE-HLS-TIMED-ID3" && finding.passed));
        assert!(findings.iter().any(|finding| {
            finding.rule_id == "FORGE-HLS-TIMED-ID3-LOUDNESS" && finding.passed
        }));

        let cmaf = json!({
            "timed_id3_aid3_compatible_brand": true,
            "timed_id3": [{
                "timescale": 1000,
                "presentation_time": 1500,
                "tag": {"relative_volume_adjustments": [{"identification": "track"}]}
            }]
        });
        let mut last = None;
        audit_cmaf_timed_id3(0, &cmaf, &mut last, &mut findings);
        assert!(findings
            .iter()
            .any(|finding| { finding.rule_id == "FORGE-HLS-CMAF-TIMED-ID3" && finding.passed }));
        assert_eq!(last, Some((1500, 1000)));
    }

    #[test]
    fn audits_rfc_media_playlist_and_keeps_apple_recommendations_as_warnings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.m3u8");
        fs::write(
            &path,
            "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:7\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXTINF:6.2,\n\
             https://example.invalid/one.ts\n\
             #EXTINF:6.2,\n\
             https://example.invalid/two.ts\n\
             #EXT-X-ENDLIST\n",
        )
        .unwrap();

        let rfc = audit(&path, HlsProfile::Rfc8216).unwrap();
        assert!(rfc.passed, "{rfc:#?}");
        let apple = audit(&path, HlsProfile::AppleHls).unwrap();
        assert!(apple.passed, "{apple:#?}");
        assert!(apple.warning_count > 0);
    }

    #[test]
    fn decodes_local_uri_path_once_and_preserves_query_fragments() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let playlist = directory.path().join("master.m3u8");
        fs::create_dir(directory.path().join("media")).unwrap();
        fs::write(directory.path().join("media/segment one.ts"), b"segment").unwrap();

        assert_eq!(
            local_reference(
                &root,
                &playlist,
                "media/segment%20one.ts?token=abc#fragment"
            ),
            LocalReference::Path(
                fs::canonicalize(directory.path().join("media/segment one.ts")).unwrap()
            )
        );
        assert_eq!(
            local_reference(&root, &playlist, "media/%252e%252e.ts"),
            LocalReference::Path(directory.path().join("media/%2e%2e.ts"))
        );
        fs::write(directory.path().join("existing.ts"), b"segment").unwrap();
        assert_eq!(
            local_reference(&root, &playlist, "existing.ts?token=abc#fragment"),
            LocalReference::Path(fs::canonicalize(directory.path().join("existing.ts")).unwrap())
        );

        for uri in [
            "https://example.invalid/segment.ts",
            "http://example.invalid/segment.ts",
            "data:application/octet-stream,segment",
            "//cdn.example.invalid/segment.ts",
            "ftp://example.invalid/segment.ts",
        ] {
            assert_eq!(
                local_reference(&root, &playlist, uri),
                LocalReference::External,
                "{uri}"
            );
        }
    }

    #[test]
    fn normalizes_dot_segments_against_the_logical_playlist_parent() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("variants")).unwrap();
        fs::create_dir_all(directory.path().join("shared")).unwrap();
        fs::write(directory.path().join("shared/segment.ts"), b"segment").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let playlist = directory.path().join("variants/audio.m3u8");

        assert_eq!(
            local_reference(&root, &playlist, "../shared/segment.ts"),
            LocalReference::Path(
                fs::canonicalize(directory.path().join("shared/segment.ts")).unwrap()
            )
        );
        assert_eq!(
            local_reference(&root, &playlist, "nested/%2e%2e/../shared/segment.ts"),
            LocalReference::Path(
                fs::canonicalize(directory.path().join("shared/segment.ts")).unwrap()
            )
        );
        assert!(matches!(
            local_reference(&root, &playlist, "%2e%2e/%2e%2e/outside.ts"),
            LocalReference::Invalid(_)
        ));
    }

    #[test]
    fn rejects_unsafe_or_invalid_local_uri_paths_on_every_platform() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let playlist = directory.path().join("master.m3u8");

        for uri in [
            "",
            "?token=abc",
            "#fragment",
            "/root/segment.ts",
            "///root/segment.ts",
            "../segment.ts",
            "nested/../../segment.ts",
            "C:/segment.ts",
            "C:segment.ts",
            r"nested\segment.ts",
            r"\\server\share\segment.ts",
            r"\\?\C:\segment.ts",
            r"\\.\C:\segment.ts",
            "segment name.ts",
            "segment%2.ts",
            "segment%GG.ts",
            "nested/segment%2Fchild.ts",
            "nested/segment%5Cchild.ts",
            "nested/segment%00.ts",
            "nested/segment%1F.ts",
            "nested/segment%FF.ts",
            "nested/segment%C3%28.ts",
            "%43%3A/segment.ts",
            "%2e%2e/outside.ts",
            "file:///root/segment.ts",
        ] {
            assert!(
                matches!(
                    local_reference(&root, &playlist, uri),
                    LocalReference::Invalid(_)
                ),
                "unsafe URI was accepted: {uri:?}"
            );
        }
    }

    #[test]
    fn reports_invalid_local_references_as_stable_errors_not_remote_warnings() {
        let directory = tempfile::tempdir().unwrap();
        for (index, uri) in [
            "../outside.ts",
            "/root/outside.ts",
            "C:/outside.ts",
            r"nested\outside.ts",
            "invalid%GG.ts",
            "?token=abc",
        ]
        .into_iter()
        .enumerate()
        {
            let path = directory.path().join(format!("unsafe-{index}.m3u8"));
            fs::write(
                &path,
                format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n{uri}\n#EXT-X-ENDLIST\n"),
            )
            .unwrap();
            let result = audit(&path, HlsProfile::Rfc8216).unwrap();
            assert!(!result.passed, "{uri}: {result:#?}");
            assert!(result.findings.iter().any(|finding| {
                finding.rule_id == "FORGE-HLS-LOCAL-REFERENCE-CONTAINMENT"
                    && finding.severity == Severity::Error
                    && !finding.passed
                    && finding.observed.as_ref().is_some_and(|observed| {
                        observed["reference_kind"] == "media segment"
                            && observed["uri"].as_str() == Some(uri)
                    })
            }));
            assert!(!result.findings.iter().any(|finding| {
                finding.rule_id == "FORGE-HLS-REMOTE-REFERENCE"
                    && finding
                        .observed
                        .as_ref()
                        .is_some_and(|observed| observed.as_str() == Some(uri))
            }));
        }

        let empty_map = directory.path().join("empty-map.m3u8");
        fs::write(
            &empty_map,
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"\"\n\
             #EXTINF:1,\nhttps://example.invalid/segment.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        let result = audit(&empty_map, HlsProfile::Rfc8216).unwrap();
        assert!(result.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-HLS-LOCAL-REFERENCE-CONTAINMENT"
                && finding.severity == Severity::Error
                && !finding.passed
                && finding.observed.as_ref().is_some_and(|observed| {
                    observed["reference_kind"] == "initialization resource" && observed["uri"] == ""
                })
        }));
    }

    #[test]
    fn reports_missing_decoded_local_paths_as_local_resources() {
        let directory = tempfile::tempdir().unwrap();
        let playlist = directory.path().join("missing.m3u8");
        fs::write(
            &playlist,
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nmissing%20segment.ts\n\
             #EXT-X-ENDLIST\n",
        )
        .unwrap();

        let result = audit(&playlist, HlsProfile::Rfc8216).unwrap();
        assert!(result
            .findings
            .iter()
            .any(|finding| { finding.rule_id == "FORGE-HLS-LOCAL-RESOURCE" && !finding.passed }));
        assert!(!result.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-HLS-LOCAL-REFERENCE-CONTAINMENT" && !finding.passed
        }));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_an_internal_playlist_symlink_as_the_logical_uri_base() {
        let package = tempfile::tempdir().unwrap();
        fs::create_dir(package.path().join("alias")).unwrap();
        fs::create_dir(package.path().join("target")).unwrap();
        fs::write(package.path().join("target/media.m3u8"), b"#EXTM3U\n").unwrap();
        fs::write(package.path().join("alias/segment.ts"), b"inside").unwrap();
        std::os::unix::fs::symlink(
            package.path().join("target/media.m3u8"),
            package.path().join("alias/media.m3u8"),
        )
        .unwrap();
        let root = fs::canonicalize(package.path()).unwrap();
        let master = package.path().join("master.m3u8");

        let LocalReference::Path(playlist) = local_reference(&root, &master, "alias/media.m3u8")
        else {
            panic!("internal playlist symlink was not accepted");
        };
        assert_eq!(playlist, package.path().join("alias/media.m3u8"));
        assert_eq!(
            local_reference(&root, &playlist, "segment.ts"),
            LocalReference::Path(package.path().join("alias/segment.ts"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_later_invalid_resolution_fails_closed_without_duplicate_findings() {
        let package = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(package.path().join("inside.ts"), b"inside").unwrap();
        fs::write(outside.path().join("outside.ts"), b"outside").unwrap();
        let link = package.path().join("segment.ts");
        std::os::unix::fs::symlink(package.path().join("inside.ts"), &link).unwrap();
        let root = fs::canonicalize(package.path()).unwrap();
        let playlist = package.path().join("media.m3u8");
        let mut invalid_references = InvalidReferenceSet::new();
        let mut findings = Vec::new();

        assert!(matches!(
            checked_local_reference(
                &root,
                &playlist,
                "media segment",
                "segment.ts",
                &mut invalid_references,
                &mut findings,
            ),
            LocalReference::Path(_)
        ));
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.ts"), &link).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                checked_local_reference(
                    &root,
                    &playlist,
                    "media segment",
                    "segment.ts",
                    &mut invalid_references,
                    &mut findings,
                ),
                LocalReference::Invalid(_)
            ));
        }
        assert_eq!(
            findings
                .iter()
                .filter(|finding| {
                    finding.rule_id == "FORGE-HLS-LOCAL-REFERENCE-CONTAINMENT" && !finding.passed
                })
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_symlinks_and_symlinked_ancestors_outside_package_root() {
        let package = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("segment.ts"), b"outside").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("segment.ts"),
            package.path().join("escape.ts"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), package.path().join("outside-dir")).unwrap();

        let playlist = package.path().join("media.m3u8");
        fs::write(
            &playlist,
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nescape.ts\n\
             #EXTINF:1,\noutside-dir/missing.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        let result = audit(&playlist, HlsProfile::Rfc8216).unwrap();
        let containment_errors = result
            .findings
            .iter()
            .filter(|finding| {
                finding.rule_id == "FORGE-HLS-LOCAL-REFERENCE-CONTAINMENT" && !finding.passed
            })
            .collect::<Vec<_>>();
        assert_eq!(containment_errors.len(), 2, "{result:#?}");
        assert!(containment_errors.iter().all(|finding| {
            finding.severity == Severity::Error
                && finding.observed.as_ref().is_some_and(|observed| {
                    observed["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("outside the HLS package root"))
                })
        }));
        assert!(!result
            .findings
            .iter()
            .any(|finding| { finding.rule_id == "FORGE-HLS-LOCAL-RESOURCE" && finding.passed }));
    }

    #[test]
    fn apple_multivariant_requires_common_target_and_duration() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("master.m3u8"),
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.2\"\n\
             a.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\n\
             b.m3u8\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("a.m3u8"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXTINF:6,\na.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("b.m3u8"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXTINF:7,\nb.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        fs::write(directory.path().join("a.ts"), []).unwrap();
        fs::write(directory.path().join("b.ts"), []).unwrap();

        let result = audit(&directory.path().join("master.m3u8"), HlsProfile::AppleHls).unwrap();
        assert!(!result.passed);
        assert!(result
            .findings
            .iter()
            .any(|item| { item.rule_id == "FORGE-APPLE-HLS-COMMON-TARGET" && !item.passed }));
        assert!(result
            .findings
            .iter()
            .any(|item| { item.rule_id == "FORGE-APPLE-HLS-COMMON-DURATION" && !item.passed }));
    }

    #[test]
    fn validates_cmaf_audio_rendition_alignment_across_languages() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("master.m3u8"),
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"en/audio.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Japanese\",LANGUAGE=\"ja\",AUTOSELECT=YES,URI=\"ja/audio.m3u8\"\n",
        )
        .unwrap();
        for language in ["en", "ja"] {
            fs::create_dir(directory.path().join(language)).unwrap();
            fs::write(
                directory.path().join(language).join("audio.m3u8"),
                "#EXTM3U\n\
                 #EXT-X-VERSION:7\n\
                 #EXT-X-TARGETDURATION:2\n\
                 #EXT-X-PLAYLIST-TYPE:VOD\n\
                 #EXT-X-MAP:URI=\"https://example.invalid/init.mp4\"\n\
                 #EXTINF:2,\n\
                 https://example.invalid/one.m4s\n\
                 #EXTINF:2,\n\
                 https://example.invalid/two.m4s\n\
                 #EXT-X-ENDLIST\n",
            )
            .unwrap();
        }

        let audit = audit(&directory.path().join("master.m3u8"), HlsProfile::Rfc8216).unwrap();
        assert!(
            audit.passed,
            "{:#?}",
            audit
                .findings
                .iter()
                .filter(|finding| finding.severity == Severity::Error && !finding.passed)
                .map(|finding| (&finding.rule_id, &finding.message))
                .collect::<Vec<_>>()
        );
        for rule in [
            "FORGE-HLS-CMAF-AUDIO-GROUP-EVIDENCE",
            "FORGE-HLS-CMAF-AUDIO-GROUP-FORMAT",
            "FORGE-HLS-CMAF-AUDIO-GROUP-DURATION",
            "FORGE-HLS-CMAF-AUDIO-GROUP-ALIGNMENT",
            "FORGE-HLS-CMAF-AUDIO-GROUP-DISCONTINUITY",
        ] {
            assert!(
                audit
                    .findings
                    .iter()
                    .any(|finding| finding.rule_id == rule && finding.passed),
                "missing passed rule {rule}"
            );
        }
    }

    #[test]
    fn rejects_misaligned_cmaf_audio_language_renditions() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("master.m3u8"),
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",LANGUAGE=\"en\",URI=\"en.m3u8\"\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"French\",LANGUAGE=\"fr\",URI=\"fr.m3u8\"\n",
        )
        .unwrap();
        let playlist = |durations: &[f64], discontinuity: bool| {
            format!(
                "#EXTM3U\n\
                 #EXT-X-VERSION:7\n\
                 #EXT-X-TARGETDURATION:3\n\
                 #EXT-X-DISCONTINUITY-SEQUENCE:{}\n\
                 #EXT-X-MAP:URI=\"https://example.invalid/init.mp4\"\n\
                 #EXTINF:{},\n\
                 https://example.invalid/one.m4s\n\
                 {}\
                 #EXTINF:{},\n\
                 https://example.invalid/two.m4s\n\
                 #EXT-X-ENDLIST\n",
                usize::from(discontinuity),
                durations[0],
                if discontinuity {
                    "#EXT-X-DISCONTINUITY\n"
                } else {
                    ""
                },
                durations[1]
            )
        };
        fs::write(
            directory.path().join("en.m3u8"),
            playlist(&[2.0, 2.0], false),
        )
        .unwrap();
        fs::write(
            directory.path().join("fr.m3u8"),
            playlist(&[1.5, 2.5], true),
        )
        .unwrap();

        let audit = audit(&directory.path().join("master.m3u8"), HlsProfile::Rfc8216).unwrap();
        assert!(!audit.passed);
        for rule in [
            "FORGE-HLS-CMAF-AUDIO-GROUP-ALIGNMENT",
            "FORGE-HLS-CMAF-AUDIO-GROUP-DISCONTINUITY",
        ] {
            assert!(
                audit
                    .findings
                    .iter()
                    .any(|finding| finding.rule_id == rule && !finding.passed),
                "missing failed rule {rule}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_attributes_and_dangling_extinf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken.m3u8");
        fs::write(
            &path,
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1,BANDWIDTH=2\n",
        )
        .unwrap();
        let result = audit(&path, HlsProfile::Rfc8216).unwrap();
        assert!(!result.passed);
        assert!(result
            .findings
            .iter()
            .any(|item| item.rule_id == "FORGE-HLS-ATTRIBUTES" && !item.passed));
    }

    #[test]
    fn rejects_unquoted_low_latency_uri_attributes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken-parts.m3u8");
        fs::write(
            &path,
            "#EXTM3U\n\
             #EXT-X-TARGETDURATION:2\n\
             #EXT-X-PART-INF:PART-TARGET=0.5\n\
             #EXT-X-PART:DURATION=0.5,URI=part.m4s\n\
             #EXTINF:0.5,\n\
             https://example.invalid/segment.ts\n",
        )
        .unwrap();
        let result = audit(&path, HlsProfile::Rfc8216).unwrap();
        assert!(!result.passed);
        assert!(result
            .findings
            .iter()
            .any(|item| item.rule_id == "FORGE-HLS-PART" && !item.passed));
    }

    #[test]
    fn validates_low_latency_byte_ranges_dates_and_completed_part_edges() {
        assert!(valid_iso8601_datetime("2024-02-29T23:59:60.123Z"));
        assert!(valid_iso8601_datetime("2026-07-29T12:34:56"));
        assert!(valid_iso8601_datetime("2026-07-29T12:34:56,123+09:00"));
        assert!(!valid_iso8601_datetime("2025-02-29T12:34:56Z"));
        assert!(!valid_iso8601_datetime("2026-07-29T25:00:00Z"));
        assert!(has_datetime_precision("2026-07-29T12:34:56.123Z"));
        assert!(!has_datetime_precision("2026-07-29T12:34:56Z"));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ranges.m3u8");
        let playlist = |first_range: &str| {
            format!(
                "#EXTM3U\n\
                 #EXT-X-TARGETDURATION:2\n\
                 #EXT-X-PART-INF:PART-TARGET=0.5\n\
                 #EXT-X-SERVER-CONTROL:PART-HOLD-BACK=1\n\
                 #EXT-X-PART:DURATION=0.5,URI=\"packed.m4s\",BYTERANGE=\"{first_range}\"\n\
                 #EXT-X-PART:DURATION=0.5,URI=\"packed.m4s\",BYTERANGE=\"10\"\n\
                 #EXTINF:1,\n\
                 https://example.invalid/segment.ts\n"
            )
        };
        fs::write(&path, playlist("10@0")).unwrap();
        let valid = audit(&path, HlsProfile::Rfc8216).unwrap();
        assert!(valid.passed, "{valid:#?}");
        fs::write(&path, playlist("10")).unwrap();
        let invalid = audit(&path, HlsProfile::Rfc8216).unwrap();
        assert!(!invalid.passed);
        assert!(invalid
            .findings
            .iter()
            .any(|item| item.rule_id == "FORGE-HLS-PART" && !item.passed));

        let completed = Playlist {
            media_sequence: Some(40),
            segment_uris: vec!["segment.ts".into()],
            parts: vec![PartialSegment {
                uri: "part.m4s".into(),
                duration: 0.5,
                independent: true,
                gap: false,
                parent: 0,
                discontinuity_sequence: 0,
                byterange: None,
            }],
            ..Playlist::default()
        };
        assert_eq!(playlist_edge(&completed), Some((40, Some(0))));
    }

    #[test]
    fn cross_checks_fragment_sequences_and_decode_times_between_local_segments() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("one.m4s"), media_segment(7, 0)).unwrap();
        fs::write(directory.path().join("two.m4s"), media_segment(8, 1_024)).unwrap();
        let playlist = Playlist {
            path: directory.path().join("audio.m3u8"),
            segment_uris: vec!["one.m4s".into(), "two.m4s".into()],
            is_fmp4: true,
            ..Playlist::default()
        };
        let mut findings = Vec::new();
        let mut invalid_references = InvalidReferenceSet::new();
        let package_root = fs::canonicalize(directory.path()).unwrap();
        audit_media_files(
            &package_root,
            &playlist,
            HlsProfile::Rfc8216,
            false,
            &mut invalid_references,
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .filter(|item| item.severity == Severity::Error)
                .all(|item| item.passed),
            "{findings:#?}"
        );

        fs::write(directory.path().join("two.m4s"), media_segment(10, 512)).unwrap();
        let mut findings = Vec::new();
        let mut invalid_references = InvalidReferenceSet::new();
        audit_media_files(
            &package_root,
            &playlist,
            HlsProfile::Rfc8216,
            false,
            &mut invalid_references,
            &mut findings,
        );
        assert!(findings
            .iter()
            .any(|item| item.rule_id == "FORGE-HLS-FRAGMENT-SEQUENCE" && !item.passed));
    }

    #[test]
    fn verifies_xhe_immediate_playout_frame_from_resolved_sample_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("segment.m4s");
        let mut file = vec![0xaa, 0xbb];
        let sample = ipf_sample();
        file.extend_from_slice(&sample);
        fs::write(&path, file).unwrap();
        let track = XheTrackInit {
            track_id: 7,
            audio_preroll: Some(crate::aac_qc::UsacAudioPrerollConfig {
                element_index: 0,
                default_payload_length_bytes: None,
                payload_fragmentation: false,
            }),
        };
        let location = json!({
            "fragment_index": 0,
            "track_id": 7,
            "offset": 2,
            "size": sample.len(),
            "is_sync_sample": true
        });
        let inspection = inspect_xhe_segment_ipf(&path, &track, Some(&location)).unwrap();
        assert!(inspection.valid_ipf);
    }

    #[test]
    fn records_independent_segments_playlist_tag() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.m3u8");
        fs::write(
            &path,
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nhttps://example.invalid/audio.m4s\n",
        )
        .unwrap();
        let mut findings = Vec::new();
        let playlist = parse_playlist(&path, HlsProfile::AppleHls, &mut findings).unwrap();
        assert!(playlist.has_independent_segments);
    }

    #[test]
    fn accepts_complete_low_latency_manifest_contract() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.m3u8");
        fs::write(
            &path,
            "#EXTM3U\n\
             #EXT-X-VERSION:9\n\
             #EXT-X-TARGETDURATION:2\n\
             #EXT-X-MEDIA-SEQUENCE:10\n\
             #EXT-X-PART-INF:PART-TARGET=0.5\n\
             #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,CAN-SKIP-UNTIL=12,HOLD-BACK=6,PART-HOLD-BACK=1.5\n\
             #EXT-X-PROGRAM-DATE-TIME:2026-07-29T00:00:00Z\n\
             #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/10.0.m4s\"\n\
             #EXT-X-PART:DURATION=0.5,URI=\"https://example.invalid/10.1.m4s\"\n\
             #EXTINF:1.0,\n\
             https://example.invalid/10.ts\n\
             #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/11.0.m4s\"\n\
             #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"https://example.invalid/11.1.m4s\"\n",
        )
        .unwrap();

        let result = audit(&path, HlsProfile::LlHls).unwrap();
        assert!(result.passed, "{result:#?}");
        assert!(result
            .findings
            .iter()
            .any(|item| { item.rule_id == "FORGE-LL-HLS-BLOCKING-RELOAD" && item.passed }));
        assert_eq!(result.properties["media_playlists"][0]["parts"], 3);
    }

    #[test]
    fn rejects_invalid_part_server_control_and_delta_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken-live.m3u8");
        fs::write(
            &path,
            "#EXTM3U\n\
             #EXT-X-VERSION:8\n\
             #EXT-X-TARGETDURATION:2\n\
             #EXT-X-PART-INF:PART-TARGET=0.5\n\
             #EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=2,CAN-SKIP-DATERANGES=YES,HOLD-BACK=2,PART-HOLD-BACK=0.5\n\
             #EXT-X-PROGRAM-DATE-TIME:2026-07-29T00:00:00Z\n\
             #EXT-X-SKIP:SKIPPED-SEGMENTS=3\n\
             #EXT-X-PART:DURATION=0.6,URI=\"part.m4s\"\n\
             #EXTINF:1.0,\n\
             https://example.invalid/segment.m4s\n",
        )
        .unwrap();

        let result = audit(&path, HlsProfile::LlHls).unwrap();
        assert!(!result.passed);
        for rule in [
            "FORGE-HLS-PART-DURATION",
            "FORGE-HLS-SERVER-CONTROL-RELATIONSHIPS",
            "FORGE-HLS-DELTA-VERSION",
            "FORGE-LL-HLS-PRELOAD",
            "FORGE-LL-HLS-BLOCKING-RELOAD",
        ] {
            assert!(
                result
                    .findings
                    .iter()
                    .any(|item| item.rule_id == rule && !item.passed),
                "missing failure for {rule}: {result:#?}"
            );
        }
    }

    #[test]
    fn cross_checks_low_latency_rendition_reports_and_discontinuity_state() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("master.m3u8"),
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=64000\n\
             a.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=128000\n\
             b.m3u8\n",
        )
        .unwrap();
        let playlist = |other: &str, discontinuity: &str| {
            format!(
                "#EXTM3U\n\
                 #EXT-X-VERSION:9\n\
                 #EXT-X-TARGETDURATION:2\n\
                 #EXT-X-MEDIA-SEQUENCE:10\n\
                 #EXT-X-DISCONTINUITY-SEQUENCE:4\n\
                 #EXT-X-PART-INF:PART-TARGET=0.5\n\
                 #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5\n\
                 #EXT-X-PROGRAM-DATE-TIME:2026-07-29T00:00:00Z\n\
                 {discontinuity}\
                 #EXTINF:1,\n\
                 https://example.invalid/10.ts\n\
                 #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/11.0.m4s\"\n\
                 #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"https://example.invalid/11.1.m4s\"\n\
                 #EXT-X-RENDITION-REPORT:URI=\"{other}\",LAST-MSN=11,LAST-PART=0\n"
            )
        };
        fs::write(directory.path().join("a.m3u8"), playlist("b.m3u8", "")).unwrap();
        fs::write(directory.path().join("b.m3u8"), playlist("a.m3u8", "")).unwrap();

        let path = directory.path().join("master.m3u8");
        let valid = audit(&path, HlsProfile::LlHls).unwrap();
        assert!(valid.passed, "{valid:#?}");
        assert!(valid
            .findings
            .iter()
            .any(|item| { item.rule_id == "FORGE-LL-HLS-RENDITION-REPORT-SET" && item.passed }));

        fs::write(
            directory.path().join("b.m3u8"),
            playlist("a.m3u8", "#EXT-X-DISCONTINUITY\n"),
        )
        .unwrap();
        let invalid = audit(&path, HlsProfile::LlHls).unwrap();
        assert!(!invalid.passed);
        assert!(invalid
            .findings
            .iter()
            .any(|item| { item.rule_id == "FORGE-LL-HLS-DISCONTINUITY-STATE" && !item.passed }));
    }
}
