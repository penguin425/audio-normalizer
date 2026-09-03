use forge_normalizer::metadata;
use forge_normalizer::wav::{AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_fixture(path: &std::path::Path) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 64,
        data: vec![vec![0.1; 64]],
        channel_roles: forge_normalizer::wav::default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write_with_metadata(
        path,
        &audio,
        PcmKind::F32,
        false,
        WavContainer::Riff,
        &[
            WaveChunk {
                id: *b"JUNK",
                body: vec![1, 2, 3, 4, 5],
            },
            WaveChunk {
                id: *b"bext",
                body: metadata::blank_bext(),
            },
        ],
    )
    .unwrap();
}

fn run_with_report(request: &Path, report: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forge-metadata-repair"))
        .arg("--output")
        .arg(report)
        .arg(request)
        .output()
        .unwrap()
}

fn request_bytes(source: &str, destination: &str) -> Vec<u8> {
    format!(
        r#"{{
          "schema_version": 1,
          "source": "{source}",
          "destination": "{destination}",
          "mode": "validate",
          "overwrite": true,
          "atomic_replace": true
        }}"#
    )
    .into_bytes()
}

fn assert_preflight_error(output: &Output, protected: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:#?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("report output aliases"), "{stderr}");
    assert!(stderr.contains(protected), "{stderr}");
}

#[test]
fn cli_repairs_bwf_into_atomic_copy_and_validates_report() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report_path = work.path().join("report.json");
    write_fixture(&source);
    fs::write(&report_path, b"pre-existing report bytes").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&report_path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let before = fs::read(&source).unwrap();
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "destination": "repaired.wav",
          "ensure_bwf_v2": true,
          "atomic_replace": true,
          "bwf_loudness": {
            "integrated_lufs": -23.0,
            "loudness_range_lu": 8.0,
            "true_peak_dbtp": -1.0,
            "max_momentary_lufs": -10.0,
            "max_short_term_lufs": -14.0
          }
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-metadata-repair"))
        .args([
            "--output",
            report_path.to_str().unwrap(),
            request.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert_eq!(before, fs::read(&source).unwrap());
    assert_ne!(before, fs::read(&destination).unwrap());
    assert_eq!(
        metadata::read_wave_chunk(&destination, *b"JUNK").unwrap(),
        Some(vec![1, 2, 3, 4, 5])
    );
    let report: Value = serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    let schema: Value = serde_json::from_str(include_str!(
        "../schema/metadata-repair-report-v1.schema.json"
    ))
    .unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&report));
    assert_eq!(report["validator"], "forge-metadata-repair-1");
    assert_eq!(report["source_format"], "wave");
    assert_eq!(report["changed"], true);
    assert_eq!(report["unknown_bytes_preserved"], true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&report_path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[test]
fn report_output_cannot_equal_repair_destination_and_preserves_every_file() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    fs::write(&destination, b"pre-existing repair destination").unwrap();
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let destination_before = fs::read(&destination).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &destination);
    assert_preflight_error(&output, "repair destination");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&destination).unwrap(), destination_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
}

#[test]
fn report_output_normalizes_parent_components_before_repair() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let subdirectory = work.path().join("sub");
    let report = subdirectory.join("..").join("repaired.wav");
    fs::create_dir(&subdirectory).unwrap();
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_preflight_error(&output, "repair destination");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn report_output_rejects_a_dangling_symlink_to_the_future_destination() {
    use std::os::unix::fs::symlink;

    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report = work.path().join("report.json");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    symlink("repaired.wav", &report).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_preflight_error(&output, "repair destination");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(fs::symlink_metadata(&report)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!destination.exists());
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn report_output_rejects_case_only_destination_alias() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("Repaired.wav");
    let request = work.path().join("request.json");
    let report = work.path().join("repaired.wav");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "Repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_preflight_error(&output, "repair destination");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
}

#[test]
fn report_output_cannot_equal_source_and_does_not_create_repair_output() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &source);
    assert_preflight_error(&output, "source");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
}

