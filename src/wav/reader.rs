//! Minimal but robust RIFF/WAVE demuxer.
//!
//! Supports PCM (8/16/24/32-bit) and IEEE-float (32/64-bit), including files
//! that use the `WAVE_FORMAT_EXTENSIBLE` tag. Unknown chunks are skipped. The
//! File-backed decoding scans and validates the chunk table first, then reads
//! the sole PCM payload once after resource limits have been enforced. Chunk
//! scanning stops at the RIFF size (or RF64/BW64 `ds64.riffSize`); bytes after
//! that declared form are deliberately ignored as out-of-container data.

use crate::channel_layout::ChannelLayoutDescriptor;
use crate::dsp::convert;
use crate::wav::{
    default_channel_roles, AudioBuffer, ChannelLayoutProvenance, ChannelRole, PcmKind, WaveFormat,
    MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ,
};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_WAVE_CHUNKS: usize = 100_000;

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
    sample_rate: u32,
    channels: u16,
    kind: PcmKind,
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

fn require_known_wave_layout<T>(
    value: T,
    provenance: ChannelLayoutProvenance,
) -> Result<T, WavReadError> {
    match provenance {
        ChannelLayoutProvenance::KnownSpeakers => Ok(value),
        ChannelLayoutProvenance::Unknown => Err(WavReadError::BadFormat(
            "ambiguous channel layout; use a with-layout API and supply explicit speaker roles",
        )),
        ChannelLayoutProvenance::SceneBased => Err(WavReadError::BadFormat(
            "scene-based channel layout cannot be represented as speaker roles; use a with-layout API",
        )),
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
    ///
    /// Multichannel input without a complete standard speaker mask is
    /// rejected. Use [`Self::open_with_layout`] when a caller can resolve an
    /// ambiguous layout explicitly.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<AudioBuffer, WavReadError> {
        let (buffer, provenance) = Self::open_with_layout(path)?;
        require_known_wave_layout(buffer, provenance)
    }

    /// Decode a WAV file while retaining whether its channel-to-speaker
    /// mapping is authoritative.
    pub fn open_with_layout<P: AsRef<Path>>(
        path: P,
    ) -> Result<(AudioBuffer, ChannelLayoutProvenance), WavReadError> {
        Self::open_with_layout_and_limits(path, u16::MAX, u64::MAX)
    }

    /// Decode a WAV file while retaining its exact container channel-layout
    /// declaration, including zero and partial extensible masks.
    pub fn open_with_channel_layout<P: AsRef<Path>>(
        path: P,
    ) -> Result<(AudioBuffer, ChannelLayoutDescriptor), WavReadError> {
        Self::open_with_channel_layout_and_limits(path, u16::MAX, u64::MAX)
    }

    /// Parse and decode one unambiguous data chunk while applying the sample
    /// bound before allocating its PCM payload.
    pub fn open_with_layout_and_limits<P: AsRef<Path>>(
        path: P,
        max_channels: u16,
        max_decoded_samples: u64,
    ) -> Result<(AudioBuffer, ChannelLayoutProvenance), WavReadError> {
        let (buffer, layout) =
            Self::open_with_channel_layout_and_limits(path, max_channels, max_decoded_samples)?;
        let provenance = layout.provenance();
        Ok((buffer, provenance))
    }

    /// Decode a WAV file with resource bounds and its exact channel-layout
    /// sidecar. This is the lossless counterpart to
    /// [`Self::open_with_layout_and_limits`].
    pub fn open_with_channel_layout_and_limits<P: AsRef<Path>>(
        path: P,
        max_channels: u16,
        max_decoded_samples: u64,
    ) -> Result<(AudioBuffer, ChannelLayoutDescriptor), WavReadError> {
        let mut file = File::open(path)?;
        let (info, channel_layout) = Self::probe_file_with_channel_layout(&mut file)?;
        if info.channels > max_channels {
            return Err(WavReadError::BadFormat("channel count exceeds limit"));
        }
        let frames = wave_frame_count(info.data_size, info.channels, info.kind)?;
        let decoded_samples = frames
            .checked_mul(u64::from(info.channels))
            .ok_or(WavReadError::BadFormat("decoded sample count overflow"))?;
        if decoded_samples > max_decoded_samples {
            return Err(WavReadError::BadFormat(
                "decoded sample count exceeds safety limit",
            ));
        }
        let data_size = usize::try_from(info.data_size)
            .map_err(|_| WavReadError::BadFormat("audio data is too large for memory"))?;
        let frames = usize::try_from(frames)
            .map_err(|_| WavReadError::BadFormat("audio data is too large for memory"))?;
        file.seek(SeekFrom::Start(info.data_offset))?;
        let mut bytes = vec![0_u8; data_size];
        file.read_exact(&mut bytes)?;
        let data = convert::decode_planar(&bytes, info.kind, usize::from(info.channels));
        Ok((
            AudioBuffer {
                sample_rate: info.sample_rate,
                channels: info.channels,
                frames,
                data,
                channel_roles: info.channel_roles,
                source_kind: info.kind,
            },
            channel_layout,
        ))
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
        let (buffer, provenance) =
            Self::read_bytes_with_layout_and_limits(bytes, max_channels, max_decoded_samples)?;
        require_known_wave_layout(buffer, provenance)
    }

    /// Decode an in-memory WAV while retaining whether its channel-to-speaker
    /// mapping is authoritative.
    pub fn read_bytes_with_layout(
        bytes: &[u8],
    ) -> Result<(AudioBuffer, ChannelLayoutProvenance), WavReadError> {
        Self::read_bytes_with_layout_and_limits(bytes, u16::MAX, usize::MAX)
    }

    /// Decode an in-memory WAVE and preserve its exact channel-layout
    /// declaration.
    pub fn read_bytes_with_channel_layout(
        bytes: &[u8],
    ) -> Result<(AudioBuffer, ChannelLayoutDescriptor), WavReadError> {
        Self::read_bytes_with_channel_layout_and_limits(bytes, u16::MAX, usize::MAX)
    }

    /// Decode an in-memory WAVE while retaining the speaker-layout confidence
    /// sidecar used by normalization entry points.
    pub fn read_bytes_with_layout_and_limits(
        bytes: &[u8],
        max_channels: u16,
        max_decoded_samples: usize,
    ) -> Result<(AudioBuffer, ChannelLayoutProvenance), WavReadError> {
        let (buffer, layout) = Self::read_bytes_with_channel_layout_and_limits(
            bytes,
            max_channels,
            max_decoded_samples,
        )?;
        let provenance = layout.provenance();
        Ok((buffer, provenance))
    }

    /// Decode an in-memory WAVE with allocation bounds and preserve its exact
    /// channel-layout declaration.
    pub fn read_bytes_with_channel_layout_and_limits(
        bytes: &[u8],
        max_channels: u16,
        max_decoded_samples: usize,
    ) -> Result<(AudioBuffer, ChannelLayoutDescriptor), WavReadError> {
        let mut cur = 0usize;
        let container = take(bytes, &mut cur, 4).ok_or(WavReadError::Truncated)?;
        if !matches!(container, b"RIFF" | b"RF64" | b"BW64") {
            return Err(WavReadError::NotWave);
        }
        let declared_riff_size = read_u32(bytes, &mut cur)?;
        if !take(bytes, &mut cur, 4)
            .ok_or(WavReadError::Truncated)?
            .eq(b"WAVE")
        {
            return Err(WavReadError::NotWave);
        }
        let large = matches!(container, b"RF64" | b"BW64");
        let (scan_end, initial_ds64_data_size) = if large {
            if declared_riff_size != u32::MAX {
                return Err(WavReadError::BadFormat(
                    "RF64/BW64 RIFF size must be 0xffffffff",
                ));
            }
            large_container_bounds_from_bytes(bytes)?
        } else {
            (
                usize::try_from(checked_container_end(
                    u64::from(declared_riff_size),
                    bytes.len() as u64,
                )?)
                .map_err(|_| WavReadError::Truncated)?,
                None,
            )
        };

        // Exactly one validated format and one data payload are authoritative.
        let mut fmt: Option<ParsedFormat> = None;
        let mut data: Option<&[u8]> = None;
        let mut ds64_data_size = initial_ds64_data_size;
        let mut seen_ds64 = false;
        let mut chunk_count = 0usize;

        while cur < scan_end {
            if chunk_count == MAX_WAVE_CHUNKS {
                return Err(WavReadError::BadFormat(
                    "WAVE chunk count exceeds safety limit",
                ));
            }
            if scan_end - cur < 8 {
                return Err(WavReadError::Truncated);
            }
            let id = take(bytes, &mut cur, 4).unwrap();
            let declared_size = read_u32(bytes, &mut cur)?;
            chunk_count += 1;
            let size = if large && id == b"data" && declared_size == u32::MAX {
                usize::try_from(ds64_data_size.ok_or(WavReadError::BadFormat(
                    "RF64/BW64 data chunk is missing ds64",
                ))?)
                .map_err(|_| WavReadError::BadFormat("audio data is too large for memory"))?
            } else {
                declared_size as usize
            };
            let end = cur.checked_add(size).ok_or(WavReadError::Truncated)?;
            if end > scan_end {
                return Err(WavReadError::Truncated);
            }
            let body = &bytes[cur..end];
            cur = end;
            if size & 1 != 0 {
                if cur == scan_end {
                    return Err(WavReadError::Truncated);
                }
                cur += 1; // chunks are word-aligned
            }
            match id {
                b"fmt " => {
                    if fmt.is_some() {
                        return Err(WavReadError::BadFormat("duplicate fmt chunk"));
                    }
                    fmt = Some(parse_fmt(body)?);
                }
                b"data" => {
                    if data.is_some() {
                        return Err(WavReadError::BadFormat("duplicate data chunk"));
                    }
                    if fmt.is_none() {
                        return Err(WavReadError::BadFormat("data precedes fmt chunk"));
                    }
                    data = Some(body);
                }
                b"ds64" => {
                    if seen_ds64 {
                        return Err(WavReadError::BadFormat("duplicate ds64 chunk"));
                    }
                    seen_ds64 = true;
                    let (riff_size, data_size) = parse_ds64_fixed_fields(body)?;
                    validate_ds64_table_size(body.len() as u64, body)?;
                    if large {
                        if chunk_count != 1 {
                            return Err(WavReadError::BadFormat(
                                "ds64 must be the first RF64/BW64 chunk",
                            ));
                        }
                        let declared_end = checked_container_end(riff_size, bytes.len() as u64)?;
                        if declared_end != scan_end as u64
                            || ds64_data_size.is_some_and(|size| size != data_size)
                        {
                            return Err(WavReadError::BadFormat(
                                "RF64/BW64 ds64 fields changed during parsing",
                            ));
                        }
                        ds64_data_size = Some(data_size);
                    }
                }
                _ => {} // skip fact, LIST, etc.
            }
        }

        let ParsedFormat {
            wave_format,
            sample_rate,
            channels,
            kind,
            channel_mask,
        } = fmt.ok_or(WavReadError::BadFormat("missing fmt chunk"))?;
        if channels > max_channels {
            return Err(WavReadError::BadFormat("channel count exceeds limit"));
        }
        let data = data.ok_or(WavReadError::NoDataChunk)?;
        let data_size = u64::try_from(data.len())
            .map_err(|_| WavReadError::BadFormat("audio data is too large for memory"))?;
        let frames = usize::try_from(wave_frame_count(data_size, channels, kind)?)
            .map_err(|_| WavReadError::BadFormat("audio data is too large for memory"))?;
        let decoded_samples = frames
            .checked_mul(channels as usize)
            .ok_or(WavReadError::BadFormat("decoded sample count overflow"))?;
        if decoded_samples > max_decoded_samples {
            return Err(WavReadError::BadFormat(
                "decoded sample count exceeds safety limit",
            ));
        }

        let planar = convert::decode_planar(data, kind, channels as usize);
        let (channel_roles, _) = resolve_wave_layout(wave_format, channel_mask, channels);
        let channel_layout = ChannelLayoutDescriptor::wave(
            channels,
            wave_format == WaveFormat::Extensible,
            channel_mask,
        );
        let buf = AudioBuffer {
            sample_rate,
            channels,
            frames,
            data: planar,
            channel_roles,
            source_kind: kind,
        };
        Ok((buf, channel_layout))
    }

    /// Read only the RIFF headers required for streaming decode.
    pub fn probe<P: AsRef<Path>>(path: P) -> Result<WavStreamInfo, WavReadError> {
        let (info, provenance) = Self::probe_with_layout(path)?;
        require_known_wave_layout(info, provenance)
    }

    /// Probe a WAVE stream while retaining whether its speaker assignment was
    /// explicit. The sidecar keeps the stable [`WavStreamInfo`] shape while
    /// allowing callers to resolve an ambiguous layout explicitly.
    pub fn probe_with_layout<P: AsRef<Path>>(
        path: P,
    ) -> Result<(WavStreamInfo, ChannelLayoutProvenance), WavReadError> {
        let (info, layout) = Self::probe_with_channel_layout(path)?;
        let provenance = layout.provenance();
        Ok((info, provenance))
    }

    /// Probe a WAVE stream while retaining its exact channel-layout
    /// declaration, including the raw `dwChannelMask` value.
    pub fn probe_with_channel_layout<P: AsRef<Path>>(
        path: P,
    ) -> Result<(WavStreamInfo, ChannelLayoutDescriptor), WavReadError> {
        let mut file = File::open(path)?;
        Self::probe_file_with_channel_layout(&mut file)
    }

    fn probe_file_with_channel_layout(
        file: &mut File,
    ) -> Result<(WavStreamInfo, ChannelLayoutDescriptor), WavReadError> {
        file.seek(SeekFrom::Start(0))?;
        let file_len = file.metadata()?.len();
        let mut riff = [0u8; 12];
        file.read_exact(&mut riff)?;
        if !matches!(&riff[..4], b"RIFF" | b"RF64" | b"BW64") || &riff[8..] != b"WAVE" {
            return Err(WavReadError::NotWave);
        }
        let large = matches!(&riff[..4], b"RF64" | b"BW64");
        let declared_riff_size = u32::from_le_bytes(riff[4..8].try_into().unwrap());
        let (scan_end, initial_ds64_data_size) = if large {
            if declared_riff_size != u32::MAX {
                return Err(WavReadError::BadFormat(
                    "RF64/BW64 RIFF size must be 0xffffffff",
                ));
            }
            large_container_bounds_from_file(file, file_len)?
        } else {
            (
                checked_container_end(u64::from(declared_riff_size), file_len)?,
                None,
            )
        };
        file.seek(SeekFrom::Start(12))?;
        let mut parsed_format: Option<ParsedFormat> = None;
        let mut ds64_data_size = initial_ds64_data_size;
        let mut seen_ds64 = false;
        let mut data_info = None;
        let mut chunk_count = 0usize;
        loop {
            let header_offset = file.stream_position()?;
            if header_offset == scan_end {
                break;
            }
            if header_offset > scan_end || scan_end - header_offset < 8 {
                return Err(WavReadError::Truncated);
            }
            if chunk_count == MAX_WAVE_CHUNKS {
                return Err(WavReadError::BadFormat(
                    "WAVE chunk count exceeds safety limit",
                ));
            }
            let mut header = [0u8; 8];
            file.read_exact(&mut header)?;
            chunk_count += 1;
            let declared_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let body_offset = file.stream_position()?;
            if &header[..4] == b"fmt " {
                if parsed_format.is_some() {
                    return Err(WavReadError::BadFormat("duplicate fmt chunk"));
                }
                if declared_size > 65_536 {
                    return Err(WavReadError::BadFormat(
                        "fmt chunk exceeds 64 KiB safety limit",
                    ));
                }
            }
            let chunk_size = if large && &header[..4] == b"data" && declared_size == u32::MAX {
                ds64_data_size.ok_or(WavReadError::BadFormat(
                    "RF64/BW64 data chunk is missing ds64",
                ))?
            } else {
                u64::from(declared_size)
            };
            let next = body_offset
                .checked_add(chunk_size)
                .and_then(|offset| offset.checked_add(chunk_size & 1))
                .ok_or(WavReadError::Truncated)?;
            if next > scan_end {
                return Err(WavReadError::Truncated);
            }

            if &header[..4] == b"fmt " {
                let mut body = vec![0; declared_size as usize];
                file.read_exact(&mut body)?;
                parsed_format = Some(parse_fmt(&body)?);
            } else if &header[..4] == b"ds64" {
                if seen_ds64 {
                    return Err(WavReadError::BadFormat("duplicate ds64 chunk"));
                }
                seen_ds64 = true;
                if declared_size < 28 {
                    return Err(WavReadError::BadFormat("ds64 chunk too short"));
                }
                let mut prefix = [0u8; 28];
                file.read_exact(&mut prefix)?;
                let (riff_size, data_size) = parse_ds64_fixed_fields(&prefix)?;
                validate_ds64_table_size(u64::from(declared_size), &prefix)?;
                if large {
                    if chunk_count != 1 {
                        return Err(WavReadError::BadFormat(
                            "ds64 must be the first RF64/BW64 chunk",
                        ));
                    }
                    let declared_end = checked_container_end(riff_size, file_len)?;
                    if declared_end != scan_end
                        || ds64_data_size.is_some_and(|size| size != data_size)
                    {
                        return Err(WavReadError::BadFormat(
                            "RF64/BW64 ds64 fields changed during parsing",
                        ));
                    }
                    ds64_data_size = Some(data_size);
                }
            } else if &header[..4] == b"data" {
                if data_info.is_some() {
                    return Err(WavReadError::BadFormat("duplicate data chunk"));
                }
                let parsed = parsed_format
                    .as_ref()
                    .ok_or(WavReadError::BadFormat("data precedes fmt chunk"))?;
                wave_frame_count(chunk_size, parsed.channels, parsed.kind)?;
                let (channel_roles, _) =
                    resolve_wave_layout(parsed.wave_format, parsed.channel_mask, parsed.channels);
                let channel_layout = ChannelLayoutDescriptor::wave(
                    parsed.channels,
                    parsed.wave_format == WaveFormat::Extensible,
                    parsed.channel_mask,
                );
                data_info = Some((
                    WavStreamInfo {
                        sample_rate: parsed.sample_rate,
                        channels: parsed.channels,
                        kind: parsed.kind,
                        channel_roles,
                        data_offset: body_offset,
                        data_size: chunk_size,
                    },
                    channel_layout,
                ));
            }
            file.seek(SeekFrom::Start(next))?;
        }
        if parsed_format.is_none() {
            return Err(WavReadError::BadFormat("missing fmt chunk"));
        }
        data_info.ok_or(WavReadError::NoDataChunk)
    }
}

