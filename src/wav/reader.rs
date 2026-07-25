//! Minimal but robust RIFF/WAVE demuxer.
//!
//! Supports PCM (8/16/24/32-bit) and IEEE-float (32/64-bit), including files
//! that use the `WAVE_FORMAT_EXTENSIBLE` tag. Unknown chunks are skipped. The
//! whole file is read into memory once, then decoded in parallel — this is
//! deliberately I/O-optimal for the normalize use case (single sequential read).

use crate::dsp::convert;
use crate::wav::{AudioBuffer, PcmKind, WaveFormat};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
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

        // (wave_format, real_tag, sample_rate, channels, bits_per_sample)
        let mut fmt: Option<(WaveFormat, u16, u32, u16, u16)> = None;
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

        let (wformat, tag, sample_rate, channels, bits) =
            fmt.ok_or(WavReadError::BadFormat("missing fmt chunk"))?;
        if channels == 0 {
            return Err(WavReadError::ZeroChannels);
        }
        let kind = pick_kind(wformat, tag, bits)?;
        let data = data.ok_or(WavReadError::NoDataChunk)?;
        let bpp = kind.bytes_per_sample();
        let frames = data.len() / (bpp * channels as usize);

        let planar = convert::decode_planar(data, kind, channels as usize);
        let buf = AudioBuffer {
            sample_rate,
            channels,
            frames,
            data: planar,
            source_kind: kind,
        };
        Ok(buf)
    }
}

fn parse_fmt(body: &[u8]) -> Result<(WaveFormat, u16, u32, u16, u16), WavReadError> {
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
    let real_tag = if let WaveFormat::Extensible = wformat {
        if body.len() < 40 {
            return Err(WavReadError::BadFormat("extensible fmt too short"));
        }
        // SubFormat GUID: first two bytes are the underlying format tag.
        read_u16_at(body, 24)?
    } else {
        tag
    };

    Ok((wformat, real_tag, rate, channels, bits))
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
