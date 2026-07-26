//! Bounded structural audits for classic PCM-oriented audio containers.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::json;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_CHUNKS: usize = 100_000;

fn read_at<const N: usize>(path: &Path, file: &mut File, offset: u64) -> Result<[u8; N], String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} to {offset}: {error}", path.display()))?;
    let mut bytes = [0; N];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read {} at {offset}: {error}", path.display()))?;
    Ok(bytes)
}

fn fourcc(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn extended80(bytes: &[u8; 10]) -> Option<f64> {
    let sign = if bytes[0] & 0x80 == 0 { 1.0 } else { -1.0 };
    let exponent = u16::from_be_bytes([bytes[0] & 0x7f, bytes[1]]);
    let mantissa = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
    if exponent == 0 && mantissa == 0 {
        return Some(0.0);
    }
    if exponent == 0x7fff || mantissa & (1_u64 << 63) == 0 {
        return None;
    }
    Some(sign * (mantissa as f64) * 2_f64.powi(i32::from(exponent) - 16383 - 63))
}

pub(crate) fn audit_aiff(
    path: &Path,
    mut file: File,
    file_size: u64,
) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    if file_size < 12 {
        wrapper.push(check(
            "FORGE-AIFF-HEADER",
            false,
            "AIFF header is truncated",
            None,
        ));
        return Ok(finish_audit(
            path,
            "aiff",
            wrapper,
            bitstream,
            xcheck,
            json!({}),
        ));
    }
    let header = read_at::<12>(path, &mut file, 0)?;
    let form = &header[8..12];
    let is_aifc = form == b"AIFC";
    let format = if is_aifc { "aifc" } else { "aiff" };
    let declared = u64::from(u32::from_be_bytes(header[4..8].try_into().unwrap())) + 8;
    wrapper.push(check(
        "FORGE-AIFF-FORM-SIZE",
        declared == file_size,
        if declared == file_size {
            "FORM size matches the file"
        } else {
            "FORM size does not match the file"
        },
        Some(json!({"declared_bytes": declared, "file_bytes": file_size})),
    ));

    let mut offset = 12_u64;
    let mut chunks = Vec::new();
    let mut counts: HashMap<[u8; 4], usize> = HashMap::new();
    let mut comm = None;
    let mut sound = None;
    let mut fver = None;
    let mut scan_ok = true;
    while offset < file_size {
        if chunks.len() == MAX_CHUNKS {
            wrapper.push(check(
                "FORGE-AIFF-CHUNK-LIMIT",
                false,
                format!("chunk count exceeds safety limit {MAX_CHUNKS}"),
                Some(json!(chunks.len())),
            ));
            scan_ok = false;
            break;
        }
        if file_size - offset < 8 {
            wrapper.push(check(
                "FORGE-AIFF-CHUNK-HEADER",
                false,
                format!("truncated chunk header at byte {offset}"),
                Some(json!(offset)),
            ));
            scan_ok = false;
            break;
        }
        let header = read_at::<8>(path, &mut file, offset)?;
        let id: [u8; 4] = header[..4].try_into().unwrap();
        let size = u64::from(u32::from_be_bytes(header[4..8].try_into().unwrap()));
        let data = offset + 8;
        let end = match data.checked_add(size) {
            Some(end) if end <= file_size => end,
            _ => {
                wrapper.push(check(
                    "FORGE-AIFF-CHUNK-BOUNDS",
                    false,
                    format!("{} chunk exceeds file bounds", fourcc(&id)),
                    Some(json!({"offset": offset, "size": size})),
                ));
                scan_ok = false;
                break;
            }
        };
        chunks.push(fourcc(&id));
        *counts.entry(id).or_default() += 1;
        match &id {
            b"COMM" => comm = Some((data, size, offset)),
            b"SSND" => sound = Some((data, size, offset)),
            b"FVER" => fver = Some((data, size, offset)),
            _ => {}
        }
        offset = match end.checked_add(size & 1) {
            Some(next) if next <= file_size => next,
            _ => {
                wrapper.push(check(
                    "FORGE-AIFF-CHUNK-PADDING",
                    false,
                    format!("{} chunk is missing its even-byte pad", fourcc(&id)),
                    Some(json!(end)),
                ));
                scan_ok = false;
                break;
            }
        };
    }
    wrapper.push(check(
        "FORGE-AIFF-CHUNK-SCAN",
        scan_ok && offset == file_size,
        if scan_ok && offset == file_size {
            "all IFF chunks are bounded and even-byte aligned"
        } else {
            "IFF chunk scan did not end at the file boundary"
        },
        Some(json!(&chunks)),
    ));
    for (id, rule, label) in [
        (*b"COMM", "FORGE-AIFF-COMM-COUNT", "COMM"),
        (*b"SSND", "FORGE-AIFF-SSND-COUNT", "SSND"),
    ] {
        let count = counts.get(&id).copied().unwrap_or(0);
        wrapper.push(check(
            rule,
            count == 1,
            format!("{label} chunk count is {count}; expected exactly one"),
            Some(json!(count)),
        ));
    }
    if let (Some((_, _, comm_offset)), Some((_, _, sound_offset))) = (comm, sound) {
        bitstream.push(check(
            "FORGE-AIFF-COMM-PLACEMENT",
            comm_offset < sound_offset,
            if comm_offset < sound_offset {
                "COMM precedes SSND"
            } else {
                "COMM must precede SSND"
            },
            Some(json!({"comm_offset": comm_offset, "ssnd_offset": sound_offset})),
        ));
    }

    let mut channels = None;
    let mut frames = None;
    let mut bits = None;
    let mut rate = None;
    let mut compression = None;
    if let Some((data, size, _)) = comm {
        let minimum = if is_aifc { 23 } else { 18 };
        let valid_size = size >= minimum;
        bitstream.push(check(
            "FORGE-AIFF-COMM-SIZE",
            valid_size,
            format!("COMM size is {size} bytes; minimum is {minimum}"),
            Some(json!(size)),
        ));
        if valid_size {
            let base = read_at::<18>(path, &mut file, data)?;
            channels = Some(u16::from_be_bytes(base[0..2].try_into().unwrap()));
            frames = Some(u32::from_be_bytes(base[2..6].try_into().unwrap()));
            bits = Some(u16::from_be_bytes(base[6..8].try_into().unwrap()));
            rate = extended80(base[8..18].try_into().unwrap());
            if is_aifc {
                compression = Some(fourcc(&read_at::<4>(path, &mut file, data + 18)?));
                let name_length = u64::from(read_at::<1>(path, &mut file, data + 22)?[0]);
                let name_ok = name_length <= size - 23;
                bitstream.push(check(
                    "FORGE-AIFC-COMPRESSION-NAME",
                    name_ok,
                    if name_ok {
                        "AIFC compression-name Pascal string is bounded"
                    } else {
                        "AIFC compression-name Pascal string exceeds COMM"
                    },
                    Some(json!(name_length)),
                ));
            }
            bitstream.push(check(
                "FORGE-AIFF-AUDIO-DESCRIPTION",
                channels.is_some_and(|value| value > 0)
                    && bits.is_some_and(|value| value > 0)
                    && rate.is_some_and(|value| value.is_finite() && value > 0.0),
                "channel count, sample size, and 80-bit sample rate must be positive",
                Some(json!({
                    "channels": channels,
                    "sample_frames": frames,
                    "bits_per_sample": bits,
                    "sample_rate": rate,
                    "compression": compression
                })),
            ));
        }
    }

    if is_aifc {
        let fver_count = counts.get(b"FVER").copied().unwrap_or(0);
        let comm_offset = comm.map(|(_, _, offset)| offset);
        let valid = fver.is_some_and(|(_, size, offset)| {
            size == 4 && comm_offset.is_some_and(|comm_offset| offset < comm_offset)
        }) && fver_count == 1;
        bitstream.push(check(
            "FORGE-AIFC-FVER",
            valid,
            if valid {
                "AIFC has one 4-byte format-version chunk before COMM"
            } else {
                "AIFC requires one 4-byte FVER chunk before COMM"
            },
            Some(json!(fver_count)),
        ));
        if let Some((data, 4, _)) = fver {
            let value = u32::from_be_bytes(read_at::<4>(path, &mut file, data)?);
            bitstream.push(check(
                "FORGE-AIFC-VERSION",
                value == 0xA280_5140,
                format!("AIFC version timestamp is 0x{value:08x}"),
                Some(json!(value)),
            ));
        }
    }

    if let Some((data, size, _)) = sound {
        let valid = size >= 8;
        bitstream.push(check(
            "FORGE-AIFF-SSND-SIZE",
            valid,
            format!("SSND payload size is {size} bytes"),
            Some(json!(size)),
        ));
        if valid {
            let ssnd = read_at::<8>(path, &mut file, data)?;
            let sound_offset = u64::from(u32::from_be_bytes(ssnd[..4].try_into().unwrap()));
            let block_size = u32::from_be_bytes(ssnd[4..8].try_into().unwrap());
            let available = size - 8;
            let offset_ok = sound_offset <= available;
            bitstream.push(check(
                "FORGE-AIFF-SSND-OFFSET",
                offset_ok,
                if offset_ok {
                    "SSND audio offset is within the chunk"
                } else {
                    "SSND audio offset exceeds the chunk"
                },
                Some(json!({"offset": sound_offset, "block_size": block_size})),
            ));
            if offset_ok {
                let audio_bytes = available - sound_offset;
                let pcm = !is_aifc
                    || compression
                        .as_deref()
                        .is_some_and(|value| matches!(value, "NONE" | "twos" | "sowt" | "raw "));
                if pcm {
                    if let (Some(channels), Some(frames), Some(bits)) = (channels, frames, bits) {
                        let bytes_per_sample = u64::from(bits).div_ceil(8);
                        let expected = u64::from(channels)
                            .checked_mul(u64::from(frames))
                            .and_then(|value| value.checked_mul(bytes_per_sample));
                        let passed = expected == Some(audio_bytes);
                        xcheck.push(check(
                            "FORGE-AIFF-PCM-BYTES",
                            passed,
                            if passed {
                                "SSND byte count matches COMM PCM geometry"
                            } else {
                                "SSND byte count does not match COMM PCM geometry"
                            },
                            Some(json!({"expected": expected, "observed": audio_bytes})),
                        ));
                    }
                }
            }
        }
    }

    Ok(finish_audit(
        path,
        format,
        wrapper,
        bitstream,
        xcheck,
        json!({
            "form_type": fourcc(form),
            "chunks": chunks,
            "channels": channels,
            "sample_frames": frames,
            "bits_per_sample": bits,
            "sample_rate": rate,
            "compression": compression
        }),
    ))
}