fn checked_container_end(declared_size: u64, available_size: u64) -> Result<u64, WavReadError> {
    if declared_size < 4 {
        return Err(WavReadError::BadFormat(
            "RIFF size is smaller than the WAVE form type",
        ));
    }
    let end = 8_u64
        .checked_add(declared_size)
        .ok_or(WavReadError::BadFormat("RIFF size overflows file offset"))?;
    if end > available_size {
        return Err(WavReadError::Truncated);
    }
    Ok(end)
}

fn parse_ds64_fixed_fields(body: &[u8]) -> Result<(u64, u64), WavReadError> {
    if body.len() < 28 {
        return Err(WavReadError::BadFormat("ds64 chunk too short"));
    }
    Ok((
        u64::from_le_bytes(body[0..8].try_into().unwrap()),
        u64::from_le_bytes(body[8..16].try_into().unwrap()),
    ))
}

fn validate_ds64_table_size(declared_size: u64, prefix: &[u8]) -> Result<(), WavReadError> {
    if prefix.len() < 28 {
        return Err(WavReadError::BadFormat("ds64 chunk too short"));
    }
    let table_length = u32::from_le_bytes(prefix[24..28].try_into().unwrap());
    let required = 28_u64
        .checked_add(
            u64::from(table_length)
                .checked_mul(12)
                .ok_or(WavReadError::BadFormat("ds64 table size overflow"))?,
        )
        .ok_or(WavReadError::BadFormat("ds64 table size overflow"))?;
    if required > declared_size {
        return Err(WavReadError::BadFormat(
            "ds64 table length exceeds chunk size",
        ));
    }
    Ok(())
}

