//! Bounded, offline RTP audio QC for RFC 3550, AES67, and SMPTE ST 2110 audio.
//!
//! The auditor validates an SDP description and, when supplied, correlates it
//! with RTP packets from a classic PCAP or PCAPNG file. It does not perform live capture,
//! decode encrypted RTP, inspect PTP packets, or claim complete device/network
//! conformance.

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::net::IpAddr;
use std::path::Path;

pub const RTP_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/rtp-audio-qc-v1";
pub const ST2022_7_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/st2022-7-qc-v1";
const MAX_SDP_BYTES: u64 = 1024 * 1024;
const MAX_SDP_LINES: usize = 16_384;
const MAX_SDP_LINE_BYTES: usize = 4_096;
const MAX_CAPTURE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_CAPTURE_PACKETS: usize = 2_000_000;
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RtpAudioProfile {
    Rfc3550,
    Aes67,
    Smpte2110_30,
    Smpte2110_31,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Serialize)]
pub struct RtpFinding {
    pub rule_id: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct RtpAudioAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub sdp_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_path: Option<String>,
    pub profile: RtpAudioProfile,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<RtpFinding>,
    pub properties: Value,
}

#[derive(Debug, Serialize)]
pub struct St2022_7Audit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub primary_sdp_path: String,
    pub primary_capture_path: String,
    pub secondary_sdp_path: String,
    pub secondary_capture_path: String,
    pub profile: RtpAudioProfile,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<RtpFinding>,
    pub properties: Value,
}

#[derive(Clone, Debug)]
struct Media {
    port: u16,
    protocol: String,
    formats: Vec<u8>,
    connection: Option<IpAddr>,
    attributes: Vec<String>,
}

#[derive(Clone, Debug)]
struct Sdp {
    session_fields: HashMap<char, Vec<String>>,
    session_attributes: Vec<String>,
    media: Vec<Media>,
}

#[derive(Clone, Debug)]
struct RtpMap {
    payload_type: u8,
    encoding: String,
    clock_rate: u32,
    channels: u16,
}

#[derive(Clone, Debug)]
struct StreamDescription {
    destination: Option<IpAddr>,
    source: Option<IpAddr>,
    port: u16,
    payload_type: u8,
    encoding: String,
    clock_rate: u32,
    channels: u16,
    packet_time_ms: Option<f64>,
    channel_order: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum Endian {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug)]
struct CaptureFormat {
    endian: Endian,
    nanoseconds: bool,
    snaplen: u32,
    link_type: u32,
}

#[derive(Clone, Copy, Debug)]
struct CaptureRecord<'a> {
    timestamp_seconds: f64,
    link_type: u32,
    frame: &'a [u8],
}

#[derive(Debug)]
struct CaptureMetadata {
    format: &'static str,
    link_types: BTreeSet<u32>,
    timestamp_resolutions: BTreeSet<String>,
    sections: usize,
    interfaces: usize,
    records: usize,
}

#[derive(Clone, Copy, Debug)]
struct PcapNgInterface {
    link_type: u32,
    snaplen: u32,
    timestamp_scale: f64,
    timestamp_offset: i64,
}

#[derive(Clone, Debug)]
struct UdpPacket<'a> {
    timestamp_seconds: f64,
    source: IpAddr,
    destination: IpAddr,
    source_port: u16,
    destination_port: u16,
    payload: &'a [u8],
}

#[derive(Clone, Debug)]
struct RtpPacket<'a> {
    marker: bool,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    csrc_count: u8,
    payload: &'a [u8],
}

#[derive(Default)]
struct CaptureStats {
    records: usize,
    udp_packets: usize,
    matching_udp_packets: usize,
    rtp_packets: usize,
    malformed_packets: usize,
    fragmented_packets: usize,
    wrong_version: usize,
    wrong_payload_type: usize,
    payload_geometry_errors: usize,
    marker_packets: usize,
    nonzero_csrc_packets: usize,
    sequence_gaps: u64,
    reordered_packets: u64,
    duplicate_packets: u64,
    timestamp_errors: u64,
    packet_sample_counts: HashSet<usize>,
    ssrcs: HashSet<u32>,
    sources: HashSet<(IpAddr, u16)>,
    first_arrival: Option<f64>,
    last_arrival: Option<f64>,
    first_rtp_timestamp: Option<u32>,
    last_rtp_timestamp: Option<u32>,
    max_jitter_ms: f64,
}

#[derive(Clone, Copy)]
struct PreviousPacket {
    sequence: u16,
    timestamp: u32,
    samples: usize,
    arrival_seconds: f64,
}

#[derive(Clone)]
struct ProtectionPacket {
    arrival_seconds: f64,
    ssrc: u32,
    rtp_datagram_sha256: [u8; 32],
}

#[derive(Default)]
struct ProtectionLeg {
    capture_format: Option<&'static str>,
    link_types: BTreeSet<u32>,
    timestamp_resolutions: BTreeSet<String>,
    sections: usize,
    interfaces: usize,
    records: usize,
    matching_udp_packets: usize,
    malformed_packets: usize,
    fragmented_packets: usize,
    wrong_version: usize,
    wrong_payload_type: usize,
    duplicate_identities: usize,
    packets: BTreeMap<(u32, u16), ProtectionPacket>,
}

pub fn audit(
    sdp_path: &Path,
    capture_path: Option<&Path>,
    profile: RtpAudioProfile,
) -> Result<RtpAudioAudit, String> {
    let text = read_sdp(sdp_path)?;
    let sdp = parse_sdp(&text)?;
    let mut findings = Vec::new();
    let stream = audit_sdp(&sdp, profile, &mut findings)?;

    let capture_properties = if let Some(path) = capture_path {
        Some(audit_capture(path, &stream, profile, &mut findings)?)
    } else {
        findings.push(finding(
            "FORGE-RTP-CAPTURE-PRESENT",
            Severity::Warning,
            false,
            "no PCAP was supplied; packet continuity and payload checks were not performed",
            None,
        ));
        None
    };

    let passed = findings
        .iter()
        .all(|item| item.severity != Severity::Error || item.passed);
    let warning_count = findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    Ok(RtpAudioAudit {
        schema: RTP_QC_SCHEMA,
        generator: "forge-rtp-qc",
        sdp_path: sdp_path.display().to_string(),
        capture_path: capture_path.map(|path| path.display().to_string()),
        profile,
        passed,
        warning_count,
        findings,
        properties: json!({
            "stream": {
                "destination": stream.destination.map(|value| value.to_string()),
                "source_filter": stream.source.map(|value| value.to_string()),
                "port": stream.port,
                "payload_type": stream.payload_type,
                "encoding": stream.encoding,
                "clock_rate": stream.clock_rate,
                "channels": stream.channels,
                "packet_time_ms": stream.packet_time_ms,
                "channel_order": stream.channel_order,
            },
            "capture": capture_properties,
            "scope": {
                "offline_only": true,
                "classic_pcap": true,
                "pcapng": true,
                "ptp_packet_validation": false,
                "rtcp_quality_validation": false,
                "encrypted_rtp": false
            }
        }),
    })
}

