//! Dependency-free Monkey's Audio 3.98+ descriptor, frame-boundary, and MD5 QC.
//!
//! The descriptor MD5 is the format's encoded-file quick-verification checksum.
//! Per-frame CRC words cover decoded PCM, so this structural audit reports their
//! presence but deliberately does not claim to verify them without decoding.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use md5::{Digest, Md5};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const DESCRIPTOR_BYTES: u64 = 52;
const HEADER_BYTES: u64 = 24;
const MAX_JUNK_BYTES: u64 = 1024 * 1024;
const MAX_CONTROL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEADER_OR_FOOTER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FRAMES: u32 = 1_000_000;
const CREATE_WAV_HEADER: u16 = 1 << 5;
const FLOATING_POINT: u16 = 1 << 12;
const KNOWN_FORMAT_FLAGS: u16 = (1 << 13) - 1;

#[derive(Debug)]
struct Parsed {
    descriptor_offset: u64,
    magic: String,
    version_encoder: u16,
    version_program: u16,
    descriptor_bytes: u64,
    header_bytes: u64,
    seek_table_bytes: u64,
    header_data_bytes: u64,
    frame_data_bytes: u64,
    terminating_data_bytes: u64,
    declared_end: u64,
    trailing_bytes: u64,
    compression_level: u16,
    format_flags: u16,
    blocks_per_frame: u32,
    final_frame_blocks: u32,
    total_frames: u32,
    bits_per_sample: u16,
    channels: u16,
    sample_rate_hz: u32,
    total_samples: u64,
    frame_crc_slots: u32,
    stored_md5: [u8; 16],
    computed_md5: [u8; 16],
}

pub(crate) fn probe(file: &mut File, file_size: u64) -> Result<bool, String> {
    Ok(find_descriptor(file, file_size)?.is_some())
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let parsed = parse(&mut file, file_size);
    let (descriptor_valid, format_valid, seek_valid, crc_present, md5_valid, sample_valid) =
        match &parsed {
            Ok(value) => (
                true,
                valid_format(value),
                true,
                value.frame_crc_slots == value.total_frames,
                value.stored_md5 == value.computed_md5 && value.stored_md5 != [0; 16],
                value.total_samples > 0,
            ),
            Err(_) => (false, false, false, false, false, false),
        };
    let error = parsed.as_ref().err().cloned();
    let value = parsed.as_ref().ok();

    let wrapper = vec![
        check(
            "FORGE-APE-DESCRIPTOR-BOUNDS",
            descriptor_valid,
            "the current-format descriptor, header, seek table, audio data, and terminating data are bounded",
            Some(json!({
                "descriptor_offset": value.map(|v| v.descriptor_offset),
                "declared_end": value.map(|v| v.declared_end),
                "file_bytes": file_size,
                "trailing_tag_or_junk_bytes": value.map(|v| v.trailing_bytes),
                "error": error
            })),
        ),
        check(
            "FORGE-APE-FORMAT",
            format_valid,
            "the encoder version, compression mode, PCM format, frame geometry, and format flags are valid and supported",
            Some(json!({
                "magic": value.map(|v| v.magic.as_str()),
                "encoder_version": value.map(|v| v.version_encoder),
                "program_version": value.map(|v| v.version_program),
                "compression_level": value.map(|v| v.compression_level),
                "format_flags": value.map(|v| v.format_flags),
                "bits_per_sample": value.map(|v| v.bits_per_sample),
                "channels": value.map(|v| v.channels),
                "sample_rate_hz": value.map(|v| v.sample_rate_hz)
            })),
        ),
    ];
    let bitstream = vec![
        check(
            "FORGE-APE-SEEK-TABLE",
            seek_valid,
            "the seek table enumerates every frame with strictly increasing in-range byte boundaries",
            Some(json!({
                "frames": value.map(|v| v.total_frames),
                "seek_table_bytes": value.map(|v| v.seek_table_bytes),
                "frame_data_bytes": value.map(|v| v.frame_data_bytes)
            })),
        ),
        check(
            "FORGE-APE-FRAME-CRC-PRESENCE",
            crc_present,
            "every frame is large enough to contain the required 32-bit decoded-PCM CRC field",
            Some(json!({
                "frame_crc_slots": value.map(|v| v.frame_crc_slots),
                "decoded_crc_note": "frame CRC equality requires decoding and is not claimed by this structural audit"
            })),
        ),
        check(
            "FORGE-APE-DESCRIPTOR-MD5",
            md5_valid,
            "the descriptor MD5 matches the original header data, encoded frames, terminating data, APE header, and seek table",
            Some(json!({
                "stored_md5": value.map(|v| hex(&v.stored_md5)),
                "computed_md5": value.map(|v| hex(&v.computed_md5))
            })),
        ),
    ];
    let xcheck = vec![check(
        "FORGE-APE-TOTAL-SAMPLES",
        sample_valid,
        "frame geometry yields a non-zero overflow-safe total sample count",
        Some(json!({
            "blocks_per_frame": value.map(|v| v.blocks_per_frame),
            "final_frame_blocks": value.map(|v| v.final_frame_blocks),
            "total_samples": value.map(|v| v.total_samples)
        })),
    )];

    Ok(finish_audit(
        path,
        "monkeys-audio",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "encoder_version": value.map(|v| v.version_encoder),
            "program_version": value.map(|v| v.version_program),
            "compression_level": value.map(|v| v.compression_level),
            "sample_rate_hz": value.map(|v| v.sample_rate_hz),
            "channels": value.map(|v| v.channels),
            "bits_per_sample": value.map(|v| v.bits_per_sample),
            "frames": value.map(|v| v.total_frames),
            "samples": value.map(|v| v.total_samples),
            "descriptor_md5_verified": md5_valid,
            "frame_crc_slots": value.map(|v| v.frame_crc_slots),
            "decoded_frame_crc_note": "stored CRCs cover decoded PCM and are not independently verified without decoding"
        }),
    ))
}

