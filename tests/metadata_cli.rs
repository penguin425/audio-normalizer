use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
};
use lofty::config::WriteOptions;
use lofty::tag::{Accessor, Tag, TagExt, TagType};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Output};

fn write_test_wave(path: &Path, metadata: &[WaveChunk]) {
    let sample_rate = 48_000_u32;
    let frames = 24_000_usize;
    let samples = (0..frames)
        .map(|frame| {
            (0.25 * (std::f64::consts::TAU * 440.0 * frame as f64 / sample_rate as f64).sin())
                as f32
        })
        .collect::<Vec<_>>();
    let buffer = AudioBuffer {
        sample_rate,
        channels: 1,
        frames,
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write_with_metadata(
        path,
        &buffer,
        PcmKind::F32,
        false,
        WavContainer::Riff,
        metadata,
    )
    .unwrap();
}

fn write_test_flac(path: &Path) {
    let sample_rate = 48_000_u32;
    let samples = (0..sample_rate as usize)
        .map(|frame| {
            (0.25 * (std::f64::consts::TAU * 440.0 * frame as f64 / sample_rate as f64).sin())
                as f32
        })
        .collect::<Vec<_>>();
    let mut writer =
        forge_normalizer::flacenc::FlacStreamWriter::create(path, sample_rate, 1, 24, false)
            .unwrap();
    writer.write_chunk(&[samples]).unwrap();
    writer.finish().unwrap();
}

fn write_test_flac_with_title(path: &Path) {
    write_test_flac(path);
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.set_title("retain this title".into());
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

fn bext_body(time_reference: u64) -> Vec<u8> {
    let mut body = vec![0_u8; 602];
    body[338..346].copy_from_slice(&time_reference.to_le_bytes());
    body
}

fn cue_body(position: u32, sample_offset: u32) -> Vec<u8> {
    let mut body = vec![0_u8; 4 + 24];
    body[..4].copy_from_slice(&1_u32.to_le_bytes());
    body[4..8].copy_from_slice(&1_u32.to_le_bytes());
    body[8..12].copy_from_slice(&position.to_le_bytes());
    body[12..16].copy_from_slice(b"data");
    body[16..20].copy_from_slice(&0_u32.to_le_bytes());
    body[20..24].copy_from_slice(&0_u32.to_le_bytes());
    body[24..28].copy_from_slice(&sample_offset.to_le_bytes());
    body
}

fn smpl_body(sample_period: u32, loop_start: u32, loop_end: u32) -> Vec<u8> {
    let mut body = vec![0_u8; 36 + 24];
    body[8..12].copy_from_slice(&sample_period.to_le_bytes());
    body[28..32].copy_from_slice(&1_u32.to_le_bytes());
    body[32..36].copy_from_slice(&0_u32.to_le_bytes());
    body[44..48].copy_from_slice(&loop_start.to_le_bytes());
    body[48..52].copy_from_slice(&loop_end.to_le_bytes());
    body
}

fn timing_metadata() -> Vec<WaveChunk> {
    vec![
        WaveChunk {
            id: *b"bext",
            body: bext_body(4_800),
        },
        WaveChunk {
            id: *b"cue ",
            body: cue_body(2_400, 1_200),
        },
        WaveChunk {
            id: *b"smpl",
            body: smpl_body(20_833, 1_200, 3_600),
        },
    ]
}

fn wave_chunks(bytes: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    assert!(bytes.len() >= 12);
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut offset = 12_usize;
    let mut chunks = Vec::new();
    while offset < bytes.len() {
        assert!(bytes.len() - offset >= 8, "truncated WAVE chunk header");
        let id: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
        let body_len =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let body_start = offset + 8;
        let body_end = body_start + body_len;
        let end = body_end + (body_len & 1);
        assert!(end <= bytes.len(), "WAVE chunk exceeds file");
        chunks.push((id, bytes[body_start..body_end].to_vec()));
        offset = end;
    }
    chunks
}

fn run_forge<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "forge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_json_schema(instance_path: &Path, schema_text: &str) -> Value {
    let instance: Value = serde_json::from_slice(&std::fs::read(instance_path).unwrap()).unwrap();
    let schema: Value = serde_json::from_str(schema_text).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&instance),
        "schema-invalid JSON: {instance}"
    );
    instance
}

