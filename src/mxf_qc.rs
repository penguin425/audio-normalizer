//! Bounded structural QC for SMPTE ST 377-1 Material Exchange Format files.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAX_RUN_IN_BYTES: u64 = 65_535;
const MAX_KLVS: u64 = 20_000_000;
const MAX_CONTROL_VALUE_BYTES: u64 = 16 * 1024 * 1024;
const PARTITION_PREFIX: [u8; 13] = [
    0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01,
];
const RIP_KEY: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x11, 0x01, 0x00,
];
const PRIMER_KEY: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x05, 0x01, 0x00,
];
const INDEX_SEGMENT_PREFIX: [u8; 14] = [
    0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x10,
];
const GC_ESSENCE_PREFIX: [u8; 12] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x02, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01,
];
const OP1A: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x01, 0x09, 0x00,
];
const AS11_CORE_FRAMEWORK: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x02, 0x7f, 0x01, 0x01, 0x0d, 0x01, 0x07, 0x01, 0x0b, 0x01, 0x01, 0x00,
];
const AUDIO_SAMPLING_RATE_UL: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x05, 0x04, 0x02, 0x03, 0x01, 0x01, 0x01, 0x00, 0x00,
];
const CHANNEL_COUNT_UL: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x05, 0x04, 0x02, 0x01, 0x01, 0x04, 0x00, 0x00, 0x00,
];
const QUANTIZATION_BITS_UL: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x04, 0x04, 0x02, 0x03, 0x03, 0x04, 0x00, 0x00, 0x00,
];
const BLOCK_ALIGN_UL: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x05, 0x04, 0x02, 0x03, 0x02, 0x01, 0x00, 0x00, 0x00,
];
const AVG_BPS_UL: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x05, 0x04, 0x02, 0x03, 0x03, 0x05, 0x00, 0x00, 0x00,
];
const CHANNEL_ASSIGNMENT_UL: [u8; 16] = [
    0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0x07, 0x04, 0x02, 0x01, 0x01, 0x05, 0x00, 0x00, 0x00,
];

#[derive(Debug, Clone)]
struct Klv {
    offset: u64,
    value_offset: u64,
    value_len: u64,
    end: u64,
    key: [u8; 16],
}

#[derive(Debug, Clone)]
struct Partition {
    offset: u64,
    kind: u8,
    status: u8,
    major: u16,
    minor: u16,
    kag_size: u32,
    this_partition: u64,
    previous_partition: u64,
    footer_partition: u64,
    header_byte_count: u64,
    index_byte_count: u64,
    index_sid: u32,
    body_offset: u64,
    body_sid: u32,
    operational_pattern: [u8; 16],
    essence_containers: Vec<[u8; 16]>,
}

#[derive(Debug, Default, Clone)]
struct SoundDescriptor {
    kind: String,
    audio_sampling_rate: Option<(u32, u32)>,
    channel_count: Option<u32>,
    quantization_bits: Option<u32>,
    block_align: Option<u16>,
    average_bytes_per_second: Option<u32>,
    channel_assignment: Option<[u8; 16]>,
}

#[derive(Debug, Default)]
struct State {
    klvs: u64,
    scanned_bytes: u64,
    bounds_valid: bool,
    key_valid: bool,
    partition_values_valid: bool,
    partitions: Vec<Partition>,
    partition_klv_ends: Vec<u64>,
    primer_packs: u64,
    primer_valid: bool,
    primer_tags: BTreeMap<u16, [u8; 16]>,
    index_segments: u64,
    essence_elements: u64,
    picture_elements: u64,
    sound_elements: u64,
    data_elements: u64,
    compound_elements: u64,
    essence_bytes: u64,
    essence_sids: BTreeSet<u32>,
    sound_descriptors: Vec<SoundDescriptor>,
    as11_core_frameworks: u64,
    rip: Option<Klv>,
    rip_entries: Vec<(u32, u64)>,
    rip_length_field: Option<u32>,
}