fn parse(file: &mut File, file_size: u64) -> Result<Parsed, String> {
    let descriptor_offset = find_descriptor(file, file_size)?
        .ok_or_else(|| "Monkey's Audio descriptor not found in the first 1 MiB".to_string())?;
    let descriptor = read_region(file, descriptor_offset, DESCRIPTOR_BYTES)?;
    let version_encoder = le_u16(&descriptor[4..6]);
    if !(3980..=3990).contains(&version_encoder) {
        return Err(format!(
            "unsupported Monkey's Audio encoder format version {version_encoder}; native QC supports the current 3.98-3.99 descriptor format"
        ));
    }
    let version_program = le_u16(&descriptor[6..8]);
    let descriptor_bytes = u64::from(le_u32(&descriptor[8..12]));
    let header_bytes = u64::from(le_u32(&descriptor[12..16]));
    let seek_table_bytes = u64::from(le_u32(&descriptor[16..20]));
    let header_data_bytes = u64::from(le_u32(&descriptor[20..24]));
    let frame_data_bytes =
        u64::from(le_u32(&descriptor[24..28])) | (u64::from(le_u32(&descriptor[28..32])) << 32);
    let terminating_data_bytes = u64::from(le_u32(&descriptor[32..36]));
    let stored_md5: [u8; 16] = descriptor[36..52].try_into().unwrap();

    if !(DESCRIPTOR_BYTES..=MAX_CONTROL_BYTES).contains(&descriptor_bytes) {
        return Err(format!("invalid descriptor size {descriptor_bytes}"));
    }
    if !(HEADER_BYTES..=MAX_CONTROL_BYTES).contains(&header_bytes) {
        return Err(format!("invalid APE header size {header_bytes}"));
    }
    if seek_table_bytes == 0 || seek_table_bytes % 4 != 0 || seek_table_bytes > MAX_CONTROL_BYTES {
        return Err(format!("invalid seek table size {seek_table_bytes}"));
    }
    if header_data_bytes > MAX_HEADER_OR_FOOTER_BYTES
        || terminating_data_bytes > MAX_HEADER_OR_FOOTER_BYTES
    {
        return Err("original header or terminating data exceeds the 8 MiB safety limit".into());
    }
    if frame_data_bytes == 0 {
        return Err("APE frame-data region is empty".into());
    }

    let header_start = checked_add(descriptor_offset, descriptor_bytes, "header offset")?;
    let seek_start = checked_add(header_start, header_bytes, "seek-table offset")?;
    let header_data_start = checked_add(seek_start, seek_table_bytes, "header-data offset")?;
    let frame_start = checked_add(header_data_start, header_data_bytes, "frame-data offset")?;
    let frame_end = checked_add(frame_start, frame_data_bytes, "frame-data end")?;
    let declared_end = checked_add(frame_end, terminating_data_bytes, "terminating-data end")?;
    if declared_end > file_size {
        return Err(format!(
            "declared APE regions end at byte {declared_end}, beyond the {file_size}-byte file"
        ));
    }
    let trailing_bytes = file_size - declared_end;
    if trailing_bytes > MAX_CONTROL_BYTES {
        return Err("trailing tag or junk exceeds the 64 MiB safety limit".into());
    }

    let header = read_region(file, header_start, HEADER_BYTES)?;
    let compression_level = le_u16(&header[0..2]);
    let format_flags = le_u16(&header[2..4]);
    let blocks_per_frame = le_u32(&header[4..8]);
    let final_frame_blocks = le_u32(&header[8..12]);
    let total_frames = le_u32(&header[12..16]);
    let bits_per_sample = le_u16(&header[16..18]);
    let channels = le_u16(&header[18..20]);
    let sample_rate_hz = le_u32(&header[20..24]);

    if total_frames == 0 || total_frames > MAX_FRAMES {
        return Err(format!("invalid frame count {total_frames}"));
    }
    if seek_table_bytes / 4 < u64::from(total_frames) {
        return Err(format!(
            "seek table has {} entries for {total_frames} frames",
            seek_table_bytes / 4
        ));
    }
    let max_blocks = if compression_level >= 5000 {
        10_000_000
    } else {
        1_000_000
    };
    if blocks_per_frame == 0 || blocks_per_frame > max_blocks {
        return Err(format!("invalid blocks-per-frame value {blocks_per_frame}"));
    }
    if final_frame_blocks == 0 || final_frame_blocks > blocks_per_frame {
        return Err(format!(
            "invalid final-frame block count {final_frame_blocks}"
        ));
    }
    let total_samples = u64::from(total_frames - 1)
        .checked_mul(u64::from(blocks_per_frame))
        .and_then(|value| value.checked_add(u64::from(final_frame_blocks)))
        .ok_or_else(|| "total sample count overflow".to_string())?;

    let seek_bytes = read_region(file, seek_start, u64::from(total_frames) * 4)?;
    let mut wrap_base = 0_u64;
    let mut previous_raw = 0_u32;
    let mut previous_absolute = None;
    let mut frame_starts = Vec::with_capacity(total_frames as usize);
    for (index, bytes) in seek_bytes.chunks_exact(4).enumerate() {
        let raw = le_u32(bytes);
        if index > 0 && raw < previous_raw {
            wrap_base = wrap_base
                .checked_add(1_u64 << 32)
                .ok_or_else(|| "seek-table wrap overflow".to_string())?;
        }
        let relative = wrap_base + u64::from(raw);
        let absolute = checked_add(descriptor_offset, relative, "frame seek offset")?;
        if absolute < frame_start || absolute >= frame_end {
            return Err(format!(
                "frame {index} seek offset {absolute} lies outside {frame_start}..{frame_end}"
            ));
        }
        if let Some(previous) = previous_absolute {
            if absolute <= previous {
                return Err(format!(
                    "frame {index} seek offset is not strictly increasing"
                ));
            }
        } else if absolute != frame_start {
            return Err(format!(
                "first frame begins at {absolute}, expected declared frame-data start {frame_start}"
            ));
        }
        frame_starts.push(absolute);
        previous_absolute = Some(absolute);
        previous_raw = raw;
    }
    for (index, start) in frame_starts.iter().copied().enumerate() {
        let end = frame_starts.get(index + 1).copied().unwrap_or(frame_end);
        if end - start < 4 {
            return Err(format!(
                "frame {index} is too short to contain its 32-bit decoded-PCM CRC"
            ));
        }
    }

    let computed_md5 = descriptor_md5(
        file,
        header_data_start,
        header_data_bytes,
        frame_start,
        frame_data_bytes,
        terminating_data_bytes,
        header_start,
        header_bytes,
        seek_start,
        seek_table_bytes,
    )?;

    Ok(Parsed {
        descriptor_offset,
        magic: String::from_utf8_lossy(&descriptor[..4]).into_owned(),
        version_encoder,
        version_program,
        descriptor_bytes,
        header_bytes,
        seek_table_bytes,
        header_data_bytes,
        frame_data_bytes,
        terminating_data_bytes,
        declared_end,
        trailing_bytes,
        compression_level,
        format_flags,
        blocks_per_frame,
        final_frame_blocks,
        total_frames,
        bits_per_sample,
        channels,
        sample_rate_hz,
        total_samples,
        frame_crc_slots: total_frames,
        stored_md5,
        computed_md5,
    })
}

