//! Wrapper, bitstream, and metadata cross-checks for delivery containers.

use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const CONTAINER_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/container-qc-v1";
const MAX_WAVE_CHUNKS: usize = 100_000;
const MAX_CONTROL_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IXML_DEPTH: usize = 64;
const MAX_IXML_ELEMENTS: usize = 100_000;

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
            "{}: unsupported container (expected WAVE, AIFF/AIFC, CAF, AU, FLAC, MP3, AAC ADTS/LOAS, AC-3/E-AC-3, standalone IAMF, MPEG-TS/M2TS, MXF, Ogg Opus/Vorbis, Matroska/WebM, or ISO-BMFF MP4/M4A/fMP4)",
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
    let mut header = [0_u8; 16];
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
    } else if crate::ac3_qc::looks_like_ac3(&header[..header_size]) {
        crate::ac3_qc::audit(path, file, file_size).map(Some)
    } else if crate::iamf_qc::looks_like_iamf(&header[..header_size]) {
        crate::iamf_qc::audit(path, file, file_size).map(Some)
    } else if crate::aac_qc::looks_like_aac(&header[..header_size]) {
        crate::aac_qc::audit(path, file, file_size).map(Some)
    } else if crate::mpegts_qc::looks_like_mpegts(&header[..header_size]) {
        crate::mpegts_qc::audit(path, file, file_size).map(Some)
    } else if crate::mp3_qc::looks_like_mp3(&header[..header_size]) {
        crate::mp3_qc::audit(path, file, file_size).map(Some)
    } else if crate::matroska_qc::looks_like_matroska(&header[..header_size]) {
        crate::matroska_qc::audit(path, file, file_size).map(Some)
    } else if crate::isobmff_qc::looks_like_isobmff(&header[..header_size], file_size) {
        crate::isobmff_qc::audit(path, file, file_size).map(Some)
    } else if crate::mxf_qc::probe(&mut file, file_size)? {
        crate::mxf_qc::audit(path, file, file_size).map(Some)
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
    bext_count: usize,
    bext: Option<BextInfo>,
    xml: crate::bwf_xml_qc::BwfXmlState,
    chna_count: usize,
    chna: Option<ChnaInfo>,
    ixml_count: usize,
    ixml: Option<IxmlInfo>,
    ds64_table: HashMap<[u8; 4], VecDeque<u64>>,
}

#[derive(Debug)]
struct BextInfo {
    description: String,
    originator: String,
    originator_reference: String,
    origination_date: String,
    origination_time: String,
    time_reference_samples: u64,
    version: u16,
    umid: Option<String>,
    loudness: Option<Value>,
    coding_history_rows: usize,
    coding_history_bytes: usize,
}