fn large_container_bounds_from_bytes(bytes: &[u8]) -> Result<(usize, Option<u64>), WavReadError> {
    let header = bytes.get(12..20).ok_or(WavReadError::Truncated)?;
    if &header[..4] != b"ds64" {
        return Err(WavReadError::BadFormat(
            "ds64 must be the first RF64/BW64 chunk",
        ));
    }
    let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let body_end = 20_usize.checked_add(size).ok_or(WavReadError::Truncated)?;
    let next = body_end
        .checked_add(size & 1)
        .ok_or(WavReadError::Truncated)?;
    if next > bytes.len() {
        return Err(WavReadError::Truncated);
    }
    let (riff_size, data_size) = parse_ds64_fixed_fields(&bytes[20..body_end])?;
    validate_ds64_table_size(size as u64, &bytes[20..body_end])?;
    let scan_end = usize::try_from(checked_container_end(riff_size, bytes.len() as u64)?)
        .map_err(|_| WavReadError::Truncated)?;
    if next > scan_end {
        return Err(WavReadError::Truncated);
    }
    Ok((scan_end, Some(data_size)))
}

fn large_container_bounds_from_file(
    file: &mut File,
    file_len: u64,
) -> Result<(u64, Option<u64>), WavReadError> {
    file.seek(SeekFrom::Start(12))?;
    let mut header = [0_u8; 8];
    file.read_exact(&mut header)?;
    if &header[..4] != b"ds64" {
        return Err(WavReadError::BadFormat(
            "ds64 must be the first RF64/BW64 chunk",
        ));
    }
    let size = u64::from(u32::from_le_bytes(header[4..8].try_into().unwrap()));
    if size < 28 {
        return Err(WavReadError::BadFormat("ds64 chunk too short"));
    }
    let body_end = 20_u64.checked_add(size).ok_or(WavReadError::Truncated)?;
    let next = body_end
        .checked_add(size & 1)
        .ok_or(WavReadError::Truncated)?;
    if next > file_len {
        return Err(WavReadError::Truncated);
    }
    let mut prefix = [0_u8; 28];
    file.read_exact(&mut prefix)?;
    validate_ds64_table_size(size, &prefix)?;
    let riff_size = u64::from_le_bytes(prefix[0..8].try_into().unwrap());
    let data_size = u64::from_le_bytes(prefix[8..16].try_into().unwrap());
    let scan_end = checked_container_end(riff_size, file_len)?;
    if next > scan_end {
        return Err(WavReadError::Truncated);
    }
    Ok((scan_end, Some(data_size)))
}