#[test]
fn wave_preserve_report_is_schema_valid_and_maps_sample_clock_on_resample() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.wav");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&input, &timing_metadata());

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("preserve"),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
        OsStr::new("--sample-rate"),
        OsStr::new("44100"),
    ]);
    assert_success(&result);
    assert!(output.is_file());
    assert!(report.is_file());

    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["schema_version"], 1);
    assert_eq!(report_value["policy"]["policy"], "preserve");
    assert_eq!(
        report_value["evidence"]["registry_revision"],
        "metadata-registry-v1"
    );
    assert_eq!(
        report_value["evidence"]["timing_revision"],
        "sample-time-transform-v1"
    );
    assert_eq!(
        report_value["evidence"]["sample_time_transform"]["source_rate_hz"],
        48_000
    );
    assert_eq!(
        report_value["evidence"]["sample_time_transform"]["output_rate_hz"],
        44_100
    );
    assert_eq!(
        report_value["evidence"]["sample_time_transform"]["crop_origin_source_frames"],
        "0"
    );
    assert_eq!(
        report_value["evidence"]["sample_time_transform"]["rounding"],
        "half-up"
    );
    let fields = report_value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["field"].as_str())
        .collect::<Vec<_>>();
    for field in [
        "wave_timing.bext.TimeReference",
        "wave_timing.cue .cue[0].dwPosition",
        "wave_timing.cue .cue[0].dwSampleOffset",
        "wave_timing.smpl.loop[0].dwStart",
        "wave_timing.smpl.loop[0].dwEnd",
    ] {
        assert!(
            fields.contains(&field),
            "missing timing ledger field {field}"
        );
    }

    let chunks = wave_chunks(&std::fs::read(&output).unwrap());
    let bext = &chunks
        .iter()
        .find(|(id, _)| id == b"bext")
        .expect("preserved bext")
        .1;
    assert_eq!(
        u64::from_le_bytes(bext[338..346].try_into().unwrap()),
        4_410
    );
    let cue = &chunks
        .iter()
        .find(|(id, _)| id == b"cue ")
        .expect("preserved cue")
        .1;
    assert_eq!(u32::from_le_bytes(cue[8..12].try_into().unwrap()), 2_205);
    assert_eq!(u32::from_le_bytes(cue[24..28].try_into().unwrap()), 1_103);
    let smpl = &chunks
        .iter()
        .find(|(id, _)| id == b"smpl")
        .expect("preserved smpl")
        .1;
    assert_eq!(u32::from_le_bytes(smpl[8..12].try_into().unwrap()), 22_675);
    assert_eq!(u32::from_le_bytes(smpl[44..48].try_into().unwrap()), 1_103);
    assert_eq!(u32::from_le_bytes(smpl[48..52].try_into().unwrap()), 3_308);

    let decoded = forge_normalizer::wav::WavReader::open(&output).unwrap();
    assert_eq!(decoded.sample_rate, 44_100);
    assert!(decoded.frames > 20_000 && decoded.frames < 23_000);
}

#[test]
fn strict_unknown_wave_metadata_fails_before_output_creation() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.wav");
    write_test_wave(
        &input,
        &[WaveChunk {
            id: *b"JUNK",
            body: b"unregistered metadata".to_vec(),
        }],
    );

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
    ]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("no semantic adapter"),
        "unexpected error: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!output.exists(), "strict preflight created an output");
}