#[derive(Debug, Serialize)]
struct IxmlInfo {
    version: Option<String>,
    declared_track_count: Option<usize>,
    tracks: Vec<IxmlTrack>,
    #[serde(skip)]
    track_list_count: usize,
    #[serde(skip)]
    track_count_field_count: usize,
    #[serde(skip)]
    invalid_channel_indices: Vec<String>,
    #[serde(skip)]
    invalid_interleave_indices: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
struct IxmlTrack {
    channel_index: Option<u32>,
    interleave_index: Option<u32>,
    name: Option<String>,
    function: Option<String>,
}

#[derive(Debug)]
struct ChnaInfo {
    declared_tracks: u16,
    track_indices: Vec<u16>,
}

#[derive(Default)]
struct ParsedIxml {
    top_level_count: usize,
    root_count: usize,
    track_list_count: usize,
    track_count_values: Vec<String>,
    version_values: Vec<String>,
    tracks: Vec<IxmlTrack>,
    active_track: Option<IxmlTrack>,
    invalid_channel_indices: Vec<String>,
    invalid_interleave_indices: Vec<String>,
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
            b"bext" => {
                state.bext_count += 1;
                if state.bext.is_none() {
                    if let Some(body) =
                        read_control_chunk(path, file, offset, size, &mut wrapper, "bext")?
                    {
                        state.bext = parse_bext(&body, &mut xcheck);
                    }
                }
            }
            b"axml" => {
                state.xml.axml_count += 1;
                if state.xml.axml.is_none() {
                    if let Some(body) =
                        read_control_chunk(path, file, offset, size, &mut wrapper, "axml")?
                    {
                        state.xml.axml = crate::bwf_xml_qc::parse_axml(&body, &mut xcheck);
                    }
                }
            }
            b"bxml" => {
                state.xml.bxml_count += 1;
                if state.xml.bxml.is_none() {
                    if let Some(body) =
                        read_control_chunk(path, file, offset, size, &mut wrapper, "bxml")?
                    {
                        state.xml.bxml = crate::bwf_xml_qc::parse_bxml(&body, &mut xcheck);
                    }
                }
            }
            b"sxml" => {
                state.xml.sxml_count += 1;
                if state.xml.sxml.is_none() {
                    if let Some(body) =
                        read_control_chunk(path, file, offset, size, &mut wrapper, "sxml")?
                    {
                        state.xml.sxml = crate::bwf_xml_qc::parse_sxml(&body, &mut xcheck);
                    }
                }
            }
            b"chna" => {
                state.chna_count += 1;
                if state.chna.is_none() {
                    if let Some(body) =
                        read_control_chunk(path, file, offset, size, &mut wrapper, "chna")?
                    {
                        state.chna = parse_chna(&body);
                    }
                }
            }
            b"iXML" => {
                state.ixml_count += 1;
                if state.ixml.is_none() {
                    if let Some(body) =
                        read_control_chunk(path, file, offset, size, &mut wrapper, "iXML")?
                    {
                        state.ixml = parse_ixml(&body, &mut xcheck);
                    }
                }
            }
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
    if state.bext_count > 0 {
        xcheck.push(check(
            "FORGE-BWF-BEXT-UNIQUE",
            state.bext_count == 1,
            if state.bext_count == 1 {
                "exactly one bext chunk is present"
            } else {
                "BWF permits exactly one bext chunk"
            },
            Some(json!(state.bext_count)),
        ));
    }
    if let (Some(bext), Some(fmt)) = (&state.bext, state.fmt) {
        let samples_per_day = u64::from(fmt.sample_rate) * 86_400;
        let valid = bext.time_reference_samples < samples_per_day;
        xcheck.push(check(
            "FORGE-BWF-TIME-REFERENCE",
            valid,
            if valid {
                "TimeReference is a sample position within one day"
            } else {
                "TimeReference exceeds one day at the declared sample rate"
            },
            Some(json!({
                "samples": bext.time_reference_samples,
                "sample_rate_hz": fmt.sample_rate,
                "seconds": bext.time_reference_samples as f64 / f64::from(fmt.sample_rate)
            })),
        ));
    }
    if state.xml.axml_count > 0 || state.xml.bxml_count > 0 || state.xml.sxml_count > 0 {
        crate::bwf_xml_qc::validate(&state.xml, state.chna_count > 0, frames, &mut xcheck);
    }
    if state.ixml_count > 0 {
        xcheck.push(check(
            "FORGE-IXML-UNIQUE",
            state.ixml_count == 1,
            if state.ixml_count == 1 {
                "exactly one iXML chunk is present"
            } else {
                "WAVE interoperability requires exactly one iXML chunk"
            },
            Some(json!(state.ixml_count)),
        ));
    }
    if let (Some(ixml), Some(fmt)) = (&state.ixml, state.fmt) {
        validate_ixml_tracks(ixml, fmt.channels, &mut xcheck);
        if state.chna_count > 0 {
            cross_check_ixml_chna(ixml, state.chna.as_ref(), fmt.channels, &mut xcheck);
        }
    }
    let bext_properties = state.bext.as_ref().map(|bext| {
        json!({
            "description": bext.description,
            "originator": bext.originator,
            "originator_reference": bext.originator_reference,
            "origination_date": bext.origination_date,
            "origination_time": bext.origination_time,
            "time_reference_samples": bext.time_reference_samples,
            "version": bext.version,
            "umid": bext.umid,
            "loudness": bext.loudness,
            "coding_history_rows": bext.coding_history_rows,
            "coding_history_bytes": bext.coding_history_bytes
        })
    });
    let properties = json!({
        "container": state.container,
        "chunks": state.chunks,
        "data_size_bytes": state.data_size,
        "frames": frames,
        "sample_rate_hz": state.fmt.map(|fmt| fmt.sample_rate),
        "channels": state.fmt.map(|fmt| fmt.channels),
        "bits_per_sample": state.fmt.map(|fmt| fmt.bits_per_sample),
        "bext": bext_properties,
        "ixml": state.ixml,
        "xml_metadata": state.xml
    });
    Ok(finish_audit(
        path, "wave", wrapper, bitstream, xcheck, properties,
    ))
}

fn parse_bext(body: &[u8], checks: &mut Vec<AuditCheck>) -> Option<BextInfo> {
    let complete = body.len() >= 602;
    checks.push(check(
        "FORGE-BWF-BEXT-SIZE",
        complete,
        if complete {
            "bext fixed fields are complete"
        } else {
            "bext chunk is shorter than the 602-byte fixed fields"
        },
        Some(json!(body.len())),
    ));
    if !complete {
        return None;
    }

    let text_fields = [
        ("Description", &body[0..256]),
        ("Originator", &body[256..288]),
        ("OriginatorReference", &body[288..320]),
        ("OriginationDate", &body[320..330]),
        ("OriginationTime", &body[330..338]),
    ];
    let malformed_text: Vec<&str> = text_fields
        .iter()
        .filter_map(|(name, field)| (!valid_fixed_ascii(field)).then_some(*name))
        .collect();
    checks.push(check(
        "FORGE-BWF-BEXT-TEXT",
        malformed_text.is_empty(),
        if malformed_text.is_empty() {
            "fixed BWF text fields are ASCII and null-terminated when shorter than their field"
        } else {
            "one or more fixed BWF text fields are not valid ASCII strings"
        },
        Some(json!({"invalid_fields": malformed_text})),
    ));

    let origination_date = fixed_text(&body[320..330]);
    let origination_time = fixed_text(&body[330..338]);
    let date_valid = origination_date.is_empty() || valid_bwf_date(&origination_date);
    let time_valid = origination_time.is_empty() || valid_bwf_time(&origination_time);
    checks.push(check(
        "FORGE-BWF-BEXT-DATETIME",
        date_valid && time_valid,
        if date_valid && time_valid {
            "populated origination date/time fields have valid EBU Tech 3285 values"
        } else {
            "origination date/time is outside the EBU Tech 3285 field format or range"
        },
        Some(json!({
            "origination_date": origination_date,
            "origination_time": origination_time,
            "date_valid": date_valid,
            "time_valid": time_valid
        })),
    ));

    let time_reference_samples = u64::from_le_bytes(body[338..346].try_into().unwrap());
    let version = u16::from_le_bytes(body[346..348].try_into().unwrap());
    let version_valid = version <= 2;
    checks.push(check(
        "FORGE-BWF-BEXT-VERSION",
        version_valid,
        if version_valid {
            format!("bext version {version} is defined by EBU Tech 3285 v2")
        } else {
            format!("bext version {version} is not defined by EBU Tech 3285 v2")
        },
        Some(json!(version)),
    ));

    let umid_bytes = &body[348..412];
    let umid_present = umid_bytes.iter().any(|byte| *byte != 0);
    let umid_valid = version > 0 || !umid_present;
    checks.push(check(
        "FORGE-BWF-BEXT-UMID",
        umid_valid,
        if umid_valid {
            "UMID presence is consistent with the bext version"
        } else {
            "version 0 reserves the UMID field and requires zero bytes"
        },
        Some(json!({"present": umid_present, "version": version})),
    ));

    let reserved = match version {
        0 => &body[348..602],
        1 => &body[412..602],
        _ => &body[422..602],
    };
    let reserved_valid = version_valid && reserved.iter().all(|byte| *byte == 0);
    checks.push(check(
        "FORGE-BWF-BEXT-RESERVED",
        reserved_valid,
        if reserved_valid {
            "version-specific reserved bytes are zero"
        } else {
            "version-specific reserved bytes must be zero"
        },
        Some(json!({
            "version": version,
            "nonzero_reserved_bytes": reserved.iter().filter(|byte| **byte != 0).count()
        })),
    ));

    let loudness = (version == 2).then(|| {
        let values: [i16; 5] = std::array::from_fn(|index| {
            let offset = 412 + index * 2;
            i16::from_le_bytes(body[offset..offset + 2].try_into().unwrap())
        });
        let valid = values.iter().enumerate().all(|(index, value)| {
            *value == i16::MAX
                || if index == 1 {
                    (0..=9_999).contains(value)
                } else {
                    (-9_999..=9_999).contains(value)
                }
        });
        checks.push(check(
            "FORGE-BWF-BEXT-LOUDNESS",
            valid,
            if valid {
                "version 2 loudness fields use valid hundredth-unit values or 0x7fff"
            } else {
                "one or more version 2 loudness fields are outside the valid range"
            },
            Some(json!({
                "raw": values,
                "unavailable_sentinel": i16::MAX
            })),
        ));
        json!({
            "integrated_lufs": bwf_loudness_value(values[0]),
            "range_lu": bwf_loudness_value(values[1]),
            "max_true_peak_dbtp": bwf_loudness_value(values[2]),
            "max_momentary_lufs": bwf_loudness_value(values[3]),
            "max_short_term_lufs": bwf_loudness_value(values[4])
        })
    });

    let coding_history = &body[602..];
    let coding_history_valid = valid_coding_history(coding_history);
    let coding_history_rows = coding_history
        .windows(2)
        .filter(|window| *window == b"\r\n")
        .count();
    checks.push(check(
        "FORGE-BWF-BEXT-CODING-HISTORY",
        coding_history_valid,
        if coding_history_valid {
            "CodingHistory is ASCII and every populated row is CR/LF-terminated"
        } else {
            "CodingHistory must be ASCII with CR/LF-terminated rows"
        },
        Some(json!({
            "bytes": coding_history.len(),
            "rows": coding_history_rows
        })),
    ));

    Some(BextInfo {
        description: fixed_text(&body[0..256]),
        originator: fixed_text(&body[256..288]),
        originator_reference: fixed_text(&body[288..320]),
        origination_date,
        origination_time,
        time_reference_samples,
        version,
        umid: umid_present.then(|| hex_bytes(umid_bytes)),
        loudness,
        coding_history_rows,
        coding_history_bytes: coding_history.len(),
    })
}

fn valid_fixed_ascii(field: &[u8]) -> bool {
    let content = field
        .iter()
        .position(|byte| *byte == 0)
        .map_or(field, |end| &field[..end]);
    content.is_ascii()
}

fn fixed_text(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn valid_bwf_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
        || bytes[4].is_ascii_digit()
        || bytes[7].is_ascii_digit()
    {
        return false;
    }
    let year = value[..4].parse::<u32>().unwrap();
    let month = value[5..7].parse::<u32>().unwrap();
    let day = value[8..].parse::<u32>().unwrap();
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

fn valid_bwf_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 8
        || !bytes[..2].iter().all(u8::is_ascii_digit)
        || !bytes[3..5].iter().all(u8::is_ascii_digit)
        || !bytes[6..].iter().all(u8::is_ascii_digit)
        || bytes[2].is_ascii_digit()
        || bytes[5].is_ascii_digit()
    {
        return false;
    }
    let hour = value[..2].parse::<u8>().unwrap();
    let minute = value[3..5].parse::<u8>().unwrap();
    let second = value[6..].parse::<u8>().unwrap();
    hour < 24 && minute < 60 && second < 60
}

fn valid_coding_history(value: &[u8]) -> bool {
    if value.is_empty() {
        return true;
    }
    if !value.is_ascii() || value.contains(&0) || !value.ends_with(b"\r\n") {
        return false;
    }
    value.iter().enumerate().all(|(index, byte)| match byte {
        b'\r' => value.get(index + 1) == Some(&b'\n'),
        b'\n' => index > 0 && value[index - 1] == b'\r',
        _ => true,
    })
}

fn bwf_loudness_value(value: i16) -> Option<f64> {
    (value != i16::MAX).then_some(f64::from(value) / 100.0)
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn parse_ixml(body: &[u8], checks: &mut Vec<AuditCheck>) -> Option<IxmlInfo> {
    let parsed = match read_ixml(body) {
        Ok(parsed) => parsed,
        Err(error) => {
            checks.push(check(
                "FORGE-IXML-XML",
                false,
                format!("iXML is not safe, well-formed XML: {error}"),
                Some(json!({"bytes": body.len()})),
            ));
            return None;
        }
    };
    checks.push(check(
        "FORGE-IXML-XML",
        true,
        "iXML is well-formed XML within the parser safety limits",
        Some(json!({"bytes": body.len()})),
    ));

    let root_valid = parsed.top_level_count == 1 && parsed.root_count == 1;
    checks.push(check(
        "FORGE-IXML-ROOT",
        root_valid,
        if root_valid {
            "iXML has one BWFXML document root"
        } else {
            "iXML must have exactly one BWFXML document root"
        },
        Some(json!({
            "top_level_elements": parsed.top_level_count,
            "bwfxml_roots": parsed.root_count
        })),
    ));

    let track_list_valid = parsed.track_list_count <= 1;
    checks.push(check(
        "FORGE-IXML-TRACK-LIST",
        track_list_valid,
        if track_list_valid {
            "iXML contains at most one TRACK_LIST"
        } else {
            "iXML must not contain multiple TRACK_LIST objects"
        },
        Some(json!(parsed.track_list_count)),
    ));

    let declared_track_count = (parsed.track_count_values.len() == 1)
        .then(|| parsed.track_count_values[0].parse::<usize>().ok())
        .flatten();
    Some(IxmlInfo {
        version: (parsed.version_values.len() == 1).then(|| parsed.version_values[0].clone()),
        declared_track_count,
        tracks: parsed.tracks,
        track_list_count: parsed.track_list_count,
        track_count_field_count: parsed.track_count_values.len(),
        invalid_channel_indices: parsed.invalid_channel_indices,
        invalid_interleave_indices: parsed.invalid_interleave_indices,
    })
}

fn read_ixml(body: &[u8]) -> Result<ParsedIxml, String> {
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedIxml::default();
    let mut stack = Vec::<String>::new();
    let mut text_stack = Vec::<String>::new();
    let mut elements = 0_usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                elements += 1;
                if elements > MAX_IXML_ELEMENTS {
                    return Err(format!("element count exceeds {MAX_IXML_ELEMENTS}"));
                }
                if stack.len() == MAX_IXML_DEPTH {
                    return Err(format!("nesting depth exceeds {MAX_IXML_DEPTH}"));
                }
                let name = xml_local_name(element.name().as_ref());
                if stack.is_empty() {
                    parsed.top_level_count += 1;
                    if name == "BWFXML" {
                        parsed.root_count += 1;
                    }
                }
                if name == "TRACK_LIST" {
                    parsed.track_list_count += 1;
                } else if name == "TRACK"
                    && stack.last().is_some_and(|parent| parent == "TRACK_LIST")
                {
                    if parsed.active_track.is_some() {
                        return Err("nested TRACK objects are not supported".into());
                    }
                    parsed.active_track = Some(IxmlTrack::default());
                }
                stack.push(name);
                text_stack.push(String::new());
            }
            Ok(Event::Empty(element)) => {
                elements += 1;
                if elements > MAX_IXML_ELEMENTS {
                    return Err(format!("element count exceeds {MAX_IXML_ELEMENTS}"));
                }
                if stack.len() == MAX_IXML_DEPTH {
                    return Err(format!("nesting depth exceeds {MAX_IXML_DEPTH}"));
                }
                let name = xml_local_name(element.name().as_ref());
                if stack.is_empty() {
                    parsed.top_level_count += 1;
                    if name == "BWFXML" {
                        parsed.root_count += 1;
                    }
                }
                if name == "TRACK_LIST" {
                    parsed.track_list_count += 1;
                } else if name == "TRACK"
                    && stack.last().is_some_and(|parent| parent == "TRACK_LIST")
                {
                    parsed.active_track = Some(IxmlTrack::default());
                }
                close_ixml_element(&name, "", stack.last().map(String::as_str), &mut parsed)?;
            }
            Ok(Event::Text(text)) => {
                if let Some(value) = text_stack.last_mut() {
                    let decoded = text
                        .xml10_content()
                        .map_err(|error| format!("decode XML text: {error}"))?;
                    value.push_str(
                        &unescape(&decoded)
                            .map_err(|error| format!("decode XML entity: {error}"))?,
                    );
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(value) = text_stack.last_mut() {
                    value.push_str(
                        &text
                            .xml10_content()
                            .map_err(|error| format!("decode XML CDATA: {error}"))?,
                    );
                }
            }
            Ok(Event::End(element)) => {
                let expected = stack
                    .pop()
                    .ok_or_else(|| "closing element without an open element".to_string())?;
                let actual = xml_local_name(element.name().as_ref());
                if actual != expected {
                    return Err(format!(
                        "closing element {actual} does not match {expected}"
                    ));
                }
                let value = text_stack.pop().unwrap_or_default();
                close_ixml_element(
                    &actual,
                    value.trim(),
                    stack.last().map(String::as_str),
                    &mut parsed,
                )?;
            }
            Ok(Event::DocType(_)) => return Err("DOCTYPE declarations are not allowed".into()),
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(format!(
                    "XML error at byte {}: {error}",
                    reader.error_position()
                ))
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    Ok(parsed)
}

fn close_ixml_element(
    name: &str,
    value: &str,
    parent: Option<&str>,
    parsed: &mut ParsedIxml,
) -> Result<(), String> {
    match (parent, name) {
        (Some("BWFXML"), "IXML_VERSION") => parsed.version_values.push(value.to_owned()),
        (Some("TRACK_LIST"), "TRACK_COUNT") => parsed.track_count_values.push(value.to_owned()),
        (Some("TRACK"), "CHANNEL_INDEX") => set_ixml_index(
            &mut parsed.active_track,
            value,
            "CHANNEL_INDEX",
            &mut parsed.invalid_channel_indices,
            |track| &mut track.channel_index,
        ),
        (Some("TRACK"), "INTERLEAVE_INDEX") => set_ixml_index(
            &mut parsed.active_track,
            value,
            "INTERLEAVE_INDEX",
            &mut parsed.invalid_interleave_indices,
            |track| &mut track.interleave_index,
        ),
        (Some("TRACK"), "NAME") => {
            if let Some(track) = &mut parsed.active_track {
                track.name = nonempty(value);
            }
        }
        (Some("TRACK"), "FUNCTION") => {
            if let Some(track) = &mut parsed.active_track {
                track.function = nonempty(value);
            }
        }
        (Some("TRACK_LIST"), "TRACK") => {
            let track = parsed
                .active_track
                .take()
                .ok_or_else(|| "TRACK closed without a matching start".to_string())?;
            parsed.tracks.push(track);
        }
        _ => {}
    }
    Ok(())
}

fn set_ixml_index(
    track: &mut Option<IxmlTrack>,
    value: &str,
    field: &str,
    invalid: &mut Vec<String>,
    select: impl FnOnce(&mut IxmlTrack) -> &mut Option<u32>,
) {
    let Some(track) = track else {
        return;
    };
    let target = select(track);
    let parsed = value.parse::<u32>().ok().filter(|value| *value > 0);
    if target.is_some() || parsed.is_none() {
        invalid.push(format!("TRACK {} {field}", invalid.len() + 1));
    } else {
        *target = parsed;
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn xml_local_name(name: &[u8]) -> String {
    String::from_utf8_lossy(name.rsplit(|byte| *byte == b':').next().unwrap_or(name)).into_owned()
}

fn validate_ixml_tracks(ixml: &IxmlInfo, channels: u16, checks: &mut Vec<AuditCheck>) {
    let track_count_valid = if ixml.track_list_count == 0 {
        ixml.track_count_field_count == 0 && ixml.tracks.is_empty()
    } else {
        (ixml.track_count_field_count == 0
            || (ixml.track_count_field_count == 1
                && ixml.declared_track_count == Some(ixml.tracks.len())))
            && ixml.tracks.len() == usize::from(channels)
    };
    checks.push(check(
        "FORGE-IXML-TRACK-COUNT",
        track_count_valid,
        if ixml.track_list_count == 0 {
            "optional TRACK_LIST is absent"
        } else if track_count_valid {
            "TRACK objects match PCM channels and optional TRACK_COUNT is consistent"
        } else {
            "TRACK objects must match PCM channels and optional TRACK_COUNT must be consistent"
        },
        Some(json!({
            "fields": ixml.track_count_field_count,
            "declared": ixml.declared_track_count,
            "track_objects": ixml.tracks.len(),
            "pcm_channels": channels
        })),
    ));

    let channel_indices = ixml
        .tracks
        .iter()
        .filter_map(|track| track.channel_index)
        .collect::<Vec<_>>();
    let channel_valid = ixml.invalid_channel_indices.is_empty();
    checks.push(check(
        "FORGE-IXML-CHANNEL-INDEX",
        channel_valid,
        if channel_valid {
            "populated CHANNEL_INDEX values use one-based positive source numbers"
        } else {
            "CHANNEL_INDEX values must be one-based positive integers"
        },
        Some(json!({
            "channel_indices": channel_indices,
            "invalid_fields": ixml.invalid_channel_indices
        })),
    ));

    let interleave_indices = ixml
        .tracks
        .iter()
        .filter_map(|track| track.interleave_index)
        .collect::<Vec<_>>();
    let provided = interleave_indices.len();
    let unique = interleave_indices.iter().collect::<HashSet<_>>().len() == provided;
    let in_range = interleave_indices
        .iter()
        .all(|index| *index <= u32::from(channels));
    let complete = provided == ixml.tracks.len();
    let coverage = (1..=u32::from(channels)).all(|index| interleave_indices.contains(&index));
    let interleave_valid = ixml.invalid_interleave_indices.is_empty()
        && (provided == 0
            || (complete
                && unique
                && in_range
                && coverage
                && ixml.tracks.len() == usize::from(channels)));
    checks.push(check(
        "FORGE-IXML-INTERLEAVE-INDEX",
        interleave_valid,
        if provided == 0 {
            "optional INTERLEAVE_INDEX values are absent"
        } else if interleave_valid {
            "INTERLEAVE_INDEX values map every PCM channel exactly once"
        } else {
            "populated INTERLEAVE_INDEX values must uniquely cover every PCM channel"
        },
        Some(json!({
            "interleave_indices": interleave_indices,
            "invalid_fields": ixml.invalid_interleave_indices,
            "pcm_channels": channels
        })),
    ));
}

fn parse_chna(body: &[u8]) -> Option<ChnaInfo> {
    if body.len() < 4 || !(body.len() - 4).is_multiple_of(40) {
        return None;
    }
    let declared_tracks = u16::from_le_bytes(body[..2].try_into().unwrap());
    let num_uids = usize::from(u16::from_le_bytes(body[2..4].try_into().unwrap()));
    let required = 4_usize.checked_add(num_uids.checked_mul(40)?)?;
    if body.len() < required {
        return None;
    }
    if body[required..].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(ChnaInfo {
        declared_tracks,
        track_indices: body[4..required]
            .chunks_exact(40)
            .map(|entry| u16::from_le_bytes(entry[..2].try_into().unwrap()))
            .collect(),
    })
}

fn cross_check_ixml_chna(
    ixml: &IxmlInfo,
    chna: Option<&ChnaInfo>,
    channels: u16,
    checks: &mut Vec<AuditCheck>,
) {
    let ixml_indices = ixml
        .tracks
        .iter()
        .filter_map(|track| track.interleave_index)
        .map(|index| u16::try_from(index).ok())
        .collect::<Option<Vec<_>>>();
    let ixml_complete = ixml_indices
        .as_ref()
        .is_some_and(|indices| indices.len() == usize::from(channels));
    let chna_set = chna.map(|info| info.track_indices.iter().copied().collect::<HashSet<_>>());
    let chna_valid = chna.is_some_and(|info| {
        info.declared_tracks == channels
            && chna_set.as_ref().is_some_and(|indices| {
                indices.len() == usize::from(channels)
                    && (1..=channels).all(|index| indices.contains(&index))
            })
    });
    let passed = if !chna_valid {
        false
    } else if ixml_complete {
        ixml_indices.as_ref().is_some_and(|indices| {
            indices.iter().copied().collect::<HashSet<_>>() == *chna_set.as_ref().unwrap()
        })
    } else {
        true
    };
    checks.push(check(
        "FORGE-IXML-CHNA-XCHECK",
        passed,
        if !chna_valid {
            "chna is malformed or does not map every declared PCM track"
        } else if !ixml_complete {
            "iXML has no complete INTERLEAVE_INDEX map; optional ADM reconciliation was skipped"
        } else if passed {
            "iXML INTERLEAVE_INDEX values match the ADM chna track indexes"
        } else {
            "iXML INTERLEAVE_INDEX values do not match ADM chna track indexes"
        },
        Some(json!({
            "ixml_interleave_indices": ixml_indices,
            "chna_declared_tracks": chna.map(|info| info.declared_tracks),
            "chna_track_indices": chna.map(|info| &info.track_indices)
        })),
    ));
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
    use crate::wav::{AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk};
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

    fn bwf_audio() -> AudioBuffer {
        AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 10,
            data: vec![vec![0.0; 10]],
            channel_roles: crate::wav::default_channel_roles(1),
            source_kind: PcmKind::S16,
        }
    }

    fn valid_bext() -> Vec<u8> {
        let mut body = vec![0_u8; 602];
        body[..12].copy_from_slice(b"Evening news");
        body[256..265].copy_from_slice(b"EBU Forge");
        body[288..300].copy_from_slice(b"EU-FORGE-001");
        body[320..330].copy_from_slice(b"2026-07-29");
        body[330..338].copy_from_slice(b"16:45:30");
        body[338..346].copy_from_slice(&(48_000_u64 * 3_600).to_le_bytes());
        body[346..348].copy_from_slice(&2_u16.to_le_bytes());
        body[348..380].copy_from_slice(&[
            0x06, 0x0a, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x05, 0x01, 0x01, 0x0f, 0x20, 0x13, 0, 0, 0,
            0x46, 0x4f, 0x52, 0x47, 0x45, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1,
        ]);
        for (index, value) in [-2_300_i16, 700, -100, -1_800, -1_900].iter().enumerate() {
            let offset = 412 + index * 2;
            body[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(b"A=PCM,F=48000,W=24,M=mono\r\n");
        body
    }

    fn write_bext_fixture(path: &Path, chunks: Vec<WaveChunk>) {
        WavWriter::write_with_metadata(
            path,
            &bwf_audio(),
            PcmKind::S16,
            false,
            WavContainer::Riff,
            &chunks,
        )
        .unwrap();
    }

    #[test]
    fn bwf_bext_v2_fields_are_validated_and_reported() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("valid-bwf.wav");
        write_bext_fixture(
            &path,
            vec![WaveChunk {
                id: *b"bext",
                body: valid_bext(),
            }],
        );

        let result = audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["bext"]["version"], 2);
        assert_eq!(
            result.properties["bext"]["time_reference_samples"],
            48_000 * 3_600
        );
        assert_eq!(
            result.properties["bext"]["loudness"]["integrated_lufs"],
            -23.0
        );
        assert_eq!(result.properties["bext"]["coding_history_rows"], 1);
        assert_eq!(
            result.properties["bext"]["umid"].as_str().unwrap().len(),
            128
        );
    }

    #[test]
    fn bwf_bext_rejects_bad_metadata_ranges_and_line_endings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-bwf.wav");
        let mut bext = valid_bext();
        bext[320..330].copy_from_slice(b"2025-02-29");
        bext[338..346].copy_from_slice(&(48_000_u64 * 86_400).to_le_bytes());
        bext[414..416].copy_from_slice(&(-1_i16).to_le_bytes());
        bext[500] = 1;
        bext.truncate(602);
        bext.extend_from_slice(b"A=PCM,F=48000,W=24,M=mono\n");
        write_bext_fixture(
            &path,
            vec![WaveChunk {
                id: *b"bext",
                body: bext,
            }],
        );

        let result = audit(&path).unwrap();
        assert!(!result.passed);
        let failed: Vec<_> = result.layers[2]
            .checks
            .iter()
            .filter(|check| !check.passed)
            .map(|check| check.rule_id)
            .collect();
        for expected in [
            "FORGE-BWF-BEXT-DATETIME",
            "FORGE-BWF-BEXT-RESERVED",
            "FORGE-BWF-BEXT-LOUDNESS",
            "FORGE-BWF-BEXT-CODING-HISTORY",
            "FORGE-BWF-TIME-REFERENCE",
        ] {
            assert!(
                failed.contains(&expected),
                "missing {expected}: {result:#?}"
            );
        }
    }

    #[test]
    fn bwf_bext_rejects_duplicate_chunks_and_version_zero_umid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("duplicate-bwf.wav");
        let mut bext = valid_bext();
        bext[346..348].copy_from_slice(&0_u16.to_le_bytes());
        bext[412..602].fill(0);
        write_bext_fixture(
            &path,
            vec![
                WaveChunk {
                    id: *b"bext",
                    body: bext.clone(),
                },
                WaveChunk {
                    id: *b"bext",
                    body: bext,
                },
            ],
        );

        let result = audit(&path).unwrap();
        assert!(!result.passed);
        assert!(result.layers[2]
            .checks
            .iter()
            .any(|check| { check.rule_id == "FORGE-BWF-BEXT-UNIQUE" && !check.passed }));
        assert!(result.layers[2]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-BWF-BEXT-UMID" && !check.passed));
    }

    fn stereo_audio() -> AudioBuffer {
        AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 10,
            data: vec![vec![0.0; 10], vec![0.0; 10]],
            channel_roles: crate::wav::default_channel_roles(2),
            source_kind: PcmKind::S16,
        }
    }

    fn chna(indices: &[u16], declared_tracks: u16, unused_slots: usize) -> Vec<u8> {
        let mut body = Vec::with_capacity(4 + (indices.len() + unused_slots) * 40);
        body.extend_from_slice(&declared_tracks.to_le_bytes());
        body.extend_from_slice(&(indices.len() as u16).to_le_bytes());
        for (uid, index) in indices.iter().enumerate() {
            let mut record = [0_u8; 40];
            record[..2].copy_from_slice(&index.to_le_bytes());
            record[2..14].copy_from_slice(format!("ATU_{:08X}", uid + 1).as_bytes());
            body.extend_from_slice(&record);
        }
        body.resize(body.len() + unused_slots * 40, 0);
        body
    }

    fn write_ixml_fixture(path: &Path, ixml: &[u8], chna_body: Option<Vec<u8>>) {
        let mut chunks = vec![WaveChunk {
            id: *b"iXML",
            body: ixml.to_vec(),
        }];
        if let Some(chna_body) = chna_body {
            chunks.push(WaveChunk {
                id: *b"axml",
                body: b"<metadata/>".to_vec(),
            });
            chunks.push(WaveChunk {
                id: *b"chna",
                body: chna_body,
            });
        }
        WavWriter::write_with_metadata(
            path,
            &stereo_audio(),
            PcmKind::S16,
            false,
            WavContainer::Riff,
            &chunks,
        )
        .unwrap();
    }

    fn serial_xml_chunk(xml: &[u8], samples: u32) -> Vec<u8> {
        let table_bytes = 4_u64 + 8 + xml.len() as u64;
        let mut body = Vec::new();
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&(table_bytes as u32).to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        body.extend_from_slice(&samples.to_le_bytes());
        body.extend_from_slice(xml);
        body.extend_from_slice(&0_u32.to_le_bytes());
        body
    }

    #[test]
    fn bs2088_xml_chunks_are_parsed_and_cross_checked() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bs2088-xml.wav");
        let axml = br#"<eb:ebuCoreMain xmlns:eb="urn:ebu:metadata-schema:ebuCore_2015">
<eb:coreMetadata><eb:format><audioFormatExtended>
<audioTrackUID UID="ATU_00000001"/>
</audioFormatExtended></eb:format></eb:coreMetadata>
</eb:ebuCoreMain>"#;
        let mut bxml = 0_u16.to_le_bytes().to_vec();
        bxml.extend_from_slice(b"<metadata/>");
        let sxml = serial_xml_chunk(
            b"<frame><audioFormatExtended><audioTrackUID UID=\"ATU_00000002\"/></audioFormatExtended></frame>",
            10,
        );
        write_bext_fixture(
            &path,
            vec![
                WaveChunk {
                    id: *b"axml",
                    body: axml.to_vec(),
                },
                WaveChunk {
                    id: *b"bxml",
                    body: bxml,
                },
                WaveChunk {
                    id: *b"sxml",
                    body: sxml,
                },
                WaveChunk {
                    id: *b"chna",
                    body: chna(&[1], 1, 0),
                },
            ],
        );

        let result = audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(
            result.properties["xml_metadata"]["axml"]["classification"],
            "adm"
        );
        assert_eq!(
            result.properties["xml_metadata"]["bxml"]["compression"],
            "none"
        );
        assert_eq!(
            result.properties["xml_metadata"]["sxml"]["subchunks"][0]["document"]["classification"],
            "s-adm"
        );
        for rule in [
            "FORGE-BS2088-2-AXML-XML",
            "FORGE-BS2088-2-BXML-XML",
            "FORGE-BS2088-2-SXML-STRUCTURE",
            "FORGE-BS2088-2-ADM-PLACEMENT",
            "FORGE-BS2088-2-ADM-CHNA",
            "FORGE-BS2088-2-SADM-PLACEMENT",
            "FORGE-BS2088-2-SXML-SAMPLE-COUNT",
        ] {
            assert!(
                result.layers[2]
                    .checks
                    .iter()
                    .any(|check| check.rule_id == rule && check.passed),
                "missing {rule}: {result:#?}"
            );
        }
    }

