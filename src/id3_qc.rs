//! Bounded in-memory ID3v2 validation for timed metadata carriage.

use serde::Serialize;
use std::collections::HashSet;

const MAX_ID3_BYTES: usize = 16 * 1024 * 1024;
const MAX_ID3_FRAMES: usize = 100_000;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Id3Tag {
    pub version_major: u8,
    pub version_revision: u8,
    pub frame_count: usize,
    pub size_bytes: usize,
    pub relative_volume_adjustments: Vec<RelativeVolumeAdjustment>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RelativeVolumeAdjustment {
    pub identification: String,
    pub channels: Vec<RelativeVolumeChannel>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RelativeVolumeChannel {
    pub channel_type: u8,
    pub adjustment_db: f64,
    pub peak_bits: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_value_hex: Option<String>,
}

pub(crate) fn parse_prefix(bytes: &[u8], require_v24: bool) -> Result<(Id3Tag, usize), String> {
    if bytes.len() < 10 || &bytes[..3] != b"ID3" {
        return Err("timed metadata does not start with an ID3v2 header".into());
    }
    let major = bytes[3];
    let revision = bytes[4];
    if !(2..=4).contains(&major) || revision == 0xff || (require_v24 && major != 4) {
        return Err(if require_v24 {
            format!("CMAF timed metadata requires ID3v2.4, found ID3v2.{major}.{revision}")
        } else {
            format!("unsupported ID3v2 version {major}.{revision}")
        });
    }
    let allowed_flags = match major {
        2 => 0xc0,
        3 => 0xe0,
        4 => 0xf0,
        _ => unreachable!(),
    };
    let flags = bytes[5];
    if flags & !allowed_flags != 0 {
        return Err(format!("ID3v2.{major} header has reserved flag bits set"));
    }
    if flags & 0x80 != 0 {
        return Err("tag-wide ID3 unsynchronisation is not supported in timed metadata".into());
    }
    let body_size = synchsafe(bytes[6..10].try_into().expect("four-byte slice"))
        .ok_or_else(|| "ID3 tag size is not synchsafe".to_string())? as usize;
    let footer_size = usize::from(major == 4 && flags & 0x10 != 0) * 10;
    let total = 10_usize
        .checked_add(body_size)
        .and_then(|value| value.checked_add(footer_size))
        .ok_or_else(|| "ID3 tag size overflows".to_string())?;
    if total > MAX_ID3_BYTES {
        return Err(format!(
            "ID3 tag exceeds the {MAX_ID3_BYTES}-byte safety limit"
        ));
    }
    if total > bytes.len() {
        return Err(format!(
            "truncated ID3 tag declares {total} bytes but only {} are available",
            bytes.len()
        ));
    }
    if footer_size != 0 {
        let footer = &bytes[10 + body_size..total];
        if &footer[..3] != b"3DI" || footer[3..] != bytes[3..10] {
            return Err("ID3v2.4 footer does not mirror the header".into());
        }
    }

    let mut offset = 10_usize;
    let body_end = 10 + body_size;
    if flags & 0x40 != 0 {
        offset = skip_extended_header(bytes, major, offset, body_end)?;
    }

    let mut frame_count = 0_usize;
    let mut relative_volume_adjustments = Vec::new();
    let mut rva2_ids = HashSet::new();
    while offset < body_end {
        if bytes[offset..body_end].iter().all(|byte| *byte == 0) {
            break;
        }
        if frame_count == MAX_ID3_FRAMES {
            return Err(format!(
                "ID3 frame count exceeds the {MAX_ID3_FRAMES} safety limit"
            ));
        }
        let (identifier, frame_size, header_size, format_flags) =
            frame_header(bytes, major, offset, body_end)?;
        let data_start = offset + header_size;
        let data_end = data_start
            .checked_add(frame_size)
            .filter(|end| *end <= body_end)
            .ok_or_else(|| format!("ID3 frame {identifier} exceeds the declared tag body"))?;
        if format_flags {
            return Err(format!(
                "ID3 frame {identifier} uses compression, encryption, grouping, or frame unsynchronisation"
            ));
        }
        if identifier == "RVA2" {
            if major != 4 {
                return Err(format!(
                    "RVA2 is an ID3v2.4 frame but appears in ID3v2.{major}"
                ));
            }
            let adjustment = parse_rva2(&bytes[data_start..data_end])?;
            if !rva2_ids.insert(adjustment.identification.clone()) {
                return Err(format!(
                    "duplicate RVA2 identification {:?}",
                    adjustment.identification
                ));
            }
            relative_volume_adjustments.push(adjustment);
        }
        frame_count += 1;
        offset = data_end;
    }

    Ok((
        Id3Tag {
            version_major: major,
            version_revision: revision,
            frame_count,
            size_bytes: total,
            relative_volume_adjustments,
        },
        total,
    ))
}

fn skip_extended_header(
    bytes: &[u8],
    major: u8,
    offset: usize,
    body_end: usize,
) -> Result<usize, String> {
    if major == 2 || body_end.saturating_sub(offset) < 4 {
        return Err("truncated or unsupported ID3 extended header".into());
    }
    let raw: [u8; 4] = bytes[offset..offset + 4]
        .try_into()
        .expect("four-byte slice");
    let size = if major == 4 {
        synchsafe(raw).ok_or_else(|| "ID3v2.4 extended-header size is not synchsafe".to_string())?
            as usize
    } else {
        u32::from_be_bytes(raw) as usize + 4
    };
    if size < 4 || offset + size > body_end {
        return Err("ID3 extended header exceeds the tag body".into());
    }
    Ok(offset + size)
}

fn frame_header(
    bytes: &[u8],
    major: u8,
    offset: usize,
    body_end: usize,
) -> Result<(String, usize, usize, bool), String> {
    let header_size = if major == 2 { 6 } else { 10 };
    if body_end.saturating_sub(offset) < header_size {
        return Err("truncated ID3 frame header".into());
    }
    let id_size = if major == 2 { 3 } else { 4 };
    let identifier_bytes = &bytes[offset..offset + id_size];
    if !identifier_bytes
        .iter()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err("ID3 frame identifier contains invalid bytes".into());
    }
    let identifier =
        String::from_utf8(identifier_bytes.to_vec()).expect("validated ASCII ID3 identifier");
    let size = match major {
        2 => {
            (usize::from(bytes[offset + 3]) << 16)
                | (usize::from(bytes[offset + 4]) << 8)
                | usize::from(bytes[offset + 5])
        }
        3 => u32::from_be_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("four-byte slice"),
        ) as usize,
        4 => synchsafe(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("four-byte slice"),
        )
        .ok_or_else(|| format!("ID3v2.4 frame {identifier} size is not synchsafe"))?
            as usize,
        _ => unreachable!(),
    };
    if size == 0 {
        return Err(format!("ID3 frame {identifier} has an empty payload"));
    }
    let format_flags = match major {
        3 => {
            if bytes[offset + 8] & !0xe0 != 0 || bytes[offset + 9] & !0xe0 != 0 {
                return Err(format!("ID3v2.3 frame {identifier} has reserved flags set"));
            }
            bytes[offset + 9] & 0xe0 != 0
        }
        4 => {
            if bytes[offset + 8] & !0x70 != 0 || bytes[offset + 9] & !0x4f != 0 {
                return Err(format!("ID3v2.4 frame {identifier} has reserved flags set"));
            }
            bytes[offset + 9] & 0x4f != 0
        }
        _ => false,
    };
    Ok((identifier, size, header_size, format_flags))
}