pub fn audit_st2022_7(
    primary_sdp_path: &Path,
    primary_capture_path: &Path,
    secondary_sdp_path: &Path,
    secondary_capture_path: &Path,
    profile: RtpAudioProfile,
    max_skew_ms: Option<f64>,
) -> Result<St2022_7Audit, String> {
    if primary_sdp_path == secondary_sdp_path || primary_capture_path == secondary_capture_path {
        return Err("primary and secondary inputs must be distinct paths".into());
    }
    let primary_sdp = parse_sdp(&read_sdp(primary_sdp_path)?)?;
    let secondary_sdp = parse_sdp(&read_sdp(secondary_sdp_path)?)?;
    let mut findings = Vec::new();
    let primary_stream = audit_sdp(&primary_sdp, profile, &mut findings)?;
    let secondary_stream = audit_sdp(&secondary_sdp, profile, &mut findings)?;

    let descriptions_match = primary_stream.payload_type == secondary_stream.payload_type
        && primary_stream.encoding == secondary_stream.encoding
        && primary_stream.clock_rate == secondary_stream.clock_rate
        && primary_stream.channels == secondary_stream.channels
        && primary_stream.packet_time_ms == secondary_stream.packet_time_ms
        && primary_stream.channel_order == secondary_stream.channel_order;
    findings.push(finding(
        "FORGE-ST2022-7-FORMAT",
        Severity::Error,
        descriptions_match,
        "both legs declare the same RTP payload format, clock, channels, packet time, and channel order",
        Some(json!({
            "primary": stream_properties(&primary_stream),
            "secondary": stream_properties(&secondary_stream)
        })),
    ));
    let endpoints_are_diverse = primary_stream.destination != secondary_stream.destination
        || primary_stream.port != secondary_stream.port
        || primary_stream.source != secondary_stream.source;
    findings.push(finding(
        "FORGE-ST2022-7-DIVERSE-ENDPOINTS",
        Severity::Error,
        endpoints_are_diverse,
        "the two SDP legs use distinct source, destination, or port addressing",
        None,
    ));

    let primary = collect_protection_leg(primary_capture_path, &primary_stream)?;
    let secondary = collect_protection_leg(secondary_capture_path, &secondary_stream)?;
    add_protection_leg_findings("PRIMARY", &primary, &mut findings);
    add_protection_leg_findings("SECONDARY", &secondary, &mut findings);

    let primary_ssrcs = primary
        .packets
        .values()
        .map(|packet| packet.ssrc)
        .collect::<HashSet<_>>();
    let secondary_ssrcs = secondary
        .packets
        .values()
        .map(|packet| packet.ssrc)
        .collect::<HashSet<_>>();
    let ssrcs_match =
        primary_ssrcs.len() == 1 && secondary_ssrcs.len() == 1 && primary_ssrcs == secondary_ssrcs;
    findings.push(finding(
        "FORGE-ST2022-7-SSRC",
        Severity::Error,
        ssrcs_match,
        "both protection legs carry one identical RTP synchronization source",
        Some(json!({"primary": primary_ssrcs, "secondary": secondary_ssrcs})),
    ));

    let mut all_keys = primary
        .packets
        .keys()
        .chain(secondary.packets.keys())
        .copied()
        .collect::<Vec<_>>();
    all_keys.sort_unstable();
    all_keys.dedup();
    let shared_keys = primary
        .packets
        .keys()
        .filter(|key| secondary.packets.contains_key(key))
        .copied()
        .collect::<Vec<_>>();
    let primary_only = all_keys
        .iter()
        .filter(|key| !secondary.packets.contains_key(key))
        .count();
    let secondary_only = all_keys
        .iter()
        .filter(|key| !primary.packets.contains_key(key))
        .count();
    let payload_mismatches = shared_keys
        .iter()
        .filter(|key| {
            primary.packets[*key].rtp_datagram_sha256 != secondary.packets[*key].rtp_datagram_sha256
                || primary.packets[*key].ssrc != secondary.packets[*key].ssrc
        })
        .count();
    let primary_timestamps_by_sequence = timestamps_by_sequence(primary.packets.keys().copied());
    let secondary_timestamps_by_sequence =
        timestamps_by_sequence(secondary.packets.keys().copied());
    let identity_mismatches = primary_timestamps_by_sequence
        .iter()
        .filter(|(sequence, timestamps)| {
            secondary_timestamps_by_sequence
                .get(sequence)
                .is_some_and(|other| timestamps.is_disjoint(other))
        })
        .count();
    findings.push(finding(
        "FORGE-ST2022-7-DATAGRAM-EQUIVALENCE",
        Severity::Error,
        !shared_keys.is_empty() && payload_mismatches == 0 && identity_mismatches == 0,
        "matching RTP sequence/timestamp identities and complete RTP datagrams agree on both legs",
        Some(json!({
            "shared_packets": shared_keys.len(),
            "datagram_mismatches": payload_mismatches,
            "identity_mismatches": identity_mismatches
        })),
    ));

    let merged_sequence_gaps = count_merged_sequence_gaps(&all_keys);
    findings.push(finding(
        "FORGE-ST2022-7-MERGED-CONTINUITY",
        Severity::Error,
        !all_keys.is_empty() && merged_sequence_gaps == 0,
        "the union of both protection legs has continuous RTP sequence numbers",
        Some(json!({
            "merged_packets": all_keys.len(),
            "missing_after_merge": merged_sequence_gaps,
            "recovered_from_primary": primary_only,
            "recovered_from_secondary": secondary_only
        })),
    ));

    let mut skews_ms = shared_keys
        .iter()
        .map(|key| {
            (primary.packets[key].arrival_seconds - secondary.packets[key].arrival_seconds).abs()
                * 1000.0
        })
        .collect::<Vec<_>>();
    skews_ms.sort_by(f64::total_cmp);
    let max_observed_skew_ms = skews_ms.last().copied();
    let p95_skew_ms = percentile(&skews_ms, 0.95);
    if let Some(limit) = max_skew_ms {
        findings.push(finding(
            "FORGE-ST2022-7-SKEW",
            Severity::Error,
            max_observed_skew_ms.is_some_and(|observed| observed <= limit),
            "maximum matching-packet arrival skew is within the configured receiver budget",
            Some(json!({
                "limit_ms": limit,
                "maximum_ms": max_observed_skew_ms,
                "p95_ms": p95_skew_ms
            })),
        ));
    } else {
        findings.push(finding(
            "FORGE-ST2022-7-SKEW-BUDGET",
            Severity::Warning,
            false,
            "arrival skew was measured but no receiver skew budget was supplied",
            Some(json!({
                "maximum_ms": max_observed_skew_ms,
                "p95_ms": p95_skew_ms
            })),
        ));
    }

    let passed = findings
        .iter()
        .all(|item| item.severity != Severity::Error || item.passed);
    let warning_count = findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    Ok(St2022_7Audit {
        schema: ST2022_7_QC_SCHEMA,
        generator: "forge-st2022-7-qc",
        primary_sdp_path: primary_sdp_path.display().to_string(),
        primary_capture_path: primary_capture_path.display().to_string(),
        secondary_sdp_path: secondary_sdp_path.display().to_string(),
        secondary_capture_path: secondary_capture_path.display().to_string(),
        profile,
        passed,
        warning_count,
        findings,
        properties: json!({
            "primary": protection_leg_properties(&primary),
            "secondary": protection_leg_properties(&secondary),
            "comparison": {
                "merged_packets": all_keys.len(),
                "shared_packets": shared_keys.len(),
                "primary_only_packets": primary_only,
                "secondary_only_packets": secondary_only,
                "datagram_mismatches": payload_mismatches,
                "identity_mismatches": identity_mismatches,
                "missing_after_merge": merged_sequence_gaps,
                "maximum_arrival_skew_ms": max_observed_skew_ms,
                "p95_arrival_skew_ms": p95_skew_ms,
                "configured_maximum_skew_ms": max_skew_ms
            },
            "scope": {
                "offline_only": true,
                "classic_pcap": true,
                "pcapng": true,
                "seamless_merge_simulation": true,
                "capture_timestamp_timebase_proof": false,
                "network_path_disjointness_proof": false,
                "live_receiver_buffer_validation": false,
                "ptp_lock_validation": false
            }
        }),
    })
}

fn stream_properties(stream: &StreamDescription) -> Value {
    json!({
        "destination": stream.destination.map(|value| value.to_string()),
        "source": stream.source.map(|value| value.to_string()),
        "port": stream.port,
        "payload_type": stream.payload_type,
        "encoding": stream.encoding,
        "clock_rate": stream.clock_rate,
        "channels": stream.channels,
        "packet_time_ms": stream.packet_time_ms,
        "channel_order": stream.channel_order
    })
}

fn add_protection_leg_findings(
    leg_name: &'static str,
    leg: &ProtectionLeg,
    findings: &mut Vec<RtpFinding>,
) {
    let (parse_rule, match_rule, duplicate_rule) = match leg_name {
        "PRIMARY" => (
            "FORGE-ST2022-7-PRIMARY-PARSE",
            "FORGE-ST2022-7-PRIMARY-MATCH",
            "FORGE-ST2022-7-PRIMARY-DUPLICATES",
        ),
        _ => (
            "FORGE-ST2022-7-SECONDARY-PARSE",
            "FORGE-ST2022-7-SECONDARY-MATCH",
            "FORGE-ST2022-7-SECONDARY-DUPLICATES",
        ),
    };
    findings.push(finding(
        parse_rule,
        Severity::Error,
        leg.malformed_packets == 0
            && leg.fragmented_packets == 0
            && leg.wrong_version == 0
            && leg.wrong_payload_type == 0,
        format!("{leg_name} leg packet headers are complete and match the selected RTP payload"),
        Some(json!({
            "malformed": leg.malformed_packets,
            "fragmented": leg.fragmented_packets,
            "wrong_version": leg.wrong_version,
            "wrong_payload_type": leg.wrong_payload_type
        })),
    ));
    findings.push(finding(
        match_rule,
        Severity::Error,
        !leg.packets.is_empty(),
        format!("{leg_name} capture contains packets matching its SDP flow"),
        Some(json!({"packets": leg.packets.len()})),
    ));
    findings.push(finding(
        duplicate_rule,
        Severity::Error,
        leg.duplicate_identities == 0,
        format!("{leg_name} leg has no duplicate RTP timestamp/sequence identities"),
        Some(json!(leg.duplicate_identities)),
    ));
}

fn count_merged_sequence_gaps(keys: &[(u32, u16)]) -> u64 {
    let mut gaps = 0_u64;
    for pair in keys.windows(2) {
        let delta = pair[1].1.wrapping_sub(pair[0].1);
        if delta > 1 && delta < 0x8000 {
            gaps += u64::from(delta - 1);
        }
    }
    gaps
}

fn timestamps_by_sequence(keys: impl Iterator<Item = (u32, u16)>) -> HashMap<u16, HashSet<u32>> {
    let mut result = HashMap::<u16, HashSet<u32>>::new();
    for (timestamp, sequence) in keys {
        result.entry(sequence).or_default().insert(timestamp);
    }
    result
}

fn percentile(sorted: &[f64], percentile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted.get(index).copied()
}

fn protection_leg_properties(leg: &ProtectionLeg) -> Value {
    json!({
        "format": leg.capture_format,
        "link_types": leg.link_types,
        "timestamp_resolutions": leg.timestamp_resolutions,
        "sections": leg.sections,
        "interfaces": leg.interfaces,
        "records": leg.records,
        "matching_udp_packets": leg.matching_udp_packets,
        "rtp_packets": leg.packets.len(),
        "malformed_packets": leg.malformed_packets,
        "fragmented_packets": leg.fragmented_packets,
        "wrong_version": leg.wrong_version,
        "wrong_payload_type": leg.wrong_payload_type,
        "duplicate_identities": leg.duplicate_identities
    })
}

fn read_sdp(path: &Path) -> Result<String, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular SDP file", path.display()));
    }
    if metadata.len() > MAX_SDP_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_SDP_BYTES}-byte SDP safety limit",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.contains(&0) {
        return Err("SDP contains a NUL byte".into());
    }
    String::from_utf8(bytes).map_err(|error| format!("SDP is not UTF-8: {error}"))
}

