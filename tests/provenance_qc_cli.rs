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
