//! RFC 8216 and Apple HLS package validation with local CMAF cross-checks.

use crate::container_qc;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const HLS_QC_SCHEMA: &str = "https://penguin425.github.io/audio-normalizer/schema/hls-qc-v1";
const MAX_PLAYLIST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REFERENCED_PLAYLISTS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HlsProfile {
    Rfc8216,
    AppleHls,
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
    map_uri: Option<String>,
    referenced_playlists: Vec<String>,
    has_endlist: bool,
    playlist_type: Option<String>,
    version: Option<u64>,
    is_fmp4: bool,
}

pub fn audit(path: &Path, profile: HlsProfile) -> Result<HlsAudit, String> {
    let mut findings = Vec::new();
    let root = parse_playlist(path, profile, &mut findings)?;
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
            let Some(reference) = local_reference(&root.path, uri) else {
                findings.push(finding(
                    "FORGE-HLS-REMOTE-REFERENCE",
                    Severity::Warning,
                    false,
                    format!("remote playlist was not fetched: {uri}"),
                    Some(json!(uri)),
                ));
                continue;
            };
            if !seen.insert(reference.clone()) {
                continue;
            }
            match parse_playlist(&reference, profile, &mut findings) {
                Ok(playlist) if playlist.kind == "media" => media.push(playlist),
                Ok(_) => findings.push(finding(
                    "FORGE-HLS-RENDITION-KIND",
                    Severity::Error,
                    false,
                    format!(
                        "referenced playlist is not a Media Playlist: {}",
                        reference.display()
                    ),
                    None,
                )),
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
    } else {
        media.push(root);
    }

    for playlist in &media {
        audit_media_files(playlist, profile, &mut findings);
    }
    cross_check_renditions(&media, profile, &mut findings);

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
                "fmp4": item.is_fmp4
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
    let mut map_count = 0_usize;
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
                playlist.total_duration += duration;
                pending_duration = None;
                continue;
            }
        }
        if let Some(value) = line.strip_prefix("#EXT-X-VERSION:") {
            singleton_tag(&mut singleton, "EXT-X-VERSION", findings);
            playlist.version = value.parse().ok();
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            singleton_tag(&mut singleton, "EXT-X-TARGETDURATION", findings);
            playlist.target_duration = value.parse().ok();
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
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
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
        !pending_stream && pending_duration.is_none(),
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
    }
    Ok(playlist)
}

fn audit_media_files(playlist: &Playlist, profile: HlsProfile, findings: &mut Vec<HlsFinding>) {
    if let Some(uri) = &playlist.map_uri {
        if let Some(path) = local_reference(&playlist.path, uri) {
            let exists = path.is_file();
            findings.push(finding(
                "FORGE-HLS-LOCAL-RESOURCE",
                Severity::Error,
                exists,
                format!("initialization resource exists: {}", path.display()),
                None,
            ));
            if exists && playlist.is_fmp4 {
                audit_isobmff(&path, true, profile, findings);
            }
        }
    }
    let mut previous_sequences = None;
    let mut last_decode: HashMap<u64, u64> = HashMap::new();
    for uri in &playlist.segment_uris {
        let Some(path) = local_reference(&playlist.path, uri) else {
            findings.push(finding(
                "FORGE-HLS-REMOTE-REFERENCE",
                Severity::Warning,
                false,
                format!("remote segment was not fetched: {uri}"),
                Some(json!(uri)),
            ));
            continue;
        };
        let exists = path.is_file();
        findings.push(finding(
            "FORGE-HLS-LOCAL-RESOURCE",
            Severity::Error,
            exists,
            format!("segment resource exists: {}", path.display()),
            None,
        ));
        if !exists || !playlist.is_fmp4 {
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
}

fn audit_isobmff(
    path: &Path,
    initialization: bool,
    profile: HlsProfile,
    findings: &mut Vec<HlsFinding>,
) {
    match container_qc::audit(path) {
        Ok(audit) => {
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
                let loudness = audit.properties["tracks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|track| track["loudness_box_count"].as_u64().unwrap_or(0) > 0);
                findings.push(finding(
                    "FORGE-APPLE-HLS-LOUDNESS-BOX",
                    Severity::Warning,
                    loudness,
                    "Apple recommends loudness metadata in non-APAC fMP4",
                    None,
                ));
            }
        }
        Err(error) => findings.push(finding(
            "FORGE-HLS-INITIALIZATION-READ",
            Severity::Error,
            false,
            error,
            Some(json!(path)),
        )),
    }
}

fn cross_check_renditions(media: &[Playlist], profile: HlsProfile, findings: &mut Vec<HlsFinding>) {
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

fn local_reference(playlist: &Path, uri: &str) -> Option<PathBuf> {
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || uri.starts_with("//")
    {
        return None;
    }
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    Some(
        playlist
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path),
    )
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
             one.ts\n\
             #EXTINF:6.2,\n\
             two.ts\n\
             #EXT-X-ENDLIST\n",
        )
        .unwrap();
        fs::write(directory.path().join("one.ts"), []).unwrap();
        fs::write(directory.path().join("two.ts"), []).unwrap();

        let rfc = audit(&path, HlsProfile::Rfc8216).unwrap();
        assert!(rfc.passed, "{rfc:#?}");
        let apple = audit(&path, HlsProfile::AppleHls).unwrap();
        assert!(apple.passed, "{apple:#?}");
        assert!(apple.warning_count > 0);
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
        audit_media_files(&playlist, HlsProfile::Rfc8216, &mut findings);
        assert!(
            findings
                .iter()
                .filter(|item| item.severity == Severity::Error)
                .all(|item| item.passed),
            "{findings:#?}"
        );

        fs::write(directory.path().join("two.m4s"), media_segment(10, 512)).unwrap();
        let mut findings = Vec::new();
        audit_media_files(&playlist, HlsProfile::Rfc8216, &mut findings);
        assert!(findings
            .iter()
            .any(|item| item.rule_id == "FORGE-HLS-FRAGMENT-SEQUENCE" && !item.passed));
    }
}
