//! EBU QC baseband checks over decoded PCM.

use crate::decoder;
use crate::normalize::Analysis;
use crate::wav::{AudioBuffer, ChannelRole, PcmKind};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;
use std::path::Path;

pub const QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ebu-qc-results-v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QcEvent {
    /// One-based channel number.
    pub channel: u16,
    pub start_seconds: f64,
    pub end_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QcResult {
    pub ebu_qc_id: String,
    pub version: String,
    pub name: String,
    pub layer: String,
    pub passed: bool,
    pub calculated: bool,
    pub events: Vec<QcEvent>,
}

#[derive(Debug, Clone)]
pub struct QcOptions {
    pub silence_threshold_dbfs: f64,
    pub silence_minimum_seconds: f64,
    pub clipping_minimum_samples: usize,
    pub tone_frequency_hz: f64,
    pub tone_threshold_dbfs: f64,
    pub tone_minimum_seconds: f64,
    pub expected_duration_seconds: Option<f64>,
    pub duration_tolerance_seconds: f64,
    pub expected_channel_count: Option<u16>,
    pub dropout_threshold_dbfs: f64,
    pub dropout_minimum_seconds: f64,
    pub dropout_maximum_seconds: f64,
    pub phase_correlation_threshold: f64,
    pub phase_window_seconds: f64,
    pub click_threshold: f64,
    pub minimum_average_level_dbfs: f64,
    pub hum_threshold_dbfs: f64,
    pub hum_minimum_seconds: f64,
}

impl Default for QcOptions {
    fn default() -> Self {
        Self {
            silence_threshold_dbfs: -60.0,
            silence_minimum_seconds: 1.0,
            clipping_minimum_samples: 3,
            tone_frequency_hz: 1_000.0,
            tone_threshold_dbfs: -30.0,
            tone_minimum_seconds: 0.5,
            expected_duration_seconds: None,
            duration_tolerance_seconds: 0.01,
            expected_channel_count: None,
            dropout_threshold_dbfs: -70.0,
            dropout_minimum_seconds: 0.002,
            dropout_maximum_seconds: 0.1,
            phase_correlation_threshold: -0.5,
            phase_window_seconds: 0.5,
            click_threshold: 0.5,
            minimum_average_level_dbfs: -50.0,
            hum_threshold_dbfs: -50.0,
            hum_minimum_seconds: 1.0,
        }
    }
}

impl QcOptions {
    pub fn validate(&self) -> Result<(), String> {
        let finite = [
            self.silence_threshold_dbfs,
            self.silence_minimum_seconds,
            self.tone_frequency_hz,
            self.tone_threshold_dbfs,
            self.tone_minimum_seconds,
            self.duration_tolerance_seconds,
            self.dropout_threshold_dbfs,
            self.dropout_minimum_seconds,
            self.dropout_maximum_seconds,
            self.phase_correlation_threshold,
            self.phase_window_seconds,
            self.click_threshold,
            self.minimum_average_level_dbfs,
            self.hum_threshold_dbfs,
            self.hum_minimum_seconds,
        ]
        .into_iter()
        .all(f64::is_finite);
        if !finite
            || self.silence_minimum_seconds <= 0.0
            || self.clipping_minimum_samples == 0
            || self.tone_frequency_hz <= 0.0
            || self.tone_minimum_seconds <= 0.0
            || self.duration_tolerance_seconds < 0.0
            || self.expected_channel_count == Some(0)
            || self.dropout_minimum_seconds <= 0.0
            || self.dropout_maximum_seconds < self.dropout_minimum_seconds
            || !(-1.0..=0.0).contains(&self.phase_correlation_threshold)
            || self.phase_window_seconds <= 0.0
            || !(0.0..=2.0).contains(&self.click_threshold)
            || self.hum_minimum_seconds <= 0.0
            || self
                .expected_duration_seconds
                .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err("invalid EBU QC threshold or duration".into());
        }
        Ok(())
    }
}

pub fn analyze_file(
    path: &Path,
    analysis: &Analysis,
    options: &QcOptions,
) -> Result<Vec<QcResult>, String> {
    options.validate()?;
    let audio = decoder::decode(path)?;
    Ok(analyze(&audio, analysis, options))
}