fn parse_sdp(text: &str) -> Result<Sdp, String> {
    let mut session_fields: HashMap<char, Vec<String>> = HashMap::new();
    let mut session_attributes = Vec::new();
    let mut media = Vec::<Media>::new();
    let mut session_connection = None;
    let mut line_count = 0;
    let mut field_order = Vec::new();
    let mut seen_media = false;

    for (index, raw) in text.lines().enumerate() {
        line_count += 1;
        if line_count > MAX_SDP_LINES {
            return Err(format!("SDP exceeds the {MAX_SDP_LINES}-line safety limit"));
        }
        if raw.len() > MAX_SDP_LINE_BYTES {
            return Err(format!(
                "SDP line {} exceeds the {MAX_SDP_LINE_BYTES}-byte safety limit",
                index + 1
            ));
        }
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 2 || bytes[1] != b'=' || !bytes[0].is_ascii_lowercase() {
            return Err(format!("invalid SDP field at line {}", index + 1));
        }
        let kind = char::from(bytes[0]);
        let value = &line[2..];
        field_order.push(kind);
        if seen_media && matches!(kind, 'v' | 'o' | 's' | 'u' | 'e' | 'p' | 't' | 'r' | 'z') {
            return Err(format!(
                "session-level field {kind}= appears after the first media description at line {}",
                index + 1
            ));
        }
        if kind == 'm' {
            seen_media = true;
            let parts: Vec<_> = value.split_ascii_whitespace().collect();
            if parts.len() < 4 || parts[0] != "audio" {
                return Err(format!(
                    "only m=audio media descriptions are supported (line {})",
                    index + 1
                ));
            }
            let port = parts[1]
                .split('/')
                .next()
                .ok_or_else(|| format!("missing media port at line {}", index + 1))?
                .parse::<u16>()
                .map_err(|_| format!("invalid media port at line {}", index + 1))?;
            let mut formats = Vec::new();
            for value in &parts[3..] {
                let payload_type = value
                    .parse::<u8>()
                    .map_err(|_| format!("invalid payload type at line {}", index + 1))?;
                if payload_type > 127 {
                    return Err(format!("payload type exceeds 127 at line {}", index + 1));
                }
                formats.push(payload_type);
            }
            media.push(Media {
                port,
                protocol: parts[2].into(),
                formats,
                connection: session_connection,
                attributes: Vec::new(),
            });
        } else if kind == 'a' {
            if let Some(current) = media.last_mut() {
                current.attributes.push(value.into());
            } else {
                session_attributes.push(value.into());
            }
        } else if kind == 'c' {
            let address = parse_connection(value)
                .map_err(|error| format!("invalid connection at line {}: {error}", index + 1))?;
            if let Some(current) = media.last_mut() {
                current.connection = Some(address);
            } else {
                session_connection = Some(address);
            }
        } else {
            session_fields.entry(kind).or_default().push(value.into());
        }
    }
    if media.is_empty() {
        return Err("SDP contains no m=audio media description".into());
    }
    if field_order.get(..3) != Some(&['v', 'o', 's']) {
        return Err("SDP must begin with v=, o=, and s= fields in that order".into());
    }
    Ok(Sdp {
        session_fields,
        session_attributes,
        media,
    })
}

fn parse_connection(value: &str) -> Result<IpAddr, String> {
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    if parts.len() != 3 || parts[0] != "IN" || !matches!(parts[1], "IP4" | "IP6") {
        return Err("expected `IN IP4 address` or `IN IP6 address`".into());
    }
    let address = parts[2].split('/').next().unwrap_or(parts[2]);
    address
        .parse()
        .map_err(|_| format!("invalid IP address {address}"))
}

fn audit_sdp(
    sdp: &Sdp,
    profile: RtpAudioProfile,
    findings: &mut Vec<RtpFinding>,
) -> Result<StreamDescription, String> {
    let required = [
        ('v', "version"),
        ('o', "origin"),
        ('s', "session name"),
        ('t', "time"),
    ];
    let missing: Vec<_> = required
        .iter()
        .filter(|(key, _)| sdp.session_fields.get(key).is_none_or(Vec::is_empty))
        .map(|(_, name)| *name)
        .collect();
    findings.push(finding(
        "FORGE-RTP-SDP-REQUIRED-FIELDS",
        Severity::Error,
        missing.is_empty(),
        "SDP contains the required v/o/s/t session fields",
        Some(json!({"missing": missing})),
    ));
    let version_zero = sdp
        .session_fields
        .get(&'v')
        .is_some_and(|items| items.len() == 1 && items[0] == "0");
    findings.push(finding(
        "FORGE-RTP-SDP-VERSION",
        Severity::Error,
        version_zero,
        "SDP version is exactly zero",
        sdp.session_fields.get(&'v').map(|value| json!(value)),
    ));
    findings.push(finding(
        "FORGE-RTP-SDP-SINGLE-MEDIA",
        Severity::Error,
        sdp.media.len() == 1,
        "SDP describes exactly one audio media stream",
        Some(json!(sdp.media.len())),
    ));
    let media = &sdp.media[0];
    findings.push(finding(
        "FORGE-RTP-SDP-SINGLE-FORMAT",
        Severity::Error,
        profile == RtpAudioProfile::Rfc3550 || media.formats.len() == 1,
        "selected interoperability profile advertises exactly one RTP payload format",
        Some(json!(media.formats)),
    ));
    findings.push(finding(
        "FORGE-RTP-SDP-PROTOCOL",
        Severity::Error,
        media.protocol == "RTP/AVP",
        "media protocol is RTP/AVP",
        Some(json!(media.protocol)),
    ));
    findings.push(finding(
        "FORGE-RTP-SDP-CONNECTION",
        Severity::Error,
        media.connection.is_some(),
        "audio media has an effective connection address",
        Some(json!(media.connection.map(|value| value.to_string()))),
    ));
    findings.push(finding(
        "FORGE-RTP-SDP-PORT",
        Severity::Error,
        media.port > 0,
        "audio RTP port is non-zero",
        Some(json!(media.port)),
    ));

    let maps: Vec<_> = media
        .attributes
        .iter()
        .filter_map(|attribute| parse_rtpmap(attribute))
        .collect::<Result<_, _>>()?;
    let selected = maps
        .iter()
        .find(|map| media.formats.contains(&map.payload_type))
        .ok_or_else(|| "no a=rtpmap attribute matches an m=audio payload type".to_string())?;
    let duplicate_map = maps
        .iter()
        .filter(|map| map.payload_type == selected.payload_type)
        .count()
        != 1;
    findings.push(finding(
        "FORGE-RTP-SDP-RTPMAP",
        Severity::Error,
        !duplicate_map,
        "selected payload type has exactly one valid rtpmap",
        Some(json!({
            "payload_type": selected.payload_type,
            "encoding": selected.encoding,
            "clock_rate": selected.clock_rate,
            "channels": selected.channels
        })),
    ));

    let packet_times: Vec<_> = media
        .attributes
        .iter()
        .filter_map(|value| value.strip_prefix("ptime:"))
        .map(|value| {
            value
                .parse::<f64>()
                .map_err(|_| format!("invalid a=ptime value {value}"))
        })
        .collect::<Result<_, _>>()?;
    let packet_time_ms = packet_times.first().copied();
    let ptime_valid = packet_times.len() <= 1
        && packet_time_ms.is_none_or(|value| value.is_finite() && value > 0.0 && value <= 1000.0);
    let ptime_required = matches!(
        profile,
        RtpAudioProfile::Aes67 | RtpAudioProfile::Smpte2110_30 | RtpAudioProfile::Smpte2110_31
    );
    findings.push(finding(
        "FORGE-RTP-SDP-PTIME",
        Severity::Error,
        ptime_valid && (!ptime_required || packet_time_ms.is_some()),
        "packet time is singular, finite, positive, and present for the selected profile",
        Some(json!(packet_times)),
    ));

    let encoding = selected.encoding.to_ascii_uppercase();
    let codec_valid = match profile {
        RtpAudioProfile::Rfc3550 => true,
        RtpAudioProfile::Aes67 | RtpAudioProfile::Smpte2110_30 => {
            matches!(encoding.as_str(), "L16" | "L24")
        }
        RtpAudioProfile::Smpte2110_31 => encoding == "AM824",
    };
    findings.push(finding(
        "FORGE-RTP-PROFILE-CODEC",
        Severity::Error,
        codec_valid,
        "RTP encoding is permitted by the selected profile",
        Some(json!(encoding)),
    ));

    let dynamic_required = matches!(
        profile,
        RtpAudioProfile::Aes67 | RtpAudioProfile::Smpte2110_30 | RtpAudioProfile::Smpte2110_31
    );
    findings.push(finding(
        "FORGE-RTP-PROFILE-DYNAMIC-PT",
        Severity::Error,
        !dynamic_required || (96..=127).contains(&selected.payload_type),
        "profile payload type is dynamically allocated",
        Some(json!(selected.payload_type)),
    ));

    let rate_valid = match profile {
        RtpAudioProfile::Rfc3550 => selected.clock_rate > 0,
        RtpAudioProfile::Aes67 | RtpAudioProfile::Smpte2110_31 => {
            matches!(selected.clock_rate, 44_100 | 48_000 | 96_000)
        }
        RtpAudioProfile::Smpte2110_30 => matches!(selected.clock_rate, 44_100 | 48_000 | 96_000),
    };
    findings.push(finding(
        "FORGE-RTP-PROFILE-CLOCK-RATE",
        Severity::Error,
        rate_valid,
        "RTP clock rate is in the selected profile's supported set",
        Some(json!(selected.clock_rate)),
    ));
    if profile == RtpAudioProfile::Smpte2110_30 {
        findings.push(finding(
            "FORGE-ST2110-30-BASELINE-RATE",
            Severity::Warning,
            selected.clock_rate == 48_000,
            "48 kHz is the mandatory ST 2110-30 baseline rate; other in-scope rates are optional",
            Some(json!(selected.clock_rate)),
        ));
    }

    let channels_valid = selected.channels > 0
        && match profile {
            RtpAudioProfile::Smpte2110_31 => selected.channels % 2 == 0,
            _ => true,
        };
    findings.push(finding(
        "FORGE-RTP-PROFILE-CHANNELS",
        Severity::Error,
        channels_valid,
        "channel count is valid for the selected profile",
        Some(json!(selected.channels)),
    ));

    if profile == RtpAudioProfile::Smpte2110_30 {
        let level = st2110_30_level(
            selected.clock_rate,
            selected.channels,
            packet_time_ms.unwrap_or_default(),
        );
        findings.push(finding(
            "FORGE-ST2110-30-CONFORMANCE-LEVEL",
            Severity::Warning,
            level.is_some(),
            "rate, channel count, and ptime match a named ST 2110-30 receiver capability level",
            Some(json!({"minimum_level": level})),
        ));
    } else if profile == RtpAudioProfile::Smpte2110_31 {
        let samples = packet_time_ms
            .map(|value| (value * f64::from(selected.clock_rate) / 1000.0).round() as u32);
        let permitted = matches!(
            (selected.clock_rate, samples),
            (48_000, Some(48 | 6 | 4)) | (96_000, Some(96 | 12 | 8)) | (44_100, Some(48 | 6 | 4))
        );
        findings.push(finding(
            "FORGE-ST2110-31-PTIME",
            Severity::Error,
            permitted,
            "AM824 ptime maps to a permitted RTP-clock period count",
            Some(json!({"packet_time_ms": packet_time_ms, "periods": samples})),
        ));
    }

    let fmtp = media
        .attributes
        .iter()
        .find_map(|value| value.strip_prefix(&format!("fmtp:{} ", selected.payload_type)));
    let channel_order = fmtp.and_then(parse_channel_order);
    if let Some(order) = &channel_order {
        let count = channel_order_count(order, profile == RtpAudioProfile::Smpte2110_31);
        findings.push(finding(
            "FORGE-ST2110-CHANNEL-ORDER",
            Severity::Error,
            count == Some(usize::from(selected.channels)),
            "SMPTE2110 channel-order syntax accounts for the declared channels",
            Some(json!({"value": order, "declared_channels": selected.channels, "mapped_channels": count})),
        ));
    } else if matches!(
        profile,
        RtpAudioProfile::Smpte2110_30 | RtpAudioProfile::Smpte2110_31
    ) {
        findings.push(finding(
            "FORGE-ST2110-CHANNEL-ORDER",
            Severity::Warning,
            false,
            "channel-order is absent; channels therefore have no declared grouping",
            None,
        ));
    }

    let all_attributes = sdp
        .session_attributes
        .iter()
        .chain(media.attributes.iter())
        .collect::<Vec<_>>();
    let ts_refclk = all_attributes
        .iter()
        .any(|value| value.starts_with("ts-refclk:"));
    let direct_clock = all_attributes
        .iter()
        .any(|value| value.as_str() == "mediaclk:direct=0");
    let clock_required = matches!(
        profile,
        RtpAudioProfile::Aes67 | RtpAudioProfile::Smpte2110_30 | RtpAudioProfile::Smpte2110_31
    );
    findings.push(finding(
        "FORGE-RTP-SDP-REFERENCE-CLOCK",
        Severity::Error,
        !clock_required || (ts_refclk && direct_clock),
        "profile SDP identifies a timestamp reference clock and direct media clock offset zero",
        Some(json!({"ts_refclk": ts_refclk, "mediaclk_direct_zero": direct_clock})),
    ));
    let source_filter = all_attributes
        .iter()
        .find_map(|value| parse_source_filter(value));
    if media
        .connection
        .is_some_and(|address| address.is_multicast())
    {
        findings.push(finding(
            "FORGE-RTP-SDP-SOURCE-FILTER",
            Severity::Warning,
            source_filter.is_some(),
            "multicast SDP declares an explicit source-filter",
            Some(json!(source_filter.map(|value| value.to_string()))),
        ));
    }

    Ok(StreamDescription {
        destination: media.connection,
        source: source_filter,
        port: media.port,
        payload_type: selected.payload_type,
        encoding,
        clock_rate: selected.clock_rate,
        channels: selected.channels,
        packet_time_ms,
        channel_order,
    })
}

