//! Read-only DSF and uncompressed DSDIFF analysis.
//!
//! Container parsing follows Sony's DSF File Format Specification 1.01 and
//! Philips' DSDIFF 1.5 specification. DSD-to-PCM conversion is deliberately
//! identified as a Forge engineering policy rather than part of either
//! container specification or ITU-R BS.1770:
//!
//! - 1-bit samples are mapped to -1.0/+1.0.
//! - Cascaded 31-tap Blackman-windowed half-band FIR filters decimate to
//!   88.2 kHz (44.1 kHz family) or 96 kHz (48 kHz family).
//! - A 127-tap Blackman-windowed sinc FIR with a 21 kHz -6 dB cutoff removes
//!   residual ultrasonic noise before loudness and peak measurement.
//! - Filters start from zero state and output is truncated to the complete
//!   decimation interval; no invented tail samples are appended.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use crate::wav::{default_channel_roles, ChannelRole, PcmKind};
use rayon::prelude::*;
use serde_json::json;
use std::f64::consts::PI;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_CHANNELS: u16 = 32;
const MAX_CHUNKS: usize = 100_000;
const MAX_CONTROL_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SOURCE_SAMPLE_RATE: u32 = 49_152_000;
const OUTPUT_CHUNK_FRAMES: usize = 4096;
const HALF_BAND_TAPS: usize = 31;
const OUTPUT_LOW_PASS_TAPS: usize = 127;
const OUTPUT_LOW_PASS_CUTOFF_HZ: f64 = 21_000.0;

pub const CONVERSION_POLICY: &str =
    "forge-dsd-pcm-v1: +/-1 mapping; cascaded 31-tap Blackman half-band FIR; \
     88.2/96 kHz output; 127-tap Blackman-sinc 21 kHz cutoff; zero initial state; \
     complete decimation intervals only";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdFormat {
    Dsf,
    Dsdiff,
}

impl DsdFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Dsf => "dsf",
            Self::Dsdiff => "dsdiff",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdBitOrder {
    LeastSignificantFirst,
    MostSignificantFirst,
}

