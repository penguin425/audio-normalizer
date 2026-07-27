//! Bounded structural QC for standalone AOMedia IAMF IA Sequences.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_OBU_BYTES: u64 = 1 << 21;
const MAX_OBU_PAYLOAD_BYTES: u64 = MAX_OBU_BYTES - 4;
const MAX_OBUS: u64 = 10_000_000;
const MAX_LEB_BYTES: usize = 8;

const CODEC_CONFIG: u8 = 0;
const AUDIO_ELEMENT: u8 = 1;
const MIX_PRESENTATION: u8 = 2;
const PARAMETER_BLOCK: u8 = 3;
const TEMPORAL_DELIMITER: u8 = 4;
const AUDIO_FRAME: u8 = 5;
const SEQUENCE_HEADER: u8 = 31;

#[derive(Debug, Default)]
struct State {
    counts: [u64; 32],
    obus: u64,
    bytes: u64,
    max_obu_bytes: u64,
    bounds_valid: bool,
    headers_valid: bool,
    order_valid: bool,
    profiles_valid: bool,
    sequence_headers_valid: bool,
    first_profile: Option<(u8, u8)>,
    sequence_count: u64,
    descriptor_sets: u64,
    descriptor_phase: bool,
    descriptor_rank: u8,
    descriptor_redundant: bool,
    current_codec_configs: u64,
    current_audio_elements: u64,
    current_mix_presentations: u64,
    saw_data: bool,
    saw_audio_frame: bool,
    saw_audio_in_temporal_unit: bool,
    uses_temporal_delimiters: bool,
    extension_headers: u64,
    trimmed_frames: u64,
    trim_at_start_samples: u64,
    trim_at_end_samples: u64,
    reserved_obus: u64,
}

