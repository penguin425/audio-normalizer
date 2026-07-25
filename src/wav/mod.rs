//! WAV (RIFF/WAVE) demuxer and muxer.

pub mod format;
pub mod reader;
pub mod writer;

pub use format::{PcmKind, WaveFormat};
pub use reader::WavReader;
pub use writer::WavWriter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    Main,
    Surround,
    Lfe,
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
