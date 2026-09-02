//! Minimal but robust RIFF/WAVE demuxer.
//!
//! Supports PCM (8/16/24/32-bit) and IEEE-float (32/64-bit), including files
//! that use the `WAVE_FORMAT_EXTENSIBLE` tag. Unknown chunks are skipped. The
//! whole file is read into memory once, then decoded in parallel — this is
//! deliberately I/O-optimal for the normalize use case (single sequential read).

use crate::dsp::convert;
use crate::wav::{default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WaveFormat};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_PROBE_CHUNKS: usize = 100_000;
const MAX_DS64_TABLE_ENTRIES: usize = 100_000;
type Ds64Table = BTreeMap<[u8; 4], VecDeque<u64>>;

#[derive(Debug)]
pub enum WavReadError {
    Io(io::Error),
    NotWave,
    Truncated,
    BadFormat(&'static str),
    UnsupportedFormatTag(u16),
    ZeroChannels,
    NoDataChunk,
}

struct ParsedFormat {
    wave_format: WaveFormat,
    real_tag: u16,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    valid_bits: u16,
    channel_mask: Option<u32>,
}

impl fmt::Display for WavReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavReadError::Io(e) => write!(f, "io error: {e}"),
            WavReadError::NotWave => write!(f, "not a RIFF/WAVE file"),
            WavReadError::Truncated => write!(f, "file truncated"),
            WavReadError::BadFormat(m) => write!(f, "bad format: {m}"),
            WavReadError::UnsupportedFormatTag(t) => write!(f, "unsupported format tag 0x{t:04X}"),
            WavReadError::ZeroChannels => write!(f, "zero channels"),
            WavReadError::NoDataChunk => write!(f, "no data chunk"),
        }
    }
}
impl Error for WavReadError {}
impl From<io::Error> for WavReadError {
    fn from(e: io::Error) -> Self {
        WavReadError::Io(e)
    }
}

pub struct WavReader;

pub struct WavStreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: PcmKind,
    pub channel_roles: Vec<ChannelRole>,
    pub data_offset: u64,
    pub data_size: u64,
}

impl WavReader {
    /// Open and fully decode a WAV file into a planar [`AudioBuffer`].
    pub fn open<P: AsRef<Path>>(path: P) -> Result<AudioBuffer, WavReadError> {
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Self::read_bytes(&bytes)
    }

    /// Decode a WAV file already held in memory.
    pub fn read_bytes(bytes: &[u8]) -> Result<AudioBuffer, WavReadError> {
        Self::read_bytes_with_limits(bytes, u16::MAX, usize::MAX)
    }

