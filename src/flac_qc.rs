//! RFC 9639 native FLAC structural and decoded-integrity audit.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde::Serialize;
use serde_json::json;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_METADATA_BLOCKS: usize = 100_000;
const MAX_PARSED_METADATA_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize)]
struct StreamInfo {
    minimum_block_size: u16,
    maximum_block_size: u16,
    minimum_frame_size: u32,
    maximum_frame_size: u32,
    sample_rate: u32,
    channels: u8,
    bits_per_sample: u8,
    total_samples: u64,
    md5_present: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DecodedIntegrity {
    frames: u64,
    sample_rate: u32,
    channels: usize,
    md5_verified: Option<bool>,
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    if file_size < 8 {
        wrapper.push(check(
            "FORGE-FLAC-METADATA-HEADER",
            false,
            "FLAC is truncated before its first metadata block",
            None,
        ));
        return Ok(finish_audit(
            path,
            "flac",
            wrapper,
            bitstream,
            xcheck,
            json!({}),
        ));
    }

    let mut offset = 4_u64;
    let mut blocks = Vec::new();
    let mut streaminfo = None;
    let mut streaminfo_count = 0_usize;
    let mut seektable_count = 0_usize;
    let mut comment_count = 0_usize;
    let mut cuesheet_count = 0_usize;
    let mut icon_types = [0_usize; 2];
    let mut last_seen = false;
    let mut scan_ok = true;
    while !last_seen {
        if blocks.len() == MAX_METADATA_BLOCKS {
            wrapper.push(check(
                "FORGE-FLAC-METADATA-LIMIT",
                false,
                format!("metadata block count exceeds safety limit {MAX_METADATA_BLOCKS}"),
                Some(json!(blocks.len())),
            ));
            scan_ok = false;
            break;
        }
        if offset.checked_add(4).is_none_or(|end| end > file_size) {
            wrapper.push(check(
                "FORGE-FLAC-METADATA-HEADER",
                false,
                format!("truncated metadata header at byte {offset}"),
                Some(json!(offset)),
            ));
            scan_ok = false;
            break;
        }
        let header = read_at::<4>(path, &mut file, offset)?;
        let is_last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        let size = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        let payload = offset + 4;
        let end = match payload.checked_add(size) {
            Some(end) if end <= file_size => end,
            _ => {
                wrapper.push(check(
                    "FORGE-FLAC-METADATA-BOUNDS",
                    false,
                    format!("metadata block type {block_type} exceeds the file"),
                    Some(json!({"offset": offset, "size": size})),
                ));
                scan_ok = false;
                break;
            }
        };
        blocks.push(json!({
            "type": block_type,
            "name": metadata_name(block_type),
            "offset": offset,
            "size": size,
            "last": is_last
        }));
        if block_type == 127 {
            bitstream.push(check(
                "FORGE-FLAC-FORBIDDEN-METADATA",
                false,
                "metadata block type 127 is forbidden by RFC 9639",
                Some(json!(offset)),
            ));
        }
        match block_type {
            0 => {
                streaminfo_count += 1;
                bitstream.push(check(
                    "FORGE-FLAC-STREAMINFO-PLACEMENT",
                    blocks.len() == 1 && size == 34,
                    if blocks.len() == 1 && size == 34 {
                        "34-byte STREAMINFO is the first metadata block"
                    } else {
                        "STREAMINFO must be the first block and exactly 34 bytes"
                    },
                    Some(json!({"index": blocks.len() - 1, "size": size})),
                ));
                if size == 34 {
                    let bytes = read_at::<34>(path, &mut file, payload)?;
                    let parsed = parse_streaminfo(&bytes);
                    validate_streaminfo(parsed, &mut bitstream);
                    streaminfo = Some(parsed);
                }
            }
            1 => parse_padding(path, &mut file, payload, size, &mut bitstream)?,
            2 => {
                bitstream.push(check(
                    "FORGE-FLAC-APPLICATION-SIZE",
                    size >= 4,
                    if size >= 4 {
                        "APPLICATION block contains its 4-byte identifier"
                    } else {
                        "APPLICATION block is shorter than its identifier"
                    },
                    Some(json!(size)),
                ));
            }
            3 => {
                seektable_count += 1;
                parse_seektable(
                    path,
                    &mut file,
                    payload,
                    size,
                    streaminfo.map(|value| value.total_samples),
                    &mut bitstream,
                )?;
            }
            4 => {
                comment_count += 1;
                parse_vorbis_comment(path, &mut file, payload, size, &mut bitstream)?;
            }
            5 => {
                cuesheet_count += 1;
                parse_cuesheet(path, &mut file, payload, size, &mut bitstream)?;
            }
            6 => {
                if let Some(picture_type) =
                    parse_picture(path, &mut file, payload, size, &mut bitstream)?
                {
                    if (1..=2).contains(&picture_type) {
                        icon_types[picture_type as usize - 1] += 1;
                    }
                }
            }
            _ => {}
        }
        offset = end;
        last_seen = is_last;
    }
    wrapper.push(check(
        "FORGE-FLAC-METADATA-SCAN",
        scan_ok && last_seen,
        if scan_ok && last_seen {
            "metadata blocks are bounded and terminate with the last-block flag"
        } else {
            "metadata chain is incomplete or invalid"
        },
        Some(json!(&blocks)),
    ));
    wrapper.push(check(
        "FORGE-FLAC-STREAMINFO-COUNT",
        streaminfo_count == 1,
        format!("STREAMINFO block count is {streaminfo_count}; expected exactly one"),
        Some(json!(streaminfo_count)),
    ));
    for (rule, kind, count) in [
        ("FORGE-FLAC-SEEKTABLE-COUNT", "SEEKTABLE", seektable_count),
        (
            "FORGE-FLAC-VORBIS-COMMENT-COUNT",
            "VORBIS_COMMENT",
            comment_count,
        ),
        ("FORGE-FLAC-CUESHEET-COUNT", "CUESHEET", cuesheet_count),
    ] {
        bitstream.push(check(
            rule,
            count <= 1,
            format!("{kind} block count is {count}; at most one is permitted"),
            Some(json!({"type": kind, "count": count})),
        ));
    }
    bitstream.push(check(
        "FORGE-FLAC-ICON-UNIQUENESS",
        icon_types.iter().all(|count| *count <= 1),
        "PICTURE types 1 and 2 occur at most once each",
        Some(json!({"type_1": icon_types[0], "type_2": icon_types[1]})),
    ));
    let audio_present = scan_ok && last_seen && offset < file_size;
    wrapper.push(check(
        "FORGE-FLAC-AUDIO-PRESENT",
        audio_present,
        if audio_present {
            "one or more encoded audio-frame bytes follow metadata"
        } else {
            "FLAC requires audio frames after metadata"
        },
        Some(json!({"audio_offset": offset, "file_size": file_size})),
    ));

    let decoded = if audio_present && streaminfo.is_some() {
        match verify_decoded_audio(path) {
            Ok(decoded) => {
                bitstream.push(check(
                    "FORGE-FLAC-FRAME-DECODE",
                    decoded.frames > 0,
                    if decoded.frames > 0 {
                        "FLAC frames pass header/frame CRC parsing and strict decoding"
                    } else {
                        "no FLAC samples decoded"
                    },
                    Some(json!(decoded.frames)),
                ));
                Some(decoded)
            }
            Err(error) => {
                bitstream.push(check("FORGE-FLAC-FRAME-DECODE", false, error, None));
                None
            }
        }
    } else {
        None
    };
    if let (Some(info), Some(decoded)) = (streaminfo, decoded.as_ref()) {
        xcheck.push(check(
            "FORGE-FLAC-DECODED-FORMAT",
            info.sample_rate == decoded.sample_rate
                && usize::from(info.channels) == decoded.channels,
            if info.sample_rate == decoded.sample_rate
                && usize::from(info.channels) == decoded.channels
            {
                "decoded sample rate and channel count match STREAMINFO"
            } else {
                "decoded format does not match STREAMINFO"
            },
            Some(json!({
                "streaminfo": {"sample_rate": info.sample_rate, "channels": info.channels},
                "decoded": {"sample_rate": decoded.sample_rate, "channels": decoded.channels}
            })),
        ));
        if info.total_samples > 0 {
            xcheck.push(check(
                "FORGE-FLAC-TOTAL-SAMPLES",
                info.total_samples == decoded.frames,
                if info.total_samples == decoded.frames {
                    "decoded sample count matches STREAMINFO"
                } else {
                    "decoded sample count does not match STREAMINFO"
                },
                Some(json!({"streaminfo": info.total_samples, "decoded": decoded.frames})),
            ));
        }
        xcheck.push(check(
            "FORGE-FLAC-MD5",
            decoded.md5_verified != Some(false),
            match decoded.md5_verified {
                Some(true) => "decoded PCM matches the STREAMINFO MD5",
                Some(false) => "decoded PCM does not match the STREAMINFO MD5",
                None => "STREAMINFO omits MD5; RFC 9639 permits an all-zero digest",
            },
            Some(json!(decoded.md5_verified)),
        ));
    }

    Ok(finish_audit(
        path,
        "flac",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "metadata_blocks": blocks,
            "audio_offset": offset,
            "streaminfo": streaminfo,
            "decoded_integrity": decoded
        }),
    ))
}

