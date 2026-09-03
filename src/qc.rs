//! EBU QC baseband checks over decoded PCM.

use crate::decoder;
use crate::normalize::Analysis;
use crate::wav::{AudioBuffer, ChannelRole, PcmKind};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;
use std::path::Path;

pub const QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ebu-qc-results-v2";
pub const EBU_QC_CATALOGUE: &str = "https://qc.ebu.io/items";
pub const FORGE_QC_SOURCE: &str =
    "https://github.com/penguin425/audio-normalizer/blob/main/ROADMAP.md";
const MAX_QC_EVENTS: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QcEvent {
    /// One-based channel number.
    pub channel: u16,
    /// One-based related channel for pair-wise checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_channel: Option<u16>,
    pub start_seconds: f64,
    pub end_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QcResult {
    /// Stable rule identifier. EBU rules use their published Item identifier.
    pub rule_id: String,
    /// Backwards-compatible alias retained from the v1 envelope.
    pub ebu_qc_id: String,
    pub version: String,
    pub name: String,
    pub layer: String,
    pub passed: bool,
    pub calculated: bool,
    pub source_url: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub events_truncated: bool,
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
    pub noise_threshold_dbfs: f64,
    pub noise_gate_dbfs: f64,
    pub noise_minimum_seconds: f64,
    pub noise_low_hz: f64,
    pub noise_high_hz: f64,
    pub crosstalk_coherence_threshold: f64,
    pub crosstalk_level_delta_db: f64,
    pub crosstalk_minimum_seconds: f64,
    pub panning_imbalance_db: f64,
    pub panning_minimum_seconds: f64,
    pub lfe_cutoff_hz: f64,
    pub lfe_out_of_band_ratio: f64,
    pub expect_mono: bool,
    pub mono_difference_threshold: f64,
    pub dc_offset_threshold_dbfs: f64,
    pub interchannel_delay_samples: usize,
    pub stuck_sample_seconds: f64,
    pub discontinuity_threshold: f64,
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
            noise_threshold_dbfs: -60.0,
            noise_gate_dbfs: -35.0,
            noise_minimum_seconds: 1.0,
            noise_low_hz: 200.0,
            noise_high_hz: 15_000.0,
            crosstalk_coherence_threshold: 0.95,
            crosstalk_level_delta_db: 18.0,
            crosstalk_minimum_seconds: 1.0,
            panning_imbalance_db: 18.0,
            panning_minimum_seconds: 2.0,
            lfe_cutoff_hz: 120.0,
            lfe_out_of_band_ratio: 0.25,
            expect_mono: false,
            mono_difference_threshold: 1.0 / 32_768.0,
            dc_offset_threshold_dbfs: -40.0,
            interchannel_delay_samples: 1,
            stuck_sample_seconds: 0.05,
            discontinuity_threshold: 0.75,
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
            self.noise_threshold_dbfs,
            self.noise_gate_dbfs,
            self.noise_minimum_seconds,
            self.noise_low_hz,
            self.noise_high_hz,
            self.crosstalk_coherence_threshold,
            self.crosstalk_level_delta_db,
            self.crosstalk_minimum_seconds,
            self.panning_imbalance_db,
            self.panning_minimum_seconds,
            self.lfe_cutoff_hz,
            self.lfe_out_of_band_ratio,
            self.mono_difference_threshold,
            self.dc_offset_threshold_dbfs,
            self.stuck_sample_seconds,
            self.discontinuity_threshold,
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
            || self.noise_minimum_seconds <= 0.0
            || self.noise_low_hz <= 0.0
            || self.noise_high_hz <= self.noise_low_hz
            || self.noise_high_hz >= 24_000.0
            || !(0.0..=1.0).contains(&self.crosstalk_coherence_threshold)
            || self.crosstalk_level_delta_db < 0.0
            || self.crosstalk_minimum_seconds <= 0.0
            || self.panning_imbalance_db < 0.0
            || self.panning_minimum_seconds <= 0.0
            || self.lfe_cutoff_hz <= 0.0
            || !(0.0..=1.0).contains(&self.lfe_out_of_band_ratio)
            || self.mono_difference_threshold < 0.0
            || self.interchannel_delay_samples > 64
            || self.stuck_sample_seconds <= 0.0
            || !(0.0..=2.0).contains(&self.discontinuity_threshold)
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
    let (mut audio, layout_provenance) = decoder::decode_with_layout(path)?;
    audio.channel_roles = crate::normalize::resolve_decoded_channel_roles(
        path,
        audio.channels,
        &audio.channel_roles,
        layout_provenance,
        Some(&analysis.channel_roles),
    )?;
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
    let noise = noise_events(audio, options);
    let crosstalk = crosstalk_events(audio, options);
    let panning = panning_events(audio, options);
    let lfe_centre = lfe_centre_events(audio, options);
    let mono = mono_events(audio, options);
    let dc_offset = dc_offset_events(audio, options);
    let interchannel_delay = interchannel_delay_events(audio, options);
    let stuck_samples = stuck_sample_events(audio, options);
    let discontinuities = discontinuity_events(audio, options);
    let duration = audio.frames as f64 / audio.sample_rate as f64;
    let duration_events = options
        .expected_duration_seconds
        .filter(|expected| (duration - expected).abs() > options.duration_tolerance_seconds)
        .map(|_| {
            vec![QcEvent {
                channel: 0,
                related_channel: None,
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
                related_channel: None,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(audio.channels as f64),
                unit: Some("channels".into()),
            }]
        })
        .unwrap_or_default();
    vec![
        result("0078B", "3.0", "Audio Silence", silence),
        result("0005B", "2.0", "Audio Digital Clipping", clipping),
        result("0014B", "2.0", "Audio Test Tones", tones),
        result("0009F", "2.0", "Audio Duration", duration_events),
        QcResult {
            rule_id: "0010B".into(),
            ebu_qc_id: "0010B".into(),
            version: "2.0".into(),
            name: "Loudness".into(),
            layer: "baseband".into(),
            passed: true,
            calculated: true,
            source_url: ebu_source("0010B"),
            method: method_for("0010B").into(),
            events_truncated: false,
            events: vec![QcEvent {
                channel: 0,
                related_channel: None,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(analysis.lufs),
                unit: Some("LUFS".into()),
            }],
        },
        QcResult {
            rule_id: "0084B".into(),
            ebu_qc_id: "0084B".into(),
            version: "1.0".into(),
            name: "Audio Peaks (TP)".into(),
            layer: "baseband".into(),
            passed: true,
            calculated: true,
            source_url: ebu_source("0084B"),
            method: method_for("0084B").into(),
            events_truncated: false,
            events: vec![QcEvent {
                channel: 0,
                related_channel: None,
                start_seconds: 0.0,
                end_seconds: duration,
                measured: Some(analysis.true_peak_db()),
                unit: Some("dBTP".into()),
            }],
        },
        QcResult {
            rule_id: "0004F".into(),
            ebu_qc_id: "0004F".into(),
            version: "2.0".into(),
            name: "Audio Channel Count".into(),
            layer: "bitstream".into(),
            passed: channel_count_events.is_empty(),
            calculated: channel_count.is_some(),
            source_url: ebu_source("0004F"),
            method: method_for("0004F").into(),
            events_truncated: false,
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
        result("0086B", "1.0", "Audio Noise", noise),
        result("0170B", "1.0", "Cross Talk", crosstalk),
        result("0230B", "1.0", "Audio Channel Panning", panning),
        result("0095B", "1.0", "LFE/Centre Channel Assignment", lfe_centre),
        conditional_result("0124B", "2.0", "Mono Audio", options.expect_mono, mono),
        forge_result("FORGE-DC-OFFSET", "DC Offset", dc_offset),
        forge_result(
            "FORGE-INTERCHANNEL-DELAY",
            "Inter-channel Sample Delay",
            interchannel_delay,
        ),
        forge_result("FORGE-STUCK-SAMPLES", "Stuck Samples", stuck_samples),
        forge_result(
            "FORGE-DISCONTINUITY",
            "Sample Discontinuity",
            discontinuities,
        ),
    ]
}

fn result(
    id: &'static str,
    version: &'static str,
    name: &'static str,
    events: Vec<QcEvent>,
) -> QcResult {
    conditional_result(id, version, name, true, events)
}

fn conditional_result(
    id: &'static str,
    version: &'static str,
    name: &'static str,
    calculated: bool,
    mut events: Vec<QcEvent>,
) -> QcResult {
    let events_truncated = events.len() > MAX_QC_EVENTS;
    events.truncate(MAX_QC_EVENTS);
    QcResult {
        rule_id: id.into(),
        ebu_qc_id: id.into(),
        version: version.into(),
        name: name.into(),
        layer: "baseband".into(),
        passed: events.is_empty(),
        calculated,
        source_url: ebu_source(id),
        method: method_for(id).into(),
        events_truncated,
        events,
    }
}

fn forge_result(id: &'static str, name: &'static str, mut events: Vec<QcEvent>) -> QcResult {
    let events_truncated = events.len() > MAX_QC_EVENTS;
    events.truncate(MAX_QC_EVENTS);
    QcResult {
        rule_id: id.into(),
        ebu_qc_id: id.into(),
        version: "1.0".into(),
        name: name.into(),
        layer: "baseband".into(),
        passed: events.is_empty(),
        calculated: true,
        source_url: FORGE_QC_SOURCE.into(),
        method: method_for(id).into(),
        events_truncated,
        events,
    }
}

fn ebu_source(id: &str) -> String {
    format!("{EBU_QC_CATALOGUE}/{id}/")
}

fn method_for(id: &str) -> &'static str {
    match id {
        "0086B" => {
            "250 ms gated band-limited RMS with adjacent-sample decorrelation noise criterion"
        }
        "0170B" => "250 ms pair-wise correlation multiplied by eight-band spectral similarity",
        "0230B" => "250 ms declared stereo-pair RMS imbalance with duration coalescing",
        "0095B" => "whole-programme LFE energy ratio above the configured low-pass cutoff",
        "0124B" => "channel-count and maximum sample difference for configured mono delivery",
        "FORGE-DC-OFFSET" => "whole-programme arithmetic mean per channel",
        "FORGE-INTERCHANNEL-DELAY" => {
            "bounded-lag correlation over 2048-sample excerpts at one-second intervals"
        }
        "FORGE-STUCK-SAMPLES" => "bit-identical active sample runs with duration threshold",
        "FORGE-DISCONTINUITY" => "coalesced adjacent-sample absolute difference",
        "0010B" => "ITU-R BS.1770-5 integrated programme loudness",
        "0084B" => "ITU-R BS.1770-5 four-times oversampled true peak",
        "0004F" => "decoded channel count compared with configured expectation",
        _ => "deterministic decoded-PCM analysis with configured threshold",
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
        push_bounded(
            events,
            QcEvent {
                channel: channel as u16 + 1,
                related_channel: None,
                start_seconds: (first * context.window) as f64 / context.sample_rate as f64,
                end_seconds: (end * context.window) as f64 / context.sample_rate as f64,
                measured: Some(context.frequency),
                unit: Some("Hz".into()),
            },
        );
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
        push_bounded(
            events,
            QcEvent {
                channel: channel as u16 + 1,
                related_channel: None,
                start_seconds: first as f64 / sample_rate as f64,
                end_seconds: end as f64 / sample_rate as f64,
                measured: Some(length as f64 / sample_rate as f64),
                unit: Some("s".into()),
            },
        );
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
                push_bounded(
                    &mut events,
                    QcEvent {
                        channel: channel as u16 + 1,
                        related_channel: None,
                        start_seconds: index as f64 / audio.sample_rate as f64,
                        end_seconds: (index + 1) as f64 / audio.sample_rate as f64,
                        measured: Some(residual),
                        unit: Some("FS".into()),
                    },
                );
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
                related_channel: None,
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

fn noise_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let window = (audio.sample_rate as usize / 4).max(1);
    let minimum_windows =
        (options.noise_minimum_seconds * audio.sample_rate as f64 / window as f64).ceil() as usize;
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut start = None;
        let mut maximum_level = f64::NEG_INFINITY;
        let windows = samples.len() / window;
        for index in 0..windows {
            let slice = &samples[index * window..(index + 1) * window];
            let programme_level = level_dbfs(slice);
            let noise_level = band_limited_level(
                slice,
                audio.sample_rate,
                options.noise_low_hz,
                options.noise_high_hz.min(audio.sample_rate as f64 * 0.49),
            );
            let detected = programme_level <= options.noise_gate_dbfs
                && noise_level >= options.noise_threshold_dbfs
                && noise_likeness(slice) >= 0.5;
            update_window_run(
                &mut events,
                &mut start,
                &mut maximum_level,
                detected,
                channel,
                None,
                index,
                windows,
                minimum_windows,
                window,
                audio.sample_rate,
                noise_level,
                &format!(
                    "dBFS {}Hz-{}Hz",
                    options.noise_low_hz, options.noise_high_hz
                ),
            );
        }
    }
    events
}

fn noise_likeness(samples: &[f32]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    1.0 - correlation(&samples[..samples.len() - 1], &samples[1..]).abs()
}

fn band_limited_level(samples: &[f32], sample_rate: u32, low_hz: f64, high_hz: f64) -> f64 {
    if samples.is_empty() || high_hz <= low_hz {
        return f64::NEG_INFINITY;
    }
    let dt = 1.0 / sample_rate as f64;
    let high_pass_rc = 1.0 / (TAU * low_hz);
    let high_pass_alpha = high_pass_rc / (high_pass_rc + dt);
    let low_pass_rc = 1.0 / (TAU * high_hz);
    let low_pass_alpha = dt / (low_pass_rc + dt);
    let mut previous_input = 0.0;
    let mut high_pass = 0.0;
    let mut low_pass = 0.0;
    let mut energy = 0.0;
    for &sample in samples {
        let input = sample as f64;
        high_pass = high_pass_alpha * (high_pass + input - previous_input);
        low_pass += low_pass_alpha * (high_pass - low_pass);
        previous_input = input;
        energy += low_pass * low_pass;
    }
    10.0 * (energy / samples.len() as f64).max(1e-30).log10()
}

fn crosstalk_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let window = (audio.sample_rate as usize / 4).max(1);
    let minimum_windows = (options.crosstalk_minimum_seconds * audio.sample_rate as f64
        / window as f64)
        .ceil() as usize;
    let mut events = Vec::new();
    for first in 0..audio.data.len() {
        if audio.channel_roles.get(first) == Some(&ChannelRole::Lfe) {
            continue;
        }
        for second in first + 1..audio.data.len() {
            if audio.channel_roles.get(second) == Some(&ChannelRole::Lfe) {
                continue;
            }
            let windows = audio.data[first].len().min(audio.data[second].len()) / window;
            let mut start = None;
            let mut maximum_coherence = 0.0;
            for index in 0..windows {
                let range = index * window..(index + 1) * window;
                let left = &audio.data[first][range.clone()];
                let right = &audio.data[second][range];
                let left_level = level_dbfs(left);
                let right_level = level_dbfs(right);
                let (victim, source, delta) = if left_level <= right_level {
                    (first, second, right_level - left_level)
                } else {
                    (second, first, left_level - right_level)
                };
                let eligible = left_level.max(right_level) >= options.noise_gate_dbfs
                    && left_level.min(right_level) >= options.noise_threshold_dbfs
                    && delta >= options.crosstalk_level_delta_db;
                let coherence = if eligible {
                    time_frequency_coherence(left, right, audio.sample_rate)
                } else {
                    0.0
                };
                let detected = eligible && coherence >= options.crosstalk_coherence_threshold;
                update_window_run(
                    &mut events,
                    &mut start,
                    &mut maximum_coherence,
                    detected,
                    victim,
                    Some(source),
                    index,
                    windows,
                    minimum_windows,
                    window,
                    audio.sample_rate,
                    coherence,
                    "coherence",
                );
            }
        }
    }
    events
}

