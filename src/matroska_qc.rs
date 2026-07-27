//! Bounded RFC 9559 Matroska/WebM structural and audio-track audit.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use crc32fast::Hasher;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const CLUSTER: u32 = 0x1F43_B675;
const TRACKS: u32 = 0x1654_AE6B;
const CUES: u32 = 0x1C53_BB6B;
const TRACK_ENTRY: u32 = 0xAE;
const AUDIO: u32 = 0xE1;
const SEEK: u32 = 0x4DBB;
const CUE_POINT: u32 = 0xBB;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const BLOCK_GROUP: u32 = 0xA0;
const CRC32: u32 = 0xBF;

const MAX_DEPTH: usize = 16;
const MAX_ELEMENTS: usize = 250_000;
const MAX_CONTROL_BYTES: u64 = 1024 * 1024;
const MAX_TRACKS: usize = 1_024;
const MAX_BLOCK_HEADER_BYTES: u64 = 64 * 1024;

pub fn looks_like_matroska(header: &[u8]) -> bool {
    header.starts_with(&EBML.to_be_bytes())
}

#[derive(Default)]
struct State {
    elements: usize,
    doc_type: Option<String>,
    ebml_version: Option<u64>,
    ebml_read_version: Option<u64>,
    doc_type_version: Option<u64>,
    doc_type_read_version: Option<u64>,
    max_id_length: Option<u64>,
    max_size_length: Option<u64>,
    segment_data: Option<u64>,
    segment_unknown: bool,
    top_offsets: HashMap<u32, u64>,
    top_counts: HashMap<u32, usize>,
    tracks: Vec<Track>,
    clusters: Vec<ClusterState>,
    seeks: Vec<(Vec<u8>, u64)>,
    cues: Vec<(u64, u64, u64)>,
    crc_records: Vec<CrcRecord>,
    timestamp_scale: u64,
    duration_ticks: Option<f64>,
    block_count: u64,
    laced_block_count: u64,
    discard_padding_ns: i128,
    discard_padding_values: Vec<i64>,
    negative_block_timestamps: u64,
    vint_limit_errors: usize,
    top_id_width_errors: usize,
    scan_errors: Vec<String>,
}

#[derive(Default)]
struct Track {
    number: Option<u64>,
    uid: Option<u64>,
    track_type: Option<u64>,
    codec_id: Option<String>,
    codec_private: Option<Vec<u8>>,
    sample_rate: Option<f64>,
    output_sample_rate: Option<f64>,
    channels: Option<u64>,
    bit_depth: Option<u64>,
    default_duration_ns: Option<u64>,
    codec_delay_ns: Option<u64>,
    seek_preroll_ns: Option<u64>,
}

#[derive(Default)]
struct ClusterState {
    offset: u64,
    timestamp: Option<u64>,
    blocks: u64,
}

struct CrcRecord {
    expected: u32,
    parent_start: u64,
    parent_end: u64,
    element_start: u64,
    element_end: u64,
}

#[derive(Clone, Copy)]
struct Element {
    id: u32,
    id_len: usize,
    size_len: usize,
    start: u64,
    data_start: u64,
    end: u64,
    unknown_size: bool,
}

