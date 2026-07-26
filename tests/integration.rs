//! Self-contained integration tests for the full Forge pipeline.
//!
//! These synthesize audio in memory, write WAVs to the temp dir, run the
//! normalizer, read the result back, and assert the loudness/peak targets are
//! met. No external tools or fixtures are required, so `cargo test` validates
//! the entire read -> measure -> gain -> write -> read round trip.

#[cfg(feature = "mp3-encoding")]
use forge_normalizer::container_qc;
use forge_normalizer::decoder;
use forge_normalizer::dsp::limiter::LimiterConfig;
use forge_normalizer::normalize::{self, DialogueRange, Mode, OutputFormat, Plan};
use forge_normalizer::wav::{
    default_channel_roles, named_channel_layout, AudioBuffer, PcmKind, WavContainer, WavReader,
    WavWriter, WaveChunk,
};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{Accessor, ItemKey, Tag, TagExt, TagType};
use std::f64::consts::PI;
use std::path::PathBuf;

fn synth_sine(sr: u32, dur_s: f64, amp: f32, freq: f64, channels: u16) -> AudioBuffer {
    let n = (sr as f64 * dur_s) as usize;
    let mut data = Vec::with_capacity(channels as usize);
    for _ in 0..channels {
        let ch: Vec<f32> = (0..n)
            .map(|t| amp * ((2.0 * PI * freq * t as f64 / sr as f64).sin() as f32))
            .collect();
        data.push(ch);
    }
    AudioBuffer {
        sample_rate: sr,
        channels,
        frames: n,
        data,
        channel_roles: default_channel_roles(channels),
        source_kind: PcmKind::F32,
    }
}

fn tmp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(name);
    p
}

#[test]
fn ambiguous_multichannel_wav_requires_an_explicit_layout() {
    let buffer = synth_sine(48_000, 0.5, 0.1, 997.0, 8);
    let input = tmp_path("forge_it_ambiguous_8ch.wav");
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

    let error = normalize::analyze_file(&input).unwrap_err();
    assert!(error.contains("ambiguous 8-channel layout"));

    let roles = named_channel_layout("7.1").unwrap();
    let analysis = normalize::analyze_file_with_roles(&input, Some(&roles)).unwrap();
    assert_eq!(analysis.channels, 8);

    let _ = std::fs::remove_file(input);
}

#[test]
fn dialogue_loudness_uses_duration_weighted_ungated_energy() {
    let input = tmp_path("forge_it_dialogue_ranges.wav");
    let mut buffer = synth_sine(48_000, 6.0, 0.2, 997.0, 1);
    for sample in &mut buffer.data[0][..48_000] {
        *sample *= 0.25;
    }
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();
    let ranges = [
        DialogueRange {
            start_seconds: 0.0,
            duration_seconds: 1.0,
        },
        DialogueRange {
            start_seconds: 2.0,
            duration_seconds: 4.0,
        },
    ];

    let measured = normalize::analyze_dialogue_ranges_with_roles(&input, None, &ranges).unwrap();
    let quiet = normalize::analyze_dialogue_ranges_with_roles(&input, None, &ranges[..1]).unwrap();
    let loud = normalize::analyze_dialogue_ranges_with_roles(&input, None, &ranges[1..]).unwrap();
    let to_energy = |lufs: f64| 10.0_f64.powf((lufs + 0.691) / 10.0);
    let expected_energy = (to_energy(quiet.lufs) + 4.0 * to_energy(loud.lufs)) / 5.0;
    let expected_lufs = -0.691 + 10.0 * expected_energy.log10();
    let naive_region_mean = (quiet.lufs + loud.lufs) / 2.0;

    assert!((measured.lufs - expected_lufs).abs() < 1e-10);
    assert!((measured.lufs - naive_region_mean).abs() > 2.0);
    assert_eq!(measured.range_count, 2);
    assert!((measured.duration_seconds - 5.0).abs() < 1e-12);
    assert_eq!(measured.standard, "ATSC A/85:2026-07");
    assert!(measured.method.contains("no relative-level gate"));

    let _ = std::fs::remove_file(input);
}

#[test]
fn stereo_downmix_uses_center_coefficient_and_omits_lfe() {
    let input = tmp_path("forge_it_downmix.wav");
    let mut buffer = synth_sine(48_000, 1.0, 0.0, 997.0, 6);
    for frame in 0..buffer.frames {
        buffer.data[2][frame] = 0.2 * (2.0 * PI * 997.0 * frame as f64 / 48_000.0).sin() as f32;
        buffer.data[3][frame] = 0.9;
    }
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

    let measured = normalize::analyze_stereo_downmix(&input).unwrap();
    assert_eq!(measured.analysis.channels, 2);
    assert!(measured.analysis.lufs.is_finite());
    assert!(measured.analysis.true_peak < 0.16);
    assert!(measured.method.contains("LFE omitted"));

    let _ = std::fs::remove_file(input);
}

