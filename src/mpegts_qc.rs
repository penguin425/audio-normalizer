//! Bounded MPEG-2 Transport Stream structural, PSI, audio, and timing QC.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const TS_PACKET_BYTES: usize = 188;
const M2TS_PACKET_BYTES: usize = 192;
const MAX_PACKETS: u64 = 50_000_000;
const MAX_PSI_SECTION_BYTES: usize = 1_024;

pub(crate) fn looks_like_mpegts(header: &[u8]) -> bool {
    header.first() == Some(&0x47) || header.get(4) == Some(&0x47)
}

#[derive(Default)]
struct PsiAssembler {
    bytes: Vec<u8>,
    expected: Option<usize>,
}

impl PsiAssembler {
    fn push(&mut self, payload: &[u8], start: bool) -> Vec<Vec<u8>> {
        let mut completed = Vec::new();
        let mut input = payload;
        if start {
            let Some((&pointer, rest)) = input.split_first() else {
                return completed;
            };
            let pointer = usize::from(pointer);
            if pointer > rest.len() {
                self.bytes.clear();
                self.expected = None;
                return completed;
            }
            if !self.bytes.is_empty() {
                self.extend(&rest[..pointer], &mut completed);
            }
            self.bytes.clear();
            self.expected = None;
            input = &rest[pointer..];
        }
        self.extend(input, &mut completed);
        completed
    }

    fn extend(&mut self, mut input: &[u8], completed: &mut Vec<Vec<u8>>) {
        while !input.is_empty() {
            if self.bytes.is_empty() && input[0] == 0xff {
                break;
            }
            if self.bytes.len() < 3 {
                let needed = 3 - self.bytes.len();
                let take = needed.min(input.len());
                self.bytes.extend_from_slice(&input[..take]);
                input = &input[take..];
                if self.bytes.len() < 3 {
                    break;
                }
                let section_length =
                    (usize::from(self.bytes[1] & 0x0f) << 8) | usize::from(self.bytes[2]);
                self.expected = Some(3 + section_length);
                if self
                    .expected
                    .is_some_and(|size| !(4..=MAX_PSI_SECTION_BYTES).contains(&size))
                {
                    self.bytes.clear();
                    self.expected = None;
                    continue;
                }
            }
            let expected = self.expected.unwrap_or(3);
            let take = (expected - self.bytes.len()).min(input.len());
            self.bytes.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.bytes.len() == expected {
                completed.push(std::mem::take(&mut self.bytes));
                self.expected = None;
            }
        }
    }
}

#[derive(Clone)]
struct AudioStream {
    program: u16,
    pid: u16,
    stream_type: u8,
    codec: &'static str,
    language: Option<String>,
}

#[derive(Default)]
struct State {
    packets: u64,
    sync_errors: u64,
    header_errors: u64,
    transport_errors: u64,
    scrambled_packets: u64,
    continuity_errors: u64,
    duplicate_packets: u64,
    psi_crc_errors: u64,
    psi_syntax_errors: u64,
    pat_sections: u64,
    pmt_sections: u64,
    pes_headers: u64,
    pes_errors: u64,
    pts_values: u64,
    pts_discontinuities: u64,
    programs: BTreeMap<u16, u16>,
    pcr_pids: HashSet<u16>,
    audio_streams: BTreeMap<u16, AudioStream>,
    assemblers: HashMap<u16, PsiAssembler>,
    continuity: HashMap<u16, u8>,
    last_packet: HashMap<u16, [u8; TS_PACKET_BYTES]>,
    last_pts: HashMap<u16, u64>,
    first_pts: HashMap<u16, u64>,
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let (packet_size, sync_offset) = detect_layout(path, file_size)?;
    let complete_layout = file_size > 0 && file_size.is_multiple_of(packet_size as u64);
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {} MPEG-TS stream: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut state = State::default();
    let mut physical = vec![0_u8; packet_size];