#[test]
fn strip_all_drops_source_wave_metadata_from_normalized_output() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.wav");
    write_test_wave(
        &input,
        &[
            WaveChunk {
                id: *b"JUNK",
                body: b"drop this".to_vec(),
            },
            WaveChunk {
                id: *b"bext",
                body: bext_body(4_800),
            },
        ],
    );

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("strip"),
    ]);
    assert_success(&result);
    let input_ids = wave_chunks(&std::fs::read(&input).unwrap())
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    assert!(input_ids.contains(b"JUNK"));
    assert!(input_ids.contains(b"bext"));
    let output_ids = wave_chunks(&std::fs::read(&output).unwrap())
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    assert!(!output_ids.contains(b"JUNK"));
    assert!(!output_ids.contains(b"bext"));
    assert!(output_ids.contains(b"fmt "));
    assert!(output_ids.contains(b"data"));
}

#[test]
fn strict_wave_to_flac_accepts_only_exact_writer_generated_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.flac");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&input, &[]);

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
    ]);
    assert_success(&result);

    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["publication"]["status"], "allowed");
    let entries = report_value["entries"].as_array().unwrap();
    assert!(entries.iter().any(|entry| {
        entry["field"]
            .as_str()
            .is_some_and(|field| field.contains("\"block_type\":4"))
            && entry["outcome"] == "recomputed"
            && entry["loss_class"] == "none"
    }));
    assert!(entries.iter().any(|entry| {
        entry["field"]
            .as_str()
            .is_some_and(|field| field.contains("\"block_type\":1"))
            && entry["outcome"] == "recomputed"
            && entry["loss_class"] == "none"
    }));
    assert!(entries.iter().all(|entry| entry["loss_class"] == "none"));
}

#[cfg(feature = "ffmpeg-encoding")]
#[test]
fn strict_wave_to_vorbis_accepts_the_exact_generated_comment_packet() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.ogg");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&input, &[]);

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--format"),
        OsStr::new("vorbis"),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
    ]);
    assert_success(&result);
    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["publication"]["status"], "allowed");
    let entries = report_value["entries"].as_array().unwrap();
    assert!(entries.iter().any(|entry| {
        entry["field"]
            .as_str()
            .is_some_and(|field| field.contains("vorbis_comment"))
            && entry["outcome"] == "recomputed"
            && entry["loss_class"] == "none"
    }));
    assert!(entries.iter().all(|entry| entry["loss_class"] == "none"));
}

#[cfg(feature = "opus-encoding")]
#[test]
fn strict_wave_to_opus_binds_the_exact_generated_tags_packet() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.opus");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&input, &[]);

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
    ]);
    assert_success(&result);
    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["publication"]["status"], "allowed");
    let entries = report_value["entries"].as_array().unwrap();
    assert!(entries.iter().any(|entry| {
        entry["field"]
            .as_str()
            .is_some_and(|field| field.contains("opus_tags"))
            && entry["outcome"] == "recomputed"
            && entry["loss_class"] == "none"
    }));
    assert!(entries.iter().all(|entry| entry["loss_class"] == "none"));
}

#[cfg(feature = "ffmpeg-encoding")]
#[test]
fn strict_wave_to_m4a_binds_writer_children_and_container_ancestors() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.m4a");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&input, &[]);

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
    ]);
    assert_success(&result);
    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["publication"]["status"], "allowed");
    let entries = report_value["entries"].as_array().unwrap();
    for id in ["\"id\":\"udta\"", "\"id\":\"meta\"", "\"id\":\"ilst\""] {
        assert!(entries.iter().any(|entry| {
            entry["field"]
                .as_str()
                .is_some_and(|field| field.contains(id))
                && entry["loss_class"] == "none"
        }));
    }
    assert!(entries.iter().all(|entry| entry["loss_class"] == "none"));
}

#[test]
fn explicit_metadata_policy_is_bound_into_batch_operation_descriptor() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    let output = directory.path().join("normalized");
    let state = directory.path().join("batch-job.json");
    write_test_wave(&first, &[]);
    write_test_wave(&second, &[]);

    let result = run_forge([
        first.as_os_str(),
        second.as_os_str(),
        OsStr::new("-o"),
        output.as_os_str(),
        OsStr::new("--job-state"),
        state.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("preserve"),
    ]);
    assert_success(&result);

    let state_value: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(
        state_value["operation"]["metadata_policy"]["policy"],
        "preserve"
    );
    assert_eq!(
        state_value["operation"]["metadata_policy"]["strip_scope"],
        "none"
    );
    assert_eq!(
        state_value["operation"]["metadata_registry_revision"],
        "metadata-registry-v1"
    );
    assert_eq!(
        state_value["operation"]["metadata_timing_revision"],
        "sample-time-transform-v1"
    );
    assert_eq!(
        state_value["operation"]["metadata_fidelity_schema_version"],
        1
    );
}

