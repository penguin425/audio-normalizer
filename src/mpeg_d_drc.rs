//! Bounded MPEG-D DRC metadata parsing for USAC/xHE-AAC decoder configs.
//!
//! ISO/IEC 23003-4 uses bit-oriented payloads inside the USAC
//! `usacExtElementConfig` and `usacConfigExtension` syntax.  Keeping this
//! parser in-process lets container QC distinguish a syntactically valid
//! Basic DRC Metadata Profile from a pair of non-empty, opaque blobs.

use serde::Serialize;
use std::collections::{BTreeSet, HashSet};

const MAX_CHANNELS: usize = 8;
const MAX_DOWNMIX_INSTRUCTIONS: usize = 16;
const MAX_DRC_INSTRUCTIONS: usize = 36;
const MAX_GAIN_SETS: usize = 24;
const MAX_BANDS: usize = 8;
const MAX_LOUDNESS_INFO: usize = 36;
const MAX_EXTENSIONS: usize = 8;

const EFFECT_LATE_NIGHT: u16 = 0x0001;
const EFFECT_NOISY: u16 = 0x0002;
const EFFECT_LIMITED: u16 = 0x0004;
const EFFECT_GENERAL_COMPRESSION: u16 = 0x0020;
const EFFECT_DUCK_OTHER: u16 = 0x0400;
const EFFECT_DUCK_SELF: u16 = 0x0800;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct UniDrcConfig {
    pub sample_rate_hz: Option<u32>,
    pub base_channel_count: u8,
    pub defined_layout: Option<u8>,
    pub downmix_instructions: Vec<DownmixInstruction>,
    pub basic_coefficient_locations: Vec<u8>,
    pub unified_coefficients: Vec<UniDrcCoefficient>,
    pub instructions: Vec<DrcInstruction>,
    pub extension_types: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DownmixInstruction {
    pub downmix_id: u8,
    pub target_channel_count: u8,
    pub target_layout: u8,
    pub coefficients_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UniDrcCoefficient {
    pub location: u8,
    pub frame_size: Option<u16>,
    pub gain_set_band_counts: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct DrcInstruction {
    pub syntax: &'static str,
    pub drc_set_id: u8,
    pub location: u8,
    pub downmix_ids: Vec<u8>,
    pub effect_mask: u16,
    pub effect_names: Vec<&'static str>,
    pub limiter_peak_target_db: Option<f32>,
    pub target_loudness_upper_lkfs: Option<i8>,
    pub target_loudness_lower_lkfs: Option<i8>,
    pub depends_on_drc_set: Option<u8>,
    pub no_independent_use: bool,
    pub gain_set_indexes: Vec<i8>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LoudnessInfoSet {
    pub album: Vec<LoudnessInfo>,
    pub track: Vec<LoudnessInfo>,
    pub extension_types: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LoudnessInfo {
    pub drc_set_id: u8,
    pub eq_set_id: u8,
    pub downmix_id: u8,
    pub sample_peak_level_dbfs: Option<f32>,
    pub true_peak_level_dbtp: Option<f32>,
    pub true_peak_measurement_system: Option<u8>,
    pub true_peak_reliability: Option<u8>,
    pub measurements: Vec<LoudnessMeasurement>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LoudnessMeasurement {
    pub method_definition: u8,
    pub method_name: &'static str,
    pub value: f32,
    pub measurement_system: u8,
    pub measurement_system_name: &'static str,
    pub reliability: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AppleDrcEffectCheck {
    pub effect: &'static str,
    pub effect_bit: u16,
    pub minimum_target_lkfs: i8,
    pub present: bool,
    pub target_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AppleBasicDrcProfile {
    pub compliant: bool,
    pub anchor_loudness_bs1770_present: bool,
    pub sample_or_bs1770_true_peak_present: bool,
    pub required_effects: Vec<AppleDrcEffectCheck>,
    pub failures: Vec<String>,
}

#[derive(Clone)]
struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            end: data.len().saturating_mul(8),
        }
    }

    fn read(&mut self, count: usize) -> Result<u64, String> {
        if count > 64 || self.remaining() < count {
            return Err("truncated MPEG-D DRC bit syntax".into());
        }
        let mut value = 0_u64;
        for _ in 0..count {
            let byte = self.data[self.position / 8];
            let shift = 7 - self.position % 8;
            value = (value << 1) | u64::from((byte >> shift) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn bit(&mut self) -> Result<bool, String> {
        Ok(self.read(1)? != 0)
    }

    fn skip(&mut self, count: usize) -> Result<(), String> {
        if self.remaining() < count {
            return Err("MPEG-D DRC field exceeds its declared payload".into());
        }
        self.position += count;
        Ok(())
    }

    fn take(&mut self, count: usize) -> Result<Self, String> {
        if self.remaining() < count {
            return Err("MPEG-D DRC extension exceeds its declared payload".into());
        }
        let start = self.position;
        self.position += count;
        Ok(Self {
            data: self.data,
            position: start,
            end: start + count,
        })
    }

    fn remaining(&self) -> usize {
        self.end - self.position
    }

    fn finish_byte_padded(&mut self, label: &str) -> Result<(), String> {
        if self.remaining() > 7 {
            return Err(format!(
                "{label} contains more than one byte of trailing padding"
            ));
        }
        while self.remaining() > 0 {
            if self.bit()? {
                return Err(format!("{label} contains non-zero trailing padding"));
            }
        }
        Ok(())
    }
}

pub(crate) fn parse_uni_drc_config(
    payload: &[u8],
    expected_sample_rate_hz: u32,
    expected_channels: u16,
    expected_frame_samples: u16,
) -> Result<UniDrcConfig, String> {
    if payload.is_empty() {
        return Err("empty uniDrcConfig payload".into());
    }
    let mut bits = BitReader::new(payload);
    let sample_rate_hz = if bits.bit()? {
        let sample_rate = u32::try_from(bits.read(18)?).unwrap() + 1_000;
        if sample_rate != expected_sample_rate_hz {
            return Err(format!(
                "uniDrcConfig sample rate {sample_rate} does not match USAC output rate {expected_sample_rate_hz}"
            ));
        }
        Some(sample_rate)
    } else {
        None
    };

    let downmix_count = usize::try_from(bits.read(7)?).unwrap();
    if downmix_count > MAX_DOWNMIX_INSTRUCTIONS {
        return Err("uniDrcConfig exceeds 16 downmix instructions".into());
    }
    let basic_present = bits.bit()?;
    let (basic_coefficient_count, basic_instruction_count) = if basic_present {
        (
            usize::try_from(bits.read(3)?).unwrap(),
            usize::try_from(bits.read(4)?).unwrap(),
        )
    } else {
        (0, 0)
    };
    let unified_coefficient_count = usize::try_from(bits.read(3)?).unwrap();
    let unified_instruction_count = usize::try_from(bits.read(6)?).unwrap();
    if basic_instruction_count + unified_instruction_count > MAX_DRC_INSTRUCTIONS {
        return Err("uniDrcConfig exceeds 36 DRC instructions".into());
    }

    let base_channel_count = u8::try_from(bits.read(7)?).unwrap();
    if base_channel_count == 0 || usize::from(base_channel_count) > MAX_CHANNELS {
        return Err("uniDrcConfig base channel count is outside 1..=8".into());
    }
    if u16::from(base_channel_count) != expected_channels {
        return Err(format!(
            "uniDrcConfig describes {base_channel_count} channels but USAC describes {expected_channels}"
        ));
    }
    let defined_layout = if bits.bit()? {
        let layout = u8::try_from(bits.read(8)?).unwrap();
        if layout == 0 {
            for _ in 0..base_channel_count {
                bits.read(7)?;
            }
        }
        Some(layout)
    } else {
        None
    };

    let mut downmix_instructions = Vec::with_capacity(downmix_count);
    let mut downmix_ids = HashSet::new();
    for _ in 0..downmix_count {
        let downmix_id = u8::try_from(bits.read(7)?).unwrap();
        let target_channel_count = u8::try_from(bits.read(7)?).unwrap();
        let target_layout = u8::try_from(bits.read(8)?).unwrap();
        let coefficients_present = bits.bit()?;
        if downmix_id == 0 || downmix_id == 0x7f || !downmix_ids.insert(downmix_id) {
            return Err("uniDrcConfig has a reserved or duplicate downmix ID".into());
        }
        if target_channel_count == 0 || usize::from(target_channel_count) > MAX_CHANNELS {
            return Err("uniDrcConfig downmix target channel count is outside 1..=8".into());
        }
        if coefficients_present {
            let coefficient_count = usize::from(target_channel_count)
                .checked_mul(usize::from(base_channel_count))
                .ok_or("uniDrcConfig downmix coefficient count overflow")?;
            bits.skip(coefficient_count * 4)?;
        }
        downmix_instructions.push(DownmixInstruction {
            downmix_id,
            target_channel_count,
            target_layout,
            coefficients_present,
        });
    }

    let mut basic_coefficient_locations = Vec::with_capacity(basic_coefficient_count);
    for _ in 0..basic_coefficient_count {
        basic_coefficient_locations.push(u8::try_from(bits.read(4)?).unwrap());
        bits.read(7)?;
    }

    let mut instructions = Vec::with_capacity(basic_instruction_count + unified_instruction_count);
    for _ in 0..basic_instruction_count {
        instructions.push(parse_basic_instruction(&mut bits)?);
    }

    let mut unified_coefficients = Vec::with_capacity(unified_coefficient_count);
    for _ in 0..unified_coefficient_count {
        unified_coefficients.push(parse_unified_coefficient(
            &mut bits,
            expected_frame_samples,
        )?);
    }

    let mut referenced_gain_sets = Vec::new();
    for _ in 0..unified_instruction_count {
        let (instruction, references) = parse_unified_instruction(
            &mut bits,
            base_channel_count,
            &downmix_instructions,
            &unified_coefficients,
        )?;
        instructions.push(instruction);
        referenced_gain_sets.extend(references);
    }

    let extension_types = if bits.bit()? {
        parse_bounded_extensions(&mut bits)?
    } else {
        Vec::new()
    };
    bits.finish_byte_padded("uniDrcConfig")?;

    validate_instruction_ids_and_references(
        &instructions,
        &downmix_instructions,
        &basic_coefficient_locations,
    )?;

    // Parametric DRC (extension type 1) can add virtual gain sets after the
    // core syntax.  Core-only references can be checked completely here.
    if !extension_types.contains(&1) {
        for (location, gain_set_index) in referenced_gain_sets {
            let coefficient = unified_coefficients
                .iter()
                .rev()
                .find(|item| item.location == location)
                .ok_or_else(|| {
                    format!(
                        "DRC instruction references gain set {gain_set_index} at missing coefficient location {location}"
                    )
                })?;
            if usize::from(gain_set_index) >= coefficient.gain_set_band_counts.len() {
                return Err(format!(
                    "DRC instruction references unavailable gain set {gain_set_index} at location {location}"
                ));
            }
        }
    }

    Ok(UniDrcConfig {
        sample_rate_hz,
        base_channel_count,
        defined_layout,
        downmix_instructions,
        basic_coefficient_locations,
        unified_coefficients,
        instructions,
        extension_types,
    })
}

fn parse_basic_instruction(bits: &mut BitReader<'_>) -> Result<DrcInstruction, String> {
    let drc_set_id = u8::try_from(bits.read(6)?).unwrap();
    let location = u8::try_from(bits.read(4)?).unwrap();
    let downmix_ids = parse_downmix_ids(bits)?;
    let effect_mask = u16::try_from(bits.read(16)?).unwrap();
    let limiter_peak_target_db = parse_limiter(bits, effect_mask)?;
    let (target_loudness_upper_lkfs, target_loudness_lower_lkfs) = parse_target_loudness(bits)?;
    Ok(DrcInstruction {
        syntax: "basic",
        drc_set_id,
        location,
        downmix_ids,
        effect_mask,
        effect_names: effect_names(effect_mask),
        limiter_peak_target_db,
        target_loudness_upper_lkfs,
        target_loudness_lower_lkfs,
        depends_on_drc_set: None,
        no_independent_use: false,
        gain_set_indexes: Vec::new(),
    })
}

fn parse_unified_coefficient(
    bits: &mut BitReader<'_>,
    expected_frame_samples: u16,
) -> Result<UniDrcCoefficient, String> {
    let location = u8::try_from(bits.read(4)?).unwrap();
    let frame_size = if bits.bit()? {
        let value = u16::try_from(bits.read(15)?).unwrap() + 1;
        if value > 4_096 {
            return Err("UniDRC frame size exceeds 4096 samples".into());
        }
        Some(value)
    } else {
        None
    };
    let gain_set_count = usize::try_from(bits.read(6)?).unwrap();
    if gain_set_count > MAX_GAIN_SETS {
        return Err("UniDRC coefficient exceeds 24 gain sets".into());
    }
    let effective_frame_size = frame_size.unwrap_or(expected_frame_samples);
    let mut gain_set_band_counts = Vec::with_capacity(gain_set_count);
    let mut gain_sequence_count = 0_usize;
    for _ in 0..gain_set_count {
        let gain_coding_profile = u8::try_from(bits.read(2)?).unwrap();
        bits.read(1)?; // gainInterpolationType
        bits.read(1)?; // fullFrame
        bits.read(1)?; // timeAlignment
        let time_delta_min_present = bits.bit()?;
        if time_delta_min_present {
            let time_delta_min = u16::try_from(bits.read(11)?).unwrap() + 1;
            if time_delta_min > effective_frame_size {
                return Err("UniDRC timeDeltaMin exceeds the DRC frame size".into());
            }
        }
        let band_count = if gain_coding_profile == 3 {
            1
        } else {
            let count = u8::try_from(bits.read(4)?).unwrap();
            if count == 0 || usize::from(count) > MAX_BANDS {
                return Err("UniDRC band count is outside 1..=8".into());
            }
            let band_type = count > 1 && bits.bit()?;
            for _ in 0..count {
                bits.read(7)?; // drcCharacteristic
            }
            if band_type {
                for _ in 1..count {
                    bits.read(4)?;
                }
            } else {
                for _ in 1..count {
                    bits.read(10)?;
                }
            }
            count
        };
        gain_sequence_count += usize::from(band_count);
        if gain_sequence_count > MAX_GAIN_SETS {
            return Err("UniDRC coefficient exceeds 24 gain sequences".into());
        }
        gain_set_band_counts.push(band_count);
    }
    Ok(UniDrcCoefficient {
        location,
        frame_size,
        gain_set_band_counts,
    })
}

fn parse_unified_instruction(
    bits: &mut BitReader<'_>,
    base_channel_count: u8,
    downmixes: &[DownmixInstruction],
    coefficients: &[UniDrcCoefficient],
) -> Result<(DrcInstruction, Vec<(u8, u8)>), String> {
    let drc_set_id = u8::try_from(bits.read(6)?).unwrap();
    let location = u8::try_from(bits.read(4)?).unwrap();
    let downmix_ids = parse_downmix_ids(bits)?;
    let effect_mask = u16::try_from(bits.read(16)?).unwrap();
    if effect_mask & EFFECT_DUCK_OTHER != 0 && effect_mask & EFFECT_DUCK_SELF != 0 {
        return Err("UniDRC instruction sets both duck-other and duck-self".into());
    }
    let limiter_peak_target_db = parse_limiter(bits, effect_mask)?;
    let (target_loudness_upper_lkfs, target_loudness_lower_lkfs) = parse_target_loudness(bits)?;
    let depends_on_drc_set = bits
        .bit()?
        .then(|| {
            bits.read(6)
                .and_then(|value| u8::try_from(value).map_err(|_| "invalid DRC set ID".into()))
        })
        .transpose()?;
    let no_independent_use = if depends_on_drc_set.is_none() {
        bits.bit()?
    } else {
        false
    };

    let ducking = effect_mask & (EFFECT_DUCK_OTHER | EFFECT_DUCK_SELF) != 0;
    let channel_count = if ducking {
        usize::from(base_channel_count)
    } else {
        instruction_channel_count(&downmix_ids, base_channel_count, downmixes)?
    };
    let mut gain_set_indexes = Vec::with_capacity(channel_count);
    if ducking {
        while gain_set_indexes.len() < channel_count {
            let raw = u8::try_from(bits.read(6)?).unwrap();
            let index = i8::try_from(raw).unwrap() - 1;
            if bits.bit()? {
                bits.read(4)?;
            }
            gain_set_indexes.push(index);
            let repeats = if bits.bit()? {
                usize::try_from(bits.read(5)?).unwrap() + 1
            } else {
                0
            };
            if gain_set_indexes.len() + repeats > channel_count {
                return Err("UniDRC ducking repeat exceeds the channel count".into());
            }
            gain_set_indexes.extend(std::iter::repeat_n(index, repeats));
        }
    } else {
        while gain_set_indexes.len() < channel_count {
            let value = bits.read(7)?;
            let index = i8::try_from(value >> 1).unwrap() - 1;
            gain_set_indexes.push(index);
            let repeats = if value & 1 != 0 {
                usize::try_from(bits.read(5)?).unwrap() + 1
            } else {
                0
            };
            if gain_set_indexes.len() + repeats > channel_count {
                return Err("UniDRC gain-set repeat exceeds the channel count".into());
            }
            gain_set_indexes.extend(std::iter::repeat_n(index, repeats));
        }
    }

    let mut referenced_gain_sets = Vec::new();
    if !ducking {
        let unique = gain_set_indexes
            .iter()
            .copied()
            .filter(|value| *value >= 0)
            .collect::<BTreeSet<_>>();
        let coefficient = coefficients
            .iter()
            .rev()
            .find(|item| item.location == location);
        for index in unique {
            let index = u8::try_from(index).unwrap();
            referenced_gain_sets.push((location, index));
            let band_count = coefficient
                .and_then(|item| item.gain_set_band_counts.get(usize::from(index)))
                .copied()
                .unwrap_or(1);
            parse_gain_modifier_v0(bits, band_count)?;
        }
    }

    Ok((
        DrcInstruction {
            syntax: "unified",
            drc_set_id,
            location,
            downmix_ids,
            effect_mask,
            effect_names: effect_names(effect_mask),
            limiter_peak_target_db,
            target_loudness_upper_lkfs,
            target_loudness_lower_lkfs,
            depends_on_drc_set,
            no_independent_use,
            gain_set_indexes,
        },
        referenced_gain_sets,
    ))
}

fn parse_downmix_ids(bits: &mut BitReader<'_>) -> Result<Vec<u8>, String> {
    let mut ids = vec![u8::try_from(bits.read(7)?).unwrap()];
    if bits.bit()? {
        let additional = usize::try_from(bits.read(3)?).unwrap();
        for _ in 0..additional {
            ids.push(u8::try_from(bits.read(7)?).unwrap());
        }
    }
    if ids.iter().copied().collect::<HashSet<_>>().len() != ids.len() {
        return Err("DRC instruction contains duplicate downmix IDs".into());
    }
    Ok(ids)
}

fn parse_limiter(bits: &mut BitReader<'_>, effect_mask: u16) -> Result<Option<f32>, String> {
    if effect_mask & (EFFECT_DUCK_OTHER | EFFECT_DUCK_SELF) != 0 || !bits.bit()? {
        return Ok(None);
    }
    Ok(Some(-(bits.read(8)? as f32) * 0.125))
}

fn parse_target_loudness(bits: &mut BitReader<'_>) -> Result<(Option<i8>, Option<i8>), String> {
    if !bits.bit()? {
        return Ok((None, None));
    }
    let upper = i8::try_from(bits.read(6)?).unwrap() - 63;
    let lower = if bits.bit()? {
        Some(i8::try_from(bits.read(6)?).unwrap() - 63)
    } else {
        None
    };
    if lower.is_some_and(|value| value > upper) {
        return Err("DRC target-loudness lower bound exceeds its upper bound".into());
    }
    Ok((Some(upper), lower))
}

fn parse_gain_modifier_v0(bits: &mut BitReader<'_>, _band_count: u8) -> Result<(), String> {
    if bits.bit()? {
        bits.read(8)?;
    }
    if bits.bit()? {
        bits.read(6)?;
    }
    Ok(())
}

fn instruction_channel_count(
    downmix_ids: &[u8],
    base_channel_count: u8,
    downmixes: &[DownmixInstruction],
) -> Result<usize, String> {
    let first = downmix_ids[0];
    if first == 0 {
        return Ok(usize::from(base_channel_count));
    }
    if first == 0x7f || downmix_ids.len() > 1 {
        return Ok(1);
    }
    downmixes
        .iter()
        .find(|item| item.downmix_id == first)
        .map(|item| usize::from(item.target_channel_count))
        .ok_or_else(|| format!("DRC instruction references missing downmix ID {first}"))
}

fn validate_instruction_ids_and_references(
    instructions: &[DrcInstruction],
    downmixes: &[DownmixInstruction],
    basic_coefficient_locations: &[u8],
) -> Result<(), String> {
    let mut set_ids = HashSet::new();
    for instruction in instructions {
        if instruction.drc_set_id == 0 || !set_ids.insert(instruction.drc_set_id) {
            return Err("uniDrcConfig has a reserved or duplicate DRC set ID".into());
        }
        validate_downmix_references(&instruction.downmix_ids, downmixes)?;
        if instruction.syntax == "basic"
            && !basic_coefficient_locations.contains(&instruction.location)
        {
            return Err(format!(
                "basic DRC instruction references missing coefficient location {}",
                instruction.location
            ));
        }
    }
    for instruction in instructions {
        if let Some(dependency) = instruction.depends_on_drc_set {
            if dependency == instruction.drc_set_id || !set_ids.contains(&dependency) {
                return Err(format!(
                    "DRC set {} has an invalid dependency on set {dependency}",
                    instruction.drc_set_id
                ));
            }
        }
    }
    Ok(())
}

fn validate_downmix_references(ids: &[u8], downmixes: &[DownmixInstruction]) -> Result<(), String> {
    for id in ids {
        if *id != 0 && *id != 0x7f && !downmixes.iter().any(|item| item.downmix_id == *id) {
            return Err(format!("metadata references missing downmix ID {id}"));
        }
    }
    Ok(())
}

fn parse_bounded_extensions(bits: &mut BitReader<'_>) -> Result<Vec<u8>, String> {
    let mut types = Vec::new();
    loop {
        let extension_type = u8::try_from(bits.read(4)?).unwrap();
        if extension_type == 0 {
            break;
        }
        if types.len() >= MAX_EXTENSIONS {
            return Err("MPEG-D DRC payload exceeds eight extensions".into());
        }
        let size_width = usize::try_from(bits.read(4)?).unwrap() + 4;
        let bit_count = usize::try_from(bits.read(size_width)?).unwrap() + 1;
        let mut extension = bits.take(bit_count)?;
        extension.skip(extension.remaining())?;
        types.push(extension_type);
    }
    Ok(types)
}

pub(crate) fn parse_loudness_info_set(payload: &[u8]) -> Result<LoudnessInfoSet, String> {
    if payload.is_empty() {
        return Err("empty loudnessInfoSet payload".into());
    }
    let mut bits = BitReader::new(payload);
    let album_count = usize::try_from(bits.read(6)?).unwrap();
    let track_count = usize::try_from(bits.read(6)?).unwrap();
    if album_count > MAX_LOUDNESS_INFO || track_count > MAX_LOUDNESS_INFO {
        return Err("loudnessInfoSet exceeds 36 entries".into());
    }
    let mut album = Vec::with_capacity(album_count);
    let mut track = Vec::with_capacity(track_count);
    for _ in 0..album_count {
        album.push(parse_loudness_info(&mut bits, false)?);
    }
    for _ in 0..track_count {
        track.push(parse_loudness_info(&mut bits, false)?);
    }

    let mut extension_types = Vec::new();
    if bits.bit()? {
        loop {
            let extension_type = u8::try_from(bits.read(4)?).unwrap();
            if extension_type == 0 {
                break;
            }
            if extension_types.len() >= MAX_EXTENSIONS {
                return Err("loudnessInfoSet exceeds eight extensions".into());
            }
            let size_width = usize::try_from(bits.read(4)?).unwrap() + 4;
            let bit_count = usize::try_from(bits.read(size_width)?).unwrap() + 1;
            let mut extension = bits.take(bit_count)?;
            if extension_type == 1 {
                let album_v1_count = usize::try_from(extension.read(6)?).unwrap();
                let track_v1_count = usize::try_from(extension.read(6)?).unwrap();
                if album.len() + album_v1_count > MAX_LOUDNESS_INFO
                    || track.len() + track_v1_count > MAX_LOUDNESS_INFO
                {
                    return Err("extended loudnessInfoSet exceeds 36 entries".into());
                }
                for _ in 0..album_v1_count {
                    album.push(parse_loudness_info(&mut extension, true)?);
                }
                for _ in 0..track_v1_count {
                    track.push(parse_loudness_info(&mut extension, true)?);
                }
                if extension.remaining() != 0 {
                    return Err("EQ loudnessInfoSet extension has trailing bits".into());
                }
            } else {
                extension.skip(extension.remaining())?;
            }
            extension_types.push(extension_type);
        }
    }
    bits.finish_byte_padded("loudnessInfoSet")?;
    validate_unique_loudness_entries(&album, &track)?;
    Ok(LoudnessInfoSet {
        album,
        track,
        extension_types,
    })
}

fn parse_loudness_info(
    bits: &mut BitReader<'_>,
    version_one: bool,
) -> Result<LoudnessInfo, String> {
    let drc_set_id = u8::try_from(bits.read(6)?).unwrap();
    let eq_set_id = if version_one {
        u8::try_from(bits.read(6)?).unwrap()
    } else {
        0
    };
    let downmix_id = u8::try_from(bits.read(7)?).unwrap();
    let sample_peak_level_dbfs = if bits.bit()? {
        decode_peak(bits.read(12)?)
    } else {
        None
    };
    let (true_peak_level_dbtp, true_peak_measurement_system, true_peak_reliability) =
        if bits.bit()? {
            let peak = decode_peak(bits.read(12)?);
            let system = u8::try_from(bits.read(4)?).unwrap();
            let reliability = u8::try_from(bits.read(2)?).unwrap();
            if system > 11 {
                return Err("true-peak measurement system is reserved".into());
            }
            (peak, Some(system), Some(reliability))
        } else {
            (None, None, None)
        };
    let measurement_count = usize::try_from(bits.read(4)?).unwrap();
    let mut measurements = Vec::with_capacity(measurement_count);
    for _ in 0..measurement_count {
        let method_definition = u8::try_from(bits.read(4)?).unwrap();
        let value = decode_method_value(bits, method_definition)?;
        let measurement_system = u8::try_from(bits.read(4)?).unwrap();
        let reliability = u8::try_from(bits.read(2)?).unwrap();
        if measurement_system > 11 {
            return Err("loudness measurement system is reserved".into());
        }
        measurements.push(LoudnessMeasurement {
            method_definition,
            method_name: method_name(method_definition),
            value,
            measurement_system,
            measurement_system_name: measurement_system_name(measurement_system),
            reliability,
        });
    }
    Ok(LoudnessInfo {
        drc_set_id,
        eq_set_id,
        downmix_id,
        sample_peak_level_dbfs,
        true_peak_level_dbtp,
        true_peak_measurement_system,
        true_peak_reliability,
        measurements,
    })
}

fn decode_peak(raw: u64) -> Option<f32> {
    (raw != 0).then_some(20.0 - raw as f32 * 0.03125)
}

fn decode_method_value(bits: &mut BitReader<'_>, method: u8) -> Result<f32, String> {
    Ok(match method {
        0..=5 => -57.75 + bits.read(8)? as f32 * 0.25,
        6 => {
            let raw = bits.read(8)? as u8;
            match raw {
                0 => 0.0,
                1..=128 => f32::from(raw) * 0.25,
                129..=204 => f32::from(raw) * 0.5 - 32.0,
                _ => f32::from(raw) - 134.0,
            }
        }
        7 => 80.0 + bits.read(5)? as f32,
        8 => bits.read(2)? as f32,
        9 => -116.0 + bits.read(8)? as f32 * 0.5,
        _ => return Err(format!("reserved loudness method definition {method}")),
    })
}

fn validate_unique_loudness_entries(
    album: &[LoudnessInfo],
    track: &[LoudnessInfo],
) -> Result<(), String> {
    for (label, entries) in [("album", album), ("track", track)] {
        let mut keys = HashSet::new();
        for entry in entries {
            if !keys.insert((entry.drc_set_id, entry.eq_set_id, entry.downmix_id)) {
                return Err(format!("duplicate {label} loudnessInfo key"));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_metadata_pair(
    config: &UniDrcConfig,
    loudness: &LoudnessInfoSet,
) -> Result<(), String> {
    let set_ids = config
        .instructions
        .iter()
        .map(|instruction| instruction.drc_set_id)
        .collect::<HashSet<_>>();
    for entry in loudness.album.iter().chain(&loudness.track) {
        if entry.drc_set_id != 0 && !set_ids.contains(&entry.drc_set_id) {
            return Err(format!(
                "loudnessInfo references missing DRC set ID {}",
                entry.drc_set_id
            ));
        }
        validate_downmix_references(&[entry.downmix_id], &config.downmix_instructions)?;
    }
    Ok(())
}

pub(crate) fn evaluate_apple_basic_profile(
    config: Option<&UniDrcConfig>,
    loudness: Option<&LoudnessInfoSet>,
) -> AppleBasicDrcProfile {
    let base_entries = loudness
        .into_iter()
        .flat_map(|set| set.track.iter())
        .filter(|entry| entry.drc_set_id == 0 && entry.eq_set_id == 0 && entry.downmix_id == 0)
        .collect::<Vec<_>>();
    let anchor_loudness_bs1770_present = base_entries.iter().any(|entry| {
        entry.measurements.iter().any(|measurement| {
            measurement.method_definition == 2 && measurement.measurement_system == 2
        })
    });
    let sample_or_bs1770_true_peak_present = base_entries.iter().any(|entry| {
        entry.sample_peak_level_dbfs.is_some()
            || (entry.true_peak_level_dbtp.is_some()
                && entry.true_peak_measurement_system == Some(2))
    });

    let required = [
        ("late_night", EFFECT_LATE_NIGHT, -24),
        ("noisy_environment", EFFECT_NOISY, -16),
        ("limited_playback_range", EFFECT_LIMITED, -16),
        ("general_compression", EFFECT_GENERAL_COMPRESSION, -24),
    ];
    let required_effects = required
        .into_iter()
        .map(|(effect, effect_bit, target)| {
            let matching = config
                .into_iter()
                .flat_map(|item| item.instructions.iter())
                .filter(|instruction| instruction.effect_mask & effect_bit != 0)
                .collect::<Vec<_>>();
            let present = !matching.is_empty();
            let target_supported = matching.iter().any(|instruction| {
                instruction.target_loudness_upper_lkfs.is_none()
                    || (instruction.target_loudness_lower_lkfs.unwrap_or(-63) <= target
                        && target <= instruction.target_loudness_upper_lkfs.unwrap())
            });
            AppleDrcEffectCheck {
                effect,
                effect_bit,
                minimum_target_lkfs: target,
                present,
                target_supported,
            }
        })
        .collect::<Vec<_>>();

    let mut failures = Vec::new();
    if config.is_none() {
        failures.push("uniDrcConfig is missing".into());
    }
    if loudness.is_none() {
        failures.push("loudnessInfoSet is missing".into());
    }
    if !anchor_loudness_bs1770_present {
        failures.push("base-layout Anchor Loudness measured with ITU-R BS.1770 is missing".into());
    }
    if !sample_or_bs1770_true_peak_present {
        failures.push("base-layout sample peak or ITU-R BS.1770 true peak is missing".into());
    }
    for effect in &required_effects {
        if !effect.present {
            failures.push(format!("required {} DRC effect is missing", effect.effect));
        } else if !effect.target_supported {
            failures.push(format!(
                "{} DRC effect does not support {} LKFS",
                effect.effect, effect.minimum_target_lkfs
            ));
        }
    }
    AppleBasicDrcProfile {
        compliant: failures.is_empty(),
        anchor_loudness_bs1770_present,
        sample_or_bs1770_true_peak_present,
        required_effects,
        failures,
    }
}

fn effect_names(mask: u16) -> Vec<&'static str> {
    [
        (0x0001, "late_night"),
        (0x0002, "noisy_environment"),
        (0x0004, "limited_playback_range"),
        (0x0008, "low_level"),
        (0x0010, "dialogue_enhancement"),
        (0x0020, "general_compression"),
        (0x0040, "expand"),
        (0x0080, "artistic"),
        (0x0100, "clipping"),
        (0x0200, "fade"),
        (0x0400, "duck_other"),
        (0x0800, "duck_self"),
    ]
    .into_iter()
    .filter_map(|(bit, name)| (mask & bit != 0).then_some(name))
    .collect()
}

fn method_name(method: u8) -> &'static str {
    match method {
        0 => "unknown_or_other",
        1 => "program_loudness",
        2 => "anchor_loudness",
        3 => "maximum_loudness_range",
        4 => "maximum_momentary_loudness",
        5 => "maximum_short_term_loudness",
        6 => "loudness_range",
        7 => "mixing_level",
        8 => "room_type",
        9 => "short_term_loudness",
        _ => "reserved",
    }
}

fn measurement_system_name(system: u8) -> &'static str {
    match system {
        0 => "unknown",
        1 => "EBU_R128",
        2 => "ITU_R_BS_1770",
        3 => "ITU_R_BS_1770_pre_processing",
        4 => "user",
        5 => "expert_panel",
        6 => "ITU_R_BS_1771",
        7 => "reserved_a",
        8 => "reserved_b",
        9 => "reserved_c",
        10 => "reserved_d",
        11 => "reserved_e",
        _ => "invalid",
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        position: usize,
    }

    impl Writer {
        fn write(&mut self, value: u64, width: usize) {
            for shift in (0..width).rev() {
                if self.position.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let index = self.position / 8;
                self.bytes[index] |= (((value >> shift) & 1) as u8) << (7 - self.position % 8);
                self.position += 1;
            }
        }
    }

    pub(crate) fn apple_basic_payloads() -> (Vec<u8>, Vec<u8>) {
        let mut drc = Writer::default();
        drc.write(1, 1); // sampleRatePresent
        drc.write(47_000, 18); // 48 kHz minus 1000
        drc.write(0, 7); // downmixInstructionCount
        drc.write(1, 1); // drcDescriptionBasicPresent
        drc.write(1, 3); // one basic coefficient
        drc.write(4, 4); // four basic instructions
        drc.write(0, 3); // no unified coefficients
        drc.write(0, 6); // no unified instructions
        drc.write(2, 7); // stereo base layout
        drc.write(1, 1); // layout signaling
        drc.write(2, 8); // CICP stereo layout
        drc.write(0, 4); // coefficient location
        drc.write(1, 7); // characteristic
        for (set_id, effect, target) in [
            (1_u64, 0x0001_u64, -24_i8),
            (2, 0x0002, -16),
            (3, 0x0004, -16),
            (4, 0x0020, -24),
        ] {
            drc.write(set_id, 6);
            drc.write(0, 4); // location
            drc.write(0, 7); // base layout
            drc.write(0, 1); // no additional downmix IDs
            drc.write(effect, 16);
            drc.write(0, 1); // no limiter target
            drc.write(1, 1); // target loudness present
            drc.write((target + 63) as u64, 6);
            drc.write(0, 1); // no lower bound
        }
        drc.write(0, 1); // no config extension

        let mut loudness = Writer::default();
        loudness.write(0, 6); // album count
        loudness.write(1, 6); // track count
        loudness.write(0, 6); // unprocessed DRC set
        loudness.write(0, 7); // base layout
        loudness.write(1, 1); // sample peak present
        loudness.write(672, 12); // -1 dBFS
        loudness.write(1, 1); // true peak present
        loudness.write(672, 12); // -1 dBTP
        loudness.write(2, 4); // ITU-R BS.1770
        loudness.write(3, 2); // accurate reliability
        loudness.write(2, 4); // two measurements
        for method in [1_u64, 2] {
            loudness.write(method, 4);
            loudness.write(135, 8); // -24 LKFS
            loudness.write(2, 4); // ITU-R BS.1770
            loudness.write(3, 2);
        }
        loudness.write(0, 1); // no loudnessInfoSet extension
        (drc.bytes, loudness.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_evaluates_apple_basic_metadata() {
        let (drc_bytes, loudness_bytes) = test_support::apple_basic_payloads();
        let drc = parse_uni_drc_config(&drc_bytes, 48_000, 2, 1_024).unwrap();
        let loudness = parse_loudness_info_set(&loudness_bytes).unwrap();
        validate_metadata_pair(&drc, &loudness).unwrap();
        let profile = evaluate_apple_basic_profile(Some(&drc), Some(&loudness));
        assert!(profile.compliant, "{profile:#?}");
        assert_eq!(drc.instructions.len(), 4);
        assert_eq!(loudness.track[0].true_peak_level_dbtp, Some(-1.0));
    }

    #[test]
    fn rejects_truncated_payloads_and_missing_profile_parts() {
        let (drc_bytes, loudness_bytes) = test_support::apple_basic_payloads();
        for end in 0..drc_bytes.len() {
            assert!(parse_uni_drc_config(&drc_bytes[..end], 48_000, 2, 1_024).is_err());
        }
        for end in 0..loudness_bytes.len() {
            assert!(parse_loudness_info_set(&loudness_bytes[..end]).is_err());
        }
        let drc = parse_uni_drc_config(&drc_bytes, 48_000, 2, 1_024).unwrap();
        let profile = evaluate_apple_basic_profile(Some(&drc), None);
        assert!(!profile.compliant);
        assert!(!profile.anchor_loudness_bs1770_present);
    }

    #[test]
    fn reports_each_apple_profile_requirement_and_cross_reference_failure() {
        let (drc_bytes, loudness_bytes) = test_support::apple_basic_payloads();
        let mut drc = parse_uni_drc_config(&drc_bytes, 48_000, 2, 1_024).unwrap();
        let mut loudness = parse_loudness_info_set(&loudness_bytes).unwrap();

        drc.instructions
            .retain(|instruction| instruction.effect_mask != EFFECT_NOISY);
        let profile = evaluate_apple_basic_profile(Some(&drc), Some(&loudness));
        assert!(!profile.compliant);
        assert!(profile
            .failures
            .iter()
            .any(|failure| failure.contains("noisy_environment")));

        loudness.track[0]
            .measurements
            .iter_mut()
            .find(|measurement| measurement.method_definition == 2)
            .unwrap()
            .measurement_system = 1;
        loudness.track[0].sample_peak_level_dbfs = None;
        loudness.track[0].true_peak_measurement_system = Some(5);
        let profile = evaluate_apple_basic_profile(Some(&drc), Some(&loudness));
        assert!(!profile.anchor_loudness_bs1770_present);
        assert!(!profile.sample_or_bs1770_true_peak_present);

        loudness.track[0].drc_set_id = 63;
        assert!(validate_metadata_pair(&drc, &loudness).is_err());
    }
}