impl DsdBitOrder {
    fn name(self) -> &'static str {
        match self {
            Self::LeastSignificantFirst => "lsb-first",
            Self::MostSignificantFirst => "msb-first",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DsdInfo {
    pub format: DsdFormat,
    pub source_sample_rate: u32,
    pub output_sample_rate: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRole>,
    pub source_samples_per_channel: u64,
    pub output_frames: u64,
    pub bit_order: DsdBitOrder,
    pub compression: String,
    pub data_offset: u64,
    pub data_size: u64,
    pub block_size_per_channel: Option<u32>,
    pub chunk_count: usize,
    layout: DsdLayout,
}

#[derive(Debug, Clone, Copy)]
enum DsdLayout {
    Dsf { block_size_per_channel: u32 },
    Dsdiff,
}

pub fn looks_like_dsd(header: &[u8]) -> bool {
    header.starts_with(b"DSD ")
        || (header.len() >= 16 && &header[..4] == b"FRM8" && &header[12..16] == b"DSD ")
}

pub fn probe(path: &Path) -> Result<DsdInfo, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    parse(&mut file, file_size).map_err(|error| format!("{}: {error}", path.display()))
}

fn parse(file: &mut File, file_size: u64) -> Result<DsdInfo, String> {
    if file_size < 4 {
        return Err("truncated DSD signature".into());
    }
    let signature = read_at::<4>(file, 0, file_size)?;
    match &signature {
        b"DSD " => parse_dsf(file, file_size),
        b"FRM8" => parse_dsdiff(file, file_size),
        _ => Err("not a DSF or DSDIFF file".into()),
    }
}

fn parse_dsf(file: &mut File, file_size: u64) -> Result<DsdInfo, String> {
    if file_size < 92 {
        return Err("truncated DSF header".into());
    }
    let dsd_header = read_vec_at(file, 0, 28, file_size)?;
    let dsd_chunk_size = le_u64(&dsd_header[4..12]);
    let declared_file_size = le_u64(&dsd_header[12..20]);
    let metadata_offset = le_u64(&dsd_header[20..28]);
    if dsd_chunk_size != 28 {
        return Err(format!(
            "DSF DSD chunk size must be 28, observed {dsd_chunk_size}"
        ));
    }
    if declared_file_size != file_size {
        return Err(format!(
            "DSF declared file size {declared_file_size} does not match {file_size}"
        ));
    }
    if metadata_offset != 0 && !(metadata_offset >= 92 && metadata_offset < file_size) {
        return Err(format!(
            "DSF metadata offset {metadata_offset} is outside the file"
        ));
    }

    let fmt = read_vec_at(file, 28, 52, file_size)?;
    if &fmt[..4] != b"fmt " {
        return Err("DSF fmt chunk must immediately follow the DSD chunk".into());
    }
    let fmt_size = le_u64(&fmt[4..12]);
    if fmt_size != 52 {
        return Err(format!(
            "DSF fmt chunk size must be 52, observed {fmt_size}"
        ));
    }
    let format_version = le_u32(&fmt[12..16]);
    let format_id = le_u32(&fmt[16..20]);
    let channel_type = le_u32(&fmt[20..24]);
    let channels_u32 = le_u32(&fmt[24..28]);
    let source_sample_rate = le_u32(&fmt[28..32]);
    let bits_per_sample = le_u32(&fmt[32..36]);
    let source_samples_per_channel = le_u64(&fmt[36..44]);
    let block_size_per_channel = le_u32(&fmt[44..48]);
    let reserved = le_u32(&fmt[48..52]);

    if format_version != 1 || format_id != 0 {
        return Err(format!(
            "unsupported DSF format version/id {format_version}/{format_id}"
        ));
    }
    let channels = u16::try_from(channels_u32)
        .map_err(|_| format!("DSF channel count {channels_u32} is too large"))?;
    validate_channels(channels)?;
    validate_dsf_channel_type(channel_type, channels)?;
    let bit_order = match bits_per_sample {
        1 => DsdBitOrder::LeastSignificantFirst,
        8 => DsdBitOrder::MostSignificantFirst,
        other => return Err(format!("unsupported DSF bits-per-sample value {other}")),
    };
    if block_size_per_channel == 0 || block_size_per_channel > 1024 * 1024 {
        return Err(format!(
            "DSF block size per channel {block_size_per_channel} is outside 1..=1048576"
        ));
    }
    if reserved != 0 {
        return Err("DSF fmt reserved field is non-zero".into());
    }
    let (output_sample_rate, decimation) = output_geometry(source_sample_rate)?;
    let output_frames = source_samples_per_channel / u64::from(decimation);

    let data_header = read_vec_at(file, 80, 12, file_size)?;
    if &data_header[..4] != b"data" {
        return Err("DSF data chunk must immediately follow the fmt chunk".into());
    }
    let data_chunk_size = le_u64(&data_header[4..12]);
    if data_chunk_size < 12 {
        return Err("DSF data chunk size is smaller than its header".into());
    }
    let data_size = data_chunk_size - 12;
    let data_offset = 92_u64;
    let data_end = data_offset
        .checked_add(data_size)
        .ok_or("DSF data chunk size overflow")?;
    let expected_data_end = if metadata_offset == 0 {
        file_size
    } else {
        metadata_offset
    };
    if data_end != expected_data_end {
        return Err(format!(
            "DSF data ends at {data_end}, expected {expected_data_end}"
        ));
    }
    let bytes_per_channel = source_samples_per_channel.div_ceil(8);
    let blocks_per_channel = bytes_per_channel.div_ceil(u64::from(block_size_per_channel));
    let required_data_size = blocks_per_channel
        .checked_mul(u64::from(block_size_per_channel))
        .and_then(|value| value.checked_mul(u64::from(channels)))
        .ok_or("DSF padded data size overflow")?;
    if data_size != required_data_size {
        return Err(format!(
            "DSF data size {data_size} does not match padded sample geometry {required_data_size}"
        ));
    }
    validate_dsf_padding(
        file,
        file_size,
        data_offset,
        channels,
        source_samples_per_channel,
        block_size_per_channel,
        bit_order,
    )?;
    if metadata_offset != 0 {
        let id3 = read_at::<3>(file, metadata_offset, file_size)?;
        if &id3 != b"ID3" {
            return Err("DSF metadata pointer does not reference an ID3v2 tag".into());
        }
    }

    Ok(DsdInfo {
        format: DsdFormat::Dsf,
        source_sample_rate,
        output_sample_rate,
        channels,
        channel_roles: dsf_channel_roles(channel_type, channels),
        source_samples_per_channel,
        output_frames,
        bit_order,
        compression: "DSD raw".into(),
        data_offset,
        data_size,
        block_size_per_channel: Some(block_size_per_channel),
        chunk_count: if metadata_offset == 0 { 3 } else { 4 },
        layout: DsdLayout::Dsf {
            block_size_per_channel,
        },
    })
}

fn parse_dsdiff(file: &mut File, file_size: u64) -> Result<DsdInfo, String> {
    if file_size < 16 {
        return Err("truncated DSDIFF FRM8 header".into());
    }
    let header = read_vec_at(file, 0, 16, file_size)?;
    if &header[..4] != b"FRM8" || &header[12..16] != b"DSD " {
        return Err("invalid DSDIFF FRM8/DSD signature".into());
    }
    let declared_size = be_u64(&header[4..12]);
    let declared_end = 12_u64
        .checked_add(declared_size)
        .ok_or("DSDIFF FRM8 size overflow")?;
    if declared_end != file_size {
        return Err(format!(
            "DSDIFF FRM8 ends at {declared_end}, actual file size is {file_size}"
        ));
    }
    if declared_size < 4 || !declared_size.is_multiple_of(2) {
        return Err("DSDIFF FRM8 size must include form type and be even".into());
    }

    let mut offset = 16_u64;
    let mut chunk_count = 0_usize;
    let mut version = None;
    let mut source_sample_rate = None;
    let mut channel_ids: Option<Vec<[u8; 4]>> = None;
    let mut compression = None;
    let mut data = None;
    let mut saw_dst = false;
    let mut saw_audio = false;
    while offset < file_size {
        chunk_count = chunk_count
            .checked_add(1)
            .ok_or("DSDIFF chunk count overflow")?;
        if chunk_count > MAX_CHUNKS {
            return Err(format!("DSDIFF chunk count exceeds {MAX_CHUNKS}"));
        }
        let (id, size, body, next) = dff_chunk(file, offset, file_size)?;
        if chunk_count == 1 && &id != b"FVER" {
            return Err("DSDIFF FVER must be the first local chunk".into());
        }
        match &id {
            b"FVER" => {
                if version.is_some() || chunk_count != 1 || size != 4 {
                    return Err("DSDIFF requires exactly one 4-byte FVER chunk".into());
                }
                version = Some(be_u32(&read_vec_at(file, body, 4, file_size)?));
            }
            b"PROP" => {
                if saw_audio {
                    return Err("DSDIFF PROP must precede sound data".into());
                }
                if source_sample_rate.is_some() || channel_ids.is_some() || compression.is_some() {
                    return Err("DSDIFF contains multiple PROP chunks".into());
                }
                let props = parse_dff_properties(file, body, size, file_size, &mut chunk_count)?;
                source_sample_rate = props.sample_rate;
                channel_ids = props.channel_ids;
                compression = props.compression;
            }
            b"DSD " => {
                if source_sample_rate.is_none() || channel_ids.is_none() || compression.is_none() {
                    return Err("DSDIFF PROP must precede DSD sound data".into());
                }
                if data.replace((body, size)).is_some() {
                    return Err("DSDIFF contains multiple DSD sound chunks".into());
                }
                saw_audio = true;
            }
            b"DST " => {
                saw_dst = true;
                saw_audio = true;
            }
            _ => {}
        }
        offset = next;
    }
    if offset != file_size {
        return Err("DSDIFF chunk alignment exceeds the FRM8 boundary".into());
    }
    let version = version.ok_or("DSDIFF is missing FVER")?;
    if version != 0x0105_0000 {
        return Err(format!(
            "unsupported DSDIFF version 0x{version:08x}; expected 1.5"
        ));
    }
    if saw_dst {
        return Err("DST-compressed DSDIFF is not supported by the read-only PCM adapter".into());
    }
    let source_sample_rate = source_sample_rate.ok_or("DSDIFF PROP is missing FS")?;
    let channel_ids = channel_ids.ok_or("DSDIFF PROP is missing CHNL")?;
    validate_dff_channel_ids(&channel_ids)?;
    let channels = u16::try_from(channel_ids.len())
        .map_err(|_| "DSDIFF channel count is too large".to_string())?;
    validate_channels(channels)?;
    let compression = compression.ok_or("DSDIFF PROP is missing CMPR")?;
    if &compression != b"DSD " {
        return Err(format!(
            "unsupported DSDIFF compression {}",
            String::from_utf8_lossy(&compression)
        ));
    }
    let (data_offset, data_size) = data.ok_or("DSDIFF is missing DSD sound data")?;
    if data_size % u64::from(channels) != 0 {
        return Err(format!(
            "DSDIFF DSD data size {data_size} is not divisible by {channels} channels"
        ));
    }
    let source_samples_per_channel = data_size
        .checked_div(u64::from(channels))
        .and_then(|bytes| bytes.checked_mul(8))
        .ok_or("DSDIFF sample count overflow")?;
    let (output_sample_rate, decimation) = output_geometry(source_sample_rate)?;

    Ok(DsdInfo {
        format: DsdFormat::Dsdiff,
        source_sample_rate,
        output_sample_rate,
        channels,
        channel_roles: dff_channel_roles(&channel_ids),
        source_samples_per_channel,
        output_frames: source_samples_per_channel / u64::from(decimation),
        bit_order: DsdBitOrder::MostSignificantFirst,
        compression: "DSD raw".into(),
        data_offset,
        data_size,
        block_size_per_channel: None,
        chunk_count,
        layout: DsdLayout::Dsdiff,
    })
}

fn validate_dsf_padding(
    file: &mut File,
    file_size: u64,
    data_offset: u64,
    channels: u16,
    source_samples_per_channel: u64,
    block_size_per_channel: u32,
    bit_order: DsdBitOrder,
) -> Result<(), String> {
    if source_samples_per_channel == 0 {
        return Ok(());
    }
    let block_size = u64::from(block_size_per_channel);
    let bytes_per_channel = source_samples_per_channel.div_ceil(8);
    let rounds = bytes_per_channel.div_ceil(block_size);
    let last_valid_bytes = bytes_per_channel - (rounds - 1) * block_size;
    let round_stride = block_size
        .checked_mul(u64::from(channels))
        .ok_or("DSF padding geometry overflow")?;
    let last_round = data_offset
        .checked_add(
            (rounds - 1)
                .checked_mul(round_stride)
                .ok_or("DSF padding offset overflow")?,
        )
        .ok_or("DSF padding offset overflow")?;
    for channel in 0..u64::from(channels) {
        let block = last_round
            .checked_add(
                channel
                    .checked_mul(block_size)
                    .ok_or("DSF channel padding offset overflow")?,
            )
            .ok_or("DSF channel padding offset overflow")?;
        let remainder_bits = source_samples_per_channel % 8;
        if remainder_bits != 0 {
            let last_byte_offset = block
                .checked_add(last_valid_bytes - 1)
                .ok_or("DSF final byte offset overflow")?;
            let byte = read_at::<1>(file, last_byte_offset, file_size)?[0];
            let unused_mask = match bit_order {
                DsdBitOrder::LeastSignificantFirst => !((1_u8 << remainder_bits) - 1),
                DsdBitOrder::MostSignificantFirst => (1_u8 << (8 - remainder_bits)) - 1,
            };
            if byte & unused_mask != 0 {
                return Err(format!(
                    "DSF channel {} has non-zero unused bits after the declared sample count",
                    channel + 1
                ));
            }
        }
        let mut padding_offset = block
            .checked_add(last_valid_bytes)
            .ok_or("DSF padding offset overflow")?;
        let padding_end = block
            .checked_add(block_size)
            .ok_or("DSF padding end overflow")?;
        while padding_offset < padding_end {
            let size = (padding_end - padding_offset).min(64 * 1024);
            let padding = read_vec_at(file, padding_offset, size, file_size)?;
            if padding.iter().any(|byte| *byte != 0) {
                return Err(format!(
                    "DSF channel {} has non-zero block padding",
                    channel + 1
                ));
            }
            padding_offset += size;
        }
    }
    Ok(())
}

struct DffProperties {
    sample_rate: Option<u32>,
    channel_ids: Option<Vec<[u8; 4]>>,
    compression: Option<[u8; 4]>,
}

fn parse_dff_properties(
    file: &mut File,
    body: u64,
    size: u64,
    file_size: u64,
    chunk_count: &mut usize,
) -> Result<DffProperties, String> {
    if size < 4 {
        return Err("DSDIFF PROP chunk is truncated".into());
    }
    let prop_type = read_at::<4>(file, body, file_size)?;
    if &prop_type != b"SND " {
        return Err("DSDIFF PROP type must be SND".into());
    }
    let end = body.checked_add(size).ok_or("DSDIFF PROP size overflow")?;
    let mut offset = body + 4;
    let mut sample_rate = None;
    let mut channel_ids = None;
    let mut compression = None;
    while offset < end {
        *chunk_count = chunk_count
            .checked_add(1)
            .ok_or("DSDIFF nested chunk count overflow")?;
        if *chunk_count > MAX_CHUNKS {
            return Err(format!("DSDIFF chunk count exceeds {MAX_CHUNKS}"));
        }
        let (id, child_size, child_body, next) = dff_chunk(file, offset, end)?;
        match &id {
            b"FS  " => {
                if sample_rate.is_some() || child_size != 4 {
                    return Err("DSDIFF PROP requires exactly one 4-byte FS chunk".into());
                }
                sample_rate = Some(be_u32(&read_vec_at(file, child_body, 4, file_size)?));
            }
            b"CHNL" => {
                if channel_ids.is_some() || child_size < 2 {
                    return Err("DSDIFF PROP contains an invalid or duplicate CHNL".into());
                }
                if child_size > MAX_CONTROL_CHUNK_BYTES {
                    return Err("DSDIFF CHNL exceeds the control-chunk byte limit".into());
                }
                let value = read_vec_at(file, child_body, child_size, file_size)?;
                let count = usize::from(be_u16(&value[..2]));
                if count == 0 || count > usize::from(MAX_CHANNELS) {
                    return Err(format!(
                        "DSDIFF CHNL count {count} is outside 1..={MAX_CHANNELS}"
                    ));
                }
                let required = 2_usize
                    .checked_add(count.checked_mul(4).ok_or("CHNL size overflow")?)
                    .ok_or("CHNL size overflow")?;
                if value.len() != required {
                    return Err(format!(
                        "DSDIFF CHNL size {} does not match {count} identifiers",
                        value.len()
                    ));
                }
                let ids: Vec<[u8; 4]> = value[2..]
                    .chunks_exact(4)
                    .map(|bytes| bytes.try_into().unwrap())
                    .collect();
                for id in &ids {
                    validate_dff_identifier(id, "channel")?;
                }
                channel_ids = Some(ids);
            }
            b"CMPR" => {
                if compression.is_some() || child_size < 5 {
                    return Err("DSDIFF PROP contains an invalid or duplicate CMPR".into());
                }
                if child_size > MAX_CONTROL_CHUNK_BYTES {
                    return Err("DSDIFF CMPR exceeds the control-chunk byte limit".into());
                }
                let value = read_vec_at(file, child_body, child_size, file_size)?;
                let name_len = usize::from(value[4]);
                if 5_usize
                    .checked_add(name_len)
                    .is_none_or(|needed| needed > value.len())
                {
                    return Err("DSDIFF CMPR name exceeds its chunk".into());
                }
                compression = Some(value[..4].try_into().unwrap());
            }
            _ => {}
        }
        offset = next;
    }
    if offset != end {
        return Err("DSDIFF PROP child alignment exceeds its boundary".into());
    }
    Ok(DffProperties {
        sample_rate,
        channel_ids,
        compression,
    })
}

fn dff_chunk(file: &mut File, offset: u64, limit: u64) -> Result<([u8; 4], u64, u64, u64), String> {
    let header_end = offset
        .checked_add(12)
        .ok_or("DSDIFF chunk header overflow")?;
    if header_end > limit {
        return Err("truncated DSDIFF chunk header".into());
    }
    let header = read_vec_at(file, offset, 12, limit)?;
    let id: [u8; 4] = header[..4].try_into().unwrap();
    validate_dff_identifier(&id, "chunk")?;
    let size = be_u64(&header[4..12]);
    let body = header_end;
    let end = body.checked_add(size).ok_or("DSDIFF chunk size overflow")?;
    if end > limit {
        return Err(format!(
            "DSDIFF chunk {} exceeds its parent boundary",
            String::from_utf8_lossy(&id)
        ));
    }
    let next = end
        .checked_add(size % 2)
        .ok_or("DSDIFF chunk padding overflow")?;
    if next > limit {
        return Err("DSDIFF chunk pad byte exceeds its parent boundary".into());
    }
    if size % 2 == 1 {
        let pad = read_at::<1>(file, end, limit)?[0];
        if pad != 0 {
            return Err(format!(
                "DSDIFF chunk {} has a non-zero pad byte",
                String::from_utf8_lossy(&id)
            ));
        }
    }
    Ok((id, size, body, next))
}

fn validate_dff_identifier(id: &[u8; 4], kind: &str) -> Result<(), String> {
    if id[0] == b' ' || !id.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        return Err(format!(
            "DSDIFF {kind} identifier contains invalid characters: {:02x?}",
            id
        ));
    }
    Ok(())
}

