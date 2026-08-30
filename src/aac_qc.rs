//! Dependency-free ADTS and LOAS/LATM AAC elementary-stream QC.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde::Serialize;
use serde_json::json;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_AAC_FRAMES: u64 = 10_000_000;
const MAX_LATM_STREAMS: usize = 128;
const ADTS_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AscInfo {
    pub audio_object_type: u8,
    pub core_sample_rate_hz: u32,
    pub output_sample_rate_hz: u32,
    pub channel_configuration: u8,
    pub output_channels: Option<u8>,
    pub sbr_present: bool,
    pub ps_present: bool,
    pub frame_samples: u16,
}

/// The delivery-relevant, bounded portion of an MPEG-D USAC configuration.
///
/// xHE-AAC is signalled as MPEG-4 Audio Object Type 42. Its decoder
/// configuration is not a GASpecificConfig, so it must not be accepted by the
/// legacy AAC parser merely because the outer AudioSpecificConfig is bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsacConfigInfo {
    pub audio_object_type: u8,
    pub output_sample_rate_hz: u32,
    pub core_sbr_frame_length_index: u8,
    pub channel_configuration_index: u8,
    pub output_channels: u16,
    pub frame_samples: u16,
    pub element_count: u16,
    pub uni_drc_config_present: bool,
    pub loudness_info_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LatmStream {
    asc: AscInfo,
    frame_length_type: u8,
    fixed_frame_length: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LatmConfig {
    all_streams_same_time_framing: bool,
    num_subframes: u8,
    streams: Vec<LatmStream>,
}

#[derive(Debug, Default)]
struct AdtsState {
    frames: u64,
    bytes: u64,
    samples: u64,
    crc_protected_frames: u64,
    vbr_fullness_frames: u64,
    numeric_fullness_frames: u64,
    first_config: Option<(u8, u8, u8, u8)>,
    config_changes: u64,
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
    audio_object_type: Option<u8>,
    sync_valid: bool,
    bounds_valid: bool,
    headers_valid: bool,
}

#[derive(Debug, Default)]
struct LoasState {
    frames: u64,
    bytes: u64,
    access_units: u64,
    samples: u64,
    new_configs: u64,
    reused_configs: u64,
    config_changes: u64,
    first_config: Option<LatmConfig>,
    current_config: Option<LatmConfig>,
    sync_valid: bool,
    bounds_valid: bool,
    mux_valid: bool,
}

pub(crate) fn looks_like_aac(header: &[u8]) -> bool {
    looks_like_adts(header) || looks_like_loas(header)
}

fn looks_like_adts(header: &[u8]) -> bool {
    header.len() >= 2 && header[0] == 0xff && header[1] & 0xf6 == 0xf0
}

fn looks_like_loas(header: &[u8]) -> bool {
    header.len() >= 2 && header[0] == 0x56 && header[1] & 0xe0 == 0xe0
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut signature = [0_u8; 3];
    let signature_size = usize::try_from(file_size.min(3)).unwrap();
    file.read_exact(&mut signature[..signature_size])
        .map_err(|error| format!("read {} AAC signature: {error}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    if looks_like_adts(&signature[..signature_size]) {
        audit_adts(path, file, file_size)
    } else {
        audit_loas(path, file, file_size)
    }
}

fn audit_adts(path: &Path, file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut reader = BufReader::new(file);
    let mut state = AdtsState {
        sync_valid: true,
        bounds_valid: true,
        headers_valid: true,
        ..AdtsState::default()
    };
    let mut offset = 0_u64;
    while offset < file_size {
        if state.frames == MAX_AAC_FRAMES {
            state.bounds_valid = false;
            break;
        }
        if file_size - offset < 7 {
            state.bounds_valid = false;
            break;
        }
        let mut header = [0_u8; 9];
        reader
            .read_exact(&mut header[..7])
            .map_err(|error| format!("read {} ADTS header at {offset}: {error}", path.display()))?;
        if !looks_like_adts(&header[..2]) {
            state.sync_valid = false;
            break;
        }
        let protection_absent = header[1] & 1 != 0;
        let header_size = if protection_absent { 7 } else { 9 };
        if !protection_absent {
            if file_size - offset < 9 {
                state.bounds_valid = false;
                break;
            }
            reader.read_exact(&mut header[7..9]).map_err(|error| {
                format!("read {} ADTS CRC at {offset}: {error}", path.display())
            })?;
            state.crc_protected_frames += 1;
        }
        let profile = header[2] >> 6;
        let sample_rate_index = (header[2] >> 2) & 0x0f;
        let channel_configuration = ((header[2] & 1) << 2) | (header[3] >> 6);
        let frame_length = (usize::from(header[3] & 3) << 11)
            | (usize::from(header[4]) << 3)
            | usize::from(header[5] >> 5);
        let buffer_fullness = (u16::from(header[5] & 0x1f) << 6) | u16::from(header[6] >> 2);
        let raw_data_blocks = header[6] & 3;
        let sample_rate = ADTS_SAMPLE_RATES
            .get(usize::from(sample_rate_index))
            .copied();
        if header[1] & 0x06 != 0
            || sample_rate.is_none()
            || frame_length < header_size
            || channel_configuration > 7
        {
            state.headers_valid = false;
            break;
        }
        let frame_end = offset
            .checked_add(frame_length as u64)
            .ok_or_else(|| "ADTS frame offset overflow".to_string())?;
        if frame_end > file_size {
            state.bounds_valid = false;
            break;
        }
        let config = (
            header[1] >> 3 & 1,
            profile,
            sample_rate_index,
            channel_configuration,
        );
        if let Some(first) = state.first_config {
            if first != config {
                state.config_changes += 1;
            }
        } else {
            state.first_config = Some(config);
            state.sample_rate_hz = sample_rate;
            state.channels = channel_count(channel_configuration);
            state.audio_object_type = Some(profile + 1);
        }
        if buffer_fullness == 0x07ff {
            state.vbr_fullness_frames += 1;
        } else {
            state.numeric_fullness_frames += 1;
        }
        let payload_size = frame_length - header_size;
        discard_exact(&mut reader, payload_size).map_err(|error| {
            format!(
                "read {} ADTS payload at {offset} ({payload_size} bytes): {error}",
                path.display()
            )
        })?;
        state.frames += 1;
        state.bytes += frame_length as u64;
        state.samples += 1024 * (u64::from(raw_data_blocks) + 1);
        offset = frame_end;
    }
    let mut wrapper = Vec::new();
    wrapper.push(check(
        "FORGE-AAC-ADTS-SYNC",
        state.sync_valid && state.frames > 0,
        "every ADTS frame starts with the 12-bit sync word and zero layer",
        Some(json!({"frames": state.frames, "scanned_bytes": state.bytes})),
    ));
    wrapper.push(check(
        "FORGE-AAC-ADTS-BOUNDS",
        state.bounds_valid && state.bytes == file_size,
        "ADTS frame lengths are bounded and consume the complete stream",
        Some(json!({"file_bytes": file_size, "frame_bytes": state.bytes, "limit": MAX_AAC_FRAMES})),
    ));
    let mut bitstream = Vec::new();
    bitstream.push(check(
        "FORGE-AAC-ADTS-HEADER",
        state.headers_valid,
        "ADTS profile, sample-rate index, channel configuration, and header size are valid",
        Some(json!({
            "audio_object_type": state.audio_object_type,
            "sample_rate_hz": state.sample_rate_hz,
            "channel_configuration": state.first_config.map(|config| config.3),
            "channels": state.channels
        })),
    ));
    bitstream.push(check(
        "FORGE-AAC-ADTS-CONFIG-CONTINUITY",
        state.config_changes == 0,
        "ADTS fixed-header MPEG ID, profile, rate, and channel configuration remain continuous",
        Some(json!({"changes": state.config_changes})),
    ));
    bitstream.push(check(
        "FORGE-AAC-ADTS-CRC-PRESENCE",
        true,
        "CRC-protected frames include the required two-byte error-check field",
        Some(json!({
            "protected_frames": state.crc_protected_frames,
            "unprotected_frames": state.frames.saturating_sub(state.crc_protected_frames)
        })),
    ));
    let fullness_consistent = state.vbr_fullness_frames == 0 || state.numeric_fullness_frames == 0;
    bitstream.push(check(
        "FORGE-AAC-ADTS-BUFFER-FULLNESS",
        fullness_consistent,
        "adts_buffer_fullness consistently uses either the VBR sentinel or numeric values",
        Some(json!({
            "vbr_sentinel_frames": state.vbr_fullness_frames,
            "numeric_frames": state.numeric_fullness_frames
        })),
    ));
    let duration = state
        .sample_rate_hz
        .map(|rate| state.samples as f64 / rate as f64);
    let xcheck = vec![check(
        "FORGE-AAC-SAMPLE-TIMING",
        state.frames > 0 && duration.is_some(),
        "decoded sample count and duration are derived from complete ADTS access units",
        Some(json!({
            "access_units": state.frames,
            "decoded_samples": state.samples,
            "duration_seconds": duration
        })),
    )];
    Ok(finish_audit(
        path,
        "aac-adts",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "standard": "ISO/IEC 14496-3:2019",
            "transport": "ADTS",
            "frames": state.frames,
            "bytes": state.bytes,
            "audio_object_type": state.audio_object_type,
            "sample_rate_hz": state.sample_rate_hz,
            "channel_configuration": state.first_config.map(|config| config.3),
            "channels": state.channels,
            "decoded_samples": state.samples,
            "duration_seconds": duration,
            "crc_protected_frames": state.crc_protected_frames
        }),
    ))
}