    /// Decode an in-memory WAV file while bounding allocation from untrusted
    /// channel and sample counts.
    pub fn read_bytes_with_limits(
        bytes: &[u8],
        max_channels: u16,
        max_decoded_samples: usize,
    ) -> Result<AudioBuffer, WavReadError> {
        let mut cur = 0usize;
        let container = take(bytes, &mut cur, 4).ok_or(WavReadError::Truncated)?;
        if !matches!(container, b"RIFF" | b"RF64" | b"BW64") {
            return Err(WavReadError::NotWave);
        }
        let _riff_size = read_u32(bytes, &mut cur)?; // file size - 8; ignored
        if !take(bytes, &mut cur, 4)
            .ok_or(WavReadError::Truncated)?
            .eq(b"WAVE")
        {
            return Err(WavReadError::NotWave);
        }

        // (wave_format, real_tag, sample_rate, channels, bits, channel mask)
        let mut fmt: Option<ParsedFormat> = None;
        let mut data: Option<&[u8]> = None;
        let mut ds64_data_size: Option<u64> = None;

        while cur + 8 <= bytes.len() {
            let id = take(bytes, &mut cur, 4).unwrap();
            let declared_size = read_u32(bytes, &mut cur)?;
            let size = if id == b"data" && declared_size == u32::MAX {
                usize::try_from(ds64_data_size.ok_or(WavReadError::BadFormat(
                    "RF64/BW64 data chunk is missing ds64",
                ))?)
                .map_err(|_| WavReadError::BadFormat("audio data is too large for memory"))?
            } else {
                declared_size as usize
            };
            let end = cur.checked_add(size).ok_or(WavReadError::Truncated)?;
            if end > bytes.len() {
                return Err(WavReadError::Truncated);
            }
            let body = &bytes[cur..end];
            cur = end;
            if cur & 1 != 0 {
                cur += 1; // chunks are word-aligned
            }
            match id {
                b"fmt " => fmt = Some(parse_fmt(body)?),
                b"data" => data = Some(body),
                b"ds64" if body.len() >= 16 => {
                    ds64_data_size = Some(u64::from_le_bytes(body[8..16].try_into().unwrap()));
                }
                _ => {} // skip fact, LIST, etc.
            }
        }

        let ParsedFormat {
            wave_format,
            real_tag,
            sample_rate,
            channels,
            bits,
            valid_bits: _valid_bits,
            channel_mask,
        } = fmt.ok_or(WavReadError::BadFormat("missing fmt chunk"))?;
        if channels == 0 {
            return Err(WavReadError::ZeroChannels);
        }
        if channels > max_channels {
            return Err(WavReadError::BadFormat("channel count exceeds limit"));
        }
        let kind = pick_kind(wave_format, real_tag, bits)?;
        let data = data.ok_or(WavReadError::NoDataChunk)?;
        let bpp = kind.bytes_per_sample();
        let frames = data.len() / (bpp * channels as usize);
        let decoded_samples = frames
            .checked_mul(channels as usize)
            .ok_or(WavReadError::BadFormat("decoded sample count overflow"))?;
        if decoded_samples > max_decoded_samples {
            return Err(WavReadError::BadFormat(
                "decoded sample count exceeds limit",
            ));
        }

        let planar = convert::decode_planar(data, kind, channels as usize);
        let channel_roles = channel_mask
            .map(|mask| roles_from_wave_mask(mask, channels))
            .unwrap_or_else(|| default_channel_roles(channels));
        let buf = AudioBuffer {
            sample_rate,
            channels,
            frames,
            data: planar,
            channel_roles,
            source_kind: kind,
        };
        Ok(buf)
    }

    /// Read only the RIFF headers required for streaming decode.
    pub fn probe<P: AsRef<Path>>(path: P) -> Result<WavStreamInfo, WavReadError> {
        let mut file = File::open(path)?;
        Self::probe_file(&mut file)
    }