pub(crate) fn probe(file: &mut File, file_size: u64) -> Result<bool, String> {
    let probe_len = file_size.min(MAX_RUN_IN_BYTES + 16);
    let mut bytes = vec![0_u8; usize::try_from(probe_len).unwrap()];
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek MXF probe: {error}"))?;
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read MXF probe: {error}"))?;
    Ok(find_partition_key(&bytes).is_some())
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    let run_in = find_first_partition(&mut file, file_size)?
        .ok_or_else(|| format!("{}: no MXF Header Partition Pack found", path.display()))?;
    let mut state = State {
        bounds_valid: true,
        key_valid: true,
        partition_values_valid: true,
        primer_valid: true,
        ..State::default()
    };
    let mut offset = run_in;
    let mut current_body_sid = 0_u32;

    while offset < file_size {
        if state.klvs == MAX_KLVS {
            state.bounds_valid = false;
            break;
        }
        let Some(klv) = read_klv(path, &mut file, offset, file_size, &mut state)? else {
            break;
        };
        state.klvs += 1;
        state.scanned_bytes = klv.end;

        if is_partition_key(&klv.key) {
            match parse_partition(path, &mut file, &klv) {
                Ok(partition) => {
                    current_body_sid = partition.body_sid;
                    state.partition_klv_ends.push(klv.end);
                    state.partitions.push(partition);
                }
                Err(()) => state.partition_values_valid = false,
            }
        } else if klv.key == RIP_KEY {
            parse_rip(path, &mut file, &klv, &mut state)?;
        } else if klv.key == PRIMER_KEY {
            state.primer_packs += 1;
            parse_primer(path, &mut file, &klv, &mut state)?;
        } else if klv.key[..INDEX_SEGMENT_PREFIX.len()] == INDEX_SEGMENT_PREFIX {
            state.index_segments += 1;
        } else if klv.key[..GC_ESSENCE_PREFIX.len()] == GC_ESSENCE_PREFIX {
            state.essence_elements += 1;
            state.essence_bytes = state.essence_bytes.saturating_add(klv.value_len);
            if current_body_sid != 0 {
                state.essence_sids.insert(current_body_sid);
            }
            match klv.key[12] {
                0x15 => state.picture_elements += 1,
                0x16 => state.sound_elements += 1,
                0x17 => state.data_elements += 1,
                0x18 => state.compound_elements += 1,
                _ => {}
            }
        } else if klv.key == AS11_CORE_FRAMEWORK {
            state.as11_core_frameworks += 1;
        } else if let Some(kind) = sound_descriptor_kind(&klv.key) {
            if klv.value_len <= MAX_CONTROL_VALUE_BYTES {
                let value = read_value(path, &mut file, &klv)?;
                state.sound_descriptors.push(parse_sound_descriptor(
                    kind,
                    &value,
                    &state.primer_tags,
                ));
            } else {
                state.partition_values_valid = false;
            }
        }
        offset = klv.end;
    }

    let structure = validate_partitions(&state, file_size, run_in);
    let rip = validate_rip(&state, file_size, run_in);
    let indexing = validate_indexing(&state);
    let essence = validate_essence(&state);
    let descriptors = validate_sound_descriptors(&state);
    let as11 = validate_as11(&state, &structure, &rip, &indexing, &descriptors);
    let operational_pattern = structure.operational_pattern.clone();
    let format = if operational_pattern == "OP-Atom" {
        "mxf-opatom"
    } else {
        "mxf"
    };

    let wrapper = vec![
        check(
            "FORGE-MXF-KLV-BOUNDS",
            state.bounds_valid && state.key_valid && state.scanned_bytes == file_size,
            "every KLV key, BER length, and value is bounded by the file and parser limits",
            Some(json!({
                "file_bytes": file_size,
                "run_in_bytes": run_in,
                "scanned_bytes": state.scanned_bytes,
                "klvs": state.klvs,
                "klv_limit": MAX_KLVS,
                "max_run_in_bytes": MAX_RUN_IN_BYTES,
            })),
        ),
        check(
            "FORGE-MXF-PARTITION-PACK",
            state.partition_values_valid && state.primer_valid && structure.values_valid,
            "partition packs have valid fixed fields, batches, kinds, versions, and KAG sizes",
            Some(json!({
                "partitions": state.partitions.len(),
                "header_partitions": structure.headers,
                "body_partitions": structure.bodies,
                "footer_partitions": structure.footers,
                "closed_complete_partitions": structure.closed_complete,
                "primer_packs": state.primer_packs,
                "primer_mappings": state.primer_tags.len(),
            })),
        ),
        check(
            "FORGE-MXF-PARTITION-LINKS",
            structure.links_valid,
            "ThisPartition, PreviousPartition, FooterPartition, and on-disk order agree",
            Some(json!({
                "partition_offsets": state.partitions.iter().map(|p| p.offset).collect::<Vec<_>>(),
                "footer_offset": structure.footer_offset,
            })),
        ),
        check(
            "FORGE-MXF-RIP",
            rip.valid,
            "the terminal Random Index Pack length and BodySID/partition entries are consistent",
            Some(json!({
                "present": state.rip.is_some(),
                "entries": state.rip_entries.len(),
                "terminal": rip.terminal,
                "declared_total_bytes": state.rip_length_field,
            })),
        ),
    ];
    let bitstream = vec![
        check(
            "FORGE-MXF-OPERATIONAL-PATTERN",
            structure.operational_pattern_valid,
            "all Partition Packs carry one stable operational-pattern UL",
            Some(json!({
                "operational_pattern": operational_pattern,
                "operational_pattern_ul": structure.operational_pattern_ul,
            })),
        ),
        check(
            "FORGE-MXF-INDEX-TABLE",
            indexing.valid,
            "IndexByteCount/IndexSID declarations agree with bounded Index Table Segments",
            Some(json!({
                "index_segments": state.index_segments,
                "declared_index_partitions": indexing.declared_partitions,
                "index_sids": indexing.index_sids,
            })),
        ),
        check(
            "FORGE-MXF-ESSENCE-CONTAINER",
            essence.valid,
            "Generic Container essence elements occur in declared non-zero BodySID partitions",
            Some(json!({
                "elements": state.essence_elements,
                "essence_bytes": state.essence_bytes,
                "picture_elements": state.picture_elements,
                "sound_elements": state.sound_elements,
                "data_elements": state.data_elements,
                "compound_elements": state.compound_elements,
                "body_sids": state.essence_sids,
                "essence_container_labels": structure.essence_container_labels,
            })),
        ),
        check(
            "FORGE-MXF-SOUND-DESCRIPTOR",
            descriptors.valid,
            "sound essence has coherent sampling-rate, channel-count, quantization, and block-align descriptors",
            Some(json!({
                "sound_descriptors": descriptor_json(&state.sound_descriptors),
                "sound_essence_present": state.sound_elements > 0,
            })),
        ),
    ];
    let xcheck = vec![
        check(
            "FORGE-MXF-SID-CROSSCHECK",
            structure.sid_valid && essence.sid_valid,
            "BodySID and IndexSID use is non-conflicting and matches essence/index declarations",
            Some(json!({
                "body_sids": structure.body_sids,
                "index_sids": indexing.index_sids,
                "essence_sids": state.essence_sids,
            })),
        ),
        check(
            "FORGE-MXF-AS11-DPP",
            as11.valid,
            if as11.detected {
                "detected AS-11 Core metadata satisfies auditable UK DPP structural/audio constraints"
            } else {
                "AS-11 Core metadata was not detected; AS-11/DPP constraints are not claimed"
            },
            Some(json!({
                "detected": as11.detected,
                "core_frameworks": state.as11_core_frameworks,
                "op1a": structure.operational_pattern == "OP1a",
                "all_kag_one": structure.all_kag_one,
                "header_closed_complete": structure.header_closed_complete,
                "audio_48khz_24bit_mono": as11.audio_valid,
                "index_precedes_essence": as11.index_precedes_essence,
            })),
        ),
    ];

    Ok(finish_audit(
        path,
        format,
        wrapper,
        bitstream,
        xcheck,
        json!({
            "standard": "SMPTE ST 377-1:2019",
            "run_in_bytes": run_in,
            "klvs": state.klvs,
            "partitions": state.partitions.len(),
            "operational_pattern": operational_pattern,
            "operational_pattern_ul": structure.operational_pattern_ul,
            "primer_packs": state.primer_packs,
            "index_segments": state.index_segments,
            "essence_elements": state.essence_elements,
            "essence_bytes": state.essence_bytes,
            "sound_descriptors": descriptor_json(&state.sound_descriptors),
            "rip_entries": state.rip_entries.iter().map(|(sid, offset)| json!({"body_sid": sid, "offset": offset})).collect::<Vec<_>>(),
            "as11_core_detected": as11.detected,
            "as11_profile": if as11.detected { Some("AMWA AS-11 UK DPP structural/audio subset") } else { None },
        }),
    ))
}