fn audit_loas(path: &Path, file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut reader = BufReader::new(file);
    let mut state = LoasState {
        sync_valid: true,
        bounds_valid: true,
        mux_valid: true,
        ..LoasState::default()
    };
    let mut offset = 0_u64;
    while offset < file_size {
        if state.frames == MAX_AAC_FRAMES {
            state.bounds_valid = false;
            break;
        }
        if file_size - offset < 3 {
            state.bounds_valid = false;
            break;
        }
        let mut header = [0_u8; 3];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("read {} LOAS header at {offset}: {error}", path.display()))?;
        if !looks_like_loas(&header) {
            state.sync_valid = false;
            break;
        }
        let mux_length = (usize::from(header[1] & 0x1f) << 8) | usize::from(header[2]);
        let frame_length = mux_length + 3;
        let frame_end = offset
            .checked_add(frame_length as u64)
            .ok_or_else(|| "LOAS frame offset overflow".to_string())?;
        if mux_length == 0 || frame_end > file_size {
            state.bounds_valid = false;
            break;
        }
        let mut payload = vec![0_u8; mux_length];
        reader.read_exact(&mut payload).map_err(|error| {
            format!(
                "read {} AudioMuxElement at {offset} ({mux_length} bytes): {error}",
                path.display()
            )
        })?;
        match parse_audio_mux_element(&payload, state.current_config.as_ref()) {
            Ok(parsed) => {
                if parsed.used_same_config {
                    state.reused_configs += 1;
                } else {
                    state.new_configs += 1;
                    if state
                        .first_config
                        .as_ref()
                        .is_some_and(|first| first != &parsed.config)
                    {
                        state.config_changes += 1;
                    }
                    state
                        .first_config
                        .get_or_insert_with(|| parsed.config.clone());
                }
                state.access_units += parsed.access_units;
                state.samples += parsed.output_samples;
                state.current_config = Some(parsed.config);
            }
            Err(_) => {
                state.mux_valid = false;
                break;
            }
        }
        state.frames += 1;
        state.bytes += frame_length as u64;
        offset = frame_end;
    }
    let mut wrapper = Vec::new();
    wrapper.push(check(
        "FORGE-AAC-LOAS-SYNC",
        state.sync_valid && state.frames > 0,
        "every AudioSyncStream frame starts with the 11-bit LOAS sync word",
        Some(json!({"frames": state.frames, "scanned_bytes": state.bytes})),
    ));
    wrapper.push(check(
        "FORGE-AAC-LOAS-BOUNDS",
        state.bounds_valid && state.bytes == file_size,
        "audioMuxLengthBytes bounds every AudioMuxElement and consumes the stream",
        Some(json!({"file_bytes": file_size, "frame_bytes": state.bytes, "limit": MAX_AAC_FRAMES})),
    ));
    let first_asc = state
        .first_config
        .as_ref()
        .and_then(|config| config.streams.first())
        .map(|stream| &stream.asc);
    let mut bitstream = Vec::new();
    bitstream.push(check(
        "FORGE-AAC-LATM-MUX",
        state.mux_valid,
        "AudioMuxElement, StreamMuxConfig, PayloadLengthInfo, and payload bounds are valid",
        Some(json!({
            "new_configs": state.new_configs,
            "reused_configs": state.reused_configs,
            "streams": state.first_config.as_ref().map(|config| config.streams.len())
        })),
    ));
    bitstream.push(check(
        "FORGE-AAC-LATM-CONFIG-CONTINUITY",
        state.config_changes == 0 && state.first_config.is_some(),
        "in-band StreamMuxConfig remains continuous and reuse never precedes configuration",
        Some(json!({"changes": state.config_changes})),
    ));
    bitstream.push(check(
        "FORGE-AAC-ASC",
        first_asc.is_some(),
        "AudioSpecificConfig exposes AAC object type, core/output rates, channels, SBR, and PS",
        first_asc.map(|asc| json!(asc)),
    ));
    let rate = first_asc.map(|asc| asc.output_sample_rate_hz);
    let duration = rate.map(|rate| state.samples as f64 / rate as f64);
    let xcheck = vec![check(
        "FORGE-AAC-SAMPLE-TIMING",
        state.access_units > 0 && duration.is_some(),
        "decoded sample count and duration include LATM subframes and SBR output expansion",
        Some(json!({
            "access_units": state.access_units,
            "decoded_samples": state.samples,
            "duration_seconds": duration
        })),
    )];
    Ok(finish_audit(
        path,
        "aac-loas",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "standard": "ISO/IEC 14496-3:2019",
            "transport": "LOAS/LATM AudioSyncStream",
            "frames": state.frames,
            "bytes": state.bytes,
            "access_units": state.access_units,
            "decoded_samples": state.samples,
            "duration_seconds": duration,
            "audio_specific_config": first_asc,
            "stream_mux_config_repetitions": state.new_configs,
            "stream_mux_config_reuses": state.reused_configs
        }),
    ))
}