pub fn analyze(audio: &AudioBuffer, analysis: &Analysis, options: &QcOptions) -> Vec<QcResult> {
    let silence = silence_events(audio, options);
    let clipping = clipping_events(audio, options);
    let tones = tone_events(audio, options);
    let dropouts = dropout_events(audio, options);
    let phase_reversals = phase_reversal_events(audio, options);
    let clicks = click_events(audio, options);
    let low_average_levels = low_average_level_events(audio, options);
    let hum = hum_events(audio, options);
    let duration = audio.frames as f64 / audio.sample_rate as f64;
    let duration_events = options
        .expected_duration_seconds
        .filter(|expected| (duration - expected).abs() > options.duration_tolerance_seconds)
        .map(|_| {
            vec![QcEvent {
                channel: 0,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(duration),
                unit: Some("s".into()),
            }]
        })
        .unwrap_or_default();
    let channel_count = options.expected_channel_count;
    let channel_count_events = channel_count
        .filter(|expected| *expected != audio.channels)
        .map(|_| {
            vec![QcEvent {
                channel: 0,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(audio.channels as f64),
                unit: Some("channels".into()),
            }]
        })
        .unwrap_or_default();
    vec![
        result("0078B", "1.0", "Audio Silence", silence),
        result("0005B", "2.0", "Audio Digital Clipping", clipping),
        result("0014B", "2.0", "Audio Test Tones", tones),
        result("0009F", "2.0", "Audio Duration", duration_events),
        QcResult {
            ebu_qc_id: "0010B".into(),
            version: "2.0".into(),
            name: "Audio Programme Loudness".into(),
            layer: "baseband".into(),
            passed: true,
            calculated: true,
            events: vec![QcEvent {
                channel: 0,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(analysis.lufs),
                unit: Some("LUFS".into()),
            }],
        },
        QcResult {
            ebu_qc_id: "0084B".into(),
            version: "1.0".into(),
            name: "Audio Peaks TP".into(),
            layer: "baseband".into(),
            passed: true,
            calculated: true,
            events: vec![QcEvent {
                channel: 0,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(analysis.true_peak_db()),
                unit: Some("dBTP".into()),
            }],
        },
        QcResult {
            ebu_qc_id: "0004F".into(),
            version: "2.0".into(),
            name: "Audio Channel Count".into(),
            layer: "bitstream".into(),
            passed: channel_count_events.is_empty(),
            calculated: channel_count.is_some(),
            events: channel_count_events,
        },
        result("0008B", "2.0", "Audio Dropouts", dropouts),
        result("0012B", "2.0", "Audio Phase Reversal", phase_reversals),
        result("0057B", "1.0", "Audio Clicks", clicks),
        result(
            "0077B",
            "1.0",
            "Average Minimum Audio Level",
            low_average_levels,
        ),
        result("0088B", "1.0", "Audio Hum & Buzz", hum),
    ]
}

fn result(
    id: &'static str,
    version: &'static str,
    name: &'static str,
    events: Vec<QcEvent>,
) -> QcResult {
    QcResult {
        ebu_qc_id: id.into(),
        version: version.into(),
        name: name.into(),
        layer: "baseband".into(),
        passed: events.is_empty(),
        calculated: true,
        events,
    }
}

fn silence_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let threshold = 10.0_f32.powf((options.silence_threshold_dbfs / 20.0) as f32);
    let minimum = seconds_to_frames(audio, options.silence_minimum_seconds);
    run_events(
        audio,
        minimum,
        |sample| sample.abs() <= threshold,
        |slice| {
            let mean_square = slice
                .iter()
                .map(|sample| (*sample as f64).powi(2))
                .sum::<f64>()
                / slice.len().max(1) as f64;
            Some(10.0 * mean_square.max(1e-30).log10())
        },
        Some("dBFS"),
    )
}

fn clipping_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let half_lsb = match audio.source_kind {
        PcmKind::U8 => 0.5 / 127.0,
        PcmKind::S16 => 0.5 / 32_767.0,
        PcmKind::S24 => 0.5 / 8_388_607.0,
        PcmKind::S32 => 0.5 / 2_147_483_647.0,
        PcmKind::F32 | PcmKind::F64 => 0.0,
    } as f32;
    run_events(
        audio,
        options.clipping_minimum_samples,
        move |sample| sample >= 1.0 - half_lsb || sample <= -1.0,
        |slice| Some(slice.len() as f64),
        Some("samples"),
    )
}