    /// Read the streaming-decode headers from an already-open file descriptor.
    ///
    /// The descriptor is rewound before probing and is never reopened by path.
    pub(crate) fn probe_file(file: &mut File) -> Result<WavStreamInfo, WavReadError> {
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(WavReadError::BadFormat("input is not a regular file"));
        }
        let file_size = metadata.len();
        if file_size < 12 {
            return Err(WavReadError::Truncated);
        }
        file.seek(SeekFrom::Start(0))?;
        let mut riff = [0u8; 12];
        file.read_exact(&mut riff)?;
        if !matches!(&riff[..4], b"RIFF" | b"RF64" | b"BW64") || &riff[8..] != b"WAVE" {
            return Err(WavReadError::NotWave);
        }
        let uses_ds64 = matches!(&riff[..4], b"RF64" | b"BW64");
        let mut parsed_format: Option<(ParsedFormat, PcmKind)> = None;
        let mut ds64_data_size: Option<u64> = None;
        let mut ds64_table = Ds64Table::new();
        let mut offset = 12_u64;
        let mut chunk_count = 0_usize;
        loop {
            if offset == file_size {
                return Err(WavReadError::NoDataChunk);
            }
            if chunk_count >= MAX_PROBE_CHUNKS {
                return Err(WavReadError::BadFormat(
                    "WAVE chunk count exceeds safety limit",
                ));
            }
            let header_end = offset.checked_add(8).ok_or(WavReadError::Truncated)?;
            if header_end > file_size {
                return Err(WavReadError::Truncated);
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut header = [0u8; 8];
            file.read_exact(&mut header)?;
            chunk_count += 1;
            let id: [u8; 4] = header[..4].try_into().unwrap();
            let declared_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let body_offset = header_end;
            if id == *b"ds64" && !uses_ds64 {
                return Err(WavReadError::BadFormat(
                    "RIFF input must not contain a ds64 chunk",
                ));
            }
            if declared_size == u32::MAX && !uses_ds64 {
                return Err(WavReadError::BadFormat(
                    "RIFF chunk must not use the RF64/BW64 size sentinel",
                ));
            }
            let effective_size = if declared_size != u32::MAX {
                u64::from(declared_size)
            } else if id == *b"data" {
                ds64_data_size.ok_or(WavReadError::BadFormat(
                    "RF64/BW64 data chunk is missing ds64",
                ))?
            } else {
                ds64_table
                    .get_mut(&id)
                    .and_then(VecDeque::pop_front)
                    .ok_or(WavReadError::BadFormat(
                        "RF64/BW64 sentinel chunk is missing a ds64 table entry",
                    ))?
            };
            if id == *b"fmt " && effective_size > 65_536 {
                return Err(WavReadError::BadFormat(
                    "fmt chunk exceeds 64 KiB safety limit",
                ));
            }
            let body_end = body_offset
                .checked_add(effective_size)
                .ok_or(WavReadError::Truncated)?;
            if body_end > file_size {
                return Err(WavReadError::Truncated);
            }
            let next = body_end
                .checked_add(effective_size & 1)
                .ok_or(WavReadError::Truncated)?;
            if next > file_size {
                return Err(WavReadError::Truncated);
            }

            if id == *b"fmt " {
                let length = usize::try_from(effective_size)
                    .map_err(|_| WavReadError::BadFormat("fmt chunk is too large"))?;
                let mut body = vec![0; length];
                file.seek(SeekFrom::Start(body_offset))?;
                file.read_exact(&mut body)?;
                let parsed = parse_fmt(&body)?;
                let kind = pick_kind(parsed.wave_format, parsed.real_tag, parsed.bits)?;
                parsed_format = Some((parsed, kind));
            } else if id == *b"ds64" {
                if effective_size < 28 {
                    return Err(WavReadError::BadFormat("ds64 chunk too short"));
                }
                file.seek(SeekFrom::Start(body_offset))?;
                let (data_size, table) = read_probe_ds64(file, effective_size)?;
                ds64_data_size = Some(data_size);
                ds64_table = table;
            } else if id == *b"data" {
                let (parsed, kind) =
                    parsed_format.ok_or(WavReadError::BadFormat("data precedes fmt chunk"))?;
                let channel_roles = parsed
                    .channel_mask
                    .map(|mask| roles_from_wave_mask(mask, parsed.channels))
                    .unwrap_or_else(|| default_channel_roles(parsed.channels));
                return Ok(WavStreamInfo {
                    sample_rate: parsed.sample_rate,
                    channels: parsed.channels,
                    kind,
                    channel_roles,
                    data_offset: body_offset,
                    data_size: effective_size,
                });
            }
            offset = next;
        }
    }
}

fn read_probe_ds64(file: &mut File, chunk_size: u64) -> Result<(u64, Ds64Table), WavReadError> {
    let mut fixed = [0_u8; 28];
    file.read_exact(&mut fixed)?;
    let table_length = u32::from_le_bytes(fixed[24..28].try_into().unwrap());
    let table_length_usize = usize::try_from(table_length)
        .map_err(|_| WavReadError::BadFormat("ds64 table count does not fit this platform"))?;
    if table_length_usize > MAX_DS64_TABLE_ENTRIES {
        return Err(WavReadError::BadFormat(
            "ds64 table count exceeds safety limit",
        ));
    }
    let table_bytes = u64::from(table_length)
        .checked_mul(12)
        .ok_or(WavReadError::Truncated)?;
    let required_size = 28_u64
        .checked_add(table_bytes)
        .ok_or(WavReadError::Truncated)?;
    if chunk_size != required_size {
        return Err(WavReadError::BadFormat(
            "ds64 size does not match its table length",
        ));
    }

    let mut table = Ds64Table::new();
    for _ in 0..table_length_usize {
        let mut entry = [0_u8; 12];
        file.read_exact(&mut entry)?;
        let id: [u8; 4] = entry[..4].try_into().unwrap();
        let size = u64::from_le_bytes(entry[4..12].try_into().unwrap());
        table.entry(id).or_default().push_back(size);
    }
    Ok((u64::from_le_bytes(fixed[8..16].try_into().unwrap()), table))
}