fn validate_dff_channel_ids(ids: &[[u8; 4]]) -> Result<(), String> {
    for (index, id) in ids.iter().enumerate() {
        if ids[..index].contains(id) {
            return Err(format!(
                "DSDIFF CHNL contains duplicate identifier {}",
                String::from_utf8_lossy(id)
            ));
        }
    }

    const STEREO: [[u8; 4]; 2] = [*b"SLFT", *b"SRGT"];
    const FIVE_CHANNEL: [[u8; 4]; 5] = [*b"MLFT", *b"MRGT", *b"C   ", *b"LS  ", *b"RS  "];
    const SIX_CHANNEL: [[u8; 4]; 6] = [*b"MLFT", *b"MRGT", *b"C   ", *b"LFE ", *b"LS  ", *b"RS  "];

    for expected in [&STEREO[..], &FIVE_CHANNEL[..], &SIX_CHANNEL[..]] {
        if ids.len() == expected.len()
            && ids.iter().all(|id| expected.contains(id))
            && ids != expected
        {
            return Err(format!(
                "DSDIFF CHNL standard {}-channel identifiers are not in specification order",
                ids.len()
            ));
        }
    }
    Ok(())
}

fn output_geometry(source_sample_rate: u32) -> Result<(u32, u32), String> {
    if source_sample_rate == 0 || source_sample_rate > MAX_SOURCE_SAMPLE_RATE {
        return Err(format!(
            "DSD sample rate {source_sample_rate} is outside 1..={MAX_SOURCE_SAMPLE_RATE}"
        ));
    }
    for output_rate in [88_200_u32, 96_000] {
        if source_sample_rate.is_multiple_of(output_rate) {
            let ratio = source_sample_rate / output_rate;
            if ratio >= 32 && ratio.is_power_of_two() {
                return Ok((output_rate, ratio));
            }
        }
    }
    Err(format!(
        "DSD sample rate {source_sample_rate} is not a supported 44.1/48 kHz-family power-of-two rate"
    ))
}