fn tone_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let window = (audio.sample_rate as usize / 10).max(32);
    let minimum_windows =
        (options.tone_minimum_seconds * audio.sample_rate as f64 / window as f64).ceil() as usize;
    let context = ToneRunContext {
        window,
        minimum_windows,
        sample_rate: audio.sample_rate,
        frequency: options.tone_frequency_hz,
    };
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut start = None;
        let windows = samples.len() / window;
        for index in 0..windows {
            let slice = &samples[index * window..(index + 1) * window];
            let is_tone = dominant_tone(
                slice,
                audio.sample_rate,
                options.tone_frequency_hz,
                options.tone_threshold_dbfs,
            );
            match (start, is_tone) {
                (None, true) => start = Some(index),
                (Some(first), false) => {
                    push_tone_event(&mut events, channel, first, index, &context);
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            push_tone_event(&mut events, channel, first, windows, &context);
        }
    }
    events
}

fn dominant_tone(samples: &[f32], sample_rate: u32, frequency: f64, threshold_dbfs: f64) -> bool {
    let mut cosine = 0.0;
    let mut sine = 0.0;
    let mut energy = 0.0;
    for (index, &sample) in samples.iter().enumerate() {
        let phase = TAU * frequency * index as f64 / sample_rate as f64;
        cosine += sample as f64 * phase.cos();
        sine += sample as f64 * phase.sin();
        energy += (sample as f64).powi(2);
    }
    if energy <= 0.0 {
        return false;
    }
    let amplitude = 2.0 * cosine.hypot(sine) / samples.len() as f64;
    let level = 20.0 * amplitude.max(1e-30).log10();
    let fitted_energy = samples.len() as f64 * amplitude.powi(2) / 2.0;
    level >= threshold_dbfs && fitted_energy / energy >= 0.8
}

struct ToneRunContext {
    window: usize,
    minimum_windows: usize,
    sample_rate: u32,
    frequency: f64,
}

fn push_tone_event(
    events: &mut Vec<QcEvent>,
    channel: usize,
    first: usize,
    end: usize,
    context: &ToneRunContext,
) {
    if end.saturating_sub(first) >= context.minimum_windows {
        events.push(QcEvent {
            channel: channel as u16 + 1,
            start_seconds: (first * context.window) as f64 / context.sample_rate as f64,
            end_seconds: (end * context.window) as f64 / context.sample_rate as f64,
            measured: Some(context.frequency),
            unit: Some("Hz".into()),
        });
    }
}

fn dropout_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let threshold = 10.0_f32.powf((options.dropout_threshold_dbfs / 20.0) as f32);
    let minimum = seconds_to_frames(audio, options.dropout_minimum_seconds);
    let maximum = seconds_to_frames(audio, options.dropout_maximum_seconds);
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut start = None;
        for (index, &sample) in samples.iter().enumerate() {
            if sample.abs() <= threshold {
                start.get_or_insert(index);
            } else if let Some(first) = start.take() {
                push_dropout_event(
                    &mut events,
                    channel,
                    first,
                    index,
                    samples.len(),
                    audio.sample_rate,
                    minimum,
                    maximum,
                );
            }
        }
    }
    events
}

#[allow(clippy::too_many_arguments)]
fn push_dropout_event(
    events: &mut Vec<QcEvent>,
    channel: usize,
    first: usize,
    end: usize,
    sample_count: usize,
    sample_rate: u32,
    minimum: usize,
    maximum: usize,
) {
    let length = end.saturating_sub(first);
    if first > 0 && end < sample_count && length >= minimum && length <= maximum {
        events.push(QcEvent {
            channel: channel as u16 + 1,
            start_seconds: first as f64 / sample_rate as f64,
            end_seconds: end as f64 / sample_rate as f64,
            measured: Some(length as f64 / sample_rate as f64),
            unit: Some("s".into()),
        });
    }
}

