//! Dependency-free AC-3 and E-AC-3 elementary-stream QC.

use crate::container_qc::{check, finish_audit, ContainerAudit};
use serde_json::json;
use std::collections::BTreeSet;
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
    stream_type: Option<u8>,
    substream_id: Option<u8>,
    channel_map: Option<u16>,
}

#[derive(Debug, Default)]
struct State {
    format: Option<Format>,
    frames: u64,
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
    acmod: Option<u8>,
    lfe: Option<bool>,
    dialnorms: BTreeSet<u8>,
    compression_words: BTreeSet<u8>,
    stream_types: BTreeSet<u8>,
    substream_ids: BTreeSet<u8>,
    channel_maps: BTreeSet<u16>,
    dependent_frames: u64,
    current_independent: Option<(u32, u8)>,
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
        if state.format.is_some_and(|first| first != format) {
            state.config_valid = false;
        } else {
            state.format.get_or_insert(format);
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
        if info.stream_type != Some(1) {
            state.decoded_samples += u64::from(info.blocks) * 256;
        }
        offset += frame_bytes as u64;
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
            Some(json!({"bsid": state.bsid, "sample_rate_hz": state.sample_rate})),
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
            state.substreams_valid,
            "dependent substreams follow a compatible independent substream",
            Some(json!({
                "dependent_frames": state.dependent_frames,
                "stream_types": state.stream_types,
                "substream_ids": state.substream_ids,
                "channel_maps": state.channel_maps,
            })),
        ));
    }
    let xcheck = vec![check(
        if format == Format::Ac3 {
            "FORGE-AC3-DIALNORM"
        } else {
            "FORGE-EAC3-DIALNORM"
        },
        state.frames > 0 && !state.dialnorms.contains(&0),
        "dialnorm is present and uses a valid -1 through -31 dB code",
        Some(json!({
            "dialnorm_db": state.dialnorms.iter().map(|value| -i16::from(*value)).collect::<Vec<_>>(),
            "compression_control_words": state.compression_words,
        })),
    )];
    debug_assert!(wrapper.iter().all(|item| item.rule_id.starts_with(prefix)));
    Ok(finish_audit(
        path,
        format.name(),
        std::mem::take(&mut wrapper),
        bitstream,
        xcheck,
        json!({
            "frames": state.frames,
            "bytes": state.bytes,
            "sample_rate_hz": state.sample_rate,
            "decoded_samples_per_channel": state.decoded_samples,
            "duration_seconds": state.sample_rate.map(|rate| state.decoded_samples as f64 / f64::from(rate)),
            "bsid": state.bsid,
            "channel_mode": state.acmod.map(channel_mode),
            "channels": state.acmod.map(|mode| channel_count(mode, state.lfe.unwrap_or(false))),
            "lfe": state.lfe,
            "dialnorm_db": state.dialnorms.iter().map(|value| -i16::from(*value)).collect::<Vec<_>>(),
            "compression_control_words": state.compression_words,
            "stream_types": state.stream_types,
            "substream_ids": state.substream_ids,
            "dependent_frames": state.dependent_frames,
            "channel_maps": state.channel_maps,
            "atmos_joc_signaling": "not asserted by the core syncframe fields",
        }),
    ))
}

fn update_state(state: &mut State, info: &FrameInfo) {
    if state
        .sample_rate
        .is_some_and(|value| value != info.sample_rate)
        || state.bsid.is_some_and(|value| value != info.bsid)
    {
        state.config_valid = false;
    }
    let primary_presentation = info.stream_type != Some(1);
    if primary_presentation
        && (state.acmod.is_some_and(|value| value != info.acmod)
            || state.lfe.is_some_and(|value| value != info.lfe))
    {
        state.config_valid = false;
    }
    state.sample_rate.get_or_insert(info.sample_rate);
    state.bsid.get_or_insert(info.bsid);
    if primary_presentation {
        state.acmod.get_or_insert(info.acmod);
        state.lfe.get_or_insert(info.lfe);
    }
    state.dialnorms.insert(info.dialnorm);
    if let Some(value) = info.compression_word {
        state.compression_words.insert(value);
    }
    if let Some(stream_type) = info.stream_type {
        state.stream_types.insert(stream_type);
        let key = (info.sample_rate, info.blocks);
        if stream_type == 1 {
            state.dependent_frames += 1;
            if state.current_independent != Some(key) {
                state.substreams_valid = false;
            }
        } else if stream_type == 0 || stream_type == 2 {
            if state.current_independent.is_none() && info.substream_id != Some(0) {
                state.substreams_valid = false;
            }
            state.current_independent = Some(key);
        } else {
            state.substreams_valid = false;
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
    if acmod == 0 {
        bits.skip(5)?;
        if bits.read(1)? != 0 {
            bits.skip(8)?;
        }
    }
    if fscod >= SAMPLE_RATES.len() || frmsizecod > 37 || bsid > 10 || dialnorm == 0 {
        return Err(());
    }
    Ok(FrameInfo {
        frame_bytes: frame.len(),
        sample_rate: SAMPLE_RATES[fscod],
        blocks: 6,
        bsid,
        acmod,
        lfe,
        dialnorm,
        compression_word,
        stream_type: None,
        substream_id: None,
        channel_map: None,
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
    let (sample_rate, blocks) = if fscod == 3 {
        let fscod2 = bits.read(2)? as usize;
        (*HALF_SAMPLE_RATES.get(fscod2).ok_or(())?, 6)
    } else {
        let code = bits.read(2)? as usize;
        let blocks = *[1_u8, 2, 3, 6].get(code).ok_or(())?;
        (*SAMPLE_RATES.get(fscod).ok_or(())?, blocks)
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
    if acmod == 0 {
        bits.skip(5)?;
        if bits.read(1)? != 0 {
            bits.skip(8)?;
        }
    }
    let channel_map = if stream_type == 1 && bits.read(1)? != 0 {
        Some(bits.read(16)? as u16)
    } else {
        None
    };
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
        stream_type: Some(stream_type),
        substream_id: Some(substream_id),
        channel_map,
    })
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
    }
}
