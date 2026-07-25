//! Allocation-free-after-construction DSP primitives for live audio callbacks.
//!
//! These primitives report causal Momentary and Short-term loudness. Final
//! Integrated LUFS remains a file/programme measurement and is intentionally
//! not presented as a live normalization target.

use crate::dsp::kwfilter::KWeight;
use crate::dsp::lufs::channel_weight;
use crate::dsp::truepeak::TruePeakMeter;
use crate::wav::ChannelRole;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RealtimeMeasurement {
    pub frames: u64,
    pub momentary_lufs: f64,
    pub short_term_lufs: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

/// Causal EBU Momentary/Short-term and peak meter.
///
/// All buffers are allocated by [`RealtimeMeter::new`]. `process_planar`
/// performs no heap allocation and is suitable for a live audio callback.
pub struct RealtimeMeter {
    sample_rate: u32,
    roles: Vec<ChannelRole>,
    filters: Vec<KWeight>,
    true_peak: Vec<TruePeakMeter>,
    energy: Vec<f64>,
    position: usize,
    filled: usize,
    momentary_window: usize,
    momentary_sum: f64,
    short_term_sum: f64,
    frames: u64,
    sample_peak: f32,
}

impl RealtimeMeter {
    pub fn new(sample_rate: u32, roles: Vec<ChannelRole>) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("real-time meter sample rate must be positive".into());
        }
        if roles.is_empty() {
            return Err("real-time meter requires at least one channel".into());
        }
        let channels = roles.len();
        let momentary_window = ((sample_rate as usize * 4) / 10).max(1);
        let short_term_window = (sample_rate as usize * 3).max(1);
        Ok(Self {
            sample_rate,
            roles,
            filters: (0..channels)
                .map(|_| KWeight::for_sample_rate(sample_rate))
                .collect(),
            true_peak: (0..channels).map(|_| TruePeakMeter::new()).collect(),
            energy: vec![0.0; short_term_window],
            position: 0,
            filled: 0,
            momentary_window,
            momentary_sum: 0.0,
            short_term_sum: 0.0,
            frames: 0,
            sample_peak: 0.0,
        })
    }

    pub fn channels(&self) -> usize {
        self.roles.len()
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[allow(clippy::needless_range_loop)] // frame-major traversal without iterator allocation
    pub fn process_planar(&mut self, planar: &[&[f32]]) -> Result<(), String> {
        if planar.len() != self.channels() {
            return Err("real-time meter channel count changed".into());
        }
        let chunk_frames = planar.first().map_or(0, |channel| channel.len());
        if planar.iter().any(|channel| channel.len() != chunk_frames) {
            return Err("real-time meter channel length mismatch".into());
        }
        for (meter, channel) in self.true_peak.iter_mut().zip(planar) {
            meter.process(channel);
        }
        for frame in 0..chunk_frames {
            let mut weighted = 0.0;
            for channel in 0..self.channels() {
                let sample = planar[channel][frame];
                self.sample_peak = self.sample_peak.max(sample.abs());
                let filtered = self.filters[channel].process(sample) as f64;
                weighted += channel_weight(self.roles[channel]) * filtered * filtered;
            }

            if self.filled >= self.momentary_window {
                let index =
                    (self.position + self.energy.len() - self.momentary_window) % self.energy.len();
                self.momentary_sum -= self.energy[index];
            }
            if self.filled == self.energy.len() {
                self.short_term_sum -= self.energy[self.position];
            }
            self.energy[self.position] = weighted;
            self.momentary_sum += weighted;
            self.short_term_sum += weighted;
            self.position = (self.position + 1) % self.energy.len();
            self.filled = (self.filled + 1).min(self.energy.len());
            self.frames += 1;
        }
        Ok(())
    }

    pub fn measurement(&self) -> RealtimeMeasurement {
        RealtimeMeasurement {
            frames: self.frames,
            momentary_lufs: window_loudness(
                self.momentary_sum,
                self.filled.min(self.momentary_window),
                self.momentary_window,
            ),
            short_term_lufs: window_loudness(self.short_term_sum, self.filled, self.energy.len()),
            sample_peak_dbfs: amplitude_db(self.sample_peak),
            true_peak_dbtp: amplitude_db(
                self.true_peak
                    .iter()
                    .map(TruePeakMeter::peak)
                    .fold(0.0, f32::max),
            ),
        }
    }

    pub fn reset(&mut self) {
        self.filters = (0..self.channels())
            .map(|_| KWeight::for_sample_rate(self.sample_rate))
            .collect();
        self.true_peak = (0..self.channels()).map(|_| TruePeakMeter::new()).collect();
        self.energy.fill(0.0);
        self.position = 0;
        self.filled = 0;
        self.momentary_sum = 0.0;
        self.short_term_sum = 0.0;
        self.frames = 0;
        self.sample_peak = 0.0;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RealtimeGainConfig {
    pub initial_gain_db: f64,
    pub ceiling_dbfs: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
}

impl Default for RealtimeGainConfig {
    fn default() -> Self {
        Self {
            initial_gain_db: 0.0,
            ceiling_dbfs: -1.0,
            attack_ms: 10.0,
            release_ms: 100.0,
        }
    }
}

/// Zero-latency smoothed gain and safety-ceiling processor for interleaved f32.
///
/// It is a deterministic live-leveling building block, not a claim that final
/// Integrated LUFS can be known before a programme ends.
pub struct RealtimeGainProcessor {
    channels: usize,
    current_gain: f32,
    target_gain: f32,
    ceiling: f32,
    attack_coefficient: f32,
    release_coefficient: f32,
}

impl RealtimeGainProcessor {
    pub fn new(
        sample_rate: u32,
        channels: usize,
        config: RealtimeGainConfig,
    ) -> Result<Self, String> {
        if sample_rate == 0 || channels == 0 {
            return Err("real-time gain processor requires a sample rate and channels".into());
        }
        if !config.initial_gain_db.is_finite()
            || !config.ceiling_dbfs.is_finite()
            || !config.attack_ms.is_finite()
            || config.attack_ms <= 0.0
            || !config.release_ms.is_finite()
            || config.release_ms <= 0.0
        {
            return Err("invalid real-time gain configuration".into());
        }
        let gain = db_amplitude(config.initial_gain_db);
        Ok(Self {
            channels,
            current_gain: gain,
            target_gain: gain,
            ceiling: db_amplitude(config.ceiling_dbfs),
            attack_coefficient: smoothing_coefficient(sample_rate, config.attack_ms),
            release_coefficient: smoothing_coefficient(sample_rate, config.release_ms),
        })
    }

    pub fn set_target_gain_db(&mut self, gain_db: f64) -> Result<(), String> {
        if !gain_db.is_finite() {
            return Err("real-time target gain must be finite".into());
        }
        self.target_gain = db_amplitude(gain_db);
        Ok(())
    }

    pub fn current_gain_db(&self) -> f64 {
        amplitude_db(self.current_gain)
    }

    pub fn latency_frames(&self) -> usize {
        0
    }

    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> Result<(), String> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err("interleaved buffer is not frame-aligned".into());
        }
        for frame in samples.chunks_exact_mut(self.channels) {
            let coefficient = if self.target_gain < self.current_gain {
                self.attack_coefficient
            } else {
                self.release_coefficient
            };
            self.current_gain += (self.target_gain - self.current_gain) * coefficient;
            for sample in frame {
                *sample = (*sample * self.current_gain).clamp(-self.ceiling, self.ceiling);
            }
        }
        Ok(())
    }
}