fn find_partition_key(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(16)
        .position(|window| is_partition_key(window.try_into().unwrap()))
}

fn find_first_partition(file: &mut File, file_size: u64) -> Result<Option<u64>, String> {
    let len = file_size.min(MAX_RUN_IN_BYTES + 16);
    let mut bytes = vec![0_u8; usize::try_from(len).unwrap()];
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek MXF header: {error}"))?;
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read MXF header: {error}"))?;
    Ok(find_partition_key(&bytes).map(|offset| offset as u64))
}

fn is_partition_key(key: &[u8; 16]) -> bool {
    key[..PARTITION_PREFIX.len()] == PARTITION_PREFIX
        && matches!(key[13], 0x02..=0x04)
        && matches!(key[14], 0x01..=0x04)
        && key[15] == 0
}

fn read_klv(
    path: &Path,
    file: &mut File,
    offset: u64,
    file_size: u64,
    state: &mut State,
) -> Result<Option<Klv>, String> {
    if file_size.saturating_sub(offset) < 17 {
        state.bounds_valid = false;
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} to {offset}: {error}", path.display()))?;
    let mut key = [0_u8; 16];
    file.read_exact(&mut key)
        .map_err(|error| format!("read {} KLV key at {offset}: {error}", path.display()))?;
    if key[..4] != [0x06, 0x0e, 0x2b, 0x34] {
        state.key_valid = false;
    }
    let Some((value_len, length_bytes)) = read_ber_length(file)? else {
        state.bounds_valid = false;
        return Ok(None);
    };
    let value_offset = offset + 16 + length_bytes;
    let Some(end) = value_offset.checked_add(value_len) else {
        state.bounds_valid = false;
        return Ok(None);
    };
    if end > file_size {
        state.bounds_valid = false;
        return Ok(None);
    }
    Ok(Some(Klv {
        offset,
        value_offset,
        value_len,
        end,
        key,
    }))
}