fn parse_source_filter(attribute: &str) -> Option<IpAddr> {
    let value = attribute.strip_prefix("source-filter:")?;
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    if parts.len() != 5 || parts[0] != "incl" || parts[1] != "IN" {
        return None;
    }
    parts[4].parse().ok()
}

fn parse_rtpmap(attribute: &str) -> Option<Result<RtpMap, String>> {
    let value = attribute.strip_prefix("rtpmap:")?;
    Some((|| {
        let (payload, encoding) = value
            .split_once(' ')
            .ok_or_else(|| format!("invalid rtpmap attribute {attribute}"))?;
        let payload_type = payload
            .parse::<u8>()
            .map_err(|_| format!("invalid rtpmap payload type {payload}"))?;
        if payload_type > 127 {
            return Err(format!("rtpmap payload type exceeds 127: {payload_type}"));
        }
        let fields: Vec<_> = encoding.split('/').collect();
        if !(2..=3).contains(&fields.len()) || fields.iter().any(|item| item.is_empty()) {
            return Err(format!("invalid rtpmap encoding {encoding}"));
        }
        let clock_rate = fields[1]
            .parse::<u32>()
            .map_err(|_| format!("invalid rtpmap clock rate {}", fields[1]))?;
        let channels = fields.get(2).map_or(Ok(1), |value| {
            value
                .parse::<u16>()
                .map_err(|_| format!("invalid rtpmap channel count {value}"))
        })?;
        Ok(RtpMap {
            payload_type,
            encoding: fields[0].into(),
            clock_rate,
            channels,
        })
    })())
}

fn parse_channel_order(fmtp: &str) -> Option<String> {
    fmtp.split(';')
        .map(str::trim)
        .find_map(|item| item.strip_prefix("channel-order="))
        .map(str::to_owned)
}

fn channel_order_count(value: &str, allow_aes3: bool) -> Option<usize> {
    let body = value.strip_prefix("SMPTE2110.(")?.strip_suffix(')')?;
    if body.is_empty() {
        return None;
    }
    let mut count = 0usize;
    for symbol in body.split(',').map(str::trim) {
        let channels = match symbol {
            "M" => 1,
            "DM" | "ST" | "LtRt" => 2,
            "51" => 6,
            "71" => 8,
            "222" => 24,
            "SGRP" => 4,
            "AES3" if allow_aes3 => 2,
            value if value.len() == 3 && value.starts_with('U') => {
                let parsed = value[1..].parse::<usize>().ok()?;
                if !(1..=64).contains(&parsed) {
                    return None;
                }
                parsed
            }
            _ => return None,
        };
        count = count.checked_add(channels)?;
    }
    Some(count)
}

fn st2110_30_level(rate: u32, channels: u16, ptime: f64) -> Option<&'static str> {
    let samples = (ptime * f64::from(rate) / 1000.0).round() as u32;
    match (rate, samples, channels) {
        (48_000, 48, 1..=8) => Some("A"),
        (96_000, 96, 1..=4) => Some("AX"),
        (48_000, 6, 1..=8) => Some("B"),
        (96_000, 12, 1..=8) => Some("BX"),
        (48_000, 6, 9..=64) => Some("C"),
        (96_000, 12, 9..=32) => Some("CX"),
        _ => None,
    }
}

