//! C2PA Content Credentials validation through the official `c2patool`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::subprocess::{
    run, CompletedProcess, EnvPolicy, Error as ProcessError, ExecutableIdentity,
    MonitoredDirectory, OutputMode, ProcessSpec, StdinMode,
};

pub const PROVENANCE_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/provenance-qc-v1";
const PROVENANCE_WORKSPACE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const PROVENANCE_WORKSPACE_MAX_ENTRIES: u64 = 32;
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

pub fn audit(path: &Path, options: &ProvenanceOptions) -> Result<ProvenanceAudit, String> {
    let input_path = std::fs::canonicalize(path)
        .map_err(|error| format!("resolve provenance input {}: {error}", path.display()))?;
    if !input_path.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if options.max_report_bytes == 0 {
        return Err("max report bytes must be greater than zero".into());
    }
    if options.timeout.is_zero() {
        return Err("timeout must be greater than zero".into());
    }

    let tool_identity = ProcessSpec::new(&options.c2pa_tool)
        .map_err(|error| format!("start {}: {error}", options.c2pa_tool.display()))?;
    let work = tempfile::Builder::new()
        .prefix("forge-provenance-")
        .tempdir()
        .map_err(|error| format!("create provenance workspace: {error}"))?;
    let version_output = run_tool(
        tool_identity.executable(),
        &options.c2pa_tool,
        &["-V".into()],
        options.timeout,
        4096,
        work.path(),
    )?;
    if !version_output.success() {
        return Err(format!(
            "{} -V failed: {}",
            options.c2pa_tool.display(),
            display_stderr(version_output.stderr())
        ));
    }
    let version = String::from_utf8(version_output.stdout().to_vec())
        .map_err(|_| "c2patool version output is not UTF-8".to_string())?
        .trim()
        .to_string();
    if version.is_empty() {
        return Err("c2patool returned an empty version".into());
    }

    let mut args = vec![input_path.as_os_str().to_owned()];
    if let Some(external) = &options.external_manifest {
        let external = std::fs::canonicalize(external).map_err(|error| {
            format!("resolve external manifest {}: {error}", external.display())
        })?;
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
            args.push(resolve_trust_location(value, "trust anchors")?);
        }
        if let Some(value) = &options.allowed_list {
            args.push("--allowed_list".into());
            args.push(resolve_trust_location(value, "allowed list")?);
        }
        if let Some(value) = &options.trust_config {
            args.push("--trust_config".into());
            args.push(resolve_trust_location(value, "trust configuration")?);
        }
    }
    let output = run_tool(
        tool_identity.executable(),
        &options.c2pa_tool,
        &args,
        options.timeout,
        options.max_report_bytes,
        work.path(),
    )?;
    if output.stdout().is_empty() {
        let stderr = display_stderr(output.stderr());
        if stderr.to_ascii_lowercase().contains("no claim found") {
            return Ok(missing_manifest(path, options, version));
        }
    }
    // c2patool reports a missing claim as a non-zero, empty-stdout result.
    // Handle that explicit sentinel above, but never accept a non-zero result
    // merely because it also happened to contain parseable JSON.
    if !output.success() {
        return Err(format!(
            "{} audit failed ({}): {}",
            options.c2pa_tool.display(),
            output.status(),
            display_stderr(output.stderr())
        ));
    }
    if output.stdout().is_empty() {
        let stderr = display_stderr(output.stderr());
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
    let report: Value = serde_json::from_slice(output.stdout()).map_err(|error| {
        format!(
            "parse c2patool JSON (exit {}): {error}; stderr: {}",
            output.status(),
            display_stderr(output.stderr())
        )
    })?;
    Ok(evaluate_report(path, options, version, report))
}

fn resolve_trust_location(value: &str, label: &str) -> Result<OsString, String> {
    // c2patool accepts remote trust-list URLs as well as local paths. Remote
    // locations are independent of the helper cwd; bind local locations to
    // the caller's cwd before the helper enters its private workspace.
    if value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || value
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        return Ok(value.into());
    }
    let path = Path::new(value);
    let resolved = std::fs::canonicalize(path)
        .map_err(|error| format!("resolve {label} {}: {error}", path.display()))?;
    if !resolved.is_file() {
        return Err(format!(
            "{label} is not a regular file: {}",
            resolved.display()
        ));
    }
    Ok(resolved.into_os_string())
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

fn run_tool(
    identity: &ExecutableIdentity,
    executable: &Path,
    args: &[OsString],
    timeout: Duration,
    limit: usize,
    current_dir: &Path,
) -> Result<CompletedProcess, String> {
    let mut spec = ProcessSpec::from_executable(identity.clone());
    spec.args(args)
        .env_policy(EnvPolicy::Minimal)
        .stdin(StdinMode::Null)
        .stdout(OutputMode::capture(limit))
        .stderr(OutputMode::capture(limit.min(1024 * 1024)))
        .timeout(timeout)
        .current_dir(current_dir.to_path_buf())
        .monitor_directory(MonitoredDirectory::new(
            current_dir.to_path_buf(),
            PROVENANCE_WORKSPACE_MAX_BYTES,
            PROVENANCE_WORKSPACE_MAX_ENTRIES,
            "provenance workspace",
        ));
    run(spec).map_err(|error| match error {
        ProcessError::Spawn(error) => format!("start {}: {error}", executable.display()),
        ProcessError::Wait(error) => format!("wait for {}: {error}", executable.display()),
        ProcessError::OutputLimit { .. } => {
            format!("{} output exceeded its safety limit", executable.display())
        }
        ProcessError::TimedOut => format!(
            "{} exceeded the {} second timeout",
            executable.display(),
            timeout.as_secs_f64()
        ),
        error => format!("run {}: {error}", executable.display()),
    })
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

    #[test]
    fn remote_trust_locations_accept_case_insensitive_http_schemes() {
        for location in [
            "HTTPS://example.invalid/anchors",
            "Http://example.invalid/list",
        ] {
            assert_eq!(
                resolve_trust_location(location, "test").unwrap(),
                OsString::from(location)
            );
        }
    }
}