fn time_frequency_coherence(left: &[f32], right: &[f32], sample_rate: u32) -> f64 {
    const FREQUENCIES: [f64; 8] = [
        250.0, 375.0, 500.0, 750.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0,
    ];
    let mut dot = 0.0;
    let mut left_energy = 0.0;
    let mut right_energy = 0.0;
    for frequency in FREQUENCIES {
        if frequency >= sample_rate as f64 * 0.49 {
            continue;
        }
        let left_amplitude = goertzel_amplitude(left, sample_rate, frequency);
        let right_amplitude = goertzel_amplitude(right, sample_rate, frequency);
        dot += left_amplitude * right_amplitude;
        left_energy += left_amplitude * left_amplitude;
        right_energy += right_amplitude * right_amplitude;
    }
    let spectral = dot / (left_energy * right_energy).sqrt().max(1e-30);
    correlation(left, right).abs() * spectral.clamp(0.0, 1.0)
}

fn panning_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let window = (audio.sample_rate as usize / 4).max(1);
    let minimum_windows = (options.panning_minimum_seconds * audio.sample_rate as f64
        / window as f64)
        .ceil() as usize;
    let mut events = Vec::new();
    for (left_channel, right_channel) in stereo_pairs(audio) {
        let windows = audio.data[left_channel]
            .len()
            .min(audio.data[right_channel].len())
            / window;
        let mut start = None;
        let mut maximum_imbalance = 0.0;
        for index in 0..windows {
            let range = index * window..(index + 1) * window;
            let left_level = level_dbfs(&audio.data[left_channel][range.clone()]);
            let right_level = level_dbfs(&audio.data[right_channel][range]);
            let imbalance = (left_level - right_level).abs();
            let (louder, quieter) = if left_level >= right_level {
                (left_channel, right_channel)
            } else {
                (right_channel, left_channel)
            };
            let detected = left_level.max(right_level) >= options.noise_gate_dbfs
                && imbalance >= options.panning_imbalance_db;
            update_window_run(
                &mut events,
                &mut start,
                &mut maximum_imbalance,
                detected,
                louder,
                Some(quieter),
                index,
                windows,
                minimum_windows,
                window,
                audio.sample_rate,
                imbalance,
                "dB imbalance",
            );
        }
    }
    events
}