fn audit_capture(
    path: &Path,
    stream: &StreamDescription,
    profile: RtpAudioProfile,
    findings: &mut Vec<RtpFinding>,
) -> Result<Value, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular capture file", path.display()));
    }
    if metadata.len() > MAX_CAPTURE_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_CAPTURE_BYTES}-byte capture safety limit",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut stats = CaptureStats::default();
    let mut previous: HashMap<u32, PreviousPacket> = HashMap::new();
    let mut seen = HashSet::new();
    let capture = walk_capture(&bytes, |record| {
        stats.records += 1;
        let arrival = record.timestamp_seconds;
        let udp = match extract_udp(record.frame, record.link_type, arrival) {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(()),
            Err(PacketError::Fragmented) => {
                stats.fragmented_packets += 1;
                return Ok(());
            }
            Err(PacketError::Malformed) => {
                stats.malformed_packets += 1;
                return Ok(());
            }
        };
        stats.udp_packets += 1;
        if udp.destination_port != stream.port
            || stream
                .destination
                .is_some_and(|destination| destination != udp.destination)
            || stream.source.is_some_and(|source| source != udp.source)
        {
            return Ok(());
        }
        stats.matching_udp_packets += 1;
        stats.sources.insert((udp.source, udp.source_port));
        let rtp = match parse_rtp(udp.payload) {
            Ok(value) => value,
            Err(RtpError::WrongVersion) => {
                stats.wrong_version += 1;
                return Ok(());
            }
            Err(RtpError::Malformed) => {
                stats.malformed_packets += 1;
                return Ok(());
            }
        };
        if rtp.payload_type != stream.payload_type {
            stats.wrong_payload_type += 1;
            return Ok(());
        }
        stats.rtp_packets += 1;
        stats.ssrcs.insert(rtp.ssrc);
        if rtp.marker {
            stats.marker_packets += 1;
        }
        if rtp.csrc_count != 0 {
            stats.nonzero_csrc_packets += 1;
        }
        let sample_count = payload_sample_count(stream, rtp.payload);
        let samples = match sample_count {
            Some(value) if value > 0 => {
                stats.packet_sample_counts.insert(value);
                value
            }
            _ => {
                stats.payload_geometry_errors += 1;
                0
            }
        };
        if profile == RtpAudioProfile::Smpte2110_31
            && rtp
                .payload
                .chunks_exact(4)
                .any(|word| word[0] & 0xc0 != 0 || word[0] & 0x20 != 0 && word[0] & 0x10 == 0)
        {
            stats.payload_geometry_errors += 1;
        }
        let identity = (rtp.ssrc, rtp.sequence, rtp.timestamp);
        if !seen.insert(identity) {
            stats.duplicate_packets += 1;
            return Ok(());
        }
        let mut advances_sequence = true;
        if let Some(last) = previous.get(&rtp.ssrc).copied() {
            let delta = rtp.sequence.wrapping_sub(last.sequence);
            if delta == 1 {
                let timestamp_delta = rtp.timestamp.wrapping_sub(last.timestamp);
                if timestamp_delta != last.samples as u32 {
                    stats.timestamp_errors += 1;
                }
                let arrival_samples =
                    (arrival - last.arrival_seconds) * f64::from(stream.clock_rate);
                let jitter = (arrival_samples - f64::from(timestamp_delta)).abs() * 1000.0
                    / f64::from(stream.clock_rate);
                stats.max_jitter_ms = stats.max_jitter_ms.max(jitter);
            } else if delta < 0x8000 {
                stats.sequence_gaps += u64::from(delta.saturating_sub(1));
            } else {
                stats.reordered_packets += 1;
                advances_sequence = false;
            }
        }
        if samples > 0 && advances_sequence {
            previous.insert(
                rtp.ssrc,
                PreviousPacket {
                    sequence: rtp.sequence,
                    timestamp: rtp.timestamp,
                    samples,
                    arrival_seconds: arrival,
                },
            );
        }
        stats.first_arrival.get_or_insert(udp.timestamp_seconds);
        stats.last_arrival = Some(udp.timestamp_seconds);
        stats.first_rtp_timestamp.get_or_insert(rtp.timestamp);
        stats.last_rtp_timestamp = Some(rtp.timestamp);
        Ok(())
    })?;

    findings.push(finding(
        "FORGE-RTP-PCAP-LINKTYPE",
        Severity::Error,
        capture
            .link_types
            .iter()
            .all(|link_type| matches!(link_type, 1 | 101 | 113)),
        "capture link types are Ethernet, raw IP, or Linux cooked capture",
        Some(json!(capture.link_types)),
    ));
    findings.push(finding(
        "FORGE-RTP-PCAP-MALFORMED",
        Severity::Error,
        stats.malformed_packets == 0,
        "matching packet parsing completed without malformed headers",
        Some(json!(stats.malformed_packets)),
    ));
    findings.push(finding(
        "FORGE-RTP-PCAP-FRAGMENTS",
        Severity::Warning,
        stats.fragmented_packets == 0,
        "capture does not require IP fragment reassembly",
        Some(json!(stats.fragmented_packets)),
    ));
    findings.push(finding(
        "FORGE-RTP-PCAP-MATCH",
        Severity::Error,
        stats.rtp_packets > 0,
        "capture contains RTP packets matching the SDP destination, port, and payload type",
        Some(json!({
            "matching_udp_packets": stats.matching_udp_packets,
            "rtp_packets": stats.rtp_packets,
            "wrong_payload_type": stats.wrong_payload_type,
            "wrong_version": stats.wrong_version
        })),
    ));
    findings.push(finding(
        "FORGE-RTP-PCAP-RTP-IDENTITY",
        Severity::Error,
        stats.wrong_version == 0 && stats.wrong_payload_type == 0,
        "all UDP payloads on the described flow are RTP version 2 with the selected payload type",
        Some(json!({
            "wrong_version": stats.wrong_version,
            "wrong_payload_type": stats.wrong_payload_type
        })),
    ));
    findings.push(finding(
        "FORGE-RTP-SSRC",
        Severity::Error,
        stats.ssrcs.len() == 1,
        "the audited RTP flow uses one synchronization source",
        Some(json!(stats.ssrcs)),
    ));
    findings.push(finding(
        "FORGE-RTP-SOURCE",
        Severity::Warning,
        stats.sources.len() == 1,
        "matching RTP packets originate from one IP address and UDP port",
        Some(json!(stats
            .sources
            .iter()
            .map(|(ip, port)| format!("{ip}:{port}"))
            .collect::<Vec<_>>())),
    ));
    findings.push(finding(
        "FORGE-RTP-SEQUENCE",
        Severity::Error,
        stats.sequence_gaps == 0 && stats.reordered_packets == 0 && stats.duplicate_packets == 0,
        "RTP sequence numbers are continuous, ordered, and unique in capture order",
        Some(json!({
            "missing": stats.sequence_gaps,
            "reordered": stats.reordered_packets,
            "duplicates": stats.duplicate_packets
        })),
    ));
    findings.push(finding(
        "FORGE-RTP-TIMESTAMP",
        Severity::Error,
        stats.timestamp_errors == 0,
        "RTP timestamp steps equal the previous packet's sample count",
        Some(json!(stats.timestamp_errors)),
    ));
    findings.push(finding(
        "FORGE-RTP-PAYLOAD-GEOMETRY",
        Severity::Error,
        stats.payload_geometry_errors == 0 && stats.packet_sample_counts.len() == 1,
        "payloads contain a stable, whole number of samples for every declared channel",
        Some(json!({
            "errors": stats.payload_geometry_errors,
            "sample_counts": stats.packet_sample_counts
        })),
    ));
    if let Some(ptime) = stream.packet_time_ms {
        let expected = (ptime * f64::from(stream.clock_rate) / 1000.0)
            .round()
            .max(1.0) as usize;
        findings.push(finding(
            "FORGE-RTP-PTIME-PAYLOAD",
            Severity::Error,
            stats.packet_sample_counts.len() == 1 && stats.packet_sample_counts.contains(&expected),
            "captured payload duration agrees with the SDP ptime",
            Some(json!({"expected_samples": expected, "observed": stats.packet_sample_counts})),
        ));
    }
    if profile == RtpAudioProfile::Smpte2110_31 {
        findings.push(finding(
            "FORGE-ST2110-31-RTP-HEADER",
            Severity::Error,
            stats.marker_packets == 0 && stats.nonzero_csrc_packets == 0,
            "AM824 RTP packets have Marker zero and CSRC count zero",
            Some(json!({
                "marker_packets": stats.marker_packets,
                "nonzero_csrc_packets": stats.nonzero_csrc_packets
            })),
        ));
    }
    let capture_duration_ms = match (stats.first_arrival, stats.last_arrival) {
        (Some(first), Some(last)) => Some((last - first).max(0.0) * 1000.0),
        _ => None,
    };
    let rtp_duration_ms = match (stats.first_rtp_timestamp, stats.last_rtp_timestamp) {
        (Some(first), Some(last)) => {
            Some(f64::from(last.wrapping_sub(first)) * 1000.0 / f64::from(stream.clock_rate))
        }
        _ => None,
    };
    let link_type = (capture.link_types.len() == 1)
        .then(|| capture.link_types.iter().next().copied())
        .flatten();
    let timestamp_resolution = if capture.format == "pcap" {
        Some(
            if capture.timestamp_resolutions.contains("10^-9 seconds") {
                "nanoseconds"
            } else {
                "microseconds"
            }
            .to_string(),
        )
    } else if capture.timestamp_resolutions.len() == 1 {
        capture.timestamp_resolutions.iter().next().cloned()
    } else {
        None
    };
    Ok(json!({
        "format": capture.format,
        "link_type": link_type,
        "link_types": capture.link_types,
        "timestamp_resolution": timestamp_resolution,
        "timestamp_resolutions": capture.timestamp_resolutions,
        "sections": capture.sections,
        "interfaces": capture.interfaces,
        "records": stats.records,
        "udp_packets": stats.udp_packets,
        "matching_udp_packets": stats.matching_udp_packets,
        "rtp_packets": stats.rtp_packets,
        "ssrcs": stats.ssrcs,
        "sources": stats.sources.iter().map(|(ip, port)| format!("{ip}:{port}")).collect::<Vec<_>>(),
        "sequence_gaps": stats.sequence_gaps,
        "reordered_packets": stats.reordered_packets,
        "duplicate_packets": stats.duplicate_packets,
        "timestamp_errors": stats.timestamp_errors,
        "packet_sample_counts": stats.packet_sample_counts,
        "max_interarrival_jitter_ms": stats.max_jitter_ms,
        "capture_duration_ms": capture_duration_ms,
        "rtp_timestamp_duration_ms": rtp_duration_ms
    }))
}

