//! C2PA Content Credentials validation through the official `c2patool`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const PROVENANCE_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/provenance-qc-v1";
const DEFAULT_MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationPolicy {
    Integrity,
    Trusted,
}

#[derive(Debug, Clone)]
pub struct ProvenanceOptions {
    pub c2pa_tool: PathBuf,
    pub external_manifest: Option<PathBuf>,
    pub trust_anchors: Option<String>,
    pub allowed_list: Option<String>,
    pub trust_config: Option<String>,
    pub policy: ValidationPolicy,
    pub timeout: Duration,
    pub max_report_bytes: usize,
}

impl Default for ProvenanceOptions {
    fn default() -> Self {
        Self {
            c2pa_tool: PathBuf::from("c2patool"),
            external_manifest: None,
            trust_anchors: None,
            allowed_list: None,
            trust_config: None,
            policy: ValidationPolicy::Integrity,
            timeout: Duration::from_secs(60),
            max_report_bytes: DEFAULT_MAX_REPORT_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProvenanceAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub passed: bool,
    pub policy: ValidationPolicy,
    pub manifest_present: bool,
    pub integrity_valid: bool,
    pub trusted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_manifest: Option<String>,
    pub manifest_count: usize,
    pub verifier: VerifierEvidence,
    pub validation_status: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifierEvidence {
    pub implementation: &'static str,
    pub version: String,
    pub executable: String,
    pub trust_anchors_configured: bool,
    pub allowed_list_configured: bool,
    pub trust_config_configured: bool,
    pub external_manifest: bool,
}

struct ToolOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub fn audit(path: &Path, options: &ProvenanceOptions) -> Result<ProvenanceAudit, String> {
    if !path.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if options.max_report_bytes == 0 {
        return Err("max report bytes must be greater than zero".into());
    }
    if options.timeout.is_zero() {
        return Err("timeout must be greater than zero".into());
    }

    let version_output = run_bounded(&options.c2pa_tool, &["-V".into()], options.timeout, 4096)?;
    if !version_output.status.success() {
        return Err(format!(
            "{} -V failed: {}",
            options.c2pa_tool.display(),
            display_stderr(&version_output.stderr)
        ));
    }
    let version = String::from_utf8(version_output.stdout)
        .map_err(|_| "c2patool version output is not UTF-8".to_string())?
        .trim()
        .to_string();
    if version.is_empty() {
        return Err("c2patool returned an empty version".into());
    }

    let mut args = vec![path.as_os_str().to_owned()];
    if let Some(external) = &options.external_manifest {
        args.push("--external-manifest".into());
        args.push(external.as_os_str().to_owned());
    }
    let use_trust = options.trust_anchors.is_some()
        || options.allowed_list.is_some()
        || options.trust_config.is_some();
    if use_trust {
        args.push("trust".into());
        if let Some(value) = &options.trust_anchors {
            args.push("--trust_anchors".into());
            args.push(value.into());
        }
        if let Some(value) = &options.allowed_list {
            args.push("--allowed_list".into());
            args.push(value.into());
        }
        if let Some(value) = &options.trust_config {
            args.push("--trust_config".into());
            args.push(value.into());
        }
    }
    let output = run_bounded(
        &options.c2pa_tool,
        &args,
        options.timeout,
        options.max_report_bytes,
    )?;
    if output.stdout.is_empty() {
        let stderr = display_stderr(&output.stderr);
        if stderr.to_ascii_lowercase().contains("no claim found") {
            return Ok(missing_manifest(path, options, version));
        }
        return Err(format!(
            "{} produced no JSON report{}",
            options.c2pa_tool.display(),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ));
    }
    let report: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "parse c2patool JSON (exit {}): {error}; stderr: {}",
            output.status,
            display_stderr(&output.stderr)
        )
    })?;
    Ok(evaluate_report(path, options, version, report))
}

