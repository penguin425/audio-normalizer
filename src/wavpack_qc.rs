//! Dependency-free WavPack 4/5 block and encoded-block checksum QC.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::json;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const HEADER_BYTES: usize = 32;
const MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;
const MAX_BLOCKS: u64 = 1_000_000;
const MAX_ANCILLARY_BYTES: u64 = 64 * 1024 * 1024;
const INITIAL_BLOCK: u32 = 1 << 11;
const FINAL_BLOCK: u32 = 1 << 12;
const HAS_CHECKSUM: u32 = 1 << 28;
const MONO_FLAG: u32 = 1 << 2;
const FALSE_STEREO: u32 = 1 << 30;
const SAMPLE_RATES: [u32; 16] = [
    6_000, 8_000, 9_600, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000,
    88_200, 96_000, 192_000, 0,
];

#[derive(Default)]
struct State {
    blocks: u64,
    audio_blocks: u64,
    audio_groups: u64,
    samples: u64,
    ancillary_bytes: u64,
    checksum_blocks: u64,
    checksum_16_blocks: u64,
    checksum_32_blocks: u64,
    metadata_items: u64,
    versions: Vec<u16>,
    sample_rate_hz: Option<u32>,
    channels: Option<u32>,
    bytes_per_sample: Option<u8>,
    float: Option<bool>,
    dsd: Option<bool>,
    total_samples: Option<u64>,
    structure_valid: bool,
    metadata_valid: bool,
    checksums_valid: bool,
    sequence_valid: bool,
    format_valid: bool,
    errors: Vec<String>,
    group: Option<Group>,
    previous_group_end: Option<u64>,
}

#[derive(Clone, Copy)]
struct Group {
    index: u64,
    samples: u32,
    channels: u32,
    sample_rate_hz: u32,
    bytes_per_sample: u8,
    float: bool,
    dsd: bool,
}

pub(crate) fn looks_like_wavpack(header: &[u8]) -> bool {
    header.windows(4).any(|window| window == b"wvpk")
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut state = State {
        structure_valid: true,
        metadata_valid: true,
        checksums_valid: true,
        sequence_valid: true,
        format_valid: true,
        ..State::default()
    };
    let mut window = [0_u8; 4];
    let mut filled = 0_usize;

    loop {
        let mut byte = [0_u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        }
        if filled < 4 {
            window[filled] = byte[0];
            filled += 1;
        } else {
            window.copy_within(1..4, 0);
            window[3] = byte[0];
            state.ancillary_bytes += 1;
        }
        if filled < 4 || &window != b"wvpk" {
            if state.ancillary_bytes > MAX_ANCILLARY_BYTES {
                fail(&mut state, "more than 64 MiB of non-block data was scanned");
                break;
            }
            continue;
        }

        if state.blocks == MAX_BLOCKS {
            fail(&mut state, "block count exceeds the 1,000,000 safety limit");
            break;
        }
        let mut header = [0_u8; HEADER_BYTES];
        header[..4].copy_from_slice(&window);
        if let Err(error) = reader.read_exact(&mut header[4..]) {
            fail(&mut state, format!("truncated block header: {error}"));
            break;
        }
        filled = 0;
        parse_block(&mut reader, &header, &mut state)?;
    }

    if filled > 0 {
        state.ancillary_bytes += filled as u64;
    }
    if state.group.is_some() {
        state.sequence_valid = false;
        record(&mut state, "unterminated multichannel block sequence");
    }
    if state.blocks == 0 {
        fail(&mut state, "no WavPack blocks found");
    }
    if state.audio_groups == 0 {
        state.sequence_valid = false;
        record(&mut state, "no complete audio block sequence found");
    }

    let wrapper = vec![
        check(
            "FORGE-WAVPACK-BLOCK-BOUNDS",
            state.structure_valid,
            "every WavPack block has a bounded complete header and payload",
            Some(
                json!({"blocks": state.blocks, "file_bytes": file_size, "ancillary_bytes": state.ancillary_bytes}),
            ),
        ),
        check(
            "FORGE-WAVPACK-METADATA-FRAMING",
            state.metadata_valid,
            "metadata sub-block word lengths, padding, and checksum placement are valid",
            Some(json!({"metadata_items": state.metadata_items})),
        ),
    ];
    let bitstream = vec![
        check(
            "FORGE-WAVPACK-ENCODED-CHECKSUM",
            state.checksums_valid,
            "every declared WavPack 5 encoded-block checksum matches its covered little-endian words",
            Some(json!({
                "checksummed_blocks": state.checksum_blocks,
                "checksum_16_blocks": state.checksum_16_blocks,
                "checksum_32_blocks": state.checksum_32_blocks,
                "unchecked_legacy_blocks": state.blocks.saturating_sub(state.checksum_blocks)
            })),
        ),
        check(
            "FORGE-WAVPACK-SEQUENCE",
            state.sequence_valid,
            "audio block groups are complete, aligned, and sample-contiguous",
            Some(json!({"audio_blocks": state.audio_blocks, "audio_groups": state.audio_groups, "samples": state.samples})),
        ),
        check(
            "FORGE-WAVPACK-FORMAT-STABILITY",
            state.format_valid,
            "sample rate, channel count, numeric representation, and stored width remain stable",
            Some(json!({
                "sample_rate_hz": state.sample_rate_hz,
                "channels": state.channels,
                "bytes_per_sample": state.bytes_per_sample,
                "header_versions": state.versions
            })),
        ),
    ];
    let total_matches = state
        .total_samples
        .is_none_or(|total| total == state.samples);
    let xcheck = vec![check(
        "FORGE-WAVPACK-TOTAL-SAMPLES",
        total_matches,
        "known first-block total_samples matches complete audio block coverage",
        Some(
            json!({"declared_total_samples": state.total_samples, "covered_samples": state.samples, "errors": state.errors}),
        ),
    )];
    Ok(finish_audit(
        path,
        "wavpack",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "blocks": state.blocks,
            "audio_groups": state.audio_groups,
            "sample_rate_hz": state.sample_rate_hz,
            "channels": state.channels,
            "samples": state.samples,
            "encoded_block_checksums": state.checksum_blocks,
            "declared_decoded_audio_crc_blocks": state.audio_blocks,
            "decoded_audio_crc_note": "header CRC covers decoded samples and is reported but not claimed as verified without decoding"
        }),
    ))
}