    #[test]
    fn bs2088_rejects_duplicate_xml_chunks_and_bad_sxml_duration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bad-bs2088-xml.wav");
        write_bext_fixture(
            &path,
            vec![
                WaveChunk {
                    id: *b"axml",
                    body: b"<metadata/>".to_vec(),
                },
                WaveChunk {
                    id: *b"axml",
                    body: b"<metadata/>".to_vec(),
                },
                WaveChunk {
                    id: *b"sxml",
                    body: serial_xml_chunk(b"<frame/>", 11),
                },
            ],
        );

        let result = audit(&path).unwrap();
        assert!(!result.passed);
        assert!(result.layers[2]
            .checks
            .iter()
            .any(|check| { check.rule_id == "FORGE-BS2088-2-XML-CHUNK-UNIQUE" && !check.passed }));
        assert!(result.layers[2]
            .checks
            .iter()
            .any(|check| { check.rule_id == "FORGE-BS2088-2-SXML-SAMPLE-COUNT" && !check.passed }));
    }

    #[test]
    fn ixml_track_map_is_reported_and_matches_adm_chna() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ixml-adm.wav");
        let ixml = br#"<?xml version="1.0" encoding="UTF-8"?>
<BWFXML>
  <IXML_VERSION>1.52</IXML_VERSION>
  <TRACK_LIST>
    <TRACK_COUNT>2</TRACK_COUNT>
    <TRACK>
      <CHANNEL_INDEX>6</CHANNEL_INDEX>
      <INTERLEAVE_INDEX>2</INTERLEAVE_INDEX>
      <NAME>Side</NAME>
      <FUNCTION>S-MID_SIDE</FUNCTION>
    </TRACK>
    <TRACK>
      <CHANNEL_INDEX>4</CHANNEL_INDEX>
      <INTERLEAVE_INDEX>1</INTERLEAVE_INDEX>
      <NAME>Mid</NAME>
      <FUNCTION>M-MID_SIDE</FUNCTION>
    </TRACK>
  </TRACK_LIST>
