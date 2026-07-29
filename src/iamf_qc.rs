//! Bounded structural QC for standalone AOMedia IAMF IA Sequences.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_OBU_BYTES: u64 = 1 << 21;
const MAX_OBU_PAYLOAD_BYTES: u64 = MAX_OBU_BYTES - 4;
const MAX_OBUS: u64 = 10_000_000;
const MAX_LEB_BYTES: usize = 8;
const MAX_DESCRIPTOR_IDS: usize = 65_536;
const MAX_EVIDENCE_ERRORS: usize = 32;

const CODEC_CONFIG: u8 = 0;
const AUDIO_ELEMENT: u8 = 1;
const MIX_PRESENTATION: u8 = 2;
const PARAMETER_BLOCK: u8 = 3;
const TEMPORAL_DELIMITER: u8 = 4;
const AUDIO_FRAME: u8 = 5;
const SEQUENCE_HEADER: u8 = 31;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodecConfig {
    id: u64,
    codec_id: String,
    num_samples_per_frame: u64,
    audio_roll_distance: i16,
    sample_rate: Option<u32>,
    sample_size: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AudioElement {
    id: u64,
    element_type: u8,
    codec_config_id: u64,
    substream_ids: Vec<u64>,
    parameter_ids: Vec<u64>,
    parameter_types: Vec<u64>,
    config: AudioElementConfig,
    trailing_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AudioElementConfig {
    ChannelBased {
        num_layers: u8,
        highest_layout: String,
        output_channels: u16,
    },
    SceneBased {
        ambisonics_mode: u8,
        output_channels: u16,
    },
    Reserved,
}

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
    codec_configs_valid: bool,
    audio_elements_valid: bool,
    descriptor_links_valid: bool,
    audio_frames_valid: bool,
    current_codec_configs_by_id: BTreeMap<u64, CodecConfig>,
    current_audio_elements_by_id: BTreeMap<u64, AudioElement>,
    current_substream_ids: BTreeSet<u64>,
    current_parameter_ids: BTreeSet<u64>,
    current_parameter_definitions: usize,
    active_codec_configs: BTreeMap<u64, CodecConfig>,
    active_audio_elements: BTreeMap<u64, AudioElement>,
    active_substream_ids: BTreeSet<u64>,
    pending_substream_ids: BTreeSet<u64>,
    frame_counts: BTreeMap<u64, u64>,
    codec_config_observations: Vec<CodecConfig>,
    audio_element_observations: Vec<AudioElement>,
    observed_substream_ids: usize,
    payload_errors: Vec<String>,
    payload_evidence_truncated: bool,
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
        codec_configs_valid: true,
        audio_elements_valid: true,
        descriptor_links_valid: true,
        audio_frames_valid: true,
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
        match obu_type {
            SEQUENCE_HEADER => parse_sequence_header(payload, redundant, &mut state),
            CODEC_CONFIG => parse_codec_config(payload, &mut state),
            AUDIO_ELEMENT => parse_audio_element(payload, &mut state),
            AUDIO_FRAME..=23 => parse_audio_frame(obu_type, payload, &mut state),
            _ => {}
        }

        state.counts[usize::from(obu_type)] += 1;
        state.obus += 1;
        state.bytes += total_bytes;
        state.max_obu_bytes = state.max_obu_bytes.max(total_bytes);
        offset += total_bytes;
    }
    finish_descriptor_set(&mut state);
    if !state.pending_substream_ids.is_empty() {
        state.audio_frames_valid = false;
        let pending_count = state.pending_substream_ids.len();
        let pending = state
            .pending_substream_ids
            .iter()
            .copied()
            .take(MAX_EVIDENCE_ERRORS)
            .collect::<Vec<_>>();
        record_payload_error(
            &mut state,
            format!(
                "{pending_count} audio substream IDs have no Audio Frame OBU; first IDs: {pending:?}"
            ),
        );
    }

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
        check(
            "FORGE-IAMF-CODEC-CONFIG",
            state.codec_configs_valid && !state.codec_config_observations.is_empty(),
            "codec configurations have unique IDs, supported 4CC semantics, frame lengths, roll distances, and bounded decoder configs",
            Some(json!({
                "codec_configs": codec_config_json(&state.codec_config_observations),
                "errors": state.payload_errors,
                "evidence_truncated": state.payload_evidence_truncated,
            })),
        ),
        check(
            "FORGE-IAMF-AUDIO-ELEMENT",
            state.audio_elements_valid && !state.audio_element_observations.is_empty(),
            "audio elements have bounded parameter definitions and conforming channel or Ambisonics configuration",
            Some(json!({
                "audio_elements": audio_element_json(&state.audio_element_observations),
                "errors": state.payload_errors,
                "evidence_truncated": state.payload_evidence_truncated,
            })),
        ),
        check(
            "FORGE-IAMF-DESCRIPTOR-LINKS",
            state.descriptor_links_valid && !state.audio_element_observations.is_empty(),
            "audio elements have unique IDs and substreams and resolve exactly one codec configuration",
            Some(json!({
                "audio_elements": audio_element_json(&state.audio_element_observations),
                "declared_substream_ids": declared_substream_ids(&state.audio_element_observations),
                "errors": state.payload_errors,
                "evidence_truncated": state.payload_evidence_truncated,
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
            "FORGE-IAMF-AUDIO-FRAME-LINKS",
            state.audio_frames_valid && !state.frame_counts.is_empty(),
            "every audio frame resolves a declared substream ID and every declared substream has data",
            Some(json!({
                "frame_counts": state.frame_counts,
                "pending_substream_ids": state.pending_substream_ids,
                "errors": state.payload_errors,
                "evidence_truncated": state.payload_evidence_truncated,
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
            "codec_configs": codec_config_json(&state.codec_config_observations),
            "audio_elements": audio_element_json(&state.audio_element_observations),
            "audio_frame_counts": state.frame_counts,
            "payload_errors": state.payload_errors,
            "payload_evidence_truncated": state.payload_evidence_truncated,
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
        state.current_codec_configs_by_id.clear();
        state.current_audio_elements_by_id.clear();
        state.current_substream_ids.clear();
        state.current_parameter_ids.clear();
        state.current_parameter_definitions = 0;
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
    let missing_codec_links = state
        .current_audio_elements_by_id
        .values()
        .filter(|element| {
            !state
                .current_codec_configs_by_id
                .contains_key(&element.codec_config_id)
        })
        .map(|element| (element.id, element.codec_config_id))
        .collect::<Vec<_>>();
    for (element_id, codec_config_id) in missing_codec_links {
        state.descriptor_links_valid = false;
        record_payload_error(
            state,
            format!("audio element {element_id} references missing codec config {codec_config_id}"),
        );
    }
    if state.descriptor_sets > 0 && state.descriptor_redundant {
        if state.current_codec_configs_by_id != state.active_codec_configs
            || state.current_audio_elements_by_id != state.active_audio_elements
        {
            state.descriptor_links_valid = false;
            record_payload_error(
                state,
                "redundant descriptor payloads differ from the active descriptor set".into(),
            );
        }
    } else {
        state.active_codec_configs = state.current_codec_configs_by_id.clone();
        state.active_audio_elements = state.current_audio_elements_by_id.clone();
        state.active_substream_ids = state.current_substream_ids.clone();
        state
            .pending_substream_ids
            .extend(state.active_substream_ids.iter().copied());
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

fn parse_codec_config(payload: &[u8], state: &mut State) {
    match codec_config(payload) {
        Ok(config) => {
            if state.current_codec_configs_by_id.len() == MAX_DESCRIPTOR_IDS
                && !state.current_codec_configs_by_id.contains_key(&config.id)
            {
                state.codec_configs_valid = false;
                record_payload_error(
                    state,
                    format!("codec config count exceeds {MAX_DESCRIPTOR_IDS}"),
                );
                return;
            }
            let duplicate_id = state
                .current_codec_configs_by_id
                .insert(config.id, config.clone())
                .is_some();
            let duplicate_codec = state
                .current_codec_configs_by_id
                .values()
                .filter(|candidate| candidate.codec_id == config.codec_id)
                .count()
                > 1;
            if duplicate_id || duplicate_codec {
                state.codec_configs_valid = false;
                record_payload_error(
                    state,
                    format!(
                        "codec config {} ({}) is not unique in its descriptor set",
                        config.id, config.codec_id
                    ),
                );
            }
            if state.codec_config_observations.len() < MAX_DESCRIPTOR_IDS {
                state.codec_config_observations.push(config);
            } else {
                state.payload_evidence_truncated = true;
            }
        }
        Err(error) => {
            state.codec_configs_valid = false;
            record_payload_error(state, format!("invalid Codec Config OBU: {error}"));
        }
    }
}

fn codec_config(payload: &[u8]) -> Result<CodecConfig, String> {
    let mut cursor = 0;
    let id = take_leb(payload, &mut cursor, "codec_config_id")?;
    let codec_bytes = take_bytes(payload, &mut cursor, 4, "codec_id")?;
    if !codec_bytes.is_ascii() {
        return Err("codec_id is not ASCII".into());
    }
    let codec_id = std::str::from_utf8(codec_bytes)
        .map_err(|_| "codec_id is not UTF-8")?
        .to_string();
    let num_samples_per_frame = take_leb(payload, &mut cursor, "num_samples_per_frame")?;
    if num_samples_per_frame == 0 {
        return Err("num_samples_per_frame is zero".into());
    }
    let roll_bytes = take_bytes(payload, &mut cursor, 2, "audio_roll_distance")?;
    let audio_roll_distance = i16::from_be_bytes([roll_bytes[0], roll_bytes[1]]);
    let decoder_config = &payload[cursor..];
    let (sample_rate, sample_size) = match codec_id.as_str() {
        "Opus" => {
            let expected_roll = -i16::try_from(3_840_u64.div_ceil(num_samples_per_frame))
                .map_err(|_| "Opus num_samples_per_frame produces an invalid roll distance")?;
            if audio_roll_distance != expected_roll {
                return Err(format!(
                    "Opus audio_roll_distance is {audio_roll_distance}, expected {expected_roll}"
                ));
            }
            validate_opus_decoder_config(decoder_config)?;
            (Some(48_000), None)
        }
        "mp4a" => {
            if audio_roll_distance != -1 {
                return Err(format!(
                    "AAC-LC audio_roll_distance is {audio_roll_distance}, expected -1"
                ));
            }
            if num_samples_per_frame != 1_024 {
                return Err(format!(
                    "AAC-LC num_samples_per_frame is {num_samples_per_frame}, expected 1024"
                ));
            }
            if decoder_config.is_empty() {
                return Err("AAC-LC DecoderConfigDescriptor is empty".into());
            }
            (None, None)
        }
        "fLaC" => {
            if audio_roll_distance != 0 {
                return Err(format!(
                    "FLAC audio_roll_distance is {audio_roll_distance}, expected 0"
                ));
            }
            let sample_rate = validate_flac_decoder_config(decoder_config, num_samples_per_frame)?;
            (Some(sample_rate), None)
        }
        "ipcm" => {
            if audio_roll_distance != 0 {
                return Err(format!(
                    "LPCM audio_roll_distance is {audio_roll_distance}, expected 0"
                ));
            }
            let (sample_rate, sample_size) = validate_ipcm_decoder_config(decoder_config)?;
            (Some(sample_rate), Some(sample_size))
        }
        _ => return Err(format!("unsupported IAMF codec_id {codec_id:?}")),
    };
    Ok(CodecConfig {
        id,
        codec_id,
        num_samples_per_frame,
        audio_roll_distance,
        sample_rate,
        sample_size,
    })
}

fn validate_opus_decoder_config(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() != 11 {
        return Err(format!(
            "Opus decoder config is {} bytes, expected 11 without OpusHead",
            bytes.len()
        ));
    }
    if bytes[0] > 15 {
        return Err(format!(
            "Opus ID header version {} is unsupported",
            bytes[0]
        ));
    }
    if bytes[1] != 2 {
        return Err(format!(
            "Opus output channel count is {}, expected 2",
            bytes[1]
        ));
    }
    if i16::from_be_bytes([bytes[8], bytes[9]]) != 0 {
        return Err("Opus output gain is not zero".into());
    }
    if bytes[10] != 0 {
        return Err(format!(
            "Opus channel mapping family is {}, expected 0",
            bytes[10]
        ));
    }
    Ok(())
}

fn validate_flac_decoder_config(bytes: &[u8], num_samples_per_frame: u64) -> Result<u32, String> {
    if bytes.len() < 38 {
        return Err("FLAC decoder config omits the 34-byte STREAMINFO block".into());
    }
    let mut cursor = 0;
    let mut first = true;
    let mut saw_last = false;
    let mut sample_rate = None;
    while cursor < bytes.len() {
        let header = take_bytes(bytes, &mut cursor, 4, "FLAC metadata header")?;
        let last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let size =
            (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
        let block = take_bytes(bytes, &mut cursor, size, "FLAC metadata payload")?;
        if first {
            if block_type != 0 || block.len() != 34 {
                return Err("FLAC decoder config does not start with 34-byte STREAMINFO".into());
            }
            let min_block = u64::from(u16::from_be_bytes([block[0], block[1]]));
            let max_block = u64::from(u16::from_be_bytes([block[2], block[3]]));
            if min_block != num_samples_per_frame || max_block != num_samples_per_frame {
                return Err(format!(
                    "FLAC STREAMINFO block sizes {min_block}/{max_block} do not match num_samples_per_frame {num_samples_per_frame}"
                ));
            }
            let rate = (u32::from(block[10]) << 12)
                | (u32::from(block[11]) << 4)
                | (u32::from(block[12]) >> 4);
            if rate == 0 {
                return Err("FLAC STREAMINFO sample rate is zero".into());
            }
            if (block[12] >> 1) & 0x07 != 1 {
                return Err(
                    "FLAC STREAMINFO channels-minus-one field is not the required value 1".into(),
                );
            }
            sample_rate = Some(rate);
            first = false;
        } else if block_type == 0 {
            return Err("FLAC decoder config repeats STREAMINFO".into());
        }
        if last {
            saw_last = true;
            if cursor != bytes.len() {
                return Err("bytes follow the last FLAC metadata block".into());
            }
        } else if cursor == bytes.len() {
            return Err("FLAC decoder config has no last-metadata-block flag".into());
        }
    }
    if !saw_last {
        return Err("FLAC decoder config has no last metadata block".into());
    }
    sample_rate.ok_or_else(|| "FLAC decoder config has no STREAMINFO".into())
}

fn validate_ipcm_decoder_config(bytes: &[u8]) -> Result<(u32, u8), String> {
    if bytes.len() != 6 {
        return Err(format!(
            "LPCM decoder config is {} bytes, expected 6",
            bytes.len()
        ));
    }
    if !matches!(bytes[0], 0 | 1) {
        return Err(format!("LPCM sample_format_flags is {}", bytes[0]));
    }
    if !matches!(bytes[1], 16 | 24 | 32) {
        return Err(format!("LPCM sample_size is {}", bytes[1]));
    }
    let sample_rate = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    if !matches!(sample_rate, 16_000 | 32_000 | 44_100 | 48_000 | 96_000) {
        return Err(format!("LPCM sample_rate is {sample_rate} Hz"));
    }
    Ok((sample_rate, bytes[1]))
}

fn parse_audio_element(payload: &[u8], state: &mut State) {
    let codec_config_id = audio_element_codec_config_id(payload);
    let codec = codec_config_id
        .and_then(|id| state.current_codec_configs_by_id.get(&id))
        .cloned();
    match audio_element(payload, codec.as_ref()) {
        Ok(element) => {
            if state.current_audio_elements_by_id.len() == MAX_DESCRIPTOR_IDS
                && !state.current_audio_elements_by_id.contains_key(&element.id)
            {
                state.descriptor_links_valid = false;
                record_payload_error(
                    state,
                    format!("audio element count exceeds {MAX_DESCRIPTOR_IDS}"),
                );
                return;
            }
            let new_substreams = element
                .substream_ids
                .iter()
                .filter(|id| !state.current_substream_ids.contains(id))
                .count();
            if state
                .current_substream_ids
                .len()
                .saturating_add(new_substreams)
                > MAX_DESCRIPTOR_IDS
            {
                state.descriptor_links_valid = false;
                record_payload_error(
                    state,
                    format!("audio substream count exceeds {MAX_DESCRIPTOR_IDS}"),
                );
                return;
            }
            let new_parameters = element
                .parameter_ids
                .iter()
                .filter(|id| !state.current_parameter_ids.contains(id))
                .count();
            if state
                .current_parameter_ids
                .len()
                .saturating_add(new_parameters)
                > MAX_DESCRIPTOR_IDS
            {
                state.audio_elements_valid = false;
                record_payload_error(
                    state,
                    format!("parameter substream count exceeds {MAX_DESCRIPTOR_IDS}"),
                );
                return;
            }
            if state
                .current_parameter_definitions
                .saturating_add(element.parameter_types.len())
                > MAX_DESCRIPTOR_IDS
            {
                state.audio_elements_valid = false;
                record_payload_error(
                    state,
                    format!("parameter definition count exceeds {MAX_DESCRIPTOR_IDS}"),
                );
                return;
            }
            let duplicate_element = state
                .current_audio_elements_by_id
                .insert(element.id, element.clone())
                .is_some();
            let mut duplicate_substream = false;
            for substream_id in &element.substream_ids {
                if !state.current_substream_ids.insert(*substream_id) {
                    duplicate_substream = true;
                }
            }
            let mut duplicate_parameter = false;
            for parameter_id in &element.parameter_ids {
                if !state.current_parameter_ids.insert(*parameter_id) {
                    duplicate_parameter = true;
                }
            }
            state.current_parameter_definitions += element.parameter_types.len();
            if duplicate_element || duplicate_substream || duplicate_parameter {
                state.descriptor_links_valid = false;
                record_payload_error(
                    state,
                    format!(
                        "audio element {}, one of its substream IDs, or one of its parameter IDs is not unique",
                        element.id
                    ),
                );
            }
            if state
                .observed_substream_ids
                .saturating_add(element.substream_ids.len())
                <= MAX_DESCRIPTOR_IDS
            {
                state.observed_substream_ids += element.substream_ids.len();
                state.audio_element_observations.push(element);
            } else {
                state.payload_evidence_truncated = true;
            }
        }
        Err(error) => {
            state.audio_elements_valid = false;
            record_payload_error(state, format!("invalid Audio Element OBU: {error}"));
        }
    }
}

fn audio_element_codec_config_id(payload: &[u8]) -> Option<u64> {
    let mut cursor = 0;
    take_leb(payload, &mut cursor, "audio_element_id").ok()?;
    take_bytes(payload, &mut cursor, 1, "audio_element_type").ok()?;
    take_leb(payload, &mut cursor, "codec_config_id").ok()
}

fn audio_element(payload: &[u8], codec: Option<&CodecConfig>) -> Result<AudioElement, String> {
    let mut cursor = 0;
    let id = take_leb(payload, &mut cursor, "audio_element_id")?;
    let flags = *take_bytes(payload, &mut cursor, 1, "audio_element_type")?
        .first()
        .ok_or_else(|| "audio_element_type is missing".to_string())?;
    let element_type = flags >> 5;
    let codec_config_id = take_leb(payload, &mut cursor, "codec_config_id")?;
    let num_substreams = take_leb(payload, &mut cursor, "num_substreams")?;
    if num_substreams == 0 {
        return Err("num_substreams is zero".into());
    }
    let num_substreams =
        usize::try_from(num_substreams).map_err(|_| "num_substreams does not fit in memory")?;
    if num_substreams > MAX_DESCRIPTOR_IDS {
        return Err(format!(
            "num_substreams {num_substreams} exceeds {MAX_DESCRIPTOR_IDS}"
        ));
    }
    let mut substream_ids = Vec::with_capacity(num_substreams);
    for _ in 0..num_substreams {
        substream_ids.push(take_leb(payload, &mut cursor, "audio_substream_id")?);
    }
    let num_parameters = take_leb(payload, &mut cursor, "num_parameters")?;
    if num_parameters > MAX_DESCRIPTOR_IDS as u64 {
        return Err(format!(
            "num_parameters {num_parameters} exceeds {MAX_DESCRIPTOR_IDS}"
        ));
    }
    if element_type == 0 && num_parameters > 2 {
        return Err(format!(
            "channel-based num_parameters is {num_parameters}, expected 0, 1, or 2"
        ));
    }
    if element_type == 1 && num_parameters != 0 {
        return Err(format!(
            "scene-based num_parameters is {num_parameters}, expected 0"
        ));
    }
    if substream_ids.iter().copied().collect::<BTreeSet<_>>().len() != substream_ids.len() {
        return Err("audio element repeats an audio_substream_id".into());
    }
    let mut parameter_ids = Vec::new();
    let mut parameter_types = Vec::new();
    let mut seen_parameter_types = BTreeSet::new();
    for _ in 0..num_parameters {
        let parameter_type = take_leb(payload, &mut cursor, "param_definition_type")?;
        if !seen_parameter_types.insert(parameter_type) {
            return Err(format!(
                "param_definition_type {parameter_type} is duplicated"
            ));
        }
        parameter_types.push(parameter_type);
        match parameter_type {
            0 => {
                return Err(
                    "PARAMETER_DEFINITION_MIX_GAIN is forbidden in an Audio Element OBU".into(),
                );
            }
            1 | 2 => {
                parameter_ids.push(parse_audio_element_parameter(
                    payload,
                    &mut cursor,
                    parameter_type,
                    codec,
                )?);
            }
            _ => {
                let size = take_leb(payload, &mut cursor, "param_definition_size")?;
                let size = usize::try_from(size)
                    .map_err(|_| "param_definition_size does not fit in memory")?;
                take_bytes(payload, &mut cursor, size, "param_definition_bytes")?;
            }
        }
    }
    if parameter_ids.iter().copied().collect::<BTreeSet<_>>().len() != parameter_ids.len() {
        return Err("audio element repeats a parameter_id".into());
    }
    let config = match element_type {
        0 => parse_channel_audio_config(
            payload,
            &mut cursor,
            num_substreams,
            &parameter_types,
            codec,
        )?,
        1 => parse_ambisonics_config(payload, &mut cursor, num_substreams, codec)?,
        _ => {
            let size = take_leb(payload, &mut cursor, "audio_element_config_size")?;
            let size = usize::try_from(size)
                .map_err(|_| "audio_element_config_size does not fit in memory")?;
            take_bytes(payload, &mut cursor, size, "audio_element_config_bytes")?;
            AudioElementConfig::Reserved
        }
    };
    Ok(AudioElement {
        id,
        element_type,
        codec_config_id,
        substream_ids,
        parameter_ids,
        parameter_types,
        config,
        trailing_bytes: payload.len().saturating_sub(cursor),
    })
}

fn parse_audio_element_parameter(
    payload: &[u8],
    cursor: &mut usize,
    parameter_type: u64,
    codec: Option<&CodecConfig>,
) -> Result<u64, String> {
    let parameter_id = take_leb(payload, cursor, "parameter_id")?;
    let parameter_rate = take_leb(payload, cursor, "parameter_rate")?;
    if parameter_rate == 0 {
        return Err(format!(
            "parameter {parameter_id} has a zero parameter_rate"
        ));
    }
    let flags = take_bytes(payload, cursor, 1, "param_definition_mode")?[0];
    let mode = flags >> 7;
    if mode != 0 {
        return Err(format!(
            "Audio Element parameter {parameter_id} param_definition_mode is {mode}, expected 0"
        ));
    }
    let duration = take_leb(payload, cursor, "parameter duration")?;
    if duration == 0 {
        return Err(format!("parameter {parameter_id} duration is zero"));
    }
    let constant_subblock_duration = take_leb(payload, cursor, "constant_subblock_duration")?;
    if constant_subblock_duration == 0 {
        let num_subblocks = take_leb(payload, cursor, "num_subblocks")?;
        if num_subblocks > MAX_DESCRIPTOR_IDS as u64 {
            return Err(format!(
                "parameter {parameter_id} num_subblocks {num_subblocks} exceeds {MAX_DESCRIPTOR_IDS}"
            ));
        }
        let mut total = 0_u64;
        for _ in 0..num_subblocks {
            let subblock_duration = take_leb(payload, cursor, "subblock_duration")?;
            if subblock_duration == 0 {
                return Err(format!(
                    "parameter {parameter_id} has a zero subblock_duration"
                ));
            }
            total = total
                .checked_add(subblock_duration)
                .ok_or_else(|| format!("parameter {parameter_id} subblock duration overflow"))?;
        }
        if total != duration {
            return Err(format!(
                "parameter {parameter_id} subblock durations total {total}, expected {duration}"
            ));
        }
    }
    if let Some(codec) = codec {
        if duration != codec.num_samples_per_frame {
            return Err(format!(
                "parameter {parameter_id} duration is {duration}, expected codec frame length {}",
                codec.num_samples_per_frame
            ));
        }
        if constant_subblock_duration != duration {
            return Err(format!(
                "parameter {parameter_id} constant_subblock_duration is {constant_subblock_duration}, expected {duration}"
            ));
        }
        if let Some(sample_rate) = codec.sample_rate {
            if parameter_rate != u64::from(sample_rate) {
                return Err(format!(
                    "parameter {parameter_id} rate is {parameter_rate}, expected {sample_rate}"
                ));
            }
        }
    }
    if parameter_type == 1 {
        let demixing_flags = take_bytes(payload, cursor, 1, "default dmixp_mode")?[0];
        let dmixp_mode = demixing_flags >> 5;
        if matches!(dmixp_mode, 3 | 7) {
            return Err(format!(
                "parameter {parameter_id} uses reserved dmixp_mode {dmixp_mode}"
            ));
        }
        let weight_flags = take_bytes(payload, cursor, 1, "default_w")?[0];
        let default_w = weight_flags >> 4;
        if default_w > 10 {
            return Err(format!(
                "parameter {parameter_id} uses reserved default_w {default_w}"
            ));
        }
    }
    Ok(parameter_id)
}

fn parse_channel_audio_config(
    payload: &[u8],
    cursor: &mut usize,
    num_substreams: usize,
    parameter_types: &[u64],
    codec: Option<&CodecConfig>,
) -> Result<AudioElementConfig, String> {
    let header = take_bytes(payload, cursor, 1, "num_layers")?[0];
    let num_layers = header >> 5;
    if num_layers == 0 || num_layers > 6 {
        return Err(format!("num_layers is {num_layers}, expected 1 through 6"));
    }
    let mut declared_substreams = 0_usize;
    let mut cumulative_channels = 0_u16;
    let mut previous_dimensions: Option<(u8, u8, u8)> = None;
    let mut highest_layout = String::new();
    let mut highest_standard_layout = None;
    let mut recon_gain_flags = Vec::with_capacity(usize::from(num_layers));
    for layer_index in 0..num_layers {
        let flags = take_bytes(payload, cursor, 1, "channel_audio_layer_config")?[0];
        let loudspeaker_layout = flags >> 4;
        let output_gain_present = flags & 0x08 != 0;
        let recon_gain_present = flags & 0x04 != 0;
        let substream_count = usize::from(take_bytes(payload, cursor, 1, "substream_count")?[0]);
        let coupled_substream_count =
            usize::from(take_bytes(payload, cursor, 1, "coupled_substream_count")?[0]);
        if substream_count == 0 {
            return Err(format!(
                "channel layer {} has zero substreams",
                layer_index + 1
            ));
        }
        if coupled_substream_count > substream_count {
            return Err(format!(
                "channel layer {} coupled_substream_count {coupled_substream_count} exceeds substream_count {substream_count}",
                layer_index + 1
            ));
        }
        declared_substreams = declared_substreams
            .checked_add(substream_count)
            .ok_or_else(|| "channel layer substream count overflow".to_string())?;
        let layer_channels = substream_count
            .checked_add(coupled_substream_count)
            .ok_or_else(|| "channel layer channel count overflow".to_string())?;
        cumulative_channels = cumulative_channels
            .checked_add(
                u16::try_from(layer_channels)
                    .map_err(|_| "channel layer channel count does not fit")?,
            )
            .ok_or_else(|| "channel layout channel count overflow".to_string())?;
        if output_gain_present {
            take_bytes(payload, cursor, 1, "output_gain_flags")?;
            take_bytes(payload, cursor, 2, "output_gain")?;
        }
        recon_gain_flags.push(recon_gain_present);
        if num_layers == 1 && (output_gain_present || recon_gain_present) {
            return Err(
                "single-layer channel audio has output or reconstruction gain flags set".into(),
            );
        }

        if loudspeaker_layout == 15 {
            if layer_index != 0 || num_layers != 1 {
                return Err("expanded loudspeaker layout is not a single first layer".into());
            }
            let expanded = take_bytes(payload, cursor, 1, "expanded_loudspeaker_layout")?[0];
            let (name, channels) = expanded_layout_info(expanded)
                .ok_or_else(|| format!("expanded_loudspeaker_layout {expanded} is reserved"))?;
            if cumulative_channels != channels {
                return Err(format!(
                    "expanded layout {name} declares {cumulative_channels} channels from substreams, expected {channels}"
                ));
            }
            highest_layout = name.into();
        } else {
            let (name, channels, dimensions) = standard_layout_info(loudspeaker_layout)
                .ok_or_else(|| format!("loudspeaker_layout {loudspeaker_layout} is reserved"))?;
            if loudspeaker_layout == 9 && num_layers != 1 {
                return Err("Binaural loudspeaker layout requires exactly one layer".into());
            }
            if cumulative_channels != channels {
                return Err(format!(
                    "channel layer {} layout {name} declares {cumulative_channels} cumulative channels from substreams, expected {channels}",
                    layer_index + 1
                ));
            }
            if let (Some(previous), Some(current)) = (previous_dimensions, dimensions) {
                if current.0 < previous.0
                    || current.1 < previous.1
                    || current.2 < previous.2
                    || current == previous
                {
                    return Err(format!(
                        "channel layer {} layout {name} is not a strictly scalable successor",
                        layer_index + 1
                    ));
                }
            }
            previous_dimensions = dimensions;
            highest_standard_layout = Some(loudspeaker_layout);
            highest_layout = name.into();
        }
    }
    if declared_substreams != num_substreams {
        return Err(format!(
            "channel layers declare {declared_substreams} substreams, expected {num_substreams}"
        ));
    }
    let has_demixing = parameter_types.contains(&1);
    let has_recon_gain = parameter_types.contains(&2);
    if let Some(codec) = codec {
        let lossless = matches!(codec.codec_id.as_str(), "fLaC" | "ipcm");
        for (layer_index, present) in recon_gain_flags.iter().copied().enumerate() {
            let expected = !lossless && layer_index > 0;
            if present != expected {
                return Err(format!(
                    "channel layer {} recon_gain_is_present_flag is {}, expected {} for {}",
                    layer_index + 1,
                    u8::from(present),
                    u8::from(expected),
                    codec.codec_id
                ));
            }
        }
        let recon_gain_required = !lossless && num_layers > 1;
        if has_recon_gain != recon_gain_required {
            return Err(format!(
                "reconstruction gain definition is {}, expected {} for {} with {num_layers} layer(s)",
                if has_recon_gain { "present" } else { "absent" },
                if recon_gain_required {
                    "present"
                } else {
                    "absent"
                },
                codec.codec_id
            ));
        }
    } else if recon_gain_flags.iter().any(|present| *present) && !has_recon_gain {
        return Err("channel layer signals reconstruction gain without its definition".into());
    }
    if num_layers == 1
        && (highest_standard_layout.is_none() || matches!(highest_standard_layout, Some(0 | 1 | 8)))
        && has_demixing
    {
        return Err("this single-layer channel layout forbids demixing parameters".into());
    }
    if num_layers > 1
        && highest_standard_layout.is_some_and(demixing_required_for_layout)
        && !has_demixing
    {
        return Err("this multi-layer channel layout requires a demixing definition".into());
    }
    Ok(AudioElementConfig::ChannelBased {
        num_layers,
        highest_layout,
        output_channels: cumulative_channels,
    })
}

fn parse_ambisonics_config(
    payload: &[u8],
    cursor: &mut usize,
    num_substreams: usize,
    codec: Option<&CodecConfig>,
) -> Result<AudioElementConfig, String> {
    let ambisonics_mode = take_leb(payload, cursor, "ambisonics_mode")?;
    if ambisonics_mode > 1 {
        return Err(format!("ambisonics_mode {ambisonics_mode} is reserved"));
    }
    if ambisonics_mode == 1 && codec.is_some_and(|codec| codec.codec_id == "ipcm") {
        return Err("LPCM scene-based audio requires MONO Ambisonics mode".into());
    }
    let output_channels = u16::from(take_bytes(payload, cursor, 1, "output_channel_count")?[0]);
    if !is_ambisonics_channel_count(output_channels) {
        return Err(format!(
            "Ambisonics output_channel_count {output_channels} is not (1 + n)^2 for n=0..14"
        ));
    }
    let substream_count = usize::from(take_bytes(payload, cursor, 1, "substream_count")?[0]);
    if substream_count != num_substreams {
        return Err(format!(
            "Ambisonics substream_count is {substream_count}, expected {num_substreams}"
        ));
    }
    if ambisonics_mode == 0 {
        if substream_count > usize::from(output_channels) {
            return Err(format!(
                "Ambisonics substream_count {substream_count} exceeds output_channel_count {output_channels}"
            ));
        }
        let mapping = take_bytes(
            payload,
            cursor,
            usize::from(output_channels),
            "channel_mapping",
        )?;
        if let Some(value) = mapping
            .iter()
            .copied()
            .find(|value| *value != u8::MAX && usize::from(*value) >= substream_count)
        {
            return Err(format!(
                "Ambisonics channel_mapping value {value} is neither a substream channel nor 255"
            ));
        }
        let mapped_substreams = mapping
            .iter()
            .copied()
            .filter(|value| *value != u8::MAX)
            .collect::<BTreeSet<_>>();
        if mapped_substreams.len() != substream_count {
            return Err(format!(
                "Ambisonics channel_mapping covers {} unique substreams, expected {substream_count}",
                mapped_substreams.len()
            ));
        }
    } else {
        let coupled_substream_count =
            usize::from(take_bytes(payload, cursor, 1, "coupled_substream_count")?[0]);
        if coupled_substream_count > substream_count {
            return Err(format!(
                "Ambisonics coupled_substream_count {coupled_substream_count} exceeds substream_count {substream_count}"
            ));
        }
        let decoded_channels = substream_count
            .checked_add(coupled_substream_count)
            .ok_or_else(|| "Ambisonics decoded channel count overflow".to_string())?;
        if decoded_channels > usize::from(output_channels) {
            return Err(format!(
                "Ambisonics projection decodes {decoded_channels} channels, exceeding output_channel_count {output_channels}"
            ));
        }
        let coefficients = decoded_channels
            .checked_mul(usize::from(output_channels))
            .ok_or_else(|| "Ambisonics matrix coefficient count overflow".to_string())?;
        let matrix_bytes = coefficients
            .checked_mul(2)
            .ok_or_else(|| "Ambisonics matrix byte count overflow".to_string())?;
        take_bytes(payload, cursor, matrix_bytes, "demixing_matrix")?;
    }
    Ok(AudioElementConfig::SceneBased {
        ambisonics_mode: u8::try_from(ambisonics_mode)
            .map_err(|_| "ambisonics_mode does not fit")?,
        output_channels,
    })
}

type ChannelDimensions = (u8, u8, u8);
type StandardLayoutInfo = (&'static str, u16, Option<ChannelDimensions>);

fn standard_layout_info(layout: u8) -> Option<StandardLayoutInfo> {
    match layout {
        0 => Some(("Mono", 1, Some((1, 0, 0)))),
        1 => Some(("Stereo", 2, Some((2, 0, 0)))),
        2 => Some(("5.1ch", 6, Some((5, 1, 0)))),
        3 => Some(("5.1.2ch", 8, Some((5, 1, 2)))),
        4 => Some(("5.1.4ch", 10, Some((5, 1, 4)))),
        5 => Some(("7.1ch", 8, Some((7, 1, 0)))),
        6 => Some(("7.1.2ch", 10, Some((7, 1, 2)))),
        7 => Some(("7.1.4ch", 12, Some((7, 1, 4)))),
        8 => Some(("3.1.2ch", 6, Some((3, 1, 2)))),
        9 => Some(("Binaural", 2, None)),
        _ => None,
    }
}

fn expanded_layout_info(layout: u8) -> Option<(&'static str, u16)> {
    match layout {
        0 => Some(("LFE", 1)),
        1 => Some(("Stereo-S", 2)),
        2 => Some(("Stereo-SS", 2)),
        3 => Some(("Stereo-RS", 2)),
        4 => Some(("Stereo-TF", 2)),
        5 => Some(("Stereo-TB", 2)),
        6 => Some(("Top-4ch", 4)),
        7 => Some(("3.0ch", 3)),
        8 => Some(("9.1.6ch", 16)),
        9 => Some(("Stereo-F", 2)),
        10 => Some(("Stereo-Si", 2)),
        11 => Some(("Stereo-TpSi", 2)),
        12 => Some(("Top-6ch", 6)),
        _ => None,
    }
}

fn demixing_required_for_layout(layout: u8) -> bool {
    matches!(layout, 2..=7)
}

fn is_ambisonics_channel_count(channels: u16) -> bool {
    (0_u16..=14).any(|order| (order + 1) * (order + 1) == channels)
}

fn parse_audio_frame(obu_type: u8, payload: &[u8], state: &mut State) {
    let parsed = if obu_type == AUDIO_FRAME {
        let mut cursor = 0;
        take_leb(payload, &mut cursor, "explicit_audio_substream_id").and_then(|id| {
            if id <= 17 {
                Err(format!(
                    "explicit_audio_substream_id {id} is not greater than 17"
                ))
            } else if cursor == payload.len() {
                Err("explicit Audio Frame OBU has no coded frame".into())
            } else {
                Ok(id)
            }
        })
    } else if payload.is_empty() {
        Err("implicit Audio Frame OBU has no coded frame".into())
    } else {
        Ok(u64::from(obu_type - 6))
    };
    match parsed {
        Ok(substream_id) if state.active_substream_ids.contains(&substream_id) => {
            *state.frame_counts.entry(substream_id).or_default() += 1;
            state.pending_substream_ids.remove(&substream_id);
        }
        Ok(substream_id) => {
            state.audio_frames_valid = false;
            record_payload_error(
                state,
                format!("audio frame references undeclared substream {substream_id}"),
            );
        }
        Err(error) => {
            state.audio_frames_valid = false;
            record_payload_error(state, format!("invalid Audio Frame OBU: {error}"));
        }
    }
}

fn take_leb(bytes: &[u8], cursor: &mut usize, name: &str) -> Result<u64, String> {
    let (value, size) = read_leb_slice(
        bytes
            .get(*cursor..)
            .ok_or_else(|| format!("{name} is truncated"))?,
    )
    .ok_or_else(|| format!("{name} has invalid bounded LEB128 syntax"))?;
    *cursor = cursor
        .checked_add(size)
        .ok_or_else(|| format!("{name} cursor overflow"))?;
    Ok(value)
}

fn take_bytes<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    size: usize,
    name: &str,
) -> Result<&'a [u8], String> {
    let end = cursor
        .checked_add(size)
        .ok_or_else(|| format!("{name} size overflow"))?;
    let result = bytes
        .get(*cursor..end)
        .ok_or_else(|| format!("{name} is truncated"))?;
    *cursor = end;
    Ok(result)
}

fn record_payload_error(state: &mut State, error: String) {
    if state.payload_errors.len() < MAX_EVIDENCE_ERRORS {
        state.payload_errors.push(error);
    } else {
        state.payload_evidence_truncated = true;
    }
}

fn codec_config_json(configs: &[CodecConfig]) -> Vec<Value> {
    configs
        .iter()
        .map(|config| {
            json!({
                "codec_config_id": config.id,
                "codec_id": config.codec_id,
                "num_samples_per_frame": config.num_samples_per_frame,
                "audio_roll_distance": config.audio_roll_distance,
                "sample_rate_hz": config.sample_rate,
                "sample_size_bits": config.sample_size,
            })
        })
        .collect()
}

fn audio_element_json(elements: &[AudioElement]) -> Vec<Value> {
    elements
        .iter()
        .map(|element| {
            let (num_layers, layout, output_channels, ambisonics_mode) = match &element.config {
                AudioElementConfig::ChannelBased {
                    num_layers,
                    highest_layout,
                    output_channels,
                } => (
                    Some(*num_layers),
                    Some(highest_layout.as_str()),
                    Some(*output_channels),
                    None,
                ),
                AudioElementConfig::SceneBased {
                    ambisonics_mode,
                    output_channels,
                } => (
                    None,
                    None,
                    Some(*output_channels),
                    Some(match ambisonics_mode {
                        0 => "mono",
                        1 => "projection",
                        _ => "reserved",
                    }),
                ),
                AudioElementConfig::Reserved => (None, None, None, None),
            };
            json!({
                "audio_element_id": element.id,
                "audio_element_type": match element.element_type {
                    0 => "channel-based",
                    1 => "scene-based",
                    _ => "reserved",
                },
                "codec_config_id": element.codec_config_id,
                "audio_substream_ids": element.substream_ids,
                "parameter_ids": element.parameter_ids,
                "parameter_definition_types": element.parameter_types,
                "num_layers": num_layers,
                "highest_layout": layout,
                "output_channels": output_channels,
                "ambisonics_mode": ambisonics_mode,
                "trailing_extension_bytes": element.trailing_bytes,
            })
        })
        .collect()
}

fn declared_substream_ids(elements: &[AudioElement]) -> Vec<u64> {
    elements
        .iter()
        .flat_map(|element| element.substream_ids.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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

    #[test]
    fn parses_normative_lpcm_and_opus_codec_configs() {
        let lpcm =
            codec_config(&[3, b'i', b'p', b'c', b'm', 1, 0, 0, 1, 24, 0, 0, 187, 128]).unwrap();
        assert_eq!(lpcm.id, 3);
        assert_eq!(lpcm.codec_id, "ipcm");
        assert_eq!(lpcm.sample_rate, Some(48_000));
        assert_eq!(lpcm.sample_size, Some(24));

        let opus = codec_config(&[
            4, b'O', b'p', b'u', b's', 0xc0, 0x07, 0xff, 0xfc, 1, 2, 1, 56, 0, 0, 187, 128, 0, 0, 0,
        ])
        .unwrap();
        assert_eq!(opus.num_samples_per_frame, 960);
        assert_eq!(opus.audio_roll_distance, -4);
        assert_eq!(opus.sample_rate, Some(48_000));
    }

    #[test]
    fn rejects_wrong_codec_roll_distance_and_lpcm_fields() {
        let opus = codec_config(&[
            0, b'O', b'p', b'u', b's', 0xc0, 0x07, 0xff, 0xfb, 1, 2, 1, 56, 0, 0, 187, 128, 0, 0, 0,
        ])
        .unwrap_err();
        assert!(opus.contains("expected -4"), "{opus}");

        let lpcm =
            codec_config(&[0, b'i', b'p', b'c', b'm', 1, 0, 0, 2, 20, 0, 0, 187, 128]).unwrap_err();
        assert!(lpcm.contains("sample_format_flags"), "{lpcm}");
    }

    #[test]
    fn validates_flac_streaminfo_frame_geometry() {
        let mut streaminfo = [0_u8; 34];
        streaminfo[0..2].copy_from_slice(&256_u16.to_be_bytes());
        streaminfo[2..4].copy_from_slice(&256_u16.to_be_bytes());
        streaminfo[10] = 0x0b;
        streaminfo[11] = 0xb8;
        streaminfo[12] = 0x02;
        streaminfo[13] = 0xf0;
        let mut payload = vec![0, b'f', b'L', b'a', b'C', 0x80, 0x02, 0, 0];
        payload.extend_from_slice(&[0x80, 0, 0, 34]);
        payload.extend_from_slice(&streaminfo);
        let flac = codec_config(&payload).unwrap();
        assert_eq!(flac.num_samples_per_frame, 256);
        assert_eq!(flac.sample_rate, Some(48_000));

        payload[9] = 0;
        let error = codec_config(&payload).unwrap_err();
        assert!(error.contains("last-metadata-block"), "{error}");
    }

    #[test]
    fn parses_complete_audio_element_and_rejects_duplicate_substreams() {
        let element = audio_element(&[9, 0, 3, 2, 0, 18, 0, 0x20, 0x10, 2, 0], None).unwrap();
        assert_eq!(element.id, 9);
        assert_eq!(element.codec_config_id, 3);
        assert_eq!(element.substream_ids, [0, 18]);

        let error = audio_element(&[9, 0, 3, 2, 7, 7, 0], None).unwrap_err();
        assert!(error.contains("repeats"), "{error}");
    }

    fn test_codec(codec_id: &str) -> CodecConfig {
        CodecConfig {
            id: 3,
            codec_id: codec_id.into(),
            num_samples_per_frame: 4,
            audio_roll_distance: 0,
            sample_rate: None,
            sample_size: None,
        }
    }

    fn two_layer_element(recon_definition: bool, recon_layer_flag: bool) -> Vec<u8> {
        let mut payload = vec![
            9,
            0,
            3,
            4,
            0,
            1,
            2,
            3, // Common fields and four substream IDs.
            u8::from(recon_definition) + 1,
            1,
            10,
            1,
            0,
            4,
            4,
            0,
            0, // Demixing definition.
        ];
        if recon_definition {
            payload.extend_from_slice(&[2, 11, 1, 0, 4, 4]);
        }
        payload.extend_from_slice(&[
            0x40, // Two layers.
            0x10,
            1,
            1, // Stereo: one coupled substream.
            0x20 | if recon_layer_flag { 0x04 } else { 0 },
            3,
            1, // 5.1: four new channels in three substreams.
        ]);
        payload
    }

    #[test]
    fn validates_scalable_channel_layout_and_codec_recon_gain_policy() {
        let opus = test_codec("Opus");
        let element = audio_element(&two_layer_element(true, true), Some(&opus)).unwrap();
        assert_eq!(
            element.config,
            AudioElementConfig::ChannelBased {
                num_layers: 2,
                highest_layout: "5.1ch".into(),
                output_channels: 6,
            }
        );

        let missing = audio_element(&two_layer_element(false, true), Some(&opus)).unwrap_err();
        assert!(
            missing.contains("reconstruction gain definition"),
            "{missing}"
        );

        let lpcm = test_codec("ipcm");
        let lossless = audio_element(&two_layer_element(false, false), Some(&lpcm)).unwrap();
        assert!(matches!(
            lossless.config,
            AudioElementConfig::ChannelBased { num_layers: 2, .. }
        ));
    }

    #[test]
    fn validates_expanded_channel_layout_and_preserves_extension_length() {
        let mut payload = vec![9, 0, 3, 8];
        payload.extend(0_u8..8);
        payload.extend_from_slice(&[
            0, // No parameters.
            0x20, 0xf0, 8, 8, 8, // One expanded 9.1.6 layer.
            0xaa, 0xbb, // Permitted bytes past recognized OBU syntax.
        ]);
        let element = audio_element(&payload, None).unwrap();
        assert_eq!(element.trailing_bytes, 2);
        assert_eq!(
            element.config,
            AudioElementConfig::ChannelBased {
                num_layers: 1,
                highest_layout: "9.1.6ch".into(),
                output_channels: 16,
            }
        );
    }

    #[test]
    fn ignores_reserved_bits_and_skips_bounded_unknown_extensions() {
        let channel = audio_element(
            &[
                9, 0x1f, 3, 1, 0, 0,    // Reserved Audio Element bits are ignored.
                0x3f, // One layer plus reserved bits.
                0x03, 1, 0, // Mono plus reserved layer bits.
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            channel.config,
            AudioElementConfig::ChannelBased {
                num_layers: 1,
                highest_layout: "Mono".into(),
                output_channels: 1,
            }
        );

        let reserved = audio_element(
            &[
                9, 0x5f, 3, 1, 0, // Reserved element type and one substream.
                1, 3, 2, 0xaa, 0xbb, // Bounded unknown parameter definition.
                3, 0xcc, 0xdd, 0xee, // Bounded reserved element configuration.
                0xff, // Permitted bytes past recognized OBU syntax.
            ],
            None,
        )
        .unwrap();
        assert_eq!(reserved.config, AudioElementConfig::Reserved);
        assert_eq!(reserved.parameter_types, [3]);
        assert_eq!(reserved.trailing_bytes, 1);
    }

    #[test]
    fn validates_ambisonics_mapping_and_projection_geometry() {
        let mono = audio_element(&[9, 0x20, 3, 2, 0, 1, 0, 0, 4, 2, 255, 1, 0, 255], None).unwrap();
        assert_eq!(
            mono.config,
            AudioElementConfig::SceneBased {
                ambisonics_mode: 0,
                output_channels: 4,
            }
        );

        let limbo =
            audio_element(&[9, 0x20, 3, 2, 0, 1, 0, 0, 4, 2, 0, 0, 0, 0], None).unwrap_err();
        assert!(limbo.contains("covers 1 unique substreams"), "{limbo}");

        let oversized_projection = audio_element(
            &[
                9, 0x20, 3, 3, 0, 1, 2, 0, 1, 4, 3,
                2, // Five decoded
                  // channels cannot project to four output channels. Matrix parsing is
                  // intentionally never reached.
            ],
            None,
        )
        .unwrap_err();
        assert!(
            oversized_projection.contains("exceeding output_channel_count"),
            "{oversized_projection}"
        );
    }

    #[test]
    fn rejects_invalid_audio_element_parameter_timing_and_reserved_values() {
        let codec = test_codec("Opus");
        let bad_duration = [
            9, 0, 3, 1, 0, 1, // Common fields and one parameter.
            1, 10, 1, 0, 3, 3, 0, 0, // Demixing duration is three, not four.
            0x20, 0x20, 3, 3, // 5.1 in three coupled substreams.
        ];
        let error = audio_element(&bad_duration, Some(&codec)).unwrap_err();
        assert!(error.contains("expected codec frame length 4"), "{error}");

        let reserved_layout =
            audio_element(&[9, 0, 3, 1, 0, 0, 0x20, 0xa0, 1, 0], None).unwrap_err();
        assert!(reserved_layout.contains("reserved"), "{reserved_layout}");
    }
}
