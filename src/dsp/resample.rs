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
    input_rate: u32,
    output_rate: u32,
    channels: usize,
    input_pending: Vec<Vec<f32>>,
    output_buffer: Vec<Vec<f32>>,
    input_frames_seen: usize,
    expected_output_frames: Option<usize>,
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
        Self::new_inner(
            input_rate,
            output_rate,
            channels,
            quality,
            Some(expected_output_frames),
        )
    }

    pub fn new_with_expected_output(
        input_rate: u32,
        output_rate: u32,
        expected_output_frames: usize,
        channels: usize,
        quality: ResampleQuality,
    ) -> Result<Self, String> {
        Self::new_inner(
            input_rate,
            output_rate,
            channels,
            quality,
            Some(expected_output_frames),
        )
    }

    /// Create a converter for a stream whose complete input duration is not
    /// known until end-of-stream. The exact rounded output duration is derived
    /// from the number of frames accepted by [`Self::process`] before the
    /// delay-compensated tail is flushed.
    pub fn new_streaming(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
        quality: ResampleQuality,
    ) -> Result<Self, String> {
        Self::new_inner(input_rate, output_rate, channels, quality, None)
    }

    fn new_inner(
        input_rate: u32,
        output_rate: u32,
        channels: usize,
        quality: ResampleQuality,
        expected_output_frames: Option<usize>,
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
        let input_capacity = resampler.input_frames_max();
        let output_capacity = resampler.output_frames_max();
        Ok(Self {
            resampler,
            input_rate,
            output_rate,
            channels,
            input_pending: (0..channels)
                .map(|_| Vec::with_capacity(input_capacity))
                .collect(),
            output_buffer: (0..channels)
                .map(|_| Vec::with_capacity(output_capacity))
                .collect(),
            input_frames_seen: 0,
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
        let frames = planar.first().map_or(0, Vec::len);
        self.input_frames_seen = self
            .input_frames_seen
            .checked_add(frames)
            .ok_or_else(|| "sample-rate converter input duration overflow".to_string())?;
        let mut start = 0;
        while start < frames {
            let input_frames = self.resampler.input_frames_next();
            let pending_frames = self.input_pending[0].len();
            let take = (input_frames - pending_frames).min(frames - start);
            for (pending, channel) in self.input_pending.iter_mut().zip(planar) {
                pending.extend_from_slice(&channel[start..start + take]);
            }
            start += take;
            if self.input_pending[0].len() == input_frames {
                self.resample_and_emit(None, "resample audio", &mut consume)?;
                for channel in &mut self.input_pending {
                    channel.clear();
                }
            }
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        mut consume: impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let calculated_output_frames = ((self.input_frames_seen as u128 * self.output_rate as u128
            + self.input_rate as u128 / 2)
            / self.input_rate as u128) as usize;
        if let Some(expected) = self.expected_output_frames {
            if expected != calculated_output_frames {
                return Err(format!(
                    "sample-rate converter expected {expected} output frames from its caller, but received enough input for {calculated_output_frames}"
                ));
            }
        } else {
            self.expected_output_frames = Some(calculated_output_frames);
        }
        let pending_frames = self.input_pending[0].len();
        if pending_frames > 0 {
            self.resample_and_emit(
                Some(pending_frames),
                "finish sample-rate conversion",
                &mut consume,
            )?;
            for channel in &mut self.input_pending {
                channel.clear();
            }
        }
        for _ in 0..self.max_flush_passes {
            if self.emitted_frames == calculated_output_frames {
                break;
            }
            self.resample_and_emit(Some(0), "flush sample-rate conversion", &mut consume)?;
        }
        if self.emitted_frames != calculated_output_frames {
            return Err(format!(
                "sample-rate converter produced {} frames, expected {}",
                self.emitted_frames, calculated_output_frames
            ));
        }
        Ok(())
    }

    fn resample_and_emit(
        &mut self,
        partial_len: Option<usize>,
        operation: &str,
        consume: &mut impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let input_frames = partial_len.unwrap_or_else(|| self.resampler.input_frames_next());
        let output_frames = self.resampler.output_frames_next();
        let input =
            SequentialSliceOfVecs::new(self.input_pending.as_slice(), self.channels, input_frames)
                .map_err(|error| format!("prepare resampler input: {error}"))?;
        for channel in &mut self.output_buffer {
            channel.resize(output_frames, 0.0);
        }
        let indexing = partial_len.map(|frames| Indexing::new().partial_len(frames));
        let produced = {
            let mut output_adapter = SequentialSliceOfVecs::new_mut(
                self.output_buffer.as_mut_slice(),
                self.channels,
                output_frames,
            )
            .map_err(|error| format!("prepare resampler output: {error}"))?;
            self.resampler
                .process_into_buffer(&input, &mut output_adapter, indexing.as_ref())
                .map_err(|error| format!("{operation}: {error}"))?
                .1
        };
        for channel in &mut self.output_buffer {
            channel.truncate(produced);
        }
        self.emit(consume)
    }

    fn emit(
        &mut self,
        consume: &mut impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let frames = self.output_buffer.first().map_or(0, Vec::len);
        let start = self.delay_frames_remaining.min(frames);
        self.delay_frames_remaining -= start;
        let remaining = self.expected_output_frames.map_or(usize::MAX, |expected| {
            expected.saturating_sub(self.emitted_frames)
        });
        let end = frames.min(start.saturating_add(remaining));
        if end <= start {
            for channel in &mut self.output_buffer {
                channel.clear();
            }
            return Ok(());
        }
        if start > 0 {
            for channel in &mut self.output_buffer {
                channel.copy_within(start..end, 0);
            }
        }
        let emitted = end - start;
        for channel in &mut self.output_buffer {
            channel.truncate(emitted);
        }
        self.emitted_frames += end - start;
        consume(&mut self.output_buffer)
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
    fn unknown_duration_matches_declared_duration_and_reuses_storage() {
        let input_rate = 44_100;
        let output_rate = 48_000;
        let frames = CHUNK_FRAMES * 3 + 317;
        let planar = [
            (0..frames)
                .map(|frame| 0.2 * (TAU * 997.0 * frame as f32 / input_rate as f32).sin())
                .collect::<Vec<_>>(),
            (0..frames)
                .map(|frame| 0.1 * (TAU * 431.0 * frame as f32 / input_rate as f32).cos())
                .collect::<Vec<_>>(),
        ];
        let ranges = [0..73, 73..901, 901..1_777, 1_777..frames];

        let mut declared = SampleRateConverter::new(
            input_rate,
            output_rate,
            frames,
            2,
            ResampleQuality::Balanced,
        )
        .unwrap();
        let mut streaming = SampleRateConverter::new_streaming(
            input_rate,
            output_rate,
            2,
            ResampleQuality::Balanced,
        )
        .unwrap();
        let mut declared_output = [Vec::new(), Vec::new()];
        let mut streaming_output = [Vec::new(), Vec::new()];
        let mut output_pointers = Vec::new();

        for range in ranges {
            let chunk = planar
                .iter()
                .map(|channel| channel[range.clone()].to_vec())
                .collect::<Vec<_>>();
            declared
                .process(&chunk, |output| {
                    for (destination, source) in declared_output.iter_mut().zip(output) {
                        destination.extend_from_slice(source);
                    }
                    Ok(())
                })
                .unwrap();
            streaming
                .process(&chunk, |output| {
                    if !output[0].is_empty() {
                        output_pointers.push(output[0].as_ptr());
                    }
                    for (destination, source) in streaming_output.iter_mut().zip(output) {
                        destination.extend_from_slice(source);
                    }
                    Ok(())
                })
                .unwrap();
        }
        declared
            .finish(|output| {
                for (destination, source) in declared_output.iter_mut().zip(output) {
                    destination.extend_from_slice(source);
                }
                Ok(())
            })
            .unwrap();
        streaming
            .finish(|output| {
                if !output[0].is_empty() {
                    output_pointers.push(output[0].as_ptr());
                }
                for (destination, source) in streaming_output.iter_mut().zip(output) {
                    destination.extend_from_slice(source);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(streaming_output, declared_output);
        assert!(output_pointers.len() > 2);
        assert!(
            output_pointers
                .iter()
                .all(|pointer| *pointer == output_pointers[0]),
            "steady-state output storage was reallocated"
        );
        assert!(
            streaming.input_pending[0].len() < streaming.resampler.input_frames_next(),
            "processed input was retained instead of clearing the fixed block"
        );
    }

    #[test]
    fn declared_output_duration_rejects_mismatched_input() {
        let mut converter = SampleRateConverter::new_with_expected_output(
            44_100,
            48_000,
            48_000,
            1,
            ResampleQuality::Balanced,
        )
        .unwrap();
        converter.process(&[vec![0.0; 22_050]], |_| Ok(())).unwrap();
        let error = converter.finish(|_| Ok(())).unwrap_err();
        assert!(error.contains("received enough input for 24000"));
    }

    #[test]
    fn unknown_duration_matches_declared_across_rate_and_block_boundaries() {
        let rate_pairs = [
            (8_000, 192_000),
            (192_000, 8_000),
            (44_100, 48_000),
            (48_000, 44_100),
        ];
        let frame_counts = [1, CHUNK_FRAMES - 1, CHUNK_FRAMES, CHUNK_FRAMES + 1, 4_097];

        for (input_rate, output_rate) in rate_pairs {
            for frames in frame_counts {
                let input = (0..frames)
                    .map(|frame| 0.2 * (TAU * 997.0 * frame as f32 / input_rate as f32).sin())
                    .collect::<Vec<_>>();
                let mut declared = SampleRateConverter::new(
                    input_rate,
                    output_rate,
                    frames,
                    1,
                    ResampleQuality::Balanced,
                )
                .unwrap();
                let mut streaming = SampleRateConverter::new_streaming(
                    input_rate,
                    output_rate,
                    1,
                    ResampleQuality::Balanced,
                )
                .unwrap();
                let mut declared_output = Vec::new();
                let mut streaming_output = Vec::new();
                let mut start = 0;
                for chunk_frames in [1, 17, 509, 1_031] {
                    if start == frames {
                        break;
                    }
                    let end = (start + chunk_frames).min(frames);
                    let chunk = vec![input[start..end].to_vec()];
                    declared
                        .process(&chunk, |output| {
                            declared_output.extend_from_slice(&output[0]);
                            Ok(())
                        })
                        .unwrap();
                    streaming
                        .process(&chunk, |output| {
                            streaming_output.extend_from_slice(&output[0]);
                            Ok(())
                        })
                        .unwrap();
                    start = end;
                }
                if start < frames {
                    let chunk = vec![input[start..].to_vec()];
                    declared
                        .process(&chunk, |output| {
                            declared_output.extend_from_slice(&output[0]);
                            Ok(())
                        })
                        .unwrap();
                    streaming
                        .process(&chunk, |output| {
                            streaming_output.extend_from_slice(&output[0]);
                            Ok(())
                        })
                        .unwrap();
                }
                declared
                    .finish(|output| {
                        declared_output.extend_from_slice(&output[0]);
                        Ok(())
                    })
                    .unwrap();
                streaming
                    .finish(|output| {
                        streaming_output.extend_from_slice(&output[0]);
                        Ok(())
                    })
                    .unwrap();

                assert_eq!(
                    streaming_output, declared_output,
                    "streaming output differs for {frames} frames at {input_rate}->{output_rate} Hz"
                );
            }
        }
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
