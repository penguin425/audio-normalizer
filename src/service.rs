//! Bounded HTTP service mode for stateless audio analysis.
//!
//! The service intentionally accepts audio bytes rather than filesystem paths.
//! This keeps a network caller from turning the normalizer into an arbitrary
//! file reader, and makes the same endpoint usable behind an object-store
//! worker or an upload gateway.  The HTTP parser is deliberately small and
//! bounded: chunked transfer encoding, keep-alive, implicit redirects, and
//! path-based inputs are not accepted.

use crate::analysis::AnalysisEngine;
use crate::channel_layout::ChannelLayoutDescriptor;
use crate::decoder;
use crate::report::{AnalysisReport, AnalysisReportWire, ComplianceProfile};
use crate::service_metrics::{RequestTimer, ServiceMetrics, PROMETHEUS_CONTENT_TYPE};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::Builder;
use url::Url;

pub const SERVICE_ANALYSIS_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/service-analysis-v1";
pub const SERVICE_ANALYSIS_SCHEMA_V1: &str = SERVICE_ANALYSIS_SCHEMA;
pub const SERVICE_ANALYSIS_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/service-analysis-v2";
pub const SERVICE_ANALYSIS_SCHEMA_V3: &str =
    "https://penguin425.github.io/audio-normalizer/schema/service-analysis-v3";
pub const SERVICE_ERROR_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/service-error-v1";
pub const SERVICE_HEALTH_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/service-health-v1";

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
const MAX_FILENAME_BYTES: usize = 256;
const MAX_ERROR_BYTES: usize = 512;
const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_DECODED_SAMPLES: u64 = 100_000_000;
const DEFAULT_WORKERS: usize = 4;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime limits and access policy for [`run`].
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub bind: SocketAddr,
    pub max_body_bytes: usize,
    pub max_decoded_samples: u64,
    pub workers: usize,
    pub timeout: Duration,
    /// When set, every endpoint requires `Authorization: Bearer <token>`.
    pub bearer_token: Option<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_decoded_samples: DEFAULT_MAX_DECODED_SAMPLES,
            workers: DEFAULT_WORKERS,
            timeout: DEFAULT_TIMEOUT,
            bearer_token: None,
        }
    }
}

impl ServiceConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_body_bytes == 0 || self.max_body_bytes > 512 * 1024 * 1024 {
            return Err("max_body_bytes must be between 1 and 536870912".into());
        }
        if self.max_decoded_samples == 0 || self.max_decoded_samples > 1_000_000_000 {
            return Err("max_decoded_samples must be between 1 and 1000000000".into());
        }
        if self.workers == 0 || self.workers > 256 {
            return Err("workers must be between 1 and 256".into());
        }
        if self.timeout < Duration::from_millis(100) || self.timeout > Duration::from_secs(120) {
            return Err("timeout must be between 100ms and 120s".into());
        }
        if self
            .bearer_token
            .as_ref()
            .is_some_and(|token| token.is_empty() || token.len() > MAX_HEADER_VALUE_BYTES)
        {
            return Err("bearer token must contain 1..=8192 bytes".into());
        }
        if !self.bind.ip().is_loopback() && self.bearer_token.is_none() {
            return Err("a bearer token is required when binding a non-loopback address".into());
        }
        Ok(())
    }
}

/// Start the service and accept connections until the listener fails.
pub fn run(config: ServiceConfig) -> io::Result<()> {
    run_internal(config, None)
}

/// Start the service with an optional shared metrics registry.
///
/// The plain [`run`] entry point remains unchanged for callers that do not
/// need observability.  This variant exposes the same bounded HTTP API and
/// additionally serves `GET /metrics`.
pub fn run_with_metrics(config: ServiceConfig, metrics: ServiceMetrics) -> io::Result<()> {
    run_internal(config, Some(metrics))
}

fn run_internal(config: ServiceConfig, metrics: Option<ServiceMetrics>) -> io::Result<()> {
    config.validate().map_err(invalid_config)?;
    let listener = TcpListener::bind(config.bind)?;
    serve_internal(listener, config, metrics)
}

