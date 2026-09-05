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
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MONO_WAV_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MULTICHANNEL_WAV_STREAM_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_PARALLEL_FLAC_DECODERS: usize = 8;
const MIN_PARALLEL_FLAC_DECODERS: usize = 4;
const FLAC_PACKETS_PER_DECODER: usize = 32;
const FLAC_SAMPLE_VALUES_PER_DECODER: u64 = 192_000;
const FLAC_FILE_BYTES_PER_DECODER: u64 = 192 * 1024;
const MAX_PARALLEL_FLAC_PACKET_BYTES: usize = 32 * 1024 * 1024;
const MAX_PARALLEL_FLAC_PCM_BYTES: usize = 32 * 1024 * 1024;
// MP3, AAC, and Vorbis normally decode much smaller packets. The normalization
// render pass groups whole packets to amortize callbacks and writer work while
// staying below the analyzer's 16,384-frame True Peak task threshold.
const TARGET_SYMPHONIA_STREAM_CHUNK_FRAMES: usize = 4_096;

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
        validate_descriptor_options(&options)?;
        let probed = probe_registry(&input, options.track)?;
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

fn probe_registry(
    input: &StableInput,
    selection: AudioTrackSelection,
) -> Result<RegistryProbe, String> {
    let path = input.stable_path();
    let route = sniff_decoder_route(path)?;
    let display = display_input(input);
    if route == DecoderRoute::Wave {
        require_single_track(selection)?;
        let (wav, channel_layout) = WavReader::probe_with_channel_layout(path)
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
        });
    }
    let identity =
        registry_identity_at(path, input.source_name_hint(), &display, route, selection)?;
    if let Some((info, decoder_channel_layout, declared_frames)) = identity.stream {
        let decoder_layout_provenance = decoder_channel_layout.provenance();
        let channel_layout = if identity.container == AudioContainer::IsoBmff {
            crate::isobmff_qc::probe_channel_layout(path, identity.track_id, info.channels)?
                .unwrap_or(decoder_channel_layout)
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
        });
    }

    const PROBE_COMPLETE: &str = "__forge_input_descriptor_probe_complete__";
    let mut captured = None;
    let decoded = decode_stream_raw_with_selection(
        path,
        route,
        selection,
        None,
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
        crate::isobmff_qc::probe_channel_layout(path, identity.track_id, info.channels)?
            .unwrap_or_else(|| {
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

fn probe_symphonia_identity_at(
    path: &Path,
    hint_path: Option<&Path>,
    display: &str,
    selection: AudioTrackSelection,
) -> Result<RegistryIdentity, String> {
    use symphonia::core::audio::AudioSpec;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::get_probe;

    let file = File::open(path).map_err(|error| format!("{display}: {error}"))?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = hint_path
        .and_then(Path::extension)
        .and_then(|extension| extension.to_str())
    {
        hint.with_extension(extension);
    }
    let mut format = get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
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
    let result = decode_stream_raw_with_selection(
        descriptor.input.stable_path(),
        descriptor.route,
        selection,
        None,
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

/// Decode the descriptor's exact programme for loudness measurement.
///
/// Native S32/F64 WAVE streams are read directly from their immutable
/// snapshot. Other inputs share the regular bounded decoder stream; its S24
/// normalization is exact and retains the optimized f32 analyzer lane.
pub(crate) fn decode_descriptor_analysis_stream<F>(
    descriptor: &InputDescriptor,
    mut consume: F,
) -> Result<StreamInfo, String>
where
    F: FnMut(&StreamInfo, ChannelLayoutProvenance, AnalysisPcmChunk<'_>) -> Result<(), String>,
{
    if descriptor.route != DecoderRoute::Wave
        || !matches!(descriptor.info.source_kind, PcmKind::S32 | PcmKind::F64)
    {
        return decode_descriptor_stream_with_layout(descriptor, |info, provenance, planar| {
            consume(info, provenance, AnalysisPcmChunk::F32(planar))
        });
    }

    let path = descriptor.input.stable_path();
    let (wav, provenance) = WavReader::probe_with_layout(path)
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
    let byte_offset = start
        .checked_mul(frame_bytes_u64)
        .and_then(|offset| wav.data_offset.checked_add(offset))
        .ok_or_else(|| "selected WAVE byte range overflows u64".to_string())?;
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(byte_offset))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let chunk_bytes = wav_stream_chunk_bytes(wav.channels, wav.kind);
    let chunk_frames = chunk_bytes / frame_bytes;
    let mut bytes = vec![0_u8; chunk_bytes];
    let mut integer_planar = Vec::new();
    let mut f64_planar = Vec::new();
    let mut remaining_frames = selected_frames;
    while remaining_frames != 0 {
        let frames = remaining_frames.min(chunk_frames as u64) as usize;
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
    use symphonia::core::errors::Error;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if route == DecoderRoute::Wave {
        require_single_track(selection)?;
        return decode_wav_stream(path, consume);
    }
    if matches!(route, DecoderRoute::Dsf | DecoderRoute::Dsdiff) {
        require_single_track(selection)?;
        return crate::dsd::decode_stream_with_layout_and_declared_frames(path, consume);
    }
    if route == DecoderRoute::Opus {
        require_single_track(selection)?;
        #[cfg(feature = "opus-encoding")]
        {
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
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if !extension.is_empty() {
        hint.with_extension(&extension);
    }
    let mut format = get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| format!("{}: probe failed: {error}", path.display()))?;
    let container_format = format.format_info().format;
    let mut track =
        select_symphonia_audio_track_with_selection(path, format.as_ref(), selection)?.0;
    require_symphonia_sample_rate(path, &track.codec_params)?;
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
        decoded.copy_to_vecs_planar::<f32>(&mut planar);
        consume(
            info.as_ref().unwrap(),
            output_format.as_ref().unwrap().layout_provenance,
            declared_frames,
            &mut planar,
        )?;
    }

    info.ok_or_else(|| format!("{}: no audio decoded", path.display()))
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

fn decode_wav_stream<F>(path: &Path, mut consume: F) -> Result<StreamInfo, String>
where
    F: FnMut(
        &StreamInfo,
        ChannelLayoutProvenance,
        Option<u64>,
        &mut [Vec<f32>],
    ) -> Result<(), String>,
{
    let (wav, layout_provenance) = WavReader::probe_with_layout(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let declared_frames =
        wav.data_size / (u64::from(wav.channels) * wav.kind.bytes_per_sample() as u64);
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
    let chunk_bytes = wav_stream_chunk_bytes(info.channels, info.source_kind);
    let mut remaining = data_size;
    let mut bytes = vec![0; chunk_bytes];
    let mut planar = Vec::new();
    while remaining >= frame_bytes {
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
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        let crossover = SymphoniaAudioTrack {
            id: 0,
            num_frames: Some(192_000),
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        let unknown = SymphoniaAudioTrack {
            id: 0,
            num_frames: None,
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        assert_eq!(parallel_flac_worker_cap(&short, u64::MAX), 1);
        assert_eq!(parallel_flac_worker_cap(&crossover, 0), 2);
        let efficient = SymphoniaAudioTrack {
            id: 0,
            num_frames: Some(384_000),
            codec_params: codec_params(48_000, CHANNEL_LAYOUT_STEREO.clone()),
        };
        assert_eq!(parallel_flac_worker_cap(&efficient, 0), 4);
        assert!(parallel_flac_worker_cap(&efficient, 0) >= MIN_PARALLEL_FLAC_DECODERS);
        assert_eq!(parallel_flac_worker_cap(&unknown, 383 * 1024), 1);
        assert_eq!(parallel_flac_worker_cap(&unknown, 384 * 1024), 2);
        assert_eq!(parallel_flac_worker_cap(&unknown, u64::MAX), 8);
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
