//! RIFF/WAVE, RF64, and BW64 streaming muxer.

use crate::channel_layout::{
    ChannelAssignment, ChannelAssignmentKind, ChannelLayoutDescriptor, ChannelLayoutOrigin,
};
use crate::dsp::convert;
use crate::wav::{
    default_channel_roles, named_channel_layout, AudioBuffer, ChannelRole, PcmKind,
    MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ,
};
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
    data_padding: bool,
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
        Self::create_with_metadata_and_mask(
            path,
            sample_rate,
            channels,
            frames,
            kind,
            dither,
            requested_container,
            channel_roles,
            metadata_chunks,
            None,
        )
    }

    /// Create a streaming WAVE writer from an exact channel-layout sidecar.
    ///
    /// A raw WAVE or RFC 9639 FLAC mask is reused byte-for-byte, including
    /// zero and partial masks. Other complete speaker layouts are converted to
    /// a WAVE mask only when every assignment has an exact representation.
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_channel_layout(
        path: &Path,
        sample_rate: u32,
        frames: usize,
        kind: PcmKind,
        dither: bool,
        requested_container: WavContainer,
        channel_layout: &ChannelLayoutDescriptor,
    ) -> Result<Self, WavWriteError> {
        Self::create_with_channel_layout_and_metadata(
            path,
            sample_rate,
            frames,
            kind,
            dither,
            requested_container,
            channel_layout,
            &[],
        )
    }

    /// Create a streaming WAVE writer with exact layout and metadata chunks.
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_channel_layout_and_metadata(
        path: &Path,
        sample_rate: u32,
        frames: usize,
        kind: PcmKind,
        dither: bool,
        requested_container: WavContainer,
        channel_layout: &ChannelLayoutDescriptor,
        metadata_chunks: &[WaveChunk],
    ) -> Result<Self, WavWriteError> {
        channel_layout
            .validate()
            .map_err(|error| WavWriteError::Io(io::Error::other(error)))?;
        let channels = u16::try_from(channel_layout.assignments().len())
            .map_err(|_| WavWriteError::Io(io::Error::other("too many WAVE channels")))?;
        let roles = channel_layout.channel_roles();
        // Conventional mono/stereo has a normative implicit assignment in a
        // non-extensible WAVE header. Keep it implicit whenever an exact
        // descriptor without explicit mask evidence resolves to that default,
        // including RFC 9639 FLAC's implicit channel order. Converting it into
        // an extensible header would needlessly change established output
        // bytes even though it carries no additional layout information.
        let implicit_default_layout = channels <= 2
            && channel_layout.wave_channel_mask().is_none()
            && channel_layout.flac_channel_mask().is_none()
            && channel_mask_from_descriptor(channel_layout).is_ok_and(|mask| {
                Some(mask) == crate::channel_layout::default_flac_channel_mask(channels)
            });
        let unmasked_wave_source = channel_layout.origin() == ChannelLayoutOrigin::Wave
            && channel_layout.wave_channel_mask().is_none();
        let mask = if implicit_default_layout || unmasked_wave_source {
            None
        } else {
            Some(channel_mask_from_descriptor(channel_layout)?)
        };
        Self::create_with_metadata_and_mask(
            path,
            sample_rate,
            channels,
            frames,
            kind,
            dither,
            requested_container,
            &roles,
            metadata_chunks,
            mask,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_with_metadata_and_mask(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        frames: usize,
        kind: PcmKind,
        dither: bool,
        requested_container: WavContainer,
        channel_roles: &[ChannelRole],
        metadata_chunks: &[WaveChunk],
        exact_channel_mask: Option<u32>,
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
        let fmt = format_chunk(
            sample_rate,
            channels,
            kind,
            channel_roles,
            exact_channel_mask,
        )?;
        let metadata_size = validate_metadata_chunks(metadata_chunks)?;
        let riff_payload_size =
            4_u64
                .checked_add(u64::try_from(fmt.len()).map_err(|_| {
                    WavWriteError::Io(io::Error::other("WAVE fmt chunk size overflow"))
                })?)
                .and_then(|size| size.checked_add(metadata_size))
                .and_then(|size| size.checked_add(8))
                .and_then(|size| size.checked_add(data_size))
                .and_then(|size| size.checked_add(data_size & 1))
                .ok_or_else(|| WavWriteError::Io(io::Error::other("WAVE file size overflow")))?;
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
        if matches!(container, WavContainer::Rf64 | WavContainer::Bw64) {
            riff_payload_size.checked_add(36).ok_or_else(|| {
                WavWriteError::Io(io::Error::other("RF64/BW64 file size overflow"))
            })?;
        }

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
            data_padding: data_size & 1 != 0,
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

    pub(crate) fn supports_borrowed_planar(&self) -> bool {
        !self.dither
            && (self.kind == PcmKind::F32 || (self.kind == PcmKind::S16 && self.rngs.len() <= 2))
    }

    pub(crate) fn write_normalized_borrowed_chunk(
        &mut self,
        planar: &[&[f32]],
        gain: f32,
        ceiling: f32,
    ) -> Result<(), WavWriteError> {
        if !self.supports_borrowed_planar() {
            return Err(WavWriteError::Io(io::Error::other(
                "borrowed planar encoding is unavailable for this WAVE format",
            )));
        }
        let frames = self.validate_borrowed_chunk(planar)?;
        convert::encode_normalized_borrowed_interleaved_with_rngs_into(
            planar,
            self.kind,
            gain,
            ceiling,
            &mut self.rngs,
            &mut self.encoded,
        );
        self.file.write_all(&self.encoded)?;
        self.remaining_frames -= frames;
        Ok(())
    }

    /// Exact interleaved PCM bytes produced by the most recent successful
    /// [`Self::write_chunk`] call.
    pub(crate) fn last_encoded_chunk(&self) -> &[u8] {
        &self.encoded
    }

    fn validate_chunk(&self, planar: &[Vec<f32>]) -> Result<usize, WavWriteError> {
        if planar.len() != self.rngs.len() {
            return Err(WavWriteError::Io(io::Error::other(
                "channel count does not match WAVE output",
            )));
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err(WavWriteError::Io(io::Error::other(
                "channel lengths do not match",
            )));
        }
        if frames > self.remaining_frames {
            return Err(WavWriteError::Io(io::Error::other(
                "more frames decoded than expected",
            )));
        }
        Ok(frames)
    }

    fn validate_borrowed_chunk(&self, planar: &[&[f32]]) -> Result<usize, WavWriteError> {
        if planar.len() != self.rngs.len() {
            return Err(WavWriteError::Io(io::Error::other(
                "channel count does not match WAVE output",
            )));
        }
        let frames = planar.first().map_or(0, |channel| channel.len());
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err(WavWriteError::Io(io::Error::other(
                "channel lengths do not match",
            )));
        }
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
        if self.data_padding {
            self.file.write_all(&[0])?;
        }
        self.file.flush()?;
        Ok(())
    }
}

fn validate_metadata_chunks(chunks: &[WaveChunk]) -> Result<u64, WavWriteError> {
    let mut total = 0_u64;
    for chunk in chunks {
        if matches!(&chunk.id, b"fmt " | b"data" | b"ds64") {
            return Err(WavWriteError::Io(io::Error::other(
                "reserved WAVE chunk cannot be supplied as metadata",
            )));
        }
        let body_size = u32::try_from(chunk.body.len())
            .map_err(|_| WavWriteError::Io(io::Error::other("WAVE metadata chunk is too large")))?;
        let encoded_size = 8_u64
            .checked_add(u64::from(body_size))
            .and_then(|size| size.checked_add(u64::from(body_size & 1)))
            .ok_or_else(|| WavWriteError::Io(io::Error::other("WAVE metadata size overflow")))?;
        total = total
            .checked_add(encoded_size)
            .ok_or_else(|| WavWriteError::Io(io::Error::other("WAVE metadata size overflow")))?;
    }
    Ok(total)
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
        if buffer.data.len() != usize::from(buffer.channels)
            || buffer
                .data
                .iter()
                .any(|channel| channel.len() != buffer.frames)
        {
            return Err(WavWriteError::Io(io::Error::other(
                "audio buffer geometry does not match its channel/frame declaration",
            )));
        }
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

    /// Write a complete buffer while preserving an exact channel-layout
    /// declaration.
    pub fn write_with_channel_layout<P: AsRef<Path>>(
        path: P,
        buffer: &AudioBuffer,
        kind: PcmKind,
        dither: bool,
        container: WavContainer,
        channel_layout: &ChannelLayoutDescriptor,
    ) -> Result<(), WavWriteError> {
        Self::write_with_channel_layout_and_metadata(
            path,
            buffer,
            kind,
            dither,
            container,
            channel_layout,
            &[],
        )
    }

    /// Write a complete buffer with an exact layout and metadata chunks.
    #[allow(clippy::too_many_arguments)]
    pub fn write_with_channel_layout_and_metadata<P: AsRef<Path>>(
        path: P,
        buffer: &AudioBuffer,
        kind: PcmKind,
        dither: bool,
        container: WavContainer,
        channel_layout: &ChannelLayoutDescriptor,
        metadata_chunks: &[WaveChunk],
    ) -> Result<(), WavWriteError> {
        validate_audio_buffer(buffer)?;
        if channel_layout.assignments().len() != usize::from(buffer.channels) {
            return Err(WavWriteError::Io(io::Error::other(
                "channel-layout count does not match audio buffer",
            )));
        }
        if channel_layout.channel_roles() != buffer.channel_roles {
            return Err(WavWriteError::Io(io::Error::other(
                "channel layout does not match audio buffer roles",
            )));
        }
        let mut writer = WavStreamWriter::create_with_channel_layout_and_metadata(
            path.as_ref(),
            buffer.sample_rate,
            buffer.frames,
            kind,
            dither,
            container,
            channel_layout,
            metadata_chunks,
        )?;
        writer.write_chunk(&buffer.data)?;
        writer.finish()
    }
}

fn validate_audio_buffer(buffer: &AudioBuffer) -> Result<(), WavWriteError> {
    if buffer.data.len() != usize::from(buffer.channels)
        || buffer
            .data
            .iter()
            .any(|channel| channel.len() != buffer.frames)
    {
        return Err(WavWriteError::Io(io::Error::other(
            "audio buffer geometry does not match its channel/frame declaration",
        )));
    }
    Ok(())
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
    exact_channel_mask: Option<u32>,
) -> io::Result<Vec<u8>> {
    if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&sample_rate) {
        return Err(io::Error::other(
            "WAVE sample rate is outside the supported 8000..=384000 Hz range",
        ));
    }
    let real_tag = if kind.is_float() {
        0x0003u16
    } else {
        0x0001u16
    };
    let bits = kind.bits_per_sample();
    let block_align = u16::try_from(
        u32::from(channels)
            .checked_mul(kind.bytes_per_sample() as u32)
            .ok_or_else(|| io::Error::other("WAVE block align overflow"))?,
    )
    .map_err(|_| io::Error::other("WAVE block align exceeds 16 bits"))?;
    let bytes_per_second = sample_rate
        .checked_mul(block_align as u32)
        .ok_or_else(|| io::Error::other("WAV byte rate overflow"))?;
    let extensible = channels > 2 || exact_channel_mask.is_some();
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
        let mask = match exact_channel_mask {
            Some(mask) => mask,
            None => channel_mask(roles)?,
        };
        body.extend_from_slice(&mask.to_le_bytes());
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

pub(crate) fn channel_mask_from_descriptor(layout: &ChannelLayoutDescriptor) -> io::Result<u32> {
    let channels = layout.assignments().len();
    if let Some(mask) = layout.wave_channel_mask().or(layout.flac_channel_mask()) {
        if mask.count_ones() as usize > channels {
            return Err(io::Error::other(
                "channel mask assigns more speakers than the channel layout contains",
            ));
        }
        return Ok(mask);
    }
    if channels <= 2
        && layout
            .assignments()
            .iter()
            .all(|assignment| assignment.kind() == ChannelAssignmentKind::LegacyRole)
        && layout.channel_roles() == default_channel_roles(channels as u16)
    {
        return crate::channel_layout::default_flac_channel_mask(channels as u16)
            .ok_or_else(|| io::Error::other("implicit channel layout has no WAVE mask"));
    }

    let mut mask = 0_u32;
    let mut exact = true;
    for assignment in layout.assignments() {
        let bit = wave_bit_from_assignment(assignment);
        let Some(bit) = bit else {
            exact = false;
            break;
        };
        let flag = 1_u32 << bit;
        if mask & flag != 0 {
            return Err(io::Error::other(
                "channel layout assigns the same WAVE speaker more than once",
            ));
        }
        mask |= flag;
    }
    if exact {
        return Ok(mask);
    }

    channel_mask(&layout.channel_roles())
}

fn wave_bit_from_assignment(assignment: &ChannelAssignment) -> Option<u8> {
    if let Some(position) = assignment.cicp_position() {
        return cicp_to_wave_bit(position);
    }
    match assignment.kind() {
        ChannelAssignmentKind::LowFrequencyEffects => Some(3),
        ChannelAssignmentKind::Speaker => {
            wave_bit_from_coordinates(assignment.azimuth_degrees(), assignment.elevation_degrees())
        }
        ChannelAssignmentKind::LegacyRole => match assignment.channel_role() {
            ChannelRole::Lfe => Some(3),
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } => wave_bit_from_coordinates(Some(azimuth_degrees), Some(elevation_degrees)),
            _ => None,
        },
        _ => None,
    }
}