#[allow(clippy::too_many_arguments)]
fn descriptor_md5(
    file: &mut File,
    header_data_start: u64,
    header_data_bytes: u64,
    frame_start: u64,
    frame_data_bytes: u64,
    terminating_data_bytes: u64,
    header_start: u64,
    header_bytes: u64,
    seek_start: u64,
    seek_table_bytes: u64,
) -> Result<[u8; 16], String> {
    let mut digest = Md5::new();
    hash_region(file, header_data_start, header_data_bytes, &mut digest)?;
    hash_region(
        file,
        frame_start,
        frame_data_bytes + terminating_data_bytes,
        &mut digest,
    )?;
    hash_region(file, header_start, header_bytes, &mut digest)?;
    hash_region(file, seek_start, seek_table_bytes, &mut digest)?;
    Ok(digest.finalize().into())
}

fn hash_region(
    file: &mut File,
    start: u64,
    mut bytes: u64,
    digest: &mut Md5,
) -> Result<(), String> {
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek to byte {start}: {error}"))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut position = start;
    while bytes > 0 {
        let amount = usize::try_from(bytes.min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..amount])
            .map_err(|error| format!("read {amount} bytes at byte {position}: {error}"))?;
        digest.update(&buffer[..amount]);
        bytes -= amount as u64;
        position += amount as u64;
    }
    Ok(())
}

