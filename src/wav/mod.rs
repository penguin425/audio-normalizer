//! WAV (RIFF/WAVE) demuxer and muxer.

pub mod format;
pub mod reader;
pub mod writer;

pub use format::{PcmKind, WaveFormat};
pub use reader::WavReader;
pub use writer::WavWriter;

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
}