struct ParsedMux {
    config: LatmConfig,
    used_same_config: bool,
    access_units: u64,
    output_samples: u64,
}

fn parse_audio_mux_element(
    payload: &[u8],
    previous: Option<&LatmConfig>,
) -> Result<ParsedMux, String> {
    let mut bits = BitReader::new(payload);
    let used_same_config = bits.bit()?;
    let config = if used_same_config {
        previous
            .cloned()
            .ok_or_else(|| "useSameStreamMux precedes StreamMuxConfig".to_string())?
    } else {
        parse_stream_mux_config(&mut bits)?
    };
    if config.streams.is_empty() || config.streams.len() > MAX_LATM_STREAMS {
        return Err("LATM stream count is outside the safety limit".into());
    }
    let mut access_units = 0_u64;
    let mut output_samples = 0_u64;
    for _ in 0..=config.num_subframes {
        for stream in &config.streams {
            let payload_length = match stream.frame_length_type {
                0 => {
                    let mut length = 0_usize;
                    loop {
                        let part = bits.read(8)? as usize;
                        length = length
                            .checked_add(part)
                            .ok_or_else(|| "LATM payload length overflow".to_string())?;
                        if part != 255 {
                            break;
                        }
                    }
                    length
                }
                1 => stream
                    .fixed_frame_length
                    .ok_or_else(|| "LATM fixed frame length is absent".to_string())?,
                _ => return Err("unsupported LATM frameLengthType for AAC".into()),
            };
            bits.skip(
                payload_length
                    .checked_mul(8)
                    .ok_or_else(|| "LATM payload bit length overflow".to_string())?,
            )?;
            access_units += 1;
            output_samples += u64::from(stream.asc.frame_samples);
        }
    }
    if bits.remaining() > 7 || !bits.remaining_bits_are_zero() {
        return Err("LATM frame has non-padding trailing bits".into());
    }
    Ok(ParsedMux {
        config,
        used_same_config,
        access_units,
        output_samples,
    })
}

