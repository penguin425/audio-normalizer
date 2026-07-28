mod common;

use forge_normalizer::normalize::Analysis;
use forge_normalizer::report::{self, AnalysisReport};
use forge_normalizer::wav::{ChannelRole, PcmKind, WavContainer, WaveChunk};
use serde_json::{json, Value};
use std::process::Command;

#[test]
fn emitted_ebu_qc_conforms_to_published_schema() {
    let audio = forge_normalizer::wav::AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 48_000,
        data: vec![vec![0.1; 48_000], vec![0.1; 48_000]],
        channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    let analysis = forge_normalizer::normalize::analyze(&audio);
    let results = forge_normalizer::qc::analyze(
        &audio,
        &analysis,
        &forge_normalizer::qc::QcOptions::default(),
    );
    let instance = json!({
        "schema": forge_normalizer::qc::QC_SCHEMA,
        "results": results
    });
    let schema: Value =
        serde_json::from_str(include_str!("../schema/ebu-qc-results-v2.schema.json"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_delivery_manifest_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.wav");
    let audio = forge_normalizer::wav::AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 100,
        data: vec![vec![0.0; 100], vec![0.0; 100]],
        channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    forge_normalizer::wav::WavWriter::write_with_metadata(
        &path,
        &audio,
        PcmKind::S16,
        false,
        WavContainer::Riff,
        &[WaveChunk {
            id: *b"axml",
            body: b"<metadata/>".to_vec(),
        }],
    )
    .unwrap();
    let analysis = Analysis {
        sample_rate: 48_000,
        channels: 2,
        channel_roles: vec![ChannelRole::Main, ChannelRole::Main],
        frames: 480_000,
        kind: PcmKind::S16,
        lufs: -23.0,
        max_momentary_lufs: -22.5,
        max_short_term_lufs: -22.8,
        loudness_range_lu: 5.0,
        rms_db: -24.0,
        sample_peak: 0.2,
        true_peak: 0.25,
        loudness_blocks: vec![],
    };
    let reports = [AnalysisReport::new(&path, &analysis)];
    let mut output = Vec::new();
    report::write_manifest(&mut output, &reports).expect("manifest serialization");
    let instance: Value = serde_json::from_slice(&output).expect("manifest JSON");
    let schema: Value =
        serde_json::from_str(include_str!("../schema/delivery-manifest-v3.schema.json"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
    assert_eq!(instance["assets"][0]["container_qc"]["passed"], true);
}

#[test]
fn emitted_comparison_conforms_to_published_schema() {
    let manifest = serde_json::to_vec(&json!({
        "schema": "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2",
        "assets": [{
            "path": "programme.wav",
            "integrated_lufs": -23.0,
            "true_peak_dbtp": -1.2
        }]
    }))
    .unwrap();
    let comparison = forge_normalizer::compare::compare_manifests(
        &manifest,
        &manifest,
        &forge_normalizer::compare::CompareOptions::default(),
    )
    .unwrap();
    let instance = serde_json::to_value(comparison).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/manifest-comparison-v1.schema.json"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_container_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.wav");
    let audio = forge_normalizer::wav::AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 100,
        data: vec![vec![0.0; 100]],
        channel_roles: vec![ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    forge_normalizer::wav::WavWriter::write_with_metadata(
        &path,
        &audio,
        PcmKind::S16,
        false,
        WavContainer::Riff,
        &[WaveChunk {
            id: *b"axml",
            body: b"<metadata/>".to_vec(),
        }],
    )
    .unwrap();
    let instance =
        serde_json::to_value(forge_normalizer::container_qc::audit(&path).unwrap()).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_webm_container_qc_conforms_to_published_schema() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.webm");
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
        .arg(&path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "webm");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_aac_container_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.aac");
    std::fs::write(&path, common::HE_AAC_ADTS).unwrap();
    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "aac-adts");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_mpegts_container_qc_conforms_to_published_schema() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.ts");
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
            "aac",
            "-f",
            "mpegts",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");
    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "mpegts");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_eac3_container_qc_conforms_to_published_schema() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.eac3");
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
            "eac3",
            "-dialnorm",
            "-27",
            "-f",
            "eac3",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");
    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "eac3");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_iamf_container_qc_conforms_to_published_schema() {
    fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![obu_type << 3, payload.len() as u8];
        bytes.extend_from_slice(payload);
        bytes
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("presentation.iamf");
    let mut bytes = obu(31, b"iamf\x00\x00");
    bytes.extend(obu(0, &[0]));
    bytes.extend(obu(1, &[0]));
    bytes.extend(obu(2, &[0]));
    bytes.extend(obu(6, &[0]));
    std::fs::write(&path, bytes).unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "iamf");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_additional_pcm_container_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.au");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b".snd");
    bytes.extend_from_slice(&24_u32.to_be_bytes());
    bytes.extend_from_slice(&4_u32.to_be_bytes());
    bytes.extend_from_slice(&3_u32.to_be_bytes());
    bytes.extend_from_slice(&48_000_u32.to_be_bytes());
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 1, 0, 2]);
    std::fs::write(&path, bytes).unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "au");
    assert!(audit.passed);
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_flac_container_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.flac");
    let mut writer =
        forge_normalizer::flacenc::FlacStreamWriter::create(&path, 48_000, 2, 16, false).unwrap();
    writer
        .write_chunk(&[vec![0.1; 1_000], vec![-0.1; 1_000]])
        .unwrap();
    writer.finish().unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "flac");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_mp3_container_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.mp3");
    let mut bytes = Vec::new();
    for padded in [false, true, false] {
        let mut header = [0xff, 0xfb, 0x00, 0x00];
        if padded {
            header[2] |= 0x02;
        }
        bytes.extend_from_slice(&header);
        bytes.resize(bytes.len() + 60 + usize::from(padded), 0);
    }
    std::fs::write(&path, bytes).unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "mp3");
    assert!(audit.passed, "{audit:#?}");
    assert_eq!(audit.properties["free_format"], true);
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_default_ogg_opus_qc_conforms_to_published_schema() {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    use std::io::Write;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.opus");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = PacketWriter::new(std::io::BufWriter::new(file));
    let mut head = b"OpusHead\x01\x01".to_vec();
    head.extend_from_slice(&0_u16.to_le_bytes());
    head.extend_from_slice(&48_000_u32.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&4_u32.to_le_bytes());
    tags.extend_from_slice(b"test");
    tags.extend_from_slice(&0_u32.to_le_bytes());
    writer
        .write_packet(head, 42, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(tags, 42, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(vec![0], 42, PacketWriteEndInfo::EndStream, 480)
        .unwrap();
    writer.into_inner().flush().unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert!(audit.passed, "{audit:#?}");
    assert_eq!(audit.format, "ogg-opus");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_ambisonic_ogg_opus_qc_conforms_to_published_schema() {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    use std::io::Write;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ambisonic.opus");
    let file = std::fs::File::create(&path).unwrap();
    let mut writer = PacketWriter::new(std::io::BufWriter::new(file));
    let mut head = b"OpusHead\x01\x04".to_vec();
    head.extend_from_slice(&0_u16.to_le_bytes());
    head.extend_from_slice(&48_000_u32.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.extend_from_slice(&[2, 2, 2, 0, 1, 2, 3]);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&4_u32.to_le_bytes());
    tags.extend_from_slice(b"test");
    tags.extend_from_slice(&0_u32.to_le_bytes());
    writer
        .write_packet(head, 84, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(tags, 84, PacketWriteEndInfo::EndPage, 0)
        .unwrap();
    writer
        .write_packet(vec![0], 84, PacketWriteEndInfo::EndStream, 480)
        .unwrap();
    writer.into_inner().flush().unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert!(audit.passed, "{audit:#?}");
    assert_eq!(
        audit.properties["chains"][0]["ambisonics"]["normalization"],
        "SN3D"
    );
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_isobmff_container_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("truncated.m4a");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&100_u32.to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(b"M4A ");
    std::fs::write(&path, bytes).unwrap();

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "isobmff");
    assert!(!audit.passed);
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_mxf_container_qc_conforms_to_published_schema() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success())
    {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.mxf");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x72:rate=25:duration=0.1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.1",
            "-c:v",
            "mpeg2video",
            "-pix_fmt",
            "yuv422p",
            "-c:a",
            "pcm_s24le",
            "-f",
            "mxf",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{generated:#?}");

    let audit = forge_normalizer::container_qc::audit(&path).unwrap();
    assert_eq!(audit.format, "mxf");
    assert!(audit.passed, "{audit:#?}");
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_provenance_qc_conforms_to_published_schema() {
    use forge_normalizer::provenance::{
        ProvenanceAudit, ValidationPolicy, VerifierEvidence, PROVENANCE_QC_SCHEMA,
    };
    let audit = ProvenanceAudit {
        schema: PROVENANCE_QC_SCHEMA,
        generator: "forge-normalizer/test",
        path: "signed.wav".into(),
        passed: true,
        policy: ValidationPolicy::Integrity,
        manifest_present: true,
        integrity_valid: true,
        trusted: false,
        validation_state: Some("Valid".into()),
        active_manifest: Some("urn:uuid:active".into()),
        manifest_count: 1,
        verifier: VerifierEvidence {
            implementation: "contentauth/c2patool",
            version: "c2patool 0.26.59".into(),
            executable: "c2patool".into(),
            trust_anchors_configured: false,
            allowed_list_configured: false,
            trust_config_configured: false,
            external_manifest: false,
        },
        validation_status: vec![json!({"code": "signingCredential.untrusted"})],
        report: Some(json!({"validation_state": "Valid"})),
    };
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/provenance-qc-v1.schema.json"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_hls_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audio.m3u8");
    std::fs::write(
        &path,
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n\
         #EXTINF:6,\na.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("a.ts"), []).unwrap();

    let audit =
        forge_normalizer::hls_qc::audit(&path, forge_normalizer::hls_qc::HlsProfile::Rfc8216)
            .unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/hls-qc-v1.schema.json")).expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_low_latency_hls_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("live.m3u8");
    std::fs::write(
        &path,
        "#EXTM3U\n\
         #EXT-X-VERSION:9\n\
         #EXT-X-TARGETDURATION:2\n\
         #EXT-X-PART-INF:PART-TARGET=0.5\n\
         #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.5\n\
         #EXT-X-PROGRAM-DATE-TIME:2026-07-29T00:00:00.000Z\n\
         #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/0.0.m4s\"\n\
         #EXTINF:0.5,\n\
         https://example.invalid/0.ts\n\
         #EXT-X-PART:DURATION=0.5,INDEPENDENT=YES,URI=\"https://example.invalid/1.0.m4s\"\n\
         #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"https://example.invalid/1.1.m4s\"\n",
    )
    .unwrap();

    let audit = forge_normalizer::hls_qc::audit(&path, forge_normalizer::hls_qc::HlsProfile::LlHls)
        .unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/hls-qc-v1.schema.json")).expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_dash_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audio.mpd");
    std::fs::write(
        &path,
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"
 mediaPresentationDuration="PT2S" minBufferTime="PT1S">
 <Period id="p0"><AdaptationSet contentType="audio" mimeType="audio/mp4"
 codecs="mp4a.40.2" audioSamplingRate="48000">
  <SegmentTemplate timescale="48000" duration="96000"
   initialization="init-$RepresentationID$.mp4" media="$RepresentationID$-$Number$.m4s"/>
  <Representation id="a1" bandwidth="128000"/>
 </AdaptationSet></Period></MPD>"#,
    )
    .unwrap();

    let audit =
        forge_normalizer::dash_qc::audit(&path, forge_normalizer::dash_qc::DashProfile::Iso23009)
            .unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/dash-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_dash_live_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("live.mpd");
    std::fs::write(
        &path,
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic"
 availabilityStartTime="2026-07-29T00:00:00Z"
 minimumUpdatePeriod="PT2S" minBufferTime="PT1S">
 <UTCTiming schemeIdUri="urn:mpeg:dash:utc:direct:2014"
  value="2026-07-29T00:00:10Z"/>
 <Period id="p0" start="PT0S" duration="PT10S">
  <AdaptationSet id="1" contentType="audio" mimeType="audio/mp4" codecs="opus">
   <BaseURL>https://example.invalid/</BaseURL>
   <SegmentTemplate timescale="48000" duration="96000"
    initialization="init-$RepresentationID$.mp4"
    media="$RepresentationID$-$Number$.m4s"/>
   <Representation id="a1" bandwidth="96000"/>
  </AdaptationSet>
 </Period>
</MPD>"#,
    )
    .unwrap();

    let audit =
        forge_normalizer::dash_qc::audit(&path, forge_normalizer::dash_qc::DashProfile::DashLive)
            .unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/dash-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_imf_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("ASSETMAP"),
        r#"<AssetMap xmlns="http://www.smpte-ra.org/schemas/429-9/2007/AM"><AssetList/></AssetMap>"#,
    )
    .unwrap();
    let audit = forge_normalizer::imf_qc::audit(directory.path()).unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/imf-qc-v1.schema.json")).expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_rtp_audio_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stream.sdp");
    std::fs::write(
        &path,
        "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.1\r\n\
s=Schema test\r\n\
c=IN IP4 239.1.2.3/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 96\r\n\
a=rtpmap:96 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n",
    )
    .unwrap();
    let audit = forge_normalizer::rtp_qc::audit(
        &path,
        None,
        forge_normalizer::rtp_qc::RtpAudioProfile::Aes67,
    )
    .unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/rtp-audio-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn emitted_nmos_qc_conforms_to_published_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.json");
    std::fs::write(&path, br#"{"nodes":[],"devices":[]}"#).unwrap();
    let audit = forge_normalizer::nmos_qc::audit(&path).unwrap();
    let instance = serde_json::to_value(audit).unwrap();
    let schema: Value = serde_json::from_str(include_str!("../schema/nmos-qc-v1.schema.json"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let errors: Vec<_> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}