fn find_descriptor(file: &mut File, file_size: u64) -> Result<Option<u64>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek Monkey's Audio input: {error}"))?;
    let bytes = usize::try_from(file_size.min(MAX_JUNK_BYTES + DESCRIPTOR_BYTES)).unwrap();
    let mut prefix = vec![0_u8; bytes];
    file.read_exact(&mut prefix)
        .map_err(|error| format!("read Monkey's Audio prefix: {error}"))?;
    Ok(prefix
        .windows(DESCRIPTOR_BYTES as usize)
        .enumerate()
        .find(|(offset, descriptor)| descriptor_is_sane(descriptor, *offset as u64, file_size))
        .map(|(offset, _)| offset as u64))
}

fn descriptor_is_sane(descriptor: &[u8], offset: u64, file_size: u64) -> bool {
    if !matches!(&descriptor[..4], b"MAC " | b"MACF")
        || !(3980..=3990).contains(&le_u16(&descriptor[4..6]))
    {
        return false;
    }
    let descriptor_bytes = u64::from(le_u32(&descriptor[8..12]));
    let header_bytes = u64::from(le_u32(&descriptor[12..16]));
    let seek_table_bytes = u64::from(le_u32(&descriptor[16..20]));
    let header_data_bytes = u64::from(le_u32(&descriptor[20..24]));
    let frame_data_bytes =
        u64::from(le_u32(&descriptor[24..28])) | (u64::from(le_u32(&descriptor[28..32])) << 32);
    let terminating_data_bytes = u64::from(le_u32(&descriptor[32..36]));
    if !(DESCRIPTOR_BYTES..=MAX_CONTROL_BYTES).contains(&descriptor_bytes)
        || !(HEADER_BYTES..=MAX_CONTROL_BYTES).contains(&header_bytes)
        || seek_table_bytes == 0
        || seek_table_bytes % 4 != 0
        || seek_table_bytes > MAX_CONTROL_BYTES
        || header_data_bytes > MAX_HEADER_OR_FOOTER_BYTES
        || terminating_data_bytes > MAX_HEADER_OR_FOOTER_BYTES
        || frame_data_bytes == 0
    {
        return false;
    }
    offset
        .checked_add(descriptor_bytes)
        .and_then(|value| value.checked_add(header_bytes))
        .and_then(|value| value.checked_add(seek_table_bytes))
        .and_then(|value| value.checked_add(header_data_bytes))
        .and_then(|value| value.checked_add(frame_data_bytes))
        .and_then(|value| value.checked_add(terminating_data_bytes))
        .is_some_and(|declared_end| declared_end <= file_size)
}