fn read_at<const N: usize>(path: &Path, file: &mut File, offset: u64) -> Result<[u8; N], String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} to {offset}: {error}", path.display()))?;
    let mut bytes = [0; N];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read {} at {offset}: {error}", path.display()))?;
    Ok(bytes)
}

fn read_block(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    label: &str,
) -> Result<Option<Vec<u8>>, String> {
    if size > MAX_PARSED_METADATA_BYTES {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} {label}: {error}", path.display()))?;
    let mut bytes = vec![0; usize::try_from(size).unwrap()];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read {} {label}: {error}", path.display()))?;
    Ok(Some(bytes))
}

fn metadata_name(block_type: u8) -> &'static str {
    match block_type {
        0 => "STREAMINFO",
        1 => "PADDING",
        2 => "APPLICATION",
        3 => "SEEKTABLE",
        4 => "VORBIS_COMMENT",
        5 => "CUESHEET",
        6 => "PICTURE",
        127 => "FORBIDDEN",
        _ => "RESERVED",
    }
}

fn parse_padding(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let Some(bytes) = read_block(path, file, offset, size, "PADDING")? else {
        unreachable!("FLAC's 24-bit metadata size cannot exceed the parser limit");
    };
    let valid = bytes.iter().all(|byte| *byte == 0);
    checks.push(check(
        "FORGE-FLAC-PADDING",
        valid,
        if valid {
            "PADDING contains only zero bits"
        } else {
            "PADDING contains non-zero data"
        },
        Some(json!(size)),
    ));
    Ok(())
}

