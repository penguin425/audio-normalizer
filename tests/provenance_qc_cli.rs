#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn fake_tool(directory: &std::path::Path, state: &str) -> std::path::PathBuf {
    let path = directory.join("c2patool");
    let body = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"-V\" ]; then\n\
           echo 'c2patool test-1'\n\
           exit 0\n\
         fi\n\
         printf '%s\\n' '{{\"active_manifest\":\"active\",\"manifests\":{{\"active\":{{}}}},\"validation_state\":\"{state}\",\"validation_status\":[]}}'\n"
    );
    fs::write(&path, body).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn fake_missing_manifest_tool(directory: &std::path::Path) -> std::path::PathBuf {
    let path = directory.join("c2patool-missing");
    fs::write(
        &path,
        "#!/bin/sh\n\
         if [ \"$1\" = \"-V\" ]; then\n\
           echo 'c2patool test-1'\n\
           exit 0\n\
         fi\n\
         echo 'No claim found in asset' >&2\n\
         exit 1\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn fake_trust_path_tool(directory: &std::path::Path) -> std::path::PathBuf {
    let path = directory.join("c2patool-trust-paths");
    fs::write(
        &path,
        "#!/bin/sh\n\
         if [ \"$1\" = \"-V\" ]; then\n\
           echo 'c2patool test-1'\n\
           exit 0\n\
         fi\n\
         previous=''\n\
         for argument in \"$@\"; do\n\
           case \"$previous\" in\n\
             --trust_anchors|--allowed_list|--trust_config)\n\
               case \"$argument\" in /*) ;; *) exit 41 ;; esac\n\
               [ -f \"$argument\" ] || exit 42\n\
               ;;\n\
           esac\n\
           previous=\"$argument\"\n\
         done\n\
         printf '%s\\n' '{\"active_manifest\":\"active\",\"manifests\":{\"active\":{}},\"validation_state\":\"Trusted\",\"validation_status\":[]}'\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn provenance_cli_returns_policy_status_and_json() {
    let directory = tempfile::tempdir().unwrap();
    let asset = directory.path().join("asset.wav");
    fs::write(&asset, b"fixture").unwrap();
    let tool = fake_tool(directory.path(), "Valid");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-provenance-qc"))
        .arg(&asset)
        .arg("--c2pa-tool")
        .arg(&tool)
        .arg("--compact")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["integrity_valid"], true);
    assert_eq!(report["verifier"]["version"], "c2patool test-1");
}

#[test]
fn provenance_cli_rejects_invalid_hard_binding() {
    let directory = tempfile::tempdir().unwrap();
    let asset = directory.path().join("asset.wav");
    fs::write(&asset, b"fixture").unwrap();
    let tool = fake_tool(directory.path(), "Invalid");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-provenance-qc"))
        .arg(&asset)
        .arg("--c2pa-tool")
        .arg(&tool)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

#[test]
fn provenance_cli_treats_nonzero_no_claim_as_missing_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let asset = directory.path().join("asset.wav");
    fs::write(&asset, b"fixture").unwrap();
    let tool = fake_missing_manifest_tool(directory.path());
    let output = Command::new(env!("CARGO_BIN_EXE_forge-provenance-qc"))
        .arg(&asset)
        .arg("--c2pa-tool")
        .arg(&tool)
        .arg("--compact")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["manifest_present"], false);
    assert_eq!(report["integrity_valid"], false);
    assert_eq!(report["passed"], false);
}

#[test]
fn provenance_cli_resolves_relative_trust_files_before_private_cwd() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("asset.wav"), b"fixture").unwrap();
    for name in ["anchors.pem", "allowed.pem", "trust.cfg"] {
        fs::write(directory.path().join(name), b"fixture").unwrap();
    }
    let tool = fake_trust_path_tool(directory.path());
    let output = Command::new(env!("CARGO_BIN_EXE_forge-provenance-qc"))
        .current_dir(directory.path())
        .arg("asset.wav")
        .arg("--c2pa-tool")
        .arg(&tool)
        .args([
            "--trust-anchors",
            "anchors.pem",
            "--allowed-list",
            "allowed.pem",
            "--trust-config",
            "trust.cfg",
            "--policy",
            "trusted",
            "--compact",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["path"], "asset.wav");
}