fn parse_rva2(bytes: &[u8]) -> Result<RelativeVolumeAdjustment, String> {
    let separator = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "RVA2 identification is not terminated".to_string())?;
    if separator == 0 {
        return Err("RVA2 identification is empty".into());
    }
    let identification = bytes[..separator].iter().copied().map(char::from).collect();
    let mut offset = separator + 1;
    let mut channels = Vec::new();
    let mut channel_types = HashSet::new();
    while offset < bytes.len() {
        if bytes.len() - offset < 4 {
            return Err("RVA2 channel entry is truncated".into());
        }
        let channel_type = bytes[offset];
        if channel_type > 8 || !channel_types.insert(channel_type) {
            return Err(format!(
                "RVA2 channel type {channel_type} is unknown or duplicated"
            ));
        }
        let adjustment = i16::from_be_bytes([bytes[offset + 1], bytes[offset + 2]]);
        let peak_bits = bytes[offset + 3];
        offset += 4;
        let peak_bytes = usize::from(peak_bits).div_ceil(8);
        if bytes.len() - offset < peak_bytes {
            return Err("RVA2 peak value is truncated".into());
        }
        let peak_value_hex = if peak_bits == 0 {
            None
        } else {
            let unused = peak_bytes * 8 - usize::from(peak_bits);
            if unused > 0 && bytes[offset] >> (8 - unused) != 0 {
                return Err("RVA2 peak value has non-zero most-significant padding bits".into());
            }
            Some(
                bytes[offset..offset + peak_bytes]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        };
        offset += peak_bytes;
        channels.push(RelativeVolumeChannel {
            channel_type,
            adjustment_db: f64::from(adjustment) / 512.0,
            peak_bits,
            peak_value_hex,
        });
    }
    if channels.is_empty() {
        return Err("RVA2 contains no channel adjustments".into());
    }
    Ok(RelativeVolumeAdjustment {
        identification,
        channels,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(frame: &[u8]) -> Vec<u8> {
        let mut output = b"ID3\x04\x00\x00".to_vec();
        output.extend_from_slice(&[0, 0, 0, frame.len() as u8]);
        output.extend_from_slice(frame);
        output
    }

    fn rva2(identification: &[u8], adjustment: i16) -> Vec<u8> {
        let mut payload = identification.to_vec();
        payload.push(0);
        payload.push(1);
        payload.extend_from_slice(&adjustment.to_be_bytes());
        payload.push(0);
        let mut frame = b"RVA2".to_vec();
        frame.extend_from_slice(&[0, 0, 0, payload.len() as u8, 0, 0]);
        frame.extend_from_slice(&payload);
        frame
    }

    #[test]
    fn parses_complete_rva2() {
        let bytes = tag(&rva2(b"track", -512));
        let (parsed, consumed) = parse_prefix(&bytes, true).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed.frame_count, 1);
        assert_eq!(
            parsed.relative_volume_adjustments[0].identification,
            "track"
        );
        assert_eq!(
            parsed.relative_volume_adjustments[0].channels[0].adjustment_db,
            -1.0
        );
    }

    #[test]
    fn rejects_duplicate_rva2_identity() {
        let frames = [rva2(b"track", 0), rva2(b"track", 1)].concat();
        let error = parse_prefix(&tag(&frames), true).unwrap_err();
        assert!(error.contains("duplicate RVA2"));
    }

    #[test]
    fn requires_v24_for_cmaf() {
        let mut bytes = tag(&rva2(b"track", 0));
        bytes[3] = 3;
        let error = parse_prefix(&bytes, true).unwrap_err();
        assert!(error.contains("requires ID3v2.4"));
    }

    #[test]
    fn rejects_rva2_in_an_older_tag_version() {
        let mut bytes = tag(&rva2(b"track", 0));
        bytes[3] = 3;
        let error = parse_prefix(&bytes, false).unwrap_err();
        assert!(error.contains("RVA2 is an ID3v2.4 frame"));
    }
}