pub(crate) fn looks_like_iamf(header: &[u8]) -> bool {
    if header.len() < 8 || header[0] >> 3 != SEQUENCE_HEADER || header[0] & 0x02 != 0 {
        return false;
    }
    let Some((size, leb_bytes)) = read_leb_slice(&header[1..]) else {
        return false;
    };
    if size < 6 {
        return false;
    }
    let mut cursor = 1 + leb_bytes;
    if header[0] & 1 != 0 {
        let Some((extension_size, extension_leb_bytes)) = read_leb_slice(&header[cursor..]) else {
            return false;
        };
        cursor += extension_leb_bytes;
        let Ok(extension_size) = usize::try_from(extension_size) else {
            return false;
        };
        let Some(end) = cursor.checked_add(extension_size) else {
            return false;
        };
        if end > header.len() {
            return false;
        }
        cursor = end;
    }
    header.get(cursor..cursor + 4) == Some(b"iamf")
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut state = State {
        bounds_valid: true,
        headers_valid: true,
        order_valid: true,
        profiles_valid: true,
        sequence_headers_valid: true,
        ..State::default()
    };
    let mut offset = 0_u64;

    while offset < file_size {
        if state.obus == MAX_OBUS {
            state.bounds_valid = false;
            break;
        }
        let mut header = [0_u8; 1];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("read {} IAMF OBU at {offset}: {error}", path.display()))?;
        let obu_type = header[0] >> 3;
        let redundant = header[0] & 0x04 != 0;
        let trimming = header[0] & 0x02 != 0;
        let extension = header[0] & 0x01 != 0;
        let (obu_size, leb_bytes) = match read_leb_reader(&mut reader) {
            Ok(value) => value,
            Err(()) => {
                state.headers_valid = false;
                break;
            }
        };
        let total_bytes = 1_u64
            .saturating_add(leb_bytes as u64)
            .saturating_add(obu_size);
        if obu_size > MAX_OBU_PAYLOAD_BYTES
            || total_bytes > MAX_OBU_BYTES
            || offset.saturating_add(total_bytes) > file_size
        {
            state.bounds_valid = false;
            break;
        }
        let mut body = vec![0_u8; obu_size as usize];
        reader.read_exact(&mut body).map_err(|error| {
            format!(
                "read {} IAMF OBU type {obu_type} at {offset}: {error}",
                path.display()
            )
        })?;
        let Some(payload_offset) = parse_optional_header(&body, trimming, extension, &mut state)
        else {
            state.headers_valid = false;
            break;
        };
        let payload = &body[payload_offset..];

        validate_header_flags(obu_type, redundant, trimming, payload, &mut state);
        update_order(obu_type, redundant, &mut state);
        if obu_type == SEQUENCE_HEADER {
            parse_sequence_header(payload, redundant, &mut state);
        }

        state.counts[usize::from(obu_type)] += 1;
        state.obus += 1;
        state.bytes += total_bytes;
        state.max_obu_bytes = state.max_obu_bytes.max(total_bytes);
        offset += total_bytes;
    }
    finish_descriptor_set(&mut state);

    let wrapper = vec![
        check(
            "FORGE-IAMF-OBU-BOUNDS",
            state.bounds_valid && state.bytes == file_size,
            "every OBU is bounded by the 2 MiB profile limit and the file",
            Some(json!({
                "file_bytes": file_size,
                "scanned_bytes": state.bytes,
                "max_obu_bytes": state.max_obu_bytes,
                "obu_limit_bytes": MAX_OBU_BYTES,
                "obu_count_limit": MAX_OBUS,
            })),
        ),
        check(
            "FORGE-IAMF-OBU-HEADER",
            state.headers_valid && state.obus > 0,
            "OBU sizes and optional trim/extension headers use bounded LEB128 syntax",
            Some(json!({
                "obus": state.obus,
                "extension_headers": state.extension_headers,
                "trimmed_frames": state.trimmed_frames,
            })),
        ),
    ];
    let bitstream = vec![
        check(
            "FORGE-IAMF-SEQUENCE-HEADER",
            state.sequence_headers_valid && state.sequence_count > 0,
            "each IA Sequence starts with an iamf sequence header and stable supported profiles",
            Some(json!({
                "sequences": state.sequence_count,
                "primary_profile": state.first_profile.map(|value| profile_name(value.0)),
                "additional_profile": state.first_profile.map(|value| profile_name(value.1)),
            })),
        ),
        check(
            "FORGE-IAMF-PROFILE",
            state.profiles_valid,
            "primary and additional profile values are defined by IAMF v1.1",
            Some(json!({"profiles": state.first_profile})),
        ),
        check(
            "FORGE-IAMF-ORDER",
            state.order_valid
                && state.saw_data
                && state.counts[usize::from(CODEC_CONFIG)] > 0
                && state.counts[usize::from(AUDIO_ELEMENT)] > 0
                && state.counts[usize::from(MIX_PRESENTATION)] > 0
                && state.saw_audio_frame,
            "descriptor OBUs precede IA data in sequence-header, codec, element, mix order",
            Some(json!({
                "descriptor_sets": state.descriptor_sets,
                "codec_configs": state.counts[usize::from(CODEC_CONFIG)],
                "audio_elements": state.counts[usize::from(AUDIO_ELEMENT)],
                "mix_presentations": state.counts[usize::from(MIX_PRESENTATION)],
                "audio_frames": audio_frame_count(&state),
            })),
        ),
    ];
    let xcheck = vec![
        check(
            "FORGE-IAMF-DATA-FLAGS",
            state.headers_valid,
            "audio/parameter data redundancy and trimming flags obey their OBU-type constraints",
            Some(json!({
                "trim_at_start_samples": state.trim_at_start_samples,
                "trim_at_end_samples": state.trim_at_end_samples,
            })),
        ),
        check(
            "FORGE-IAMF-OAR-RENDER",
            true,
            "structural QC does not claim rendered loudness; audit every OAR output with forge-presentation-qc",
            Some(json!({
                "renderer_standard": "AOMedia Open Audio Renderer v1.0.0",
                "structural_only": true,
            })),
        ),
    ];
    Ok(finish_audit(
        path,
        "iamf",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "obus": state.obus,
            "bytes": state.bytes,
            "obu_counts": obu_counts(&state),
            "sequence_count": state.sequence_count,
            "descriptor_sets": state.descriptor_sets,
            "primary_profile": state.first_profile.map(|value| profile_name(value.0)),
            "additional_profile": state.first_profile.map(|value| profile_name(value.1)),
            "extension_headers": state.extension_headers,
            "reserved_obus": state.reserved_obus,
            "trimmed_frames": state.trimmed_frames,
            "trim_at_start_samples": state.trim_at_start_samples,
            "trim_at_end_samples": state.trim_at_end_samples,
            "renderer_qc": "external OAR v1.0.0 render required",
        }),
    ))
}

