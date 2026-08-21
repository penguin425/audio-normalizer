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
        let mut output = vec![Vec::new(); self.delay.len()];
        self.process_into(planar, &mut output)?;
        Ok(output)
    }

    /// Process one chunk into caller-owned planar storage.
    ///
    /// Every output channel is cleared before use but retains its allocation,
    /// so a streaming caller can reuse the same buffers for all chunks. The
    /// first call may reserve enough space for the emitted frames; subsequent
    /// calls of the same or smaller size do not allocate.
    pub fn process_into(
        &mut self,
        planar: &[Vec<f32>],
        output: &mut [Vec<f32>],
    ) -> Result<(), String> {
        if planar.len() != self.delay.len() {
            return Err("limiter channel count changed".into());
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("limiter input has unequal channel lengths".into());
        }
        if output.len() != self.delay.len() {
            return Err("limiter output channel count changed".into());
        }
        let emit = frames.saturating_sub(self.lookahead_frames.saturating_sub(self.delay[0].len()));
        for channel in output.iter_mut() {
            channel.clear();
            channel.reserve(emit);
        }
        for (frame, _) in planar[0].iter().enumerate() {
            let detected = push_and_detect_frame(
                &mut self.meters,
                &mut self.delay,
                &mut self.last_samples,
                planar,
                frame,
            );
            self.update_envelope(detected);
            if self.delay[0].len() > self.lookahead_frames {
                self.emit_one(output);
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Vec<Vec<f32>> {
        self.finish_with_statistics().0
    }

    /// Flush delayed samples into caller-owned planar storage.
    pub fn finish_into(self, output: &mut [Vec<f32>]) -> Result<(), String> {
        self.finish_with_statistics_into(output).map(|_| ())
    }

    pub fn finish_with_statistics(self) -> (Vec<Vec<f32>>, LimiterStatistics) {
        let channels = self.delay.len();
        let mut output = vec![Vec::new(); channels];
        let statistics = self
            .finish_with_statistics_into(&mut output)
            .expect("internally allocated limiter output has the expected channel count");
        (output, statistics)
    }

    /// Flush delayed samples and return statistics while retaining the caller's
    /// output allocation for later pipeline reuse.
    pub fn finish_with_statistics_into(
        mut self,
        output: &mut [Vec<f32>],
    ) -> Result<LimiterStatistics, String> {
        if output.len() != self.delay.len() {
            return Err("limiter output channel count changed".into());
        }
        let remaining = self.delay[0].len();
        for channel in output.iter_mut() {
            channel.clear();
            channel.reserve(remaining);
        }
        while !self.delay[0].is_empty() {
            let detected = detect_repeated_frame(&mut self.meters, &self.last_samples);
            self.update_envelope(detected);
            self.emit_one(output);
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
        Ok(statistics)
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

/// Advance meters in adjacent pairs so both channels share immutable true-peak
/// coefficient loads. The detected maximum is still reduced in channel order,
/// preserving the scalar limiter's exceptional-value and rounding behavior.
#[inline]
fn push_and_detect_frame(
    meters: &mut [TruePeakMeter],
    delay: &mut [VecDeque<f32>],
    last_samples: &mut [f32],
    planar: &[Vec<f32>],
    frame: usize,
) -> f32 {
    if meters.len() == 2 {
        let left_sample = planar[0][frame];
        let right_sample = planar[1][frame];
        last_samples[0] = left_sample;
        last_samples[1] = right_sample;
        delay[0].push_back(left_sample);
        delay[1].push_back(right_sample);
        let (left, right) = meters.split_at_mut(1);
        let (left_peak, right_peak) = TruePeakMeter::process_stereo_sample(
            &mut left[0],
            &mut right[0],
            left_sample,
            right_sample,
        );
        return 0.0_f32.max(left_peak).max(right_peak);
    }

    let mut detected = 0.0_f32;
    for (((meter_pair, delay_pair), last_pair), sample_pair) in meters
        .chunks_mut(2)
        .zip(delay.chunks_mut(2))
        .zip(last_samples.chunks_mut(2))
        .zip(planar.chunks(2))
    {
        let left_sample = sample_pair[0][frame];
        last_pair[0] = left_sample;
        delay_pair[0].push_back(left_sample);
        if meter_pair.len() == 2 {
            let right_sample = sample_pair[1][frame];
            last_pair[1] = right_sample;
            delay_pair[1].push_back(right_sample);
            let (left, right) = meter_pair.split_at_mut(1);
            let (left_peak, right_peak) = TruePeakMeter::process_stereo_sample(
                &mut left[0],
                &mut right[0],
                left_sample,
                right_sample,
            );
            detected = detected.max(left_peak);
            detected = detected.max(right_peak);
        } else {
            detected = detected.max(meter_pair[0].process_sample(left_sample));
        }
    }
    detected
}

#[inline]
fn detect_repeated_frame(meters: &mut [TruePeakMeter], last_samples: &[f32]) -> f32 {
    if meters.len() == 2 {
        let (left, right) = meters.split_at_mut(1);
        let (left_peak, right_peak) = TruePeakMeter::process_stereo_sample(
            &mut left[0],
            &mut right[0],
            last_samples[0],
            last_samples[1],
        );
        return 0.0_f32.max(left_peak).max(right_peak);
    }

    let mut detected = 0.0_f32;
    for (meter_pair, sample_pair) in meters.chunks_mut(2).zip(last_samples.chunks(2)) {
        if meter_pair.len() == 2 {
            let (left, right) = meter_pair.split_at_mut(1);
            let (left_peak, right_peak) = TruePeakMeter::process_stereo_sample(
                &mut left[0],
                &mut right[0],
                sample_pair[0],
                sample_pair[1],
            );
            detected = detected.max(left_peak);
            detected = detected.max(right_peak);
        } else {
            detected = detected.max(meter_pair[0].process_sample(sample_pair[0]));
        }
    }
    detected
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

    #[test]
    fn caller_owned_output_matches_allocating_api_and_reuses_capacity() {
        let sample_rate = 48_000;
        let mut allocating =
            TruePeakLimiter::new(sample_rate, 3, -2.0, LimiterConfig::default()).unwrap();
        let mut reusing =
            TruePeakLimiter::new(sample_rate, 3, -2.0, LimiterConfig::default()).unwrap();
        let mut expected = vec![Vec::new(); 3];
        let mut actual = (0..3)
            .map(|_| Vec::with_capacity(1_024))
            .collect::<Vec<_>>();
        let initial_pointers = actual
            .iter()
            .map(|channel| channel.as_ptr())
            .collect::<Vec<_>>();

        for chunk_index in 0..12 {
            let input = (0..3)
                .map(|channel| {
                    (0..1_024)
                        .map(|frame| {
                            let phase = (chunk_index * 1_024 + frame) as f64
                                * (0.017 + channel as f64 * 0.003);
                            (phase.sin() * (0.7 + channel as f64 * 0.2)) as f32
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let allocated = allocating.process(&input).unwrap();
            reusing.process_into(&input, &mut actual).unwrap();
            for channel in 0..3 {
                expected[channel].extend_from_slice(&allocated[channel]);
                assert_eq!(actual[channel], allocated[channel]);
                assert_eq!(actual[channel].as_ptr(), initial_pointers[channel]);
            }
        }

        let allocated_tail = allocating.finish();
        reusing.finish_into(&mut actual).unwrap();
        for channel in 0..3 {
            expected[channel].extend_from_slice(&allocated_tail[channel]);
            assert_eq!(actual[channel], allocated_tail[channel]);
            assert_eq!(actual[channel].as_ptr(), initial_pointers[channel]);
            assert_eq!(expected[channel].len(), 12 * 1_024);
        }
    }

    #[test]
    fn caller_owned_output_rejects_wrong_channel_count_without_mutation() {
        let mut limiter = TruePeakLimiter::new(48_000, 2, -1.0, LimiterConfig::default()).unwrap();
        let input = vec![vec![0.0; 512], vec![0.0; 512]];
        let mut output = vec![vec![7.0]];
        assert_eq!(
            limiter.process_into(&input, &mut output).unwrap_err(),
            "limiter output channel count changed"
        );
        assert_eq!(output, [vec![7.0]]);
    }
}
