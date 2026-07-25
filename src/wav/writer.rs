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

pub struct WavStreamWriter {
    file: File,
    kind: PcmKind,
    dither: bool,
    rngs: Vec<u64>,
    remaining_frames: usize,
}

impl WavStreamWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        frames: usize,
        kind: PcmKind,
        dither: bool,
    ) -> Result<Self, WavWriteError> {
        if channels == 0 || frames == 0 {
            return Err(WavWriteError::Empty);
        }
        let data_size = frames
            .checked_mul(channels as usize)
            .and_then(|samples| samples.checked_mul(kind.bytes_per_sample()))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| WavWriteError::Io(io::Error::other("WAV data exceeds 4 GiB")))?;
        let mut file = File::create(path)?;
        write_header(&mut file, sample_rate, channels, kind, data_size)?;
        Ok(Self {
            file,
            kind,
            dither,
            rngs: convert::dither_rngs(channels as usize),
            remaining_frames: frames,
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), WavWriteError> {
        let frames = planar.first().map_or(0, Vec::len);
        if frames > self.remaining_frames {
            return Err(WavWriteError::Io(io::Error::other(
                "more frames decoded than expected",
            )));
        }
        let bytes =
            convert::encode_interleaved_with_rngs(planar, self.kind, self.dither, &mut self.rngs);
        self.file.write_all(&bytes)?;
        self.remaining_frames -= frames;
        Ok(())
    }

    pub fn finish(mut self) -> Result<(), WavWriteError> {
        if self.remaining_frames != 0 {
            return Err(WavWriteError::Io(io::Error::other(
                "fewer frames decoded than expected",
            )));
        }
        self.file.flush()?;
        Ok(())
    }
}

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
    let data_size = data.len() as u32;
    write_header(w, sample_rate, channels, kind, data_size)?;
    w.write_all(data)?;
    Ok(())
}

fn write_header(
    w: &mut File,
    sample_rate: u32,
    channels: u16,
    kind: PcmKind,
    data_size: u32,
) -> io::Result<()> {
    let fmt_tag: u16 = if kind.is_float() { 0x0003 } else { 0x0001 };
    let bits = kind.bits_per_sample();
    let block_align = (channels as u32 * kind.bytes_per_sample() as u32) as u16;
    let avg_bytes = sample_rate * block_align as u32;
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
    Ok(())
}
