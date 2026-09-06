//! Universal audio decoder.
//!
//! WAV files use Forge's own hand-written, parallelized demuxer/decoder (the
//! fast path). Every other container/codec Forge supports — MP3, FLAC, AAC/ALAC
//! in MP4/M4A, Vorbis in OGG — is decoded by `symphonia`, a pure-Rust audio
//! decoding framework. This keeps the binary dependency-free at the system level
//! (no libsndfile, no ffmpeg) while still reading the formats users actually
//! have. All paths produce the same planar-f32 [`AudioBuffer`] the DSP engine
//! consumes.

use crate::channel_layout::{
    default_flac_channel_mask, ChannelAssignment, ChannelLayoutDescriptor, ChannelLayoutOrigin,
};
use crate::stable_input::{StableInput, StableInputOptions};
pub use crate::wav::ChannelLayoutProvenance;
use crate::wav::{default_channel_roles, AudioBuffer, ChannelRole, PcmKind, WavReader};
pub(crate) use crate::wav::{MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ};
use std::fs::File;
use std::io::{self, BufReader, IoSliceMut, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

const MONO_WAV_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MULTICHANNEL_WAV_STREAM_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_PARALLEL_FLAC_DECODERS: usize = 8;
const MIN_PARALLEL_FLAC_DECODERS: usize = 4;
const FLAC_PACKETS_PER_DECODER: usize = 32;
const FLAC_SAMPLE_VALUES_PER_DECODER: u64 = 192_000;
const FLAC_FILE_BYTES_PER_DECODER: u64 = 192 * 1024;
const MAX_PARALLEL_FLAC_PACKET_BYTES: usize = 32 * 1024 * 1024;
const MAX_PARALLEL_FLAC_PCM_BYTES: usize = 32 * 1024 * 1024;
// The service admits the immutable encoded input separately, but a demuxer
// may still materialize one packet or metadata item. Preflight every
// self-delimiting container before handing it to a third-party parser and
// keep each such allocation within the decoder's fixed 64 MiB allowance.
const SERVICE_MAX_ENCODED_PACKET_BYTES: u64 = 16 * 1024 * 1024;
const SERVICE_MAX_METADATA_ITEM_BYTES: u64 = 1024 * 1024;
const SERVICE_MAX_METADATA_TOTAL_BYTES: u64 = SERVICE_MAX_ENCODED_PACKET_BYTES;
const SERVICE_MAX_CONTAINER_ITEMS: usize = 100_000;
const SERVICE_CONTAINER_CHECKPOINT_ITEMS: usize = 64;
const SERVICE_CONTROLLED_READ_BYTES: usize = 32 * 1024;
const SERVICE_SYMPHONIA_PROBE_BYTES: u64 = 1024 * 1024;
const SERVICE_MAX_ISOBMFF_SAMPLE_ENTRIES: u32 = (SERVICE_MAX_ENCODED_PACKET_BYTES / 4) as u32;
// MP3, AAC, and Vorbis normally decode much smaller packets. The normalization
// render pass groups whole packets to amortize callbacks and writer work while
// staying below the analyzer's 16,384-frame True Peak task threshold.
const TARGET_SYMPHONIA_STREAM_CHUNK_FRAMES: usize = 4_096;

/// Shared allocation-before-read contract for metadata reached through a
/// service decode. An entry count and aggregate encoded-byte budget complement
/// the per-value limit; callers decide which physical leaf bytes are counted so
/// nested container wrappers are not charged twice.
#[derive(Default)]
struct ServiceMetadataBudget {
    entries: usize,
    encoded_bytes: u64,
}

/// File-wide metadata accounting used when more than one supplemental tag
/// family can be visible to Symphonia (for example native FLAC plus APEv2).
/// Exact APE byte ranges are remembered so the header and footer probe anchors
/// cannot charge one physical tag twice.
#[derive(Default)]
struct ServiceMetadataContext {
    budget: ServiceMetadataBudget,
    ape_ranges: Vec<(u64, u64)>,
}

impl ServiceMetadataContext {
    fn record_ape(&mut self, start: u64, end: u64, entries: usize) -> Result<bool, String> {
        if self.ape_ranges.contains(&(start, end)) {
            return Ok(false);
        }
        if self
            .ape_ranges
            .iter()
            .any(|&(seen_start, seen_end)| start < seen_end && seen_start < end)
        {
            return Err("overlapping APE metadata ranges".into());
        }
        let bytes = end
            .checked_sub(start)
            .ok_or_else(|| "APE metadata range underflow".to_string())?;
        self.budget.add_entries(entries)?;
        self.budget.add_encoded_bytes(bytes)?;
        self.ape_ranges.push((start, end));
        Ok(true)
    }
}

impl ServiceMetadataBudget {
    fn add_entries(&mut self, entries: usize) -> Result<(), String> {
        self.entries = self
            .entries
            .checked_add(entries)
            .filter(|&count| count <= SERVICE_MAX_CONTAINER_ITEMS)
            .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
        Ok(())
    }

    fn add_encoded_bytes(&mut self, bytes: u64) -> Result<(), String> {
        self.encoded_bytes = self
            .encoded_bytes
            .checked_add(bytes)
            .filter(|&total| total <= SERVICE_MAX_METADATA_TOTAL_BYTES)
            .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
        Ok(())
    }

    fn validate_item(bytes: u64) -> Result<(), String> {
        if bytes > SERVICE_MAX_METADATA_ITEM_BYTES {
            Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into())
        } else {
            Ok(())
        }
    }
}

/// A Symphonia media source that observes the service's absolute request
/// control on every underlying read and seek. Forge bounds each actual read to
/// 32 KiB even when a caller supplies a larger destination and exposes only
/// the preflighted half-open physical byte range. Container preflight below
/// separately prevents a parser from allocating an attacker-declared packet
/// before the first read.
struct CheckpointMediaSource<'a, C> {
    file: File,
    base_offset: u64,
    byte_len: u64,
    checkpoint: &'a Mutex<C>,
}

impl<'a, C> CheckpointMediaSource<'a, C>
where
    C: FnMut() -> Result<(), String> + Send,
{
    fn new_range(
        mut file: File,
        checkpoint: &'a Mutex<C>,
        base_offset: u64,
        end_offset: u64,
    ) -> Result<Self, String> {
        let physical_len = file
            .metadata()
            .map_err(|error| format!("inspect controlled media source: {error}"))?
            .len();
        if end_offset > physical_len {
            return Err("controlled media source end exceeds input length".into());
        }
        let byte_len = end_offset
            .checked_sub(base_offset)
            .ok_or_else(|| "controlled media source range is reversed".to_string())?;
        file.seek(SeekFrom::Start(base_offset))
            .map_err(|error| format!("seek controlled media source: {error}"))?;
        Ok(Self {
            file,
            base_offset,
            byte_len,
            checkpoint,
        })
    }

    fn check(&self) -> io::Result<()> {
        let mut checkpoint = self
            .checkpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        checkpoint().map_err(io::Error::other)
    }
}

impl<C> Read for CheckpointMediaSource<'_, C>
where
    C: FnMut() -> Result<(), String> + Send,
{
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let physical_end = self
            .base_offset
            .checked_add(self.byte_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "input length overflow"))?;
        let position = self.file.stream_position()?;
        let remaining = physical_end.checked_sub(position).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "controlled media source position exceeds its admitted range",
            )
        })?;
        let length = output
            .len()
            .min(SERVICE_CONTROLLED_READ_BYTES)
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        self.file.read(&mut output[..length])
    }

    fn read_vectored(&mut self, outputs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let Some(output) = outputs.iter_mut().find(|output| !output.is_empty()) else {
            return Ok(0);
        };
        let length = output.len().min(SERVICE_CONTROLLED_READ_BYTES);
        self.read(&mut output[..length])
    }
}

impl<C> Seek for CheckpointMediaSource<'_, C>
where
    C: FnMut() -> Result<(), String> + Send,
{
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let physical_len = self
            .base_offset
            .checked_add(self.byte_len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "input length overflow"))?;
        let absolute = match position {
            SeekFrom::Start(offset) => self.base_offset.checked_add(offset),
            SeekFrom::Current(offset) => self.file.stream_position()?.checked_add_signed(offset),
            SeekFrom::End(offset) => physical_len.checked_add_signed(offset),
        }
        .filter(|&offset| offset >= self.base_offset && offset <= physical_len)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside controlled media source range",
            )
        })?;
        self.file.seek(SeekFrom::Start(absolute))?;
        Ok(absolute - self.base_offset)
    }
}

impl<C> symphonia::core::io::MediaSource for CheckpointMediaSource<'_, C>
where
    C: FnMut() -> Result<(), String> + Send,
{
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.byte_len)
    }
}

fn run_locked_checkpoint<C>(checkpoint: &Mutex<C>) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut checkpoint = checkpoint
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    checkpoint()
}

#[inline(always)]
fn run_decoder_checkpoint<const CONTROLLED: bool, C>(
    controlled: &Option<Mutex<C>>,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    if CONTROLLED {
        run_locked_checkpoint(
            controlled
                .as_ref()
                .expect("a controlled decoder owns its checkpoint"),
        )
    } else {
        Ok(())
    }
}

fn service_metadata_options() -> symphonia::core::meta::MetadataOptions {
    use symphonia::core::common::Limit;

    symphonia::core::meta::MetadataOptions::default()
        .limit_tag_bytes(Limit::Maximum(SERVICE_MAX_METADATA_ITEM_BYTES as usize))
        .limit_visual_bytes(Limit::Maximum(SERVICE_MAX_METADATA_ITEM_BYTES as usize))
}

fn is_wave_extension(extension: &str) -> bool {
    matches!(extension, "wav" | "wave" | "bwf" | "bw64" | "rf64")
}

fn has_wave_signature(path: &Path) -> bool {
    // Compatibility sniffing adds a separate open/read and therefore cannot
    // close the TOCTOU window before WavReader reopens the file. A future
    // InputDescriptor should probe and decode from one handle. Treat probe I/O
    // failures as "not identified" so the established decoder path retains
    // its missing, short-file, and read-error diagnostics.
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut signature = [0_u8; 12];
    if file.read_exact(&mut signature).is_err() {
        return false;
    }
    matches!(&signature[..4], b"RIFF" | b"RF64" | b"BW64") && &signature[8..12] == b"WAVE"
}

/// Convert every packet decoder failure, including a recoverable Symphonia
/// `DecodeError`, into a failed normalization input. Silently dropping a packet
/// changes programme duration and can materially change loudness measurements.
fn require_decoded_packet<T>(decoded: symphonia::core::errors::Result<T>) -> Result<T, String> {
    decoded.map_err(|error| error.to_string())
}

/// Speaker semantics carried by the two-bit MPEG audio channel-mode field.
///
/// MPEG "dual channel" contains two independent mono programmes. It must not
/// inherit Symphonia's count-derived front-left/front-right layout. Regular
/// stereo and joint stereo both decode to one conventional stereo programme,
/// so switching between those two coding modes is harmless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MpegProgrammeMode {
    Mono,
    StereoLike,
    DualChannel,
}

impl MpegProgrammeMode {
    fn name(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::StereoLike => "stereo",
            Self::DualChannel => "dual-channel",
        }
    }

    fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::StereoLike | Self::DualChannel => 2,
        }
    }
}

/// Track MPEG channel semantics across successfully decoded packets.
///
/// The provenance sidecar is fixed before the first PCM callback. Reject a
/// later semantic mode change rather than silently downgrading provenance
/// after speaker-dependent processing has already begun.
#[derive(Debug, Default)]
struct MpegChannelModeTracker {
    observed: Option<MpegProgrammeMode>,
}

impl MpegChannelModeTracker {
    fn observe_decoded_packet(
        &mut self,
        path: &Path,
        codec: symphonia::core::codecs::audio::AudioCodecId,
        packet: &[u8],
        decoded_channels: usize,
    ) -> Result<(), String> {
        let Some(mode) = mpeg_programme_mode_from_decoded_packet(codec, packet)
            .map_err(|error| format!("{}: {error}", path.display()))?
        else {
            return Ok(());
        };

        if decoded_channels != mode.channels() {
            return Err(format!(
                "{}: decoded MPEG audio channel count {decoded_channels} does not match {} mode",
                path.display(),
                mode.name()
            ));
        }
        if let Some(previous) = self.observed {
            if previous != mode {
                return Err(format!(
                    "{}: MPEG audio channel mode changed from {} to {}",
                    path.display(),
                    previous.name(),
                    mode.name()
                ));
            }
        } else {
            self.observed = Some(mode);
        }
        Ok(())
    }

    fn constrain_provenance(&self, provenance: ChannelLayoutProvenance) -> ChannelLayoutProvenance {
        match self.observed {
            Some(MpegProgrammeMode::DualChannel) => ChannelLayoutProvenance::Unknown,
            _ => provenance,
        }
    }
}

/// Read the channel mode from a packet that Symphonia has already decoded.
///
/// Symphonia's MPEG decoder searches for sync inside a packet, so decode
/// success alone does not prove that byte zero is the frame header. Standard
/// raw, ISO-BMFF, and Matroska MPEG packets do start at the header. Require that
/// invariant and enough of the structural header fields to identify the exact
/// header accepted by the decoder; never scan payload bytes for a replacement.
fn mpeg_programme_mode_from_decoded_packet(
    codec: symphonia::core::codecs::audio::AudioCodecId,
    packet: &[u8],
) -> Result<Option<MpegProgrammeMode>, &'static str> {
    use symphonia::core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};

    let expected_layer = match codec {
        CODEC_ID_MP1 => 0b11,
        CODEC_ID_MP2 => 0b10,
        CODEC_ID_MP3 => 0b01,
        _ => return Ok(None),
    };
    let header = packet
        .get(..4)
        .ok_or("decoded MPEG audio packet is shorter than its frame header")?;
    let header = u32::from_be_bytes(header.try_into().expect("four-byte MPEG header"));
    let version = (header >> 19) & 0b11;
    let layer = (header >> 17) & 0b11;
    let bitrate_index = (header >> 12) & 0b1111;
    let sample_rate_index = (header >> 10) & 0b11;
    if header >> 21 != 0x7ff
        || version == 0b01
        || layer != expected_layer
        || !(1..=14).contains(&bitrate_index)
        || sample_rate_index == 0b11
    {
        return Err("decoded MPEG audio packet does not begin with a validated frame header");
    }

    Ok(Some(match (header >> 6) & 0b11 {
        0b00 | 0b01 => MpegProgrammeMode::StereoLike,
        0b10 => MpegProgrammeMode::DualChannel,
        0b11 => MpegProgrammeMode::Mono,
        _ => unreachable!(),
    }))
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRole>,
    pub source_kind: PcmKind,
}

/// Version of the content-, track-, range-, and layout-bound input contract.
pub const INPUT_DESCRIPTOR_VERSION: u32 = 2;

/// Container identified from the retained bytes, never just a file suffix.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioContainer {
    Wave,
    Flac,
    Ogg,
    IsoBmff,
    Matroska,
    MpegAudio,
    Adts,
    Dsf,
    Dsdiff,
}

impl AudioContainer {
    /// Stable lower-case identity used by cache and catalogue bindings.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Wave => "wave",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::IsoBmff => "isobmff",
            Self::Matroska => "matroska",
            Self::MpegAudio => "mpeg-audio",
            Self::Adts => "adts",
            Self::Dsf => "dsf",
            Self::Dsdiff => "dsdiff",
        }
    }
}

/// Codec selected from the actual container track.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcm(PcmKind),
    Dsd,
    Flac,
    Mp1,
    Mp2,
    Mp3,
    Aac,
    Alac,
    Vorbis,
    Opus,
}

impl AudioCodec {
    /// Stable lower-case identity used by cache and catalogue bindings.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Pcm(PcmKind::U8) => "pcm-u8",
            Self::Pcm(PcmKind::S16) => "pcm-s16le",
            Self::Pcm(PcmKind::S24) => "pcm-s24le",
            Self::Pcm(PcmKind::S32) => "pcm-s32le",
            Self::Pcm(PcmKind::F32) => "pcm-f32le",
            Self::Pcm(PcmKind::F64) => "pcm-f64le",
            Self::Dsd => "dsd",
            Self::Flac => "flac",
            Self::Mp1 => "mp1",
            Self::Mp2 => "mp2",
            Self::Mp3 => "mp3",
            Self::Aac => "aac",
            Self::Alac => "alac",
            Self::Vorbis => "vorbis",
            Self::Opus => "opus",
        }
    }

    /// Whether the codec carries lossless or uncompressed source essence.
    pub const fn is_lossless(self) -> bool {
        matches!(self, Self::Pcm(_) | Self::Dsd | Self::Flac | Self::Alac)
    }
}

/// Deterministic audio-track selection within a probed container.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioTrackSelection {
    /// Select the container's declared default audio track.
    #[default]
    Default,
    /// Select the zero-based index among audio tracks only.
    Index(u32),
    /// Select the container's exact track identifier.
    Id(u32),
}

/// Lightweight content-probed identity used for safe output defaults before
/// an immutable [`InputDescriptor`] is captured for processing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioProgramIdentity {
    pub container: AudioContainer,
    pub codec: AudioCodec,
    pub track_index: u32,
    pub track_id: u32,
}

/// Identify a selected audio programme from container bytes, treating the file
/// name only as a non-binding probe hint.
pub fn probe_audio_program(
    path: &Path,
    selection: AudioTrackSelection,
) -> Result<AudioProgramIdentity, String> {
    let route = sniff_decoder_route(path)?;
    let identity = registry_identity_at(
        path,
        Some(path),
        &path.display().to_string(),
        route,
        selection,
    )?;
    Ok(AudioProgramIdentity {
        container: identity.container,
        codec: identity.codec,
        track_index: identity.track_index,
        track_id: identity.track_id,
    })
}

/// Requested decoded-frame interval selected for analysis and QC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFrameRange {
    start: u64,
    frames: Option<u64>,
}

impl SourceFrameRange {
    pub const fn start(self) -> u64 {
        self.start
    }

    pub const fn frames(self) -> Option<u64> {
        self.frames
    }

    pub const fn is_complete(self) -> bool {
        self.start == 0 && self.frames.is_none()
    }
}

/// Options whose complete effective value becomes part of an [`InputDescriptor`].
#[derive(Debug, Clone)]
pub struct InputDescriptorOptions {
    track: AudioTrackSelection,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    channel_roles: Option<Vec<ChannelRole>>,
    channel_layout: Option<ChannelLayoutDescriptor>,
}

impl Default for InputDescriptorOptions {
    fn default() -> Self {
        Self {
            track: AudioTrackSelection::Default,
            start_seconds: 0.0,
            duration_seconds: None,
            channel_roles: None,
            channel_layout: None,
        }
    }
}

impl InputDescriptorOptions {
    pub fn with_track(mut self, track: AudioTrackSelection) -> Self {
        self.track = track;
        self
    }

    pub fn with_time_range(mut self, start_seconds: f64, duration_seconds: Option<f64>) -> Self {
        self.start_seconds = start_seconds;
        self.duration_seconds = duration_seconds;
        self
    }

    pub fn with_channel_roles(mut self, channel_roles: Vec<ChannelRole>) -> Self {
        self.channel_roles = Some(channel_roles);
        self.channel_layout = None;
        self
    }

    /// Override the decoded PCM-plane assignment with an exact, checked
    /// channel-layout descriptor.
    pub fn with_channel_layout(mut self, channel_layout: ChannelLayoutDescriptor) -> Self {
        self.channel_layout = Some(channel_layout);
        self.channel_roles = None;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderRoute {
    Wave,
    Dsf,
    Dsdiff,
    Opus,
    Symphonia,
}

/// Immutable input bytes plus their selected codec, track, range, and layout.
///
/// A descriptor is probed from a [`StableInput`], so every later decode pass
/// reopens only the private immutable snapshot. Its file-name suffix is a probe
/// hint and never part of the selected route or cache identity.
#[derive(Clone)]
pub struct InputDescriptor {
    input: StableInput,
    route: DecoderRoute,
    container: AudioContainer,
    codec: AudioCodec,
    track_selection: AudioTrackSelection,
    track_index: u32,
    track_id: u32,
    info: StreamInfo,
    decoder_channel_roles: Vec<ChannelRole>,
    declared_frames: Option<u64>,
    decoder_layout_provenance: ChannelLayoutProvenance,
    declared_layout_provenance: ChannelLayoutProvenance,
    declared_channel_layout: ChannelLayoutDescriptor,
    channel_layout: ChannelLayoutDescriptor,
    explicit_channel_roles: bool,
    explicit_channel_layout: bool,
    range: SourceFrameRange,
    // Service-only immutable container route and byte range. A controlled
    // decode must reproduce this exact preflight before reopening the
    // snapshot; ordinary public descriptors retain `None`.
    service_preflight: Option<ServiceContainerPreflight>,
}

impl std::fmt::Debug for InputDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputDescriptor")
            .field("binding", self.input.binding())
            .field("container", &self.container)
            .field("codec", &self.codec)
            .field("track_selection", &self.track_selection)
            .field("track_index", &self.track_index)
            .field("track_id", &self.track_id)
            .field("info", &self.info)
            .field("decoder_channel_roles", &self.decoder_channel_roles)
            .field("declared_frames", &self.declared_frames)
            .field("decoder_layout_provenance", &self.decoder_layout_provenance)
            .field(
                "declared_layout_provenance",
                &self.declared_layout_provenance,
            )
            .field("declared_channel_layout", &self.declared_channel_layout)
            .field("channel_layout", &self.channel_layout)
            .field("explicit_channel_roles", &self.explicit_channel_roles)
            .field("explicit_channel_layout", &self.explicit_channel_layout)
            .field("range", &self.range)
            .finish()
    }
}

impl InputDescriptor {
    /// Probe one immutable input and bind the exact selected programme.
    pub fn probe(input: StableInput, options: InputDescriptorOptions) -> Result<Self, String> {
        Self::probe_impl(input, options, None, || Ok(()))
    }

    /// Service-only probe with cooperative cancellation and a decoded-sample
    /// ceiling. Public callers retain the exact legacy probe behaviour.
    pub(crate) fn probe_with_control<C>(
        input: StableInput,
        options: InputDescriptorOptions,
        max_decoded_samples: u64,
        checkpoint: C,
    ) -> Result<Self, String>
    where
        C: FnMut() -> Result<(), String> + Send,
    {
        Self::probe_impl(input, options, Some(max_decoded_samples), checkpoint)
    }

    fn probe_impl<C>(
        input: StableInput,
        options: InputDescriptorOptions,
        max_decoded_samples: Option<u64>,
        mut checkpoint: C,
    ) -> Result<Self, String>
    where
        C: FnMut() -> Result<(), String> + Send,
    {
        checkpoint()?;
        validate_descriptor_options(&options)?;
        let probed = probe_registry_with_control(
            &input,
            options.track,
            max_decoded_samples,
            &mut checkpoint,
        )?;
        checkpoint()?;
        let explicit_layout = if let Some(layout) = options.channel_layout.as_ref() {
            layout.validate()?;
            Some(layout.clone())
        } else if let Some(roles) = options.channel_roles.as_ref() {
            Some(ChannelLayoutDescriptor::from_channel_roles(roles.clone())?)
        } else {
            None
        };
        if let Some(layout) = explicit_layout.as_ref() {
            layout.validate_override_for_channels(probed.info.channels)?;
        }
        let range = source_frame_range(
            probed.info.sample_rate,
            options.start_seconds,
            options.duration_seconds,
        )?;
        let explicit_channel_roles = options.channel_roles.is_some();
        let explicit_channel_layout = options.channel_layout.is_some();
        let mut info = probed.info;
        let decoder_channel_roles = info.channel_roles.clone();
        let declared_channel_layout = probed.channel_layout;
        let channel_layout = explicit_layout.unwrap_or_else(|| declared_channel_layout.clone());
        if channel_layout.channel_count() != usize::from(info.channels) {
            return Err(format!(
                "channel-layout descriptor has {} channels but selected track has {}",
                channel_layout.channel_count(),
                info.channels
            ));
        }
        info.channel_roles = channel_layout.channel_roles();
        Ok(Self {
            input,
            route: probed.route,
            container: probed.container,
            codec: probed.codec,
            track_selection: options.track,
            track_index: probed.track_index,
            track_id: probed.track_id,
            info,
            decoder_channel_roles,
            declared_frames: probed.declared_frames,
            decoder_layout_provenance: probed.decoder_layout_provenance,
            declared_layout_provenance: declared_channel_layout.provenance(),
            declared_channel_layout,
            channel_layout,
            explicit_channel_roles,
            explicit_channel_layout,
            range,
            service_preflight: probed.service_preflight,
        })
    }

    /// Capture and probe a path using one bounded private snapshot.
    pub fn from_path(
        path: &Path,
        stable_options: &StableInputOptions,
        descriptor_options: InputDescriptorOptions,
    ) -> Result<Self, String> {
        let input =
            StableInput::from_path(path, stable_options).map_err(|error| error.to_string())?;
        Self::probe(input, descriptor_options)
    }

    pub const fn version(&self) -> u32 {
        INPUT_DESCRIPTOR_VERSION
    }

    pub fn stable_input(&self) -> &StableInput {
        &self.input
    }

    pub const fn container(&self) -> AudioContainer {
        self.container
    }

    pub const fn codec(&self) -> AudioCodec {
        self.codec
    }

    pub const fn track_index(&self) -> u32 {
        self.track_index
    }

    pub const fn track_id(&self) -> u32 {
        self.track_id
    }

    pub fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    pub const fn declared_frames(&self) -> Option<u64> {
        self.declared_frames
    }

    pub const fn declared_layout_provenance(&self) -> ChannelLayoutProvenance {
        self.declared_layout_provenance
    }

    /// Exact layout declared by the selected encoded programme before any
    /// caller override is applied.
    pub fn declared_channel_layout(&self) -> &ChannelLayoutDescriptor {
        &self.declared_channel_layout
    }

    /// Effective exact layout used by measurement and rendering.
    pub fn channel_layout(&self) -> &ChannelLayoutDescriptor {
        &self.channel_layout
    }

    pub const fn uses_explicit_channel_roles(&self) -> bool {
        self.explicit_channel_roles || self.explicit_channel_layout
    }

    pub const fn uses_explicit_channel_layout(&self) -> bool {
        self.explicit_channel_layout
    }

    pub const fn source_range(&self) -> SourceFrameRange {
        self.range
    }

    pub fn decoder_route_id(&self) -> String {
        format!(
            "forge-input-descriptor-v2:{}:{}:audio-index={}:track-id={}",
            self.container.id(),
            self.codec.id(),
            self.track_index,
            self.track_id
        )
    }
}

struct RegistryProbe {
    route: DecoderRoute,
    container: AudioContainer,
    codec: AudioCodec,
    track_index: u32,
    track_id: u32,
    info: StreamInfo,
    declared_frames: Option<u64>,
    decoder_layout_provenance: ChannelLayoutProvenance,
    channel_layout: ChannelLayoutDescriptor,
    service_preflight: Option<ServiceContainerPreflight>,
}

struct RegistryIdentity {
    container: AudioContainer,
    codec: AudioCodec,
    track_index: u32,
    track_id: u32,
    stream: Option<(StreamInfo, ChannelLayoutDescriptor, Option<u64>)>,
}

fn validate_descriptor_options(options: &InputDescriptorOptions) -> Result<(), String> {
    if !options.start_seconds.is_finite() || options.start_seconds < 0.0 {
        return Err("input descriptor start must be finite and non-negative".into());
    }
    if options
        .duration_seconds
        .is_some_and(|duration| !duration.is_finite() || duration <= 0.0)
    {
        return Err("input descriptor duration must be finite and positive".into());
    }
    if options
        .channel_roles
        .as_ref()
        .is_some_and(|roles| roles.is_empty() || roles.len() > usize::from(u16::MAX))
    {
        return Err("input descriptor channel layout must contain 1..=65535 roles".into());
    }
    if let Some(layout) = &options.channel_layout {
        layout.validate()?;
    }
    Ok(())
}

fn source_frame_range(
    sample_rate: u32,
    start_seconds: f64,
    duration_seconds: Option<f64>,
) -> Result<SourceFrameRange, String> {
    let frames = |name: &str, seconds: f64| {
        let value = seconds * f64::from(sample_rate);
        if !value.is_finite() || value.round() > u64::MAX as f64 {
            return Err(format!(
                "input descriptor {name} exceeds the decoded-frame domain"
            ));
        }
        Ok(value.round() as u64)
    };
    let start = frames("start", start_seconds)?;
    let frames = duration_seconds
        .map(|duration| frames("duration", duration))
        .transpose()?;
    if duration_seconds.is_some() && frames == Some(0) {
        return Err("input descriptor duration rounds to zero decoded frames".into());
    }
    if let Some(length) = frames {
        start
            .checked_add(length)
            .ok_or_else(|| "input descriptor frame range overflows u64".to_string())?;
    }
    Ok(SourceFrameRange { start, frames })
}

fn probe_registry_with_control<C>(
    input: &StableInput,
    selection: AudioTrackSelection,
    max_packet_samples: Option<u64>,
    mut checkpoint: C,
) -> Result<RegistryProbe, String>
where
    C: FnMut() -> Result<(), String> + Send,
{
    checkpoint()?;
    let path = input.stable_path();
    let service_preflight = max_packet_samples
        .map(|_| service_container_preflight(path, &mut checkpoint))
        .transpose()?;
    let route = sniff_decoder_route(path)?;
    if service_preflight
        .is_some_and(|preflight| !service_preflight_accepts_decoder_route(preflight.route, route))
    {
        return Err("service preflight route disagrees with the selected decoder route".into());
    }
    checkpoint()?;
    let display = display_input(input);
    if route == DecoderRoute::Wave {
        require_single_track(selection)?;
        let (wav, channel_layout) =
            WavReader::probe_with_channel_layout_controlled(path, &mut checkpoint)
                .map_err(|error| format!("{display}: {error}"))?;
        let bytes_per_frame = u64::from(wav.channels) * wav.kind.bytes_per_sample() as u64;
        let declared_frames = Some(wav.data_size / bytes_per_frame);
        let kind = wav.kind;
        let decoder_layout_provenance = channel_layout.provenance();
        return Ok(RegistryProbe {
            route,
            container: AudioContainer::Wave,
            codec: AudioCodec::Pcm(kind),
            track_index: 0,
            track_id: 0,
            info: StreamInfo {
                sample_rate: wav.sample_rate,
                channels: wav.channels,
                channel_roles: wav.channel_roles,
                source_kind: kind,
            },
            declared_frames,
            decoder_layout_provenance,
            channel_layout,
            service_preflight,
        });
    }
    let identity = if route == DecoderRoute::Symphonia && max_packet_samples.is_some() {
        probe_symphonia_identity_at_controlled(
            path,
            input.source_name_hint(),
            &display,
            selection,
            service_preflight.expect("controlled probe has service preflight"),
            &mut checkpoint,
        )?
    } else {
        registry_identity_at(path, input.source_name_hint(), &display, route, selection)?
    };
    if let Some((info, decoder_channel_layout, declared_frames)) = identity.stream {
        let decoder_layout_provenance = decoder_channel_layout.provenance();
        let channel_layout = if identity.container == AudioContainer::IsoBmff {
            let exact = if max_packet_samples.is_some() {
                crate::isobmff_qc::probe_channel_layout_controlled(
                    path,
                    identity.track_id,
                    info.channels,
                    &mut checkpoint,
                )?
            } else {
                crate::isobmff_qc::probe_channel_layout(path, identity.track_id, info.channels)?
            };
            exact.unwrap_or(decoder_channel_layout)
        } else {
            decoder_channel_layout
        };
        return Ok(RegistryProbe {
            route,
            container: identity.container,
            codec: identity.codec,
            track_index: identity.track_index,
            track_id: identity.track_id,
            info,
            declared_frames,
            decoder_layout_provenance,
            channel_layout,
            service_preflight,
        });
    }

    const PROBE_COMPLETE: &str = "__forge_input_descriptor_probe_complete__";
    let mut captured = None;
    let decoded = decode_stream_raw_with_selection_and_control(
        path,
        route,
        selection,
        None,
        max_packet_samples.map(|max_packet_samples| ServiceDecodeControl {
            max_packet_samples,
            expected_preflight: service_preflight,
        }),
        &mut checkpoint,
        |info, provenance, declared_frames, _| {
            captured = Some((info.clone(), provenance, declared_frames));
            Err(PROBE_COMPLETE.into())
        },
    );
    match decoded {
        Err(error) if error == PROBE_COMPLETE => {}
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let (info, layout_provenance, declared_frames) = captured.ok_or_else(|| {
        format!(
            "{}: selected audio track decoded no frames",
            display_input(input)
        )
    })?;
    let channel_layout = if identity.container == AudioContainer::IsoBmff {
        let exact = if max_packet_samples.is_some() {
            crate::isobmff_qc::probe_channel_layout_controlled(
                path,
                identity.track_id,
                info.channels,
                &mut checkpoint,
            )?
        } else {
            crate::isobmff_qc::probe_channel_layout(path, identity.track_id, info.channels)?
        };
        exact.unwrap_or_else(|| {
            ChannelLayoutDescriptor::decoded_from_roles(&info.channel_roles, layout_provenance)
        })
    } else {
        ChannelLayoutDescriptor::decoded_from_roles(&info.channel_roles, layout_provenance)
    };
    Ok(RegistryProbe {
        route,
        container: identity.container,
        codec: identity.codec,
        track_index: identity.track_index,
        track_id: identity.track_id,
        info,
        declared_frames,
        decoder_layout_provenance: layout_provenance,
        channel_layout,
        service_preflight,
    })
}

fn registry_identity_at(
    path: &Path,
    hint_path: Option<&Path>,
    display: &str,
    route: DecoderRoute,
    selection: AudioTrackSelection,
) -> Result<RegistryIdentity, String> {
    match route {
        DecoderRoute::Wave => {
            require_single_track(selection)?;
            let wav = WavReader::probe_with_layout(path)
                .map_err(|error| format!("{display}: {error}"))?
                .0;
            Ok(RegistryIdentity {
                container: AudioContainer::Wave,
                codec: AudioCodec::Pcm(wav.kind),
                track_index: 0,
                track_id: 0,
                stream: None,
            })
        }
        DecoderRoute::Dsf => {
            require_single_track(selection)?;
            Ok(RegistryIdentity {
                container: AudioContainer::Dsf,
                codec: AudioCodec::Dsd,
                track_index: 0,
                track_id: 0,
                stream: None,
            })
        }
        DecoderRoute::Dsdiff => {
            require_single_track(selection)?;
            Ok(RegistryIdentity {
                container: AudioContainer::Dsdiff,
                codec: AudioCodec::Dsd,
                track_index: 0,
                track_id: 0,
                stream: None,
            })
        }
        DecoderRoute::Opus => {
            require_single_track(selection)?;
            Ok(RegistryIdentity {
                container: AudioContainer::Ogg,
                codec: AudioCodec::Opus,
                track_index: 0,
                track_id: 0,
                stream: None,
            })
        }
        DecoderRoute::Symphonia => probe_symphonia_identity_at(path, hint_path, display, selection),
    }
}

fn display_input(input: &StableInput) -> String {
    input
        .source_name_hint()
        .unwrap_or_else(|| input.stable_path())
        .display()
        .to_string()
}

fn require_single_track(selection: AudioTrackSelection) -> Result<(), String> {
    match selection {
        AudioTrackSelection::Default
        | AudioTrackSelection::Index(0)
        | AudioTrackSelection::Id(0) => Ok(()),
        AudioTrackSelection::Index(index) => Err(format!(
            "audio track index {index} is unavailable; this container has one audio track"
        )),
        AudioTrackSelection::Id(id) => Err(format!(
            "audio track ID {id} is unavailable; this container uses track ID 0"
        )),
    }
}

/// Validate attacker-controlled container framing without materializing media
/// payloads. The immutable service snapshot is scanned before any third-party
/// demuxer sees it, so a later parser allocation cannot exceed the admitted
/// packet/metadata bounds merely by trusting an encoded length field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceContainerSniff {
    NativeBounded,
    Ogg,
    Matroska,
    Flac,
    IsoBmff,
    RawMpegOrAdts,
    UnsupportedMetadataContainer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceContainerPreflightRoute {
    NativeBounded,
    Ogg,
    Matroska,
    Flac,
    IsoBmff,
    Mpa,
    Adts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ServiceContainerPreflight {
    route: ServiceContainerPreflightRoute,
    media_offset: u64,
    media_end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ServiceDecodeControl {
    max_packet_samples: u64,
    expected_preflight: Option<ServiceContainerPreflight>,
}

fn service_container_preflight_sniff(prefix: &[u8], file_len: u64) -> ServiceContainerSniff {
    if (prefix.len() >= 12
        && matches!(&prefix[..4], b"RIFF" | b"RF64" | b"BW64")
        && &prefix[8..12] == b"WAVE")
        || prefix.starts_with(b"DSD ")
        || (prefix.len() >= 16 && &prefix[..4] == b"FRM8" && &prefix[12..16] == b"DSD ")
    {
        ServiceContainerSniff::NativeBounded
    } else if prefix.starts_with(b"OggS") {
        ServiceContainerSniff::Ogg
    } else if prefix.starts_with(&0x1a45_dfa3_u32.to_be_bytes()) {
        ServiceContainerSniff::Matroska
    } else if prefix.starts_with(b"fLaC") {
        ServiceContainerSniff::Flac
    } else if crate::isobmff_qc::looks_like_isobmff(prefix, file_len) {
        ServiceContainerSniff::IsoBmff
    } else if (prefix.len() >= 12
        && &prefix[..4] == b"FORM"
        && matches!(&prefix[8..12], b"AIFF" | b"AIFC"))
        || prefix.starts_with(b"caff")
    {
        // These formats are not enabled in the pinned Symphonia build. Keep an
        // explicit fail-closed registry entry so enabling one cannot silently
        // bypass metadata allocation preflight.
        ServiceContainerSniff::UnsupportedMetadataContainer
    } else {
        ServiceContainerSniff::RawMpegOrAdts
    }
}

fn service_container_preflight<C>(
    path: &Path,
    mut checkpoint: C,
) -> Result<ServiceContainerPreflight, String>
where
    C: FnMut() -> Result<(), String> + Send,
{
    checkpoint()?;
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len();
    let mut prefix = [0_u8; 16];
    let prefix_len = file
        .read(&mut prefix)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let prefix = &prefix[..prefix_len];
    let mut metadata = ServiceMetadataContext::default();
    let preflight = match service_container_preflight_sniff(prefix, file_len) {
        ServiceContainerSniff::NativeBounded => {
            // The WAVE and DSD readers perform their own controlled structural
            // preflight before allocating decoded output.
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::NativeBounded,
                media_offset: 0,
                media_end: file_len,
            }
        }
        ServiceContainerSniff::Ogg => {
            preflight_ogg_packets(
                path,
                file_len,
                SERVICE_MAX_ENCODED_PACKET_BYTES,
                &mut checkpoint,
            )?;
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Ogg,
                media_offset: 0,
                media_end: file_len,
            }
        }
        ServiceContainerSniff::Matroska => {
            preflight_matroska(
                path,
                file_len,
                SERVICE_MAX_ENCODED_PACKET_BYTES,
                &mut checkpoint,
            )?;
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Matroska,
                media_offset: 0,
                media_end: file_len,
            }
        }
        ServiceContainerSniff::Flac => {
            preflight_flac_metadata(path, file_len, &mut metadata.budget, &mut checkpoint)?;
            preflight_trailing_ape(path, file_len, &mut metadata, &mut checkpoint)?;
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Flac,
                media_offset: 0,
                media_end: file_len,
            }
        }
        ServiceContainerSniff::IsoBmff => {
            preflight_isobmff_top_level(path, file_len, &mut checkpoint)?;
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::IsoBmff,
                media_offset: 0,
                media_end: file_len,
            }
        }
        ServiceContainerSniff::RawMpegOrAdts => {
            preflight_trailing_ape(path, file_len, &mut metadata, &mut checkpoint)?;
            preflight_raw_mpeg_or_id3(path, file_len, prefix, &mut metadata, &mut checkpoint)?
        }
        ServiceContainerSniff::UnsupportedMetadataContainer => {
            return Err(format!(
                "{}: service input format has no registered bounded metadata preflight",
                path.display()
            ));
        }
    };
    checkpoint()?;
    Ok(preflight)
}

fn preflight_raw_mpeg_or_id3<C>(
    path: &Path,
    file_len: u64,
    prefix: &[u8],
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<ServiceContainerPreflight, String>
where
    C: FnMut() -> Result<(), String>,
{
    if service_raw_audio_frame(prefix, file_len).is_some()
        || (prefix.len() >= 3 && &prefix[..3] == b"ID3")
    {
        return preflight_raw_mpeg_or_id3_at(path, file_len, 0, prefix, metadata, checkpoint);
    }

    let Some(audio_start) = preflight_leading_ape(path, file_len, metadata, checkpoint)? else {
        return Err(format!(
            "{}: service input has no bounded audio-container signature",
            path.display()
        ));
    };
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(audio_start))
        .map_err(|error| format!("seek {} after leading APE: {error}", path.display()))?;
    let mut audio_prefix = [0_u8; 16];
    let prefix_len = file
        .read(&mut audio_prefix)
        .map_err(|error| format!("read {} after leading APE: {error}", path.display()))?;
    preflight_raw_mpeg_or_id3_at(
        path,
        file_len,
        audio_start,
        &audio_prefix[..prefix_len],
        metadata,
        checkpoint,
    )
}

fn preflight_raw_mpeg_or_id3_at<C>(
    path: &Path,
    file_len: u64,
    start: u64,
    prefix: &[u8],
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<ServiceContainerPreflight, String>
where
    C: FnMut() -> Result<(), String>,
{
    if service_raw_audio_frame(prefix, file_len.saturating_sub(start)).is_some() {
        return preflight_raw_audio_frames(path, file_len, start, metadata, checkpoint);
    }
    if prefix.starts_with(b"fLaC") {
        preflight_flac_metadata_at(path, file_len, start, &mut metadata.budget, checkpoint)?;
        return Ok(ServiceContainerPreflight {
            route: ServiceContainerPreflightRoute::Flac,
            media_offset: start,
            media_end: file_len,
        });
    }
    if prefix.len() < 10 || &prefix[..3] != b"ID3" {
        return Err(format!(
            "{}: service input has no bounded audio-container signature",
            path.display()
        ));
    }
    let end = preflight_id3v2_at(path, file_len, start, metadata, checkpoint)?;
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(end))
        .map_err(|error| format!("seek {} after ID3: {error}", path.display()))?;
    let mut sync = [0_u8; 16];
    let read = file
        .read(&mut sync)
        .map_err(|error| format!("read {} after ID3: {error}", path.display()))?;
    if service_raw_audio_frame(&sync[..read], file_len.saturating_sub(end)).is_none() {
        return Err(format!(
            "{}: ID3 metadata is not followed by bounded MPEG/ADTS audio",
            path.display()
        ));
    }
    preflight_raw_audio_frames(path, file_len, end, metadata, checkpoint)
}

/// Return the complete first-frame size only after validating the encoded
/// MPEG audio or ADTS geometry. A two-byte sync word is deliberately
/// insufficient: supplemental metadata probing must not stop at an arbitrary
/// `ff fb` sequence embedded before a later tag.
fn service_raw_audio_frame(
    prefix: &[u8],
    remaining: u64,
) -> Option<(ServiceContainerPreflightRoute, u64)> {
    if prefix.len() < 4 || prefix[0] != 0xff || prefix[1] & 0xe0 != 0xe0 {
        return None;
    }
    let layer = (prefix[1] >> 1) & 0x03;
    if layer == 0 {
        if prefix.len() < 7 || prefix[1] & 0x06 != 0 || prefix[2] >> 2 & 0x0f >= 13 {
            return None;
        }
        let header_bytes = if prefix[1] & 1 == 0 { 9 } else { 7 };
        if prefix.len() < header_bytes {
            return None;
        }
        let channel_configuration = ((u16::from(prefix[2] & 1)) << 2) | u16::from(prefix[3] >> 6);
        let frame_bytes = (u64::from(prefix[3] & 0x03) << 11)
            | (u64::from(prefix[4]) << 3)
            | u64::from(prefix[5] >> 5);
        return (channel_configuration != 0
            && frame_bytes >= header_bytes as u64
            && frame_bytes <= SERVICE_MAX_ENCODED_PACKET_BYTES
            && frame_bytes <= remaining)
            .then_some((ServiceContainerPreflightRoute::Adts, frame_bytes));
    }

    let version = (prefix[1] >> 3) & 0x03;
    let bitrate_index = usize::from(prefix[2] >> 4);
    let rate_index = usize::from((prefix[2] >> 2) & 0x03);
    if version == 1
        || bitrate_index == 0
        || bitrate_index == 15
        || rate_index == 3
        || prefix[3] & 0x03 == 2
    {
        return None;
    }
    const MPEG1_L1: [u32; 14] = [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const MPEG1_L2: [u32; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const MPEG1_L3: [u32; 14] = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const MPEG2_L1: [u32; 14] = [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const MPEG2_L23: [u32; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let table = match (version, layer) {
        (3, 3) => &MPEG1_L1,
        (3, 2) => &MPEG1_L2,
        (3, 1) => &MPEG1_L3,
        (_, 3) => &MPEG2_L1,
        _ => &MPEG2_L23,
    };
    let bitrate = u64::from(table[bitrate_index - 1]) * 1_000;
    let base_rate = [44_100_u64, 48_000, 32_000][rate_index];
    let sample_rate = match version {
        3 => base_rate,
        2 => base_rate / 2,
        0 => base_rate / 4,
        _ => return None,
    };
    let padding = u64::from((prefix[2] >> 1) & 1);
    let frame_bytes = match layer {
        3 => (12 * bitrate / sample_rate + padding) * 4,
        2 => 144 * bitrate / sample_rate + padding,
        1 if version == 3 => 144 * bitrate / sample_rate + padding,
        1 => 72 * bitrate / sample_rate + padding,
        _ => return None,
    };
    ((4..=SERVICE_MAX_ENCODED_PACKET_BYTES).contains(&frame_bytes) && frame_bytes <= remaining)
        .then_some((ServiceContainerPreflightRoute::Mpa, frame_bytes))
}

#[cfg(test)]
fn service_raw_audio_frame_bytes(prefix: &[u8], remaining: u64) -> Option<u64> {
    service_raw_audio_frame(prefix, remaining).map(|(_, bytes)| bytes)
}

fn service_raw_audio_end(
    path: &Path,
    file_len: u64,
    audio_start: u64,
    metadata: &ServiceMetadataContext,
) -> Result<u64, String> {
    let id3v1_start = if let Some(start) = file_len.checked_sub(128) {
        let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        file.seek(SeekFrom::Start(start))
            .map_err(|error| format!("seek {} ID3v1 tag: {error}", path.display()))?;
        let mut marker = [0_u8; 3];
        file.read_exact(&mut marker)
            .map_err(|error| format!("read {} ID3v1 tag: {error}", path.display()))?;
        (&marker == b"TAG").then_some(start)
    } else {
        None
    };
    let mut audio_end = metadata
        .ape_ranges
        .iter()
        .filter_map(|&(start, end)| {
            (start >= audio_start && (end == file_len || id3v1_start == Some(end))).then_some(start)
        })
        .min()
        .unwrap_or(file_len);
    if let Some(id3v1_start) = id3v1_start {
        audio_end = audio_end.min(id3v1_start);
    }
    Ok(audio_end)
}

fn preflight_raw_audio_frames<C>(
    path: &Path,
    file_len: u64,
    audio_start: u64,
    metadata: &ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<ServiceContainerPreflight, String>
where
    C: FnMut() -> Result<(), String>,
{
    let audio_end = service_raw_audio_end(path, file_len, audio_start, metadata)?;
    if audio_end <= audio_start {
        return Err(format!("{}: raw audio contains no frames", path.display()));
    }
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut cursor = audio_start;
    let mut frame_count = 0_u64;
    let mut route = None;
    while cursor < audio_end {
        if frame_count.is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS as u64) {
            checkpoint()?;
        }
        file.seek(SeekFrom::Start(cursor))
            .map_err(|error| format!("seek {} raw audio frame: {error}", path.display()))?;
        let remaining = audio_end - cursor;
        let mut prefix = [0_u8; 16];
        let wanted = usize::try_from(remaining.min(prefix.len() as u64)).unwrap();
        file.read_exact(&mut prefix[..wanted])
            .map_err(|error| format!("read {} raw audio frame: {error}", path.display()))?;
        let Some((frame_route, frame_bytes)) =
            service_raw_audio_frame(&prefix[..wanted], remaining)
        else {
            return Err(format!(
                "{}: raw MPEG/ADTS stream contains inter-frame data or invalid geometry at byte {cursor}",
                path.display()
            ));
        };
        if route.is_some_and(|route| route != frame_route) {
            return Err(format!(
                "{}: raw audio changes MPEG/ADTS format class at byte {cursor}",
                path.display()
            ));
        }
        route = Some(frame_route);
        cursor = cursor
            .checked_add(frame_bytes)
            .ok_or_else(|| "raw audio frame offset overflow".to_string())?;
        frame_count = frame_count
            .checked_add(1)
            .ok_or_else(|| "raw audio frame count overflow".to_string())?;
    }
    let route = route.ok_or_else(|| format!("{}: raw audio contains no frames", path.display()))?;
    Ok(ServiceContainerPreflight {
        route,
        media_offset: audio_start,
        media_end: audio_end,
    })
}

struct ServiceId3BodyReader<'a, C> {
    path: &'a Path,
    file: &'a mut File,
    raw_remaining: u64,
    unsynchronised: bool,
    previous: u8,
    buffer: [u8; SERVICE_CONTROLLED_READ_BYTES],
    buffer_offset: usize,
    buffer_len: usize,
    checkpoint: &'a mut C,
}

impl<C> ServiceId3BodyReader<'_, C>
where
    C: FnMut() -> Result<(), String>,
{
    fn raw_byte(&mut self) -> Result<u8, String> {
        if self.raw_remaining == 0 {
            return Err(format!("{}: truncated ID3 body", self.path.display()));
        }
        if self.buffer_offset == self.buffer_len {
            (self.checkpoint)()?;
            let wanted = usize::try_from(self.raw_remaining.min(self.buffer.len() as u64)).unwrap();
            self.file
                .read_exact(&mut self.buffer[..wanted])
                .map_err(|error| format!("read {} ID3 body: {error}", self.path.display()))?;
            self.buffer_offset = 0;
            self.buffer_len = wanted;
        }
        let byte = self.buffer[self.buffer_offset];
        self.buffer_offset += 1;
        self.raw_remaining -= 1;
        Ok(byte)
    }

    fn byte(&mut self) -> Result<u8, String> {
        let mut byte = self.raw_byte()?;
        if self.unsynchronised && self.previous == 0xff && byte == 0 {
            byte = self.raw_byte()?;
        }
        self.previous = byte;
        Ok(byte)
    }

    fn read<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut bytes = [0_u8; N];
        for byte in &mut bytes {
            *byte = self.byte()?;
        }
        Ok(bytes)
    }

    fn skip(&mut self, bytes: u64) -> Result<(), String> {
        for _ in 0..bytes {
            self.byte()?;
        }
        Ok(())
    }
}

fn service_syncsafe(bytes: [u8; 4]) -> Result<u64, String> {
    if bytes.iter().any(|byte| byte & 0x80 != 0) {
        return Err("invalid ID3 syncsafe integer".into());
    }
    Ok(bytes
        .into_iter()
        .fold(0_u64, |value, byte| (value << 7) | u64::from(byte)))
}

fn service_id3_frame_id_valid(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn preflight_id3v2_at<C>(
    path: &Path,
    file_len: u64,
    start: u64,
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<u64, String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek {} ID3 header: {error}", path.display()))?;
    let mut header = [0_u8; 10];
    file.read_exact(&mut header)
        .map_err(|error| format!("read {} ID3 header: {error}", path.display()))?;
    let version = header[3];
    let flags = header[5];
    if &header[..3] != b"ID3"
        || !(2..=4).contains(&version)
        || header[4] == 0xff
        || (version == 2 && flags & 0x40 != 0)
    {
        return Err(format!("{}: invalid ID3 header", path.display()));
    }
    let body = service_syncsafe(header[6..10].try_into().unwrap())?;
    ServiceMetadataBudget::validate_item(body)?;
    let footer_bytes = if version == 4 && flags & 0x10 != 0 {
        10
    } else {
        0
    };
    let end = start
        .checked_add(10)
        .and_then(|value| value.checked_add(body))
        .and_then(|value| value.checked_add(footer_bytes))
        .filter(|&value| value <= file_len)
        .ok_or_else(|| format!("{}: truncated ID3 tag", path.display()))?;
    metadata.budget.add_encoded_bytes(
        10_u64
            .checked_add(body)
            .and_then(|value| value.checked_add(footer_bytes))
            .ok_or_else(|| "ID3 metadata size overflow".to_string())?,
    )?;
    checkpoint()?;

    {
        let mut reader = ServiceId3BodyReader {
            path,
            file: &mut file,
            raw_remaining: body,
            unsynchronised: version < 4 && flags & 0x80 != 0,
            previous: 0,
            buffer: [0; SERVICE_CONTROLLED_READ_BYTES],
            buffer_offset: 0,
            buffer_len: 0,
            checkpoint,
        };
        if flags & 0x40 != 0 {
            match version {
                3 => {
                    let size = u32::from_be_bytes(reader.read()?);
                    if !matches!(size, 6 | 10) {
                        return Err("invalid ID3v2.3 extended header size".into());
                    }
                    let ext_flags = u16::from_be_bytes(reader.read()?);
                    reader.read::<4>()?;
                    if size == 10 {
                        if ext_flags & 0x8000 == 0 {
                            return Err("invalid ID3v2.3 CRC extended header".into());
                        }
                        reader.read::<4>()?;
                    }
                }
                4 => {
                    let size = service_syncsafe(reader.read()?)?;
                    if size < 6 {
                        return Err("invalid ID3v2.4 extended header size".into());
                    }
                    let flag_bytes = reader.byte()?;
                    let ext_flags = reader.byte()?;
                    if flag_bytes != 1 || ext_flags & 0x8f != 0 {
                        return Err("invalid ID3v2.4 extended header flags".into());
                    }
                    let mut consumed = 6_u64;
                    for flag in [0x40, 0x20, 0x10] {
                        if ext_flags & flag != 0 {
                            let length = reader.byte()?;
                            let valid = match flag {
                                0x40 => matches!(length, 0 | 1),
                                0x20 => length == 5,
                                0x10 => length == 1,
                                _ => unreachable!(),
                            };
                            if !valid {
                                return Err("invalid ID3v2.4 extended header field".into());
                            }
                            reader.skip(u64::from(length))?;
                            consumed = consumed
                                .checked_add(1 + u64::from(length))
                                .ok_or_else(|| "ID3 extended header overflow".to_string())?;
                        }
                    }
                    if consumed > size {
                        return Err("truncated ID3v2.4 extended header".into());
                    }
                    reader.skip(size - consumed)?;
                }
                _ => return Err("ID3v2.2 does not define an extended header".into()),
            }
        }

        let frame_header_bytes = if version == 2 { 6_u64 } else { 10_u64 };
        let mut frame_count = 0_usize;
        while reader.raw_remaining >= frame_header_bytes {
            let mut id = [0_u8; 4];
            let id_len = if version == 2 {
                id[..3].copy_from_slice(&reader.read::<3>()?);
                3
            } else {
                id.copy_from_slice(&reader.read::<4>()?);
                4
            };
            if !service_id3_frame_id_valid(&id[..id_len]) {
                break;
            }
            frame_count = frame_count
                .checked_add(1)
                .filter(|&count| count <= SERVICE_MAX_CONTAINER_ITEMS)
                .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
            metadata.budget.add_entries(1)?;
            if frame_count.is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS) {
                (reader.checkpoint)()?;
            }
            let size = match version {
                2 => {
                    let bytes = reader.read::<3>()?;
                    u64::from(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
                }
                3 => u64::from(u32::from_be_bytes(reader.read()?)),
                4 => service_syncsafe(reader.read()?)?,
                _ => unreachable!(),
            };
            if version >= 3 {
                let frame_flags = u16::from_be_bytes(reader.read()?);
                if (version == 3 && frame_flags & 0x1f1f != 0)
                    || (version == 4 && frame_flags & 0x8fb0 != 0)
                    || (version == 4 && frame_flags & 0x08 != 0 && frame_flags & 0x01 == 0)
                {
                    return Err("invalid ID3 frame flags".into());
                }
                let flag_bytes = if version == 3 {
                    u64::from(frame_flags & 0x80 != 0) * 4
                        + u64::from(frame_flags & 0x40 != 0)
                        + u64::from(frame_flags & 0x20 != 0)
                } else {
                    u64::from(frame_flags & 0x40 != 0)
                        + u64::from(frame_flags & 0x04 != 0)
                        + u64::from(frame_flags & 0x01 != 0) * 4
                };
                if flag_bytes > size {
                    return Err("ID3 frame is smaller than its flag fields".into());
                }
            }
            ServiceMetadataBudget::validate_item(size)?;
            reader.skip(size)?;
        }
    }

    if footer_bytes != 0 {
        file.seek(SeekFrom::Start(end - 10))
            .map_err(|error| format!("seek {} ID3 footer: {error}", path.display()))?;
        let mut footer = [0_u8; 10];
        file.read_exact(&mut footer)
            .map_err(|error| format!("read {} ID3 footer: {error}", path.display()))?;
        if &footer[..3] != b"3DI" || footer[3..6] != header[3..6] || footer[6..10] != header[6..10]
        {
            return Err(format!("{}: invalid ID3 footer", path.display()));
        }
    }
    Ok(end)
}

const SERVICE_APE_DESCRIPTOR_BYTES: u64 = 32;
const SERVICE_APE_HAS_HEADER: u32 = 0x8000_0000;
const SERVICE_APE_HAS_FOOTER: u32 = 0x4000_0000;
const SERVICE_APE_IS_HEADER: u32 = 0x2000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ServiceApeDescriptor {
    version: u32,
    declared_size: u64,
    item_count: u32,
    has_header: bool,
    has_footer: bool,
    is_header: bool,
}

fn read_service_ape_descriptor(
    path: &Path,
    file: &mut File,
    offset: u64,
) -> Result<Option<ServiceApeDescriptor>, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} APE descriptor: {error}", path.display()))?;
    let mut raw = [0_u8; SERVICE_APE_DESCRIPTOR_BYTES as usize];
    file.read_exact(&mut raw)
        .map_err(|error| format!("read {} APE descriptor: {error}", path.display()))?;
    let version = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    if &raw[..8] != b"APETAGEX" || !matches!(version, 1000 | 2000) {
        return Ok(None);
    }

    let declared_size = u64::from(u32::from_le_bytes(raw[12..16].try_into().unwrap()));
    let item_count = u32::from_le_bytes(raw[16..20].try_into().unwrap());
    let flags = u32::from_le_bytes(raw[20..24].try_into().unwrap());
    let (has_header, has_footer, is_header) = if version == 1000 {
        (false, true, false)
    } else {
        (
            flags & SERVICE_APE_HAS_HEADER != 0,
            flags & SERVICE_APE_HAS_FOOTER != 0,
            flags & SERVICE_APE_IS_HEADER != 0,
        )
    };
    if !(SERVICE_APE_DESCRIPTOR_BYTES..=SERVICE_MAX_ENCODED_PACKET_BYTES).contains(&declared_size)
        || usize::try_from(item_count)
            .ok()
            .is_none_or(|count| count > SERVICE_MAX_CONTAINER_ITEMS)
    {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    Ok(Some(ServiceApeDescriptor {
        version,
        declared_size,
        item_count,
        has_header,
        has_footer,
        is_header,
    }))
}

fn service_ape_descriptors_match(
    header: ServiceApeDescriptor,
    footer: ServiceApeDescriptor,
) -> bool {
    header.version == footer.version
        && header.declared_size == footer.declared_size
        && header.item_count == footer.item_count
        && header.has_header == footer.has_header
        && header.has_footer == footer.has_footer
        && header.is_header != footer.is_header
}

fn preflight_ape_items<C>(
    path: &Path,
    file: &mut File,
    items_start: u64,
    items_end: u64,
    descriptor: ServiceApeDescriptor,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut cursor = items_start;
    for item in 0..descriptor.item_count {
        if usize::try_from(item)
            .unwrap_or(usize::MAX)
            .is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS)
        {
            checkpoint()?;
        }
        let fields_end = cursor
            .checked_add(8)
            .ok_or_else(|| "APE item header offset overflow".to_string())?;
        if fields_end > items_end {
            return Err(format!("{}: truncated APE item", path.display()));
        }
        file.seek(SeekFrom::Start(cursor))
            .map_err(|error| format!("seek {} APE item: {error}", path.display()))?;
        let mut fields = [0_u8; 8];
        file.read_exact(&mut fields)
            .map_err(|error| format!("read {} APE item: {error}", path.display()))?;
        let value_len = u64::from(u32::from_le_bytes(fields[..4].try_into().unwrap()));
        ServiceMetadataBudget::validate_item(value_len)?;
        cursor = fields_end;

        let mut key_bytes = 0_usize;
        loop {
            if cursor >= items_end || key_bytes > 255 {
                return Err(format!(
                    "{}: invalid or unterminated APE item key",
                    path.display()
                ));
            }
            file.seek(SeekFrom::Start(cursor))
                .map_err(|error| format!("seek {} APE item key: {error}", path.display()))?;
            let mut byte = [0_u8; 1];
            file.read_exact(&mut byte)
                .map_err(|error| format!("read {} APE item key: {error}", path.display()))?;
            cursor = cursor
                .checked_add(1)
                .ok_or_else(|| "APE item key offset overflow".to_string())?;
            if byte[0] == 0 {
                break;
            }
            key_bytes += 1;
        }
        if !(2..=255).contains(&key_bytes) {
            return Err(format!("{}: invalid APE item key", path.display()));
        }
        cursor = cursor
            .checked_add(value_len)
            .ok_or_else(|| "APE item value offset overflow".to_string())?;
        if cursor > items_end {
            return Err(format!("{}: truncated APE value", path.display()));
        }
    }
    if cursor != items_end {
        return Err(format!(
            "{}: APE item table does not match its declared size",
            path.display()
        ));
    }
    Ok(())
}

fn preflight_ape_footer_at<C>(
    path: &Path,
    file: &mut File,
    file_len: u64,
    footer_start: u64,
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<bool, String>
where
    C: FnMut() -> Result<(), String>,
{
    let Some(footer) = read_service_ape_descriptor(path, file, footer_start)? else {
        return Ok(false);
    };
    checkpoint()?;
    if footer.is_header {
        preflight_ape_header_descriptor(
            path,
            file,
            footer_start,
            footer,
            file_len,
            metadata,
            checkpoint,
        )?;
        return Ok(true);
    }
    let footer_end = footer_start
        .checked_add(SERVICE_APE_DESCRIPTOR_BYTES)
        .ok_or_else(|| "APE footer offset overflow".to_string())?;
    let physical_size = footer
        .declared_size
        .checked_add(if footer.has_header {
            SERVICE_APE_DESCRIPTOR_BYTES
        } else {
            0
        })
        .filter(|&size| size <= SERVICE_MAX_ENCODED_PACKET_BYTES)
        .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
    let tag_start = footer_end
        .checked_sub(physical_size)
        .ok_or_else(|| format!("{}: truncated trailing APE metadata", path.display()))?;
    let items_start = if footer.has_header {
        let header = read_service_ape_descriptor(path, file, tag_start)?
            .ok_or_else(|| format!("{}: missing trailing APE header", path.display()))?;
        if !header.is_header || !service_ape_descriptors_match(header, footer) {
            return Err(format!(
                "{}: trailing APE header/footer mismatch",
                path.display()
            ));
        }
        tag_start
            .checked_add(SERVICE_APE_DESCRIPTOR_BYTES)
            .ok_or_else(|| "APE item offset overflow".to_string())?
    } else {
        tag_start
    };
    if !metadata.record_ape(
        tag_start,
        footer_end,
        usize::try_from(footer.item_count)
            .map_err(|_| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?,
    )? {
        return Ok(true);
    }
    preflight_ape_items(path, file, items_start, footer_start, footer, checkpoint)?;
    Ok(true)
}

fn preflight_ape_header_descriptor<C>(
    path: &Path,
    file: &mut File,
    marker_start: u64,
    header: ServiceApeDescriptor,
    file_len: u64,
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<u64, String>
where
    C: FnMut() -> Result<(), String>,
{
    if header.version != 2000 || !header.is_header || !header.has_header || !header.has_footer {
        return Err(format!("{}: invalid leading APEv2 header", path.display()));
    }
    let physical_size = header
        .declared_size
        .checked_add(SERVICE_APE_DESCRIPTOR_BYTES)
        .filter(|&size| size <= SERVICE_MAX_ENCODED_PACKET_BYTES)
        .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
    let tag_end = marker_start
        .checked_add(physical_size)
        .filter(|&end| end <= file_len)
        .ok_or_else(|| format!("{}: truncated leading APE metadata", path.display()))?;
    let footer_start = tag_end - SERVICE_APE_DESCRIPTOR_BYTES;
    let footer = read_service_ape_descriptor(path, file, footer_start)?
        .ok_or_else(|| format!("{}: missing leading APE footer", path.display()))?;
    if footer.is_header || !service_ape_descriptors_match(header, footer) {
        return Err(format!(
            "{}: leading APE header/footer mismatch",
            path.display()
        ));
    }
    if !metadata.record_ape(
        marker_start,
        tag_end,
        usize::try_from(header.item_count)
            .map_err(|_| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?,
    )? {
        return Ok(tag_end);
    }
    preflight_ape_items(
        path,
        file,
        marker_start
            .checked_add(SERVICE_APE_DESCRIPTOR_BYTES)
            .ok_or_else(|| "APE item offset overflow".to_string())?,
        footer_start,
        header,
        checkpoint,
    )?;
    Ok(tag_end)
}

/// Validate both offsets Symphonia probes for trailing APE metadata. The
/// `-160` candidate is inspected even without ID3v1 so an unsafe declaration
/// cannot reach the supplemental parser. It is excluded from a raw audio
/// range only when the final 128 bytes are a real `TAG` record.
fn preflight_trailing_ape<C>(
    path: &Path,
    file_len: u64,
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    for anchor in [SERVICE_APE_DESCRIPTOR_BYTES, 160] {
        let Some(footer_start) = file_len.checked_sub(anchor) else {
            continue;
        };
        preflight_ape_footer_at(
            path,
            &mut file,
            file_len,
            footer_start,
            metadata,
            checkpoint,
        )?;
    }
    Ok(())
}

/// Find and validate the first leading APEv2 marker in Symphonia's bounded
/// supplemental probe window. The returned offset is the first byte following
/// the tag, where the actual format marker must be validated separately.
fn preflight_leading_ape<C>(
    path: &Path,
    file_len: u64,
    metadata: &mut ServiceMetadataContext,
    checkpoint: &mut C,
) -> Result<Option<u64>, String>
where
    C: FnMut() -> Result<(), String>,
{
    const MARKER: &[u8; 12] = b"APETAGEX\xd0\x07\0\0";
    // Symphonia increments its byte counter before checking the two-byte
    // marker bloom filter, so a marker must start no later than depth - 2.
    let marker_start_limit = SERVICE_SYMPHONIA_PROBE_BYTES
        .checked_sub(2)
        .ok_or_else(|| "APE probe depth underflow".to_string())?;
    let scan_limit = marker_start_limit
        .checked_add(MARKER.len() as u64)
        .ok_or_else(|| "APE probe size overflow".to_string())?;
    let scan_len = file_len.min(scan_limit);
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut offset = 0_u64;
    let mut carry = Vec::new();
    let mut block = [0_u8; SERVICE_CONTROLLED_READ_BYTES];
    while offset < scan_len {
        checkpoint()?;
        let count = usize::try_from((scan_len - offset).min(block.len() as u64)).unwrap();
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} APE probe: {error}", path.display()))?;
        file.read_exact(&mut block[..count])
            .map_err(|error| format!("read {} APE probe: {error}", path.display()))?;
        let carry_len = carry.len();
        carry.extend_from_slice(&block[..count]);
        if let Some(position) = carry
            .windows(MARKER.len())
            .position(|bytes| bytes == MARKER)
        {
            let marker_start = offset
                .checked_sub(carry_len as u64)
                .and_then(|base| base.checked_add(position as u64))
                .ok_or_else(|| "APE probe offset overflow".to_string())?;
            if marker_start > marker_start_limit {
                return Ok(None);
            }
            let header = read_service_ape_descriptor(path, &mut file, marker_start)?
                .ok_or_else(|| "APEv2 probe marker disappeared".to_string())?;
            let tag_end = preflight_ape_header_descriptor(
                path,
                &mut file,
                marker_start,
                header,
                file_len,
                metadata,
                checkpoint,
            )?;
            return Ok(Some(tag_end));
        }
        if carry.len() >= MARKER.len() {
            carry.drain(..carry.len() - (MARKER.len() - 1));
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or_else(|| "APE probe offset overflow".to_string())?;
    }
    Ok(None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceOggCommentCodec {
    Other,
    Vorbis,
    Opus,
    Flac,
}

struct ServiceOggCommentScanner {
    codec: ServiceOggCommentCodec,
    stage: u8,
    prefix_seen: usize,
    field: [u8; 4],
    field_len: usize,
    remaining: u64,
    comments_remaining: u32,
    comments_seen: usize,
    declared_comments: usize,
    budgeted_comments: usize,
    total_bytes: u64,
    tail_bytes: u64,
    tail_first: Option<u8>,
}

impl ServiceOggCommentScanner {
    const PREFIX: u8 = 0;
    const VENDOR_LENGTH: u8 = 1;
    const VENDOR: u8 = 2;
    const COMMENT_COUNT: u8 = 3;
    const COMMENT_LENGTH: u8 = 4;
    const COMMENT: u8 = 5;
    const TAIL: u8 = 6;

    fn new(codec: ServiceOggCommentCodec) -> Self {
        Self {
            codec,
            stage: Self::PREFIX,
            prefix_seen: 0,
            field: [0; 4],
            field_len: 0,
            remaining: 0,
            comments_remaining: 0,
            comments_seen: 0,
            declared_comments: 0,
            budgeted_comments: 0,
            total_bytes: 0,
            tail_bytes: 0,
            tail_first: None,
        }
    }

    fn prefix(&self) -> &'static [u8] {
        match self.codec {
            ServiceOggCommentCodec::Vorbis => b"\x03vorbis",
            ServiceOggCommentCodec::Opus => b"OpusTags",
            ServiceOggCommentCodec::Flac | ServiceOggCommentCodec::Other => &[],
        }
    }

    fn push<C>(&mut self, bytes: &[u8], checkpoint: &mut C) -> Result<(), String>
    where
        C: FnMut() -> Result<(), String>,
    {
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "Ogg comment packet size overflow".to_string())?;
        if self.total_bytes > SERVICE_MAX_ENCODED_PACKET_BYTES {
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        }

        let mut offset = 0_usize;
        while offset < bytes.len() {
            match self.stage {
                Self::PREFIX => {
                    let prefix = self.prefix();
                    let count = (prefix.len() - self.prefix_seen).min(bytes.len() - offset);
                    if bytes[offset..offset + count]
                        != prefix[self.prefix_seen..self.prefix_seen + count]
                    {
                        return Err("invalid Ogg comment packet signature".into());
                    }
                    offset += count;
                    self.prefix_seen += count;
                    if self.prefix_seen == prefix.len() {
                        self.stage = Self::VENDOR_LENGTH;
                    }
                }
                Self::VENDOR_LENGTH | Self::COMMENT_COUNT | Self::COMMENT_LENGTH => {
                    let count = (4 - self.field_len).min(bytes.len() - offset);
                    self.field[self.field_len..self.field_len + count]
                        .copy_from_slice(&bytes[offset..offset + count]);
                    offset += count;
                    self.field_len += count;
                    if self.field_len != 4 {
                        continue;
                    }
                    let value = u32::from_le_bytes(self.field);
                    self.field_len = 0;
                    match self.stage {
                        Self::VENDOR_LENGTH => {
                            if u64::from(value) > SERVICE_MAX_METADATA_ITEM_BYTES {
                                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
                            }
                            self.remaining = u64::from(value);
                            self.stage = if value == 0 {
                                Self::COMMENT_COUNT
                            } else {
                                Self::VENDOR
                            };
                        }
                        Self::COMMENT_COUNT => {
                            let count = usize::try_from(value)
                                .ok()
                                .filter(|&count| count <= SERVICE_MAX_CONTAINER_ITEMS)
                                .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
                            if self.declared_comments != 0 {
                                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
                            }
                            self.declared_comments = count;
                            self.comments_remaining = value;
                            self.stage = if value == 0 {
                                Self::TAIL
                            } else {
                                Self::COMMENT_LENGTH
                            };
                        }
                        Self::COMMENT_LENGTH => {
                            if u64::from(value) > SERVICE_MAX_METADATA_ITEM_BYTES {
                                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
                            }
                            self.remaining = u64::from(value);
                            if value == 0 {
                                self.complete_comment(checkpoint)?;
                            } else {
                                self.stage = Self::COMMENT;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                Self::VENDOR | Self::COMMENT => {
                    let count = self.remaining.min((bytes.len() - offset) as u64);
                    offset += count as usize;
                    self.remaining -= count;
                    if self.remaining == 0 {
                        if self.stage == Self::VENDOR {
                            self.stage = Self::COMMENT_COUNT;
                        } else {
                            self.complete_comment(checkpoint)?;
                        }
                    }
                }
                Self::TAIL => {
                    if self.tail_first.is_none() {
                        self.tail_first = Some(bytes[offset]);
                    }
                    self.tail_bytes = self
                        .tail_bytes
                        .checked_add((bytes.len() - offset) as u64)
                        .ok_or_else(|| "Ogg comment tail size overflow".to_string())?;
                    offset = bytes.len();
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    fn complete_comment<C>(&mut self, checkpoint: &mut C) -> Result<(), String>
    where
        C: FnMut() -> Result<(), String>,
    {
        self.comments_remaining = self
            .comments_remaining
            .checked_sub(1)
            .ok_or_else(|| "Ogg comment count underflow".to_string())?;
        self.comments_seen = self
            .comments_seen
            .checked_add(1)
            .ok_or_else(|| "Ogg comment count overflow".to_string())?;
        if self
            .comments_seen
            .is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS)
        {
            checkpoint()?;
        }
        self.stage = if self.comments_remaining == 0 {
            Self::TAIL
        } else {
            Self::COMMENT_LENGTH
        };
        Ok(())
    }

    fn take_unbudgeted_comments(&mut self) -> usize {
        let comments = self
            .declared_comments
            .saturating_sub(self.budgeted_comments);
        self.budgeted_comments = self.declared_comments;
        comments
    }

    fn finish(&self) -> Result<(), String> {
        if self.stage != Self::TAIL {
            return Err("truncated Ogg comment packet".into());
        }
        if self.codec == ServiceOggCommentCodec::Vorbis
            && (self.tail_bytes != 1 || self.tail_first != Some(1))
        {
            return Err("invalid Vorbis comment framing byte".into());
        }
        if self.codec == ServiceOggCommentCodec::Flac && self.tail_bytes != 0 {
            return Err("FLAC Vorbis-comment block has trailing bytes".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceFlacPictureStage {
    PictureType,
    MimeLength,
    Mime,
    DescriptionLength,
    Description,
    Dimensions,
    DataLength,
    Data,
    Done,
}

struct ServiceFlacPictureScanner {
    stage: ServiceFlacPictureStage,
    field: [u8; 4],
    field_len: usize,
    remaining: u64,
}

impl Default for ServiceFlacPictureScanner {
    fn default() -> Self {
        Self {
            stage: ServiceFlacPictureStage::PictureType,
            field: [0; 4],
            field_len: 0,
            remaining: 0,
        }
    }
}

impl ServiceFlacPictureScanner {
    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut offset = 0_usize;
        while offset < bytes.len() {
            match self.stage {
                ServiceFlacPictureStage::PictureType
                | ServiceFlacPictureStage::MimeLength
                | ServiceFlacPictureStage::DescriptionLength
                | ServiceFlacPictureStage::DataLength => {
                    let count = (4 - self.field_len).min(bytes.len() - offset);
                    self.field[self.field_len..self.field_len + count]
                        .copy_from_slice(&bytes[offset..offset + count]);
                    self.field_len += count;
                    offset += count;
                    if self.field_len != 4 {
                        continue;
                    }
                    let value = u64::from(u32::from_be_bytes(self.field));
                    self.field_len = 0;
                    match self.stage {
                        ServiceFlacPictureStage::PictureType => {
                            self.stage = ServiceFlacPictureStage::MimeLength;
                        }
                        ServiceFlacPictureStage::MimeLength => {
                            ServiceMetadataBudget::validate_item(value)?;
                            self.remaining = value;
                            self.stage = if value == 0 {
                                ServiceFlacPictureStage::DescriptionLength
                            } else {
                                ServiceFlacPictureStage::Mime
                            };
                        }
                        ServiceFlacPictureStage::DescriptionLength => {
                            ServiceMetadataBudget::validate_item(value)?;
                            self.remaining = value;
                            self.stage = if value == 0 {
                                ServiceFlacPictureStage::Dimensions
                            } else {
                                ServiceFlacPictureStage::Description
                            };
                            if value == 0 {
                                self.remaining = 16;
                            }
                        }
                        ServiceFlacPictureStage::DataLength => {
                            ServiceMetadataBudget::validate_item(value)?;
                            self.remaining = value;
                            self.stage = if value == 0 {
                                ServiceFlacPictureStage::Done
                            } else {
                                ServiceFlacPictureStage::Data
                            };
                        }
                        _ => unreachable!(),
                    }
                }
                ServiceFlacPictureStage::Mime
                | ServiceFlacPictureStage::Description
                | ServiceFlacPictureStage::Dimensions
                | ServiceFlacPictureStage::Data => {
                    let count = self.remaining.min((bytes.len() - offset) as u64);
                    self.remaining -= count;
                    offset += count as usize;
                    if self.remaining != 0 {
                        continue;
                    }
                    self.stage = match self.stage {
                        ServiceFlacPictureStage::Mime => ServiceFlacPictureStage::DescriptionLength,
                        ServiceFlacPictureStage::Description => {
                            self.remaining = 16;
                            ServiceFlacPictureStage::Dimensions
                        }
                        ServiceFlacPictureStage::Dimensions => ServiceFlacPictureStage::DataLength,
                        ServiceFlacPictureStage::Data => ServiceFlacPictureStage::Done,
                        _ => unreachable!(),
                    };
                }
                ServiceFlacPictureStage::Done => {
                    return Err("FLAC picture block has trailing bytes".into());
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.stage == ServiceFlacPictureStage::Done {
            Ok(())
        } else {
            Err("truncated FLAC picture block".into())
        }
    }
}

enum ServiceOggFlacPayloadScanner {
    Skip,
    Comment(ServiceOggCommentScanner),
    Picture(ServiceFlacPictureScanner),
}

#[derive(Default)]
struct ServiceOggFlacPacketScanner {
    header: [u8; 4],
    header_len: usize,
    payload_remaining: u64,
    payload: Option<ServiceOggFlacPayloadScanner>,
    audio: bool,
}

impl ServiceOggFlacPacketScanner {
    fn push<C>(
        &mut self,
        bytes: &[u8],
        budget: &mut ServiceMetadataBudget,
        checkpoint: &mut C,
    ) -> Result<(), String>
    where
        C: FnMut() -> Result<(), String>,
    {
        let mut offset = 0_usize;
        if self.header_len == 0 && bytes.first() == Some(&0xff) {
            self.audio = true;
            return Ok(());
        }
        if self.audio {
            return Ok(());
        }
        if self.header_len < self.header.len() {
            let count = (self.header.len() - self.header_len).min(bytes.len());
            self.header[self.header_len..self.header_len + count].copy_from_slice(&bytes[..count]);
            self.header_len += count;
            offset += count;
            if self.header_len != self.header.len() {
                return Ok(());
            }
            let block_type = self.header[0] & 0x7f;
            if matches!(block_type, 0 | 0x7f) {
                return Err("invalid Ogg-FLAC metadata packet type".into());
            }
            let payload_len = u64::from(u32::from_be_bytes([
                0,
                self.header[1],
                self.header[2],
                self.header[3],
            ]));
            if matches!(block_type, 4 | 6) {
                ServiceMetadataBudget::validate_item(payload_len)?;
            }
            budget.add_entries(1)?;
            budget.add_encoded_bytes(
                4_u64
                    .checked_add(payload_len)
                    .ok_or_else(|| "Ogg-FLAC metadata size overflow".to_string())?,
            )?;
            self.payload_remaining = payload_len;
            self.payload = Some(match block_type {
                4 => ServiceOggFlacPayloadScanner::Comment(ServiceOggCommentScanner::new(
                    ServiceOggCommentCodec::Flac,
                )),
                6 => ServiceOggFlacPayloadScanner::Picture(ServiceFlacPictureScanner::default()),
                _ => ServiceOggFlacPayloadScanner::Skip,
            });
        }

        let available = (bytes.len() - offset) as u64;
        if available > self.payload_remaining {
            return Err("Ogg-FLAC metadata packet exceeds its declared block size".into());
        }
        let payload = &bytes[offset..];
        match self
            .payload
            .as_mut()
            .expect("header selects payload scanner")
        {
            ServiceOggFlacPayloadScanner::Skip => {}
            ServiceOggFlacPayloadScanner::Comment(scanner) => {
                scanner.push(payload, checkpoint)?;
                budget.add_entries(scanner.take_unbudgeted_comments())?;
            }
            ServiceOggFlacPayloadScanner::Picture(scanner) => scanner.push(payload)?,
        }
        self.payload_remaining -= available;
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.audio {
            return Ok(());
        }
        if self.header_len != self.header.len() || self.payload_remaining != 0 {
            return Err("truncated Ogg-FLAC metadata packet".into());
        }
        match self
            .payload
            .as_ref()
            .expect("complete header selects scanner")
        {
            ServiceOggFlacPayloadScanner::Skip => Ok(()),
            ServiceOggFlacPayloadScanner::Comment(scanner) => scanner.finish(),
            ServiceOggFlacPayloadScanner::Picture(scanner) => scanner.finish(),
        }
    }
}

struct ServiceOggMetadataPreflight {
    packet_index: usize,
    identity: [u8; 51],
    identity_len: usize,
    identity_bytes: u64,
    codec: Option<ServiceOggCommentCodec>,
    comment: Option<ServiceOggCommentScanner>,
    flac_packet: Option<ServiceOggFlacPacketScanner>,
    budget: ServiceMetadataBudget,
}

impl Default for ServiceOggMetadataPreflight {
    fn default() -> Self {
        Self {
            packet_index: 0,
            identity: [0; 51],
            identity_len: 0,
            identity_bytes: 0,
            codec: None,
            comment: None,
            flac_packet: None,
            budget: ServiceMetadataBudget::default(),
        }
    }
}

impl ServiceOggMetadataPreflight {
    fn wants_packet_bytes(&self) -> bool {
        self.packet_index == 0
            || (self.packet_index == 1
                && self.codec.is_some_and(|codec| {
                    matches!(
                        codec,
                        ServiceOggCommentCodec::Vorbis | ServiceOggCommentCodec::Opus
                    )
                }))
            || (self.packet_index >= 1 && self.codec == Some(ServiceOggCommentCodec::Flac))
    }

    fn push<C>(&mut self, bytes: &[u8], checkpoint: &mut C) -> Result<(), String>
    where
        C: FnMut() -> Result<(), String>,
    {
        match self.packet_index {
            0 => {
                self.identity_bytes = self
                    .identity_bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| "Ogg identity packet size overflow".to_string())?;
                let count = (self.identity.len() - self.identity_len).min(bytes.len());
                self.identity[self.identity_len..self.identity_len + count]
                    .copy_from_slice(&bytes[..count]);
                self.identity_len += count;
                Ok(())
            }
            _ if self.codec == Some(ServiceOggCommentCodec::Flac) => self
                .flac_packet
                .get_or_insert_with(ServiceOggFlacPacketScanner::default)
                .push(bytes, &mut self.budget, checkpoint),
            1 => {
                let Some(codec) = self.codec else {
                    return Err("Ogg codec identity was not completed".into());
                };
                if codec == ServiceOggCommentCodec::Other {
                    return Ok(());
                }
                self.budget.add_encoded_bytes(bytes.len() as u64)?;
                if self.comment.is_none() {
                    self.budget.add_entries(1)?;
                }
                let scanner = self
                    .comment
                    .get_or_insert_with(|| ServiceOggCommentScanner::new(codec));
                scanner.push(bytes, checkpoint)?;
                self.budget.add_entries(scanner.take_unbudgeted_comments())
            }
            _ => Ok(()),
        }
    }

    fn end_packet(&mut self) -> Result<(), String> {
        match self.packet_index {
            0 => {
                self.codec = Some(
                    if self.identity_bytes == 51
                        && self.identity_len == 51
                        && &self.identity[..5] == b"\x7fFLAC"
                        && self.identity[5] == 1
                        && &self.identity[9..13] == b"fLaC"
                        && self.identity[13] & 0x7f == 0
                        && &self.identity[14..17] == b"\0\0\x22"
                    {
                        ServiceOggCommentCodec::Flac
                    } else if self.identity_len >= 7 && &self.identity[..7] == b"\x01vorbis" {
                        ServiceOggCommentCodec::Vorbis
                    } else if self.identity_len >= 8 && &self.identity[..8] == b"OpusHead" {
                        ServiceOggCommentCodec::Opus
                    } else {
                        ServiceOggCommentCodec::Other
                    },
                );
            }
            _ if self.codec == Some(ServiceOggCommentCodec::Flac) => {
                self.flac_packet
                    .take()
                    .ok_or_else(|| "empty Ogg-FLAC packet".to_string())?
                    .finish()?;
            }
            1 => {
                if let Some(comment) = &self.comment {
                    comment.finish()?;
                } else if self.codec != Some(ServiceOggCommentCodec::Other) {
                    return Err("missing Ogg comment packet".into());
                }
            }
            _ => {}
        }
        self.packet_index = self
            .packet_index
            .checked_add(1)
            .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
        Ok(())
    }

    fn finish_stream(&self) -> Result<(), String> {
        if matches!(
            self.codec,
            Some(ServiceOggCommentCodec::Vorbis | ServiceOggCommentCodec::Opus)
        ) && self.packet_index < 2
        {
            return Err("Ogg stream is missing its comment packet".into());
        }
        if self.codec == Some(ServiceOggCommentCodec::Flac) && self.flac_packet.is_some() {
            return Err("Ogg-FLAC stream ends in an incomplete metadata packet".into());
        }
        Ok(())
    }
}

fn preflight_ogg_packets<C>(
    path: &Path,
    file_len: u64,
    packet_limit: u64,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String> + Send,
{
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut offset = 0_u64;
    let mut page_count = 0_usize;
    let mut current_serial = None;
    let mut current_ended = false;
    let mut pending_packet_bytes = 0_u64;
    let mut metadata = ServiceOggMetadataPreflight::default();
    while offset < file_len {
        if page_count == SERVICE_MAX_CONTAINER_ITEMS {
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        }
        checkpoint()?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} Ogg page: {error}", path.display()))?;
        let mut header = [0_u8; 27];
        file.read_exact(&mut header)
            .map_err(|error| format!("read {} Ogg page header: {error}", path.display()))?;
        if &header[..4] != b"OggS" || header[4] != 0 {
            return Err(format!(
                "{}: invalid Ogg page at byte {offset}",
                path.display()
            ));
        }
        let header_type = header[5];
        let continued = header_type & 0x01 != 0;
        let beginning = header_type & 0x02 != 0;
        let end_of_stream = header_type & 0x04 != 0;
        let serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        match current_serial {
            None => {
                if !beginning || continued {
                    return Err(format!(
                        "{}: first Ogg page is not a complete stream beginning",
                        path.display()
                    ));
                }
                current_serial = Some(serial);
            }
            Some(previous) if previous != serial => {
                if !current_ended || !beginning || continued || pending_packet_bytes != 0 {
                    return Err(format!(
                        "{}: multiplexed or overlapping Ogg logical streams are unsupported",
                        path.display()
                    ));
                }
                metadata.finish_stream()?;
                let budget = std::mem::take(&mut metadata.budget);
                metadata = ServiceOggMetadataPreflight::default();
                metadata.budget = budget;
                current_serial = Some(serial);
                current_ended = false;
            }
            Some(_) => {
                if current_ended || beginning || continued != (pending_packet_bytes != 0) {
                    return Err(format!(
                        "{}: invalid Ogg continuation flags at byte {offset}",
                        path.display()
                    ));
                }
            }
        }
        let segment_count = usize::from(header[26]);
        let mut lacing = [0_u8; 255];
        file.read_exact(&mut lacing[..segment_count])
            .map_err(|error| format!("read {} Ogg lacing table: {error}", path.display()))?;
        let body_bytes = lacing[..segment_count]
            .iter()
            .try_fold(0_u64, |total, &length| total.checked_add(u64::from(length)))
            .ok_or_else(|| "Ogg page byte count overflow".to_string())?;
        let header_bytes = 27_u64
            .checked_add(segment_count as u64)
            .ok_or_else(|| "Ogg page header size overflow".to_string())?;
        let mut body_offset = offset
            .checked_add(header_bytes)
            .ok_or_else(|| "Ogg page body offset overflow".to_string())?;
        let mut segment = [0_u8; 255];
        for &length in &lacing[..segment_count] {
            pending_packet_bytes = pending_packet_bytes
                .checked_add(u64::from(length))
                .ok_or_else(|| SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.to_string())?;
            if pending_packet_bytes > packet_limit {
                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
            }
            if metadata.wants_packet_bytes() && length != 0 {
                file.seek(SeekFrom::Start(body_offset))
                    .map_err(|error| format!("seek {} Ogg packet: {error}", path.display()))?;
                file.read_exact(&mut segment[..usize::from(length)])
                    .map_err(|error| format!("read {} Ogg packet: {error}", path.display()))?;
                metadata.push(&segment[..usize::from(length)], checkpoint)?;
            }
            body_offset = body_offset
                .checked_add(u64::from(length))
                .ok_or_else(|| "Ogg packet offset overflow".to_string())?;
            if length < 255 {
                metadata.end_packet()?;
                pending_packet_bytes = 0;
            }
        }
        let next = offset
            .checked_add(header_bytes)
            .and_then(|value| value.checked_add(body_bytes))
            .ok_or_else(|| "Ogg page size overflow".to_string())?;
        if next > file_len {
            return Err(format!("{}: truncated Ogg page body", path.display()));
        }
        if end_of_stream {
            if pending_packet_bytes != 0 {
                return Err(format!(
                    "{}: Ogg EOS page ends with an incomplete packet",
                    path.display()
                ));
            }
            metadata.finish_stream()?;
            current_ended = true;
        }
        offset = next;
        page_count += 1;
    }
    if page_count == 0 || pending_packet_bytes != 0 || !current_ended {
        return Err(format!("{}: incomplete Ogg logical stream", path.display()));
    }
    metadata.finish_stream()?;
    Ok(())
}

fn preflight_flac_metadata<C>(
    path: &Path,
    file_len: u64,
    budget: &mut ServiceMetadataBudget,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String> + Send,
{
    preflight_flac_metadata_at(path, file_len, 0, budget, checkpoint)
}

fn preflight_flac_metadata_at<C>(
    path: &Path,
    file_len: u64,
    start: u64,
    budget: &mut ServiceMetadataBudget,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata_start = start
        .checked_add(4)
        .filter(|&offset| offset <= file_len)
        .ok_or_else(|| format!("{}: truncated FLAC signature", path.display()))?;
    file.seek(SeekFrom::Start(metadata_start))
        .map_err(|error| format!("seek {} FLAC metadata: {error}", path.display()))?;
    let mut offset = metadata_start;
    for block_count in 0..SERVICE_MAX_CONTAINER_ITEMS {
        if block_count.is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS) {
            checkpoint()?;
        }
        let mut header = [0_u8; 4];
        file.read_exact(&mut header)
            .map_err(|error| format!("read {} FLAC metadata: {error}", path.display()))?;
        let last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7f;
        if block_type == 0x7f {
            return Err(format!(
                "{}: invalid reserved FLAC metadata type",
                path.display()
            ));
        }
        let length = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
        budget.add_entries(1)?;
        budget.add_encoded_bytes(
            4_u64
                .checked_add(length)
                .ok_or_else(|| "FLAC metadata size overflow".to_string())?,
        )?;
        // PADDING and unknown blocks are skipped by Symphonia and therefore do
        // not create a payload-sized allocation. Known retained blocks do.
        if matches!(block_type, 2..=6) {
            ServiceMetadataBudget::validate_item(length)?;
        }
        let payload_start = offset
            .checked_add(4)
            .ok_or_else(|| "FLAC metadata offset overflow".to_string())?;
        offset = payload_start
            .checked_add(length)
            .ok_or_else(|| "FLAC metadata offset overflow".to_string())?;
        if offset > file_len {
            return Err(format!("{}: truncated FLAC metadata", path.display()));
        }
        if matches!(block_type, 4 | 6) {
            file.seek(SeekFrom::Start(payload_start))
                .map_err(|error| format!("seek {} FLAC metadata: {error}", path.display()))?;
            let mut remaining = length;
            let mut block = [0_u8; SERVICE_CONTROLLED_READ_BYTES];
            let mut scanner = if block_type == 4 {
                ServiceOggFlacPayloadScanner::Comment(ServiceOggCommentScanner::new(
                    ServiceOggCommentCodec::Flac,
                ))
            } else {
                ServiceOggFlacPayloadScanner::Picture(ServiceFlacPictureScanner::default())
            };
            while remaining != 0 {
                checkpoint()?;
                let count = usize::try_from(remaining.min(block.len() as u64)).unwrap();
                file.read_exact(&mut block[..count])
                    .map_err(|error| format!("read {} FLAC metadata: {error}", path.display()))?;
                match &mut scanner {
                    ServiceOggFlacPayloadScanner::Comment(comment) => {
                        comment.push(&block[..count], checkpoint)?;
                        budget.add_entries(comment.take_unbudgeted_comments())?;
                    }
                    ServiceOggFlacPayloadScanner::Picture(picture) => {
                        picture.push(&block[..count])?;
                    }
                    ServiceOggFlacPayloadScanner::Skip => unreachable!(),
                }
                remaining -= count as u64;
            }
            match &scanner {
                ServiceOggFlacPayloadScanner::Comment(comment) => comment.finish()?,
                ServiceOggFlacPayloadScanner::Picture(picture) => picture.finish()?,
                ServiceOggFlacPayloadScanner::Skip => unreachable!(),
            }
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} FLAC metadata: {error}", path.display()))?;
        if last {
            return Ok(());
        }
    }
    Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into())
}

fn preflight_isobmff_top_level<C>(
    path: &Path,
    file_len: u64,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut state = ServiceIsoBmffPreflight::default();
    let mut offset = 0_u64;
    while offset < file_len {
        service_isobmff_checkpoint(&mut state, checkpoint)?;
        let header = read_service_isobmff_box(path, &mut file, offset, file_len)?;
        let payload = header.end - header.body_start;
        if !matches!(&header.kind, b"mdat" | b"free" | b"skip" | b"wide")
            && payload > SERVICE_MAX_ENCODED_PACKET_BYTES
        {
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        }
        match &header.kind {
            b"moov" | b"moof" => preflight_isobmff_region(
                path,
                &mut file,
                header.body_start,
                header.end,
                1,
                &mut state,
                checkpoint,
            )?,
            b"udta" => preflight_isobmff_metadata_region(
                path,
                &mut file,
                header.body_start,
                header.end,
                1,
                ServiceIsoBmffMetadataRegion::Container,
                &mut state,
                checkpoint,
            )?,
            b"meta" => preflight_isobmff_metadata_region(
                path,
                &mut file,
                header.body_start,
                header.end,
                1,
                ServiceIsoBmffMetadataRegion::FullBox,
                &mut state,
                checkpoint,
            )?,
            _ => {}
        }
        offset = header.end;
        if header.extends_to_end {
            break;
        }
    }
    if offset == file_len {
        Ok(())
    } else {
        Err(format!(
            "{}: ISO-BMFF top-level boxes do not cover the file",
            path.display()
        ))
    }
}

#[derive(Clone, Copy)]
struct ServiceIsoBmffBox {
    kind: [u8; 4],
    body_start: u64,
    end: u64,
    extends_to_end: bool,
}

#[derive(Default)]
struct ServiceIsoBmffPreflight {
    boxes: usize,
    metadata: ServiceMetadataBudget,
    track_default_sample_sizes: Vec<(u32, u32)>,
}

#[derive(Default)]
struct ServiceIsoBmffFragmentDefaults {
    track_id: Option<u32>,
    sample_size: Option<u32>,
}

fn service_isobmff_checkpoint<C>(
    state: &mut ServiceIsoBmffPreflight,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    if state.boxes == SERVICE_MAX_CONTAINER_ITEMS {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    if state
        .boxes
        .is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS)
    {
        checkpoint()?;
    }
    state.boxes += 1;
    Ok(())
}

fn read_service_isobmff_box(
    path: &Path,
    file: &mut File,
    offset: u64,
    region_end: u64,
) -> Result<ServiceIsoBmffBox, String> {
    if offset > region_end || region_end - offset < 8 {
        return Err(format!(
            "{}: truncated ISO-BMFF box header at byte {offset}",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {} ISO-BMFF box: {error}", path.display()))?;
    let mut base = [0_u8; 8];
    file.read_exact(&mut base)
        .map_err(|error| format!("read {} ISO-BMFF box: {error}", path.display()))?;
    let size32 = u32::from_be_bytes(base[..4].try_into().unwrap());
    let kind = base[4..8].try_into().unwrap();
    let (size, header_bytes, extends_to_end) = match size32 {
        0 => (region_end - offset, 8_u64, true),
        1 => {
            if region_end - offset < 16 {
                return Err(format!(
                    "{}: truncated extended ISO-BMFF box header at byte {offset}",
                    path.display()
                ));
            }
            let mut extended = [0_u8; 8];
            file.read_exact(&mut extended).map_err(|error| {
                format!("read {} extended ISO-BMFF box: {error}", path.display())
            })?;
            (u64::from_be_bytes(extended), 16, false)
        }
        size => (u64::from(size), 8, false),
    };
    if size < header_bytes {
        return Err(format!(
            "{}: ISO-BMFF box at byte {offset} is smaller than its header",
            path.display()
        ));
    }
    let end = offset
        .checked_add(size)
        .ok_or_else(|| "ISO-BMFF box size overflow".to_string())?;
    if end > region_end || end <= offset {
        return Err(format!(
            "{}: ISO-BMFF box at byte {offset} exceeds its parent",
            path.display()
        ));
    }
    Ok(ServiceIsoBmffBox {
        kind,
        body_start: offset + header_bytes,
        end,
        extends_to_end,
    })
}

fn preflight_isobmff_region<C>(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    depth: usize,
    state: &mut ServiceIsoBmffPreflight,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let mut offset = start;
    while offset < end {
        service_isobmff_checkpoint(state, checkpoint)?;
        let child = read_service_isobmff_box(path, file, offset, end)?;
        let payload = child.end - child.body_start;
        match &child.kind {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"mvex" | b"moof" => {
                preflight_isobmff_region(
                    path,
                    file,
                    child.body_start,
                    child.end,
                    depth + 1,
                    state,
                    checkpoint,
                )?;
            }
            b"udta" => preflight_isobmff_metadata_region(
                path,
                file,
                child.body_start,
                child.end,
                depth + 1,
                ServiceIsoBmffMetadataRegion::Container,
                state,
                checkpoint,
            )?,
            b"meta" => preflight_isobmff_metadata_region(
                path,
                file,
                child.body_start,
                child.end,
                depth + 1,
                ServiceIsoBmffMetadataRegion::FullBox,
                state,
                checkpoint,
            )?,
            b"ilst" => preflight_isobmff_metadata_region(
                path,
                file,
                child.body_start,
                child.end,
                depth + 1,
                ServiceIsoBmffMetadataRegion::ItemList,
                state,
                checkpoint,
            )?,
            b"traf" => preflight_isobmff_traf(
                path,
                file,
                child.body_start,
                child.end,
                depth + 1,
                state,
                checkpoint,
            )?,
            b"stsz" => preflight_isobmff_stsz(path, file, child, checkpoint)?,
            b"stz2" => preflight_isobmff_stz2(path, file, child, checkpoint)?,
            b"trex" => preflight_isobmff_trex(path, file, child, state)?,
            b"free" | b"skip" => {}
            _ if payload > SERVICE_MAX_ENCODED_PACKET_BYTES => {
                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
            }
            _ => {}
        }
        offset = child.end;
        if child.extends_to_end {
            break;
        }
    }
    if offset == end {
        Ok(())
    } else {
        Err(format!(
            "{}: nested ISO-BMFF boxes do not cover their parent",
            path.display()
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceIsoBmffMetadataRegion {
    Container,
    FullBox,
    ItemList,
    Item,
}

/// Walk the `udta/meta/ilst` hierarchy without reading metadata payloads.
/// `ilst` entry fourcc values are user-defined metadata keys, so every direct
/// child is treated as a container while only `data`/`mean`/`name` leaves are
/// subject to the strict per-item metadata allocation bound.
#[allow(clippy::too_many_arguments)]
fn preflight_isobmff_metadata_region<C>(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    depth: usize,
    region: ServiceIsoBmffMetadataRegion,
    state: &mut ServiceIsoBmffPreflight,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let mut offset = if region == ServiceIsoBmffMetadataRegion::FullBox {
        start
            .checked_add(4)
            .filter(|offset| *offset <= end)
            .ok_or_else(|| format!("{}: truncated ISO-BMFF meta FullBox", path.display()))?
    } else {
        start
    };
    while offset < end {
        service_isobmff_checkpoint(state, checkpoint)?;
        let child = read_service_isobmff_box(path, file, offset, end)?;
        let payload = child.end - child.body_start;
        state.metadata.add_entries(1)?;
        let strict_leaf = matches!(&child.kind, b"data" | b"mean" | b"name");
        let is_container = !strict_leaf
            && (region == ServiceIsoBmffMetadataRegion::ItemList
                || matches!(&child.kind, b"udta" | b"meta" | b"ilst"));
        if !is_container {
            state.metadata.add_encoded_bytes(
                child
                    .end
                    .checked_sub(offset)
                    .ok_or_else(|| "ISO-BMFF metadata size underflow".to_string())?,
            )?;
        }
        match region {
            _ if strict_leaf => {
                ServiceMetadataBudget::validate_item(payload)?;
            }
            ServiceIsoBmffMetadataRegion::ItemList => preflight_isobmff_metadata_region(
                path,
                file,
                child.body_start,
                child.end,
                depth + 1,
                ServiceIsoBmffMetadataRegion::Item,
                state,
                checkpoint,
            )?,
            _ => match &child.kind {
                b"udta" => preflight_isobmff_metadata_region(
                    path,
                    file,
                    child.body_start,
                    child.end,
                    depth + 1,
                    ServiceIsoBmffMetadataRegion::Container,
                    state,
                    checkpoint,
                )?,
                b"meta" => preflight_isobmff_metadata_region(
                    path,
                    file,
                    child.body_start,
                    child.end,
                    depth + 1,
                    ServiceIsoBmffMetadataRegion::FullBox,
                    state,
                    checkpoint,
                )?,
                b"ilst" => preflight_isobmff_metadata_region(
                    path,
                    file,
                    child.body_start,
                    child.end,
                    depth + 1,
                    ServiceIsoBmffMetadataRegion::ItemList,
                    state,
                    checkpoint,
                )?,
                _ if payload > SERVICE_MAX_ENCODED_PACKET_BYTES => {
                    return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
                }
                _ => {}
            },
        }
        offset = child.end;
        if child.extends_to_end {
            break;
        }
    }
    if offset == end {
        Ok(())
    } else {
        Err(format!(
            "{}: ISO-BMFF metadata boxes do not cover their parent",
            path.display()
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn preflight_isobmff_traf<C>(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    depth: usize,
    state: &mut ServiceIsoBmffPreflight,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    if depth > 16 {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let mut defaults = ServiceIsoBmffFragmentDefaults::default();
    let mut offset = start;
    while offset < end {
        service_isobmff_checkpoint(state, checkpoint)?;
        let child = read_service_isobmff_box(path, file, offset, end)?;
        let payload = child.end - child.body_start;
        match &child.kind {
            b"tfhd" => preflight_isobmff_tfhd(path, file, child, &mut defaults)?,
            b"trun" => preflight_isobmff_trun(path, file, child, &defaults, state, checkpoint)?,
            b"free" | b"skip" => {}
            _ if payload > SERVICE_MAX_ENCODED_PACKET_BYTES => {
                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
            }
            _ => {}
        }
        offset = child.end;
        if child.extends_to_end {
            break;
        }
    }
    if offset == end {
        Ok(())
    } else {
        Err(format!(
            "{}: TrackFragmentBox children do not cover their parent",
            path.display()
        ))
    }
}

fn read_service_box_prefix<const N: usize>(
    path: &Path,
    file: &mut File,
    header: ServiceIsoBmffBox,
) -> Result<[u8; N], String> {
    if header.end - header.body_start < N as u64 {
        return Err(format!(
            "{}: {} box is truncated",
            path.display(),
            String::from_utf8_lossy(&header.kind)
        ));
    }
    file.seek(SeekFrom::Start(header.body_start))
        .map_err(|error| format!("seek {} ISO-BMFF payload: {error}", path.display()))?;
    let mut bytes = [0_u8; N];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read {} ISO-BMFF payload: {error}", path.display()))?;
    Ok(bytes)
}

fn preflight_isobmff_stsz<C>(
    path: &Path,
    file: &mut File,
    header: ServiceIsoBmffBox,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let prefix = read_service_box_prefix::<12>(path, file, header)?;
    if prefix[0] != 0 || prefix[1..4] != [0, 0, 0] {
        return Err(format!("{}: unsupported stsz FullBox", path.display()));
    }
    let fixed_size = u32::from_be_bytes(prefix[4..8].try_into().unwrap());
    let sample_count = u32::from_be_bytes(prefix[8..12].try_into().unwrap());
    if sample_count > SERVICE_MAX_ISOBMFF_SAMPLE_ENTRIES {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    if u64::from(fixed_size) > SERVICE_MAX_ENCODED_PACKET_BYTES {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let entries = if fixed_size == 0 {
        u64::from(sample_count)
            .checked_mul(4)
            .ok_or_else(|| "stsz table length overflow".to_string())?
    } else {
        0
    };
    let expected_end = header
        .body_start
        .checked_add(12)
        .and_then(|offset| offset.checked_add(entries))
        .ok_or_else(|| "stsz table offset overflow".to_string())?;
    if expected_end != header.end {
        return Err(format!("{}: malformed stsz sample table", path.display()));
    }
    if fixed_size == 0 {
        preflight_isobmff_sample_sizes(
            path,
            file,
            header.body_start + 12,
            sample_count,
            4,
            0,
            checkpoint,
        )?;
    }
    Ok(())
}

fn preflight_isobmff_stz2<C>(
    path: &Path,
    file: &mut File,
    header: ServiceIsoBmffBox,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let prefix = read_service_box_prefix::<12>(path, file, header)?;
    if prefix[0] != 0 || prefix[1..4] != [0, 0, 0] {
        return Err(format!("{}: unsupported stz2 FullBox", path.display()));
    }
    let field_size = prefix[7];
    if !matches!(field_size, 4 | 8 | 16) {
        return Err(format!("{}: invalid stz2 field size", path.display()));
    }
    let sample_count = u32::from_be_bytes(prefix[8..12].try_into().unwrap());
    if sample_count > SERVICE_MAX_ISOBMFF_SAMPLE_ENTRIES {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let table_bits = u64::from(sample_count)
        .checked_mul(u64::from(field_size))
        .ok_or_else(|| "stz2 table length overflow".to_string())?;
    let table_bytes = table_bits
        .checked_add(7)
        .ok_or_else(|| "stz2 table length overflow".to_string())?
        / 8;
    let expected_end = header
        .body_start
        .checked_add(12)
        .and_then(|offset| offset.checked_add(table_bytes))
        .ok_or_else(|| "stz2 table offset overflow".to_string())?;
    if expected_end != header.end {
        return Err(format!("{}: malformed stz2 sample table", path.display()));
    }

    // Compact entries are at most 16 bits, but scan their complete table in
    // bounded chunks so cancellation/deadline checks do not depend on how a
    // third-party parser chooses to consume the box.
    file.seek(SeekFrom::Start(header.body_start + 12))
        .map_err(|error| format!("seek {} stz2 table: {error}", path.display()))?;
    let mut remaining = table_bytes;
    // At most 64 compact four-bit entries are crossed between checks.
    let mut buffer = [0_u8; SERVICE_CONTAINER_CHECKPOINT_ITEMS / 2];
    while remaining != 0 {
        checkpoint()?;
        let take = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded ISO-BMFF table read fits usize");
        file.read_exact(&mut buffer[..take])
            .map_err(|error| format!("read {} stz2 table: {error}", path.display()))?;
        remaining -= take as u64;
    }
    Ok(())
}

fn preflight_isobmff_trex(
    path: &Path,
    file: &mut File,
    header: ServiceIsoBmffBox,
    state: &mut ServiceIsoBmffPreflight,
) -> Result<(), String> {
    if header.end - header.body_start != 24 {
        return Err(format!("{}: malformed trex box", path.display()));
    }
    let prefix = read_service_box_prefix::<24>(path, file, header)?;
    if prefix[0] != 0 || prefix[1..4] != [0, 0, 0] {
        return Err(format!("{}: unsupported trex FullBox", path.display()));
    }
    let track_id = u32::from_be_bytes(prefix[4..8].try_into().unwrap());
    let sample_size = u32::from_be_bytes(prefix[16..20].try_into().unwrap());
    if track_id == 0 {
        return Err(format!("{}: trex track ID is zero", path.display()));
    }
    if u64::from(sample_size) > SERVICE_MAX_ENCODED_PACKET_BYTES {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    if let Some((_, existing)) = state
        .track_default_sample_sizes
        .iter_mut()
        .find(|(id, _)| *id == track_id)
    {
        *existing = sample_size;
    } else {
        state
            .track_default_sample_sizes
            .push((track_id, sample_size));
    }
    Ok(())
}

fn preflight_isobmff_tfhd(
    path: &Path,
    file: &mut File,
    header: ServiceIsoBmffBox,
    defaults: &mut ServiceIsoBmffFragmentDefaults,
) -> Result<(), String> {
    let prefix = read_service_box_prefix::<8>(path, file, header)?;
    let version = prefix[0];
    let flags = u32::from_be_bytes([0, prefix[1], prefix[2], prefix[3]]);
    const ALLOWED_FLAGS: u32 = 0x03_003b;
    if version != 0 || flags & !ALLOWED_FLAGS != 0 {
        return Err(format!("{}: unsupported tfhd FullBox", path.display()));
    }
    let track_id = u32::from_be_bytes(prefix[4..8].try_into().unwrap());
    if track_id == 0 {
        return Err(format!("{}: tfhd track ID is zero", path.display()));
    }
    let mut cursor = header.body_start + 8;
    let mut remaining = header.end - cursor;
    let take_u32 = |file: &mut File, cursor: &mut u64, remaining: &mut u64| {
        if *remaining < 4 {
            return Err(format!("{}: truncated tfhd field", path.display()));
        }
        file.seek(SeekFrom::Start(*cursor))
            .map_err(|error| format!("seek {} tfhd field: {error}", path.display()))?;
        let mut bytes = [0_u8; 4];
        file.read_exact(&mut bytes)
            .map_err(|error| format!("read {} tfhd field: {error}", path.display()))?;
        *cursor += 4;
        *remaining -= 4;
        Ok(u32::from_be_bytes(bytes))
    };
    if flags & 0x000001 != 0 {
        if remaining < 8 {
            return Err(format!("{}: truncated tfhd base offset", path.display()));
        }
        cursor += 8;
        remaining -= 8;
    }
    if flags & 0x000002 != 0 {
        let _ = take_u32(file, &mut cursor, &mut remaining)?;
    }
    if flags & 0x000008 != 0 {
        let _ = take_u32(file, &mut cursor, &mut remaining)?;
    }
    let sample_size = if flags & 0x000010 != 0 {
        Some(take_u32(file, &mut cursor, &mut remaining)?)
    } else {
        None
    };
    if flags & 0x000020 != 0 {
        let _ = take_u32(file, &mut cursor, &mut remaining)?;
    }
    if remaining != 0 {
        return Err(format!("{}: malformed tfhd box", path.display()));
    }
    if sample_size.is_some_and(|size| u64::from(size) > SERVICE_MAX_ENCODED_PACKET_BYTES) {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    defaults.track_id = Some(track_id);
    defaults.sample_size = sample_size;
    Ok(())
}

fn preflight_isobmff_trun<C>(
    path: &Path,
    file: &mut File,
    header: ServiceIsoBmffBox,
    defaults: &ServiceIsoBmffFragmentDefaults,
    state: &ServiceIsoBmffPreflight,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let prefix = read_service_box_prefix::<8>(path, file, header)?;
    let version = prefix[0];
    let flags = u32::from_be_bytes([0, prefix[1], prefix[2], prefix[3]]);
    const ALLOWED_FLAGS: u32 = 0x000f05;
    if !matches!(version, 0 | 1) || flags & !ALLOWED_FLAGS != 0 {
        return Err(format!("{}: unsupported trun FullBox", path.display()));
    }
    let sample_count = u32::from_be_bytes(prefix[4..8].try_into().unwrap());
    if sample_count > SERVICE_MAX_ISOBMFF_SAMPLE_ENTRIES {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let header_fields = usize::from(flags & 0x000001 != 0) + usize::from(flags & 0x000004 != 0);
    let entry_fields = usize::from(flags & 0x000100 != 0)
        + usize::from(flags & 0x000200 != 0)
        + usize::from(flags & 0x000400 != 0)
        + usize::from(flags & 0x000800 != 0);
    let header_bytes = u64::try_from(header_fields)
        .ok()
        .and_then(|fields| fields.checked_mul(4))
        .and_then(|bytes| bytes.checked_add(8))
        .ok_or_else(|| "trun header length overflow".to_string())?;
    let entry_width = entry_fields
        .checked_mul(4)
        .ok_or_else(|| "trun entry width overflow".to_string())?;
    let table_bytes = u64::from(sample_count)
        .checked_mul(entry_width as u64)
        .ok_or_else(|| "trun table length overflow".to_string())?;
    let expected_end = header
        .body_start
        .checked_add(header_bytes)
        .and_then(|offset| offset.checked_add(table_bytes))
        .ok_or_else(|| "trun table offset overflow".to_string())?;
    if expected_end != header.end {
        return Err(format!("{}: malformed trun sample table", path.display()));
    }

    if flags & 0x000200 != 0 {
        let size_offset = usize::from(flags & 0x000100 != 0) * 4;
        preflight_isobmff_sample_sizes(
            path,
            file,
            header.body_start + header_bytes,
            sample_count,
            entry_width,
            size_offset,
            checkpoint,
        )?;
    } else {
        let default_size = defaults.sample_size.or_else(|| {
            let track_id = defaults.track_id?;
            state
                .track_default_sample_sizes
                .iter()
                .find_map(|(id, size)| (*id == track_id).then_some(*size))
        });
        let Some(default_size) = default_size else {
            // Without an explicit or inherited size, a parser may need to
            // discover an arbitrarily large sample from the mdat payload.
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        };
        if u64::from(default_size) > SERVICE_MAX_ENCODED_PACKET_BYTES {
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        }
        if sample_count != 0 && default_size == 0 {
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        }
        checkpoint()?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn preflight_isobmff_sample_sizes<C>(
    path: &Path,
    file: &mut File,
    start: u64,
    sample_count: u32,
    entry_width: usize,
    size_offset: usize,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    if entry_width < size_offset + 4 || entry_width > SERVICE_CONTROLLED_READ_BYTES {
        return Err(format!(
            "{}: invalid ISO-BMFF sample-size table",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek {} sample-size table: {error}", path.display()))?;
    let records_per_chunk =
        (SERVICE_CONTROLLED_READ_BYTES / entry_width).min(SERVICE_CONTAINER_CHECKPOINT_ITEMS);
    let mut buffer = [0_u8; SERVICE_CONTROLLED_READ_BYTES];
    let mut remaining = sample_count as usize;
    while remaining != 0 {
        checkpoint()?;
        let records = remaining.min(records_per_chunk);
        let bytes = records
            .checked_mul(entry_width)
            .ok_or_else(|| "ISO-BMFF sample-size chunk overflow".to_string())?;
        file.read_exact(&mut buffer[..bytes])
            .map_err(|error| format!("read {} sample-size table: {error}", path.display()))?;
        for entry in buffer[..bytes].chunks_exact(entry_width) {
            let sample_size =
                u32::from_be_bytes(entry[size_offset..size_offset + 4].try_into().unwrap());
            if u64::from(sample_size) > SERVICE_MAX_ENCODED_PACKET_BYTES {
                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
            }
        }
        remaining -= records;
    }
    Ok(())
}

fn preflight_matroska<C>(
    path: &Path,
    file_len: u64,
    packet_limit: u64,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut elements = 0_usize;
    let mut metadata = ServiceMetadataBudget::default();
    preflight_ebml_region(
        path,
        &mut file,
        0,
        file_len,
        0,
        packet_limit,
        &mut elements,
        &mut metadata,
        checkpoint,
    )
}

#[allow(clippy::too_many_arguments)]
fn preflight_ebml_region<C>(
    path: &Path,
    file: &mut File,
    start: u64,
    end: u64,
    depth: usize,
    packet_limit: u64,
    elements: &mut usize,
    metadata: &mut ServiceMetadataBudget,
    checkpoint: &mut C,
) -> Result<(), String>
where
    C: FnMut() -> Result<(), String>,
{
    const EBML_MAX_DEPTH: usize = 16;
    if depth > EBML_MAX_DEPTH {
        return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
    }
    let mut offset = start;
    while offset < end {
        if *elements == SERVICE_MAX_CONTAINER_ITEMS {
            return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
        }
        if (*elements).is_multiple_of(SERVICE_CONTAINER_CHECKPOINT_ITEMS) {
            checkpoint()?;
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("seek {} EBML element: {error}", path.display()))?;
        let (id, id_bytes, _) = read_service_ebml_vint(file, true)?;
        let (size, size_bytes, unknown) = read_service_ebml_vint(file, false)?;
        let data_start = offset
            .checked_add(id_bytes as u64)
            .and_then(|value| value.checked_add(size_bytes as u64))
            .ok_or_else(|| "EBML element header overflow".to_string())?;
        let element_end = if unknown {
            if id != 0x1853_8067 {
                return Err(format!(
                    "{}: unknown-sized non-Segment EBML element",
                    path.display()
                ));
            }
            end
        } else {
            data_start
                .checked_add(size)
                .ok_or_else(|| "EBML element size overflow".to_string())?
        };
        if element_end > end || element_end <= offset {
            return Err(format!(
                "{}: invalid EBML element bounds at byte {offset}",
                path.display()
            ));
        }
        *elements += 1;
        if service_ebml_master(id) {
            preflight_ebml_region(
                path,
                file,
                data_start,
                element_end,
                depth + 1,
                packet_limit,
                elements,
                metadata,
                checkpoint,
            )?;
        } else if id != 0xec {
            let payload = element_end - data_start;
            if service_ebml_packet_leaf(id) && payload > packet_limit {
                return Err(SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED.into());
            }
            if service_ebml_retained_metadata_leaf(id) {
                ServiceMetadataBudget::validate_item(payload)?;
                metadata.add_entries(1)?;
                metadata.add_encoded_bytes(payload)?;
            }
        }
        offset = element_end;
    }
    Ok(())
}

fn service_ebml_packet_leaf(id: u64) -> bool {
    matches!(id, 0xa1 | 0xa2 | 0xa3 | 0xa4 | 0xa5 | 0xaf)
}

/// Binary and string leaves the pinned Symphonia Matroska reader materializes
/// or retains. Numeric geometry, master elements, Void/CRC, and encoded block
/// payloads are intentionally absent: charging those as metadata either
/// double-counts audio or rejects skip-only structure without reducing heap.
fn service_ebml_retained_metadata_leaf(id: u64) -> bool {
    matches!(
        id,
        0x4282
            | 0x4283
            | 0x465c
            | 0x467e
            | 0x4660
            | 0x466e
            | 0x4675
            | 0x6933
            | 0x450d
            | 0x437e
            | 0x437c
            | 0x437d
            | 0x85
            | 0x6e67
            | 0x5654
            | 0x45e4
            | 0x4521
            | 0x69a5
            | 0x4d80
            | 0x3e83bb
            | 0x3eb923
            | 0x3c83ab
            | 0x3cb923
            | 0x4444
            | 0x7384
            | 0x73a4
            | 0x7ba9
            | 0x5741
            | 0x53ab
            | 0x4485
            | 0x447a
            | 0x447b
            | 0x45a3
            | 0x4487
            | 0x63ca
            | 0x7d7b
            | 0x41ed
            | 0x41a4
            | 0x26b240
            | 0x86
            | 0x3b4040
            | 0x258688
            | 0x63a2
            | 0x3a9697
            | 0x4255
            | 0x47e2
            | 0x47e4
            | 0x47e3
            | 0x22b59c
            | 0x22b59d
            | 0x536e
            | 0x66a5
            | 0xc4
            | 0xc1
            | 0x7672
            | 0x2eb524
    )
}

fn read_service_ebml_vint(file: &mut File, id: bool) -> Result<(u64, usize, bool), String> {
    let mut first = [0_u8; 1];
    file.read_exact(&mut first)
        .map_err(|error| format!("read EBML variable integer: {error}"))?;
    if first[0] == 0 {
        return Err("EBML variable integer begins with zero".into());
    }
    let length = first[0].leading_zeros() as usize + 1;
    let maximum = if id { 4 } else { 8 };
    if length > maximum {
        return Err(format!("EBML variable integer exceeds {maximum} bytes"));
    }
    let mut value = if id {
        u64::from(first[0])
    } else {
        u64::from(first[0] & (0xff >> length))
    };
    for _ in 1..length {
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)
            .map_err(|error| format!("read EBML variable integer: {error}"))?;
        value = (value << 8) | u64::from(byte[0]);
    }
    let unknown = !id && value == (1_u64 << (7 * length)) - 1;
    Ok((value, length, unknown))
}

fn service_ebml_master(id: u64) -> bool {
    matches!(
        id,
        0x1a45_dfa3
            | 0x4281
            | 0x1853_8067
            | 0x114d_9b74
            | 0x4dbb
            | 0x1549_a966
            | 0x6924
            | 0x1f43_b675
            | 0x1654_ae6b
            | 0xae
            | 0xe1
            | 0xe0
            | 0x55b0
            | 0x55d0
            | 0x7670
            | 0x41e4
            | 0x6624
            | 0xe2
            | 0xe3
            | 0xe4
            | 0xe9
            | 0x6d80
            | 0x6240
            | 0x5034
            | 0x5035
            | 0x47e7
            | 0x1c53_bb6b
            | 0xbb
            | 0xb7
            | 0xdb
            | 0xa0
            | 0x75a1
            | 0xa6
            | 0xc8
            | 0x8e
            | 0xe8
            | 0x5854
            | 0x1941_a469
            | 0x61a7
            | 0x1043_a770
            | 0x45b9
            | 0xb6
            | 0x80
            | 0x8f
            | 0x4520
            | 0x6944
            | 0x6911
            | 0x1254_c367
            | 0x7373
            | 0x63c0
            | 0x67c8
    )
}

fn sniff_decoder_route(path: &Path) -> Result<DecoderRoute, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut prefix = [0_u8; 16];
    let length = file
        .read(&mut prefix)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let prefix = &prefix[..length];
    if prefix.len() >= 12
        && matches!(&prefix[..4], b"RIFF" | b"RF64" | b"BW64")
        && &prefix[8..12] == b"WAVE"
    {
        return Ok(DecoderRoute::Wave);
    }
    if prefix.starts_with(b"DSD ") {
        return Ok(DecoderRoute::Dsf);
    }
    if prefix.len() >= 16 && &prefix[..4] == b"FRM8" && &prefix[12..16] == b"DSD " {
        return Ok(DecoderRoute::Dsdiff);
    }
    if prefix.starts_with(b"OggS") {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut packets = ogg::PacketReader::new(BufReader::new(file));
        if packets
            .read_packet()
            .ok()
            .flatten()
            .is_some_and(|packet| packet.data.starts_with(b"OpusHead"))
        {
            return Ok(DecoderRoute::Opus);
        }
    }
    Ok(DecoderRoute::Symphonia)
}

fn service_preflight_accepts_decoder_route(
    service_route: ServiceContainerPreflightRoute,
    decoder_route: DecoderRoute,
) -> bool {
    match service_route {
        ServiceContainerPreflightRoute::NativeBounded => matches!(
            decoder_route,
            DecoderRoute::Wave | DecoderRoute::Dsf | DecoderRoute::Dsdiff
        ),
        ServiceContainerPreflightRoute::Ogg => {
            matches!(decoder_route, DecoderRoute::Opus | DecoderRoute::Symphonia)
        }
        ServiceContainerPreflightRoute::Matroska
        | ServiceContainerPreflightRoute::Flac
        | ServiceContainerPreflightRoute::IsoBmff
        | ServiceContainerPreflightRoute::Mpa
        | ServiceContainerPreflightRoute::Adts => decoder_route == DecoderRoute::Symphonia,
    }
}

fn service_preflight_accepts_container(
    route: ServiceContainerPreflightRoute,
    container: AudioContainer,
) -> bool {
    match route {
        ServiceContainerPreflightRoute::NativeBounded => {
            matches!(
                container,
                AudioContainer::Wave | AudioContainer::Dsf | AudioContainer::Dsdiff
            )
        }
        ServiceContainerPreflightRoute::Ogg => container == AudioContainer::Ogg,
        ServiceContainerPreflightRoute::Matroska => container == AudioContainer::Matroska,
        ServiceContainerPreflightRoute::Flac => container == AudioContainer::Flac,
        ServiceContainerPreflightRoute::IsoBmff => container == AudioContainer::IsoBmff,
        ServiceContainerPreflightRoute::Mpa => container == AudioContainer::MpegAudio,
        ServiceContainerPreflightRoute::Adts => container == AudioContainer::Adts,
    }
}

/// Build the service-only format registry after Forge has fully preflighted a
/// single container class. No metadata readers are registered: the controlled
/// source starts at the exact audio offset returned by the checked ID3/APEv2
/// scanner, and supplemental tags are not part of a normalization response.
fn service_symphonia_probe(
    route: ServiceContainerPreflightRoute,
) -> Result<symphonia::core::formats::probe::Probe, String> {
    use symphonia::core::formats::probe::Probe;
    use symphonia::default::formats::{
        AdtsReader, FlacReader, IsoMp4Reader, MkvReader, MpaReader, OggReader,
    };

    let mut probe = Probe::new();
    match route {
        ServiceContainerPreflightRoute::Ogg => probe.register_format::<OggReader<'_>>(),
        ServiceContainerPreflightRoute::Matroska => probe.register_format::<MkvReader<'_>>(),
        ServiceContainerPreflightRoute::Flac => probe.register_format::<FlacReader<'_>>(),
        ServiceContainerPreflightRoute::IsoBmff => probe.register_format::<IsoMp4Reader<'_>>(),
        ServiceContainerPreflightRoute::Mpa => probe.register_format::<MpaReader<'_>>(),
        ServiceContainerPreflightRoute::Adts => probe.register_format::<AdtsReader<'_>>(),
        ServiceContainerPreflightRoute::NativeBounded => {
            return Err("native service decoder route does not use Symphonia probing".into());
        }
    }
    Ok(probe)
}

fn probe_symphonia_identity_at(
    path: &Path,
    hint_path: Option<&Path>,
    display: &str,
    selection: AudioTrackSelection,
) -> Result<RegistryIdentity, String> {
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::default::get_probe;

    let file = File::open(path).map_err(|error| format!("{display}: {error}"))?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    probe_symphonia_identity_from_stream(
        path,
        hint_path,
        display,
        selection,
        stream,
        symphonia::core::meta::MetadataOptions::default(),
        get_probe(),
    )
}

fn probe_symphonia_identity_at_controlled<C>(
    path: &Path,
    hint_path: Option<&Path>,
    display: &str,
    selection: AudioTrackSelection,
    preflight: ServiceContainerPreflight,
    checkpoint: C,
) -> Result<RegistryIdentity, String>
where
    C: FnMut() -> Result<(), String> + Send,
{
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};

    let checkpoint = Mutex::new(checkpoint);
    run_locked_checkpoint(&checkpoint)?;
    let file = File::open(path).map_err(|error| format!("{display}: {error}"))?;
    let source = CheckpointMediaSource::new_range(
        file,
        &checkpoint,
        preflight.media_offset,
        preflight.media_end,
    )?;
    let stream = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
    let probe = service_symphonia_probe(preflight.route)?;
    let identity = probe_symphonia_identity_from_stream(
        path,
        hint_path,
        display,
        selection,
        stream,
        service_metadata_options(),
        &probe,
    )?;
    if !service_preflight_accepts_container(preflight.route, identity.container) {
        return Err("controlled Symphonia probe selected a non-preflighted container".into());
    }
    run_locked_checkpoint(&checkpoint)?;
    Ok(identity)
}

fn probe_symphonia_identity_from_stream(
    path: &Path,
    hint_path: Option<&Path>,
    display: &str,
    selection: AudioTrackSelection,
    stream: symphonia::core::io::MediaSourceStream<'_>,
    metadata_options: symphonia::core::meta::MetadataOptions,
    probe: &symphonia::core::formats::probe::Probe,
) -> Result<RegistryIdentity, String> {
    use symphonia::core::audio::AudioSpec;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;

    let mut hint = Hint::new();
    if let Some(extension) = hint_path
        .and_then(Path::extension)
        .and_then(|extension| extension.to_str())
    {
        hint.with_extension(extension);
    }
    let mut format = probe
        .probe(&hint, stream, FormatOptions::default(), metadata_options)
        .map_err(|error| format!("{display}: probe failed: {error}"))?;
    let container =
        audio_container_from_symphonia(format.format_info().format).ok_or_else(|| {
            format!(
                "{}: container {} is not registered for normalization",
                display,
                format.format_info().short_name
            )
        })?;
    let (track, track_index) =
        select_symphonia_audio_track_with_selection(path, format.as_ref(), selection)?;
    let codec = audio_codec_from_symphonia(track.codec_params.codec)
        .ok_or_else(|| format!("{}: selected audio codec is not registered", display))?;
    let stream = if container == AudioContainer::Flac && codec == AudioCodec::Flac {
        let sample_rate = require_symphonia_sample_rate(path, &track.codec_params)?;
        let channels = track
            .codec_params
            .channels
            .clone()
            .ok_or_else(|| format!("{display}: selected FLAC track has no channel layout"))?;
        let decoded = AudioSpec::new(sample_rate, channels);
        let mut metadata = FlacMetadataTracker::default();
        let channel_mask = metadata.scan(format.as_mut(), &track);
        let output = establish_symphonia_output_format(
            path,
            format.format_info().format,
            &track.codec_params,
            &decoded,
            PcmKind::F32,
            channel_mask,
        )?;
        Some((
            StreamInfo {
                sample_rate: output.sample_rate,
                channels: output.channels,
                channel_roles: output.channel_roles,
                source_kind: output.source_kind,
            },
            output.channel_layout,
            track.num_frames,
        ))
    } else {
        None
    };
    Ok(RegistryIdentity {
        container,
        codec,
        track_index,
        track_id: track.id,
        stream,
    })
}

fn audio_container_from_symphonia(
    format: symphonia::core::formats::FormatId,
) -> Option<AudioContainer> {
    use symphonia::core::formats::well_known::*;
    Some(match format {
        FORMAT_ID_FLAC => AudioContainer::Flac,
        FORMAT_ID_OGG => AudioContainer::Ogg,
        FORMAT_ID_ISOMP4 => AudioContainer::IsoBmff,
        FORMAT_ID_MKV => AudioContainer::Matroska,
        FORMAT_ID_MP1 | FORMAT_ID_MP2 | FORMAT_ID_MP3 => AudioContainer::MpegAudio,
        FORMAT_ID_ADTS => AudioContainer::Adts,
        FORMAT_ID_WAVE => AudioContainer::Wave,
        _ => return None,
    })
}

fn audio_codec_from_symphonia(
    codec: symphonia::core::codecs::audio::AudioCodecId,
) -> Option<AudioCodec> {
    use symphonia::core::codecs::audio::well_known::*;
    Some(match codec {
        CODEC_ID_FLAC => AudioCodec::Flac,
        CODEC_ID_MP1 => AudioCodec::Mp1,
        CODEC_ID_MP2 => AudioCodec::Mp2,
        CODEC_ID_MP3 => AudioCodec::Mp3,
        CODEC_ID_AAC => AudioCodec::Aac,
        CODEC_ID_ALAC => AudioCodec::Alac,
        CODEC_ID_VORBIS => AudioCodec::Vorbis,
        CODEC_ID_OPUS => AudioCodec::Opus,
        CODEC_ID_PCM_U8 | CODEC_ID_PCM_U8_PLANAR => AudioCodec::Pcm(PcmKind::U8),
        CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S16LE_PLANAR => AudioCodec::Pcm(PcmKind::S16),
        CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S24LE_PLANAR => AudioCodec::Pcm(PcmKind::S24),
        CODEC_ID_PCM_S32LE | CODEC_ID_PCM_S32LE_PLANAR => AudioCodec::Pcm(PcmKind::S32),
        CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F32LE_PLANAR => AudioCodec::Pcm(PcmKind::F32),
        CODEC_ID_PCM_F64LE | CODEC_ID_PCM_F64LE_PLANAR => AudioCodec::Pcm(PcmKind::F64),
        _ => return None,
    })
}

/// RFC 9639 section 8.6.2 channel-mask metadata observed for one FLAC stream.
///
/// Keep syntax/duplication validity separate from layout validity. A parsed
/// mask may still be unusable for the decoded channel count (zero, partial,
/// or outside the standardized 18-bit speaker set).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum FlacChannelMaskState {
    #[default]
    Absent,
    Valid(u32),
    Invalid,
}

impl FlacChannelMaskState {
    fn observe(&mut self, value: Option<u32>) {
        *self = match (*self, value) {
            (Self::Invalid, _) | (_, None) => Self::Invalid,
            (Self::Absent, Some(mask)) => Self::Valid(mask),
            (Self::Valid(previous), Some(mask)) if previous == mask => Self::Valid(previous),
            (Self::Valid(_), Some(_)) => Self::Invalid,
        };
    }
}

/// Symphonia retains the newest metadata revision and appends later revisions
/// (notably when an Ogg physical stream is chained). Track how much of the log
/// has already been consumed so each physical stream gets an independent mask
/// state while every revision is still inspected.
#[derive(Debug, Default)]
struct FlacMetadataTracker {
    retained_revision: bool,
    current: FlacChannelMaskState,
}

impl FlacMetadataTracker {
    fn scan(
        &mut self,
        format: &mut dyn symphonia::core::formats::FormatReader,
        selected_track: &SymphoniaAudioTrack,
    ) -> FlacChannelMaskState {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;

        let selected_is_flac = selected_track.codec_params.codec == CODEC_ID_FLAC;
        let flac_track_count = format
            .tracks()
            .iter()
            .filter(|track| {
                track
                    .codec_params
                    .as_ref()
                    .and_then(|params| params.audio())
                    .is_some_and(|params| params.codec == CODEC_ID_FLAC)
            })
            .count();
        let media_tags_are_attributable = selected_is_flac && flac_track_count == 1;
        let selected_track_id = u64::from(selected_track.id);
        self.scan_revisions(
            format.metadata(),
            selected_track_id,
            selected_is_flac,
            media_tags_are_attributable,
        )
    }

    fn scan_revisions(
        &mut self,
        mut metadata: symphonia::core::meta::Metadata<'_>,
        selected_track_id: u64,
        selected_is_flac: bool,
        media_tags_are_attributable: bool,
    ) -> FlacChannelMaskState {
        let mut state = FlacChannelMaskState::Absent;

        if self.retained_revision {
            // The one revision that could not be popped on the previous scan
            // is the retained cursor, not metadata for the new Ogg stream.
            if metadata.pop().is_none() {
                self.current = state;
                return self.current;
            }
        }

        while let Some(revision) = metadata.pop() {
            if selected_is_flac {
                observe_flac_channel_mask_revision(
                    &mut state,
                    &revision,
                    selected_track_id,
                    media_tags_are_attributable,
                );
            }
        }
        if selected_is_flac {
            if let Some(revision) = metadata.current() {
                observe_flac_channel_mask_revision(
                    &mut state,
                    revision,
                    selected_track_id,
                    media_tags_are_attributable,
                );
            }
        }
        self.retained_revision = metadata.current().is_some();
        self.current = state;
        self.current
    }

    fn current(&self) -> FlacChannelMaskState {
        self.current
    }
}

fn observe_flac_channel_mask_revision(
    state: &mut FlacChannelMaskState,
    revision: &symphonia::core::meta::MetadataRevision,
    selected_track_id: u64,
    media_tags_are_attributable: bool,
) {
    use symphonia::core::meta::well_known::METADATA_ID_FLAC;

    if revision.info.metadata != METADATA_ID_FLAC {
        return;
    }

    if media_tags_are_attributable {
        observe_flac_channel_mask_tags(state, &revision.media.tags);
    } else if revision
        .media
        .tags
        .iter()
        .any(|tag| is_flac_channel_mask_key(&tag.raw.key))
    {
        // Ogg-FLAC exposes comment revisions as media metadata without a
        // serial/track binding. Never apply one stream's mask to another.
        *state = FlacChannelMaskState::Invalid;
    }

    for per_track in &revision.per_track {
        if per_track.track_id == selected_track_id {
            observe_flac_channel_mask_tags(state, &per_track.metadata.tags);
        }
    }
}

fn observe_flac_channel_mask_tags(
    state: &mut FlacChannelMaskState,
    tags: &[symphonia::core::meta::Tag],
) {
    use symphonia::core::meta::RawValue;

    for tag in tags {
        if !is_flac_channel_mask_key(&tag.raw.key) {
            continue;
        }
        let value = match &tag.raw.value {
            RawValue::String(value) => parse_flac_channel_mask(value),
            _ => None,
        };
        state.observe(value);
    }
}

fn is_flac_channel_mask_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("WAVEFORMATEXTENSIBLE_CHANNEL_MASK")
}

fn parse_flac_channel_mask(value: &str) -> Option<u32> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    // RFC 9639 explicitly permits zero-padding. Remove it before checking the
    // u32 width so an arbitrarily padded but bounded value remains valid.
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Some(0);
    }
    if significant.len() > 8 {
        return None;
    }
    u32::from_str_radix(significant, 16).ok()
}

fn require_known_layout(path: &Path, provenance: ChannelLayoutProvenance) -> Result<(), String> {
    match provenance {
        ChannelLayoutProvenance::KnownSpeakers => Ok(()),
        ChannelLayoutProvenance::Unknown => Err(format!(
            "{}: ambiguous channel layout; use a with-layout decoder API and supply explicit speaker roles",
            path.display()
        )),
        ChannelLayoutProvenance::SceneBased => Err(format!(
            "{}: scene-based channel layout cannot be represented as speaker roles; use a with-layout decoder API",
            path.display()
        )),
    }
}

/// Decode any supported audio file into a planar-f32 [`AudioBuffer`].
///
/// Inputs without a complete physical-speaker layout are rejected. Use
/// [`decode_with_layout`] when a caller can resolve the returned provenance
/// explicitly.
pub fn decode(path: &Path) -> Result<AudioBuffer, String> {
    let (buffer, provenance) = decode_with_layout(path)?;
    require_known_layout(path, provenance)?;
    Ok(buffer)
}

/// Full-buffer decode that retains whether its channel-to-speaker mapping is
/// authoritative.
pub fn decode_with_layout(path: &Path) -> Result<(AudioBuffer, ChannelLayoutProvenance), String> {
    decode_limited_with_layout(path, u64::MAX)
}

/// Full-buffer decode with the exact, versioned channel-layout sidecar.
pub fn decode_with_channel_layout(
    path: &Path,
) -> Result<(AudioBuffer, ChannelLayoutDescriptor), String> {
    decode_limited_with_channel_layout(path, u64::MAX)
}

/// Decode supported audio while bounding frames multiplied by channels.
///
/// WAVE inputs are rejected from their headers before the fast path allocates
/// its planar buffer. Compressed inputs are checked after every decoded packet.
pub fn decode_limited(path: &Path, max_decoded_samples: u64) -> Result<AudioBuffer, String> {
    let (buffer, provenance) = decode_limited_with_layout(path, max_decoded_samples)?;
    require_known_layout(path, provenance)?;
    Ok(buffer)
}

/// Bounded full-buffer decode that retains channel-layout provenance without
/// expanding the stable public [`AudioBuffer`] structure.
pub fn decode_limited_with_layout(
    path: &Path,
    max_decoded_samples: u64,
) -> Result<(AudioBuffer, ChannelLayoutProvenance), String> {
    if max_decoded_samples == 0 {
        return Err("decoded sample limit must be greater than zero".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    // Fast path: Forge's own WAV demuxer (parallel, SIMD-friendly).
    if is_wave_extension(&ext) || has_wave_signature(path) {
        return WavReader::open_with_layout_and_limits(path, u16::MAX, max_decoded_samples)
            .map_err(|e| format!("{}: {e}", path.display()));
    }
    if matches!(ext.as_str(), "dsf" | "dff") {
        // The provenance-aware decoder must be able to inspect ambiguous DSD
        // layouts so callers can supply explicit roles.  The legacy `probe`
        // adapter deliberately fails closed for those layouts.
        let dsd = crate::dsd::probe_with_layout(path)?.0;
        enforce_decoded_sample_limit(
            path,
            dsd.output_frames,
            u64::from(dsd.channels),
            max_decoded_samples,
        )?;
        let mut data = vec![Vec::new(); dsd.channels as usize];
        let mut layout_provenance = None;
        let info = crate::dsd::decode_stream_with_layout_and_declared_frames(
            path,
            |stream_info, provenance, _, planar| {
                if layout_provenance
                    .replace(provenance)
                    .is_some_and(|previous| previous != provenance)
                {
                    return Err("DSD channel layout provenance changed".into());
                }
                if planar.len() != data.len() {
                    return Err("DSD decoded channel count changed".into());
                }
                for (destination, source) in data.iter_mut().zip(planar) {
                    destination.append(source);
                }
                if stream_info.channels != dsd.channels {
                    return Err("DSD stream metadata changed".into());
                }
                Ok(())
            },
        )?;
        let frames = data.first().map_or(0, Vec::len);
        if frames as u64 != dsd.output_frames {
            return Err(format!(
                "{}: decoded DSD frame count {frames} does not match {}",
                path.display(),
                dsd.output_frames
            ));
        }
        return Ok((
            AudioBuffer {
                sample_rate: info.sample_rate,
                channels: info.channels,
                frames,
                data,
                channel_roles: info.channel_roles,
                source_kind: info.source_kind,
            },
            layout_provenance.ok_or_else(|| {
                format!("{}: DSD decoder produced no channel layout", path.display())
            })?,
        ));
    }
    if ext == "opus" {
        #[cfg(feature = "opus-encoding")]
        {
            let mut data: Vec<Vec<f32>> = Vec::new();
            let info = crate::opus::decode_stream(path, |info, planar| {
                let existing_frames = data.first().map_or(0, Vec::len) as u64;
                let packet_frames = planar.first().map_or(0, |samples| samples.len()) as u64;
                enforce_decoded_sample_limit(
                    path,
                    existing_frames.saturating_add(packet_frames),
                    u64::from(info.channels),
                    max_decoded_samples,
                )?;
                if data.is_empty() {
                    data = vec![Vec::new(); info.channels as usize];
                }
                for (destination, source) in data.iter_mut().zip(planar) {
                    destination.extend_from_slice(source);
                }
                Ok(())
            })?;
            let frames = data.first().map_or(0, Vec::len);
            return Ok((
                AudioBuffer {
                    sample_rate: info.sample_rate,
                    channels: info.channels,
                    frames,
                    data,
                    channel_roles: info.channel_roles,
                    source_kind: info.source_kind,
                },
                ChannelLayoutProvenance::KnownSpeakers,
            ));
        }
        #[cfg(not(feature = "opus-encoding"))]
        {
            return Err(
                "Ogg Opus support is unavailable; rebuild with `--features opus-encoding`".into(),
            );
        }
    }

    // Everything else via symphonia.
    decode_symphonia(path, &ext, max_decoded_samples)
}

/// Bounded full-buffer decode with exact container layout evidence.
pub fn decode_limited_with_channel_layout(
    path: &Path,
    max_decoded_samples: u64,
) -> Result<(AudioBuffer, ChannelLayoutDescriptor), String> {
    if max_decoded_samples == 0 {
        return Err("decoded sample limit must be greater than zero".into());
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if is_wave_extension(&extension) || has_wave_signature(path) {
        return WavReader::open_with_channel_layout_and_limits(path, u16::MAX, max_decoded_samples)
            .map_err(|error| format!("{}: {error}", path.display()));
    }

    let route = sniff_decoder_route(path)?;
    if route == DecoderRoute::Symphonia {
        let decoded = decode_symphonia_exact(path, &extension, max_decoded_samples)?;
        let mut layout = decoded.channel_layout;
        if decoded.is_iso_bmff {
            if let Some(container_layout) = crate::isobmff_qc::probe_channel_layout(
                path,
                decoded.track_id,
                decoded.buffer.channels,
            )? {
                layout = container_layout;
            }
        }
        if layout.channel_count() != usize::from(decoded.buffer.channels) {
            return Err("decoded exact channel layout does not match the PCM stream".into());
        }
        layout.validate()?;
        let mut buffer = decoded.buffer;
        buffer.channel_roles = layout.channel_roles();
        return Ok((buffer, layout));
    }

    let (mut buffer, provenance) = decode_limited_with_layout(path, max_decoded_samples)?;
    let layout = ChannelLayoutDescriptor::decoded_from_roles(&buffer.channel_roles, provenance);
    if layout.channel_count() != usize::from(buffer.channels) {
        return Err("decoded exact channel layout does not match the PCM stream".into());
    };
    layout.validate()?;
    buffer.channel_roles = layout.channel_roles();
    Ok((buffer, layout))
}

fn decode_symphonia(
    path: &Path,
    ext: &str,
    max_decoded_samples: u64,
) -> Result<(AudioBuffer, ChannelLayoutProvenance), String> {
    let decoded = decode_symphonia_exact(path, ext, max_decoded_samples)?;
    let provenance = decoded.channel_layout.provenance();
    Ok((decoded.buffer, provenance))
}

fn decode_symphonia_exact(
    path: &Path,
    ext: &str,
    max_decoded_samples: u64,
) -> Result<SymphoniaDecoded, String> {
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if !ext.is_empty() {
        hint.with_extension(ext);
    }

    let mut format = get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("{}: probe failed: {e}", path.display()))?;
    let container_format = format.format_info().format;

    let mut track = select_symphonia_audio_track(path, format.as_ref())?;
    require_symphonia_sample_rate(path, &track.codec_params)?;
    let mut flac_metadata = FlacMetadataTracker::default();
    let mut flac_channel_mask = flac_metadata.scan(format.as_mut(), &track);
    let decoder_options = symphonia_decoder_options();
    let mut decoder = get_codecs()
        .make_audio_decoder(&track.codec_params, &decoder_options)
        .map_err(|e| format!("{}: unsupported codec: {e}", path.display()))?;

    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut output_format: Option<SymphoniaOutputFormat> = None;
    let mut packet_planar = Vec::new();
    let mut mpeg_channel_mode = MpegChannelModeTracker::default();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(Error::ResetRequired) => {
                // Chained Ogg replaces the complete track list and uses a new
                // serial as its track ID. Re-select before accepting packets
                // from the new physical stream.
                let next_track = select_symphonia_audio_track(path, format.as_ref())?;
                require_symphonia_sample_rate(path, &next_track.codec_params)?;
                let next_flac_channel_mask = flac_metadata.scan(format.as_mut(), &next_track);
                if let Some(output) = output_format.as_ref() {
                    validate_symphonia_track_compatibility(
                        path,
                        output,
                        &next_track.codec_params,
                        PcmKind::F32,
                        next_flac_channel_mask,
                    )?;
                }
                let next_decoder = get_codecs()
                    .make_audio_decoder(&next_track.codec_params, &decoder_options)
                    .map_err(|e| format!("{}: reinit decoder: {e}", path.display()))?;
                track = next_track;
                flac_channel_mask = next_flac_channel_mask;
                decoder = next_decoder;
                continue;
            }
            Err(e) => return Err(format!("{}: read packet: {e}", path.display())),
        };
        if packet.track_id != track.id {
            continue;
        }

        let decoded = require_decoded_packet(decoder.decode(&packet))
            .map_err(|error| format!("{}: decode: {error}", path.display()))?;

        let spec = decoded.spec();
        let ch = spec.channels().count();
        mpeg_channel_mode.observe_decoded_packet(
            path,
            track.codec_params.codec,
            &packet.data,
            ch,
        )?;
        if ch == 0 {
            continue;
        }
        if let Some(output) = output_format.as_ref() {
            validate_symphonia_decoded_compatibility(path, output, spec, PcmKind::F32)?;
        } else {
            let mut output = establish_symphonia_output_format_with_mpeg_mode(
                path,
                container_format,
                &track.codec_params,
                spec,
                PcmKind::F32,
                flac_channel_mask,
                mpeg_channel_mode.observed,
            )?;
            output.layout_provenance =
                mpeg_channel_mode.constrain_provenance(output.layout_provenance);
            output.channel_layout = output
                .channel_layout
                .with_provenance(output.layout_provenance);
            planar = (0..ch).map(|_| Vec::new()).collect();
            output_format = Some(output);
        }
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let total_frames = planar
            .first()
            .map_or(0, Vec::len)
            .checked_add(frames)
            .ok_or_else(|| format!("{}: decoded frame count overflow", path.display()))?;
        enforce_decoded_sample_limit(
            path,
            total_frames as u64,
            u64::from(output_format.as_ref().unwrap().channels),
            max_decoded_samples,
        )?;
        decoded.copy_to_vecs_planar::<f32>(&mut packet_planar);
        for (destination, source) in planar.iter_mut().zip(&packet_planar) {
            destination.extend_from_slice(source);
        }
    }

    let output_format = output_format
        .filter(|_| planar.first().is_some_and(|channel| !channel.is_empty()))
        .ok_or_else(|| format!("{}: no audio decoded", path.display()))?;
    if planar.len() != usize::from(output_format.channels) {
        return Err(format!("{}: no audio decoded", path.display()));
    }

    let frames = planar[0].len();
    let layout_provenance = output_format.layout_provenance;
    let channel_layout = output_format.channel_layout;
    debug_assert_eq!(channel_layout.provenance(), layout_provenance);
    Ok(SymphoniaDecoded {
        buffer: AudioBuffer {
            sample_rate: output_format.sample_rate,
            channels: output_format.channels,
            frames,
            data: planar,
            channel_roles: output_format.channel_roles,
            source_kind: output_format.source_kind,
        },
        channel_layout,
        track_id: track.id,
        is_iso_bmff: container_format == symphonia::core::formats::well_known::FORMAT_ID_ISOMP4,
    })
}

struct SymphoniaAudioTrack {
    id: u32,
    num_frames: Option<u64>,
    time_base: Option<symphonia::core::units::TimeBase>,
    codec_params: symphonia::core::codecs::audio::AudioCodecParameters,
}

struct SymphoniaOutputFormat {
    sample_rate: u32,
    channels: u16,
    decoded_layout: symphonia::core::audio::Channels,
    declared_layout: Option<symphonia::core::audio::Channels>,
    channel_roles: Vec<ChannelRole>,
    layout_provenance: ChannelLayoutProvenance,
    channel_layout: ChannelLayoutDescriptor,
    flac_channel_mask: FlacChannelMaskState,
    source_kind: PcmKind,
}

struct SymphoniaDecoded {
    buffer: AudioBuffer,
    channel_layout: ChannelLayoutDescriptor,
    track_id: u32,
    is_iso_bmff: bool,
}

fn select_symphonia_audio_track(
    path: &Path,
    format: &dyn symphonia::core::formats::FormatReader,
) -> Result<SymphoniaAudioTrack, String> {
    select_symphonia_audio_track_with_selection(path, format, AudioTrackSelection::Default)
        .map(|(track, _)| track)
}

fn select_symphonia_audio_track_with_selection(
    path: &Path,
    format: &dyn symphonia::core::formats::FormatReader,
    selection: AudioTrackSelection,
) -> Result<(SymphoniaAudioTrack, u32), String> {
    use symphonia::core::formats::TrackType;

    let audio_tracks = format
        .tracks()
        .iter()
        .filter(|track| track.track_type() == Some(TrackType::Audio))
        .collect::<Vec<_>>();
    let (track, index) = match selection {
        AudioTrackSelection::Default => {
            let selected = format
                .default_track(TrackType::Audio)
                .ok_or_else(|| format!("{}: no audio track", path.display()))?;
            let index = audio_tracks
                .iter()
                .position(|track| track.id == selected.id)
                .ok_or_else(|| {
                    format!(
                        "{}: default audio track is not in the track list",
                        path.display()
                    )
                })?;
            (selected, index)
        }
        AudioTrackSelection::Index(index) => {
            let index = usize::try_from(index).map_err(|_| {
                format!(
                    "{}: audio track index does not fit this platform",
                    path.display()
                )
            })?;
            let selected = audio_tracks.get(index).copied().ok_or_else(|| {
                format!(
                    "{}: audio track index {index} is unavailable; found {} audio track(s)",
                    path.display(),
                    audio_tracks.len()
                )
            })?;
            (selected, index)
        }
        AudioTrackSelection::Id(id) => {
            let index = audio_tracks
                .iter()
                .position(|track| track.id == id)
                .ok_or_else(|| format!("{}: audio track ID {id} is unavailable", path.display()))?;
            (audio_tracks[index], index)
        }
    };
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| format!("{}: audio codec parameters are missing", path.display()))?
        .clone();
    Ok((
        SymphoniaAudioTrack {
            id: track.id,
            num_frames: track.num_frames,
            time_base: track.time_base,
            codec_params,
        },
        u32::try_from(index)
            .map_err(|_| format!("{}: audio track index exceeds u32", path.display()))?,
    ))
}

fn symphonia_decoder_options() -> symphonia::core::codecs::audio::AudioDecoderOptions {
    // Normalization and measurement operate on the audible programme. Trim
    // codec encoder delay and end padding so frame counts remain sample-accurate.
    symphonia::core::codecs::audio::AudioDecoderOptions::default().gapless(true)
}

fn require_symphonia_sample_rate(
    path: &Path,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
) -> Result<u32, String> {
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| format!("{}: unknown sample rate", path.display()))?;
    validate_symphonia_sample_rate(path, "track", sample_rate)?;
    Ok(sample_rate)
}

fn validate_symphonia_sample_rate(
    path: &Path,
    source: &str,
    sample_rate: u32,
) -> Result<(), String> {
    if !(MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&sample_rate) {
        return Err(format!(
            "{}: {source} sample rate {sample_rate} is outside the supported {MIN_DECODE_SAMPLE_RATE_HZ}..={MAX_DECODE_SAMPLE_RATE_HZ} Hz range",
            path.display()
        ));
    }
    Ok(())
}

fn establish_symphonia_output_format(
    path: &Path,
    container_format: symphonia::core::formats::FormatId,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    decoded: &symphonia::core::audio::AudioSpec,
    source_kind: PcmKind,
    flac_channel_mask: FlacChannelMaskState,
) -> Result<SymphoniaOutputFormat, String> {
    establish_symphonia_output_format_with_mpeg_mode(
        path,
        container_format,
        codec_params,
        decoded,
        source_kind,
        flac_channel_mask,
        None,
    )
}

fn establish_symphonia_output_format_with_mpeg_mode(
    path: &Path,
    container_format: symphonia::core::formats::FormatId,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    decoded: &symphonia::core::audio::AudioSpec,
    source_kind: PcmKind,
    flac_channel_mask: FlacChannelMaskState,
    mpeg_mode: Option<MpegProgrammeMode>,
) -> Result<SymphoniaOutputFormat, String> {
    let sample_rate = require_symphonia_sample_rate(path, codec_params)?;
    validate_symphonia_sample_rate(path, "decoded", decoded.rate())?;
    if decoded.rate() != sample_rate {
        return Err(format!(
            "{}: decoded sample rate {} does not match track sample rate {sample_rate}",
            path.display(),
            decoded.rate()
        ));
    }
    let channel_count = decoded.channels().count();
    let channels = u16::try_from(channel_count).map_err(|_| {
        format!(
            "{}: too many decoded channels: {channel_count}",
            path.display()
        )
    })?;
    if let Some(declared) = codec_params.channels.as_ref() {
        if declared.count() != channel_count {
            return Err(format!(
                "{}: decoded channel count {channel_count} does not match track channel count {}",
                path.display(),
                declared.count()
            ));
        }
    }
    // Symphonia currently describes a native mono MPEG track as FRONT_LEFT
    // while its decoder reports FRONT_CENTER. The successfully decoded MPEG
    // frame header is authoritative for the one-channel programme mode, so
    // accept that single-channel alias without weakening layout checks for
    // other codecs or multichannel streams.
    let symphonia_layout_provenance = if mpeg_mode == Some(MpegProgrammeMode::Mono) {
        ChannelLayoutProvenance::KnownSpeakers
    } else {
        reconcile_symphonia_layouts(path, codec_params.channels.as_ref(), decoded.channels())?
    };
    let symphonia_layout_provenance = constrain_symphonia_layout_provenance(
        symphonia_layout_provenance,
        container_format,
        codec_params,
        channel_count,
    );
    let role_layout = codec_params.channels.as_ref().unwrap_or(decoded.channels());
    let mut channel_roles = roles_from_symphonia(role_layout);
    if mpeg_mode == Some(MpegProgrammeMode::Mono) {
        channel_roles = default_channel_roles(channels);
    }
    if channel_roles.len() != channel_count {
        channel_roles = default_channel_roles(channels);
    }
    use symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;
    use symphonia::core::formats::well_known::FORMAT_ID_ISOMP4;
    let flac_in_isobmff =
        codec_params.codec == CODEC_ID_FLAC && container_format == FORMAT_ID_ISOMP4;
    let layout_provenance = if flac_in_isobmff {
        // RFC 9639's absent-comment default applies to a native FLAC metadata
        // stream. Symphonia 0.6.1 does not expose the dfLa metadata embedded
        // in ISO BMFF. Neither an absent observation nor a same-named generic
        // MP4 tag can prove the codec stream's physical speaker assignment.
        symphonia_layout_provenance
    } else {
        match flac_channel_mask {
            FlacChannelMaskState::Absent if codec_params.codec == CODEC_ID_FLAC => {
                if let Some(mask) = default_flac_channel_mask(channels) {
                    // RFC 9639 defines an exact default speaker order when no
                    // WAVEFORMATEXTENSIBLE_CHANNEL_MASK comment is present.
                    channel_roles = crate::wav::reader::roles_from_wave_mask(mask, channels);
                    ChannelLayoutProvenance::KnownSpeakers
                } else {
                    ChannelLayoutProvenance::Unknown
                }
            }
            FlacChannelMaskState::Absent => symphonia_layout_provenance,
            FlacChannelMaskState::Valid(mask)
                if crate::wav::reader::wave_mask_is_complete_standard(mask, channels) =>
            {
                // RFC 9639 binds FLAC planes to the set bits in increasing bit
                // order. Use that explicit mapping instead of Symphonia's
                // channel-count default, including for valid non-default masks.
                channel_roles = crate::wav::reader::roles_from_wave_mask(mask, channels);
                ChannelLayoutProvenance::KnownSpeakers
            }
            // Zero, partial, reserved-bit, malformed, or conflicting masks do
            // not identify every decoded plane and remain non-authoritative.
            FlacChannelMaskState::Valid(_) | FlacChannelMaskState::Invalid => {
                ChannelLayoutProvenance::Unknown
            }
        }
    };
    let channel_layout = if codec_params.codec == CODEC_ID_FLAC && !flac_in_isobmff {
        match flac_channel_mask {
            FlacChannelMaskState::Absent => ChannelLayoutDescriptor::flac(channels, None),
            FlacChannelMaskState::Valid(mask) => {
                ChannelLayoutDescriptor::flac(channels, Some(mask))
            }
            FlacChannelMaskState::Invalid => {
                channel_layout_from_symphonia(role_layout, ChannelLayoutProvenance::Unknown)
                    .with_origin(ChannelLayoutOrigin::Flac)
            }
        }
    } else {
        channel_layout_from_symphonia(role_layout, layout_provenance)
    };
    Ok(SymphoniaOutputFormat {
        sample_rate,
        channels,
        decoded_layout: decoded.channels().clone(),
        declared_layout: codec_params.channels.clone(),
        channel_roles,
        layout_provenance,
        channel_layout,
        flac_channel_mask,
        source_kind,
    })
}

fn constrain_symphonia_layout_provenance(
    provenance: ChannelLayoutProvenance,
    container_format: symphonia::core::formats::FormatId,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    decoded_channels: usize,
) -> ChannelLayoutProvenance {
    use symphonia::core::codecs::audio::well_known::{CODEC_ID_ALAC, CODEC_ID_FLAC};
    use symphonia::core::formats::well_known::FORMAT_ID_ISOMP4;

    // Symphonia 0.6.1 loses the ISO BMFF LPCM sample-entry version after
    // parsing. Version 2 may describe auxiliary/discrete channels, but the
    // reader substitutes a count-derived standard speaker set, including for
    // mono/stereo. No decoded MP4 PCM layout is therefore proven.
    if container_format == FORMAT_ID_ISOMP4 && is_symphonia_pcm_codec(codec_params.codec) {
        return ChannelLayoutProvenance::Unknown;
    }

    // Symphonia 0.6.1's ISO BMFF reader parses dfLa but does not expose the
    // subsequent FLAC metadata needed to prove an RFC 9639 channel-mask tag.
    if container_format == FORMAT_ID_ISOMP4 && codec_params.codec == CODEC_ID_FLAC {
        return ChannelLayoutProvenance::Unknown;
    }

    // A 24-byte ALAC cookie has no explicit channel-layout atom. For more than
    // two channels Symphonia 0.6.1 substitutes the standard layout for the
    // channel count even though the additional channels are auxiliary. Its
    // decoder validates the explicit layout carried by the 48-byte cookie.
    if codec_params.codec == CODEC_ID_ALAC
        && decoded_channels > 2
        && codec_params
            .extra_data
            .as_ref()
            .map_or(0, |data| data.len())
            != 48
    {
        return ChannelLayoutProvenance::Unknown;
    }

    provenance
}

fn is_symphonia_pcm_codec(codec: symphonia::core::codecs::audio::AudioCodecId) -> bool {
    use symphonia::core::codecs::audio::well_known::*;

    matches!(
        codec,
        CODEC_ID_PCM_S32LE
            | CODEC_ID_PCM_S32LE_PLANAR
            | CODEC_ID_PCM_S32BE
            | CODEC_ID_PCM_S32BE_PLANAR
            | CODEC_ID_PCM_S24LE
            | CODEC_ID_PCM_S24LE_PLANAR
            | CODEC_ID_PCM_S24BE
            | CODEC_ID_PCM_S24BE_PLANAR
            | CODEC_ID_PCM_S16LE
            | CODEC_ID_PCM_S16LE_PLANAR
            | CODEC_ID_PCM_S16BE
            | CODEC_ID_PCM_S16BE_PLANAR
            | CODEC_ID_PCM_S8
            | CODEC_ID_PCM_S8_PLANAR
            | CODEC_ID_PCM_U32LE
            | CODEC_ID_PCM_U32LE_PLANAR
            | CODEC_ID_PCM_U32BE
            | CODEC_ID_PCM_U32BE_PLANAR
            | CODEC_ID_PCM_U24LE
            | CODEC_ID_PCM_U24LE_PLANAR
            | CODEC_ID_PCM_U24BE
            | CODEC_ID_PCM_U24BE_PLANAR
            | CODEC_ID_PCM_U16LE
            | CODEC_ID_PCM_U16LE_PLANAR
            | CODEC_ID_PCM_U16BE
            | CODEC_ID_PCM_U16BE_PLANAR
            | CODEC_ID_PCM_U8
            | CODEC_ID_PCM_U8_PLANAR
            | CODEC_ID_PCM_F32LE
            | CODEC_ID_PCM_F32LE_PLANAR
            | CODEC_ID_PCM_F32BE
            | CODEC_ID_PCM_F32BE_PLANAR
            | CODEC_ID_PCM_F64LE
            | CODEC_ID_PCM_F64LE_PLANAR
            | CODEC_ID_PCM_F64BE
            | CODEC_ID_PCM_F64BE_PLANAR
            | CODEC_ID_PCM_ALAW
            | CODEC_ID_PCM_MULAW
    )
}

fn reconcile_symphonia_layouts(
    path: &Path,
    declared: Option<&symphonia::core::audio::Channels>,
    decoded: &symphonia::core::audio::Channels,
) -> Result<ChannelLayoutProvenance, String> {
    use ChannelLayoutProvenance::{KnownSpeakers, SceneBased, Unknown};

    let decoded_provenance = layout_provenance_from_symphonia(decoded);
    let Some(declared) = declared else {
        return Ok(decoded_provenance);
    };

    // A channel count alone cannot prove that two PCM planes refer to the same
    // speakers. Compare exact ordered speaker positions whenever both sides
    // provide them, including unsupported positions that remain Unknown.
    if let (Some(declared_positions), Some(decoded_positions)) = (
        symphonia_speaker_sequence(declared),
        symphonia_speaker_sequence(decoded),
    ) {
        if declared_positions != decoded_positions {
            return Err(format!(
                "{}: decoded channel layout {decoded} does not match track channel layout {declared}",
                path.display()
            ));
        }
    }

    let declared_provenance = layout_provenance_from_symphonia(declared);
    Ok(match (declared_provenance, decoded_provenance) {
        (KnownSpeakers, KnownSpeakers) => KnownSpeakers,
        (SceneBased, SceneBased) => SceneBased,
        _ => Unknown,
    })
}

fn symphonia_speaker_sequence(channels: &symphonia::core::audio::Channels) -> Option<Vec<u64>> {
    use symphonia::core::audio::{ChannelLabel, Channels};

    match channels {
        Channels::Positioned(positions) => {
            Some(positions.iter().map(|position| position.bits()).collect())
        }
        Channels::Custom(labels) => labels
            .iter()
            .map(|label| {
                let ChannelLabel::Positioned(position) = label else {
                    return None;
                };
                (position.bits().count_ones() == 1).then_some(position.bits())
            })
            .collect(),
        _ => None,
    }
}

fn validate_symphonia_track_compatibility(
    path: &Path,
    output: &SymphoniaOutputFormat,
    codec_params: &symphonia::core::codecs::audio::AudioCodecParameters,
    source_kind: PcmKind,
    flac_channel_mask: FlacChannelMaskState,
) -> Result<(), String> {
    let sample_rate = require_symphonia_sample_rate(path, codec_params)?;
    if sample_rate != output.sample_rate {
        return Err(format!(
            "{}: chained stream sample rate changed from {} to {sample_rate}",
            path.display(),
            output.sample_rate
        ));
    }
    if source_kind != output.source_kind {
        return Err(format!(
            "{}: chained stream source sample kind changed from {:?} to {:?}",
            path.display(),
            output.source_kind,
            source_kind
        ));
    }
    if flac_channel_mask != output.flac_channel_mask {
        return Err(format!(
            "{}: chained stream FLAC channel-mask metadata changed",
            path.display()
        ));
    }
    if let Some(layout) = codec_params.channels.as_ref() {
        if layout.count() != usize::from(output.channels) {
            return Err(format!(
                "{}: chained stream channel count changed from {} to {}",
                path.display(),
                output.channels,
                layout.count()
            ));
        }
        let expected_layout = output
            .declared_layout
            .as_ref()
            .unwrap_or(&output.decoded_layout);
        if layout != expected_layout {
            return Err(format!(
                "{}: chained stream channel layout changed from {expected_layout} to {layout}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_symphonia_decoded_compatibility(
    path: &Path,
    output: &SymphoniaOutputFormat,
    decoded: &symphonia::core::audio::AudioSpec,
    source_kind: PcmKind,
) -> Result<(), String> {
    validate_symphonia_sample_rate(path, "decoded", decoded.rate())?;
    if decoded.rate() != output.sample_rate {
        return Err(format!(
            "{}: decoded sample rate changed from {} to {}",
            path.display(),
            output.sample_rate,
            decoded.rate()
        ));
    }
    if decoded.channels().count() != usize::from(output.channels) {
        return Err(format!(
            "{}: decoded channel count changed from {} to {}",
            path.display(),
            output.channels,
            decoded.channels().count()
        ));
    }
    if decoded.channels() != &output.decoded_layout {
        return Err(format!(
            "{}: decoded channel layout changed from {} to {}",
            path.display(),
            output.decoded_layout,
            decoded.channels()
        ));
    }
    if source_kind != output.source_kind {
        return Err(format!(
            "{}: decoded source sample kind changed from {:?} to {:?}",
            path.display(),
            output.source_kind,
            source_kind
        ));
    }
    Ok(())
}

fn enforce_decoded_sample_limit(
    path: &Path,
    frames: u64,
    channels: u64,
    max_decoded_samples: u64,
) -> Result<(), String> {
    let samples = frames
        .checked_mul(channels)
        .ok_or_else(|| format!("{}: decoded sample count overflow", path.display()))?;
    if samples > max_decoded_samples {
        return Err(format!(
            "{}: decoded sample count {samples} exceeds safety limit {max_decoded_samples}",
            path.display()
        ));
    }
    Ok(())
}

fn roles_from_symphonia(channels: &symphonia::core::audio::Channels) -> Vec<ChannelRole> {
    use symphonia::core::audio::{ChannelLabel, Channels};

    match channels {
        Channels::Positioned(positions) => {
            let sequence = positions
                .iter()
                .map(|position| position.bits())
                .collect::<Vec<_>>();
            standard_wave_roles_from_symphonia_sequence(&sequence)
                .unwrap_or_else(|| positions.iter().map(role_from_symphonia_position).collect())
        }
        Channels::Discrete(count) => default_channel_roles(*count),
        Channels::Ambisonic(order) => {
            let count = (1 + usize::from(*order)) * (1 + usize::from(*order));
            vec![ChannelRole::Main; count]
        }
        Channels::Custom(labels) => {
            let sequence = labels
                .iter()
                .map(|label| match label {
                    ChannelLabel::Positioned(position) => Some(position.bits()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            if let Some(roles) = sequence
                .as_deref()
                .and_then(standard_wave_roles_from_symphonia_sequence)
            {
                return roles;
            }
            labels
                .iter()
                .map(|label| match label {
                    ChannelLabel::Positioned(position) => role_from_symphonia_position(*position),
                    ChannelLabel::Discrete(_)
                    | ChannelLabel::Ambisonic(_)
                    | ChannelLabel::AmbisonicBFormat(_) => ChannelRole::Main,
                    _ => ChannelRole::Main,
                })
                .collect()
        }
        Channels::None => Vec::new(),
        _ => vec![ChannelRole::Main; channels.count()],
    }
}

fn channel_layout_from_symphonia(
    channels: &symphonia::core::audio::Channels,
    provenance: ChannelLayoutProvenance,
) -> ChannelLayoutDescriptor {
    use symphonia::core::audio::{ChannelLabel, Channels};

    let compatibility_roles = roles_from_symphonia(channels);
    let assignments = match channels {
        Channels::Positioned(positions) => positions
            .iter()
            .zip(compatibility_roles.iter().copied())
            .enumerate()
            .map(|(index, (position, role))| {
                assignment_from_symphonia_position(position, role, index)
            })
            .collect(),
        Channels::Discrete(count) => (0..usize::from(*count))
            .map(|index| ChannelAssignment::unassigned(index as u32))
            .collect(),
        Channels::Ambisonic(order) => {
            let count = (1 + usize::from(*order)) * (1 + usize::from(*order));
            (0..count)
                .map(|index| ChannelAssignment::ambisonic(index as u32))
                .collect()
        }
        Channels::Custom(labels) => labels
            .iter()
            .zip(compatibility_roles.iter().copied())
            .enumerate()
            .map(|(index, (label, role))| match label {
                ChannelLabel::Positioned(position) => {
                    assignment_from_symphonia_position(*position, role, index)
                }
                ChannelLabel::Discrete(component) => {
                    ChannelAssignment::unassigned(u32::from(*component))
                }
                ChannelLabel::Ambisonic(component) => {
                    ChannelAssignment::ambisonic(u32::from(*component))
                }
                ChannelLabel::AmbisonicBFormat(_) => ChannelAssignment::ambisonic(index as u32),
                _ => ChannelAssignment::unassigned(index as u32),
            })
            .collect(),
        _ => Vec::new(),
    };
    if assignments.is_empty() {
        let roles = roles_from_symphonia(channels);
        return ChannelLayoutDescriptor::decoded_from_roles(&roles, provenance);
    }
    ChannelLayoutDescriptor::decoded(assignments, provenance)
}

fn assignment_from_symphonia_position(
    position: symphonia::core::audio::Position,
    role: ChannelRole,
    index: usize,
) -> ChannelAssignment {
    let bits = position.bits();
    if bits.count_ones() != 1 {
        return ChannelAssignment::unassigned(index as u32);
    }
    let bit = bits.trailing_zeros() as u8;
    let cicp = match bit {
        4 if role == ChannelRole::positioned(-110, 0) => 4,
        5 if role == ChannelRole::positioned(110, 0) => 5,
        0..=17 => crate::channel_layout::wave_bit_to_cicp(bit),
        18 => 26,
        _ => return ChannelAssignment::unassigned(index as u32),
    };
    let assignment = ChannelAssignment::cicp(cicp);
    if assignment.channel_role() == role {
        assignment
    } else {
        ChannelAssignment::legacy_role(role)
    }
}

fn standard_wave_roles_from_symphonia_sequence(sequence: &[u64]) -> Option<Vec<ChannelRole>> {
    let mut mask = 0_u32;
    let mut previous = None;
    for bits in sequence {
        if bits.count_ones() != 1 || *bits >= 1 << 18 || previous.is_some_and(|bit| bit >= *bits) {
            return None;
        }
        previous = Some(*bits);
        mask |= u32::try_from(*bits).ok()?;
    }
    let channels = u16::try_from(sequence.len()).ok()?;
    Some(crate::wav::reader::roles_from_wave_mask(mask, channels))
}

fn layout_provenance_from_symphonia(
    channels: &symphonia::core::audio::Channels,
) -> ChannelLayoutProvenance {
    use symphonia::core::audio::{ChannelLabel, Channels};

    match channels {
        Channels::Positioned(positions) if supported_speaker_positions(*positions) => {
            ChannelLayoutProvenance::KnownSpeakers
        }
        Channels::Ambisonic(_) => ChannelLayoutProvenance::SceneBased,
        Channels::Custom(labels)
            if !labels.is_empty()
                && labels.iter().all(|label| {
                    matches!(
                        label,
                        ChannelLabel::Ambisonic(_) | ChannelLabel::AmbisonicBFormat(_)
                    )
                }) =>
        {
            ChannelLayoutProvenance::SceneBased
        }
        Channels::Custom(labels) if custom_speaker_positions_are_supported(labels) => {
            ChannelLayoutProvenance::KnownSpeakers
        }
        _ => ChannelLayoutProvenance::Unknown,
    }
}

fn custom_speaker_positions_are_supported(labels: &[symphonia::core::audio::ChannelLabel]) -> bool {
    use symphonia::core::audio::ChannelLabel;

    let mut seen = 0_u64;
    !labels.is_empty()
        && labels.iter().all(|label| {
            let ChannelLabel::Positioned(position) = label else {
                return false;
            };
            let bits = position.bits();
            if bits.count_ones() != 1 || !supported_speaker_positions(*position) || seen & bits != 0
            {
                return false;
            }
            seen |= bits;
            true
        })
}

fn supported_speaker_positions(positions: symphonia::core::audio::Position) -> bool {
    // The first 18 bits are the standardized WAVE speaker set. Symphonia's
    // immediately following LFE2 bit is also represented exactly by Forge.
    const SUPPORTED_BITS: u64 = (1 << 19) - 1;
    positions.bits() != 0 && positions.bits() & !SUPPORTED_BITS == 0
}

fn role_from_symphonia_position(position: symphonia::core::audio::Position) -> ChannelRole {
    use symphonia::core::audio::Position;

    let p = ChannelRole::positioned;
    match position {
        Position::FRONT_LEFT => p(-30, 0),
        Position::FRONT_RIGHT => p(30, 0),
        Position::FRONT_CENTER => p(0, 0),
        Position::LFE1 | Position::LFE2 => ChannelRole::Lfe,
        Position::REAR_LEFT => p(-135, 0),
        Position::REAR_RIGHT => p(135, 0),
        Position::FRONT_LEFT_CENTER => p(-15, 0),
        Position::FRONT_RIGHT_CENTER => p(15, 0),
        Position::REAR_CENTER => p(180, 0),
        Position::SIDE_LEFT => p(-90, 0),
        Position::SIDE_RIGHT => p(90, 0),
        Position::TOP_CENTER => p(0, 90),
        Position::TOP_FRONT_LEFT => p(-30, 45),
        Position::TOP_FRONT_CENTER => p(0, 45),
        Position::TOP_FRONT_RIGHT => p(30, 45),
        Position::TOP_REAR_LEFT => p(-135, 45),
        Position::TOP_REAR_CENTER => p(180, 45),
        Position::TOP_REAR_RIGHT => p(135, 45),
        _ => ChannelRole::Main,
    }
}

/// Decode an audio file in bounded chunks without retaining the complete
/// sample stream.
pub fn decode_stream<F>(path: &Path, consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let mut consume = consume;
    decode_stream_with_layout(path, |info, provenance, planar| {
        require_known_layout(path, provenance)?;
        consume(info, planar)
    })
}

/// Decode while exposing container-declared duration only to internal callers.
///
/// The extra value is deliberately kept out of the public [`StreamInfo`] API:
/// it is a storage-planning hint, and only format-specific callers that can
/// prove the declaration exact may trust it as an allocation bound.
pub(crate) fn decode_stream_with_declared_frames<F>(
    path: &Path,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, Option<u64>, &mut [Vec<f32>]) -> Result<(), String>,
{
    decode_stream_with_layout_and_declared_frames(path, |info, _, declared_frames, planar| {
        consume(info, declared_frames, planar)
    })
}

/// Decode while retaining the provenance of the channel-to-speaker mapping.
///
/// Every native and Symphonia-backed route supplies this sidecar before its
/// first PCM callback. Callers that apply speaker-dependent DSP can therefore
/// reject ambiguous or scene-based inputs without changing the public decode
/// API or treating fallback roles as authoritative metadata.
pub fn decode_stream_with_layout<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, &mut [Vec<f32>]) -> Result<(), String>,
{
    decode_stream_with_layout_and_declared_frames(path, |info, provenance, _, planar| {
        consume(info, provenance, planar)
    })
}

pub(crate) fn decode_stream_with_layout_and_declared_frames<F>(
    path: &Path,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
{
    decode_stream_with_flac_workers(path, None, consume)
}

/// Decode exactly the track, frame range, and layout bound by a descriptor.
pub fn decode_descriptor_stream_with_layout<F>(
    descriptor: &InputDescriptor,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, &mut [Vec<f32>]) -> Result<(), String>,
{
    decode_descriptor_stream_with_layout_and_declared_frames(
        descriptor,
        |info, provenance, _, planar| consume(info, provenance, planar),
    )
}

/// Decode a descriptor-bound programme while supplying the effective exact
/// channel layout before every PCM callback.
pub fn decode_descriptor_stream_with_channel_layout<F>(
    descriptor: &InputDescriptor,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &ChannelLayoutDescriptor, &mut [Vec<f32>]) -> Result<(), String>,
{
    decode_descriptor_stream_with_layout(descriptor, |info, provenance, planar| {
        if provenance != descriptor.channel_layout.provenance() {
            return Err("descriptor exact layout provenance changed during decode".into());
        }
        consume(info, &descriptor.channel_layout, planar)
    })
}

pub(crate) fn decode_descriptor_stream_with_layout_and_declared_frames<F>(
    descriptor: &InputDescriptor,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
{
    decode_descriptor_stream_with_layout_and_declared_frames_controlled(
        descriptor,
        None,
        None,
        || Ok(()),
        consume,
    )
}

fn decode_descriptor_stream_with_layout_and_declared_frames_controlled<F, C>(
    descriptor: &InputDescriptor,
    forced_flac_workers: Option<usize>,
    max_packet_samples: Option<u64>,
    mut checkpoint: C,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
    C: FnMut() -> Result<(), String> + Send,
{
    const RANGE_COMPLETE: &str = "__forge_input_descriptor_range_complete__";
    let selection = descriptor.track_selection;
    let range = descriptor.range;
    let range_end = range.frames.map(|frames| {
        range
            .start
            .checked_add(frames)
            .expect("validated descriptor range")
    });
    let declared_frames = descriptor.declared_frames.map(|declared| {
        let available = declared.saturating_sub(range.start);
        range
            .frames
            .map_or(available, |frames| available.min(frames))
    });
    let effective_provenance = if descriptor.uses_explicit_channel_roles() {
        descriptor.channel_layout.provenance()
    } else {
        descriptor.declared_layout_provenance
    };
    let mut source_frame = 0_u64;
    let mut delivered = 0_u64;
    let result = decode_stream_raw_with_selection_and_control(
        descriptor.input.stable_path(),
        descriptor.route,
        selection,
        forced_flac_workers,
        max_packet_samples.map(|max_packet_samples| ServiceDecodeControl {
            max_packet_samples,
            expected_preflight: descriptor.service_preflight,
        }),
        &mut checkpoint,
        |info, provenance, _, planar| {
            validate_descriptor_decode(descriptor, info, provenance)?;
            let chunk_frames = planar.first().map_or(0, Vec::len);
            if planar.iter().any(|channel| channel.len() != chunk_frames) {
                return Err("decoded descriptor stream has unequal channel lengths".into());
            }
            let chunk_frames = u64::try_from(chunk_frames)
                .map_err(|_| "decoded chunk frame count exceeds u64".to_string())?;
            let chunk_start = source_frame;
            let chunk_end = chunk_start
                .checked_add(chunk_frames)
                .ok_or_else(|| "decoded descriptor frame count overflow".to_string())?;
            source_frame = chunk_end;
            if range_end.is_some_and(|end| chunk_start >= end) {
                return Err(RANGE_COMPLETE.into());
            }
            let overlap_start = chunk_start.max(range.start);
            let overlap_end = range_end.map_or(chunk_end, |end| chunk_end.min(end));
            if overlap_start < overlap_end {
                let start = usize::try_from(overlap_start - chunk_start)
                    .map_err(|_| "descriptor range start exceeds usize".to_string())?;
                let end = usize::try_from(overlap_end - chunk_start)
                    .map_err(|_| "descriptor range end exceeds usize".to_string())?;
                if start != 0 || end != usize::try_from(chunk_frames).unwrap_or(usize::MAX) {
                    for channel in planar.iter_mut() {
                        channel.copy_within(start..end, 0);
                        channel.truncate(end - start);
                    }
                }
                delivered = delivered
                    .checked_add(overlap_end - overlap_start)
                    .ok_or_else(|| "descriptor delivered frame count overflow".to_string())?;
                consume(
                    &descriptor.info,
                    effective_provenance,
                    declared_frames,
                    planar,
                )?;
            }
            if range_end.is_some_and(|end| chunk_end >= end) {
                Err(RANGE_COMPLETE.into())
            } else {
                Ok(())
            }
        },
    );
    match result {
        Ok(_) => {}
        Err(error) if error == RANGE_COMPLETE => {}
        Err(error) => return Err(error),
    }
    if delivered == 0 {
        return Err(format!(
            "{}: selected input range contains no audio",
            display_input(&descriptor.input)
        ));
    }
    Ok(descriptor.info.clone())
}

/// Ownership-transferring descriptor decode used by bounded producer/consumer
/// pipelines. The selected track and range remain enforced by the regular
/// descriptor decoder; only the reusable channel buffers cross the handoff.
pub(crate) fn decode_descriptor_stream_owned_with_layout_and_declared_frames<F>(
    descriptor: &InputDescriptor,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        Vec<Vec<f32>>,
    ) -> Result<Vec<Vec<f32>>, String>,
{
    let mut handoff = Vec::new();
    decode_descriptor_stream_with_layout_and_declared_frames(
        descriptor,
        |info, provenance, declared_frames, planar| {
            handoff.reserve(planar.len());
            for channel in planar.iter_mut() {
                handoff.push(std::mem::take(channel));
            }
            let mut recycled = consume(
                info,
                provenance,
                declared_frames,
                std::mem::take(&mut handoff),
            )?;
            if recycled.len() != planar.len() {
                return Err(format!(
                    "descriptor stream consumer returned {} channels, expected {}",
                    recycled.len(),
                    planar.len()
                ));
            }
            for (slot, channel) in planar.iter_mut().zip(recycled.drain(..)) {
                *slot = channel;
            }
            handoff = recycled;
            Ok(())
        },
    )
}

/// One bounded PCM chunk in the narrowest exact representation needed by the
/// loudness analyzer. Common, compressed, and S24 formats retain the optimized
/// f32 lane; every S24 code is exactly representable after power-of-two
/// normalization. S32 and F64 WAVE samples avoid an irreversible conversion.
pub(crate) enum AnalysisPcmChunk<'a> {
    F32(&'a [Vec<f32>]),
    S32(&'a [Vec<i32>]),
    F64(&'a [Vec<f64>]),
}

pub(crate) const SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED: &str =
    "__forge_service_packet_sample_limit_exceeded__";
pub(crate) const SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED: &str =
    "__forge_service_encoded_packet_limit_exceeded__";

impl AnalysisPcmChunk<'_> {
    pub(crate) fn frames(&self) -> usize {
        match self {
            Self::F32(planar) => planar.first().map_or(0, Vec::len),
            Self::S32(planar) => planar.first().map_or(0, Vec::len),
            Self::F64(planar) => planar.first().map_or(0, Vec::len),
        }
    }
}

/// Decode the descriptor's exact programme for loudness measurement.
///
/// Native S32/F64 WAVE streams are read directly from their immutable
/// snapshot. Other inputs share the regular bounded decoder stream; its S24
/// normalization is exact and retains the optimized f32 analyzer lane.
pub(crate) fn decode_descriptor_analysis_stream<F>(
    descriptor: &InputDescriptor,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, AnalysisPcmChunk<'_>) -> Result<(), String>,
{
    decode_descriptor_analysis_stream_impl::<false, _, _>(
        descriptor,
        None,
        None,
        || Ok(()),
        consume,
    )
}

/// Service-only descriptor decode with bounded cooperative checkpoints.
///
/// A controlled decode forces native FLAC onto one decoder so every packet is
/// observed by the same checkpoint closure. Ordinary library callers retain
/// the existing parallel route and results.
pub(crate) fn decode_descriptor_analysis_stream_with_control<F, C>(
    descriptor: &InputDescriptor,
    max_decoded_samples: u64,
    checkpoint: C,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, AnalysisPcmChunk<'_>) -> Result<(), String>,
    C: FnMut() -> Result<(), String> + Send,
{
    decode_descriptor_analysis_stream_impl::<true, _, _>(
        descriptor,
        Some(1),
        Some(max_decoded_samples),
        checkpoint,
        consume,
    )
}

fn decode_descriptor_analysis_stream_impl<const CONTROLLED: bool, F, C>(
    descriptor: &InputDescriptor,
    forced_flac_workers: Option<usize>,
    max_packet_samples: Option<u64>,
    mut checkpoint: C,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, AnalysisPcmChunk<'_>) -> Result<(), String>,
    C: FnMut() -> Result<(), String> + Send,
{
    debug_assert_eq!(CONTROLLED, max_packet_samples.is_some());
    if CONTROLLED {
        checkpoint()?;
    }
    if descriptor.route != DecoderRoute::Wave
        || !matches!(descriptor.info.source_kind, PcmKind::S32 | PcmKind::F64)
    {
        return decode_descriptor_stream_with_layout_and_declared_frames_controlled(
            descriptor,
            forced_flac_workers,
            max_packet_samples,
            &mut checkpoint,
            |info, provenance, _, planar| consume(info, provenance, AnalysisPcmChunk::F32(planar)),
        );
    }

    let path = descriptor.input.stable_path();
    let (wav, provenance) = if CONTROLLED {
        WavReader::probe_with_layout_controlled(path, &mut checkpoint)
            .map_err(|error| error.to_string())
    } else {
        WavReader::probe_with_layout(path).map_err(|error| error.to_string())
    }
    .map_err(|error| format!("{}: {error}", display_input(&descriptor.input)))?;
    let decoded_info = StreamInfo {
        sample_rate: wav.sample_rate,
        channels: wav.channels,
        channel_roles: wav.channel_roles,
        source_kind: wav.kind,
    };
    validate_descriptor_decode(descriptor, &decoded_info, provenance)?;
    let effective_provenance = if descriptor.uses_explicit_channel_roles() {
        descriptor.channel_layout.provenance()
    } else {
        provenance
    };
    let channels = usize::from(wav.channels);
    let frame_bytes = channels
        .checked_mul(wav.kind.bytes_per_sample())
        .ok_or_else(|| "WAVE frame size overflow".to_string())?;
    let frame_bytes_u64 =
        u64::try_from(frame_bytes).map_err(|_| "WAVE frame size exceeds u64".to_string())?;
    let total_frames = wav.data_size / frame_bytes_u64;
    let start = descriptor.range.start.min(total_frames);
    let available = total_frames.saturating_sub(start);
    let selected_frames = descriptor
        .range
        .frames
        .map_or(available, |frames| frames.min(available));
    if selected_frames == 0 {
        return Err(format!(
            "{}: selected input range contains no audio",
            display_input(&descriptor.input)
        ));
    }
    if CONTROLLED {
        let limit = max_packet_samples.expect("a controlled analysis owns a packet limit");
        let selected_samples = selected_frames
            .checked_mul(channels as u64)
            .ok_or_else(|| "selected WAVE sample count overflow".to_string())?;
        if selected_samples > limit {
            return Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into());
        }
    }
    let byte_offset = start
        .checked_mul(frame_bytes_u64)
        .and_then(|offset| wav.data_offset.checked_add(offset))
        .ok_or_else(|| "selected WAVE byte range overflows u64".to_string())?;
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(byte_offset))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let configured_chunk_bytes = wav_stream_chunk_bytes(wav.channels, wav.kind);
    let chunk_bytes = if CONTROLLED {
        let limit = max_packet_samples.expect("a controlled analysis owns a packet limit");
        let max_frames = limit / channels as u64;
        let max_frames = usize::try_from(max_frames).unwrap_or(usize::MAX).max(1);
        configured_chunk_bytes.min(max_frames.saturating_mul(frame_bytes).max(frame_bytes))
    } else {
        configured_chunk_bytes
    };
    let chunk_frames = chunk_bytes / frame_bytes;
    let mut bytes = vec![0_u8; chunk_bytes];
    let mut integer_planar = Vec::new();
    let mut f64_planar = Vec::new();
    let mut remaining_frames = selected_frames;
    while remaining_frames != 0 {
        if CONTROLLED {
            checkpoint()?;
        }
        let frames = remaining_frames.min(chunk_frames as u64) as usize;
        if CONTROLLED {
            let limit = max_packet_samples.expect("a controlled analysis owns a packet limit");
            let packet_samples = u64::try_from(frames)
                .ok()
                .and_then(|frames| frames.checked_mul(channels as u64))
                .ok_or_else(|| "decoded WAVE chunk sample count overflow".to_string())?;
            if packet_samples > limit {
                return Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into());
            }
        }
        let bytes_to_read = frames
            .checked_mul(frame_bytes)
            .ok_or_else(|| "selected WAVE chunk size overflow".to_string())?;
        file.read_exact(&mut bytes[..bytes_to_read])
            .map_err(|error| format!("{}: {error}", path.display()))?;
        match wav.kind {
            PcmKind::S32 => {
                crate::dsp::convert::decode_s32_planar_into(
                    &bytes[..bytes_to_read],
                    channels,
                    &mut integer_planar,
                );
                consume(
                    &descriptor.info,
                    effective_provenance,
                    AnalysisPcmChunk::S32(&integer_planar),
                )?;
            }
            PcmKind::F64 => {
                crate::dsp::convert::decode_f64_planar_into(
                    &bytes[..bytes_to_read],
                    channels,
                    &mut f64_planar,
                );
                consume(
                    &descriptor.info,
                    effective_provenance,
                    AnalysisPcmChunk::F64(&f64_planar),
                )?;
            }
            _ => unreachable!("high-precision WAVE kind was selected above"),
        }
        if CONTROLLED {
            checkpoint()?;
        }
        remaining_frames -= frames as u64;
    }
    Ok(descriptor.info.clone())
}

/// Bounded full-buffer decode of the programme selected by a descriptor.
pub fn decode_descriptor_limited_with_layout(
    descriptor: &InputDescriptor,
    max_decoded_samples: u64,
) -> Result<(AudioBuffer, ChannelLayoutProvenance), String> {
    if max_decoded_samples == 0 {
        return Err("decoded sample limit must be greater than zero".into());
    }
    let mut data = vec![Vec::new(); usize::from(descriptor.info.channels)];
    let mut layout = None;
    let info = decode_descriptor_stream_with_layout(descriptor, |info, provenance, planar| {
        let packet_frames = planar.first().map_or(0, Vec::len) as u64;
        let accumulated = data.first().map_or(0, Vec::len) as u64;
        enforce_decoded_sample_limit(
            descriptor.input.stable_path(),
            accumulated.saturating_add(packet_frames),
            u64::from(info.channels),
            max_decoded_samples,
        )?;
        if layout
            .replace(provenance)
            .is_some_and(|previous| previous != provenance)
        {
            return Err("descriptor layout provenance changed during decode".into());
        }
        for (destination, source) in data.iter_mut().zip(planar) {
            destination.extend_from_slice(source);
        }
        Ok(())
    })?;
    let frames = data.first().map_or(0, Vec::len);
    Ok((
        AudioBuffer {
            sample_rate: info.sample_rate,
            channels: info.channels,
            channel_roles: info.channel_roles,
            frames,
            data,
            source_kind: info.source_kind,
        },
        layout.expect("descriptor decoding delivers at least one chunk"),
    ))
}

/// Bounded full-buffer descriptor decode with its effective exact layout.
pub fn decode_descriptor_limited_with_channel_layout(
    descriptor: &InputDescriptor,
    max_decoded_samples: u64,
) -> Result<(AudioBuffer, ChannelLayoutDescriptor), String> {
    let (buffer, provenance) =
        decode_descriptor_limited_with_layout(descriptor, max_decoded_samples)?;
    if provenance != descriptor.channel_layout.provenance()
        || descriptor.channel_layout.channel_count() != usize::from(buffer.channels)
    {
        return Err("descriptor exact layout does not match the decoded PCM stream".into());
    }
    Ok((buffer, descriptor.channel_layout.clone()))
}

fn validate_descriptor_decode(
    descriptor: &InputDescriptor,
    info: &StreamInfo,
    provenance: ChannelLayoutProvenance,
) -> Result<(), String> {
    if info.sample_rate != descriptor.info.sample_rate
        || info.channels != descriptor.info.channels
        || info.source_kind != descriptor.info.source_kind
        || info.channel_roles != descriptor.decoder_channel_roles
        || provenance != descriptor.decoder_layout_provenance
    {
        return Err("decoded stream no longer matches its input descriptor".into());
    }
    Ok(())
}

/// Decode with small planar-f32 packets coalesced for the normalization render
/// pass. Analysis deliberately keeps codec packet boundaries: larger chunks
/// can reduce True Peak pruning efficiency on some architectures.
pub(crate) fn decode_stream_coalesced<F>(path: &Path, consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let mut consume = consume;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !matches!(
        extension.as_str(),
        "mp3" | "aac" | "m4a" | "mp4" | "ogg" | "oga"
    ) {
        return decode_stream_with_declared_frames(path, |info, _, planar| consume(info, planar));
    }

    let mut pending = Vec::new();
    let info = decode_stream_with_declared_frames(path, |info, _, planar| {
        append_symphonia_stream_chunk(info, planar, &mut pending, &mut consume)
    })?;
    flush_symphonia_stream_chunk(&info, &mut pending, &mut consume)?;
    Ok(info)
}

/// Descriptor-bound counterpart of [`decode_stream_coalesced`].
pub(crate) fn decode_descriptor_stream_coalesced<F>(
    descriptor: &InputDescriptor,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let mut consume = consume;
    if !matches!(
        descriptor.codec,
        AudioCodec::Mp1
            | AudioCodec::Mp2
            | AudioCodec::Mp3
            | AudioCodec::Aac
            | AudioCodec::Alac
            | AudioCodec::Vorbis
            | AudioCodec::Opus
    ) {
        return decode_descriptor_stream_with_layout(descriptor, |info, _, planar| {
            consume(info, planar)
        });
    }

    let mut pending = Vec::new();
    let info = decode_descriptor_stream_with_layout(descriptor, |info, _, planar| {
        append_symphonia_stream_chunk(info, planar, &mut pending, &mut consume)
    })?;
    flush_symphonia_stream_chunk(&info, &mut pending, &mut consume)?;
    Ok(info)
}

fn decode_stream_with_flac_workers<F>(
    path: &Path,
    forced_flac_workers: Option<usize>,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
{
    let route = sniff_decoder_route(path)?;
    decode_stream_raw_with_selection(
        path,
        route,
        AudioTrackSelection::Default,
        forced_flac_workers,
        consume,
    )
}

fn decode_stream_raw_with_selection<F>(
    path: &Path,
    route: DecoderRoute,
    selection: AudioTrackSelection,
    forced_flac_workers: Option<usize>,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
{
    decode_stream_raw_with_selection_and_control(
        path,
        route,
        selection,
        forced_flac_workers,
        None,
        || Ok(()),
        consume,
    )
}

fn symphonia_packet_sample_upper_bound(
    track: &SymphoniaAudioTrack,
    packet: &symphonia::core::packet::Packet,
) -> Result<Option<u64>, String> {
    let Some(channels) = track.codec_params.channels.as_ref() else {
        return Ok(None);
    };
    if let Some(bytes_per_sample) = symphonia_pcm_bytes_per_sample(track.codec_params.codec) {
        let channels = channels.count() as u64;
        let bytes_per_frame = channels
            .checked_mul(bytes_per_sample)
            .ok_or_else(|| "PCM packet frame size overflow".to_string())?;
        let packet_bytes = u64::try_from(packet.data.len())
            .map_err(|_| "PCM packet byte count exceeds u64".to_string())?;
        let frames = packet_bytes
            .checked_add(bytes_per_frame - 1)
            .ok_or_else(|| "PCM packet frame count overflow".to_string())?
            / bytes_per_frame;
        return frames
            .checked_mul(channels)
            .map(Some)
            .ok_or_else(|| "PCM packet sample count overflow".to_string());
    }
    let frames = if let (Some(time_base), Some(sample_rate)) =
        (track.time_base, track.codec_params.sample_rate)
    {
        let numerator = u128::from(packet.block_dur().get())
            .checked_mul(u128::from(time_base.numer.get()))
            .and_then(|value| value.checked_mul(u128::from(sample_rate)))
            .ok_or_else(|| "declared packet frame geometry overflow".to_string())?;
        let denominator = u128::from(time_base.denom.get());
        let frames = numerator
            .checked_add(denominator - 1)
            .ok_or_else(|| "declared packet frame geometry overflow".to_string())?
            / denominator;
        let frames = u64::try_from(frames)
            .map_err(|_| "declared packet frame count exceeds u64".to_string())?;
        if frames == 0 && !packet.data.is_empty() {
            return Ok(None);
        }
        frames
    } else if let Some(frames) = track.codec_params.max_frames_per_packet {
        if frames == 0 && !packet.data.is_empty() {
            return Ok(None);
        }
        frames
    } else {
        return Ok(None);
    };
    frames
        .checked_mul(channels.count() as u64)
        .map(Some)
        .ok_or_else(|| "declared packet sample count overflow".to_string())
}

fn symphonia_pcm_bytes_per_sample(
    codec: symphonia::core::codecs::audio::AudioCodecId,
) -> Option<u64> {
    use symphonia::core::codecs::audio::well_known::*;

    Some(match codec {
        CODEC_ID_PCM_S8
        | CODEC_ID_PCM_S8_PLANAR
        | CODEC_ID_PCM_U8
        | CODEC_ID_PCM_U8_PLANAR
        | CODEC_ID_PCM_ALAW
        | CODEC_ID_PCM_MULAW => 1,
        CODEC_ID_PCM_S16LE
        | CODEC_ID_PCM_S16LE_PLANAR
        | CODEC_ID_PCM_S16BE
        | CODEC_ID_PCM_S16BE_PLANAR
        | CODEC_ID_PCM_U16LE
        | CODEC_ID_PCM_U16LE_PLANAR
        | CODEC_ID_PCM_U16BE
        | CODEC_ID_PCM_U16BE_PLANAR => 2,
        CODEC_ID_PCM_S24LE
        | CODEC_ID_PCM_S24LE_PLANAR
        | CODEC_ID_PCM_S24BE
        | CODEC_ID_PCM_S24BE_PLANAR
        | CODEC_ID_PCM_U24LE
        | CODEC_ID_PCM_U24LE_PLANAR
        | CODEC_ID_PCM_U24BE
        | CODEC_ID_PCM_U24BE_PLANAR => 3,
        CODEC_ID_PCM_S32LE
        | CODEC_ID_PCM_S32LE_PLANAR
        | CODEC_ID_PCM_S32BE
        | CODEC_ID_PCM_S32BE_PLANAR
        | CODEC_ID_PCM_U32LE
        | CODEC_ID_PCM_U32LE_PLANAR
        | CODEC_ID_PCM_U32BE
        | CODEC_ID_PCM_U32BE_PLANAR
        | CODEC_ID_PCM_F32LE
        | CODEC_ID_PCM_F32LE_PLANAR
        | CODEC_ID_PCM_F32BE
        | CODEC_ID_PCM_F32BE_PLANAR => 4,
        CODEC_ID_PCM_F64LE
        | CODEC_ID_PCM_F64LE_PLANAR
        | CODEC_ID_PCM_F64BE
        | CODEC_ID_PCM_F64BE_PLANAR => 8,
        _ => return None,
    })
}

fn enforce_symphonia_packet_sample_limit(
    track: &SymphoniaAudioTrack,
    packet: &symphonia::core::packet::Packet,
    limit: u64,
) -> Result<(), String> {
    let packet_samples = symphonia_packet_sample_upper_bound(track, packet)?
        .ok_or_else(|| SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.to_string())?;
    if packet_samples > limit {
        Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into())
    } else {
        Ok(())
    }
}

fn decode_stream_raw_with_selection_and_control<F, C>(
    path: &Path,
    route: DecoderRoute,
    selection: AudioTrackSelection,
    forced_flac_workers: Option<usize>,
    service_control: Option<ServiceDecodeControl>,
    checkpoint: C,
    consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
    C: FnMut() -> Result<(), String> + Send,
{
    if service_control.is_some() {
        decode_stream_raw_impl::<true, _, _>(
            path,
            route,
            selection,
            forced_flac_workers,
            service_control,
            checkpoint,
            consume,
        )
    } else {
        decode_stream_raw_impl::<false, _, _>(
            path,
            route,
            selection,
            forced_flac_workers,
            service_control,
            checkpoint,
            consume,
        )
    }
}

// Static specialization keeps service polling and packet admission out of the
// ordinary CLI/library packet loop while sharing the decoder state machine.
fn decode_stream_raw_impl<const CONTROLLED: bool, F, C>(
    path: &Path,
    route: DecoderRoute,
    selection: AudioTrackSelection,
    forced_flac_workers: Option<usize>,
    service_control: Option<ServiceDecodeControl>,
    mut checkpoint: C,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
    C: FnMut() -> Result<(), String> + Send,
{
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::default::{get_codecs, get_probe};

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    debug_assert_eq!(CONTROLLED, service_control.is_some());
    if CONTROLLED {
        checkpoint()?;
    }
    let active_service_control = if CONTROLLED {
        Some(service_control.expect("a controlled decode owns service limits"))
    } else {
        None
    };
    let max_packet_samples = active_service_control.map(|control| control.max_packet_samples);
    let service_preflight = if CONTROLLED {
        let observed = service_container_preflight(path, &mut checkpoint)?;
        if active_service_control
            .and_then(|control| control.expected_preflight)
            .is_some_and(|expected| expected != observed)
        {
            return Err("service container route changed after descriptor preflight".into());
        }
        if !service_preflight_accepts_decoder_route(observed.route, route) {
            return Err("service preflight route disagrees with the selected decoder route".into());
        }
        Some(observed)
    } else {
        None
    };
    if route == DecoderRoute::Wave {
        require_single_track(selection)?;
        return decode_wav_stream::<CONTROLLED, _, _>(
            path,
            max_packet_samples,
            &mut checkpoint,
            |info, provenance, declared, planar| {
                if CONTROLLED {
                    enforce_service_packet_sample_limit(info, planar, max_packet_samples)?;
                }
                consume(info, provenance, declared, planar)
            },
        );
    }
    if matches!(route, DecoderRoute::Dsf | DecoderRoute::Dsdiff) {
        require_single_track(selection)?;
        if let Some(max_decoded_samples) = max_packet_samples {
            return crate::dsd::decode_stream_with_layout_and_declared_frames_controlled(
                path,
                max_decoded_samples,
                &mut checkpoint,
                |info, provenance, declared, planar| {
                    enforce_service_packet_sample_limit(info, planar, max_packet_samples)?;
                    consume(info, provenance, declared, planar)
                },
            );
        }
        return crate::dsd::decode_stream_with_layout_and_declared_frames(
            path,
            |info, provenance, declared, planar| consume(info, provenance, declared, planar),
        );
    }
    if route == DecoderRoute::Opus {
        require_single_track(selection)?;
        #[cfg(feature = "opus-encoding")]
        {
            if max_packet_samples.is_some() {
                return crate::opus::decode_stream_controlled(
                    path,
                    &mut checkpoint,
                    |info, planar| {
                        enforce_service_packet_sample_limit(info, planar, max_packet_samples)?;
                        // The native Opus parser accepts only RFC 7845 mapping
                        // families 0 and 1, both of which have canonical speakers.
                        consume(info, ChannelLayoutProvenance::KnownSpeakers, None, planar)
                    },
                );
            }
            return crate::opus::decode_stream(path, |info, planar| {
                // The native Opus parser accepts only RFC 7845 mapping
                // families 0 and 1, both of which have canonical speakers.
                consume(info, ChannelLayoutProvenance::KnownSpeakers, None, planar)
            });
        }
        #[cfg(not(feature = "opus-encoding"))]
        {
            return Err(
                "Ogg Opus support is unavailable; rebuild with `--features opus-encoding`".into(),
            );
        }
    }
    let controlled_checkpoint = if CONTROLLED {
        Some(Mutex::new(checkpoint))
    } else {
        None
    };
    run_decoder_checkpoint::<CONTROLLED, _>(&controlled_checkpoint)?;
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let source: Box<dyn MediaSource + '_> = if let Some(checkpoint) = &controlled_checkpoint {
        let preflight = service_preflight.expect("controlled decode has service preflight");
        Box::new(CheckpointMediaSource::new_range(
            file,
            checkpoint,
            preflight.media_offset,
            preflight.media_end,
        )?)
    } else {
        Box::new(file)
    };
    let stream = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if !extension.is_empty() {
        hint.with_extension(&extension);
    }
    let metadata_options = if CONTROLLED {
        service_metadata_options()
    } else {
        symphonia::core::meta::MetadataOptions::default()
    };
    let service_probe = service_preflight
        .map(|preflight| service_symphonia_probe(preflight.route))
        .transpose()?;
    let probe = service_probe.as_ref().unwrap_or_else(|| get_probe());
    let mut format = probe
        .probe(&hint, stream, FormatOptions::default(), metadata_options)
        .map_err(|error| format!("{}: probe failed: {error}", path.display()))?;
    let container_format = format.format_info().format;
    if service_preflight.is_some_and(|preflight| {
        audio_container_from_symphonia(container_format).is_none_or(|container| {
            !service_preflight_accepts_container(preflight.route, container)
        })
    }) {
        return Err("controlled Symphonia decode selected a non-preflighted container".into());
    }
    let mut track =
        select_symphonia_audio_track_with_selection(path, format.as_ref(), selection)?.0;
    require_symphonia_sample_rate(path, &track.codec_params)?;
    if let (Some(limit), Some(frames), Some(channels)) = (
        max_packet_samples,
        track.num_frames,
        track.codec_params.channels.as_ref(),
    ) {
        let declared_samples = frames
            .checked_mul(channels.count() as u64)
            .ok_or_else(|| "declared decoded sample count overflow".to_string())?;
        if declared_samples > limit {
            return Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into());
        }
    }
    if let (Some(limit), Some(frames), Some(channels)) = (
        max_packet_samples,
        track.codec_params.max_frames_per_packet,
        track.codec_params.channels.as_ref(),
    ) {
        let maximum_packet_samples = frames
            .checked_mul(channels.count() as u64)
            .ok_or_else(|| "maximum decoded packet sample count overflow".to_string())?;
        if maximum_packet_samples > limit {
            return Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into());
        }
    }
    let mut flac_metadata = FlacMetadataTracker::default();
    let mut flac_channel_mask = flac_metadata.scan(format.as_mut(), &track);
    let decoder_options = symphonia_decoder_options();
    let native_flac = container_format == symphonia::core::formats::well_known::FORMAT_ID_FLAC
        && track.codec_params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;
    let file_bytes = if native_flac && track.num_frames.is_none() {
        std::fs::metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len()
    } else {
        0
    };
    let flac_worker_cap = parallel_flac_worker_cap(&track, file_bytes);
    let parallel_flac = native_flac && flac_worker_cap >= MIN_PARALLEL_FLAC_DECODERS;
    let flac_workers = if parallel_flac {
        forced_flac_workers
            .unwrap_or_else(|| {
                if rayon::current_thread_index().is_none() {
                    rayon::current_num_threads()
                } else {
                    1
                }
            })
            .clamp(1, flac_worker_cap)
    } else {
        1
    };
    if parallel_flac && flac_workers > 1 {
        return decode_native_flac_stream_parallel(
            path,
            format.as_mut(),
            track,
            decoder_options,
            flac_workers,
            flac_metadata,
            selection,
            consume,
        );
    }
    let mut decoder = get_codecs()
        .make_audio_decoder(&track.codec_params, &decoder_options)
        .map_err(|error| format!("{}: unsupported codec: {error}", path.display()))?;
    let mut output_format: Option<SymphoniaOutputFormat> = None;
    let mut info: Option<StreamInfo> = None;
    let mut declared_frames = None;
    let mut planar = Vec::new();
    let mut mpeg_channel_mode = MpegChannelModeTracker::default();

    loop {
        run_decoder_checkpoint::<CONTROLLED, _>(&controlled_checkpoint)?;
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(Error::ResetRequired) => {
                let next_track =
                    select_symphonia_audio_track_with_selection(path, format.as_ref(), selection)?
                        .0;
                require_symphonia_sample_rate(path, &next_track.codec_params)?;
                let next_flac_channel_mask = flac_metadata.scan(format.as_mut(), &next_track);
                let next_source_kind = PcmKind::F32;
                if let Some(output) = output_format.as_ref() {
                    validate_symphonia_track_compatibility(
                        path,
                        output,
                        &next_track.codec_params,
                        next_source_kind,
                        next_flac_channel_mask,
                    )?;
                }
                let next_decoder = get_codecs()
                    .make_audio_decoder(&next_track.codec_params, &decoder_options)
                    .map_err(|error| format!("{}: reinit decoder: {error}", path.display()))?;
                track = next_track;
                flac_channel_mask = next_flac_channel_mask;
                decoder = next_decoder;
                continue;
            }
            Err(error) => return Err(format!("{}: read packet: {error}", path.display())),
        };
        if packet.track_id != track.id {
            continue;
        }
        if CONTROLLED {
            let limit = max_packet_samples.expect("a controlled decode owns a packet limit");
            enforce_symphonia_packet_sample_limit(&track, &packet, limit)?;
        }
        run_decoder_checkpoint::<CONTROLLED, _>(&controlled_checkpoint)?;
        let decoded = require_decoded_packet(decoder.decode(&packet))
            .map_err(|error| format!("{}: decode: {error}", path.display()))?;
        let spec = decoded.spec();
        let decoded_channels = spec.channels().count();
        mpeg_channel_mode.observe_decoded_packet(
            path,
            track.codec_params.codec,
            &packet.data,
            decoded_channels,
        )?;
        if decoded_channels == 0 {
            continue;
        }
        // Every Symphonia codec is handed to this render path as normalized
        // planar f32. The source-name suffix is only a probe hint and must not
        // change the PCM contract or cache result for identical bytes.
        let current_source_kind = PcmKind::F32;
        if let Some(output) = output_format.as_ref() {
            validate_symphonia_decoded_compatibility(path, output, spec, current_source_kind)?;
        } else {
            let mut output = establish_symphonia_output_format_with_mpeg_mode(
                path,
                container_format,
                &track.codec_params,
                spec,
                current_source_kind,
                flac_channel_mask,
                mpeg_channel_mode.observed,
            )?;
            output.layout_provenance =
                mpeg_channel_mode.constrain_provenance(output.layout_provenance);
            output.channel_layout = output
                .channel_layout
                .with_provenance(output.layout_provenance);
            info = Some(StreamInfo {
                sample_rate: output.sample_rate,
                channels: output.channels,
                channel_roles: output.channel_roles.clone(),
                source_kind: output.source_kind,
            });
            declared_frames = track.num_frames;
            output_format = Some(output);
        }
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        if CONTROLLED {
            let limit = max_packet_samples.expect("a controlled decode owns a packet limit");
            let packet_samples = u64::try_from(frames)
                .ok()
                .and_then(|frames| frames.checked_mul(decoded_channels as u64))
                .ok_or_else(|| "decoded packet sample count overflow".to_string())?;
            if packet_samples > limit {
                return Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into());
            }
        }
        run_decoder_checkpoint::<CONTROLLED, _>(&controlled_checkpoint)?;
        decoded.copy_to_vecs_planar::<f32>(&mut planar);
        consume(
            info.as_ref().unwrap(),
            output_format.as_ref().unwrap().layout_provenance,
            declared_frames,
            &mut planar,
        )?;
        run_decoder_checkpoint::<CONTROLLED, _>(&controlled_checkpoint)?;
    }

    info.ok_or_else(|| format!("{}: no audio decoded", path.display()))
}

fn enforce_service_packet_sample_limit<T>(
    info: &StreamInfo,
    planar: &[Vec<T>],
    limit: Option<u64>,
) -> Result<(), String> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let frames = planar.first().map_or(0, Vec::len);
    let packet_samples = u64::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(u64::from(info.channels)))
        .ok_or_else(|| "decoded packet sample count overflow".to_string())?;
    if packet_samples > limit {
        Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into())
    } else {
        Ok(())
    }
}

fn append_symphonia_stream_chunk<F>(
    info: &StreamInfo,
    decoded: &mut [Vec<f32>],
    planar: &mut Vec<Vec<f32>>,
    consume: &mut F,
) -> Result<(), String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    let channels = decoded.len();
    let decoded_frames = decoded.first().map_or(0, Vec::len);
    if decoded
        .iter()
        .any(|channel| channel.len() != decoded_frames)
    {
        return Err("stream channel length mismatch".into());
    }
    if planar.is_empty() {
        planar.resize_with(channels, || {
            Vec::with_capacity(TARGET_SYMPHONIA_STREAM_CHUNK_FRAMES)
        });
    }
    if planar.len() != channels {
        return Err("stream channel count changed".into());
    }
    let buffered_frames = planar.first().map_or(0, Vec::len);
    if planar
        .iter()
        .any(|channel| channel.len() != buffered_frames)
    {
        return Err("stream channel length mismatch".into());
    }
    if decoded_frames >= TARGET_SYMPHONIA_STREAM_CHUNK_FRAMES {
        // Keep an already-large decoder packet intact instead of copying the
        // pending tail into it and growing the reusable allocation needlessly.
        flush_symphonia_stream_chunk(info, planar.as_mut_slice(), consume)?;
        return consume(info, decoded);
    }
    for (destination, source) in planar.iter_mut().zip(decoded.iter()) {
        destination.extend_from_slice(source);
    }
    if planar
        .first()
        .is_some_and(|channel| channel.len() >= TARGET_SYMPHONIA_STREAM_CHUNK_FRAMES)
    {
        consume_and_clear_stream_chunk(info, planar.as_mut_slice(), consume)?;
    }
    Ok(())
}

fn flush_symphonia_stream_chunk<F>(
    info: &StreamInfo,
    planar: &mut [Vec<f32>],
    consume: &mut F,
) -> Result<(), String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    if planar.first().is_some_and(|channel| !channel.is_empty()) {
        consume_and_clear_stream_chunk(info, planar, consume)?;
    }
    Ok(())
}

fn consume_and_clear_stream_chunk<F>(
    info: &StreamInfo,
    planar: &mut [Vec<f32>],
    consume: &mut F,
) -> Result<(), String>
where
    F: FnMut(&StreamInfo, &mut [Vec<f32>]) -> Result<(), String>,
{
    consume(info, planar)?;
    for channel in planar {
        channel.clear();
    }
    Ok(())
}

enum FlacPacketStatus {
    Decoded {
        spec: symphonia::core::audio::AudioSpec,
        frames: usize,
    },
    Error(String),
}

enum FlacDemuxBoundary {
    BatchFull,
    End,
    Reset,
    Error(String),
}

fn parallel_flac_worker_cap(track: &SymphoniaAudioTrack, file_bytes: u64) -> usize {
    let estimated_workers = track.num_frames.map_or_else(
        || file_bytes / FLAC_FILE_BYTES_PER_DECODER,
        |frames| {
            let channels = track
                .codec_params
                .channels
                .as_ref()
                .map_or(1, |channels| channels.count() as u64);
            frames.saturating_mul(channels) / FLAC_SAMPLE_VALUES_PER_DECODER
        },
    );
    usize::try_from(estimated_workers)
        .unwrap_or(MAX_PARALLEL_FLAC_DECODERS)
        .clamp(1, MAX_PARALLEL_FLAC_DECODERS)
}

fn create_parallel_flac_decoders(
    params: &symphonia::core::codecs::audio::AudioCodecParameters,
    options: symphonia::core::codecs::audio::AudioDecoderOptions,
    workers: usize,
) -> Result<Vec<Box<dyn symphonia::core::codecs::audio::AudioDecoder>>, String> {
    use symphonia::default::get_codecs;

    (0..workers)
        .map(|_| {
            get_codecs()
                .make_audio_decoder(params, &options)
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn parallel_flac_batch_limit(workers: usize, max_frames_per_packet: u64, channels: usize) -> usize {
    let pcm_bytes_per_packet = usize::try_from(max_frames_per_packet)
        .unwrap_or(usize::MAX)
        .saturating_mul(channels.max(1))
        .saturating_mul(std::mem::size_of::<f32>())
        .max(1);
    let pcm_limit = (MAX_PARALLEL_FLAC_PCM_BYTES / pcm_bytes_per_packet).max(1);
    workers
        .max(1)
        .saturating_mul(FLAC_PACKETS_PER_DECODER)
        .min(pcm_limit)
        .max(1)
}

fn admit_parallel_flac_packet(
    batch_bytes: usize,
    packet_bytes: usize,
    batch_is_empty: bool,
) -> Result<Option<usize>, &'static str> {
    if packet_bytes > MAX_PARALLEL_FLAC_PACKET_BYTES {
        return Err("FLAC packet exceeds the 32 MiB parallel decode safety limit");
    }
    let next = batch_bytes
        .checked_add(packet_bytes)
        .ok_or("parallel FLAC packet byte count overflow")?;
    if !batch_is_empty && next > MAX_PARALLEL_FLAC_PACKET_BYTES {
        Ok(None)
    } else {
        Ok(Some(next))
    }
}

fn decode_parallel_flac_batch(
    decoders: &mut [Box<dyn symphonia::core::codecs::audio::AudioDecoder>],
    packets: &[symphonia::core::packet::Packet],
    buffers: &mut [Vec<Vec<f32>>],
) -> Vec<FlacPacketStatus> {
    use rayon::prelude::*;
    debug_assert!(!decoders.is_empty());
    debug_assert!(buffers.len() >= packets.len());
    if packets.is_empty() {
        return Vec::new();
    }
    let chunk_size = packets.len().div_ceil(decoders.len());
    decoders
        .par_iter_mut()
        .zip(packets.par_chunks(chunk_size))
        .zip(buffers[..packets.len()].par_chunks_mut(chunk_size))
        .map(|((decoder, packet_chunk), buffer_chunk)| {
            decoder.reset();
            packet_chunk
                .iter()
                .zip(buffer_chunk)
                .map(
                    |(packet, planar)| match require_decoded_packet(decoder.decode(packet)) {
                        Ok(decoded) => {
                            let spec = decoded.spec().clone();
                            let frames = decoded.frames();
                            if frames == 0 {
                                planar.clear();
                            } else {
                                decoded.copy_to_vecs_planar::<f32>(planar);
                            }
                            FlacPacketStatus::Decoded { spec, frames }
                        }
                        Err(error) => {
                            planar.clear();
                            FlacPacketStatus::Error(error)
                        }
                    },
                )
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn decode_native_flac_stream_parallel<F>(
    path: &Path,
    format: &mut dyn symphonia::core::formats::FormatReader,
    mut track: SymphoniaAudioTrack,
    decoder_options: symphonia::core::codecs::audio::AudioDecoderOptions,
    worker_count: usize,
    mut flac_metadata: FlacMetadataTracker,
    selection: AudioTrackSelection,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
{
    use symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC;
    use symphonia::core::errors::Error;

    let mut flac_channel_mask = flac_metadata.current();
    let mut decoders =
        create_parallel_flac_decoders(&track.codec_params, decoder_options, worker_count)
            .map_err(|error| format!("{}: unsupported codec: {error}", path.display()))?;
    let mut batch_limit = parallel_flac_batch_limit(
        decoders.len(),
        decoders[0]
            .codec_params()
            .max_frames_per_packet
            .unwrap_or(65_535),
        decoders[0]
            .codec_params()
            .channels
            .as_ref()
            .map_or(1, |channels| channels.count()),
    );
    let mut packets = Vec::with_capacity(batch_limit);
    let mut pending_packet = None;
    let mut buffers = vec![Vec::new(); batch_limit];
    let mut output_format: Option<SymphoniaOutputFormat> = None;
    let mut info: Option<StreamInfo> = None;
    let mut declared_frames = None;

    loop {
        packets.clear();
        let mut packet_bytes = 0_usize;
        let boundary = loop {
            let packet = if let Some(packet) = pending_packet.take() {
                packet
            } else {
                match format.next_packet() {
                    Ok(Some(packet)) => packet,
                    Ok(None) => break FlacDemuxBoundary::End,
                    Err(Error::ResetRequired) => break FlacDemuxBoundary::Reset,
                    Err(error) => break FlacDemuxBoundary::Error(error.to_string()),
                }
            };
            if packet.track_id != track.id {
                continue;
            }
            packet_bytes = match admit_parallel_flac_packet(
                packet_bytes,
                packet.data.len(),
                packets.is_empty(),
            ) {
                Ok(Some(next)) => next,
                Ok(None) => {
                    pending_packet = Some(packet);
                    break FlacDemuxBoundary::BatchFull;
                }
                Err(error) => break FlacDemuxBoundary::Error(error.into()),
            };
            packets.push(packet);
            if packets.len() == batch_limit {
                break FlacDemuxBoundary::BatchFull;
            }
        };

        if !packets.is_empty() {
            let statuses = decode_parallel_flac_batch(&mut decoders, &packets, &mut buffers);
            for (status, planar) in statuses.into_iter().zip(&mut buffers) {
                match status {
                    FlacPacketStatus::Error(error) => {
                        return Err(format!("{}: decode: {error}", path.display()));
                    }
                    FlacPacketStatus::Decoded { spec, frames } => {
                        let decoded_channels = spec.channels().count();
                        if decoded_channels == 0 {
                            continue;
                        }
                        if let Some(output) = output_format.as_ref() {
                            validate_symphonia_decoded_compatibility(
                                path,
                                output,
                                &spec,
                                PcmKind::F32,
                            )?;
                        } else {
                            let output = establish_symphonia_output_format(
                                path,
                                symphonia::core::formats::well_known::FORMAT_ID_FLAC,
                                &track.codec_params,
                                &spec,
                                PcmKind::F32,
                                flac_channel_mask,
                            )?;
                            info = Some(StreamInfo {
                                sample_rate: output.sample_rate,
                                channels: output.channels,
                                channel_roles: output.channel_roles.clone(),
                                source_kind: output.source_kind,
                            });
                            declared_frames = track.num_frames;
                            output_format = Some(output);
                        }
                        if frames != 0 {
                            consume(
                                info.as_ref().unwrap(),
                                output_format.as_ref().unwrap().layout_provenance,
                                declared_frames,
                                planar,
                            )?;
                        }
                    }
                }
            }
        }

        match boundary {
            FlacDemuxBoundary::BatchFull => {}
            FlacDemuxBoundary::End => break,
            FlacDemuxBoundary::Error(error) => {
                return Err(format!("{}: read packet: {error}", path.display()));
            }
            FlacDemuxBoundary::Reset => {
                let next_track =
                    select_symphonia_audio_track_with_selection(path, format, selection)?.0;
                require_symphonia_sample_rate(path, &next_track.codec_params)?;
                let next_flac_channel_mask = flac_metadata.scan(format, &next_track);
                if next_track.codec_params.codec != CODEC_ID_FLAC {
                    return Err(format!(
                        "{}: codec changed during native FLAC decode",
                        path.display()
                    ));
                }
                if let Some(output) = output_format.as_ref() {
                    validate_symphonia_track_compatibility(
                        path,
                        output,
                        &next_track.codec_params,
                        PcmKind::F32,
                        next_flac_channel_mask,
                    )?;
                }
                track = next_track;
                flac_channel_mask = next_flac_channel_mask;
                decoders = create_parallel_flac_decoders(
                    &track.codec_params,
                    decoder_options,
                    worker_count,
                )
                .map_err(|error| format!("{}: reinit decoder: {error}", path.display()))?;
                batch_limit = parallel_flac_batch_limit(
                    decoders.len(),
                    decoders[0]
                        .codec_params()
                        .max_frames_per_packet
                        .unwrap_or(65_535),
                    decoders[0]
                        .codec_params()
                        .channels
                        .as_ref()
                        .map_or(1, |channels| channels.count()),
                );
                packets = Vec::with_capacity(batch_limit);
                buffers.resize_with(batch_limit, Vec::new);
            }
        }
    }

    info.ok_or_else(|| format!("{}: no audio decoded", path.display()))
}

/// Decode bounded chunks while transferring ownership of each channel buffer
/// to the consumer. The consumer returns an equally sized set of buffers for
/// the decoder to refill, allowing downstream stages to overlap without
/// copying the PCM payload.
pub fn decode_stream_owned<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String>,
{
    decode_stream_owned_with_layout(path, |info, provenance, planar| {
        require_known_layout(path, provenance)?;
        consume(info, planar)
    })
}

/// Ownership-transferring decode with the same exact-duration planning hint as
/// [`decode_stream_with_declared_frames`]. Keeping this crate-private avoids
/// exposing container metadata as part of the public streaming API while the
/// analysis pipeline can still preallocate its bounded PCM spool.
pub(crate) fn decode_stream_owned_with_declared_frames<F>(
    path: &Path,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, Option<u64>, Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String>,
{
    decode_stream_owned_with_layout_and_declared_frames(path, |info, _, declared_frames, planar| {
        consume(info, declared_frames, planar)
    })
}

/// Ownership-transferring counterpart of [`decode_stream_with_layout`].
pub fn decode_stream_owned_with_layout<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String>,
{
    decode_stream_owned_with_layout_and_declared_frames(path, |info, provenance, _, planar| {
        consume(info, provenance, planar)
    })
}

pub(crate) fn decode_stream_owned_with_layout_and_declared_frames<F>(
    path: &Path,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        Vec<Vec<f32>>,
    ) -> Result<Vec<Vec<f32>>, String>,
{
    let mut handoff = Vec::new();
    decode_stream_with_layout_and_declared_frames(
        path,
        |info, provenance, declared_frames, planar| {
            handoff.reserve(planar.len());
            for channel in planar.iter_mut() {
                handoff.push(std::mem::take(channel));
            }
            let mut recycled = consume(
                info,
                provenance,
                declared_frames,
                std::mem::take(&mut handoff),
            )?;
            if recycled.len() != planar.len() {
                return Err(format!(
                    "stream consumer returned {} channels, expected {}",
                    recycled.len(),
                    planar.len()
                ));
            }
            for (slot, channel) in planar.iter_mut().zip(recycled.drain(..)) {
                *slot = channel;
            }
            handoff = recycled;
            Ok(())
        },
    )
}

// WAVE has its own chunk loop, so preserve the same compile-time control split.
fn decode_wav_stream<const CONTROLLED: bool, F, C>(
    path: &Path,
    max_decoded_samples: Option<u64>,
    mut checkpoint: C,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
    C: FnMut() -> Result<(), String>,
{
    if CONTROLLED {
        checkpoint()?;
    }
    let (wav, layout_provenance) = if CONTROLLED {
        WavReader::probe_with_layout_controlled(path, &mut checkpoint)
            .map_err(|error| error.to_string())
    } else {
        WavReader::probe_with_layout(path).map_err(|error| error.to_string())
    }
    .map_err(|error| format!("{}: {error}", path.display()))?;
    let declared_frames =
        wav.data_size / (u64::from(wav.channels) * wav.kind.bytes_per_sample() as u64);
    if let Some(limit) = max_decoded_samples {
        let declared_samples = declared_frames
            .checked_mul(u64::from(wav.channels))
            .ok_or_else(|| "declared WAVE sample count overflow".to_string())?;
        if declared_samples > limit {
            return Err(SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED.into());
        }
    }
    let info = StreamInfo {
        sample_rate: wav.sample_rate,
        channels: wav.channels,
        channel_roles: wav.channel_roles,
        source_kind: wav.kind,
    };
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(wav.data_offset))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let data_size = usize::try_from(wav.data_size).map_err(|_| {
        format!(
            "{}: audio data is too large for this platform",
            path.display()
        )
    })?;

    let frame_bytes = info.channels as usize * info.source_kind.bytes_per_sample();
    let configured_chunk_bytes = wav_stream_chunk_bytes(info.channels, info.source_kind);
    let chunk_bytes = if let Some(limit) = max_decoded_samples {
        let max_frames = limit / u64::from(info.channels);
        let max_frames = usize::try_from(max_frames).unwrap_or(usize::MAX).max(1);
        configured_chunk_bytes.min(max_frames.saturating_mul(frame_bytes).max(frame_bytes))
    } else {
        configured_chunk_bytes
    };
    let mut remaining = data_size;
    let mut bytes = vec![0; chunk_bytes];
    let mut planar = Vec::new();
    while remaining >= frame_bytes {
        if CONTROLLED {
            checkpoint()?;
        }
        let read_size = remaining.min(chunk_bytes);
        let aligned = read_size - read_size % frame_bytes;
        file.read_exact(&mut bytes[..aligned])
            .map_err(|error| format!("{}: {error}", path.display()))?;
        crate::dsp::convert::decode_planar_into(
            &bytes[..aligned],
            info.source_kind,
            info.channels as usize,
            &mut planar,
        );
        consume(&info, layout_provenance, Some(declared_frames), &mut planar)?;
        if CONTROLLED {
            checkpoint()?;
        }
        remaining -= aligned;
    }
    Ok(info)
}

fn wav_stream_chunk_bytes(channels: u16, kind: PcmKind) -> usize {
    debug_assert!(channels > 0);
    let frame_bytes = channels as usize * kind.bytes_per_sample();
    let target = if channels == 1 {
        MONO_WAV_STREAM_CHUNK_BYTES
    } else {
        MULTICHANNEL_WAV_STREAM_CHUNK_BYTES
    };
    (target / frame_bytes).max(1) * frame_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::layouts::{
        CHANNEL_LAYOUT_4P0, CHANNEL_LAYOUT_5P1, CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO,
    };
    use symphonia::core::audio::{AmbisonicBFormat, AudioSpec, ChannelLabel, Channels, Position};
    use symphonia::core::codecs::audio::{AudioCodecId, AudioCodecParameters};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::well_known::{FORMAT_ID_FLAC, FORMAT_ID_ISOMP4, FORMAT_ID_OGG};
    use symphonia::core::meta::well_known::METADATA_ID_FLAC;
    use symphonia::core::meta::{
        MetadataBuilder, MetadataInfo, MetadataLog, MetadataRevision, PerTrackMetadataBuilder,
        RawTag, Tag, METADATA_ID_NULL,
    };

    const TEST_FLAC_METADATA_INFO: MetadataInfo = MetadataInfo {
        metadata: METADATA_ID_FLAC,
        short_name: "flac-test",
        long_name: "FLAC test metadata",
    };
    const TEST_OTHER_METADATA_INFO: MetadataInfo = MetadataInfo {
        metadata: METADATA_ID_NULL,
        short_name: "other-test",
        long_name: "Other test metadata",
    };

    fn mask_tag(key: &str, value: &str) -> Tag {
        Tag::new(RawTag::new(key, value))
    }

    fn mask_state_from_tags(tags: Vec<Tag>) -> FlacChannelMaskState {
        let mut state = FlacChannelMaskState::Absent;
        observe_flac_channel_mask_tags(&mut state, &tags);
        state
    }

    fn metadata_revision(info: MetadataInfo, tags: Vec<Tag>) -> MetadataRevision {
        let mut builder = MetadataBuilder::new(info);
        for tag in tags {
            builder.add_tag(tag);
        }
        builder.build()
    }

    #[test]
    fn packet_decode_errors_are_fail_closed() {
        let error =
            require_decoded_packet::<()>(Err(Error::DecodeError("corrupt packet"))).unwrap_err();
        assert_eq!(error, "malformed stream: corrupt packet");
    }

    fn mpeg_header(codec: AudioCodecId, mode: u8) -> [u8; 4] {
        use symphonia::core::codecs::audio::well_known::{
            CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3,
        };

        assert!(mode < 4);
        let layer = match codec {
            CODEC_ID_MP1 => 0b11,
            CODEC_ID_MP2 => 0b10,
            CODEC_ID_MP3 => 0b01,
            _ => panic!("test requires an MPEG audio codec"),
        };
        [0xff, 0xe0 | (0b11 << 3) | (layer << 1) | 1, 0x90, mode << 6]
    }

    fn silent_mpeg1_layer3_frame(mode: u8) -> Vec<u8> {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;

        // MPEG-1 Layer III, 128 kbit/s, 44.1 kHz, no CRC or padding. A
        // zeroed side-information block has no Huffman data and decodes to
        // silence; the rest of the 417-byte frame is valid ancillary data.
        let mut frame = vec![0_u8; 417];
        frame[..4].copy_from_slice(&mpeg_header(CODEC_ID_MP3, mode));
        frame
    }

    fn silent_mp3_stream(modes: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(417 * modes.len());
        for mode in modes {
            bytes.extend_from_slice(&silent_mpeg1_layer3_frame(*mode));
        }
        bytes
    }

    #[test]
    fn mpeg_channel_mode_parser_distinguishes_dual_channel_for_every_layer() {
        use symphonia::core::codecs::audio::well_known::{
            CODEC_ID_AAC, CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3,
        };

        for codec in [CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3] {
            for (mode, expected) in [
                (0b00, MpegProgrammeMode::StereoLike),
                (0b01, MpegProgrammeMode::StereoLike),
                (0b10, MpegProgrammeMode::DualChannel),
                (0b11, MpegProgrammeMode::Mono),
            ] {
                assert_eq!(
                    mpeg_programme_mode_from_decoded_packet(codec, &mpeg_header(codec, mode)),
                    Ok(Some(expected)),
                    "codec={codec:?}, mode={mode:02b}"
                );
            }
        }
        assert_eq!(
            mpeg_programme_mode_from_decoded_packet(CODEC_ID_AAC, &[]),
            Ok(None)
        );

        let mut protected = mpeg_header(CODEC_ID_MP3, 0b10);
        protected[1] &= !1;
        assert_eq!(
            mpeg_programme_mode_from_decoded_packet(CODEC_ID_MP3, &protected),
            Ok(Some(MpegProgrammeMode::DualChannel))
        );
    }

    #[test]
    fn mpeg_channel_mode_parser_never_searches_for_a_payload_header() {
        use symphonia::core::codecs::audio::well_known::{CODEC_ID_MP2, CODEC_ID_MP3};

        assert!(
            mpeg_programme_mode_from_decoded_packet(CODEC_ID_MP3, &[0xff, 0xfb, 0x90])
                .unwrap_err()
                .contains("shorter")
        );

        let mut leading_junk = vec![0];
        leading_junk.extend_from_slice(&mpeg_header(CODEC_ID_MP3, 0b10));
        assert!(mpeg_programme_mode_from_decoded_packet(CODEC_ID_MP3, &leading_junk).is_err());
        assert!(mpeg_programme_mode_from_decoded_packet(
            CODEC_ID_MP3,
            &mpeg_header(CODEC_ID_MP2, 0b10)
        )
        .is_err());

        let mut free_format = mpeg_header(CODEC_ID_MP3, 0b10);
        free_format[2] &= 0x0f;
        assert!(mpeg_programme_mode_from_decoded_packet(CODEC_ID_MP3, &free_format).is_err());
    }

    #[test]
    fn mpeg_channel_mode_tracker_allows_stereo_coding_changes_only() {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;

        let path = Path::new("fixture.mp3");
        let mut stereo = MpegChannelModeTracker::default();
        stereo
            .observe_decoded_packet(path, CODEC_ID_MP3, &mpeg_header(CODEC_ID_MP3, 0b00), 2)
            .unwrap();
        stereo
            .observe_decoded_packet(path, CODEC_ID_MP3, &mpeg_header(CODEC_ID_MP3, 0b01), 2)
            .unwrap();
        assert_eq!(
            stereo.constrain_provenance(ChannelLayoutProvenance::KnownSpeakers),
            ChannelLayoutProvenance::KnownSpeakers
        );
        let error = stereo
            .observe_decoded_packet(path, CODEC_ID_MP3, &mpeg_header(CODEC_ID_MP3, 0b10), 2)
            .unwrap_err();
        assert!(error.contains("changed from stereo to dual-channel"));

        let mut dual = MpegChannelModeTracker::default();
        for _ in 0..2 {
            dual.observe_decoded_packet(path, CODEC_ID_MP3, &mpeg_header(CODEC_ID_MP3, 0b10), 2)
                .unwrap();
        }
        assert_eq!(
            dual.constrain_provenance(ChannelLayoutProvenance::KnownSpeakers),
            ChannelLayoutProvenance::Unknown
        );
        let error = dual
            .observe_decoded_packet(path, CODEC_ID_MP3, &mpeg_header(CODEC_ID_MP3, 0b00), 2)
            .unwrap_err();
        assert!(error.contains("changed from dual-channel to stereo"));
    }

    #[test]
    fn decoded_mpeg_mono_mode_resolves_symphonia_left_center_alias() {
        use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
        use symphonia::core::formats::well_known::FORMAT_ID_MP3;

        let path = Path::new("mono.mp3");
        let params = codec_params_for_codec(
            44_100,
            Channels::Positioned(Position::FRONT_LEFT),
            CODEC_ID_MP3,
            None,
        );
        let decoded = AudioSpec::new(44_100, Channels::Positioned(Position::FRONT_CENTER));

        assert!(establish_symphonia_output_format(
            path,
            FORMAT_ID_MP3,
            &params,
            &decoded,
            PcmKind::F32,
            FlacChannelMaskState::Absent,
        )
        .is_err());
        let output = establish_symphonia_output_format_with_mpeg_mode(
            path,
            FORMAT_ID_MP3,
            &params,
            &decoded,
            PcmKind::F32,
            FlacChannelMaskState::Absent,
            Some(MpegProgrammeMode::Mono),
        )
        .unwrap();
        assert_eq!(output.channel_roles, default_channel_roles(1));
        assert_eq!(
            output.layout_provenance,
            ChannelLayoutProvenance::KnownSpeakers
        );
    }

    #[test]
    fn raw_mp3_dual_channel_is_unknown_across_decode_routes_and_path_hints() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = silent_mp3_stream(&[0b10, 0b10, 0b10]);
        let mut paths = Vec::new();
        for name in ["dual.mp3", "dual", "dual.audio"] {
            let path = directory.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            let (decoded, provenance) = decode_with_layout(&path).unwrap();
            assert_eq!(decoded.channels, 2, "path={}", path.display());
            assert_eq!(decoded.frames, 3 * 1_152, "path={}", path.display());
            assert_eq!(
                provenance,
                ChannelLayoutProvenance::Unknown,
                "path={}",
                path.display()
            );
            let error = crate::normalize::resolve_decoded_channel_roles(
                &path,
                decoded.channels,
                &decoded.channel_roles,
                provenance,
                None,
            )
            .unwrap_err();
            assert!(error.contains("ambiguous 2-channel layout"));
            assert_eq!(
                crate::normalize::resolve_decoded_channel_roles(
                    &path,
                    decoded.channels,
                    &decoded.channel_roles,
                    provenance,
                    Some(&default_channel_roles(2)),
                )
                .unwrap(),
                default_channel_roles(2)
            );
            paths.push(path);
        }

        let mut serial_frames = 0;
        let serial_info = decode_stream_with_layout(&paths[0], |_, provenance, planar| {
            assert_eq!(provenance, ChannelLayoutProvenance::Unknown);
            serial_frames += planar[0].len();
            Ok(())
        })
        .unwrap();
        assert_eq!(serial_info.channels, 2);
        assert_eq!(serial_frames, 3 * 1_152);

        let mut owned_frames = 0;
        let owned_info = decode_stream_owned_with_layout(&paths[0], |_, provenance, mut planar| {
            assert_eq!(provenance, ChannelLayoutProvenance::Unknown);
            owned_frames += planar[0].len();
            for channel in &mut planar {
                channel.clear();
            }
            Ok(planar)
        })
        .unwrap();
        assert_eq!(owned_info.channels, 2);
        assert_eq!(owned_frames, serial_frames);
    }

    #[test]
    fn raw_mp3_stereo_and_joint_stereo_remain_known() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stereo.mp3");
        std::fs::write(&path, silent_mp3_stream(&[0b00, 0b01, 0b00])).unwrap();

        let (decoded, provenance) = decode_with_layout(&path).unwrap();
        assert_eq!(decoded.frames, 3 * 1_152);
        assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
        let mut callbacks = 0;
        decode_stream_with_layout(&path, |_, provenance, _| {
            callbacks += 1;
            assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
            Ok(())
        })
        .unwrap();
        assert_eq!(callbacks, 3);
    }

    #[test]
    fn raw_mp3_channel_semantics_change_fails_before_changed_pcm_is_published() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mode-change.mp3");
        std::fs::write(&path, silent_mp3_stream(&[0b00, 0b00, 0b10])).unwrap();

        let full_error = decode_with_layout(&path).unwrap_err();
        assert!(full_error.contains("changed from stereo to dual-channel"));

        let mut callbacks = 0;
        let mut published_frames = 0;
        let stream_error = decode_stream_with_layout(&path, |_, _, planar| {
            callbacks += 1;
            published_frames += planar[0].len();
            Ok(())
        })
        .unwrap_err();
        assert!(stream_error.contains("changed from stereo to dual-channel"));
        assert_eq!(callbacks, 2);
        assert_eq!(published_frames, 2 * 1_152);
    }

    #[test]
    fn raw_mp3_id3_and_info_headers_do_not_hide_dual_channel_audio() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tagged-dual.mp3");
        let mut bytes = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        let mut info = silent_mpeg1_layer3_frame(0b00);
        info[36..40].copy_from_slice(b"Info");
        bytes.extend_from_slice(&info);
        bytes.extend_from_slice(&silent_mp3_stream(&[0b10, 0b10, 0b10]));
        std::fs::write(&path, bytes).unwrap();

        let (decoded, provenance) = decode_with_layout(&path).unwrap();
        assert_eq!(decoded.frames, 3 * 1_152);
        assert_eq!(provenance, ChannelLayoutProvenance::Unknown);
    }

    fn output_format() -> SymphoniaOutputFormat {
        SymphoniaOutputFormat {
            sample_rate: 48_000,
            channels: 2,
            decoded_layout: CHANNEL_LAYOUT_STEREO.clone(),
            declared_layout: Some(CHANNEL_LAYOUT_STEREO.clone()),
            channel_roles: default_channel_roles(2),
            layout_provenance: ChannelLayoutProvenance::KnownSpeakers,
            channel_layout: channel_layout_from_symphonia(
                &CHANNEL_LAYOUT_STEREO,
                ChannelLayoutProvenance::KnownSpeakers,
            ),
            flac_channel_mask: FlacChannelMaskState::Absent,
            source_kind: PcmKind::F32,
        }
    }

    #[test]
    fn flac_channel_mask_parser_is_exact_case_insensitive_and_zero_pad_safe() {
        assert_eq!(parse_flac_channel_mask("0x3"), Some(0x3));
        assert_eq!(parse_flac_channel_mask("0XfF"), Some(0xff));
        assert_eq!(
            parse_flac_channel_mask("0x000000000000000000005003"),
            Some(0x5003)
        );
        assert_eq!(parse_flac_channel_mask("0x000000000000"), Some(0));

        for malformed in [
            "",
            "0",
            "0x",
            " 0x3",
            "0x3 ",
            "+0x3",
            "-0x3",
            "0x3g",
            "0x1_0",
            "0x100000000",
        ] {
            assert_eq!(
                parse_flac_channel_mask(malformed),
                None,
                "value={malformed:?}"
            );
        }
    }

    #[test]
    fn flac_channel_mask_tags_accept_case_and_identical_duplicates() {
        let state = mask_state_from_tags(vec![
            mask_tag("waveformatextensible_channel_mask", "0X00000003"),
            mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x3"),
        ]);
        assert_eq!(state, FlacChannelMaskState::Valid(0x3));
    }

    #[test]
    fn flac_channel_mask_tags_reject_conflicts_malformed_and_non_strings() {
        assert_eq!(
            mask_state_from_tags(vec![
                mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x3"),
                mask_tag("waveformatextensible_channel_mask", "0x4"),
            ]),
            FlacChannelMaskState::Invalid
        );

        for tag in [
            mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x"),
            Tag::new(RawTag::new("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", 3_u64)),
        ] {
            assert_eq!(
                mask_state_from_tags(vec![tag]),
                FlacChannelMaskState::Invalid
            );
        }
    }

    #[test]
    fn flac_channel_mask_revision_filtering_and_attribution_fail_closed() {
        let mut ignored = MetadataBuilder::new(TEST_OTHER_METADATA_INFO);
        ignored.add_tag(mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x4"));
        let mut state = FlacChannelMaskState::Absent;
        observe_flac_channel_mask_revision(&mut state, &ignored.build(), 7, true);
        assert_eq!(state, FlacChannelMaskState::Absent);

        let mut ambiguous = MetadataBuilder::new(TEST_FLAC_METADATA_INFO);
        ambiguous.add_tag(mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x3"));
        observe_flac_channel_mask_revision(&mut state, &ambiguous.build(), 7, false);
        assert_eq!(state, FlacChannelMaskState::Invalid);

        let mut attributed = MetadataBuilder::new(TEST_FLAC_METADATA_INFO);
        let mut other_track = PerTrackMetadataBuilder::new(8);
        other_track.add_tag(mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x4"));
        attributed.add_track(other_track.build());
        let mut selected_track = PerTrackMetadataBuilder::new(7);
        selected_track.add_tag(mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x3"));
        attributed.add_track(selected_track.build());
        let mut state = FlacChannelMaskState::Absent;
        observe_flac_channel_mask_revision(&mut state, &attributed.build(), 7, false);
        assert_eq!(state, FlacChannelMaskState::Valid(0x3));
    }

    #[test]
    fn flac_metadata_tracker_separates_appended_ogg_revision_groups() {
        let mut log = MetadataLog::default();
        log.push(metadata_revision(
            TEST_FLAC_METADATA_INFO,
            vec![mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x3")],
        ));
        log.push(metadata_revision(
            TEST_FLAC_METADATA_INFO,
            vec![mask_tag("waveformatextensible_channel_mask", "0X0003")],
        ));
        let mut tracker = FlacMetadataTracker::default();
        assert_eq!(
            tracker.scan_revisions(log.metadata(), 1, true, true),
            FlacChannelMaskState::Valid(0x3)
        );

        // Symphonia retains the previous newest revision, then appends the
        // next physical Ogg stream's FLAC comment and picture revisions.
        log.push(metadata_revision(
            TEST_FLAC_METADATA_INFO,
            vec![mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x4")],
        ));
        log.push(metadata_revision(TEST_FLAC_METADATA_INFO, Vec::new()));
        assert_eq!(
            tracker.scan_revisions(log.metadata(), 2, true, true),
            FlacChannelMaskState::Valid(0x4)
        );
        assert_eq!(
            tracker.scan_revisions(log.metadata(), 2, true, true),
            FlacChannelMaskState::Absent
        );

        // A stream with no revision leaves no cursor to skip. The first
        // revision appended by a later chained stream must still be scanned.
        let mut initially_empty = MetadataLog::default();
        let mut tracker = FlacMetadataTracker::default();
        assert_eq!(
            tracker.scan_revisions(initially_empty.metadata(), 3, true, true),
            FlacChannelMaskState::Absent
        );
        initially_empty.push(metadata_revision(
            TEST_FLAC_METADATA_INFO,
            vec![mask_tag("WAVEFORMATEXTENSIBLE_CHANNEL_MASK", "0x7")],
        ));
        assert_eq!(
            tracker.scan_revisions(initially_empty.metadata(), 4, true, true),
            FlacChannelMaskState::Valid(0x7)
        );
    }

    #[test]
    fn complete_rfc_flac_masks_have_known_speakers() {
        use ChannelLayoutProvenance::{KnownSpeakers, Unknown};

        let path = Path::new("fixture.flac");
        let cases = [
            (
                CHANNEL_LAYOUT_STEREO.clone(),
                FlacChannelMaskState::Absent,
                KnownSpeakers,
            ),
            (
                CHANNEL_LAYOUT_STEREO.clone(),
                FlacChannelMaskState::Valid(0x0003),
                KnownSpeakers,
            ),
            (
                CHANNEL_LAYOUT_MONO.clone(),
                FlacChannelMaskState::Valid(0x0008),
                KnownSpeakers,
            ),
            (
                Channels::Positioned(
                    Position::FRONT_LEFT
                        | Position::FRONT_RIGHT
                        | Position::REAR_LEFT
                        | Position::REAR_RIGHT,
                ),
                FlacChannelMaskState::Valid(0x0000_5003),
                KnownSpeakers,
            ),
            (
                Channels::Positioned(
                    Position::FRONT_LEFT
                        | Position::FRONT_RIGHT
                        | Position::REAR_LEFT
                        | Position::REAR_RIGHT,
                ),
                FlacChannelMaskState::Valid(0x0003),
                Unknown,
            ),
            (
                CHANNEL_LAYOUT_MONO.clone(),
                FlacChannelMaskState::Valid(1 << 18),
                Unknown,
            ),
            (
                CHANNEL_LAYOUT_STEREO.clone(),
                FlacChannelMaskState::Valid(0),
                Unknown,
            ),
            (
                CHANNEL_LAYOUT_STEREO.clone(),
                FlacChannelMaskState::Invalid,
                Unknown,
            ),
        ];

        for (layout, mask, expected) in cases {
            let params = codec_params(48_000, layout.clone());
            let spec = AudioSpec::new(48_000, layout);
            let output = establish_symphonia_output_format(
                path,
                FORMAT_ID_FLAC,
                &params,
                &spec,
                PcmKind::F32,
                mask,
            )
            .unwrap();
            assert_eq!(output.layout_provenance, expected, "mask={mask:?}");
        }
    }

    #[test]
    fn rfc_default_flac_channel_masks_cover_every_supported_count() {
        assert_eq!(
            (1..=8)
                .map(|channels| default_flac_channel_mask(channels).unwrap())
                .collect::<Vec<_>>(),
            [0x0004, 0x0003, 0x0007, 0x0033, 0x0037, 0x003f, 0x070f, 0x063f]
        );
        assert_eq!(default_flac_channel_mask(0), None);
        assert_eq!(default_flac_channel_mask(9), None);
    }

    #[test]
    fn symphonia_layout_provenance_table_is_fail_closed() {
        use ChannelLayoutProvenance::{KnownSpeakers, SceneBased, Unknown};

        let cases = vec![
            (
                Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                KnownSpeakers,
            ),
            (Channels::Positioned(Position::LFE2), KnownSpeakers),
            (Channels::Positioned(Position::TOP_SIDE_LEFT), Unknown),
            (Channels::Discrete(2), Unknown),
            (Channels::None, Unknown),
            (Channels::Ambisonic(1), SceneBased),
            (
                Channels::Custom(
                    vec![
                        ChannelLabel::Positioned(Position::FRONT_LEFT),
                        ChannelLabel::Positioned(Position::FRONT_RIGHT),
                    ]
                    .into_boxed_slice(),
                ),
                KnownSpeakers,
            ),
            (
                Channels::Custom(
                    vec![ChannelLabel::Ambisonic(0), ChannelLabel::Ambisonic(1)].into_boxed_slice(),
                ),
                SceneBased,
            ),
            (
                Channels::Custom(
                    vec![
                        ChannelLabel::AmbisonicBFormat(AmbisonicBFormat::W),
                        ChannelLabel::AmbisonicBFormat(AmbisonicBFormat::X),
                    ]
                    .into_boxed_slice(),
                ),
                SceneBased,
            ),
            (
                Channels::Custom(
                    vec![
                        ChannelLabel::Positioned(Position::FRONT_LEFT),
                        ChannelLabel::Discrete(1),
                    ]
                    .into_boxed_slice(),
                ),
                Unknown,
            ),
            (
                Channels::Custom(
                    vec![
                        ChannelLabel::Positioned(Position::FRONT_LEFT),
                        ChannelLabel::Positioned(Position::FRONT_LEFT),
                    ]
                    .into_boxed_slice(),
                ),
                Unknown,
            ),
        ];

        for (layout, expected) in cases {
            assert_eq!(
                layout_provenance_from_symphonia(&layout),
                expected,
                "layout={layout}"
            );
        }
    }

    #[test]
    fn symphonia_known_speakers_retain_positions_outside_mono_and_stereo() {
        let front_left_and_center =
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_CENTER);
        assert_eq!(
            roles_from_symphonia(&front_left_and_center),
            [
                ChannelRole::positioned(-30, 0),
                ChannelRole::positioned(0, 0),
            ]
        );
        assert_ne!(
            roles_from_symphonia(&front_left_and_center),
            default_channel_roles(2)
        );

        let rear_center = Channels::Positioned(Position::REAR_CENTER);
        let rear_center_roles = roles_from_symphonia(&rear_center);
        assert_eq!(rear_center_roles, [ChannelRole::positioned(180, 0)]);
        assert_eq!(crate::dsp::lufs::channel_weight(rear_center_roles[0]), 1.0);

        assert_eq!(
            roles_from_symphonia(&CHANNEL_LAYOUT_MONO),
            default_channel_roles(1)
        );
        assert_eq!(
            roles_from_symphonia(&CHANNEL_LAYOUT_STEREO),
            default_channel_roles(2)
        );
    }

    #[test]
    fn symphonia_five_one_keeps_cicp_bed_identity_and_compatibility_roles_aligned() {
        let layout = channel_layout_from_symphonia(
            &CHANNEL_LAYOUT_5P1,
            ChannelLayoutProvenance::KnownSpeakers,
        );
        layout.validate().unwrap();
        assert_eq!(
            layout.channel_roles(),
            roles_from_symphonia(&CHANNEL_LAYOUT_5P1)
        );
        assert_eq!(
            layout
                .assignments()
                .iter()
                .map(ChannelAssignment::cicp_position)
                .collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
    }

    #[test]
    fn symphonia_output_keeps_layout_provenance_sidecar() {
        use ChannelLayoutProvenance::{KnownSpeakers, SceneBased, Unknown};

        let path = Path::new("fixture.audio");
        let cases = [
            (CHANNEL_LAYOUT_STEREO.clone(), KnownSpeakers),
            (Channels::Discrete(2), Unknown),
            (Channels::Ambisonic(1), SceneBased),
        ];
        for (layout, expected) in cases {
            let params = codec_params(48_000, layout.clone());
            let spec = AudioSpec::new(48_000, layout);
            let output = establish_symphonia_output_format(
                path,
                FORMAT_ID_OGG,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .unwrap();
            assert_eq!(output.layout_provenance, expected);
        }
    }

    #[test]
    fn symphonia_first_packet_rejects_conflicting_positioned_layout_before_callback() {
        let path = Path::new("fixture.audio");
        let params = codec_params(
            48_000,
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
        );
        let spec = AudioSpec::new(
            48_000,
            Channels::Positioned(Position::FRONT_CENTER | Position::LFE1),
        );
        let mut callbacks = 0;

        let error = establish_symphonia_output_format(
            path,
            FORMAT_ID_OGG,
            &params,
            &spec,
            PcmKind::F32,
            FlacChannelMaskState::Absent,
        )
        .map(|_| callbacks += 1)
        .unwrap_err();

        assert!(error.contains("decoded channel layout"));
        assert!(error.contains("does not match track channel layout"));
        assert_eq!(callbacks, 0);
    }

    #[test]
    fn symphonia_layout_reconciliation_is_conservative() {
        use ChannelLayoutProvenance::{KnownSpeakers, Unknown};

        let path = Path::new("fixture.audio");
        let cases = [
            (
                Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                KnownSpeakers,
            ),
            (
                Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                Channels::Discrete(2),
                Unknown,
            ),
            (
                Channels::Discrete(2),
                Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                Unknown,
            ),
            (Channels::Discrete(2), Channels::Discrete(2), Unknown),
        ];

        for (declared, decoded, expected) in cases {
            assert_eq!(
                reconcile_symphonia_layouts(path, Some(&declared), &decoded).unwrap(),
                expected,
                "declared={declared}, decoded={decoded}"
            );
        }
        assert_eq!(
            reconcile_symphonia_layouts(path, None, &CHANNEL_LAYOUT_STEREO).unwrap(),
            KnownSpeakers
        );
    }

    #[test]
    fn symphonia_container_and_codec_placeholders_are_unknown() {
        use symphonia::core::codecs::audio::well_known::{
            CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_PCM_S16LE,
        };
        use ChannelLayoutProvenance::{KnownSpeakers, Unknown};

        let path = Path::new("fixture.audio");
        for layout in [
            CHANNEL_LAYOUT_MONO.clone(),
            CHANNEL_LAYOUT_STEREO.clone(),
            CHANNEL_LAYOUT_4P0.clone(),
        ] {
            let channels = layout.count();
            let params = codec_params_for_codec(48_000, layout.clone(), CODEC_ID_PCM_S16LE, None);
            let spec = AudioSpec::new(48_000, layout);
            let output = establish_symphonia_output_format(
                path,
                FORMAT_ID_ISOMP4,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .unwrap();
            assert_eq!(output.layout_provenance, Unknown, "channels={channels}");
        }

        let params =
            codec_params_for_codec(48_000, CHANNEL_LAYOUT_STEREO.clone(), CODEC_ID_FLAC, None);
        let spec = AudioSpec::new(48_000, CHANNEL_LAYOUT_STEREO.clone());
        assert_eq!(
            establish_symphonia_output_format(
                path,
                FORMAT_ID_ISOMP4,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .unwrap()
            .layout_provenance,
            Unknown
        );
        assert_eq!(
            establish_symphonia_output_format(
                path,
                FORMAT_ID_ISOMP4,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Valid(0x0003),
            )
            .unwrap()
            .layout_provenance,
            Unknown
        );

        let alac_cases = [
            (CHANNEL_LAYOUT_5P1.clone(), Some(24), Unknown),
            (CHANNEL_LAYOUT_STEREO.clone(), Some(24), KnownSpeakers),
            (CHANNEL_LAYOUT_5P1.clone(), Some(48), KnownSpeakers),
        ];
        for (layout, extra_data_len, expected) in alac_cases {
            let channels = layout.count();
            let params =
                codec_params_for_codec(48_000, layout.clone(), CODEC_ID_ALAC, extra_data_len);
            let spec = AudioSpec::new(48_000, layout);
            let output = establish_symphonia_output_format(
                path,
                FORMAT_ID_ISOMP4,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .unwrap();
            assert_eq!(
                output.layout_provenance, expected,
                "ALAC channels={channels}, extra_data_len={extra_data_len:?}"
            );
        }

        let aac = codec_params_for_codec(48_000, CHANNEL_LAYOUT_STEREO.clone(), CODEC_ID_AAC, None);
        let stereo_spec = AudioSpec::new(48_000, CHANNEL_LAYOUT_STEREO.clone());
        assert_eq!(
            establish_symphonia_output_format(
                path,
                FORMAT_ID_ISOMP4,
                &aac,
                &stereo_spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .unwrap()
            .layout_provenance,
            KnownSpeakers
        );

        let native_flac =
            codec_params_for_codec(48_000, CHANNEL_LAYOUT_STEREO.clone(), CODEC_ID_FLAC, None);
        assert_eq!(
            establish_symphonia_output_format(
                path,
                FORMAT_ID_FLAC,
                &native_flac,
                &stereo_spec,
                PcmKind::F32,
                FlacChannelMaskState::Valid(0x3),
            )
            .unwrap()
            .layout_provenance,
            KnownSpeakers
        );
    }

    #[test]
    fn symphonia_pcm_codec_allowlist_covers_every_well_known_pcm_codec() {
        use symphonia::core::codecs::audio::well_known::*;

        let pcm_codecs = [
            CODEC_ID_PCM_S32LE,
            CODEC_ID_PCM_S32LE_PLANAR,
            CODEC_ID_PCM_S32BE,
            CODEC_ID_PCM_S32BE_PLANAR,
            CODEC_ID_PCM_S24LE,
            CODEC_ID_PCM_S24LE_PLANAR,
            CODEC_ID_PCM_S24BE,
            CODEC_ID_PCM_S24BE_PLANAR,
            CODEC_ID_PCM_S16LE,
            CODEC_ID_PCM_S16LE_PLANAR,
            CODEC_ID_PCM_S16BE,
            CODEC_ID_PCM_S16BE_PLANAR,
            CODEC_ID_PCM_S8,
            CODEC_ID_PCM_S8_PLANAR,
            CODEC_ID_PCM_U32LE,
            CODEC_ID_PCM_U32LE_PLANAR,
            CODEC_ID_PCM_U32BE,
            CODEC_ID_PCM_U32BE_PLANAR,
            CODEC_ID_PCM_U24LE,
            CODEC_ID_PCM_U24LE_PLANAR,
            CODEC_ID_PCM_U24BE,
            CODEC_ID_PCM_U24BE_PLANAR,
            CODEC_ID_PCM_U16LE,
            CODEC_ID_PCM_U16LE_PLANAR,
            CODEC_ID_PCM_U16BE,
            CODEC_ID_PCM_U16BE_PLANAR,
            CODEC_ID_PCM_U8,
            CODEC_ID_PCM_U8_PLANAR,
            CODEC_ID_PCM_F32LE,
            CODEC_ID_PCM_F32LE_PLANAR,
            CODEC_ID_PCM_F32BE,
            CODEC_ID_PCM_F32BE_PLANAR,
            CODEC_ID_PCM_F64LE,
            CODEC_ID_PCM_F64LE_PLANAR,
            CODEC_ID_PCM_F64BE,
            CODEC_ID_PCM_F64BE_PLANAR,
            CODEC_ID_PCM_ALAW,
            CODEC_ID_PCM_MULAW,
        ];
        assert_eq!(pcm_codecs.len(), 38);

        for codec in pcm_codecs {
            assert!(is_symphonia_pcm_codec(codec), "codec={codec}");
            let params = codec_params_for_codec(48_000, CHANNEL_LAYOUT_STEREO.clone(), codec, None);
            assert_eq!(
                constrain_symphonia_layout_provenance(
                    ChannelLayoutProvenance::KnownSpeakers,
                    FORMAT_ID_ISOMP4,
                    &params,
                    2,
                ),
                ChannelLayoutProvenance::Unknown,
                "codec={codec}"
            );
        }
        assert!(!is_symphonia_pcm_codec(CODEC_ID_AAC));
    }

    #[test]
    fn symphonia_sample_rate_bounds_are_checked_before_pcm_handoff() {
        let path = Path::new("fixture.audio");

        for sample_rate in [
            0,
            MIN_DECODE_SAMPLE_RATE_HZ - 1,
            MAX_DECODE_SAMPLE_RATE_HZ + 1,
        ] {
            let params = codec_params(sample_rate, CHANNEL_LAYOUT_STEREO.clone());
            let spec = AudioSpec::new(sample_rate, CHANNEL_LAYOUT_STEREO.clone());
            let mut callbacks = 0;
            let error = establish_symphonia_output_format(
                path,
                FORMAT_ID_OGG,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .map(|_| callbacks += 1)
            .unwrap_err();
            assert!(error.contains("sample rate"));
            assert!(error.contains("outside the supported"));
            assert_eq!(callbacks, 0);
        }

        for sample_rate in [MIN_DECODE_SAMPLE_RATE_HZ, MAX_DECODE_SAMPLE_RATE_HZ] {
            let params = codec_params(sample_rate, CHANNEL_LAYOUT_STEREO.clone());
            let spec = AudioSpec::new(sample_rate, CHANNEL_LAYOUT_STEREO.clone());
            assert!(establish_symphonia_output_format(
                path,
                FORMAT_ID_OGG,
                &params,
                &spec,
                PcmKind::F32,
                FlacChannelMaskState::Absent,
            )
            .is_ok());
        }
    }

    fn riff_wave(chunks: impl IntoIterator<Item = ([u8; 4], Vec<u8>)>) -> Vec<u8> {
        let mut wave = b"RIFF\0\0\0\0WAVE".to_vec();
        for (id, body) in chunks {
            wave.extend_from_slice(&id);
            wave.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
            wave.extend_from_slice(&body);
            if body.len() & 1 != 0 {
                wave.push(0);
            }
        }
        let riff_size = u32::try_from(wave.len() - 8).unwrap();
        wave[4..8].copy_from_slice(&riff_size.to_le_bytes());
        wave
    }

    fn exact_pcm_wave(kind: PcmKind, channels: u16, data: Vec<u8>) -> Vec<u8> {
        let format_tag = if kind.is_float() { 3_u16 } else { 1_u16 };
        let sample_rate = 48_000_u32;
        let frame_bytes = channels * kind.bytes_per_sample() as u16;
        let mut format = Vec::new();
        format.extend_from_slice(&format_tag.to_le_bytes());
        format.extend_from_slice(&channels.to_le_bytes());
        format.extend_from_slice(&sample_rate.to_le_bytes());
        format.extend_from_slice(&(sample_rate * u32::from(frame_bytes)).to_le_bytes());
        format.extend_from_slice(&frame_bytes.to_le_bytes());
        format.extend_from_slice(&kind.bits_per_sample().to_le_bytes());
        riff_wave([(*b"fmt ", format), (*b"data", data)])
    }

    fn exact_pcm_wave_with_junk_chunks(
        kind: PcmKind,
        channels: u16,
        junk_chunks: usize,
        data: Vec<u8>,
    ) -> Vec<u8> {
        let format_tag = if kind.is_float() { 3_u16 } else { 1_u16 };
        let sample_rate = 48_000_u32;
        let frame_bytes = channels * kind.bytes_per_sample() as u16;
        let mut format = Vec::new();
        format.extend_from_slice(&format_tag.to_le_bytes());
        format.extend_from_slice(&channels.to_le_bytes());
        format.extend_from_slice(&sample_rate.to_le_bytes());
        format.extend_from_slice(&(sample_rate * u32::from(frame_bytes)).to_le_bytes());
        format.extend_from_slice(&frame_bytes.to_le_bytes());
        format.extend_from_slice(&kind.bits_per_sample().to_le_bytes());
        riff_wave(
            std::iter::once((*b"fmt ", format))
                .chain((0..junk_chunks).map(|_| (*b"JUNK", Vec::new())))
                .chain(std::iter::once((*b"data", data))),
        )
    }

    #[test]
    fn descriptor_analysis_stream_preserves_exact_wave_values_and_range() {
        let directory = tempfile::tempdir().unwrap();
        let stable_options = StableInputOptions::new(1024 * 1024).unwrap();

        let s32_path = directory.path().join("exact-s32.wav");
        let s32_source = [
            1_073_741_823_i32,
            1_073_741_824,
            1_073_741_825,
            1_073_741_826,
        ];
        let s32_bytes = s32_source
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        std::fs::write(&s32_path, exact_pcm_wave(PcmKind::S32, 1, s32_bytes)).unwrap();
        let descriptor = InputDescriptor::from_path(
            &s32_path,
            &stable_options,
            InputDescriptorOptions::default().with_time_range(1.0 / 48_000.0, Some(2.0 / 48_000.0)),
        )
        .unwrap();
        let mut decoded_s32 = Vec::new();
        decode_descriptor_analysis_stream(&descriptor, |_, _, chunk| {
            let AnalysisPcmChunk::S32(planar) = chunk else {
                panic!("S32 WAVE must use the exact analysis lane");
            };
            decoded_s32.extend_from_slice(&planar[0]);
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded_s32, s32_source[1..3]);
        assert_eq!(
            decoded_s32[0] as f32, decoded_s32[1] as f32,
            "the exact lane must retain codes that normalized f32 cannot distinguish"
        );

        let f64_path = directory.path().join("exact-f64.wav");
        let f64_source = [0.5_f64, f64::from_bits(0x3fe0_0000_0000_0001), -0.0];
        let f64_bytes = f64_source
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        std::fs::write(&f64_path, exact_pcm_wave(PcmKind::F64, 1, f64_bytes)).unwrap();
        let descriptor = InputDescriptor::from_path(
            &f64_path,
            &stable_options,
            InputDescriptorOptions::default(),
        )
        .unwrap();
        let mut decoded_f64 = Vec::new();
        decode_descriptor_analysis_stream(&descriptor, |_, _, chunk| {
            let AnalysisPcmChunk::F64(planar) = chunk else {
                panic!("F64 WAVE must use the exact analysis lane");
            };
            decoded_f64.extend(planar[0].iter().map(|sample| sample.to_bits()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            decoded_f64,
            f64_source
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn controlled_descriptor_decode_checks_before_pcm_chunk_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controlled-s32.wav");
        let samples = (0_i32..8_192)
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        std::fs::write(&path, exact_pcm_wave(PcmKind::S32, 1, samples)).unwrap();
        let descriptor = InputDescriptor::from_path(
            &path,
            &StableInputOptions::new(1024 * 1024).unwrap(),
            InputDescriptorOptions::default(),
        )
        .unwrap();
        let mut checkpoints = 0;
        let mut callbacks = 0;
        let error = decode_descriptor_analysis_stream_with_control(
            &descriptor,
            8_192,
            || {
                checkpoints += 1;
                if checkpoints == 2 {
                    Err("cancelled before WAVE chunk".into())
                } else {
                    Ok(())
                }
            },
            |_, _, _| {
                callbacks += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.ends_with("cancelled before WAVE chunk"));
        assert_eq!(callbacks, 0);

        let f32_path = directory.path().join("controlled-f32.wav");
        std::fs::write(
            &f32_path,
            pcm16_wave_with_layout_and_frames(48_000, 2, None, 8_192),
        )
        .unwrap();
        let descriptor = InputDescriptor::from_path(
            &f32_path,
            &StableInputOptions::new(1024 * 1024).unwrap(),
            InputDescriptorOptions::default(),
        )
        .unwrap();
        let mut callbacks = 0;
        let error = decode_descriptor_analysis_stream_with_control(
            &descriptor,
            16_383,
            || Ok(()),
            |_, _, _| {
                callbacks += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED);
        assert_eq!(callbacks, 0);
    }

    #[test]
    fn controlled_wave_probe_checks_during_maximum_chunk_scan_and_exact_reprobe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("many-junk-chunks.wav");
        // fmt + 99,998 JUNK + data reaches the parser's 100,000-chunk
        // acceptance boundary without crossing it.
        let wave =
            exact_pcm_wave_with_junk_chunks(PcmKind::S32, 1, 99_998, 1_i32.to_le_bytes().to_vec());
        std::fs::write(&path, wave).unwrap();
        let input =
            StableInput::from_path(&path, &StableInputOptions::new(1024 * 1024).unwrap()).unwrap();

        let mut probe_checkpoints = 0;
        let error = InputDescriptor::probe_with_control(
            input.clone(),
            InputDescriptorOptions::default(),
            1,
            || {
                probe_checkpoints += 1;
                if probe_checkpoints == 8 {
                    Err("cancelled during WAVE chunk-table probe".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert!(error.ends_with("cancelled during WAVE chunk-table probe"));

        // A regular probe traverses the complete boundary-sized table. The
        // controlled high-precision S32 decode then performs its required
        // second probe with the same bounded checkpoints.
        let descriptor = InputDescriptor::probe(input, InputDescriptorOptions::default()).unwrap();
        let mut reprobe_checkpoints = 0;
        let mut callbacks = 0;
        let error = decode_descriptor_analysis_stream_with_control(
            &descriptor,
            1,
            || {
                reprobe_checkpoints += 1;
                if reprobe_checkpoints == 5 {
                    Err("cancelled during exact WAVE re-probe".into())
                } else {
                    Ok(())
                }
            },
            |_, _, _| {
                callbacks += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.ends_with("cancelled during exact WAVE re-probe"));
        assert_eq!(callbacks, 0);
    }

    fn pcm16_fmt_body(sample_rate: u32, channels: u16, channel_mask: Option<u32>) -> Vec<u8> {
        let format_tag = if channel_mask.is_some() {
            0xfffe_u16
        } else {
            1
        };
        let block_align = channels.checked_mul(2).unwrap();
        let byte_rate = sample_rate.checked_mul(u32::from(block_align)).unwrap();

        let mut body = Vec::with_capacity(if channel_mask.is_some() { 40 } else { 16 });
        body.extend_from_slice(&format_tag.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&sample_rate.to_le_bytes());
        body.extend_from_slice(&byte_rate.to_le_bytes());
        body.extend_from_slice(&block_align.to_le_bytes());
        body.extend_from_slice(&16_u16.to_le_bytes());
        if let Some(mask) = channel_mask {
            body.extend_from_slice(&22_u16.to_le_bytes());
            body.extend_from_slice(&16_u16.to_le_bytes());
            body.extend_from_slice(&mask.to_le_bytes());
            // KSDATAFORMAT_SUBTYPE_PCM.
            body.extend_from_slice(&[
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38,
                0x9b, 0x71,
            ]);
        }
        body
    }

    fn pcm16_wave_with_layout_and_frames(
        sample_rate: u32,
        channels: u16,
        channel_mask: Option<u32>,
        frames: usize,
    ) -> Vec<u8> {
        let data_len = usize::from(channels)
            .checked_mul(2)
            .and_then(|frame_bytes| frame_bytes.checked_mul(frames))
            .unwrap();
        riff_wave([
            (
                *b"fmt ",
                pcm16_fmt_body(sample_rate, channels, channel_mask),
            ),
            (*b"data", vec![0; data_len]),
        ])
    }

    fn pcm16_wave_with_layout(
        sample_rate: u32,
        channels: u16,
        channel_mask: Option<u32>,
    ) -> Vec<u8> {
        pcm16_wave_with_layout_and_frames(sample_rate, channels, channel_mask, 1)
    }

    fn large_wave_from_riff(riff: &[u8], container: [u8; 4]) -> Vec<u8> {
        let mut chunks = riff[12..].to_vec();
        let data = chunks
            .windows(4)
            .position(|window| window == b"data")
            .unwrap();
        let data_size = u32::from_le_bytes(chunks[data + 4..data + 8].try_into().unwrap());
        chunks[data + 4..data + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let block_align = u16::from_le_bytes(chunks[20..22].try_into().unwrap());
        let sample_count = u64::from(data_size / u32::from(block_align));

        let mut wave = container.to_vec();
        wave.extend_from_slice(&u32::MAX.to_le_bytes());
        wave.extend_from_slice(b"WAVEds64");
        wave.extend_from_slice(&28_u32.to_le_bytes());
        let riff_size = u64::try_from(4 + 36 + chunks.len()).unwrap();
        wave.extend_from_slice(&riff_size.to_le_bytes());
        wave.extend_from_slice(&u64::from(data_size).to_le_bytes());
        wave.extend_from_slice(&sample_count.to_le_bytes());
        wave.extend_from_slice(&0_u32.to_le_bytes());
        wave.extend_from_slice(&chunks);
        wave
    }

    fn pcm16_wave_bytes(sample_rate: u32) -> Vec<u8> {
        pcm16_wave_with_layout(sample_rate, 1, None)
    }

    fn assert_native_wave_decode_routes(
        path: &Path,
        channels: u16,
        expected_provenance: ChannelLayoutProvenance,
    ) {
        let (decoded, provenance) = decode_with_layout(path).unwrap();
        assert_eq!(decoded.channels, channels, "{}", path.display());
        assert_eq!(decoded.frames, 1, "{}", path.display());
        assert_eq!(provenance, expected_provenance, "{}", path.display());

        let mut stream_callbacks = 0;
        let stream_info = decode_stream_with_layout_and_declared_frames(
            path,
            |info, provenance, declared_frames, planar| {
                stream_callbacks += 1;
                assert_eq!(info.channels, channels, "{}", path.display());
                assert_eq!(provenance, expected_provenance, "{}", path.display());
                assert_eq!(declared_frames, Some(1), "{}", path.display());
                assert_eq!(planar.len(), usize::from(channels), "{}", path.display());
                assert!(
                    planar.iter().all(|channel| channel.len() == 1),
                    "{}",
                    path.display()
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(stream_info.channels, channels, "{}", path.display());
        assert_eq!(stream_callbacks, 1, "{}", path.display());

        let mut owned_callbacks = 0;
        let owned_info = decode_stream_owned_with_layout_and_declared_frames(
            path,
            |info, provenance, declared_frames, planar| {
                owned_callbacks += 1;
                assert_eq!(info.channels, channels, "{}", path.display());
                assert_eq!(provenance, expected_provenance, "{}", path.display());
                assert_eq!(declared_frames, Some(1), "{}", path.display());
                assert_eq!(planar.len(), usize::from(channels), "{}", path.display());
                assert!(
                    planar.iter().all(|channel| channel.len() == 1),
                    "{}",
                    path.display()
                );
                Ok(planar)
            },
        )
        .unwrap();
        assert_eq!(owned_info.channels, channels, "{}", path.display());
        assert_eq!(owned_callbacks, 1, "{}", path.display());

        match expected_provenance {
            ChannelLayoutProvenance::KnownSpeakers => {
                assert_eq!(decode(path).unwrap().channels, channels);
                assert_eq!(decode_limited(path, u64::MAX).unwrap().channels, channels);
                assert_eq!(WavReader::open(path).unwrap().channels, channels);
                assert_eq!(WavReader::probe(path).unwrap().channels, channels);

                let mut borrowed_callbacks = 0;
                decode_stream(path, |_, _| {
                    borrowed_callbacks += 1;
                    Ok(())
                })
                .unwrap();
                assert_eq!(borrowed_callbacks, 1, "{}", path.display());

                let mut public_owned_callbacks = 0;
                decode_stream_owned(path, |_, planar| {
                    public_owned_callbacks += 1;
                    Ok(planar)
                })
                .unwrap();
                assert_eq!(public_owned_callbacks, 1, "{}", path.display());
            }
            ChannelLayoutProvenance::Unknown | ChannelLayoutProvenance::SceneBased => {
                for error in [
                    decode(path).unwrap_err(),
                    decode_limited(path, u64::MAX).unwrap_err(),
                    wave_error(WavReader::open(path)),
                    wave_error(WavReader::probe(path)),
                ] {
                    assert!(
                        error.contains("ambiguous channel layout"),
                        "{}: {error}",
                        path.display()
                    );
                }

                let mut borrowed_callbacks = 0;
                let error = decode_stream(path, |_, _| {
                    borrowed_callbacks += 1;
                    Ok(())
                })
                .unwrap_err();
                assert!(error.contains("ambiguous channel layout"), "{error}");
                assert_eq!(borrowed_callbacks, 0, "{}", path.display());

                let mut public_owned_callbacks = 0;
                let error = decode_stream_owned(path, |_, planar| {
                    public_owned_callbacks += 1;
                    Ok(planar)
                })
                .unwrap_err();
                assert!(error.contains("ambiguous channel layout"), "{error}");
                assert_eq!(public_owned_callbacks, 0, "{}", path.display());
            }
        }
    }

    fn wave_error<T>(result: Result<T, crate::wav::reader::WavReadError>) -> String {
        match result {
            Ok(_) => panic!("malformed WAVE unexpectedly decoded"),
            Err(error) => error.to_string(),
        }
    }

    fn assert_wave_rejected_everywhere(path: &Path, bytes: &[u8], expected: &str) {
        let memory_error = wave_error(WavReader::read_bytes(bytes));
        assert!(memory_error.contains(expected), "{memory_error}");

        std::fs::write(path, bytes).unwrap();
        let probe_error = wave_error(WavReader::probe(path));
        assert!(probe_error.contains(expected), "{probe_error}");
        let open_error = wave_error(WavReader::open(path));
        assert!(open_error.contains(expected), "{open_error}");
        let full_error = decode_with_layout(path).unwrap_err();
        assert!(full_error.contains(expected), "{full_error}");

        let mut stream_callbacks = 0;
        let stream_error = decode_stream_with_layout(path, |_, _, _| {
            stream_callbacks += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(stream_error.contains(expected), "{stream_error}");
        assert_eq!(stream_callbacks, 0);
    }

    #[test]
    fn wav_sample_rate_bounds_are_enforced_before_stream_callback() {
        let directory = tempfile::tempdir().unwrap();

        for sample_rate in [
            0,
            MIN_DECODE_SAMPLE_RATE_HZ - 1,
            MAX_DECODE_SAMPLE_RATE_HZ + 1,
        ] {
            let path = directory.path().join(format!("rate-{sample_rate}.wav"));
            std::fs::write(&path, pcm16_wave_bytes(sample_rate)).unwrap();
            let mut callbacks = 0;
            let error = decode_stream_with_layout(&path, |_, _, _| {
                callbacks += 1;
                Ok(())
            })
            .unwrap_err();
            assert!(error.contains("sample rate"));
            assert!(error.contains("outside the supported"));
            assert_eq!(callbacks, 0);
        }

        for sample_rate in [MIN_DECODE_SAMPLE_RATE_HZ, MAX_DECODE_SAMPLE_RATE_HZ] {
            let path = directory.path().join(format!("rate-{sample_rate}.wav"));
            std::fs::write(&path, pcm16_wave_bytes(sample_rate)).unwrap();
            let mut callbacks = 0;
            let info = decode_stream_with_layout(&path, |_, _, _| {
                callbacks += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(info.sample_rate, sample_rate);
            assert_eq!(callbacks, 1);
        }
    }

    #[test]
    fn wave_signature_routes_extensionless_and_misnamed_layouts_natively() {
        use ChannelLayoutProvenance::{KnownSpeakers, Unknown};

        let directory = tempfile::tempdir().unwrap();
        let layouts = [
            ("stereo", 2, None, KnownSpeakers),
            ("maskless-multichannel", 6, None, Unknown),
            ("zero-mask-multichannel", 6, Some(0), Unknown),
            ("partial-mask-multichannel", 6, Some(0x0003), Unknown),
            ("canonical-seven-one-mask", 8, Some(0x063f), KnownSpeakers),
            ("surround-only-stereo-mask", 2, Some(0x0030), KnownSpeakers),
            ("lfe-only-mono-mask", 1, Some(0x0008), KnownSpeakers),
            ("side-five-one-mask", 6, Some(0x060f), KnownSpeakers),
        ];

        for (container_name, container) in
            [("riff", *b"RIFF"), ("rf64", *b"RF64"), ("bw64", *b"BW64")]
        {
            for (layout_name, channels, mask, expected_provenance) in layouts {
                for (path_kind, suffix) in [("extensionless", ""), ("misnamed", ".mp3")] {
                    let path = directory.path().join(format!(
                        "{container_name}-{layout_name}-{path_kind}{suffix}"
                    ));
                    let riff = pcm16_wave_with_layout(48_000, channels, mask);
                    let wave = if container == *b"RIFF" {
                        riff
                    } else {
                        large_wave_from_riff(&riff, container)
                    };
                    std::fs::write(&path, wave).unwrap();

                    assert!(has_wave_signature(&path), "{}", path.display());
                    assert_native_wave_decode_routes(&path, channels, expected_provenance);
                }
            }
        }
    }

    #[test]
    fn wave_signature_sniff_is_exact_and_wave_suffix_remains_native() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        assert!(!has_wave_signature(&missing));

        let short = directory.path().join("short");
        std::fs::write(&short, b"RIFF\0\0\0").unwrap();
        assert!(!has_wave_signature(&short));

        for (name, signature) in [
            ("wrong-container", *b"RIFX\0\0\0\0WAVE"),
            ("wrong-form", *b"RIFF\0\0\0\0AVI "),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, signature).unwrap();
            assert!(!has_wave_signature(&path), "{}", path.display());
        }

        let misleading_suffix = directory.path().join("not-a-wave.wav");
        std::fs::write(&misleading_suffix, b"ID3 \0\0\0\0audio").unwrap();
        let error = decode_with_layout(&misleading_suffix).unwrap_err();
        assert!(error.contains("not a RIFF/WAVE file"), "{error}");
    }

    #[test]
    fn wave_chunk_selection_rejects_duplicates_and_data_before_format() {
        let directory = tempfile::tempdir().unwrap();
        let stereo_format = pcm16_fmt_body(48_000, 2, None);
        let six_channel_format = pcm16_fmt_body(48_000, 6, Some(0x003f));
        let ambiguous = riff_wave([
            (*b"fmt ", stereo_format.clone()),
            (*b"data", vec![0; 4]),
            (*b"fmt ", six_channel_format),
            (*b"data", vec![0; 6 * 2 * 4_096]),
        ]);
        let ambiguous_path = directory.path().join("small-then-large");
        assert_wave_rejected_everywhere(&ambiguous_path, &ambiguous, "duplicate fmt chunk");
        let limited_error = decode_limited(&ambiguous_path, 2).unwrap_err();
        assert!(
            limited_error.contains("duplicate fmt chunk"),
            "{limited_error}"
        );

        let large = pcm16_wave_with_layout_and_frames(48_000, 6, Some(0x003f), 4_096);
        let large_path = directory.path().join("single-large-data");
        std::fs::write(&large_path, large).unwrap();
        let limited_error = decode_limited(&large_path, 2).unwrap_err();
        assert!(
            limited_error.contains("decoded sample count exceeds safety limit"),
            "{limited_error}"
        );

        let duplicate_data = riff_wave([
            (*b"fmt ", stereo_format.clone()),
            (*b"data", vec![0; 4]),
            (*b"data", vec![0; 4]),
        ]);
        assert_wave_rejected_everywhere(
            &directory.path().join("duplicate-data"),
            &duplicate_data,
            "duplicate data chunk",
        );

        let data_first = riff_wave([(*b"data", vec![0; 4]), (*b"fmt ", stereo_format)]);
        assert_wave_rejected_everywhere(
            &directory.path().join("data-first"),
            &data_first,
            "data precedes fmt chunk",
        );
    }

    #[test]
    fn wave_format_contract_is_shared_by_memory_full_probe_and_stream_paths() {
        let directory = tempfile::tempdir().unwrap();

        let mut bad_block_align = pcm16_fmt_body(48_000, 2, None);
        bad_block_align[12..14].copy_from_slice(&2_u16.to_le_bytes());
        let bytes = riff_wave([(*b"fmt ", bad_block_align), (*b"data", vec![0; 4])]);
        assert_wave_rejected_everywhere(
            &directory.path().join("bad-block-align"),
            &bytes,
            "block align",
        );

        let mut bad_byte_rate = pcm16_fmt_body(48_000, 2, None);
        bad_byte_rate[8..12].copy_from_slice(&1_u32.to_le_bytes());
        let bytes = riff_wave([(*b"fmt ", bad_byte_rate), (*b"data", vec![0; 4])]);
        assert_wave_rejected_everywhere(
            &directory.path().join("bad-byte-rate"),
            &bytes,
            "average bytes per second",
        );

        let mut short_extensible = pcm16_fmt_body(48_000, 2, Some(0x0003));
        short_extensible.truncate(39);
        let bytes = riff_wave([(*b"fmt ", short_extensible), (*b"data", vec![0; 4])]);
        assert_wave_rejected_everywhere(
            &directory.path().join("short-extensible"),
            &bytes,
            "cbSize exceeds fmt chunk",
        );

        let mut bad_cb_size = pcm16_fmt_body(48_000, 2, Some(0x0003));
        bad_cb_size[16..18].copy_from_slice(&21_u16.to_le_bytes());
        let bytes = riff_wave([(*b"fmt ", bad_cb_size), (*b"data", vec![0; 4])]);
        assert_wave_rejected_everywhere(
            &directory.path().join("bad-cb-size"),
            &bytes,
            "cbSize must be at least 22",
        );

        let mut fake_guid = pcm16_fmt_body(48_000, 2, Some(0x0003));
        fake_guid[39] ^= 1;
        let bytes = riff_wave([(*b"fmt ", fake_guid), (*b"data", vec![0; 4])]);
        assert_wave_rejected_everywhere(
            &directory.path().join("fake-guid"),
            &bytes,
            "subformat GUID",
        );

        let partial_frame = riff_wave([
            (*b"fmt ", pcm16_fmt_body(48_000, 2, None)),
            (*b"data", vec![0; 3]),
        ]);
        assert_wave_rejected_everywhere(
            &directory.path().join("partial-frame"),
            &partial_frame,
            "partial PCM frame",
        );

        let mut truncated = pcm16_wave_with_layout(48_000, 2, None);
        truncated.truncate(truncated.len() - 2);
        assert_wave_rejected_everywhere(
            &directory.path().join("truncated-data"),
            &truncated,
            "file truncated",
        );
    }

    #[test]
    fn wave_extensible_float_guid_and_riff_padding_are_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("float-with-padding.bin");
        let mut float_format = pcm16_fmt_body(48_000, 2, Some(0x0003));
        float_format[8..12].copy_from_slice(&384_000_u32.to_le_bytes());
        float_format[12..14].copy_from_slice(&8_u16.to_le_bytes());
        float_format[14..16].copy_from_slice(&32_u16.to_le_bytes());
        float_format[18..20].copy_from_slice(&32_u16.to_le_bytes());
        float_format[24] = 0x03;
        let wave = riff_wave([
            (*b"fmt ", float_format),
            (*b"JUNK", vec![1, 2, 3]),
            (*b"data", vec![0; 8]),
        ]);
        let memory = WavReader::read_bytes(&wave).unwrap();
        assert_eq!(memory.source_kind, PcmKind::F32);
        assert_eq!(memory.frames, 1);
        std::fs::write(&path, wave).unwrap();
        assert_eq!(WavReader::probe(&path).unwrap().data_size, 8);
        assert_native_wave_decode_routes(&path, 2, ChannelLayoutProvenance::KnownSpeakers);
    }

    #[test]
    fn zero_channel_wave_is_rejected_by_probe_full_and_stream_decoders() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zero-channel-without-extension");
        std::fs::write(&path, pcm16_wave_with_layout(48_000, 0, None)).unwrap();

        assert!(has_wave_signature(&path));
        assert!(matches!(
            WavReader::probe(&path),
            Err(crate::wav::reader::WavReadError::ZeroChannels)
        ));
        assert!(matches!(
            WavReader::open(&path),
            Err(crate::wav::reader::WavReadError::ZeroChannels)
        ));
        assert!(decode_with_layout(&path)
            .unwrap_err()
            .contains("zero channels"));

        let mut callbacks = 0;
        let error = decode_stream_with_layout(&path, |_, _, _| {
            callbacks += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("zero channels"));
        assert_eq!(callbacks, 0);
    }

    #[test]
    fn native_wav_stream_reports_known_stereo_speakers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stereo.wav");
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 32,
            data: vec![vec![0.0; 32], vec![0.0; 32]],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::S16,
        };
        crate::wav::WavWriter::write(&path, &buffer, PcmKind::S16, false).unwrap();

        let (decoded, provenance) = decode_with_layout(&path).unwrap();
        assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.frames, 32);

        let mut callbacks = 0;
        decode_stream_with_layout_and_declared_frames(
            &path,
            |_, provenance, declared_frames, planar| {
                callbacks += 1;
                assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
                assert_eq!(declared_frames, Some(32));
                assert_eq!(planar.len(), 2);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(callbacks, 1);
    }

    #[test]
    fn wav_stream_chunks_are_adaptive_and_frame_aligned() {
        assert_eq!(wav_stream_chunk_bytes(1, PcmKind::S16), 64 * 1024);
        assert_eq!(wav_stream_chunk_bytes(2, PcmKind::S16), 1024 * 1024);

        for kind in [
            PcmKind::U8,
            PcmKind::S16,
            PcmKind::S24,
            PcmKind::S32,
            PcmKind::F32,
            PcmKind::F64,
        ] {
            for channels in [1, 2, 8] {
                let frame_bytes = channels as usize * kind.bytes_per_sample();
                let chunk_bytes = wav_stream_chunk_bytes(channels, kind);
                let target = if channels == 1 {
                    MONO_WAV_STREAM_CHUNK_BYTES
                } else {
                    MULTICHANNEL_WAV_STREAM_CHUNK_BYTES
                };
                assert_eq!(chunk_bytes % frame_bytes, 0);
                assert!(chunk_bytes <= target);
                assert!(target - chunk_bytes < frame_bytes);
            }
        }
    }

    #[test]
    fn symphonia_render_packets_coalesce_without_splitting_large_chunks() {
        fn packet(start: usize, frames: usize) -> Vec<Vec<f32>> {
            let mut buffer = vec![vec![0.0; frames], vec![0.0; frames]];
            let (left, right) = buffer.split_at_mut(1);
            for frame in 0..frames {
                let sample = (start + frame) as f32 / 65_536.0;
                left[0][frame] = sample;
                right[0][frame] = -sample;
            }
            buffer
        }

        let info = StreamInfo {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        let mut planar = Vec::new();
        let mut observed = vec![Vec::new(), Vec::new()];
        let mut chunk_frames = Vec::new();
        let mut consume = |_: &StreamInfo, chunk: &mut [Vec<f32>]| {
            chunk_frames.push(chunk[0].len());
            for (destination, source) in observed.iter_mut().zip(chunk) {
                destination.extend_from_slice(source);
            }
            Ok(())
        };

        let packet_frames = [1_500, 1_500, 1_500, 1_000, 20_000, 123];
        let mut start = 0;
        for frames in packet_frames {
            let mut packet = packet(start, frames);
            append_symphonia_stream_chunk(&info, &mut packet, &mut planar, &mut consume).unwrap();
            start += frames;
        }
        flush_symphonia_stream_chunk(&info, &mut planar, &mut consume).unwrap();

        assert_eq!(chunk_frames, [4_500, 1_000, 20_000, 123]);
        let expected_left = (0..start)
            .map(|frame| frame as f32 / 65_536.0)
            .collect::<Vec<_>>();
        let expected_right = expected_left
            .iter()
            .map(|sample| -*sample)
            .collect::<Vec<_>>();
        assert_eq!(observed, [expected_left, expected_right]);
        assert!(planar.iter().all(Vec::is_empty));
        assert!(planar
            .iter()
            .all(|channel| channel.capacity() >= TARGET_SYMPHONIA_STREAM_CHUNK_FRAMES));
    }

    #[test]
    fn parallel_flac_batch_geometry_is_memory_bounded() {
        assert_eq!(parallel_flac_batch_limit(8, 4_096, 2), 256);
        assert_eq!(parallel_flac_batch_limit(8, 65_535, 8), 16);
        assert_eq!(parallel_flac_batch_limit(8, u64::MAX, 8), 1);

        assert_eq!(
            admit_parallel_flac_packet(0, MAX_PARALLEL_FLAC_PACKET_BYTES, true),
            Ok(Some(MAX_PARALLEL_FLAC_PACKET_BYTES))
        );
        assert!(admit_parallel_flac_packet(0, MAX_PARALLEL_FLAC_PACKET_BYTES + 1, true).is_err());
        assert_eq!(
            admit_parallel_flac_packet(MAX_PARALLEL_FLAC_PACKET_BYTES - 1, 1, false),
            Ok(Some(MAX_PARALLEL_FLAC_PACKET_BYTES))
        );
        assert_eq!(
            admit_parallel_flac_packet(MAX_PARALLEL_FLAC_PACKET_BYTES, 1, false),
            Ok(None)
        );

        let short = SymphoniaAudioTrack {
            id: 0,
            num_frames: Some(48_000),
            time_base: None,
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        let crossover = SymphoniaAudioTrack {
            id: 0,
            num_frames: Some(192_000),
            time_base: None,
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        let unknown = SymphoniaAudioTrack {
            id: 0,
            num_frames: None,
            time_base: None,
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        assert_eq!(parallel_flac_worker_cap(&short, u64::MAX), 1);
        assert_eq!(parallel_flac_worker_cap(&crossover, 0), 2);
        let efficient = SymphoniaAudioTrack {
            id: 0,
            num_frames: Some(384_000),
            time_base: None,
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        assert_eq!(parallel_flac_worker_cap(&efficient, 0), 4);
        assert!(parallel_flac_worker_cap(&efficient, 0) >= MIN_PARALLEL_FLAC_DECODERS);
        assert_eq!(parallel_flac_worker_cap(&unknown, 383 * 1024), 1);
        assert_eq!(parallel_flac_worker_cap(&unknown, 384 * 1024), 2);
        assert_eq!(parallel_flac_worker_cap(&unknown, u64::MAX), 8);
    }

    #[test]
    fn packet_preflight_converts_container_timebase_to_pcm_frames() {
        use symphonia::core::packet::Packet;
        use symphonia::core::units::{Duration, TimeBase, Timestamp};

        let track = SymphoniaAudioTrack {
            id: 0,
            num_frames: None,
            time_base: TimeBase::try_new(1, 90_000),
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        let packet = Packet::new(0, Timestamp::new(0), Duration::new(1_920), Vec::new());
        assert_eq!(
            symphonia_packet_sample_upper_bound(&track, &packet).unwrap(),
            Some(2_048)
        );
    }

    #[test]
    fn packet_preflight_uses_pcm_bytes_and_rejects_unknown_compressed_geometry() {
        use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_PCM_S16LE};
        use symphonia::core::packet::Packet;
        use symphonia::core::units::{Duration, Timestamp};

        let pcm = SymphoniaAudioTrack {
            id: 0,
            num_frames: None,
            time_base: None,
            codec_params: codec_params_for_codec(
                48_000,
                CHANNEL_LAYOUT_STEREO.clone(),
                CODEC_ID_PCM_S16LE,
                None,
            ),
        };
        // Nine bytes are three conservative 4-byte stereo frames. The
        // malformed trailing byte must round up, never under-admit decoder
        // output.
        let packet = Packet::new(0, Timestamp::new(0), Duration::new(0), vec![0; 9]);
        assert_eq!(
            symphonia_packet_sample_upper_bound(&pcm, &packet).unwrap(),
            Some(6)
        );
        assert_eq!(
            enforce_symphonia_packet_sample_limit(&pcm, &packet, 5).unwrap_err(),
            SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED
        );
        enforce_symphonia_packet_sample_limit(&pcm, &packet, 6).unwrap();

        let compressed = SymphoniaAudioTrack {
            id: 0,
            num_frames: None,
            time_base: None,
            codec_params: codec_params_for_codec(
                48_000,
                CHANNEL_LAYOUT_STEREO.clone(),
                CODEC_ID_AAC,
                None,
            ),
        };
        assert_eq!(
            symphonia_packet_sample_upper_bound(&compressed, &packet).unwrap(),
            None
        );
        assert_eq!(
            enforce_symphonia_packet_sample_limit(&compressed, &packet, u64::MAX).unwrap_err(),
            SERVICE_PACKET_SAMPLE_LIMIT_EXCEEDED
        );
    }

    fn ape_footer(version: u32, size: u32, items: u32, flags: u32) -> [u8; 32] {
        let mut footer = [0_u8; 32];
        footer[..8].copy_from_slice(b"APETAGEX");
        footer[8..12].copy_from_slice(&version.to_le_bytes());
        footer[12..16].copy_from_slice(&size.to_le_bytes());
        footer[16..20].copy_from_slice(&items.to_le_bytes());
        footer[20..24].copy_from_slice(&flags.to_le_bytes());
        footer
    }

    fn ape_v2_tag(item: &[u8]) -> Vec<u8> {
        let declared_size = u32::try_from(item.len() + 32).unwrap();
        let footer_flags = SERVICE_APE_HAS_HEADER | SERVICE_APE_HAS_FOOTER;
        let footer = ape_footer(
            2000,
            declared_size,
            u32::from(!item.is_empty()),
            footer_flags,
        );
        let mut header = footer;
        header[20..24].copy_from_slice(&(footer_flags | SERVICE_APE_IS_HEADER).to_le_bytes());
        [header.as_slice(), item, footer.as_slice()].concat()
    }

    #[test]
    fn service_trailing_ape_preflight_bounds_items_and_validates_framing() {
        let directory = tempfile::tempdir().unwrap();
        let mpeg_prefix = silent_mpeg1_layer3_frame(0);
        let mut item = Vec::new();
        item.extend_from_slice(&3_u32.to_le_bytes());
        item.extend_from_slice(&0_u32.to_le_bytes());
        item.extend_from_slice(b"Title\0abc");
        let tag = ape_v2_tag(&item);

        let mpeg_path = directory.path().join("bounded-ape.mp3");
        let mut bytes = mpeg_prefix.to_vec();
        bytes.extend_from_slice(&tag);
        std::fs::write(&mpeg_path, &bytes).unwrap();
        service_container_preflight(&mpeg_path, || Ok(())).unwrap();

        let with_id3v1_path = directory.path().join("bounded-ape-id3v1.mp3");
        let mut with_id3v1 = mpeg_prefix.to_vec();
        with_id3v1.extend_from_slice(&tag);
        with_id3v1.extend_from_slice(b"TAG");
        with_id3v1.extend_from_slice(&[0; 125]);
        std::fs::write(&with_id3v1_path, &with_id3v1).unwrap();
        service_container_preflight(&with_id3v1_path, || Ok(())).unwrap();

        let flac_path = directory.path().join("bounded-ape.flac");
        let mut flac = b"fLaC\x80\0\0\0".to_vec();
        flac.extend_from_slice(&tag);
        std::fs::write(&flac_path, &flac).unwrap();
        service_container_preflight(&flac_path, || Ok(())).unwrap();

        let huge_path = directory.path().join("huge-ape.mp3");
        let mut huge_item = Vec::new();
        huge_item.extend_from_slice(
            &u32::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1)
                .unwrap()
                .to_le_bytes(),
        );
        huge_item.extend_from_slice(&0_u32.to_le_bytes());
        huge_item.extend_from_slice(b"Title\0");
        let huge_footer = ape_footer(
            2000,
            u32::try_from(huge_item.len() + 32).unwrap(),
            1,
            0x4000_0000,
        );
        let mut huge = mpeg_prefix.to_vec();
        huge.extend_from_slice(&huge_item);
        huge.extend_from_slice(&huge_footer);
        std::fs::write(&huge_path, &huge).unwrap();
        assert_eq!(
            service_container_preflight(&huge_path, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let false_footer_path = directory.path().join("false-ape-footer.mp3");
        let mut false_footer = mpeg_prefix.to_vec();
        false_footer.extend_from_slice(&ape_footer(9999, 32, 0, 0x4000_0000));
        std::fs::write(&false_footer_path, &false_footer).unwrap();
        let error = service_container_preflight(&false_footer_path, || Ok(())).unwrap_err();
        assert!(
            error.contains("inter-frame data or invalid geometry"),
            "{error}"
        );

        let truncated_path = directory.path().join("truncated-ape.mp3");
        let mut truncated = mpeg_prefix.to_vec();
        truncated.extend_from_slice(&ape_footer(2000, 4096, 0, 0x4000_0000));
        std::fs::write(&truncated_path, &truncated).unwrap();
        let error = service_container_preflight(&truncated_path, || Ok(())).unwrap_err();
        assert!(error.contains("truncated trailing APE metadata"), "{error}");

        // Symphonia probes both absolute trailing anchors. A valid marker at
        // -32 must not hide an unsafe marker at -160 merely because the 128
        // intervening bytes are not an ID3v1 footer.
        let both_path = directory.path().join("both-ape-anchors.mp3");
        let mut both = mpeg_prefix.to_vec();
        both.extend_from_slice(&huge_item);
        both.extend_from_slice(&huge_footer);
        both.extend_from_slice(&[0; 96]);
        both.extend_from_slice(&ape_footer(2000, 32, 0, SERVICE_APE_HAS_FOOTER));
        std::fs::write(&both_path, &both).unwrap();
        assert_eq!(
            service_container_preflight(&both_path, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        // A supplemental-probe anchor can point at an APEv2 header followed
        // by unrelated bytes. Even though the metadata parser can consume the
        // tag, the complete frame-chain contract rejects the trailing junk.
        let anchored_header_path = directory.path().join("ape-header-at-minus-160.mp3");
        let mut anchored_header = mpeg_prefix.to_vec();
        anchored_header.extend(ape_v2_tag(&[]));
        anchored_header.extend_from_slice(&[0; 96]);
        std::fs::write(&anchored_header_path, &anchored_header).unwrap();
        let error = service_container_preflight(&anchored_header_path, || Ok(())).unwrap_err();
        assert!(
            error.contains("inter-frame data or invalid geometry"),
            "{error}"
        );
    }

    #[test]
    fn service_leading_ape_preflight_scans_probe_window_without_allocating_values() {
        let directory = tempfile::tempdir().unwrap();
        let mut item = Vec::new();
        item.extend_from_slice(&3_u32.to_le_bytes());
        item.extend_from_slice(&0_u32.to_le_bytes());
        item.extend_from_slice(b"Title\0abc");

        // Place the marker across the end of Symphonia's 1 MiB search range;
        // the fixed 32 KiB scanner overlap must still recognize its full
        // 12-byte identity without allocating a probe-sized buffer.
        let valid_path = directory.path().join("leading-ape.mp3");
        let mut valid = vec![0; usize::try_from(SERVICE_SYMPHONIA_PROBE_BYTES).unwrap() - 5];
        valid.extend(ape_v2_tag(&item));
        valid.extend_from_slice(&silent_mpeg1_layer3_frame(0));
        std::fs::write(&valid_path, &valid).unwrap();
        assert_eq!(
            service_container_preflight(&valid_path, || Ok(())).unwrap(),
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Mpa,
                media_offset: u64::try_from(valid.len() - 417).unwrap(),
                media_end: u64::try_from(valid.len()).unwrap(),
            }
        );

        let leading_flac_path = directory.path().join("leading-ape.flac");
        let leading_ape = ape_v2_tag(&item);
        let mut leading_flac = leading_ape.clone();
        leading_flac.extend_from_slice(b"fLaC\x80\0\0\0");
        std::fs::write(&leading_flac_path, &leading_flac).unwrap();
        assert_eq!(
            service_container_preflight(&leading_flac_path, || Ok(())).unwrap(),
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Flac,
                media_offset: u64::try_from(leading_ape.len()).unwrap(),
                media_end: u64::try_from(leading_flac.len()).unwrap(),
            }
        );

        let mut huge_item = Vec::new();
        huge_item.extend_from_slice(
            &u32::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1)
                .unwrap()
                .to_le_bytes(),
        );
        huge_item.extend_from_slice(&0_u32.to_le_bytes());
        huge_item.extend_from_slice(b"Title\0");
        let huge_path = directory.path().join("leading-huge-ape.mp3");
        let mut huge = b"junk".to_vec();
        huge.extend(ape_v2_tag(&huge_item));
        huge.extend_from_slice(&silent_mpeg1_layer3_frame(0));
        std::fs::write(&huge_path, &huge).unwrap();
        assert_eq!(
            service_container_preflight(&huge_path, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let truncated_path = directory.path().join("leading-truncated-ape.mp3");
        let mut truncated = b"junk".to_vec();
        let mut header = ape_footer(
            2000,
            4096,
            0,
            SERVICE_APE_HAS_HEADER | SERVICE_APE_HAS_FOOTER | SERVICE_APE_IS_HEADER,
        );
        header[24] = 1;
        truncated.extend_from_slice(&header);
        std::fs::write(&truncated_path, &truncated).unwrap();
        let error = service_container_preflight(&truncated_path, || Ok(())).unwrap_err();
        assert!(error.contains("truncated leading APE metadata"), "{error}");

        // Unsupported-version lookalikes are not admitted as trailing metadata
        // and therefore fail the complete raw frame-chain check.
        let fake_path = directory.path().join("fake-leading-ape.mp3");
        let mut fake = silent_mpeg1_layer3_frame(0);
        fake.extend_from_slice(&ape_footer(9999, 32, 0, SERVICE_APE_HAS_FOOTER));
        std::fs::write(&fake_path, &fake).unwrap();
        let error = service_container_preflight(&fake_path, || Ok(())).unwrap_err();
        assert!(
            error.contains("inter-frame data or invalid geometry"),
            "{error}"
        );
    }

    fn unchecked_ogg_page(flags: u8, serial: u32, sequence: u32, lacing: &[u8]) -> Vec<u8> {
        let mut page = Vec::with_capacity(
            27 + lacing.len()
                + lacing
                    .iter()
                    .map(|&value| usize::from(value))
                    .sum::<usize>(),
        );
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&0_u64.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&0_u32.to_le_bytes());
        page.push(u8::try_from(lacing.len()).unwrap());
        page.extend_from_slice(lacing);
        for &length in lacing {
            page.resize(page.len() + usize::from(length), 0);
        }
        page
    }

    fn ogg_page_with_packets(flags: u8, serial: u32, sequence: u32, packets: &[&[u8]]) -> Vec<u8> {
        let mut lacing = Vec::new();
        let mut body = Vec::new();
        for packet in packets {
            let mut remaining = packet.len();
            let mut offset = 0_usize;
            while remaining >= 255 {
                lacing.push(255);
                body.extend_from_slice(&packet[offset..offset + 255]);
                offset += 255;
                remaining -= 255;
            }
            lacing.push(u8::try_from(remaining).unwrap());
            body.extend_from_slice(&packet[offset..]);
        }
        assert!(lacing.len() <= 255);
        let mut page = Vec::with_capacity(27 + lacing.len() + body.len());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&0_u64.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&0_u32.to_le_bytes());
        page.push(u8::try_from(lacing.len()).unwrap());
        page.extend_from_slice(&lacing);
        page.extend_from_slice(&body);
        page
    }

    fn ogg_page_with_lacing_body(
        flags: u8,
        serial: u32,
        sequence: u32,
        lacing: &[u8],
        body: &[u8],
    ) -> Vec<u8> {
        assert_eq!(
            body.len(),
            lacing
                .iter()
                .map(|&length| usize::from(length))
                .sum::<usize>()
        );
        let mut page = Vec::with_capacity(27 + lacing.len() + body.len());
        page.extend_from_slice(b"OggS");
        page.push(0);
        page.push(flags);
        page.extend_from_slice(&0_u64.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&sequence.to_le_bytes());
        page.extend_from_slice(&0_u32.to_le_bytes());
        page.push(u8::try_from(lacing.len()).unwrap());
        page.extend_from_slice(lacing);
        page.extend_from_slice(body);
        page
    }

    fn append_ogg_packet_pages(
        output: &mut Vec<u8>,
        serial: u32,
        sequence: &mut u32,
        first_flags: u8,
        last_flags: u8,
        packet: &[u8],
    ) {
        let mut offset = 0_usize;
        let mut first = true;
        loop {
            let remaining = packet.len() - offset;
            let full_segments = (remaining / 255).min(255);
            let page_payload = full_segments * 255;
            let has_room_for_end = full_segments < 255;
            let tail = if has_room_for_end {
                Some(remaining - page_payload)
            } else {
                None
            };
            let mut lacing = vec![255; full_segments];
            if let Some(tail) = tail {
                lacing.push(u8::try_from(tail).unwrap());
            }
            let body_len = page_payload + tail.unwrap_or(0);
            let final_page = offset + body_len == packet.len() && tail.is_some();
            let flags =
                (if first { first_flags } else { 0x01 }) | if final_page { last_flags } else { 0 };
            output.extend(ogg_page_with_lacing_body(
                flags,
                serial,
                *sequence,
                &lacing,
                &packet[offset..offset + body_len],
            ));
            *sequence += 1;
            offset += body_len;
            first = false;
            if final_page {
                break;
            }
        }
    }

    fn ogg_flac_identity(header_packets: u16) -> Vec<u8> {
        let mut packet = b"\x7fFLAC\x01\x00".to_vec();
        packet.extend_from_slice(&header_packets.to_be_bytes());
        packet.extend_from_slice(b"fLaC");
        packet.extend_from_slice(&[0, 0, 0, 34]);
        packet.extend_from_slice(&[0; 34]);
        assert_eq!(packet.len(), 51);
        packet
    }

    fn ogg_flac_metadata_packet(block_type: u8, payload: &[u8]) -> Vec<u8> {
        let length = u32::try_from(payload.len()).unwrap();
        assert!(length <= 0x00ff_ffff);
        let mut packet = Vec::with_capacity(4 + payload.len());
        packet.push(block_type);
        packet.extend_from_slice(&length.to_be_bytes()[1..]);
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn service_ogg_preflight_bounds_vorbis_and_opus_comment_items() {
        let directory = tempfile::tempdir().unwrap();

        let mut valid_tags = b"OpusTags".to_vec();
        valid_tags.extend_from_slice(&3_u32.to_le_bytes());
        valid_tags.extend_from_slice(b"abc");
        valid_tags.extend_from_slice(&0_u32.to_le_bytes());
        let valid = ogg_page_with_packets(0x02 | 0x04, 1, 0, &[b"OpusHead", &valid_tags]);
        let valid_path = directory.path().join("valid-comments.opus");
        std::fs::write(&valid_path, &valid).unwrap();
        preflight_ogg_packets(
            &valid_path,
            valid.len() as u64,
            SERVICE_MAX_ENCODED_PACKET_BYTES,
            &mut || Ok(()),
        )
        .unwrap();

        for (name, identity, mut comment) in [
            (
                "huge-opus-comment.opus",
                b"OpusHead".as_slice(),
                b"OpusTags".to_vec(),
            ),
            (
                "huge-vorbis-comment.ogg",
                b"\x01vorbis".as_slice(),
                b"\x03vorbis".to_vec(),
            ),
        ] {
            comment.extend_from_slice(&0_u32.to_le_bytes());
            comment.extend_from_slice(&1_u32.to_le_bytes());
            comment.extend_from_slice(
                &u32::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1)
                    .unwrap()
                    .to_le_bytes(),
            );
            let bytes = ogg_page_with_packets(0x02 | 0x04, 2, 0, &[identity, &comment]);
            let path = directory.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                preflight_ogg_packets(
                    &path,
                    bytes.len() as u64,
                    SERVICE_MAX_ENCODED_PACKET_BYTES,
                    &mut || Ok(()),
                )
                .unwrap_err(),
                SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
            );
        }

        let mut many_tags = b"OpusTags".to_vec();
        many_tags.extend_from_slice(&0_u32.to_le_bytes());
        many_tags.extend_from_slice(&64_u32.to_le_bytes());
        for _ in 0..64 {
            many_tags.extend_from_slice(&0_u32.to_le_bytes());
        }
        let many = ogg_page_with_packets(0x02 | 0x04, 3, 0, &[b"OpusHead", &many_tags]);
        let many_path = directory.path().join("many-comments.opus");
        std::fs::write(&many_path, &many).unwrap();
        let mut checkpoints = 0;
        preflight_ogg_packets(
            &many_path,
            many.len() as u64,
            SERVICE_MAX_ENCODED_PACKET_BYTES,
            &mut || {
                checkpoints += 1;
                Ok(())
            },
        )
        .unwrap();
        assert!(checkpoints >= 2);
    }

    #[test]
    fn service_ogg_preflight_bounds_continued_packets_and_checks_every_page() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("continued.ogg");
        let mut bytes = unchecked_ogg_page(0x02, 7, 0, &[255]);
        bytes.extend(unchecked_ogg_page(0x01 | 0x04, 7, 1, &[1]));
        std::fs::write(&path, &bytes).unwrap();

        let mut checkpoints = 0;
        let error = preflight_ogg_packets(&path, bytes.len() as u64, 255, &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);
        assert_eq!(checkpoints, 2);

        let mut checkpoints = 0;
        preflight_ogg_packets(&path, bytes.len() as u64, 256, &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(checkpoints, 2);
    }

    #[test]
    fn service_ogg_flac_preflight_validates_metadata_across_continued_pages() {
        let directory = tempfile::tempdir().unwrap();
        let identity = ogg_flac_identity(2);
        let mut comment_payload = 300_u32.to_le_bytes().to_vec();
        comment_payload.extend_from_slice(&[b'v'; 300]);
        comment_payload.extend_from_slice(&1_u32.to_le_bytes());
        comment_payload.extend_from_slice(&5_u32.to_le_bytes());
        comment_payload.extend_from_slice(b"A=B=C");
        let comment = ogg_flac_metadata_packet(0x04, &comment_payload);

        let mut picture_payload = 3_u32.to_be_bytes().to_vec();
        picture_payload.extend_from_slice(&9_u32.to_be_bytes());
        picture_payload.extend_from_slice(b"image/png");
        picture_payload.extend_from_slice(&0_u32.to_be_bytes());
        picture_payload.extend_from_slice(&[0; 16]);
        picture_payload.extend_from_slice(&3_u32.to_be_bytes());
        picture_payload.extend_from_slice(b"png");
        let picture = ogg_flac_metadata_packet(0x86, &picture_payload);

        let first_comment = &comment[..255];
        let mut first_body = identity.clone();
        first_body.extend_from_slice(first_comment);
        let mut bytes = ogg_page_with_lacing_body(0x02, 11, 0, &[51, 255], &first_body);
        let mut final_body = comment[255..].to_vec();
        final_body.extend_from_slice(&picture);
        bytes.extend(ogg_page_with_lacing_body(
            0x01 | 0x04,
            11,
            1,
            &[
                u8::try_from(comment.len() - 255).unwrap(),
                u8::try_from(picture.len()).unwrap(),
            ],
            &final_body,
        ));
        let path = directory.path().join("metadata.oga");
        std::fs::write(&path, &bytes).unwrap();
        preflight_ogg_packets(
            &path,
            bytes.len() as u64,
            SERVICE_MAX_ENCODED_PACKET_BYTES,
            &mut || Ok(()),
        )
        .unwrap();

        let huge_le = u32::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1)
            .unwrap()
            .to_le_bytes();
        let huge_be = u32::from_le_bytes(huge_le).to_be_bytes();
        let mut huge_comment = 0_u32.to_le_bytes().to_vec();
        huge_comment.extend_from_slice(&1_u32.to_le_bytes());
        huge_comment.extend_from_slice(&huge_le);
        let mut huge_count = 0_u32.to_le_bytes().to_vec();
        huge_count.extend_from_slice(
            &u32::try_from(SERVICE_MAX_CONTAINER_ITEMS + 1)
                .unwrap()
                .to_le_bytes(),
        );
        let mut huge_mime = 3_u32.to_be_bytes().to_vec();
        huge_mime.extend_from_slice(&huge_be);
        let mut huge_description = 3_u32.to_be_bytes().to_vec();
        huge_description.extend_from_slice(&0_u32.to_be_bytes());
        huge_description.extend_from_slice(&huge_be);
        let mut huge_data = 3_u32.to_be_bytes().to_vec();
        huge_data.extend_from_slice(&0_u32.to_be_bytes());
        huge_data.extend_from_slice(&0_u32.to_be_bytes());
        huge_data.extend_from_slice(&[0; 16]);
        huge_data.extend_from_slice(&huge_be);
        for (name, kind, payload) in [
            ("huge-vendor.oga", 0x04, huge_le.to_vec()),
            ("huge-comment.oga", 0x04, huge_comment),
            ("huge-comment-count.oga", 0x04, huge_count),
            ("huge-picture-mime.oga", 0x86, huge_mime),
            ("huge-picture-description.oga", 0x86, huge_description),
            ("huge-picture-data.oga", 0x86, huge_data),
        ] {
            let metadata = ogg_flac_metadata_packet(kind, &payload);
            let bytes = ogg_page_with_packets(0x02 | 0x04, 12, 0, &[&identity, &metadata]);
            let path = directory.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                preflight_ogg_packets(
                    &path,
                    bytes.len() as u64,
                    SERVICE_MAX_ENCODED_PACKET_BYTES,
                    &mut || Ok(()),
                )
                .unwrap_err(),
                SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
            );
        }

        let mut truncated_payload = 5_u32.to_le_bytes().to_vec();
        truncated_payload.extend_from_slice(b"xy");
        let truncated = ogg_flac_metadata_packet(0x84, &truncated_payload);
        let bytes = ogg_page_with_packets(0x02 | 0x04, 13, 0, &[&identity, &truncated]);
        let path = directory.path().join("truncated-comments.oga");
        std::fs::write(&path, &bytes).unwrap();
        let error = preflight_ogg_packets(
            &path,
            bytes.len() as u64,
            SERVICE_MAX_ENCODED_PACKET_BYTES,
            &mut || Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("truncated Ogg comment packet"), "{error}");
    }

    #[test]
    fn service_ogg_flac_preflight_bounds_block_total_before_next_payload() {
        let mut metadata = ServiceOggMetadataPreflight {
            packet_index: 1,
            codec: Some(ServiceOggCommentCodec::Flac),
            ..ServiceOggMetadataPreflight::default()
        };
        let payload = vec![0; SERVICE_MAX_METADATA_ITEM_BYTES as usize];
        let packet = ogg_flac_metadata_packet(0x01, &payload);
        for _ in 0..15 {
            metadata.push(&packet, &mut || Ok(())).unwrap();
            metadata.end_packet().unwrap();
        }
        // Fifteen full blocks fit after their headers. The sixteenth crosses
        // the 16 MiB aggregate budget at its four-byte header, before its
        // payload is examined or copied by a third-party mapper.
        let error = metadata.push(&packet[..4], &mut || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);

        let mut count_limited = ServiceOggMetadataPreflight {
            packet_index: 1,
            codec: Some(ServiceOggCommentCodec::Flac),
            ..ServiceOggMetadataPreflight::default()
        };
        count_limited.budget.entries = SERVICE_MAX_CONTAINER_ITEMS;
        assert_eq!(
            count_limited
                .push(&[0x01, 0, 0, 0], &mut || Ok(()))
                .unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn service_ogg_metadata_budget_is_file_wide_across_chained_streams() {
        let directory = tempfile::tempdir().unwrap();
        let payload_len = usize::try_from(SERVICE_MAX_METADATA_TOTAL_BYTES / 2 + 1).unwrap();
        let metadata_packet = ogg_flac_metadata_packet(0x81, &vec![0; payload_len]);
        let identity = ogg_flac_identity(1);

        let mut one_stream = Vec::new();
        let mut sequence = 0;
        append_ogg_packet_pages(&mut one_stream, 41, &mut sequence, 0x02, 0, &identity);
        append_ogg_packet_pages(
            &mut one_stream,
            41,
            &mut sequence,
            0,
            0x04,
            &metadata_packet,
        );
        let one_path = directory.path().join("one-large-padding.oga");
        std::fs::write(&one_path, &one_stream).unwrap();
        preflight_ogg_packets(
            &one_path,
            one_stream.len() as u64,
            SERVICE_MAX_ENCODED_PACKET_BYTES,
            &mut || Ok(()),
        )
        .unwrap();

        let mut chained = one_stream;
        let mut second_sequence = 0;
        append_ogg_packet_pages(&mut chained, 42, &mut second_sequence, 0x02, 0, &identity);
        append_ogg_packet_pages(
            &mut chained,
            42,
            &mut second_sequence,
            0,
            0x04,
            &metadata_packet,
        );
        let chained_path = directory.path().join("aggregate-over-chains.oga");
        std::fs::write(&chained_path, &chained).unwrap();
        assert_eq!(
            preflight_ogg_packets(
                &chained_path,
                chained.len() as u64,
                SERVICE_MAX_ENCODED_PACKET_BYTES,
                &mut || Ok(()),
            )
            .unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn service_flac_allows_large_skip_blocks_but_bounds_retained_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let large = usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1).unwrap();
        for (name, kind) in [("padding.flac", 0x81_u8), ("unknown.flac", 0x87)] {
            let mut bytes = b"fLaC".to_vec();
            bytes.push(kind);
            bytes.extend_from_slice(&(large as u32).to_be_bytes()[1..]);
            bytes.resize(bytes.len() + large, 0);
            let path = directory.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            service_container_preflight(&path, || Ok(())).unwrap();
        }

        for (name, kind) in [("comment.flac", 0x84_u8), ("picture.flac", 0x86)] {
            let mut bytes = b"fLaC".to_vec();
            bytes.push(kind);
            bytes.extend_from_slice(&(large as u32).to_be_bytes()[1..]);
            let path = directory.path().join(name);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                service_container_preflight(&path, || Ok(())).unwrap_err(),
                SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
            );
        }

        let count_path = directory.path().join("comment-count.flac");
        let mut count = b"fLaC\x84\0\0\x08".to_vec();
        count.extend_from_slice(&0_u32.to_le_bytes());
        count.extend_from_slice(
            &u32::try_from(SERVICE_MAX_CONTAINER_ITEMS + 1)
                .unwrap()
                .to_le_bytes(),
        );
        std::fs::write(&count_path, count).unwrap();
        assert_eq!(
            service_container_preflight(&count_path, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let mut ape_item = Vec::new();
        ape_item.extend_from_slice(&(SERVICE_MAX_METADATA_ITEM_BYTES as u32).to_le_bytes());
        ape_item.extend_from_slice(&0_u32.to_le_bytes());
        ape_item.extend_from_slice(b"Title\0");
        ape_item.resize(
            ape_item.len() + usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES).unwrap(),
            b'x',
        );
        let ape = ape_v2_tag(&ape_item);
        let padding_len = 15 * 1024 * 1024;
        let mut mixed = b"fLaC\x81\xf0\0\0".to_vec();
        mixed.resize(mixed.len() + padding_len, 0);
        mixed.extend(ape);
        let mixed_path = directory.path().join("flac-plus-ape-total.flac");
        std::fs::write(&mixed_path, mixed).unwrap();
        assert_eq!(
            service_container_preflight(&mixed_path, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let mut budget = ServiceMetadataBudget::default();
        let mut padding = ServiceOggFlacPacketScanner::default();
        let header = [0x81, (large as u32).to_be_bytes()[1], 0, 1];
        padding.push(&header, &mut budget, &mut || Ok(())).unwrap();
        for chunk in vec![0_u8; large].chunks(SERVICE_CONTROLLED_READ_BYTES) {
            padding.push(chunk, &mut budget, &mut || Ok(())).unwrap();
        }
        padding.finish().unwrap();
        for kind in [0x84, 0x86] {
            let mut scanner = ServiceOggFlacPacketScanner::default();
            assert_eq!(
                scanner
                    .push(
                        &[kind, (large as u32).to_be_bytes()[1], 0, 1],
                        &mut ServiceMetadataBudget::default(),
                        &mut || Ok(())
                    )
                    .unwrap_err(),
                SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
            );
        }
    }

    #[test]
    fn service_matroska_preflight_rejects_block_before_payload_materialization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bounded.mka");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x1a45_dfa3_u32.to_be_bytes());
        bytes.push(0x80); // Empty EBML header.
        bytes.extend_from_slice(&0x1853_8067_u32.to_be_bytes());
        bytes.push(0x87); // Seven-byte Segment payload.
        bytes.extend_from_slice(&[0xa3, 0x85, 0, 0, 0, 0, 0]);
        std::fs::write(&path, &bytes).unwrap();

        let error = preflight_matroska(&path, bytes.len() as u64, 4, &mut || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);
        preflight_matroska(&path, bytes.len() as u64, 5, &mut || Ok(())).unwrap();
    }

    fn service_ebml_size(size: usize) -> Vec<u8> {
        let size = u64::try_from(size).unwrap();
        for length in 1..=8 {
            let marker = 1_u64 << (7 * length);
            if size < marker - 1 {
                let encoded = (marker | size).to_be_bytes();
                return encoded[8 - length..].to_vec();
            }
        }
        panic!("test EBML element is too large")
    }

    fn service_ebml_element(id: &[u8], payload: Vec<u8>) -> Vec<u8> {
        let mut element = Vec::with_capacity(id.len() + 8 + payload.len());
        element.extend_from_slice(id);
        element.extend(service_ebml_size(payload.len()));
        element.extend(payload);
        element
    }

    fn service_matroska_file(segment_payload: Vec<u8>) -> Vec<u8> {
        let ebml = service_ebml_element(&0x1a45_dfa3_u32.to_be_bytes(), Vec::new());
        [
            ebml,
            service_ebml_element(&0x1853_8067_u32.to_be_bytes(), segment_payload),
        ]
        .concat()
    }

    #[test]
    fn service_matroska_uses_file_wide_budget_only_for_retained_leaves() {
        let directory = tempfile::tempdir().unwrap();
        let value = vec![b'x'; usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES - 1).unwrap()];
        let mut simple_tags = Vec::new();
        for _ in 0..17 {
            let value = service_ebml_element(&[0x44, 0x87], value.clone());
            simple_tags.extend(service_ebml_element(&[0x67, 0xc8], value));
        }
        let tag = service_ebml_element(&[0x73, 0x73], simple_tags);
        let tags = service_ebml_element(&[0x12, 0x54, 0xc3, 0x67], tag);
        let bytes = service_matroska_file(tags);
        let path = directory.path().join("aggregate-metadata.mka");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            preflight_matroska(
                &path,
                bytes.len() as u64,
                SERVICE_MAX_ENCODED_PACKET_BYTES,
                &mut || Ok(()),
            )
            .unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let audio = service_ebml_element(
            &[0xa3],
            vec![0; usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1).unwrap()],
        );
        let bytes = service_matroska_file(audio);
        let path = directory.path().join("large-audio-block.mka");
        std::fs::write(&path, &bytes).unwrap();
        preflight_matroska(
            &path,
            bytes.len() as u64,
            SERVICE_MAX_ENCODED_PACKET_BYTES,
            &mut || Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn service_metadata_preflight_registry_is_explicit_and_fail_closed() {
        use ServiceContainerSniff::{
            Flac, IsoBmff, Matroska, NativeBounded, Ogg, RawMpegOrAdts,
            UnsupportedMetadataContainer,
        };

        for (prefix, expected) in [
            (b"RIFF\0\0\0\0WAVE----".as_slice(), NativeBounded),
            (b"RF64\0\0\0\0WAVE----".as_slice(), NativeBounded),
            (b"BW64\0\0\0\0WAVE----".as_slice(), NativeBounded),
            (b"DSD -------------".as_slice(), NativeBounded),
            (b"FRM8\0\0\0\0\0\0\0\0DSD ".as_slice(), NativeBounded),
            (b"OggS------------".as_slice(), Ogg),
            (&0x1a45_dfa3_u32.to_be_bytes(), Matroska),
            (b"fLaC------------".as_slice(), Flac),
            (b"\0\0\0\x18ftypM4A ----".as_slice(), IsoBmff),
            (b"ID3\x04\0\0\0\0\0\0".as_slice(), RawMpegOrAdts),
            (
                b"FORM\0\0\0\0AIFF----".as_slice(),
                UnsupportedMetadataContainer,
            ),
            (
                b"FORM\0\0\0\0AIFC----".as_slice(),
                UnsupportedMetadataContainer,
            ),
            (b"caff------------".as_slice(), UnsupportedMetadataContainer),
        ] {
            assert_eq!(
                service_container_preflight_sniff(prefix, prefix.len() as u64),
                expected,
                "{prefix:?}"
            );
        }

        // Every controlled Symphonia route is an explicit opt-in to exactly
        // one format reader. A future route makes the exhaustive builder fail
        // to compile until its allocation preflight is reviewed.
        for route in [
            ServiceContainerPreflightRoute::Ogg,
            ServiceContainerPreflightRoute::Matroska,
            ServiceContainerPreflightRoute::Flac,
            ServiceContainerPreflightRoute::IsoBmff,
            ServiceContainerPreflightRoute::Mpa,
            ServiceContainerPreflightRoute::Adts,
        ] {
            service_symphonia_probe(route).unwrap();
        }
        assert!(service_symphonia_probe(ServiceContainerPreflightRoute::NativeBounded).is_err());

        // Keep the retained-leaf registry tied to every Binary/String schema
        // ID in the pinned Matroska reader, excluding skip-only framing and
        // packet-bearing elements. Adding format support or a retained leaf
        // requires an explicit review of this table.
        for id in [
            0x4282, 0x4283, 0x465c, 0x467e, 0x4660, 0x466e, 0x4675, 0x6933, 0x450d, 0x437e, 0x437c,
            0x437d, 0x85, 0x6e67, 0x5654, 0x45e4, 0x4521, 0x69a5, 0x4d80, 0x3e83bb, 0x3eb923,
            0x3c83ab, 0x3cb923, 0x4444, 0x7384, 0x73a4, 0x7ba9, 0x5741, 0x53ab, 0x4485, 0x447a,
            0x447b, 0x45a3, 0x4487, 0x63ca, 0x7d7b, 0x41ed, 0x41a4, 0x26b240, 0x86, 0x3b4040,
            0x258688, 0x63a2, 0x3a9697, 0x4255, 0x47e2, 0x47e4, 0x47e3, 0x22b59c, 0x22b59d, 0x536e,
            0x66a5, 0xc4, 0xc1, 0x7672, 0x2eb524,
        ] {
            assert!(service_ebml_retained_metadata_leaf(id), "missing {id:#x}");
        }
        for id in [0xec, 0xbf, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xaf] {
            assert!(
                !service_ebml_retained_metadata_leaf(id),
                "misclassified {id:#x}"
            );
        }
        for id in [
            0x1a45dfa3, 0x4281, 0x18538067, 0x1941a469, 0x61a7, 0x1043a770, 0x45b9, 0xb6, 0x6944,
            0x6911, 0x80, 0x8f, 0x4520, 0x1f43b675, 0xa0, 0x75a1, 0xa6, 0xc8, 0x8e, 0xe8, 0x5854,
            0x1c53bb6b, 0xbb, 0xb7, 0xdb, 0x1549a966, 0x6924, 0x114d9b74, 0x4dbb, 0x1254c367,
            0x7373, 0x67c8, 0x63c0, 0x1654ae6b, 0xae, 0xe1, 0x41e4, 0x6d80, 0x6240, 0x5034, 0x5035,
            0x47e7, 0xe2, 0xe3, 0xe4, 0xe9, 0x6624, 0xe0, 0x55b0, 0x55d0, 0x7670,
        ] {
            assert!(service_ebml_master(id), "missing master {id:#x}");
        }
    }

    fn isobmff_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let length = u32::try_from(8 + payload.len()).unwrap();
        let mut bytes = Vec::with_capacity(length as usize);
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn isobmff_extended_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let length = u64::try_from(16 + payload.len()).unwrap();
        let mut bytes = Vec::with_capacity(length as usize);
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn isobmff_nested_box(kind: &[u8; 4], child: Vec<u8>) -> Vec<u8> {
        isobmff_box(kind, &child)
    }

    fn isobmff_preflight_file(metadata: Vec<u8>) -> Vec<u8> {
        let mut bytes = isobmff_box(b"ftyp", b"M4A \0\0\0\0M4A ");
        bytes.extend(isobmff_box(b"moov", &metadata));
        bytes.extend(isobmff_box(b"mdat", &[]));
        bytes
    }

    #[test]
    fn service_isobmff_preflight_rejects_oversized_stsz_before_demux() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized-stsz.m4a");
        let mut stsz = vec![0; 12];
        stsz[4..8].copy_from_slice(&((SERVICE_MAX_ENCODED_PACKET_BYTES + 1) as u32).to_be_bytes());
        stsz[8..12].copy_from_slice(&1_u32.to_be_bytes());
        let stbl = isobmff_nested_box(b"stbl", isobmff_box(b"stsz", &stsz));
        let minf = isobmff_nested_box(b"minf", stbl);
        let mdia = isobmff_nested_box(b"mdia", minf);
        let trak = isobmff_nested_box(b"trak", mdia);
        std::fs::write(&path, isobmff_preflight_file(trak)).unwrap();

        let error = service_container_preflight(&path, || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);

        let mut variable_stsz = vec![0; 16];
        variable_stsz[8..12].copy_from_slice(&1_u32.to_be_bytes());
        variable_stsz[12..16]
            .copy_from_slice(&((SERVICE_MAX_ENCODED_PACKET_BYTES + 1) as u32).to_be_bytes());
        let metadata = isobmff_nested_box(
            b"trak",
            isobmff_nested_box(
                b"mdia",
                isobmff_nested_box(
                    b"minf",
                    isobmff_nested_box(b"stbl", isobmff_box(b"stsz", &variable_stsz)),
                ),
            ),
        );
        std::fs::write(&path, isobmff_preflight_file(metadata)).unwrap();
        let error = service_container_preflight(&path, || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);
    }

    #[test]
    fn service_isobmff_preflight_accepts_compact_and_fragmented_sample_tables() {
        let directory = tempfile::tempdir().unwrap();
        let compact_path = directory.path().join("compact-stz2.m4a");
        let mut stz2 = vec![0; 14];
        stz2[7] = 8;
        stz2[8..12].copy_from_slice(&2_u32.to_be_bytes());
        stz2[12..14].copy_from_slice(&[7, 9]);
        let metadata = isobmff_nested_box(
            b"trak",
            isobmff_nested_box(
                b"mdia",
                isobmff_nested_box(
                    b"minf",
                    isobmff_nested_box(b"stbl", isobmff_box(b"stz2", &stz2)),
                ),
            ),
        );
        std::fs::write(&compact_path, isobmff_preflight_file(metadata)).unwrap();
        service_container_preflight(&compact_path, || Ok(())).unwrap();

        let fragmented_path = directory.path().join("fragmented.m4a");
        let mut trex = vec![0; 24];
        trex[4..8].copy_from_slice(&7_u32.to_be_bytes());
        trex[16..20].copy_from_slice(&4096_u32.to_be_bytes());
        let mvex = isobmff_nested_box(b"mvex", isobmff_box(b"trex", &trex));
        let mut bytes = isobmff_box(b"ftyp", b"M4A \0\0\0\0M4A ");
        bytes.extend(isobmff_box(b"moov", &mvex));
        let mut tfhd = vec![0; 8];
        tfhd[1..4].copy_from_slice(&[0x02, 0x00, 0x00]); // default-base-is-moof
        tfhd[4..8].copy_from_slice(&7_u32.to_be_bytes());
        let mut trun = vec![0; 8];
        trun[4..8].copy_from_slice(&3_u32.to_be_bytes());
        let mut traf = isobmff_box(b"tfhd", &tfhd);
        traf.extend(isobmff_box(b"trun", &trun));
        bytes.extend(isobmff_box(b"moof", &isobmff_box(b"traf", &traf)));
        bytes.extend(isobmff_box(b"mdat", &[0; 12_288]));
        std::fs::write(&fragmented_path, bytes).unwrap();
        service_container_preflight(&fragmented_path, || Ok(())).unwrap();
    }

    #[test]
    fn service_isobmff_preflight_rejects_oversized_fragment_sample() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized-fragment.m4a");
        let mut tfhd = vec![0; 8];
        tfhd[1..4].copy_from_slice(&[0x02, 0x00, 0x00]);
        tfhd[4..8].copy_from_slice(&1_u32.to_be_bytes());
        let mut trun = vec![0; 12];
        trun[1..4].copy_from_slice(&[0, 0x02, 0]); // sample-size-present
        trun[4..8].copy_from_slice(&1_u32.to_be_bytes());
        trun[8..12].copy_from_slice(&((SERVICE_MAX_ENCODED_PACKET_BYTES + 1) as u32).to_be_bytes());
        let mut traf = isobmff_box(b"tfhd", &tfhd);
        traf.extend(isobmff_box(b"trun", &trun));
        let mut bytes = isobmff_box(b"ftyp", b"M4A \0\0\0\0M4A ");
        bytes.extend(isobmff_box(b"moof", &isobmff_box(b"traf", &traf)));
        bytes.extend(isobmff_box(b"mdat", &[]));
        std::fs::write(&path, bytes).unwrap();

        let error = service_container_preflight(&path, || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);
    }

    #[test]
    fn service_isobmff_preflight_bounds_nested_metadata_items() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata.m4a");

        let data = isobmff_box(b"data", b"\0\0\0\x01\0\0\0\0title");
        let unknown = isobmff_box(b"xtra", b"preserved");
        let item = isobmff_box(b"\xa9nam", &[data, unknown].concat());
        let ilst = isobmff_box(b"ilst", &item);
        let meta = isobmff_box(b"meta", &[vec![0; 4], ilst].concat());
        let udta = isobmff_box(b"udta", &meta);
        std::fs::write(&path, isobmff_preflight_file(udta)).unwrap();
        service_container_preflight(&path, || Ok(())).unwrap();

        for (name, data) in [
            (
                "huge-metadata.m4a",
                isobmff_box(
                    b"data",
                    &vec![0; usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1).unwrap()],
                ),
            ),
            (
                "huge-extended-metadata.m4a",
                isobmff_extended_box(
                    b"mean",
                    &vec![0; usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1).unwrap()],
                ),
            ),
        ] {
            let item = isobmff_box(b"\xa9nam", &data);
            let ilst = isobmff_box(b"ilst", &item);
            let meta = isobmff_box(b"meta", &[vec![0; 4], ilst].concat());
            let udta = isobmff_box(b"udta", &meta);
            let path = directory.path().join(name);
            std::fs::write(&path, isobmff_preflight_file(udta)).unwrap();
            assert_eq!(
                service_container_preflight(&path, || Ok(())).unwrap_err(),
                SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
            );
        }

        let direct = directory.path().join("huge-direct-udta-data.m4a");
        let data = isobmff_box(
            b"data",
            &vec![0; usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1).unwrap()],
        );
        std::fs::write(&direct, isobmff_preflight_file(isobmff_box(b"udta", &data))).unwrap();
        assert_eq!(
            service_container_preflight(&direct, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let mut truncated_data = Vec::new();
        truncated_data.extend_from_slice(&64_u32.to_be_bytes());
        truncated_data.extend_from_slice(b"data");
        let item = isobmff_box(b"\xa9nam", &truncated_data);
        let ilst = isobmff_box(b"ilst", &item);
        let meta = isobmff_box(b"meta", &[vec![0; 4], ilst].concat());
        let udta = isobmff_box(b"udta", &meta);
        let truncated = directory.path().join("truncated-metadata.m4a");
        std::fs::write(&truncated, isobmff_preflight_file(udta)).unwrap();
        let error = service_container_preflight(&truncated, || Ok(())).unwrap_err();
        assert!(error.contains("exceeds its parent"), "{error}");
    }

    #[test]
    fn service_isobmff_preflight_bounds_ilst_children_and_aggregate_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let payload_len = usize::try_from(SERVICE_MAX_METADATA_ITEM_BYTES - 8).unwrap();
        let data = isobmff_box(b"data", &vec![0; payload_len]);

        let build_ilst = |count: usize| {
            let mut ilst = Vec::new();
            for _ in 0..count {
                ilst.extend(isobmff_box(b"covr", &data));
            }
            ilst
        };

        let accepted = build_ilst(16);
        let accepted_path = directory.path().join("metadata-total-boundary.bin");
        std::fs::write(&accepted_path, &accepted).unwrap();
        let mut accepted_file = File::open(&accepted_path).unwrap();
        preflight_isobmff_metadata_region(
            &accepted_path,
            &mut accepted_file,
            0,
            accepted.len() as u64,
            0,
            ServiceIsoBmffMetadataRegion::ItemList,
            &mut ServiceIsoBmffPreflight::default(),
            &mut || Ok(()),
        )
        .unwrap();

        let rejected = build_ilst(17);
        let rejected_path = directory.path().join("metadata-total-over.bin");
        std::fs::write(&rejected_path, &rejected).unwrap();
        let mut rejected_file = File::open(&rejected_path).unwrap();
        assert_eq!(
            preflight_isobmff_metadata_region(
                &rejected_path,
                &mut rejected_file,
                0,
                rejected.len() as u64,
                0,
                ServiceIsoBmffMetadataRegion::ItemList,
                &mut ServiceIsoBmffPreflight::default(),
                &mut || Ok(()),
            )
            .unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );

        let mut budget = ServiceMetadataBudget::default();
        budget.add_entries(SERVICE_MAX_CONTAINER_ITEMS).unwrap();
        assert_eq!(
            budget.add_entries(1).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn service_raw_preflight_rejects_unknown_prefix_and_bounds_id3() {
        let directory = tempfile::tempdir().unwrap();

        let disguised = directory.path().join("disguised.audio");
        let mut bytes = b"junk".to_vec();
        bytes.extend_from_slice(&0x1a45_dfa3_u32.to_be_bytes());
        std::fs::write(&disguised, &bytes).unwrap();
        let error = service_container_preflight(&disguised, || Ok(())).unwrap_err();
        assert!(
            error.contains("no bounded audio-container signature"),
            "{error}"
        );

        let oversized = directory.path().join("oversized-id3.mp3");
        let oversized_header = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f";
        std::fs::write(&oversized, oversized_header).unwrap();
        let error = service_container_preflight(&oversized, || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);

        let tagged = directory.path().join("bounded-id3.mp3");
        let mut tagged_bytes = b"ID3\x04\x00\x00\x00\x00\x00\x03tag".to_vec();
        tagged_bytes.extend_from_slice(&silent_mpeg1_layer3_frame(0));
        std::fs::write(&tagged, &tagged_bytes).unwrap();
        let mut checkpoints = 0;
        service_container_preflight(&tagged, || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(checkpoints, 4);
    }

    fn test_syncsafe(value: usize) -> [u8; 4] {
        assert!(value < (1 << 28));
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    fn service_id3_tag(version: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut header = b"ID3\0\0\0\0\0\0\0".to_vec();
        header[3] = version;
        header[5] = flags;
        header[6..10].copy_from_slice(&test_syncsafe(body.len()));
        header.extend_from_slice(body);
        if version == 4 && flags & 0x10 != 0 {
            let mut footer = header[..10].to_vec();
            footer[..3].copy_from_slice(b"3DI");
            header.extend(footer);
        }
        header
    }

    #[test]
    fn service_raw_audio_requires_complete_mpeg_or_adts_geometry() {
        let mpeg = silent_mpeg1_layer3_frame(0);
        assert_eq!(
            service_raw_audio_frame_bytes(&mpeg[..16], mpeg.len() as u64),
            Some(417)
        );
        assert_eq!(service_raw_audio_frame_bytes(&[0xff, 0xfb], 2), None);
        assert_eq!(
            service_raw_audio_frame_bytes(&[0xff, 0xfb, 0, 0], 4096),
            None
        );

        let adts = [0xff, 0xf1, 0x50, 0x80, 0x00, 0xff, 0xfc];
        assert_eq!(
            service_raw_audio_frame_bytes(&adts, adts.len() as u64),
            Some(7)
        );
        let mut bad_adts = adts;
        bad_adts[2] = 0x7c; // Reserved sample-rate index.
        assert_eq!(service_raw_audio_frame_bytes(&bad_adts, 7), None);
    }

    #[test]
    fn service_raw_preflight_validates_the_complete_frame_chain_and_audio_offset() {
        let directory = tempfile::tempdir().unwrap();
        for count in [1_usize, 2, 65] {
            let path = directory.path().join(format!("{count}-frames.mp3"));
            let bytes = silent_mp3_stream(&vec![0; count]);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                service_container_preflight(&path, || Ok(())).unwrap(),
                ServiceContainerPreflight {
                    route: ServiceContainerPreflightRoute::Mpa,
                    media_offset: 0,
                    media_end: u64::try_from(bytes.len()).unwrap(),
                }
            );
        }

        let adts_frame = [0xff, 0xf1, 0x50, 0x80, 0x00, 0xff, 0xfc];
        for count in [1_usize, 2, 65] {
            let path = directory.path().join(format!("{count}-frames.aac"));
            let bytes = adts_frame.repeat(count);
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                service_container_preflight(&path, || Ok(())).unwrap(),
                ServiceContainerPreflight {
                    route: ServiceContainerPreflightRoute::Adts,
                    media_offset: 0,
                    media_end: u64::try_from(bytes.len()).unwrap(),
                }
            );
        }

        let tag = service_id3_tag(4, 0, &[]);
        let mut tagged = tag.clone();
        tagged.extend(silent_mp3_stream(&[0, 0]));
        let path = directory.path().join("leading-id3.mp3");
        std::fs::write(&path, &tagged).unwrap();
        assert_eq!(
            service_container_preflight(&path, || Ok(())).unwrap(),
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Mpa,
                media_offset: tag.len() as u64,
                media_end: tagged.len() as u64,
            }
        );

        let stable = StableInput::from_path(
            &path,
            &StableInputOptions::new(u64::try_from(tagged.len()).unwrap()).unwrap(),
        )
        .unwrap();
        let descriptor = InputDescriptor::probe_with_control(
            stable,
            InputDescriptorOptions::default(),
            10_000,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(descriptor.container(), AudioContainer::MpegAudio);
        let mut decoded_frames = 0_usize;
        decode_descriptor_analysis_stream_with_control(
            &descriptor,
            10_000,
            || Ok(()),
            |_, _, chunk| {
                decoded_frames += chunk.frames();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(decoded_frames, 2 * 1_152);
    }

    #[test]
    fn controlled_raw_decode_cannot_read_frames_embedded_in_trailing_tags() {
        let directory = tempfile::tempdir().unwrap();
        let audio = silent_mp3_stream(&[0, 0]);
        let clean_path = directory.path().join("clean.mp3");
        std::fs::write(&clean_path, &audio).unwrap();

        // A complete MPEG frame is an APE item value and a complete ADTS
        // frame is present in the ID3v1 value. Neither is part of the admitted
        // half-open media range, even though either byte sequence is a valid
        // raw-audio signature in isolation.
        let embedded_mpeg = silent_mpeg1_layer3_frame(0);
        let mut item = Vec::new();
        item.extend_from_slice(&u32::try_from(embedded_mpeg.len()).unwrap().to_le_bytes());
        item.extend_from_slice(&0_u32.to_le_bytes());
        item.extend_from_slice(b"Payload\0");
        item.extend_from_slice(&embedded_mpeg);
        let tag = ape_v2_tag(&item);
        let mut id3v1 = [0_u8; 128];
        id3v1[..3].copy_from_slice(b"TAG");
        id3v1[3..10].copy_from_slice(&[0xff, 0xf1, 0x50, 0x80, 0x00, 0xff, 0xfc]);
        let mut tagged = audio.clone();
        tagged.extend_from_slice(&tag);
        tagged.extend_from_slice(&id3v1);
        let tagged_path = directory.path().join("tag-values-contain-frames.mp3");
        std::fs::write(&tagged_path, &tagged).unwrap();

        assert_eq!(
            service_container_preflight(&tagged_path, || Ok(())).unwrap(),
            ServiceContainerPreflight {
                route: ServiceContainerPreflightRoute::Mpa,
                media_offset: 0,
                media_end: u64::try_from(audio.len()).unwrap(),
            }
        );

        let analyze = |path: &Path| {
            let input_len = std::fs::metadata(path).unwrap().len();
            let stable =
                StableInput::from_path(path, &StableInputOptions::new(input_len).unwrap()).unwrap();
            let descriptor = InputDescriptor::probe_with_control(
                stable,
                InputDescriptorOptions::default(),
                100_000,
                || Ok(()),
            )
            .unwrap();
            assert_eq!(
                descriptor.service_preflight,
                Some(service_container_preflight(path, || Ok(())).unwrap())
            );
            let info = descriptor.stream_info().clone();
            let mut analyzer = crate::dsp::lufs::StreamingAnalyzer::new(
                info.sample_rate,
                descriptor.channel_layout().channel_roles(),
            );
            let mut decoded_frames = 0_usize;
            decode_descriptor_analysis_stream_with_control(
                &descriptor,
                100_000,
                || Ok(()),
                |_, _, chunk| {
                    let AnalysisPcmChunk::F32(planar) = chunk else {
                        panic!("MPEG analysis must use the f32 lane");
                    };
                    decoded_frames += planar.first().map_or(0, Vec::len);
                    analyzer.process(planar)
                },
            )
            .unwrap();
            (decoded_frames, analyzer.finish())
        };

        let (clean_frames, clean) = analyze(&clean_path);
        let (tagged_frames, tagged) = analyze(&tagged_path);
        assert_eq!(clean_frames, 2 * 1_152);
        assert_eq!(tagged_frames, clean_frames);
        assert_eq!(tagged.frames, clean.frames);
        assert_eq!(
            tagged.ebu.integrated_lufs.to_bits(),
            clean.ebu.integrated_lufs.to_bits()
        );
        assert_eq!(
            tagged.weighted_mean_square.to_bits(),
            clean.weighted_mean_square.to_bits()
        );
        assert_eq!(tagged.rms_db.to_bits(), clean.rms_db.to_bits());
        assert_eq!(tagged.sample_peak.to_bits(), clean.sample_peak.to_bits());
        assert_eq!(tagged.true_peak.to_bits(), clean.true_peak.to_bits());
        assert_eq!(tagged.ebu.gating_blocks, clean.ebu.gating_blocks);
    }

    #[test]
    fn trailing_ape_before_non_id3v1_suffix_is_not_excluded_from_audio() {
        let directory = tempfile::tempdir().unwrap();
        let mut bytes = silent_mpeg1_layer3_frame(0);
        bytes.extend(ape_v2_tag(&[]));
        bytes.extend_from_slice(&[b'X'; 128]);
        let path = directory.path().join("ape-before-non-id3v1.mp3");
        std::fs::write(&path, bytes).unwrap();

        let error = service_container_preflight(&path, || Ok(())).unwrap_err();
        assert!(
            error.contains("inter-frame data or invalid geometry"),
            "{error}"
        );
    }

    #[test]
    fn service_raw_preflight_rejects_in_band_metadata_and_route_confusion() {
        let directory = tempfile::tempdir().unwrap();
        let mut huge_ape_item = Vec::new();
        huge_ape_item.extend_from_slice(
            &u32::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1)
                .unwrap()
                .to_le_bytes(),
        );
        huge_ape_item.extend_from_slice(&0_u32.to_le_bytes());
        huge_ape_item.extend_from_slice(b"Title\0");

        let huge_id3 = b"ID3\x04\0\0\x7f\x7f\x7f\x7f".to_vec();
        let huge_ape = ape_v2_tag(&huge_ape_item);
        for (codec, frame) in [
            ("mp3", silent_mpeg1_layer3_frame(0)),
            ("adts", vec![0xff, 0xf1, 0x50, 0x80, 0x00, 0xff, 0xfc]),
        ] {
            for (metadata_kind, metadata) in
                [("id3", huge_id3.as_slice()), ("ape", huge_ape.as_slice())]
            {
                let mut in_band = frame.clone();
                in_band.extend_from_slice(metadata);
                in_band.extend_from_slice(&frame);
                let path = directory
                    .path()
                    .join(format!("in-band-{metadata_kind}.{codec}"));
                std::fs::write(&path, in_band).unwrap();
                assert!(service_container_preflight(&path, || Ok(())).is_err());
            }
        }

        let raw_inputs = [
            ("mp3", silent_mpeg1_layer3_frame(0), vec![0xff, 0xfb, 0, 0]),
            (
                "adts",
                vec![0xff, 0xf1, 0x50, 0x80, 0x00, 0xff, 0xfc],
                vec![0xff, 0xf1, 0x7c, 0, 0, 0, 0],
            ),
        ];
        for (codec, first, invalid) in raw_inputs {
            for (name, marker) in [
                ("ebml", b"\x1a\x45\xdf\xa3\x01\xff\xff\xff".as_slice()),
                ("isobmff", b"\xff\xff\xff\xffmoov".as_slice()),
                ("ogg", b"OggS\0\0\0\0\0\0\0\0".as_slice()),
                ("flac", b"fLaC\x04\xff\xff\xff".as_slice()),
            ] {
                let mut confused = first.clone();
                confused.extend_from_slice(&invalid);
                confused.extend_from_slice(marker);
                let path = directory
                    .path()
                    .join(format!("route-confusion-{codec}-{name}.bin"));
                std::fs::write(&path, confused).unwrap();
                let error = service_container_preflight(&path, || Ok(())).unwrap_err();
                assert!(
                    error.contains("inter-frame data or invalid geometry"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn service_probe_cannot_fall_back_to_an_unregistered_container() {
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::FormatOptions;
        use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};

        let probe = service_symphonia_probe(ServiceContainerPreflightRoute::Mpa).unwrap();
        let wrong_container = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(b"OggS----------------".to_vec())),
            MediaSourceStreamOptions::default(),
        );
        assert!(probe
            .probe(
                &Hint::new(),
                wrong_container,
                FormatOptions::default(),
                service_metadata_options(),
            )
            .is_err());

        let ogg_probe = service_symphonia_probe(ServiceContainerPreflightRoute::Ogg).unwrap();
        let reverse_mismatch = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(silent_mp3_stream(&[0, 0, 0]))),
            MediaSourceStreamOptions::default(),
        );
        assert!(ogg_probe
            .probe(
                &Hint::new(),
                reverse_mismatch,
                FormatOptions::default(),
                service_metadata_options(),
            )
            .is_err());

        let expected = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(silent_mp3_stream(&[0, 0, 0]))),
            MediaSourceStreamOptions::default(),
        );
        let format = probe
            .probe(
                &Hint::new(),
                expected,
                FormatOptions::default(),
                service_metadata_options(),
            )
            .unwrap();
        assert_eq!(
            audio_container_from_symphonia(format.format_info().format),
            Some(AudioContainer::MpegAudio)
        );
    }

    #[test]
    fn service_id3_scans_every_version_and_rejects_false_sync_and_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let frames = [
            (2, 0, [b"TT2".as_slice(), &[0, 0, 1], b"x"].concat()),
            (
                3,
                0,
                [b"TIT2".as_slice(), &1_u32.to_be_bytes(), &[0, 0], b"x"].concat(),
            ),
            (
                4,
                0x10,
                [b"TIT2".as_slice(), &test_syncsafe(1), &[0, 0], b"x"].concat(),
            ),
        ];
        for (version, flags, body) in frames {
            let mut bytes = service_id3_tag(version, flags, &body);
            bytes.extend(silent_mpeg1_layer3_frame(0));
            let path = directory.path().join(format!("v{version}.mp3"));
            std::fs::write(&path, &bytes).unwrap();
            service_container_preflight(&path, || Ok(())).unwrap();
        }

        let mut v3_extended = 6_u32.to_be_bytes().to_vec();
        v3_extended.extend_from_slice(&[0; 6]);
        v3_extended.extend_from_slice(b"TIT2");
        v3_extended.extend_from_slice(&1_u32.to_be_bytes());
        v3_extended.extend_from_slice(&[0, 0, b'x']);
        let mut bytes = service_id3_tag(3, 0x40, &v3_extended);
        bytes.extend(silent_mpeg1_layer3_frame(0));
        let path = directory.path().join("v3-extended.mp3");
        std::fs::write(&path, &bytes).unwrap();
        service_container_preflight(&path, || Ok(())).unwrap();

        let mut v4_extended = test_syncsafe(6).to_vec();
        v4_extended.extend_from_slice(&[1, 0]);
        v4_extended.extend_from_slice(b"TIT2");
        v4_extended.extend_from_slice(&test_syncsafe(1));
        v4_extended.extend_from_slice(&[0, 0, b'x']);
        let mut bytes = service_id3_tag(4, 0x40, &v4_extended);
        bytes.extend(silent_mpeg1_layer3_frame(0));
        let path = directory.path().join("v4-extended.mp3");
        std::fs::write(&path, &bytes).unwrap();
        service_container_preflight(&path, || Ok(())).unwrap();

        let mut unsynchronised = b"TIT2".to_vec();
        unsynchronised.extend_from_slice(&2_u32.to_be_bytes());
        unsynchronised.extend_from_slice(&[0, 0, 0xff, 0, 0xe0]);
        let mut bytes = service_id3_tag(3, 0x80, &unsynchronised);
        bytes.extend(silent_mpeg1_layer3_frame(0));
        let path = directory.path().join("v3-unsynchronised.mp3");
        std::fs::write(&path, &bytes).unwrap();
        service_container_preflight(&path, || Ok(())).unwrap();

        let truncated_body = [b"TIT2".as_slice(), &8_u32.to_be_bytes(), &[0, 0]].concat();
        let truncated = service_id3_tag(3, 0, &truncated_body);
        let path = directory.path().join("truncated-frame.mp3");
        std::fs::write(&path, truncated).unwrap();
        assert!(service_container_preflight(&path, || Ok(())).is_err());

        let mut huge_ape_item = Vec::new();
        huge_ape_item.extend_from_slice(
            &u32::try_from(SERVICE_MAX_METADATA_ITEM_BYTES + 1)
                .unwrap()
                .to_le_bytes(),
        );
        huge_ape_item.extend_from_slice(&0_u32.to_le_bytes());
        huge_ape_item.extend_from_slice(b"Title\0");
        let mut false_sync = service_id3_tag(4, 0, &[]);
        false_sync.extend_from_slice(&[0xff, 0xfb, 0, 0]);
        false_sync.extend(ape_v2_tag(&huge_ape_item));
        let path = directory.path().join("false-sync-before-ape.mp3");
        std::fs::write(&path, false_sync).unwrap();
        let error = service_container_preflight(&path, || Ok(())).unwrap_err();
        assert_eq!(error, SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED);

        let mut leading_ape = ape_v2_tag(&[]);
        leading_ape.extend_from_slice(&[0xff, 0xfb, 0, 0]);
        leading_ape.extend_from_slice(b"ID3\x04\0\0\x7f\x7f\x7f\x7f");
        let path = directory
            .path()
            .join("leading-ape-false-sync-before-id3.mp3");
        std::fs::write(&path, leading_ape).unwrap();
        let error = service_container_preflight(&path, || Ok(())).unwrap_err();
        assert!(
            error.contains("no bounded audio-container signature"),
            "{error}"
        );
    }

    #[test]
    fn service_id3_frame_count_is_bounded_before_frame_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let build = |count: usize| {
            let mut body = Vec::with_capacity(count * 6);
            for _ in 0..count {
                body.extend_from_slice(b"TT2\0\0\0");
            }
            let mut bytes = service_id3_tag(2, 0, &body);
            bytes.extend(silent_mpeg1_layer3_frame(0));
            bytes
        };
        let boundary = build(SERVICE_MAX_CONTAINER_ITEMS);
        let path = directory.path().join("id3-frame-boundary.mp3");
        std::fs::write(&path, boundary).unwrap();
        service_container_preflight(&path, || Ok(())).unwrap();

        let over = build(SERVICE_MAX_CONTAINER_ITEMS + 1);
        let path = directory.path().join("id3-frame-over.mp3");
        std::fs::write(&path, over).unwrap();
        assert_eq!(
            service_container_preflight(&path, || Ok(())).unwrap_err(),
            SERVICE_ENCODED_PACKET_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn controlled_symphonia_probe_checks_underlying_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controlled.flac");
        write_silent_test_flac(&path, 1, 32);
        let preflight = service_container_preflight(&path, || Ok(())).unwrap();

        let mut checkpoints = 0;
        let result = probe_symphonia_identity_at_controlled(
            &path,
            Some(&path),
            &path.display().to_string(),
            AudioTrackSelection::Default,
            preflight,
            || {
                checkpoints += 1;
                if checkpoints == 2 {
                    Err("controlled Symphonia I/O stopped".into())
                } else {
                    Ok(())
                }
            },
        );
        let error = match result {
            Ok(_) => panic!("controlled probe unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(
            error.contains("controlled Symphonia I/O stopped"),
            "{error}"
        );
        assert_eq!(checkpoints, 2);
    }

    #[test]
    fn controlled_media_source_splits_large_scalar_and_vectored_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-controlled-read.bin");
        std::fs::write(&path, vec![0x5a; SERVICE_CONTROLLED_READ_BYTES * 3]).unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&calls);
        let checkpoint = Mutex::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        let file = File::open(&path).unwrap();
        let base = 11_u64;
        let admitted_bytes = u64::try_from(SERVICE_CONTROLLED_READ_BYTES * 2).unwrap();
        let mut source =
            CheckpointMediaSource::new_range(file, &checkpoint, base, base + admitted_bytes)
                .unwrap();
        assert_eq!(
            symphonia::core::io::MediaSource::byte_len(&source),
            Some(admitted_bytes)
        );

        let mut scalar = vec![0_u8; SERVICE_CONTROLLED_READ_BYTES * 2];
        assert_eq!(
            source.read(&mut scalar).unwrap(),
            SERVICE_CONTROLLED_READ_BYTES
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

        let mut first = vec![0_u8; SERVICE_CONTROLLED_READ_BYTES * 2];
        let mut second = [0_u8; 16];
        let mut outputs = [IoSliceMut::new(&mut first), IoSliceMut::new(&mut second)];
        assert_eq!(
            source.read_vectored(&mut outputs).unwrap(),
            SERVICE_CONTROLLED_READ_BYTES
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);

        let mut byte = [0_u8; 1];
        assert_eq!(source.read(&mut byte).unwrap(), 0);
        assert_eq!(source.seek(SeekFrom::End(0)).unwrap(), admitted_bytes);
        assert!(source.seek(SeekFrom::End(1)).is_err());
        assert!(source.seek(SeekFrom::Start(admitted_bytes + 1)).is_err());
        assert_eq!(source.seek(SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(source.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, [0x5a]);
    }

    fn decode_flac_with_workers(
        path: &Path,
        workers: usize,
    ) -> (StreamInfo, ChannelLayoutProvenance, Vec<Vec<f32>>) {
        let mut samples = Vec::new();
        let mut observed_provenance = None;
        let info =
            decode_stream_with_flac_workers(path, Some(workers), |info, provenance, _, planar| {
                assert!(observed_provenance
                    .replace(provenance)
                    .is_none_or(|previous| previous == provenance));
                if samples.is_empty() {
                    samples = vec![Vec::new(); info.channels as usize];
                }
                for (destination, source) in samples.iter_mut().zip(planar) {
                    destination.extend_from_slice(source);
                }
                Ok(())
            })
            .unwrap();
        (info, observed_provenance.unwrap(), samples)
    }

    fn write_silent_test_flac(path: &Path, channels: u16, frames: usize) {
        let mut writer =
            crate::flacenc::FlacStreamWriter::create(path, 48_000, channels, 16, false).unwrap();
        writer
            .write_chunk(&vec![vec![0.0; frames]; usize::from(channels)])
            .unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn descriptor_pcm_contract_is_independent_of_a_misleading_suffix() {
        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("programme.flac");
        let misleading = directory.path().join("programme.wav");
        write_silent_test_flac(&regular, 2, 48_000);
        std::fs::copy(&regular, &misleading).unwrap();
        let options = StableInputOptions::new(u64::MAX).unwrap();
        let regular =
            InputDescriptor::from_path(&regular, &options, InputDescriptorOptions::default())
                .unwrap();
        let misleading =
            InputDescriptor::from_path(&misleading, &options, InputDescriptorOptions::default())
                .unwrap();

        assert_eq!(regular.container(), AudioContainer::Flac);
        assert_eq!(regular.codec(), AudioCodec::Flac);
        assert_eq!(regular.decoder_route_id(), misleading.decoder_route_id());
        assert_eq!(regular.stream_info().source_kind, PcmKind::F32);
        assert_eq!(
            regular.stream_info().source_kind,
            misleading.stream_info().source_kind
        );
    }

    fn inject_flac_comments(path: &Path, comments: &[&str]) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..4], b"fLaC");
        assert_eq!(bytes[4] & 0x7f, 0, "first block must be STREAMINFO");
        assert_ne!(bytes[4] & 0x80, 0, "test writer should emit one block");
        let streaminfo_len = u32::from_be_bytes([0, bytes[5], bytes[6], bytes[7]]) as usize;
        let insert_at = 8 + streaminfo_len;
        assert!(insert_at <= bytes.len());

        let vendor = b"forge-decoder-tests";
        let mut payload = Vec::new();
        payload.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        payload.extend_from_slice(vendor);
        payload.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for comment in comments {
            payload.extend_from_slice(&(comment.len() as u32).to_le_bytes());
            payload.extend_from_slice(comment.as_bytes());
        }
        assert!(payload.len() <= 0x00ff_ffff);

        let mut result = Vec::with_capacity(bytes.len() + 4 + payload.len());
        result.extend_from_slice(&bytes[..insert_at]);
        result[4] &= 0x7f;
        result.push(0x80 | 4); // Last metadata block, VORBIS_COMMENT.
        result.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        result.extend_from_slice(&payload);
        result.extend_from_slice(&bytes[insert_at..]);
        std::fs::write(path, result).unwrap();
    }

    #[test]
    fn native_flac_absent_and_explicit_default_masks_remain_known() {
        let directory = tempfile::tempdir().unwrap();
        let absent = directory.path().join("absent.flac");
        write_silent_test_flac(&absent, 2, 137);
        let (decoded, provenance) = decode_with_layout(&absent).unwrap();
        assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(decoded.channel_roles, default_channel_roles(2));

        let explicit = directory.path().join("explicit.flac");
        write_silent_test_flac(&explicit, 2, 137);
        inject_flac_comments(
            &explicit,
            &[
                "waveformatextensible_channel_mask=0X000000000003",
                "WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x3",
            ],
        );
        let (decoded, provenance) = decode_with_layout(&explicit).unwrap();
        assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(decoded.channel_roles, default_channel_roles(2));

        let six_channel = directory.path().join("absent-six-channel.flac");
        write_silent_test_flac(&six_channel, 6, 137);
        let (decoded, provenance) = decode_with_layout(&six_channel).unwrap();
        assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(
            decoded.channel_roles,
            crate::wav::reader::roles_from_wave_mask(0x0000_003f, 6)
        );
        assert_eq!(decoded.channel_roles[4], ChannelRole::positioned(-110, 0));
        assert_eq!(decoded.channel_roles[5], ChannelRole::positioned(110, 0));
    }

    #[test]
    fn native_flac_non_default_complete_masks_are_authoritative_for_analysis() {
        let directory = tempfile::tempdir().unwrap();
        let mono_lfe = directory.path().join("mono-lfe.flac");
        write_silent_test_flac(&mono_lfe, 1, 137);
        inject_flac_comments(&mono_lfe, &["waveformatextensible_channel_mask=0X00000008"]);
        let (decoded, provenance) = decode_with_layout(&mono_lfe).unwrap();
        assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.channel_roles, vec![ChannelRole::Lfe]);
        assert_eq!(decode(&mono_lfe).unwrap().frames, 137);
        assert!(crate::normalize::analyze_file(&mono_lfe).is_ok());
        let descriptor = InputDescriptor::from_path(
            &mono_lfe,
            &StableInputOptions::new(u64::MAX).unwrap(),
            InputDescriptorOptions::default(),
        )
        .unwrap();
        assert_eq!(descriptor.declared_frames(), Some(137));
        assert_eq!(
            descriptor.declared_layout_provenance(),
            ChannelLayoutProvenance::KnownSpeakers
        );
        assert_eq!(
            descriptor.stream_info().channel_roles,
            vec![ChannelRole::Lfe]
        );

        let top = directory.path().join("top.flac");
        write_silent_test_flac(&top, 4, 137);
        inject_flac_comments(&top, &["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x00005003"]);
        let mut callbacks = 0;
        let info = decode_stream_with_layout(&top, |_, provenance, _| {
            callbacks += 1;
            assert_eq!(provenance, ChannelLayoutProvenance::KnownSpeakers);
            Ok(())
        })
        .unwrap();
        assert_eq!(info.channels, 4);
        assert!(callbacks > 0);
    }

    #[test]
    fn input_descriptor_retains_exact_wave_layout_and_rejects_source_as_override() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("non-default.wav");
        let exact = ChannelLayoutDescriptor::wave(4, true, Some(0x0000_5003));
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 4,
            frames: 32,
            data: vec![vec![0.0; 32]; 4],
            channel_roles: exact.channel_roles(),
            source_kind: PcmKind::S16,
        };
        crate::wav::WavWriter::write_with_channel_layout(
            &input,
            &buffer,
            PcmKind::S16,
            false,
            crate::wav::WavContainer::Riff,
            &exact,
        )
        .unwrap();

        let stable = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor =
            InputDescriptor::from_path(&input, &stable, InputDescriptorOptions::default()).unwrap();
        assert_eq!(descriptor.version(), 2);
        assert!(descriptor
            .decoder_route_id()
            .starts_with("forge-input-descriptor-v2:"));
        assert_eq!(descriptor.declared_channel_layout(), &exact);
        assert_eq!(descriptor.channel_layout(), &exact);
        assert_eq!(
            descriptor.channel_layout().wave_channel_mask(),
            Some(0x5003)
        );

        let error = InputDescriptor::from_path(
            &input,
            &stable,
            InputDescriptorOptions::default().with_channel_layout(exact),
        )
        .unwrap_err();
        assert!(error.contains("explicit-override origin"));
    }

    #[test]
    fn native_flac_unusable_mask_metadata_is_unknown_not_a_decode_error() {
        let directory = tempfile::tempdir().unwrap();
        let cases = [
            ("zero", vec!["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x0"]),
            ("partial", vec!["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x1"]),
            (
                "high-bit",
                vec!["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x40000"],
            ),
            (
                "malformed",
                vec!["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x3junk"],
            ),
            (
                "conflict",
                vec![
                    "WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x3",
                    "waveformatextensible_channel_mask=0x4",
                ],
            ),
        ];

        for (name, comments) in cases {
            let path = directory.path().join(format!("{name}.flac"));
            write_silent_test_flac(&path, 2, 137);
            inject_flac_comments(&path, &comments);
            let (decoded, provenance) = decode_with_layout(&path).unwrap();
            assert_eq!(provenance, ChannelLayoutProvenance::Unknown, "{name}");
            assert_eq!(decoded.frames, 137);
            for error in [
                decode(&path).unwrap_err(),
                decode_limited(&path, u64::MAX).unwrap_err(),
            ] {
                assert!(
                    error.contains("ambiguous channel layout"),
                    "{name}: {error}"
                );
            }
        }
    }

    #[test]
    fn native_flac_parallel_decode_matches_serial_packets_bit_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("parallel.flac");
        let frames = 96 * 4_096 + 137;
        let mut planar = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
        for frame in 0..frames {
            planar[0].push(((frame * 97 % 32_000) as f32 - 16_000.0) / 32_768.0);
            planar[1].push(((frame * 131 % 30_000) as f32 - 15_000.0) / 32_768.0);
        }
        let mut writer =
            crate::flacenc::FlacStreamWriter::create(&path, 48_000, 2, 16, false).unwrap();
        writer.write_chunk(&planar).unwrap();
        writer.finish().unwrap();
        // A complete non-default FC+LFE mask exercises authoritative metadata
        // and role propagation in both the serial and native parallel routes.
        inject_flac_comments(&path, &["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x0000000c"]);

        let (serial_info, serial_provenance, serial) = decode_flac_with_workers(&path, 1);
        let (parallel_info, parallel_provenance, parallel) = decode_flac_with_workers(&path, 4);
        let (full, full_provenance) = decode_with_layout(&path).unwrap();
        assert_eq!(serial_provenance, ChannelLayoutProvenance::KnownSpeakers);
        assert_eq!(parallel_provenance, serial_provenance);
        assert_eq!(full_provenance, serial_provenance);
        assert_eq!(
            serial_info.channel_roles,
            [ChannelRole::positioned(0, 0), ChannelRole::Lfe]
        );
        assert_eq!(full.channel_roles, serial_info.channel_roles);
        assert_eq!(full.data, serial);
        assert_eq!(parallel_info.sample_rate, serial_info.sample_rate);
        assert_eq!(parallel_info.channels, serial_info.channels);
        assert_eq!(parallel_info.channel_roles, serial_info.channel_roles);
        assert_eq!(parallel_info.source_kind, serial_info.source_kind);
        assert_eq!(parallel, serial);
        assert_eq!(parallel[0].len(), frames);
    }

    fn codec_params(
        sample_rate: u32,
        layout: symphonia::core::audio::Channels,
    ) -> AudioCodecParameters {
        let mut params = AudioCodecParameters::new();
        params.with_sample_rate(sample_rate).with_channels(layout);
        params
    }

    fn codec_params_for_codec(
        sample_rate: u32,
        layout: symphonia::core::audio::Channels,
        codec: AudioCodecId,
        extra_data_len: Option<usize>,
    ) -> AudioCodecParameters {
        let mut params = codec_params(sample_rate, layout);
        params.for_codec(codec);
        if let Some(len) = extra_data_len {
            params.with_extra_data(vec![0; len].into_boxed_slice());
        }
        params
    }

    #[test]
    fn reset_compatibility_rejects_rate_and_layout_changes() {
        let path = Path::new("fixture.ogg");
        let output = output_format();

        let rate_error = validate_symphonia_track_compatibility(
            path,
            &output,
            &codec_params(44_100, CHANNEL_LAYOUT_STEREO.clone()),
            PcmKind::F32,
            FlacChannelMaskState::Absent,
        )
        .unwrap_err();
        assert!(rate_error.contains("sample rate changed from 48000 to 44100"));

        let layout_error = validate_symphonia_track_compatibility(
            path,
            &output,
            &codec_params(48_000, CHANNEL_LAYOUT_MONO.clone()),
            PcmKind::F32,
            FlacChannelMaskState::Absent,
        )
        .unwrap_err();
        assert!(layout_error.contains("channel count changed from 2 to 1"));
    }

    #[test]
    fn reset_compatibility_rejects_flac_channel_mask_state_changes() {
        let path = Path::new("fixture.oga");
        let mut output = output_format();
        output.flac_channel_mask = FlacChannelMaskState::Valid(0x3);
        let params = codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone());

        validate_symphonia_track_compatibility(
            path,
            &output,
            &params,
            PcmKind::F32,
            FlacChannelMaskState::Valid(0x3),
        )
        .unwrap();

        for changed in [
            FlacChannelMaskState::Absent,
            FlacChannelMaskState::Valid(0x0c),
            FlacChannelMaskState::Invalid,
        ] {
            let error = validate_symphonia_track_compatibility(
                path,
                &output,
                &params,
                PcmKind::F32,
                changed,
            )
            .unwrap_err();
            assert!(error.contains("FLAC channel-mask metadata changed"));
        }
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn write_exact_tone(path: &Path, sample_rate: u32, frames: usize) {
        let samples: Vec<f32> = (0..frames)
            .map(|frame| {
                0.1 * (std::f64::consts::TAU * 997.0 * frame as f64 / sample_rate as f64).sin()
                    as f32
            })
            .collect();
        let buffer = AudioBuffer {
            sample_rate,
            channels: 2,
            frames,
            data: vec![samples.clone(), samples],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        crate::wav::WavWriter::write(path, &buffer, PcmKind::S24, false).unwrap();
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn encode_vorbis(input: &Path, output: &Path, serial_offset: u32) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-i"])
            .arg(input)
            .args(["-map_metadata", "-1", "-c:a", "libvorbis", "-q:a", "4"])
            .arg("-serial_offset")
            .arg(serial_offset.to_string())
            .arg(output)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(feature = "ffmpeg-encoding")]
    fn ogg_serial(path: &Path) -> u32 {
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() >= 18 && &bytes[..4] == b"OggS");
        u32::from_le_bytes(bytes[14..18].try_into().unwrap())
    }

    #[cfg(feature = "ffmpeg-encoding")]
    #[test]
    fn chained_vorbis_reselects_track_and_preserves_gapless_duration() {
        let directory = tempfile::tempdir().unwrap();
        let first_wav = directory.path().join("first.wav");
        let second_wav = directory.path().join("second.wav");
        let first_ogg = directory.path().join("first.ogg");
        let second_ogg = directory.path().join("second.ogg");
        let chained = directory.path().join("chained.ogg");
        write_exact_tone(&first_wav, 48_000, 4_800);
        write_exact_tone(&second_wav, 48_000, 7_200);
        encode_vorbis(&first_wav, &first_ogg, 100);
        encode_vorbis(&second_wav, &second_ogg, 200);
        assert_ne!(ogg_serial(&first_ogg), ogg_serial(&second_ogg));

        let first = decode(&first_ogg).unwrap();
        let second = decode(&second_ogg).unwrap();
        // Explicit gapless decoding removes codec priming/padding and keeps
        // the audible programme length sample-accurate.
        assert_eq!(first.frames, 4_800);
        assert_eq!(second.frames, 7_200);
        let audit = crate::container_qc::audit(&first_ogg).unwrap();
        assert!(audit.passed, "{audit:#?}");
        assert_eq!(
            audit.properties["decoded"]["frames"].as_u64(),
            Some(first.frames as u64)
        );

        let mut bytes = std::fs::read(&first_ogg).unwrap();
        bytes.extend_from_slice(&std::fs::read(&second_ogg).unwrap());
        std::fs::write(&chained, bytes).unwrap();

        let decoded = decode(&chained).unwrap();
        assert_eq!(decoded.frames, first.frames + second.frames);
        for channel in 0..decoded.channels as usize {
            let mut expected = first.data[channel].clone();
            expected.extend_from_slice(&second.data[channel]);
            assert_eq!(decoded.data[channel], expected);
        }

        let mut streamed = vec![Vec::new(); decoded.channels as usize];
        let info = decode_stream(&chained, |_, planar| {
            for (destination, source) in streamed.iter_mut().zip(planar) {
                destination.extend_from_slice(source);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(info.sample_rate, decoded.sample_rate);
        assert_eq!(info.channels, decoded.channels);
        assert_eq!(streamed, decoded.data);
    }

    #[cfg(feature = "ffmpeg-encoding")]
    #[test]
    fn chained_vorbis_rejects_sample_rate_change() {
        let directory = tempfile::tempdir().unwrap();
        let first_wav = directory.path().join("first.wav");
        let second_wav = directory.path().join("second.wav");
        let first_ogg = directory.path().join("first.ogg");
        let second_ogg = directory.path().join("second.ogg");
        let chained = directory.path().join("rate-change.ogg");
        write_exact_tone(&first_wav, 48_000, 4_800);
        write_exact_tone(&second_wav, 44_100, 4_410);
        encode_vorbis(&first_wav, &first_ogg, 300);
        encode_vorbis(&second_wav, &second_ogg, 400);

        let mut bytes = std::fs::read(&first_ogg).unwrap();
        bytes.extend_from_slice(&std::fs::read(&second_ogg).unwrap());
        std::fs::write(&chained, bytes).unwrap();

        let error = decode(&chained).unwrap_err();
        assert!(error.contains("sample rate changed from 48000 to 44100"));
        let error = decode_stream(&chained, |_, _| Ok(())).unwrap_err();
        assert!(error.contains("sample rate changed from 48000 to 44100"));
    }
}
