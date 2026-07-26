//! Wrapper, bitstream, and metadata cross-checks for delivery containers.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const CONTAINER_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/container-qc-v1";
const MAX_WAVE_CHUNKS: usize = 100_000;
const MAX_CONTROL_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

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
    audit_if_supported(path)?.ok_or_else(|| {
        format!(
            "{}: unsupported container (expected WAVE, AIFF/AIFC, CAF, AU, FLAC, Ogg Opus/Vorbis, or ISO-BMFF MP4/M4A/fMP4)",
            path.display()
        )
    })
}

pub fn audit_if_supported(path: &Path) -> Result<Option<ContainerAudit>, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    let mut header = [0_u8; 12];
    let header_size = usize::try_from(file_size.min(header.len() as u64)).unwrap();
    file.read_exact(&mut header[..header_size])
        .map_err(|error| format!("read {} header: {error}", path.display()))?;
    if header_size >= 4 && matches!(&header[..4], b"RIFF" | b"RF64" | b"BW64") {
        audit_wave(path, &mut file, &header[..header_size], file_size).map(Some)
    } else if header_size >= 12
        && &header[..4] == b"FORM"
        && matches!(&header[8..12], b"AIFF" | b"AIFC")
    {
        crate::pcm_container_qc::audit_aiff(path, file, file_size).map(Some)
    } else if header_size >= 4 && &header[..4] == b"caff" {
        crate::pcm_container_qc::audit_caf(path, file, file_size).map(Some)
    } else if header_size >= 4 && &header[..4] == b".snd" {
        crate::pcm_container_qc::audit_au(path, file, file_size).map(Some)
    } else if header_size >= 4 && &header[..4] == b"fLaC" {
        crate::flac_qc::audit(path, file, file_size).map(Some)
    } else if header_size >= 4 && &header[..4] == b"OggS" {
        crate::ogg_qc::audit(path).map(Some)
    } else if crate::isobmff_qc::looks_like_isobmff(&header[..header_size], file_size) {
        crate::isobmff_qc::audit(path, file, file_size).map(Some)
    } else {
        Ok(None)
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
    ds64_table: HashMap<[u8; 4], VecDeque<u64>>,
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

fn audit_wave(
    path: &Path,
    file: &mut File,
    header: &[u8],
    file_size: u64,
) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();
    let mut state = WaveState::default();
    if header.len() < 12 || &header[8..12] != b"WAVE" {
        wrapper.push(check(
            "FORGE-WAVE-SIGNATURE",
            false,
            "truncated or invalid WAVE signature",
            None,
        ));
        return Ok(finish_audit(
            path,
            "wave",
            wrapper,
            bitstream,
            xcheck,
            json!({}),
        ));
    }
    state.container = String::from_utf8_lossy(&header[..4]).into_owned();
    wrapper.push(check(
        "FORGE-WAVE-SIGNATURE",
        true,
        format!("{} WAVE signature is valid", state.container),
        Some(json!(state.container)),
    ));
    let declared_riff_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let large = matches!(&header[..4], b"RF64" | b"BW64");
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

    let mut offset = 12_u64;
    let mut chunk_index = 0_usize;
    let mut scan_ok = true;
    while offset < file_size {
        if chunk_index == MAX_WAVE_CHUNKS {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-LIMIT",
                false,
                format!("chunk count exceeds safety limit {MAX_WAVE_CHUNKS}"),
                Some(json!(chunk_index)),
            ));
            scan_ok = false;
            break;
        }
        if file_size - offset < 8 {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-HEADER",
                false,
                format!("truncated chunk header at byte {offset}"),
                Some(json!(offset)),
            ));
            scan_ok = false;
            break;
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} to {offset}: {error}", path.display()))?;
        let mut chunk_header = [0_u8; 8];
        file.read_exact(&mut chunk_header)
            .map_err(|error| format!("read {} chunk at {offset}: {error}", path.display()))?;
        let id: [u8; 4] = chunk_header[..4].try_into().unwrap();
        let id_text = String::from_utf8_lossy(&id).into_owned();
        let declared = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap());
        offset += 8;
        chunk_index += 1;
        let effective = if declared != u32::MAX {
            Some(u64::from(declared))
        } else if id == *b"data" {
            state.ds64_data_size
        } else {
            state.ds64_table.get_mut(&id).and_then(VecDeque::pop_front)
        };
        let Some(size) = effective else {
            wrapper.push(check(
                "FORGE-WAVE-DS64-ORDER",
                false,
                format!("0xffffffff {id_text} size appears before a matching ds64 size entry"),
                Some(json!(id_text)),
            ));
            scan_ok = false;
            break;
        };
        let Some(end) = offset.checked_add(size) else {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-SIZE",
                false,
                format!("{id_text} chunk size overflows its file offset"),
                Some(json!(size)),
            ));
            scan_ok = false;
            break;
        };
        if end > file_size {
            wrapper.push(check(
                "FORGE-WAVE-CHUNK-BOUNDS",
                false,
                format!("{id_text} chunk ending at byte {end} exceeds file size {file_size}"),
                Some(json!({"offset": offset, "size": size})),
            ));
            scan_ok = false;
            break;
        }
        state.chunks.push(id_text.clone());
        match &id {
            b"ds64" => {
                if let Some(body) =
                    read_control_chunk(path, file, offset, size, &mut wrapper, "ds64")?
                {
                    parse_ds64(&body, chunk_index, &mut state, &mut wrapper, large);
                }
            }
            b"fmt " => {
                if state.fmt.is_some() {
                    bitstream.push(check(
                        "FORGE-WAVE-FMT-UNIQUE",
                        false,
                        "multiple fmt chunks are not allowed",
                        None,
                    ));
                } else if let Some(body) =
                    read_control_chunk(path, file, offset, size, &mut wrapper, "fmt ")?
                {
                    state.fmt = parse_wave_fmt(&body, &mut bitstream);
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
            if offset == file_size {
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
        scan_ok && offset == file_size,
        if scan_ok && offset == file_size {
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
                riff_size.checked_add(8) == Some(file_size),
                format!(
                    "ds64 RIFF size {} file size",
                    if riff_size.checked_add(8) == Some(file_size) {
                        "matches"
                    } else {
                        "does not match"
                    }
                ),
                Some(json!({"declared": riff_size, "actual": file_size - 8})),
            ));
        }
    } else {
        wrapper.push(check(
            "FORGE-WAVE-RIFF-SIZE",
            u64::from(declared_riff_size).checked_add(8) == Some(file_size),
            format!(
                "RIFF size {} file size",
                if u64::from(declared_riff_size).checked_add(8) == Some(file_size) {
                    "matches"
                } else {
                    "does not match"
                }
            ),
            Some(json!({"declared": declared_riff_size, "actual": file_size - 8})),
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
    Ok(finish_audit(
        path, "wave", wrapper, bitstream, xcheck, properties,
    ))
}

fn read_control_chunk(
    path: &Path,
    file: &mut File,
    offset: u64,
    size: u64,
    wrapper: &mut Vec<AuditCheck>,
    name: &str,
) -> Result<Option<Vec<u8>>, String> {
    if size > MAX_CONTROL_CHUNK_BYTES {
        wrapper.push(check(
            "FORGE-WAVE-CONTROL-CHUNK-LIMIT",
            false,
            format!("{name} chunk exceeds the bounded-read safety limit"),
            Some(json!({"size": size, "limit": MAX_CONTROL_CHUNK_BYTES})),
        ));
        return Ok(None);
    }
    let size = usize::try_from(size).expect("bounded control chunk fits usize");
    let mut body = vec![0_u8; size];
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} to {offset}: {error}", path.display()))?;
    file.read_exact(&mut body)
        .map_err(|error| format!("read {} {name} chunk: {error}", path.display()))?;
    Ok(Some(body))
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
        let table_fits = body.len() >= required;
        wrapper.push(check(
            "FORGE-WAVE-DS64-TABLE",
            table_fits,
            "ds64 table length fits the chunk",
            Some(json!({"entries": table_length, "size": body.len()})),
        ));
        if table_fits {
            for entry in body[28..required].chunks_exact(12) {
                let id = entry[..4].try_into().unwrap();
                let size = u64::from_le_bytes(entry[4..12].try_into().unwrap());
                state.ds64_table.entry(id).or_default().push_back(size);
            }
        }
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

pub(crate) fn check(
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

pub(crate) fn finish_audit(
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
    use std::io::{Seek, SeekFrom, Write};

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
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
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

    #[test]
    fn sparse_rf64_larger_than_four_gib_is_audited_without_reading_audio() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.rf64");
        let data_size = u64::from(u32::MAX) + 1;
        let data_offset = 12 + 8 + 28 + 8 + 16 + 8;
        let file_size = data_offset + data_size;
        let mut file = File::create(&path).unwrap();
        file.write_all(b"RF64").unwrap();
        file.write_all(&u32::MAX.to_le_bytes()).unwrap();
        file.write_all(b"WAVE").unwrap();
        file.write_all(b"ds64").unwrap();
        file.write_all(&28_u32.to_le_bytes()).unwrap();
        file.write_all(&(file_size - 8).to_le_bytes()).unwrap();
        file.write_all(&data_size.to_le_bytes()).unwrap();
        file.write_all(&(data_size / 2).to_le_bytes()).unwrap();
        file.write_all(&0_u32.to_le_bytes()).unwrap();
        file.write_all(b"fmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&48_000_u32.to_le_bytes()).unwrap();
        file.write_all(&96_000_u32.to_le_bytes()).unwrap();
        file.write_all(&2_u16.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert_eq!(file.stream_position().unwrap(), data_offset);
        file.seek(SeekFrom::Start(file_size - 1)).unwrap();
        file.write_all(&[0]).unwrap();
        drop(file);

        let result = audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["data_size_bytes"], json!(data_size));
    }
}