    while state.packets < MAX_PACKETS {
        let mut read = 0;
        while read < packet_size {
            let count = reader.read(&mut physical[read..]).map_err(|error| {
                format!(
                    "read {} MPEG-TS packet {}: {error}",
                    path.display(),
                    state.packets
                )
            })?;
            if count == 0 {
                break;
            }
            read += count;
        }
        if read == 0 {
            break;
        }
        if read != packet_size {
            state.header_errors += 1;
            break;
        }
        let packet: &[u8; TS_PACKET_BYTES] = physical[sync_offset..sync_offset + TS_PACKET_BYTES]
            .try_into()
            .expect("detected packet layout always contains 188 TS bytes");
        parse_packet(packet, &mut state);
        state.packets += 1;
    }

    let limit_hit = state.packets == MAX_PACKETS && file_size > state.packets * packet_size as u64;
    let mut wrapper = vec![
        check(
            "FORGE-MPEGTS-PACKET-LAYOUT",
            complete_layout && state.packets > 0,
            "the file contains complete 188-byte TS or 192-byte M2TS packets",
            Some(json!({
                "packet_size": packet_size,
                "packets": state.packets,
                "file_bytes": file_size
            })),
        ),
        check(
            "FORGE-MPEGTS-SYNC",
            state.sync_errors == 0,
            "every transport packet has the 0x47 sync byte at the detected offset",
            Some(json!({"errors": state.sync_errors, "sync_offset": sync_offset})),
        ),
        check(
            "FORGE-MPEGTS-PACKET-LIMIT",
            !limit_hit,
            format!("packet count is within the safety limit {MAX_PACKETS}"),
            Some(json!(state.packets)),
        ),
    ];
    let mut bitstream = vec![
        check(
            "FORGE-MPEGTS-HEADER",
            state.header_errors == 0 && state.transport_errors == 0,
            "packet headers, adaptation fields, and transport-error indicators are valid",
            Some(json!({
                "header_errors": state.header_errors,
                "transport_error_packets": state.transport_errors,
                "scrambled_packets": state.scrambled_packets
            })),
        ),
        check(
            "FORGE-MPEGTS-CONTINUITY",
            state.continuity_errors == 0,
            "payload continuity counters advance without unexplained gaps",
            Some(json!({
                "errors": state.continuity_errors,
                "exact_duplicate_packets": state.duplicate_packets
            })),
        ),
        check(
            "FORGE-MPEGTS-PAT",
            state.pat_sections > 0 && !state.programs.is_empty(),
            "at least one CRC-valid PAT describes a non-network programme",
            Some(json!({"sections": state.pat_sections, "programs": state.programs})),
        ),
        check(
            "FORGE-MPEGTS-PMT",
            state.pmt_sections > 0,
            "at least one CRC-valid PMT was assembled",
            Some(json!({"sections": state.pmt_sections})),
        ),
        check(
            "FORGE-MPEGTS-PSI",
            state.psi_crc_errors == 0 && state.psi_syntax_errors == 0,
            "PAT and PMT sections have valid bounds, syntax, and MPEG-2 CRC-32 values",
            Some(json!({
                "crc_errors": state.psi_crc_errors,
                "syntax_errors": state.psi_syntax_errors
            })),
        ),
        check(
            "FORGE-MPEGTS-AUDIO-STREAM",
            !state.audio_streams.is_empty(),
            "the programme map declares at least one recognized audio elementary stream",
            Some(json!(state.audio_streams.len())),
        ),
        check(
            "FORGE-MPEGTS-PES",
            state.pes_errors == 0 && state.pes_headers > 0,
            "audio payload-unit starts contain bounded PES headers",
            Some(json!({"headers": state.pes_headers, "errors": state.pes_errors})),
        ),
        check(
            "FORGE-MPEGTS-PTS",
            state.pts_discontinuities == 0,
            "audio PTS values are syntactically valid and monotonic modulo wraparound",
            Some(json!({
                "values": state.pts_values,
                "discontinuities": state.pts_discontinuities
            })),
        ),
    ];
    let declared_pmt_pids: HashSet<_> = state.programs.values().copied().collect();
    let assembled_pmt_pids: HashSet<_> = state
        .audio_streams
        .values()
        .filter_map(|audio| state.programs.get(&audio.program).copied())
        .collect();
    let xcheck = vec![check(
        "FORGE-MPEGTS-PROGRAM-MAP",
        !declared_pmt_pids.is_empty() && assembled_pmt_pids.is_subset(&declared_pmt_pids),
        "audio streams belong to programmes declared by the PAT",
        Some(json!({
            "declared_pmt_pids": declared_pmt_pids,
            "audio_programs": assembled_pmt_pids
        })),
    )];
    let audio_streams = state
        .audio_streams
        .values()
        .map(|stream| {
            json!({
                "program": stream.program,
                "pid": stream.pid,
                "stream_type": stream.stream_type,
                "codec": stream.codec,
                "language": stream.language,
                "first_pts_90khz": state.first_pts.get(&stream.pid),
                "last_pts_90khz": state.last_pts.get(&stream.pid)
            })
        })
        .collect::<Vec<_>>();
    let durations = state
        .last_pts
        .iter()
        .filter_map(|(pid, last)| {
            state
                .first_pts
                .get(pid)
                .map(|first| pts_forward_delta(*first, *last) as f64 / 90_000.0)
        })
        .collect::<Vec<_>>();
    let properties = json!({
        "packet_size": packet_size,
        "sync_offset": sync_offset,
        "packets": state.packets,
        "programs": state.programs,
        "pcr_pids": state.pcr_pids,
        "audio_streams": audio_streams,
        "audio_pts_spans_seconds": durations,
        "scrambled_packets": state.scrambled_packets,
        "exact_duplicate_packets": state.duplicate_packets
    });
    wrapper.shrink_to_fit();
    bitstream.shrink_to_fit();
    Ok(finish_audit(
        path,
        if packet_size == M2TS_PACKET_BYTES {
            "m2ts"
        } else {
            "mpegts"
        },
        wrapper,
        bitstream,
        xcheck,
        properties,
    ))
}