fn lfe_centre_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let duration = audio.frames as f64 / audio.sample_rate as f64;
    let mut events = Vec::new();
    for (channel, role) in audio.channel_roles.iter().enumerate() {
        if *role != ChannelRole::Lfe {
            continue;
        }
        let samples = &audio.data[channel];
        let total = rms(samples).powi(2);
        if total <= 1e-30 {
            continue;
        }
        let low_level = band_limited_level(
            samples,
            audio.sample_rate,
            10.0,
            options.lfe_cutoff_hz.min(audio.sample_rate as f64 * 0.49),
        );
        let low_energy = 10.0_f64.powf(low_level / 10.0);
        let out_of_band_ratio = (1.0 - low_energy / total).clamp(0.0, 1.0);
        if out_of_band_ratio >= options.lfe_out_of_band_ratio {
            push_bounded(
                &mut events,
                QcEvent {
                    channel: channel as u16 + 1,
                    related_channel: centre_channel(audio).map(|index| index as u16 + 1),
                    start_seconds: 0.0,
                    end_seconds: duration,
                    measured: Some(out_of_band_ratio),
                    unit: Some(format!("ratio above {} Hz", options.lfe_cutoff_hz)),
                },
            );
        }
    }
    events
}

fn centre_channel(audio: &AudioBuffer) -> Option<usize> {
    (audio.data.len() >= 5
        && audio.channel_roles.get(2) == Some(&ChannelRole::Main)
        && audio.channel_roles.contains(&ChannelRole::Lfe))
    .then_some(2)
}