fn read_ber_length(file: &mut File) -> Result<Option<(u64, u64)>, String> {
    let mut first = [0_u8; 1];
    file.read_exact(&mut first)
        .map_err(|error| format!("read MXF BER length: {error}"))?;
    if first[0] < 0x80 {
        return Ok(Some((u64::from(first[0]), 1)));
    }
    let count = usize::from(first[0] & 0x7f);
    if count == 0 || count > 8 {
        return Ok(None);
    }
    let mut bytes = [0_u8; 8];
    file.read_exact(&mut bytes[8 - count..])
        .map_err(|error| format!("read MXF BER long length: {error}"))?;
    Ok(Some((u64::from_be_bytes(bytes), 1 + count as u64)))
}

fn read_value(path: &Path, file: &mut File, klv: &Klv) -> Result<Vec<u8>, String> {
    let len = usize::try_from(klv.value_len)
        .map_err(|_| format!("{} KLV value is too large for this host", path.display()))?;
    let mut value = vec![0_u8; len];
    file.seek(SeekFrom::Start(klv.value_offset))
        .map_err(|error| format!("seek {} KLV value: {error}", path.display()))?;
    file.read_exact(&mut value)
        .map_err(|error| format!("read {} KLV value: {error}", path.display()))?;
    Ok(value)
}

fn parse_partition(path: &Path, file: &mut File, klv: &Klv) -> Result<Partition, ()> {
    if klv.value_len < 88 || klv.value_len > MAX_CONTROL_VALUE_BYTES {
        return Err(());
    }
    let value = read_value(path, file, klv).map_err(|_| ())?;
    let count = be_u32(&value, 80).ok_or(())?;
    let item_len = be_u32(&value, 84).ok_or(())?;
    if item_len != 16 {
        return Err(());
    }
    let labels_bytes = u64::from(count).checked_mul(16).ok_or(())?;
    if 88_u64.checked_add(labels_bytes).ok_or(())? != klv.value_len {
        return Err(());
    }
    let mut essence_containers = Vec::with_capacity(usize::try_from(count).map_err(|_| ())?);
    for index in 0..usize::try_from(count).map_err(|_| ())? {
        let start = 88 + index * 16;
        essence_containers.push(value[start..start + 16].try_into().unwrap());
    }
    Ok(Partition {
        offset: klv.offset,
        kind: klv.key[13],
        status: klv.key[14],
        major: be_u16(&value, 0).ok_or(())?,
        minor: be_u16(&value, 2).ok_or(())?,
        kag_size: be_u32(&value, 4).ok_or(())?,
        this_partition: be_u64(&value, 8).ok_or(())?,
        previous_partition: be_u64(&value, 16).ok_or(())?,
        footer_partition: be_u64(&value, 24).ok_or(())?,
        header_byte_count: be_u64(&value, 32).ok_or(())?,
        index_byte_count: be_u64(&value, 40).ok_or(())?,
        index_sid: be_u32(&value, 48).ok_or(())?,
        body_offset: be_u64(&value, 52).ok_or(())?,
        body_sid: be_u32(&value, 60).ok_or(())?,
        operational_pattern: value[64..80].try_into().unwrap(),
        essence_containers,
    })
}

fn parse_rip(path: &Path, file: &mut File, klv: &Klv, state: &mut State) -> Result<(), String> {
    if state.rip.is_some()
        || klv.value_len < 4
        || !(klv.value_len - 4).is_multiple_of(12)
        || klv.value_len > MAX_CONTROL_VALUE_BYTES
    {
        state.partition_values_valid = false;
        return Ok(());
    }
    let value = read_value(path, file, klv)?;
    for entry in value[..value.len() - 4].chunks_exact(12) {
        state.rip_entries.push((
            u32::from_be_bytes(entry[..4].try_into().unwrap()),
            u64::from_be_bytes(entry[4..12].try_into().unwrap()),
        ));
    }
    state.rip_length_field = Some(u32::from_be_bytes(
        value[value.len() - 4..].try_into().unwrap(),
    ));
    state.rip = Some(klv.clone());
    Ok(())
}