const fn wave_bit_from_coordinates(
    azimuth_degrees: Option<i16>,
    elevation_degrees: Option<i16>,
) -> Option<u8> {
    match (azimuth_degrees, elevation_degrees) {
        (Some(-30), Some(0)) => Some(0),
        (Some(30), Some(0)) => Some(1),
        (Some(0), Some(0)) => Some(2),
        (Some(-135 | -150), Some(0)) => Some(4),
        (Some(135 | 150), Some(0)) => Some(5),
        (Some(-15), Some(0)) => Some(6),
        (Some(15), Some(0)) => Some(7),
        (Some(180 | -180), Some(0)) => Some(8),
        (Some(-90), Some(0)) => Some(9),
        (Some(90), Some(0)) => Some(10),
        (Some(0), Some(90)) => Some(11),
        (Some(-30), Some(45)) => Some(12),
        (Some(0), Some(45)) => Some(13),
        (Some(30), Some(45)) => Some(14),
        (Some(-135 | -150), Some(45)) => Some(15),
        (Some(180 | -180), Some(45)) => Some(16),
        (Some(135 | 150), Some(45)) => Some(17),
        _ => None,
    }
}

const fn cicp_to_wave_bit(position: u8) -> Option<u8> {
    match position {
        0 => Some(0),
        1 => Some(1),
        2 => Some(2),
        3 => Some(3),
        4 | 8 => Some(4),
        5 | 9 => Some(5),
        6 => Some(6),
        7 => Some(7),
        10 => Some(8),
        13 => Some(9),
        14 => Some(10),
        25 => Some(11),
        17 => Some(12),
        19 => Some(13),
        18 => Some(14),
        20 => Some(15),
        22 => Some(16),
        21 => Some(17),
        _ => None,
    }
}

