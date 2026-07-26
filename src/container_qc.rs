//! Wrapper, bitstream, and metadata cross-checks for delivery containers.

use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub const CONTAINER_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/container-qc-v1";

#[derive(Debug, Clone, Serialize)]
pub struct ContainerAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub format: String,
    pub passed: bool,
    pub layers: Vec<AuditLayer>,
    pub properties: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditLayer {
    pub layer: &'static str,
    pub passed: bool,
    pub checks: Vec<AuditCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditCheck {
    pub rule_id: &'static str,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

pub fn audit(path: &Path) -> Result<ContainerAudit, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.starts_with(b"RIFF") || bytes.starts_with(b"RF64") || bytes.starts_with(b"BW64") {
        Ok(audit_wave(path, &bytes))
    } else if bytes.starts_with(b"OggS") {
        audit_ogg_opus(path)
    } else {
        Err(format!(
            "{}: unsupported container (expected RIFF/RF64/BW64 WAVE or Ogg Opus)",
            path.display()
        ))
    }
}

fn audit_ogg_opus(path: &Path) -> Result<ContainerAudit, String> {
    #[cfg(feature = "opus-encoding")]
    {
        let mut wrapper = Vec::new();
        let mut bitstream = Vec::new();
        let mut xcheck = Vec::new();
        let inspection = match crate::opus::inspect(path) {
            Ok(inspection) => {
                wrapper.push(check(
                    "FORGE-OGG-CRC",
                    true,
                    "every Ogg page checksum is valid",
                    None,
                ));
                wrapper.push(check(
                    "FORGE-OGG-SEQUENTIAL-CHAINS",
                    true,
                    format!("{} sequential logical stream(s)", inspection.chain_count),
                    Some(json!(inspection.chain_count)),
                ));
                bitstream.push(check(
                    "FORGE-OPUS-HEADERS",
                    true,
                    "every chain has valid OpusHead and OpusTags packets",
                    None,
                ));
                bitstream.push(check(
                    "FORGE-OPUS-GRANULES",
                    true,
                    "granule positions are monotonic and cover each pre-skip",
                    Some(json!(inspection.total_frames)),
                ));
                xcheck.push(check(
                    "FORGE-OPUS-CHAIN-LAYOUT",
                    true,
                    format!(
                        "all chains use the same {}-channel layout",
                        inspection.channels
                    ),
                    Some(json!(inspection.channels)),
                ));
                Some(inspection)
            }
            Err(error) => {
                let lower = error.to_ascii_lowercase();
                let wrapper_failure = lower.contains("hash mismatch")
                    || lower.contains("hashmismatch")
                    || lower.contains("checksum")
                    || lower.contains("crc")
                    || error.contains("Ogg");
                let target = if wrapper_failure {
                    &mut wrapper
                } else {
                    &mut bitstream
                };
                target.push(check(
                    if wrapper_failure {
                        "FORGE-OGG-WRAPPER"
                    } else {
                        "FORGE-OPUS-BITSTREAM"
                    },
                    false,
                    error,
                    None,
                ));
                None
            }
        };
        Ok(finish_audit(
            path,
            "ogg-opus",
            wrapper,
            bitstream,
            xcheck,
            inspection
                .map(|value| serde_json::to_value(value).expect("Opus inspection serializes"))
                .unwrap_or_else(|| json!({})),
        ))
    }
    #[cfg(not(feature = "opus-encoding"))]
    {
        let _ = path;
        Err("Ogg Opus QC requires `--features opus-encoding`".into())
    }
}

#[derive(Default)]
struct WaveState {
    container: String,
    chunks: Vec<String>,
    fmt: Option<WaveFormat>,
    data_size: Option<u64>,
    ds64_riff_size: Option<u64>,
    ds64_data_size: Option<u64>,
    ds64_sample_count: Option<u64>,
    bext_size: Option<u64>,
    axml: bool,
    chna: bool,
}

#[derive(Clone, Copy)]
struct WaveFormat {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
}

fn audit_wave(path: &Path, bytes: &[u8]) -> ContainerAudit {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let mut state = WaveState::default();
    if bytes.len() < 12 || &bytes[8..12] != b"WAVE" {
        wrapper.push(check(
            "FORGE-WAVE-SIGNATURE",
            false,
            "truncated or invalid WAVE signature",
            None,
        ));
        return finish_audit(path, "wave", wrapper, bitstream, xcheck, json!({}));
    }
    state.container = String::from_utf8_lossy(&bytes[..4]).into_owned();
    wrapper.push(check(
        "FORGE-WAVE-SIGNATURE",
        true,
        format!("{} WAVE signature is valid", state.container),
        Some(json!(state.container)),
    ));
    let declared_riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let large = matches!(&bytes[..4], b"RF64" | b"BW64");
    wrapper.push(check(
        "FORGE-WAVE-RIFF-SENTINEL",
        !large || declared_riff_size == u32::MAX,
        if !large || declared_riff_size == u32::MAX {
            "container size sentinel is valid"
        } else {
            "RF64/BW64 must use 0xffffffff in the RIFF size field"
        },
        Some(json!(declared_riff_size)),
    ));

    let mut offset = 12_usize;
    let mut chunk_index = 0_usize;
    let mut scan_ok = true;
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-HEADER",
                false,
                format!("truncated chunk header at byte {offset}"),
                Some(json!(offset)),
            ));
            scan_ok = false;
            break;
        }
        let id: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
        let id_text = String::from_utf8_lossy(&id).into_owned();
        let declared = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        offset += 8;
        chunk_index += 1;
        let effective = if id == *b"data" && declared == u32::MAX {
            state.ds64_data_size
        } else {
            Some(u64::from(declared))
        };
        let Some(size) = effective else {
            wrapper.push(check(
                "FORGE-WAVE-DS64-ORDER",
                false,
                "0xffffffff data size appears before a usable ds64 chunk",
                None,
            ));
            scan_ok = false;
            break;
        };
        let Ok(size_usize) = usize::try_from(size) else {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-SIZE",
                false,
                format!("{id_text} chunk is too large for this platform"),
                Some(json!(size)),
            ));
            scan_ok = false;
            break;
        };
        let Some(end) = offset.checked_add(size_usize) else {
            scan_ok = false;
            break;
        };
        if end > bytes.len() {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-BOUNDS",
                false,
                format!(
                    "{id_text} chunk ending at byte {end} exceeds file size {}",
                    bytes.len()
                ),
                Some(json!({"offset": offset, "size": size})),
            ));
            scan_ok = false;
            break;
        }
        let body = &bytes[offset..end];
        state.chunks.push(id_text.clone());
        match &id {
            b"ds64" => parse_ds64(body, chunk_index, &mut state, &mut wrapper, large),
            b"fmt " => {
                if state.fmt.is_some() {
                    bitstream.push(check(
                        "FORGE-WAVE-FMT-UNIQUE",
                        false,
                        "multiple fmt chunks are not allowed",
                        None,
                    ));
                } else {
                    state.fmt = parse_wave_fmt(body, &mut bitstream);
                }
            }
            b"data" => {
                if state.data_size.is_some() {
                    bitstream.push(check(
                        "FORGE-WAVE-DATA-UNIQUE",
                        false,
                        "multiple data chunks are not supported",
                        None,
                    ));
                } else {
                    state.data_size = Some(size);
                }
            }
            b"bext" => state.bext_size = Some(size),
            b"axml" => state.axml = true,
            b"chna" => state.chna = true,
            _ => {}
        }
        offset = end;
        if size & 1 == 1 {
            if offset == bytes.len() {
                wrapper.push(check(
                    "FORGE-WAVE-CHUNK-ALIGNMENT",
                    false,
                    format!("{id_text} chunk is missing its word-alignment pad byte"),
                    Some(json!(offset)),
                ));
                scan_ok = false;
                break;
            }
            offset += 1;
        }
    }
    wrapper.push(check(
        "FORGE-WAVE-CHUNK-SCAN",
        scan_ok && offset == bytes.len(),
        if scan_ok && offset == bytes.len() {
            format!("{} aligned chunk(s) cover the file", state.chunks.len())
        } else {
            "chunk table does not cover the file exactly".into()
        },
        Some(json!(state.chunks)),
    ));

    if large {
        wrapper.push(check(
            "FORGE-WAVE-DS64-REQUIRED",
            state.ds64_riff_size.is_some() && state.ds64_data_size.is_some(),
            "RF64/BW64 contains a usable ds64 chunk",
            state.ds64_riff_size.map(|size| json!(size)),
        ));
        if let Some(riff_size) = state.ds64_riff_size {
            wrapper.push(check(
                "FORGE-WAVE-RF64-SIZE",
                riff_size.checked_add(8) == Some(bytes.len() as u64),
                format!(
                    "ds64 RIFF size {} file size",
                    if riff_size.checked_add(8) == Some(bytes.len() as u64) {
                        "matches"
                    } else {
                        "does not match"
                    }
                ),
                Some(json!({"declared": riff_size, "actual": bytes.len() - 8})),
            ));
        }
    } else {
        wrapper.push(check(
            "FORGE-WAVE-RIFF-SIZE",
            u64::from(declared_riff_size).checked_add(8) == Some(bytes.len() as u64),
            format!(
                "RIFF size {} file size",
                if u64::from(declared_riff_size).checked_add(8) == Some(bytes.len() as u64) {
                    "matches"
                } else {
                    "does not match"
                }
            ),
            Some(json!({"declared": declared_riff_size, "actual": bytes.len() - 8})),
        ));
    }

    bitstream.push(check(
        "FORGE-WAVE-FMT-REQUIRED",
        state.fmt.is_some(),
        if state.fmt.is_some() {
            "fmt chunk is present"
        } else {
            "fmt chunk is missing"
        },
        None,
    ));
    bitstream.push(check(
        "FORGE-WAVE-DATA-REQUIRED",
        state.data_size.is_some(),
        if state.data_size.is_some() {
            "data chunk is present"
        } else {
            "data chunk is missing"
        },
        state.data_size.map(|size| json!(size)),
    ));

    let mut frames = None;
    if let (Some(fmt), Some(data_size)) = (state.fmt, state.data_size) {
        let frame_aligned = fmt.block_align != 0 && data_size % u64::from(fmt.block_align) == 0;
        bitstream.push(check(
            "FORGE-WAVE-FRAME-ALIGNMENT",
            frame_aligned,
            if frame_aligned {
                "data size is frame-aligned"
            } else {
                "data size is not divisible by block alignment"
            },
            Some(json!({"data_size": data_size, "block_align": fmt.block_align})),
        ));
        if frame_aligned {
            frames = Some(data_size / u64::from(fmt.block_align));
        }
        let expected_byte_rate = u64::from(fmt.sample_rate) * u64::from(fmt.block_align);
        xcheck.push(check(
            "FORGE-WAVE-BYTE-RATE-XCHECK",
            u64::from(fmt.byte_rate) == expected_byte_rate,
            "byte rate equals sample rate times block alignment",
            Some(json!({"declared": fmt.byte_rate, "expected": expected_byte_rate})),
        ));
        if matches!(fmt.tag, 1 | 3 | 0xfffe) && fmt.bits_per_sample % 8 == 0 {
            let expected_align = u32::from(fmt.channels) * u32::from(fmt.bits_per_sample / 8);
            xcheck.push(check(
                "FORGE-WAVE-BLOCK-ALIGN-XCHECK",
                u32::from(fmt.block_align) == expected_align,
                "block alignment matches channels and container bits",
                Some(json!({"declared": fmt.block_align, "expected": expected_align})),
            ));
        }
    }
    if let (Some(ds64_data), Some(data_size)) = (state.ds64_data_size, state.data_size) {
        xcheck.push(check(
            "FORGE-WAVE-DS64-DATA-XCHECK",
            ds64_data == data_size,
            "ds64 data size matches the data chunk",
            Some(json!({"ds64": ds64_data, "data": data_size})),
        ));
    }
    if let (Some(sample_count), Some(frames)) = (state.ds64_sample_count, frames) {
        xcheck.push(check(
            "FORGE-WAVE-DS64-SAMPLE-XCHECK",
            sample_count == frames,
            "ds64 sample count matches decoded frame count",
            Some(json!({"ds64": sample_count, "frames": frames})),
        ));
    }
    if let Some(size) = state.bext_size {
        xcheck.push(check(
            "FORGE-BWF-BEXT-SIZE",
            size >= 602,
            if size >= 602 {
                "bext fixed fields are complete"
            } else {
                "bext chunk is shorter than the 602-byte fixed fields"
            },
            Some(json!(size)),
        ));
    }
    if state.axml || state.chna {
        xcheck.push(check(
            "FORGE-ADM-CHUNK-PAIR",
            state.axml && state.chna,
            if state.axml && state.chna {
                "axml and chna ADM chunks are both present"
            } else {
                "ADM requires both axml and chna chunks"
            },
            Some(json!({"axml": state.axml, "chna": state.chna})),
        ));
    }
    let properties = json!({
        "container": state.container,
        "chunks": state.chunks,
        "data_size_bytes": state.data_size,
        "frames": frames,
        "sample_rate_hz": state.fmt.map(|fmt| fmt.sample_rate),
        "channels": state.fmt.map(|fmt| fmt.channels),
        "bits_per_sample": state.fmt.map(|fmt| fmt.bits_per_sample)
    });
    finish_audit(path, "wave", wrapper, bitstream, xcheck, properties)
}