fn validate_channels(channels: u16) -> Result<(), String> {
    if channels == 0 || channels > MAX_CHANNELS {
        Err(format!(
            "DSD channel count {channels} is outside 1..={MAX_CHANNELS}"
        ))
    } else {
        Ok(())
    }
}

fn validate_dsf_channel_type(channel_type: u32, channels: u16) -> Result<(), String> {
    let expected = match channel_type {
        1 => 1,
        2 => 2,
        3 => 3,
        4 | 5 => 4,
        6 => 5,
        7 => 6,
        other => return Err(format!("unsupported DSF channel type {other}")),
    };
    if channels != expected {
        return Err(format!(
            "DSF channel type {channel_type} requires {expected} channels, observed {channels}"
        ));
    }
    Ok(())
}

fn dsf_channel_roles(channel_type: u32, channels: u16) -> Vec<ChannelRole> {
    use ChannelRole::{Lfe, Main, Surround};
    match channel_type {
        4 => vec![Main, Main, Surround, Surround],
        5 => vec![Main, Main, Main, Lfe],
        6 => vec![Main, Main, Main, Surround, Surround],
        7 => vec![Main, Main, Main, Lfe, Surround, Surround],
        _ => default_channel_roles(channels),
    }
}

fn dff_channel_roles(ids: &[[u8; 4]]) -> Vec<ChannelRole> {
    ids.iter()
        .map(|id| match id {
            b"LFE " => ChannelRole::Lfe,
            b"LS  " | b"RS  " => ChannelRole::Surround,
            _ => ChannelRole::Main,
        })
        .collect()
}

pub fn decode_stream<F>(path: &Path, consume: F) -> Result<crate::decoder::StreamInfo, String>
where
    F: FnMut(&crate::decoder::StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let mut consume = consume;
    decode_stream_with_declared_frames(path, |info, _, planar| consume(info, planar))
}

pub(crate) fn decode_stream_with_declared_frames<F>(
    path: &Path,
    mut consume: F,
) -> Result<crate::decoder::StreamInfo, String>
where
    F: FnMut(&crate::decoder::StreamInfo, Option<u64>, &mut [Vec<f32>]) -> Result<(), String>,
{
    let info = probe(path)?;
    let stream_info = crate::decoder::StreamInfo {
        sample_rate: info.output_sample_rate,
        channels: info.channels,
        channel_roles: info.channel_roles.clone(),
        source_kind: PcmKind::F32,
    };
    if info.output_frames == 0 {
        return Err(format!(
            "{}: DSD stream is shorter than one output frame",
            path.display()
        ));
    }
    let ratio = info.source_sample_rate / info.output_sample_rate;
    let stages = ratio.trailing_zeros() as usize;
    let mut pipelines: Vec<DsdPipeline> = (0..info.channels)
        .map(|_| DsdPipeline::new(stages, info.output_sample_rate))
        .collect();
    let mut pending: Vec<Vec<f32>> = (0..info.channels).map(|_| Vec::new()).collect();
    let mut source_samples = vec![0_u64; info.channels as usize];
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(info.data_offset))
        .map_err(|error| format!("seek {} DSD data: {error}", path.display()))?;
    let declared_frames = Some(info.output_frames);
    let mut consume_without_metadata =
        |stream_info: &crate::decoder::StreamInfo, planar: &mut [Vec<f32>]| {
            consume(stream_info, declared_frames, planar)
        };

    match info.layout {
        DsdLayout::Dsf { .. } => decode_dsf_data(
            &mut file,
            &info,
            &mut pipelines,
            &mut pending,
            &mut source_samples,
            &stream_info,
            &mut consume_without_metadata,
        )?,
        DsdLayout::Dsdiff => decode_dsdiff_data(
            &mut file,
            &info,
            &mut pipelines,
            &mut pending,
            &mut source_samples,
            &stream_info,
            &mut consume_without_metadata,
        )?,
    }
    flush_pending(
        &mut pending,
        &stream_info,
        &mut consume_without_metadata,
        true,
    )?;
    if source_samples
        .iter()
        .any(|count| *count != info.source_samples_per_channel)
    {
        return Err(format!(
            "{}: DSD sample count did not reconcile",
            path.display()
        ));
    }
    if pipelines
        .iter()
        .any(|pipeline| pipeline.produced != info.output_frames)
    {
        return Err(format!(
            "{}: DSD output frame count did not reconcile",
            path.display()
        ));
    }
    Ok(stream_info)
}

fn decode_dsf_data<F>(
    file: &mut File,
    info: &DsdInfo,
    pipelines: &mut [DsdPipeline],
    pending: &mut [Vec<f32>],
    source_samples: &mut [u64],
    stream_info: &crate::decoder::StreamInfo,
    consume: &mut F,
) -> Result<(), String>
where
    F: FnMut(&crate::decoder::StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let block_size_per_channel = match info.layout {
        DsdLayout::Dsf {
            block_size_per_channel,
        } => block_size_per_channel,
        DsdLayout::Dsdiff => return Err("internal DSD layout mismatch".into()),
    };
    let block_size = block_size_per_channel as usize;
    let channels = info.channels as usize;
    let mut blocks = (0..channels)
        .map(|_| vec![0_u8; block_size])
        .collect::<Vec<_>>();
    let bytes_per_channel = info.source_samples_per_channel.div_ceil(8);
    let rounds = bytes_per_channel.div_ceil(u64::from(block_size_per_channel));
    for _ in 0..rounds {
        for block in &mut blocks {
            file.read_exact(block)
                .map_err(|error| format!("read DSF channel block: {error}"))?;
        }
        push_dsd_channel_blocks(
            &blocks,
            info.bit_order,
            info.source_samples_per_channel,
            source_samples,
            pipelines,
            pending,
        );
        flush_pending(pending, stream_info, consume, false)?;
    }
    Ok(())
}

fn decode_dsdiff_data<F>(
    file: &mut File,
    info: &DsdInfo,
    pipelines: &mut [DsdPipeline],
    pending: &mut [Vec<f32>],
    source_samples: &mut [u64],
    stream_info: &crate::decoder::StreamInfo,
    consume: &mut F,
) -> Result<(), String>
where
    F: FnMut(&crate::decoder::StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let channels = info.channels as usize;
    let frame_bytes = channels;
    let chunk_frames = (64 * 1024 / frame_bytes).max(1);
    let mut bytes = vec![0_u8; chunk_frames * frame_bytes];
    let mut channel_bytes = (0..channels)
        .map(|_| Vec::with_capacity(chunk_frames))
        .collect::<Vec<_>>();
    let mut remaining = info.data_size;
    while remaining != 0 {
        let read_size = usize::try_from(remaining.min(bytes.len() as u64)).unwrap();
        file.read_exact(&mut bytes[..read_size])
            .map_err(|error| format!("read DSDIFF sound data: {error}"))?;
        for channel in &mut channel_bytes {
            channel.clear();
        }
        // DSDIFF interleaves one byte at a time. Deinterleave the bounded read
        // once so each channel can enter its FIR pipeline as one contiguous
        // call instead of dispatching for every byte.
        for frame in bytes[..read_size].chunks_exact(channels) {
            for (channel, &byte) in channel_bytes.iter_mut().zip(frame) {
                channel.push(byte);
            }
        }
        push_dsd_channel_blocks(
            &channel_bytes,
            DsdBitOrder::MostSignificantFirst,
            info.source_samples_per_channel,
            source_samples,
            pipelines,
            pending,
        );
        remaining -= read_size as u64;
        flush_pending(pending, stream_info, consume, false)?;
    }
    Ok(())
}

fn push_dsd_channel_blocks(
    blocks: &[Vec<u8>],
    order: DsdBitOrder,
    limit: u64,
    source_samples: &mut [u64],
    pipelines: &mut [DsdPipeline],
    pending: &mut [Vec<f32>],
) {
    // Channel pipelines share no filter or output state. Preserve every
    // channel's byte, bit, and floating-point operation order while using the
    // existing worker budget for a latency-sensitive top-level decode. Nested
    // album/file work retains its established asset-level scheduling.
    let parallel = blocks.len() > 1
        && rayon::current_num_threads() > 1
        && rayon::current_thread_index().is_none();
    push_dsd_channel_blocks_with_mode(
        blocks,
        order,
        limit,
        source_samples,
        pipelines,
        pending,
        parallel,
    );
}