fn detect_layout(path: &Path, file_size: u64) -> Result<(usize, usize), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut probe = [0_u8; M2TS_PACKET_BYTES * 3];
    let count = file
        .read(&mut probe)
        .map_err(|error| format!("read {} MPEG-TS probe: {error}", path.display()))?;
    let probe = &probe[..count];
    let ts_score = sync_score(probe, TS_PACKET_BYTES, 0);
    let m2ts_score = sync_score(probe, M2TS_PACKET_BYTES, 4);
    if m2ts_score > ts_score
        || (m2ts_score == ts_score && file_size.is_multiple_of(192) && m2ts_score > 0)
    {
        Ok((M2TS_PACKET_BYTES, 4))
    } else {
        Ok((TS_PACKET_BYTES, 0))
    }
}

fn sync_score(bytes: &[u8], packet_size: usize, offset: usize) -> usize {
    (offset..bytes.len())
        .step_by(packet_size)
        .take(3)
        .filter(|index| bytes.get(*index) == Some(&0x47))
        .count()
}

fn parse_packet(packet: &[u8; TS_PACKET_BYTES], state: &mut State) {
    if packet[0] != 0x47 {
        state.sync_errors += 1;
        return;
    }
    if packet[1] & 0x80 != 0 {
        state.transport_errors += 1;
    }
    let start = packet[1] & 0x40 != 0;
    let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
    let scrambling = packet[3] >> 6;
    let adaptation_control = (packet[3] >> 4) & 3;
    let counter = packet[3] & 0x0f;
    if scrambling != 0 {
        state.scrambled_packets += 1;
    }
    if adaptation_control == 0 {
        state.header_errors += 1;
        return;
    }
    let has_adaptation = adaptation_control & 2 != 0;
    let has_payload = adaptation_control & 1 != 0;
    let mut offset = 4_usize;
    let mut discontinuity = false;
    if has_adaptation {
        let length = usize::from(packet[offset]);
        offset += 1;
        if offset + length > packet.len() {
            state.header_errors += 1;
            return;
        }
        if length > 0 {
            discontinuity = packet[offset] & 0x80 != 0;
            if packet[offset] & 0x10 != 0 {
                if length < 7 {
                    state.header_errors += 1;
                    return;
                }
                state.pcr_pids.insert(pid);
            }
        }
        offset += length;
    }
    if has_payload {
        if let Some(previous) = state.continuity.get(&pid).copied() {
            let expected = (previous + 1) & 0x0f;
            if counter != expected && !discontinuity {
                if counter == previous
                    && state
                        .last_packet
                        .get(&pid)
                        .is_some_and(|last| last == packet)
                {
                    state.duplicate_packets += 1;
                } else {
                    state.continuity_errors += 1;
                }
            }
        }
        state.continuity.insert(pid, counter);
        state.last_packet.insert(pid, *packet);
    }
    if !has_payload || offset >= packet.len() || scrambling != 0 {
        return;
    }
    let payload = &packet[offset..];
    let is_psi = pid == 0 || state.programs.values().any(|value| *value == pid);
    if is_psi {
        let sections = state
            .assemblers
            .entry(pid)
            .or_default()
            .push(payload, start);
        for section in sections {
            if mpeg_crc32(&section) != 0 {
                state.psi_crc_errors += 1;
                continue;
            }
            match section.first().copied() {
                Some(0x00) if pid == 0 => parse_pat(&section, state),
                Some(0x02) => parse_pmt(&section, state),
                _ => state.psi_syntax_errors += 1,
            }
        }
    } else if start && state.audio_streams.contains_key(&pid) {
        parse_audio_pes(pid, payload, state);
    }
}

