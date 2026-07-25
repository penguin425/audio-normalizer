//! Sample-format definitions for the WAV container.

/// The kind of PCM sample stored in a `fmt ` chunk.
///
/// Covers the encodings Forge reads and writes: integer PCM at 8/16/24/32 bits
/// and IEEE float at 32/64 bits. 8-bit WAV is unsigned (offset by 128); every
/// other integer depth is two's-complement little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmKind {
    /// Unsigned 8-bit (centered at 128).
    U8,
    /// Signed 16-bit little-endian.
    S16,
    /// Signed 24-bit little-endian.
    S24,
    /// Signed 32-bit little-endian integer.
    S32,
    /// 32-bit IEEE-754 float.
    F32,
    /// 64-bit IEEE-754 float.
    F64,
}

impl PcmKind {
    #[inline]
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PcmKind::U8 => 1,
            PcmKind::S16 => 2,
            PcmKind::S24 => 3,
            PcmKind::S32 | PcmKind::F32 => 4,
            PcmKind::F64 => 8,
        }
    }

    #[inline]
    pub fn is_float(self) -> bool {
        matches!(self, PcmKind::F32 | PcmKind::F64)
    }

    /// WAVEFORMATEX `wBitsPerSample` value.
    #[inline]
    pub fn bits_per_sample(self) -> u16 {
        (self.bytes_per_sample() * 8) as u16
    }
}

/// The WAVE format tag (`wFormatTag`).
///
/// Only the tags Forge understands are modelled; everything else is rejected at
/// parse time so we never silently misinterpret audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveFormat {
    /// `WAVE_FORMAT_PCM` (0x0001).
    Pcm,
    /// `WAVE_FORMAT_IEEE_FLOAT` (0x0003).
    IeeeFloat,
    /// `WAVE_FORMAT_EXTENSIBLE` (0xFFFE); the real tag lives in the sub-format GUID.
    Extensible,
}

impl WaveFormat {
    /// Parse a 16-bit format tag into a [`WaveFormat`].
    pub fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            0x0001 => Some(WaveFormat::Pcm),
            0x0003 => Some(WaveFormat::IeeeFloat),
            0xFFFE => Some(WaveFormat::Extensible),
            _ => None,
        }
    }
}