#[test]
fn adm_qc_measures_each_mapped_presentation() {
    let input = tmp_path("forge_it_adm.wav");
    let buffer = synth_sine(48_000, 1.0, 0.05, 997.0, 6);
    WavWriter::write_with_metadata(
        &input,
        &buffer,
        PcmKind::F32,
        false,
        WavContainer::Bw64,
        &[
            WaveChunk {
                id: *b"axml",
                body: br#"<audioProgramme audioProgrammeID="APR_1001"/>"#.to_vec(),
            },
            WaveChunk {
                id: *b"chna",
                body: vec![1, 0, 1, 0],
            },
        ],
    )
    .unwrap();
    let map = normalize::AdmPresentationMap {
        presentations: vec![normalize::AdmPresentationSpec {
            id: "APR_1001".into(),
            name: "English".into(),
            channels: vec![1, 2],
        }],
    };

    let result = normalize::analyze_adm_presentations(&input, None, &map).unwrap();
    assert!(result.passed);
    assert!(result.axml_present);
    assert!(result.chna_present);
    assert_eq!(result.presentations[0].channels, vec![1, 2]);
    assert!(result.presentations[0].integrated_lufs.is_finite());
    assert_eq!(
        result.presentations[0].render_method,
        "direct-channel-map (no ADM object renderer)"
    );

    let _ = std::fs::remove_file(input);
}

#[test]
fn dialogue_detector_emits_auditable_merged_ranges() {
    let input = tmp_path("forge_it_dialogue_detector.wav");
    let mut buffer = synth_sine(48_000, 4.0, 0.0, 997.0, 6);
    for frame in 48_000..144_000 {
        buffer.data[2][frame] = 0.2 * (2.0 * PI * 180.0 * frame as f64 / 48_000.0).sin() as f32;
    }
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

    let detection = normalize::detect_dialogue_ranges(&input, None, 0.6).unwrap();
    assert_eq!(detection.detector, "forge-dialogue-deterministic");
    assert_eq!(
        detection.features,
        vec![
            "window_rms_dbfs",
            "adaptive_noise_floor_dbfs",
            "signal_to_noise_db",
            "center_or_mid_focus",
            "zero_crossing_rate",
            "speech_band_energy_ratio",
            "amplitude_modulation_db",
            "periodicity",
        ]
    );
    assert!(detection.detector_version.starts_with("v2/"));
    assert_eq!(detection.window_seconds, 0.25);
    assert_eq!(detection.frames.len(), 16);
    assert!(detection
        .frames
        .iter()
        .all(|frame| frame.confidence.is_finite()));
    assert!(detection.frames.iter().any(|frame| frame.selected));
    assert_eq!(detection.ranges.len(), 1);
    assert_eq!(detection.ranges[0].start_seconds, 1.0);
    assert_eq!(detection.ranges[0].duration_seconds, 2.25);
    assert!(detection.ranges[0].confidence >= 0.6);
    let repeated = normalize::detect_dialogue_ranges(&input, None, 0.6).unwrap();
    assert_eq!(
        serde_json::to_value(&detection).unwrap(),
        serde_json::to_value(&repeated).unwrap(),
        "detector output must be bit-for-bit deterministic"
    );

    let _ = std::fs::remove_file(input);
}

#[test]
fn dialogue_detector_rejects_broadband_noise() {
    let input = tmp_path("forge_it_dialogue_detector_noise.wav");
    let mut state = 0x1234_5678_u32;
    let samples = (0..192_000)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 8) as f32 / 16_777_215.0 - 0.5) * 0.4
        })
        .collect::<Vec<_>>();
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: samples.len(),
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();
    let result = normalize::detect_dialogue_ranges(&input, None, 0.6);
    assert!(
        result.is_err(),
        "broadband noise was classified as dialogue"
    );
    let _ = std::fs::remove_file(input);
}