fn parse_block<R: Read>(
    reader: &mut R,
    header: &[u8; HEADER_BYTES],
    state: &mut State,
) -> Result<(), String> {
    let ck_size = le_u32(&header[4..8]) as usize;
    let block_bytes = match ck_size.checked_add(8) {
        Some(value) if (HEADER_BYTES..=MAX_BLOCK_BYTES).contains(&value) && value % 2 == 0 => value,
        _ => {
            fail(state, format!("invalid WavPack ckSize {ck_size}"));
            return Ok(());
        }
    };
    let version = le_u16(&header[8..10]);
    if !(0x402..=0x410).contains(&version) {
        fail(state, format!("unsupported block version 0x{version:03x}"));
    }
    if !state.versions.contains(&version) {
        state.versions.push(version);
    }
    let mut block = vec![0_u8; block_bytes];
    block[..HEADER_BYTES].copy_from_slice(header);
    if let Err(error) = reader.read_exact(&mut block[HEADER_BYTES..]) {
        fail(
            state,
            format!("truncated {block_bytes}-byte block: {error}"),
        );
        return Ok(());
    }
    state.blocks += 1;
    let flags = le_u32(&header[24..28]);
    if flags & FALSE_STEREO != 0 && flags & MONO_FLAG != 0 {
        state.format_valid = false;
        record(state, "FALSE_STEREO and MONO flags are both set");
    }
    let mut pos = HEADER_BYTES;
    let mut checksum_count = 0_u8;
    let mut custom_sample_rate = None;
    while pos < block.len() {
        if block.len() - pos < 2 {
            state.metadata_valid = false;
            record(state, "truncated metadata header");
            break;
        }
        let start = pos;
        let id = block[pos];
        let mut words = block[pos + 1] as usize;
        pos += 2;
        if id & 0x80 != 0 {
            if block.len() - pos < 2 {
                state.metadata_valid = false;
                record(state, "truncated large metadata length");
                break;
            }
            words |= (block[pos] as usize) << 8 | (block[pos + 1] as usize) << 16;
            pos += 2;
        }
        let padded = match words.checked_mul(2) {
            Some(value) => value,
            None => {
                state.metadata_valid = false;
                record(state, "metadata word length overflow");
                break;
            }
        };
        if padded > block.len() - pos || (id & 0x40 != 0 && padded == 0) {
            state.metadata_valid = false;
            record(
                state,
                format!("metadata 0x{:02x} exceeds block bounds", id & 0x3f),
            );
            break;
        }
        state.metadata_items += 1;
        let actual = padded - usize::from(id & 0x40 != 0);
        if id & 0x3f == 0x27 {
            if matches!(actual, 3 | 4) {
                let rate = u32::from(block[pos])
                    | (u32::from(block[pos + 1]) << 8)
                    | (u32::from(block[pos + 2]) << 16);
                if rate == 0 || custom_sample_rate.replace(rate).is_some() {
                    state.metadata_valid = false;
                    record(state, "invalid or duplicate custom sample-rate metadata");
                }
            } else {
                state.metadata_valid = false;
                record(
                    state,
                    "custom sample-rate metadata must contain 3 or 4 bytes",
                );
            }
        }
        if id & 0x3f == 0x2f {
            checksum_count += 1;
            let placement_ok = checksum_count == 1
                && id & 0xc0 == 0
                && matches!(actual, 2 | 4)
                && pos + padded == block.len();
            if !placement_ok {
                state.metadata_valid = false;
                state.checksums_valid = false;
                record(
                    state,
                    "block checksum metadata is duplicated, malformed, or not last",
                );
            } else {
                let expected = encoded_checksum(&block[..start], actual);
                if block[pos..pos + actual] != expected[..actual] {
                    state.checksums_valid = false;
                    record(
                        state,
                        format!("encoded checksum mismatch in block {}", state.blocks),
                    );
                }
                state.checksum_blocks += 1;
                if actual == 2 {
                    state.checksum_16_blocks += 1;
                } else {
                    state.checksum_32_blocks += 1;
                }
            }
        }
        pos += padded;
    }
    if (flags & HAS_CHECKSUM != 0) != (checksum_count == 1) {
        state.metadata_valid = false;
        state.checksums_valid = false;
        record(
            state,
            "HAS_CHECKSUM flag and checksum metadata presence disagree",
        );
    }

    let block_samples = le_u32(&header[20..24]);
    if block_samples > 131_072 {
        state.sequence_valid = false;
        record(
            state,
            format!("block sample count {block_samples} exceeds 131072"),
        );
    }
    if block_samples > 0 {
        state.audio_blocks += 1;
        update_sequence(header, flags, block_samples, custom_sample_rate, state);
    }
    Ok(())
}