fn window_loudness(sum: f64, filled: usize, required: usize) -> f64 {
    if filled < required || sum <= 0.0 {
        f64::NEG_INFINITY
    } else {
        -0.691 + 10.0 * (sum / required as f64).log10()
    }
}

fn smoothing_coefficient(sample_rate: u32, milliseconds: f64) -> f32 {
    (1.0 - (-1.0 / (sample_rate as f64 * milliseconds / 1000.0)).exp()) as f32
}

fn db_amplitude(db: f64) -> f32 {
    10.0_f64.powf(db / 20.0) as f32
}

fn amplitude_db(amplitude: f32) -> f64 {
    if amplitude > 0.0 {
        20.0 * (amplitude as f64).log10()
    } else {
        f64::NEG_INFINITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_is_chunk_boundary_invariant() {
        let samples: Vec<f32> = (0..192_000)
            .map(|index| (index as f32 * 0.013).sin() * 0.1)
            .collect();
        let mut whole = RealtimeMeter::new(48_000, vec![ChannelRole::Main]).unwrap();
        whole.process_planar(&[&samples]).unwrap();
        let mut chunked = RealtimeMeter::new(48_000, vec![ChannelRole::Main]).unwrap();
        for chunk in samples.chunks(997) {
            chunked.process_planar(&[chunk]).unwrap();
        }
        let a = whole.measurement();
        let b = chunked.measurement();
        assert!((a.momentary_lufs - b.momentary_lufs).abs() < 1e-10);
        assert!((a.short_term_lufs - b.short_term_lufs).abs() < 1e-10);
        assert!((a.true_peak_dbtp - b.true_peak_dbtp).abs() < 1e-10);
    }

    #[test]
    fn gain_processor_smooths_and_respects_ceiling() {
        let mut processor =
            RealtimeGainProcessor::new(48_000, 2, RealtimeGainConfig::default()).unwrap();
        processor.set_target_gain_db(12.0).unwrap();
        let mut samples = vec![0.9; 48_000 * 2];
        processor.process_interleaved(&mut samples).unwrap();
        let ceiling = db_amplitude(-1.0);
        assert!(samples.iter().all(|sample| sample.abs() <= ceiling));
        assert!(processor.current_gain_db() > 0.0);
        assert_eq!(processor.latency_frames(), 0);
    }
}