#[allow(clippy::too_many_arguments)]
fn push_dsd_channel_blocks_with_mode(
    blocks: &[Vec<u8>],
    order: DsdBitOrder,
    limit: u64,
    source_samples: &mut [u64],
    pipelines: &mut [DsdPipeline],
    pending: &mut [Vec<f32>],
    parallel: bool,
) {
    debug_assert_eq!(blocks.len(), source_samples.len());
    debug_assert_eq!(blocks.len(), pipelines.len());
    debug_assert_eq!(blocks.len(), pending.len());
    if parallel {
        blocks
            .par_iter()
            .zip(source_samples.par_iter_mut())
            .zip(pipelines.par_iter_mut())
            .zip(pending.par_iter_mut())
            .for_each(|(((bytes, consumed), pipeline), output)| {
                push_dsd_bytes(bytes, order, limit, consumed, pipeline, output);
            });
    } else {
        blocks
            .iter()
            .zip(source_samples)
            .zip(pipelines)
            .zip(pending)
            .for_each(|(((bytes, consumed), pipeline), output)| {
                push_dsd_bytes(bytes, order, limit, consumed, pipeline, output);
            });
    }
}

fn push_dsd_bytes(
    bytes: &[u8],
    order: DsdBitOrder,
    limit: u64,
    consumed: &mut u64,
    pipeline: &mut DsdPipeline,
    output: &mut Vec<f32>,
) {
    match order {
        DsdBitOrder::LeastSignificantFirst => {
            push_dsd_bytes_ordered::<true>(bytes, limit, consumed, pipeline, output);
        }
        DsdBitOrder::MostSignificantFirst => {
            push_dsd_bytes_ordered::<false>(bytes, limit, consumed, pipeline, output);
        }
    }
}

fn push_dsd_bytes_ordered<const LEAST_SIGNIFICANT_FIRST: bool>(
    bytes: &[u8],
    limit: u64,
    consumed: &mut u64,
    pipeline: &mut DsdPipeline,
    output: &mut Vec<f32>,
) {
    let remaining = limit.saturating_sub(*consumed);
    let complete_bytes = usize::try_from(remaining / 8)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    for &byte in &bytes[..complete_bytes] {
        for bit in 0..8 {
            let shift = if LEAST_SIGNIFICANT_FIRST {
                bit
            } else {
                7 - bit
            };
            let sample = if byte & (1 << shift) == 0 { -1.0 } else { 1.0 };
            if let Some(value) = pipeline.push(sample) {
                output.push(value as f32);
            }
        }
    }
    *consumed += complete_bytes as u64 * 8;
    if complete_bytes == bytes.len() {
        return;
    }
    let tail_bits = limit.saturating_sub(*consumed).min(7) as usize;
    let byte = bytes[complete_bytes];
    for bit in 0..tail_bits {
        let shift = if LEAST_SIGNIFICANT_FIRST {
            bit
        } else {
            7 - bit
        };
        let sample = if byte & (1 << shift) == 0 { -1.0 } else { 1.0 };
        if let Some(value) = pipeline.push(sample) {
            output.push(value as f32);
        }
    }
    *consumed += tail_bits as u64;
}

fn flush_pending<F>(
    pending: &mut [Vec<f32>],
    info: &crate::decoder::StreamInfo,
    consume: &mut F,
    final_chunk: bool,
) -> Result<(), String>
where
    F: FnMut(&crate::decoder::StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    loop {
        let available = pending.iter().map(Vec::len).min().unwrap_or(0);
        let frames = if available >= OUTPUT_CHUNK_FRAMES {
            OUTPUT_CHUNK_FRAMES
        } else if final_chunk {
            available
        } else {
            0
        };
        if frames == 0 {
            break;
        }
        let mut chunk: Vec<Vec<f32>> = pending
            .iter_mut()
            .map(|channel| channel.drain(..frames).collect())
            .collect();
        consume(info, &mut chunk)?;
    }
    if final_chunk && pending.iter().any(|channel| !channel.is_empty()) {
        return Err("DSD decoded channel lengths diverged".into());
    }
    Ok(())
}

struct DsdPipeline {
    decimators: Vec<FirDecimator2>,
    low_pass: Fir,
    produced: u64,
}

impl DsdPipeline {
    fn new(stages: usize, output_sample_rate: u32) -> Self {
        let half_band = low_pass_coefficients(HALF_BAND_TAPS, 0.25);
        let output_filter = low_pass_coefficients(
            OUTPUT_LOW_PASS_TAPS,
            OUTPUT_LOW_PASS_CUTOFF_HZ / f64::from(output_sample_rate),
        );
        Self {
            decimators: (0..stages)
                .map(|_| FirDecimator2::new(half_band.clone()))
                .collect(),
            low_pass: Fir::new(output_filter),
            produced: 0,
        }
    }

    fn push(&mut self, sample: f64) -> Option<f64> {
        let mut value = sample;
        for decimator in &mut self.decimators {
            value = decimator.push(value)?;
        }
        self.produced += 1;
        Some(self.low_pass.push(value))
    }
}

struct FirDecimator2 {
    fir: Fir,
    phase: bool,
}

impl FirDecimator2 {
    fn new(coefficients: Vec<f64>) -> Self {
        Self {
            fir: Fir::new(coefficients),
            phase: false,
        }
    }

    fn push(&mut self, sample: f64) -> Option<f64> {
        self.phase = !self.phase;
        // Every input still enters the delay line, but only one phase survives
        // the 2:1 decimator. Avoid evaluating the 31-tap dot product for the
        // discarded phase; retained outputs use the same coefficient and
        // accumulation order as the full-rate FIR.
        self.fir.push_if(sample, !self.phase)
    }
}

struct Fir {
    coefficients: Vec<f64>,
    delay: Vec<f64>,
    period: usize,
    cursor: usize,
}

impl Fir {
    fn new(coefficients: Vec<f64>) -> Self {
        let period = coefficients.len();
        debug_assert_ne!(period, 0);
        // Mirror each ring entry one period later. The newest-to-oldest
        // history is then one contiguous reverse slice for every cursor,
        // removing a wrap branch from every FIR tap without changing the
        // coefficient or accumulation order.
        let delay = vec![0.0; period * 2];
        Self {
            coefficients,
            delay,
            period,
            cursor: 0,
        }
    }

    fn push(&mut self, sample: f64) -> f64 {
        self.push_if(sample, true)
            .expect("an unconditional FIR output cannot be absent")
    }

    fn push_if(&mut self, sample: f64, emit: bool) -> Option<f64> {
        self.delay[self.cursor] = sample;
        self.delay[self.cursor + self.period] = sample;
        let output = emit.then(|| {
            let history = &self.delay[self.cursor + 1..self.cursor + self.period + 1];
            let mut output = 0.0;
            for (coefficient, delayed) in self.coefficients.iter().zip(history.iter().rev()) {
                output += coefficient * delayed;
            }
            output
        });
        self.cursor += 1;
        if self.cursor == self.period {
            self.cursor = 0;
        }
        output
    }
}

