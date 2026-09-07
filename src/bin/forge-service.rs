//! Bounded HTTP analysis service.

use clap::Parser;
use forge_normalizer::service::{
    self, ScopedServiceToken, ServiceConfig, ServiceRuntimeLimits, ServiceScope, ServiceSecurity,
};
#[cfg(feature = "grpc-service")]
use forge_normalizer::service_grpc;
use forge_normalizer::service_metrics::{JsonlSpanRecorder, ServiceMetrics};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "forge-service",
    version,
    about = "Forge's bounded stateless HTTP audio analysis service"
)]
struct Args {
    /// HTTP listen address. Non-loopback binds require a scoped token and a
    /// trusted TLS-terminating proxy (or a future in-process TLS mode).
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Maximum upload body size in MiB.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(usize))]
    max_body_mib: usize,

    /// Maximum decoded samples (frames multiplied by channels) per request.
    #[arg(long, default_value_t = 100_000_000, value_parser = clap::value_parser!(u64))]
    max_decoded_samples: u64,

    /// Process-wide admitted analysis working-set quota in MiB. By default it
    /// is derived from workers, body/sample limits, and fixed DSP allowances.
    #[arg(long, value_parser = clap::value_parser!(u64))]
    memory_quota_mib: Option<u64>,

    /// Process-wide active upload-spool quota in MiB. By default every worker
    /// can retain one --max-body-mib upload.
    #[arg(long, value_parser = clap::value_parser!(u64))]
    temp_quota_mib: Option<u64>,

    /// Maximum number of in-flight requests.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(usize))]
    workers: usize,

    /// Read and write timeout in milliseconds.
    #[arg(long, default_value_t = 30_000, value_parser = clap::value_parser!(u64))]
    timeout_ms: u64,

    /// Environment variable containing the legacy all-scope bearer token.
    /// Empty/unset means unauthenticated loopback-only mode.
    #[arg(long, default_value = "FORGE_SERVICE_BEARER_TOKEN")]
    auth_token_env: String,

    /// Exact peer IP of a TLS-terminating trusted proxy. Repeatable for a
    /// small explicit allowlist; required for non-loopback binds.
    #[arg(long = "trusted-proxy-ip", value_name = "IP")]
    trusted_proxy_ip: Vec<IpAddr>,

    /// Scoped token environment mapping in the form `SCOPES=ENV`, repeatable.
    /// SCOPES is a comma-separated list of analyze, cancel, health, metrics.
    #[arg(long = "auth-scoped-token-env", value_name = "SCOPES=ENV")]
    auth_scoped_token_env: Vec<String>,

    /// Start the gRPC endpoint instead of the REST endpoint. Requires the
    /// grpc-service Cargo feature and uses the same limits and auth policy.
    #[arg(long)]
    grpc_bind: Option<SocketAddr>,

    /// Expose Prometheus metrics at GET /metrics (REST) or the Metrics RPC.
    #[arg(long)]
    metrics: bool,

    /// Append bounded OpenTelemetry-compatible request spans as JSONL.
    #[arg(long, value_name = "PATH")]
    otel_jsonl: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let token = match std::env::var(&args.auth_token_env) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) | Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("legacy bearer token environment variable is not valid UTF-8");
            return ExitCode::from(2);
        }
    };
    let max_body_bytes = match args.max_body_mib.checked_mul(1024 * 1024) {
        Some(value) => value,
        None => {
            eprintln!("--max-body-mib is too large");
            return ExitCode::from(2);
        }
    };
    let selected_bind = args.grpc_bind.unwrap_or(args.bind);
    let config = ServiceConfig {
        bind: selected_bind,
        max_body_bytes,
        max_decoded_samples: args.max_decoded_samples,
        workers: args.workers,
        timeout: Duration::from_millis(args.timeout_ms),
        bearer_token: token,
    };
    let security = match build_security(&args.trusted_proxy_ip, &args.auth_scoped_token_env) {
        Ok(security) => security,
        Err(error) => {
            eprintln!("invalid service security: {error}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = validate_security_before_start(&config, &security) {
        eprintln!("invalid service security: {error}");
        return ExitCode::from(2);
    }
    let default_limits = match ServiceRuntimeLimits::for_config(&config) {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("invalid service resource limits: {error}");
            return ExitCode::from(2);
        }
    };
    let quota_bytes = |name: &str, mib: Option<u64>, default: u64| -> Result<u64, String> {
        mib.map_or(Ok(default), |mib| {
            mib.checked_mul(1024 * 1024)
                .ok_or_else(|| format!("--{name}-quota-mib is too large"))
        })
    };
    let memory_quota = match quota_bytes(
        "memory",
        args.memory_quota_mib,
        default_limits.memory_capacity_bytes(),
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let temp_quota = match quota_bytes(
        "temp",
        args.temp_quota_mib,
        default_limits.temporary_storage_capacity_bytes(),
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let runtime_limits = match ServiceRuntimeLimits::new(memory_quota, temp_quota) {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("invalid service resource limits: {error}");
            return ExitCode::from(2);
        }
    };
    let metrics = if args.metrics || args.otel_jsonl.is_some() {
        let metrics = ServiceMetrics::new();
        if let Some(path) = args.otel_jsonl.as_ref() {
            let recorder = match JsonlSpanRecorder::from_path(path) {
                Ok(recorder) => recorder,
                Err(error) => {
                    eprintln!("could not open --otel-jsonl {}: {error}", path.display());
                    return ExitCode::from(2);
                }
            };
            Some(metrics.with_span_recorder(Arc::new(recorder)))
        } else {
            Some(metrics)
        }
    } else {
        None
    };
    if let Some(bind) = args.grpc_bind {
        #[cfg(feature = "grpc-service")]
        {
            eprintln!("forge-service gRPC listening on {bind}");
            let result = match metrics {
                Some(metrics) => service_grpc::run_with_security_metrics_and_runtime_limits(
                    config,
                    bind,
                    security,
                    metrics,
                    runtime_limits,
                ),
                None => service_grpc::run_with_security_and_runtime_limits(
                    config,
                    bind,
                    security,
                    runtime_limits,
                ),
            };
            return match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("forge-service gRPC failed: {error}");
                    ExitCode::from(1)
                }
            };
        }
        #[cfg(not(feature = "grpc-service"))]
        {
            let _ = bind;
            let _ = metrics;
            eprintln!("--grpc-bind requires building with --features grpc-service");
            return ExitCode::from(2);
        }
    }
    eprintln!("forge-service listening on {}", config.bind);
    let result = match metrics {
        Some(metrics) => service::run_with_security_metrics_and_runtime_limits(
            config,
            security,
            metrics,
            runtime_limits,
        ),
        None => service::run_with_security_and_runtime_limits(config, security, runtime_limits),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forge-service failed: {error}");
            ExitCode::from(1)
        }
    }
}

