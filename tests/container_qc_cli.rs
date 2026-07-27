mod common;

use forge_normalizer::wav::{AudioBuffer, ChannelRole, PcmKind, WavWriter};
use serde_json::Value;
use std::fs;
use std::process::Command;

#[test]
fn container_qc_cli_returns_pass_and_fail_status() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.wav");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 100,
        data: vec![vec![0.0; 100]],
        channel_roles: vec![ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    WavWriter::write(&path, &audio, PcmKind::S16, false).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], true);

    let mut bytes = fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
    fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("FAIL"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], false);
}

#[test]
fn container_qc_cli_reports_malformed_isobmff_as_qc_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.m4a");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20_u32.to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(b"M4A ");
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(b"isom");
    bytes.extend_from_slice(&16_u32.to_be_bytes());
    bytes.extend_from_slice(b"moov");
    bytes.extend_from_slice(&32_u32.to_be_bytes());
    bytes.extend_from_slice(b"trak");
    fs::write(&path, bytes).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("FAIL"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "isobmff");
    assert_eq!(audit["passed"], false);
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-ISOBMFF-MOVIE-STRUCTURE"));
}

#[test]
fn container_qc_cli_audits_real_aac_lc_and_he_aac_without_runtime_decoding() {
    let directory = tempfile::tempdir().unwrap();
    let he_path = directory.path().join("he-aac.aac");
    fs::write(&he_path, common::HE_AAC_ADTS).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&he_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "aac-adts");
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["properties"]["frames"], 4);

    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let lc_path = directory.path().join("aac-lc.aac");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.1",
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-f",
            "adts",
        ])
        .arg(&lc_path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&lc_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "aac-adts");
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["properties"]["audio_object_type"], 2);
    assert_eq!(audit["properties"]["sample_rate_hz"], 48_000);

    let loas_path = directory.path().join("aac-lc.loas");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.1",
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-f",
            "latm",
        ])
        .arg(&loas_path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&loas_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "aac-loas");
    assert_eq!(audit["passed"], true);
    assert_eq!(
        audit["properties"]["audio_specific_config"]["audio_object_type"],
        2
    );
    assert_eq!(
        audit["properties"]["audio_specific_config"]["output_sample_rate_hz"],
        48_000
    );
}

#[test]
fn container_qc_cli_audits_mpegts_program_maps_audio_pes_and_continuity() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broadcast.ts");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.5",
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-mpegts_flags",
            "+resend_headers",
            "-f",
            "mpegts",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "mpegts");
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["properties"]["packet_size"], 188);
    assert_eq!(audit["properties"]["audio_streams"][0]["codec"], "aac-adts");
    assert!(
        audit["properties"]["audio_streams"][0]["last_pts_90khz"]
            .as_u64()
            .unwrap()
            >= audit["properties"]["audio_streams"][0]["first_pts_90khz"]
                .as_u64()
                .unwrap()
    );

    let m2ts = directory.path().join("camera.m2ts");
    let bytes = fs::read(&path).unwrap();
    let mut wrapped = Vec::with_capacity(bytes.len() / 188 * 192);
    for (index, packet) in bytes.chunks_exact(188).enumerate() {
        wrapped.extend_from_slice(&(index as u32).to_be_bytes());
        wrapped.extend_from_slice(packet);
    }
    fs::write(&m2ts, wrapped).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&m2ts)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "m2ts");
    assert_eq!(audit["properties"]["packet_size"], 192);

    let mut corrupt = fs::read(&path).unwrap();
    let audio_pid = audit_audio_pid(&path);
    let mut matching = corrupt.chunks_exact_mut(188).filter(|packet| {
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        pid == audio_pid && packet[3] & 0x10 != 0
    });
    let first_counter = matching.next().unwrap()[3] & 0x0f;
    let second = matching.next().unwrap();
    second[3] = (second[3] & 0xf0) | first_counter.wrapping_add(5) & 0x0f;
    let corrupt_path = directory.path().join("continuity-error.ts");
    fs::write(&corrupt_path, corrupt).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&corrupt_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-MPEGTS-CONTINUITY" && check["passed"] == false));

    let mut corrupt = fs::read(&path).unwrap();
    let pat = corrupt
        .chunks_exact_mut(188)
        .find(|packet| packet[1] & 0x1f == 0 && packet[2] == 0 && packet[1] & 0x40 != 0)
        .unwrap();
    let payload = ts_payload_offset(pat).unwrap();
    let section = payload + 1 + usize::from(pat[payload]);
    pat[section + 8] ^= 1;
    let corrupt_path = directory.path().join("pat-crc-error.ts");
    fs::write(&corrupt_path, corrupt).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&corrupt_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-MPEGTS-PSI" && check["passed"] == false));
}

fn audit_audio_pid(path: &std::path::Path) -> u16 {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(path)
        .output()
        .unwrap();
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    audit["properties"]["audio_streams"][0]["pid"]
        .as_u64()
        .unwrap() as u16
}

fn ts_payload_offset(packet: &[u8]) -> Option<usize> {
    let adaptation_control = packet[3] >> 4 & 3;
    if adaptation_control & 1 == 0 {
        return None;
    }
    if adaptation_control & 2 == 0 {
        Some(4)
    } else {
        Some(5 + usize::from(packet[4])).filter(|offset| *offset < packet.len())
    }
}

#[test]
fn container_qc_cli_audits_real_matroska_and_webm_files() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let matroska = directory.path().join("pcm.mka");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.2",
            "-c:a",
            "pcm_s16le",
            "-write_crc32",
            "1",
        ])
        .arg(&matroska)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&matroska)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "matroska");
    assert_eq!(audit["passed"], true);
    assert!(audit["properties"]["crc32_elements"].as_u64().unwrap() > 0);

    let webm = directory.path().join("opus.webm");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.2",
            "-c:a",
            "libopus",
            "-f",
            "webm",
        ])
        .arg(&webm)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&webm)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "webm");
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["properties"]["audio_tracks"][0]["codec_id"], "A_OPUS");
    assert_eq!(
        audit["properties"]["audio_tracks"][0]["seek_preroll_ns"],
        80_000_000
    );
}