fn parse_primer(path: &Path, file: &mut File, klv: &Klv, state: &mut State) -> Result<(), String> {
    if klv.value_len < 8 || klv.value_len > MAX_CONTROL_VALUE_BYTES {
        state.primer_valid = false;
        return Ok(());
    }
    let value = read_value(path, file, klv)?;
    let count = be_u32(&value, 0).unwrap();
    let item_len = be_u32(&value, 4).unwrap();
    let expected_len = u64::from(count)
        .checked_mul(u64::from(item_len))
        .and_then(|bytes| 8_u64.checked_add(bytes));
    let Ok(count) = usize::try_from(count) else {
        state.primer_valid = false;
        return Ok(());
    };
    if item_len != 18 || expected_len != Some(klv.value_len) {
        state.primer_valid = false;
        return Ok(());
    }
    for index in 0..count {
        let start = 8 + index * 18;
        let tag = u16::from_be_bytes(value[start..start + 2].try_into().unwrap());
        let ul = value[start + 2..start + 18].try_into().unwrap();
        if let Some(existing) = state.primer_tags.insert(tag, ul) {
            state.primer_valid &= existing == ul;
        }
    }
    Ok(())
}

fn sound_descriptor_kind(key: &[u8; 16]) -> Option<&'static str> {
    if key[..13]
        != [
            0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x01, 0x01, 0x01,
        ]
        || key[15] != 0
    {
        return None;
    }
    match key[14] {
        0x42 => Some("generic-sound"),
        0x47 => Some("aes3"),
        0x48 => Some("wave-audio"),
        _ => None,
    }
}

fn parse_sound_descriptor(
    kind: &str,
    value: &[u8],
    primer_tags: &BTreeMap<u16, [u8; 16]>,
) -> SoundDescriptor {
    let mut result = SoundDescriptor {
        kind: kind.into(),
        ..SoundDescriptor::default()
    };
    let mut offset = 0_usize;
    while value.len().saturating_sub(offset) >= 4 {
        let tag = u16::from_be_bytes(value[offset..offset + 2].try_into().unwrap());
        let len = usize::from(u16::from_be_bytes(
            value[offset + 2..offset + 4].try_into().unwrap(),
        ));
        offset += 4;
        let Some(end) = offset.checked_add(len) else {
            break;
        };
        if end > value.len() {
            break;
        }
        let field = &value[offset..end];
        match (sound_property(tag, primer_tags), field.len()) {
            (Some(SoundProperty::SamplingRate), 8) => {
                result.audio_sampling_rate =
                    Some((be_u32(field, 0).unwrap(), be_u32(field, 4).unwrap()))
            }
            (Some(SoundProperty::ChannelCount), 4) => result.channel_count = be_u32(field, 0),
            (Some(SoundProperty::QuantizationBits), 4) => {
                result.quantization_bits = be_u32(field, 0)
            }
            (Some(SoundProperty::BlockAlign), 2) => result.block_align = be_u16(field, 0),
            (Some(SoundProperty::AverageBytesPerSecond), 4) => {
                result.average_bytes_per_second = be_u32(field, 0)
            }
            (Some(SoundProperty::ChannelAssignment), 16) => {
                result.channel_assignment = Some(field.try_into().unwrap())
            }
            _ => {}
        }
        offset = end;
    }
    result
}

#[derive(Clone, Copy)]
enum SoundProperty {
    SamplingRate,
    ChannelCount,
    QuantizationBits,
    BlockAlign,
    AverageBytesPerSecond,
    ChannelAssignment,
}

fn sound_property(tag: u16, primer_tags: &BTreeMap<u16, [u8; 16]>) -> Option<SoundProperty> {
    match tag {
        0x3d03 => return Some(SoundProperty::SamplingRate),
        0x3d07 => return Some(SoundProperty::ChannelCount),
        0x3d01 => return Some(SoundProperty::QuantizationBits),
        0x3d0a => return Some(SoundProperty::BlockAlign),
        0x3d09 => return Some(SoundProperty::AverageBytesPerSecond),
        0x3d32 => return Some(SoundProperty::ChannelAssignment),
        _ => {}
    }
    match primer_tags.get(&tag)? {
        ul if *ul == AUDIO_SAMPLING_RATE_UL => Some(SoundProperty::SamplingRate),
        ul if *ul == CHANNEL_COUNT_UL => Some(SoundProperty::ChannelCount),
        ul if *ul == QUANTIZATION_BITS_UL => Some(SoundProperty::QuantizationBits),
        ul if *ul == BLOCK_ALIGN_UL => Some(SoundProperty::BlockAlign),
        ul if *ul == AVG_BPS_UL => Some(SoundProperty::AverageBytesPerSecond),
        ul if *ul == CHANNEL_ASSIGNMENT_UL => Some(SoundProperty::ChannelAssignment),
        _ => None,
    }
}

