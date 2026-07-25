//! Streaming look-ahead true-peak limiter.

use super::truepeak::TruePeakMeter;
use std::collections::VecDeque;

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
            meters: (0..channels).map(|_| TruePeakMeter::new()).collect(),
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
        })
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

    pub fn finish(mut self) -> Vec<Vec<f32>> {
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
        output
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
        for (channel, queue) in self.delay.iter_mut().enumerate() {
            if let Some(sample) = queue.pop_front() {
                output[channel].push(sample * self.envelope);
            }
        }
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
}