#[test]
fn explicit_metadata_policy_is_bound_into_watch_operation_descriptor() {
    let directory = tempfile::tempdir().unwrap();
    let input_directory = directory.path().join("input");
    let output_directory = directory.path().join("output");
    let state = directory.path().join("watch.json");
    std::fs::create_dir(&input_directory).unwrap();
    write_test_wave(&input_directory.join("tone.wav"), &[]);

    let result = run_forge([
        input_directory.as_os_str(),
        OsStr::new("--watch"),
        OsStr::new("--watch-once"),
        OsStr::new("--watch-state"),
        state.as_os_str(),
        OsStr::new("--watch-stable-seconds"),
        OsStr::new("1"),
        OsStr::new("-o"),
        output_directory.as_os_str(),
        OsStr::new("--metadata-policy"),
        OsStr::new("preserve"),
    ]);
    assert_success(&result);

    let state_value: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(
        state_value["operation"]["metadata_policy"]["policy"],
        "preserve"
    );
    assert_eq!(
        state_value["operation"]["metadata_policy"]["strip_scope"],
        "none"
    );
    assert_eq!(
        state_value["operation"]["metadata_registry_revision"],
        "metadata-registry-v1"
    );
    assert_eq!(
        state_value["operation"]["metadata_timing_revision"],
        "sample-time-transform-v1"
    );
    assert_eq!(
        state_value["operation"]["metadata_fidelity_schema_version"],
        1
    );
}

#[test]
fn metadata_write_transaction_commit_and_resume_are_idempotent_and_schema_valid() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let state = directory.path().join("metadata-job.json");
    write_test_wave(&input, &[]);

    let first = run_forge([
        input.as_os_str(),
        OsStr::new("--write-tags"),
        OsStr::new("--metadata-job-state"),
        state.as_os_str(),
    ]);
    assert_success(&first);
    assert!(String::from_utf8_lossy(&first.stderr).contains("metadata transaction committed"));
    let after_first = std::fs::read(&input).unwrap();
    let state_after_first = std::fs::read(&state).unwrap();
    let state_value = assert_json_schema(
        &state,
        include_str!("../schema/metadata-job-v1.schema.json"),
    );
    assert_eq!(state_value["phase"], "committed");
    assert_eq!(state_value["policy"], "legacy_generic");
    assert_eq!(
        state_value["operation"]["metadata_timing_revision"],
        "sample-time-transform-v1"
    );
    assert!(state_value["output"]["sha256"].is_string());
    assert!(state_value["mutation"].is_object());
    assert!(state_value["verification"].is_object());
    assert!(state_value["verification"]["audio_round_trip"]["loudness_block_count"].is_u64());
    assert_eq!(
        state_value["verification"]["audio_round_trip"]["loudness_blocks_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(state_value["verification"]["audio_round_trip"]
        .get("loudness_blocks_bits")
        .is_none());

    let second = run_forge([
        input.as_os_str(),
        OsStr::new("--write-tags"),
        OsStr::new("--metadata-job-state"),
        state.as_os_str(),
    ]);
    assert_success(&second);
    assert!(String::from_utf8_lossy(&second.stderr).contains("metadata transaction committed"));
    assert_eq!(std::fs::read(&input).unwrap(), after_first);
    assert_eq!(std::fs::read(&state).unwrap(), state_after_first);
    let resumed = assert_json_schema(
        &state,
        include_str!("../schema/metadata-job-v1.schema.json"),
    );
    assert_eq!(resumed, state_value);
}