#[derive(Default)]
struct StructureValidation {
    values_valid: bool,
    links_valid: bool,
    sid_valid: bool,
    operational_pattern_valid: bool,
    operational_pattern: String,
    operational_pattern_ul: Option<String>,
    headers: usize,
    bodies: usize,
    footers: usize,
    closed_complete: usize,
    footer_offset: Option<u64>,
    header_closed_complete: bool,
    all_kag_one: bool,
    body_sids: BTreeSet<u32>,
    essence_container_labels: Vec<String>,
}

fn validate_partitions(state: &State, file_size: u64, run_in: u64) -> StructureValidation {
    let mut result = StructureValidation {
        values_valid: !state.partitions.is_empty(),
        links_valid: !state.partitions.is_empty(),
        sid_valid: true,
        operational_pattern_valid: !state.partitions.is_empty(),
        all_kag_one: !state.partitions.is_empty(),
        ..StructureValidation::default()
    };
    let footer_offsets: Vec<u64> = state
        .partitions
        .iter()
        .filter(|partition| partition.kind == 0x04)
        .map(|partition| partition.offset)
        .collect();
    result.footer_offset = footer_offsets.last().copied();
    let logical_footer = result
        .footer_offset
        .and_then(|offset| offset.checked_sub(run_in));
    let first_op = state
        .partitions
        .first()
        .map(|partition| partition.operational_pattern);
    let mut previous: Option<u64> = None;
    let mut labels = BTreeSet::new();
    let mut sid_kinds = BTreeMap::<u32, &'static str>::new();

    for (index, partition) in state.partitions.iter().enumerate() {
        result.headers += usize::from(partition.kind == 0x02);
        result.bodies += usize::from(partition.kind == 0x03);
        result.footers += usize::from(partition.kind == 0x04);
        result.closed_complete += usize::from(partition.status == 0x04);
        result.all_kag_one &= partition.kag_size == 1;
        result.values_valid &= partition.major == 1
            && matches!(partition.minor, 2 | 3)
            && partition.kag_size > 0
            && partition.offset < file_size;
        let partition_end = state
            .partitions
            .get(index + 1)
            .map(|next| next.offset)
            .or_else(|| state.rip.as_ref().map(|rip| rip.offset))
            .unwrap_or(file_size);
        let control_start = state
            .partition_klv_ends
            .get(index)
            .copied()
            .unwrap_or(partition_end);
        result.values_valid &= partition
            .header_byte_count
            .checked_add(partition.index_byte_count)
            .is_some_and(|bytes| bytes <= partition_end.saturating_sub(control_start));
        let logical_offset = partition.offset.saturating_sub(run_in);
        result.links_valid &= partition.this_partition == logical_offset;
        result.links_valid &= if index == 0 {
            partition.kind == 0x02 && partition.previous_partition == 0
        } else {
            partition.previous_partition == previous.unwrap().saturating_sub(run_in)
                && partition.kind != 0x02
                && partition.offset > previous.unwrap()
        };
        if let Some(footer) = logical_footer {
            result.links_valid &= partition.footer_partition == 0
                || partition.footer_partition == footer
                || (partition.kind == 0x04 && partition.footer_partition == logical_offset);
        } else {
            result.links_valid &= partition.footer_partition == 0;
        }
        result.operational_pattern_valid &= first_op == Some(partition.operational_pattern);
        if partition.body_sid != 0 {
            result.body_sids.insert(partition.body_sid);
            result.sid_valid &= sid_kinds.insert(partition.body_sid, "body").is_none()
                || sid_kinds.get(&partition.body_sid) == Some(&"body");
        }
        if partition.index_sid != 0 {
            result.sid_valid &= partition.index_byte_count > 0;
        } else {
            result.sid_valid &= partition.index_byte_count == 0;
        }
        if partition.body_sid == 0 {
            result.sid_valid &= partition.body_offset == 0;
        }
        for label in &partition.essence_containers {
            labels.insert(ul_string(label));
        }
        previous = Some(partition.offset);
    }
    result.links_valid &= result.headers == 1 && result.footers <= 1;
    if let Some(footer) = result.footer_offset {
        result.links_valid &= state.partitions.last().map(|p| p.offset) == Some(footer);
    }
    result.header_closed_complete = state
        .partitions
        .first()
        .is_some_and(|partition| partition.kind == 0x02 && partition.status == 0x04);
    result.essence_container_labels = labels.into_iter().collect();
    if let Some(op) = first_op {
        result.operational_pattern_ul = Some(ul_string(&op));
        result.operational_pattern = if op == OP1A {
            "OP1a".into()
        } else if is_op_atom(&op) {
            "OP-Atom".into()
        } else {
            "other".into()
        };
    }
    result
}