fn mono_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    if !options.expect_mono || audio.channels == 1 {
        return Vec::new();
    }
    let duration = audio.frames as f64 / audio.sample_rate as f64;
    let mut events = Vec::new();
    let (measured, related_channel, unit) = if audio.channels == 2 {
        let maximum_difference = audio.data[0]
            .iter()
            .zip(&audio.data[1])
            .map(|(&left, &right)| (left as f64 - right as f64).abs())
            .fold(0.0_f64, f64::max);
        if maximum_difference <= options.mono_difference_threshold {
            return events;
        }
        (maximum_difference, Some(2), "max FS difference")
    } else {
        (audio.channels as f64, None, "channels")
    };
    push_bounded(
        &mut events,
        QcEvent {
            channel: 1,
            related_channel,
            start_seconds: 0.0,
            end_seconds: duration,
            measured: Some(measured),
            unit: Some(unit.into()),
        },
    );
    events
}

fn dc_offset_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let duration = audio.frames as f64 / audio.sample_rate as f64;
    let threshold = 10.0_f64.powf(options.dc_offset_threshold_dbfs / 20.0);
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let mean =
            samples.iter().map(|sample| *sample as f64).sum::<f64>() / samples.len().max(1) as f64;
        if mean.abs() >= threshold {
            push_bounded(
                &mut events,
                QcEvent {
                    channel: channel as u16 + 1,
                    related_channel: None,
                    start_seconds: 0.0,
                    end_seconds: duration,
                    measured: Some(20.0 * mean.abs().max(1e-30).log10()),
                    unit: Some("dBFS mean".into()),
                },
            );
        }
    }
    events
}