fn evaluate_report(
    path: &Path,
    options: &ProvenanceOptions,
    version: String,
    report: Value,
) -> ProvenanceAudit {
    let active_manifest = report
        .get("active_manifest")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let manifest_count = report
        .get("manifests")
        .and_then(Value::as_object)
        .map_or(0, serde_json::Map::len);
    let manifest_present = active_manifest.is_some() && manifest_count > 0;
    let validation_state = report
        .get("validation_state")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let validation_status = report
        .get("validation_status")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let fallback_integrity = validation_status.iter().all(trust_only_status);
    let integrity_valid = manifest_present
        && match validation_state.as_deref() {
            Some("Valid" | "Trusted") => true,
            Some("Invalid") => false,
            Some(_) => false,
            None => fallback_integrity,
        };
    let trust_configured = options.trust_anchors.is_some()
        || options.allowed_list.is_some()
        || options.trust_config.is_some();
    let trusted = manifest_present
        && match validation_state.as_deref() {
            Some("Trusted") => true,
            Some(_) => false,
            None => trust_configured && validation_status.is_empty(),
        };
    let passed = integrity_valid && (options.policy == ValidationPolicy::Integrity || trusted);

    ProvenanceAudit {
        schema: PROVENANCE_QC_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        path: path.to_string_lossy().into_owned(),
        passed,
        policy: options.policy,
        manifest_present,
        integrity_valid,
        trusted,
        validation_state,
        active_manifest,
        manifest_count,
        verifier: verifier(options, version),
        validation_status,
        report: Some(report),
    }
}

fn trust_only_status(status: &Value) -> bool {
    matches!(
        status.get("code").and_then(Value::as_str),
        Some("signingCredential.untrusted" | "timeStamp.untrusted")
    )
}

fn missing_manifest(path: &Path, options: &ProvenanceOptions, version: String) -> ProvenanceAudit {
    ProvenanceAudit {
        schema: PROVENANCE_QC_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        path: path.to_string_lossy().into_owned(),
        passed: false,
        policy: options.policy,
        manifest_present: false,
        integrity_valid: false,
        trusted: false,
        validation_state: None,
        active_manifest: None,
        manifest_count: 0,
        verifier: verifier(options, version),
        validation_status: Vec::new(),
        report: None,
    }
}

fn verifier(options: &ProvenanceOptions, version: String) -> VerifierEvidence {
    VerifierEvidence {
        implementation: "contentauth/c2patool",
        version,
        executable: options.c2pa_tool.to_string_lossy().into_owned(),
        trust_anchors_configured: options.trust_anchors.is_some(),
        allowed_list_configured: options.allowed_list.is_some(),
        trust_config_configured: options.trust_config.is_some(),
        external_manifest: options.external_manifest.is_some(),
    }
}

