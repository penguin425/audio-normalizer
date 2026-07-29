mod common;

use forge_normalizer::wav::{
    AudioBuffer, ChannelRole, PcmKind, WavContainer, WavWriter, WaveChunk,
};
use serde_json::Value;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
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
fn container_qc_cli_reports_ebu_bext_metadata_and_failures() {
    let directory = tempfile::tempdir().unwrap();
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 100,
        data: vec![vec![0.0; 100]],
        channel_roles: vec![ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    let mut bext = vec![0_u8; 602];
    bext[..12].copy_from_slice(b"Evening news");
    bext[256..265].copy_from_slice(b"EBU Forge");
    bext[288..300].copy_from_slice(b"EU-FORGE-001");
    bext[320..330].copy_from_slice(b"2026-07-29");
    bext[330..338].copy_from_slice(b"16:45:30");
    bext[338..346].copy_from_slice(&(48_000_u64 * 3_600).to_le_bytes());
    bext[346..348].copy_from_slice(&2_u16.to_le_bytes());
    for (index, value) in [-2_300_i16, 700, -100, -1_800, -1_900].iter().enumerate() {
        let offset = 412 + index * 2;
        bext[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    bext.extend_from_slice(b"A=PCM,F=48000,W=16,M=mono\r\n");

    let valid_path = directory.path().join("valid-bwf.wav");
    WavWriter::write_with_metadata(
        &valid_path,
        &audio,
        PcmKind::S16,
        false,
        WavContainer::Riff,
        &[WaveChunk {
            id: *b"bext",
            body: bext.clone(),
        }],
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&valid_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["properties"]["bext"]["version"], 2);
    assert_eq!(
        audit["properties"]["bext"]["loudness"]["integrated_lufs"],
        -23.0
    );
    assert_eq!(audit["properties"]["bext"]["coding_history_rows"], 1);

    bext[320..330].copy_from_slice(b"2025-02-29");
    let invalid_path = directory.path().join("invalid-bwf.wav");
    WavWriter::write_with_metadata(
        &invalid_path,
        &audio,
        PcmKind::S16,
        false,
        WavContainer::Riff,
        &[WaveChunk {
            id: *b"bext",
            body: bext,
        }],
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&invalid_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-BWF-BEXT-DATETIME" && check["passed"] == false));
}

#[test]
fn container_qc_cli_reports_ixml_track_mapping() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ixml.wav");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 100,
        data: vec![vec![0.0; 100], vec![0.0; 100]],
        channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    WavWriter::write_with_metadata(
        &path,
        &audio,
        PcmKind::S16,
        false,
        WavContainer::Riff,
        &[WaveChunk {
            id: *b"iXML",
            body: br#"<BWFXML><IXML_VERSION>1.52</IXML_VERSION><TRACK_LIST>
<TRACK_COUNT>2</TRACK_COUNT>
<TRACK><CHANNEL_INDEX>4</CHANNEL_INDEX><INTERLEAVE_INDEX>1</INTERLEAVE_INDEX><NAME>Mid</NAME></TRACK>
<TRACK><CHANNEL_INDEX>6</CHANNEL_INDEX><INTERLEAVE_INDEX>2</INTERLEAVE_INDEX><NAME>Side</NAME></TRACK>
</TRACK_LIST></BWFXML>"#
                .to_vec(),
        }],
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["properties"]["ixml"]["declared_track_count"], 2);
    assert_eq!(audit["properties"]["ixml"]["tracks"][1]["channel_index"], 6);
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
fn container_qc_cli_audits_ac3_and_eac3_syncframes_and_dialnorm() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    for (codec, muxer, dialnorm, expected_bsid) in
        [("ac3", "ac3", -24_i64, 8_i64), ("eac3", "eac3", -27, 16)]
    {
        let path = directory.path().join(format!("delivery.{muxer}"));
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
                codec,
                "-b:a",
                "192k",
                "-dialnorm",
                &dialnorm.to_string(),
                "-f",
                muxer,
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
        assert_eq!(audit["format"], muxer);
        assert_eq!(audit["passed"], true);
        assert_eq!(audit["properties"]["sample_rate_hz"], 48_000);
        assert_eq!(audit["properties"]["bsid"], expected_bsid);
        assert_eq!(audit["properties"]["dialnorm_db"][0], dialnorm);
        assert!(audit["properties"]["frames"].as_u64().unwrap() > 0);
        assert_eq!(
            audit["properties"]["drc_encoded_gain_ranges_db"]["rf_mode_compr"],
            serde_json::json!([-48.165, 47.889])
        );
        if codec == "eac3" {
            assert_eq!(audit["properties"]["access_units"]["valid"], true);
            assert!(
                audit["properties"]["access_units"]["complete"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_eq!(audit["properties"]["atmos_joc"]["signaled"], false);
            assert_eq!(audit["properties"]["presentations"][0]["channels"], 1);
        }
    }
}

#[test]
fn container_qc_cli_rejects_truncated_ac3_syncframe() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("truncated.ac3");
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
            "ac3",
            "-b:a",
            "192k",
            "-f",
            "ac3",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");
    let mut bytes = fs::read(&path).unwrap();
    bytes.pop();
    fs::write(&path, bytes).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "ac3");
    assert_eq!(audit["passed"], false);
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-AC3-BOUNDS" && check["passed"] == false));
}

#[test]
fn container_qc_cli_audits_standalone_iamf_obu_structure() {
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut bytes = vec![obu_type << 3, payload.len() as u8];
        bytes.extend_from_slice(payload);
        bytes
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("presentation.iamf");
    let mut bytes = obu(31, b"iamf\x00\x00");
    bytes.extend(obu(
        0,
        &[0, b'i', b'p', b'c', b'm', 1, 0, 0, 0, 16, 0, 0, 187, 128],
    ));
    bytes.extend(obu(1, &[0, 0, 0, 1, 0, 0]));
    bytes.extend(obu(2, &[0]));
    bytes.extend(obu(6, &[0]));
    fs::write(&path, bytes).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "iamf");
    assert_eq!(audit["passed"], true);
    assert_eq!(audit["properties"]["primary_profile"], "simple");
    assert_eq!(audit["properties"]["obu_counts"]["audio-frame-id0"], 1);
    assert_eq!(audit["properties"]["codec_configs"][0]["codec_id"], "ipcm");
    assert_eq!(
        audit["properties"]["codec_configs"][0]["sample_rate_hz"],
        48_000
    );
    assert_eq!(
        audit["properties"]["audio_elements"][0]["audio_substream_ids"],
        serde_json::json!([0])
    );
    assert_eq!(audit["properties"]["audio_frame_counts"]["0"], 1);
    for rule_id in [
        "FORGE-IAMF-CODEC-CONFIG",
        "FORGE-IAMF-DESCRIPTOR-LINKS",
        "FORGE-IAMF-AUDIO-FRAME-LINKS",
    ] {
        assert!(audit["layers"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|layer| layer["checks"].as_array().unwrap())
            .any(|check| check["rule_id"] == rule_id && check["passed"] == true));
    }
}

#[test]
fn container_qc_cli_rejects_oversized_and_misordered_iamf_obus() {
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![obu_type << 3, payload.len() as u8];
        bytes.extend_from_slice(payload);
        bytes
    }

    let directory = tempfile::tempdir().unwrap();
    let order_path = directory.path().join("misordered.iamf");
    let mut bytes = obu(31, b"iamf\x00\x00");
    bytes.extend(obu(
        0,
        &[0, b'i', b'p', b'c', b'm', 1, 0, 0, 0, 16, 0, 0, 187, 128],
    ));
    bytes.extend(obu(2, &[0]));
    bytes.extend(obu(1, &[0, 0, 0, 1, 0, 0]));
    bytes.extend(obu(6, &[0]));
    fs::write(&order_path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&order_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-IAMF-ORDER" && check["passed"] == false));

    let size_path = directory.path().join("oversized.iamf");
    let mut bytes = obu(31, b"iamf\x00\x00");
    bytes.extend_from_slice(&[24 << 3, 0x80, 0x80, 0x80, 0x01]);
    fs::write(&size_path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&size_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-IAMF-OBU-BOUNDS" && check["passed"] == false));
}

#[test]
fn container_qc_cli_rejects_invalid_iamf_codec_and_substream_links() {
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![obu_type << 3, payload.len() as u8];
        bytes.extend_from_slice(payload);
        bytes
    }

    fn checks(audit: &Value) -> impl Iterator<Item = &Value> {
        audit["layers"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|layer| layer["checks"].as_array().unwrap())
    }

    let directory = tempfile::tempdir().unwrap();
    let bad_codec = directory.path().join("bad-codec.iamf");
    let mut bytes = obu(31, b"iamf\x00\x00");
    bytes.extend(obu(
        0,
        &[0, b'i', b'p', b'c', b'm', 1, 0, 1, 0, 16, 0, 0, 187, 128],
    ));
    bytes.extend(obu(1, &[0, 0, 0, 1, 0, 0]));
    bytes.extend(obu(2, &[0]));
    bytes.extend(obu(6, &[0]));
    fs::write(&bad_codec, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&bad_codec)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(checks(&audit).any(|check| {
        check["rule_id"] == "FORGE-IAMF-CODEC-CONFIG" && check["passed"] == false
    }));

    let bad_frame = directory.path().join("bad-frame-link.iamf");
    let mut bytes = obu(31, b"iamf\x00\x00");
    bytes.extend(obu(
        0,
        &[0, b'i', b'p', b'c', b'm', 1, 0, 0, 0, 16, 0, 0, 187, 128],
    ));
    bytes.extend(obu(1, &[0, 0, 0, 1, 0, 0]));
    bytes.extend(obu(2, &[0]));
    bytes.extend(obu(7, &[0]));
    fs::write(&bad_frame, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&bad_frame)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(checks(&audit).any(|check| {
        check["rule_id"] == "FORGE-IAMF-AUDIO-FRAME-LINKS" && check["passed"] == false
    }));
    assert!(audit["properties"]["payload_errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error.as_str().unwrap().contains("undeclared substream 1")));
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

#[test]
fn container_qc_cli_audits_aaf_stored_format_and_schema() {
    const AAF_ROOT_CLSID: &str = "b3b398a5-1c90-11d4-8053-080036210804";
    const AAF_V4_HEADER_CLSID_LE: [u8; 16] = [
        0x01, 0x02, 0x01, 0x0d, 0x00, 0x02, 0x00, 0x00, 0x06, 0x0e, 0x2b, 0x34, 0x03, 0x02, 0x01,
        0x01,
    ];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("project.aaf");
    {
        let mut compound = cfb::create(&path).unwrap();
        compound
            .set_storage_clsid("/", uuid::Uuid::parse_str(AAF_ROOT_CLSID).unwrap())
            .unwrap();
        for storage in [
            "/MetaDictionary-1",
            "/MetaDictionary-1/ClassDefinitions-3{0}",
            "/MetaDictionary-1/TypeDefinitions-4{0}",
            "/Header-2",
            "/Header-2/Content-3b03",
            "/Header-2/Dictionary-3b04",
            "/Header-2/Identifi-ionList-3b06{0}",
        ] {
            compound.create_storage(storage).unwrap();
        }
        let name: Vec<u8> = "MetaDictionary-1"
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut root_properties = vec![0x4c, 32, 1, 0, 1, 0, 0x22, 0];
        root_properties.extend_from_slice(&(name.len() as u16).to_le_bytes());
        root_properties.extend_from_slice(&name);
        compound
            .create_stream("/properties")
            .unwrap()
            .write_all(&root_properties)
            .unwrap();
        compound
            .create_stream("/MetaDictionary-1/properties")
            .unwrap()
            .write_all(&[0x4c, 32, 0, 0])
            .unwrap();
        compound
            .create_stream("/referenced properties")
            .unwrap()
            .write_all(&[0x4c, 0, 0, 0, 0, 0, 0])
            .unwrap();
        compound.flush().unwrap();
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(8)).unwrap();
    file.write_all(&AAF_V4_HEADER_CLSID_LE).unwrap();
    drop(file);

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:#?}");
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "aaf");
    assert_eq!(audit["passed"], false);
    assert_eq!(
        audit["properties"]["method"],
        "forge-aaf-effect-profiles-metadictionary-object-model-edit-protocol-v3"
    );
    let failures: Vec<_> = audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .filter(|check| check["passed"] == false)
        .map(|check| check["rule_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        failures,
        [
            "FORGE-AAF-METADICTIONARY-DEFINITIONS",
            "FORGE-AAF-EXTENSION-PROPERTY-TYPES"
        ]
    );

    let schema: Value =
        serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&audit),
        "{:?}",
        validator.validate(&audit)
    );
}