fn parse_streaminfo(bytes: &[u8; 34]) -> StreamInfo {
    let packed = u64::from_be_bytes(bytes[10..18].try_into().unwrap());
    StreamInfo {
        minimum_block_size: u16::from_be_bytes(bytes[0..2].try_into().unwrap()),
        maximum_block_size: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
        minimum_frame_size: u32::from_be_bytes([0, bytes[4], bytes[5], bytes[6]]),
        maximum_frame_size: u32::from_be_bytes([0, bytes[7], bytes[8], bytes[9]]),
        sample_rate: ((packed >> 44) & 0x0f_ffff) as u32,
        channels: (((packed >> 41) & 0x07) + 1) as u8,
        bits_per_sample: (((packed >> 36) & 0x1f) + 1) as u8,
        total_samples: packed & 0x0f_ffff_ffff,
        md5_present: bytes[18..34].iter().any(|byte| *byte != 0),
    }
}

fn validate_streaminfo(info: StreamInfo, checks: &mut Vec<AuditCheck>) {
    let blocks_ok =
        info.minimum_block_size >= 16 && info.maximum_block_size >= info.minimum_block_size;
    checks.push(check(
        "FORGE-FLAC-BLOCK-SIZES",
        blocks_ok,
        if blocks_ok {
            "STREAMINFO block-size range is valid"
        } else {
            "STREAMINFO block sizes must be at least 16 and ordered"
        },
        Some(json!({
            "minimum": info.minimum_block_size,
            "maximum": info.maximum_block_size
        })),
    ));
    let frames_ok = info.minimum_frame_size == 0
        || info.maximum_frame_size == 0
        || info.minimum_frame_size <= info.maximum_frame_size;
    checks.push(check(
        "FORGE-FLAC-FRAME-SIZES",
        frames_ok,
        if frames_ok {
            "STREAMINFO frame-size range is valid or unspecified"
        } else {
            "STREAMINFO minimum frame size exceeds maximum"
        },
        Some(json!({
            "minimum": info.minimum_frame_size,
            "maximum": info.maximum_frame_size
        })),
    ));
    checks.push(check(
        "FORGE-FLAC-AUDIO-DESCRIPTION",
        info.sample_rate > 0 && (4..=32).contains(&info.bits_per_sample),
        "sample rate must be non-zero and bit depth must be 4..=32",
        Some(json!({
            "sample_rate": info.sample_rate,
            "channels": info.channels,
            "bits_per_sample": info.bits_per_sample,
            "total_samples": info.total_samples,
            "md5_present": info.md5_present
        })),
    ));
}

