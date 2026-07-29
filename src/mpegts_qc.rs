//! Bounded MPEG-2 Transport Stream structural, PSI, audio, and timing QC.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const TS_PACKET_BYTES: usize = 188;
const M2TS_PACKET_BYTES: usize = 192;
const MAX_PACKETS: u64 = 50_000_000;
const MAX_PSI_SECTION_BYTES: usize = 1_024;
const MAX_METADATA_PES_BYTES: usize = 16 * 1024 * 1024;
const MAX_METADATA_PES_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_METADATA_ERRORS: usize = 256;
const MAX_TIMED_ID3_EVENTS: usize = 4_096;
const MAX_TIMED_ID3_STORED_BYTES: usize = 64 * 1024 * 1024;

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

#[derive(Clone, Serialize)]
struct TimedId3Descriptor {
    metadata_service_id: u8,
}

#[derive(Clone)]
struct MetadataStream {
    program: u16,
    pid: u16,
    stream_type: u8,
    id3_descriptor: Option<TimedId3Descriptor>,
    descriptor_error: Option<String>,
}

#[derive(Serialize)]
struct TimedId3 {
    program: u16,
    pid: u16,
    pts_90khz: Option<u64>,
    tag: crate::id3_qc::Id3Tag,
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
    metadata_streams: BTreeMap<u16, MetadataStream>,
    metadata_pes: HashMap<u16, Vec<u8>>,
    metadata_pes_bytes: usize,
    timed_id3: Vec<TimedId3>,
    timed_id3_bytes: usize,
    timed_id3_limit_hit: bool,
    metadata_errors: Vec<String>,
    metadata_error_count: u64,
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
    flush_metadata_pes(&mut state);

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
    if !state.metadata_streams.is_empty() {
        let descriptor_errors = state
            .metadata_streams
            .values()
            .filter_map(|stream| {
                stream
                    .descriptor_error
                    .as_ref()
                    .map(|error| format!("PID {}: {error}", stream.pid))
            })
            .collect::<Vec<_>>();
        bitstream.push(check(
            "FORGE-MPEGTS-TIMED-ID3",
            descriptor_errors.is_empty() && state.metadata_error_count == 0,
            "stream_type 0x15 timed-ID3 streams have canonical ID3 metadata descriptors, bounded PES headers, and complete ID3v2 tags",
            Some(json!({
                "streams": state.metadata_streams.len(),
                "tags": state.timed_id3.len(),
                "descriptor_errors": descriptor_errors,
                "payload_error_count": state.metadata_error_count,
                "payload_errors": state.metadata_errors,
                "payload_errors_truncated":
                    state.metadata_error_count > state.metadata_errors.len() as u64,
                "evidence_limit_hit": state.timed_id3_limit_hit
            })),
        ));
    }
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
        "timed_id3_streams": state.metadata_streams.values().map(|stream| json!({
            "program": stream.program,
            "pid": stream.pid,
            "stream_type": stream.stream_type,
            "id3_descriptor": stream.id3_descriptor,
            "descriptor_error": stream.descriptor_error
        })).collect::<Vec<_>>(),
        "timed_id3": state.timed_id3.iter().map(|item| {
            serde_json::to_value(item).unwrap_or(Value::Null)
        }).collect::<Vec<_>>(),
        "timed_id3_errors": state.metadata_errors,
        "timed_id3_error_count": state.metadata_error_count,
        "timed_id3_stored_bytes": state.timed_id3_bytes,
        "timed_id3_evidence_limit_hit": state.timed_id3_limit_hit,
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
    } else if state.metadata_streams.contains_key(&pid) {
        push_metadata_pes(pid, payload, start, state);
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
        } else if stream_type == 0x15 {
            let (id3_descriptor, descriptor_error) = match timed_id3_descriptor(descriptors) {
                Ok(descriptor) => (Some(descriptor), None),
                Err(error) => (None, Some(error)),
            };
            state.metadata_streams.insert(
                pid,
                MetadataStream {
                    program,
                    pid,
                    stream_type,
                    id3_descriptor,
                    descriptor_error,
                },
            );
        }
        offset += info_length;
    }
    state.pmt_sections += 1;
}