</BWFXML>"#;
        write_ixml_fixture(&path, ixml, Some(chna(&[1, 2], 2, 1)));

        let result = audit(&path).unwrap();
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["ixml"]["version"], "1.52");
        assert_eq!(result.properties["ixml"]["declared_track_count"], 2);
        assert_eq!(result.properties["ixml"]["tracks"][0]["channel_index"], 6);
        assert!(result.layers[2]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-IXML-CHNA-XCHECK" && check.passed));
    }

    #[test]
    fn ixml_rejects_bad_counts_indexes_and_chna_mapping() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-ixml-adm.wav");
        let ixml = br#"<BWFXML><TRACK_LIST>
<TRACK_COUNT>1</TRACK_COUNT>
<TRACK><CHANNEL_INDEX>0</CHANNEL_INDEX><INTERLEAVE_INDEX>1</INTERLEAVE_INDEX></TRACK>
<TRACK><CHANNEL_INDEX>6</CHANNEL_INDEX><INTERLEAVE_INDEX>1</INTERLEAVE_INDEX></TRACK>
</TRACK_LIST></BWFXML>"#;
        write_ixml_fixture(&path, ixml, Some(chna(&[1, 1], 2, 0)));

        let result = audit(&path).unwrap();
        assert!(!result.passed);
        let failed = result.layers[2]
            .checks
            .iter()
            .filter(|check| !check.passed)
            .map(|check| check.rule_id)
            .collect::<Vec<_>>();
        for rule in [
            "FORGE-IXML-TRACK-COUNT",
            "FORGE-IXML-CHANNEL-INDEX",
            "FORGE-IXML-INTERLEAVE-INDEX",
            "FORGE-IXML-CHNA-XCHECK",
        ] {
            assert!(failed.contains(&rule), "missing {rule}: {result:#?}");
        }
    }

    #[test]
    fn ixml_rejects_doctype_and_allows_descriptive_only_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let valid_path = directory.path().join("descriptive-ixml.wav");
        write_ixml_fixture(
            &valid_path,
            b"<BWFXML><IXML_VERSION>1.52</IXML_VERSION><PROJECT>Forge</PROJECT></BWFXML>",
            None,
        );
        assert!(audit(&valid_path).unwrap().passed);

        let invalid_path = directory.path().join("doctype-ixml.wav");
        write_ixml_fixture(
            &invalid_path,
            b"<!DOCTYPE BWFXML [<!ENTITY x \"bad\">]><BWFXML><NOTE>&x;</NOTE></BWFXML>",
            None,
        );
        let result = audit(&invalid_path).unwrap();
        assert!(!result.passed);
        assert!(result.layers[2]
            .checks
            .iter()
            .any(|check| check.rule_id == "FORGE-IXML-XML" && !check.passed));
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