fn parse_seektable(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    total_samples: Option<u64>,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let size_ok = size.is_multiple_of(18);
    checks.push(check(
        "FORGE-FLAC-SEEKTABLE-SIZE",
        size_ok,
        if size_ok {
            "SEEKTABLE consists of complete 18-byte seek points"
        } else {
            "SEEKTABLE length is not divisible by 18"
        },
        Some(json!(size)),
    ));
    if !size_ok {
        return Ok(());
    }
    let Some(bytes) = read_block(path, file, offset, size, "SEEKTABLE")? else {
        checks.push(check(
            "FORGE-FLAC-SEEKTABLE-LIMIT",
            false,
            format!("SEEKTABLE exceeds {MAX_PARSED_METADATA_BYTES} byte safety limit"),
            Some(json!(size)),
        ));
        return Ok(());
    };
    let mut previous = None;
    let mut placeholder_seen = false;
    let mut valid = true;
    let mut points = 0_usize;
    for point in bytes.chunks_exact(18) {
        let sample = u64::from_be_bytes(point[..8].try_into().unwrap());
        if sample == u64::MAX {
            placeholder_seen = true;
        } else {
            valid &= !placeholder_seen;
            valid &= previous.is_none_or(|previous| sample > previous);
            valid &= total_samples.is_none_or(|total| total == 0 || sample < total);
            previous = Some(sample);
        }
        points += 1;
    }
    checks.push(check(
        "FORGE-FLAC-SEEKTABLE-POINTS",
        valid,
        if valid {
            "seek points are ordered, unique, bounded, and placeholders trail"
        } else {
            "SEEKTABLE has unordered, duplicate, out-of-range, or malformed placeholder points"
        },
        Some(json!(points)),
    ));
    Ok(())
}

fn parse_cuesheet(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let Some(bytes) = read_block(path, file, offset, size, "CUESHEET")? else {
        unreachable!("FLAC's 24-bit metadata size cannot exceed the parser limit");
    };
    let mut valid = bytes.len() >= 396;
    let mut tracks_observed = 0_usize;
    let mut compact_disc = false;
    let mut declared_tracks = 0_u8;
    if valid {
        let catalog = &bytes[..128];
        let catalog_end = catalog
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(catalog.len());
        valid &= catalog[..catalog_end]
            .iter()
            .all(|byte| (0x20..=0x7e).contains(byte));
        valid &= catalog[catalog_end..].iter().all(|byte| *byte == 0);
        let lead_in = u64::from_be_bytes(bytes[128..136].try_into().unwrap());
        compact_disc = bytes[136] & 0x80 != 0;
        valid &= bytes[136] & 0x7f == 0;
        valid &= bytes[137..395].iter().all(|byte| *byte == 0);
        if !compact_disc {
            valid &= lead_in == 0;
        } else {
            valid &= lead_in >= 88_200;
        }
        declared_tracks = bytes[395];
        valid &= declared_tracks > 0 && (!compact_disc || declared_tracks <= 100);

        let mut cursor = 396_usize;
        let mut track_numbers = std::collections::HashSet::new();
        for track_index in 0..usize::from(declared_tracks) {
            let Some(track_end) = cursor.checked_add(36) else {
                valid = false;
                break;
            };
            let Some(track) = bytes.get(cursor..track_end) else {
                valid = false;
                break;
            };
            let track_offset = u64::from_be_bytes(track[..8].try_into().unwrap());
            let number = track[8];
            let isrc = &track[9..21];
            let flags = track[21];
            let index_count = track[35];
            let lead_out = track_index + 1 == usize::from(declared_tracks);
            valid &= number != 0 && track_numbers.insert(number);
            valid &= isrc.iter().all(|byte| *byte == 0)
                || isrc.iter().all(|byte| byte.is_ascii_alphanumeric());
            valid &= flags & 0x3f == 0 && track[22..35].iter().all(|byte| *byte == 0);
            valid &= if lead_out {
                index_count == 0 && number == if compact_disc { 170 } else { 255 }
            } else {
                index_count > 0 && (!compact_disc || (1..=99).contains(&number))
            };
            if compact_disc {
                valid &= track_offset % 588 == 0 && index_count <= 100;
            }
            cursor = track_end;
            let mut expected_index = None;
            for index_position in 0..usize::from(index_count) {
                let Some(index_end) = cursor.checked_add(12) else {
                    valid = false;
                    break;
                };
                let Some(index) = bytes.get(cursor..index_end) else {
                    valid = false;
                    break;
                };
                let index_offset = u64::from_be_bytes(index[..8].try_into().unwrap());
                let number = index[8];
                valid &= index[9..12].iter().all(|byte| *byte == 0);
                if index_position == 0 {
                    valid &= number <= 1;
                    expected_index = Some(number);
                }
                valid &= expected_index == Some(number);
                expected_index = number.checked_add(1);
                if compact_disc {
                    valid &= index_offset % 588 == 0;
                }
                cursor = index_end;
            }
            tracks_observed += 1;
        }
        valid &= tracks_observed == usize::from(declared_tracks) && cursor == bytes.len();
    }
    checks.push(check(
        "FORGE-FLAC-CUESHEET",
        valid,
        if valid {
            "CUESHEET header, tracks, indexes, reserved bits, and lead-out are valid"
        } else {
            "CUESHEET is truncated or violates track/index/CD-DA constraints"
        },
        Some(json!({
            "size": size,
            "compact_disc": compact_disc,
            "declared_tracks": declared_tracks,
            "parsed_tracks": tracks_observed
        })),
    ));
    Ok(())
}

