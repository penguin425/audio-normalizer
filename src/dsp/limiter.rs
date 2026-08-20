//! Streaming look-ahead true-peak limiter.

use super::truepeak::TruePeakMeter;
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize)]
pub struct LimiterEnvelopePoint {
    pub start_frame: usize,
    pub end_frame: usize,
    pub mean_reduction_db: f64,
    pub maximum_reduction_db: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LimiterStatistics {
    pub processed_frames: usize,
    pub limited_frames: usize,
    pub maximum_reduction_db: f64,
    pub mean_reduction_db: f64,
    pub envelope_interval_frames: usize,
    pub envelope: Vec<LimiterEnvelopePoint>,
}

#[derive(Debug, Clone, Copy)]
pub struct LimiterConfig {
    pub lookahead_ms: f64,
    pub release_ms: f64,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            lookahead_ms: 5.0,
            release_ms: 100.0,
        }
    }
}

pub struct TruePeakLimiter {
    meters: Vec<TruePeakMeter>,
    delay: Vec<VecDeque<f32>>,
    lookahead_frames: usize,
    release_coeff: f32,
    ceiling: f32,
    envelope: f32,
    hold_frames: usize,
    last_samples: Vec<f32>,
    max_reduction_db: f64,
    emitted_frames: usize,
    limited_frames: usize,
    reduction_sum_db: f64,
    statistics_enabled: bool,
    statistics_interval_frames: usize,
    interval_start_frame: usize,
    interval_frames: usize,
    interval_reduction_sum_db: f64,
    interval_max_reduction_db: f64,
    statistics_envelope: Vec<LimiterEnvelopePoint>,
}