fn parse_fmt(body: &[u8]) -> Result<ParsedFormat, WavReadError> {
    if body.len() < 16 {
        return Err(WavReadError::BadFormat("fmt chunk too short"));
    }
    let mut c = 0usize;
    let tag = read_u16(body, &mut c)?;
    let channels = read_u16(body, &mut c)?;
    let rate = read_u32(body, &mut c)?;
    let _avg = read_u32(body, &mut c)?;
    let _block = read_u16(body, &mut c)?;
    let bits = read_u16(body, &mut c)?;

    let wformat = WaveFormat::from_tag(tag).ok_or(WavReadError::UnsupportedFormatTag(tag))?;

    // Resolve the *real* format tag for extensible files.
    let (real_tag, valid_bits, channel_mask) = if let WaveFormat::Extensible = wformat {
        if body.len() < 18 {
            return Err(WavReadError::BadFormat("extensible fmt too short"));
        }
        let extension_size = usize::from(read_u16_at(body, 16)?);
        if extension_size < 22 || body.len() != 18 + extension_size {
            return Err(WavReadError::BadFormat(
                "extensible fmt cbSize does not match the chunk size",
            ));
        }
        let valid_bits = read_u16_at(body, 18)?;
        if valid_bits == 0 || valid_bits > bits {
            return Err(WavReadError::BadFormat(
                "extensible valid bits must be between 1 and the container bits",
            ));
        }
        const PCM_SUBFORMAT: [u8; 16] = [
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ];
        const FLOAT_SUBFORMAT: [u8; 16] = [
            3, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ];
        let subformat: [u8; 16] = body[24..40].try_into().unwrap();
        let real_tag = if subformat == PCM_SUBFORMAT {
            0x0001
        } else if subformat == FLOAT_SUBFORMAT {
            0x0003
        } else {
            return Err(WavReadError::BadFormat(
                "unsupported WAVE_FORMAT_EXTENSIBLE SubFormat GUID",
            ));
        };
        (real_tag, valid_bits, Some(read_u32_at(body, 20)?))
    } else {
        if tag == 0x0001 && !matches!(body.len(), 16 | 18) {
            return Err(WavReadError::BadFormat(
                "legacy PCM fmt must contain 16 bytes or an 18-byte zero cbSize form",
            ));
        }
        if tag == 0x0001 && body.len() == 18 && read_u16_at(body, 16)? != 0 {
            return Err(WavReadError::BadFormat(
                "legacy PCM fmt extension must have cbSize zero",
            ));
        }
        (tag, bits, None)
    };

    Ok(ParsedFormat {
        wave_format: wformat,
        real_tag,
        sample_rate: rate,
        channels,
        bits,
        valid_bits,
        channel_mask,
    })
}

pub(crate) fn roles_from_wave_mask(mask: u32, channels: u16) -> Vec<ChannelRole> {
    use ChannelRole::{Lfe, Main, Surround};
    let p = ChannelRole::positioned;
    let mut roles = Vec::with_capacity(channels as usize);
    for bit in 0..32 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        roles.push(match (bit, channels <= 6) {
            (3, _) => Lfe,
            // BS.1770 Annex 1 applies +1.5 dB to the surrounds in conventional
            // mono/stereo/5.1 programmes.
            (4 | 5 | 8 | 9 | 10, true) => Surround,
            // WAVE_FORMAT_EXTENSIBLE speaker positions used by Annex 3.
            (0, false) => p(-30, 0),
            (1, false) => p(30, 0),
            (2, false) => p(0, 0),
            (4, false) => p(-135, 0),
            (5, false) => p(135, 0),
            (8, false) => p(180, 0),
            (9, false) => p(-90, 0),
            (10, false) => p(90, 0),
            (11, false) => p(0, 90),
            (12, false) => p(-30, 45),
            (13, false) => p(0, 45),
            (14, false) => p(30, 45),
            (15, false) => p(-135, 45),
            (16, false) => p(180, 45),
            (17, false) => p(135, 45),
            _ => Main,
        });
    }
    if roles.len() == channels as usize {
        roles
    } else {
        default_channel_roles(channels)
    }
}

