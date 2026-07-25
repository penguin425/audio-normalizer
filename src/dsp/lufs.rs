//! Gated integrated loudness per ITU-R BS.1770-5 / EBU R128.
//!
//! Implements the full two-stage gating scheme:
//!   1. 400 ms blocks with 75% overlap (100 ms hop), absolute gate at -70 LUFS.
//!   2. Relative gate at -10 dB below the absolute-gated mean loudness.
//! The relative gate is applied in the *linear* mean-square domain (it is just
//! the absolute-gated mean square divided by 10), which avoids repeated
//! log/exp conversions and is numerically exact.
//!
//! Performance notes:
//!   * K-weighting runs once per channel in parallel (rayon).
//!   * Per-channel prefix sums of squared K-weighted samples make every block's
//!     energy an O(1) difference — no redundant work despite 75% overlap.
//!   * Squared-sample summation uses the SIMD `sum_squares_f64` primitive.

use crate::dsp::kwfilter::KWeight;
use crate::dsp::simd;
use crate::dsp::truepeak::TruePeakMeter;
use crate::wav::{AudioBuffer, ChannelRole};
use rayon::prelude::*;
use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct EbuMeasurements {
    pub integrated_lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub gating_blocks: Vec<f64>,
}

pub struct StreamingMeasurements {
    pub ebu: EbuMeasurements,
    pub frames: usize,
    pub rms_db: f64,
    pub sample_peak: f32,
    pub true_peak: f32,
    pub timeline: Vec<LoudnessTimelinePoint>,
}

#[derive(Debug, Clone)]
pub struct LoudnessTimelinePoint {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub momentary_lufs: Option<f64>,
    pub short_term_lufs: Option<f64>,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

pub struct StreamingAnalyzer {
    sample_rate: u32,
    roles: Vec<ChannelRole>,
    filters: Vec<KWeight>,
    true_peak_meters: Vec<TruePeakMeter>,
    momentary: VecDeque<f64>,
    short_term: VecDeque<f64>,
    momentary_sum: f64,
    short_term_sum: f64,
    gating_blocks: Vec<f64>,
    short_term_blocks: Vec<f64>,
    max_momentary_ms: f64,
    max_short_term_ms: f64,
    frames: usize,
    raw_sum_squares: f64,
    sample_peak: f32,
    timeline_interval_frames: Option<usize>,
    timeline: Vec<LoudnessTimelinePoint>,
    timeline_start_frame: usize,
    interval_sample_peak: f32,
    interval_true_peak: f32,
}

impl StreamingAnalyzer {
    pub fn new(sample_rate: u32, roles: Vec<ChannelRole>) -> Self {
        Self::with_timeline_interval(sample_rate, roles, None)
    }

    pub fn with_timeline_interval(
        sample_rate: u32,
        roles: Vec<ChannelRole>,
        interval_frames: Option<usize>,
    ) -> Self {
        let channels = roles.len();
        Self {
            sample_rate,
            roles,
            filters: (0..channels)
                .map(|_| KWeight::for_sample_rate(sample_rate))
                .collect(),
            true_peak_meters: (0..channels).map(|_| TruePeakMeter::new()).collect(),
            momentary: VecDeque::new(),
            short_term: VecDeque::new(),
            momentary_sum: 0.0,
            short_term_sum: 0.0,
            gating_blocks: Vec::new(),
            short_term_blocks: Vec::new(),
            max_momentary_ms: 0.0,
            max_short_term_ms: 0.0,
            frames: 0,
            raw_sum_squares: 0.0,
            sample_peak: 0.0,
            timeline_interval_frames: interval_frames,
            timeline: Vec::new(),
            timeline_start_frame: 0,
            interval_sample_peak: 0.0,
            interval_true_peak: 0.0,
        }
    }