fn parse_stream_mux_config(bits: &mut BitReader<'_>) -> Result<LatmConfig, String> {
    let audio_mux_version = bits.bit()?;
    if audio_mux_version && bits.bit()? {
        return Err("audioMuxVersionA is unsupported".into());
    }
    if audio_mux_version {
        let _tara_buffer_fullness = latm_get_value(bits)?;
    }
    let all_streams_same_time_framing = bits.bit()?;
    let num_subframes = bits.read(6)? as u8;
    let num_programs = bits.read(4)? as usize + 1;
    let mut streams: Vec<LatmStream> = Vec::new();
    for _ in 0..num_programs {
        let num_layers = bits.read(3)? as usize + 1;
        for _ in 0..num_layers {
            if streams.len() == MAX_LATM_STREAMS {
                return Err("LATM stream count exceeds safety limit".into());
            }
            let use_same_config = !streams.is_empty() && bits.bit()?;
            let asc = if use_same_config {
                streams
                    .last()
                    .ok_or_else(|| "LATM useSameConfig has no prior stream".to_string())?
                    .asc
                    .clone()
            } else if audio_mux_version {
                let asc_length = usize::try_from(latm_get_value(bits)?)
                    .map_err(|_| "ASC length does not fit memory".to_string())?;
                let mut asc_bits = bits.sub_reader(asc_length)?;
                let asc = parse_audio_specific_config(&mut asc_bits)?;
                if !asc_bits.remaining_bits_are_zero() {
                    return Err("AudioSpecificConfig has non-padding trailing bits".into());
                }
                asc
            } else {
                parse_audio_specific_config(bits)?
            };
            let frame_length_type = bits.read(3)? as u8;
            let fixed_frame_length = match frame_length_type {
                0 => {
                    bits.skip(8)?;
                    None
                }
                1 => Some(bits.read(9)? as usize),
                _ => return Err("unsupported LATM frameLengthType for AAC".into()),
            };
            streams.push(LatmStream {
                asc,
                frame_length_type,
                fixed_frame_length,
            });
        }
    }
    let other_data_present = bits.bit()?;
    if other_data_present {
        if audio_mux_version {
            let _other_data_bits = latm_get_value(bits)?;
        } else {
            loop {
                let escape = bits.bit()?;
                bits.skip(8)?;
                if !escape {
                    break;
                }
            }
        }
    }
    if bits.bit()? {
        bits.skip(8)?;
    }
    Ok(LatmConfig {
        all_streams_same_time_framing,
        num_subframes,
        streams,
    })
}

pub(crate) fn parse_asc_bytes(bytes: &[u8]) -> Result<AscInfo, String> {
    parse_audio_specific_config(&mut BitReader::new(bytes))
}

