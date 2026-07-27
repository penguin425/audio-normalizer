use forge_normalizer::normalize;
use forge_normalizer::qc::{self, QcOptions, QcResult};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use std::f64::consts::TAU;

const SAMPLE_RATE: u32 = 48_000;
const FRAMES: usize = 96_000;

fn stereo(left: Vec<f32>, right: Vec<f32>) -> AudioBuffer {
    AudioBuffer {
        sample_rate: SAMPLE_RATE,
        channels: 2,
        frames: left.len(),
        data: vec![left, right],
        channel_roles: default_channel_roles(2),
        source_kind: PcmKind::F32,
    }
}

fn tone(frequency: f64, amplitude: f32) -> Vec<f32> {
    (0..FRAMES)
        .map(|index| amplitude * (TAU * frequency * index as f64 / SAMPLE_RATE as f64).sin() as f32)
        .collect()
}

fn pseudo_noise(amplitude: f32) -> Vec<f32> {
    let mut state = 0x6d2b_79f5_u32;
    (0..FRAMES)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 8) as f32 / 16_777_216.0 - 0.5) * amplitude
        })
        .collect()
}

fn audit_file(
    directory: &tempfile::TempDir,
    name: &str,
    audio: &AudioBuffer,
    options: &QcOptions,
) -> Vec<QcResult> {
    let path = directory.path().join(name);
    WavWriter::write(&path, audio, PcmKind::F32, false).unwrap();
    let analysis = normalize::analyze(audio);
    qc::analyze_file(&path, &analysis, options).unwrap()
}

fn assert_only_target_fails(results: &[QcResult], target: &str, peers: &[&str]) {
    for id in peers {
        let result = results
            .iter()
            .find(|result| result.rule_id == *id)
            .unwrap_or_else(|| panic!("missing rule {id}"));
        assert_eq!(
            result.passed,
            *id != target,
            "fixture for {target} unexpectedly changed {id}"
        );
    }
}

#[test]
fn isolated_positive_files_and_clean_content_controls() {
    const EBU_RULES: [&str; 5] = ["0086B", "0170B", "0230B", "0095B", "0124B"];
    const FORGE_RULES: [&str; 4] = [
        "FORGE-DC-OFFSET",
        "FORGE-INTERCHANNEL-DELAY",
        "FORGE-STUCK-SAMPLES",
        "FORGE-DISCONTINUITY",
    ];
    let directory = tempfile::tempdir().unwrap();

    let noise = AudioBuffer {
        sample_rate: SAMPLE_RATE,
        channels: 1,
        frames: FRAMES,
        data: vec![pseudo_noise(0.008)],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    let noise_results = audit_file(
        &directory,
        "0086B-noise.wav",
        &noise,
        &QcOptions {
            noise_threshold_dbfs: -65.0,
            ..QcOptions::default()
        },
    );
    assert_only_target_fails(&noise_results, "0086B", &EBU_RULES[..4]);

    let source = tone(997.0, 0.2);
    let crosstalk = stereo(
        source.clone(),
        source.iter().map(|sample| sample * 0.04).collect(),
    );
    let crosstalk_results = audit_file(
        &directory,
        "0170B-crosstalk.wav",
        &crosstalk,
        &QcOptions {
            panning_imbalance_db: 100.0,
            ..QcOptions::default()
        },
    );
    assert_only_target_fails(&crosstalk_results, "0170B", &EBU_RULES[..4]);

    let panning = stereo(tone(440.0, 0.2), tone(733.0, 0.01));
    let panning_results = audit_file(
        &directory,
        "0230B-panning.wav",
        &panning,
        &QcOptions::default(),
    );
    assert_only_target_fails(&panning_results, "0230B", &EBU_RULES[..4]);

    let mut lfe_data = vec![vec![0.0; FRAMES]; 6];
    lfe_data[3] = tone(1_000.0, 0.2);
    let lfe = AudioBuffer {
        sample_rate: SAMPLE_RATE,
        channels: 6,
        frames: FRAMES,
        data: lfe_data,
        channel_roles: default_channel_roles(6),
        source_kind: PcmKind::F32,
    };
    let lfe_results = audit_file(
        &directory,
        "0095B-lfe-assignment.wav",
        &lfe,
        &QcOptions::default(),
    );
    assert_only_target_fails(&lfe_results, "0095B", &EBU_RULES[..4]);

    let non_mono = stereo(tone(440.0, 0.1), tone(554.37, 0.1));
    let mono_results = audit_file(
        &directory,
        "0124B-non-mono.wav",
        &non_mono,
        &QcOptions {
            expect_mono: true,
            ..QcOptions::default()
        },
    );
    assert_only_target_fails(&mono_results, "0124B", &EBU_RULES);

    let dc = stereo(
        tone(440.0, 0.05)
            .into_iter()
            .map(|sample| sample + 0.02)
            .collect(),
        tone(554.37, 0.05),
    );
    let dc_results = audit_file(&directory, "forge-dc.wav", &dc, &QcOptions::default());
    assert_only_target_fails(&dc_results, "FORGE-DC-OFFSET", &FORGE_RULES);

    let reference = pseudo_noise(0.2);
    let mut delayed = vec![0.0; FRAMES];
    delayed[4..].copy_from_slice(&reference[..FRAMES - 4]);
    let delay = stereo(reference, delayed);
    let delay_results = audit_file(&directory, "forge-delay.wav", &delay, &QcOptions::default());
    assert_only_target_fails(&delay_results, "FORGE-INTERCHANNEL-DELAY", &FORGE_RULES);

    let mut held = tone(440.0, 0.1);
    held[24_000..27_000].fill(0.1);
    let stuck = stereo(held, tone(554.37, 0.1));
    let stuck_results = audit_file(&directory, "forge-stuck.wav", &stuck, &QcOptions::default());
    assert_only_target_fails(&stuck_results, "FORGE-STUCK-SAMPLES", &FORGE_RULES);

    let mut jumped = tone(440.0, 0.1);
    jumped[48_000] = 1.0;
    jumped[48_001] = -1.0;
    let discontinuity = stereo(jumped, tone(554.37, 0.1));
    let discontinuity_results = audit_file(
        &directory,
        "forge-discontinuity.wav",
        &discontinuity,
        &QcOptions::default(),
    );
    assert_only_target_fails(&discontinuity_results, "FORGE-DISCONTINUITY", &FORGE_RULES);

    for (name, clean) in [
        (
            "clean-music.wav",
            stereo(tone(440.0, 0.1), tone(554.37, 0.1)),
        ),
        (
            "clean-speech.wav",
            stereo(tone(180.0, 0.08), tone(230.0, 0.08)),
        ),
        (
            "clean-ambience.wav",
            stereo(pseudo_noise(0.1), pseudo_noise(0.1)),
        ),
    ] {
        let results = audit_file(&directory, name, &clean, &QcOptions::default());
        for id in EBU_RULES[..4].iter().chain(FORGE_RULES.iter()) {
            let result = results.iter().find(|result| result.rule_id == *id).unwrap();
            assert!(result.passed, "{name} triggered {id}");
        }
    }
}