    pub fn process(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.roles.len() {
            return Err("stream channel count changed".into());
        }
        let chunk_frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != chunk_frames) {
            return Err("stream channel length mismatch".into());
        }
        let momentary_window = ((self.sample_rate as usize * 4) / 10).max(1);
        let short_term_window = (self.sample_rate as usize * 3).max(1);
        let hop = (self.sample_rate as usize / 10).max(1);
        for frame in 0..chunk_frames {
            let mut weighted = 0.0;
            for ((index, channel), filter) in planar.iter().enumerate().zip(self.filters.iter_mut())
            {
                let sample = channel[frame];
                let reconstructed_peak = self.true_peak_meters[index].process_sample(sample);
                self.interval_true_peak = self.interval_true_peak.max(reconstructed_peak);
                self.interval_sample_peak = self.interval_sample_peak.max(sample.abs());
                let filtered = filter.process(sample) as f64;
                weighted += channel_weight(self.roles[index]) * filtered * filtered;
                let raw = sample as f64;
                self.raw_sum_squares += raw * raw;
                self.sample_peak = self.sample_peak.max(sample.abs());
            }
            push_window(
                &mut self.momentary,
                &mut self.momentary_sum,
                weighted,
                momentary_window,
            );
            push_window(
                &mut self.short_term,
                &mut self.short_term_sum,
                weighted,
                short_term_window,
            );
            self.frames += 1;
            if self.momentary.len() == momentary_window {
                self.max_momentary_ms = self
                    .max_momentary_ms
                    .max(self.momentary_sum / momentary_window as f64);
            }
            if self.short_term.len() == short_term_window {
                self.max_short_term_ms = self
                    .max_short_term_ms
                    .max(self.short_term_sum / short_term_window as f64);
            }
            if self.momentary.len() == momentary_window
                && (self.frames - momentary_window).is_multiple_of(hop)
            {
                self.gating_blocks
                    .push(self.momentary_sum / momentary_window as f64);
            }
            if self.short_term.len() == short_term_window
                && (self.frames - short_term_window).is_multiple_of(hop)
            {
                self.short_term_blocks
                    .push(self.short_term_sum / short_term_window as f64);
            }
            if self
                .timeline_interval_frames
                .is_some_and(|interval| self.frames.is_multiple_of(interval))
            {
                self.record_timeline_point(momentary_window, short_term_window);
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> StreamingMeasurements {
        if self.timeline_interval_frames.is_some() && self.timeline_start_frame < self.frames {
            let momentary_window = ((self.sample_rate as usize * 4) / 10).max(1);
            let short_term_window = (self.sample_rate as usize * 3).max(1);
            self.record_timeline_point(momentary_window, short_term_window);
        }
        let channels = self.roles.len();
        let total_samples = self.frames * channels;
        let rms = if total_samples == 0 {
            0.0
        } else {
            (self.raw_sum_squares / total_samples as f64).sqrt()
        };
        let mut ebu = measurements_from_blocks(self.gating_blocks, &self.short_term_blocks);
        ebu.max_momentary_lufs = maximum_loudness(&[self.max_momentary_ms]);
        ebu.max_short_term_lufs = maximum_loudness(&[self.max_short_term_ms]);
        StreamingMeasurements {
            ebu,
            frames: self.frames,
            rms_db: if rms > 0.0 {
                20.0 * rms.log10()
            } else {
                f64::NEG_INFINITY
            },
            sample_peak: self.sample_peak,
            true_peak: self
                .true_peak_meters
                .iter()
                .map(TruePeakMeter::peak)
                .fold(0.0, f32::max),
            timeline: self.timeline,
        }
    }

    fn record_timeline_point(&mut self, momentary_window: usize, short_term_window: usize) {
        self.timeline.push(LoudnessTimelinePoint {
            start_seconds: self.timeline_start_frame as f64 / self.sample_rate as f64,
            end_seconds: self.frames as f64 / self.sample_rate as f64,
            momentary_lufs: complete_window_loudness(
                self.momentary_sum,
                self.momentary.len(),
                momentary_window,
            ),
            short_term_lufs: complete_window_loudness(
                self.short_term_sum,
                self.short_term.len(),
                short_term_window,
            ),
            sample_peak_dbfs: amplitude_db(self.interval_sample_peak),
            true_peak_dbtp: amplitude_db(self.interval_true_peak),
        });
        self.timeline_start_frame = self.frames;
        self.interval_sample_peak = 0.0;
        self.interval_true_peak = 0.0;
    }
}

fn complete_window_loudness(sum: f64, length: usize, required: usize) -> Option<f64> {
    (length == required && sum > 0.0).then(|| mean_square_to_lufs(sum / required as f64))
}

fn amplitude_db(amplitude: f32) -> f64 {
    if amplitude > 0.0 {
        20.0 * (amplitude as f64).log10()
    } else {
        f64::NEG_INFINITY
    }
}

fn push_window(queue: &mut VecDeque<f64>, sum: &mut f64, value: f64, limit: usize) {
    queue.push_back(value);
    *sum += value;
    if queue.len() > limit {
        *sum -= queue.pop_front().unwrap();
    }
}

/// Per-channel loudness weight (BS.1770).
pub fn channel_weight(role: ChannelRole) -> f64 {
    match role {
        ChannelRole::Main => 1.0,
        ChannelRole::Surround => 1.41,
        ChannelRole::DualMono => 2.0,
        ChannelRole::Positioned {
            azimuth_degrees,
            elevation_degrees,
        } => {
            let azimuth = azimuth_degrees.unsigned_abs();
            let elevation = elevation_degrees.unsigned_abs();
            if elevation < 30 && (60..=120).contains(&azimuth) {
                1.41
            } else {
                1.0
            }
        }
        ChannelRole::Lfe => 0.0,
    }
}

/// Integrated gated loudness in LUFS, or `-inf` for silence.
pub fn measure_lufs(buf: &AudioBuffer) -> f64 {
    measure_ebu(buf).integrated_lufs
}

/// Weighted mean-square energies for every complete 400 ms gating block.
pub fn measure_blocks(buf: &AudioBuffer) -> Vec<f64> {
    measure_ebu(buf).gating_blocks
}

/// Complete EBU Mode file measurement.
pub fn measure_ebu(buf: &AudioBuffer) -> EbuMeasurements {
    let fs = buf.sample_rate as usize;
    let momentary_window = (0.4 * fs as f64).round() as usize;
    let short_term_window = (3.0 * fs as f64).round() as usize;
    let hop = (0.1 * fs as f64).round() as usize;
    if momentary_window == 0 || hop == 0 || buf.frames < momentary_window {
        return EbuMeasurements {
            integrated_lufs: f64::NEG_INFINITY,
            max_momentary_lufs: f64::NEG_INFINITY,
            max_short_term_lufs: f64::NEG_INFINITY,
            loudness_range_lu: 0.0,
            gating_blocks: Vec::new(),
        };
    }

    // K-weight each channel (parallel) and build prefix sums of squares.
    let prefixes: Vec<Vec<f64>> = buf
        .data
        .par_iter()
        .map(|ch| {
            let mut kw = KWeight::for_sample_rate(buf.sample_rate);
            let mut filt = vec![0.0f32; ch.len()];
            kw.process_block(ch, &mut filt);
            let mut p = Vec::with_capacity(ch.len() + 1);
            p.push(0.0);
            let mut acc = 0.0f64;
            for &x in &filt {
                let v = x as f64;
                acc += v * v;
                p.push(acc);
            }
            p
        })
        .collect();

    let weights: Vec<f64> = (0..buf.channels as usize)
        .map(|index| channel_weight(buf.channel_role(index)))
        .collect();

    let gating_blocks = window_mean_squares(&prefixes, &weights, buf.frames, momentary_window, hop);
    let short_term_blocks =
        window_mean_squares(&prefixes, &weights, buf.frames, short_term_window, hop);

    let mut measurements = measurements_from_blocks(gating_blocks, &short_term_blocks);
    measurements.max_momentary_lufs =
        maximum_window_loudness(&prefixes, &weights, buf.frames, momentary_window);
    measurements.max_short_term_lufs =
        maximum_window_loudness(&prefixes, &weights, buf.frames, short_term_window);
    measurements
}

fn maximum_window_loudness(
    prefixes: &[Vec<f64>],
    weights: &[f64],
    frames: usize,
    window: usize,
) -> f64 {
    if window == 0 || frames < window {
        return f64::NEG_INFINITY;
    }
    let mut maximum = 0.0_f64;
    for start in 0..=frames - window {
        let mut total = 0.0;
        for channel in 0..prefixes.len() {
            if weights[channel] != 0.0 {
                total += weights[channel]
                    * (prefixes[channel][start + window] - prefixes[channel][start]);
            }
        }
        maximum = maximum.max(total / window as f64);
    }
    maximum_loudness(&[maximum])
}

fn measurements_from_blocks(gating_blocks: Vec<f64>, short_term_blocks: &[f64]) -> EbuMeasurements {
    EbuMeasurements {
        integrated_lufs: gated_lufs(&gating_blocks),
        max_momentary_lufs: maximum_loudness(&gating_blocks),
        max_short_term_lufs: maximum_loudness(short_term_blocks),
        loudness_range_lu: loudness_range(short_term_blocks),
        gating_blocks,
    }
}

fn window_mean_squares(
    prefixes: &[Vec<f64>],
    weights: &[f64],
    frames: usize,
    window: usize,
    hop: usize,
) -> Vec<f64> {
    if window == 0 || hop == 0 || frames < window {
        return Vec::new();
    }
    let mut means = Vec::new();
    let mut b = 0usize;
    while b + window <= frames {
        let mut total = 0.0f64;
        for c in 0..prefixes.len() {
            let w = weights[c];
            if w == 0.0 {
                continue;
            }
            let ss = prefixes[c][b + window] - prefixes[c][b];
            total += w * ss;
        }
        means.push(total / window as f64);
        b += hop;
    }
    means
}

/// Apply the BS.1770 absolute and relative gates to a population of blocks.
pub fn gated_lufs(block_ms: &[f64]) -> f64 {
    if block_ms.is_empty() {
        return f64::NEG_INFINITY;
    }

    let abs_gate_ms = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
    let abs_gated: Vec<f64> = block_ms
        .iter()
        .copied()
        .filter(|&m| m >= abs_gate_ms)
        .collect();
    if abs_gated.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mean_ms: f64 = abs_gated.iter().sum::<f64>() / abs_gated.len() as f64;
    let rel_gate_ms = mean_ms / 10.0; // -10 dB in the linear domain
    let gate = abs_gate_ms.max(rel_gate_ms);
    let final_set: Vec<f64> = block_ms.iter().copied().filter(|&m| m >= gate).collect();
    let used = if final_set.is_empty() {
        mean_ms
    } else {
        final_set.iter().sum::<f64>() / final_set.len() as f64
    };
    -0.691 + 10.0 * used.log10()
}

fn maximum_loudness(blocks: &[f64]) -> f64 {
    blocks
        .iter()
        .copied()
        .filter(|value| *value > 0.0)
        .map(mean_square_to_lufs)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Loudness Range per EBU Tech 3342.
pub fn loudness_range(short_term_ms: &[f64]) -> f64 {
    let abs_gate_ms = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
    let absolute: Vec<f64> = short_term_ms
        .iter()
        .copied()
        .filter(|value| *value >= abs_gate_ms)
        .collect();
    if absolute.is_empty() {
        return 0.0;
    }

    let absolute_mean = absolute.iter().sum::<f64>() / absolute.len() as f64;
    let relative_gate = absolute_mean / 100.0; // -20 LU
    let mut gated: Vec<f64> = absolute
        .into_iter()
        .filter(|value| *value >= relative_gate)
        .map(mean_square_to_lufs)
        .collect();
    if gated.len() < 2 {
        return 0.0;
    }
    gated.sort_by(f64::total_cmp);
    percentile(&gated, 0.95) - percentile(&gated, 0.10)
}

fn mean_square_to_lufs(value: f64) -> f64 {
    -0.691 + 10.0 * value.log10()
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let position = fraction * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let mix = position - lower as f64;
    sorted[lower] * (1.0 - mix) + sorted[upper] * mix
}

/// RMS level (dBFS) and sample peak (0..1) across all channels, computed in
/// parallel with SIMD primitives.
pub fn measure_rms_peak(buf: &AudioBuffer) -> (f64, f32) {
    let (sumsq, peak) = buf
        .data
        .par_iter()
        .map(|ch| (simd::sum_squares_f64(ch), simd::abs_max(ch)))
        .reduce(|| (0.0f64, 0.0f32), |(s, p), (s2, p2)| (s + s2, p.max(p2)));
    let total = (buf.frames as f64) * (buf.channels as f64);
    let rms = if total > 0.0 {
        (sumsq / total).sqrt()
    } else {
        0.0
    };
    let rms_db = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    (rms_db, peak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::PcmKind;

    fn mono(samples: Vec<f32>, sample_rate: u32) -> AudioBuffer {
        AudioBuffer {
            sample_rate,
            channels: 1,
            frames: samples.len(),
            data: vec![samples],
            channel_roles: vec![ChannelRole::Main],
            source_kind: PcmKind::F32,
        }
    }

    #[test]
    fn incomplete_trailing_hop_does_not_create_a_block() {
        let sr = 1_000;
        let mut samples = vec![0.1; 400];
        samples.extend(vec![1.0; 50]);
        let with_tail = mono(samples, sr);
        let complete_only = mono(vec![0.1; 400], sr);

        assert_eq!(measure_blocks(&with_tail).len(), 1);
        assert_eq!(measure_lufs(&with_tail), measure_lufs(&complete_only));
    }

    #[test]
    fn channel_roles_select_bs1770_weights() {
        assert_eq!(channel_weight(ChannelRole::Main), 1.0);
        assert_eq!(channel_weight(ChannelRole::Surround), 1.41);
        assert_eq!(channel_weight(ChannelRole::DualMono), 2.0);
        assert_eq!(
            channel_weight(ChannelRole::positioned(-90, 0)),
            1.41,
            "side channels receive the Annex 3 +1.5 dB weighting"
        );
        assert_eq!(
            channel_weight(ChannelRole::positioned(-135, 0)),
            1.0,
            "rear channels outside ±120 degrees use unity weighting"
        );
        assert_eq!(
            channel_weight(ChannelRole::positioned(-90, 45)),
            1.0,
            "elevated channels use unity weighting"
        );
        assert_eq!(channel_weight(ChannelRole::Lfe), 0.0);
    }

    #[test]
    fn dual_mono_adds_the_two_speaker_pan_law() {
        let samples: Vec<f32> = (0..48_000)
            .map(|index| ((index as f64 * 0.13).sin() * 0.1) as f32)
            .collect();
        let ordinary = mono(samples.clone(), 48_000);
        let mut dual = mono(samples, 48_000);
        dual.channel_roles[0] = ChannelRole::DualMono;
        let difference = measure_lufs(&dual) - measure_lufs(&ordinary);
        assert!((difference - 10.0 * 2.0_f64.log10()).abs() < 1e-9);
    }

    #[test]
    fn loudness_range_uses_tenth_and_ninety_fifth_percentiles() {
        let blocks: Vec<f64> = (0..=100)
            .map(|step| {
                let lufs = -30.0 + step as f64 / 10.0;
                10.0_f64.powf((lufs + 0.691) / 10.0)
            })
            .collect();
        let range = loudness_range(&blocks);
        assert!((range - 8.5).abs() < 0.01, "LRA = {range}");
    }

    #[test]
    fn streaming_measurement_matches_whole_buffer() {
        let samples: Vec<f32> = (0..192_000)
            .map(|index| ((index as f64 * 0.071).sin() * 0.3) as f32)
            .collect();
        let buffer = mono(samples.clone(), 48_000);
        let whole_ebu = measure_ebu(&buffer);
        let (whole_rms, whole_peak) = measure_rms_peak(&buffer);
        let whole_true_peak = crate::dsp::truepeak::measure_true_peak(&buffer);

        let mut streaming = StreamingAnalyzer::new(48_000, vec![ChannelRole::Main]);
        for chunk in samples.chunks(137) {
            streaming.process(&[chunk.to_vec()]).unwrap();
        }
        let streamed = streaming.finish();

        assert!(
            (streamed.ebu.integrated_lufs - whole_ebu.integrated_lufs).abs() < 1e-6,
            "streamed={}, whole={}",
            streamed.ebu.integrated_lufs,
            whole_ebu.integrated_lufs
        );
        assert!((streamed.ebu.max_momentary_lufs - whole_ebu.max_momentary_lufs).abs() < 1e-6);
        assert!((streamed.ebu.max_short_term_lufs - whole_ebu.max_short_term_lufs).abs() < 1e-6);
        assert!((streamed.ebu.loudness_range_lu - whole_ebu.loudness_range_lu).abs() < 1e-6);
        assert!((streamed.rms_db - whole_rms).abs() < 1e-9);
        assert_eq!(streamed.sample_peak, whole_peak);
        assert_eq!(streamed.true_peak, whole_true_peak);
    }

    #[test]
    fn timeline_uses_complete_windows_and_keeps_the_partial_interval() {
        let samples: Vec<f32> = (0..50_400)
            .map(|index| ((index as f64 * 0.13).sin() * 0.2) as f32)
            .collect();
        let mut analyzer =
            StreamingAnalyzer::with_timeline_interval(48_000, vec![ChannelRole::Main], Some(4_800));
        for chunk in samples.chunks(997) {
            analyzer.process(&[chunk.to_vec()]).unwrap();
        }
        let measured = analyzer.finish();
        assert_eq!(measured.timeline.len(), 11);
        assert_eq!(measured.timeline[0].start_seconds, 0.0);
        assert_eq!(measured.timeline[0].end_seconds, 0.1);
        assert!(measured.timeline[2].momentary_lufs.is_none());
        assert!(measured.timeline[3].momentary_lufs.is_some());
        assert!(measured
            .timeline
            .iter()
            .all(|point| point.short_term_lufs.is_none()));
        assert_eq!(measured.timeline[10].start_seconds, 1.0);
        assert_eq!(measured.timeline[10].end_seconds, 1.05);
        assert!(measured.timeline[10].true_peak_dbtp.is_finite());
    }
}
