//! Native DTS core/DTS-HD framing checks and a bounded reference-decoder adapter.
//!
//! Forge owns elementary-stream framing and independently measures every WAVE
//! render.  Normative asset/presentation decoding remains in an explicitly
//! selected licensed or reference adapter; no DTS decoder is bundled.

use crate::wav::{AudioBuffer, ChannelRole};
use crate::{analysis, decoder, normalize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-dts-reference-adapter-1";
pub const STANDARD: &str = "ETSI TS 102 114 V1.6.1 (2019-08)";
pub const REQUEST_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/dts-adapter-request-v1";
pub const RESPONSE_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/dts-adapter-response-v1";
pub const REPORT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/dts-adapter-report-v1";

const CORE_BE: u32 = 0x7FFE_8001;
const CORE_LE: u32 = 0xFE7F_0180;
const CORE_14_BE: u32 = 0x1FFF_E800;
const CORE_14_LE: u32 = 0xFF1F_00E8;
const EXSS: u32 = 0x6458_2025;
const MAX_FRAMES: u64 = 1_000_000;
const MAX_EXSS_HEADER_BYTES: usize = 4_096;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ASSETS: usize = 32;
const MAX_PRESENTATIONS: usize = 32;
const HARD_MAX_DECODED_SAMPLES: u64 = 200_000_000;
const MAX_TIMEOUT_SECONDS: u64 = 3_600;
const TOOL_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AdapterOptions {
    pub input: PathBuf,
    pub adapter: PathBuf,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub max_true_peak_dbtp: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum CoreWireFormat {
    SixteenBitBigEndian,
    SixteenBitLittleEndian,
    FourteenBitBigEndian,
    FourteenBitLittleEndian,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WireFormatCount {
    pub format: CoreWireFormat,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoreConfiguration {
    pub sample_rate_hz: u32,
    pub sample_blocks: u16,
    pub audio_mode: u8,
    pub channels: u8,
    pub lfe_mode: u8,
    pub bit_rate_code: u8,
    pub bit_rate_bps: Option<u32>,
    pub pcm_resolution_bits: u8,
    pub extension_audio_present: bool,
    pub extension_audio_type: u8,
    pub extension_audio_name: &'static str,
    pub dialog_normalization_code: Option<u8>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CoreConfigurationCount {
    #[serde(flatten)]
    pub configuration: CoreConfiguration,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExtensionSubstreamSummary {
    pub index: u8,
    pub count: u64,
    pub static_header_count: u64,
    pub maximum_presentations: u8,
    pub maximum_assets: u8,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DtsInventory {
    pub stream_bytes: u64,
    pub frame_count: u64,
    pub core_frame_count: u64,
    pub extension_substream_count: u64,
    pub padding_bytes: u64,
    pub core_sample_blocks: u64,
    pub wire_formats: Vec<WireFormatCount>,
    pub core_configurations: Vec<CoreConfigurationCount>,
    pub extension_substreams: Vec<ExtensionSubstreamSummary>,
    pub declared_presentation_count: u8,
    pub declared_asset_count: u8,
}

#[derive(Debug, Serialize)]
struct AdapterRequest {
    schema: &'static str,
    protocol_version: u32,
    input_path: String,
    input_sha256: String,
    input_bytes: u64,
    output_directory: String,
    native_inventory: DtsInventory,
    requirements: AdapterRequirements,
}

#[derive(Debug, Serialize)]
struct AdapterRequirements {
    enumerate_all_assets: bool,
    enumerate_all_presentations: bool,
    render_every_presentation_once: bool,
    rendered_format: &'static str,
    dialog_normalization: &'static str,
    dynamic_range_control: &'static str,
    standard: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DecoderEvidence {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DtsProfile {
    Core,
    Es,
    #[serde(rename = "96-24")]
    NinetySixTwentyFour,
    HighResolution,
    MasterAudio,
    Express,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum CodingComponent {
    Core,
    Xch,
    Xxch,
    X96,
    Xbr,
    Lbr,
    Xll,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessingPolicy {
    Disabled,
    Applied,
    NotSupported,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetMetadata {
    pub id: String,
    #[serde(default)]
    pub extension_substream_index: Option<u8>,
    pub asset_index: u8,
    #[serde(default)]
    pub language: Option<String>,
    pub channels: u16,
    pub maximum_sample_rate_hz: u32,
    pub pcm_resolution_bits: u8,
    pub coding_components: Vec<CodingComponent>,
    #[serde(default)]
    pub dialog_normalization_db: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterPresentation {
    id: String,
    asset_ids: Vec<String>,
    rendered_path: PathBuf,
    output_layout: String,
    declared_sample_rate_hz: u32,
    declared_channels: u16,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    accessibility: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterResponse {
    schema: String,
    protocol_version: u32,
    input_sha256: String,
    decoder: DecoderEvidence,
    standard: String,
    profile: DtsProfile,
    dialog_normalization_policy: ProcessingPolicy,
    dynamic_range_control_policy: ProcessingPolicy,
    asset_count: usize,
    assets: Vec<AssetMetadata>,
    presentation_count: usize,
    presentations: Vec<AdapterPresentation>,
}

#[derive(Debug, Serialize)]
pub struct DtsAdapterReport {
    pub schema: &'static str,
    pub protocol_version: u32,
    pub validator: &'static str,
    pub input_path: String,
    pub input_bytes: u64,
    pub input_sha256: String,
    pub adapter_path: String,
    pub adapter_sha256: String,
    pub decoder: DecoderEvidence,
    pub standard: &'static str,
    pub profile: DtsProfile,
    pub dialog_normalization_policy: ProcessingPolicy,
    pub dynamic_range_control_policy: ProcessingPolicy,
    pub native_inventory: DtsInventory,
    pub timeout_seconds: u64,
    pub max_decoded_samples_per_presentation: u64,
    pub max_true_peak_dbtp: Option<f64>,
    pub asset_count: usize,
    pub assets: Vec<AssetMetadata>,
    pub presentation_count: usize,
    pub passed: bool,
    pub presentations: Vec<PresentationResult>,
}

#[derive(Debug, Serialize)]
pub struct PresentationResult {
    pub id: String,
    pub asset_ids: Vec<String>,
    pub output_layout: String,
    pub language: Option<String>,
    pub accessibility: Option<String>,
    pub declared_sample_rate_hz: u32,
    pub declared_channels: u16,
    pub rendered_sha256: String,
    pub rendered_bytes: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub measured_integrated_lufs: f64,
    pub measured_true_peak_dbtp: f64,
    pub sample_rate_passed: bool,
    pub channels_passed: bool,
    pub true_peak_passed: Option<bool>,
    pub passed: bool,
    pub checks: Vec<DtsCheck>,
}

#[derive(Debug, Serialize)]
pub struct DtsCheck {
    pub rule_id: &'static str,
    pub standard: &'static str,
    pub measured: f64,
    pub expected: f64,
    pub comparison: &'static str,
    pub unit: &'static str,
    pub passed: bool,
}

/// Parse every raw DTS core and DTS-HD extension-substream frame boundary.
pub fn inspect_stream(path: &Path) -> Result<DtsInventory, String> {
    let mut file =
        File::open(path).map_err(|error| format!("open DTS stream {}: {error}", path.display()))?;
    let stream_bytes = file
        .metadata()
        .map_err(|error| format!("stat DTS stream {}: {error}", path.display()))?
        .len();
    if stream_bytes == 0 {
        return Err("DTS stream is empty".into());
    }

    let mut offset = 0_u64;
    let mut frame_count = 0_u64;
    let mut core_frame_count = 0_u64;
    let mut extension_substream_count = 0_u64;
    let mut padding_bytes = 0_u64;
    let mut core_sample_blocks = 0_u64;
    let mut wire_formats = BTreeMap::<CoreWireFormat, u64>::new();
    let mut core_configurations = BTreeMap::<CoreConfiguration, u64>::new();
    let mut exss = BTreeMap::<u8, ExtensionAccumulator>::new();

    while offset < stream_bytes {
        frame_count = frame_count
            .checked_add(1)
            .ok_or_else(|| "DTS frame count overflow".to_string())?;
        if frame_count > MAX_FRAMES {
            return Err(format!(
                "DTS frame count exceeds the {MAX_FRAMES} frame safety limit"
            ));
        }
        let sync = read_u32_at(&mut file, offset, stream_bytes)?;
        match core_wire_format(sync) {
            Some(format) => {
                let header = parse_core_header(&mut file, offset, stream_bytes, format)?;
                let end = offset
                    .checked_add(u64::from(header.frame_bytes))
                    .ok_or_else(|| format!("DTS core frame at {offset} overflows its offset"))?;
                if end > stream_bytes {
                    return Err(format!(
                        "DTS core frame at {offset} declares {} bytes, but only {} remain",
                        header.frame_bytes,
                        stream_bytes - offset
                    ));
                }
                core_frame_count += 1;
                core_sample_blocks = core_sample_blocks
                    .checked_add(u64::from(header.configuration.sample_blocks))
                    .ok_or_else(|| "DTS core sample-block count overflow".to_string())?;
                *wire_formats.entry(format).or_default() += 1;
                *core_configurations.entry(header.configuration).or_default() += 1;
                offset = end;

                // DTS-HD permits zero fill between the core and the following
                // DWORD-aligned extension substream. Never consume bytes unless
                // the aligned destination is an EXSS sync word.
                if offset < stream_bytes && !offset.is_multiple_of(4) {
                    let aligned = offset + (4 - offset % 4);
                    if aligned + 4 <= stream_bytes
                        && read_u32_at(&mut file, aligned, stream_bytes)? == EXSS
                    {
                        let pad = usize::try_from(aligned - offset)
                            .map_err(|_| "DTS alignment padding overflow".to_string())?;
                        let bytes = read_at(&mut file, offset, pad, stream_bytes)?;
                        if bytes.iter().any(|byte| *byte != 0) {
                            return Err(format!(
                                "DTS core frame at {offset} has non-zero DWORD alignment padding"
                            ));
                        }
                        padding_bytes += pad as u64;
                        offset = aligned;
                    }
                }
            }
            None if sync == EXSS => {
                if !offset.is_multiple_of(4) {
                    return Err(format!(
                        "DTS-HD extension substream at {offset} is not DWORD-aligned"
                    ));
                }
                let header = parse_exss_header(&mut file, offset, stream_bytes)?;
                let end = offset
                    .checked_add(u64::from(header.frame_bytes))
                    .ok_or_else(|| format!("DTS-HD extension frame at {offset} overflows"))?;
                if end > stream_bytes {
                    return Err(format!(
                        "DTS-HD extension substream at {offset} declares {} bytes, but only {} remain",
                        header.frame_bytes,
                        stream_bytes - offset
                    ));
                }
                extension_substream_count += 1;
                let item = exss.entry(header.index).or_default();
                item.count += 1;
                if header.static_fields {
                    item.static_header_count += 1;
                }
                item.maximum_presentations = item.maximum_presentations.max(header.presentations);
                item.maximum_assets = item.maximum_assets.max(header.assets);
                offset = end;
            }
            None => {
                return Err(format!(
                    "unrecognized DTS sync word 0x{sync:08X} at byte offset {offset}; resynchronization is intentionally disabled"
                ));
            }
        }
    }
    if core_frame_count == 0 && extension_substream_count == 0 {
        return Err("DTS stream contains no complete core or extension frames".into());
    }
    let declared_presentation_count = exss
        .values()
        .map(|item| item.maximum_presentations)
        .max()
        .unwrap_or(1)
        .max(1);
    let declared_asset_count = if exss.is_empty() {
        1
    } else {
        exss.values()
            .try_fold(0_u8, |total, item| {
                total.checked_add(item.maximum_assets.max(1))
            })
            .ok_or_else(|| "DTS-HD declared asset count overflow".to_string())?
    };
    Ok(DtsInventory {
        stream_bytes,
        frame_count,
        core_frame_count,
        extension_substream_count,
        padding_bytes,
        core_sample_blocks,
        wire_formats: wire_formats
            .into_iter()
            .map(|(format, count)| WireFormatCount { format, count })
            .collect(),
        core_configurations: core_configurations
            .into_iter()
            .map(|(configuration, count)| CoreConfigurationCount {
                configuration,
                count,
            })
            .collect(),
        extension_substreams: exss
            .into_iter()
            .map(|(index, item)| ExtensionSubstreamSummary {
                index,
                count: item.count,
                static_header_count: item.static_header_count,
                maximum_presentations: item.maximum_presentations,
                maximum_assets: item.maximum_assets,
            })
            .collect(),
        declared_presentation_count,
        declared_asset_count,
    })
}

#[derive(Default)]
struct ExtensionAccumulator {
    count: u64,
    static_header_count: u64,
    maximum_presentations: u8,
    maximum_assets: u8,
}

struct ParsedCoreHeader {
    frame_bytes: u16,
    configuration: CoreConfiguration,
}

fn parse_core_header(
    file: &mut File,
    offset: u64,
    stream_bytes: u64,
    format: CoreWireFormat,
) -> Result<ParsedCoreHeader, String> {
    let available = usize::try_from((stream_bytes - offset).min(40))
        .map_err(|_| "DTS core header length overflow".to_string())?;
    if available < 18 {
        return Err(format!("truncated DTS core header at byte offset {offset}"));
    }
    let wire = read_at(file, offset, available, stream_bytes)?;
    let canonical = canonicalize_core(&wire, format)?;
    let mut bits = SliceBits::new(&canonical, "DTS core header");
    if bits.read(32)? != u64::from(CORE_BE) {
        return Err(format!("invalid canonical DTS core sync at {offset}"));
    }
    let normal_frame = bits.read(1)? != 0;
    let deficit_samples = bits.read(5)? as u8 + 1;
    if normal_frame && deficit_samples != 32 {
        return Err(format!(
            "normal DTS core frame at {offset} has deficit sample count {deficit_samples}, expected 32"
        ));
    }
    let crc_present = bits.read(1)? != 0;
    let sample_blocks = bits.read(7)? as u16 + 1;
    if sample_blocks < 8 || !sample_blocks.is_multiple_of(8) {
        return Err(format!(
            "DTS core frame at {offset} has invalid PCM sample-block count {sample_blocks}"
        ));
    }
    let frame_bytes = bits.read(14)? as u16 + 1;
    if frame_bytes < 96 {
        return Err(format!(
            "DTS core frame at {offset} is smaller than the 96-byte minimum"
        ));
    }
    let audio_mode = bits.read(6)? as u8;
    let channels = *CORE_CHANNELS.get(usize::from(audio_mode)).ok_or_else(|| {
        format!("DTS core frame at {offset} uses reserved audio mode {audio_mode}")
    })?;
    let sample_rate_code = bits.read(4)? as u8;
    let sample_rate_hz = CORE_SAMPLE_RATES[usize::from(sample_rate_code)];
    if sample_rate_hz == 0 {
        return Err(format!(
            "DTS core frame at {offset} uses reserved sample-rate code {sample_rate_code}"
        ));
    }
    let bit_rate_code = bits.read(5)? as u8;
    if bits.read(1)? != 0 {
        return Err(format!("DTS core frame at {offset} sets the reserved bit"));
    }
    bits.skip(4)?; // DRC, timestamp, auxiliary and HDCD flags.
    let extension_audio_type = bits.read(3)? as u8;
    let extension_audio_present = bits.read(1)? != 0;
    bits.skip(1)?; // audio sync-word insertion flag
    let lfe_mode = bits.read(2)? as u8;
    if lfe_mode == 3 {
        return Err(format!("DTS core frame at {offset} uses reserved LFE mode"));
    }
    bits.skip(1)?; // predictor history
    if crc_present {
        bits.skip(16)?;
    }
    bits.skip(1)?; // multirate interpolator
    let encoder_revision = bits.read(4)? as u8;
    bits.skip(2)?; // copy history
    let pcm_resolution_code = bits.read(3)? as usize;
    let pcm_resolution_bits = PCM_RESOLUTIONS[pcm_resolution_code];
    if pcm_resolution_bits == 0 {
        return Err(format!(
            "DTS core frame at {offset} uses reserved PCM resolution code {pcm_resolution_code}"
        ));
    }
    bits.skip(2)?; // sum/difference coding flags
    let dialog_normalization_code = bits.read(4)? as u8;
    Ok(ParsedCoreHeader {
        frame_bytes,
        configuration: CoreConfiguration {
            sample_rate_hz,
            sample_blocks,
            audio_mode,
            channels: channels + u8::from(lfe_mode != 0),
            lfe_mode,
            bit_rate_code,
            bit_rate_bps: CORE_BIT_RATES[usize::from(bit_rate_code)],
            pcm_resolution_bits,
            extension_audio_present,
            extension_audio_type,
            extension_audio_name: extension_audio_name(extension_audio_type),
            dialog_normalization_code: matches!(encoder_revision, 6 | 7)
                .then_some(dialog_normalization_code),
        },
    })
}

struct ParsedExssHeader {
    index: u8,
    frame_bytes: u32,
    static_fields: bool,
    presentations: u8,
    assets: u8,
}

fn parse_exss_header(
    file: &mut File,
    offset: u64,
    stream_bytes: u64,
) -> Result<ParsedExssHeader, String> {
    let prefix_len = usize::try_from((stream_bytes - offset).min(16))
        .map_err(|_| "DTS-HD header length overflow".to_string())?;
    if prefix_len < 10 {
        return Err(format!(
            "truncated DTS-HD extension header at byte offset {offset}"
        ));
    }
    let prefix = read_at(file, offset, prefix_len, stream_bytes)?;
    let mut bits = SliceBits::new(&prefix, "DTS-HD extension header");
    if bits.read(32)? != u64::from(EXSS) {
        return Err(format!("invalid DTS-HD extension sync at {offset}"));
    }
    bits.skip(8)?; // user-defined bits
    let index = bits.read(2)? as u8;
    let wide_header = bits.read(1)? != 0;
    let header_bits = if wide_header { 12 } else { 8 };
    let frame_bits = if wide_header { 20 } else { 16 };
    let header_bytes = bits.read(header_bits)? as usize + 1;
    let frame_bytes = bits.read(frame_bits)? as u32 + 1;
    if !(10..=MAX_EXSS_HEADER_BYTES).contains(&header_bytes) || frame_bytes < header_bytes as u32 {
        return Err(format!(
            "DTS-HD extension at {offset} has invalid header/frame sizes {header_bytes}/{frame_bytes}"
        ));
    }
    if offset + u64::from(frame_bytes) > stream_bytes {
        return Err(format!(
            "DTS-HD extension substream at {offset} exceeds the input"
        ));
    }
    let header = read_at(file, offset, header_bytes, stream_bytes)?;
    if header_bytes < 7 || crc16_ccitt(&header[5..]) != 0 {
        return Err(format!(
            "DTS-HD extension at {offset} has an invalid header CRC-16/CCITT"
        ));
    }
    let mut bits = SliceBits::new(&header, "DTS-HD extension header");
    bits.skip(32 + 8 + 2 + 1 + header_bits + frame_bits)?;
    let static_fields = bits.read(1)? != 0;
    let (presentations, assets) = if static_fields {
        let reference_clock = bits.read(2)? as u8;
        if reference_clock == 3 {
            return Err(format!(
                "DTS-HD extension at {offset} uses reserved reference clock code"
            ));
        }
        bits.skip(3)?; // frame-duration code
        if bits.read(1)? != 0 {
            bits.skip(36)?;
        }
        (bits.read(3)? as u8 + 1, bits.read(3)? as u8 + 1)
    } else {
        (1, 1)
    };
    Ok(ParsedExssHeader {
        index,
        frame_bytes,
        static_fields,
        presentations,
        assets,
    })
}

fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn canonicalize_core(wire: &[u8], format: CoreWireFormat) -> Result<Vec<u8>, String> {
    match format {
        CoreWireFormat::SixteenBitBigEndian => Ok(wire.to_vec()),
        CoreWireFormat::SixteenBitLittleEndian => {
            if !wire.len().is_multiple_of(2) {
                return Err("16-bit little-endian DTS header ends in a partial word".into());
            }
            let mut output = wire.to_vec();
            for word in output.chunks_exact_mut(2) {
                word.swap(0, 1);
            }
            Ok(output)
        }
        CoreWireFormat::FourteenBitBigEndian | CoreWireFormat::FourteenBitLittleEndian => {
            if !wire.len().is_multiple_of(2) {
                return Err("14-bit DTS header ends in a partial word".into());
            }
            let little = format == CoreWireFormat::FourteenBitLittleEndian;
            let mut output = Vec::with_capacity(wire.len() * 7 / 8);
            let mut accumulator = 0_u64;
            let mut available = 0_u8;
            for pair in wire.chunks_exact(2) {
                let word = if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                };
                accumulator = (accumulator << 14) | u64::from(word & 0x3FFF);
                available += 14;
                while available >= 8 {
                    available -= 8;
                    output.push((accumulator >> available) as u8);
                    accumulator &= (1_u64 << available).saturating_sub(1);
                }
            }
            Ok(output)
        }
    }
}

fn core_wire_format(sync: u32) -> Option<CoreWireFormat> {
    match sync {
        CORE_BE => Some(CoreWireFormat::SixteenBitBigEndian),
        CORE_LE => Some(CoreWireFormat::SixteenBitLittleEndian),
        CORE_14_BE => Some(CoreWireFormat::FourteenBitBigEndian),
        CORE_14_LE => Some(CoreWireFormat::FourteenBitLittleEndian),
        _ => None,
    }
}

fn extension_audio_name(value: u8) -> &'static str {
    match value {
        0 => "xch",
        1 => "unknown-1",
        2 => "x96",
        3 => "unknown-3",
        4 => "unknown-4",
        5 => "unknown-5",
        6 => "xxch",
        _ => "unknown-7",
    }
}

const CORE_CHANNELS: [u8; 16] = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
const CORE_SAMPLE_RATES: [u32; 16] = [
    0, 8_000, 16_000, 32_000, 0, 0, 11_025, 22_050, 44_100, 0, 0, 12_000, 24_000, 48_000, 96_000,
    192_000,
];
const CORE_BIT_RATES: [Option<u32>; 32] = [
    Some(32_000),
    Some(56_000),
    Some(64_000),
    Some(96_000),
    Some(112_000),
    Some(128_000),
    Some(192_000),
    Some(224_000),
    Some(256_000),
    Some(320_000),
    Some(384_000),
    Some(448_000),
    Some(512_000),
    Some(576_000),
    Some(640_000),
    Some(768_000),
    Some(896_000),
    Some(1_024_000),
    Some(1_152_000),
    Some(1_280_000),
    Some(1_344_000),
    Some(1_408_000),
    Some(1_411_200),
    Some(1_472_000),
    Some(1_536_000),
    Some(1_920_000),
    Some(2_048_000),
    Some(3_072_000),
    Some(3_840_000),
    None,
    None,
    None,
];
const PCM_RESOLUTIONS: [u8; 8] = [16, 16, 20, 20, 0, 24, 24, 0];

struct SliceBits<'a> {
    bytes: &'a [u8],
    position: usize,
    label: &'static str,
}

impl<'a> SliceBits<'a> {
    fn new(bytes: &'a [u8], label: &'static str) -> Self {
        Self {
            bytes,
            position: 0,
            label,
        }
    }

    fn read(&mut self, count: usize) -> Result<u64, String> {
        if count > 64 || self.position + count > self.bytes.len() * 8 {
            return Err(format!(
                "truncated {} at bit {}: need {count} bits, have {}",
                self.label,
                self.position,
                self.bytes.len() * 8 - self.position
            ));
        }
        let mut value = 0_u64;
        for _ in 0..count {
            let byte = self.bytes[self.position / 8];
            let bit = (byte >> (7 - self.position % 8)) & 1;
            value = (value << 1) | u64::from(bit);
            self.position += 1;
        }
        Ok(value)
    }

    fn skip(&mut self, count: usize) -> Result<(), String> {
        if self.position + count > self.bytes.len() * 8 {
            return Err(format!(
                "truncated {} at bit {}: need {count} bits, have {}",
                self.label,
                self.position,
                self.bytes.len() * 8 - self.position
            ));
        }
        self.position += count;
        Ok(())
    }
}

fn read_u32_at(file: &mut File, offset: u64, stream_bytes: u64) -> Result<u32, String> {
    let bytes = read_at(file, offset, 4, stream_bytes)?;
    Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
}

fn read_at(
    file: &mut File,
    offset: u64,
    length: usize,
    stream_bytes: u64,
) -> Result<Vec<u8>, String> {
    let end = offset
        .checked_add(length as u64)
        .ok_or_else(|| "DTS read offset overflow".to_string())?;
    if end > stream_bytes {
        return Err(format!(
            "truncated DTS stream at byte offset {offset}: need {length} bytes"
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek DTS stream to {offset}: {error}"))?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read DTS stream at {offset}: {error}"))?;
    Ok(bytes)
}

pub fn run(options: &AdapterOptions) -> Result<DtsAdapterReport, String> {
    validate_options(options)?;
    let input = fs::canonicalize(&options.input)
        .map_err(|error| format!("resolve DTS input {}: {error}", options.input.display()))?;
    let adapter = fs::canonicalize(&options.adapter)
        .map_err(|error| format!("resolve DTS adapter {}: {error}", options.adapter.display()))?;
    ensure_regular_file(&input, "DTS input")?;
    ensure_regular_file(&adapter, "DTS adapter")?;
    let (input_sha256, input_bytes) = sha256_file(&input)?;
    let inventory = inspect_stream(&input)?;
    let (input_after_inspection, bytes_after_inspection) = sha256_file(&input)?;
    if input_after_inspection != input_sha256 || bytes_after_inspection != input_bytes {
        return Err("DTS input changed while its native framing was inspected".into());
    }
    let (adapter_sha256, _) = sha256_file(&adapter)?;

    let work = tempfile::tempdir().map_err(|error| format!("create adapter workspace: {error}"))?;
    let renders = work.path().join("renders");
    fs::create_dir(&renders).map_err(|error| format!("create render directory: {error}"))?;
    let request_path = work.path().join("request.json");
    let response_path = work.path().join("response.json");
    let request = AdapterRequest {
        schema: REQUEST_SCHEMA,
        protocol_version: PROTOCOL_VERSION,
        input_path: input.to_string_lossy().into_owned(),
        input_sha256: input_sha256.clone(),
        input_bytes,
        output_directory: renders.to_string_lossy().into_owned(),
        native_inventory: inventory.clone(),
        requirements: AdapterRequirements {
            enumerate_all_assets: true,
            enumerate_all_presentations: true,
            render_every_presentation_once: true,
            rendered_format: "wave",
            dialog_normalization: "disable-or-report",
            dynamic_range_control: "disable-or-report",
            standard: STANDARD,
        },
    };
    let mut request_bytes = serde_json::to_vec_pretty(&request)
        .map_err(|error| format!("serialize DTS adapter request: {error}"))?;
    request_bytes.push(b'\n');
    fs::write(&request_path, request_bytes)
        .map_err(|error| format!("write DTS adapter request: {error}"))?;

    let tool = run_bounded(
        &adapter,
        &[
            "--request".into(),
            request_path.as_os_str().to_owned(),
            "--response".into(),
            response_path.as_os_str().to_owned(),
        ],
        Duration::from_secs(options.timeout_seconds),
    )?;
    if !tool.status.success() {
        return Err(format!(
            "DTS adapter failed ({}): {}",
            tool.status,
            String::from_utf8_lossy(&tool.stderr).trim()
        ));
    }
    let (adapter_after, _) = sha256_file(&adapter)?;
    if adapter_after != adapter_sha256 {
        return Err("DTS adapter executable changed while it was running".into());
    }
    let response_bytes = read_response(work.path(), &response_path)?;
    let response: AdapterResponse = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("parse DTS adapter response: {error}"))?;
    validate_response(&response, &input_sha256, &inventory)?;
    let (input_after, bytes_after) = sha256_file(&input)?;
    if input_after != input_sha256 || bytes_after != input_bytes {
        return Err("DTS input changed while the decoder adapter was running".into());
    }

    let render_root = fs::canonicalize(&renders)
        .map_err(|error| format!("resolve adapter render directory: {error}"))?;
    let mut results = Vec::with_capacity(response.presentations.len());
    for presentation in response.presentations {
        let rendered = resolve_render(&render_root, &presentation.rendered_path)?;
        let (rendered_sha256, rendered_bytes) = sha256_file(&rendered)?;
        let (buffer, layout_provenance) = decoder::decode_limited_with_layout(
            &rendered,
            options.max_decoded_samples_per_presentation,
        )?;
        let buffer = resolve_rendered_layout(
            &rendered,
            buffer,
            layout_provenance,
            &presentation.output_layout,
            presentation.declared_channels,
        )?;
        let measured = analysis::analyze(&buffer);
        let (rendered_after, bytes_after) = sha256_file(&rendered)?;
        if rendered_after != rendered_sha256 || bytes_after != rendered_bytes {
            return Err(format!(
                "presentation {} render changed while it was measured",
                presentation.id
            ));
        }
        if !measured.lufs.is_finite() || !measured.true_peak_db().is_finite() {
            return Err(format!(
                "presentation {} did not produce finite loudness and true-peak measurements",
                presentation.id
            ));
        }
        let sample_rate_passed = measured.sample_rate == presentation.declared_sample_rate_hz;
        let channels_passed = measured.channels == presentation.declared_channels;
        let true_peak_passed = options
            .max_true_peak_dbtp
            .map(|ceiling| measured.true_peak_db() <= ceiling);
        let mut checks = vec![
            DtsCheck {
                rule_id: "FORGE-DTS-RENDER-SAMPLE-RATE",
                standard: STANDARD,
                measured: f64::from(measured.sample_rate),
                expected: f64::from(presentation.declared_sample_rate_hz),
                comparison: "equal",
                unit: "Hz",
                passed: sample_rate_passed,
            },
            DtsCheck {
                rule_id: "FORGE-DTS-RENDER-CHANNELS",
                standard: STANDARD,
                measured: f64::from(measured.channels),
                expected: f64::from(presentation.declared_channels),
                comparison: "equal",
                unit: "channels",
                passed: channels_passed,
            },
        ];
        if let Some(ceiling) = options.max_true_peak_dbtp {
            checks.push(DtsCheck {
                rule_id: "FORGE-DTS-TRUE-PEAK",
                standard: "ITU-R BS.1770-5",
                measured: measured.true_peak_db(),
                expected: ceiling,
                comparison: "less-than-or-equal",
                unit: "dBTP",
                passed: true_peak_passed == Some(true),
            });
        }
        let passed = sample_rate_passed && channels_passed && true_peak_passed != Some(false);
        results.push(PresentationResult {
            id: presentation.id,
            asset_ids: presentation.asset_ids,
            output_layout: presentation.output_layout,
            language: presentation.language,
            accessibility: presentation.accessibility,
            declared_sample_rate_hz: presentation.declared_sample_rate_hz,
            declared_channels: presentation.declared_channels,
            rendered_sha256,
            rendered_bytes,
            sample_rate_hz: measured.sample_rate,
            channels: measured.channels,
            duration_seconds: measured.duration_secs(),
            measured_integrated_lufs: measured.lufs,
            measured_true_peak_dbtp: measured.true_peak_db(),
            sample_rate_passed,
            channels_passed,
            true_peak_passed,
            passed,
            checks,
        });
    }
    let passed = results.iter().all(|item| item.passed);
    Ok(DtsAdapterReport {
        schema: REPORT_SCHEMA,
        protocol_version: PROTOCOL_VERSION,
        validator: VALIDATOR,
        input_path: input.to_string_lossy().into_owned(),
        input_bytes,
        input_sha256,
        adapter_path: adapter.to_string_lossy().into_owned(),
        adapter_sha256,
        decoder: response.decoder,
        standard: STANDARD,
        profile: response.profile,
        dialog_normalization_policy: response.dialog_normalization_policy,
        dynamic_range_control_policy: response.dynamic_range_control_policy,
        native_inventory: inventory,
        timeout_seconds: options.timeout_seconds,
        max_decoded_samples_per_presentation: options.max_decoded_samples_per_presentation,
        max_true_peak_dbtp: options.max_true_peak_dbtp,
        asset_count: response.asset_count,
        assets: response.assets,
        presentation_count: results.len(),
        passed,
        presentations: results,
    })
}

pub fn write_report(
    path: &Path,
    report: &DtsAdapterReport,
    compact: bool,
    overwrite: bool,
) -> Result<(), String> {
    if path.exists() && !overwrite {
        return Err(format!(
            "refusing to replace existing DTS report {}; pass --overwrite",
            path.display()
        ));
    }
    let mut bytes = if compact {
        serde_json::to_vec(report)
    } else {
        serde_json::to_vec_pretty(report)
    }
    .map_err(|error| format!("serialize DTS adapter report: {error}"))?;
    bytes.push(b'\n');
    let mut output = crate::atomic::AtomicOutput::new_with_overwrite(path, overwrite)?;
    output.write_all(&bytes)?;
    output.commit()
}

fn validate_options(options: &AdapterOptions) -> Result<(), String> {
    if options.timeout_seconds == 0 || options.timeout_seconds > MAX_TIMEOUT_SECONDS {
        return Err(format!(
            "adapter timeout must be 1..={MAX_TIMEOUT_SECONDS} seconds"
        ));
    }
    if options.max_decoded_samples_per_presentation == 0
        || options.max_decoded_samples_per_presentation > HARD_MAX_DECODED_SAMPLES
    {
        return Err(format!(
            "decoded sample limit must be 1..={HARD_MAX_DECODED_SAMPLES}"
        ));
    }
    if options
        .max_true_peak_dbtp
        .is_some_and(|value| !value.is_finite() || !(-100.0..=0.0).contains(&value))
    {
        return Err("true-peak ceiling must be finite and between -100 and 0 dBTP".into());
    }
    Ok(())
}

fn validate_response(
    response: &AdapterResponse,
    input_sha256: &str,
    inventory: &DtsInventory,
) -> Result<(), String> {
    if response.schema != RESPONSE_SCHEMA || response.protocol_version != PROTOCOL_VERSION {
        return Err("unsupported DTS adapter response schema or protocol version".into());
    }
    if !response.input_sha256.eq_ignore_ascii_case(input_sha256) {
        return Err("DTS adapter response is not bound to the requested input SHA-256".into());
    }
    if !valid_text(&response.decoder.name, 128) || !valid_text(&response.decoder.version, 128) {
        return Err("DTS decoder name and version are required".into());
    }
    if response.standard != STANDARD {
        return Err("DTS adapter does not claim the required current ETSI standard".into());
    }
    if response.assets.is_empty()
        || response.assets.len() > MAX_ASSETS
        || response.asset_count != response.assets.len()
        || response.asset_count != usize::from(inventory.declared_asset_count)
    {
        return Err(format!(
            "adapter must enumerate exactly the {} declared DTS assets",
            inventory.declared_asset_count
        ));
    }
    if response.presentations.is_empty()
        || response.presentations.len() > MAX_PRESENTATIONS
        || response.presentation_count != response.presentations.len()
        || response.presentation_count != usize::from(inventory.declared_presentation_count)
    {
        return Err(format!(
            "adapter must enumerate and render exactly the {} declared DTS presentations",
            inventory.declared_presentation_count
        ));
    }
    validate_profile(response.profile, &response.assets, inventory)?;
    let expected_locations: HashSet<(Option<u8>, u8)> = if inventory.extension_substreams.is_empty()
    {
        HashSet::from([(None, 0)])
    } else {
        inventory
            .extension_substreams
            .iter()
            .flat_map(|substream| {
                (0..substream.maximum_assets.max(1))
                    .map(move |asset| (Some(substream.index), asset))
            })
            .collect()
    };
    let mut asset_ids = HashSet::new();
    let mut asset_locations = HashSet::new();
    let mut assets_by_id = HashMap::new();
    for asset in &response.assets {
        if !valid_id(&asset.id) || !asset_ids.insert(asset.id.as_str()) {
            return Err("DTS asset IDs must be unique ASCII identifiers".into());
        }
        assets_by_id.insert(asset.id.as_str(), asset);
        let location = (asset.extension_substream_index, asset.asset_index);
        if !expected_locations.contains(&location) || !asset_locations.insert(location) {
            return Err(
                "DTS asset locations must enumerate every declared substream asset exactly once"
                    .into(),
            );
        }
        if !valid_optional_text(asset.language.as_deref(), 35)
            || asset.channels == 0
            || asset.channels > 256
            || !(8_000..=384_000).contains(&asset.maximum_sample_rate_hz)
            || !(1..=32).contains(&asset.pcm_resolution_bits)
            || asset.coding_components.is_empty()
            || asset.coding_components.len() > 7
        {
            return Err(format!("DTS asset {} has invalid metadata", asset.id));
        }
        let mut components = HashSet::new();
        if asset
            .coding_components
            .iter()
            .any(|component| !components.insert(*component))
        {
            return Err(format!("DTS asset {} repeats a coding component", asset.id));
        }
        if asset
            .dialog_normalization_db
            .is_some_and(|value| !value.is_finite() || !(-31.0..=0.0).contains(&value))
        {
            return Err(format!(
                "DTS asset {} has invalid dialog normalization metadata",
                asset.id
            ));
        }
    }
    if asset_locations != expected_locations {
        return Err("DTS adapter did not enumerate every declared substream asset".into());
    }

    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut used_assets = HashSet::new();
    for presentation in &response.presentations {
        if !valid_id(&presentation.id) || !ids.insert(presentation.id.as_str()) {
            return Err("DTS presentation IDs must be unique ASCII identifiers".into());
        }
        if presentation.asset_ids.is_empty() || presentation.asset_ids.len() > response.asset_count
        {
            return Err(format!(
                "DTS presentation {} has an invalid asset list",
                presentation.id
            ));
        }
        let mut local_assets = HashSet::new();
        for id in &presentation.asset_ids {
            if !asset_ids.contains(id.as_str()) || !local_assets.insert(id.as_str()) {
                return Err(format!(
                    "DTS presentation {} has unknown or duplicate assets",
                    presentation.id
                ));
            }
            used_assets.insert(id.as_str());
        }
        let maximum_rate = presentation
            .asset_ids
            .iter()
            .filter_map(|id| assets_by_id.get(id.as_str()))
            .map(|asset| asset.maximum_sample_rate_hz)
            .max()
            .unwrap_or(0);
        let maximum_channels: u32 = presentation
            .asset_ids
            .iter()
            .filter_map(|id| assets_by_id.get(id.as_str()))
            .map(|asset| u32::from(asset.channels))
            .sum();
        if presentation.declared_sample_rate_hz > maximum_rate
            || u32::from(presentation.declared_channels) > maximum_channels
        {
            return Err(format!(
                "DTS presentation {} render geometry exceeds its referenced assets",
                presentation.id
            ));
        }
        if !valid_text(&presentation.output_layout, 64)
            || !valid_optional_text(presentation.language.as_deref(), 35)
            || !valid_optional_text(presentation.accessibility.as_deref(), 64)
            || !(8_000..=384_000).contains(&presentation.declared_sample_rate_hz)
            || presentation.declared_channels == 0
            || presentation.declared_channels > 256
        {
            return Err(format!(
                "DTS presentation {} has invalid render metadata",
                presentation.id
            ));
        }
        if declared_layout_roles(&presentation.output_layout)
            .is_some_and(|roles| roles.len() != usize::from(presentation.declared_channels))
        {
            return Err(format!(
                "DTS presentation {} output layout {} conflicts with its declared {} channels",
                presentation.id, presentation.output_layout, presentation.declared_channels
            ));
        }
        validate_relative_path(&presentation.rendered_path)?;
        if presentation
            .rendered_path
            .extension()
            .and_then(|value| value.to_str())
            != Some("wav")
            || !paths.insert(presentation.rendered_path.clone())
        {
            return Err("each DTS presentation requires a distinct relative .wav path".into());
        }
    }
    if used_assets != asset_ids {
        return Err("every DTS asset must be referenced by at least one presentation".into());
    }
    Ok(())
}

fn declared_layout_roles(name: &str) -> Option<Vec<ChannelRole>> {
    let normalized = name.trim().to_ascii_lowercase();
    crate::downmix::Layout::parse(&normalized).map(crate::downmix::Layout::roles)
}

fn resolve_rendered_layout(
    path: &Path,
    mut buffer: AudioBuffer,
    provenance: decoder::ChannelLayoutProvenance,
    declared_layout: &str,
    declared_channels: u16,
) -> Result<AudioBuffer, String> {
    if buffer.channels != declared_channels {
        return Err(format!(
            "DTS render {} decoded {} channels but the presentation declares {}",
            path.display(),
            buffer.channels,
            declared_channels
        ));
    }

    let declared_roles = declared_layout_roles(declared_layout);
    if let Some(roles) = declared_roles.as_deref() {
        if roles.len() != usize::from(declared_channels) {
            return Err(format!(
                "DTS render {} declares layout {declared_layout} with {} channels but the presentation declares {declared_channels}",
                path.display(),
                roles.len()
            ));
        }
        if provenance == decoder::ChannelLayoutProvenance::KnownSpeakers
            && !decoded_layout_matches_declared(&buffer.channel_roles, roles)
        {
            return Err(format!(
                "DTS render {} decoded speaker layout conflicts with declared layout {declared_layout}",
                path.display()
            ));
        }
    }

    buffer.channel_roles = normalize::resolve_decoded_channel_roles(
        path,
        buffer.channels,
        &buffer.channel_roles,
        provenance,
        declared_roles.as_deref(),
    )?;
    Ok(buffer)
}

fn decoded_layout_matches_declared(
    decoded_roles: &[ChannelRole],
    declared_roles: &[ChannelRole],
) -> bool {
    decoded_roles == declared_roles
        || (declared_roles.len() > 2
            && crate::wav::writer::persisted_channel_roles(declared_roles)
                .is_ok_and(|roles| roles == decoded_roles))
}

fn validate_profile(
    profile: DtsProfile,
    assets: &[AssetMetadata],
    inventory: &DtsInventory,
) -> Result<(), String> {
    let components: HashSet<_> = assets
        .iter()
        .flat_map(|asset| asset.coding_components.iter().copied())
        .collect();
    let core_extensions: HashSet<u8> = inventory
        .core_configurations
        .iter()
        .filter(|item| item.configuration.extension_audio_present)
        .map(|item| item.configuration.extension_audio_type)
        .collect();
    let valid = match profile {
        DtsProfile::Core => {
            inventory.core_frame_count > 0
                && inventory.extension_substream_count == 0
                && core_extensions.is_empty()
                && components == HashSet::from([CodingComponent::Core])
        }
        DtsProfile::Es => {
            let signalled = components.contains(&CodingComponent::Xch)
                || components.contains(&CodingComponent::Xxch);
            signalled
                && (inventory.extension_substream_count > 0
                    || core_extensions.contains(&0)
                    || core_extensions.contains(&6))
        }
        DtsProfile::NinetySixTwentyFour => {
            components.contains(&CodingComponent::X96)
                && (inventory.extension_substream_count > 0 || core_extensions.contains(&2))
        }
        DtsProfile::HighResolution => {
            inventory.extension_substream_count > 0
                && (components.contains(&CodingComponent::Xbr)
                    || components.contains(&CodingComponent::Xxch)
                    || components.contains(&CodingComponent::X96))
        }
        DtsProfile::MasterAudio => {
            inventory.extension_substream_count > 0 && components.contains(&CodingComponent::Xll)
        }
        DtsProfile::Express => {
            inventory.extension_substream_count > 0 && components.contains(&CodingComponent::Lbr)
        }
    };
    if valid {
        Ok(())
    } else {
        Err("DTS profile is inconsistent with native framing or adapter coding components".into())
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_optional_text(value: Option<&str>, maximum: usize) -> bool {
    value.is_none_or(|text| valid_text(text, maximum))
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("DTS render paths must remain relative to the adapter workspace".into());
    }
    Ok(())
}

fn resolve_render(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let candidate = root.join(relative);
    let resolved = fs::canonicalize(&candidate)
        .map_err(|error| format!("resolve DTS render {}: {error}", candidate.display()))?;
    if !resolved.starts_with(root) {
        return Err("DTS render escapes the adapter workspace".into());
    }
    ensure_regular_file(&resolved, "DTS render")?;
    Ok(resolved)
}

fn read_response(root: &Path, response: &Path) -> Result<Vec<u8>, String> {
    let resolved = fs::canonicalize(response)
        .map_err(|error| format!("resolve DTS adapter response: {error}"))?;
    if !resolved.starts_with(root) {
        return Err("DTS adapter response escapes its workspace".into());
    }
    let metadata =
        fs::metadata(&resolved).map_err(|error| format!("stat DTS adapter response: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "DTS adapter response must be a regular file no larger than {MAX_RESPONSE_BYTES} bytes"
        ));
    }
    fs::read(&resolved).map_err(|error| format!("read DTS adapter response: {error}"))
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("stat {label}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{label} must be a regular file"));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| "file length overflow".to_string())?;
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok((hex, bytes))
}

struct ToolOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

fn run_bounded(
    executable: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
) -> Result<ToolOutput, String> {
    let mut stdout_file =
        tempfile::tempfile().map_err(|error| format!("create stdout spool: {error}"))?;
    let mut stderr_file =
        tempfile::tempfile().map_err(|error| format!("create stderr spool: {error}"))?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            stdout_file
                .try_clone()
                .map_err(|error| format!("clone stdout spool: {error}"))?,
        ))
        .stderr(Stdio::from(
            stderr_file
                .try_clone()
                .map_err(|error| format!("clone stderr spool: {error}"))?,
        ))
        .spawn()
        .map_err(|error| format!("start DTS adapter {}: {error}", executable.display()))?;
    let started = Instant::now();
    let status = loop {
        let stdout_len = stdout_file
            .metadata()
            .map_err(|error| format!("stat adapter stdout: {error}"))?
            .len();
        let stderr_len = stderr_file
            .metadata()
            .map_err(|error| format!("stat adapter stderr: {error}"))?
            .len();
        if stdout_len > TOOL_OUTPUT_LIMIT as u64 || stderr_len > TOOL_OUTPUT_LIMIT as u64 {
            let _ = child.kill();
            let _ = child.wait();
            return Err("DTS adapter output exceeded its 1 MiB safety limit".into());
        }
        match child
            .try_wait()
            .map_err(|error| format!("wait for DTS adapter: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "DTS adapter exceeded the {} second timeout",
                    timeout.as_secs()
                ));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let _ = read_bounded(&mut stdout_file, TOOL_OUTPUT_LIMIT, "stdout")?;
    let stderr = read_bounded(&mut stderr_file, TOOL_OUTPUT_LIMIT, "stderr")?;
    Ok(ToolOutput { status, stderr })
}

fn read_bounded(file: &mut File, limit: usize, label: &str) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek adapter {label}: {error}"))?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read adapter {label}: {error}"))?;
    if bytes.len() > limit {
        return Err(format!("DTS adapter {label} exceeded its safety limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{named_channel_layout, PcmKind, WavWriter};

    #[test]
    fn converts_all_four_core_wire_formats() {
        let canonical = [0x7F, 0xFE, 0x80, 0x01, 0xFC, 0x7C, 0, 0];
        assert_eq!(
            canonicalize_core(&canonical, CoreWireFormat::SixteenBitBigEndian).unwrap(),
            canonical
        );
        let little = [0xFE, 0x7F, 0x01, 0x80, 0x7C, 0xFC, 0, 0];
        assert_eq!(
            canonicalize_core(&little, CoreWireFormat::SixteenBitLittleEndian).unwrap(),
            canonical
        );
        let packed = pack_fourteen(&canonical, false);
        assert_eq!(
            u32::from_be_bytes(packed[..4].try_into().unwrap()),
            CORE_14_BE
        );
        assert_eq!(
            &canonicalize_core(&packed, CoreWireFormat::FourteenBitBigEndian).unwrap()
                [..canonical.len()],
            &canonical
        );
        let packed = pack_fourteen(&canonical, true);
        assert_eq!(
            u32::from_be_bytes(packed[..4].try_into().unwrap()),
            CORE_14_LE
        );
        assert_eq!(
            &canonicalize_core(&packed, CoreWireFormat::FourteenBitLittleEndian).unwrap()
                [..canonical.len()],
            &canonical
        );
    }

    #[test]
    fn rejects_reserved_core_configuration_values() {
        assert_eq!(CORE_SAMPLE_RATES[0], 0);
        assert_eq!(PCM_RESOLUTIONS[4], 0);
        assert!(core_wire_format(0x1234_5678).is_none());
    }

    #[test]
    fn inspects_all_wire_formats_and_static_exss_counts() {
        let work = tempfile::tempdir().unwrap();
        for (name, frame) in [
            ("be", core_fixture(CoreWireFormat::SixteenBitBigEndian)),
            ("le", core_fixture(CoreWireFormat::SixteenBitLittleEndian)),
            ("14be", core_fixture(CoreWireFormat::FourteenBitBigEndian)),
            (
                "14le",
                core_fixture(CoreWireFormat::FourteenBitLittleEndian),
            ),
        ] {
            let path = work.path().join(name);
            fs::write(&path, frame).unwrap();
            let inventory = inspect_stream(&path).unwrap();
            assert_eq!(inventory.core_frame_count, 1);
            assert_eq!(
                inventory.wire_formats[0].format,
                match name {
                    "be" => CoreWireFormat::SixteenBitBigEndian,
                    "le" => CoreWireFormat::SixteenBitLittleEndian,
                    "14be" => CoreWireFormat::FourteenBitBigEndian,
                    _ => CoreWireFormat::FourteenBitLittleEndian,
                }
            );
        }

        let path = work.path().join("exss");
        fs::write(&path, exss_fixture(2, 3)).unwrap();
        let inventory = inspect_stream(&path).unwrap();
        assert_eq!(inventory.extension_substream_count, 1);
        assert_eq!(inventory.declared_presentation_count, 2);
        assert_eq!(inventory.declared_asset_count, 3);
        assert_eq!(inventory.extension_substreams[0].index, 0);

        let mut corrupt = exss_fixture(1, 1);
        corrupt[9] ^= 1;
        let path = work.path().join("bad-crc");
        fs::write(&path, corrupt).unwrap();
        assert!(inspect_stream(&path).unwrap_err().contains("CRC-16/CCITT"));
    }

    #[test]
    fn rejects_profile_count_and_path_contract_violations() {
        let inventory = core_inventory();
        let mut value = response();
        assert!(validate_response(&value, &"a".repeat(64), &inventory).is_ok());

        value.profile = DtsProfile::MasterAudio;
        assert!(validate_response(&value, &"a".repeat(64), &inventory)
            .unwrap_err()
            .contains("profile"));

        let mut value = response();
        value.asset_count = 2;
        assert!(validate_response(&value, &"a".repeat(64), &inventory)
            .unwrap_err()
            .contains("exactly"));

        let mut value = response();
        value.presentations[0].rendered_path = "../escape.wav".into();
        assert!(validate_response(&value, &"a".repeat(64), &inventory)
            .unwrap_err()
            .contains("workspace"));

        let mut value = response();
        value.presentations[0].output_layout = "5.1".into();
        assert!(validate_response(&value, &"a".repeat(64), &inventory)
            .unwrap_err()
            .contains("conflicts with its declared 2 channels"));
    }

    #[test]
    fn declared_layout_resolves_a_maskless_five_one_render() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("maskless.wav");
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames: 480,
            data: vec![vec![0.0; 480]; 6],
            channel_roles: vec![ChannelRole::Main; 6],
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&path, &source, PcmKind::F32, false).unwrap();
        let (decoded, provenance) = decoder::decode_limited_with_layout(&path, 10_000).unwrap();
        assert_eq!(provenance, decoder::ChannelLayoutProvenance::Unknown);

        let resolved = resolve_rendered_layout(&path, decoded, provenance, "5.1", 6).unwrap();
        assert_eq!(resolved.channel_roles, named_channel_layout("5.1").unwrap());
    }

    #[test]
    fn rendered_layout_resolution_fails_closed_on_ambiguous_evidence() {
        let ambiguous = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames: 1,
            data: vec![vec![0.0]; 6],
            channel_roles: vec![ChannelRole::Main; 6],
            source_kind: PcmKind::F32,
        };
        let error = resolve_rendered_layout(
            Path::new("render.wav"),
            ambiguous.clone(),
            decoder::ChannelLayoutProvenance::Unknown,
            "vendor-private",
            6,
        )
        .unwrap_err();
        assert!(error.contains("ambiguous 6-channel layout"), "{error}");

        let error = resolve_rendered_layout(
            Path::new("render.wav"),
            ambiguous.clone(),
            decoder::ChannelLayoutProvenance::SceneBased,
            "vendor-private",
            6,
        )
        .unwrap_err();
        assert!(error.contains("scene-based 6-channel audio"), "{error}");

        let error = resolve_rendered_layout(
            Path::new("render.wav"),
            ambiguous.clone(),
            decoder::ChannelLayoutProvenance::Unknown,
            "5.1",
            5,
        )
        .unwrap_err();
        assert!(error.contains("decoded 6 channels"), "{error}");

        let error = resolve_rendered_layout(
            Path::new("render.wav"),
            ambiguous,
            decoder::ChannelLayoutProvenance::Unknown,
            "stereo",
            6,
        )
        .unwrap_err();
        assert!(error.contains("layout stereo with 2 channels"), "{error}");
    }

    #[test]
    fn rendered_layout_resolution_preserves_known_stereo() {
        let stereo_roles = named_channel_layout("stereo").unwrap();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 1,
            data: vec![vec![0.0], vec![0.0]],
            channel_roles: stereo_roles.clone(),
            source_kind: PcmKind::F32,
        };
        let resolved = resolve_rendered_layout(
            Path::new("render.wav"),
            buffer,
            decoder::ChannelLayoutProvenance::KnownSpeakers,
            "stereo",
            2,
        )
        .unwrap();
        assert_eq!(resolved.channel_roles, stereo_roles);

        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("known-7.1.4.wav");
        let immersive_roles = named_channel_layout("7.1.4").unwrap();
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 12,
            frames: 1,
            data: vec![vec![0.0]; 12],
            channel_roles: immersive_roles.clone(),
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&path, &source, PcmKind::F32, false).unwrap();
        let (decoded, provenance) = decoder::decode_limited_with_layout(&path, 100).unwrap();
        assert_eq!(provenance, decoder::ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(
            decoded.channel_roles,
            crate::wav::writer::persisted_channel_roles(&immersive_roles).unwrap()
        );
        let resolved = resolve_rendered_layout(&path, decoded, provenance, "7.1.4", 12).unwrap();
        assert_eq!(resolved.channel_roles, immersive_roles);
    }

    fn response() -> AdapterResponse {
        AdapterResponse {
            schema: RESPONSE_SCHEMA.into(),
            protocol_version: 1,
            input_sha256: "a".repeat(64),
            decoder: DecoderEvidence {
                name: "reference".into(),
                version: "1".into(),
            },
            standard: STANDARD.into(),
            profile: DtsProfile::Core,
            dialog_normalization_policy: ProcessingPolicy::Disabled,
            dynamic_range_control_policy: ProcessingPolicy::Disabled,
            asset_count: 1,
            assets: vec![AssetMetadata {
                id: "core".into(),
                extension_substream_index: None,
                asset_index: 0,
                language: None,
                channels: 2,
                maximum_sample_rate_hz: 48_000,
                pcm_resolution_bits: 16,
                coding_components: vec![CodingComponent::Core],
                dialog_normalization_db: None,
            }],
            presentation_count: 1,
            presentations: vec![AdapterPresentation {
                id: "main".into(),
                asset_ids: vec!["core".into()],
                rendered_path: "main.wav".into(),
                output_layout: "stereo".into(),
                declared_sample_rate_hz: 48_000,
                declared_channels: 2,
                language: None,
                accessibility: None,
            }],
        }
    }

    fn core_inventory() -> DtsInventory {
        DtsInventory {
            stream_bytes: 96,
            frame_count: 1,
            core_frame_count: 1,
            extension_substream_count: 0,
            padding_bytes: 0,
            core_sample_blocks: 8,
            wire_formats: vec![],
            core_configurations: vec![],
            extension_substreams: vec![],
            declared_presentation_count: 1,
            declared_asset_count: 1,
        }
    }

    fn core_fixture(format: CoreWireFormat) -> Vec<u8> {
        let physical_size = if matches!(
            format,
            CoreWireFormat::FourteenBitBigEndian | CoreWireFormat::FourteenBitLittleEndian
        ) {
            110
        } else {
            96
        };
        let mut bits = TestBits::default();
        bits.push(u64::from(CORE_BE), 32);
        bits.push(1, 1);
        bits.push(31, 5);
        bits.push(0, 1);
        bits.push(7, 7);
        bits.push(physical_size - 1, 14);
        bits.push(2, 6);
        bits.push(13, 4);
        bits.push(24, 5);
        bits.push(0, 1);
        bits.push(0, 4 + 3 + 1 + 1 + 2 + 1 + 1 + 4 + 2);
        bits.push(0, 3);
        bits.push(0, 2 + 4);
        let mut canonical = bits.finish();
        canonical.resize(96, 0);
        match format {
            CoreWireFormat::SixteenBitBigEndian => canonical,
            CoreWireFormat::SixteenBitLittleEndian => {
                for pair in canonical.chunks_exact_mut(2) {
                    pair.swap(0, 1);
                }
                canonical
            }
            CoreWireFormat::FourteenBitBigEndian => pack_fourteen(&canonical, false),
            CoreWireFormat::FourteenBitLittleEndian => pack_fourteen(&canonical, true),
        }
    }

    fn exss_fixture(presentations: u8, assets: u8) -> Vec<u8> {
        let mut bits = TestBits::default();
        bits.push(u64::from(EXSS), 32);
        bits.push(0, 8);
        bits.push(0, 2);
        bits.push(0, 1);
        bits.push(11, 8); // 12-byte header including CRC-16
        bits.push(15, 16); // 16-byte frame
        bits.push(1, 1);
        bits.push(0, 2 + 3 + 1);
        bits.push(u64::from(presentations - 1), 3);
        bits.push(u64::from(assets - 1), 3);
        let mut bytes = bits.finish();
        let crc = crc16_ccitt(&bytes[5..]);
        bytes.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(bytes.len(), 12);
        assert_eq!(crc16_ccitt(&bytes[5..]), 0);
        bytes.resize(16, 0);
        bytes
    }

    fn pack_fourteen(canonical: &[u8], little: bool) -> Vec<u8> {
        let mut bits = SliceBits::new(canonical, "test");
        let groups = (canonical.len() * 8).div_ceil(14);
        let mut output = Vec::with_capacity(groups * 2);
        for group in 0..groups {
            let remaining = canonical.len() * 8 - group * 14;
            let count = remaining.min(14);
            let mut word = (bits.read(count).unwrap() << (14 - count)) as u16;
            // The standardized 14-bit sync marker carries two ignored
            // stuffing bits in its second 16-bit word.
            if group == 1 {
                word |= 0xC000;
            }
            let encoded = if little {
                word.to_le_bytes()
            } else {
                word.to_be_bytes()
            };
            output.extend_from_slice(&encoded);
        }
        output
    }

    #[derive(Default)]
    struct TestBits {
        bytes: Vec<u8>,
        current: u8,
        used: u8,
    }

    impl TestBits {
        fn push(&mut self, value: u64, count: u8) {
            for shift in (0..count).rev() {
                self.current = (self.current << 1) | ((value >> shift) as u8 & 1);
                self.used += 1;
                if self.used == 8 {
                    self.bytes.push(self.current);
                    self.current = 0;
                    self.used = 0;
                }
            }
        }

        fn finish(mut self) -> Vec<u8> {
            if self.used != 0 {
                self.current <<= 8 - self.used;
                self.bytes.push(self.current);
            }
            self.bytes
        }
    }
}