#[test]
fn strict_metadata_transaction_does_not_bless_or_publish_a_changed_composite_tag() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.flac");
    let state = directory.path().join("metadata-job.json");
    write_test_flac_with_title(&input);
    let original = std::fs::read(&input).unwrap();

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("--write-tags"),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
        OsStr::new("--metadata-job-state"),
        state.as_os_str(),
    ]);
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("metadata publication blocked"),
        "unexpected error: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(&input).unwrap(), original);
    let state_value = assert_json_schema(
        &state,
        include_str!("../schema/metadata-job-v1.schema.json"),
    );
    assert_eq!(state_value["phase"], "prepared");
}

#[test]
fn strict_metadata_transaction_accepts_an_exact_new_flac_comment() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.flac");
    let state = directory.path().join("metadata-job.json");
    let report = directory.path().join("metadata-report.json");
    write_test_flac(&input);
    let original = std::fs::read(&input).unwrap();

    let args = || {
        [
            input.as_os_str(),
            OsStr::new("--write-tags"),
            OsStr::new("--metadata-policy"),
            OsStr::new("strict"),
            OsStr::new("--metadata-job-state"),
            state.as_os_str(),
            OsStr::new("--metadata-report"),
            report.as_os_str(),
        ]
    };
    let first = run_forge(args());
    assert_success(&first);
    assert_ne!(std::fs::read(&input).unwrap(), original);
    let state_value = assert_json_schema(
        &state,
        include_str!("../schema/metadata-job-v1.schema.json"),
    );
    assert_eq!(state_value["phase"], "committed");
    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["publication"]["status"], "allowed");
    assert!(report_value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| { entry["loss_class"] == "none" }));

    let committed_audio = std::fs::read(&input).unwrap();
    let committed_state = std::fs::read(&state).unwrap();
    let committed_report = std::fs::read(&report).unwrap();
    let second = run_forge(args());
    assert_success(&second);
    assert_eq!(std::fs::read(&input).unwrap(), committed_audio);
    assert_eq!(std::fs::read(&state).unwrap(), committed_state);
    assert_eq!(std::fs::read(&report).unwrap(), committed_report);
}

#[cfg(feature = "ffmpeg-encoding")]
#[test]
fn preserve_metadata_transaction_rechecks_sound_check_after_all_m4a_writers() {
    let directory = tempfile::tempdir().unwrap();
    let wave = directory.path().join("input.wav");
    let input = directory.path().join("input.m4a");
    let state = directory.path().join("metadata-job.json");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&wave, &[]);
    assert_success(&run_forge([
        wave.as_os_str(),
        OsStr::new("-o"),
        input.as_os_str(),
    ]));

    let result = run_forge([
        input.as_os_str(),
        OsStr::new("--write-tags"),
        OsStr::new("--sound-check"),
        OsStr::new("--metadata-policy"),
        OsStr::new("preserve"),
        OsStr::new("--metadata-job-state"),
        state.as_os_str(),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
    ]);
    assert_success(&result);
    assert!(forge_normalizer::metadata::read_sound_check(&input)
        .unwrap()
        .is_some());
    let state_value = assert_json_schema(
        &state,
        include_str!("../schema/metadata-job-v1.schema.json"),
    );
    assert_eq!(state_value["phase"], "committed");
    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["publication"]["status"], "allowed");
}

