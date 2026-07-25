//! RIFF/WAVE muxer.
//!
//! Writes a canonical 44-byte PCM/float WAV header followed by interleaved
//! sample data. The output format tag is `WAVE_FORMAT_PCM` (0x0001) for integer
//! kinds and `WAVE_FORMAT_IEEE_FLOAT` (0x0003) for float kinds — the simplest
//! tags that every player accepts.

use crate::dsp::convert;
use crate::wav::{AudioBuffer, PcmKind};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug)]
pub enum WavWriteError {
    Io(io::Error),
    Empty,
}

impl fmt::Display for WavWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavWriteError::Io(e) => write!(f, "io error: {e}"),
            WavWriteError::Empty => write!(f, "no channels/frames to write"),
        }
    }
}
impl Error for WavWriteError {}
impl From<io::Error> for WavWriteError {
    fn from(e: io::Error) -> Self {
        WavWriteError::Io(e)
    }
}

pub struct WavWriter;

impl WavWriter {
    /// Encode `buf` as `kind` and write a WAV file to `path`.
    pub fn write<P: AsRef<Path>>(
        path: P,
        buf: &AudioBuffer,
        kind: PcmKind,
        dither: bool,
    ) -> Result<(), WavWriteError> {
        if buf.channels == 0 || buf.frames == 0 {
            return Err(WavWriteError::Empty);
        }
        let data = convert::encode_interleaved(&buf.data, kind, dither);
        let mut file = File::create(path)?;
        write_wav(&mut file, buf.sample_rate, buf.channels, kind, &data)?;
        file.flush()?;
        Ok(())
    }
}

fn write_wav(
    w: &mut File,
    sample_rate: u32,
    channels: u16,
    kind: PcmKind,
    data: &[u8],
) -> io::Result<()> {
    let fmt_tag: u16 = if kind.is_float() { 0x0003 } else { 0x0001 };
    let bits = kind.bits_per_sample();
    let block_align = (channels as u32 * kind.bytes_per_sample() as u32) as u16;
    let avg_bytes = sample_rate * block_align as u32;
    let data_size = data.len() as u32;
    let riff_size = 36u32.checked_add(data_size).expect("file too large");

    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&riff_size.to_le_bytes());
    hdr.extend_from_slice(b"WAVE");

    hdr.extend_from_slice(b"fmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    hdr.extend_from_slice(&fmt_tag.to_le_bytes());
    hdr.extend_from_slice(&channels.to_le_bytes());
    hdr.extend_from_slice(&sample_rate.to_le_bytes());
    hdr.extend_from_slice(&avg_bytes.to_le_bytes());
    hdr.extend_from_slice(&block_align.to_le_bytes());
    hdr.extend_from_slice(&bits.to_le_bytes());

    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_size.to_le_bytes());

    w.write_all(&hdr)?;
    w.write_all(data)?;
    Ok(())
}