fn update_sequence(
    header: &[u8; HEADER_BYTES],
    flags: u32,
    block_samples: u32,
    custom_sample_rate: Option<u32>,
    state: &mut State,
) {
    let index = le_u32(&header[16..20]) as u64 | ((header[10] as u64) << 32);
    let rate_code = ((flags >> 23) & 0xf) as usize;
    let rate = if rate_code == 15 {
        custom_sample_rate
            .or_else(|| state.group.map(|group| group.sample_rate_hz))
            .or(state.sample_rate_hz)
            .unwrap_or(0)
    } else {
        SAMPLE_RATES[rate_code]
    };
    let channels = if flags & MONO_FLAG != 0 { 1 } else { 2 };
    let bytes = ((flags & 3) + 1) as u8;
    let part = Group {
        index,
        samples: block_samples,
        channels,
        sample_rate_hz: rate,
        bytes_per_sample: bytes,
        float: flags & (1 << 7) != 0,
        dsd: flags & (1 << 31) != 0,
    };
    if flags & INITIAL_BLOCK != 0 {
        if state.group.is_some() {
            state.sequence_valid = false;
            record(state, "new INITIAL_BLOCK before prior group ended");
        }
        state.group = Some(part);
    } else if state.group.is_none() {
        state.sequence_valid = false;
        record(state, "audio sequence does not begin with INITIAL_BLOCK");
        state.group = Some(part);
    }
    if let Some(group) = state.group.as_mut() {
        if group.index != index
            || group.samples != block_samples
            || group.sample_rate_hz != rate
            || group.bytes_per_sample != bytes
            || group.float != part.float
            || group.dsd != part.dsd
        {
            state.sequence_valid = false;
            record(
                state,
                "blocks within an audio group disagree on index, sample count, or format",
            );
        } else if flags & INITIAL_BLOCK == 0 {
            group.channels = group.channels.saturating_add(channels);
        }
    }
    if flags & FINAL_BLOCK != 0 {
        if let Some(group) = state.group.take() {
            if state
                .previous_group_end
                .is_some_and(|end| end != group.index)
            {
                state.sequence_valid = false;
                record(state, "audio groups are not sample-contiguous");
            }
            let end = group.index.checked_add(group.samples as u64);
            if end.is_none() {
                state.sequence_valid = false;
                record(state, "sample index overflow");
            }
            state.previous_group_end = end;
            state.audio_groups += 1;
            state.samples = state.samples.saturating_add(group.samples as u64);
            let config = (group.sample_rate_hz, group.channels, group.bytes_per_sample);
            if state.sample_rate_hz.is_none() {
                state.sample_rate_hz = Some(config.0);
                state.channels = Some(config.1);
                state.bytes_per_sample = Some(config.2);
                state.float = Some(group.float);
                state.dsd = Some(group.dsd);
                if group.index == 0 {
                    let low = le_u32(&header[12..16]);
                    if low != u32::MAX {
                        state.total_samples = Some(low as u64 | ((header[11] as u64) << 32));
                    }
                }
            } else if state.sample_rate_hz != Some(config.0)
                || state.channels != Some(config.1)
                || state.bytes_per_sample != Some(config.2)
                || state.float != Some(group.float)
                || state.dsd != Some(group.dsd)
            {
                state.format_valid = false;
                record(state, "audio format changes between block groups");
            }
            if group.sample_rate_hz == 0 {
                state.format_valid = false;
                record(state, "custom sample-rate code lacks valid metadata");
            }
        }
    }
}

