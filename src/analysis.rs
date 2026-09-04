//! Format-independent loudness and peak analysis.
//!
//! This module is intentionally limited to decoded PCM and pure DSP so it can
//! be shared by native applications and the browser WebAssembly build.

use crate::dsp::{lufs, truepeak};
use crate::wav::{AudioBuffer, ChannelRole, PcmKind};

/// Measurement implementation selected for analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisEngine {
    /// SIMD/parallel production engine.
    Fast,
    /// CPU-only scalar engine with committed coefficient bits and fixed order.
    Reference,
}

impl AnalysisEngine {
    /// Stable identifier recorded in reports and cache identities.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Fast => "forge-fast-bs1770-r4",
            Self::Reference => "forge-reference-bs1770-r1",
        }
    }
}

impl std::str::FromStr for AnalysisEngine {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fast" => Ok(Self::Fast),
            "reference" => Ok(Self::Reference),
            _ => Err(format!("unsupported analysis engine: {value}")),
        }
    }
}

/// Loudness/peak analysis of a single audio stream.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRole>,
    pub frames: usize,
    pub kind: PcmKind,
    pub lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub rms_db: f64,
    pub sample_peak: f32,
    pub true_peak: f32,
    /// Complete 400 ms block energies used to recompute album gating.
    #[doc(hidden)]
    pub loudness_blocks: Vec<f64>,
}

impl Analysis {
    /// EBU Tech 3341 warns that LRA is not stable during the first minute.
    pub const LRA_STABLE_AFTER_SECONDS: f64 = 60.0;

    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }

    pub fn loudness_range_stable(&self) -> bool {
        self.duration_secs() >= Self::LRA_STABLE_AFTER_SECONDS
    }

    pub fn sample_peak_db(&self) -> f64 {
        to_db(self.sample_peak as f64)
    }

    pub fn true_peak_db(&self) -> f64 {
        to_db(self.true_peak as f64)
    }

    /// Peak-to-Loudness Ratio (PLR), expressed in LU.
    pub fn peak_to_loudness_ratio_lu(&self) -> f64 {
        self.true_peak_db() - self.lufs
    }
}

#[inline]
fn to_db(x: f64) -> f64 {
    if x > 0.0 {
        20.0 * x.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// Analyze an already-decoded buffer.
pub fn analyze(buf: &AudioBuffer) -> Analysis {
    let ebu = lufs::measure_ebu(buf);
    let (rms_db, sample_peak) = lufs::measure_rms_peak(buf);
    let true_peak = truepeak::measure_true_peak(buf);
    Analysis {
        sample_rate: buf.sample_rate,
        channels: buf.channels,
        channel_roles: buf.channel_roles.clone(),
        frames: buf.frames,
        kind: buf.source_kind,
        lufs: ebu.integrated_lufs,
        max_momentary_lufs: ebu.max_momentary_lufs,
        max_short_term_lufs: ebu.max_short_term_lufs,
        loudness_range_lu: ebu.loudness_range_lu,
        rms_db,
        sample_peak,
        true_peak,
        loudness_blocks: ebu.gating_blocks,
    }
}

/// Analyze a decoded buffer with an explicitly identified engine.
pub fn analyze_with_engine(buf: &AudioBuffer, engine: AnalysisEngine) -> Result<Analysis, String> {
    if engine == AnalysisEngine::Fast {
        return Ok(analyze(buf));
    }
    let mut analyzer =
        lufs::ReferenceStreamingAnalyzer::new(buf.sample_rate, buf.channel_roles.clone())?;
    analyzer.process(&buf.data)?;
    let measured = analyzer.finish();
    Ok(Analysis {
        sample_rate: buf.sample_rate,
        channels: buf.channels,
        channel_roles: buf.channel_roles.clone(),
        frames: measured.frames,
        kind: buf.source_kind,
        lufs: measured.ebu.integrated_lufs,
        max_momentary_lufs: measured.ebu.max_momentary_lufs,
        max_short_term_lufs: measured.ebu.max_short_term_lufs,
        loudness_range_lu: measured.ebu.loudness_range_lu,
        rms_db: measured.rms_db,
        sample_peak: measured.sample_peak,
        true_peak: measured.true_peak,
        loudness_blocks: measured.ebu.gating_blocks,
    })
}