fn parse_pat(section: &[u8], state: &mut State) {
    if section.len() < 12 || section[1] & 0x80 == 0 || section[5] & 1 == 0 {
        state.psi_syntax_errors += 1;
        return;
    }
    let end = section.len() - 4;
    if !(end - 8).is_multiple_of(4) {
        state.psi_syntax_errors += 1;
        return;
    }
    for entry in section[8..end].chunks_exact(4) {
        let program = u16::from_be_bytes([entry[0], entry[1]]);
        let pid = (u16::from(entry[2] & 0x1f) << 8) | u16::from(entry[3]);
        if program != 0 {
            state.programs.insert(program, pid);
        }
    }
    state.pat_sections += 1;
}

fn parse_pmt(section: &[u8], state: &mut State) {
    if section.len() < 16 || section[1] & 0x80 == 0 || section[5] & 1 == 0 {
        state.psi_syntax_errors += 1;
        return;
    }
    let program = u16::from_be_bytes([section[3], section[4]]);
    if !state.programs.contains_key(&program) {
        state.psi_syntax_errors += 1;
    }
    let pcr_pid = (u16::from(section[8] & 0x1f) << 8) | u16::from(section[9]);
    state.pcr_pids.insert(pcr_pid);
    let program_info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut offset = 12 + program_info_length;
    let end = section.len() - 4;
    if offset > end {
        state.psi_syntax_errors += 1;
        return;
    }
    while offset < end {
        if end - offset < 5 {
            state.psi_syntax_errors += 1;
            return;
        }
        let stream_type = section[offset];
        let pid = (u16::from(section[offset + 1] & 0x1f) << 8) | u16::from(section[offset + 2]);
        let info_length =
            (usize::from(section[offset + 3] & 0x0f) << 8) | usize::from(section[offset + 4]);
        offset += 5;
        if offset + info_length > end {
            state.psi_syntax_errors += 1;
            return;
        }
        let descriptors = &section[offset..offset + info_length];
        if let Some(codec) = audio_codec(stream_type, descriptors) {
            state.audio_streams.insert(
                pid,
                AudioStream {
                    program,
                    pid,
                    stream_type,
                    codec,
                    language: iso639_language(descriptors),
                },
            );
        }
        offset += info_length;
    }
    state.pmt_sections += 1;
}