fn interchannel_delay_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let mut events = Vec::new();
    let step = audio.sample_rate as usize;
    let comparison_length = 2_048;
    let search = (options.interchannel_delay_samples + 8).min(64);
    for (first, second) in stereo_pairs(audio) {
        let length = audio.data[first].len().min(audio.data[second].len());
        for start in (0..length.saturating_sub(comparison_length + search)).step_by(step.max(1)) {
            let left = &audio.data[first][start..start + comparison_length + search];
            let right = &audio.data[second][start..start + comparison_length + search];
            let (delay, coefficient, delayed_channel, reference_channel) =
                best_pair_delay(left, right, first, second, search);
            if delay > options.interchannel_delay_samples && coefficient >= 0.8 {
                push_bounded(
                    &mut events,
                    QcEvent {
                        channel: delayed_channel as u16 + 1,
                        related_channel: Some(reference_channel as u16 + 1),
                        start_seconds: start as f64 / audio.sample_rate as f64,
                        end_seconds: (start + comparison_length) as f64 / audio.sample_rate as f64,
                        measured: Some(delay as f64),
                        unit: Some("samples delayed".into()),
                    },
                );
            }
        }
    }
    events
}

fn best_pair_delay(
    first_samples: &[f32],
    second_samples: &[f32],
    first_channel: usize,
    second_channel: usize,
    maximum: usize,
) -> (usize, f64, usize, usize) {
    let comparison_length = first_samples.len().min(second_samples.len()) - maximum;
    let second_delayed =
        best_positive_delay(&first_samples[..comparison_length], second_samples, maximum);
    let first_delayed =
        best_positive_delay(&second_samples[..comparison_length], first_samples, maximum);
    if first_delayed.1 > second_delayed.1 {
        (
            first_delayed.0,
            first_delayed.1,
            first_channel,
            second_channel,
        )
    } else {
        (
            second_delayed.0,
            second_delayed.1,
            second_channel,
            first_channel,
        )
    }
}