fn collect_protection_leg(
    path: &Path,
    stream: &StreamDescription,
) -> Result<ProtectionLeg, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular capture file", path.display()));
    }
    if metadata.len() > MAX_CAPTURE_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_CAPTURE_BYTES}-byte capture safety limit",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut leg = ProtectionLeg::default();
    let capture = walk_capture(&bytes, |record| {
        leg.records += 1;
        let arrival = record.timestamp_seconds;
        let udp = match extract_udp(record.frame, record.link_type, arrival) {
            Ok(Some(packet)) => packet,
            Ok(None) => return Ok(()),
            Err(PacketError::Fragmented) => {
                leg.fragmented_packets += 1;
                return Ok(());
            }
            Err(PacketError::Malformed) => {
                leg.malformed_packets += 1;
                return Ok(());
            }
        };
        if udp.destination_port != stream.port
            || stream
                .destination
                .is_some_and(|destination| destination != udp.destination)
            || stream.source.is_some_and(|source| source != udp.source)
        {
            return Ok(());
        }
        leg.matching_udp_packets += 1;
        let rtp = match parse_rtp(udp.payload) {
            Ok(packet) => packet,
            Err(RtpError::WrongVersion) => {
                leg.wrong_version += 1;
                return Ok(());
            }
            Err(RtpError::Malformed) => {
                leg.malformed_packets += 1;
                return Ok(());
            }
        };
        if rtp.payload_type != stream.payload_type {
            leg.wrong_payload_type += 1;
            return Ok(());
        }
        let key = (rtp.timestamp, rtp.sequence);
        let packet = ProtectionPacket {
            arrival_seconds: arrival,
            ssrc: rtp.ssrc,
            rtp_datagram_sha256: Sha256::digest(udp.payload).into(),
        };
        if leg.packets.insert(key, packet).is_some() {
            leg.duplicate_identities += 1;
        }
        Ok(())
    })?;
    leg.capture_format = Some(capture.format);
    leg.link_types = capture.link_types;
    leg.timestamp_resolutions = capture.timestamp_resolutions;
    leg.sections = capture.sections;
    leg.interfaces = capture.interfaces;
    Ok(leg)
}

fn walk_capture<'a>(
    bytes: &'a [u8],
    visit: impl FnMut(CaptureRecord<'a>) -> Result<(), String>,
) -> Result<CaptureMetadata, String> {
    if bytes.starts_with(&[0x0a, 0x0d, 0x0d, 0x0a]) {
        walk_pcapng(bytes, visit)
    } else {
        walk_classic_pcap(bytes, visit)
    }
}

fn walk_classic_pcap<'a>(
    bytes: &'a [u8],
    mut visit: impl FnMut(CaptureRecord<'a>) -> Result<(), String>,
) -> Result<CaptureMetadata, String> {
    let (format, mut offset) = parse_pcap_header(bytes)?;
    let mut records = 0;
    while offset < bytes.len() {
        if records == MAX_CAPTURE_PACKETS {
            return Err(format!(
                "capture exceeds the {MAX_CAPTURE_PACKETS}-packet safety limit"
            ));
        }
        let record = bytes
            .get(offset..offset + 16)
            .ok_or_else(|| format!("truncated PCAP record header at byte {offset}"))?;
        let seconds = read_u32(&record[0..4], format.endian);
        let fraction = read_u32(&record[4..8], format.endian);
        let captured = read_u32(&record[8..12], format.endian) as usize;
        let original = read_u32(&record[12..16], format.endian) as usize;
        if captured > MAX_PACKET_BYTES || captured > format.snaplen as usize || captured > original
        {
            return Err(format!(
                "invalid PCAP record length at packet {}",
                records + 1
            ));
        }
        let fraction_limit = if format.nanoseconds {
            1_000_000_000
        } else {
            1_000_000
        };
        if fraction >= fraction_limit {
            return Err(format!(
                "invalid PCAP timestamp fraction at packet {}",
                records + 1
            ));
        }
        offset += 16;
        let frame = bytes
            .get(offset..offset + captured)
            .ok_or_else(|| format!("truncated PCAP packet at byte {offset}"))?;
        offset += captured;
        records += 1;
        let divisor = if format.nanoseconds { 1e9 } else { 1e6 };
        visit(CaptureRecord {
            timestamp_seconds: f64::from(seconds) + f64::from(fraction) / divisor,
            link_type: format.link_type,
            frame,
        })?;
    }
    Ok(CaptureMetadata {
        format: "pcap",
        link_types: BTreeSet::from([format.link_type]),
        timestamp_resolutions: BTreeSet::from([if format.nanoseconds {
            "10^-9 seconds".to_string()
        } else {
            "10^-6 seconds".to_string()
        }]),
        sections: 1,
        interfaces: 1,
        records,
    })
}

fn walk_pcapng<'a>(
    bytes: &'a [u8],
    mut visit: impl FnMut(CaptureRecord<'a>) -> Result<(), String>,
) -> Result<CaptureMetadata, String> {
    const SECTION_HEADER: [u8; 4] = [0x0a, 0x0d, 0x0d, 0x0a];
    const INTERFACE_DESCRIPTION: u32 = 1;
    const SIMPLE_PACKET: u32 = 3;
    const ENHANCED_PACKET: u32 = 6;

    let mut offset = 0;
    let mut endian = None;
    let mut interfaces = Vec::<PcapNgInterface>::new();
    let mut metadata = CaptureMetadata {
        format: "pcapng",
        link_types: BTreeSet::new(),
        timestamp_resolutions: BTreeSet::new(),
        sections: 0,
        interfaces: 0,
        records: 0,
    };
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 12)
            .ok_or_else(|| format!("truncated PCAPNG block header at byte {offset}"))?;
        if header[..4] == SECTION_HEADER {
            let section_endian = match &header[8..12] {
                [0x4d, 0x3c, 0x2b, 0x1a] => Endian::Little,
                [0x1a, 0x2b, 0x3c, 0x4d] => Endian::Big,
                _ => {
                    return Err(format!(
                        "invalid PCAPNG byte-order magic at byte {}",
                        offset + 8
                    ));
                }
            };
            let block = pcapng_block(bytes, offset, section_endian, 28)?;
            let major = read_u16(&block[12..14], section_endian);
            let minor = read_u16(&block[14..16], section_endian);
            if (major, minor) != (1, 0) {
                return Err(format!("unsupported PCAPNG version {major}.{minor}"));
            }
            validate_pcapng_options(&block[24..block.len() - 4], section_endian, "section")?;
            offset += block.len();
            endian = Some(section_endian);
            interfaces.clear();
            metadata.sections += 1;
            continue;
        }

        let section_endian =
            endian.ok_or_else(|| "PCAPNG must begin with a Section Header Block".to_string())?;
        let block_type = read_u32(&header[..4], section_endian);
        let block = pcapng_block(bytes, offset, section_endian, 12)?;
        match block_type {
            INTERFACE_DESCRIPTION => {
                if block.len() < 20 {
                    return Err(format!(
                        "PCAPNG Interface Description Block at byte {offset} is too short"
                    ));
                }
                let link_type = u32::from(read_u16(&block[8..10], section_endian));
                validate_capture_link_type(link_type, "PCAPNG")?;
                let snaplen = read_u32(&block[12..16], section_endian);
                if snaplen as usize > MAX_PACKET_BYTES {
                    return Err(format!("invalid PCAPNG interface snaplen {snaplen}"));
                }
                let mut timestamp_scale = 1e-6;
                let mut timestamp_resolution = "10^-6 seconds".to_string();
                let mut timestamp_offset = 0_i64;
                let mut saw_resolution = false;
                let mut saw_offset = false;
                walk_pcapng_options(
                    &block[16..block.len() - 4],
                    section_endian,
                    "interface",
                    |code, value| {
                        match code {
                            9 => {
                                if saw_resolution || value.len() != 1 {
                                    return Err(
                                        "PCAPNG if_tsresol must occur once with length one".into(),
                                    );
                                }
                                saw_resolution = true;
                                let raw = value[0];
                                let exponent = i32::from(raw & 0x7f);
                                if raw & 0x80 == 0 {
                                    timestamp_scale = 10_f64.powi(-exponent);
                                    timestamp_resolution = format!("10^-{exponent} seconds");
                                } else {
                                    timestamp_scale = 2_f64.powi(-exponent);
                                    timestamp_resolution = format!("2^-{exponent} seconds");
                                }
                            }
                            14 => {
                                if saw_offset || value.len() != 8 {
                                    return Err(
                                        "PCAPNG if_tsoffset must occur once with length eight"
                                            .into(),
                                    );
                                }
                                saw_offset = true;
                                timestamp_offset = read_u64(value, section_endian) as i64;
                            }
                            _ => {}
                        }
                        Ok(())
                    },
                )?;
                if !timestamp_scale.is_finite() || timestamp_scale <= 0.0 {
                    return Err("invalid PCAPNG timestamp resolution".into());
                }
                interfaces.push(PcapNgInterface {
                    link_type,
                    snaplen,
                    timestamp_scale,
                    timestamp_offset,
                });
                metadata.link_types.insert(link_type);
                metadata.timestamp_resolutions.insert(timestamp_resolution);
                metadata.interfaces += 1;
            }
            ENHANCED_PACKET => {
                if block.len() < 32 {
                    return Err(format!(
                        "PCAPNG Enhanced Packet Block at byte {offset} is too short"
                    ));
                }
                if metadata.records == MAX_CAPTURE_PACKETS {
                    return Err(format!(
                        "capture exceeds the {MAX_CAPTURE_PACKETS}-packet safety limit"
                    ));
                }
                let interface_id = read_u32(&block[8..12], section_endian) as usize;
                let interface = interfaces.get(interface_id).copied().ok_or_else(|| {
                    format!(
                        "PCAPNG packet at byte {offset} references undefined interface {interface_id}"
                    )
                })?;
                let timestamp = u64::from(read_u32(&block[12..16], section_endian)) << 32
                    | u64::from(read_u32(&block[16..20], section_endian));
                let captured = read_u32(&block[20..24], section_endian) as usize;
                let original = read_u32(&block[24..28], section_endian) as usize;
                if captured > MAX_PACKET_BYTES
                    || interface.snaplen != 0 && captured > interface.snaplen as usize
                    || captured > original
                {
                    return Err(format!(
                        "invalid PCAPNG packet length at packet {}",
                        metadata.records + 1
                    ));
                }
                let padded = captured
                    .checked_add(3)
                    .ok_or_else(|| "PCAPNG packet length overflow".to_string())?
                    & !3;
                let data_end = 28_usize
                    .checked_add(padded)
                    .ok_or_else(|| "PCAPNG packet length overflow".to_string())?;
                if data_end + 4 > block.len() {
                    return Err(format!(
                        "truncated PCAPNG packet data at byte {}",
                        offset + 28
                    ));
                }
                validate_pcapng_options(
                    &block[data_end..block.len() - 4],
                    section_endian,
                    "packet",
                )?;
                let arrival = timestamp as f64 * interface.timestamp_scale
                    + interface.timestamp_offset as f64;
                if !arrival.is_finite() {
                    return Err(format!(
                        "PCAPNG timestamp at packet {} is not representable",
                        metadata.records + 1
                    ));
                }
                metadata.records += 1;
                visit(CaptureRecord {
                    timestamp_seconds: arrival,
                    link_type: interface.link_type,
                    frame: &block[28..28 + captured],
                })?;
            }
            SIMPLE_PACKET => {
                return Err(
                    "PCAPNG Simple Packet Blocks have no timestamp; use Enhanced Packet Blocks"
                        .into(),
                );
            }
            _ => {}
        }
        offset += block.len();
    }
    if metadata.sections == 0 {
        return Err("PCAPNG contains no Section Header Block".into());
    }
    Ok(metadata)
}