pub(crate) fn parse_usac_config_bytes(bytes: &[u8]) -> Result<UsacConfigInfo, String> {
    let mut bits = BitReader::new(bytes);
    let audio_object_type = read_audio_object_type(&mut bits)?;
    if audio_object_type != 42 {
        return Err(format!(
            "AudioSpecificConfig object type {audio_object_type} is not MPEG-D USAC"
        ));
    }
    let outer_sample_rate = read_sample_rate(&mut bits)?;
    let outer_channel_configuration = bits.read(4)? as u8;

    let frequency_index = bits.read(5)? as usize;
    let output_sample_rate_hz = if frequency_index == 31 {
        let frequency = bits.read(24)? as u32;
        if frequency == 0 {
            return Err("zero explicit USAC sampling frequency".into());
        }
        frequency
    } else {
        USAC_SAMPLE_RATES
            .get(frequency_index)
            .copied()
            .flatten()
            .ok_or_else(|| "reserved USAC sampling-frequency index".to_string())?
    };
    if output_sample_rate_hz != outer_sample_rate {
        return Err("outer and USAC sampling frequencies disagree".into());
    }

    let core_sbr_frame_length_index = bits.read(3)? as u8;
    let frame_samples = match core_sbr_frame_length_index {
        0 => 768,
        1 => 1_024,
        2 | 3 => 2_048,
        4 => 4_096,
        _ => return Err("reserved USAC core/SBR frame-length index".into()),
    };
    let channel_configuration_index = bits.read(5)? as u8;
    if channel_configuration_index > 15
        || outer_channel_configuration != channel_configuration_index
    {
        return Err("outer and USAC channel configurations disagree".into());
    }
    let output_channels = match channel_configuration_index {
        0 => {
            let channels = read_escape_value(&mut bits, 5, 8, 16)?;
            if !(1..=255).contains(&channels) {
                return Err("USAC explicit channel count is outside 1..=255".into());
            }
            for _ in 0..channels {
                bits.read(5)?;
            }
            channels as u16
        }
        1 => 1,
        2 | 8 => 2,
        _ => return Err("unsupported USAC channel-configuration index".into()),
    };

    let element_count = read_escape_value(&mut bits, 4, 8, 16)?
        .checked_add(1)
        .ok_or_else(|| "USAC element count overflow".to_string())?;
    if element_count > 16 {
        return Err("USAC configuration exceeds 16 elements".into());
    }
    let sbr = core_sbr_frame_length_index >= 2;
    let mut described_channels = 0_u16;
    let mut uni_drc_config_present = false;
    for _ in 0..element_count {
        match bits.read(2)? {
            0 => {
                described_channels = described_channels.saturating_add(1);
                validate_xhe_tw_mdct(&mut bits)?;
                if sbr {
                    skip_usac_sbr_config(&mut bits)?;
                }
            }
            1 => {
                described_channels = described_channels.saturating_add(2);
                validate_xhe_tw_mdct(&mut bits)?;
                if sbr {
                    skip_usac_sbr_config(&mut bits)?;
                    let stereo_config_index = bits.read(2)? as u8;
                    if stereo_config_index > 0 {
                        skip_usac_mps212_config(&mut bits, stereo_config_index)?;
                    }
                }
            }
            2 => described_channels = described_channels.saturating_add(1),
            3 => {
                let extension_type = read_escape_value(&mut bits, 4, 8, 16)?;
                let extension_bytes = read_escape_value(&mut bits, 4, 8, 16)?;
                if extension_bytes >= 768 {
                    return Err("USAC extension-element config exceeds 767 bytes".into());
                }
                if bits.bit()? {
                    read_escape_value(&mut bits, 8, 16, 0)?;
                }
                bits.bit()?;
                bits.skip(
                    usize::try_from(extension_bytes)
                        .map_err(|_| "USAC extension length overflow")?
                        .checked_mul(8)
                        .ok_or("USAC extension bit length overflow")?,
                )?;
                uni_drc_config_present |= extension_type == 4 && extension_bytes > 0;
            }
            _ => unreachable!(),
        }
        if described_channels > 2 {
            return Err("xHE-AAC configuration exceeds the mono/stereo profile".into());
        }
    }
    if described_channels != output_channels {
        return Err(format!(
            "USAC elements describe {described_channels} channels but the configuration declares {output_channels}"
        ));
    }

    let mut loudness_info_present = false;
    if bits.bit()? {
        let extension_count = read_escape_value(&mut bits, 2, 4, 8)?
            .checked_add(1)
            .ok_or_else(|| "USAC config-extension count overflow".to_string())?;
        if extension_count > 16 {
            return Err("USAC configuration exceeds 16 config extensions".into());
        }
        for _ in 0..extension_count {
            let extension_type = read_escape_value(&mut bits, 4, 8, 16)?;
            let extension_bytes = read_escape_value(&mut bits, 4, 8, 16)?;
            if extension_bytes > 768 {
                return Err("USAC config extension exceeds 768 bytes".into());
            }
            let byte_count = usize::try_from(extension_bytes)
                .map_err(|_| "USAC config-extension length overflow")?;
            if extension_type == 0 {
                for _ in 0..byte_count {
                    if bits.read(8)? != 0xa5 {
                        return Err("USAC fill extension contains a non-0xa5 byte".into());
                    }
                }
            } else {
                bits.skip(
                    byte_count
                        .checked_mul(8)
                        .ok_or("USAC config-extension bit length overflow")?,
                )?;
            }
            loudness_info_present |= extension_type == 2 && extension_bytes > 0;
        }
    }
    if !bits.remaining_bits_are_zero() {
        return Err("USAC AudioSpecificConfig has non-padding trailing bits".into());
    }

    Ok(UsacConfigInfo {
        audio_object_type,
        output_sample_rate_hz,
        core_sbr_frame_length_index,
        channel_configuration_index,
        output_channels,
        frame_samples,
        element_count: element_count as u16,
        uni_drc_config_present,
        loudness_info_present,
    })
}

const USAC_SAMPLE_RATES: [Option<u32>; 31] = [
    Some(96_000),
    Some(88_200),
    Some(64_000),
    Some(48_000),
    Some(44_100),
    Some(32_000),
    Some(24_000),
    Some(22_050),
    Some(16_000),
    Some(12_000),
    Some(11_025),
    Some(8_000),
    Some(7_350),
    None,
    None,
    Some(57_600),
    Some(51_200),
    Some(40_000),
    Some(38_400),
    Some(34_150),
    Some(28_800),
    Some(25_600),
    Some(20_000),
    Some(19_200),
    Some(17_075),
    Some(14_400),
    Some(12_800),
    Some(9_600),
    None,
    None,
    None,
];

fn validate_xhe_tw_mdct(bits: &mut BitReader<'_>) -> Result<(), String> {
    if bits.bit()? {
        return Err("xHE-AAC requires tw_mdct to be zero".into());
    }
    bits.bit()?;
    Ok(())
}

fn skip_usac_sbr_config(bits: &mut BitReader<'_>) -> Result<(), String> {
    bits.skip(3 + 4 + 4)?;
    let extra_one = bits.bit()?;
    let extra_two = bits.bit()?;
    if extra_one {
        bits.skip(2 + 1 + 2)?;
    }
    if extra_two {
        bits.skip(2 + 2 + 1 + 1)?;
    }
    Ok(())
}

fn skip_usac_mps212_config(
    bits: &mut BitReader<'_>,
    stereo_config_index: u8,
) -> Result<(), String> {
    bits.skip(3 + 3)?;
    let temporal_shape = bits.read(2)? as u8;
    if bits.read(2)? > 2 {
        return Err("reserved USAC MPS decorrelation configuration".into());
    }
    bits.skip(1 + 1)?;
    if bits.bit()? && bits.read(5)? > 28 {
        return Err("USAC MPS phase bands exceed 28".into());
    }
    if stereo_config_index > 1 {
        if bits.read(5)? > 28 {
            return Err("USAC MPS residual bands exceed 28".into());
        }
        bits.bit()?;
    }
    if temporal_shape == 2 {
        bits.bit()?;
    }
    Ok(())
}