pub fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let mut state = State {
        timestamp_scale: 1_000_000,
        ..State::default()
    };

    scan_master(path, &mut file, 0, file_size, 0, 0, None, None, &mut state)?;

    wrapper.push(check(
        "FORGE-MATROSKA-EBML-HEADER",
        state.top_counts.get(&EBML) == Some(&1) && state.top_offsets.get(&EBML).copied() == Some(0),
        "exactly one EBML Header must start the document",
        state
            .top_counts
            .get(&EBML)
            .copied()
            .map(|value| json!(value)),
    ));
    wrapper.push(check(
        "FORGE-MATROSKA-SEGMENT-UNIQUE",
        state.top_counts.get(&SEGMENT) == Some(&1),
        "exactly one Segment is required",
        state
            .top_counts
            .get(&SEGMENT)
            .copied()
            .map(|value| json!(value)),
    ));
    wrapper.push(check(
        "FORGE-MATROSKA-DOC-TYPE",
        matches!(state.doc_type.as_deref(), Some("matroska" | "webm")),
        "DocType must be matroska or webm",
        state.doc_type.as_ref().map(|value| json!(value)),
    ));
    wrapper.push(check(
        "FORGE-MATROSKA-VERSION",
        state.ebml_version == Some(1)
            && state.ebml_read_version == Some(1)
            && state.doc_type_version.is_some_and(|version| version > 0)
            && state
                .doc_type_read_version
                .is_some_and(|version| version > 0)
            && state.doc_type_read_version <= state.doc_type_version,
        "EBML and DocType versions are present, supported, and internally consistent",
        Some(json!({
            "ebml_version": state.ebml_version,
            "ebml_read_version": state.ebml_read_version,
            "doc_type_version": state.doc_type_version,
            "doc_type_read_version": state.doc_type_read_version
        })),
    ));
    wrapper.push(check(
        "FORGE-MATROSKA-EBML-LIMITS",
        state.max_id_length.unwrap_or(4) <= 4
            && state.max_size_length.unwrap_or(8) <= 8
            && state.vint_limit_errors == 0,
        "declared and encountered EBML ID/size lengths are within Matroska limits",
        Some(json!({
            "max_id_length": state.max_id_length,
            "max_size_length": state.max_size_length,
            "violations": state.vint_limit_errors
        })),
    ));
    wrapper.push(check(
        "FORGE-MATROSKA-ELEMENT-LIMIT",
        state.elements < MAX_ELEMENTS,
        format!("element count is below the safety limit {MAX_ELEMENTS}"),
        Some(json!(state.elements)),
    ));
    wrapper.push(check(
        "FORGE-MATROSKA-BOUNDS",
        state.scan_errors.is_empty(),
        if state.scan_errors.is_empty() {
            "all element sizes, depths, and unknown-size uses are valid".into()
        } else {
            state.scan_errors.join("; ")
        },
        None,
    ));

    validate_top_level(&state, &mut bitstream);
    validate_tracks(&state, &mut bitstream, &mut xcheck);
    validate_clusters(&state, &mut bitstream);
    validate_seek_and_cues(&state, &mut xcheck);
    validate_crc(path, &mut file, &state, &mut wrapper)?;

    let format = if state.doc_type.as_deref() == Some("webm") {
        "webm"
    } else {
        "matroska"
    };
    let audio_tracks: Vec<_> = state
        .tracks
        .iter()
        .filter(|track| track.track_type == Some(2))
        .map(|track| {
            json!({
                "number": track.number,
                "uid": track.uid,
                "codec_id": track.codec_id,
                "sample_rate_hz": track.sample_rate,
                "output_sample_rate_hz": track.output_sample_rate,
                "channels": track.channels,
                "bit_depth": track.bit_depth,
                "codec_delay_ns": track.codec_delay_ns,
                "seek_preroll_ns": track.seek_preroll_ns
            })
        })
        .collect();
    Ok(finish_audit(
        path,
        format,
        wrapper,
        bitstream,
        xcheck,
        json!({
            "standard": "RFC 9559",
            "doc_type": state.doc_type,
            "file_size_bytes": file_size,
            "elements": state.elements,
            "segment_unknown_size": state.segment_unknown,
            "timestamp_scale_ns": state.timestamp_scale,
            "duration_ticks": state.duration_ticks,
            "tracks": state.tracks.len(),
            "audio_tracks": audio_tracks,
            "clusters": state.clusters.len(),
            "blocks": state.block_count,
            "laced_blocks": state.laced_block_count,
            "discard_padding_ns": state.discard_padding_ns,
            "negative_block_timestamps": state.negative_block_timestamps,
            "crc32_elements": state.crc_records.len(),
            "seek_entries": state.seeks.len(),
            "cue_positions": state.cues.len()
        }),
    ))
}