fn parse_vorbis_comment(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    checks: &mut Vec<AuditCheck>,
) -> Result<(), String> {
    let Some(bytes) = read_block(path, file, offset, size, "VORBIS_COMMENT")? else {
        checks.push(check(
            "FORGE-FLAC-COMMENT-LIMIT",
            false,
            format!("VORBIS_COMMENT exceeds {MAX_PARSED_METADATA_BYTES} byte safety limit"),
            Some(json!(size)),
        ));
        return Ok(());
    };
    let mut cursor = 0_usize;
    let vendor = take_le_string(&bytes, &mut cursor);
    let count = take_le_u32(&bytes, &mut cursor);
    let mut valid = vendor
        .as_ref()
        .is_some_and(|value| std::str::from_utf8(value).is_ok())
        && count.is_some();
    let mut observed = 0_u32;
    if let Some(count) = count {
        for _ in 0..count {
            let Some(comment) = take_le_string(&bytes, &mut cursor) else {
                valid = false;
                break;
            };
            valid &= valid_comment(comment);
            observed += 1;
        }
        valid &= observed == count;
    }
    valid &= cursor == bytes.len();
    checks.push(check(
        "FORGE-FLAC-VORBIS-COMMENT",
        valid,
        if valid {
            "Vorbis vendor and comment vectors are bounded UTF-8 fields"
        } else {
            "VORBIS_COMMENT has invalid lengths, UTF-8, field names, or trailing bytes"
        },
        Some(json!({"declared_comments": count, "parsed_comments": observed})),
    ));
    Ok(())
}

fn take_le_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = cursor.checked_add(4)?;
    let value = u32::from_le_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

fn take_le_string<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let length = usize::try_from(take_le_u32(bytes, cursor)?).ok()?;
    let end = cursor.checked_add(length)?;
    let value = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(value)
}

fn valid_comment(comment: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(comment) else {
        return false;
    };
    let Some((name, _)) = text.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b'=')
}

fn parse_picture(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    checks: &mut Vec<AuditCheck>,
) -> Result<Option<u32>, String> {
    let Some(bytes) = read_block(path, file, offset, size, "PICTURE")? else {
        checks.push(check(
            "FORGE-FLAC-PICTURE-LIMIT",
            false,
            format!("PICTURE exceeds {MAX_PARSED_METADATA_BYTES} byte safety limit"),
            Some(json!(size)),
        ));
        return Ok(None);
    };
    let mut cursor = 0_usize;
    let picture_type = take_be_u32(&bytes, &mut cursor);
    let mime = take_be_string(&bytes, &mut cursor);
    let description = take_be_string(&bytes, &mut cursor);
    let dimensions = (
        take_be_u32(&bytes, &mut cursor),
        take_be_u32(&bytes, &mut cursor),
        take_be_u32(&bytes, &mut cursor),
        take_be_u32(&bytes, &mut cursor),
    );
    let data_length =
        take_be_u32(&bytes, &mut cursor).and_then(|value| usize::try_from(value).ok());
    let valid = picture_type.is_some()
        && mime
            .as_ref()
            .is_some_and(|value| value.iter().all(|byte| (0x20..=0x7e).contains(byte)))
        && description
            .as_ref()
            .is_some_and(|value| std::str::from_utf8(value).is_ok())
        && dimensions.0.is_some()
        && dimensions.1.is_some()
        && dimensions.2.is_some()
        && dimensions.3.is_some()
        && data_length.is_some_and(|length| cursor.checked_add(length) == Some(bytes.len()));
    checks.push(check(
        "FORGE-FLAC-PICTURE",
        valid,
        if valid {
            "PICTURE strings, dimensions, and image payload are bounded"
        } else {
            "PICTURE metadata is truncated or has invalid UTF-8/lengths"
        },
        Some(json!({"picture_type": picture_type, "data_bytes": data_length})),
    ));
    Ok(picture_type)
}