fn is_op_atom(op: &[u8; 16]) -> bool {
    op[..7] == [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01]
        && op[8..13] == [0x0d, 0x01, 0x02, 0x01, 0x10]
}

#[derive(Default)]
struct RipValidation {
    valid: bool,
    terminal: bool,
}

fn validate_rip(state: &State, file_size: u64, run_in: u64) -> RipValidation {
    let Some(rip) = &state.rip else {
        return RipValidation::default();
    };
    let total = rip.end - rip.offset;
    let terminal = rip.end == file_size;
    let entries_match = state.rip_entries.len() == state.partitions.len()
        && state
            .rip_entries
            .iter()
            .zip(&state.partitions)
            .all(|((sid, offset), partition)| {
                *offset == partition.offset.saturating_sub(run_in) && *sid == partition.body_sid
            });
    RipValidation {
        valid: terminal && entries_match && state.rip_length_field == u32::try_from(total).ok(),
        terminal,
    }
}

#[derive(Default)]
struct IndexValidation {
    valid: bool,
    declared_partitions: usize,
    index_sids: BTreeSet<u32>,
}

fn validate_indexing(state: &State) -> IndexValidation {
    let declared: Vec<_> = state
        .partitions
        .iter()
        .filter(|partition| partition.index_byte_count > 0)
        .collect();
    let index_sids = declared
        .iter()
        .map(|partition| partition.index_sid)
        .collect();
    IndexValidation {
        valid: if declared.is_empty() {
            state.index_segments == 0
        } else {
            state.index_segments > 0
                && state.index_segments >= declared.len() as u64
                && declared.iter().all(|partition| partition.index_sid != 0)
        },
        declared_partitions: declared.len(),
        index_sids,
    }
}

#[derive(Default)]
struct EssenceValidation {
    valid: bool,
    sid_valid: bool,
}

fn validate_essence(state: &State) -> EssenceValidation {
    let declared_sids: BTreeSet<_> = state
        .partitions
        .iter()
        .filter_map(|partition| (partition.body_sid != 0).then_some(partition.body_sid))
        .collect();
    let sid_valid = !state.essence_sids.contains(&0)
        && state
            .essence_sids
            .iter()
            .all(|sid| declared_sids.contains(sid));
    EssenceValidation {
        valid: state.essence_elements > 0
            && state.essence_bytes > 0
            && !state.essence_sids.is_empty()
            && sid_valid
            && state
                .partitions
                .first()
                .is_some_and(|partition| !partition.essence_containers.is_empty()),
        sid_valid,
    }
}

#[derive(Default)]
struct DescriptorValidation {
    valid: bool,
}

fn validate_sound_descriptors(state: &State) -> DescriptorValidation {
    if state.sound_elements == 0 {
        return DescriptorValidation { valid: true };
    }
    let valid = !state.sound_descriptors.is_empty()
        && state.sound_descriptors.iter().all(|descriptor| {
            descriptor
                .audio_sampling_rate
                .is_some_and(|(numerator, denominator)| numerator > 0 && denominator > 0)
                && descriptor
                    .channel_count
                    .is_some_and(|channels| channels > 0)
                && descriptor
                    .quantization_bits
                    .is_some_and(|bits| matches!(bits, 8 | 16 | 20 | 24 | 32))
                && match (
                    descriptor.block_align,
                    descriptor.channel_count,
                    descriptor.quantization_bits,
                ) {
                    (Some(align), Some(channels), Some(bits)) => {
                        u32::from(align) >= channels.saturating_mul(bits.div_ceil(8))
                    }
                    _ => true,
                }
                && descriptor
                    .channel_assignment
                    .is_none_or(|ul| ul[..4] == [0x06, 0x0e, 0x2b, 0x34])
        });
    DescriptorValidation { valid }
}

#[derive(Default)]
struct As11Validation {
    detected: bool,
    valid: bool,
    audio_valid: bool,
    index_precedes_essence: bool,
}

fn validate_as11(
    state: &State,
    structure: &StructureValidation,
    rip: &RipValidation,
    indexing: &IndexValidation,
    descriptors: &DescriptorValidation,
) -> As11Validation {
    let detected = state.as11_core_frameworks > 0;
    if !detected {
        return As11Validation {
            valid: true,
            ..As11Validation::default()
        };
    }
    let audio_valid = !state.sound_descriptors.is_empty()
        && state.sound_descriptors.iter().all(|descriptor| {
            descriptor.audio_sampling_rate == Some((48_000, 1))
                && descriptor.quantization_bits == Some(24)
                && descriptor.channel_count == Some(1)
        });
    let first_index = state
        .partitions
        .iter()
        .filter(|partition| partition.index_byte_count > 0)
        .map(|partition| partition.offset)
        .min();
    let first_essence = state
        .partitions
        .iter()
        .filter(|partition| partition.body_sid != 0)
        .map(|partition| partition.offset)
        .min();
    let index_precedes_essence =
        matches!((first_index, first_essence), (Some(index), Some(essence)) if index <= essence);
    As11Validation {
        detected,
        valid: state.as11_core_frameworks == 1
            && structure.operational_pattern == "OP1a"
            && structure.header_closed_complete
            && structure.all_kag_one
            && rip.valid
            && indexing.valid
            && descriptors.valid
            && audio_valid
            && index_precedes_essence,
        audio_valid,
        index_precedes_essence,
    }
}