pub(crate) fn audit_caf(
    path: &Path,
    mut file: File,
    file_size: u64,
) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    if file_size < 8 {
        wrapper.push(check(
            "FORGE-CAF-HEADER",
            false,
            "CAF header is truncated",
            None,
        ));
        return Ok(finish_audit(
            path,
            "caf",
            wrapper,
            bitstream,
            xcheck,
            json!({}),
        ));
    }
    let header = read_at::<8>(path, &mut file, 0)?;
    let version = u16::from_be_bytes(header[4..6].try_into().unwrap());
    let flags = u16::from_be_bytes(header[6..8].try_into().unwrap());
    wrapper.push(check(
        "FORGE-CAF-HEADER",
        version == 1 && flags == 0,
        format!("CAF version is {version} and flags are 0x{flags:04x}"),
        Some(json!({"version": version, "flags": flags})),
    ));
    let mut offset = 8_u64;
    let mut chunks = Vec::new();
    let mut counts: HashMap<[u8; 4], usize> = HashMap::new();
    let mut desc = None;
    let mut data = None;
    let mut pakt = None;
    let mut scan_ok = true;
    while offset < file_size {
        if chunks.len() == MAX_CHUNKS {
            wrapper.push(check(
                "FORGE-CAF-CHUNK-LIMIT",
                false,
                format!("chunk count exceeds safety limit {MAX_CHUNKS}"),
                Some(json!(chunks.len())),
            ));
            scan_ok = false;
            break;
        }
        if file_size - offset < 12 {
            wrapper.push(check(
                "FORGE-CAF-CHUNK-HEADER",
                false,
                format!("truncated chunk header at byte {offset}"),
                Some(json!(offset)),
            ));
            scan_ok = false;
            break;
        }
        let chunk = read_at::<12>(path, &mut file, offset)?;
        let id: [u8; 4] = chunk[..4].try_into().unwrap();
        let signed_size = i64::from_be_bytes(chunk[4..12].try_into().unwrap());
        let payload = offset + 12;
        let size = if signed_size == -1 && id == *b"data" {
            file_size - payload
        } else if signed_size >= 0 {
            signed_size as u64
        } else {
            wrapper.push(check(
                "FORGE-CAF-CHUNK-SIZE",
                false,
                format!("{} has invalid negative size {signed_size}", fourcc(&id)),
                Some(json!(signed_size)),
            ));
            scan_ok = false;
            break;
        };
        let end = match payload.checked_add(size) {
            Some(end) if end <= file_size => end,
            _ => {
                wrapper.push(check(
                    "FORGE-CAF-CHUNK-BOUNDS",
                    false,
                    format!("{} chunk exceeds file bounds", fourcc(&id)),
                    Some(json!({"offset": offset, "size": size})),
                ));
                scan_ok = false;
                break;
            }
        };
        if signed_size == -1 && end != file_size {
            scan_ok = false;
        }
        chunks.push(fourcc(&id));
        *counts.entry(id).or_default() += 1;
        match &id {
            b"desc" => desc = Some((payload, size, offset)),
            b"data" => data = Some((payload, size, signed_size)),
            b"pakt" => pakt = Some((payload, size)),
            _ => {}
        }
        offset = end;
    }
    wrapper.push(check(
        "FORGE-CAF-CHUNK-SCAN",
        scan_ok && offset == file_size,
        if scan_ok && offset == file_size {
            "all CAF chunks are bounded"
        } else {
            "CAF chunk scan did not end at the file boundary"
        },
        Some(json!(&chunks)),
    ));
    for (id, rule, label) in [
        (*b"desc", "FORGE-CAF-DESC-COUNT", "desc"),
        (*b"data", "FORGE-CAF-DATA-COUNT", "data"),
    ] {
        let count = counts.get(&id).copied().unwrap_or(0);
        wrapper.push(check(
            rule,
            count == 1,
            format!("{label} chunk count is {count}; expected exactly one"),
            Some(json!(count)),
        ));
    }

    let mut sample_rate = None;
    let mut format_id = None;
    let mut bytes_per_packet = None;
    let mut frames_per_packet = None;
    let mut channels = None;
    let mut bits = None;
    let mut variable_packets = false;
    if let Some((payload, size, chunk_offset)) = desc {
        bitstream.push(check(
            "FORGE-CAF-DESC-PLACEMENT",
            chunk_offset == 8 && size == 32,
            if chunk_offset == 8 && size == 32 {
                "32-byte desc chunk immediately follows the CAF header"
            } else {
                "desc must be 32 bytes and immediately follow the CAF header"
            },
            Some(json!({"offset": chunk_offset, "size": size})),
        ));
        if size >= 32 {
            let value = read_at::<32>(path, &mut file, payload)?;
            sample_rate = Some(f64::from_bits(u64::from_be_bytes(
                value[..8].try_into().unwrap(),
            )));
            format_id = Some(fourcc(&value[8..12]));
            bytes_per_packet = Some(u32::from_be_bytes(value[16..20].try_into().unwrap()));
            frames_per_packet = Some(u32::from_be_bytes(value[20..24].try_into().unwrap()));
            channels = Some(u32::from_be_bytes(value[24..28].try_into().unwrap()));
            bits = Some(u32::from_be_bytes(value[28..32].try_into().unwrap()));
            let valid = sample_rate.is_some_and(|rate| rate.is_finite() && rate > 0.0)
                && channels.is_some_and(|value| value > 0);
            bitstream.push(check(
                "FORGE-CAF-AUDIO-DESCRIPTION",
                valid,
                "sample rate and channels per frame must be positive",
                Some(json!({
                    "sample_rate": sample_rate,
                    "format_id": format_id,
                    "bytes_per_packet": bytes_per_packet,
                    "frames_per_packet": frames_per_packet,
                    "channels": channels,
                    "bits_per_channel": bits
                })),
            ));
            variable_packets = bytes_per_packet == Some(0) || frames_per_packet == Some(0);
            let pakt_count = counts.get(b"pakt").copied().unwrap_or(0);
            bitstream.push(check(
                "FORGE-CAF-PACKET-TABLE",
                pakt_count <= 1 && (!variable_packets || pakt_count == 1),
                if pakt_count <= 1 && (!variable_packets || pakt_count == 1) {
                    "packet table requirement is satisfied"
                } else if pakt_count > 1 {
                    "CAF contains duplicate pakt chunks"
                } else {
                    "variable packet size/rate requires exactly one pakt chunk"
                },
                Some(json!({"variable": variable_packets, "packet_table_count": pakt_count})),
            ));
        }
    }
    let mut packet_count = None;
    if let Some((payload, size)) = pakt {
        let valid_size = size >= 24;
        bitstream.push(check(
            "FORGE-CAF-PAKT-SIZE",
            valid_size,
            if valid_size {
                "packet-table header is complete"
            } else {
                "pakt is too short for its 24-byte header"
            },
            Some(json!(size)),
        ));
        if valid_size {
            let value = read_at::<24>(path, &mut file, payload)?;
            let packets = i64::from_be_bytes(value[..8].try_into().unwrap());
            let valid_frames = i64::from_be_bytes(value[8..16].try_into().unwrap());
            let priming = i32::from_be_bytes(value[16..20].try_into().unwrap());
            let remainder = i32::from_be_bytes(value[20..24].try_into().unwrap());
            let values_ok = packets >= 0 && valid_frames >= 0 && priming >= 0 && remainder >= 0;
            bitstream.push(check(
                "FORGE-CAF-PAKT-VALUES",
                values_ok,
                "packet count, valid frames, priming, and remainder must be non-negative",
                Some(json!({
                    "packets": packets,
                    "valid_frames": valid_frames,
                    "priming_frames": priming,
                    "remainder_frames": remainder
                })),
            ));
            if packets >= 0 {
                packet_count = Some(packets as u64);
            }
        }
    }
    if let Some((_, size, signed_size)) = data {
        bitstream.push(check(
            "FORGE-CAF-DATA-EDIT-COUNT",
            size >= 4,
            if size >= 4 {
                "data chunk contains its edit-count field"
            } else {
                "data chunk is too short for its edit-count field"
            },
            Some(json!(size)),
        ));
        if signed_size == -1 {
            bitstream.push(check(
                "FORGE-CAF-UNKNOWN-DATA-SIZE",
                offset == file_size,
                "unknown-size data chunk extends to end of file",
                None,
            ));
        }
        if size >= 4 {
            if let Some(packet_bytes) = bytes_per_packet.filter(|value| *value > 0) {
                let audio_bytes = size - 4;
                let aligned = audio_bytes % u64::from(packet_bytes) == 0;
                xcheck.push(check(
                    "FORGE-CAF-PACKET-BYTES",
                    aligned,
                    if aligned {
                        "audio bytes are an integral number of constant-size packets"
                    } else {
                        "audio bytes are not aligned to bytes-per-packet"
                    },
                    Some(json!({"audio_bytes": audio_bytes, "bytes_per_packet": packet_bytes})),
                ));
                if let Some(packet_count) = packet_count {
                    let observed = audio_bytes / u64::from(packet_bytes);
                    xcheck.push(check(
                        "FORGE-CAF-PACKET-COUNT",
                        aligned && observed == packet_count,
                        if aligned && observed == packet_count {
                            "packet-table count matches constant-size audio data"
                        } else {
                            "packet-table count does not match constant-size audio data"
                        },
                        Some(json!({"packet_table": packet_count, "audio_data": observed})),
                    ));
                }
            }
        }
    }
    if channels.is_some_and(|value| value > 2) {
        let count = counts.get(b"chan").copied().unwrap_or(0);
        bitstream.push(check(
            "FORGE-CAF-CHANNEL-LAYOUT",
            count <= 1,
            if count == 1 {
                "multichannel CAF has one channel-layout chunk"
            } else if count == 0 {
                "multichannel CAF omits channel layout; this is valid only when channels have no defined ordering"
            } else {
                "CAF contains duplicate channel-layout chunks"
            },
            Some(json!(count)),
        ));
    }

    Ok(finish_audit(
        path,
        "caf",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "chunks": chunks,
            "sample_rate": sample_rate,
            "format_id": format_id,
            "bytes_per_packet": bytes_per_packet,
            "frames_per_packet": frames_per_packet,
            "variable_packets": variable_packets,
            "packet_count": packet_count,
            "channels": channels,
            "bits_per_channel": bits
        }),
    ))
}