#[cfg(feature = "aac-encoding")]
#[test]
fn aac_m4a_roundtrips_gaplessly_and_writes_loudness_tags() {
    let input = tmp_path("forge_it_aac_input.wav");
    let output = tmp_path("forge_it_aac_output.m4a");
    let buffer = synth_sine(44_100, 4.0, 0.1, 997.0, 2);
    WavWriter::write(&input, &buffer, PcmKind::S24, false).unwrap();
    let mut input_tag = Tag::new(TagType::Id3v2);
    input_tag.set_title("AAC Roundtrip".to_string());
    input_tag
        .save_to_path(&input, WriteOptions::default())
        .unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let corrected =
        normalize::normalize_one_corrected(&input, &output, &plan, OutputFormat::M4a, 0.6, 2)
            .unwrap();
    assert!(corrected.verification.passed());

    let decoded = decoder::decode(&output).unwrap();
    assert_eq!((decoded.sample_rate, decoded.channels), (44_100, 2));
    let probe = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert!(probe.status.success());
    let duration: f64 = String::from_utf8(probe.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!((duration - 4.0).abs() < 0.001);
    let measured = normalize::analyze(&decoded);
    assert!((measured.lufs - (-16.0)).abs() < 0.6);
    let output_tags = lofty::read_from_path(&output).unwrap();
    let tag = output_tags.primary_tag().unwrap();
    assert_eq!(tag.title().as_deref(), Some("AAC Roundtrip"));
    assert!(tag.get_string(ItemKey::ReplayGainTrackGain).is_some());
    assert!(tag.get_string(ItemKey::ReplayGainTrackPeak).is_some());

    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[cfg(feature = "ffmpeg-encoding")]
#[test]
fn alac_and_vorbis_outputs_roundtrip() {
    let buffer = synth_sine(48_000, 2.0, 0.1, 997.0, 2);
    let input = tmp_path("forge_it_ffmpeg_codec_input.wav");
    WavWriter::write(&input, &buffer, PcmKind::S24, false).unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    for (format, extension, codec) in [
        (OutputFormat::Alac, "m4a", "alac"),
        (OutputFormat::Vorbis, "ogg", "vorbis"),
    ] {
        let output = tmp_path(&format!("forge_it_{codec}_output.{extension}"));
        normalize::normalize_one(&input, &output, &plan, format).unwrap();
        let decoded = decoder::decode(&output).unwrap();
        assert_eq!((decoded.sample_rate, decoded.channels), (48_000, 2));
        assert!(
            decoded.frames.abs_diff(buffer.frames) <= 1_024,
            "{codec} duration drifted: {} vs {} frames",
            decoded.frames,
            buffer.frames
        );
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        assert_eq!(String::from_utf8(probe.stdout).unwrap().trim(), codec);
        let _ = std::fs::remove_file(output);
    }
    let _ = std::fs::remove_file(input);
}

#[test]
fn range_analysis_reports_absolute_timeline_times() {
    let buffer = synth_sine(48_000, 5.0, 0.2, 997.0, 1);
    let input = tmp_path("forge_it_timeline_range.wav");
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

    let timed = normalize::analyze_file_range_with_roles(&input, None, 0.5, Some(3.5), Some(100.0))
        .unwrap();
    assert_eq!(timed.analysis.frames, 168_000);
    assert_eq!(timed.timeline.len(), 35);
    assert!((timed.timeline[0].start_seconds - 0.5).abs() < 1e-12);
    assert!((timed.timeline[0].end_seconds - 0.6).abs() < 1e-12);
    assert!((timed.timeline.last().unwrap().end_seconds - 4.0).abs() < 1e-12);
    assert!(timed.timeline[2].momentary_lufs.is_none());
    assert!(timed.timeline[3].momentary_lufs.is_some());
    assert!(timed.timeline[28].short_term_lufs.is_none());
    assert!(timed.timeline[29].short_term_lufs.is_some());
    assert!(timed
        .timeline
        .iter()
        .all(|point| point.true_peak_dbtp.is_finite()));

    let _ = std::fs::remove_file(input);
}

#[test]
fn bw64_output_preserves_bext_and_writes_measured_loudness() {
    let input = tmp_path("forge_it_bwf_input.wav");
    let output = tmp_path("forge_it_bwf_output.wav");
    let buffer = synth_sine(48_000, 4.0, 0.1, 997.0, 2);
    let mut source_bext = forge_normalizer::metadata::blank_bext();
    source_bext[..14].copy_from_slice(b"Forge BWF test");
    WavWriter::write_with_metadata(
        &input,
        &buffer,
        PcmKind::S24,
        false,
        WavContainer::Riff,
        &[
            WaveChunk {
                id: *b"bext",
                body: source_bext,
            },
            WaveChunk {
                id: *b"axml",
                body: b"<ebuCoreMain/>".to_vec(),
            },
            WaveChunk {
                id: *b"chna",
                body: b"ADM channel assignment".to_vec(),
            },
        ],
    )
    .unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -18.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Bw64,
        bwf: true,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    normalize::normalize_one(&input, &output, &plan, OutputFormat::Wav).unwrap();

    assert_eq!(&std::fs::read(&output).unwrap()[..4], b"BW64");
    let output_bext = forge_normalizer::metadata::read_bext(&output)
        .unwrap()
        .unwrap();
    assert_eq!(&output_bext[..14], b"Forge BWF test");
    assert_eq!(
        forge_normalizer::metadata::read_wave_chunk(&output, *b"axml")
            .unwrap()
            .unwrap(),
        b"<ebuCoreMain/>"
    );
    assert_eq!(
        forge_normalizer::metadata::read_wave_chunk(&output, *b"chna")
            .unwrap()
            .unwrap(),
        b"ADM channel assignment"
    );
    assert_eq!(
        u16::from_le_bytes(output_bext[346..348].try_into().unwrap()),
        2
    );
    let measured = normalize::analyze_file(&output).unwrap();
    let stored_loudness =
        i16::from_le_bytes(output_bext[412..414].try_into().unwrap()) as f64 / 100.0;
    let stored_true_peak =
        i16::from_le_bytes(output_bext[416..418].try_into().unwrap()) as f64 / 100.0;
    assert!((stored_loudness - measured.lufs).abs() <= 0.01);
    assert!((stored_true_peak - measured.true_peak_db()).abs() <= 0.01);

    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[test]
fn flac_output_roundtrips_at_16_and_24_bits() {
    let buf = synth_sine(48_000, 1.0, 0.25, 997.0, 2);
    let input = tmp_path("forge_it_flac_in.wav");
    WavWriter::write(&input, &buf, PcmKind::S24, false).unwrap();

    for (bits, kind) in [("16", PcmKind::S16), ("24", PcmKind::S24)] {
        let output = tmp_path(&format!("forge_it_flac_{bits}.flac"));
        let plan = Plan {
            mode: Mode::Peak,
            target_lufs: -16.0,
            target_peak_db: -6.0,
            target_rms_db: -18.0,
            ceiling_db: -1.0,
            max_gain_db: None,
            dither: true,
            output_kind: Some(kind),
            mp3_bitrate: 192,
            mp3_quality: 2,
            limiter: None,
            wav_container: WavContainer::Auto,
            bwf: false,
            output_sample_rate: None,
            resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
        };
        normalize::normalize_one(&input, &output, &plan, OutputFormat::Flac).unwrap();
        let decoded = decoder::decode(&output).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.frames, buf.frames);
        let peak = normalize::analyze(&decoded).sample_peak_db();
        assert!((peak - (-6.0)).abs() < 0.02, "{bits}-bit peak was {peak}");
        let _ = std::fs::remove_file(output);
    }
    let _ = std::fs::remove_file(input);
}

#[test]
fn failed_encode_preserves_an_existing_destination() {
    let input = tmp_path("forge_it_atomic_input.wav");
    let output = tmp_path("forge_it_atomic_output.wav");
    let buffer = synth_sine(48_000, 1.0, 0.1, 997.0, 2);
    WavWriter::write(&input, &buffer, PcmKind::S24, false).unwrap();
    std::fs::write(&output, b"existing destination").unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: Some(LimiterConfig {
            lookahead_ms: 0.0,
            release_ms: 100.0,
        }),
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    assert!(normalize::normalize_one(&input, &output, &plan, OutputFormat::Wav).is_err());
    assert_eq!(
        std::fs::read(&output).unwrap(),
        b"existing destination",
        "a failed render replaced the destination"
    );
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[cfg(feature = "opus-encoding")]
#[test]
fn opus_resamples_roundtrips_and_writes_r128_track_gain() {
    let input = tmp_path("forge_it_opus_input.wav");
    let output = tmp_path("forge_it_opus_output.opus");
    let buffer = synth_sine(44_100, 6.0, 0.1, 997.0, 2);
    WavWriter::write(&input, &buffer, PcmKind::S24, false).unwrap();
    let mut input_tag = Tag::new(TagType::Id3v2);
    input_tag.set_title("Opus Roundtrip".to_string());
    input_tag
        .save_to_path(&input, WriteOptions::default())
        .unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 128,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    normalize::normalize_one(&input, &output, &plan, OutputFormat::Opus).unwrap();
    let decoded = decoder::decode(&output).unwrap();
    assert_eq!(decoded.sample_rate, 48_000);
    assert_eq!(decoded.channels, 2);
    assert!((decoded.frames as f64 / decoded.sample_rate as f64 - 6.0).abs() < 0.01);
    let analysis = normalize::analyze(&decoded);
    assert!((analysis.lufs - (-16.0)).abs() < 0.5);
    let (track, album) = forge_normalizer::opus::read_r128_tags(&output).unwrap();
    assert_eq!(track, Some(-7 * 256));
    assert_eq!(album, None);
    let output_tags = lofty::read_from_path(&output).unwrap();
    assert_eq!(
        output_tags.primary_tag().unwrap().title().as_deref(),
        Some("Opus Roundtrip")
    );
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[cfg(feature = "opus-encoding")]
#[test]
fn chained_opus_preserves_each_pre_skip_and_end_trim() {
    let first = tmp_path("forge_it_opus_chain_first.opus");
    let second = tmp_path("forge_it_opus_chain_second.opus");
    let chained = tmp_path("forge_it_opus_chained.opus");
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -18.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 128,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let first_audio = synth_sine(48_000, 1.013, 0.05, 440.0, 2);
    let second_audio = synth_sine(48_000, 0.527, 0.05, 880.0, 2);
    normalize::write(&first_audio, &first, &plan, OutputFormat::Opus).unwrap();
    normalize::write(&second_audio, &second, &plan, OutputFormat::Opus).unwrap();
    let mut bytes = std::fs::read(&first).unwrap();
    bytes.extend_from_slice(&std::fs::read(&second).unwrap());
    std::fs::write(&chained, bytes).unwrap();

    let inspection = forge_normalizer::opus::inspect(&chained).unwrap();
    assert_eq!(inspection.chain_count, 2);
    assert_ne!(inspection.chains[0].serial, inspection.chains[1].serial);
    assert_eq!(
        inspection.total_frames,
        (first_audio.frames + second_audio.frames) as u64
    );
    let decoded = decoder::decode(&chained).unwrap();
    assert_eq!(decoded.frames, first_audio.frames + second_audio.frames);
    let audit = forge_normalizer::container_qc::audit(&chained).unwrap();
    assert!(audit.passed);
    assert_eq!(audit.properties["chain_count"], 2);

    forge_normalizer::opus::rewrite_r128_tags(&chained, -18.0, Some(-20.0)).unwrap();
    let inspection = forge_normalizer::opus::inspect(&chained).unwrap();
    assert!(inspection
        .chains
        .iter()
        .all(|chain| chain.r128_track_gain_q7_8 == Some(-5 * 256)));
    assert!(inspection
        .chains
        .iter()
        .all(|chain| chain.r128_album_gain_q7_8 == Some(-3 * 256)));

    let corrupt = tmp_path("forge_it_opus_chained_corrupt.opus");
    let mut bytes = std::fs::read(&chained).unwrap();
    bytes[30] ^= 1;
    std::fs::write(&corrupt, bytes).unwrap();
    let audit = forge_normalizer::container_qc::audit(&corrupt).unwrap();
    assert!(!audit.passed);
    assert!(!audit.layers[0].passed);

    for path in [first, second, chained, corrupt] {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(feature = "opus-encoding")]
#[test]
fn opus_album_writes_shared_r128_album_gain() {
    let input_a = tmp_path("forge_it_opus_album_a.wav");
    let input_b = tmp_path("forge_it_opus_album_b.wav");
    let output_a = tmp_path("forge_it_opus_album_a.opus");
    let output_b = tmp_path("forge_it_opus_album_b.opus");
    WavWriter::write(
        &input_a,
        &synth_sine(48_000, 4.0, 0.03, 440.0, 2),
        PcmKind::S24,
        false,
    )
    .unwrap();
    WavWriter::write(
        &input_b,
        &synth_sine(48_000, 4.0, 0.06, 660.0, 2),
        PcmKind::S24,
        false,
    )
    .unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -18.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 128,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    normalize::normalize_album(
        &[input_a.clone(), input_b.clone()],
        &[output_a.clone(), output_b.clone()],
        &plan,
        &[OutputFormat::Opus, OutputFormat::Opus],
    )
    .unwrap();
    for output in [&output_a, &output_b] {
        let (track, album) = forge_normalizer::opus::read_r128_tags(output).unwrap();
        assert!(track.is_some());
        assert_eq!(album, Some(-5 * 256));
    }
    for path in [input_a, input_b, output_a, output_b] {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(feature = "opus-encoding")]
#[test]
fn opus_mapping_family_one_roundtrips_5_1_through_7_1() {
    use ogg::PacketReader;
    use std::io::BufReader;

    for channels in [6_u16, 7, 8] {
        let frames = 48_000 * 2;
        let roles = if channels >= 7 {
            named_channel_layout(if channels == 7 { "6.1" } else { "7.1" }).unwrap()
        } else {
            default_channel_roles(channels)
        };
        let data = (0..channels as usize)
            .map(|channel| {
                let amplitude = 0.015 * (channel + 1) as f32;
                (0..frames)
                    .map(|frame| {
                        amplitude
                            * (2.0 * PI * (300.0 + channel as f64 * 97.0) * frame as f64 / 48_000.0)
                                .sin() as f32
                    })
                    .collect()
            })
            .collect();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels,
            frames,
            data,
            channel_roles: roles.clone(),
            source_kind: PcmKind::F32,
        };
        let output = tmp_path(&format!("forge_it_opus_{channels}ch.opus"));
        let plan = Plan {
            mode: Mode::Lufs,
            target_lufs: -18.0,
            target_peak_db: -1.0,
            target_rms_db: -18.0,
            ceiling_db: -1.0,
            max_gain_db: None,
            dither: false,
            output_kind: None,
            mp3_bitrate: 384,
            mp3_quality: 2,
            limiter: None,
            wav_container: WavContainer::Auto,
            bwf: false,
            output_sample_rate: None,
            resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
        };
        normalize::write(&buffer, &output, &plan, OutputFormat::Opus).unwrap();

        let mut packets = PacketReader::new(BufReader::new(std::fs::File::open(&output).unwrap()));
        let head = packets.read_packet().unwrap().unwrap().data;
        assert_eq!(head[18], 1, "{channels}ch mapping family");
        assert_eq!(
            &head[21..],
            match channels {
                6 => &[0, 4, 1, 2, 3, 5][..],
                7 => &[0, 4, 1, 2, 3, 5, 6][..],
                _ => &[0, 6, 1, 2, 3, 4, 5, 7][..],
            }
        );

        let decoded = decoder::decode(&output).unwrap();
        assert_eq!(decoded.channels, channels);
        assert_eq!(decoded.channel_roles, roles);
        let rms: Vec<f64> = decoded
            .data
            .iter()
            .map(|channel| {
                (channel
                    .iter()
                    .map(|sample| f64::from(*sample).powi(2))
                    .sum::<f64>()
                    / channel.len() as f64)
                    .sqrt()
            })
            .collect();
        assert!(
            rms.windows(2).all(|pair| pair[0] < pair[1]),
            "{channels}ch order was not preserved: {rms:?}"
        );
        let _ = std::fs::remove_file(output);
    }
}

#[test]
fn replaygain_tags_leave_decoded_audio_unchanged() {
    let input = tmp_path("forge_it_replaygain.flac");
    let buf = synth_sine(48_000, 1.0, 0.25, 440.0, 2);
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    normalize::write(&buf, &input, &plan, OutputFormat::Flac).unwrap();
    let before = decoder::decode(&input).unwrap();
    let analysis = normalize::analyze(&before);
    forge_normalizer::metadata::write_replaygain(
        &input,
        analysis.lufs,
        analysis.sample_peak,
        Some((analysis.lufs - 1.0, analysis.sample_peak)),
    )
    .unwrap();

    let tags = lofty::read_from_path(&input).unwrap();
    let tag = tags.primary_tag().unwrap();
    assert_eq!(
        tag.get_string(ItemKey::ReplayGainTrackGain),
        Some(format!("{:+.2} dB", -18.0 - analysis.lufs).as_str())
    );
    assert!(tag.get_string(ItemKey::ReplayGainTrackPeak).is_some());
    assert!(tag.get_string(ItemKey::ReplayGainAlbumGain).is_some());
    let after = decoder::decode(&input).unwrap();
    assert_eq!(before.frames, after.frames);
    assert_eq!(before.data, after.data);
    let _ = std::fs::remove_file(input);
}

#[test]
fn post_encode_verification_detects_level_mismatch() {
    let input = tmp_path("forge_it_verify_in.wav");
    let output = tmp_path("forge_it_verify_out.flac");
    let buf = synth_sine(48_000, 4.0, 0.1, 1000.0, 2);
    WavWriter::write(&input, &buf, PcmKind::S24, false).unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let (source, gain) =
        normalize::normalize_one(&input, &output, &plan, OutputFormat::Flac).unwrap();

    let valid = normalize::verify_file(&output, &source, gain, &plan, 0.05).unwrap();
    assert!(valid.passed(), "{valid:?}");
    assert!(valid.deviation < 0.01);

    let wrong_gain = gain * 10.0_f32.powf(3.0 / 20.0);
    let invalid = normalize::verify_file(&output, &source, wrong_gain, &plan, 0.5).unwrap();
    assert!(!invalid.level_ok);
    assert!(!invalid.passed());
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[test]
fn true_peak_limiter_reaches_loudness_despite_isolated_transient() {
    let input = tmp_path("forge_it_limiter_in.wav");
    let output = tmp_path("forge_it_limiter_out.flac");
    let mut buf = synth_sine(48_000, 6.0, 0.02, 1000.0, 2);
    for channel in &mut buf.data {
        channel[48_000] = 0.95;
    }
    WavWriter::write(&input, &buf, PcmKind::F32, false).unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: Some(LimiterConfig::default()),
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let (source, gain) =
        normalize::normalize_one(&input, &output, &plan, OutputFormat::Flac).unwrap();
    assert!(20.0 * (gain as f64).log10() > 10.0);
    let result = normalize::analyze_file(&output).unwrap();
    assert!((result.lufs - plan.target_lufs).abs() < 0.3, "{result:?}");
    assert!(
        result.true_peak_db() <= plan.ceiling_db + 0.05,
        "{result:?}"
    );
    assert!(source.true_peak_db() > -1.0);
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[test]
fn roundtrip_lufs_hits_target() {
    let sr = 48_000;
    let buf = synth_sine(sr, 6.0, 0.10, 1000.0, 2);
    let inp = tmp_path("forge_it_lufs_in.wav");
    let outp = tmp_path("forge_it_lufs_out.wav");
    WavWriter::write(&inp, &buf, PcmKind::S16, false).unwrap();
    let mut input_tag = Tag::new(TagType::Id3v2);
    input_tag.set_title("Conformance Tone".to_string());
    input_tag.set_artist("Forge Tests".to_string());
    input_tag
        .save_to_path(&inp, WriteOptions::default())
        .unwrap();

    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::F32),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let (an, _gain) = normalize::normalize_one(&inp, &outp, &plan, OutputFormat::Wav).unwrap();
    assert!(an.lufs < -19.0 && an.lufs > -21.0, "input LUFS {}", an.lufs);
    assert!((an.max_momentary_lufs - an.lufs).abs() < 0.05);
    assert!((an.max_short_term_lufs - an.lufs).abs() < 0.05);
    assert!(an.loudness_range_lu < 0.05);

    let out = WavReader::open(&outp).unwrap();
    let an2 = normalize::analyze(&out);
    assert!(
        (an2.lufs - (-16.0)).abs() < 0.1,
        "output LUFS {} != -16",
        an2.lufs
    );
    // ceiling protection: true peak must not exceed the -1 dBFS ceiling.
    assert!(
        an2.true_peak_db() <= -0.9,
        "true peak {} exceeded ceiling",
        an2.true_peak_db()
    );
    let output_tags = lofty::read_from_path(&outp).unwrap();
    let output_tag = output_tags.primary_tag().unwrap();
    assert_eq!(output_tag.title().as_deref(), Some("Conformance Tone"));
    assert_eq!(output_tag.artist().as_deref(), Some("Forge Tests"));
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}

#[test]
fn roundtrip_peak_mode_hits_target() {
    let sr = 48_000;
    let buf = synth_sine(sr, 4.0, 0.20, 500.0, 1);
    let inp = tmp_path("forge_it_peak_in.wav");
    let outp = tmp_path("forge_it_peak_out.wav");
    WavWriter::write(&inp, &buf, PcmKind::S16, false).unwrap();

    let plan = Plan {
        mode: Mode::Peak,
        target_lufs: -16.0,
        target_peak_db: -3.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let (an, _gain) = normalize::normalize_one(&inp, &outp, &plan, OutputFormat::Wav).unwrap();
    let in_peak_db = an.sample_peak_db();
    assert!(
        (in_peak_db - (-14.0)).abs() < 0.2,
        "input peak {}",
        in_peak_db
    );

    let out = WavReader::open(&outp).unwrap();
    let an2 = normalize::analyze(&out);
    assert!(
        (an2.sample_peak_db() - (-3.0)).abs() < 0.2,
        "output peak {} != -3",
        an2.sample_peak_db()
    );
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
}

#[test]
fn output_sample_rate_conversion_is_exact_and_normalized_after_src() {
    let input = tmp_path("forge_it_src_48k.wav");
    let output = tmp_path("forge_it_src_44k1.wav");
    let buffer = synth_sine(48_000, 2.0, 0.2, 997.0, 2);
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();
    let plan = Plan {
        mode: Mode::Peak,
        target_lufs: -16.0,
        target_peak_db: -6.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: true,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: Some(44_100),
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Best,
    };
    let (source, _) = normalize::normalize_one(&input, &output, &plan, OutputFormat::Wav).unwrap();
    assert_eq!(source.sample_rate, 44_100);
    assert_eq!(source.frames, 88_200);
    let decoded = decoder::decode(&output).unwrap();
    assert_eq!(decoded.sample_rate, 44_100);
    assert_eq!(decoded.frames, 88_200);
    let measured = normalize::analyze(&decoded);
    assert!((measured.sample_peak_db() - (-6.0)).abs() < 0.02);
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}

#[test]
fn album_mode_applies_shared_gain() {
    let sr = 48_000;
    let quiet = synth_sine(sr, 5.0, 0.05, 1000.0, 2);
    let loud = synth_sine(sr, 5.0, 0.60, 1000.0, 2);
    let i1 = tmp_path("forge_it_album_q.wav");
    let i2 = tmp_path("forge_it_album_l.wav");
    let o1 = tmp_path("forge_it_album_q_n.wav");
    let o2 = tmp_path("forge_it_album_l_n.wav");
    WavWriter::write(&i1, &quiet, PcmKind::S16, false).unwrap();
    WavWriter::write(&i2, &loud, PcmKind::S16, false).unwrap();

    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -20.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };
    let results = normalize::normalize_album(
        &[i1.clone(), i2.clone()],
        &[o1.clone(), o2.clone()],
        &plan,
        &[OutputFormat::Wav, OutputFormat::Wav],
    )
    .unwrap();
    // Both files must share the exact same gain.
    let g0 = results[0].1;
    let g1 = results[1].1;
    assert_eq!(g0, g1, "album gain not shared");
    // The album loudness should land on the target.
    let analyses: Vec<_> = results.iter().map(|(a, _)| a.clone()).collect();
    let album_l = normalize::album_lufs(&analyses);
    let out0 = WavReader::open(&o1).unwrap();
    let out1 = WavReader::open(&o2).unwrap();
    let an0 = normalize::analyze(&out0);
    let an1 = normalize::analyze(&out1);
    let out_album = normalize::album_lufs(&[an0, an1]);
    assert!(
        (out_album - (-20.0)).abs() < 0.15,
        "album output {} != -20",
        out_album
    );
    let _ = (album_l,);
    for p in [&i1, &i2, &o1, &o2] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn corrected_album_verifies_a_shared_gain() {
    let quiet = synth_sine(48_000, 5.0, 0.04, 997.0, 2);
    let loud = synth_sine(48_000, 5.0, 0.20, 997.0, 2);
    let input_a = tmp_path("forge_it_corrected_album_a.wav");
    let input_b = tmp_path("forge_it_corrected_album_b.wav");
    let output_a = tmp_path("forge_it_corrected_album_a.flac");
    let output_b = tmp_path("forge_it_corrected_album_b.flac");
    WavWriter::write(&input_a, &quiet, PcmKind::F32, false).unwrap();
    WavWriter::write(&input_b, &loud, PcmKind::F32, false).unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -18.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: Some(PcmKind::S24),
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };

    let result = normalize::normalize_album_corrected(
        &[input_a.clone(), input_b.clone()],
        &[output_a.clone(), output_b.clone()],
        &plan,
        &[OutputFormat::Flac, OutputFormat::Flac],
        0.05,
        2,
    )
    .unwrap();

    assert_eq!(result.attempts, 1);
    assert!(result.verifications.iter().all(|item| item.passed()));
    assert!((result.actual_album_lufs - result.expected_album_lufs).abs() <= 0.05);
    for path in [input_a, input_b, output_a, output_b] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn album_loudness_uses_the_combined_block_population() {
    let short_quiet = normalize::analyze(&synth_sine(48_000, 1.0, 0.01, 1000.0, 1));
    let long_loud = normalize::analyze(&synth_sine(48_000, 10.0, 0.10, 1000.0, 1));
    let album = normalize::album_lufs(&[short_quiet.clone(), long_loud.clone()]);

    assert!(
        (album - long_loud.lufs).abs() < 0.1,
        "long track's blocks should dominate: album={album}, long={}",
        long_loud.lufs
    );
    let old_equal_track_average = -0.691
        + 10.0
            * ((10.0_f64.powf((short_quiet.lufs + 0.691) / 10.0)
                + 10.0_f64.powf((long_loud.lufs + 0.691) / 10.0))
                / 2.0)
                .log10();
    assert!((album - old_equal_track_average).abs() > 2.0);
}

#[test]
fn silence_normalizes_to_neg_infinity_without_panicking() {
    let sr = 48_000;
    let buf = synth_sine(sr, 2.0, 0.0, 1000.0, 2);
    let an = normalize::analyze(&buf);
    assert!(an.lufs == f64::NEG_INFINITY, "silence LUFS {}", an.lufs);
    assert!(an.sample_peak == 0.0);
}

#[test]
fn streaming_decoder_emits_bounded_chunks() {
    let buffer = synth_sine(48_000, 6.0, 0.1, 440.0, 2);
    let path = tmp_path("forge_it_stream_chunks.wav");
    WavWriter::write(&path, &buffer, PcmKind::S16, false).unwrap();
    let mut total_frames = 0usize;
    let mut largest_chunk = 0usize;
    let info = decoder::decode_stream(&path, |_, planar| {
        let frames = planar[0].len();
        total_frames += frames;
        largest_chunk = largest_chunk.max(frames);
        Ok(())
    })
    .unwrap();
    assert_eq!(info.channels, 2);
    assert_eq!(total_frames, buffer.frames);
    assert!(largest_chunk < total_frames);
    let _ = std::fs::remove_file(path);
}

#[test]
#[cfg(feature = "mp3-encoding")]
fn mp3_encode_and_decode_roundtrip() {
    // End-to-end MP3 support: synthesize audio, write WAV, normalize it to an
    // MP3 file (LAME encoder), then decode that MP3 back (symphonia decoder)
    // and verify the loudness landed near the target. This exercises both the
    // new encoder and the new decoder in one round trip.
    let sr = 48_000;
    let buf = synth_sine(sr, 6.0, 0.10, 1000.0, 2);
    let wav_in = tmp_path("forge_it_mp3_in.wav");
    let mp3_out = tmp_path("forge_it_mp3_out.mp3");
    WavWriter::write(&wav_in, &buf, PcmKind::F32, false).unwrap();

    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 192,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };

    // WAV -> MP3 (encode via LAME).
    let (an, _gain) =
        normalize::normalize_one(&wav_in, &mp3_out, &plan, OutputFormat::Mp3).unwrap();
    assert!(an.lufs < -19.0 && an.lufs > -21.0, "input LUFS {}", an.lufs);

    // The MP3 file must be a real, non-empty encoded stream.
    let mp3_size = std::fs::metadata(&mp3_out).unwrap().len();
    assert!(mp3_size > 50_000, "mp3 output too small: {mp3_size} bytes");
    let container = container_qc::audit(&mp3_out).unwrap();
    assert!(container.passed, "{container:#?}");
    assert_eq!(container.format, "mp3");
    assert_eq!(container.properties["lame"]["encoder"], "LAME3.100");
    assert!(container.properties["lame"]["encoder_delay"]
        .as_u64()
        .is_some_and(|value| value > 0));

    // MP3 -> planar buffer (decode via symphonia) and re-measure.
    let mp3_buf = normalize::load(&mp3_out).unwrap();
    assert_eq!(mp3_buf.channels, 2, "decoded mp3 channel count");
    assert_eq!(mp3_buf.sample_rate, sr, "decoded mp3 sample rate");

    let an2 = normalize::analyze(&mp3_buf);
    // Lossy MP3 encoding shifts loudness by a fraction of a dB; the encoder
    // delay/padding adds a few ms of edge material. 1.0 LU is a generous but
    // still meaningful tolerance for a 6 s tone.
    assert!(
        (an2.lufs - (-16.0)).abs() < 1.0,
        "decoded mp3 LUFS {} not near -16",
        an2.lufs
    );

    let _ = std::fs::remove_file(&wav_in);
    let _ = std::fs::remove_file(&mp3_out);
}

#[test]
#[cfg(feature = "mp3-encoding")]
fn mp3_mono_encoding_is_gapless_and_does_not_abort_lame() {
    let buffer = synth_sine(44_100, 2.0, 0.1, 440.0, 1);
    let output = tmp_path("forge_it_mp3_mono.mp3");
    forge_normalizer::mp3enc::write_mp3(&output, &buffer, 192, 2).unwrap();

    let audit = container_qc::audit(&output).unwrap();
    assert!(audit.passed, "{audit:#?}");
    assert_eq!(audit.properties["channels"], 1);
    assert_eq!(audit.properties["gapless_samples"], 88_200);
    let decoded = normalize::load(&output).unwrap();
    assert_eq!(decoded.channels, 1);
    assert_eq!(decoded.sample_rate, 44_100);

    let _ = std::fs::remove_file(output);
}

#[test]
#[cfg(feature = "mp3-encoding")]
fn mp3_post_encode_correction_converges_from_the_original_source() {
    let buf = synth_sine(48_000, 6.0, 0.10, 997.0, 2);
    let input = tmp_path("forge_it_mp3_correction_in.wav");
    let output = tmp_path("forge_it_mp3_correction_out.mp3");
    WavWriter::write(&input, &buf, PcmKind::F32, false).unwrap();
    let plan = Plan {
        mode: Mode::Lufs,
        target_lufs: -16.0,
        target_peak_db: -1.0,
        target_rms_db: -18.0,
        ceiling_db: -1.0,
        max_gain_db: None,
        dither: false,
        output_kind: None,
        mp3_bitrate: 128,
        mp3_quality: 2,
        limiter: None,
        wav_container: WavContainer::Auto,
        bwf: false,
        output_sample_rate: None,
        resample_quality: forge_normalizer::dsp::resample::ResampleQuality::Balanced,
    };

    let result =
        normalize::normalize_one_corrected(&input, &output, &plan, OutputFormat::Mp3, 0.01, 3)
            .unwrap();

    assert!(result.attempts > 1, "MP3 drift did not exercise a retry");
    assert!(result.verification.passed(), "{result:?}");
    assert!(result.verification.deviation <= 0.01, "{result:?}");
    let _ = std::fs::remove_file(input);
    let _ = std::fs::remove_file(output);
}