fn parse_scopes(value: &str) -> Result<Vec<ServiceScope>, String> {
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err("scope list contains empty or whitespace-delimited entries".into());
    }
    let mut scopes = Vec::new();
    for name in value.split(',') {
        let scope = match name {
            "analyze" => ServiceScope::Analyze,
            "cancel" => ServiceScope::Cancel,
            "health" => ServiceScope::Health,
            "metrics" => ServiceScope::Metrics,
            "" => return Err("scope list contains an empty entry".into()),
            _ => return Err("scope list contains an unknown scope".into()),
        };
        if scopes.contains(&scope) {
            return Err("scope list contains a duplicate scope".into());
        }
        scopes.push(scope);
    }
    if scopes.is_empty() {
        Err("at least one scope is required".into())
    } else {
        Ok(scopes)
    }
}

fn parse_scoped_mapping(mapping: &str) -> Result<(&str, &str), String> {
    let mut pieces = mapping.split('=');
    let scope_text = pieces
        .next()
        .ok_or_else(|| "scoped token mapping is malformed".to_string())?;
    let env_name = pieces
        .next()
        .ok_or_else(|| "scoped token mapping is malformed".to_string())?;
    if pieces.next().is_some()
        || scope_text.is_empty()
        || env_name.is_empty()
        || env_name
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == 0)
    {
        return Err("scoped token mapping is malformed".into());
    }
    // parse_scopes rejects all whitespace in the scope portion, including
    // whitespace around the equals sign.
    let _ = parse_scopes(scope_text)?;
    Ok((scope_text, env_name))
}

fn build_security(
    trusted_proxy_ips: &[IpAddr],
    scoped_mappings: &[String],
) -> Result<ServiceSecurity, String> {
    let mut security = ServiceSecurity::new();
    let mut seen_mappings: Vec<(&str, &str)> = Vec::new();
    for &ip in trusted_proxy_ips {
        security = security.with_trusted_proxy_ip(ip);
    }
    for mapping in scoped_mappings {
        let (scope_text, env_name) = parse_scoped_mapping(mapping)?;
        if seen_mappings
            .iter()
            .any(|&(seen_scope, seen_env)| seen_scope == scope_text && seen_env == env_name)
        {
            return Err("scoped token mapping is duplicated".into());
        }
        seen_mappings.push((scope_text, env_name));
        let scopes = parse_scopes(scope_text)?;
        let secret = std::env::var(env_name).map_err(|error| match error {
            std::env::VarError::NotPresent => {
                "scoped token environment variable is not set".to_string()
            }
            std::env::VarError::NotUnicode(_) => {
                "scoped token environment variable is not valid UTF-8".to_string()
            }
        })?;
        if secret.is_empty() {
            return Err("scoped token environment variable is empty".into());
        }
        let token = ScopedServiceToken::new(secret.as_bytes(), scopes)
            .map_err(|_| "scoped token is invalid".to_string())?;
        security = security
            .with_token(token)
            .map_err(|_| "scoped service security has too many tokens".to_string())?;
    }
    Ok(security)
}