fn best_positive_delay(reference: &[f32], delayed: &[f32], maximum: usize) -> (usize, f64) {
    (0..=maximum)
        .map(|delay| {
            (
                delay,
                correlation(reference, &delayed[delay..delay + reference.len()]),
            )
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .unwrap_or((0, 0.0))
}

fn stuck_sample_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let minimum = seconds_to_frames(audio, options.stuck_sample_seconds);
    let active = 10.0_f32.powf((options.silence_threshold_dbfs / 20.0) as f32);
    let mut events = Vec::new();
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut first = 0;
        while first < samples.len() {
            let mut end = first + 1;
            while end < samples.len() && samples[end].to_bits() == samples[first].to_bits() {
                end += 1;
            }
            if end - first >= minimum && samples[first].abs() > active {
                push_bounded(
                    &mut events,
                    QcEvent {
                        channel: channel as u16 + 1,
                        related_channel: None,
                        start_seconds: first as f64 / audio.sample_rate as f64,
                        end_seconds: end as f64 / audio.sample_rate as f64,
                        measured: Some(samples[first] as f64),
                        unit: Some("FS constant".into()),
                    },
                );
            }
            first = end;
        }
    }
    events
}

fn discontinuity_events(audio: &AudioBuffer, options: &QcOptions) -> Vec<QcEvent> {
    let mut events = Vec::new();
    let coalesce = (audio.sample_rate as usize / 1_000).max(1);
    for (channel, samples) in audio.data.iter().enumerate() {
        let mut previous_event = None;
        for index in 1..samples.len() {
            let delta = (samples[index] as f64 - samples[index - 1] as f64).abs();
            if delta >= options.discontinuity_threshold
                && previous_event.is_none_or(|previous| index - previous > coalesce)
            {
                push_bounded(
                    &mut events,
                    QcEvent {
                        channel: channel as u16 + 1,
                        related_channel: None,
                        start_seconds: index as f64 / audio.sample_rate as f64,
                        end_seconds: (index + 1) as f64 / audio.sample_rate as f64,
                        measured: Some(delta),
                        unit: Some("FS delta".into()),
                    },
                );
                previous_event = Some(index);
            }
        }
    }
    events
}

