//! Dependency-free ID3 and MPEG Layer III structural quality control.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_ID3_FRAMES: usize = 100_000;
const MAX_MP3_FRAMES: usize = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpegVersion {
    One,
    Two,
    TwoPointFive,
}

impl MpegVersion {
    const fn name(self) -> &'static str {
        match self {
            Self::One => "MPEG-1",
            Self::Two => "MPEG-2",
            Self::TwoPointFive => "MPEG-2.5",
        }
    }

    const fn samples_per_frame(self) -> u32 {
        match self {
            Self::One => 1_152,
            Self::Two | Self::TwoPointFive => 576,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameHeader {
    version: MpegVersion,
    bitrate_kbps: u16,
    sample_rate: u32,
    channels: u8,
    protected_by_crc: bool,
    frame_size: u32,
}

#[derive(Debug, Default)]
struct Id3Info {
    present: bool,
    version: Option<String>,
    frame_count: usize,
    size: u64,
    audio_start: u64,
}

#[derive(Debug, Default)]
struct XingInfo {
    kind: Option<String>,
    flags: u32,
    declared_frames: Option<u32>,
    declared_bytes: Option<u32>,
    toc_monotonic: Option<bool>,
    vbr_scale: Option<u32>,
    encoder: Option<String>,
    encoder_delay: Option<u16>,
    encoder_padding: Option<u16>,
}

pub(crate) fn looks_like_mp3(header: &[u8]) -> bool {
    header.starts_with(b"ID3")
        || header
            .get(..4)
            .and_then(|bytes| bytes.try_into().ok())
            .and_then(parse_frame_header)
            .is_some()
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let id3 = parse_id3(path, &mut file, file_size, &mut wrapper)?;
    let (audio_end, id3v1, apev2) = trailing_metadata(path, &mut file, file_size)?;

    wrapper.push(check(
        "FORGE-MP3-TRAILING-METADATA",
        audio_end >= id3.audio_start,
        if audio_end >= id3.audio_start {
            "trailing ID3v1/APEv2 metadata boundaries are valid"
        } else {
            "trailing metadata overlaps the MPEG audio region"
        },
        Some(json!({"id3v1": id3v1, "apev2": apev2, "audio_end": audio_end})),
    ));

    let mut offset = id3.audio_start;
    let mut frame_count = 0_usize;
    let mut crc_frame_count = 0_usize;
    let mut bitrates = BTreeSet::new();
    let mut first_header: Option<FrameHeader> = None;
    let mut first_frame = Vec::new();
    let mut scan_ok = audio_end >= offset;
    while scan_ok && offset < audio_end {
        if frame_count == MAX_MP3_FRAMES {
            bitstream.push(check(
                "FORGE-MP3-FRAME-LIMIT",
                false,
                format!("frame count exceeds safety limit {MAX_MP3_FRAMES}"),
                Some(json!(frame_count)),
            ));
            scan_ok = false;
            break;
        }
        if audio_end - offset < 4 {
            bitstream.push(check(
                "FORGE-MP3-FRAME-BOUNDS",
                false,
                format!("truncated MPEG audio header at byte {offset}"),
                Some(json!(offset)),
            ));
            scan_ok = false;
            break;
        }
        let bytes = read_at::<4>(path, &mut file, offset)?;
        let Some(header) = parse_frame_header(bytes) else {
            bitstream.push(check(
                "FORGE-MP3-FRAME-HEADER",
                false,
                format!("invalid MPEG Layer III frame header at byte {offset}"),
                Some(json!({"offset": offset, "header": bytes})),
            ));
            scan_ok = false;
            break;
        };
        let frame_end = offset.saturating_add(u64::from(header.frame_size));
        if frame_end > audio_end {
            bitstream.push(check(
                "FORGE-MP3-FRAME-BOUNDS",
                false,
                format!("frame at byte {offset} ends at {frame_end}, beyond audio end {audio_end}"),
                Some(json!({"offset": offset, "frame_size": header.frame_size})),
            ));
            scan_ok = false;
            break;
        }
        if let Some(first) = first_header {
            let consistent = header.version == first.version
                && header.sample_rate == first.sample_rate
                && header.channels == first.channels;
            if !consistent {
                bitstream.push(check(
                    "FORGE-MP3-STREAM-CONFIG",
                    false,
                    format!("stream configuration changes at frame {frame_count}"),
                    Some(json!({
                        "offset": offset,
                        "version": header.version.name(),
                        "sample_rate": header.sample_rate,
                        "channels": header.channels
                    })),
                ));
                scan_ok = false;
                break;
            }
        } else {
            first_header = Some(header);
            first_frame.resize(header.frame_size as usize, 0);
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(&mut first_frame))
                .map_err(|error| format!("read first MP3 frame in {}: {error}", path.display()))?;
        }
        bitrates.insert(header.bitrate_kbps);
        crc_frame_count += usize::from(header.protected_by_crc);
        frame_count += 1;
        offset = frame_end;
    }

    bitstream.push(check(
        "FORGE-MP3-FRAME-SEQUENCE",
        scan_ok && frame_count > 0 && offset == audio_end,
        if scan_ok && frame_count > 0 && offset == audio_end {
            format!("{frame_count} MPEG Layer III frames are contiguous and complete")
        } else if frame_count == 0 {
            "no complete MPEG Layer III audio frame was found".into()
        } else {
            format!("MPEG frame scan stopped at byte {offset} before audio end {audio_end}")
        },
        Some(json!({"frame_count": frame_count, "scan_end": offset, "audio_end": audio_end})),
    ));

    let xing = if let Some(header) = first_header {
        parse_xing(&first_frame, header, &mut bitstream)
    } else {
        XingInfo::default()
    };
    add_xing_cross_checks(
        &xing,
        frame_count,
        audio_end.saturating_sub(id3.audio_start),
        &bitrates,
        first_header.map(|header| header.version.samples_per_frame()),
        &mut xcheck,
    );

    let coded_frame_count = frame_count.saturating_sub(usize::from(xing.kind.is_some()));
    let raw_samples = first_header.map(|header| {
        u64::try_from(coded_frame_count).unwrap_or(u64::MAX)
            * u64::from(header.version.samples_per_frame())
    });
    let gapless_samples = raw_samples.and_then(|samples| {
        let trim = u64::from(xing.encoder_delay.unwrap_or(0))
            + u64::from(xing.encoder_padding.unwrap_or(0));
        samples.checked_sub(trim)
    });
    let duration_seconds = gapless_samples
        .zip(first_header)
        .map(|(samples, header)| samples as f64 / header.sample_rate as f64);
    let properties = json!({
        "id3v2": {
            "present": id3.present,
            "version": id3.version,
            "size": id3.size,
            "frame_count": id3.frame_count
        },
        "id3v1": id3v1,
        "apev2": apev2,
        "mpeg_version": first_header.map(|header| header.version.name()),
        "layer": first_header.map(|_| 3),
        "sample_rate": first_header.map(|header| header.sample_rate),
        "channels": first_header.map(|header| header.channels),
        "frame_count": frame_count,
        "audio_frame_count": coded_frame_count,
        "crc_frame_count": crc_frame_count,
        "bitrates_kbps": bitrates,
        "raw_samples": raw_samples,
        "gapless_samples": gapless_samples,
        "duration_seconds": duration_seconds,
        "xing": {
            "kind": xing.kind,
            "flags": xing.flags,
            "declared_frames": xing.declared_frames,
            "declared_bytes": xing.declared_bytes,
            "toc_monotonic": xing.toc_monotonic,
            "vbr_scale": xing.vbr_scale
        },
        "lame": {
            "encoder": xing.encoder,
            "encoder_delay": xing.encoder_delay,
            "encoder_padding": xing.encoder_padding
        }
    });
    Ok(finish_audit(
        path, "mp3", wrapper, bitstream, xcheck, properties,
    ))
}

fn parse_id3(
    path: &Path,
    file: &mut File,
    file_size: u64,
    checks: &mut Vec<AuditCheck>,
) -> Result<Id3Info, String> {
    if file_size < 10 {
        return Ok(Id3Info::default());
    }
    let header = read_at::<10>(path, file, 0)?;
    if &header[..3] != b"ID3" {
        checks.push(check(
            "FORGE-MP3-ID3V2",
            true,
            "optional leading ID3v2 tag is absent",
            None,
        ));
        return Ok(Id3Info::default());
    }
    let major = header[3];
    let revision = header[4];
    let flags = header[5];
    let supported = matches!(major, 2..=4) && revision != 0xff;
    let allowed_flags = match major {
        2 => 0xc0,
        3 => 0xe0,
        4 => 0xf0,
        _ => 0,
    };
    let size = synchsafe(header[6..10].try_into().unwrap());
    let size_valid = size.is_some();
    let body_size = u64::from(size.unwrap_or(0));
    let footer_size = u64::from(major == 4 && flags & 0x10 != 0) * 10;
    let total_size = 10_u64
        .checked_add(body_size)
        .and_then(|value| value.checked_add(footer_size))
        .unwrap_or(u64::MAX);
    let bounds_valid = total_size <= file_size;
    let flags_valid = flags & !allowed_flags == 0;
    let footer_valid = if footer_size == 0 || !bounds_valid {
        footer_size == 0
    } else {
        let footer = read_at::<10>(path, file, 10 + body_size)?;
        &footer[..3] == b"3DI" && footer[3..] == header[3..]
    };
    checks.push(check(
        "FORGE-MP3-ID3V2",
        supported && size_valid && bounds_valid && flags_valid && footer_valid,
        if supported && size_valid && bounds_valid && flags_valid && footer_valid {
            format!("ID3v2.{major}.{revision} header and bounds are valid")
        } else {
            "invalid or unsupported ID3v2 header, flags, size, or bounds".into()
        },
        Some(json!({
            "major": major,
            "revision": revision,
            "flags": flags,
            "body_size": body_size,
            "total_size": total_size,
            "footer_valid": footer_valid
        })),
    ));
    let mut info = Id3Info {
        present: true,
        version: Some(format!("2.{major}.{revision}")),
        frame_count: 0,
        size: total_size.min(file_size),
        audio_start: total_size.min(file_size),
    };
    if !(supported && size_valid && bounds_valid && flags_valid && footer_valid) {
        return Ok(info);
    }

    let compressed_v22 = major == 2 && flags & 0x40 != 0;
    let unsynchronised = flags & 0x80 != 0;
    if compressed_v22 || unsynchronised {
        checks.push(check(
            "FORGE-MP3-ID3-FRAMES",
            true,
            "ID3 frame scan skipped because tag-wide compression/unsynchronisation is active",
            Some(json!({"compressed_v22": compressed_v22, "unsynchronised": unsynchronised})),
        ));
        return Ok(info);
    }

    let body_end = 10 + body_size;
    let mut offset = 10_u64;
    if flags & 0x40 != 0 && major >= 3 {
        let extended_size_bytes = read_at::<4>(path, file, offset)?;
        let extended_size = if major == 4 {
            synchsafe(extended_size_bytes).map(u64::from)
        } else {
            Some(u64::from(u32::from_be_bytes(extended_size_bytes)) + 4)
        };
        let Some(extended_size) = extended_size else {
            checks.push(check(
                "FORGE-MP3-ID3-FRAMES",
                false,
                "invalid ID3v2.4 extended-header size",
                None,
            ));
            return Ok(info);
        };
        offset = offset.saturating_add(extended_size);
        if offset > body_end {
            checks.push(check(
                "FORGE-MP3-ID3-FRAMES",
                false,
                "ID3 extended header exceeds the tag body",
                Some(json!(extended_size)),
            ));
            return Ok(info);
        }
    }

    let header_size = if major == 2 { 6_u64 } else { 10_u64 };
    let mut valid = true;
    while offset < body_end {
        if info.frame_count == MAX_ID3_FRAMES {
            valid = false;
            break;
        }
        if body_end - offset < header_size {
            valid = region_all_zero(path, file, offset, body_end)?;
            break;
        }
        let mut frame_header = [0_u8; 10];
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(&mut frame_header[..header_size as usize]))
            .map_err(|error| format!("read ID3 frame in {}: {error}", path.display()))?;
        if frame_header[..header_size as usize]
            .iter()
            .all(|byte| *byte == 0)
        {
            valid = region_all_zero(path, file, offset, body_end)?;
            break;
        }
        let id_size = if major == 2 { 3 } else { 4 };
        let id_valid = frame_header[..id_size]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
        let frame_size = if major == 2 {
            u32::from_be_bytes([0, frame_header[3], frame_header[4], frame_header[5]])
        } else if major == 4 {
            let Some(size) = synchsafe(frame_header[4..8].try_into().unwrap()) else {
                valid = false;
                break;
            };
            size
        } else {
            u32::from_be_bytes(frame_header[4..8].try_into().unwrap())
        };
        let flag_valid = match major {
            3 => frame_header[8] & 0x1f == 0 && frame_header[9] & 0x1f == 0,
            4 => frame_header[8] & 0x8f == 0 && frame_header[9] & 0xb0 == 0,
            _ => true,
        };
        let next = offset
            .checked_add(header_size)
            .and_then(|value| value.checked_add(u64::from(frame_size)))
            .unwrap_or(u64::MAX);
        if !id_valid || !flag_valid || frame_size == 0 || next > body_end {
            valid = false;
            break;
        }
        info.frame_count += 1;
        offset = next;
    }
    checks.push(check(
        "FORGE-MP3-ID3-FRAMES",
        valid,
        if valid {
            format!(
                "{} ID3 frame(s) and trailing padding are structurally valid",
                info.frame_count
            )
        } else {
            format!("invalid ID3 frame or padding near byte {offset}")
        },
        Some(json!({"frame_count": info.frame_count, "scan_end": offset})),
    ));
    Ok(info)
}