fn audio_codec(stream_type: u8, descriptors: &[u8]) -> Option<&'static str> {
    match stream_type {
        0x03 | 0x04 => Some("mpeg-audio"),
        0x0f => Some("aac-adts"),
        0x11 => Some("aac-latm"),
        0x81 => Some("ac-3"),
        0x87 => Some("e-ac-3"),
        0x06 if has_descriptor(descriptors, 0x6a) => Some("ac-3"),
        0x06 if has_descriptor(descriptors, 0x7a) => Some("e-ac-3"),
        0x06 if has_registration(descriptors, b"AC-3") => Some("ac-3"),
        0x06 if has_registration(descriptors, b"EAC3") => Some("e-ac-3"),
        _ => None,
    }
}

fn walk_descriptors(mut bytes: &[u8], mut visit: impl FnMut(u8, &[u8]) -> bool) -> bool {
    while bytes.len() >= 2 {
        let tag = bytes[0];
        let length = usize::from(bytes[1]);
        if bytes.len() < 2 + length {
            return false;
        }
        if visit(tag, &bytes[2..2 + length]) {
            return true;
        }
        bytes = &bytes[2 + length..];
    }
    false
}

fn has_descriptor(bytes: &[u8], wanted: u8) -> bool {
    walk_descriptors(bytes, |tag, _| tag == wanted)
}

fn has_registration(bytes: &[u8], wanted: &[u8; 4]) -> bool {
    walk_descriptors(bytes, |tag, body| tag == 0x05 && body.starts_with(wanted))
}

fn iso639_language(bytes: &[u8]) -> Option<String> {
    let mut language = None;
    walk_descriptors(bytes, |tag, body| {
        if tag == 0x0a && body.len() >= 3 {
            language = std::str::from_utf8(&body[..3]).ok().map(str::to_owned);
            true
        } else {
            false
        }
    });
    language
}

fn parse_audio_pes(pid: u16, payload: &[u8], state: &mut State) {
    if payload.len() < 9 || payload[..3] != [0, 0, 1] {
        state.pes_errors += 1;
        return;
    }
    let stream_id = payload[3];
    if !matches!(stream_id, 0xbd | 0xc0..=0xdf) {
        state.pes_errors += 1;
        return;
    }
    let header_length = usize::from(payload[8]);
    if 9 + header_length > payload.len() {
        state.pes_errors += 1;
        return;
    }
    state.pes_headers += 1;
    let pts_dts = payload[7] >> 6;
    if pts_dts & 2 != 0 {
        if header_length < 5 {
            state.pes_errors += 1;
            return;
        }
        let Some(pts) = decode_pts(&payload[9..14]) else {
            state.pes_errors += 1;
            return;
        };
        if let Some(previous) = state.last_pts.get(&pid).copied() {
            let delta = pts_forward_delta(previous, pts);
            if delta >= (1_u64 << 32) {
                state.pts_discontinuities += 1;
            }
        } else {
            state.first_pts.insert(pid, pts);
        }
        state.last_pts.insert(pid, pts);
        state.pts_values += 1;
    }
}

fn decode_pts(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 5 || bytes[0] & 1 == 0 || bytes[2] & 1 == 0 || bytes[4] & 1 == 0 {
        return None;
    }
    Some(
        (u64::from((bytes[0] >> 1) & 7) << 30)
            | (u64::from(bytes[1]) << 22)
            | (u64::from(bytes[2] >> 1) << 15)
            | (u64::from(bytes[3]) << 7)
            | u64::from(bytes[4] >> 1),
    )
}

fn pts_forward_delta(previous: u64, current: u64) -> u64 {
    current.wrapping_sub(previous) & ((1_u64 << 33) - 1)
}

fn mpeg_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_accepts_known_pat() {
        let pat = [
            0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xf0, 0x00, 0x2a, 0xb1,
            0x04, 0xb2,
        ];
        assert_eq!(mpeg_crc32(&pat), 0);
    }

    #[test]
    fn decodes_pts_and_wrap_delta() {
        assert_eq!(decode_pts(&[0x21, 0x00, 0x01, 0x00, 0x01]), Some(0));
        assert_eq!(pts_forward_delta((1 << 33) - 10, 5), 15);
    }
}
