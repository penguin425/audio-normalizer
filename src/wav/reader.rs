//! Minimal but robust RIFF/WAVE demuxer.
//!
//! Supports PCM (8/16/24/32-bit) and IEEE-float (32/64-bit), including files
//! that use the `WAVE_FORMAT_EXTENSIBLE` tag. Unknown chunks are skipped. The
//! whole file is read into memory once, then decoded in parallel — this is
//! deliberately I/O-optimal for the normalize use case (single sequential read).

use crate::dsp::convert;
use crate::wav::{default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WaveFormat};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

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
        let mut riff = [0u8; 12];
        file.read_exact(&mut riff)?;
        if !matches!(&riff[..4], b"RIFF" | b"RF64" | b"BW64") || &riff[8..] != b"WAVE" {
            return Err(WavReadError::NotWave);
        }
        let mut parsed_format: Option<(ParsedFormat, PcmKind)> = None;
        let mut ds64_data_size: Option<u64> = None;
        loop {
            let mut header = [0u8; 8];
            file.read_exact(&mut header)?;
            let declared_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let body_offset = file.stream_position()?;
            if &header[..4] == b"fmt " {
                if declared_size > 65_536 {
                    return Err(WavReadError::BadFormat(
                        "fmt chunk exceeds 64 KiB safety limit",
                    ));
                }
                let mut body = vec![0; declared_size as usize];
                file.read_exact(&mut body)?;
                let parsed = parse_fmt(&body)?;
                let kind = pick_kind(parsed.wave_format, parsed.real_tag, parsed.bits)?;
                parsed_format = Some((parsed, kind));
            } else if &header[..4] == b"ds64" {
                if declared_size < 16 {
                    return Err(WavReadError::BadFormat("ds64 chunk too short"));
                }
                let mut prefix = [0u8; 16];
                file.read_exact(&mut prefix)?;
                ds64_data_size = Some(u64::from_le_bytes(prefix[8..16].try_into().unwrap()));
            } else if &header[..4] == b"data" {
                let data_size = if declared_size == u32::MAX {
                    ds64_data_size.ok_or(WavReadError::BadFormat(
                        "RF64/BW64 data chunk is missing ds64",
                    ))?
                } else {
                    declared_size as u64
                };
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
                    data_size,
                });
            }
            let next = body_offset
                .checked_add(declared_size as u64)
                .and_then(|offset| offset.checked_add((declared_size & 1) as u64))
                .ok_or(WavReadError::Truncated)?;
            file.seek(SeekFrom::Start(next))?;
        }
    }
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
    let (real_tag, channel_mask) = if let WaveFormat::Extensible = wformat {
        if body.len() < 40 {
            return Err(WavReadError::BadFormat("extensible fmt too short"));
        }
        // SubFormat GUID: first two bytes are the underlying format tag.
        (read_u16_at(body, 24)?, Some(read_u32_at(body, 20)?))
    } else {
        (tag, None)
    };

    Ok(ParsedFormat {
        wave_format: wformat,
        real_tag,
        sample_rate: rate,
        channels,
        bits,
        channel_mask,
    })
}

fn roles_from_wave_mask(mask: u32, channels: u16) -> Vec<ChannelRole> {
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
