//! Bounded-memory, delay-compensated sample-rate conversion.

use crate::wav::AudioBuffer;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
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

    fn parameters(self) -> SincInterpolationParameters {
        match self {
            Self::Fast => SincInterpolationParameters {
                sinc_len: 64,
                f_cutoff: 0.90,
                oversampling_factor: 64,
                interpolation: SincInterpolationType::Linear,
                window: WindowFunction::Hann2,
            },
            Self::Balanced => SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                oversampling_factor: 128,
                interpolation: SincInterpolationType::Cubic,
                window: WindowFunction::BlackmanHarris,
            },
            Self::Best => SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: 0.96,
                oversampling_factor: 256,
                interpolation: SincInterpolationType::Cubic,
                window: WindowFunction::BlackmanHarris2,
            },
        }
    }
}

pub struct SampleRateConverter {
    resampler: SincFixedIn<f32>,
    channels: usize,
    input_pending: Vec<Vec<f32>>,
    expected_output_frames: usize,
    emitted_frames: usize,
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
        let resampler = SincFixedIn::new(
            output_rate as f64 / input_rate as f64,
            1.0,
            quality.parameters(),
            CHUNK_FRAMES,
            channels,
        )
        .map_err(|error| format!("create sample-rate converter: {error}"))?;
        Ok(Self {
            resampler,
            channels,
            input_pending: vec![Vec::new(); channels],
            expected_output_frames,
            emitted_frames: 0,
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
        while self.input_pending[0].len() >= CHUNK_FRAMES {
            let block = self
                .input_pending
                .iter_mut()
                .map(|channel| channel.drain(..CHUNK_FRAMES).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            let output = self
                .resampler
                .process(&block, None)
                .map_err(|error| format!("resample audio: {error}"))?;
            self.emit(output, &mut consume)?;
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        mut consume: impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        for pass in 0..8 {
            let input: Option<&[Vec<f32>]> = (pass == 0 && !self.input_pending[0].is_empty())
                .then_some(self.input_pending.as_slice());
            let before = self.emitted_frames;
            let output = self
                .resampler
                .process_partial(input, None)
                .map_err(|error| format!("finish sample-rate conversion: {error}"))?;
            self.emit(output, &mut consume)?;
            if self.emitted_frames == self.expected_output_frames {
                break;
            }
            if self.emitted_frames == before && pass > 0 {
                break;
            }
        }
        if self.emitted_frames != self.expected_output_frames {
            return Err(format!(
                "sample-rate converter produced {} frames, expected {}",
                self.emitted_frames, self.expected_output_frames
            ));
        }
        Ok(())
    }

    fn emit(
        &mut self,
        output: Vec<Vec<f32>>,
        consume: &mut impl FnMut(&mut [Vec<f32>]) -> Result<(), String>,
    ) -> Result<(), String> {
        let frames = output.first().map_or(0, Vec::len);
        // SincFixedIn's samples are already on the source time axis. Its
        // output_delay() is processing latency, not leading padding to discard.
        let start = 0_usize;
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
}