fn read_escape_value(
    bits: &mut BitReader<'_>,
    first_bits: usize,
    second_bits: usize,
    third_bits: usize,
) -> Result<u64, String> {
    let first_max = (1_u64 << first_bits) - 1;
    let second_max = (1_u64 << second_bits) - 1;
    let mut value = bits.read(first_bits)?;
    if value == first_max {
        let second = bits.read(second_bits)?;
        value = value
            .checked_add(second)
            .ok_or_else(|| "USAC escaped value overflow".to_string())?;
        if second == second_max && third_bits > 0 {
            value = value
                .checked_add(bits.read(third_bits)?)
                .ok_or_else(|| "USAC escaped value overflow".to_string())?;
        }
    }
    Ok(value)
}

fn parse_audio_specific_config(bits: &mut BitReader<'_>) -> Result<AscInfo, String> {
    let initial_audio_object_type = read_audio_object_type(bits)?;
    let core_sample_rate = read_sample_rate(bits)?;
    let mut channel_configuration = bits.read(4)? as u8;
    let mut audio_object_type = initial_audio_object_type;
    let mut output_sample_rate = core_sample_rate;
    let mut sbr_present = false;
    let mut ps_present = false;
    if matches!(initial_audio_object_type, 5 | 29) {
        sbr_present = true;
        ps_present = initial_audio_object_type == 29;
        output_sample_rate = read_sample_rate(bits)?;
        audio_object_type = read_audio_object_type(bits)?;
        if audio_object_type == 22 {
            channel_configuration = bits.read(4)? as u8;
        }
    }
    if !matches!(
        audio_object_type,
        1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23
    ) {
        return Err(format!(
            "unsupported AudioSpecificConfig object type {audio_object_type}"
        ));
    }
    if channel_configuration > 7 {
        return Err("reserved AudioSpecificConfig channelConfiguration".into());
    }
    if channel_configuration == 0 {
        return Err("program_config_element channel mapping is not supported".into());
    }
    let frame_length_flag = bits.bit()?;
    if bits.bit()? {
        bits.skip(14)?;
    }
    let extension_flag = bits.bit()?;
    if matches!(audio_object_type, 6 | 20) {
        bits.skip(3)?;
    }
    if extension_flag {
        if audio_object_type == 22 {
            bits.skip(16)?;
        }
        if matches!(audio_object_type, 17 | 19 | 20 | 23) {
            bits.skip(3)?;
        }
        bits.skip(1)?;
    }
    if !matches!(initial_audio_object_type, 5 | 29) && bits.remaining() >= 16 {
        let checkpoint = bits.position();
        if bits.read(11)? == 0x02b7 {
            let extension_type = read_audio_object_type(bits)?;
            if extension_type == 5 {
                sbr_present = bits.bit()?;
                if sbr_present {
                    output_sample_rate = read_sample_rate(bits)?;
                    if bits.remaining() >= 12 {
                        let ps_checkpoint = bits.position();
                        if bits.read(11)? == 0x0548 {
                            ps_present = bits.bit()?;
                        } else {
                            bits.set_position(ps_checkpoint)?;
                        }
                    }
                }
            }
        } else {
            bits.set_position(checkpoint)?;
        }
    }
    let core_frame_samples = if frame_length_flag { 960 } else { 1024 };
    let frame_samples = if sbr_present {
        core_frame_samples * 2
    } else {
        core_frame_samples
    };
    let output_channels = if ps_present {
        Some(2)
    } else {
        channel_count(channel_configuration)
    };
    if sbr_present && output_sample_rate < core_sample_rate {
        return Err("SBR output sampling frequency is below the core rate".into());
    }
    if ps_present && output_channels != Some(2) {
        return Err("Parametric Stereo must expose two output channels".into());
    }
    Ok(AscInfo {
        audio_object_type,
        core_sample_rate_hz: core_sample_rate,
        output_sample_rate_hz: output_sample_rate,
        channel_configuration,
        output_channels,
        sbr_present,
        ps_present,
        frame_samples,
    })
}

fn read_audio_object_type(bits: &mut BitReader<'_>) -> Result<u8, String> {
    let value = bits.read(5)? as u8;
    if value == 31 {
        let extended = bits.read(6)? as u8;
        32_u8
            .checked_add(extended)
            .ok_or_else(|| "audio object type overflow".to_string())
    } else {
        Ok(value)
    }
}

fn read_sample_rate(bits: &mut BitReader<'_>) -> Result<u32, String> {
    let index = bits.read(4)? as usize;
    if index == 15 {
        let explicit = bits.read(24)? as u32;
        if explicit == 0 {
            Err("zero explicit AAC sampling frequency".into())
        } else {
            Ok(explicit)
        }
    } else {
        ADTS_SAMPLE_RATES
            .get(index)
            .copied()
            .ok_or_else(|| "reserved AAC sampling-frequency index".into())
    }
}

fn channel_count(configuration: u8) -> Option<u8> {
    match configuration {
        1 => Some(1),
        2 => Some(2),
        3 => Some(3),
        4 => Some(4),
        5 => Some(5),
        6 => Some(6),
        7 => Some(8),
        _ => None,
    }
}

fn latm_get_value(bits: &mut BitReader<'_>) -> Result<u64, String> {
    let bytes = bits.read(2)? as usize + 1;
    let mut value = 0_u64;
    for _ in 0..bytes {
        value = value
            .checked_shl(8)
            .ok_or_else(|| "LATM value overflow".to_string())?
            | bits.read(8)?;
    }
    Ok(value)
}

fn discard_exact(reader: &mut impl Read, bytes: usize) -> std::io::Result<()> {
    let copied = std::io::copy(&mut reader.take(bytes as u64), &mut std::io::sink())?;
    if copied == bytes as u64 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated AAC payload",
        ))
    }
}

