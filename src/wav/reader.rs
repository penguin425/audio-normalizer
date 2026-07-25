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
        let mut cur = 0usize;
        if !take(bytes, &mut cur, 4)
            .ok_or(WavReadError::Truncated)?
            .eq(b"RIFF")
        {
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

        while cur + 8 <= bytes.len() {
            let id = take(bytes, &mut cur, 4).unwrap();
            let size = read_u32(bytes, &mut cur)? as usize;
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
        let kind = pick_kind(wave_format, real_tag, bits)?;
        let data = data.ok_or(WavReadError::NoDataChunk)?;
        let bpp = kind.bytes_per_sample();
        let frames = data.len() / (bpp * channels as usize);

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
        if &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
            return Err(WavReadError::NotWave);
        }
        loop {
            let mut header = [0u8; 8];
            file.read_exact(&mut header)?;
            let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            if &header[..4] == b"fmt " {
                let mut body = vec![0; size];
                file.read_exact(&mut body)?;
                let parsed = parse_fmt(&body)?;
                let kind = pick_kind(parsed.wave_format, parsed.real_tag, parsed.bits)?;
                let channel_roles = parsed
                    .channel_mask
                    .map(|mask| roles_from_wave_mask(mask, parsed.channels))
                    .unwrap_or_else(|| default_channel_roles(parsed.channels));
                return Ok(WavStreamInfo {
                    sample_rate: parsed.sample_rate,
                    channels: parsed.channels,
                    kind,
                    channel_roles,
                });
            }
            let skip = size + (size & 1);
            file.seek(SeekFrom::Current(skip as i64))?;
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
    let mut roles = Vec::with_capacity(channels as usize);
    for bit in 0..32 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        roles.push(match bit {
            3 => ChannelRole::Lfe,
            4 | 5 | 8 | 9 | 10 | 15 | 16 | 17 => ChannelRole::Surround,
            _ => ChannelRole::Main,
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
}