fn phase_reversal_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let window = seconds_to_frames(audio, options.phase_window_seconds).max(1);
    let activity = 10.0_f64.powf(options.silence_threshold_dbfs / 20.0);
    let mut events = Vec::new();
    for (first_channel, second_channel) in stereo_pairs(audio) {
        let left = &audio.data[first_channel];
        let right = &audio.data[second_channel];
        let windows = left.len().min(right.len()) / window;
        let mut start = None;
        let mut minimum_correlation = 1.0_f64;
        for index in 0..windows {
            let range = index * window..(index + 1) * window;
            let correlation = correlation(&left[range.clone()], &right[range]);
            let active = rms(&left[index * window..(index + 1) * window]) >= activity
                || rms(&right[index * window..(index + 1) * window]) >= activity;
            let reversed = active && correlation <= options.phase_correlation_threshold;
            match (start, reversed) {
                (None, true) => {
                    start = Some(index);
                    minimum_correlation = correlation;
                }
                (Some(_), true) => minimum_correlation = minimum_correlation.min(correlation),
                (Some(first), false) => {
                    push_window_event(
                        &mut events,
                        first_channel,
                        first,
                        index,
                        window,
                        audio.sample_rate,
                        minimum_correlation,
                        "correlation",
                    );
                    start = None;
                    minimum_correlation = 1.0;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            push_window_event(
                &mut events,
                first_channel,
                first,
                windows,
                window,
                audio.sample_rate,
                minimum_correlation,
                "correlation",
            );
        }
    }
    events
}

fn stereo_pairs(audio: &AudioBuffer) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    if audio.data.len() >= 2
        && audio.channel_roles.first() != Some(&ChannelRole::Lfe)
        && audio.channel_roles.get(1) != Some(&ChannelRole::Lfe)
    {
        pairs.push((0, 1));
    }
    let mut index = 2;
    while index < audio.channel_roles.len() {
        match audio.channel_roles[index] {
            ChannelRole::Surround
                if audio.channel_roles.get(index + 1) == Some(&ChannelRole::Surround) =>
            {
                pairs.push((index, index + 1));
                index += 2;
            }
            ChannelRole::Positioned {
                azimuth_degrees,
                elevation_degrees,
            } if azimuth_degrees < 0 => {
                if let Some(partner) = audio
                    .channel_roles
                    .iter()
                    .enumerate()
                    .skip(index + 1)
                    .find_map(|(candidate, role)| {
                        (*role
                            == ChannelRole::Positioned {
                                azimuth_degrees: -azimuth_degrees,
                                elevation_degrees,
                            })
                        .then_some(candidate)
                    })
                {
                    pairs.push((index, partner));
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    pairs
}

fn correlation(left: &[f32], right: &[f32]) -> f64 {
    let mut product = 0.0;
    let mut left_energy = 0.0;
    let mut right_energy = 0.0;
    for (&left, &right) in left.iter().zip(right) {
        let left = left as f64;
        let right = right as f64;
        product += left * right;
        left_energy += left * left;
        right_energy += right * right;
    }
    let denominator = (left_energy * right_energy).sqrt();
    if denominator > 0.0 {
        product / denominator
    } else {
        0.0
    }
}

fn click_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let mut events = Vec::new();
    let coalesce = (audio.sample_rate as usize / 1_000).max(1);
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut last = None;
        for index in 1..samples.len().saturating_sub(1) {
            let previous = samples[index - 1] as f64;
            let current = samples[index] as f64;
            let next = samples[index + 1] as f64;
            let residual = (current - (previous + next) * 0.5).abs();
            let impulse = (current - previous) * (next - current) < 0.0;
            if residual >= options.click_threshold && impulse {
                if last.is_some_and(|previous_index| index - previous_index <= coalesce) {
                    continue;
                }
                events.push(QcEvent {
                    channel: channel as u16 + 1,
                    start_seconds: index as f64 / audio.sample_rate as f64,
                    end_seconds: (index + 1) as f64 / audio.sample_rate as f64,
                    measured: Some(residual),
                    unit: Some("FS".into()),
                });
                last = Some(index);
            }
        }
    }
    events
}

fn low_average_level_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let duration = audio.frames as f64 / audio.sample_rate as f64;
    audio
        .data
        .iter()
        .enumerate()
        .filter_map(|(channel, samples)| {
            let level = 20.0 * rms(samples).max(1e-30).log10();
            (level < options.minimum_average_level_dbfs).then(|| QcEvent {
                channel: channel as u16 + 1,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(level),
                unit: Some("dBFS".into()),
            })
        })
        .collect()
}

fn rms(samples: &[f32]) -> f64 {
    let mean_square = samples
        .iter()
        .map(|sample| (*sample as f64).powi(2))
        .sum::<f64>()
        / samples.len().max(1) as f64;
    mean_square.sqrt()
}

fn hum_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let window = audio.sample_rate as usize;
    let minimum_windows = (options.hum_minimum_seconds).ceil() as usize;
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let windows = samples.len() / window;
        let mut start = None;
        let mut strongest_frequency = 0.0;
        for index in 0..windows {
            let slice = &samples[index * window..(index + 1) * window];
            let (frequency, level, ratio) = [50.0, 60.0]
                .into_iter()
                .map(|frequency| {
                    let (level, ratio) = harmonic_fit(slice, audio.sample_rate, frequency);
                    (frequency, level, ratio)
                })
                .max_by(|left, right| left.2.total_cmp(&right.2))
                .unwrap_or((0.0, f64::NEG_INFINITY, 0.0));
            let detected = level >= options.hum_threshold_dbfs && ratio >= 0.5;
            match (start, detected) {
                (None, true) => {
                    start = Some(index);
                    strongest_frequency = frequency;
                }
                (Some(_), true) => strongest_frequency = frequency,
                (Some(first), false) => {
                    if index - first >= minimum_windows {
                        push_window_event(
                            &mut events,
                            channel,
                            first,
                            index,
                            window,
                            audio.sample_rate,
                            strongest_frequency,
                            "Hz",
                        );
                    }
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            if windows - first >= minimum_windows {
                push_window_event(
                    &mut events,
                    channel,
                    first,
                    windows,
                    window,
                    audio.sample_rate,
                    strongest_frequency,
                    "Hz",
                );
            }
        }
    }
    events
}

fn harmonic_fit(samples: &[f32], sample_rate: u32, fundamental: f64) -> (f64, f64) {
    let total_energy = samples
        .iter()
        .map(|sample| (*sample as f64).powi(2))
        .sum::<f64>();
    if total_energy <= 0.0 {
        return (f64::NEG_INFINITY, 0.0);
    }
    let mut fitted_energy = 0.0;
    let mut amplitudes = Vec::with_capacity(4);
    for harmonic in 1..=4 {
        let frequency = fundamental * harmonic as f64;
        let amplitude = goertzel_amplitude(samples, sample_rate, frequency);
        fitted_energy += samples.len() as f64 * amplitude.powi(2) / 2.0;
        amplitudes.push(amplitude);
    }
    let combined_amplitude = amplitudes
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    (
        20.0 * combined_amplitude.max(1e-30).log10(),
        (fitted_energy / total_energy).min(1.0),
    )
}

fn goertzel_amplitude(samples: &[f32], sample_rate: u32, frequency: f64) -> f64 {
    let omega = TAU * frequency / sample_rate as f64;
    let coefficient = 2.0 * omega.cos();
    let mut previous = 0.0;
    let mut previous_two = 0.0;
    for &sample in samples {
        let current = sample as f64 + coefficient * previous - previous_two;
        previous_two = previous;
        previous = current;
    }
    let power =
        previous * previous + previous_two * previous_two - coefficient * previous * previous_two;
    2.0 * power.max(0.0).sqrt() / samples.len().max(1) as f64
}

#[allow(clippy::too_many_arguments)]
fn push_window_event(
    events: &mut Vec<QcEvent>,
    channel: usize,
    first: usize,
    end: usize,
    window: usize,
    sample_rate: u32,
    measured: f64,
    unit: &'static str,
) {
    events.push(QcEvent {
        channel: channel as u16 + 1,
        start_seconds: (first * window) as f64 / sample_rate as f64,
        end_seconds: (end * window) as f64 / sample_rate as f64,
        measured: Some(measured),
        unit: Some(unit.into()),
    });
}

fn run_events<F, M>(
    audio: &AudioBuffer,
    minimum: usize,
    predicate: F,
    measure: M,
    unit: Option<&'static str>,
) -> Vec<QcEvent>
where
    F: Fn(f32) -> bool,
    M: Fn(&[f32]) -> Option<f64>,
{
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut start = None;
        for (index, &sample) in samples.iter().enumerate() {
            if predicate(sample) {
                start.get_or_insert(index);
            } else if let Some(first) = start.take() {
                if index - first >= minimum {
                    events.push(QcEvent {
                        channel: channel as u16 + 1,
                        start_seconds: first as f64 / audio.sample_rate as f64,
                        end_seconds: index as f64 / audio.sample_rate as f64,
                        measured: measure(&samples[first..index]),
                        unit: unit.map(str::to_owned),
                    });
                }
            }
        }
        if let Some(first) = start {
            if samples.len() - first >= minimum {
                events.push(QcEvent {
                    channel: channel as u16 + 1,
                    start_seconds: first as f64 / audio.sample_rate as f64,
                    end_seconds: samples.len() as f64 / audio.sample_rate as f64,
                    measured: measure(&samples[first..]),
                    unit: unit.map(str::to_owned),
                });
            }
        }
    }
    events
}