fn read_region(file: &mut File, start: u64, bytes: u64) -> Result<Vec<u8>, String> {
    let size = usize::try_from(bytes).map_err(|_| "control region is too large".to_string())?;
    let mut value = vec![0_u8; size];
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek to byte {start}: {error}"))?;
    file.read_exact(&mut value)
        .map_err(|error| format!("read {bytes} bytes at byte {start}: {error}"))?;
    Ok(value)
}

fn valid_format(value: &Parsed) -> bool {
    let floating = value.format_flags & FLOATING_POINT != 0;
    matches!(value.compression_level, 1000 | 2000 | 3000 | 4000 | 5000)
        && value.format_flags & !KNOWN_FORMAT_FLAGS == 0
        && (1..=32).contains(&value.channels)
        && matches!(value.bits_per_sample, 8 | 16 | 24 | 32)
        && value.sample_rate_hz > 0
        && value.sample_rate_hz <= 1_000_000
        && (value.magic == "MACF") == floating
        && (!floating || value.bits_per_sample == 32)
        && (value.format_flags & CREATE_WAV_HEADER != 0 || value.header_data_bytes > 0)
        && value.descriptor_bytes >= DESCRIPTOR_BYTES
        && value.header_bytes >= HEADER_BYTES
        && value.terminating_data_bytes <= MAX_HEADER_OR_FOOTER_BYTES
}

