//! Bounded-memory, delay-compensated sample-rate conversion.

use crate::wav::AudioBuffer;
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Indexing, Resampler, WindowFunction};
use serde::{Deserialize, Serialize};

const CHUNK_FRAMES: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResampleQuality {
    Fast,
    #[default]
    Balanced,
    Best,
}

impl ResampleQuality {
    pub fn parse(value: &str) -> Self {
        match value {
            "fast" => Self::Fast,
            "best" => Self::Best,
            _ => Self::Balanced,
        }
    }

    fn fft_settings(self) -> (usize, WindowFunction) {
        match self {
            // More sub-chunks use shorter FFT filters. This lowers latency and
            // CPU cost at the expense of a wider transition band.
            Self::Fast => (8, WindowFunction::Hann2),
            Self::Balanced => (2, WindowFunction::BlackmanHarris),
            Self::Best => (1, WindowFunction::BlackmanHarris2),
        }
    }
}

pub struct SampleRateConverter {
    resampler: Fft<f32>,
    channels: usize,
    input_pending: Vec<Vec<f32>>,
    expected_output_frames: usize,
    emitted_frames: usize,
    delay_frames_remaining: usize,
    max_flush_passes: usize,
}

impl SampleRateConverter {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        input_frames: usize,
        channels: usize,
        quality: ResampleQuality,
    ) -> Result<Self, String> {
        let expected_output_frames = ((input_frames as u128 * output_rate as u128
            + input_rate as u128 / 2)
            / input_rate as u128) as usize;
        Self::new_with_expected_output(
            input_rate,
            output_rate,
            expected_output_frames,
            channels,
            quality,
        )
    }

    pub fn new_with_expected_output(
        input_rate: u32,
        output_rate: u32,
        expected_output_frames: usize,
        channels: usize,
        quality: ResampleQuality,
    ) -> Result<Self, String> {
        if input_rate == 0 || output_rate == 0 || channels == 0 {
            return Err("sample rates and channel count must be positive".into());
        }
        if input_rate == output_rate {
            return Err("sample-rate converter requires different input and output rates".into());
        }
        let (sub_chunks, window) = quality.fft_settings();
        let resampler = Fft::new_custom(
            input_rate as usize,
            output_rate as usize,
            CHUNK_FRAMES,
            sub_chunks,
            channels,
            window,
            FixedSync::Input,
        )
        .map_err(|error| format!("create sample-rate converter: {error}"))?;
        let delay_frames_remaining = resampler.output_delay();
        // Fixed-input FFT resampling can temporarily retain almost one complete
        // FFT block. Bound tail flushing by one block plus a small safety margin,
        // including rate pairs whose minimum FFT block is larger than CHUNK_FRAMES.
        let ratio = resampler.resample_ratio();
        // output_delay() is floor(fft_size_out / 2). Add one before mapping
        // back to input frames so odd output block sizes remain bounded too.
        let fft_block_output_upper = delay_frames_remaining.saturating_mul(2).saturating_add(1);
        let fft_block_input_frames = (fft_block_output_upper as f64 / ratio).ceil() as usize;
        let max_flush_passes = fft_block_input_frames.div_ceil(CHUNK_FRAMES) + 3;
        Ok(Self {
            resampler,
            channels,
            input_pending: vec![Vec::new(); channels],
            expected_output_frames,
            emitted_frames: 0,
            delay_frames_remaining,
            max_flush_passes,
        })
    }

    pub fn process(
        &mut self,
        planar: &[Vec<f32>],
        mut consume: impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate(planar)?;
        for (pending, channel) in self.input_pending.iter_mut().zip(planar) {
            pending.extend_from_slice(channel);
        }
        let input_frames = self.resampler.input_frames_next();
        while self.input_pending[0].len() >= input_frames {
            let output = self.resample(None, "resample audio")?;
            for channel in &mut self.input_pending {
                channel.drain(..input_frames);
            }
            self.emit(output, &mut consume)?;
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        mut consume: impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let pending_frames = self.input_pending[0].len();
        if pending_frames > 0 {
            let output = self.resample(Some(pending_frames), "finish sample-rate conversion")?;
            for channel in &mut self.input_pending {
                channel.clear();
            }
            self.emit(output, &mut consume)?;
        }
        for _ in 0..self.max_flush_passes {
            if self.emitted_frames == self.expected_output_frames {
                break;
            }
            let output = self.resample(Some(0), "flush sample-rate conversion")?;
            self.emit(output, &mut consume)?;
        }
        if self.emitted_frames != self.expected_output_frames {
            return Err(format!(
                "sample-rate converter produced {} frames, expected {}",
                self.emitted_frames, self.expected_output_frames
            ));
        }
        Ok(())
    }

    fn resample(
        &mut self,
        partial_len: Option<usize>,
        operation: &str,
    ) -> Result<Vec<Vec<f32>>, String> {
        let input_frames = partial_len.unwrap_or_else(|| self.resampler.input_frames_next());
        let output_frames = self.resampler.output_frames_next();
        let input =
            SequentialSliceOfVecs::new(self.input_pending.as_slice(), self.channels, input_frames)
                .map_err(|error| format!("prepare resampler input: {error}"))?;
        let mut output = vec![vec![0.0_f32; output_frames]; self.channels];
        let indexing = partial_len.map(|frames| Indexing::new().partial_len(frames));
        let produced = {
            let mut output_adapter =
                SequentialSliceOfVecs::new_mut(output.as_mut_slice(), self.channels, output_frames)
                    .map_err(|error| format!("prepare resampler output: {error}"))?;
            self.resampler
                .process_into_buffer(&input, &mut output_adapter, indexing.as_ref())
                .map_err(|error| format!("{operation}: {error}"))?
                .1
        };
        for channel in &mut output {
            channel.truncate(produced);
        }
        Ok(output)
    }

    fn emit(
        &mut self,
        output: Vec<Vec<f32>>,
        consume: &mut impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let frames = output.first().map_or(0, Vec::len);
        let start = self.delay_frames_remaining.min(frames);
        self.delay_frames_remaining -= start;
        let remaining = self
            .expected_output_frames
            .saturating_sub(self.emitted_frames);
        let end = frames.min(start.saturating_add(remaining));
        if end <= start {
            return Ok(());
        }
        let mut trimmed = output
            .into_iter()
            .map(|channel| channel[start..end].to_vec())
            .collect::<Vec<_>>();
        self.emitted_frames += end - start;
        consume(&mut trimmed)
    }

    fn validate(&self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.channels {
            return Err(format!(
                "sample-rate converter expected {} channels, got {}",
                self.channels,
                planar.len()
            ));
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("sample-rate converter received uneven channel lengths".into());
        }
        Ok(())
    }
}