#[allow(clippy::too_many_arguments)]
fn update_window_run(
    events: &mut Vec<QcEvent>,
    start: &mut Option<usize>,
    maximum: &mut f64,
    detected: bool,
    channel: usize,
    related_channel: Option<usize>,
    index: usize,
    windows: usize,
    minimum_windows: usize,
    window: usize,
    sample_rate: u32,
    measured: f64,
    unit: &str,
) {
    if detected {
        start.get_or_insert(index);
        *maximum = maximum.max(measured);
    }
    if let Some(first) = *start {
        if (!detected || index + 1 == windows)
            && index + usize::from(detected) - first >= minimum_windows
        {
            let end = index + usize::from(detected);
            push_bounded(
                events,
                QcEvent {
                    channel: channel as u16 + 1,
                    related_channel: related_channel.map(|value| value as u16 + 1),
                    start_seconds: (first * window) as f64 / sample_rate as f64,
                    end_seconds: (end * window) as f64 / sample_rate as f64,
                    measured: Some(*maximum),
                    unit: Some(unit.into()),
                },
            );
        }
        if !detected || index + 1 == windows {
            *start = None;
            *maximum = f64::NEG_INFINITY;
        }
    }
}

fn level_dbfs(samples: &[f32]) -> f64 {
    20.0 * rms(samples).max(1e-30).log10()
}

