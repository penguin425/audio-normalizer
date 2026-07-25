//! Self-contained integration tests for the full Forge pipeline.
//!
//! These synthesize audio in memory, write WAVs to the temp dir, run the
//! normalizer, read the result back, and assert the loudness/peak targets are
//! met. No external tools or fixtures are required, so `cargo test` validates
//! the entire read -> measure -> gain -> write -> read round trip.

use forge_normalizer::decoder;
use forge_normalizer::dsp::limiter::LimiterConfig;
use forge_normalizer::normalize::{self, Mode, OutputFormat, Plan};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavReader, WavWriter};
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
    };

    // WAV -> MP3 (encode via LAME).
    let (an, _gain) =
        normalize::normalize_one(&wav_in, &mp3_out, &plan, OutputFormat::Mp3).unwrap();
    assert!(an.lufs < -19.0 && an.lufs > -21.0, "input LUFS {}", an.lufs);

    // The MP3 file must be a real, non-empty encoded stream.
    let mp3_size = std::fs::metadata(&mp3_out).unwrap().len();
    assert!(mp3_size > 50_000, "mp3 output too small: {mp3_size} bytes");

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