fn descriptor_json(descriptors: &[SoundDescriptor]) -> Vec<serde_json::Value> {
    descriptors
        .iter()
        .map(|descriptor| {
            json!({
                "kind": descriptor.kind,
                "audio_sampling_rate": descriptor.audio_sampling_rate.map(|(n, d)| format!("{n}/{d}")),
                "channel_count": descriptor.channel_count,
                "quantization_bits": descriptor.quantization_bits,
                "block_align": descriptor.block_align,
                "average_bytes_per_second": descriptor.average_bytes_per_second,
                "channel_assignment": descriptor.channel_assignment.map(|ul| ul_string(&ul)),
            })
        })
        .collect()
}

fn ul_string(ul: &[u8; 16]) -> String {
    ul.chunks_exact(4)
        .map(|chunk| {
            chunk
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn be_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn recognizes_registry_versioned_op_atom_label() {
        assert!(is_op_atom(&[
            0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x02, 0x0d, 0x01, 0x02, 0x01, 0x10, 0x03,
            0x00, 0x00,
        ]));
    }

    #[test]
    fn primer_mapping_resolves_dynamic_sound_descriptor_tags() {
        let mut tags = BTreeMap::new();
        tags.insert(0x9001, AUDIO_SAMPLING_RATE_UL);
        tags.insert(0x9002, CHANNEL_ASSIGNMENT_UL);
        assert!(matches!(
            sound_property(0x9001, &tags),
            Some(SoundProperty::SamplingRate)
        ));
        assert!(matches!(
            sound_property(0x9002, &tags),
            Some(SoundProperty::ChannelAssignment)
        ));
    }

    #[test]
    fn real_op1a_mxf_has_partitions_rip_index_essence_and_sound_descriptor() {
        if !Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("programme.mxf");
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=128x72:rate=25:duration=0.2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=997:sample_rate=48000:duration=0.2",
                "-c:v",
                "mpeg2video",
                "-pix_fmt",
                "yuv422p",
                "-c:a",
                "pcm_s24le",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-f",
                "mxf",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:#?}");
        let result = crate::container_qc::audit(&path).unwrap();
        assert_eq!(result.format, "mxf");
        assert!(result.passed, "{result:#?}");
        assert_eq!(result.properties["operational_pattern"], "OP1a");
        assert!(result.properties["index_segments"].as_u64().unwrap() > 0);
        assert!(result.properties["essence_elements"].as_u64().unwrap() > 0);
        assert!(!result.properties["sound_descriptors"]
            .as_array()
            .unwrap()
            .is_empty());

        let run_in_path = directory.path().join("programme-with-run-in.mxf");
        let mut run_in_bytes = vec![0x55; 32];
        run_in_bytes.extend_from_slice(&std::fs::read(&path).unwrap());
        std::fs::write(&run_in_path, run_in_bytes).unwrap();
        let run_in_result = crate::container_qc::audit(&run_in_path).unwrap();
        assert!(run_in_result.passed, "{run_in_result:#?}");
        assert_eq!(run_in_result.properties["run_in_bytes"], 32);
    }

    #[test]
    fn truncated_klv_and_broken_rip_are_qc_failures() {
        if !Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let valid_path = directory.path().join("valid.mxf");
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=128x72:rate=25:duration=0.1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=997:sample_rate=48000:duration=0.1",
                "-c:v",
                "mpeg2video",
                "-pix_fmt",
                "yuv422p",
                "-c:a",
                "pcm_s24le",
                "-f",
                "mxf",
            ])
            .arg(&valid_path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:#?}");
        let mut bytes = std::fs::read(&valid_path).unwrap();
        bytes.truncate(bytes.len() - 2);
        let truncated_path = directory.path().join("truncated.mxf");
        File::create(&truncated_path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let result = crate::container_qc::audit(&truncated_path).unwrap();
        assert!(!result.passed);
        assert!(result.layers[0]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MXF-KLV-BOUNDS" && !item.passed));
        assert!(result.layers[0]
            .checks
            .iter()
            .any(|item| item.rule_id == "FORGE-MXF-RIP" && !item.passed));
    }
}