fn push_metadata_pes(pid: u16, payload: &[u8], start: bool, state: &mut State) {
    if start {
        if let Some(previous) = state.metadata_pes.remove(&pid) {
            state.metadata_pes_bytes = state.metadata_pes_bytes.saturating_sub(previous.len());
            parse_metadata_pes(pid, &previous, state);
        }
        state.metadata_pes.insert(pid, Vec::new());
    }
    let Some(current_length) = state.metadata_pes.get(&pid).map(Vec::len) else {
        return;
    };
    if current_length.saturating_add(payload.len()) > MAX_METADATA_PES_BYTES
        || state.metadata_pes_bytes.saturating_add(payload.len()) > MAX_METADATA_PES_TOTAL_BYTES
    {
        if let Some(discarded) = state.metadata_pes.remove(&pid) {
            state.metadata_pes_bytes = state.metadata_pes_bytes.saturating_sub(discarded.len());
        }
        record_metadata_error(
            state,
            format!(
            "PID {pid} timed metadata PES exceeds an individual or aggregate assembly safety limit"
        ),
        );
        return;
    }
    let pes = state
        .metadata_pes
        .get_mut(&pid)
        .expect("metadata PES exists after its length was read");
    pes.extend_from_slice(payload);
    state.metadata_pes_bytes += payload.len();
    let expected = pes
        .get(4..6)
        .map(|bytes| usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
        .filter(|length| *length > 0)
        .map(|length| length + 6);
    if expected.is_some_and(|length| pes.len() >= length) {
        let complete = state
            .metadata_pes
            .remove(&pid)
            .expect("metadata PES exists while being assembled");
        state.metadata_pes_bytes = state.metadata_pes_bytes.saturating_sub(complete.len());
        parse_metadata_pes(
            pid,
            &complete[..expected.expect("checked expected length")],
            state,
        );
    }
}

fn flush_metadata_pes(state: &mut State) {
    let pending = std::mem::take(&mut state.metadata_pes);
    state.metadata_pes_bytes = 0;
    for (pid, pes) in pending {
        parse_metadata_pes(pid, &pes, state);
    }
}

fn parse_metadata_pes(pid: u16, pes: &[u8], state: &mut State) {
    let result = (|| -> Result<TimedId3, String> {
        if pes.len() < 9 || pes[..3] != [0, 0, 1] || pes[3] != 0xbd {
            return Err("invalid private_stream_1 PES header".into());
        }
        let header_length = usize::from(pes[8]);
        let data_start = 9_usize
            .checked_add(header_length)
            .filter(|offset| *offset <= pes.len())
            .ok_or_else(|| "timed metadata PES header exceeds the packet".to_string())?;
        let pts = if pes[7] >> 6 & 2 != 0 {
            if header_length < 5 {
                return Err("timed metadata PES declares a truncated PTS".into());
            }
            Some(
                decode_pts(&pes[9..14])
                    .ok_or_else(|| "timed metadata PES has an invalid PTS".to_string())?,
            )
        } else {
            None
        };
        let (tag, consumed) = crate::id3_qc::parse_prefix(&pes[data_start..], false)?;
        if pes[data_start + consumed..]
            .iter()
            .any(|byte| !matches!(*byte, 0x00 | 0xff))
        {
            return Err("timed metadata PES has non-padding bytes after the ID3 tag".into());
        }
        let stream = state
            .metadata_streams
            .get(&pid)
            .ok_or_else(|| "timed metadata PID is absent from the PMT".to_string())?;
        Ok(TimedId3 {
            program: stream.program,
            pid,
            pts_90khz: pts,
            tag,
        })
    })();
    match result {
        Ok(tag) => {
            let size = tag.tag.size_bytes;
            if !state.timed_id3_limit_hit
                && state.timed_id3.len() < MAX_TIMED_ID3_EVENTS
                && state.timed_id3_bytes.saturating_add(size) <= MAX_TIMED_ID3_STORED_BYTES
            {
                state.timed_id3_bytes += size;
                state.timed_id3.push(tag);
            } else if !state.timed_id3_limit_hit {
                state.timed_id3_limit_hit = true;
                record_metadata_error(
                    state,
                    "timed ID3 evidence exceeds the event-count or stored-byte safety limit".into(),
                );
            }
        }
        Err(error) => record_metadata_error(state, format!("PID {pid} timed ID3: {error}")),
    }
}

fn record_metadata_error(state: &mut State, error: String) {
    state.metadata_error_count = state.metadata_error_count.saturating_add(1);
    if state.metadata_errors.len() < MAX_METADATA_ERRORS {
        state.metadata_errors.push(error);
    }
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

fn timed_id3_descriptor(bytes: &[u8]) -> Result<TimedId3Descriptor, String> {
    let mut input = bytes;
    let mut found = None;
    while !input.is_empty() {
        if input.len() < 2 {
            return Err("PMT descriptor loop has a truncated header".into());
        }
        let tag = input[0];
        let length = usize::from(input[1]);
        if input.len() < 2 + length {
            return Err("PMT descriptor loop has a truncated descriptor".into());
        }
        let body = &input[2..2 + length];
        if tag == 0x26 {
            if found.is_some() {
                return Err("PMT has more than one metadata descriptor".into());
            }
            if body.len() != 13 {
                return Err(format!(
                    "ID3 metadata descriptor has length {}, expected 13",
                    body.len()
                ));
            }
            if body[..2] != [0xff, 0xff] || body[2..6] != *b"ID3 " {
                return Err(
                    "metadata descriptor does not identify the ID3 metadata application".into(),
                );
            }
            if body[6] != 0xff || body[7..11] != *b"ID3 " {
                return Err("metadata descriptor does not identify the ID3 metadata format".into());
            }
            if body[12] != 0x0f {
                return Err(format!(
                    "ID3 metadata descriptor has unsupported decoder/configuration flags 0x{:02x}",
                    body[12]
                ));
            }
            found = Some(TimedId3Descriptor {
                metadata_service_id: body[11],
            });
        }
        input = &input[2 + length..];
    }
    found.ok_or_else(|| "stream_type 0x15 is missing the ID3 metadata descriptor".into())
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

    fn timed_id3_tag() -> Vec<u8> {
        let payload = [b"track".as_slice(), &[0, 1, 0xfe, 0x00, 0]].concat();
        let frame = [
            b"RVA2".as_slice(),
            &[0, 0, 0, payload.len() as u8, 0, 0],
            &payload,
        ]
        .concat();
        [
            b"ID3\x04\x00\x00".as_slice(),
            &[0, 0, 0, frame.len() as u8],
            &frame,
        ]
        .concat()
    }

    fn timed_id3_descriptor_bytes() -> Vec<u8> {
        [
            &[0x26, 13, 0xff, 0xff][..],
            b"ID3 ",
            &[0xff],
            b"ID3 ",
            &[0, 0x0f],
        ]
        .concat()
    }

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

    #[test]
    fn identifies_canonical_timed_id3_metadata_descriptor() {
        let descriptor = timed_id3_descriptor(&timed_id3_descriptor_bytes()).unwrap();
        assert_eq!(descriptor.metadata_service_id, 0);
    }

    #[test]
    fn rejects_missing_or_malformed_timed_id3_metadata_descriptor() {
        assert!(timed_id3_descriptor(&[]).is_err());

        let mut wrong_format = timed_id3_descriptor_bytes();
        wrong_format[11] = b'X';
        assert!(timed_id3_descriptor(&wrong_format).is_err());

        assert!(timed_id3_descriptor(&[0x26, 13, 0xff]).is_err());
    }

    #[test]
    fn bounds_aggregate_timed_metadata_pes_assembly() {
        let mut state = State {
            metadata_pes_bytes: MAX_METADATA_PES_TOTAL_BYTES,
            ..State::default()
        };
        state.metadata_pes.insert(0x101, vec![0]);
        push_metadata_pes(0x101, &[1], false, &mut state);
        assert!(!state.metadata_pes.contains_key(&0x101));
        assert_eq!(state.metadata_error_count, 1);
    }

    #[test]
    fn parses_timed_id3_private_pes_and_rva2() {
        let tag = timed_id3_tag();
        let packet_length = 3 + 5 + tag.len();
        let pes = [
            &[
                0,
                0,
                1,
                0xbd,
                (packet_length >> 8) as u8,
                packet_length as u8,
            ][..],
            &[0x80, 0x80, 5][..],
            &[0x21, 0, 1, 0, 1][..],
            tag.as_slice(),
        ]
        .concat();
        let mut state = State::default();
        state.metadata_streams.insert(
            0x101,
            MetadataStream {
                program: 1,
                pid: 0x101,
                stream_type: 0x15,
                id3_descriptor: Some(timed_id3_descriptor(&timed_id3_descriptor_bytes()).unwrap()),
                descriptor_error: None,
            },
        );
        parse_metadata_pes(0x101, &pes, &mut state);
        assert!(
            state.metadata_errors.is_empty(),
            "{:?}",
            state.metadata_errors
        );
        assert_eq!(state.timed_id3.len(), 1);
        assert_eq!(state.timed_id3[0].pts_90khz, Some(0));
        assert_eq!(state.timed_id3[0].tag.relative_volume_adjustments.len(), 1);
    }
}
