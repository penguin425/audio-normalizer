//! RIFF/WAVE, RF64, and BW64 streaming muxer.

use crate::dsp::convert;
use crate::wav::{default_channel_roles, named_channel_layout, AudioBuffer, ChannelRole, PcmKind};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavContainer {
    Auto,
    Riff,
    Rf64,
    Bw64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveChunk {
    pub id: [u8; 4],
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum WavWriteError {
    Io(io::Error),
    Empty,
}

impl fmt::Display for WavWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavWriteError::Io(error) => write!(f, "io error: {error}"),
            WavWriteError::Empty => write!(f, "no channels/frames to write"),
        }
    }
}

impl Error for WavWriteError {}

impl From<io::Error> for WavWriteError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct WavWriter;

pub struct WavStreamWriter {
    file: File,
    kind: PcmKind,
    dither: bool,
    rngs: Vec<u64>,
    encoded: Vec<u8>,
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
        Self::create_with_options(
            path,
            sample_rate,
            channels,
            frames,
            kind,
            dither,
            WavContainer::Auto,
            &default_channel_roles(channels),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_with_options(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        frames: usize,
        kind: PcmKind,
        dither: bool,
        requested_container: WavContainer,
        channel_roles: &[ChannelRole],
        bext: Option<&[u8]>,
    ) -> Result<Self, WavWriteError> {
        let chunks = bext
            .map(|body| {
                vec![WaveChunk {
                    id: *b"bext",
                    body: body.to_vec(),
                }]
            })
            .unwrap_or_default();
        Self::create_with_metadata(
            path,
            sample_rate,
            channels,
            frames,
            kind,
            dither,
            requested_container,
            channel_roles,
            &chunks,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_with_metadata(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        frames: usize,
        kind: PcmKind,
        dither: bool,
        requested_container: WavContainer,
        channel_roles: &[ChannelRole],
        metadata_chunks: &[WaveChunk],
    ) -> Result<Self, WavWriteError> {
        if channels == 0 || frames == 0 {
            return Err(WavWriteError::Empty);
        }
        if channel_roles.len() != channels as usize {
            return Err(WavWriteError::Io(io::Error::other(
                "channel-role count does not match channel count",
            )));
        }
        let data_size = (frames as u64)
            .checked_mul(channels as u64)
            .and_then(|samples| samples.checked_mul(kind.bytes_per_sample() as u64))
            .ok_or_else(|| WavWriteError::Io(io::Error::other("audio data size overflow")))?;
        let fmt = format_chunk(sample_rate, channels, kind, channel_roles)?;
        let metadata_size: usize = metadata_chunks
            .iter()
            .map(|chunk| 8 + chunk.body.len() + (chunk.body.len() & 1))
            .sum();
        let riff_payload_size = 4u64 + fmt.len() as u64 + metadata_size as u64 + 8 + data_size;
        let container = match requested_container {
            WavContainer::Auto if riff_payload_size <= u32::MAX as u64 => WavContainer::Riff,
            WavContainer::Auto => WavContainer::Rf64,
            WavContainer::Riff if riff_payload_size > u32::MAX as u64 => {
                return Err(WavWriteError::Io(io::Error::other(
                    "RIFF/WAVE output exceeds 4 GiB; use RF64 or BW64",
                )))
            }
            value => value,
        };

        let mut file = File::create(path)?;
        write_container_header(
            &mut file,
            container,
            riff_payload_size,
            data_size,
            frames as u64,
        )?;
        file.write_all(&fmt)?;
        for chunk in metadata_chunks {
            if matches!(&chunk.id, b"fmt " | b"data" | b"ds64") {
                return Err(WavWriteError::Io(io::Error::other(
                    "reserved WAVE chunk cannot be supplied as metadata",
                )));
            }
            write_chunk(&mut file, &chunk.id, &chunk.body)?;
        }
        file.write_all(b"data")?;
        file.write_all(
            &if container == WavContainer::Riff {
                u32::try_from(data_size).expect("RIFF size checked above")
            } else {
                u32::MAX
            }
            .to_le_bytes(),
        )?;

        Ok(Self {
            file,
            kind,
            dither,
            rngs: convert::dither_rngs(channels as usize),
            encoded: Vec::new(),
            remaining_frames: frames,
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), WavWriteError> {
        let frames = self.validate_chunk(planar)?;
        convert::encode_interleaved_with_rngs_into(
            planar,
            self.kind,
            self.dither,
            &mut self.rngs,
            &mut self.encoded,
        );
        self.file.write_all(&self.encoded)?;
        self.remaining_frames -= frames;
        Ok(())
    }

    fn validate_chunk(&self, planar: &[Vec<f32>]) -> Result<usize, WavWriteError> {
        let frames = planar.first().map_or(0, Vec::len);
        if frames > self.remaining_frames {
            return Err(WavWriteError::Io(io::Error::other(
                "more frames decoded than expected",
            )));
        }
        Ok(frames)
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
    pub fn write<P: AsRef<Path>>(
        path: P,
        buffer: &AudioBuffer,
        kind: PcmKind,
        dither: bool,
    ) -> Result<(), WavWriteError> {
        Self::write_with_options(path, buffer, kind, dither, WavContainer::Auto, None)
    }

    pub fn write_with_options<P: AsRef<Path>>(
        path: P,
        buffer: &AudioBuffer,
        kind: PcmKind,
        dither: bool,
        container: WavContainer,
        bext: Option<&[u8]>,
    ) -> Result<(), WavWriteError> {
        let chunks = bext
            .map(|body| {
                vec![WaveChunk {
                    id: *b"bext",
                    body: body.to_vec(),
                }]
            })
            .unwrap_or_default();
        Self::write_with_metadata(path, buffer, kind, dither, container, &chunks)
    }

    pub fn write_with_metadata<P: AsRef<Path>>(
        path: P,
        buffer: &AudioBuffer,
        kind: PcmKind,
        dither: bool,
        container: WavContainer,
        metadata_chunks: &[WaveChunk],
    ) -> Result<(), WavWriteError> {
        let mut writer = WavStreamWriter::create_with_metadata(
            path.as_ref(),
            buffer.sample_rate,
            buffer.channels,
            buffer.frames,
            kind,
            dither,
            container,
            &buffer.channel_roles,
            metadata_chunks,
        )?;
        writer.write_chunk(&buffer.data)?;
        writer.finish()
    }
}

fn write_container_header(
    output: &mut File,
    container: WavContainer,
    riff_payload_size: u64,
    data_size: u64,
    sample_count: u64,
) -> io::Result<()> {
    match container {
        WavContainer::Riff => {
            output.write_all(b"RIFF")?;
            output.write_all(&(riff_payload_size as u32).to_le_bytes())?;
            output.write_all(b"WAVE")
        }
        WavContainer::Rf64 | WavContainer::Bw64 => {
            output.write_all(if container == WavContainer::Rf64 {
                b"RF64"
            } else {
                b"BW64"
            })?;
            output.write_all(&u32::MAX.to_le_bytes())?;
            output.write_all(b"WAVE")?;
            output.write_all(b"ds64")?;
            output.write_all(&28u32.to_le_bytes())?;
            // ds64 itself adds 36 bytes to the RIFF payload.
            output.write_all(&(riff_payload_size + 36).to_le_bytes())?;
            output.write_all(&data_size.to_le_bytes())?;
            output.write_all(&sample_count.to_le_bytes())?;
            output.write_all(&0u32.to_le_bytes())
        }
        WavContainer::Auto => unreachable!("auto container is resolved before writing"),
    }
}

fn format_chunk(
    sample_rate: u32,
    channels: u16,
    kind: PcmKind,
    roles: &[ChannelRole],
) -> io::Result<Vec<u8>> {
    let real_tag = if kind.is_float() {
        0x0003u16
    } else {
        0x0001u16
    };
    let bits = kind.bits_per_sample();
    let block_align = (channels as u32 * kind.bytes_per_sample() as u32) as u16;
    let bytes_per_second = sample_rate
        .checked_mul(block_align as u32)
        .ok_or_else(|| io::Error::other("WAV byte rate overflow"))?;
    let extensible = channels > 2;
    let mut body = Vec::with_capacity(if extensible { 40 } else { 16 });
    body.extend_from_slice(&if extensible { 0xfffeu16 } else { real_tag }.to_le_bytes());
    body.extend_from_slice(&channels.to_le_bytes());
    body.extend_from_slice(&sample_rate.to_le_bytes());
    body.extend_from_slice(&bytes_per_second.to_le_bytes());
    body.extend_from_slice(&block_align.to_le_bytes());
    body.extend_from_slice(&bits.to_le_bytes());
    if extensible {
        body.extend_from_slice(&22u16.to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body.extend_from_slice(&channel_mask(roles)?.to_le_bytes());
        body.extend_from_slice(&real_tag.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&[
            0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ]);
    }
    let mut chunk = Vec::with_capacity(8 + body.len());
    chunk.extend_from_slice(b"fmt ");
    chunk.extend_from_slice(&(body.len() as u32).to_le_bytes());
    chunk.extend_from_slice(&body);
    Ok(chunk)
}

fn channel_mask(roles: &[ChannelRole]) -> io::Result<u32> {
    for (name, mask) in [
        ("5.1", 0x0000_003f),
        ("6.1", 0x0000_070f),
        ("7.1", 0x0000_063f),
        ("5.1.4", 0x0002_d03f),
        ("7.1.4", 0x0002_d63f),
    ] {
        if named_channel_layout(name).as_deref() == Some(roles) {
            return Ok(mask);
        }
    }
    // A zero WAVE_FORMAT_EXTENSIBLE mask explicitly means that the channel
    // assignment is unspecified. Preserve that ambiguity so callers can still
    // create or inspect legacy multichannel files and require an explicit
    // layout at measurement time.
    if roles.iter().all(|role| *role == ChannelRole::Main) {
        return Ok(0);
    }
    Err(io::Error::other(
        "cannot represent channel layout in WAVE_FORMAT_EXTENSIBLE",
    ))
}

fn write_chunk(output: &mut File, id: &[u8; 4], body: &[u8]) -> io::Result<()> {
    let size = u32::try_from(body.len()).map_err(|_| io::Error::other("WAV chunk too large"))?;
    output.write_all(id)?;
    output.write_all(&size.to_le_bytes())?;
    output.write_all(body)?;
    if body.len() & 1 != 0 {
        output.write_all(&[0])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::WavReader;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn advanced_layout_masks_are_stable() {
        assert_eq!(
            channel_mask(&named_channel_layout("6.1").unwrap()).unwrap(),
            0x070f
        );
        assert_eq!(
            channel_mask(&named_channel_layout("7.1").unwrap()).unwrap(),
            0x063f
        );
        assert_eq!(
            channel_mask(&named_channel_layout("7.1.4").unwrap()).unwrap(),
            0x0002_d63f
        );
    }

    #[test]
    fn forced_rf64_and_bw64_roundtrip() {
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 480,
            data: vec![vec![0.1; 480], vec![-0.1; 480]],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        for (container, magic) in [
            (WavContainer::Rf64, *b"RF64"),
            (WavContainer::Bw64, *b"BW64"),
        ] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            WavWriter::write_with_options(
                file.path(),
                &buffer,
                PcmKind::F32,
                false,
                container,
                None,
            )
            .unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut actual = [0; 4];
            file.read_exact(&mut actual).unwrap();
            assert_eq!(actual, magic);
            let decoded = WavReader::open(file.path()).unwrap();
            assert_eq!((decoded.channels, decoded.frames), (2, 480));
        }
    }

    #[test]
    fn stream_writer_reuses_encoded_chunk_storage() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("stream.wav");
        let chunk = vec![vec![0.25; 257], vec![-0.25; 257]];
        let mut writer =
            WavStreamWriter::create(&output, 48_000, 2, chunk[0].len() * 2, PcmKind::S16, false)
                .unwrap();

        writer.write_chunk(&chunk).unwrap();
        let first_capacity = writer.encoded.capacity();
        assert!(first_capacity >= chunk[0].len() * chunk.len() * 2);
        writer.write_chunk(&chunk).unwrap();
        assert_eq!(writer.encoded.capacity(), first_capacity);
        writer.finish().unwrap();
    }
}