fn parse_ds64(
    body: &[u8],
    chunk_index: usize,
    state: &mut WaveState,
    wrapper: &mut Vec<AuditCheck>,
    large: bool,
) {
    let valid_size = body.len() >= 28;
    wrapper.push(check(
        "FORGE-WAVE-DS64-SIZE",
        valid_size,
        if valid_size {
            "ds64 fixed fields are complete"
        } else {
            "ds64 chunk is shorter than 28 bytes"
        },
        Some(json!(body.len())),
    ));
    wrapper.push(check(
        "FORGE-WAVE-DS64-FIRST",
        !large || chunk_index == 1,
        if !large || chunk_index == 1 {
            "ds64 is the first RF64/BW64 chunk"
        } else {
            "ds64 must be the first RF64/BW64 chunk"
        },
        Some(json!(chunk_index)),
    ));
    if valid_size {
        state.ds64_riff_size = Some(u64::from_le_bytes(body[0..8].try_into().unwrap()));
        state.ds64_data_size = Some(u64::from_le_bytes(body[8..16].try_into().unwrap()));
        state.ds64_sample_count = Some(u64::from_le_bytes(body[16..24].try_into().unwrap()));
        let table_length = u32::from_le_bytes(body[24..28].try_into().unwrap()) as usize;
        let required = 28_usize.saturating_add(table_length.saturating_mul(12));
        wrapper.push(check(
            "FORGE-WAVE-DS64-TABLE",
            body.len() >= required,
            "ds64 table length fits the chunk",
            Some(json!({"entries": table_length, "size": body.len()})),
        ));
    }
}