fn parse_optional_header(
    body: &[u8],
    trimming: bool,
    extension: bool,
    state: &mut State,
) -> Option<usize> {
    let mut cursor = 0_usize;
    if trimming {
        let (trim_end, bytes) = read_leb_slice(body.get(cursor..)?)?;
        cursor = cursor.checked_add(bytes)?;
        let (trim_start, bytes) = read_leb_slice(body.get(cursor..)?)?;
        cursor = cursor.checked_add(bytes)?;
        state.trimmed_frames += 1;
        state.trim_at_end_samples = state.trim_at_end_samples.saturating_add(trim_end);
        state.trim_at_start_samples = state.trim_at_start_samples.saturating_add(trim_start);
    }
    if extension {
        let (size, bytes) = read_leb_slice(body.get(cursor..)?)?;
        cursor = cursor.checked_add(bytes)?;
        let size = usize::try_from(size).ok()?;
        cursor = cursor.checked_add(size)?;
        if cursor > body.len() {
            return None;
        }
        state.extension_headers += 1;
    }
    Some(cursor)
}

fn validate_header_flags(
    obu_type: u8,
    redundant: bool,
    trimming: bool,
    payload: &[u8],
    state: &mut State,
) {
    let audio_frame = (AUDIO_FRAME..=23).contains(&obu_type);
    if trimming && !audio_frame {
        state.headers_valid = false;
    }
    if redundant && (obu_type == TEMPORAL_DELIMITER || audio_frame || obu_type == PARAMETER_BLOCK) {
        state.headers_valid = false;
    }
    if obu_type == TEMPORAL_DELIMITER && !payload.is_empty() {
        state.headers_valid = false;
    }
    if obu_type == PARAMETER_BLOCK
        && state.uses_temporal_delimiters
        && state.saw_audio_in_temporal_unit
    {
        state.order_valid = false;
    }
    if obu_type == TEMPORAL_DELIMITER {
        if !state.uses_temporal_delimiters && state.saw_data {
            state.order_valid = false;
        }
        state.uses_temporal_delimiters = true;
        state.saw_audio_in_temporal_unit = false;
    } else if audio_frame {
        state.saw_audio_frame = true;
        state.saw_audio_in_temporal_unit = true;
    } else if (24..=30).contains(&obu_type) {
        state.reserved_obus += 1;
    }
}

fn update_order(obu_type: u8, redundant: bool, state: &mut State) {
    if obu_type == SEQUENCE_HEADER {
        if state.descriptor_phase {
            state.order_valid = false;
            finish_descriptor_set(state);
        }
        state.descriptor_phase = true;
        state.descriptor_rank = 0;
        state.descriptor_redundant = redundant;
        state.current_codec_configs = 0;
        state.current_audio_elements = 0;
        state.current_mix_presentations = 0;
        state.saw_data = false;
        return;
    }
    if matches!(obu_type, CODEC_CONFIG | AUDIO_ELEMENT | MIX_PRESENTATION) {
        if !state.descriptor_phase {
            state.order_valid = false;
            return;
        }
        let rank = obu_type + 1;
        if rank < state.descriptor_rank
            || (state.descriptor_sets > 0 && redundant != state.descriptor_redundant)
        {
            state.order_valid = false;
        }
        state.descriptor_rank = rank;
        match obu_type {
            CODEC_CONFIG => state.current_codec_configs += 1,
            AUDIO_ELEMENT => state.current_audio_elements += 1,
            MIX_PRESENTATION => state.current_mix_presentations += 1,
            _ => {}
        }
        return;
    }
    if (24..=30).contains(&obu_type) {
        if state.descriptor_phase && state.current_mix_presentations > 0 {
            state.order_valid = false;
        }
        return;
    }
    if obu_type <= 23 {
        if state.descriptor_phase {
            finish_descriptor_set(state);
        }
        state.saw_data = true;
    }
}