#[derive(Clone)]
struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            end: data.len() * 8,
        }
    }

    fn bit(&mut self) -> Result<bool, String> {
        Ok(self.read(1)? != 0)
    }

    fn read(&mut self, count: usize) -> Result<u64, String> {
        if count > 64 || self.remaining() < count {
            return Err("truncated AAC bit syntax".into());
        }
        let mut value = 0_u64;
        for _ in 0..count {
            let byte = self.data[self.position / 8];
            let shift = 7 - self.position % 8;
            value = (value << 1) | u64::from((byte >> shift) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn skip(&mut self, count: usize) -> Result<(), String> {
        if self.remaining() < count {
            return Err("AAC bit length exceeds frame".into());
        }
        self.position += count;
        Ok(())
    }

    fn sub_reader(&mut self, count: usize) -> Result<Self, String> {
        if self.remaining() < count {
            return Err("AudioSpecificConfig exceeds StreamMuxConfig".into());
        }
        let start = self.position;
        self.position += count;
        Ok(Self {
            data: self.data,
            position: start,
            end: start + count,
        })
    }

    fn remaining(&self) -> usize {
        self.end - self.position
    }

    fn position(&self) -> usize {
        self.position
    }

    fn set_position(&mut self, position: usize) -> Result<(), String> {
        if position > self.end {
            return Err("AAC bit position exceeds syntax bounds".into());
        }
        self.position = position;
        Ok(())
    }

    fn remaining_bits_are_zero(&self) -> bool {
        let mut copy = self.clone();
        while copy.remaining() > 0 {
            if copy.bit().unwrap_or(true) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl BitWriter {
        fn write(&mut self, value: u64, count: usize) {
            for shift in (0..count).rev() {
                if self.bit.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let index = self.bit / 8;
                self.bytes[index] |= (((value >> shift) & 1) as u8) << (7 - self.bit % 8);
                self.bit += 1;
            }
        }

        fn bytes(self) -> Vec<u8> {
            self.bytes
        }

        fn write_escape(&mut self, mut value: u64, first: usize, second: usize, third: usize) {
            for width in [first, second, third] {
                if width == 0 {
                    break;
                }
                let maximum = (1_u64 << width) - 1;
                let part = value.min(maximum);
                self.write(part, width);
                if part < maximum {
                    break;
                }
                value -= part;
            }
        }
    }

    fn adts_frame(
        profile: u8,
        sample_rate_index: u8,
        channels: u8,
        protected: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let header_size = if protected { 9 } else { 7 };
        let length = header_size + payload.len();
        let fullness = 0x07ff_u16;
        let mut header = vec![0_u8; header_size];
        header[0] = 0xff;
        header[1] = if protected { 0xf0 } else { 0xf1 };
        header[2] = (profile << 6) | (sample_rate_index << 2) | ((channels >> 2) & 1);
        header[3] = (channels & 3) << 6 | ((length >> 11) & 3) as u8;
        header[4] = (length >> 3) as u8;
        header[5] = ((length & 7) as u8) << 5 | (fullness >> 6) as u8;
        header[6] = ((fullness & 0x3f) << 2) as u8;
        if protected {
            header[7..9].copy_from_slice(&0x1234_u16.to_be_bytes());
        }
        header.extend_from_slice(payload);
        header
    }

    fn loas_frame(use_same: bool, sample_rate_index: u8, payload: &[u8]) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.write(u64::from(use_same), 1);
        if !use_same {
            bits.write(0, 1); // audioMuxVersion
            bits.write(1, 1); // allStreamsSameTimeFraming
            bits.write(0, 6); // numSubFrames
            bits.write(0, 4); // numProgram
            bits.write(0, 3); // numLayer
            bits.write(2, 5); // AAC LC
            bits.write(u64::from(sample_rate_index), 4);
            bits.write(2, 4); // stereo
            bits.write(0, 1); // frameLengthFlag
            bits.write(0, 1); // dependsOnCoreCoder
            bits.write(0, 1); // extensionFlag
            bits.write(0, 3); // frameLengthType
            bits.write(0xff, 8); // latmBufferFullness
            bits.write(0, 1); // otherDataPresent
            bits.write(0, 1); // crcCheckPresent
        }
        bits.write(payload.len() as u64, 8);
        for &byte in payload {
            bits.write(u64::from(byte), 8);
        }
        let mux = bits.bytes();
        let mut frame = vec![
            0x56,
            0xe0 | ((mux.len() >> 8) & 0x1f) as u8,
            mux.len() as u8,
        ];
        frame.extend(mux);
        frame
    }

    fn audit_bytes(bytes: &[u8]) -> ContainerAudit {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        let reopened = file.reopen().unwrap();
        audit(file.path(), reopened, bytes.len() as u64).unwrap()
    }

    #[test]
    fn audits_adts_headers_crc_timing_and_config_continuity() {
        let mut bytes = adts_frame(1, 3, 2, true, &[1, 2, 3, 4]);
        bytes.extend(adts_frame(1, 3, 2, false, &[5, 6, 7]));
        let audit = audit_bytes(&bytes);
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "aac-adts");
        assert_eq!(audit.properties["frames"], 2);
        assert_eq!(audit.properties["decoded_samples"], 2048);
        assert_eq!(audit.properties["crc_protected_frames"], 1);

        let mut changed = adts_frame(1, 3, 2, false, &[1]);
        changed.extend(adts_frame(1, 4, 2, false, &[2]));
        let audit = audit_bytes(&changed);
        assert!(!audit.passed);
        assert!(audit.layers[1]
            .checks
            .iter()
            .any(|check| { check.rule_id == "FORGE-AAC-ADTS-CONFIG-CONTINUITY" && !check.passed }));
    }

    #[test]
    fn rejects_truncated_adts_and_loas_frames_without_allocating_declared_size() {
        let mut adts = adts_frame(1, 3, 2, false, &[1, 2, 3, 4]);
        adts.pop();
        let audit = audit_bytes(&adts);
        assert!(!audit.passed);
        assert!(audit.layers[0]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-AAC-ADTS-BOUNDS" && !check.passed));

        let loas = vec![0x56, 0xff, 0xff, 0];
        let audit = audit_bytes(&loas);
        assert!(!audit.passed);
        assert!(audit.layers[0]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-AAC-LOAS-BOUNDS" && !check.passed));
    }

    #[test]
    fn parses_loas_stream_mux_config_reuse_and_payload_bounds() {
        let mut bytes = loas_frame(false, 3, &[1, 2, 3, 4]);
        bytes.extend(loas_frame(true, 3, &[5, 6, 7]));
        let audit = audit_bytes(&bytes);
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "aac-loas");
        assert_eq!(audit.properties["frames"], 2);
        assert_eq!(audit.properties["stream_mux_config_repetitions"], 1);
        assert_eq!(audit.properties["stream_mux_config_reuses"], 1);

        let audit = audit_bytes(&loas_frame(true, 3, &[1]));
        assert!(!audit.passed);
        assert!(audit.layers[1]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-AAC-LATM-MUX" && !check.passed));
    }

    #[test]
    fn parses_explicit_and_implicit_he_aac_and_parametric_stereo() {
        let mut explicit = BitWriter::default();
        explicit.write(29, 5); // explicit PS (and SBR)
        explicit.write(7, 4); // 22.05 kHz core
        explicit.write(1, 4); // mono core
        explicit.write(4, 4); // 44.1 kHz output
        explicit.write(2, 5); // AAC LC core
        explicit.write(0, 3); // GASpecificConfig flags
        let explicit = parse_asc_bytes(&explicit.bytes()).unwrap();
        assert!(explicit.sbr_present);
        assert!(explicit.ps_present);
        assert_eq!(explicit.core_sample_rate_hz, 22_050);
        assert_eq!(explicit.output_sample_rate_hz, 44_100);
        assert_eq!(explicit.output_channels, Some(2));
        assert_eq!(explicit.frame_samples, 2048);

        let mut implicit = BitWriter::default();
        implicit.write(2, 5); // backwards-compatible AAC LC
        implicit.write(7, 4); // 22.05 kHz core
        implicit.write(1, 4); // mono core
        implicit.write(0, 3); // GASpecificConfig flags
        implicit.write(0x02b7, 11); // syncExtensionType
        implicit.write(5, 5); // SBR
        implicit.write(1, 1); // sbrPresentFlag
        implicit.write(4, 4); // 44.1 kHz output
        implicit.write(0x0548, 11); // syncExtensionType for PS
        implicit.write(1, 1); // psPresentFlag
        let implicit = parse_asc_bytes(&implicit.bytes()).unwrap();
        assert!(implicit.sbr_present);
        assert!(implicit.ps_present);
        assert_eq!(implicit.output_sample_rate_hz, 44_100);
        assert_eq!(implicit.output_channels, Some(2));
    }

    #[test]
    fn parses_bounded_xhe_aac_usac_and_metadata_extensions() {
        let mut bits = BitWriter::default();
        bits.write(31, 5); // escaped Audio Object Type
        bits.write(10, 6); // 32 + 10 = USAC (42)
        bits.write(3, 4); // outer 48 kHz
        bits.write(2, 4); // outer stereo
        bits.write(3, 5); // USAC 48 kHz
        bits.write(1, 3); // 1024 samples, no SBR
        bits.write(2, 5); // USAC stereo
        bits.write_escape(1, 4, 8, 16); // two elements minus one
        bits.write(1, 2); // channel-pair element
        bits.write(0, 1); // xHE tw_mdct
        bits.write(1, 1); // noise filling
        bits.write(3, 2); // extension element
        bits.write_escape(4, 4, 8, 16); // MPEG-D UniDRC
        bits.write_escape(1, 4, 8, 16); // one config byte
        bits.write(0, 1); // no default payload length
        bits.write(0, 1); // payload fragmentation flag
        bits.write(0, 8); // bounded UniDRC configuration payload
        bits.write(1, 1); // usacConfigExtensionPresent
        bits.write_escape(0, 2, 4, 8); // one extension minus one
        bits.write_escape(2, 4, 8, 16); // loudnessInfoSet
        bits.write_escape(1, 4, 8, 16); // one payload byte
        bits.write(0, 8);

        let config = parse_usac_config_bytes(&bits.bytes()).unwrap();
        assert_eq!(config.audio_object_type, 42);
        assert_eq!(config.output_sample_rate_hz, 48_000);
        assert_eq!(config.output_channels, 2);
        assert_eq!(config.frame_samples, 1_024);
        assert_eq!(config.element_count, 2);
        assert!(config.uni_drc_config_present);
        assert!(config.loudness_info_present);
    }

    #[test]
    fn xhe_aac_rejects_outer_usac_configuration_mismatch() {
        let mut bits = BitWriter::default();
        bits.write(31, 5);
        bits.write(10, 6);
        bits.write(3, 4); // outer 48 kHz
        bits.write(2, 4);
        bits.write(4, 5); // USAC 44.1 kHz
        bits.write(1, 3);
        bits.write(2, 5);
        assert!(parse_usac_config_bytes(&bits.bytes()).is_err());
    }
}