#[test]
fn report_output_cannot_equal_request_and_does_not_create_repair_output() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &request);
    assert_preflight_error(&output, "request file");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn report_output_rejects_a_source_symlink_alias_without_following_it() {
    use std::os::unix::fs::symlink;

    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report = work.path().join("report-link.json");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    symlink(&source, &report).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_preflight_error(&output, "source");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(fs::symlink_metadata(&report)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!destination.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn report_output_rejects_a_request_hardlink_alias_without_modifying_it() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report = work.path().join("report-hardlink.json");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    fs::hard_link(&request, &report).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_preflight_error(&output, "request file");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert_eq!(fs::read(&report).unwrap(), request_before);
    assert!(!destination.exists());
}

#[test]
fn report_output_rejects_an_explicit_decoded_reference_before_repair() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let reference = work.path().join("decoded.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    write_fixture(&reference);
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "destination": "repaired.wav",
          "atomic_replace": true,
          "isobmff_loudness": {
            "decoded_reference": "decoded.wav"
          }
        }"#,
    )
    .unwrap();
    let source_before = fs::read(&source).unwrap();
    let reference_before = fs::read(&reference).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &reference);
    assert_preflight_error(&output, "decoded reference");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&reference).unwrap(), reference_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
}

#[test]
fn report_output_rejects_an_album_decoded_reference_before_repair() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let reference = work.path().join("album.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    write_fixture(&reference);
    fs::write(
        &request,
        r#"{
          "schema_version": 2,
          "source": "source.wav",
          "destination": "repaired.wav",
          "atomic_replace": true,
          "isobmff_loudness": {
            "album_decoded_references": ["album.wav"]
          }
        }"#,
    )
    .unwrap();
    let source_before = fs::read(&source).unwrap();
    let reference_before = fs::read(&reference).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &reference);
    assert_preflight_error(&output, "album decoded reference 0");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&reference).unwrap(), reference_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
}

#[test]
fn repair_error_preserves_an_existing_report() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.bin");
    let destination = work.path().join("repaired.bin");
    let request = work.path().join("request.json");
    let report = work.path().join("report.json");
    fs::write(&source, b"not a supported container").unwrap();
    fs::write(&request, request_bytes("source.bin", "repaired.bin")).unwrap();
    fs::write(&report, b"pre-existing report").unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();
    let report_before = fs::read(&report).unwrap();

    let output = run_with_report(&request, &report);
    assert_eq!(output.status.code(), Some(2), "{output:#?}");
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert_eq!(fs::read(&report).unwrap(), report_before);
    assert!(!destination.exists());
}

#[test]
fn missing_report_parent_is_rejected_before_creating_repair_output() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report = work.path().join("missing").join("report.json");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_eq!(output.status.code(), Some(2), "{output:#?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("output directory does not exist"),
        "{output:#?}"
    );
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
    assert!(!report.exists());
}

#[cfg(unix)]
#[test]
fn report_output_rejects_a_fifo_without_opening_it() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;

    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report = work.path().join("report.fifo");
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let report_bytes = CString::new(report.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(report_bytes.as_ptr(), 0o600) }, 0);
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_eq!(output.status.code(), Some(2), "{output:#?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("report output is not a regular file"),
        "{output:#?}"
    );
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
    assert!(fs::symlink_metadata(&report).unwrap().file_type().is_fifo());
}

#[cfg(target_os = "linux")]
#[test]
fn unwritable_report_parent_is_rejected_before_creating_repair_output() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report = std::path::PathBuf::from(format!(
        "/proc/forge-metadata-repair-{}-report.json",
        std::process::id()
    ));
    write_fixture(&source);
    fs::write(&request, request_bytes("source.wav", "repaired.wav")).unwrap();
    let source_before = fs::read(&source).unwrap();
    let request_before = fs::read(&request).unwrap();

    let output = run_with_report(&request, &report);
    assert_eq!(output.status.code(), Some(2), "{output:#?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("create atomic metadata repair report"),
        "{output:#?}"
    );
    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(fs::read(&request).unwrap(), request_before);
    assert!(!destination.exists());
    assert!(!report.exists());
}

#[test]
fn validate_mode_copies_without_mutating_and_rejects_same_path() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("copy.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "destination": "copy.wav",
          "mode": "validate"
        }"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-metadata-repair"))
        .arg(&request)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert_eq!(fs::read(&source).unwrap(), fs::read(&destination).unwrap());
}