#[allow(clippy::too_many_arguments)]
fn scan_master(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    depth: usize,
    parent_id: u32,
    track_index: Option<usize>,
    cluster_index: Option<usize>,
    state: &mut State,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        state.scan_errors.push(format!(
            "nesting exceeds {MAX_DEPTH} levels at byte {start}"
        ));
        return Ok(());
    }
    let mut offset = start;
    while offset < end {
        if state.elements >= MAX_ELEMENTS {
            return Ok(());
        }
        let element = match read_element(file, offset, end) {
            Ok(element) => element,
            Err(error) => {
                state.scan_errors.push(error);
                return Ok(());
            }
        };
        state.elements += 1;
        offset = element.end;
        if element.id != EBML
            && (element.id_len as u64 > state.max_id_length.unwrap_or(4)
                || element.size_len as u64 > state.max_size_length.unwrap_or(8))
        {
            state.vint_limit_errors += 1;
        }

        if depth == 0 {
            *state.top_counts.entry(element.id).or_default() += 1;
            state.top_offsets.entry(element.id).or_insert(element.start);
            if element.id == SEGMENT {
                state.segment_data = Some(element.data_start);
                state.segment_unknown = element.unknown_size;
            }
        } else if parent_id == SEGMENT {
            *state.top_counts.entry(element.id).or_default() += 1;
            state.top_offsets.entry(element.id).or_insert(element.start);
            if !matches!(element.id, CRC32 | 0xEC) && element.id_len != 4 {
                state.top_id_width_errors += 1;
            }
        }

        if element.unknown_size && element.id != SEGMENT {
            state.scan_errors.push(format!(
                "unknown-sized element 0x{:x} is not allowed in a file audit",
                element.id
            ));
            return Ok(());
        }

        let mut next_track = track_index;
        let mut next_cluster = cluster_index;
        if element.id == TRACK_ENTRY {
            if state.tracks.len() == MAX_TRACKS {
                state
                    .scan_errors
                    .push(format!("track count exceeds {MAX_TRACKS}"));
                continue;
            }
            state.tracks.push(Track::default());
            next_track = Some(state.tracks.len() - 1);
        } else if element.id == CLUSTER {
            state.clusters.push(ClusterState {
                offset: element.start,
                ..ClusterState::default()
            });
            next_cluster = Some(state.clusters.len() - 1);
        }

        if is_master(element.id) {
            scan_master(
                path,
                file,
                element.data_start,
                element.end,
                depth + 1,
                element.id,
                next_track,
                next_cluster,
                state,
            )?;
            continue;
        }

        observe_leaf(
            path,
            file,
            element,
            parent_id,
            track_index,
            cluster_index,
            start,
            end,
            state,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn observe_leaf(
    path: &Path,
    file: &mut File,
    element: Element,
    parent_id: u32,
    track_index: Option<usize>,
    cluster_index: Option<usize>,
    parent_start: u64,
    parent_end: u64,
    state: &mut State,
) -> Result<(), String> {
    let size = element.end - element.data_start;
    match element.id {
        0x4282 => state.doc_type = read_string(path, file, element, size)?,
        0x4286 => state.ebml_version = read_uint(path, file, element, size)?,
        0x42F7 => state.ebml_read_version = read_uint(path, file, element, size)?,
        0x42F2 => state.max_id_length = read_uint(path, file, element, size)?,
        0x42F3 => state.max_size_length = read_uint(path, file, element, size)?,
        0x4287 => state.doc_type_version = read_uint(path, file, element, size)?,
        0x4285 => state.doc_type_read_version = read_uint(path, file, element, size)?,
        0x2AD7B1 => {
            state.timestamp_scale = read_uint(path, file, element, size)?.unwrap_or(1_000_000)
        }
        0x4489 => state.duration_ticks = read_float(path, file, element, size)?,
        0xD7 => set_track_uint(
            state,
            track_index,
            |track, value| track.number = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x73C5 => set_track_uint(
            state,
            track_index,
            |track, value| track.uid = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x83 => set_track_uint(
            state,
            track_index,
            |track, value| track.track_type = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x23E383 => set_track_uint(
            state,
            track_index,
            |track, value| track.default_duration_ns = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x56AA => set_track_uint(
            state,
            track_index,
            |track, value| track.codec_delay_ns = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x56BB => set_track_uint(
            state,
            track_index,
            |track, value| track.seek_preroll_ns = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x86 => {
            if let (Some(index), Some(value)) =
                (track_index, read_string(path, file, element, size)?)
            {
                state.tracks[index].codec_id = Some(value);
            }
        }
        0x63A2 => {
            if let Some(index) = track_index {
                state.tracks[index].codec_private = read_bytes(path, file, element, size)?;
            }
        }
        0xB5 => {
            if let (Some(index), Some(value)) =
                (track_index, read_float(path, file, element, size)?)
            {
                state.tracks[index].sample_rate = Some(value);
            }
        }
        0x78B5 => {
            if let (Some(index), Some(value)) =
                (track_index, read_float(path, file, element, size)?)
            {
                state.tracks[index].output_sample_rate = Some(value);
            }
        }
        0x9F => set_track_uint(
            state,
            track_index,
            |track, value| track.channels = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0x6264 => set_track_uint(
            state,
            track_index,
            |track, value| track.bit_depth = Some(value),
            path,
            file,
            element,
            size,
        )?,
        0xE7 => {
            if let (Some(index), Some(value)) =
                (cluster_index, read_uint(path, file, element, size)?)
            {
                state.clusters[index].timestamp = Some(value);
            }
        }
        0xA3 | 0xA1 => parse_block(path, file, element, cluster_index, state)?,
        0x75A2 => {
            if parent_id != BLOCK_GROUP {
                state
                    .scan_errors
                    .push("DiscardPadding occurs outside BlockGroup".into());
            }
            if let Some(value) = read_int(path, file, element, size)? {
                state.discard_padding_ns += i128::from(value);
                state.discard_padding_values.push(value);
            }
        }
        0x53AB => {
            if parent_id == SEEK {
                if let Some(bytes) = read_bytes(path, file, element, size)? {
                    state.seeks.push((bytes, u64::MAX));
                }
            }
        }
        0x53AC => {
            if parent_id == SEEK {
                if let (Some((_, position)), Some(value)) = (
                    state.seeks.last_mut(),
                    read_uint(path, file, element, size)?,
                ) {
                    *position = value;
                }
            }
        }
        0xB3 => {
            if parent_id == CUE_POINT {
                if let Some(value) = read_uint(path, file, element, size)? {
                    state.cues.push((value, 0, u64::MAX));
                }
            }
        }
        0xF7 => {
            if parent_id == CUE_TRACK_POSITIONS {
                if let (Some(cue), Some(value)) =
                    (state.cues.last_mut(), read_uint(path, file, element, size)?)
                {
                    cue.1 = value;
                }
            }
        }
        0xF1 => {
            if parent_id == CUE_TRACK_POSITIONS {
                if let (Some(cue), Some(value)) =
                    (state.cues.last_mut(), read_uint(path, file, element, size)?)
                {
                    cue.2 = value;
                }
            }
        }
        CRC32 => {
            if size != 4 {
                state
                    .scan_errors
                    .push("CRC-32 payload must be 4 bytes".into());
            } else if let Some(bytes) = read_bytes(path, file, element, size)? {
                state.crc_records.push(CrcRecord {
                    // RFC 8794 stores CRC-32 payloads least-significant byte first,
                    // unlike EBML unsigned integer elements.
                    expected: u32::from_le_bytes(bytes.try_into().expect("four-byte CRC-32")),
                    parent_start,
                    parent_end,
                    element_start: element.start,
                    element_end: element.end,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_element(file: &mut File, offset: u64, parent_end: u64) -> Result<Element, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek element at {offset}: {error}"))?;
    let (id, id_len, _) = read_vint(file, true)?;
    let (size, size_len, unknown_size) = read_vint(file, false)?;
    let data_start = offset
        .checked_add(id_len as u64 + size_len as u64)
        .ok_or_else(|| format!("element header overflows at byte {offset}"))?;
    let end = if unknown_size {
        parent_end
    } else {
        data_start
            .checked_add(size)
            .ok_or_else(|| format!("element size overflows at byte {offset}"))?
    };
    if end > parent_end {
        return Err(format!(
            "element 0x{id:x} ending at {end} exceeds parent ending at {parent_end}"
        ));
    }
    Ok(Element {
        id: u32::try_from(id).map_err(|_| "EBML ID exceeds 32 bits")?,
        id_len,
        size_len,
        start: offset,
        data_start,
        end,
        unknown_size,
    })
}

fn read_vint(file: &mut File, id: bool) -> Result<(u64, usize, bool), String> {
    let mut first = [0_u8; 1];
    file.read_exact(&mut first)
        .map_err(|error| format!("truncated EBML variable integer: {error}"))?;
    if first[0] == 0 {
        return Err("EBML variable integer begins with zero".into());
    }
    let length = first[0].leading_zeros() as usize + 1;
    let max = if id { 4 } else { 8 };
    if length > max {
        return Err(format!(
            "EBML variable integer length {length} exceeds {max}"
        ));
    }
    let mut value = if id {
        u64::from(first[0])
    } else {
        u64::from(first[0] & vint_value_mask(length))
    };
    for _ in 1..length {
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)
            .map_err(|error| format!("truncated EBML variable integer: {error}"))?;
        value = (value << 8) | u64::from(byte[0]);
    }
    let unknown = !id && value == ((1_u64 << (7 * length)) - 1);
    if id {
        let (minimum, maximum) = match length {
            1 => (0x81, 0xfe),
            2 => (0x407f, 0x7ffe),
            3 => (0x20_3fff, 0x3f_fffe),
            4 => (0x101f_ffff, 0x1fff_fffe),
            _ => unreachable!(),
        };
        if !(minimum..=maximum).contains(&value) {
            return Err("EBML ID uses reserved data bits or a non-minimal width".into());
        }
    }
    Ok((value, length, unknown))
}

fn is_master(id: u32) -> bool {
    matches!(
        id,
        EBML | SEGMENT
            | SEEK_HEAD
            | SEEK
            | INFO
            | CLUSTER
            | TRACKS
            | TRACK_ENTRY
            | AUDIO
            | 0xE0
            | 0x41E4
            | 0x6624
            | 0xE2
            | 0xE3
            | 0xE4
            | 0xE9
            | 0x6D80
            | 0x6240
            | 0x5034
            | 0x5035
            | 0x47E7
            | CUES
            | CUE_POINT
            | CUE_TRACK_POSITIONS
            | 0xDB
            | BLOCK_GROUP
            | 0x75A1
            | 0xA6
            | 0x1941_A469
            | 0x61A7
            | 0x1043_A770
            | 0x45B9
            | 0xB6
            | 0x8F
            | 0x6944
            | 0x6911
            | 0x1254_C367
            | 0x7373
            | 0x63C0
            | 0x67C8
    )
}

fn read_bytes(
    path: &Path,
    file: &mut File,
    element: Element,
    size: u64,
) -> Result<Option<Vec<u8>>, String> {
    if size > MAX_CONTROL_BYTES {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(element.data_start))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut bytes = vec![0; size as usize];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read {} element: {error}", path.display()))?;
    Ok(Some(bytes))
}

fn read_uint(
    path: &Path,
    file: &mut File,
    element: Element,
    size: u64,
) -> Result<Option<u64>, String> {
    if !(1..=8).contains(&size) {
        return Ok(None);
    }
    let Some(bytes) = read_bytes(path, file, element, size)? else {
        return Ok(None);
    };
    Ok(Some(
        bytes
            .into_iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(byte)),
    ))
}

fn read_int(
    path: &Path,
    file: &mut File,
    element: Element,
    size: u64,
) -> Result<Option<i64>, String> {
    let Some(value) = read_uint(path, file, element, size)? else {
        return Ok(None);
    };
    let shift = 64 - size as u32 * 8;
    Ok(Some(((value << shift) as i64) >> shift))
}

fn read_float(
    path: &Path,
    file: &mut File,
    element: Element,
    size: u64,
) -> Result<Option<f64>, String> {
    let Some(bytes) = read_bytes(path, file, element, size)? else {
        return Ok(None);
    };
    Ok(match bytes.as_slice() {
        [a, b, c, d] => Some(f32::from_be_bytes([*a, *b, *c, *d]) as f64),
        [a, b, c, d, e, f, g, h] => Some(f64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h])),
        _ => None,
    })
}

fn read_string(
    path: &Path,
    file: &mut File,
    element: Element,
    size: u64,
) -> Result<Option<String>, String> {
    let Some(bytes) = read_bytes(path, file, element, size)? else {
        return Ok(None);
    };
    Ok(String::from_utf8(bytes).ok())
}

#[allow(clippy::too_many_arguments)]
fn set_track_uint(
    state: &mut State,
    index: Option<usize>,
    setter: impl FnOnce(&mut Track, u64),
    path: &Path,
    file: &mut File,
    element: Element,
    size: u64,
) -> Result<(), String> {
    if let (Some(index), Some(value)) = (index, read_uint(path, file, element, size)?) {
        setter(&mut state.tracks[index], value);
    }
    Ok(())
}

fn parse_block(
    path: &Path,
    file: &mut File,
    element: Element,
    cluster_index: Option<usize>,
    state: &mut State,
) -> Result<(), String> {
    let size = element.end - element.data_start;
    let read_size = size.min(MAX_BLOCK_HEADER_BYTES);
    let Some(bytes) = read_bytes(path, file, element, read_size)? else {
        return Ok(());
    };
    let Ok((track, track_len)) = vint_from_slice(&bytes) else {
        state.scan_errors.push("invalid Block track number".into());
        return Ok(());
    };
    if bytes.len() < track_len + 3 {
        state.scan_errors.push("truncated Block header".into());
        return Ok(());
    }
    if !state.tracks.iter().any(|entry| entry.number == Some(track)) {
        state
            .scan_errors
            .push(format!("Block references unknown TrackNumber {track}"));
    }
    let relative_timestamp = i16::from_be_bytes([bytes[track_len], bytes[track_len + 1]]);
    if cluster_index
        .and_then(|index| state.clusters[index].timestamp)
        .is_some_and(|cluster| i128::from(cluster) + i128::from(relative_timestamp) < 0)
    {
        state.negative_block_timestamps += 1;
    }
    let flags = bytes[track_len + 2];
    let lacing = (flags >> 1) & 0x03;
    if lacing != 0 {
        state.laced_block_count += 1;
        if let Err(error) =
            validate_lacing(&bytes[track_len + 3..], size - track_len as u64 - 3, lacing)
        {
            state.scan_errors.push(error);
        }
    }
    state.block_count += 1;
    if let Some(index) = cluster_index {
        state.clusters[index].blocks += 1;
    }
    Ok(())
}

fn vint_from_slice(bytes: &[u8]) -> Result<(u64, usize), ()> {
    let first = *bytes.first().ok_or(())?;
    if first == 0 {
        return Err(());
    }
    let length = first.leading_zeros() as usize + 1;
    if length > 8 || bytes.len() < length {
        return Err(());
    }
    let mut value = u64::from(first & vint_value_mask(length));
    for byte in &bytes[1..length] {
        value = (value << 8) | u64::from(*byte);
    }
    Ok((value, length))
}

fn vint_value_mask(length: usize) -> u8 {
    if length == 8 {
        0
    } else {
        0xff_u8 >> length
    }
}

fn validate_lacing(bytes: &[u8], total: u64, mode: u8) -> Result<(), String> {
    let Some(&lace_count_minus_one) = bytes.first() else {
        return Err("laced Block omits frame count".into());
    };
    let frames = u64::from(lace_count_minus_one) + 1;
    let payload = total.checked_sub(1).ok_or("invalid laced Block size")?;
    if mode == 2 && payload % frames != 0 {
        return Err("fixed-size lacing payload is not divisible by frame count".into());
    }
    if mode == 1 {
        let mut cursor = 1;
        let mut declared = 0_u64;
        for _ in 1..frames {
            loop {
                let byte = *bytes.get(cursor).ok_or("truncated Xiph lace sizes")?;
                cursor += 1;
                declared = declared
                    .checked_add(u64::from(byte))
                    .ok_or("Xiph lace size overflow")?;
                if byte != 255 {
                    break;
                }
            }
        }
        if declared + cursor as u64 > total {
            return Err("Xiph lace sizes exceed Block payload".into());
        }
    } else if mode == 3 {
        let mut cursor = 1;
        let (_, length) = vint_from_slice(bytes.get(cursor..).ok_or("truncated EBML lace")?)
            .map_err(|_| "invalid EBML lace size")?;
        cursor += length;
        for _ in 2..frames {
            let (_, length) = vint_from_slice(bytes.get(cursor..).ok_or("truncated EBML lace")?)
                .map_err(|_| "invalid EBML lace delta")?;
            cursor += length;
        }
        if cursor as u64 > total {
            return Err("EBML lace header exceeds Block payload".into());
        }
    }
    Ok(())
}

fn validate_top_level(state: &State, checks: &mut Vec<AuditCheck>) {
    checks.push(check(
        "FORGE-MATROSKA-INFO-UNIQUE",
        state.top_counts.get(&INFO) == Some(&1),
        "Segment must contain exactly one Info element",
        state
            .top_counts
            .get(&INFO)
            .copied()
            .map(|value| json!(value)),
    ));
    checks.push(check(
        "FORGE-MATROSKA-TRACKS-UNIQUE",
        state.top_counts.get(&TRACKS) == Some(&1),
        "Segment must contain exactly one Tracks element",
        state
            .top_counts
            .get(&TRACKS)
            .copied()
            .map(|value| json!(value)),
    ));
    let first_cluster = state.top_offsets.get(&CLUSTER).copied();
    let available_before_cluster = |id| {
        first_cluster.is_none_or(|cluster| {
            state
                .top_offsets
                .get(&id)
                .is_some_and(|offset| *offset < cluster)
                || (state
                    .top_offsets
                    .get(&SEEK_HEAD)
                    .is_some_and(|offset| *offset < cluster)
                    && state.seeks.iter().any(|(seek_id, _)| {
                        seek_id
                            .iter()
                            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
                            == id
                    }))
        })
    };
    checks.push(check(
        "FORGE-MATROSKA-HEADER-ORDER",
        available_before_cluster(INFO) && available_before_cluster(TRACKS),
        "Info and Tracks must precede the first Cluster or be indexed by an earlier SeekHead",
        first_cluster.map(|value| json!(value)),
    ));
    checks.push(check(
        "FORGE-MATROSKA-TOP-LEVEL-ID",
        state.top_id_width_errors == 0,
        "all Matroska Top-Level Elements use four-octet EBML IDs",
        Some(json!(state.top_id_width_errors)),
    ));
    checks.push(check(
        "FORGE-MATROSKA-CLUSTER",
        !state.clusters.is_empty(),
        "a playable Matroska audio Segment must contain at least one Cluster",
        Some(json!(state.clusters.len())),
    ));
    checks.push(check(
        "FORGE-MATROSKA-RECOMMENDATION-CUES",
        true,
        if state.cues.is_empty() {
            "recommendation: Cues are absent"
        } else {
            "recommendation satisfied: Cues are present"
        },
        Some(json!(state.cues.len())),
    ));
}

fn validate_tracks(state: &State, checks: &mut Vec<AuditCheck>, xcheck: &mut Vec<AuditCheck>) {
    let mut numbers = HashSet::new();
    let mut uids = HashSet::new();
    let valid = state.tracks.iter().all(|track| {
        track
            .number
            .is_some_and(|value| value > 0 && numbers.insert(value))
            && track
                .uid
                .is_some_and(|value| value > 0 && uids.insert(value))
            && track.track_type.is_some()
            && track
                .codec_id
                .as_ref()
                .is_some_and(|value| !value.is_empty())
    });
    checks.push(check(
        "FORGE-MATROSKA-TRACK-IDENTITY",
        valid && !state.tracks.is_empty(),
        "TrackNumber and TrackUID must be positive and unique; TrackType and CodecID are required",
        Some(json!(state.tracks.len())),
    ));
    let audio_valid = state
        .tracks
        .iter()
        .filter(|track| track.track_type == Some(2))
        .all(|track| {
            track
                .sample_rate
                .is_some_and(|value| value.is_finite() && value > 0.0)
                && track.channels.is_some_and(|value| value > 0)
        });
    checks.push(check(
        "FORGE-MATROSKA-AUDIO-GEOMETRY",
        audio_valid,
        "audio tracks require a finite positive sampling frequency and channel count",
        None,
    ));
    for track in state
        .tracks
        .iter()
        .filter(|track| track.track_type == Some(2))
    {
        let private_valid = match track.codec_id.as_deref() {
            Some("A_OPUS") => track.codec_private.as_ref().is_some_and(|data| {
                crate::ogg_qc::validate_opus_identification(data)
                    .is_ok_and(|(channels, _)| track.channels == Some(u64::from(channels)))
            }),
            Some("A_VORBIS") => track
                .codec_private
                .as_ref()
                .is_some_and(|data| validate_vorbis_private(data, track)),
            Some("A_FLAC") => track.codec_private.as_ref().is_some_and(|data| {
                data.len() >= 42
                    && data.starts_with(b"fLaC")
                    && data[4] & 0x7f == 0
                    && u32::from_be_bytes([0, data[5], data[6], data[7]]) == 34
            }),
            Some("A_PCM/INT/LIT" | "A_PCM/INT/BIG" | "A_PCM/FLOAT/IEEE") => track
                .bit_depth
                .is_some_and(|value| value > 0 && value <= 64),
            Some("A_MPEG/L3") => true,
            Some(_) => true,
            None => false,
        };
        xcheck.push(check(
            "FORGE-MATROSKA-CODEC-PRIVATE",
            private_valid,
            format!(
                "{} codec-private data is structurally consistent",
                track.codec_id.as_deref().unwrap_or("unknown")
            ),
            track.codec_id.as_ref().map(|value| json!(value)),
        ));
        if track.codec_id.as_deref() == Some("A_OPUS") {
            let pre_skip = track
                .codec_private
                .as_ref()
                .and_then(|data| crate::ogg_qc::validate_opus_identification(data).ok())
                .map(|(_, pre_skip)| pre_skip);
            let expected_delay =
                pre_skip.map(|samples| u64::from(samples) * 1_000_000_000 / 48_000);
            xcheck.push(check(
                "FORGE-MATROSKA-OPUS-TIMING",
                track.codec_delay_ns == expected_delay
                    && track.seek_preroll_ns.is_some_and(|value| value >= 80_000_000),
                "Opus CodecDelay must match OpusHead pre-skip and SeekPreRoll must be at least 80 ms",
                Some(json!({
                    "pre_skip_samples": pre_skip,
                    "codec_delay_ns": track.codec_delay_ns,
                    "seek_preroll_ns": track.seek_preroll_ns
                })),
            ));
        }
    }
}

fn validate_vorbis_private(data: &[u8], track: &Track) -> bool {
    if data.first() != Some(&2) {
        return false;
    }
    let mut cursor = 1;
    let mut lengths = [0_usize; 2];
    for length in &mut lengths {
        loop {
            let Some(&byte) = data.get(cursor) else {
                return false;
            };
            cursor += 1;
            *length = match length.checked_add(usize::from(byte)) {
                Some(value) => value,
                None => return false,
            };
            if byte != 255 {
                break;
            }
        }
    }
    let Some(first_end) = cursor.checked_add(lengths[0]) else {
        return false;
    };
    let Some(second_end) = first_end.checked_add(lengths[1]) else {
        return false;
    };
    let (Some(identification), Some(comment), Some(setup)) = (
        data.get(cursor..first_end),
        data.get(first_end..second_end),
        data.get(second_end..),
    ) else {
        return false;
    };
    crate::ogg_qc::validate_vorbis_identification(identification).is_ok_and(
        |(channels, sample_rate)| {
            track.channels == Some(u64::from(channels))
                && track.sample_rate == Some(f64::from(sample_rate))
                && comment.starts_with(b"\x03vorbis")
                && setup.starts_with(b"\x05vorbis")
        },
    )
}

fn validate_clusters(state: &State, checks: &mut Vec<AuditCheck>) {
    checks.push(check(
        "FORGE-MATROSKA-CLUSTER-TIMESTAMP",
        state
            .clusters
            .iter()
            .all(|cluster| cluster.timestamp.is_some()),
        "every Cluster must contain a Timestamp",
        Some(json!(state.clusters.len())),
    ));
    let monotonic = state
        .clusters
        .windows(2)
        .all(|pair| pair[0].timestamp <= pair[1].timestamp);
    checks.push(check(
        "FORGE-MATROSKA-BLOCK-TIMELINE",
        monotonic,
        "Cluster timestamps are monotonic and Block track references/lacing are valid",
        Some(json!({"blocks": state.block_count, "laced": state.laced_block_count})),
    ));
    let duration_ns = state
        .duration_ticks
        .filter(|duration| duration.is_finite() && *duration >= 0.0)
        .map(|duration| duration * state.timestamp_scale as f64);
    let opus = state
        .tracks
        .iter()
        .any(|track| track.codec_id.as_deref() == Some("A_OPUS"));
    let padding_valid = state.discard_padding_values.iter().all(|padding| {
        let magnitude = padding.unsigned_abs() as f64;
        duration_ns.is_none_or(|duration| magnitude <= duration)
            && (!opus || magnitude <= 120_000_000.0)
    });
    checks.push(check(
        "FORGE-MATROSKA-DURATION",
        state.timestamp_scale > 0
            && state
                .duration_ticks
                .is_none_or(|duration| duration.is_finite() && duration >= 0.0)
            && padding_valid,
        "TimestampScale/Duration are valid and DiscardPadding fits the encoded duration",
        Some(json!({
            "duration_ticks": state.duration_ticks,
            "timestamp_scale_ns": state.timestamp_scale,
            "discard_padding_ns": state.discard_padding_values
        })),
    ));
    checks.push(check(
        "FORGE-MATROSKA-RECOMMENDATION-NONNEGATIVE-TIMESTAMP",
        true,
        if state.negative_block_timestamps == 0 {
            "recommendation satisfied: Block presentation timestamps are nonnegative"
        } else {
            "recommendation: Block presentation timestamps should not be negative"
        },
        Some(json!(state.negative_block_timestamps)),
    ));
}

fn validate_seek_and_cues(state: &State, checks: &mut Vec<AuditCheck>) {
    let segment_data = state.segment_data.unwrap_or(0);
    let seek_valid = state.seeks.iter().all(|(id, position)| {
        if *position == u64::MAX || id.is_empty() || id.len() > 4 {
            return false;
        }
        let target_id = id
            .iter()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte));
        state
            .top_offsets
            .get(&target_id)
            .is_some_and(|offset| *offset == segment_data.saturating_add(*position))
    });
    checks.push(check(
        "FORGE-MATROSKA-SEEK-TARGET",
        seek_valid,
        "SeekID and SeekPosition pairs resolve to matching Segment children",
        Some(json!(state.seeks.len())),
    ));
    let cue_valid = state.cues.windows(2).all(|pair| pair[0].0 <= pair[1].0)
        && state.cues.iter().all(|(_, track, position)| {
            *position != u64::MAX
                && state
                    .tracks
                    .iter()
                    .any(|entry| entry.number == Some(*track))
                && state
                    .clusters
                    .iter()
                    .any(|cluster| cluster.offset == segment_data.saturating_add(*position))
        });
    checks.push(check(
        "FORGE-MATROSKA-CUE-TARGET",
        cue_valid,
        "Cue times are monotonic and CueTrack/CueClusterPosition resolve",
        Some(json!(state.cues.len())),
    ));
}

fn validate_crc(
    path: &Path,
    file: &mut File,
    state: &State,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let mut valid = true;
    for record in &state.crc_records {
        let mut hasher = Hasher::new();
        hash_range(
            path,
            file,
            record.parent_start,
            record.element_start,
            &mut hasher,
        )?;
        hash_range(
            path,
            file,
            record.element_end,
            record.parent_end,
            &mut hasher,
        )?;
        valid &= hasher.finalize() == record.expected;
    }
    checks.push(check(
        "FORGE-MATROSKA-CRC32",
        valid,
        "all CRC-32 elements match their parent payload without buffering media",
        Some(json!(state.crc_records.len())),
    ));
    let misplaced = state
        .crc_records
        .iter()
        .filter(|record| record.element_start != record.parent_start)
        .count();
    checks.push(check(
        "FORGE-MATROSKA-CRC-FIRST",
        misplaced == 0,
        if misplaced == 0 {
            "CRC-32 is the first child of each parent"
        } else {
            "CRC-32 must be the first child of its parent"
        },
        Some(json!(misplaced)),
    ));
    Ok(())
}

fn hash_range(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    hasher: &mut Hasher,
) -> Result<(), String> {
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek {} for CRC: {error}", path.display()))?;
    let mut remaining = end.saturating_sub(start);
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..count])
            .map_err(|error| format!("read {} for CRC: {error}", path.display()))?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn vint_size(value: usize) -> Vec<u8> {
        assert!(value < 127);
        vec![0x80 | value as u8]
    }

    fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut output = id.to_vec();
        output.extend(vint_size(body.len()));
        output.extend(body);
        output
    }

    fn minimal_matroska(block: &[u8], info_extra: &[u8]) -> Vec<u8> {
        let mut ebml = Vec::new();
        ebml.extend(element(&[0x42, 0x86], &[1]));
        ebml.extend(element(&[0x42, 0xf7], &[1]));
        ebml.extend(element(&[0x42, 0x82], b"matroska"));
        ebml.extend(element(&[0x42, 0xf2], &[4]));
        ebml.extend(element(&[0x42, 0xf3], &[8]));
        ebml.extend(element(&[0x42, 0x87], &[4]));
        ebml.extend(element(&[0x42, 0x85], &[2]));

        let mut track = Vec::new();
        track.extend(element(&[0xd7], &[1]));
        track.extend(element(&[0x73, 0xc5], &[1]));
        track.extend(element(&[0x83], &[2]));
        track.extend(element(&[0x86], b"A_PCM/INT/LIT"));
        let mut audio = Vec::new();
        audio.extend(element(&[0xb5], &48_000_f64.to_be_bytes()));
        audio.extend(element(&[0x9f], &[1]));
        audio.extend(element(&[0x62, 0x64], &[16]));
        track.extend(element(&[0xe1], &audio));
        let tracks = element(&[0x16, 0x54, 0xae, 0x6b], &element(&[0xae], &track));
        let mut info_body = info_extra.to_vec();
        info_body.extend(element(&[0x2a, 0xd7, 0xb1], &[0x0f, 0x42, 0x40]));
        let info = element(&[0x15, 0x49, 0xa9, 0x66], &info_body);
        let mut cluster = element(&[0xe7], &[0]);
        cluster.extend(element(&[0xa3], block));
        let cluster = element(&[0x1f, 0x43, 0xb6, 0x75], &cluster);
        let mut segment = Vec::new();
        segment.extend(info);
        segment.extend(tracks);
        segment.extend(cluster);

        let mut output = element(&[0x1a, 0x45, 0xdf, 0xa3], &ebml);
        output.extend(element(&[0x18, 0x53, 0x80, 0x67], &segment));
        output
    }

    #[test]
    fn rejects_truncated_and_oversized_elements_without_panicking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad.mkv");
        std::fs::write(&path, [0x1a, 0x45, 0xdf, 0xa3, 0xff]).unwrap();
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(!result.passed);
        assert_eq!(result.format, "matroska");
    }

    #[test]
    fn accepts_minimal_audio_matroska_structure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("minimal.mka");
        let mut file = File::create(&path).unwrap();
        file.write_all(&minimal_matroska(&[0x81, 0, 0, 0x80, 0, 0], &[]))
            .unwrap();
        drop(file);
        let result = crate::container_qc::audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
    }

    #[test]
    fn rejects_bad_crc_and_malformed_lacing() {
        let directory = tempfile::tempdir().unwrap();
        let bad_crc = directory.path().join("bad-crc.mka");
        std::fs::write(
            &bad_crc,
            minimal_matroska(
                &[0x81, 0, 0, 0x80, 0, 0],
                &element(&[CRC32 as u8], &[0, 0, 0, 0]),
            ),
        )
        .unwrap();
        let audit = crate::container_qc::audit(&bad_crc).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|check| check.rule_id == "FORGE-MATROSKA-CRC32" && !check.passed));

        let bad_lacing = directory.path().join("bad-lacing.mka");
        std::fs::write(
            &bad_lacing,
            minimal_matroska(&[0x81, 0, 0, 0x84, 1, 0], &[]),
        )
        .unwrap();
        let audit = crate::container_qc::audit(&bad_lacing).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|check| check.rule_id == "FORGE-MATROSKA-BOUNDS" && !check.passed));
    }
}