fn run_bounded(
    executable: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
    limit: usize,
) -> Result<ToolOutput, String> {
    let mut stdout_file =
        tempfile::tempfile().map_err(|error| format!("create stdout spool: {error}"))?;
    let mut stderr_file =
        tempfile::tempfile().map_err(|error| format!("create stderr spool: {error}"))?;
    let stdout_child = stdout_file
        .try_clone()
        .map_err(|error| format!("clone stdout spool: {error}"))?;
    let stderr_child = stderr_file
        .try_clone()
        .map_err(|error| format!("clone stderr spool: {error}"))?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_child))
        .stderr(Stdio::from(stderr_child))
        .spawn()
        .map_err(|error| format!("start {}: {error}", executable.display()))?;
    let started = Instant::now();
    let status = loop {
        let stdout_bytes = stdout_file
            .metadata()
            .map_err(|error| format!("stat stdout spool: {error}"))?
            .len();
        let stderr_limit = limit.min(1024 * 1024);
        let stderr_bytes = stderr_file
            .metadata()
            .map_err(|error| format!("stat stderr spool: {error}"))?
            .len();
        if stdout_bytes > u64::try_from(limit).unwrap_or(u64::MAX)
            || stderr_bytes > u64::try_from(stderr_limit).unwrap_or(u64::MAX)
        {
            child
                .kill()
                .map_err(|error| format!("terminate {}: {error}", executable.display()))?;
            let _ = child.wait();
            return Err(format!(
                "{} output exceeded its safety limit",
                executable.display()
            ));
        }
        match child
            .try_wait()
            .map_err(|error| format!("wait for {}: {error}", executable.display()))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                child
                    .kill()
                    .map_err(|error| format!("terminate {}: {error}", executable.display()))?;
                let _ = child.wait();
                return Err(format!(
                    "{} exceeded the {} second timeout",
                    executable.display(),
                    timeout.as_secs_f64()
                ));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = read_bounded(&mut stdout_file, limit, "stdout")?;
    let stderr = read_bounded(&mut stderr_file, limit.min(1024 * 1024), "stderr")?;
    Ok(ToolOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_bounded(file: &mut File, limit: usize, label: &str) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek {label} spool: {error}"))?;
    let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    file.take(take_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {label} spool: {error}"))?;
    if bytes.len() > limit {
        return Err(format!("{label} exceeds the {limit} byte safety limit"));
    }
    Ok(bytes)
}

fn display_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integrity_policy_accepts_valid_untrusted_claim() {
        let options = ProvenanceOptions::default();
        let audit = evaluate_report(
            Path::new("asset.wav"),
            &options,
            "c2patool 0.26.59".into(),
            json!({
                "active_manifest": "urn:uuid:active",
                "manifests": {"urn:uuid:active": {}},
                "validation_state": "Valid",
                "validation_status": [{"code": "signingCredential.untrusted"}]
            }),
        );
        assert!(audit.integrity_valid);
        assert!(!audit.trusted);
        assert!(audit.passed);
    }

    #[test]
    fn trusted_policy_rejects_untrusted_claim() {
        let options = ProvenanceOptions {
            policy: ValidationPolicy::Trusted,
            ..ProvenanceOptions::default()
        };
        let audit = evaluate_report(
            Path::new("asset.wav"),
            &options,
            "c2patool 0.26.59".into(),
            json!({
                "active_manifest": "urn:uuid:active",
                "manifests": {"urn:uuid:active": {}},
                "validation_state": "Valid",
                "validation_status": [{"code": "signingCredential.untrusted"}]
            }),
        );
        assert!(!audit.passed);
    }

    #[test]
    fn trusted_state_passes_trusted_policy() {
        let options = ProvenanceOptions {
            policy: ValidationPolicy::Trusted,
            trust_anchors: Some("anchors.pem".into()),
            ..ProvenanceOptions::default()
        };
        let audit = evaluate_report(
            Path::new("asset.wav"),
            &options,
            "c2patool 0.26.59".into(),
            json!({
                "active_manifest": "urn:uuid:active",
                "manifests": {"urn:uuid:active": {}},
                "validation_state": "Trusted",
                "validation_status": []
            }),
        );
        assert!(audit.integrity_valid);
        assert!(audit.trusted);
        assert!(audit.passed);
    }

    #[test]
    fn rejects_invalid_hard_binding_even_for_integrity_policy() {
        let options = ProvenanceOptions::default();
        let audit = evaluate_report(
            Path::new("asset.wav"),
            &options,
            "c2patool 0.26.59".into(),
            json!({
                "active_manifest": "urn:uuid:active",
                "manifests": {"urn:uuid:active": {}},
                "validation_state": "Invalid",
                "validation_status": [{"code": "assertion.dataHash.mismatch"}]
            }),
        );
        assert!(!audit.integrity_valid);
        assert!(!audit.passed);
    }

    #[test]
    fn older_report_fallback_only_ignores_explicit_trust_statuses() {
        let options = ProvenanceOptions::default();
        let valid = evaluate_report(
            Path::new("asset.wav"),
            &options,
            "old".into(),
            json!({
                "active_manifest": "active",
                "manifests": {"active": {}},
                "validation_status": [{"code": "signingCredential.untrusted"}]
            }),
        );
        assert!(valid.integrity_valid);
        let invalid = evaluate_report(
            Path::new("asset.wav"),
            &options,
            "old".into(),
            json!({
                "active_manifest": "active",
                "manifests": {"active": {}},
                "validation_status": [{"code": "claimSignature.mismatch"}]
            }),
        );
        assert!(!invalid.integrity_valid);
    }
}