fn resolve_wave_layout(
    wave_format: WaveFormat,
    channel_mask: Option<u32>,
    channels: u16,
) -> (Vec<ChannelRole>, ChannelLayoutProvenance) {
    let provenance = wave_layout_provenance(wave_format, channel_mask, channels);
    let roles = channel_mask
        .map(|mask| roles_from_wave_mask(mask, channels))
        .unwrap_or_else(|| default_channel_roles(channels));
    (roles, provenance)
}

fn canonical_wave_layout_name(channels: u16, mask: u32) -> Option<&'static str> {
    Some(match (channels, mask) {
        (1, 0x0000_0004) => "mono",
        (2, 0x0000_0003) => "stereo",
        (6, 0x0000_003f) => "5.1",
        (7, 0x0000_070f) => "6.1",
        (8, 0x0000_063f) => "7.1",
        (10, 0x0002_d03f) => "5.1.4",
        (12, 0x0002_d63f) => "7.1.4",
        _ => return None,
    })
}

fn wave_frame_count(data_size: u64, channels: u16, kind: PcmKind) -> Result<u64, WavReadError> {
    if channels == 0 {
        return Err(WavReadError::ZeroChannels);
    }
    let frame_bytes = u64::from(channels)
        .checked_mul(kind.bytes_per_sample() as u64)
        .ok_or(WavReadError::BadFormat("WAVE frame size overflow"))?;
    if !data_size.is_multiple_of(frame_bytes) {
        return Err(WavReadError::BadFormat(
            "data chunk contains a partial PCM frame",
        ));
    }
    Ok(data_size / frame_bytes)
}