fn encoded_checksum(bytes: &[u8], width: usize) -> [u8; 4] {
    let mut sum = u32::MAX;
    for word in bytes.chunks_exact(2) {
        sum = sum
            .wrapping_mul(3)
            .wrapping_add(u16::from_le_bytes([word[0], word[1]]) as u32);
    }
    if width == 2 {
        sum ^= sum >> 16;
    }
    sum.to_le_bytes()
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().unwrap())
}
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}
fn fail(state: &mut State, message: impl Into<String>) {
    state.structure_valid = false;
    record(state, message);
}
fn record(state: &mut State, message: impl Into<String>) {
    if state.errors.len() < 32 {
        state.errors.push(message.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn block_with(
        checksum_bytes: usize,
        index: u32,
        total: u32,
        samples: u32,
        flags: u32,
        metadata: &[u8],
    ) -> Vec<u8> {
        let mut bytes = vec![0_u8; HEADER_BYTES];
        bytes[..4].copy_from_slice(b"wvpk");
        bytes[8..10].copy_from_slice(&0x410_u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&total.to_le_bytes());
        bytes[16..20].copy_from_slice(&index.to_le_bytes());
        bytes[20..24].copy_from_slice(&samples.to_le_bytes());
        bytes[24..28].copy_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(&[0x2a, 0]);
        bytes.extend_from_slice(metadata);
        let final_ck_size = (bytes.len() + 2 + checksum_bytes - 8) as u32;
        bytes[4..8].copy_from_slice(&final_ck_size.to_le_bytes());
        let checksum = encoded_checksum(&bytes, checksum_bytes);
        bytes.push(0x2f);
        bytes.push((checksum_bytes / 2) as u8);
        bytes.extend_from_slice(&checksum[..checksum_bytes]);
        bytes
    }

    fn block(checksum_bytes: usize) -> Vec<u8> {
        let flags = 1_u32 | MONO_FLAG | INITIAL_BLOCK | FINAL_BLOCK | (10 << 23) | HAS_CHECKSUM;
        block_with(checksum_bytes, 0, 16, 16, flags, &[])
    }

    #[test]
    fn accepts_16_and_32_bit_encoded_checksums() {
        for width in [2, 4] {
            let bytes = block(width);
            let mut state = State {
                structure_valid: true,
                metadata_valid: true,
                checksums_valid: true,
                sequence_valid: true,
                format_valid: true,
                ..State::default()
            };
            let mut header = [0_u8; HEADER_BYTES];
            header.copy_from_slice(&bytes[..HEADER_BYTES]);
            parse_block(
                &mut Cursor::new(&bytes[HEADER_BYTES..]),
                &header,
                &mut state,
            )
            .unwrap();
            assert!(state.checksums_valid);
            assert_eq!(state.checksum_blocks, 1);
            assert_eq!(state.audio_groups, 1);
        }
    }

    #[test]
    fn rejects_checksum_corruption_and_truncation() {
        let mut bytes = block(4);
        *bytes.last_mut().unwrap() ^= 1;
        let mut state = State {
            structure_valid: true,
            metadata_valid: true,
            checksums_valid: true,
            sequence_valid: true,
            format_valid: true,
            ..State::default()
        };
        let mut header = [0_u8; HEADER_BYTES];
        header.copy_from_slice(&bytes[..HEADER_BYTES]);
        parse_block(
            &mut Cursor::new(&bytes[HEADER_BYTES..]),
            &header,
            &mut state,
        )
        .unwrap();
        assert!(!state.checksums_valid);
    }

    #[test]
    fn accepts_multichannel_groups_and_custom_sample_rate() {
        let common = 1_u32 | (15 << 23) | HAS_CHECKSUM;
        let custom_rate = [0x67, 2, 0x80, 0xbb, 0x00, 0x00];
        let mut stream = Vec::new();
        for (index, total) in [(0, 32), (16, 32)] {
            stream.extend(block_with(
                4,
                index,
                total,
                16,
                common | INITIAL_BLOCK,
                &custom_rate,
            ));
            stream.extend(block_with(4, index, total, 16, common | MONO_FLAG, &[]));
            stream.extend(block_with(4, index, total, 16, common | FINAL_BLOCK, &[]));
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &stream).unwrap();
        let report = audit(
            file.path(),
            File::open(file.path()).unwrap(),
            stream.len() as u64,
        )
        .unwrap();
        assert!(report.passed, "{report:#?}");
        assert_eq!(report.properties["channels"], 5);
        assert_eq!(report.properties["sample_rate_hz"], 48_000);
        assert_eq!(report.properties["samples"], 32);
    }
}