fn validate_security_before_start(
    config: &ServiceConfig,
    security: &ServiceSecurity,
) -> Result<(), String> {
    security.validate_for_config(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_repeatable_security_options() {
        let args = Args::try_parse_from([
            "forge-service",
            "--trusted-proxy-ip",
            "127.0.0.1",
            "--trusted-proxy-ip",
            "::1",
            "--auth-scoped-token-env",
            "analyze,health=FORGE_ANALYZE_TOKEN",
            "--auth-scoped-token-env",
            "metrics=FORGE_METRICS_TOKEN",
        ])
        .unwrap();
        assert_eq!(args.trusted_proxy_ip.len(), 2);
        assert_eq!(args.auth_scoped_token_env.len(), 2);
    }

    #[test]
    fn scope_parser_fails_closed_on_empty_whitespace_duplicate_and_unknown() {
        for value in [
            "",
            ",",
            "analyze,",
            ",health",
            "analyze,,health",
            "analyze,analyze",
            "analyze, health",
            " analyze",
            "analyze ",
            "analyze,unknown",
        ] {
            assert!(parse_scopes(value).is_err(), "{value:?} should be rejected");
        }
        assert_eq!(
            parse_scopes("analyze,health").unwrap(),
            vec![ServiceScope::Analyze, ServiceScope::Health]
        );
    }

    #[test]
    fn scoped_mapping_rejects_empty_whitespace_and_extra_equals() {
        for value in [
            "=TOKEN",
            "analyze=",
            "analyze,,health=TOKEN",
            "analyze, health=TOKEN",
            "analyze =TOKEN",
            "analyze= TOKEN",
            "analyze=TOKEN=OTHER",
        ] {
            assert!(
                parse_scoped_mapping(value).is_err(),
                "{value:?} should be rejected"
            );
        }
        assert_eq!(
            parse_scoped_mapping("analyze,health=FORGE_TOKEN").unwrap(),
            ("analyze,health", "FORGE_TOKEN")
        );
        let duplicate = vec![
            "analyze=FORGE_TOKEN".to_owned(),
            "analyze=FORGE_TOKEN".to_owned(),
        ];
        assert!(build_security(&[], &duplicate).is_err());
    }

    #[test]
    fn scoped_missing_environment_and_debug_never_expose_secret_values() {
        let env_name = "FORGE_SERVICE_TEST_MISSING_7B8A0C1E";
        let error = build_security(&[], &[format!("health={env_name}")]).unwrap_err();
        assert!(!error.contains(env_name));
        assert!(!error.contains("super-secret"));

        let token = ScopedServiceToken::all("super-secret").unwrap();
        let security = ServiceSecurity::new().with_token(token).unwrap();
        let debug = format!("{security:?}");
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn common_security_preflight_covers_rest_and_grpc_bind_modes() {
        let config = ServiceConfig {
            bind: "0.0.0.0:8080".parse().unwrap(),
            bearer_token: None,
            ..ServiceConfig::default()
        };
        let empty = ServiceSecurity::new();
        assert!(validate_security_before_start(&config, &empty).is_err());

        let token = ScopedServiceToken::all("service-secret").unwrap();
        let token_only = ServiceSecurity::new().with_token(token.clone()).unwrap();
        assert!(validate_security_before_start(&config, &token_only).is_err());

        let shared = token_only.with_trusted_proxy_ip("127.0.0.1".parse().unwrap());
        assert!(validate_security_before_start(&config, &shared).is_ok());
        // Both endpoint modes select the same config/security preflight before
        // their respective run_with_security call, including distinct ports.
        for bind in ["0.0.0.0:8080", "0.0.0.0:50051"] {
            let config = ServiceConfig {
                bind: bind.parse().unwrap(),
                bearer_token: None,
                ..ServiceConfig::default()
            };
            assert!(validate_security_before_start(&config, &shared).is_ok());
        }
    }
}
