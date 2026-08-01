//! Bounded HTTP analysis service.

use clap::Parser;
use forge_normalizer::service::{self, ServiceConfig};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "forge-service",
    version,
    about = "Forge's bounded stateless HTTP audio analysis service"
)]
struct Args {
    /// HTTP listen address. Non-loopback binds require a bearer token.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

    /// Maximum upload body size in MiB.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(usize))]
    max_body_mib: usize,

    /// Maximum decoded samples (frames multiplied by channels) per request.
    #[arg(long, default_value_t = 100_000_000, value_parser = clap::value_parser!(u64))]
    max_decoded_samples: u64,

    /// Maximum number of in-flight requests.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(usize))]
    workers: usize,

    /// Read and write timeout in milliseconds.
    #[arg(long, default_value_t = 30_000, value_parser = clap::value_parser!(u64))]
    timeout_ms: u64,

    /// Environment variable containing the bearer token. Empty/unset means
    /// unauthenticated loopback-only mode.
    #[arg(long, default_value = "FORGE_SERVICE_BEARER_TOKEN")]
    auth_token_env: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let token = match std::env::var(&args.auth_token_env) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) | Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            eprintln!("{} is not valid UTF-8", args.auth_token_env);
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
    let config = ServiceConfig {
        bind: args.bind,
        max_body_bytes,
        max_decoded_samples: args.max_decoded_samples,
        workers: args.workers,
        timeout: Duration::from_millis(args.timeout_ms),
        bearer_token: token,
    };
    if let Err(error) = config.validate() {
        eprintln!("invalid service configuration: {error}");
        return ExitCode::from(2);
    }
    eprintln!("forge-service listening on {}", config.bind);
    match service::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forge-service failed: {error}");
            ExitCode::from(1)
        }
    }
}