#[test]
fn committed_metadata_transaction_recognizes_and_republishes_its_exact_report() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let state = directory.path().join("metadata-job.json");
    let report = directory.path().join("metadata-report.json");
    write_test_wave(&input, &[]);

    let args = || {
        [
            input.as_os_str(),
            OsStr::new("--write-tags"),
            OsStr::new("--metadata-policy"),
            OsStr::new("preserve"),
            OsStr::new("--metadata-job-state"),
            state.as_os_str(),
            OsStr::new("--metadata-report"),
            report.as_os_str(),
        ]
    };
    let first = run_forge(args());
    assert_success(&first);
    let committed_audio = std::fs::read(&input).unwrap();
    let committed_state = std::fs::read(&state).unwrap();
    let expected_report = std::fs::read(&report).unwrap();
    let report_value = assert_json_schema(
        &report,
        include_str!("../schema/metadata-fidelity-report-v1.schema.json"),
    );
    assert_eq!(report_value["policy"]["policy"], "preserve");
    assert_eq!(report_value["publication"]["status"], "allowed");

    // This is the observable state after a crash immediately following the
    // report rename: both destinations are complete, but the caller did not
    // receive success. A resume must recognize exact deterministic bytes even
    // without --overwrite.
    let second = run_forge(args());
    assert_success(&second);
    assert_eq!(std::fs::read(&input).unwrap(), committed_audio);
    assert_eq!(std::fs::read(&state).unwrap(), committed_state);
    assert_eq!(std::fs::read(&report).unwrap(), expected_report);

    // The committed job retains the validated report in verification
    // evidence, so it can repair the audio/report publication gap without
    // repeating the metadata mutation.
    std::fs::remove_file(&report).unwrap();
    let third = run_forge(args());
    assert_success(&third);
    assert_eq!(std::fs::read(&input).unwrap(), committed_audio);
    assert_eq!(std::fs::read(&state).unwrap(), committed_state);
    assert_eq!(std::fs::read(&report).unwrap(), expected_report);

    std::fs::write(&report, b"different report\n").unwrap();
    let fourth = run_forge(args());
    assert!(!fourth.status.success());
    assert!(String::from_utf8_lossy(&fourth.stderr).contains("already exists"));
    assert_eq!(std::fs::read(&input).unwrap(), committed_audio);
    assert_eq!(std::fs::read(&state).unwrap(), committed_state);
    assert_eq!(std::fs::read(&report).unwrap(), b"different report\n");
}

#[test]
fn committed_metadata_transaction_rejects_tampered_audio_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let state = directory.path().join("metadata-job.json");
    write_test_wave(&input, &[]);

    let args = || {
        [
            input.as_os_str(),
            OsStr::new("--write-tags"),
            OsStr::new("--metadata-job-state"),
            state.as_os_str(),
        ]
    };
    let first = run_forge(args());
    assert_success(&first);
    let committed_audio = std::fs::read(&input).unwrap();

    let mut document: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    let frames = document["verification"]["audio_round_trip"]["frames"]
        .as_u64()
        .unwrap();
    document["verification"]["audio_round_trip"]["frames"] = Value::from(frames + 1);
    std::fs::write(&state, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let resumed = run_forge(args());
    assert!(!resumed.status.success());
    assert!(
        String::from_utf8_lossy(&resumed.stderr)
            .contains("audio round-trip evidence does not match"),
        "unexpected resume error: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read(&input).unwrap(), committed_audio);
}

#[test]
fn metadata_cli_conflicts_and_release_boundary_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let report = directory.path().join("report.json");
    let state = directory.path().join("state.json");
    write_test_wave(&input, &[]);

    let analyze_policy = run_forge([
        input.as_os_str(),
        OsStr::new("--analyze"),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
    ]);
    assert_eq!(analyze_policy.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&analyze_policy.stderr).contains("cannot be used with"));

    let report_without_policy = run_forge([
        input.as_os_str(),
        OsStr::new("--metadata-report"),
        report.as_os_str(),
    ]);
    assert_eq!(report_without_policy.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&report_without_policy.stderr).contains("--metadata-policy"));

    let locator_without_policy = run_forge([
        input.as_os_str(),
        OsStr::new("--metadata-strip-locator"),
        OsStr::new("wave:[\"JUNK\"]/#0"),
    ]);
    assert_eq!(locator_without_policy.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&locator_without_policy.stderr).contains("--metadata-policy"));

    let state_without_tags = run_forge([
        input.as_os_str(),
        OsStr::new("--metadata-job-state"),
        state.as_os_str(),
    ]);
    assert_eq!(state_without_tags.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&state_without_tags.stderr).contains("--write-tags"));

    let album_policy = run_forge([
        input.as_os_str(),
        OsStr::new("--album"),
        OsStr::new("--metadata-policy"),
        OsStr::new("preserve"),
    ]);
    assert!(!album_policy.status.success());
    assert!(
        String::from_utf8_lossy(&album_policy.stderr)
            .contains("deferred to the generation transaction in v0.189.15"),
        "unexpected album policy error: {}",
        String::from_utf8_lossy(&album_policy.stderr)
    );

    let tags_policy_without_state = run_forge([
        input.as_os_str(),
        OsStr::new("--write-tags"),
        OsStr::new("--metadata-policy"),
        OsStr::new("strict"),
    ]);
    assert!(!tags_policy_without_state.status.success());
    assert!(String::from_utf8_lossy(&tags_policy_without_state.stderr)
        .contains("requires --metadata-job-state"));
}