pub fn convert_buffer(
    input: &AudioBuffer,
    output_rate: u32,
    quality: ResampleQuality,
) -> Result<AudioBuffer, String> {
    if output_rate == input.sample_rate {
        return Ok(input.clone());
    }
    let mut converter = SampleRateConverter::new(
        input.sample_rate,
        output_rate,
        input.frames,
        input.channels as usize,
        quality,
    )?;
    let mut data = vec![Vec::new(); input.channels as usize];
    converter.process(&input.data, |chunk| {
        for (destination, source) in data.iter_mut().zip(chunk) {
            destination.extend_from_slice(source);
        }
        Ok(())
    })?;
    converter.finish(|chunk| {
        for (destination, source) in data.iter_mut().zip(chunk) {
            destination.extend_from_slice(source);
        }
        Ok(())
    })?;
    let frames = data.first().map_or(0, Vec::len);
    Ok(AudioBuffer {
        sample_rate: output_rate,
        channels: input.channels,
        frames,
        data,
        channel_roles: input.channel_roles.clone(),
        source_kind: input.source_kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{default_channel_roles, PcmKind};
    use std::f32::consts::TAU;

    fn sine(sample_rate: u32, frames: usize) -> AudioBuffer {
        let data = (0..frames)
            .map(|frame| 0.25 * (TAU * 997.0 * frame as f32 / sample_rate as f32).sin())
            .collect::<Vec<_>>();
        AudioBuffer {
            sample_rate,
            channels: 1,
            frames,
            data: vec![data],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        }
    }

    #[test]
    fn conversion_has_exact_duration_and_preserves_level() {
        let input = sine(44_100, 44_100);
        let output = convert_buffer(&input, 48_000, ResampleQuality::Best).unwrap();
        assert_eq!(output.frames, 48_000);
        let input_rms =
            (input.data[0].iter().map(|x| x * x).sum::<f32>() / input.frames as f32).sqrt();
        let output_rms =
            (output.data[0].iter().map(|x| x * x).sum::<f32>() / output.frames as f32).sqrt();
        assert!((input_rms - output_rms).abs() < 0.001);
    }

    #[test]
    fn conversion_compensates_filter_delay() {
        let mut input = sine(48_000, 48_000);
        input.data[0].fill(0.0);
        input.data[0][10_000] = 1.0;
        let output = convert_buffer(&input, 44_100, ResampleQuality::Balanced).unwrap();
        let peak = output.data[0]
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.abs().total_cmp(&right.1.abs()))
            .unwrap()
            .0;
        let expected = (10_000.0_f64 * 44_100.0 / 48_000.0).round() as usize;
        assert!(
            peak.abs_diff(expected) <= 1,
            "impulse at {peak}, expected {expected}"
        );
    }

    #[test]
    fn round_trip_preserves_duration_and_signal() {
        let mut input = sine(48_000, 48_000);
        for (frame, sample) in input.data[0].iter_mut().enumerate() {
            *sample += 0.1 * (TAU * 3_127.0 * frame as f32 / 48_000.0).sin()
                + 0.05 * (TAU * 521.0 * frame as f32 / 48_000.0).sin();
        }
        let down = convert_buffer(&input, 44_100, ResampleQuality::Balanced).unwrap();
        let output = convert_buffer(&down, 48_000, ResampleQuality::Balanced).unwrap();
        assert_eq!(output.frames, input.frames);
        let (lag, error) = (-128_i32..=128)
            .map(|lag| {
                let start_input = lag.max(0) as usize;
                let start_output = (-lag).max(0) as usize;
                let length = (input.frames - start_input).min(output.frames - start_output);
                let error = input.data[0][start_input..start_input + length]
                    .iter()
                    .zip(&output.data[0][start_output..start_output + length])
                    .map(|(a, b)| (a - b).abs())
                    .sum::<f32>()
                    / length as f32;
                (lag, error)
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
            .unwrap();
        assert!(lag.abs() <= 1, "uncompensated delay of {lag} frames");
        assert!(error < 0.005, "mean absolute error {error}");
    }

    #[test]
    fn streaming_exact_chunks_flush_delay_and_keep_channel_order() {
        let input_rate = 44_100;
        let output_rate = 48_000;
        let frames = CHUNK_FRAMES * 4;
        let mut planar = vec![vec![0.0_f32; frames]; 2];
        planar[0][1_000] = 1.0;
        planar[1][3_000] = -1.0;
        let expected_frames = ((frames as u128 * output_rate as u128 + input_rate as u128 / 2)
            / input_rate as u128) as usize;
        let mut converter = SampleRateConverter::new(
            input_rate,
            output_rate,
            frames,
            2,
            ResampleQuality::Balanced,
        )
        .unwrap();
        let output_bound = converter.resampler.output_frames_max();
        let mut output = [Vec::new(), Vec::new()];
        let mut largest_chunk = 0;
        for range in [0..137, 137..1_111, 1_111..2_047, 2_047..frames] {
            let chunk = planar
                .iter()
                .map(|channel| channel[range.clone()].to_vec())
                .collect::<Vec<_>>();
            converter
                .process(&chunk, |resampled| {
                    largest_chunk = largest_chunk.max(resampled[0].len());
                    for (destination, source) in output.iter_mut().zip(resampled) {
                        destination.extend_from_slice(source);
                    }
                    Ok(())
                })
                .unwrap();
        }
        converter
            .finish(|resampled| {
                largest_chunk = largest_chunk.max(resampled[0].len());
                for (destination, source) in output.iter_mut().zip(resampled) {
                    destination.extend_from_slice(source);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(output[0].len(), expected_frames);
        assert_eq!(output[1].len(), expected_frames);
        assert!(largest_chunk <= output_bound);
        let peaks = output
            .iter()
            .map(|channel| {
                channel
                    .iter()
                    .enumerate()
                    .max_by(|left, right| left.1.abs().total_cmp(&right.1.abs()))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let expected_left = (1_000.0_f64 * output_rate as f64 / input_rate as f64).round();
        let expected_right = (3_000.0_f64 * output_rate as f64 / input_rate as f64).round();
        assert!(peaks[0].0.abs_diff(expected_left as usize) <= 1);
        assert!(peaks[1].0.abs_diff(expected_right as usize) <= 1);
        assert!(*peaks[0].1 > 0.5);
        assert!(*peaks[1].1 < -0.5);
    }

    #[test]
    fn every_quality_preserves_high_frequency_passband_and_partial_tail() {
        let input_rate = 48_000;
        let output_rate = 44_100;
        let frames = 48_000;
        let frequency = 16_000.0_f32;
        let samples = (0..frames)
            .map(|frame| 0.25 * (TAU * frequency * frame as f32 / input_rate as f32).sin())
            .collect::<Vec<_>>();
        let input = AudioBuffer {
            sample_rate: input_rate,
            channels: 1,
            frames,
            data: vec![samples],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
        let input_rms = (input.data[0].iter().map(|x| x * x).sum::<f32>() / frames as f32).sqrt();

        for quality in [
            ResampleQuality::Fast,
            ResampleQuality::Balanced,
            ResampleQuality::Best,
        ] {
            let output = convert_buffer(&input, output_rate, quality).unwrap();
            assert_eq!(output.frames, 44_100, "{quality:?}");
            let passband = &output.data[0][1_000..output.frames - 1_000];
            let output_rms =
                (passband.iter().map(|x| x * x).sum::<f32>() / passband.len() as f32).sqrt();
            let gain_db = 20.0 * (output_rms / input_rms).log10();
            assert!(
                (-0.5..=0.5).contains(&gain_db),
                "{quality:?} changed 16 kHz by {gain_db} dB"
            );
        }
    }

    #[test]
    fn coprime_rates_flush_across_zero_output_chunks() {
        let input = sine(1_031, CHUNK_FRAMES);
        let output = convert_buffer(&input, 1_033, ResampleQuality::Balanced).unwrap();
        let expected = ((CHUNK_FRAMES as u128 * 1_033 + 1_031 / 2) / 1_031) as usize;
        assert_eq!(output.frames, expected);
        assert!(output.data[0].iter().any(|sample| sample.abs() > 0.01));
    }

    #[test]
    fn odd_fft_output_block_flushes_the_complete_tail() {
        let input = sine(48_000, 24_000);
        let output = convert_buffer(&input, 1, ResampleQuality::Balanced).unwrap();
        assert_eq!(output.frames, 1);
        assert!(output.data[0][0].is_finite());
    }
}