fn seconds_to_frames(audio: &AudioBuffer, seconds: f64) -> usize {
    (seconds * audio.sample_rate as f64).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize;
    use crate::wav::default_channel_roles;

    fn buffer(samples: Vec<f32>) -> AudioBuffer {
        AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: samples.len(),
            data: vec![samples],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::S16,
        }
    }

    fn stereo_buffer(left: Vec<f32>, right: Vec<f32>) -> AudioBuffer {
        assert_eq!(left.len(), right.len());
        AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: left.len(),
            data: vec![left, right],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::S16,
        }
    }

    #[test]
    fn detects_silence_clipping_and_tone_with_channel_ranges() {
        let mut samples = vec![0.0; 48_000];
        samples.extend(vec![1.0; 4]);
        samples.extend(
            (0..48_000).map(|index| 0.2 * (TAU * 1_000.0 * index as f64 / 48_000.0).sin() as f32),
        );
        let audio = buffer(samples);
        let analysis = normalize::analyze(&audio);
        let results = analyze(&audio, &analysis, &QcOptions::default());
        assert!(!results[0].passed);
        assert_eq!(results[0].events[0].channel, 1);
        assert!(!results[1].passed);
        assert!(!results[2].passed);
        assert_eq!(results[4].ebu_qc_id, "0010B");
        assert_eq!(results[5].ebu_qc_id, "0084B");
    }

    #[test]
    fn detects_channel_count_dropouts_phase_clicks_level_and_hum() {
        let mut left = (0..96_000)
            .map(|index| 0.1 * (TAU * 50.0 * index as f64 / 48_000.0).sin() as f32)
            .collect::<Vec<_>>();
        let mut right = left.iter().map(|sample| -*sample).collect::<Vec<_>>();
        left[24_000..24_480].fill(0.0);
        right[24_000..24_480].fill(0.0);
        left[72_000] = 1.0;
        right[72_000] = -1.0;
        let audio = stereo_buffer(left, right);
        let analysis = normalize::analyze(&audio);
        let options = QcOptions {
            expected_channel_count: Some(1),
            ..QcOptions::default()
        };
        let results = analyze(&audio, &analysis, &options);

        assert_eq!(results.len(), 12);
        assert_eq!(results[6].ebu_qc_id, "0004F");
        assert!(!results[6].passed);
        assert_eq!(results[6].events[0].measured, Some(2.0));
        assert_eq!(results[7].ebu_qc_id, "0008B");
        assert!(!results[7].passed);
        assert_eq!(results[8].ebu_qc_id, "0012B");
        assert!(!results[8].passed);
        assert_eq!(results[9].ebu_qc_id, "0057B");
        assert!(!results[9].passed);
        assert_eq!(results[10].ebu_qc_id, "0077B");
        assert!(results[10].passed);
        assert_eq!(results[11].ebu_qc_id, "0088B");
        assert!(!results[11].passed);
    }

    #[test]
    fn reports_low_average_level_and_skips_unconfigured_channel_count() {
        let audio = buffer(vec![0.0; 48_000]);
        let analysis = normalize::analyze(&audio);
        let results = analyze(&audio, &analysis, &QcOptions::default());
        assert!(results[6].passed);
        assert!(!results[6].calculated);
        assert!(!results[10].passed);
        assert_eq!(results[10].events[0].channel, 1);
    }

    #[test]
    fn phase_detection_pairs_fronts_and_surrounds_but_not_center_and_lfe() {
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames: 1,
            data: vec![vec![0.0]; 6],
            channel_roles: default_channel_roles(6),
            source_kind: PcmKind::S16,
        };
        assert_eq!(stereo_pairs(&audio), vec![(0, 1), (4, 5)]);
    }
}