fn invalid_config(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Serve an already-bound listener.  Keeping this separate makes integration
/// tests able to bind an ephemeral port without exposing a shutdown primitive
/// in the public daemon API.
pub fn serve(listener: TcpListener, config: ServiceConfig) -> io::Result<()> {
    serve_internal(listener, config, None)
}

/// Serve an already-bound listener with a shared metrics registry.
pub fn serve_with_metrics(
    listener: TcpListener,
    config: ServiceConfig,
    metrics: ServiceMetrics,
) -> io::Result<()> {
    serve_internal(listener, config, Some(metrics))
}

fn serve_internal(
    listener: TcpListener,
    config: ServiceConfig,
    metrics: Option<ServiceMetrics>,
) -> io::Result<()> {
    config.validate().map_err(invalid_config)?;
    let config = Arc::new(config);
    let gate = Arc::new(WorkerGate::new(config.workers));
    for incoming in listener.incoming() {
        let stream = incoming?;
        let timer = metrics.as_ref().map(ServiceMetrics::start_http_request);
        let Some(permit) = gate.try_acquire() else {
            if let Some(timer) = timer {
                timer.finish(503, 0);
            }
            if let Some(metrics) = metrics.as_ref() {
                metrics.record_busy();
            }
            let _ = write_response(stream, Response::error(503, "busy", "service is busy"));
            continue;
        };
        let config = Arc::clone(&config);
        let metrics = metrics.clone();
        thread::spawn(move || {
            handle_connection(stream, &config, metrics.as_ref(), timer);
            drop(permit);
        });
    }
    Ok(())
}

struct WorkerGate {
    active: AtomicUsize,
    max: usize,
}

impl WorkerGate {
    fn new(max: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            max,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<WorkerPermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.max {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(WorkerPermit {
                        gate: Arc::clone(self),
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }
}

struct WorkerPermit {
    gate: Arc<WorkerGate>,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct RequestError {
    status: u16,
    code: &'static str,
    message: String,
}

impl RequestError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json<T: Serialize>(status: u16, value: &T) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| {
            br#"{"schema":"https://penguin425.github.io/audio-normalizer/schema/service-error-v1","error_code":"serialization","message":"failed to serialize response"}"#.to_vec()
        });
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
        }
    }

    fn error(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self::json(
            status,
            &ErrorResponse {
                schema: SERVICE_ERROR_SCHEMA,
                generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
                error_code: code,
                message: bounded_message(&message.into()),
            },
        )
    }

    fn text(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            body: body.into_bytes(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    schema: &'static str,
    generator: &'static str,
    error_code: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct AnalysisResponse<'a> {
    schema: &'static str,
    generator: &'static str,
    filename: &'a str,
    content_type: &'a str,
    bytes_received: usize,
    max_decoded_samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    compliance_profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_layout: Option<&'a ChannelLayoutDescriptor>,
    report: ServiceReport<'a>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ServiceReport<'a> {
    V1(&'a AnalysisReport),
    V2(Box<AnalysisReportWire<'a>>),
}

fn handle_connection(
    mut stream: TcpStream,
    config: &ServiceConfig,
    metrics: Option<&ServiceMetrics>,
    mut timer: Option<RequestTimer>,
) {
    let _ = stream.set_read_timeout(Some(config.timeout));
    let _ = stream.set_write_timeout(Some(config.timeout));
    let mut request_bytes = 0_u64;
    let response = match read_request(&mut stream, config.max_body_bytes) {
        Ok(request) => {
            request_bytes = request.body.len() as u64;
            if let Some(timer) = timer.as_mut() {
                timer.set_traceparent(request.headers.get("traceparent").map(String::as_str));
            }
            route(request, config, metrics, timer.as_mut())
        }
        Err(error) => Response::error(error.status, error.code, error.message),
    };
    if let Some(timer) = timer {
        timer.finish(response.status, request_bytes);
    }
    let _ = write_response(stream, response);
}

fn route(
    request: HttpRequest,
    config: &ServiceConfig,
    metrics: Option<&ServiceMetrics>,
    timer: Option<&mut RequestTimer>,
) -> Response {
    if let Some(token) = &config.bearer_token {
        let expected = format!("Bearer {token}");
        if request.headers.get("authorization") != Some(&expected) {
            return Response::error(401, "unauthorized", "a valid bearer token is required");
        }
    }

    let target = match Url::parse(&format!("http://forge.invalid{}", request.target)) {
        Ok(target) => target,
        Err(_) => return Response::error(400, "invalid_target", "request target is invalid"),
    };
    if target.fragment().is_some() {
        return Response::error(
            400,
            "invalid_target",
            "fragments are not valid in HTTP targets",
        );
    }
    let path = target.path();
    match (request.method.as_str(), path) {
        ("GET", "/healthz") | ("GET", "/readyz") => Response::json(
            200,
            &HealthResponse {
                schema: SERVICE_HEALTH_SCHEMA,
                generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
                status: "ok",
            },
        ),
        ("GET", "/metrics") => metrics.map_or_else(
            || Response::error(404, "not_found", "endpoint not found"),
            |metrics| Response::text(200, PROMETHEUS_CONTENT_TYPE, metrics.render_prometheus()),
        ),
        ("POST", "/v1/analyze") => analyze_upload(
            request,
            config,
            &target,
            metrics,
            timer,
            SERVICE_ANALYSIS_SCHEMA_V1,
        ),
        ("POST", "/v2/analyze") => analyze_upload(
            request,
            config,
            &target,
            metrics,
            timer,
            SERVICE_ANALYSIS_SCHEMA_V2,
        ),
        ("POST", "/v3/analyze") => analyze_upload(
            request,
            config,
            &target,
            metrics,
            timer,
            SERVICE_ANALYSIS_SCHEMA_V3,
        ),
        _ => Response::error(404, "not_found", "endpoint not found"),
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    schema: &'static str,
    generator: &'static str,
    status: &'static str,
}

fn analyze_upload(
    request: HttpRequest,
    config: &ServiceConfig,
    target: &Url,
    metrics: Option<&ServiceMetrics>,
    mut timer: Option<&mut RequestTimer>,
    response_schema: &'static str,
) -> Response {
    if request.body.is_empty() {
        return Response::error(400, "empty_body", "audio request body is empty");
    }
    let params: HashMap<String, String> = target
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let filename = match safe_filename(request.headers.get("x-forge-filename")) {
        Ok(value) => value,
        Err(message) => return Response::error(400, "invalid_filename", message),
    };
    let suffix = match audio_suffix(&filename, request.headers.get("content-type")) {
        Ok(value) => value,
        Err(message) => return Response::error(400, "unsupported_format", message),
    };
    let profile = match params.get("profile") {
        Some(name) => match ComplianceProfile::builtin(name) {
            Some(profile) if !profile.requires_dialogue() => Some(profile),
            Some(_) => {
                return Response::error(
                    422,
                    "unsupported_profile",
                    "dialogue-based profiles require an explicit dialogue source",
                )
            }
            None => return Response::error(400, "unknown_profile", "unknown built-in profile"),
        },
        None => None,
    };
    let requested_layout = if response_schema == SERVICE_ANALYSIS_SCHEMA_V3 {
        match request.headers.get("x-forge-channel-layout") {
            Some(json) => match ChannelLayoutDescriptor::from_json(json) {
                Ok(layout) => Some(layout),
                Err(error) => {
                    return Response::error(400, "invalid_channel_layout", error);
                }
            },
            None => None,
        }
    } else {
        if request.headers.contains_key("x-forge-channel-layout") {
            return Response::error(
                400,
                "unsupported_channel_layout",
                "channel-layout overrides require /v3/analyze",
            );
        }
        None
    };

    let mut temporary = match Builder::new()
        .prefix("forge-service-")
        .suffix(&suffix)
        .tempfile()
    {
        Ok(file) => file,
        Err(_) => return Response::error(500, "temporary_file", "could not create upload file"),
    };
    if temporary.write_all(&request.body).is_err() || temporary.flush().is_err() {
        return Response::error(500, "temporary_file", "could not store upload");
    }
    let path = temporary.path().to_path_buf();
    let has_layout_override = requested_layout.is_some();
    let (mut decoded, declared_layout) =
        match decoder::decode_limited_with_channel_layout(&path, config.max_decoded_samples) {
            Ok(decoded) => decoded,
            Err(_) => return Response::error(422, "decode_failed", "audio could not be decoded"),
        };
    let effective_layout = requested_layout.unwrap_or(declared_layout);
    if has_layout_override {
        if let Err(error) = effective_layout.validate_override_for_channels(decoded.channels) {
            return Response::error(400, "invalid_channel_layout", error);
        }
    }
    let override_roles = has_layout_override.then(|| effective_layout.channel_roles());
    decoded.channel_roles = match crate::normalize::resolve_decoded_channel_roles(
        &path,
        decoded.channels,
        &decoded.channel_roles,
        effective_layout.provenance(),
        override_roles.as_deref(),
    ) {
        Ok(roles) => roles,
        Err(_) => return Response::error(422, "decode_failed", "audio could not be decoded"),
    };
    let analysis = crate::analysis::analyze(&decoded);
    let report = profile.as_ref().map_or_else(
        || AnalysisReport::new(&path, &analysis),
        |profile| AnalysisReport::with_compliance(&path, &analysis, Some(profile)),
    );
    let mut report = report;
    report.path = filename.clone();
    if response_schema == SERVICE_ANALYSIS_SCHEMA_V1
        && (!report.integrated_lufs.is_finite() || !report.true_peak_dbtp.is_finite())
    {
        return Response::error(
            422,
            "non_finite_measurement",
            "the v1 response contract cannot represent a non-finite measurement; use /v2/analyze",
        );
    }
    if serde_json::to_value(&report).is_err() {
        return Response::error(
            422,
            "non_finite_measurement",
            "the audio measurement contains a non-finite value",
        );
    }
    let decoded_samples = (decoded.frames as u64).saturating_mul(u64::from(decoded.channels));
    if let Some(metrics) = metrics {
        metrics.observe_analysis(
            request.body.len() as u64,
            decoded_samples,
            report.integrated_lufs,
        );
    }
    if let Some(timer) = timer.as_mut() {
        timer.observe_analysis(decoded_samples, report.integrated_lufs);
    }
    let content_type = request
        .headers
        .get("content-type")
        .map_or("application/octet-stream", String::as_str);
    let report = if response_schema == SERVICE_ANALYSIS_SCHEMA_V1 {
        ServiceReport::V1(&report)
    } else {
        ServiceReport::V2(Box::new(AnalysisReportWire::new(
            &report,
            AnalysisEngine::Fast,
        )))
    };
    Response::json(
        200,
        &AnalysisResponse {
            schema: response_schema,
            generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
            filename: &filename,
            content_type,
            bytes_received: request.body.len(),
            max_decoded_samples: config.max_decoded_samples,
            compliance_profile: params.get("profile").map(String::as_str),
            channel_layout: (response_schema == SERVICE_ANALYSIS_SCHEMA_V3)
                .then_some(&effective_layout),
            report,
        },
    )
}

fn safe_filename(value: Option<&String>) -> Result<String, String> {
    let Some(value) = value else {
        return Ok("upload.wav".into());
    };
    if value.is_empty()
        || value.len() > MAX_FILENAME_BYTES
        || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err("filename must contain 1..=256 printable bytes".into());
    }
    let basename = value.rsplit(['/', '\\']).next().unwrap_or(value);
    if basename.is_empty() || basename == "." || basename == ".." {
        return Err("filename must contain a basename".into());
    }
    Ok(basename.to_owned())
}

fn audio_suffix(filename: &str, content_type: Option<&String>) -> Result<String, String> {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let extension = match extension.as_str() {
        "wav" | "wave" | "bwf" | "bw64" | "rf64" | "flac" | "mp3" | "opus" | "ogg" | "m4a"
        | "mp4" | "aac" | "dsf" | "dff" => extension,
        "" => content_type
            .and_then(|value| content_type_extension(value))
            .unwrap_or_else(|| "wav".into()),
        _ => return Err("filename extension is not a supported audio format".into()),
    };
    Ok(format!(".{extension}"))
}

fn content_type_extension(value: &str) -> Option<String> {
    let value = value.split(';').next()?.trim().to_ascii_lowercase();
    Some(
        match value.as_str() {
            "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/mpeg" => "mp3",
            "audio/ogg" => "ogg",
            "audio/opus" => "opus",
            "audio/mp4" | "audio/x-m4a" => "m4a",
            _ => return None,
        }
        .into(),
    )
}

fn read_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<HttpRequest, RequestError> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).map_err(|_| {
            RequestError::new(408, "read_timeout", "request headers could not be read")
        })?;
        if read == 0 {
            return Err(RequestError::new(
                400,
                "incomplete_request",
                "request ended before headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(RequestError::new(
                431,
                "headers_too_large",
                "request headers are too large",
            ));
        }
    };
    let head = &bytes[..header_end];
    let remainder = &bytes[header_end + 4..];
    let text = std::str::from_utf8(head)
        .map_err(|_| RequestError::new(400, "invalid_headers", "request headers are not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| RequestError::new(400, "invalid_request", "request line is missing"))?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() || target.len() > MAX_HEADER_VALUE_BYTES || !target.starts_with('/') {
        return Err(RequestError::new(
            400,
            "invalid_request",
            "request line is invalid",
        ));
    }
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return Err(RequestError::new(
            505,
            "http_version",
            "only HTTP/1.0 and HTTP/1.1 are supported",
        ));
    }
    if version == "HTTP/1.1"
        && !lines
            .clone()
            .any(|line| line.to_ascii_lowercase().starts_with("host:"))
    {
        return Err(RequestError::new(
            400,
            "missing_host",
            "HTTP/1.1 requests require a Host header",
        ));
    }
    let mut headers = HashMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(RequestError::new(
                400,
                "invalid_headers",
                "header line is invalid",
            ));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-'))
            || value.len() > MAX_HEADER_VALUE_BYTES
            || value.bytes().any(|byte| byte < 0x20 && byte != b'\t')
        {
            return Err(RequestError::new(
                400,
                "invalid_headers",
                "header value is invalid",
            ));
        }
        if headers.insert(name.clone(), value.to_owned()).is_some() {
            return Err(RequestError::new(
                400,
                "duplicate_header",
                "duplicate headers are not accepted",
            ));
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(RequestError::new(
            501,
            "transfer_encoding",
            "chunked transfer encoding is not supported",
        ));
    }
    let content_length = headers
        .get("content-length")
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                RequestError::new(400, "invalid_content_length", "content-length is invalid")
            })
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > max_body_bytes {
        return Err(RequestError::new(
            413,
            "body_too_large",
            "request body exceeds the configured limit",
        ));
    }
    if remainder.len() > content_length {
        return Err(RequestError::new(
            400,
            "body_mismatch",
            "request contains bytes beyond content-length",
        ));
    }
    let mut body = remainder.to_vec();
    body.resize(content_length, 0);
    if body.len() > remainder.len() {
        stream
            .read_exact(&mut body[remainder.len()..])
            .map_err(|_| RequestError::new(408, "incomplete_body", "request body is incomplete"))?;
    }
    Ok(HttpRequest {
        method: method.to_ascii_uppercase(),
        target: target.to_owned(),
        headers,
        body,
    })
}

fn write_response(mut stream: TcpStream, response: Response) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    )?;
    stream.write_all(&response.body)
}

