//! Dependency-free AC-3 and E-AC-3 elementary-stream QC.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_FRAMES: u64 = 10_000_000;
const MAX_FRAME_BYTES: usize = 4096;
const SAMPLE_RATES: [u32; 3] = [48_000, 44_100, 32_000];
const HALF_SAMPLE_RATES: [u32; 3] = [24_000, 22_050, 16_000];
const BITRATES_KBPS: [u32; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Ac3,
    Eac3,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
        }
    }
}

#[derive(Debug)]
struct FrameInfo {
    frame_bytes: usize,
    sample_rate: u32,
    blocks: u8,
    bsid: u8,
    acmod: u8,
    lfe: bool,
    dialnorm: u8,
    compression_word: Option<u8>,
    compression_word2: Option<u8>,
    stream_type: Option<u8>,
    substream_id: Option<u8>,
    channel_map: Option<u16>,
    additional_bsi: Vec<u8>,
}

#[derive(Debug, Default)]
struct Eac3Group {
    sample_rate: u32,
    blocks: u8,
    independents: Vec<Eac3Independent>,
}

#[derive(Debug)]
struct Eac3Independent {
    id: u8,
    kind: IndependentKind,
    channel_mask: u16,
    dependents: Vec<Eac3Dependent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndependentKind {
    LegacyAc3,
    Eac3,
    Converted,
}

#[derive(Debug)]
struct Eac3Dependent {
    id: u8,
    channel_mask: u16,
    compression_word: Option<u8>,
}

#[derive(Debug)]
struct Eac3AccessUnits {
    valid: bool,
    groups: u64,
    complete_units: u64,
    current: Option<Eac3Group>,
    signature: Option<Vec<(u8, IndependentKind, Vec<u8>)>>,
    accumulated_blocks: BTreeMap<u8, u8>,
    presentation_masks: BTreeMap<u8, u16>,
}

impl Default for Eac3AccessUnits {
    fn default() -> Self {
        Self {
            valid: true,
            groups: 0,
            complete_units: 0,
            current: None,
            signature: None,
            accumulated_blocks: BTreeMap::new(),
            presentation_masks: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    format: Option<Format>,
    frames: u64,
    ac3_frames: u64,
    eac3_frames: u64,
    bytes: u64,
    decoded_samples: u64,
    sync_valid: bool,
    bounds_valid: bool,
    headers_valid: bool,
    config_valid: bool,
    substreams_valid: bool,
    little_endian: Option<bool>,
    sample_rate: Option<u32>,
    bsid: Option<u8>,
    ac3_bsid: Option<u8>,
    eac3_bsid: Option<u8>,
    bsids: BTreeSet<u8>,
    acmod: Option<u8>,
    lfe: Option<bool>,
    dialnorms: BTreeSet<u8>,
    compression_words: BTreeSet<u8>,
    stream_types: BTreeSet<u8>,
    substream_ids: BTreeSet<u8>,
    channel_maps: BTreeSet<u16>,
    dependent_frames: u64,
    access_units: Eac3AccessUnits,
    additional_bsi_frames: u64,
    primary_i0_frames: u64,
    joc_frames: u64,
    joc_valid: bool,
    joc_complexities: BTreeSet<u8>,
}

pub(crate) fn looks_like_ac3(header: &[u8]) -> bool {
    header.len() >= 2 && matches!((header[0], header[1]), (0x0b, 0x77) | (0x77, 0x0b))
}

pub(crate) fn audit(path: &Path, mut file: File, file_size: u64) -> Result<ContainerAudit, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut state = State {
        sync_valid: true,
        bounds_valid: true,
        headers_valid: true,
        config_valid: true,
        substreams_valid: true,
        joc_valid: true,
        ..State::default()
    };
    let mut offset = 0_u64;

    while offset < file_size {
        if state.frames == MAX_FRAMES {
            state.bounds_valid = false;
            break;
        }
        if file_size - offset < 8 {
            state.bounds_valid = false;
            break;
        }
        let mut prefix = [0_u8; 8];
        reader
            .read_exact(&mut prefix)
            .map_err(|error| format!("read {} AC-3 header at {offset}: {error}", path.display()))?;
        let little_endian = match (prefix[0], prefix[1]) {
            (0x0b, 0x77) => false,
            (0x77, 0x0b) => true,
            _ => {
                state.sync_valid = false;
                break;
            }
        };
        if little_endian {
            swap_words(&mut prefix);
        }
        if state
            .little_endian
            .is_some_and(|first| first != little_endian)
        {
            state.config_valid = false;
        } else {
            state.little_endian.get_or_insert(little_endian);
        }

        let format = if prefix[5] >> 3 <= 10 {
            Format::Ac3
        } else {
            Format::Eac3
        };
        if state.format.is_none() || format == Format::Eac3 {
            state.format = Some(format);
        }
        let frame_bytes = match format {
            Format::Ac3 => ac3_frame_size(&prefix),
            Format::Eac3 => {
                Some(2 * (usize::from(prefix[2] & 0x07) * 256 + usize::from(prefix[3]) + 1))
            }
        };
        let Some(frame_bytes) = frame_bytes else {
            state.headers_valid = false;
            break;
        };
        if !(8..=MAX_FRAME_BYTES).contains(&frame_bytes)
            || offset.saturating_add(frame_bytes as u64) > file_size
        {
            state.bounds_valid = false;
            break;
        }
        let mut frame = vec![0_u8; frame_bytes];
        frame[..8].copy_from_slice(&prefix);
        reader
            .read_exact(&mut frame[8..])
            .map_err(|error| format!("read {} AC-3 frame at {offset}: {error}", path.display()))?;
        if little_endian {
            swap_words(&mut frame[8..]);
        }
        let info = match parse_frame(&frame, format) {
            Ok(info) => info,
            Err(()) => {
                state.headers_valid = false;
                break;
            }
        };
        update_state(&mut state, &info);
        state.frames += 1;
        state.bytes += frame_bytes as u64;
        if info.stream_type.is_none()
            || (matches!(info.stream_type, Some(0 | 2)) && info.substream_id == Some(0))
        {
            state.decoded_samples += u64::from(info.blocks) * 256;
        }
        offset += frame_bytes as u64;
    }
    if state.format == Some(Format::Eac3) {
        finish_eac3_access_units(&mut state.access_units);
        if state.joc_frames > 0 && state.joc_frames != state.primary_i0_frames {
            state.joc_valid = false;
        }
    }

    let format = state.format.unwrap_or(Format::Ac3);
    let prefix = if format == Format::Ac3 {
        "FORGE-AC3"
    } else {
        "FORGE-EAC3"
    };
    let mut wrapper = vec![
        check(
            if format == Format::Ac3 {
                "FORGE-AC3-SYNC"
            } else {
                "FORGE-EAC3-SYNC"
            },
            state.sync_valid && state.frames > 0,
            "every syncframe starts with the AC-3 sync word",
            Some(json!({"frames": state.frames, "scanned_bytes": state.bytes})),
        ),
        check(
            if format == Format::Ac3 {
                "FORGE-AC3-BOUNDS"
            } else {
                "FORGE-EAC3-BOUNDS"
            },
            state.bounds_valid && state.bytes == file_size,
            "syncframe sizes are bounded and consume the complete elementary stream",
            Some(json!({"file_bytes": file_size, "frame_bytes": state.bytes, "limit": MAX_FRAMES})),
        ),
    ];
    let mut bitstream = vec![
        check(
            if format == Format::Ac3 {
                "FORGE-AC3-HEADER"
            } else {
                "FORGE-EAC3-HEADER"
            },
            state.headers_valid && state.frames > 0,
            "sample rate, frame size, bitstream id, channel mode, and dialnorm syntax are valid",
            Some(json!({
                "bsid": state.bsid,
                "bitstream_ids": state.bsids,
                "sample_rate_hz": state.sample_rate,
            })),
        ),
        check(
            if format == Format::Ac3 {
                "FORGE-AC3-CONFIG"
            } else {
                "FORGE-EAC3-CONFIG"
            },
            state.config_valid,
            "core codec configuration and byte order remain stable",
            Some(json!({
                "acmod": state.acmod,
                "lfe": state.lfe,
                "byte_order": if state.little_endian == Some(true) {"little-endian words"} else {"big-endian"}
            })),
        ),
    ];
    if format == Format::Eac3 {
        bitstream.push(check(
            "FORGE-EAC3-SUBSTREAM",
            state.substreams_valid && state.access_units.valid,
            "independent and dependent substreams are ordered, sequential, and compatible",
            Some(json!({
                "dependent_frames": state.dependent_frames,
                "stream_types": state.stream_types,
                "substream_ids": state.substream_ids,
                "channel_maps": state.channel_maps,
            })),
        ));
        bitstream.push(check(
            "FORGE-EAC3-ACCESS-UNITS",
            state.access_units.valid && state.access_units.complete_units > 0,
            "each access unit carries exactly six blocks for every stable presentation",
            Some(json!({
                "frame_groups": state.access_units.groups,
                "complete_access_units": state.access_units.complete_units,
                "remaining_blocks": state.access_units.accumulated_blocks,
            })),
        ));
        bitstream.push(check(
            "FORGE-EAC3-ATMOS-JOC",
            state.joc_valid,
            if state.joc_frames == 0 {
                "no Dolby Atmos/JOC Extension Type A claim is present"
            } else {
                "Dolby Atmos/JOC Extension Type A is consistently signalled on 48 kHz 5.1 independent substream I0"
            },
            Some(json!({
                "signaled": state.joc_frames > 0,
                "joc_frames": state.joc_frames,
                "primary_i0_frames": state.primary_i0_frames,
                "complexity_index_type_a": state.joc_complexities,
                "required_complexity_range": [1, 16],
            })),
        ));
    }
    let xcheck = vec![
        check(
            if format == Format::Ac3 {
                "FORGE-AC3-DIALNORM"
            } else {
                "FORGE-EAC3-DIALNORM"
            },
            state.frames > 0 && !state.dialnorms.contains(&0),
            "dialnorm is present and uses a valid -1 through -31 dB code",
            Some(json!({
                "dialnorm_db": state.dialnorms.iter().map(|value| -i16::from(*value)).collect::<Vec<_>>(),
            })),
        ),
        check(
            if format == Format::Ac3 {
                "FORGE-AC3-DRC"
            } else {
                "FORGE-EAC3-DRC"
            },
            state.headers_valid,
            "heavy-compression control words are syntactically valid and interpreted as decoder gain",
            Some(json!({
                "rf_mode_gain_words": interpreted_gain_words(&state.compression_words, GainWord::Compression),
                "encoded_gain_ranges_db": {
                    "line_mode_dynrng": gain_range(GainWord::DynamicRange),
                    "rf_mode_compr": gain_range(GainWord::Compression),
                },
                "profile_note": "authoring preset names are not carried in AC-3/E-AC-3; reported values are the normative decoder gains",
            })),
        ),
    ];
    debug_assert!(wrapper.iter().all(|item| item.rule_id.starts_with(prefix)));
    Ok(finish_audit(
        path,
        format.name(),
        std::mem::take(&mut wrapper),
        bitstream,
        xcheck,
        json!({
            "frames": state.frames,
            "syncframe_formats": {
                "ac3": state.ac3_frames,
                "eac3": state.eac3_frames,
            },
            "bytes": state.bytes,
            "sample_rate_hz": state.sample_rate,
            "decoded_samples_per_channel": state.decoded_samples,
            "duration_seconds": state.sample_rate.map(|rate| state.decoded_samples as f64 / f64::from(rate)),
            "bsid": state.bsid,
            "bitstream_ids": state.bsids,
            "channel_mode": state.acmod.map(channel_mode),
            "channels": state.acmod.map(|mode| channel_count(mode, state.lfe.unwrap_or(false))),
            "lfe": state.lfe,
            "dialnorm_db": state.dialnorms.iter().map(|value| -i16::from(*value)).collect::<Vec<_>>(),
            "compression_control_words": state.compression_words,
            "heavy_compression_gains": interpreted_gain_words(&state.compression_words, GainWord::Compression),
            "drc_encoded_gain_ranges_db": {
                "line_mode_dynrng": gain_range(GainWord::DynamicRange),
                "rf_mode_compr": gain_range(GainWord::Compression),
            },
            "stream_types": state.stream_types,
            "substream_ids": state.substream_ids,
            "dependent_frames": state.dependent_frames,
            "channel_maps": state.channel_maps,
            "access_units": {
                "frame_groups": state.access_units.groups,
                "complete": state.access_units.complete_units,
                "valid": state.access_units.valid,
            },
            "presentations": presentations(&state.access_units.presentation_masks),
            "additional_bsi_frames": state.additional_bsi_frames,
            "atmos_joc": {
                "signaled": state.joc_frames > 0,
                "valid": state.joc_valid,
                "frames": state.joc_frames,
                "complexity_index_type_a": state.joc_complexities,
            },
        }),
    ))
}

fn update_state(state: &mut State, info: &FrameInfo) {
    if state
        .sample_rate
        .is_some_and(|value| value != info.sample_rate)
    {
        state.config_valid = false;
    }
    let format_bsid = if info.stream_type.is_none() {
        state.ac3_frames += 1;
        &mut state.ac3_bsid
    } else {
        state.eac3_frames += 1;
        &mut state.eac3_bsid
    };
    if format_bsid.is_some_and(|value| value != info.bsid) {
        state.config_valid = false;
    }
    format_bsid.get_or_insert(info.bsid);
    let primary_presentation = info.stream_type.is_none()
        || (matches!(info.stream_type, Some(0 | 2)) && info.substream_id == Some(0));
    if primary_presentation
        && (state.acmod.is_some_and(|value| value != info.acmod)
            || state.lfe.is_some_and(|value| value != info.lfe))
    {
        state.config_valid = false;
    }
    state.sample_rate.get_or_insert(info.sample_rate);
    state.bsid.get_or_insert(info.bsid);
    state.bsids.insert(info.bsid);
    if primary_presentation {
        state.acmod.get_or_insert(info.acmod);
        state.lfe.get_or_insert(info.lfe);
    }
    state.dialnorms.insert(info.dialnorm);
    if let Some(value) = info.compression_word {
        state.compression_words.insert(value);
    }
    if let Some(value) = info.compression_word2 {
        state.compression_words.insert(value);
    }
    update_eac3_group(&mut state.access_units, info);
    if let Some(stream_type) = info.stream_type {
        state.stream_types.insert(stream_type);
        if stream_type == 1 {
            state.dependent_frames += 1;
        } else if stream_type > 2 {
            state.substreams_valid = false;
        }
        state.substreams_valid &= state.access_units.valid;
        let primary_i0 = stream_type == 0 && info.substream_id == Some(0);
        if primary_i0 {
            state.primary_i0_frames += 1;
        }
        if !info.additional_bsi.is_empty() {
            state.additional_bsi_frames += 1;
        }
        match joc_signal(&info.additional_bsi) {
            JocSignal::None => {}
            JocSignal::Invalid => state.joc_valid = false,
            JocSignal::Valid(complexity) => {
                state.joc_frames += 1;
                state.joc_complexities.insert(complexity);
                if !primary_i0 || info.sample_rate != 48_000 || info.acmod != 7 || !info.lfe {
                    state.joc_valid = false;
                }
            }
        }
        if state.joc_complexities.len() > 1 {
            state.joc_valid = false;
        }
    }
    if let Some(value) = info.substream_id {
        state.substream_ids.insert(value);
    }
    if let Some(value) = info.channel_map {
        state.channel_maps.insert(value);
    }
    if info.frame_bytes == 0 {
        state.headers_valid = false;
    }
}

fn update_eac3_group(access: &mut Eac3AccessUnits, info: &FrameInfo) {
    let stream_type = info.stream_type.unwrap_or(0);
    let id = info.substream_id.unwrap_or(0);
    if matches!(stream_type, 0 | 2) {
        if id == 0 {
            finalize_eac3_group(access);
            access.current = Some(Eac3Group {
                sample_rate: info.sample_rate,
                blocks: info.blocks,
                independents: Vec::new(),
            });
        }
        let Some(group) = access.current.as_mut() else {
            access.valid = false;
            return;
        };
        if group.sample_rate != info.sample_rate
            || group.blocks != info.blocks
            || usize::from(id) != group.independents.len()
        {
            access.valid = false;
        }
        group.independents.push(Eac3Independent {
            id,
            kind: match info.stream_type {
                None => IndependentKind::LegacyAc3,
                Some(2) => IndependentKind::Converted,
                _ => IndependentKind::Eac3,
            },
            channel_mask: channel_mask(info.acmod, info.lfe),
            dependents: Vec::new(),
        });
    } else if stream_type == 1 {
        let Some(group) = access.current.as_mut() else {
            access.valid = false;
            return;
        };
        let Some(independent) = group.independents.last_mut() else {
            access.valid = false;
            return;
        };
        if independent.kind == IndependentKind::Converted
            || group.sample_rate != info.sample_rate
            || group.blocks != info.blocks
            || usize::from(id) != independent.dependents.len()
        {
            access.valid = false;
        }
        independent.dependents.push(Eac3Dependent {
            id,
            channel_mask: info
                .channel_map
                .unwrap_or_else(|| channel_mask(info.acmod, info.lfe)),
            compression_word: info.compression_word,
        });
    } else {
        access.valid = false;
    }
}

fn finalize_eac3_group(access: &mut Eac3AccessUnits) {
    let Some(group) = access.current.take() else {
        return;
    };
    access.groups += 1;
    if group.independents.is_empty() || group.independents[0].id != 0 {
        access.valid = false;
        return;
    }
    let signature: Vec<_> = group
        .independents
        .iter()
        .map(|independent| {
            (
                independent.id,
                independent.kind,
                independent
                    .dependents
                    .iter()
                    .map(|dependent| dependent.id)
                    .collect(),
            )
        })
        .collect();
    if let Some(expected) = &access.signature {
        if expected != &signature {
            access.valid = false;
        }
    } else {
        access.signature = Some(signature);
    }
    for independent in group.independents {
        if !independent.dependents.is_empty() {
            for dependent in &independent.dependents[..independent.dependents.len() - 1] {
                if dependent.compression_word.is_some() {
                    access.valid = false;
                }
            }
            if independent
                .dependents
                .last()
                .is_some_and(|dependent| dependent.compression_word.is_none())
            {
                access.valid = false;
            }
        }
        let mask = independent
            .dependents
            .iter()
            .fold(independent.channel_mask, |mask, item| {
                mask | item.channel_mask
            });
        match access.presentation_masks.entry(independent.id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(mask);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != mask {
                    access.valid = false;
                }
            }
        }
        let blocks = access.accumulated_blocks.entry(independent.id).or_default();
        *blocks = blocks.saturating_add(group.blocks);
        if *blocks > 6 {
            access.valid = false;
        }
    }
    if access.accumulated_blocks.get(&0) == Some(&6) {
        if access
            .accumulated_blocks
            .values()
            .all(|blocks| *blocks == 6)
        {
            access.complete_units += 1;
            for blocks in access.accumulated_blocks.values_mut() {
                *blocks = 0;
            }
        } else {
            access.valid = false;
        }
    }
}

fn finish_eac3_access_units(access: &mut Eac3AccessUnits) {
    finalize_eac3_group(access);
    if access
        .accumulated_blocks
        .values()
        .any(|blocks| *blocks != 0)
    {
        access.valid = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JocSignal {
    None,
    Valid(u8),
    Invalid,
}

fn joc_signal(additional_bsi: &[u8]) -> JocSignal {
    if additional_bsi.first().is_none_or(|value| value & 1 == 0) {
        return JocSignal::None;
    }
    if additional_bsi.len() != 2 || additional_bsi[0] != 1 {
        return JocSignal::Invalid;
    }
    match additional_bsi[1] {
        complexity @ 1..=16 => JocSignal::Valid(complexity),
        _ => JocSignal::Invalid,
    }
}

#[derive(Clone, Copy)]
enum GainWord {
    DynamicRange,
    Compression,
}

fn gain_db(word: u8, kind: GainWord) -> f64 {
    let (upper_bits, lower_mask, denominator) = match kind {
        GainWord::DynamicRange => (3, 0x1f, 64.0),
        GainWord::Compression => (4, 0x0f, 32.0),
    };
    let upper = word >> (8 - upper_bits);
    let sign_bit = 1_u8 << (upper_bits - 1);
    let signed = if upper & sign_bit == 0 {
        i32::from(upper)
    } else {
        i32::from(upper) - (1_i32 << upper_bits)
    };
    let fraction = (denominator / 2.0 + f64::from(word & lower_mask)) / denominator;
    let linear = 2_f64.powi(signed + 1) * fraction;
    let db = 20.0 * linear.log10();
    if db.abs() < 0.000_000_1 {
        0.0
    } else {
        (db * 1000.0).round() / 1000.0
    }
}

fn gain_range(kind: GainWord) -> [f64; 2] {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for word in 0..=u8::MAX {
        let gain = gain_db(word, kind);
        minimum = minimum.min(gain);
        maximum = maximum.max(gain);
    }
    [minimum, maximum]
}

fn interpreted_gain_words(words: &BTreeSet<u8>, kind: GainWord) -> Vec<serde_json::Value> {
    words
        .iter()
        .map(|word| {
            json!({
                "word": word,
                "hex": format!("0x{word:02x}"),
                "gain_db": gain_db(*word, kind),
            })
        })
        .collect()
}

fn channel_mask(acmod: u8, lfe: bool) -> u16 {
    const L: u16 = 1 << 15;
    const C: u16 = 1 << 14;
    const R: u16 = 1 << 13;
    const LS: u16 = 1 << 12;
    const RS: u16 = 1 << 11;
    const CS: u16 = 1 << 8;
    const LFE: u16 = 1;
    let mut mask = match acmod {
        0 | 2 => L | R,
        1 => C,
        3 => L | C | R,
        4 => L | R | CS,
        5 => L | C | R | CS,
        6 => L | R | LS | RS,
        7 => L | C | R | LS | RS,
        _ => 0,
    };
    if lfe {
        mask |= LFE;
    }
    mask
}

fn channel_map_count(mask: u16) -> u8 {
    const WEIGHTS: [u8; 16] = [1, 1, 1, 1, 1, 2, 2, 1, 1, 2, 2, 2, 1, 2, 1, 1];
    WEIGHTS
        .iter()
        .enumerate()
        .filter(|(location, _)| mask & (1 << (15 - location)) != 0)
        .map(|(_, weight)| *weight)
        .sum()
}

fn presentations(masks: &BTreeMap<u8, u16>) -> Vec<serde_json::Value> {
    masks
        .iter()
        .map(|(id, mask)| {
            json!({
                "independent_substream_id": id,
                "channels": channel_map_count(*mask),
                "channel_map": format!("0x{mask:04x}"),
            })
        })
        .collect()
}

fn parse_frame(frame: &[u8], format: Format) -> Result<FrameInfo, ()> {
    match format {
        Format::Ac3 => parse_ac3(frame),
        Format::Eac3 => parse_eac3(frame),
    }
}

fn parse_ac3(frame: &[u8]) -> Result<FrameInfo, ()> {
    let mut bits = Bits::new(frame);
    if bits.read(16)? != 0x0b77 {
        return Err(());
    }
    bits.skip(16)?;
    let fscod = bits.read(2)? as usize;
    let frmsizecod = bits.read(6)? as u8;
    let bsid = bits.read(5)? as u8;
    bits.skip(3)?;
    let acmod = bits.read(3)? as u8;
    if acmod & 1 != 0 && acmod != 1 {
        bits.skip(2)?;
    }
    if acmod & 4 != 0 {
        bits.skip(2)?;
    }
    if acmod == 2 {
        bits.skip(2)?;
    }
    let lfe = bits.read(1)? != 0;
    let dialnorm = bits.read(5)? as u8;
    let compression_word = if bits.read(1)? != 0 {
        Some(bits.read(8)? as u8)
    } else {
        None
    };
    let compression_word2 = if acmod == 0 {
        bits.skip(5)?;
        optional_byte(&mut bits)?
    } else {
        None
    };
    skip_optional(&mut bits, 8)?;
    skip_optional(&mut bits, 7)?;
    if acmod == 0 {
        skip_optional(&mut bits, 8)?;
        skip_optional(&mut bits, 7)?;
    }
    bits.skip(2)?;
    skip_optional(&mut bits, 14)?;
    skip_optional(&mut bits, 14)?;
    let additional_bsi = additional_bsi(&mut bits)?;
    if fscod >= SAMPLE_RATES.len() || frmsizecod > 37 || bsid > 10 || dialnorm == 0 {
        return Err(());
    }
    Ok(FrameInfo {
        frame_bytes: frame.len(),
        sample_rate: SAMPLE_RATES[fscod] >> bsid.saturating_sub(8),
        blocks: 6,
        bsid,
        acmod,
        lfe,
        dialnorm,
        compression_word,
        compression_word2,
        stream_type: None,
        substream_id: None,
        channel_map: None,
        additional_bsi,
    })
}

fn parse_eac3(frame: &[u8]) -> Result<FrameInfo, ()> {
    let mut bits = Bits::new(frame);
    if bits.read(16)? != 0x0b77 {
        return Err(());
    }
    let stream_type = bits.read(2)? as u8;
    let substream_id = bits.read(3)? as u8;
    let frame_size = 2 * (bits.read(11)? as usize + 1);
    let fscod = bits.read(2)? as usize;
    let (sample_rate, blocks, numblkscod) = if fscod == 3 {
        let fscod2 = bits.read(2)? as usize;
        (*HALF_SAMPLE_RATES.get(fscod2).ok_or(())?, 6, 3)
    } else {
        let code = bits.read(2)? as usize;
        let blocks = *[1_u8, 2, 3, 6].get(code).ok_or(())?;
        (*SAMPLE_RATES.get(fscod).ok_or(())?, blocks, code as u8)
    };
    let acmod = bits.read(3)? as u8;
    let lfe = bits.read(1)? != 0;
    let bsid = bits.read(5)? as u8;
    let dialnorm = bits.read(5)? as u8;
    let compression_word = if bits.read(1)? != 0 {
        Some(bits.read(8)? as u8)
    } else {
        None
    };
    let compression_word2 = if acmod == 0 {
        bits.skip(5)?;
        optional_byte(&mut bits)?
    } else {
        None
    };
    let channel_map = if stream_type == 1 && bits.read(1)? != 0 {
        Some(bits.read(16)? as u16)
    } else {
        None
    };
    skip_eac3_mixing_metadata(&mut bits, stream_type, acmod, lfe, numblkscod, blocks)?;
    skip_eac3_informational_metadata(&mut bits, acmod, fscod)?;
    if stream_type == 0 && numblkscod != 3 {
        bits.skip(1)?;
    }
    if stream_type == 2 {
        let block_id = if numblkscod == 3 {
            true
        } else {
            bits.read(1)? != 0
        };
        if block_id {
            bits.skip(6)?;
        }
    }
    let additional_bsi = additional_bsi(&mut bits)?;
    if stream_type == 3 || !(11..=16).contains(&bsid) || dialnorm == 0 || frame_size != frame.len()
    {
        return Err(());
    }
    Ok(FrameInfo {
        frame_bytes: frame.len(),
        sample_rate,
        blocks,
        bsid,
        acmod,
        lfe,
        dialnorm,
        compression_word,
        compression_word2,
        stream_type: Some(stream_type),
        substream_id: Some(substream_id),
        channel_map,
        additional_bsi,
    })
}

fn optional_byte(bits: &mut Bits<'_>) -> Result<Option<u8>, ()> {
    if bits.read(1)? != 0 {
        Ok(Some(bits.read(8)? as u8))
    } else {
        Ok(None)
    }
}

fn skip_optional(bits: &mut Bits<'_>, payload_bits: usize) -> Result<(), ()> {
    if bits.read(1)? != 0 {
        bits.skip(payload_bits)?;
    }
    Ok(())
}

fn additional_bsi(bits: &mut Bits<'_>) -> Result<Vec<u8>, ()> {
    if bits.read(1)? == 0 {
        return Ok(Vec::new());
    }
    let length = bits.read(6)? as usize + 1;
    let mut bytes = Vec::with_capacity(length);
    for _ in 0..length {
        bytes.push(bits.read(8)? as u8);
    }
    Ok(bytes)
}

fn skip_eac3_mixing_metadata(
    bits: &mut Bits<'_>,
    stream_type: u8,
    acmod: u8,
    lfe: bool,
    numblkscod: u8,
    blocks: u8,
) -> Result<(), ()> {
    if bits.read(1)? == 0 {
        return Ok(());
    }
    if acmod > 2 {
        bits.skip(2)?;
    }
    if acmod & 1 != 0 && acmod > 2 {
        bits.skip(6)?;
    }
    if acmod & 4 != 0 {
        bits.skip(6)?;
    }
    if lfe {
        skip_optional(bits, 5)?;
    }
    if stream_type == 0 {
        skip_optional(bits, 6)?;
        if acmod == 0 {
            skip_optional(bits, 6)?;
        }
        skip_optional(bits, 6)?;
        match bits.read(2)? {
            0 => {}
            1 => bits.skip(5)?,
            2 => bits.skip(12)?,
            3 => {
                let length = bits.read(5)? as usize;
                bits.skip(8 * (length + 2))?;
            }
            _ => unreachable!(),
        }
        if acmod < 2 {
            skip_optional(bits, 14)?;
            if acmod == 0 {
                skip_optional(bits, 14)?;
            }
        }
        if bits.read(1)? != 0 {
            if numblkscod == 0 {
                bits.skip(5)?;
            } else {
                for _ in 0..blocks {
                    skip_optional(bits, 5)?;
                }
            }
        }
    }
    Ok(())
}

fn skip_eac3_informational_metadata(
    bits: &mut Bits<'_>,
    acmod: u8,
    fscod: usize,
) -> Result<(), ()> {
    if bits.read(1)? == 0 {
        return Ok(());
    }
    bits.skip(5)?;
    if acmod == 2 {
        bits.skip(4)?;
    }
    if acmod >= 6 {
        bits.skip(2)?;
    }
    skip_optional(bits, 8)?;
    if acmod == 0 {
        skip_optional(bits, 8)?;
    }
    if fscod < 3 {
        bits.skip(1)?;
    }
    Ok(())
}

fn ac3_frame_size(prefix: &[u8; 8]) -> Option<usize> {
    let fscod = usize::from(prefix[4] >> 6);
    let code = usize::from(prefix[4] & 0x3f);
    let bitrate = *BITRATES_KBPS.get(code / 2)?;
    match fscod {
        0 => Some(4 * bitrate as usize),
        1 => Some(2 * ((320 * bitrate as usize / 147) + (code & 1))),
        2 => Some(6 * bitrate as usize),
        _ => None,
    }
}

fn channel_count(acmod: u8, lfe: bool) -> u8 {
    let main = [2_u8, 1, 2, 3, 3, 4, 4, 5][usize::from(acmod)];
    main + u8::from(lfe)
}

fn channel_mode(acmod: u8) -> &'static str {
    [
        "dual-mono",
        "mono",
        "stereo",
        "3/0",
        "2/1",
        "3/1",
        "2/2",
        "3/2",
    ][usize::from(acmod)]
}

fn swap_words(bytes: &mut [u8]) {
    for word in bytes.chunks_exact_mut(2) {
        word.swap(0, 1);
    }
}

struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read(&mut self, count: usize) -> Result<u32, ()> {
        if count > 32 || self.position.checked_add(count).ok_or(())? > self.bytes.len() * 8 {
            return Err(());
        }
        let mut value = 0_u32;
        for _ in 0..count {
            let byte = self.bytes[self.position / 8];
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn skip(&mut self, count: usize) -> Result<(), ()> {
        self.read(count).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BitWriter {
        bytes: Vec<u8>,
        position: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                position: 0,
            }
        }

        fn push(&mut self, value: u32, count: usize) {
            assert!(count <= 32);
            assert!(count == 32 || value < (1_u32 << count));
            for shift in (0..count).rev() {
                if self.position & 7 == 0 {
                    self.bytes.push(0);
                }
                self.bytes[self.position / 8] |=
                    (((value >> shift) & 1) as u8) << (7 - self.position % 8);
                self.position += 1;
            }
        }

        fn finish(mut self, bytes: usize) -> Vec<u8> {
            assert!(self.bytes.len() <= bytes);
            self.bytes.resize(bytes, 0);
            self.bytes
        }
    }

    fn eac3_frame(
        stream_type: u8,
        substream_id: u8,
        acmod: u8,
        lfe: bool,
        compression_word: Option<u8>,
        channel_map: Option<u16>,
        additional_bsi: &[u8],
    ) -> Vec<u8> {
        const FRAME_BYTES: usize = 64;
        let mut bits = BitWriter::new();
        bits.push(0x0b77, 16);
        bits.push(u32::from(stream_type), 2);
        bits.push(u32::from(substream_id), 3);
        bits.push((FRAME_BYTES / 2 - 1) as u32, 11);
        bits.push(0, 2); // 48 kHz
        bits.push(3, 2); // six audio blocks
        bits.push(u32::from(acmod), 3);
        bits.push(u32::from(lfe), 1);
        bits.push(16, 5);
        bits.push(24, 5);
        bits.push(u32::from(compression_word.is_some()), 1);
        if let Some(word) = compression_word {
            bits.push(u32::from(word), 8);
        }
        if acmod == 0 {
            bits.push(24, 5);
            bits.push(0, 1);
        }
        if stream_type == 1 {
            bits.push(u32::from(channel_map.is_some()), 1);
            if let Some(mask) = channel_map {
                bits.push(u32::from(mask), 16);
            }
        }
        bits.push(0, 1); // mixing metadata absent
        bits.push(0, 1); // informational metadata absent
        if stream_type == 2 {
            bits.push(0, 6); // converted syncframe size code
        }
        bits.push(u32::from(!additional_bsi.is_empty()), 1);
        if !additional_bsi.is_empty() {
            bits.push((additional_bsi.len() - 1) as u32, 6);
            for byte in additional_bsi {
                bits.push(u32::from(*byte), 8);
            }
        }
        bits.finish(FRAME_BYTES)
    }

    fn eac3_info(stream_type: u8, id: u8, blocks: u8) -> FrameInfo {
        FrameInfo {
            frame_bytes: 64,
            sample_rate: 48_000,
            blocks,
            bsid: 16,
            acmod: 7,
            lfe: true,
            dialnorm: 24,
            compression_word: (stream_type == 1).then_some(0),
            compression_word2: None,
            stream_type: Some(stream_type),
            substream_id: Some(id),
            channel_map: (stream_type == 1).then_some(1 << 9),
            additional_bsi: Vec::new(),
        }
    }

    fn audit_frames(frames: &[Vec<u8>]) -> serde_json::Value {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("programme.ac3");
        let bytes: Vec<_> = frames.iter().flatten().copied().collect();
        std::fs::write(&path, &bytes).unwrap();
        serde_json::to_value(
            audit(
                &path,
                File::open(&path).unwrap(),
                u64::try_from(bytes.len()).unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn ac3_frame_size_table_covers_48k_and_44k_parity() {
        let mut prefix = [0_u8; 8];
        prefix[4] = 0;
        assert_eq!(ac3_frame_size(&prefix), Some(128));
        prefix[4] = 0x40;
        assert_eq!(ac3_frame_size(&prefix), Some(138));
        prefix[4] = 0x41;
        assert_eq!(ac3_frame_size(&prefix), Some(140));
    }

    #[test]
    fn bit_reader_is_msb_first_and_bounded() {
        let mut bits = Bits::new(&[0b1010_0101]);
        assert_eq!(bits.read(3), Ok(5));
        assert_eq!(bits.read(5), Ok(5));
        assert_eq!(bits.read(1), Err(()));
    }

    #[test]
    fn parses_ac3_header() {
        let mut frame = vec![0_u8; 768];
        frame[..8].copy_from_slice(&[0x0b, 0x77, 0xe3, 0x2b, 0x14, 0x40, 0x2c, 0x04]);
        let info = parse_ac3(&frame).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.dialnorm, 24);

        frame[5] = 0x48;
        let info = parse_ac3(&frame).unwrap();
        assert_eq!(info.bsid, 9);
        assert_eq!(info.sample_rate, 24_000);

        frame[5] = 0x50;
        let info = parse_ac3(&frame).unwrap();
        assert_eq!(info.bsid, 10);
        assert_eq!(info.sample_rate, 12_000);
    }

    #[test]
    fn interprets_normative_drc_gain_words() {
        assert_eq!(gain_db(0, GainWord::DynamicRange), 0.0);
        assert_eq!(gain_db(0, GainWord::Compression), 0.0);
        assert_eq!(gain_db(0x80, GainWord::DynamicRange), -24.082);
        assert_eq!(gain_db(0x7f, GainWord::DynamicRange), 23.946);
        assert_eq!(gain_db(0x80, GainWord::Compression), -48.165);
        assert_eq!(gain_db(0x7f, GainWord::Compression), 47.889);
        assert_eq!(gain_range(GainWord::DynamicRange), [-24.082, 23.946]);
        assert_eq!(gain_range(GainWord::Compression), [-48.165, 47.889]);
    }

    #[test]
    fn recognizes_only_strict_joc_extension_type_a() {
        assert_eq!(joc_signal(&[]), JocSignal::None);
        assert_eq!(joc_signal(&[0]), JocSignal::None);
        assert_eq!(joc_signal(&[1, 1]), JocSignal::Valid(1));
        assert_eq!(joc_signal(&[1, 16]), JocSignal::Valid(16));
        assert_eq!(joc_signal(&[1]), JocSignal::Invalid);
        assert_eq!(joc_signal(&[3, 8]), JocSignal::Invalid);
        assert_eq!(joc_signal(&[1, 0]), JocSignal::Invalid);
        assert_eq!(joc_signal(&[1, 17]), JocSignal::Invalid);
    }

    #[test]
    fn parses_strict_joc_addbsi_from_complete_eac3_bsi() {
        let frame = eac3_frame(0, 0, 7, true, Some(0), None, &[1, 8]);
        let info = parse_eac3(&frame).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.blocks, 6);
        assert_eq!(info.additional_bsi, [1, 8]);
        assert_eq!(joc_signal(&info.additional_bsi), JocSignal::Valid(8));

        let malformed = eac3_frame(0, 0, 7, true, None, None, &[1, 17]);
        let info = parse_eac3(&malformed).unwrap();
        assert_eq!(joc_signal(&info.additional_bsi), JocSignal::Invalid);
    }

    #[test]
    fn audits_joc_signalling_on_every_primary_syncframe() {
        fn joc_check(report: &serde_json::Value) -> &serde_json::Value {
            report["layers"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|layer| layer["checks"].as_array().unwrap())
                .find(|check| check["rule_id"] == "FORGE-EAC3-ATMOS-JOC")
                .unwrap()
        }

        let joc = eac3_frame(0, 0, 7, true, Some(0), None, &[1, 8]);
        let report = audit_frames(&[joc.clone(), joc]);
        assert_eq!(report["passed"], true);
        assert_eq!(joc_check(&report)["passed"], true);
        assert_eq!(report["properties"]["atmos_joc"]["frames"], 2);

        let missing = eac3_frame(0, 0, 7, true, Some(0), None, &[]);
        let report = audit_frames(&[eac3_frame(0, 0, 7, true, Some(0), None, &[1, 8]), missing]);
        assert_eq!(report["passed"], false);
        assert_eq!(joc_check(&report)["passed"], false);

        let mono = eac3_frame(0, 0, 1, false, Some(0), None, &[1, 8]);
        let report = audit_frames(&[mono]);
        assert_eq!(joc_check(&report)["passed"], false);
    }

    #[test]
    fn groups_legacy_ac3_core_with_eac3_dependent_substream() {
        let mut core = vec![0_u8; 768];
        core[..8].copy_from_slice(&[0x0b, 0x77, 0xe3, 0x2b, 0x14, 0x40, 0x2c, 0x04]);
        let dependent = eac3_frame(1, 0, 1, false, Some(0), Some(1 << 9), &[]);
        let report = audit_frames(&[core, dependent]);
        assert_eq!(report["format"], "eac3");
        assert_eq!(report["passed"], true);
        assert_eq!(
            report["properties"]["bitstream_ids"],
            serde_json::json!([8, 16])
        );
        assert_eq!(
            report["properties"]["syncframe_formats"],
            serde_json::json!({"ac3": 1, "eac3": 1})
        );
        assert_eq!(report["properties"]["access_units"]["complete"], 1);
        assert_eq!(report["properties"]["presentations"][0]["channels"], 3);
    }

    #[test]
    fn counts_pair_locations_in_dependent_channel_maps() {
        assert_eq!(channel_map_count(1 << 15), 1);
        assert_eq!(channel_map_count(1 << 10), 2);
        assert_eq!(channel_map_count((1 << 15) | (1 << 10) | 1), 4);
    }

    #[test]
    fn groups_six_block_and_accumulated_eac3_access_units() {
        let mut access = Eac3AccessUnits::default();
        update_eac3_group(&mut access, &eac3_info(0, 0, 6));
        finish_eac3_access_units(&mut access);
        assert!(access.valid);
        assert_eq!(access.groups, 1);
        assert_eq!(access.complete_units, 1);

        let mut access = Eac3AccessUnits::default();
        for _ in 0..2 {
            update_eac3_group(&mut access, &eac3_info(0, 0, 3));
            update_eac3_group(&mut access, &eac3_info(0, 1, 3));
        }
        finish_eac3_access_units(&mut access);
        assert!(access.valid);
        assert_eq!(access.groups, 2);
        assert_eq!(access.complete_units, 1);
    }

    #[test]
    fn permits_distinct_channel_modes_for_secondary_presentations() {
        let mut state = State {
            config_valid: true,
            substreams_valid: true,
            joc_valid: true,
            ..State::default()
        };
        update_state(&mut state, &eac3_info(0, 0, 6));
        let mut secondary = eac3_info(0, 1, 6);
        secondary.acmod = 1;
        secondary.lfe = false;
        update_state(&mut state, &secondary);
        assert!(state.config_valid);
        assert_eq!(state.acmod, Some(7));
        assert_eq!(state.lfe, Some(true));
    }

    #[test]
    fn enforces_dependent_order_and_complete_mix_compression_word() {
        let mut access = Eac3AccessUnits::default();
        update_eac3_group(&mut access, &eac3_info(0, 0, 6));
        let mut first = eac3_info(1, 0, 6);
        first.compression_word = None;
        update_eac3_group(&mut access, &first);
        update_eac3_group(&mut access, &eac3_info(1, 1, 6));
        finish_eac3_access_units(&mut access);
        assert!(access.valid);
        assert_eq!(access.complete_units, 1);

        let mut invalid = Eac3AccessUnits::default();
        update_eac3_group(&mut invalid, &eac3_info(0, 0, 6));
        update_eac3_group(&mut invalid, &eac3_info(1, 0, 6));
        update_eac3_group(&mut invalid, &eac3_info(1, 1, 6));
        finish_eac3_access_units(&mut invalid);
        assert!(!invalid.valid);
    }

    #[test]
    fn rejects_missing_or_incomplete_eac3_presentations() {
        let mut out_of_order = Eac3AccessUnits::default();
        update_eac3_group(&mut out_of_order, &eac3_info(0, 1, 6));
        finish_eac3_access_units(&mut out_of_order);
        assert!(!out_of_order.valid);

        let mut incomplete = Eac3AccessUnits::default();
        update_eac3_group(&mut incomplete, &eac3_info(0, 0, 3));
        finish_eac3_access_units(&mut incomplete);
        assert!(!incomplete.valid);

        let mut unstable = Eac3AccessUnits::default();
        update_eac3_group(&mut unstable, &eac3_info(0, 0, 3));
        update_eac3_group(&mut unstable, &eac3_info(0, 1, 3));
        update_eac3_group(&mut unstable, &eac3_info(0, 0, 3));
        finish_eac3_access_units(&mut unstable);
        assert!(!unstable.valid);

        let mut switched_core = Eac3AccessUnits::default();
        update_eac3_group(&mut switched_core, &eac3_info(0, 0, 6));
        let mut legacy = eac3_info(0, 0, 6);
        legacy.bsid = 8;
        legacy.stream_type = None;
        legacy.substream_id = None;
        update_eac3_group(&mut switched_core, &legacy);
        finish_eac3_access_units(&mut switched_core);
        assert!(!switched_core.valid);
    }
}
