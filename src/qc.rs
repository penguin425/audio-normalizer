//! EBU QC baseband checks over decoded PCM.

use crate::decoder;
use crate::normalize::Analysis;
use crate::wav::{AudioBuffer, PcmKind};
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
        ]
        .into_iter()
        .all(f64::is_finite);
        if !finite
            || self.silence_minimum_seconds <= 0.0
            || self.clipping_minimum_samples == 0
            || self.tone_frequency_hz <= 0.0
            || self.tone_minimum_seconds <= 0.0
            || self.duration_tolerance_seconds < 0.0
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
    vec![
        result("0078B", "3.0", "Audio Silence", silence),
        result("0005B", "2.0.0", "Audio Digital Clipping", clipping),
        result("0014B", "1.0", "Audio Test Tones", tones),
        result("0009F", "2.0.0", "Audio Duration", duration_events),
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
}
