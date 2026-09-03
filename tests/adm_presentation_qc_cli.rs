#[cfg(unix)]
use forge_normalizer::analysis;
#[cfg(unix)]
use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
};
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn exposes_exhaustive_and_bounded_adm_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-presentation-qc"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for option in [
        "--renderer",
        "--layout",
        "--output",
        "--timeout-seconds",
        "--max-presentations",
        "--max-decoded-samples",
        "--loudness-tolerance-lu",
        "--true-peak-tolerance-db",
        "--retain-renders",
        "--overwrite",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[cfg(unix)]
#[test]
fn renders_and_measures_every_complementary_presentation() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("programme.bw64");
    let template = work.path().join("reference.wav");
    let renderer = work.path().join("ear-render");
    let invocations = work.path().join("invocations.txt");
    let report = work.path().join("report.json");
    let retained = work.path().join("renders");

    let input_audio = sine();
    let rendered_audio = stereo_sine();
    WavWriter::write(&template, &rendered_audio, PcmKind::F32, false).unwrap();
    let measured = analysis::analyze(&rendered_audio);
    write_adm(&input, &input_audio, measured.lufs, measured.true_peak_db());
    let script = format!(
        r#"#!/bin/sh
set -eu
programme=
objects=
input=
output=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -s) shift 2 ;;
    --programme) programme=$2; shift 2 ;;
    --comp-object) objects="${{objects}}${{objects:+,}}$2"; shift 2 ;;
    *)
      if [ -z "$input" ]; then input=$1; else output=$1; fi
      shift
      ;;
  esac
done
test -f "$input"
printf '%s|%s\n' "$programme" "$objects" >> '{invocations}'
cp '{template}' "$output"
"#,
        invocations = invocations.display(),
        template = template.display(),
    );
    std::fs::write(&renderer, script).unwrap();
    std::fs::set_permissions(&renderer, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-presentation-qc"))
        .arg(&input)
        .args(["--renderer", renderer.to_str().unwrap()])
        .args(["--output", report.to_str().unwrap()])
        .args(["--retain-renders", retained.to_str().unwrap()])
        .output()
        .unwrap();
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(
        output.status.success(),
        "{}\n{instance:#}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(instance["passed"], true);
    assert_eq!(instance["presentation_count"], 3);
    assert_eq!(instance["inventory"]["programme_count"], 1);
    assert_eq!(
        instance["inventory"]["programmes"][0]["presentation_count"],
        3
    );
    assert_eq!(
        instance["presentations"][0]["selected_complementary_object_ids"][0],
        "AO_1001"
    );
    assert_eq!(
        instance["presentations"][2]["selected_complementary_object_ids"][0],
        "AO_1003"
    );
    assert_eq!(
        instance["presentations"][0]["loudness_metadata_passed"],
        true
    );
    assert_eq!(
        instance["presentations"][0]["true_peak_metadata_passed"],
        true
    );
    for key in ["input_sha256", "renderer_sha256"] {
        assert_eq!(instance[key].as_str().unwrap().len(), 64);
    }
    assert_eq!(std::fs::read_dir(&retained).unwrap().count(), 3);
    assert_eq!(
        std::fs::read_to_string(&invocations).unwrap(),
        "APR_1001|AO_1001\nAPR_1001|AO_1002\nAPR_1001|AO_1003\n"
    );
    validate_schema(
        &instance,
        include_str!("../schema/adm-presentation-report-v1.schema.json"),
    );
}

#[cfg(unix)]
#[test]
fn refuses_combinatorial_expansion_without_starting_renderer() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("programme.bw64");
    let renderer = work.path().join("must-not-run");
    let marker = work.path().join("started");
    let report = work.path().join("report.json");
    let audio = sine();
    let measured = analysis::analyze(&audio);
    write_adm(&input, &audio, measured.lufs, measured.true_peak_db());
    std::fs::write(
        &renderer,
        format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&renderer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-presentation-qc"))
        .arg(&input)
        .args(["--renderer", renderer.to_str().unwrap()])
        .args(["--output", report.to_str().unwrap()])
        .args(["--max-presentations", "2"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("no renderer was started"));
    assert!(!marker.exists());
    assert!(!report.exists());
}

#[cfg(unix)]
fn sine() -> AudioBuffer {
    let samples = (0..48_000)
        .map(|frame| 0.1 * (std::f32::consts::TAU * 997.0 * frame as f32 / 48_000.0).sin())
        .collect::<Vec<_>>();
    AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 48_000,
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    }
}

#[cfg(unix)]
fn stereo_sine() -> AudioBuffer {
    let mono = sine();
    AudioBuffer {
        sample_rate: mono.sample_rate,
        channels: 2,
        frames: mono.frames,
        data: vec![mono.data[0].clone(), mono.data[0].clone()],
        channel_roles: default_channel_roles(2),
        source_kind: mono.source_kind,
    }
}

#[cfg(unix)]
fn write_adm(input: &std::path::Path, audio: &AudioBuffer, lufs: f64, peak: f64) {
    let axml = format!(
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList><profile profileName="EBU Production Profile" profileVersion="1.0" profileLevel="1">EBU Tech 3393</profile></profileList>
  <tagList><tagGroup><tag class="urn:profile:production:Layer">1</tag><audioProgrammeIDRef>APR_1001</audioProgrammeIDRef></tagGroup></tagList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Main" audioProgrammeLanguage="en">
    <audioContentIDRef>ACO_1001</audioContentIDRef>
    <loudnessMetadata loudnessMethod="ITU-R BS.1770-5"><integratedLoudness>{lufs:.8}</integratedLoudness><maxTruePeak>{peak:.8}</maxTruePeak></loudnessMetadata>
  </audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Content"><audioObjectIDRef>AO_1001</audioObjectIDRef><dialogue>1</dialogue></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="English" importance="10" interact="1">
    <audioComplementaryObjectIDRef>AO_1002</audioComplementaryObjectIDRef><audioComplementaryObjectIDRef>AO_1003</audioComplementaryObjectIDRef>
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef>
  </audioObject>
  <audioObject audioObjectID="AO_1002" audioObjectName="French" importance="10" interact="1"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef></audioObject>
  <audioObject audioObjectID="AO_1003" audioObjectName="German" importance="10" interact="1"><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef></audioObject>
  <audioTrackUID UID="ATU_00000001"><audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef></audioTrackUID>
</audioFormatExtended>"#
    );
    let mut chna = Vec::with_capacity(44);
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(b"ATU_00000001");
    chna.extend_from_slice(&[0; 14]);
    chna.extend_from_slice(&[0; 11]);
    chna.push(0);
    WavWriter::write_with_metadata(
        input,
        audio,
        PcmKind::F32,
        false,
        WavContainer::Bw64,
        &[
            WaveChunk {
                id: *b"axml",
                body: axml.into_bytes(),
            },
            WaveChunk {
                id: *b"chna",
                body: chna,
            },
        ],
    )
    .unwrap();
}

#[cfg(unix)]
fn validate_schema(instance: &Value, schema: &str) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}