fn wave_layout_provenance(
    wave_format: WaveFormat,
    channel_mask: Option<u32>,
    channels: u16,
) -> ChannelLayoutProvenance {
    if wave_format != WaveFormat::Extensible {
        return if matches!(channels, 1 | 2) {
            ChannelLayoutProvenance::KnownSpeakers
        } else {
            ChannelLayoutProvenance::Unknown
        };
    }

    if channel_mask.is_some_and(|mask| wave_mask_is_complete_standard(mask, channels)) {
        ChannelLayoutProvenance::KnownSpeakers
    } else {
        // Zero/partial masks and reserved speaker bits do not bind every PCM
        // plane to a standardized physical speaker.
        ChannelLayoutProvenance::Unknown
    }
}

pub(crate) fn wave_mask_is_complete_standard(mask: u32, channels: u16) -> bool {
    const STANDARD_SPEAKER_BITS: u32 = (1 << 18) - 1;
    mask != 0 && mask & !STANDARD_SPEAKER_BITS == 0 && mask.count_ones() == u32::from(channels)
}

fn parse_fmt(body: &[u8]) -> Result<ParsedFormat, WavReadError> {
    const PCM_SUBFORMAT_GUID: &[u8; 16] = &[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ];
    const IEEE_FLOAT_SUBFORMAT_GUID: &[u8; 16] = &[
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ];

    if body.len() < 16 {
        return Err(WavReadError::BadFormat("fmt chunk too short"));
    }
    let mut c = 0usize;
    let tag = read_u16(body, &mut c)?;
    let channels = read_u16(body, &mut c)?;
    if channels == 0 {
        return Err(WavReadError::ZeroChannels);
    }
    let rate = read_u32(body, &mut c)?;
    if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&rate) {
        return Err(WavReadError::BadFormat(
            "sample rate is outside the supported 8000..=384000 Hz range",
        ));
    }
    let avg_bytes_per_second = read_u32(body, &mut c)?;
    let block_align = read_u16(body, &mut c)?;
    let bits = read_u16(body, &mut c)?;

    let wformat = WaveFormat::from_tag(tag).ok_or(WavReadError::UnsupportedFormatTag(tag))?;

    // Resolve the real format tag only after validating the complete
    // KSDATAFORMAT_SUBTYPE GUID. Looking at its first two bytes alone would
    // allow an unrelated or malformed subtype to masquerade as PCM.
    let (real_tag, channel_mask) = if let WaveFormat::Extensible = wformat {
        if body.len() < 18 {
            return Err(WavReadError::BadFormat("extensible fmt too short"));
        }
        let extension_size = usize::from(read_u16_at(body, 16)?);
        if extension_size < 22 {
            return Err(WavReadError::BadFormat(
                "WAVE_FORMAT_EXTENSIBLE cbSize must be at least 22",
            ));
        }
        let declared_end = 18_usize
            .checked_add(extension_size)
            .ok_or(WavReadError::BadFormat("extensible fmt size overflow"))?;
        if declared_end > body.len() {
            return Err(WavReadError::BadFormat(
                "WAVE_FORMAT_EXTENSIBLE cbSize exceeds fmt chunk",
            ));
        }
        let real_tag = if &body[24..40] == PCM_SUBFORMAT_GUID {
            0x0001
        } else if &body[24..40] == IEEE_FLOAT_SUBFORMAT_GUID {
            0x0003
        } else {
            return Err(WavReadError::BadFormat(
                "unsupported WAVE_FORMAT_EXTENSIBLE subformat GUID",
            ));
        };
        (real_tag, Some(read_u32_at(body, 20)?))
    } else {
        (tag, None)
    };
    let kind = pick_kind(wformat, real_tag, bits)?;
    let expected_block_align = usize::from(channels)
        .checked_mul(kind.bytes_per_sample())
        .and_then(|value| u16::try_from(value).ok())
        .ok_or(WavReadError::BadFormat("WAVE block align overflow"))?;
    if block_align != expected_block_align {
        return Err(WavReadError::BadFormat(
            "block align does not match channel count and sample size",
        ));
    }
    let expected_avg_bytes_per_second = rate
        .checked_mul(u32::from(block_align))
        .ok_or(WavReadError::BadFormat("WAVE byte rate overflow"))?;
    if avg_bytes_per_second != expected_avg_bytes_per_second {
        return Err(WavReadError::BadFormat(
            "average bytes per second does not match sample rate and block align",
        ));
    }

    Ok(ParsedFormat {
        wave_format: wformat,
        sample_rate: rate,
        channels,
        kind,
        channel_mask,
    })
}