pub(crate) fn audit_au(
    path: &Path,
    mut file: File,
    file_size: u64,
) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    if file_size < 24 {
        wrapper.push(check(
            "FORGE-AU-HEADER",
            false,
            "AU header is truncated",
            None,
        ));
        return Ok(finish_audit(
            path,
            "au",
            wrapper,
            bitstream,
            xcheck,
            json!({}),
        ));
    }
    let header = read_at::<24>(path, &mut file, 0)?;
    let data_offset = u64::from(u32::from_be_bytes(header[4..8].try_into().unwrap()));
    let declared = u32::from_be_bytes(header[8..12].try_into().unwrap());
    let encoding = u32::from_be_bytes(header[12..16].try_into().unwrap());
    let sample_rate = u32::from_be_bytes(header[16..20].try_into().unwrap());
    let channels = u32::from_be_bytes(header[20..24].try_into().unwrap());
    let offset_ok = data_offset >= 24 && data_offset <= file_size;
    wrapper.push(check(
        "FORGE-AU-DATA-OFFSET",
        offset_ok,
        if offset_ok {
            "AU data offset is within the file"
        } else {
            "AU data offset is outside the file"
        },
        Some(json!(data_offset)),
    ));
    let available = file_size.saturating_sub(data_offset);
    let data_bytes = if declared == u32::MAX {
        available
    } else {
        u64::from(declared)
    };
    let size_ok = offset_ok && data_bytes == available;
    wrapper.push(check(
        "FORGE-AU-DATA-SIZE",
        size_ok,
        if size_ok {
            "AU data size matches the file"
        } else {
            "AU declared data size does not match the file"
        },
        Some(json!({"declared": declared, "available": available})),
    ));
    bitstream.push(check(
        "FORGE-AU-AUDIO-DESCRIPTION",
        encoding != 0 && sample_rate > 0 && channels > 0,
        "encoding, sample rate, and channel count must be non-zero",
        Some(json!({
            "encoding": encoding,
            "sample_rate": sample_rate,
            "channels": channels
        })),
    ));
    let bytes_per_sample = match encoding {
        2 => Some(1_u64),
        3 => Some(2),
        4 => Some(3),
        5 | 6 => Some(4),
        7 => Some(8),
        _ => None,
    };
    if let Some(bytes_per_sample) = bytes_per_sample {
        let frame_bytes = u64::from(channels).checked_mul(bytes_per_sample);
        let aligned = frame_bytes.is_some_and(|value| value > 0 && data_bytes % value == 0);
        xcheck.push(check(
            "FORGE-AU-PCM-FRAMES",
            aligned,
            if aligned {
                "linear PCM payload contains complete sample frames"
            } else {
                "linear PCM payload ends within a sample frame"
            },
            Some(json!({"data_bytes": data_bytes, "frame_bytes": frame_bytes})),
        ));
    }
    Ok(finish_audit(
        path,
        "au",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "data_offset": data_offset,
            "data_bytes": data_bytes,
            "encoding": encoding,
            "sample_rate": sample_rate,
            "channels": channels
        }),
    ))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn temp(bytes: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    fn ext80_44100() -> [u8; 10] {
        [0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]
    }

    #[test]
    fn accepts_minimal_pcm_aiff() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FORM");
        bytes.extend_from_slice(&50_u32.to_be_bytes());
        bytes.extend_from_slice(b"AIFFCOMM");
        bytes.extend_from_slice(&18_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&16_u16.to_be_bytes());
        bytes.extend_from_slice(&ext80_44100());
        bytes.extend_from_slice(b"SSND");
        bytes.extend_from_slice(&12_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 1, 0, 2]);
        let file = temp(&bytes);
        let audit = crate::container_qc::audit(file.path()).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "aiff");
    }

    #[test]
    fn rejects_aiff_pcm_geometry_mismatch() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FORM");
        bytes.extend_from_slice(&48_u32.to_be_bytes());
        bytes.extend_from_slice(b"AIFFCOMM");
        bytes.extend_from_slice(&18_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&16_u16.to_be_bytes());
        bytes.extend_from_slice(&ext80_44100());
        bytes.extend_from_slice(b"SSND");
        bytes.extend_from_slice(&10_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 1]);
        let file = temp(&bytes);
        assert!(!crate::container_qc::audit(file.path()).unwrap().passed);
    }

    #[test]
    fn accepts_minimal_pcm_caf() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"caff");
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(b"desc");
        bytes.extend_from_slice(&32_i64.to_be_bytes());
        bytes.extend_from_slice(&44_100_f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(b"lpcm");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&16_u32.to_be_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&8_i64.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 1, 0, 2]);
        let file = temp(&bytes);
        let audit = crate::container_qc::audit(file.path()).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "caf");
    }

    #[test]
    fn rejects_variable_caf_without_packet_table() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"caff");
        bytes.extend_from_slice(&1_u16.to_be_bytes());
        bytes.extend_from_slice(&0_u16.to_be_bytes());
        bytes.extend_from_slice(b"desc");
        bytes.extend_from_slice(&32_i64.to_be_bytes());
        bytes.extend_from_slice(&48_000_f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(b"aac ");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&1024_u32.to_be_bytes());
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4_i64.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        let file = temp(&bytes);
        assert!(!crate::container_qc::audit(file.path()).unwrap().passed);
    }

    #[test]
    fn accepts_linear_pcm_au() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b".snd");
        bytes.extend_from_slice(&24_u32.to_be_bytes());
        bytes.extend_from_slice(&4_u32.to_be_bytes());
        bytes.extend_from_slice(&3_u32.to_be_bytes());
        bytes.extend_from_slice(&44_100_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 1, 0, 2]);
        let file = temp(&bytes);
        let audit = crate::container_qc::audit(file.path()).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "au");
    }

    #[test]
    fn rejects_truncated_au_data() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b".snd");
        bytes.extend_from_slice(&24_u32.to_be_bytes());
        bytes.extend_from_slice(&8_u32.to_be_bytes());
        bytes.extend_from_slice(&3_u32.to_be_bytes());
        bytes.extend_from_slice(&44_100_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&[0, 1]);
        let file = temp(&bytes);
        assert!(!crate::container_qc::audit(file.path()).unwrap().passed);
    }
}
