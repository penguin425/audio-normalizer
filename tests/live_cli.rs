use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn plugin_parity_live_cli_output_matches_realtime_processor() {
    let sample_rate = 48_000.0_f32;
    let frames = 192_000;
    let samples = (0..frames)
        .flat_map(|frame| {
            let sample = 0.1 * (std::f32::consts::TAU * 997.0 * frame as f32 / sample_rate).sin();
            [sample, -sample]
        })
        .collect::<Vec<_>>();
    let input = samples
        .iter()
        .copied()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    let input_len = input.len();
    let mut expected = samples;
    let mut reference = forge_normalizer::realtime::RealtimeGainProcessor::new(
        48_000,
        2,
        forge_normalizer::realtime::RealtimeGainConfig {
            initial_gain_db: 3.0,
            ceiling_dbfs: -1.0,
            attack_ms: 10.0,
            release_ms: 100.0,
        },
    )
    .unwrap();
    reference.process_interleaved(&mut expected).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_forge-live"))
        .args([
            "--sample-rate",
            "48000",
            "--channels",
            "2",
            "--block-frames",
            "37",
            "--meter-interval-ms",
            "500",
            "--gain-db",
            "3",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&input).unwrap());
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.len(), input_len);
    let actual = output
        .stdout
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    let reports = String::from_utf8(output.stderr)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(!reports.is_empty());
    let final_report = reports.last().unwrap();
    assert_eq!(final_report["schema"], "forge-live-v1");
    assert_eq!(final_report["sample_rate_hz"], 48_000);
    assert_eq!(final_report["channels"], 2);
    assert!(final_report["latency_frames"].as_u64().unwrap() >= 16);
    assert!(final_report["momentary_lufs"].is_number());
    assert!(final_report["short_term_lufs"].is_number());
    assert!(final_report["true_peak_dbtp"].is_number());
}

#[test]
fn live_cli_rejects_partial_pcm_frames() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_forge-live"))
        .args(["--channels", "2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&[0; 7]).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("middle of an interleaved"));
}