#[test]
fn config_enabled_analysis_rejects_metadata_and_batch_controls() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let config = directory.path().join("analysis.toml");
    let report = directory.path().join("report.json");
    let job_state = directory.path().join("job.json");
    let progress = directory.path().join("progress.ndjson");
    write_test_wave(&input, &[]);
    std::fs::write(&config, "[analysis]\nenabled = true\n").unwrap();

    let run = |extra: Vec<OsString>| {
        let mut args = vec![
            input.as_os_str().to_os_string(),
            OsString::from("--config"),
            config.as_os_str().to_os_string(),
        ];
        args.extend(extra);
        run_forge(args)
    };
    let assert_conflict = |result: Output| {
        assert_eq!(
            result.status.code(),
            Some(2),
            "unexpected status: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("cannot be used with '--analyze'"),
            "unexpected error: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };

    assert_conflict(run(vec![
        OsString::from("--metadata-policy"),
        OsString::from("strict"),
        OsString::from("--metadata-report"),
        report.as_os_str().to_os_string(),
    ]));
    assert_conflict(run(vec![
        OsString::from("--job-state"),
        job_state.as_os_str().to_os_string(),
    ]));
    assert_conflict(run(vec![
        OsString::from("--progress"),
        progress.as_os_str().to_os_string(),
    ]));
    assert!(!report.exists());
    assert!(!job_state.exists());
    assert!(!progress.exists());
}

#[test]
fn config_enabled_analysis_rejects_watch_and_normalization_modes() {
    let directory = tempfile::tempdir().unwrap();
    let input_directory = directory.path().join("input");
    let output_directory = directory.path().join("output");
    let state = directory.path().join("watch.json");
    let config = directory.path().join("analysis.toml");
    std::fs::create_dir(&input_directory).unwrap();
    write_test_wave(&input_directory.join("input.wav"), &[]);
    std::fs::write(&config, "[analysis]\nenabled = true\n").unwrap();

    let run = |extra: Vec<OsString>| {
        let mut args = vec![
            input_directory.as_os_str().to_os_string(),
            OsString::from("--config"),
            config.as_os_str().to_os_string(),
        ];
        args.extend(extra);
        run_forge(args)
    };
    let assert_conflict = |result: Output| {
        assert_eq!(
            result.status.code(),
            Some(2),
            "unexpected status: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("cannot be used with '--analyze'"),
            "unexpected error: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    };

    assert_conflict(run(vec![
        OsString::from("--watch"),
        OsString::from("--watch-once"),
        OsString::from("--watch-state"),
        state.as_os_str().to_os_string(),
        OsString::from("--watch-stable-seconds"),
        OsString::from("1"),
        OsString::from("--output"),
        output_directory.as_os_str().to_os_string(),
    ]));
    assert_conflict(run(vec![OsString::from("--verify")]));
    assert_conflict(run(vec![
        OsString::from("--sample-rate"),
        OsString::from("44100"),
    ]));
    assert_conflict(run(vec![
        OsString::from("--preset"),
        OsString::from("spotify"),
    ]));
    assert_conflict(run(vec![OsString::from("--gain-only")]));
    assert_conflict(run(vec![OsString::from("--dry-run")]));
    assert_conflict(run(vec![OsString::from("--album")]));
    assert!(!state.exists());
    assert!(!output_directory.exists());
}