fn pcapng_block(
    bytes: &[u8],
    offset: usize,
    endian: Endian,
    minimum_length: usize,
) -> Result<&[u8], String> {
    let header = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| format!("truncated PCAPNG block header at byte {offset}"))?;
    let length = read_u32(&header[4..8], endian) as usize;
    if length < minimum_length || !length.is_multiple_of(4) {
        return Err(format!(
            "invalid PCAPNG block length {length} at byte {offset}"
        ));
    }
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "PCAPNG block length overflow".to_string())?;
    let block = bytes
        .get(offset..end)
        .ok_or_else(|| format!("truncated PCAPNG block at byte {offset}"))?;
    let trailing = read_u32(&block[length - 4..], endian) as usize;
    if trailing != length {
        return Err(format!(
            "PCAPNG block lengths disagree at byte {offset}: {length} != {trailing}"
        ));
    }
    Ok(block)
}

fn validate_pcapng_options(bytes: &[u8], endian: Endian, context: &str) -> Result<(), String> {
    walk_pcapng_options(bytes, endian, context, |_, _| Ok(()))
}

fn walk_pcapng_options(
    bytes: &[u8],
    endian: Endian,
    context: &str,
    mut visit: impl FnMut(u16, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut offset = 0;
    while offset < bytes.len() {
        let header = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| format!("truncated PCAPNG {context} option header at byte {offset}"))?;
        let code = read_u16(&header[..2], endian);
        let length = usize::from(read_u16(&header[2..4], endian));
        offset += 4;
        if code == 0 {
            if length != 0 {
                return Err(format!("invalid PCAPNG {context} end-of-options marker"));
            }
            return Ok(());
        }
        let padded = length
            .checked_add(3)
            .ok_or_else(|| format!("PCAPNG {context} option length overflow"))?
            & !3;
        let option = bytes
            .get(offset..offset + padded)
            .ok_or_else(|| format!("truncated PCAPNG {context} option {code}"))?;
        visit(code, &option[..length])?;
        offset += padded;
    }
    Ok(())
}

fn validate_capture_link_type(link_type: u32, format: &str) -> Result<(), String> {
    if matches!(link_type, 1 | 101 | 113) {
        Ok(())
    } else {
        Err(format!(
            "unsupported {format} link type {link_type}; expected Ethernet (1), raw IP (101), or Linux cooked (113)"
        ))
    }
}

fn parse_pcap_header(bytes: &[u8]) -> Result<(CaptureFormat, usize), String> {
    let header = bytes
        .get(..24)
        .ok_or_else(|| "capture is shorter than a classic PCAP global header".to_string())?;
    let (endian, nanoseconds) = match &header[..4] {
        [0xd4, 0xc3, 0xb2, 0xa1] => (Endian::Little, false),
        [0xa1, 0xb2, 0xc3, 0xd4] => (Endian::Big, false),
        [0x4d, 0x3c, 0xb2, 0xa1] => (Endian::Little, true),
        [0xa1, 0xb2, 0x3c, 0x4d] => (Endian::Big, true),
        _ => return Err("unrecognized capture magic; expected classic PCAP".into()),
    };
    let major = read_u16(&header[4..6], endian);
    let minor = read_u16(&header[6..8], endian);
    if (major, minor) != (2, 4) {
        return Err(format!("unsupported PCAP version {major}.{minor}"));
    }
    let snaplen = read_u32(&header[16..20], endian);
    if snaplen == 0 || snaplen as usize > MAX_PACKET_BYTES {
        return Err(format!("invalid PCAP snaplen {snaplen}"));
    }
    let link_type = read_u32(&header[20..24], endian);
    validate_capture_link_type(link_type, "PCAP")?;
    Ok((
        CaptureFormat {
            endian,
            nanoseconds,
            snaplen,
            link_type,
        },
        24,
    ))
}

#[derive(Clone, Copy, Debug)]
enum PacketError {
    Fragmented,
    Malformed,
}

fn extract_udp(
    frame: &[u8],
    link_type: u32,
    timestamp_seconds: f64,
) -> Result<Option<UdpPacket<'_>>, PacketError> {
    let (mut offset, mut ether_type) = match link_type {
        1 => {
            let header = frame.get(..14).ok_or(PacketError::Malformed)?;
            (14, u16::from_be_bytes([header[12], header[13]]))
        }
        101 => {
            let version = frame.first().ok_or(PacketError::Malformed)? >> 4;
            (0, if version == 4 { 0x0800 } else { 0x86dd })
        }
        113 => {
            let header = frame.get(..16).ok_or(PacketError::Malformed)?;
            (16, u16::from_be_bytes([header[14], header[15]]))
        }
        _ => return Ok(None),
    };
    for _ in 0..2 {
        if matches!(ether_type, 0x8100 | 0x88a8 | 0x9100) {
            let vlan = frame
                .get(offset..offset + 4)
                .ok_or(PacketError::Malformed)?;
            ether_type = u16::from_be_bytes([vlan[2], vlan[3]]);
            offset += 4;
        }
    }
    match ether_type {
        0x0800 => extract_ipv4_udp(frame, offset, timestamp_seconds),
        0x86dd => extract_ipv6_udp(frame, offset, timestamp_seconds),
        _ => Ok(None),
    }
}

fn extract_ipv4_udp(
    frame: &[u8],
    offset: usize,
    timestamp_seconds: f64,
) -> Result<Option<UdpPacket<'_>>, PacketError> {
    let base = frame
        .get(offset..offset + 20)
        .ok_or(PacketError::Malformed)?;
    if base[0] >> 4 != 4 {
        return Err(PacketError::Malformed);
    }
    let header_len = usize::from(base[0] & 0x0f) * 4;
    if header_len < 20 {
        return Err(PacketError::Malformed);
    }
    let total_len = usize::from(u16::from_be_bytes([base[2], base[3]]));
    if total_len < header_len || offset + total_len > frame.len() {
        return Err(PacketError::Malformed);
    }
    let fragment = u16::from_be_bytes([base[6], base[7]]);
    if fragment & 0x3fff != 0 {
        return Err(PacketError::Fragmented);
    }
    if base[9] != 17 {
        return Ok(None);
    }
    let source = IpAddr::from([base[12], base[13], base[14], base[15]]);
    let destination = IpAddr::from([base[16], base[17], base[18], base[19]]);
    extract_udp_header(
        frame,
        offset + header_len,
        offset + total_len,
        timestamp_seconds,
        source,
        destination,
    )
}

fn extract_ipv6_udp(
    frame: &[u8],
    offset: usize,
    timestamp_seconds: f64,
) -> Result<Option<UdpPacket<'_>>, PacketError> {
    let base = frame
        .get(offset..offset + 40)
        .ok_or(PacketError::Malformed)?;
    if base[0] >> 4 != 6 {
        return Err(PacketError::Malformed);
    }
    let payload_len = usize::from(u16::from_be_bytes([base[4], base[5]]));
    let end = offset
        .checked_add(40)
        .and_then(|value| value.checked_add(payload_len))
        .filter(|value| *value <= frame.len())
        .ok_or(PacketError::Malformed)?;
    let mut source_bytes = [0_u8; 16];
    source_bytes.copy_from_slice(&base[8..24]);
    let mut destination_bytes = [0_u8; 16];
    destination_bytes.copy_from_slice(&base[24..40]);
    let source = IpAddr::from(source_bytes);
    let destination = IpAddr::from(destination_bytes);
    let mut next = base[6];
    let mut cursor = offset + 40;
    for _ in 0..8 {
        match next {
            17 => {
                return extract_udp_header(
                    frame,
                    cursor,
                    end,
                    timestamp_seconds,
                    source,
                    destination,
                )
            }
            0 | 43 | 60 => {
                let extension = frame
                    .get(cursor..cursor + 2)
                    .ok_or(PacketError::Malformed)?;
                next = extension[0];
                let length = (usize::from(extension[1]) + 1) * 8;
                cursor = cursor
                    .checked_add(length)
                    .filter(|value| *value <= end)
                    .ok_or(PacketError::Malformed)?;
            }
            44 => return Err(PacketError::Fragmented),
            _ => return Ok(None),
        }
    }
    Err(PacketError::Malformed)
}

fn extract_udp_header(
    frame: &[u8],
    offset: usize,
    end: usize,
    timestamp_seconds: f64,
    source: IpAddr,
    destination: IpAddr,
) -> Result<Option<UdpPacket<'_>>, PacketError> {
    let header = frame
        .get(offset..offset + 8)
        .ok_or(PacketError::Malformed)?;
    let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
    if length < 8 || offset + length > end {
        return Err(PacketError::Malformed);
    }
    Ok(Some(UdpPacket {
        timestamp_seconds,
        source,
        destination,
        source_port: u16::from_be_bytes([header[0], header[1]]),
        destination_port: u16::from_be_bytes([header[2], header[3]]),
        payload: &frame[offset + 8..offset + length],
    }))
}