fn low_pass_coefficients(taps: usize, cutoff_cycles_per_sample: f64) -> Vec<f64> {
    debug_assert!(taps % 2 == 1);
    debug_assert!(cutoff_cycles_per_sample > 0.0 && cutoff_cycles_per_sample < 0.5);
    let midpoint = (taps - 1) as f64 / 2.0;
    let mut coefficients: Vec<f64> = (0..taps)
        .map(|index| {
            let distance = index as f64 - midpoint;
            let ideal = if distance == 0.0 {
                2.0 * cutoff_cycles_per_sample
            } else {
                (2.0 * PI * cutoff_cycles_per_sample * distance).sin() / (PI * distance)
            };
            let phase = 2.0 * PI * index as f64 / (taps - 1) as f64;
            let blackman = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            ideal * blackman
        })
        .collect();
    let sum: f64 = coefficients.iter().sum();
    for coefficient in &mut coefficients {
        *coefficient /= sum;
    }
    coefficients
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let info = match parse(&mut file, file_size) {
        Ok(info) => info,
        Err(error) => {
            wrapper.push(check(
                "FORGE-DSD-STRUCTURE",
                false,
                error,
                Some(json!({"file_size_bytes": file_size})),
            ));
            return Ok(finish_audit(
                path,
                "dsd",
                wrapper,
                bitstream,
                xcheck,
                json!({"file_size_bytes": file_size}),
            ));
        }
    };
    wrapper.push(check(
        "FORGE-DSD-STRUCTURE",
        true,
        format!("{} structure is valid", info.format.name()),
        Some(json!({
            "format": info.format.name(),
            "chunks": info.chunk_count,
            "file_size_bytes": file_size,
        })),
    ));
    wrapper.push(check(
        "FORGE-DSD-DATA-GEOMETRY",
        true,
        "DSD data size reconciles with channels and declared samples",
        Some(json!({
            "data_offset": info.data_offset,
            "data_size": info.data_size,
            "source_samples_per_channel": info.source_samples_per_channel,
            "block_size_per_channel": info.block_size_per_channel,
        })),
    ));
    bitstream.push(check(
        "FORGE-DSD-RAW-CODING",
        info.compression == "DSD raw",
        "uncompressed 1-bit DSD coding is supported for read-only analysis",
        Some(json!({
            "compression": info.compression,
            "bit_order": info.bit_order.name(),
        })),
    ));
    bitstream.push(check(
        "FORGE-DSD-SAMPLE-RATE",
        true,
        "DSD rate has a supported power-of-two decimation geometry",
        Some(json!({
            "source_hz": info.source_sample_rate,
            "output_hz": info.output_sample_rate,
            "ratio": info.source_sample_rate / info.output_sample_rate,
        })),
    ));
    xcheck.push(check(
        "FORGE-DSD-CHANNELS",
        info.channel_roles.len() == info.channels as usize,
        "container channel declarations map to BS.1770 channel roles",
        Some(json!({"channels": info.channels})),
    ));
    xcheck.push(check(
        "FORGE-DSD-CONVERSION-POLICY",
        true,
        "non-normative DSD-to-PCM policy is explicit for downstream measurement",
        Some(json!({
            "policy_id": "forge-dsd-pcm-v1",
            "description": CONVERSION_POLICY,
            "half_band_taps": HALF_BAND_TAPS,
            "output_low_pass_taps": OUTPUT_LOW_PASS_TAPS,
            "output_low_pass_cutoff_hz": OUTPUT_LOW_PASS_CUTOFF_HZ,
            "edge_policy": "zero initial state; complete decimation intervals only; no tail padding",
        })),
    ));
    Ok(finish_audit(
        path,
        info.format.name(),
        wrapper,
        bitstream,
        xcheck,
        json!({
            "source_sample_rate_hz": info.source_sample_rate,
            "analysis_sample_rate_hz": info.output_sample_rate,
            "channels": info.channels,
            "source_samples_per_channel": info.source_samples_per_channel,
            "analysis_frames": info.output_frames,
            "duration_seconds": info.source_samples_per_channel as f64
                / f64::from(info.source_sample_rate),
            "bit_order": info.bit_order.name(),
            "compression": info.compression,
            "conversion_policy": CONVERSION_POLICY,
        }),
    ))
}

fn read_at<const N: usize>(file: &mut File, offset: u64, limit: u64) -> Result<[u8; N], String> {
    let bytes = read_vec_at(file, offset, N as u64, limit)?;
    Ok(bytes.try_into().unwrap())
}

fn read_vec_at(file: &mut File, offset: u64, size: u64, limit: u64) -> Result<Vec<u8>, String> {
    let end = offset.checked_add(size).ok_or("DSD read range overflow")?;
    if end > limit {
        return Err(format!(
            "DSD read range {offset}..{end} exceeds boundary {limit}"
        ));
    }
    let size = usize::try_from(size).map_err(|_| "DSD read is too large for this platform")?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek DSD field: {error}"))?;
    let mut bytes = vec![0_u8; size];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read DSD field: {error}"))?;
    Ok(bytes)
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes.try_into().unwrap())
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}

