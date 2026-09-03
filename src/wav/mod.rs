//! WAV (RIFF/WAVE) demuxer and muxer.

/// Bounds shared by every decoder entry point, including the browser build.
pub(crate) const MIN_DECODE_SAMPLE_RATE_HZ: u32 = 8_000;
pub(crate) const MAX_DECODE_SAMPLE_RATE_HZ: u32 = 384_000;

/// How confidently a decoder can bind PCM planes to physical speakers.
///
/// The layout-preserving decoder APIs return this sidecar because a channel
/// count alone cannot identify a multichannel speaker layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLayoutProvenance {
    /// Every decoded channel is bound to a known physical speaker.
    KnownSpeakers,
    /// The container did not completely identify the speaker assignment.
    Unknown,
    /// The channels describe a scene (for example Ambisonics), not speakers.
    SceneBased,
}

pub mod format;
pub mod reader;
pub mod writer;

pub use format::{PcmKind, WaveFormat};
pub use reader::{WavReader, WavStreamInfo};
pub use writer::{WavContainer, WavStreamWriter, WavWriter, WaveChunk};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    /// A conventional front or centre channel with unity loudness weighting.
    Main,
    /// A conventional surround channel in a mono/stereo/5.1 programme.
    Surround,
    /// Mono content intended to be reproduced identically by two speakers.
    DualMono,
    /// A channel with a known loudspeaker position. BS.1770-5 Annex 3 derives
    /// its weight from azimuth and elevation instead of a generic name.
    Positioned {
        azimuth_degrees: i16,
        elevation_degrees: i16,
    },
    Lfe,
}

impl ChannelRole {
    pub const fn positioned(azimuth_degrees: i16, elevation_degrees: i16) -> Self {
        Self::Positioned {
            azimuth_degrees,
            elevation_degrees,
        }
    }
}

pub fn default_channel_roles(channels: u16) -> Vec<ChannelRole> {
    use ChannelRole::{Lfe, Main, Surround};
    match channels {
        0 => Vec::new(),
        1..=3 => vec![Main; channels as usize],
        4 => vec![Main, Main, Surround, Surround],
        5 => vec![Main, Main, Main, Surround, Surround],
        6 => vec![Main, Main, Main, Lfe, Surround, Surround],
        _ => vec![Main; channels as usize],
    }
}

/// A named, ordered channel layout accepted by the CLI.
///
/// Orders follow WAVE_FORMAT_EXTENSIBLE: FL, FR, FC, LFE, back L/R, side L/R,
/// then top-front L/R and top-back L/R where present.
pub fn named_channel_layout(name: &str) -> Option<Vec<ChannelRole>> {
    use ChannelRole::{Lfe, Main, Surround};
    let p = ChannelRole::positioned;
    Some(match name {
        "mono" => vec![Main],
        "stereo" => vec![Main, Main],
        "5.1" => vec![Main, Main, Main, Lfe, Surround, Surround],
        "6.1" => vec![Main, Main, Main, Lfe, p(180, 0), p(-90, 0), p(90, 0)],
        "7.1" => vec![
            Main,
            Main,
            Main,
            Lfe,
            p(-135, 0),
            p(135, 0),
            p(-90, 0),
            p(90, 0),
        ],
        "5.1.4" => vec![
            Main,
            Main,
            Main,
            Lfe,
            Surround,
            Surround,
            p(-30, 45),
            p(30, 45),
            p(-135, 45),
            p(135, 45),
        ],
        "7.1.4" => vec![
            Main,
            Main,
            Main,
            Lfe,
            p(-135, 0),
            p(135, 0),
            p(-90, 0),
            p(90, 0),
            p(-30, 45),
            p(30, 45),
            p(-135, 45),
            p(135, 45),
        ],
        _ => return None,
    })
}

/// A fully-decoded, **planar** audio buffer (one `Vec<f32>` per channel).
///
/// All DSP in Forge operates on planar f32 normalized to roughly [-1.0, 1.0),
/// which keeps the hot loops branch-free and trivially parallelizable across
/// channels.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Number of frames (samples per channel).
    pub frames: usize,
    /// Planar channel data: `data[ch][frame]`. `data.len() == channels`.
    pub data: Vec<Vec<f32>>,
    /// BS.1770 loudness role for each sample plane.
    pub channel_roles: Vec<ChannelRole>,
    /// The sample format the file was stored in. Used as the default output
    /// format when the caller does not request a specific one.
    pub source_kind: PcmKind,
}

impl AudioBuffer {
    #[inline]
    #[allow(dead_code)]
    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }

    #[inline]
    pub fn channel_role(&self, index: usize) -> ChannelRole {
        self.channel_roles
            .get(index)
            .copied()
            .unwrap_or(ChannelRole::Main)
    }
}