fn take_be_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = cursor.checked_add(4)?;
    let value = u32::from_be_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

fn take_be_string<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let length = usize::try_from(take_be_u32(bytes, cursor)?).ok()?;
    let end = cursor.checked_add(length)?;
    let value = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(value)
}

fn verify_decoded_audio(path: &Path) -> Result<DecodedIntegrity, String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let input = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let source = MediaSourceStream::new(Box::new(input), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");
    let mut format = get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("strict FLAC probe: {error}"))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "strict FLAC probe found no audio track".to_string())?
        .clone();
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| "strict FLAC probe found no audio codec parameters".to_string())?
        .clone();
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| "strict FLAC probe found no sample rate".to_string())?;
    let channels = codec_params
        .channels
        .as_ref()
        .map(|value| value.count())
        .ok_or_else(|| "strict FLAC probe found no channel layout".to_string())?;
    let mut decoder = get_codecs()
        // Verify the MD5 over the audible programme after codec delay/padding
        // trimming, matching the samples used by normalization and analysis.
        .make_audio_decoder(
            &codec_params,
            &AudioDecoderOptions::default().gapless(true).verify(true),
        )
        .map_err(|error| format!("create strict FLAC decoder: {error}"))?;
    let mut frames = 0_u64;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(error) => return Err(format!("read strict FLAC packet: {error}")),
        };
        if packet.track_id != track.id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|error| format!("decode strict FLAC packet: {error}"))?;
        frames = frames
            .checked_add(decoded.frames() as u64)
            .ok_or_else(|| "decoded FLAC sample count overflow".to_string())?;
    }
    let md5_verified = decoder.finalize().verify_ok;
    Ok(DecodedIntegrity {
        frames,
        sample_rate,
        channels,
        md5_verified,
    })
}

#[cfg(test)]
mod tests {
    fn valid_flac() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.flac");
        let mut writer =
            crate::flacenc::FlacStreamWriter::create(&path, 48_000, 2, 16, false).unwrap();
        writer
            .write_chunk(&[vec![0.1; 5_000], vec![-0.1; 5_000]])
            .unwrap();
        writer.finish().unwrap();
        (directory, path)
    }

    #[test]
    fn accepts_forge_flac_with_crc_count_and_md5_verification() {
        let (_directory, path) = valid_flac();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "flac");
        assert_eq!(audit.properties["decoded_integrity"]["frames"], 5_000);
        assert_eq!(audit.properties["decoded_integrity"]["md5_verified"], true);
    }

    #[test]
    fn rejects_corrupted_flac_frame() {
        let (_directory, path) = valid_flac();
        let mut bytes = std::fs::read(&path).unwrap();
        let index = bytes.len() - 5;
        bytes[index] ^= 0x40;
        std::fs::write(&path, bytes).unwrap();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed, "{audit:#?}");
    }

    #[test]
    fn rejects_streaminfo_with_invalid_size() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.flac");
        std::fs::write(&path, b"fLaC\x80\0\0\x21").unwrap();
        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed);
    }

    #[test]
    fn validates_non_cd_cuesheet_lead_out() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cuesheet.bin");
        let mut bytes = vec![0_u8; 396 + 36];
        bytes[395] = 1;
        bytes[396 + 8] = 255;
        std::fs::write(&path, &bytes).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut checks = Vec::new();
        super::parse_cuesheet(&path, &mut file, 0, bytes.len() as u64, &mut checks).unwrap();
        assert!(checks[0].passed, "{checks:#?}");

        bytes[136] = 1;
        std::fs::write(&path, &bytes).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        checks.clear();
        super::parse_cuesheet(&path, &mut file, 0, bytes.len() as u64, &mut checks).unwrap();
        assert!(!checks[0].passed);
    }
}