fn pick_kind(wformat: WaveFormat, real_tag: u16, bits: u16) -> Result<PcmKind, WavReadError> {
    let kind = match (real_tag, bits) {
        (0x0001, 8) => PcmKind::U8,
        (0x0001, 16) => PcmKind::S16,
        (0x0001, 24) => PcmKind::S24,
        (0x0001, 32) => PcmKind::S32,
        (0x0003, 32) => PcmKind::F32,
        (0x0003, 64) => PcmKind::F64,
        (0x0001, _) => return Err(WavReadError::BadFormat("unsupported PCM bit depth")),
        (0x0003, _) => return Err(WavReadError::BadFormat("unsupported float bit depth")),
        _ => return Err(WavReadError::UnsupportedFormatTag(real_tag)),
    };
    let _ = wformat;
    Ok(kind)
}

// --- little-endian readers --------------------------------------------------

#[inline]
fn take<'a>(buf: &'a [u8], cur: &mut usize, n: usize) -> Option<&'a [u8]> {
    if *cur + n <= buf.len() {
        let s = &buf[*cur..*cur + n];
        *cur += n;
        Some(s)
    } else {
        None
    }
}

#[inline]
fn read_u16(buf: &[u8], cur: &mut usize) -> Result<u16, WavReadError> {
    take(buf, cur, 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or(WavReadError::Truncated)
}

#[inline]
fn read_u32(buf: &[u8], cur: &mut usize) -> Result<u32, WavReadError> {
    take(buf, cur, 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(WavReadError::Truncated)
}

#[inline]
fn read_u16_at(buf: &[u8], off: usize) -> Result<u16, WavReadError> {
    if off + 2 <= buf.len() {
        Ok(u16::from_le_bytes([buf[off], buf[off + 1]]))
    } else {
        Err(WavReadError::Truncated)
    }
}

#[inline]
fn read_u32_at(buf: &[u8], off: usize) -> Result<u32, WavReadError> {
    if off + 4 <= buf.len() {
        Ok(u32::from_le_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ]))
    } else {
        Err(WavReadError::Truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn mono_s16_fmt() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&48_000_u32.to_le_bytes());
        body.extend_from_slice(&96_000_u32.to_le_bytes());
        body.extend_from_slice(&2_u16.to_le_bytes());
        body.extend_from_slice(&16_u16.to_le_bytes());
        body
    }

    fn extensible_pcm_fmt(valid_bits: u16) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0xfffe_u16.to_le_bytes());
        body.extend_from_slice(&2_u16.to_le_bytes());
        body.extend_from_slice(&48_000_u32.to_le_bytes());
        body.extend_from_slice(&288_000_u32.to_le_bytes());
        body.extend_from_slice(&6_u16.to_le_bytes());
        body.extend_from_slice(&24_u16.to_le_bytes());
        body.extend_from_slice(&22_u16.to_le_bytes());
        body.extend_from_slice(&valid_bits.to_le_bytes());
        body.extend_from_slice(&3_u32.to_le_bytes());
        body.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
        body
    }

    fn append_probe_chunk(bytes: &mut Vec<u8>, id: [u8; 4], declared: u32, body: &[u8]) {
        bytes.extend_from_slice(&id);
        bytes.extend_from_slice(&declared.to_le_bytes());
        bytes.extend_from_slice(body);
        if body.len() & 1 == 1 {
            bytes.push(0);
        }
    }

    fn bw64_probe_fixture(
        table: &[([u8; 4], u64)],
        chunks: &[([u8; 4], u32, Vec<u8>)],
        data_size: u64,
    ) -> Vec<u8> {
        let mut bytes = Vec::from(&b"BW64\xff\xff\xff\xffWAVE"[..]);
        let mut ds64 = vec![0_u8; 28];
        ds64[8..16].copy_from_slice(&data_size.to_le_bytes());
        ds64[24..28].copy_from_slice(&u32::try_from(table.len()).unwrap().to_le_bytes());
        for (id, size) in table {
            ds64.extend_from_slice(id);
            ds64.extend_from_slice(&size.to_le_bytes());
        }
        append_probe_chunk(
            &mut bytes,
            *b"ds64",
            u32::try_from(ds64.len()).unwrap(),
            &ds64,
        );
        for (id, declared, body) in chunks {
            append_probe_chunk(&mut bytes, *id, *declared, body);
        }
        let riff_size = u64::try_from(bytes.len() - 8).unwrap();
        bytes[20..28].copy_from_slice(&riff_size.to_le_bytes());
        bytes
    }

    #[test]
    fn wave_mask_identifies_lfe_and_surround_by_position() {
        // FL, FR, LFE, SL, SR: LFE is deliberately not channel index 3.
        let mask = 0x0000_0001 | 0x0000_0002 | 0x0000_0008 | 0x0000_0200 | 0x0000_0400;
        assert_eq!(
            roles_from_wave_mask(mask, 5),
            vec![
                ChannelRole::Main,
                ChannelRole::Main,
                ChannelRole::Lfe,
                ChannelRole::Surround,
                ChannelRole::Surround,
            ]
        );
    }

    #[test]
    fn invalid_wave_mask_falls_back_to_conventional_layout() {
        assert_eq!(roles_from_wave_mask(0x3, 6), default_channel_roles(6));
    }

    #[test]
    fn advanced_wave_mask_uses_position_dependent_roles() {
        // FL, FR, FC, LFE, BL, BR, SL, SR.
        let roles = roles_from_wave_mask(0x0000_063f, 8);
        assert_eq!(roles[3], ChannelRole::Lfe);
        assert_eq!(roles[4], ChannelRole::positioned(-135, 0));
        assert_eq!(roles[5], ChannelRole::positioned(135, 0));
        assert_eq!(roles[6], ChannelRole::positioned(-90, 0));
        assert_eq!(roles[7], ChannelRole::positioned(90, 0));
    }

    #[test]
    fn extensible_pcm_requires_consistent_extension_valid_bits_and_exact_guid() {
        let valid = parse_fmt(&extensible_pcm_fmt(20)).unwrap();
        assert_eq!(valid.real_tag, 1);
        assert_eq!(valid.bits, 24);
        assert_eq!(valid.valid_bits, 20);
        assert_eq!(valid.channel_mask, Some(3));

        let mut cb_size_zero = extensible_pcm_fmt(20);
        cb_size_zero[16..18].copy_from_slice(&0_u16.to_le_bytes());
        assert!(parse_fmt(&cb_size_zero).is_err());

        let mut bogus_guid = extensible_pcm_fmt(20);
        bogus_guid[26] = 1;
        assert!(parse_fmt(&bogus_guid).is_err());

        for invalid in [0, 25] {
            assert!(parse_fmt(&extensible_pcm_fmt(invalid)).is_err());
        }
    }

    #[test]
    fn legacy_pcm_accepts_only_canonical_fmt_chunk_sizes() {
        assert!(parse_fmt(&mono_s16_fmt()).is_ok());

        let mut zero_cb_size = mono_s16_fmt();
        zero_cb_size.extend_from_slice(&0_u16.to_le_bytes());
        assert!(parse_fmt(&zero_cb_size).is_ok());

        let mut nonzero_cb_size = mono_s16_fmt();
        nonzero_cb_size.extend_from_slice(&1_u16.to_le_bytes());
        assert!(parse_fmt(&nonzero_cb_size).is_err());

        let mut trailing_garbage = mono_s16_fmt();
        trailing_garbage.extend_from_slice(&[0; 24]);
        assert!(parse_fmt(&trailing_garbage).is_err());
    }

    #[test]
    fn public_reader_apis_reject_packed_20_bit_pcm() {
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1_u16.to_le_bytes());
        fmt.extend_from_slice(&1_u16.to_le_bytes());
        fmt.extend_from_slice(&48_000_u32.to_le_bytes());
        fmt.extend_from_slice(&144_000_u32.to_le_bytes());
        fmt.extend_from_slice(&3_u16.to_le_bytes());
        fmt.extend_from_slice(&20_u16.to_le_bytes());

        let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
        append_probe_chunk(&mut bytes, *b"fmt ", fmt.len() as u32, &fmt);
        append_probe_chunk(&mut bytes, *b"data", 3, &[0; 3]);
        let riff_size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());

        assert!(matches!(
            WavReader::read_bytes(&bytes),
            Err(WavReadError::BadFormat("unsupported PCM bit depth"))
        ));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("packed-20.wav");
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            WavReader::probe(&path),
            Err(WavReadError::BadFormat("unsupported PCM bit depth"))
        ));
        assert!(matches!(
            WavReader::open(path),
            Err(WavReadError::BadFormat("unsupported PCM bit depth"))
        ));
    }

    #[test]
    fn probe_restricts_ds64_and_sentinel_sizes_to_large_wave_containers() {
        let chunks = [
            (*b"fmt ", 16, mono_s16_fmt()),
            (*b"data", u32::MAX, vec![0; 2]),
        ];
        for container in [*b"BW64", *b"RF64"] {
            let mut bytes = bw64_probe_fixture(&[], &chunks, 2);
            bytes[..4].copy_from_slice(&container);
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("large.wav");
            std::fs::write(&path, bytes).unwrap();
            assert!(WavReader::probe(path).is_ok());
        }

        let mut disguised = bw64_probe_fixture(&[], &chunks, 2);
        disguised[..4].copy_from_slice(b"RIFF");
        let riff_size = u32::try_from(disguised.len() - 8).unwrap();
        disguised[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("riff-ds64.wav");
        std::fs::write(&path, disguised).unwrap();
        assert!(WavReader::probe(path).is_err());

        let mut regular = b"RIFF\0\0\0\0WAVE".to_vec();
        append_probe_chunk(&mut regular, *b"JUNK", 3, &[0; 3]);
        append_probe_chunk(&mut regular, *b"fmt ", 16, &mono_s16_fmt());
        append_probe_chunk(&mut regular, *b"data", 2, &[0; 2]);
        let riff_size = u32::try_from(regular.len() - 8).unwrap();
        regular[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("riff-junk.wav");
        std::fs::write(&path, regular).unwrap();
        assert!(WavReader::probe(path).is_ok());
    }

    #[test]
    fn probe_rejects_oversized_format_chunk_before_allocating_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.wav");
        let mut bytes = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        bytes.extend_from_slice(&65_537_u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            WavReader::probe(path),
            Err(WavReadError::BadFormat(
                "fmt chunk exceeds 64 KiB safety limit"
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn probe_file_uses_the_open_descriptor_after_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.wav");
        let moved_path = directory.path().join("opened.wav");
        let mut wave = b"RIFF\x2c\0\0\0WAVEfmt \x10\0\0\0".to_vec();
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&48_000_u32.to_le_bytes());
        wave.extend_from_slice(&96_000_u32.to_le_bytes());
        wave.extend_from_slice(&2_u16.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        wave.extend_from_slice(b"data\x08\0\0\0\0\0\0\0\0\0\0\0");
        std::fs::write(&path, wave).unwrap();

        let mut opened = File::open(&path).unwrap();
        opened.seek(SeekFrom::End(0)).unwrap();
        std::fs::rename(&path, &moved_path).unwrap();
        std::fs::write(&path, b"not-a-wave!!").unwrap();

        let info = WavReader::probe_file(&mut opened).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.data_size, 8);
        assert!(matches!(
            WavReader::probe(&path),
            Err(WavReadError::NotWave)
        ));
    }

    #[test]
    fn probe_resolves_fifo_ds64_sizes_for_sentinel_ancillary_chunks() {
        let bytes = bw64_probe_fixture(
            &[(*b"axml", 3), (*b"axml", 5)],
            &[
                (*b"axml", u32::MAX, b"one".to_vec()),
                (*b"axml", u32::MAX, b"three".to_vec()),
                (*b"fmt ", 16, mono_s16_fmt()),
                (*b"data", u32::MAX, vec![0; 4]),
            ],
            4,
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sentinel-ancillary.bw64");
        std::fs::write(&path, bytes).unwrap();

        let info = WavReader::probe(path).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.data_size, 4);
    }

    #[test]
    fn probe_rejects_sentinel_ancillary_without_a_ds64_table_entry() {
        let bytes = bw64_probe_fixture(
            &[],
            &[
                (*b"axml", u32::MAX, b"xml".to_vec()),
                (*b"fmt ", 16, mono_s16_fmt()),
                (*b"data", u32::MAX, vec![0; 4]),
            ],
            4,
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-table-entry.bw64");
        std::fs::write(&path, bytes).unwrap();

        assert!(matches!(
            WavReader::probe(path),
            Err(WavReadError::BadFormat(
                "RF64/BW64 sentinel chunk is missing a ds64 table entry"
            ))
        ));
    }

    #[test]
    fn probe_rejects_chunk_bodies_and_pads_beyond_the_descriptor_length() {
        let directory = tempfile::tempdir().unwrap();
        let body_path = directory.path().join("truncated-body.wav");
        let mut body = Vec::from(&b"RIFF\0\0\0\0WAVEJUNK\x04\0\0\0\x01\x02"[..]);
        let riff_size = u32::try_from(body.len() - 8).unwrap();
        body[4..8].copy_from_slice(&riff_size.to_le_bytes());
        std::fs::write(&body_path, body).unwrap();
        assert!(matches!(
            WavReader::probe(body_path),
            Err(WavReadError::Truncated)
        ));

        let pad_path = directory.path().join("missing-pad.wav");
        let mut pad = Vec::from(&b"RIFF\0\0\0\0WAVEJUNK\x03\0\0\0\x01\x02\x03"[..]);
        let riff_size = u32::try_from(pad.len() - 8).unwrap();
        pad[4..8].copy_from_slice(&riff_size.to_le_bytes());
        std::fs::write(&pad_path, pad).unwrap();
        assert!(matches!(
            WavReader::probe(pad_path),
            Err(WavReadError::Truncated)
        ));
    }

    #[test]
    fn in_memory_limits_are_checked_before_pcm_allocation() {
        let mut wave = b"RIFF\x28\0\0\0WAVEfmt \x10\0\0\0".to_vec();
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&48_000_u32.to_le_bytes());
        wave.extend_from_slice(&96_000_u32.to_le_bytes());
        wave.extend_from_slice(&2_u16.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        wave.extend_from_slice(b"data\x08\0\0\0\0\0\0\0\0\0\0\0");

        assert!(matches!(
            WavReader::read_bytes_with_limits(&wave, 0, usize::MAX),
            Err(WavReadError::BadFormat("channel count exceeds limit"))
        ));
        assert!(matches!(
            WavReader::read_bytes_with_limits(&wave, 1, 3),
            Err(WavReadError::BadFormat(
                "decoded sample count exceeds limit"
            ))
        ));
        assert_eq!(
            WavReader::read_bytes_with_limits(&wave, 1, 4)
                .unwrap()
                .frames,
            4
        );
    }

    proptest! {
        #[test]
        fn arbitrary_wave_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let _ = WavReader::read_bytes(&bytes);

            let mut wave = b"RIFF\0\0\0\0WAVE".to_vec();
            wave.extend_from_slice(&bytes);
            let _ = WavReader::read_bytes(&wave);
        }
    }
}
