//! Dependency-free ID3 and MPEG Layer III structural quality control.

use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_ID3_FRAMES: usize = 100_000;
const MAX_MP3_FRAMES: usize = 10_000_000;
const MAX_REPORTED_CRC_MISMATCHES: usize = 64;
const MAX_VBRI_TOC_ENTRIES: usize = 65_535;

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

impl FrameHeader {
    const fn side_info_size(self) -> usize {
        match (self.version, self.channels) {
            (MpegVersion::One, 1) => 17,
            (MpegVersion::One, _) => 32,
            (_, 1) => 9,
            (_, _) => 17,
        }
    }
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
    replaygain_peak_sample: Option<f64>,
    replaygain_radio_db: Option<f64>,
    replaygain_audiophile_db: Option<f64>,
    replaygain_valid: Option<bool>,
    tag_crc_expected: Option<u16>,
    tag_crc_actual: Option<u16>,
    tag_crc_valid: Option<bool>,
}

#[derive(Debug, Default)]
struct VbriInfo {
    present: bool,
    version: Option<u16>,
    delay: Option<u16>,
    quality: Option<u16>,
    declared_bytes: Option<u32>,
    declared_frames: Option<u32>,
    toc_entries: Option<u16>,
    toc_scale: Option<u16>,
    toc_entry_bytes: Option<u16>,
    toc_frames_per_entry: Option<u16>,
    toc_total_bytes: Option<u64>,
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
    let mut crc_mismatch_count = 0_usize;
    let mut crc_mismatch_offsets = Vec::new();
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
        if header.protected_by_crc {
            crc_frame_count += 1;
            if !validate_frame_crc(path, &mut file, offset, header)? {
                crc_mismatch_count += 1;
                if crc_mismatch_offsets.len() < MAX_REPORTED_CRC_MISMATCHES {
                    crc_mismatch_offsets.push(offset);
                }
            }
        }
        frame_count += 1;
        offset = frame_end;
    }

    bitstream.push(check(
        "FORGE-MP3-FRAME-CRC",
        crc_mismatch_count == 0,
        if crc_frame_count == 0 {
            "no CRC-protected MPEG Layer III frames are present".into()
        } else if crc_mismatch_count == 0 {
            format!("{crc_frame_count} protected frame CRCs match")
        } else {
            format!("{crc_mismatch_count} of {crc_frame_count} protected frame CRCs do not match")
        },
        Some(json!({
            "protected_frames": crc_frame_count,
            "mismatches": crc_mismatch_count,
            "mismatch_offsets": crc_mismatch_offsets,
            "reported_mismatch_limit": MAX_REPORTED_CRC_MISMATCHES
        })),
    ));
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
    let vbri = if first_header.is_some() {
        parse_vbri(&first_frame, &mut bitstream)
    } else {
        VbriInfo::default()
    };
    add_xing_cross_checks(
        &xing,
        frame_count,
        audio_end.saturating_sub(id3.audio_start),
        &bitrates,
        first_header.map(|header| header.version.samples_per_frame()),
        &mut xcheck,
    );
    add_vbri_cross_checks(
        &xing,
        &vbri,
        frame_count,
        audio_end.saturating_sub(id3.audio_start),
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
            "encoder_padding": xing.encoder_padding,
            "replaygain_peak_sample": xing.replaygain_peak_sample,
            "replaygain_radio_db": xing.replaygain_radio_db,
            "replaygain_audiophile_db": xing.replaygain_audiophile_db,
            "replaygain_valid": xing.replaygain_valid,
            "tag_crc_expected": xing.tag_crc_expected,
            "tag_crc_actual": xing.tag_crc_actual,
            "tag_crc_valid": xing.tag_crc_valid
        },
        "vbri": {
            "present": vbri.present,
            "version": vbri.version,
            "delay": vbri.delay,
            "quality": vbri.quality,
            "declared_bytes": vbri.declared_bytes,
            "declared_frames": vbri.declared_frames,
            "toc_entries": vbri.toc_entries,
            "toc_scale": vbri.toc_scale,
            "toc_entry_bytes": vbri.toc_entry_bytes,
            "toc_frames_per_entry": vbri.toc_frames_per_entry,
            "toc_total_bytes": vbri.toc_total_bytes
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
    // Xing deliberately keeps a fixed ancillary-data position even when the
    // MPEG protection bit is set. LAME subtracts the two CRC bytes from its
    // normal side-information offset for protected tag frames.
    let offset = 4 + header.side_info_size();
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
            if let Some(fields) = frame.get(cursor + 11..cursor + 19) {
                let peak = u32::from_be_bytes(fields[..4].try_into().unwrap());
                info.replaygain_peak_sample =
                    (peak != 0).then_some(32_767.0 * f64::from(peak) / 8_388_608.0);
                let radio = u16::from_be_bytes(fields[4..6].try_into().unwrap());
                let audiophile = u16::from_be_bytes(fields[6..8].try_into().unwrap());
                let parsed_radio = parse_lame_replaygain(radio, 1);
                let parsed_audiophile = parse_lame_replaygain(audiophile, 2);
                let replaygain_valid = parsed_radio.is_some() && parsed_audiophile.is_some();
                info.replaygain_radio_db = parsed_radio.flatten();
                info.replaygain_audiophile_db = parsed_audiophile.flatten();
                info.replaygain_valid = Some(replaygain_valid);
                valid &= replaygain_valid;
                checks.push(check(
                    "FORGE-MP3-LAME-REPLAYGAIN",
                    replaygain_valid,
                    if replaygain_valid {
                        "LAME peak and ReplayGain fields use valid names, origins, signs, and ranges"
                    } else {
                        "LAME ReplayGain fields contain a reserved name/origin or gain magnitude"
                    },
                    Some(json!({
                        "peak_fixed_9_23": peak,
                        "peak_sample": info.replaygain_peak_sample,
                        "radio_raw": radio,
                        "radio_db": info.replaygain_radio_db,
                        "audiophile_raw": audiophile,
                        "audiophile_db": info.replaygain_audiophile_db
                    })),
                ));
            } else {
                info.replaygain_valid = Some(false);
                valid = false;
                checks.push(check(
                    "FORGE-MP3-LAME-REPLAYGAIN",
                    false,
                    "truncated LAME peak or ReplayGain fields",
                    None,
                ));
            }
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
            if let Some(stored) = frame.get(cursor + 34..cursor + 36) {
                let expected = u16::from_be_bytes(stored.try_into().unwrap());
                let actual = crc16_ansi_reflected(0, &frame[..cursor + 34]);
                let crc_valid = expected == actual;
                info.tag_crc_expected = Some(expected);
                info.tag_crc_actual = Some(actual);
                info.tag_crc_valid = Some(crc_valid);
                valid &= crc_valid;
                checks.push(check(
                    "FORGE-MP3-LAME-TAG-CRC",
                    crc_valid,
                    if crc_valid {
                        "LAME tag CRC matches the complete tag prefix"
                    } else {
                        "LAME tag CRC does not match the complete tag prefix"
                    },
                    Some(json!({"expected": expected, "actual": actual})),
                ));
            } else {
                info.tag_crc_valid = Some(false);
                valid = false;
                checks.push(check(
                    "FORGE-MP3-LAME-TAG-CRC",
                    false,
                    "truncated LAME extension has no complete tag CRC",
                    None,
                ));
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

fn parse_lame_replaygain(value: u16, expected_name: u16) -> Option<Option<f64>> {
    if value == 0 {
        return Some(None);
    }
    let name = (value >> 13) & 0x7;
    let originator = (value >> 10) & 0x7;
    let magnitude = value & 0x1ff;
    if name != expected_name || originator > 3 || magnitude > 0x1fe {
        return None;
    }
    let gain = f64::from(magnitude) / 10.0;
    Some(Some(if value & 0x200 != 0 { -gain } else { gain }))
}

fn parse_vbri(frame: &[u8], checks: &mut Vec<AuditCheck>) -> VbriInfo {
    const VBRI_OFFSET: usize = 36;
    let Some(id) = frame.get(VBRI_OFFSET..VBRI_OFFSET + 4) else {
        checks.push(check(
            "FORGE-MP3-VBRI",
            true,
            "optional Fraunhofer VBRI header is absent",
            None,
        ));
        return VbriInfo::default();
    };
    if id != b"VBRI" {
        checks.push(check(
            "FORGE-MP3-VBRI",
            true,
            "optional Fraunhofer VBRI header is absent",
            None,
        ));
        return VbriInfo::default();
    }

    let mut info = VbriInfo {
        present: true,
        ..VbriInfo::default()
    };
    let Some(header) = frame.get(VBRI_OFFSET + 4..VBRI_OFFSET + 26) else {
        checks.push(check(
            "FORGE-MP3-VBRI",
            false,
            "truncated Fraunhofer VBRI fixed header",
            None,
        ));
        return info;
    };
    info.version = Some(u16::from_be_bytes(header[0..2].try_into().unwrap()));
    info.delay = Some(u16::from_be_bytes(header[2..4].try_into().unwrap()));
    info.quality = Some(u16::from_be_bytes(header[4..6].try_into().unwrap()));
    info.declared_bytes = Some(u32::from_be_bytes(header[6..10].try_into().unwrap()));
    info.declared_frames = Some(u32::from_be_bytes(header[10..14].try_into().unwrap()));
    info.toc_entries = Some(u16::from_be_bytes(header[14..16].try_into().unwrap()));
    info.toc_scale = Some(u16::from_be_bytes(header[16..18].try_into().unwrap()));
    info.toc_entry_bytes = Some(u16::from_be_bytes(header[18..20].try_into().unwrap()));
    info.toc_frames_per_entry = Some(u16::from_be_bytes(header[20..22].try_into().unwrap()));

    let entries = usize::from(info.toc_entries.unwrap());
    let entry_bytes = usize::from(info.toc_entry_bytes.unwrap());
    let table_size = entries.checked_mul(entry_bytes);
    let table_end = table_size.and_then(|size| (VBRI_OFFSET + 26).checked_add(size));
    let fixed_valid = info.version == Some(1)
        && info.declared_bytes.is_some_and(|value| value > 0)
        && info.declared_frames.is_some_and(|value| value > 0)
        && entries > 0
        && entries <= MAX_VBRI_TOC_ENTRIES
        && info.toc_scale.is_some_and(|value| value > 0)
        && (1..=4).contains(&entry_bytes)
        && info.toc_frames_per_entry.is_some_and(|value| value > 0);
    let table = table_end.and_then(|end| frame.get(VBRI_OFFSET + 26..end));
    let mut table_valid = fixed_valid && table.is_some();
    let mut total = 0_u64;
    if let Some(table) = table.filter(|_| entry_bytes > 0) {
        for entry in table.chunks_exact(entry_bytes) {
            let value = entry
                .iter()
                .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte));
            let scaled = u64::from(value).saturating_mul(u64::from(info.toc_scale.unwrap()));
            table_valid &= value > 0 && scaled != u64::MAX;
            total = total.saturating_add(scaled);
            table_valid &= total != u64::MAX;
        }
    }
    info.toc_total_bytes = table.is_some().then_some(total);
    if let Some(declared_bytes) = info.declared_bytes {
        table_valid &= total <= u64::from(declared_bytes);
    }
    checks.push(check(
        "FORGE-MP3-VBRI",
        table_valid,
        if table_valid {
            format!("{entries} Fraunhofer VBRI seek entries are structurally valid")
        } else {
            "invalid or truncated Fraunhofer VBRI header or seek table".into()
        },
        Some(json!({
            "version": info.version,
            "delay": info.delay,
            "quality": info.quality,
            "declared_bytes": info.declared_bytes,
            "declared_frames": info.declared_frames,
            "toc_entries": info.toc_entries,
            "toc_scale": info.toc_scale,
            "toc_entry_bytes": info.toc_entry_bytes,
            "toc_frames_per_entry": info.toc_frames_per_entry,
            "toc_total_bytes": info.toc_total_bytes
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

fn add_vbri_cross_checks(
    xing: &XingInfo,
    vbri: &VbriInfo,
    frame_count: usize,
    audio_bytes: u64,
    checks: &mut Vec<AuditCheck>,
) {
    let header_count = usize::from(xing.kind.is_some()) + usize::from(vbri.present);
    checks.push(check(
        "FORGE-MP3-VBR-HEADER",
        header_count <= 1,
        if header_count <= 1 {
            "Xing/Info and Fraunhofer VBRI headers are mutually exclusive"
        } else {
            "the first frame contains both Xing/Info and Fraunhofer VBRI headers"
        },
        Some(json!({
            "xing_or_info": xing.kind,
            "vbri": vbri.present
        })),
    ));
    if let Some(declared) = vbri.declared_frames {
        let matches = u64::from(declared) == u64::try_from(frame_count).unwrap_or(u64::MAX);
        checks.push(check(
            "FORGE-MP3-VBRI-FRAMES",
            matches,
            if matches {
                "VBRI frame count matches the scanned bitstream"
            } else {
                "VBRI frame count does not match the scanned bitstream"
            },
            Some(json!({"declared": declared, "scanned": frame_count})),
        ));
    }
    if let Some(declared) = vbri.declared_bytes {
        let matches = u64::from(declared) == audio_bytes;
        checks.push(check(
            "FORGE-MP3-VBRI-BYTES",
            matches,
            if matches {
                "VBRI byte count matches the MPEG audio region"
            } else {
                "VBRI byte count does not match the MPEG audio region"
            },
            Some(json!({"declared": declared, "scanned": audio_bytes})),
        ));
    }
}

fn validate_frame_crc(
    path: &Path,
    file: &mut File,
    offset: u64,
    header: FrameHeader,
) -> Result<bool, String> {
    let side_info_size = header.side_info_size();
    let protected_bytes = 6_u64
        .checked_add(u64::try_from(side_info_size).unwrap())
        .ok_or_else(|| "MP3 CRC range overflow".to_owned())?;
    if protected_bytes > u64::from(header.frame_size) {
        return Ok(false);
    }
    let protected_bytes = usize::try_from(protected_bytes).unwrap();
    let mut frame_prefix = [0_u8; 38];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut frame_prefix[..protected_bytes]))
        .map_err(|error| {
            format!(
                "read protected MP3 frame prefix in {} at byte {offset}: {error}",
                path.display()
            )
        })?;
    let expected = u16::from_be_bytes(frame_prefix[4..6].try_into().unwrap());
    let crc = crc16_mpeg(
        crc16_mpeg(0xffff, &frame_prefix[2..4]),
        &frame_prefix[6..protected_bytes],
    );
    Ok(crc == expected)
}

fn crc16_mpeg(mut crc: u16, bytes: &[u8]) -> u16 {
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc16_ansi_reflected(mut crc: u16, bytes: &[u8]) -> u16 {
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
    }
    crc
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

    fn protected_header(bitrate_index: u8, sample_index: u8, mono: bool) -> [u8; 4] {
        let mut bytes = header(bitrate_index, sample_index, mono);
        bytes[1] &= !1;
        bytes
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

    #[test]
    fn crc_algorithms_match_independent_check_values() {
        let mut protected = vec![0x90, 0x00];
        protected.extend_from_slice(&[0; 32]);
        assert_eq!(crc16_mpeg(0xffff, &protected), 0xc05c);
        assert_eq!(crc16_ansi_reflected(0, b"123456789"), 0xbb3d);
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

    #[test]
    fn audit_validates_protected_frame_crc() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protected.mp3");
        let header = protected_header(9, 0, false);
        let mut frame = vec![0_u8; 417];
        frame[..4].copy_from_slice(&header);
        let mut protected = Vec::from(&header[2..]);
        protected.extend_from_slice(&frame[6..38]);
        let crc = crc16_mpeg(0xffff, &protected);
        frame[4..6].copy_from_slice(&crc.to_be_bytes());
        std::fs::write(&path, &frame).unwrap();

        let valid = crate::container_qc::audit(&path).unwrap();
        assert!(valid.passed);
        assert_eq!(valid.properties["crc_frame_count"], 1);

        frame[12] = 1;
        std::fs::write(&path, frame).unwrap();
        let invalid = crate::container_qc::audit(&path).unwrap();
        assert!(!invalid.passed);
        assert!(invalid.layers[1].checks.iter().any(|item| {
            item.rule_id == "FORGE-MP3-FRAME-CRC"
                && !item.passed
                && item.observed.as_ref().unwrap()["mismatch_offsets"][0] == 0
        }));
    }

    fn put_vbri(bytes: &mut [u8], declared_frames: u32) {
        let offset = 36;
        bytes[offset..offset + 4].copy_from_slice(b"VBRI");
        bytes[offset + 4..offset + 6].copy_from_slice(&1_u16.to_be_bytes());
        bytes[offset + 6..offset + 8].copy_from_slice(&576_u16.to_be_bytes());
        bytes[offset + 8..offset + 10].copy_from_slice(&50_u16.to_be_bytes());
        bytes[offset + 10..offset + 14].copy_from_slice(&1_251_u32.to_be_bytes());
        bytes[offset + 14..offset + 18].copy_from_slice(&declared_frames.to_be_bytes());
        bytes[offset + 18..offset + 20].copy_from_slice(&declared_frames.to_be_bytes()[2..]);
        bytes[offset + 20..offset + 22].copy_from_slice(&1_u16.to_be_bytes());
        bytes[offset + 22..offset + 24].copy_from_slice(&2_u16.to_be_bytes());
        bytes[offset + 24..offset + 26].copy_from_slice(&1_u16.to_be_bytes());
        let values: &[u16] = if declared_frames == 3 {
            &[417, 417, 417]
        } else {
            &[417, 417, 416, 1]
        };
        for (index, value) in values.iter().enumerate() {
            let start = offset + 26 + index * 2;
            bytes[start..start + 2].copy_from_slice(&value.to_be_bytes());
        }
    }

    #[test]
    fn audit_parses_vbri_and_cross_checks_counts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vbri.mp3");
        let mut bytes = fake_frames(3);
        put_vbri(&mut bytes, 3);
        std::fs::write(&path, &bytes).unwrap();

        let valid = crate::container_qc::audit(&path).unwrap();
        assert!(valid.passed);
        assert_eq!(valid.properties["vbri"]["version"], 1);
        assert_eq!(valid.properties["vbri"]["toc_total_bytes"], 1_251);

        bytes[58..60].copy_from_slice(&5_u16.to_be_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let malformed = crate::container_qc::audit(&path).unwrap();
        assert!(!malformed.passed);
        assert!(malformed.layers[1]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MP3-VBRI" && !item.passed));

        bytes[58..60].copy_from_slice(&0_u16.to_be_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let zero_width = crate::container_qc::audit(&path).unwrap();
        assert!(!zero_width.passed);
        assert!(zero_width.layers[1]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MP3-VBRI" && !item.passed));

        put_vbri(&mut bytes, 4);
        std::fs::write(&path, bytes).unwrap();
        let invalid = crate::container_qc::audit(&path).unwrap();
        assert!(!invalid.passed);
        assert!(invalid.layers[2]
            .checks
            .iter()
            .any(|item| { item.rule_id == "FORGE-MP3-VBRI-FRAMES" && !item.passed }));
    }

    fn put_lame_info(bytes: &mut [u8], radio: u16) {
        let xing = 36;
        bytes[xing..xing + 4].copy_from_slice(b"Info");
        bytes[xing + 4..xing + 8].copy_from_slice(&3_u32.to_be_bytes());
        bytes[xing + 8..xing + 12].copy_from_slice(&2_u32.to_be_bytes());
        bytes[xing + 12..xing + 16].copy_from_slice(&1_251_u32.to_be_bytes());
        let lame = xing + 16;
        bytes[lame..lame + 9].copy_from_slice(b"LAME3.100");
        bytes[lame + 15..lame + 17].copy_from_slice(&radio.to_be_bytes());
        bytes[lame + 21..lame + 24].copy_from_slice(&[0x24, 0, 0]);
        let crc = crc16_ansi_reflected(0, &bytes[..lame + 34]);
        bytes[lame + 34..lame + 36].copy_from_slice(&crc.to_be_bytes());
    }

    #[test]
    fn audit_validates_lame_replaygain_and_tag_crc() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lame.mp3");
        let mut bytes = fake_frames(3);
        put_lame_info(&mut bytes, 0x2c0f);
        std::fs::write(&path, &bytes).unwrap();

        let valid = crate::container_qc::audit(&path).unwrap();
        assert!(valid.passed);
        assert_eq!(valid.properties["lame"]["replaygain_radio_db"], 1.5);
        assert_eq!(valid.properties["lame"]["tag_crc_valid"], true);

        let tag_crc_offset = 36 + 16 + 34;
        bytes[tag_crc_offset] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let bad_crc = crate::container_qc::audit(&path).unwrap();
        assert!(!bad_crc.passed);
        assert!(bad_crc.layers[1]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MP3-LAME-TAG-CRC" && !item.passed));

        put_lame_info(&mut bytes, 0x300f);
        std::fs::write(&path, bytes).unwrap();
        let invalid = crate::container_qc::audit(&path).unwrap();
        assert!(!invalid.passed);
        assert!(invalid.layers[1]
            .checks
            .iter()
            .any(|item| { item.rule_id == "FORGE-MP3-LAME-REPLAYGAIN" && !item.passed }));
    }
}