fn channel_mask(roles: &[ChannelRole]) -> io::Result<u32> {
    for (name, mask) in [
        ("5.1", 0x0000_003f),
        ("6.1", 0x0000_070f),
        ("7.1", 0x0000_063f),
        ("5.1.4", 0x0002_d03f),
        ("7.1.4", 0x0002_d63f),
    ] {
        let exact = crate::wav::reader::roles_from_wave_mask(mask, mask.count_ones() as u16);
        if named_channel_layout(name).as_deref() == Some(roles) || exact == roles {
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

/// Channel roles a subsequent Forge WAVE decode will recover from the header
/// written for `roles`.
pub(crate) fn persisted_channel_roles(roles: &[ChannelRole]) -> io::Result<Vec<ChannelRole>> {
    let channels =
        u16::try_from(roles.len()).map_err(|_| io::Error::other("too many WAVE channels"))?;
    if channels <= 2 {
        return Ok(default_channel_roles(channels));
    }
    Ok(crate::wav::reader::roles_from_wave_mask(
        channel_mask(roles)?,
        channels,
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
    use crate::channel_layout::ChannelLayoutDescriptor;
    use crate::wav::WavReader;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    fn invalid_metadata_is_rejected_before_existing_destination_is_opened() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.wav");
        for id in [*b"fmt ", *b"data", *b"ds64"] {
            std::fs::write(&destination, b"existing destination").unwrap();
            let error = match WavStreamWriter::create_with_metadata(
                &destination,
                48_000,
                1,
                32,
                PcmKind::S16,
                false,
                WavContainer::Riff,
                &default_channel_roles(1),
                &[WaveChunk {
                    id,
                    body: vec![1, 2, 3],
                }],
            ) {
                Ok(_) => panic!("reserved metadata chunk unexpectedly accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("reserved WAVE chunk"));
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"existing destination"
            );
        }
        assert_eq!(
            validate_metadata_chunks(&[WaveChunk {
                id: *b"JUNK",
                body: vec![1],
            }])
            .unwrap(),
            10
        );
    }

    #[test]
    fn invalid_format_geometry_is_rejected_before_destination_is_opened() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.wav");
        for sample_rate in [
            0,
            MIN_DECODE_SAMPLE_RATE_HZ - 1,
            MAX_DECODE_SAMPLE_RATE_HZ + 1,
        ] {
            std::fs::write(&destination, b"existing destination").unwrap();
            assert!(
                WavStreamWriter::create(&destination, sample_rate, 1, 32, PcmKind::S16, false,)
                    .is_err()
            );
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                b"existing destination"
            );
        }

        std::fs::write(&destination, b"existing destination").unwrap();
        let channels = u16::MAX;
        let roles = vec![ChannelRole::Main; usize::from(channels)];
        assert!(WavStreamWriter::create_with_metadata(
            &destination,
            48_000,
            channels,
            1,
            PcmKind::F64,
            false,
            WavContainer::Rf64,
            &roles,
            &[],
        )
        .is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"existing destination"
        );
    }

    #[test]
    fn invalid_full_buffer_geometry_preserves_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.wav");
        std::fs::write(&destination, b"existing destination").unwrap();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 32,
            data: vec![vec![0.0; 32]],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        assert!(WavWriter::write(&destination, &buffer, PcmKind::F32, false).is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"existing destination"
        );
    }

    #[test]
    fn invalid_stream_chunk_shape_does_not_write_or_advance_state() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            WavStreamWriter::create(file.path(), 48_000, 2, 32, PcmKind::F32, false).unwrap();
        let header_bytes = std::fs::metadata(file.path()).unwrap().len();

        assert!(writer.write_chunk(&[vec![0.0; 32]]).is_err());
        assert!(writer.write_chunk(&[vec![0.0; 32], vec![0.0; 31]]).is_err());
        assert_eq!(writer.remaining_frames, 32);
        assert!(writer.last_encoded_chunk().is_empty());
        assert_eq!(std::fs::metadata(file.path()).unwrap().len(), header_bytes);

        writer.write_chunk(&[vec![0.0; 32], vec![0.0; 32]]).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            std::fs::metadata(file.path()).unwrap().len(),
            header_bytes + 32 * 2 * 4
        );
    }

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
        let persisted = persisted_channel_roles(&named_channel_layout("7.1").unwrap()).unwrap();
        assert_eq!(persisted[0], ChannelRole::positioned(-30, 0));
        assert_eq!(persisted[1], ChannelRole::positioned(30, 0));
        assert_eq!(persisted[3], ChannelRole::Lfe);
        assert_eq!(persisted[7], ChannelRole::positioned(90, 0));

        for (name, mask) in [
            ("5.1", 0x0000_003f),
            ("6.1", 0x0000_070f),
            ("7.1", 0x0000_063f),
            ("5.1.4", 0x0002_d03f),
            ("7.1.4", 0x0002_d63f),
        ] {
            let generic = named_channel_layout(name).unwrap();
            let exact = crate::wav::reader::roles_from_wave_mask(mask, generic.len() as u16);
            assert_eq!(channel_mask(&generic).unwrap(), mask, "generic {name}");
            assert_eq!(channel_mask(&exact).unwrap(), mask, "exact {name}");
            assert_eq!(persisted_channel_roles(&generic).unwrap(), exact, "{name}");
            assert_eq!(persisted_channel_roles(&exact).unwrap(), exact, "{name}");
        }
    }

    #[test]
    fn exact_wave_masks_round_trip_without_canonicalization() {
        let directory = tempfile::tempdir().unwrap();
        for mask in (0..18).map(|bit| 1_u32 << bit).chain([0, 0x0003, 0x5003]) {
            let channels = if mask == 0 {
                4
            } else if mask == 0x0003 {
                6
            } else {
                mask.count_ones() as u16
            };
            let layout = ChannelLayoutDescriptor::wave(channels, true, Some(mask));
            let buffer = AudioBuffer {
                sample_rate: 48_000,
                channels,
                frames: 16,
                data: vec![vec![0.0; 16]; usize::from(channels)],
                channel_roles: layout.channel_roles(),
                source_kind: PcmKind::S16,
            };
            let output = directory.path().join(format!("mask-{mask:08x}.wav"));
            WavWriter::write_with_channel_layout(
                &output,
                &buffer,
                PcmKind::S16,
                false,
                WavContainer::Riff,
                &layout,
            )
            .unwrap();

            let (decoded, actual) = WavReader::open_with_channel_layout(&output).unwrap();
            assert_eq!(decoded.channels, channels, "mask {mask:#010x}");
            assert_eq!(actual.wave_channel_mask(), Some(mask), "mask {mask:#010x}");
            assert_eq!(
                actual.assignments(),
                layout.assignments(),
                "mask {mask:#010x}"
            );
            assert_eq!(
                actual.provenance(),
                layout.provenance(),
                "mask {mask:#010x}"
            );
        }
    }

    #[test]
    fn implicit_flac_mono_and_stereo_keep_byte_identical_wave_headers() {
        let directory = tempfile::tempdir().unwrap();
        for channels in [1_u16, 2] {
            let buffer = AudioBuffer {
                sample_rate: 48_000,
                channels,
                frames: 16,
                data: vec![vec![0.0; 16]; usize::from(channels)],
                channel_roles: default_channel_roles(channels),
                source_kind: PcmKind::S16,
            };
            let legacy = directory.path().join(format!("legacy-{channels}.wav"));
            let exact = directory.path().join(format!("exact-{channels}.wav"));
            WavWriter::write(&legacy, &buffer, PcmKind::S16, false).unwrap();
            WavWriter::write_with_channel_layout(
                &exact,
                &buffer,
                PcmKind::S16,
                false,
                WavContainer::Riff,
                &ChannelLayoutDescriptor::flac(channels, None),
            )
            .unwrap();

            assert_eq!(
                std::fs::read(exact).unwrap(),
                std::fs::read(legacy).unwrap()
            );
        }
    }

    #[test]
    fn invalid_exact_mask_preserves_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.wav");
        std::fs::write(&destination, b"existing destination").unwrap();
        let layout = ChannelLayoutDescriptor::wave(2, true, Some(0x7));
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 16,
            data: vec![vec![0.0; 16]; 2],
            channel_roles: layout.channel_roles(),
            source_kind: PcmKind::S16,
        };
        assert!(WavWriter::write_with_channel_layout(
            &destination,
            &buffer,
            PcmKind::S16,
            false,
            WavContainer::Riff,
            &layout,
        )
        .is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"existing destination"
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
    fn odd_u8_data_is_padded_and_roundtrips_in_every_container() {
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 3,
            data: vec![vec![-1.0, 0.0, 1.0]],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::U8,
        };
        for container in [WavContainer::Riff, WavContainer::Rf64, WavContainer::Bw64] {
            let file = tempfile::NamedTempFile::new().unwrap();
            WavWriter::write_with_options(
                file.path(),
                &buffer,
                PcmKind::U8,
                false,
                container,
                None,
            )
            .unwrap();

            let bytes = std::fs::read(file.path()).unwrap();
            assert_eq!(bytes.len() & 1, 0, "{container:?}");
            assert_eq!(bytes.last(), Some(&0), "{container:?}");
            match container {
                WavContainer::Riff => assert_eq!(
                    u64::from(u32::from_le_bytes(bytes[4..8].try_into().unwrap())),
                    bytes.len() as u64 - 8
                ),
                WavContainer::Rf64 | WavContainer::Bw64 => {
                    assert_eq!(
                        u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
                        bytes.len() as u64 - 8
                    );
                    assert_eq!(u64::from_le_bytes(bytes[28..36].try_into().unwrap()), 3);
                }
                WavContainer::Auto => unreachable!(),
            }

            let decoded = WavReader::open(file.path()).unwrap();
            assert_eq!(decoded.source_kind, PcmKind::U8, "{container:?}");
            assert_eq!((decoded.channels, decoded.frames), (1, 3));
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

    #[test]
    fn borrowed_stream_chunk_matches_owned_wave_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let owned_output = directory.path().join("owned.wav");
        let borrowed_output = directory.path().join("borrowed.wav");
        let chunk = vec![
            vec![f32::NAN, -1.25, -0.0, 0.0, 0.25, 1.0, 1.25, 0.3, -0.7],
            vec![1.25, 1.0, 0.5, 0.0, -0.0, -0.5, -1.0, -1.25, 0.1],
        ];

        let gain = 0.75;
        let ceiling = 0.9;
        let mut normalized = chunk.clone();
        for channel in &mut normalized {
            crate::dsp::simd::apply_gain_and_hard_clip(channel, gain, ceiling);
        }
        let mut owned =
            WavStreamWriter::create(&owned_output, 48_000, 2, 9, PcmKind::S16, false).unwrap();
        owned.write_chunk(&normalized).unwrap();
        owned.finish().unwrap();

        let borrowed_storage = chunk;
        let borrowed = borrowed_storage
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        let mut borrowed_writer =
            WavStreamWriter::create(&borrowed_output, 48_000, 2, 9, PcmKind::S16, false).unwrap();
        assert!(borrowed_writer.supports_borrowed_planar());
        borrowed_writer
            .write_normalized_borrowed_chunk(&borrowed, gain, ceiling)
            .unwrap();
        borrowed_writer.finish().unwrap();

        assert_eq!(
            std::fs::read(borrowed_output).unwrap(),
            std::fs::read(owned_output).unwrap()
        );
    }
}