fn be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn conversion_geometry_accepts_standard_families() {
        assert_eq!(output_geometry(2_822_400).unwrap(), (88_200, 32));
        assert_eq!(output_geometry(5_644_800).unwrap(), (88_200, 64));
        assert_eq!(output_geometry(3_072_000).unwrap(), (96_000, 32));
        assert!(output_geometry(2_000_000).is_err());
    }

    #[test]
    fn mirrored_fir_history_matches_wrapping_reference_bit_exactly() {
        let coefficient_sets = [
            low_pass_coefficients(HALF_BAND_TAPS, 0.25),
            low_pass_coefficients(OUTPUT_LOW_PASS_TAPS, OUTPUT_LOW_PASS_CUTOFF_HZ / 88_200.0),
        ];
        for coefficients in coefficient_sets {
            let mut candidate = Fir::new(coefficients.clone());
            let mut reference_delay = vec![0.0; coefficients.len()];
            let mut reference_cursor = 0_usize;
            for index in 0_usize..131_071 {
                let sample = if (index.wrapping_mul(97).wrapping_add(53) & 1) == 0 {
                    -1.0
                } else {
                    1.0
                };
                reference_delay[reference_cursor] = sample;
                let mut delay_index = reference_cursor;
                let mut reference = 0.0;
                for coefficient in &coefficients {
                    reference += coefficient * reference_delay[delay_index];
                    delay_index = if delay_index == 0 {
                        reference_delay.len() - 1
                    } else {
                        delay_index - 1
                    };
                }
                reference_cursor += 1;
                if reference_cursor == reference_delay.len() {
                    reference_cursor = 0;
                }
                assert_eq!(candidate.push(sample).to_bits(), reference.to_bits());
            }
        }
    }

    #[test]
    fn decimator_retained_phases_match_full_rate_fir_bit_exactly() {
        let coefficients = low_pass_coefficients(HALF_BAND_TAPS, 0.25);
        let mut candidate = FirDecimator2::new(coefficients.clone());
        let mut reference = Fir::new(coefficients);
        let mut reference_phase = false;
        for index in 0_usize..131_071 {
            let sample = if (index.wrapping_mul(193).wrapping_add(17) & 1) == 0 {
                -1.0
            } else {
                1.0
            };
            let full_rate = reference.push(sample);
            reference_phase = !reference_phase;
            let expected = (!reference_phase).then_some(full_rate);
            let observed = candidate.push(sample);
            assert_eq!(
                observed.map(f64::to_bits),
                expected.map(f64::to_bits),
                "sample {index}"
            );
        }
    }

    #[test]
    fn ordered_byte_batches_match_per_bit_reference_for_every_tail() {
        let bytes = [0x00, 0xff, 0xa5, 0x3c, 0x81];
        for order in [
            DsdBitOrder::LeastSignificantFirst,
            DsdBitOrder::MostSignificantFirst,
        ] {
            for limit in 0..=bytes.len() as u64 * 8 + 1 {
                let mut candidate_pipeline = DsdPipeline::new(2, 88_200);
                let mut candidate_consumed = 0;
                let mut candidate_output = Vec::new();
                push_dsd_bytes(
                    &bytes,
                    order,
                    limit,
                    &mut candidate_consumed,
                    &mut candidate_pipeline,
                    &mut candidate_output,
                );

                let mut reference_pipeline = DsdPipeline::new(2, 88_200);
                let mut reference_consumed = 0;
                let mut reference_output = Vec::new();
                'bytes: for &byte in &bytes {
                    for bit in 0..8 {
                        if reference_consumed == limit {
                            break 'bytes;
                        }
                        let shift = match order {
                            DsdBitOrder::LeastSignificantFirst => bit,
                            DsdBitOrder::MostSignificantFirst => 7 - bit,
                        };
                        let sample = if byte & (1 << shift) == 0 { -1.0 } else { 1.0 };
                        if let Some(value) = reference_pipeline.push(sample) {
                            reference_output.push(value as f32);
                        }
                        reference_consumed += 1;
                    }
                }

                assert_eq!(candidate_consumed, reference_consumed);
                assert_eq!(candidate_pipeline.produced, reference_pipeline.produced);
                assert_eq!(candidate_output, reference_output);
            }
        }
    }

    #[test]
    fn filters_preserve_dc_after_settling() {
        let mut pipeline = DsdPipeline::new(5, 88_200);
        let mut output = Vec::new();
        for _ in 0..2_822_400 / 10 {
            if let Some(value) = pipeline.push(1.0) {
                output.push(value);
            }
        }
        let settled = &output[output.len() / 2..];
        let mean = settled.iter().sum::<f64>() / settled.len() as f64;
        assert!((mean - 1.0).abs() < 1e-9, "{mean}");
    }

    #[test]
    fn alternating_dsd_rejects_ultrasonic_carrier() {
        let mut pipeline = DsdPipeline::new(5, 88_200);
        let mut output = Vec::new();
        for index in 0..2_822_400 / 10 {
            if let Some(value) = pipeline.push(if index % 2 == 0 { 1.0 } else { -1.0 }) {
                output.push(value);
            }
        }
        let peak = output[output.len() / 2..]
            .iter()
            .map(|value| value.abs())
            .fold(0.0_f64, f64::max);
        assert!(peak < 1e-5, "{peak}");
    }

    #[test]
    fn output_low_pass_rejects_ultrasonic_programme_energy() {
        let source_rate = 2_822_400.0;
        let mut error = 0.0;
        let mut pipeline = DsdPipeline::new(5, 88_200);
        let mut output = Vec::new();
        for index in 0..282_240 {
            let desired = 0.25 * (2.0 * PI * 30_000.0 * index as f64 / source_rate).sin();
            let quantized = if error + desired >= 0.0 { 1.0 } else { -1.0 };
            error += desired - quantized;
            if let Some(value) = pipeline.push(quantized) {
                output.push(value);
            }
        }
        let settled = &output[1000..];
        let rms = (settled.iter().map(|sample| sample * sample).sum::<f64>()
            / settled.len() as f64)
            .sqrt();
        assert!(rms < 1e-3, "{rms}");
    }

    #[test]
    fn dsf_and_dsdiff_decode_the_same_signal() {
        let source_rate = 2_822_400;
        let source_samples = source_rate as usize / 20;
        let samples: Vec<f64> = (0..source_samples)
            .map(|index| 0.25 * (2.0 * PI * 1_000.0 * index as f64 / source_rate as f64).sin())
            .collect();
        let bits = sigma_delta(&samples);
        let directory = tempdir().unwrap();
        let dsf_path = directory.path().join("tone.dsf");
        let dff_path = directory.path().join("tone.dff");
        fs::write(
            &dsf_path,
            make_dsf(&[bits.clone(), bits.clone()], source_rate),
        )
        .unwrap();
        fs::write(&dff_path, make_dsdiff(&[bits.clone(), bits], source_rate)).unwrap();

        let dsf = crate::decoder::decode(&dsf_path).unwrap();
        let dff = crate::decoder::decode(&dff_path).unwrap();
        assert_eq!(dsf.sample_rate, 88_200);
        assert_eq!(dsf.channels, 2);
        assert_eq!(dsf.frames, source_samples / 32);
        assert_eq!(dsf.frames, dff.frames);
        for channel in 0..2 {
            let difference = dsf.data[channel]
                .iter()
                .zip(&dff.data[channel])
                .map(|(left, right)| (left - right).abs())
                .fold(0.0_f32, f32::max);
            assert!(difference < 1e-6, "{difference}");
        }
        let settled = &dsf.data[0][1000..];
        let rms = (settled
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / settled.len() as f64)
            .sqrt();
        assert!((rms - 0.25 / 2.0_f64.sqrt()).abs() < 0.01, "{rms}");
    }

    #[test]
    fn parallel_channel_blocks_match_serial_processing_bit_exactly() {
        let channels = 4_usize;
        let make_blocks = |salt: usize| {
            (0..channels)
                .map(|channel| {
                    (0_usize..8_193)
                        .map(|index| {
                            index
                                .wrapping_mul(97 + channel * 31)
                                .wrapping_add(channel * 53)
                                .wrapping_add(salt) as u8
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let first = make_blocks(7);
        let second = make_blocks(193);
        let limit = (first[0].len() + second[0].len()) as u64 * 8 - 3;
        let mut serial_pipelines = (0..channels)
            .map(|_| DsdPipeline::new(5, 88_200))
            .collect::<Vec<_>>();
        let mut parallel_pipelines = (0..channels)
            .map(|_| DsdPipeline::new(5, 88_200))
            .collect::<Vec<_>>();
        let mut serial_pending = vec![Vec::new(); channels];
        let mut parallel_pending = vec![Vec::new(); channels];
        let mut serial_samples = vec![0_u64; channels];
        let mut parallel_samples = vec![0_u64; channels];
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(channels)
            .build()
            .unwrap();
        for blocks in [&first, &second] {
            push_dsd_channel_blocks_with_mode(
                blocks,
                DsdBitOrder::LeastSignificantFirst,
                limit,
                &mut serial_samples,
                &mut serial_pipelines,
                &mut serial_pending,
                false,
            );
            pool.install(|| {
                push_dsd_channel_blocks_with_mode(
                    blocks,
                    DsdBitOrder::LeastSignificantFirst,
                    limit,
                    &mut parallel_samples,
                    &mut parallel_pipelines,
                    &mut parallel_pending,
                    true,
                );
            });
        }

        assert_eq!(serial_samples, parallel_samples);
        assert_eq!(
            serial_pipelines
                .iter()
                .map(|pipeline| pipeline.produced)
                .collect::<Vec<_>>(),
            parallel_pipelines
                .iter()
                .map(|pipeline| pipeline.produced)
                .collect::<Vec<_>>()
        );
        for (serial, parallel) in serial_pending.iter().zip(&parallel_pending) {
            assert_eq!(
                serial
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                parallel
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn container_audit_exposes_conversion_policy() {
        let bits = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 1024)
            .collect::<Vec<_>>();
        let directory = tempdir().unwrap();
        let path = directory.path().join("silence.dsf");
        fs::write(&path, make_dsf(&[bits], 2_822_400)).unwrap();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(audit.passed);
        assert_eq!(audit.format, "dsf");
        assert_eq!(audit.properties["analysis_sample_rate_hz"], json!(88_200));
        assert!(audit
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|item| item.rule_id == "FORGE-DSD-CONVERSION-POLICY"));
    }

    #[test]
    fn malformed_dsf_size_becomes_a_stable_qc_failure() {
        let bits = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 128)
            .collect();
        let mut bytes = make_dsf(&[bits], 2_822_400);
        bytes[12..20].copy_from_slice(&1_u64.to_le_bytes());
        let directory = tempdir().unwrap();
        let path = directory.path().join("bad.dsf");
        fs::write(&path, bytes).unwrap();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed);
        assert_eq!(audit.layers[0].checks[0].rule_id, "FORGE-DSD-STRUCTURE");
    }

    #[test]
    fn dsf_non_zero_padding_is_rejected() {
        let bits = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 128)
            .collect();
        let mut bytes = make_dsf(&[bits], 2_822_400);
        *bytes.last_mut().unwrap() = 1;
        let directory = tempdir().unwrap();
        let path = directory.path().join("bad-padding.dsf");
        fs::write(&path, bytes).unwrap();
        let error = probe(&path).unwrap_err();
        assert!(error.contains("non-zero block padding"), "{error}");
    }

    #[test]
    fn full_buffer_decode_enforces_output_sample_limit_before_processing() {
        let bits = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 1024)
            .collect::<Vec<_>>();
        let directory = tempdir().unwrap();
        let path = directory.path().join("bounded.dsf");
        fs::write(&path, make_dsf(&[bits], 2_822_400)).unwrap();
        let error = crate::decoder::decode_limited(&path, 100).unwrap_err();
        assert!(error.contains("safety limit"), "{error}");
    }

    #[test]
    fn dsdiff_rejects_non_raw_compression() {
        let bits = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 1024)
            .collect::<Vec<_>>();
        let mut bytes = make_dsdiff(&[bits], 2_822_400);
        let cmpr = bytes
            .windows(4)
            .position(|window| window == b"CMPR")
            .unwrap();
        bytes[cmpr + 12..cmpr + 16].copy_from_slice(b"DST ");
        let directory = tempdir().unwrap();
        let path = directory.path().join("compressed.dff");
        fs::write(&path, bytes).unwrap();
        let error = probe(&path).unwrap_err();
        assert!(error.contains("unsupported DSDIFF compression"), "{error}");
    }

    #[test]
    fn dsdiff_rejects_invalid_chunk_identifier() {
        let bits: Vec<bool> = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 1024)
            .collect();
        let mut bytes = make_dsdiff(&[bits.clone(), bits], 2_822_400);
        bytes[16] = 0x1f;
        let directory = tempdir().unwrap();
        let path = directory.path().join("invalid-id.dff");
        fs::write(&path, bytes).unwrap();
        let error = probe(&path).unwrap_err();
        assert!(error.contains("chunk identifier"), "{error}");
    }

    #[test]
    fn dsdiff_rejects_duplicate_channel_identifier() {
        let bits: Vec<bool> = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 1024)
            .collect();
        let mut bytes = make_dsdiff(&[bits.clone(), bits], 2_822_400);
        let chnl = bytes
            .windows(4)
            .position(|window| window == b"CHNL")
            .unwrap();
        bytes[chnl + 18..chnl + 22].copy_from_slice(b"SLFT");
        let directory = tempdir().unwrap();
        let path = directory.path().join("duplicate-channel.dff");
        fs::write(&path, bytes).unwrap();
        let error = probe(&path).unwrap_err();
        assert!(error.contains("duplicate identifier"), "{error}");
    }

    #[test]
    fn dsdiff_rejects_reordered_standard_channels() {
        let bits: Vec<bool> = vec![false, true]
            .into_iter()
            .cycle()
            .take(32 * 1024)
            .collect();
        let mut bytes = make_dsdiff(&[bits.clone(), bits], 2_822_400);
        let chnl = bytes
            .windows(4)
            .position(|window| window == b"CHNL")
            .unwrap();
        bytes[chnl + 14..chnl + 18].copy_from_slice(b"SRGT");
        bytes[chnl + 18..chnl + 22].copy_from_slice(b"SLFT");
        let directory = tempdir().unwrap();
        let path = directory.path().join("reordered-channels.dff");
        fs::write(&path, bytes).unwrap();
        let error = probe(&path).unwrap_err();
        assert!(error.contains("specification order"), "{error}");
    }

    fn sigma_delta(samples: &[f64]) -> Vec<bool> {
        let mut error = 0.0;
        samples
            .iter()
            .map(|sample| {
                let output = if error + sample >= 0.0 { 1.0 } else { -1.0 };
                error += sample - output;
                output > 0.0
            })
            .collect()
    }

    fn pack_bits(bits: &[bool], order: DsdBitOrder) -> Vec<u8> {
        bits.chunks(8)
            .map(|chunk| {
                let mut byte = 0_u8;
                for (index, bit) in chunk.iter().enumerate() {
                    if *bit {
                        let shift = match order {
                            DsdBitOrder::LeastSignificantFirst => index,
                            DsdBitOrder::MostSignificantFirst => 7 - index,
                        };
                        byte |= 1 << shift;
                    }
                }
                byte
            })
            .collect()
    }

    fn make_dsf(channels: &[Vec<bool>], sample_rate: u32) -> Vec<u8> {
        assert!(!channels.is_empty());
        assert!(channels
            .iter()
            .all(|channel| channel.len() == channels[0].len()));
        let block_size = 4096_usize;
        let packed: Vec<Vec<u8>> = channels
            .iter()
            .map(|channel| pack_bits(channel, DsdBitOrder::LeastSignificantFirst))
            .collect();
        let rounds = packed[0].len().div_ceil(block_size);
        let mut data = Vec::new();
        for round in 0..rounds {
            for channel in &packed {
                let start = round * block_size;
                let end = (start + block_size).min(channel.len());
                data.extend_from_slice(&channel[start..end]);
                data.resize(data.len() + block_size - (end - start), 0);
            }
        }
        let total_size = 92_u64 + data.len() as u64;
        let mut output = Vec::new();
        output.extend_from_slice(b"DSD ");
        output.extend_from_slice(&28_u64.to_le_bytes());
        output.extend_from_slice(&total_size.to_le_bytes());
        output.extend_from_slice(&0_u64.to_le_bytes());
        output.extend_from_slice(b"fmt ");
        output.extend_from_slice(&52_u64.to_le_bytes());
        output.extend_from_slice(&1_u32.to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        let channel_type = match channels.len() {
            1 => 1_u32,
            2 => 2,
            _ => panic!("test helper only supports mono/stereo"),
        };
        output.extend_from_slice(&channel_type.to_le_bytes());
        output.extend_from_slice(&(channels.len() as u32).to_le_bytes());
        output.extend_from_slice(&sample_rate.to_le_bytes());
        output.extend_from_slice(&1_u32.to_le_bytes());
        output.extend_from_slice(&(channels[0].len() as u64).to_le_bytes());
        output.extend_from_slice(&(block_size as u32).to_le_bytes());
        output.extend_from_slice(&0_u32.to_le_bytes());
        output.extend_from_slice(b"data");
        output.extend_from_slice(&(data.len() as u64 + 12).to_le_bytes());
        output.extend_from_slice(&data);
        assert_eq!(output.len() as u64, total_size);
        output
    }

    fn make_dsdiff(channels: &[Vec<bool>], sample_rate: u32) -> Vec<u8> {
        assert!(!channels.is_empty());
        assert!(channels
            .iter()
            .all(|channel| channel.len() == channels[0].len()));
        let packed: Vec<Vec<u8>> = channels
            .iter()
            .map(|channel| pack_bits(channel, DsdBitOrder::MostSignificantFirst))
            .collect();
        let mut data = Vec::new();
        for byte in 0..packed[0].len() {
            for channel in &packed {
                data.push(channel[byte]);
            }
        }
        let mut properties = Vec::from(&b"SND "[..]);
        append_dff_chunk(&mut properties, b"FS  ", &sample_rate.to_be_bytes());
        let mut channel_body = Vec::new();
        channel_body.extend_from_slice(&(channels.len() as u16).to_be_bytes());
        for id in [b"SLFT", b"SRGT"].iter().take(channels.len()) {
            channel_body.extend_from_slice(*id);
        }
        append_dff_chunk(&mut properties, b"CHNL", &channel_body);
        append_dff_chunk(&mut properties, b"CMPR", b"DSD \x03DSD");

        let mut body = Vec::from(&b"DSD "[..]);
        append_dff_chunk(&mut body, b"FVER", &0x0105_0000_u32.to_be_bytes());
        append_dff_chunk(&mut body, b"PROP", &properties);
        append_dff_chunk(&mut body, b"DSD ", &data);
        let mut output = Vec::from(&b"FRM8"[..]);
        output.extend_from_slice(&(body.len() as u64).to_be_bytes());
        output.extend_from_slice(&body);
        output
    }

    fn append_dff_chunk(output: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        output.extend_from_slice(id);
        output.extend_from_slice(&(body.len() as u64).to_be_bytes());
        output.extend_from_slice(body);
        if body.len() % 2 == 1 {
            output.push(0);
        }
    }
}