fn parse_wave_fmt(body: &[u8], checks: &mut Vec<AuditCheck>) -> Option<WaveFormat> {
    if body.len() < 16 {
        checks.push(check(
            "FORGE-WAVE-FMT-SIZE",
            false,
            "fmt chunk is shorter than 16 bytes",
            Some(json!(body.len())),
        ));
        return None;
    }
    let format = WaveFormat {
        tag: u16::from_le_bytes(body[0..2].try_into().unwrap()),
        channels: u16::from_le_bytes(body[2..4].try_into().unwrap()),
        sample_rate: u32::from_le_bytes(body[4..8].try_into().unwrap()),
        byte_rate: u32::from_le_bytes(body[8..12].try_into().unwrap()),
        block_align: u16::from_le_bytes(body[12..14].try_into().unwrap()),
        bits_per_sample: u16::from_le_bytes(body[14..16].try_into().unwrap()),
    };
    checks.push(check(
        "FORGE-WAVE-FMT-VALUES",
        format.channels > 0
            && format.sample_rate > 0
            && format.block_align > 0
            && format.bits_per_sample > 0,
        "fmt channel, rate, alignment, and bit-depth fields are positive",
        Some(json!({
            "tag": format.tag,
            "channels": format.channels,
            "sample_rate_hz": format.sample_rate,
            "block_align": format.block_align,
            "bits_per_sample": format.bits_per_sample
        })),
    ));
    Some(format)
}