fn trailing_metadata(
    path: &Path,
    file: &mut File,
    file_size: u64,
) -> Result<(u64, bool, bool), String> {
    let mut end = file_size;
    let mut id3v1 = false;
    let mut apev2 = false;
    if end >= 128 && &read_at::<3>(path, file, end - 128)? == b"TAG" {
        id3v1 = true;
        end -= 128;
    }
    if end >= 32 {
        let footer = read_at::<32>(path, file, end - 32)?;
        if &footer[..8] == b"APETAGEX" {
            let size = u64::from(u32::from_le_bytes(footer[12..16].try_into().unwrap()));
            if size >= 32 && size <= end {
                apev2 = true;
                end -= size;
            }
        }
    }
    Ok((end, id3v1, apev2))
}

fn parse_frame_header(bytes: [u8; 4]) -> Option<FrameHeader> {
    let word = u32::from_be_bytes(bytes);
    if word >> 21 != 0x7ff {
        return None;
    }
    let version = match (word >> 19) & 0x3 {
        0 => MpegVersion::TwoPointFive,
        2 => MpegVersion::Two,
        3 => MpegVersion::One,
        _ => return None,
    };
    if (word >> 17) & 0x3 != 1 {
        return None;
    }
    let bitrate_index = ((word >> 12) & 0xf) as usize;
    if bitrate_index == 0 || bitrate_index == 15 {
        return None;
    }
    const MPEG1_BITRATES: [u16; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const MPEG2_BITRATES: [u16; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    let bitrate_kbps = if version == MpegVersion::One {
        MPEG1_BITRATES[bitrate_index]
    } else {
        MPEG2_BITRATES[bitrate_index]
    };
    let sample_index = ((word >> 10) & 0x3) as usize;
    if sample_index == 3 {
        return None;
    }
    let base_rate = [44_100, 48_000, 32_000][sample_index];
    let sample_rate = match version {
        MpegVersion::One => base_rate,
        MpegVersion::Two => base_rate / 2,
        MpegVersion::TwoPointFive => base_rate / 4,
    };
    let padding = (word >> 9) & 1;
    let coefficient = if version == MpegVersion::One {
        144_000
    } else {
        72_000
    };
    let frame_size = coefficient * u32::from(bitrate_kbps) / sample_rate + padding;
    Some(FrameHeader {
        version,
        bitrate_kbps,
        sample_rate,
        channels: if (word >> 6) & 0x3 == 3 { 1 } else { 2 },
        protected_by_crc: (word >> 16) & 1 == 0,
        frame_size,
    })
}

fn parse_xing(frame: &[u8], header: FrameHeader, checks: &mut Vec<AuditCheck>) -> XingInfo {
    let side_info = match (header.version, header.channels) {
        (MpegVersion::One, 1) => 17,
        (MpegVersion::One, _) => 32,
        (_, 1) => 9,
        (_, _) => 17,
    };
    let offset = 4 + usize::from(header.protected_by_crc) * 2 + side_info;
    let Some(kind) = frame.get(offset..offset + 4) else {
        checks.push(check(
            "FORGE-MP3-XING",
            true,
            "optional Xing/Info header is absent",
            None,
        ));
        return XingInfo::default();
    };
    if !matches!(kind, b"Xing" | b"Info") {
        checks.push(check(
            "FORGE-MP3-XING",
            true,
            "optional Xing/Info header is absent",
            None,
        ));
        return XingInfo::default();
    }
    let mut info = XingInfo {
        kind: Some(String::from_utf8_lossy(kind).into_owned()),
        ..XingInfo::default()
    };
    let Some(flags_bytes) = frame.get(offset + 4..offset + 8) else {
        checks.push(check(
            "FORGE-MP3-XING",
            false,
            "truncated Xing/Info flags",
            None,
        ));
        return info;
    };
    info.flags = u32::from_be_bytes(flags_bytes.try_into().unwrap());
    let mut cursor = offset + 8;
    let mut valid = info.flags & !0xf == 0;
    if info.flags & 1 != 0 {
        info.declared_frames = take_u32_be(frame, &mut cursor);
        valid &= info.declared_frames.is_some();
    }
    if info.flags & 2 != 0 {
        info.declared_bytes = take_u32_be(frame, &mut cursor);
        valid &= info.declared_bytes.is_some();
    }
    if info.flags & 4 != 0 {
        if let Some(toc) = frame.get(cursor..cursor + 100) {
            info.toc_monotonic = Some(toc.windows(2).all(|pair| pair[0] <= pair[1]));
            valid &= info.toc_monotonic == Some(true);
            cursor += 100;
        } else {
            valid = false;
        }
    }
    if info.flags & 8 != 0 {
        info.vbr_scale = take_u32_be(frame, &mut cursor);
        valid &= info.vbr_scale.is_some();
    }
    if let Some(encoder) = frame.get(cursor..cursor + 9) {
        if encoder.starts_with(b"LAME") {
            info.encoder = Some(
                String::from_utf8_lossy(encoder)
                    .trim_end_matches('\0')
                    .to_owned(),
            );
            if let Some(delay_padding) = frame.get(cursor + 21..cursor + 24) {
                let delay = (u16::from(delay_padding[0]) << 4) | u16::from(delay_padding[1] >> 4);
                let padding =
                    (u16::from(delay_padding[1] & 0x0f) << 8) | u16::from(delay_padding[2]);
                info.encoder_delay = Some(delay);
                info.encoder_padding = Some(padding);
                valid &= delay <= 3_000 && padding <= 3_000;
            } else {
                valid = false;
            }
        }
    }
    checks.push(check(
        "FORGE-MP3-XING",
        valid,
        if valid {
            format!(
                "{} header fields are structurally valid",
                info.kind.as_deref().unwrap()
            )
        } else {
            "invalid or truncated Xing/Info header fields".into()
        },
        Some(json!({
            "kind": info.kind,
            "flags": info.flags,
            "declared_frames": info.declared_frames,
            "declared_bytes": info.declared_bytes,
            "toc_monotonic": info.toc_monotonic
        })),
    ));
    info
}

fn add_xing_cross_checks(
    xing: &XingInfo,
    frame_count: usize,
    audio_bytes: u64,
    bitrates: &BTreeSet<u16>,
    samples_per_frame: Option<u32>,
    checks: &mut Vec<AuditCheck>,
) {
    if let Some(declared) = xing.declared_frames {
        let scanned_audio_frames = frame_count.saturating_sub(1);
        checks.push(check(
            "FORGE-MP3-XING-FRAMES",
            u64::from(declared) == u64::try_from(scanned_audio_frames).unwrap_or(u64::MAX),
            if u64::from(declared) == u64::try_from(scanned_audio_frames).unwrap_or(u64::MAX) {
                "Xing/Info frame count matches the scanned bitstream"
            } else {
                "Xing/Info frame count does not match the scanned bitstream"
            },
            Some(json!({"declared": declared, "scanned_audio_frames": scanned_audio_frames})),
        ));
    }
    if let Some(declared) = xing.declared_bytes {
        checks.push(check(
            "FORGE-MP3-XING-BYTES",
            u64::from(declared) == audio_bytes,
            if u64::from(declared) == audio_bytes {
                "Xing/Info byte count matches the MPEG audio region"
            } else {
                "Xing/Info byte count does not match the MPEG audio region"
            },
            Some(json!({"declared": declared, "scanned": audio_bytes})),
        ));
    }
    if xing.kind.as_deref() == Some("Info") {
        checks.push(check(
            "FORGE-MP3-INFO-CBR",
            bitrates.len() == 1,
            if bitrates.len() == 1 {
                "Info header identifies a constant-bitrate stream"
            } else {
                "Info header is inconsistent with changing frame bitrates"
            },
            Some(json!(bitrates)),
        ));
    }
    if let (Some(delay), Some(padding), Some(frames), Some(samples_per_frame)) = (
        xing.encoder_delay,
        xing.encoder_padding,
        xing.declared_frames,
        samples_per_frame,
    ) {
        let samples = u64::from(frames) * u64::from(samples_per_frame);
        checks.push(check(
            "FORGE-MP3-LAME-GAPLESS",
            u64::from(delay) + u64::from(padding) < samples,
            if u64::from(delay) + u64::from(padding) < samples {
                "LAME encoder delay and padding fit within the coded duration"
            } else {
                "LAME encoder delay and padding exceed the coded duration"
            },
            Some(json!({"delay": delay, "padding": padding, "coded_samples": samples})),
        ));
    }
}

fn take_u32_be(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let value = bytes
        .get(*cursor..cursor.checked_add(4)?)
        .map(|value| u32::from_be_bytes(value.try_into().unwrap()));
    *cursor = cursor.saturating_add(4);
    value
}

fn synchsafe(bytes: [u8; 4]) -> Option<u32> {
    if bytes.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    Some(
        (u32::from(bytes[0]) << 21)
            | (u32::from(bytes[1]) << 14)
            | (u32::from(bytes[2]) << 7)
            | u32::from(bytes[3]),
    )
}

fn region_all_zero(
    path: &Path,
    file: &mut File,
    mut offset: u64,
    end: u64,
) -> Result<bool, String> {
    let mut buffer = [0_u8; 8_192];
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    while offset < end {
        let count = usize::try_from((end - offset).min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..count])
            .map_err(|error| format!("read ID3 padding in {}: {error}", path.display()))?;
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        offset += count as u64;
    }
    Ok(true)
}

fn read_at<const N: usize>(path: &Path, file: &mut File, offset: u64) -> Result<[u8; N], String> {
    let mut bytes = [0_u8; N];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|error| format!("read {} at byte {offset}: {error}", path.display()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(bitrate_index: u8, sample_index: u8, mono: bool) -> [u8; 4] {
        let mut word = 0x7ff_u32 << 21;
        word |= 3 << 19;
        word |= 1 << 17;
        word |= 1 << 16;
        word |= u32::from(bitrate_index) << 12;
        word |= u32::from(sample_index) << 10;
        if mono {
            word |= 3 << 6;
        }
        word.to_be_bytes()
    }

    #[test]
    fn parses_mpeg_one_layer_three_frame_geometry() {
        let parsed = parse_frame_header(header(9, 0, false)).unwrap();
        assert_eq!(parsed.version, MpegVersion::One);
        assert_eq!(parsed.bitrate_kbps, 128);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.frame_size, 417);
    }

    #[test]
    fn rejects_reserved_and_free_format_headers() {
        assert!(parse_frame_header(header(0, 0, false)).is_none());
        let mut reserved = header(9, 0, false);
        reserved[1] &= !(0b11 << 3);
        reserved[1] |= 0b01 << 3;
        assert!(parse_frame_header(reserved).is_none());
    }

    #[test]
    fn synchsafe_rejects_high_bits() {
        assert_eq!(synchsafe([0, 0, 2, 1]), Some(257));
        assert_eq!(synchsafe([0x80, 0, 0, 0]), None);
    }

    fn fake_frames(count: usize) -> Vec<u8> {
        let header = header(9, 0, false);
        let mut bytes = Vec::new();
        for _ in 0..count {
            bytes.extend_from_slice(&header);
            bytes.resize(bytes.len() + 413, 0);
        }
        bytes
    }

    #[test]
    fn audit_detects_corrupt_frame_and_xing_count() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.mp3");
        let mut bytes = fake_frames(3);
        std::fs::write(&path, &bytes).unwrap();
        assert!(crate::container_qc::audit(&path).unwrap().passed);

        bytes[417] = 0;
        std::fs::write(&path, &bytes).unwrap();
        let corrupt = crate::container_qc::audit(&path).unwrap();
        assert!(!corrupt.passed);
        assert!(corrupt.layers[1]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MP3-FRAME-HEADER" && !item.passed));

        let mut xing = fake_frames(3);
        xing[36..40].copy_from_slice(b"Info");
        xing[40..44].copy_from_slice(&1_u32.to_be_bytes());
        xing[44..48].copy_from_slice(&1_u32.to_be_bytes());
        std::fs::write(&path, xing).unwrap();
        let mismatched = crate::container_qc::audit(&path).unwrap();
        assert!(!mismatched.passed);
        assert!(mismatched.layers[2]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MP3-XING-FRAMES" && !item.passed));
    }
}