fn push_bounded(events: &mut Vec<QcEvent>, event: QcEvent) {
    if events.len() <= MAX_QC_EVENTS {
        events.push(event);
    }
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
    push_bounded(
        events,
        QcEvent {
            channel: channel as u16 + 1,
            related_channel: None,
            start_seconds: (first * window) as f64 / sample_rate as f64,
            end_seconds: (end * window) as f64 / sample_rate as f64,
            measured: Some(measured),
            unit: Some(unit.into()),
        },
    );
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
                    push_bounded(
                        &mut events,
                        QcEvent {
                            channel: channel as u16 + 1,
                            related_channel: None,
                            start_seconds: first as f64 / audio.sample_rate as f64,
                            end_seconds: index as f64 / audio.sample_rate as f64,
                            measured: measure(&samples[first..index]),
                            unit: unit.map(str::to_owned),
                        },
                    );
                }
            }
        }
        if let Some(first) = start {
            if samples.len() - first >= minimum {
                push_bounded(
                    &mut events,
                    QcEvent {
                        channel: channel as u16 + 1,
                        related_channel: None,
                        start_seconds: first as f64 / audio.sample_rate as f64,
                        end_seconds: samples.len() as f64 / audio.sample_rate as f64,
                        measured: measure(&samples[first..]),
                        unit: unit.map(str::to_owned),
                    },
                );
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

    fn result_by_id<'a>(results: &'a [QcResult], id: &str) -> &'a QcResult {
        results
            .iter()
            .find(|result| result.ebu_qc_id == id)
            .unwrap_or_else(|| panic!("missing QC result {id}"))
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

        assert_eq!(results.len(), 21);
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

    #[test]
    fn detects_noise_crosstalk_and_panning_with_pair_evidence() {
        let mut state = 0x1234_5678_u32;
        let noise = (0..96_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 8) as f32 / 16_777_216.0 - 0.5) * 0.008
            })
            .collect::<Vec<_>>();
        let noise_audio = buffer(noise);
        let noise_analysis = normalize::analyze(&noise_audio);
        let noise_options = QcOptions {
            noise_threshold_dbfs: -65.0,
            ..QcOptions::default()
        };
        let noise_results = analyze(&noise_audio, &noise_analysis, &noise_options);
        assert!(!result_by_id(&noise_results, "0086B").passed);

        let source = (0..96_000)
            .map(|index| 0.2 * (TAU * 997.0 * index as f64 / 48_000.0).sin() as f32)
            .collect::<Vec<_>>();
        let victim = source.iter().map(|sample| sample * 0.04).collect();
        let pair_audio = stereo_buffer(source, victim);
        let pair_analysis = normalize::analyze(&pair_audio);
        let pair_results = analyze(&pair_audio, &pair_analysis, &QcOptions::default());
        let crosstalk = result_by_id(&pair_results, "0170B");
        assert!(!crosstalk.passed);
        assert_eq!(crosstalk.events[0].channel, 2);
        assert_eq!(crosstalk.events[0].related_channel, Some(1));
        assert!(!result_by_id(&pair_results, "0230B").passed);
    }

    #[test]
    fn detects_lfe_assignment_and_configured_non_mono_delivery() {
        let frames = 96_000;
        let mut data = vec![vec![0.0; frames]; 6];
        data[3] = (0..frames)
            .map(|index| 0.2 * (TAU * 1_000.0 * index as f64 / 48_000.0).sin() as f32)
            .collect();
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames,
            data,
            channel_roles: default_channel_roles(6),
            source_kind: PcmKind::S16,
        };
        let analysis = normalize::analyze(&audio);
        let results = analyze(&audio, &analysis, &QcOptions::default());
        let assignment = result_by_id(&results, "0095B");
        assert!(!assignment.passed);
        assert_eq!(assignment.events[0].channel, 4);
        assert_eq!(assignment.events[0].related_channel, Some(3));

        let left = vec![0.1; 48_000];
        let right = vec![0.11; 48_000];
        let stereo = stereo_buffer(left, right);
        let stereo_analysis = normalize::analyze(&stereo);
        let mono_results = analyze(
            &stereo,
            &stereo_analysis,
            &QcOptions {
                expect_mono: true,
                ..QcOptions::default()
            },
        );
        assert!(!result_by_id(&mono_results, "0124B").passed);
        assert!(result_by_id(&mono_results, "0124B").calculated);
    }

    #[test]
    fn detects_forge_dc_delay_stuck_sample_and_discontinuity_rules() {
        let mut first = (0..96_000)
            .map(|index| {
                let value = ((index * 7_919) % 65_521) as f32 / 65_521.0;
                (value - 0.5) * 0.2
            })
            .collect::<Vec<_>>();
        let mut second = vec![0.0; first.len()];
        second[4..].copy_from_slice(&first[..first.len() - 4]);
        first[20_000..23_000].fill(0.1);
        first[40_000] = 1.0;
        first[40_001] = -1.0;
        for sample in &mut second {
            *sample += 0.02;
        }
        let audio = stereo_buffer(first, second);
        let analysis = normalize::analyze(&audio);
        let results = analyze(&audio, &analysis, &QcOptions::default());
        for id in [
            "FORGE-DC-OFFSET",
            "FORGE-INTERCHANNEL-DELAY",
            "FORGE-STUCK-SAMPLES",
            "FORGE-DISCONTINUITY",
        ] {
            assert!(!result_by_id(&results, id).passed, "{id} did not fail");
        }
    }

    #[test]
    fn clean_music_speech_and_ambience_controls_do_not_trigger_new_rules() {
        let fixtures = [
            stereo_buffer(
                (0..96_000)
                    .map(|index| 0.1 * (TAU * 440.0 * index as f64 / 48_000.0).sin() as f32)
                    .collect(),
                (0..96_000)
                    .map(|index| 0.1 * (TAU * 554.37 * index as f64 / 48_000.0).sin() as f32)
                    .collect(),
            ),
            stereo_buffer(
                (0..96_000)
                    .map(|index| 0.08 * (TAU * 180.0 * index as f64 / 48_000.0).sin() as f32)
                    .collect(),
                (0..96_000)
                    .map(|index| 0.08 * (TAU * 230.0 * index as f64 / 48_000.0).sin() as f32)
                    .collect(),
            ),
            stereo_buffer(
                (0..96_000)
                    .map(|index| (((index * 7_919) % 65_521) as f32 / 65_521.0 - 0.5) * 0.1)
                    .collect(),
                (0..96_000)
                    .map(|index| (((index * 3_571) % 65_519) as f32 / 65_519.0 - 0.5) * 0.1)
                    .collect(),
            ),
        ];
        for audio in fixtures {
            let analysis = normalize::analyze(&audio);
            let results = analyze(&audio, &analysis, &QcOptions::default());
            for id in ["0086B", "0170B", "0230B", "0095B"] {
                assert!(result_by_id(&results, id).passed, "{id} false positive");
            }
            assert!(!result_by_id(&results, "0124B").calculated);
        }
    }

    #[test]
    fn bounds_event_evidence_and_reports_truncation() {
        let samples = (0..550_000)
            .map(|index| if index % 2 == 0 { -1.0 } else { 1.0 })
            .collect();
        let audio = buffer(samples);
        let discontinuities = forge_result(
            "FORGE-DISCONTINUITY",
            "Sample Discontinuity",
            discontinuity_events(&audio, &QcOptions::default()),
        );
        assert_eq!(discontinuities.events.len(), MAX_QC_EVENTS);
        assert!(discontinuities.events_truncated);
    }
}