fn finish_descriptor_set(state: &mut State) {
    if !state.descriptor_phase {
        return;
    }
    if state.current_codec_configs != 1
        || state.current_audio_elements == 0
        || state.current_mix_presentations == 0
    {
        state.order_valid = false;
    }
    state.descriptor_sets += 1;
    state.descriptor_phase = false;
}

fn parse_sequence_header(payload: &[u8], redundant: bool, state: &mut State) {
    state.sequence_count += 1;
    if payload.len() < 6 || &payload[..4] != b"iamf" {
        state.sequence_headers_valid = false;
        return;
    }
    let profiles = (payload[4], payload[5]);
    if profiles.0 > 2 || profiles.1 > 2 {
        state.profiles_valid = false;
    }
    if let Some(first) = state.first_profile {
        if redundant && profiles != first {
            state.sequence_headers_valid = false;
        }
    } else {
        state.first_profile = Some(profiles);
    }
}

fn read_leb_reader(reader: &mut impl Read) -> Result<(u64, usize), ()> {
    let mut value = 0_u64;
    for index in 0..MAX_LEB_BYTES {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte).map_err(|_| ())?;
        value |= u64::from(byte[0] & 0x7f) << (index * 7);
        if byte[0] & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(())
}

fn read_leb_slice(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in bytes.iter().copied().take(MAX_LEB_BYTES).enumerate() {
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

fn audio_frame_count(state: &State) -> u64 {
    state.counts[usize::from(AUDIO_FRAME)..=23].iter().sum()
}

fn profile_name(profile: u8) -> &'static str {
    match profile {
        0 => "simple",
        1 => "base",
        2 => "base-enhanced",
        _ => "reserved",
    }
}

fn obu_counts(state: &State) -> Value {
    let mut result = Map::new();
    for (obu_type, count) in state.counts.iter().copied().enumerate() {
        if count == 0 {
            continue;
        }
        let name = match obu_type {
            0 => "codec-config".to_string(),
            1 => "audio-element".to_string(),
            2 => "mix-presentation".to_string(),
            3 => "parameter-block".to_string(),
            4 => "temporal-delimiter".to_string(),
            5 => "audio-frame-explicit-id".to_string(),
            6..=23 => format!("audio-frame-id{}", obu_type - 6),
            24..=30 => format!("reserved-{obu_type}"),
            31 => "sequence-header".to_string(),
            _ => unreachable!(),
        };
        result.insert(name, json!(count));
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut bytes = vec![obu_type << 3, payload.len() as u8];
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn detects_sequence_header_with_iamf_code() {
        assert!(looks_like_iamf(&[0xf8, 6, b'i', b'a', b'm', b'f', 0, 0]));
        assert!(!looks_like_iamf(&[0xf8, 6, b'n', b'o', b'p', b'e', 0, 0]));
    }

    #[test]
    fn leb128_is_bounded() {
        assert_eq!(read_leb_slice(&[0xf2, 0x01]), Some((242, 2)));
        assert_eq!(read_leb_slice(&[0x80; 8]), None);
    }

    #[test]
    fn descriptor_order_and_data_are_accepted() {
        let mut state = State {
            bounds_valid: true,
            headers_valid: true,
            order_valid: true,
            profiles_valid: true,
            sequence_headers_valid: true,
            ..State::default()
        };
        for (kind, redundant) in [
            (SEQUENCE_HEADER, false),
            (CODEC_CONFIG, false),
            (AUDIO_ELEMENT, false),
            (MIX_PRESENTATION, false),
            (AUDIO_FRAME + 1, false),
        ] {
            update_order(kind, redundant, &mut state);
        }
        finish_descriptor_set(&mut state);
        assert!(state.order_valid);
        assert!(state.saw_data);
    }

    #[test]
    fn test_obu_builder_matches_sequence_signature() {
        let bytes = obu(SEQUENCE_HEADER, b"iamf\x00\x00");
        assert_eq!(bytes, b"\xf8\x06iamf\x00\x00");
    }
}