fn checked_add(left: u64, right: u64, label: &str) -> Result<u64, String> {
    left.checked_add(right)
        .ok_or_else(|| format!("{label} overflow"))
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").unwrap();
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn current_file() -> Vec<u8> {
        current_file_with_wrappers(&[], &[])
    }

    fn current_file_with_wrappers(header_data: &[u8], terminating_data: &[u8]) -> Vec<u8> {
        let mut header = vec![0_u8; HEADER_BYTES as usize];
        header[0..2].copy_from_slice(&2000_u16.to_le_bytes());
        header[2..4].copy_from_slice(&CREATE_WAV_HEADER.to_le_bytes());
        header[4..8].copy_from_slice(&73_728_u32.to_le_bytes());
        header[8..12].copy_from_slice(&123_u32.to_le_bytes());
        header[12..16].copy_from_slice(&2_u32.to_le_bytes());
        header[16..18].copy_from_slice(&16_u16.to_le_bytes());
        header[18..20].copy_from_slice(&2_u16.to_le_bytes());
        header[20..24].copy_from_slice(&48_000_u32.to_le_bytes());

        let frame_start = DESCRIPTOR_BYTES + HEADER_BYTES + 8 + header_data.len() as u64;
        let first = frame_start as u32;
        let second = first + 7;
        let mut seek = Vec::new();
        seek.extend_from_slice(&first.to_le_bytes());
        seek.extend_from_slice(&second.to_le_bytes());
        let frames = [1, 2, 3, 4, 0xaa, 0xbb, 0xcc, 5, 6, 7, 8, 0xdd, 0xee];

        let mut digest = Md5::new();
        digest.update(header_data);
        digest.update(frames);
        digest.update(terminating_data);
        digest.update(&header);
        digest.update(&seek);
        let md5: [u8; 16] = digest.finalize().into();

        let mut descriptor = vec![0_u8; DESCRIPTOR_BYTES as usize];
        descriptor[..4].copy_from_slice(b"MAC ");
        descriptor[4..6].copy_from_slice(&3990_u16.to_le_bytes());
        descriptor[8..12].copy_from_slice(&(DESCRIPTOR_BYTES as u32).to_le_bytes());
        descriptor[12..16].copy_from_slice(&(HEADER_BYTES as u32).to_le_bytes());
        descriptor[16..20].copy_from_slice(&8_u32.to_le_bytes());
        descriptor[20..24].copy_from_slice(&(header_data.len() as u32).to_le_bytes());
        descriptor[24..28].copy_from_slice(&(frames.len() as u32).to_le_bytes());
        descriptor[32..36].copy_from_slice(&(terminating_data.len() as u32).to_le_bytes());
        descriptor[36..52].copy_from_slice(&md5);
        descriptor.extend_from_slice(&header);
        descriptor.extend_from_slice(&seek);
        descriptor.extend_from_slice(header_data);
        descriptor.extend_from_slice(&frames);
        descriptor.extend_from_slice(terminating_data);
        descriptor
    }

    #[test]
    fn validates_descriptor_md5_and_frame_boundaries() {
        let mut temporary = tempfile::tempfile().unwrap();
        let bytes = current_file();
        temporary.write_all(&bytes).unwrap();
        let parsed = parse(&mut temporary, bytes.len() as u64).unwrap();
        assert_eq!(parsed.total_frames, 2);
        assert_eq!(parsed.total_samples, 73_851);
        assert_eq!(parsed.stored_md5, parsed.computed_md5);
    }

    #[test]
    fn detects_encoded_frame_corruption() {
        let mut bytes = current_file();
        *bytes.last_mut().unwrap() ^= 1;
        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(&bytes).unwrap();
        let parsed = parse(&mut temporary, bytes.len() as u64).unwrap();
        assert_ne!(parsed.stored_md5, parsed.computed_md5);
    }

    #[test]
    fn hashes_original_header_and_terminating_data() {
        let header_data = b"RIFF-original-header";
        let terminating_data = b"original-footer";
        let bytes = current_file_with_wrappers(header_data, terminating_data);
        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(&bytes).unwrap();
        let parsed = parse(&mut temporary, bytes.len() as u64).unwrap();
        assert_eq!(parsed.header_data_bytes, header_data.len() as u64);
        assert_eq!(parsed.terminating_data_bytes, terminating_data.len() as u64);
        assert_eq!(parsed.stored_md5, parsed.computed_md5);

        let header_offset = (DESCRIPTOR_BYTES + HEADER_BYTES + 8) as usize;
        let mut corrupted = bytes;
        corrupted[header_offset] ^= 1;
        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(&corrupted).unwrap();
        let parsed = parse(&mut temporary, corrupted.len() as u64).unwrap();
        assert_ne!(parsed.stored_md5, parsed.computed_md5);
    }

    #[test]
    fn rejects_non_monotonic_seek_table() {
        let mut bytes = current_file();
        let seek = (DESCRIPTOR_BYTES + HEADER_BYTES) as usize;
        bytes[seek + 4..seek + 8].copy_from_slice(&1_u32.to_le_bytes());
        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(&bytes).unwrap();
        assert!(parse(&mut temporary, bytes.len() as u64)
            .unwrap_err()
            .contains("outside"));
    }

    #[test]
    fn accepts_bounded_leading_junk_and_trailing_tag_data() {
        let original = current_file();
        let mut bytes = b"ID3\x04\0\0\0\0\0\0padding".to_vec();
        let descriptor_offset = bytes.len() as u64;
        bytes.extend_from_slice(&original);
        bytes.extend_from_slice(b"APETAGEX-test-data");
        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(&bytes).unwrap();
        let parsed = parse(&mut temporary, bytes.len() as u64).unwrap();
        assert_eq!(parsed.descriptor_offset, descriptor_offset);
        assert_eq!(parsed.trailing_bytes, 18);
        assert_eq!(parsed.stored_md5, parsed.computed_md5);
    }

    #[test]
    fn probe_rejects_embedded_magic_without_a_sane_descriptor() {
        let bytes = b"unrelated data with MAC \x96\x0f but no descriptor";
        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(bytes).unwrap();
        assert!(!probe(&mut temporary, bytes.len() as u64).unwrap());
    }
}