fn check(
    rule_id: &'static str,
    passed: bool,
    message: impl Into<String>,
    observed: Option<Value>,
) -> AuditCheck {
    AuditCheck {
        rule_id,
        passed,
        message: message.into(),
        observed,
    }
}

fn finish_audit(
    path: &Path,
    format: &str,
    wrapper: Vec<AuditCheck>,
    bitstream: Vec<AuditCheck>,
    xcheck: Vec<AuditCheck>,
    properties: Value,
) -> ContainerAudit {
    let mut layers = Vec::new();
    for (layer, checks) in [
        ("wrapper", wrapper),
        ("bitstream", bitstream),
        ("x-check", xcheck),
    ] {
        layers.push(AuditLayer {
            layer,
            passed: checks.iter().all(|check| check.passed),
            checks,
        });
    }
    let passed = layers.iter().all(|layer| layer.passed);
    ContainerAudit {
        schema: CONTAINER_QC_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        path: path.to_string_lossy().into_owned(),
        format: format.into(),
        passed,
        layers,
        properties,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, PcmKind, WavContainer, WavWriter};

    #[test]
    fn wave_audit_detects_wrapper_size_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("valid.wav");
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 10,
            data: vec![vec![0.0; 10]],
            channel_roles: crate::wav::default_channel_roles(1),
            source_kind: PcmKind::S16,
        };
        WavWriter::write(&path, &audio, PcmKind::S16, false).unwrap();
        let valid = audit(&path).unwrap();
        assert!(valid.passed);
        let mut bytes = fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let invalid = audit(&path).unwrap();
        assert!(!invalid.passed);
        assert!(invalid.layers[0]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-WAVE-RIFF-SIZE" && !check.passed));
    }

    #[test]
    fn rf64_and_bw64_ds64_cross_checks_pass() {
        let directory = tempfile::tempdir().unwrap();
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 101,
            data: vec![vec![0.0; 101], vec![0.0; 101]],
            channel_roles: crate::wav::default_channel_roles(2),
            source_kind: PcmKind::S24,
        };
        for container in [WavContainer::Rf64, WavContainer::Bw64] {
            let path = directory.path().join(format!("{container:?}.wav"));
            WavWriter::write_with_options(&path, &audio, PcmKind::S24, false, container, None)
                .unwrap();
            let audit = audit(&path).unwrap();
            assert!(audit.passed, "{container:?}: {audit:#?}");
        }
    }
}