#[derive(Clone, Copy, Debug)]
enum RtpError {
    WrongVersion,
    Malformed,
}

fn parse_rtp(bytes: &[u8]) -> Result<RtpPacket<'_>, RtpError> {
    let header = bytes.get(..12).ok_or(RtpError::Malformed)?;
    if header[0] >> 6 != 2 {
        return Err(RtpError::WrongVersion);
    }
    let padding = header[0] & 0x20 != 0;
    let extension = header[0] & 0x10 != 0;
    let csrc_count = header[0] & 0x0f;
    let mut offset = 12 + usize::from(csrc_count) * 4;
    if offset > bytes.len() {
        return Err(RtpError::Malformed);
    }
    if extension {
        let extension_header = bytes.get(offset..offset + 4).ok_or(RtpError::Malformed)?;
        let words = usize::from(u16::from_be_bytes([
            extension_header[2],
            extension_header[3],
        ]));
        offset = offset
            .checked_add(4)
            .and_then(|value| value.checked_add(words * 4))
            .filter(|value| *value <= bytes.len())
            .ok_or(RtpError::Malformed)?;
    }
    let padding_len = if padding {
        usize::from(*bytes.last().ok_or(RtpError::Malformed)?)
    } else {
        0
    };
    if padding_len == 0 && padding || offset + padding_len > bytes.len() {
        return Err(RtpError::Malformed);
    }
    let payload_end = bytes.len() - padding_len;
    Ok(RtpPacket {
        marker: header[1] & 0x80 != 0,
        payload_type: header[1] & 0x7f,
        sequence: u16::from_be_bytes([header[2], header[3]]),
        timestamp: u32::from_be_bytes([header[4], header[5], header[6], header[7]]),
        ssrc: u32::from_be_bytes([header[8], header[9], header[10], header[11]]),
        csrc_count,
        payload: &bytes[offset..payload_end],
    })
}

fn payload_sample_count(stream: &StreamDescription, payload: &[u8]) -> Option<usize> {
    let bytes_per_sample = match stream.encoding.as_str() {
        "L16" => 2,
        "L24" => 3,
        "AM824" => 4,
        _ => return None,
    };
    let frame_bytes = bytes_per_sample * usize::from(stream.channels);
    if frame_bytes == 0 || !payload.len().is_multiple_of(frame_bytes) {
        return None;
    }
    Some(payload.len() / frame_bytes)
}

fn read_u16(bytes: &[u8], endian: Endian) -> u16 {
    match endian {
        Endian::Little => u16::from_le_bytes([bytes[0], bytes[1]]),
        Endian::Big => u16::from_be_bytes([bytes[0], bytes[1]]),
    }
}

fn read_u32(bytes: &[u8], endian: Endian) -> u32 {
    match endian {
        Endian::Little => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        Endian::Big => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    }
}

fn read_u64(bytes: &[u8], endian: Endian) -> u64 {
    let value: [u8; 8] = bytes.try_into().expect("eight-byte slice");
    match endian {
        Endian::Little => u64::from_le_bytes(value),
        Endian::Big => u64::from_be_bytes(value),
    }
}

fn finding(
    rule_id: &'static str,
    severity: Severity,
    passed: bool,
    message: impl Into<String>,
    observed: Option<Value>,
) -> RtpFinding {
    RtpFinding {
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

    fn append_u16(output: &mut Vec<u8>, value: u16, endian: Endian) {
        match endian {
            Endian::Little => output.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => output.extend_from_slice(&value.to_be_bytes()),
        }
    }

    fn append_u32(output: &mut Vec<u8>, value: u32, endian: Endian) {
        match endian {
            Endian::Little => output.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => output.extend_from_slice(&value.to_be_bytes()),
        }
    }

    fn append_pcapng_block(output: &mut Vec<u8>, block_type: u32, body: &[u8], endian: Endian) {
        let length = (12 + body.len()) as u32;
        append_u32(output, block_type, endian);
        append_u32(output, length, endian);
        output.extend_from_slice(body);
        append_u32(output, length, endian);
    }

    fn append_pcapng_section(output: &mut Vec<u8>, endian: Endian) {
        let mut body = Vec::new();
        append_u32(&mut body, 0x1a2b_3c4d, endian);
        append_u16(&mut body, 1, endian);
        append_u16(&mut body, 0, endian);
        match endian {
            Endian::Little => body.extend_from_slice(&u64::MAX.to_le_bytes()),
            Endian::Big => body.extend_from_slice(&u64::MAX.to_be_bytes()),
        }
        append_pcapng_block(output, 0x0a0d_0d0a, &body, endian);
    }

    fn append_pcapng_interface(
        output: &mut Vec<u8>,
        endian: Endian,
        link_type: u16,
        reserved: u16,
        snaplen: u32,
        options: &[u8],
    ) {
        let mut body = Vec::new();
        append_u16(&mut body, link_type, endian);
        append_u16(&mut body, reserved, endian);
        append_u32(&mut body, snaplen, endian);
        body.extend_from_slice(options);
        append_pcapng_block(output, 1, &body, endian);
    }

    fn append_empty_pcapng_packet(output: &mut Vec<u8>, endian: Endian, timestamp: u64) {
        let mut body = Vec::new();
        append_u32(&mut body, 0, endian);
        append_u32(&mut body, (timestamp >> 32) as u32, endian);
        append_u32(&mut body, timestamp as u32, endian);
        append_u32(&mut body, 0, endian);
        append_u32(&mut body, 0, endian);
        append_pcapng_block(output, 6, &body, endian);
    }

    #[test]
    fn parses_st2110_channel_order() {
        assert_eq!(
            channel_order_count("SMPTE2110.(51,ST,U02)", false),
            Some(10)
        );
        assert_eq!(channel_order_count("SMPTE2110.(AES3)", false), None);
        assert_eq!(channel_order_count("SMPTE2110.(AES3)", true), Some(2));
        assert_eq!(channel_order_count("SMPTE2110.(U00)", false), None);
    }

    #[test]
    fn maps_st2110_30_levels() {
        assert_eq!(st2110_30_level(48_000, 8, 1.0), Some("A"));
        assert_eq!(st2110_30_level(48_000, 64, 0.125), Some("C"));
        assert_eq!(st2110_30_level(96_000, 32, 0.125), Some("CX"));
        assert_eq!(st2110_30_level(48_000, 9, 1.0), None);
    }

    #[test]
    fn parses_multiple_pcapng_sections_and_timestamp_options() {
        let mut bytes = Vec::new();
        append_pcapng_section(&mut bytes, Endian::Little);
        append_pcapng_interface(&mut bytes, Endian::Little, 1, 42, 0, &[]);
        append_empty_pcapng_packet(&mut bytes, Endian::Little, 1_000_000);

        append_pcapng_section(&mut bytes, Endian::Big);
        let mut options = Vec::new();
        append_u16(&mut options, 9, Endian::Big);
        append_u16(&mut options, 1, Endian::Big);
        options.extend_from_slice(&[0x8a, 0, 0, 0]);
        append_u16(&mut options, 14, Endian::Big);
        append_u16(&mut options, 8, Endian::Big);
        options.extend_from_slice(&(-2_i64).to_be_bytes());
        append_u16(&mut options, 0, Endian::Big);
        append_u16(&mut options, 0, Endian::Big);
        append_pcapng_interface(&mut bytes, Endian::Big, 101, 0, 65_535, &options);
        append_empty_pcapng_packet(&mut bytes, Endian::Big, 1024);

        let mut timestamps = Vec::new();
        let metadata = walk_capture(&bytes, |record| {
            timestamps.push(record.timestamp_seconds);
            Ok(())
        })
        .unwrap();
        assert_eq!(metadata.format, "pcapng");
        assert_eq!(metadata.sections, 2);
        assert_eq!(metadata.interfaces, 2);
        assert_eq!(metadata.records, 2);
        assert_eq!(metadata.link_types, BTreeSet::from([1, 101]));
        assert_eq!(timestamps, vec![1.0, -1.0]);
    }

    #[test]
    fn rejects_pcapng_simple_packet_without_arrival_time() {
        let mut bytes = Vec::new();
        append_pcapng_section(&mut bytes, Endian::Little);
        append_pcapng_interface(&mut bytes, Endian::Little, 1, 0, 65_535, &[]);
        append_pcapng_block(&mut bytes, 3, &0_u32.to_le_bytes(), Endian::Little);
        let error = walk_capture(&bytes, |_| Ok(())).unwrap_err();
        assert!(error.contains("no timestamp"));
    }

    #[test]
    fn rejects_mismatched_pcapng_block_lengths() {
        let mut bytes = Vec::new();
        append_pcapng_section(&mut bytes, Endian::Little);
        let trailing_length = bytes.len() - 4;
        bytes[trailing_length] = 0;
        let error = walk_capture(&bytes, |_| Ok(())).unwrap_err();
        assert!(error.contains("lengths disagree"));
    }

    #[test]
    fn parses_rtp_extension_and_padding() {
        let mut packet = vec![
            0xb0, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0xbe, 0xde, 0, 1, 0, 0, 0, 0, 1, 2, 3, 2,
        ];
        let rtp = parse_rtp(&packet).unwrap();
        assert_eq!(rtp.payload, &[1, 2]);
        packet[0] = 0x70;
        assert!(matches!(parse_rtp(&packet), Err(RtpError::WrongVersion)));
    }
}