impl TruePeakLimiter {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        ceiling_db: f64,
        config: LimiterConfig,
    ) -> Result<Self, String> {
        if channels == 0 {
            return Err("limiter requires at least one channel".into());
        }
        if !config.lookahead_ms.is_finite()
            || config.lookahead_ms < 1.0
            || !config.release_ms.is_finite()
            || config.release_ms <= 0.0
        {
            return Err("limiter look-ahead must be >= 1 ms and release must be > 0 ms".into());
        }
        let lookahead_frames =
            ((sample_rate as f64 * config.lookahead_ms / 1000.0).ceil() as usize).max(16);
        let release_samples = sample_rate as f64 * config.release_ms / 1000.0;
        Ok(Self {
            meters: (0..channels)
                .map(|_| TruePeakMeter::for_sample_rate(sample_rate))
                .collect(),
            delay: (0..channels)
                .map(|_| VecDeque::with_capacity(lookahead_frames + 1))
                .collect(),
            lookahead_frames,
            release_coeff: (-1.0 / release_samples).exp() as f32,
            ceiling: 10.0_f64.powf(ceiling_db / 20.0) as f32,
            envelope: 1.0,
            hold_frames: 0,
            last_samples: vec![0.0; channels as usize],
            max_reduction_db: 0.0,
            emitted_frames: 0,
            limited_frames: 0,
            reduction_sum_db: 0.0,
            statistics_enabled: false,
            statistics_interval_frames: (sample_rate as usize / 10).max(1),
            interval_start_frame: 0,
            interval_frames: 0,
            interval_reduction_sum_db: 0.0,
            interval_max_reduction_db: 0.0,
            statistics_envelope: Vec::new(),
        })
    }

    /// Select the evidence interval used by [`LimiterStatistics`].
    ///
    /// Rendering is unaffected. Callers can increase the interval for long
    /// programmes to keep the serialized gain envelope bounded.
    pub fn set_statistics_interval_frames(&mut self, frames: usize) {
        self.statistics_enabled = true;
        self.statistics_interval_frames = frames.max(1);
    }

    pub fn process(&mut self, planar: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, String> {
        if planar.len() != self.delay.len() {
            return Err("limiter channel count changed".into());
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("limiter input has unequal channel lengths".into());
        }
        let emit = frames.saturating_sub(self.lookahead_frames.saturating_sub(self.delay[0].len()));
        let mut output = (0..self.delay.len())
            .map(|_| Vec::with_capacity(emit))
            .collect::<Vec<_>>();
        for (frame, _) in planar[0].iter().enumerate() {
            let mut detected = 0.0_f32;
            for (channel, samples) in planar.iter().enumerate() {
                let sample = samples[frame];
                self.last_samples[channel] = sample;
                self.delay[channel].push_back(sample);
                detected = detected.max(self.meters[channel].process_sample(sample));
            }
            self.update_envelope(detected);
            if self.delay[0].len() > self.lookahead_frames {
                self.emit_one(&mut output);
            }
        }
        Ok(output)
    }

    pub fn finish(self) -> Vec<Vec<f32>> {
        self.finish_with_statistics().0
    }

    pub fn finish_with_statistics(mut self) -> (Vec<Vec<f32>>, LimiterStatistics) {
        let mut output = (0..self.delay.len())
            .map(|_| Vec::with_capacity(self.delay[0].len()))
            .collect::<Vec<_>>();
        while !self.delay[0].is_empty() {
            let mut detected = 0.0_f32;
            for channel in 0..self.delay.len() {
                detected =
                    detected.max(self.meters[channel].process_sample(self.last_samples[channel]));
            }
            self.update_envelope(detected);
            self.emit_one(&mut output);
        }
        if self.statistics_enabled {
            self.finish_statistics_interval();
        }
        let statistics = LimiterStatistics {
            processed_frames: self.emitted_frames,
            limited_frames: self.limited_frames,
            maximum_reduction_db: self.max_reduction_db,
            mean_reduction_db: if self.emitted_frames == 0 {
                0.0
            } else {
                self.reduction_sum_db / self.emitted_frames as f64
            },
            envelope_interval_frames: self.statistics_interval_frames,
            envelope: self.statistics_envelope,
        };
        (output, statistics)
    }

    pub fn max_reduction_db(&self) -> f64 {
        self.max_reduction_db
    }

    fn update_envelope(&mut self, detected: f32) {
        let required = if detected > self.ceiling {
            (self.ceiling / detected) * 0.9999
        } else {
            1.0
        };
        if required < self.envelope {
            self.envelope = required;
            self.hold_frames = self.lookahead_frames;
        } else if self.hold_frames > 0 {
            self.hold_frames -= 1;
        } else {
            self.envelope = 1.0 - (1.0 - self.envelope) * self.release_coeff;
        }
        if self.envelope > 0.0 {
            self.max_reduction_db = self
                .max_reduction_db
                .max(-20.0 * (self.envelope as f64).log10());
        }
    }

    fn emit_one(&mut self, output: &mut [Vec<f32>]) {
        if self.statistics_enabled {
            let reduction_db = if self.envelope > 0.0 {
                -20.0 * (self.envelope as f64).log10()
            } else {
                f64::INFINITY
            };
            if reduction_db > 1e-6 {
                self.limited_frames += 1;
            }
            self.emitted_frames += 1;
            self.reduction_sum_db += reduction_db;
            self.interval_frames += 1;
            self.interval_reduction_sum_db += reduction_db;
            self.interval_max_reduction_db = self.interval_max_reduction_db.max(reduction_db);
        }
        for (channel, queue) in self.delay.iter_mut().enumerate() {
            if let Some(sample) = queue.pop_front() {
                output[channel].push(sample * self.envelope);
            }
        }
        if self.statistics_enabled && self.interval_frames == self.statistics_interval_frames {
            self.finish_statistics_interval();
        }
    }

    fn finish_statistics_interval(&mut self) {
        if self.interval_frames == 0 {
            return;
        }
        let end_frame = self.interval_start_frame + self.interval_frames;
        self.statistics_envelope.push(LimiterEnvelopePoint {
            start_frame: self.interval_start_frame,
            end_frame,
            mean_reduction_db: self.interval_reduction_sum_db / self.interval_frames as f64,
            maximum_reduction_db: self.interval_max_reduction_db,
        });
        self.interval_start_frame = end_frame;
        self.interval_frames = 0;
        self.interval_reduction_sum_db = 0.0;
        self.interval_max_reduction_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_limiter_preserves_frames_and_ceiling() {
        let sample_rate = 48_000;
        let ceiling_db = -3.0;
        let ceiling = 10.0_f64.powf(ceiling_db / 20.0) as f32;
        let input = (0..sample_rate as usize)
            .map(|index| {
                (1.2 * (index as f64 * std::f64::consts::TAU * 997.0 / sample_rate as f64).sin())
                    as f32
            })
            .collect::<Vec<_>>();
        let mut limiter =
            TruePeakLimiter::new(sample_rate, 1, ceiling_db, LimiterConfig::default()).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(317) {
            output.extend(limiter.process(&[chunk.to_vec()]).unwrap().remove(0));
        }
        output.extend(limiter.finish().remove(0));
        assert_eq!(output.len(), input.len());
        let mut meter = TruePeakMeter::new();
        meter.process(&output);
        assert!(
            meter.peak() <= ceiling * 1.001,
            "{} > {}",
            meter.peak(),
            ceiling
        );
    }

    #[test]
    fn limiter_reports_bounded_gain_reduction_envelope() {
        let mut limiter = TruePeakLimiter::new(48_000, 1, -6.0, LimiterConfig::default()).unwrap();
        limiter.set_statistics_interval_frames(1_000);
        let input = vec![vec![1.0; 4_800]];
        let mut output = limiter.process(&input).unwrap();
        let (tail, statistics) = limiter.finish_with_statistics();
        output[0].extend(tail[0].iter().copied());

        assert_eq!(statistics.processed_frames, input[0].len());
        assert!(statistics.limited_frames > 0);
        assert!(statistics.maximum_reduction_db > 5.9);
        assert_eq!(statistics.envelope.len(), 5);
        assert_eq!(statistics.envelope.last().unwrap().end_frame, 4_800);
        assert_eq!(output[0].len(), input[0].len());
    }
}