pub(crate) fn roles_from_wave_mask(mask: u32, channels: u16) -> Vec<ChannelRole> {
    if let Some(name) = canonical_wave_layout_name(channels, mask) {
        // Mono and stereo have an unambiguous legacy representation. Keep it
        // for public API compatibility. Multichannel layouts retain physical
        // positions below so containers with different 5.1 beds cannot compare
        // equal merely because both used generic Surround roles.
        if matches!(name, "mono" | "stereo") {
            return crate::wav::named_channel_layout(name)
                .expect("canonical WAVE mask names a supported channel layout");
        }
    }

    use ChannelRole::{Lfe, Main};
    let p = ChannelRole::positioned;
    // WAVE's conventional rear-labelled 5.x bed is normally reproduced at
    // about +/-110 degrees. Preserve the established +1.5 dB surround weight
    // while keeping it distinguishable from an explicit +/-90 degree side bed.
    let conventional_five_x_bed =
        crate::channel_layout::wave_mask_uses_conventional_five_x_surround(mask);
    let mut roles = Vec::with_capacity(channels as usize);
    for bit in 0..32 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        roles.push(match bit {
            // A non-canonical complete mask must retain every declared WAVE
            // speaker position. Collapsing any of these to Main/Surround can
            // make distinct layouts compare equal at the output preflight.
            0 => p(-30, 0),
            1 => p(30, 0),
            2 => p(0, 0),
            3 => Lfe,
            4 if conventional_five_x_bed => p(-110, 0),
            5 if conventional_five_x_bed => p(110, 0),
            4 => p(-135, 0),
            5 => p(135, 0),
            6 => p(-15, 0),
            7 => p(15, 0),
            8 => p(180, 0),
            9 => p(-90, 0),
            10 => p(90, 0),
            11 => p(0, 90),
            12 => p(-30, 45),
            13 => p(0, 45),
            14 => p(30, 45),
            15 => p(-135, 45),
            16 => p(180, 45),
            17 => p(135, 45),
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
    fn noncanonical_wave_mask_retains_each_declared_position() {
        // FL, FR, LFE, SL, SR: LFE is deliberately not channel index 3.
        let mask = 0x0000_0001 | 0x0000_0002 | 0x0000_0008 | 0x0000_0200 | 0x0000_0400;
        assert_eq!(
            roles_from_wave_mask(mask, 5),
            vec![
                ChannelRole::positioned(-30, 0),
                ChannelRole::positioned(30, 0),
                ChannelRole::Lfe,
                ChannelRole::positioned(-90, 0),
                ChannelRole::positioned(90, 0),
            ]
        );
    }

    #[test]
    fn noncanonical_complete_masks_do_not_collide_with_canonical_roles_or_weights() {
        let front_left_and_center = roles_from_wave_mask((1 << 0) | (1 << 2), 2);
        assert_eq!(
            front_left_and_center,
            vec![
                ChannelRole::positioned(-30, 0),
                ChannelRole::positioned(0, 0),
            ]
        );
        assert_ne!(front_left_and_center, default_channel_roles(2));

        let back_center = roles_from_wave_mask(1 << 8, 1);
        assert_eq!(back_center, [ChannelRole::positioned(180, 0)]);
        assert_eq!(crate::dsp::lufs::channel_weight(back_center[0]), 1.0);

        let canonical_five_one = roles_from_wave_mask(0x0000_003f, 6);
        assert_eq!(
            canonical_five_one,
            vec![
                ChannelRole::positioned(-30, 0),
                ChannelRole::positioned(30, 0),
                ChannelRole::positioned(0, 0),
                ChannelRole::Lfe,
                ChannelRole::positioned(-110, 0),
                ChannelRole::positioned(110, 0),
            ]
        );
        assert_eq!(
            crate::dsp::lufs::channel_weight(canonical_five_one[4]),
            1.41
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
        assert_eq!(roles[0], ChannelRole::positioned(-30, 0));
        assert_eq!(roles[1], ChannelRole::positioned(30, 0));
        assert_eq!(roles[2], ChannelRole::positioned(0, 0));
        assert_eq!(roles[3], ChannelRole::Lfe);
        assert_eq!(roles[4], ChannelRole::positioned(-135, 0));
        assert_eq!(roles[5], ChannelRole::positioned(135, 0));
        assert_eq!(roles[6], ChannelRole::positioned(-90, 0));
        assert_eq!(roles[7], ChannelRole::positioned(90, 0));
    }

    #[test]
    fn sparse_height_mask_keeps_positions_regardless_of_channel_count() {
        let mask = (1 << 11) | (1 << 12) | (1 << 13) | (1 << 14);
        assert!(wave_mask_is_complete_standard(mask, 4));
        assert_eq!(
            roles_from_wave_mask(mask, 4),
            vec![
                ChannelRole::positioned(0, 90),
                ChannelRole::positioned(-30, 45),
                ChannelRole::positioned(0, 45),
                ChannelRole::positioned(30, 45),
            ]
        );
        assert_eq!(
            wave_layout_provenance(WaveFormat::Extensible, Some(mask), 4),
            ChannelLayoutProvenance::KnownSpeakers
        );
    }

    #[test]
    fn wave_layout_provenance_accepts_only_complete_standard_masks() {
        use ChannelLayoutProvenance::{KnownSpeakers, Unknown};

        let cases = [
            (WaveFormat::Pcm, None, 1, KnownSpeakers),
            (WaveFormat::IeeeFloat, None, 2, KnownSpeakers),
            (WaveFormat::Pcm, None, 3, Unknown),
            (WaveFormat::Pcm, None, 6, Unknown),
            (WaveFormat::Extensible, Some(0x0000_0004), 1, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_0003), 2, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_003f), 6, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_070f), 7, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_063f), 8, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0002_d03f), 10, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0002_d63f), 12, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_0008), 1, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_0030), 2, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_0007), 3, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_0033), 4, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_0037), 5, KnownSpeakers),
            (WaveFormat::Extensible, Some(0x0000_060f), 6, KnownSpeakers),
            (WaveFormat::Extensible, Some(0), 2, Unknown),
            (WaveFormat::Extensible, Some(0x0000_0003), 6, Unknown),
            (WaveFormat::Extensible, Some(1 << 18), 1, Unknown),
            (WaveFormat::Extensible, None, 2, Unknown),
        ];

        for (format, mask, channels, expected) in cases {
            assert_eq!(
                wave_layout_provenance(format, mask, channels),
                expected,
                "format={format:?}, mask={mask:?}, channels={channels}"
            );
        }
    }

    #[test]
    fn known_extensible_masks_preserve_roles_through_the_current_writer() {
        for (name, channels, mask) in [
            ("mono", 1, 0x0000_0004),
            ("stereo", 2, 0x0000_0003),
            ("5.1", 6, 0x0000_003f),
            ("6.1", 7, 0x0000_070f),
            ("7.1", 8, 0x0000_063f),
            ("5.1.4", 10, 0x0002_d03f),
            ("7.1.4", 12, 0x0002_d63f),
        ] {
            let encoded_roles = crate::wav::named_channel_layout(name).unwrap();
            let decoded_roles = roles_from_wave_mask(mask, channels);
            assert_eq!(
                crate::wav::writer::persisted_channel_roles(&encoded_roles).unwrap(),
                decoded_roles,
                "{name}"
            );
        }
    }

    #[test]
    fn probe_rejects_oversized_format_chunk_before_allocating_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized.wav");
        let mut bytes = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        bytes.extend_from_slice(&65_537_u32.to_le_bytes());
        bytes.resize(20 + 65_537 + 1, 0);
        let riff_size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
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
        let mut wave = b"RIFF\x2c\0\0\0WAVEfmt \x10\0\0\0".to_vec();
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
                "decoded sample count exceeds safety limit"
            ))
        ));
        assert_eq!(
            WavReader::read_bytes_with_limits(&wave, 1, 4)
                .unwrap()
                .frames,
            4
        );
    }

    fn pcm16_wave_with_layout(
        sample_rate: u32,
        channels: u16,
        channel_mask: Option<u32>,
    ) -> Vec<u8> {
        let fmt_size = if channel_mask.is_some() { 40_u32 } else { 16 };
        let format_tag = if channel_mask.is_some() {
            0xfffe_u16
        } else {
            1
        };
        let block_align = channels.checked_mul(2).unwrap();
        let byte_rate = sample_rate.checked_mul(u32::from(block_align)).unwrap();

        let mut wave = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wave.extend_from_slice(&fmt_size.to_le_bytes());
        wave.extend_from_slice(&format_tag.to_le_bytes());
        wave.extend_from_slice(&channels.to_le_bytes());
        wave.extend_from_slice(&sample_rate.to_le_bytes());
        wave.extend_from_slice(&byte_rate.to_le_bytes());
        wave.extend_from_slice(&block_align.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        if let Some(mask) = channel_mask {
            wave.extend_from_slice(&22_u16.to_le_bytes());
            wave.extend_from_slice(&16_u16.to_le_bytes());
            wave.extend_from_slice(&mask.to_le_bytes());
            // KSDATAFORMAT_SUBTYPE_PCM.
            wave.extend_from_slice(&[
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
                0x9b, 0x71,
            ]);
        }
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&u32::from(block_align).to_le_bytes());
        wave.resize(wave.len() + usize::from(block_align), 0);
        let riff_size = u32::try_from(wave.len() - 8).unwrap();
        wave[4..8].copy_from_slice(&riff_size.to_le_bytes());
        wave
    }

    fn pcm16_wave_bytes(sample_rate: u32) -> Vec<u8> {
        pcm16_wave_with_layout(sample_rate, 1, None)
    }

    fn large_pcm16_wave(container: [u8; 4]) -> Vec<u8> {
        let riff = pcm16_wave_with_layout(48_000, 2, None);
        let mut chunks = riff[12..].to_vec();
        let data = chunks
            .windows(4)
            .position(|window| window == b"data")
            .unwrap();
        let data_size = u32::from_le_bytes(chunks[data + 4..data + 8].try_into().unwrap());
        chunks[data + 4..data + 8].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut wave = container.to_vec();
        wave.extend_from_slice(&u32::MAX.to_le_bytes());
        wave.extend_from_slice(b"WAVEds64");
        wave.extend_from_slice(&28_u32.to_le_bytes());
        let riff_size = u64::try_from(4 + 36 + chunks.len()).unwrap();
        wave.extend_from_slice(&riff_size.to_le_bytes());
        wave.extend_from_slice(&u64::from(data_size).to_le_bytes());
        wave.extend_from_slice(&1_u64.to_le_bytes());
        wave.extend_from_slice(&0_u32.to_le_bytes());
        wave.extend_from_slice(&chunks);
        wave
    }

    #[test]
    fn extensible_cbsize_accepts_appended_data_and_checks_its_declared_boundary() {
        let wave = pcm16_wave_with_layout(48_000, 2, Some(0x3));
        let mut body = wave[20..60].to_vec();
        body[16..18].copy_from_slice(&24_u16.to_le_bytes());
        body.extend_from_slice(&[0xaa, 0xbb]);
        assert!(parse_fmt(&body).is_ok());

        body[16..18].copy_from_slice(&25_u16.to_le_bytes());
        assert!(matches!(
            parse_fmt(&body),
            Err(WavReadError::BadFormat(
                "WAVE_FORMAT_EXTENSIBLE cbSize exceeds fmt chunk"
            ))
        ));

        body[16..18].copy_from_slice(&21_u16.to_le_bytes());
        assert!(matches!(
            parse_fmt(&body),
            Err(WavReadError::BadFormat(
                "WAVE_FORMAT_EXTENSIBLE cbSize must be at least 22"
            ))
        ));
    }

    #[test]
    fn riff_declared_boundary_is_enforced_and_trailing_file_bytes_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boundary.wav");
        let valid = pcm16_wave_with_layout(48_000, 2, None);

        let mut trailing = valid.clone();
        trailing.extend_from_slice(b"data\x04\0\0\0\0\0\0\0");
        assert_eq!(WavReader::read_bytes(&trailing).unwrap().frames, 1);
        std::fs::write(&path, &trailing).unwrap();
        assert_eq!(WavReader::probe(&path).unwrap().data_size, 4);

        let mut outside = valid.clone();
        outside[4..8].copy_from_slice(&4_u32.to_le_bytes());
        assert!(matches!(
            WavReader::read_bytes(&outside),
            Err(WavReadError::BadFormat("missing fmt chunk"))
        ));
        std::fs::write(&path, &outside).unwrap();
        assert!(matches!(
            WavReader::probe(&path),
            Err(WavReadError::BadFormat("missing fmt chunk"))
        ));

        let mut crossing = valid.clone();
        let shortened = u32::try_from(crossing.len() - 8 - 2).unwrap();
        crossing[4..8].copy_from_slice(&shortened.to_le_bytes());
        assert!(matches!(
            WavReader::read_bytes(&crossing),
            Err(WavReadError::Truncated)
        ));
        std::fs::write(&path, &crossing).unwrap();
        assert!(matches!(
            WavReader::probe(&path),
            Err(WavReadError::Truncated)
        ));

        let mut truncated = valid;
        let oversized = u32::try_from(truncated.len() - 8 + 1).unwrap();
        truncated[4..8].copy_from_slice(&oversized.to_le_bytes());
        assert!(matches!(
            WavReader::read_bytes(&truncated),
            Err(WavReadError::Truncated)
        ));
        std::fs::write(&path, &truncated).unwrap();
        assert!(matches!(
            WavReader::probe(&path),
            Err(WavReadError::Truncated)
        ));
    }

    #[test]
    fn rf64_and_bw64_use_the_ds64_declared_boundary() {
        let directory = tempfile::tempdir().unwrap();
        for container in [*b"RF64", *b"BW64"] {
            let path = directory.path().join(format!(
                "{}.wav",
                String::from_utf8_lossy(&container).to_ascii_lowercase()
            ));
            let valid = large_pcm16_wave(container);
            assert_eq!(WavReader::read_bytes(&valid).unwrap().frames, 1);
            std::fs::write(&path, &valid).unwrap();
            assert_eq!(WavReader::probe(&path).unwrap().data_size, 4);

            let mut crossing = valid.clone();
            let riff_size = u64::from_le_bytes(crossing[20..28].try_into().unwrap()) - 2;
            crossing[20..28].copy_from_slice(&riff_size.to_le_bytes());
            assert!(matches!(
                WavReader::read_bytes(&crossing),
                Err(WavReadError::Truncated)
            ));
            std::fs::write(&path, &crossing).unwrap();
            assert!(matches!(
                WavReader::probe(&path),
                Err(WavReadError::Truncated)
            ));

            let mut wrong_sentinel = valid;
            wrong_sentinel[4..8].copy_from_slice(&0_u32.to_le_bytes());
            assert!(matches!(
                WavReader::read_bytes(&wrong_sentinel),
                Err(WavReadError::BadFormat(
                    "RF64/BW64 RIFF size must be 0xffffffff"
                ))
            ));
        }
    }

    #[test]
    fn in_memory_decode_exposes_the_shared_wave_layout_provenance() {
        use ChannelLayoutProvenance::{KnownSpeakers, Unknown};

        let cases = [
            ("classic mono", 1, None, KnownSpeakers),
            ("classic stereo", 2, None, KnownSpeakers),
            ("classic maskless multichannel", 6, None, Unknown),
            ("extensible zero mask", 2, Some(0), Unknown),
            ("extensible partial mask", 6, Some(0x0003), Unknown),
            ("extensible complete mask", 6, Some(0x003f), KnownSpeakers),
        ];

        for (name, channels, mask, expected) in cases {
            let bytes = pcm16_wave_with_layout(48_000, channels, mask);
            let (buffer, provenance) =
                WavReader::read_bytes_with_layout_and_limits(&bytes, u16::MAX, usize::MAX).unwrap();
            assert_eq!(buffer.channels, channels, "{name}");
            assert_eq!(provenance, expected, "{name}");

            match expected {
                KnownSpeakers => {
                    let public = WavReader::read_bytes(&bytes).unwrap();
                    assert_eq!(public.channel_roles, buffer.channel_roles, "{name}");
                    assert_eq!(public.data, buffer.data, "{name}");
                }
                Unknown => {
                    let error = WavReader::read_bytes(&bytes).unwrap_err().to_string();
                    assert!(
                        error.contains("ambiguous channel layout"),
                        "{name}: {error}"
                    );
                }
                ChannelLayoutProvenance::SceneBased => unreachable!(),
            }
        }
    }

    #[test]
    fn wav_sample_rate_bounds_are_checked_while_parsing_fmt() {
        for sample_rate in [
            0,
            MIN_DECODE_SAMPLE_RATE_HZ - 1,
            MAX_DECODE_SAMPLE_RATE_HZ + 1,
        ] {
            assert!(matches!(
                WavReader::read_bytes(&pcm16_wave_bytes(sample_rate)),
                Err(WavReadError::BadFormat(
                    "sample rate is outside the supported 8000..=384000 Hz range"
                ))
            ));
        }

        for sample_rate in [MIN_DECODE_SAMPLE_RATE_HZ, MAX_DECODE_SAMPLE_RATE_HZ] {
            assert_eq!(
                WavReader::read_bytes(&pcm16_wave_bytes(sample_rate))
                    .unwrap()
                    .sample_rate,
                sample_rate
            );
        }
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