fn bounded_message(message: &str) -> String {
    if message.chars().count() <= MAX_ERROR_BYTES {
        return message.to_owned();
    }
    let mut value = message.chars().take(MAX_ERROR_BYTES).collect::<String>();
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn config_requires_auth_for_non_loopback() {
        let config = ServiceConfig {
            bind: "0.0.0.0:8080".parse().unwrap(),
            ..ServiceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn content_type_supplies_extension() {
        assert_eq!(
            audio_suffix("upload", Some(&"audio/flac".to_owned())).unwrap(),
            ".flac"
        );
    }

    #[test]
    fn filename_is_reduced_to_a_safe_basename() {
        assert_eq!(
            safe_filename(Some(&"/tmp/../mix.wav".to_owned())).unwrap(),
            "mix.wav"
        );
        assert!(safe_filename(Some(&"../".to_owned())).is_err());
    }

    #[test]
    fn parser_rejects_chunked_and_oversized_body() {
        let request =
            b"POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut stream = mock_stream(request);
        assert_eq!(read_request(&mut stream, 1024).unwrap_err().status, 501);

        let request = b"POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nContent-Length: 2048\r\n\r\n";
        let mut stream = mock_stream(request);
        assert_eq!(read_request(&mut stream, 1024).unwrap_err().status, 413);
    }

    fn mock_stream(bytes: &[u8]) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(false).unwrap();
        let mut client_writer = client.try_clone().unwrap();
        client_writer.write_all(bytes).unwrap();
        client_writer.shutdown(std::net::Shutdown::Write).unwrap();
        // The server side is the stream read by the parser.  Keep the client
        // alive through this helper by leaking only the tiny test handle.
        let _ = Cursor::new(Vec::<u8>::new());
        server
    }
}
