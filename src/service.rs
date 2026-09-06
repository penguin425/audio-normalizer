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
use crate::report::{AnalysisReport, AnalysisReportWire, ComplianceProfile};
use crate::service_metrics::{RequestTimer, ServiceMetrics, PROMETHEUS_CONTENT_TYPE};
use crate::service_runtime::{
    analyze_stable_input, service_analysis_working_set_reservation_bytes, ControlledAnalysisError,
    RequestControl, ResourceGovernor, ServiceRuntimeError, ServiceRuntimeErrorKind, UploadSpool,
    SERVICE_RESPONSE_WIRE_ALLOWANCE_BYTES,
};
use crate::stable_input::StableInputOptions;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
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
pub const SERVICE_ERROR_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/service-error-v2";
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
const UPLOAD_READ_CHUNK_BYTES: usize = 64 * 1024;
const DEADLINE_RESPONSE_WRITE_GRACE: Duration = Duration::from_millis(100);
// The largest current v3 metadata/wire envelope is about 265 KiB. Reserve a
// rounded upper allowance so every configured worker can admit one maximum
// audio body plus the complete bounded protobuf message with room for framing.
const SERVICE_TRANSPORT_MESSAGE_OVERHEAD_ALLOWANCE_BYTES: u64 = 512 * 1024;

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

/// Process-wide memory and temporary-storage admission limits for service work.
///
/// Clones share the same counters, so one value can govern REST and gRPC
/// listeners in the same process. The memory budget is an admission charge for
/// the major Forge-owned decode/analysis allocations and explicit conservative
/// allowances. It is not a hard RSS cap: third-party codec overhead, allocator
/// fragmentation, thread stacks, and transport buffers are outside this
/// counter. The gRPC listener separately bounds accepted connections, HTTP/2
/// streams/windows, complete protobuf message size, and concurrent decoding.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct ServiceRuntimeLimits {
    governor: ResourceGovernor,
}

impl ServiceRuntimeLimits {
    /// Create explicitly shared process-wide byte budgets.
    pub fn new(
        memory_capacity_bytes: u64,
        temporary_storage_capacity_bytes: u64,
    ) -> Result<Self, String> {
        if memory_capacity_bytes == 0 {
            return Err("service memory quota must be greater than zero".into());
        }
        if temporary_storage_capacity_bytes == 0 {
            return Err("service temporary-storage quota must be greater than zero".into());
        }
        Ok(Self {
            governor: ResourceGovernor::new(
                memory_capacity_bytes,
                temporary_storage_capacity_bytes,
            ),
        })
    }

    /// Derive safe defaults that allow every configured worker one maximum
    /// upload, the bounded protobuf metadata/wire envelope, and one
    /// conservative maximum decoded-PCM admission.
    pub fn for_config(config: &ServiceConfig) -> Result<Self, String> {
        config.validate()?;
        let workers = u64::try_from(config.workers)
            .map_err(|_| "service worker count exceeds the byte-count domain")?;
        let upload_bytes = u64::try_from(config.max_body_bytes)
            .map_err(|_| "max_body_bytes exceeds the byte-count domain")?;
        let analysis_bytes = service_analysis_working_set_reservation_bytes(
            upload_bytes,
            config.max_decoded_samples,
        )
        .map_err(|error| error.to_string())?;
        // gRPC transfers its decoded protobuf audio Vec into the worker
        // without copying, so the encoded-message lease remains live beside
        // the decode/analysis working-set lease. Include both for every
        // configured worker; REST simply leaves the encoded-memory portion
        // unused because its immutable upload lives in temp storage.
        let encoded_message_bytes = upload_bytes
            .checked_add(SERVICE_TRANSPORT_MESSAGE_OVERHEAD_ALLOWANCE_BYTES)
            .ok_or_else(|| "default encoded-message admission overflows u64".to_string())?;
        // Input protobuf storage and response serialization do not overlap:
        // the former is released before decode begins. Charge the larger
        // phase beside the decode/analysis working set for each worker.
        let transport_phase_bytes =
            encoded_message_bytes.max(SERVICE_RESPONSE_WIRE_ALLOWANCE_BYTES);
        let per_worker_memory = analysis_bytes
            .checked_add(transport_phase_bytes)
            .ok_or_else(|| "default per-worker service memory quota overflows u64".to_string())?;
        let memory_capacity = per_worker_memory
            .checked_mul(workers)
            .ok_or_else(|| "default service memory quota overflows u64".to_string())?;
        let temporary_storage_capacity = upload_bytes
            .checked_mul(workers)
            .ok_or_else(|| "default service temporary-storage quota overflows u64".to_string())?;
        Self::new(memory_capacity, temporary_storage_capacity)
    }

    pub fn memory_capacity_bytes(&self) -> u64 {
        self.governor.memory_capacity()
    }

    pub fn temporary_storage_capacity_bytes(&self) -> u64 {
        self.governor.temporary_storage_capacity()
    }

    /// Current accepted-work memory reservations. This low-cardinality gauge
    /// is suitable for process diagnostics and does not identify requests.
    pub fn memory_used_bytes(&self) -> u64 {
        self.governor.memory_used()
    }

    /// Current accepted-work temporary-storage reservations.
    pub fn temporary_storage_used_bytes(&self) -> u64 {
        self.governor.temporary_storage_used()
    }

    pub(crate) fn governor(&self) -> &ResourceGovernor {
        &self.governor
    }
}

/// Start the service and accept connections until the listener fails.
pub fn run(config: ServiceConfig) -> io::Result<()> {
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    run_internal(config, None, limits)
}

/// Start the service with an optional shared metrics registry.
///
/// The plain [`run`] entry point remains unchanged for callers that do not
/// need observability.  This variant exposes the same bounded HTTP API and
/// additionally serves `GET /metrics`.
pub fn run_with_metrics(config: ServiceConfig, metrics: ServiceMetrics) -> io::Result<()> {
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    run_internal(config, Some(metrics), limits)
}

/// Start the service with explicitly shared process-wide resource budgets.
pub fn run_with_runtime_limits(
    config: ServiceConfig,
    limits: ServiceRuntimeLimits,
) -> io::Result<()> {
    run_internal(config, None, limits)
}

/// Start the service with metrics and explicitly shared resource budgets.
pub fn run_with_metrics_and_runtime_limits(
    config: ServiceConfig,
    metrics: ServiceMetrics,
    limits: ServiceRuntimeLimits,
) -> io::Result<()> {
    run_internal(config, Some(metrics), limits)
}

fn run_internal(
    config: ServiceConfig,
    metrics: Option<ServiceMetrics>,
    limits: ServiceRuntimeLimits,
) -> io::Result<()> {
    config.validate().map_err(invalid_config)?;
    let listener = TcpListener::bind(config.bind)?;
    serve_internal(listener, config, metrics, limits)
}

fn invalid_config(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Serve an already-bound listener.  Keeping this separate makes integration
/// tests able to bind an ephemeral port without exposing a shutdown primitive
/// in the public daemon API.
pub fn serve(listener: TcpListener, config: ServiceConfig) -> io::Result<()> {
    let config = effective_listener_config(&listener, config)?;
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    serve_internal(listener, config, None, limits)
}

/// Serve an already-bound listener with a shared metrics registry.
pub fn serve_with_metrics(
    listener: TcpListener,
    config: ServiceConfig,
    metrics: ServiceMetrics,
) -> io::Result<()> {
    let config = effective_listener_config(&listener, config)?;
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    serve_internal(listener, config, Some(metrics), limits)
}

/// Serve an already-bound listener with shared process-wide resource budgets.
pub fn serve_with_runtime_limits(
    listener: TcpListener,
    config: ServiceConfig,
    limits: ServiceRuntimeLimits,
) -> io::Result<()> {
    let config = effective_listener_config(&listener, config)?;
    serve_internal(listener, config, None, limits)
}

/// Serve an already-bound listener with metrics and shared resource budgets.
pub fn serve_with_metrics_and_runtime_limits(
    listener: TcpListener,
    config: ServiceConfig,
    metrics: ServiceMetrics,
    limits: ServiceRuntimeLimits,
) -> io::Result<()> {
    let config = effective_listener_config(&listener, config)?;
    serve_internal(listener, config, Some(metrics), limits)
}

fn effective_listener_config(
    listener: &TcpListener,
    mut config: ServiceConfig,
) -> io::Result<ServiceConfig> {
    config.bind = listener.local_addr()?;
    config.validate().map_err(invalid_config)?;
    Ok(config)
}

fn serve_internal(
    listener: TcpListener,
    config: ServiceConfig,
    metrics: Option<ServiceMetrics>,
    limits: ServiceRuntimeLimits,
) -> io::Result<()> {
    let config = effective_listener_config(&listener, config)?;
    let config = Arc::new(config);
    // Header/auth parsing has its own small bound so a slow unauthenticated
    // peer cannot consume an analysis worker. One control connection of
    // headroom remains available while every worker is analyzing audio.
    let connection_gate = Arc::new(ConcurrencyGate::new(
        config
            .workers
            .checked_add(1)
            .expect("validated worker count has connection headroom"),
    ));
    let worker_gate = Arc::new(ConcurrencyGate::new(config.workers));
    for incoming in listener.incoming() {
        let stream = incoming?;
        let timer = metrics.as_ref().map(ServiceMetrics::start_http_request);
        let Some(connection_permit) = connection_gate.try_acquire() else {
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
        let limits = limits.clone();
        let worker_gate = Arc::clone(&worker_gate);
        thread::spawn(move || {
            handle_connection(
                stream,
                &config,
                &limits,
                metrics.as_ref(),
                timer,
                &worker_gate,
            );
            drop(connection_permit);
        });
    }
    Ok(())
}

struct ConcurrencyGate {
    active: AtomicUsize,
    max: usize,
}

impl ConcurrencyGate {
    fn new(max: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            max,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ConcurrencyPermit> {
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
                    return Some(ConcurrencyPermit {
                        gate: Arc::clone(self),
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }
}

struct ConcurrencyPermit {
    gate: Arc<ConcurrencyGate>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    content_length: u64,
    initial_body: Vec<u8>,
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
    allow_deadline_write_grace: bool,
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
            allow_deadline_write_grace: false,
        }
    }

    fn error(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        let mut response = Self::json(
            status,
            &ErrorResponse {
                schema: error_schema_for_code(code),
                generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
                error_code: code,
                message: bounded_message(&message.into()),
            },
        );
        // These failure discriminators predate the absolute request control.
        // Give only their terminal response a single short best-effort grace
        // period so introducing deadlines does not silently erase the v1 wire
        // error that existing clients already handle.
        response.allow_deadline_write_grace = matches!(code, "read_timeout" | "incomplete_body");
        response
    }

    fn text(status: u16, content_type: &'static str, body: String) -> Self {
        Self {
            status,
            content_type,
            body: body.into_bytes(),
            allow_deadline_write_grace: false,
        }
    }
}

fn error_schema_for_code(code: &str) -> &'static str {
    if matches!(
        code,
        "invalid_limit"
            | "limit_exceeded"
            | "quota_exceeded"
            | "arithmetic_overflow"
            | "cancelled"
            | "deadline_exceeded"
            | "io"
            | "incomplete_upload"
            | "invalid_snapshot"
    ) {
        SERVICE_ERROR_SCHEMA_V2
    } else {
        SERVICE_ERROR_SCHEMA
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
    limits: &ServiceRuntimeLimits,
    metrics: Option<&ServiceMetrics>,
    mut timer: Option<RequestTimer>,
    worker_gate: &Arc<ConcurrencyGate>,
) {
    let _ = stream.set_write_timeout(Some(config.timeout));
    let mut request_bytes = 0_u64;
    let control = match RequestControl::from_timeout(config.timeout) {
        Ok(control) => control,
        Err(error) => {
            let response = response_from_runtime(error);
            if let Some(timer) = timer {
                timer.finish(response.status, 0);
            }
            let _ = write_response(stream, response);
            return;
        }
    };
    let response = match read_request_head(&mut stream, config.max_body_bytes, &control) {
        Ok(request) => {
            if let Some(timer) = timer.as_mut() {
                timer.set_traceparent(request.headers.get("traceparent").map(String::as_str));
            }
            if let Some(response) = authorize_http_request(&request, config) {
                response
            } else if let Some(worker_permit) = worker_gate.try_acquire() {
                let mut context = HttpRequestContext {
                    stream: &mut stream,
                    config,
                    limits,
                    control: &control,
                    metrics,
                    timer: timer.as_mut(),
                    request_bytes: &mut request_bytes,
                };
                let response = route(request, &mut context);
                drop(worker_permit);
                response
            } else {
                if let Some(metrics) = metrics {
                    metrics.record_busy();
                }
                Response::error(503, "busy", "service is busy")
            }
        }
        Err(error) => Response::error(error.status, error.code, error.message),
    };
    let response_status = response.status;
    let write_status = write_response_controlled(stream, response, &control)
        .err()
        .map_or(response_status, |error| error.status);
    if let Some(timer) = timer {
        timer.finish(write_status, request_bytes);
    }
}

fn authorize_http_request(request: &HttpRequest, config: &ServiceConfig) -> Option<Response> {
    let token = config.bearer_token.as_ref()?;
    let expected = format!("Bearer {token}");
    (request.headers.get("authorization") != Some(&expected))
        .then(|| Response::error(401, "unauthorized", "a valid bearer token is required"))
}

struct HttpRequestContext<'a> {
    stream: &'a mut TcpStream,
    config: &'a ServiceConfig,
    limits: &'a ServiceRuntimeLimits,
    control: &'a RequestControl,
    metrics: Option<&'a ServiceMetrics>,
    timer: Option<&'a mut RequestTimer>,
    request_bytes: &'a mut u64,
}

fn route(request: HttpRequest, context: &mut HttpRequestContext<'_>) -> Response {
    if let Some(token) = &context.config.bearer_token {
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
        ("GET", "/metrics") => context.metrics.map_or_else(
            || Response::error(404, "not_found", "endpoint not found"),
            |metrics| Response::text(200, PROMETHEUS_CONTENT_TYPE, metrics.render_prometheus()),
        ),
        ("POST", "/v1/analyze") => {
            analyze_upload(request, &target, SERVICE_ANALYSIS_SCHEMA_V1, context)
        }
        ("POST", "/v2/analyze") => {
            analyze_upload(request, &target, SERVICE_ANALYSIS_SCHEMA_V2, context)
        }
        ("POST", "/v3/analyze") => {
            analyze_upload(request, &target, SERVICE_ANALYSIS_SCHEMA_V3, context)
        }
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
    target: &Url,
    response_schema: &'static str,
    context: &mut HttpRequestContext<'_>,
) -> Response {
    if request.content_length == 0 {
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

    let stable_options = match StableInputOptions::new(context.config.max_body_bytes as u64) {
        Ok(options) => options.with_source_name_hint(format!("upload{suffix}")),
        Err(error) => return Response::error(500, "invalid_limit", error.to_string()),
    };
    let mut spool = match UploadSpool::create(
        context.limits.governor(),
        context.control.clone(),
        request.content_length,
        stable_options,
    ) {
        Ok(spool) => spool,
        Err(error) => return response_from_upload_spool(error),
    };
    if let Err(error) = spool.write_chunk(&request.initial_body) {
        return response_from_upload_spool(error);
    }
    *context.request_bytes = request.initial_body.len() as u64;
    let mut remaining = request
        .content_length
        .saturating_sub(request.initial_body.len() as u64);
    let mut chunk = [0_u8; UPLOAD_READ_CHUNK_BYTES];
    while remaining != 0 {
        let wanted = usize::try_from(remaining.min(chunk.len() as u64))
            .expect("a read bounded by a usize buffer fits usize");
        let read = match read_controlled(context.stream, &mut chunk[..wanted], context.control) {
            Ok(0) => return Response::error(408, "incomplete_body", "request body is incomplete"),
            Ok(read) => read,
            Err(error) => return Response::error(error.status, error.code, error.message),
        };
        if let Err(error) = spool.write_chunk(&chunk[..read]) {
            return response_from_upload_spool(error);
        }
        let read = read as u64;
        remaining -= read;
        *context.request_bytes = (*context.request_bytes).saturating_add(read);
    }
    if let Err(error) = reject_buffered_extra_framing(context.stream) {
        return Response::error(error.status, error.code, error.message);
    }
    let stable_input = match spool.finish_into_stable_input() {
        Ok(input) => input,
        Err(error) => return response_from_upload_spool(error),
    };
    let controlled = match analyze_stable_input(
        stable_input,
        requested_layout,
        context.config.max_decoded_samples,
        context.limits.governor(),
        context.control,
    ) {
        Ok(analysis) => analysis,
        Err(ControlledAnalysisError::Runtime(error)) => return response_from_runtime(error),
        Err(ControlledAnalysisError::Media(_)) => {
            return Response::error(422, "decode_failed", "audio could not be decoded")
        }
    };
    let analysis = &controlled.analysis;
    let effective_layout = &controlled.channel_layout;
    if response_schema == SERVICE_ANALYSIS_SCHEMA_V3 && effective_layout.to_json().is_err() {
        return Response::error(
            500,
            "invalid_channel_layout",
            "effective channel layout exceeds its bounded JSON contract",
        );
    }
    let report = profile.as_ref().map_or_else(
        || AnalysisReport::new(Path::new(&filename), analysis),
        |profile| AnalysisReport::with_compliance(Path::new(&filename), analysis, Some(profile)),
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
    let decoded_samples = controlled.decoded_samples;
    if let Some(metrics) = context.metrics {
        metrics.observe_analysis(
            request.content_length,
            decoded_samples,
            report.integrated_lufs,
        );
    }
    if let Some(timer) = context.timer.as_deref_mut() {
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
    let response = Response::json(
        200,
        &AnalysisResponse {
            schema: response_schema,
            generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
            filename: &filename,
            content_type,
            bytes_received: usize::try_from(request.content_length)
                .expect("content length was validated against a usize limit"),
            max_decoded_samples: context.config.max_decoded_samples,
            compliance_profile: params.get("profile").map(String::as_str),
            channel_layout: (response_schema == SERVICE_ANALYSIS_SCHEMA_V3)
                .then_some(effective_layout),
            report,
        },
    );
    match context.control.check() {
        Ok(()) => response,
        Err(error) => response_from_runtime(error),
    }
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

fn read_request_head(
    stream: &mut TcpStream,
    max_body_bytes: usize,
    control: &RequestControl,
) -> Result<HttpRequest, RequestError> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = read_controlled(stream, &mut chunk, control)?;
        if read == 0 {
            return Err(RequestError::new(
                400,
                "incomplete_request",
                "request ended before headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            if index > MAX_HEADER_BYTES {
                return Err(RequestError::new(
                    431,
                    "headers_too_large",
                    "request headers are too large",
                ));
            }
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
            value.parse::<u64>().map_err(|_| {
                RequestError::new(400, "invalid_content_length", "content-length is invalid")
            })
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > max_body_bytes as u64 {
        return Err(RequestError::new(
            413,
            "body_too_large",
            "request body exceeds the configured limit",
        ));
    }
    if remainder.len() as u64 > content_length {
        return Err(RequestError::new(
            400,
            "body_mismatch",
            "request contains bytes beyond content-length",
        ));
    }
    Ok(HttpRequest {
        method: method.to_ascii_uppercase(),
        target: target.to_owned(),
        headers,
        content_length,
        initial_body: remainder.to_vec(),
    })
}

fn read_controlled(
    stream: &mut TcpStream,
    destination: &mut [u8],
    control: &RequestControl,
) -> Result<usize, RequestError> {
    loop {
        let remaining = control
            .remaining()
            .map_err(request_read_error_from_runtime)?;
        stream
            .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(|error| RequestError::new(500, "io", format!("set read timeout: {error}")))?;
        match stream.read(destination) {
            Ok(read) => {
                if read != 0 {
                    control.check().map_err(request_read_error_from_runtime)?;
                }
                return Ok(read);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                control.check().map_err(request_read_error_from_runtime)?;
                return Err(RequestError::new(
                    408,
                    "read_timeout",
                    "request data could not be read before the deadline",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(RequestError::new(
                    400,
                    "io",
                    format!("request data could not be read: {error}"),
                ));
            }
        }
    }
}

/// Reject bytes that are already queued past Content-Length. HTTP framing
/// defines the declared length as authoritative, so this deliberately does not
/// wait for speculative future bytes and therefore cannot turn every valid
/// request into an extra timeout interval.
fn reject_buffered_extra_framing(stream: &TcpStream) -> Result<(), RequestError> {
    stream.set_nonblocking(true).map_err(|error| {
        RequestError::new(500, "io", format!("inspect request framing: {error}"))
    })?;
    let mut byte = [0_u8; 1];
    let peek = stream.peek(&mut byte);
    let restore = stream.set_nonblocking(false);
    if let Err(error) = restore {
        return Err(RequestError::new(
            500,
            "io",
            format!("restore request socket mode: {error}"),
        ));
    }
    match peek {
        Ok(0) => Ok(()),
        Ok(_) => Err(RequestError::new(
            400,
            "body_mismatch",
            "request contains bytes beyond content-length",
        )),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            reject_buffered_extra_framing(stream)
        }
        Err(error) => Err(RequestError::new(
            400,
            "io",
            format!("request framing could not be inspected: {error}"),
        )),
    }
}

fn request_error_from_runtime(error: ServiceRuntimeError) -> RequestError {
    let (status, code) = runtime_http_status(error.kind());
    RequestError::new(status, code, error.to_string())
}

fn request_read_error_from_runtime(error: ServiceRuntimeError) -> RequestError {
    if error.kind() == ServiceRuntimeErrorKind::DeadlineExceeded {
        RequestError::new(
            408,
            "read_timeout",
            "request data could not be read before the deadline",
        )
    } else {
        request_error_from_runtime(error)
    }
}

fn response_from_runtime(error: ServiceRuntimeError) -> Response {
    let (status, code) = runtime_http_status(error.kind());
    Response::error(status, code, error.to_string())
}

fn response_from_upload_spool(error: ServiceRuntimeError) -> Response {
    match error.kind() {
        // Temporary-file failures existed before the resource-control API.
        // Preserve that v1 discriminator; only quota/control failures use the
        // additive v2 error schema.
        ServiceRuntimeErrorKind::Io | ServiceRuntimeErrorKind::InvalidSnapshot => {
            Response::error(500, "temporary_file", error.to_string())
        }
        _ => response_from_runtime(error),
    }
}

fn runtime_http_status(kind: ServiceRuntimeErrorKind) -> (u16, &'static str) {
    match kind {
        ServiceRuntimeErrorKind::InvalidLimit => (400, "invalid_limit"),
        ServiceRuntimeErrorKind::LimitExceeded => (413, "limit_exceeded"),
        ServiceRuntimeErrorKind::QuotaExceeded => (503, "quota_exceeded"),
        ServiceRuntimeErrorKind::ArithmeticOverflow => (400, "arithmetic_overflow"),
        ServiceRuntimeErrorKind::Cancelled => (408, "cancelled"),
        ServiceRuntimeErrorKind::DeadlineExceeded => (408, "deadline_exceeded"),
        ServiceRuntimeErrorKind::Io => (500, "io"),
        ServiceRuntimeErrorKind::IncompleteUpload => (408, "incomplete_upload"),
        ServiceRuntimeErrorKind::InvalidSnapshot => (500, "invalid_snapshot"),
    }
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

fn write_response_controlled(
    mut stream: TcpStream,
    response: Response,
    control: &RequestControl,
) -> Result<(), RequestError> {
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
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    let mut write_deadline = ControlledWriteDeadline {
        allow_grace: response.allow_deadline_write_grace,
        grace_deadline: None,
    };
    write_all_controlled(&mut stream, head.as_bytes(), control, &mut write_deadline)?;
    write_all_controlled(&mut stream, &response.body, control, &mut write_deadline)
}

struct ControlledWriteDeadline {
    allow_grace: bool,
    grace_deadline: Option<Instant>,
}

impl ControlledWriteDeadline {
    fn remaining(&mut self, control: &RequestControl) -> Result<Duration, RequestError> {
        match control.remaining() {
            Ok(remaining) => Ok(remaining),
            Err(error)
                if self.allow_grace
                    && error.kind() == ServiceRuntimeErrorKind::DeadlineExceeded =>
            {
                let deadline = *self.grace_deadline.get_or_insert_with(|| {
                    Instant::now()
                        .checked_add(DEADLINE_RESPONSE_WRITE_GRACE)
                        .unwrap_or_else(Instant::now)
                });
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err(request_error_from_runtime(error))
                } else {
                    Ok(remaining)
                }
            }
            Err(error) => Err(request_error_from_runtime(error)),
        }
    }
}

fn write_all_controlled(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    control: &RequestControl,
    deadline: &mut ControlledWriteDeadline,
) -> Result<(), RequestError> {
    while !bytes.is_empty() {
        let remaining = deadline.remaining(control)?;
        stream
            .set_write_timeout(Some(remaining.max(Duration::from_millis(1))))
            .map_err(|error| {
                RequestError::new(500, "io", format!("set response timeout: {error}"))
            })?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(RequestError::new(
                    500,
                    "io",
                    "response socket accepted zero bytes",
                ));
            }
            Ok(written) => {
                bytes = &bytes[written..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                let _ = deadline.remaining(control)?;
            }
            Err(error) => {
                return Err(RequestError::new(
                    500,
                    "io",
                    format!("response data could not be written: {error}"),
                ));
            }
        }
    }
    Ok(())
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

    fn mono_s16_wave(frames: usize) -> Vec<u8> {
        let sample_rate = 48_000_u32;
        let data_bytes = u32::try_from(frames * 2).unwrap();
        let mut audio = Vec::with_capacity(44 + data_bytes as usize);
        audio.extend_from_slice(b"RIFF");
        audio.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        audio.extend_from_slice(b"WAVEfmt ");
        audio.extend_from_slice(&16_u32.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&sample_rate.to_le_bytes());
        audio.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        audio.extend_from_slice(&2_u16.to_le_bytes());
        audio.extend_from_slice(&16_u16.to_le_bytes());
        audio.extend_from_slice(b"data");
        audio.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..frames {
            audio.extend_from_slice(&((frame % 101) as i16 * 100).to_le_bytes());
        }
        audio
    }

    fn http_analyze(
        audio: &[u8],
        declared_length: u64,
        config: ServiceConfig,
        limits: ServiceRuntimeLimits,
    ) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let worker_gate = Arc::new(ConcurrencyGate::new(config.workers));
            handle_connection(stream, &config, &limits, None, None, &worker_gate);
        });
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "POST /v2/analyze HTTP/1.1\r\nHost: forge\r\nContent-Type: audio/wav\r\nX-Forge-Filename: input.wav\r\nContent-Length: {declared_length}\r\n\r\n"
        )
        .unwrap();
        client.write_all(audio).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = Vec::new();
        if let Err(error) = client.read_to_end(&mut response) {
            assert_eq!(
                error.kind(),
                io::ErrorKind::ConnectionReset,
                "unexpected response read error: {error}"
            );
        }
        server.join().unwrap();
        response
    }

    fn http_timeout_response(partial_request: &[u8]) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = ServiceConfig {
            timeout: Duration::from_millis(150),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let worker_gate = Arc::new(ConcurrencyGate::new(config.workers));
            handle_connection(stream, &config, &limits, None, None, &worker_gate);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(partial_request).unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        server.join().unwrap();
        response
    }

    fn response_json(response: &[u8]) -> serde_json::Value {
        let (_, body) = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| response.split_at(index + 4))
            .expect("HTTP response contains a header delimiter");
        serde_json::from_slice(body).unwrap()
    }

    #[test]
    fn config_requires_auth_for_non_loopback() {
        let config = ServiceConfig {
            bind: "0.0.0.0:8080".parse().unwrap(),
            ..ServiceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn prebound_wildcard_listener_cannot_bypass_auth_validation() {
        let bind_wildcard = || TcpListener::bind("0.0.0.0:0").unwrap();
        let config = ServiceConfig::default();
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();

        assert_eq!(
            serve(bind_wildcard(), config.clone()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            serve_with_metrics(bind_wildcard(), config.clone(), ServiceMetrics::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            serve_with_runtime_limits(bind_wildcard(), config.clone(), limits.clone())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            serve_with_metrics_and_runtime_limits(
                bind_wildcard(),
                config,
                ServiceMetrics::new(),
                limits,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn rest_worker_gate_is_applied_after_header_and_auth_admission() {
        let config = ServiceConfig {
            bearer_token: Some("secret".into()),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let gate = Arc::new(ConcurrencyGate::new(1));
        let held = gate.try_acquire().unwrap();

        let invoke = |request: &'static [u8]| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let config = config.clone();
            let limits = limits.clone();
            let gate = Arc::clone(&gate);
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                handle_connection(stream, &config, &limits, None, None, &gate);
            });
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(request).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            server.join().unwrap();
            response
        };

        let unauthorized = invoke(b"GET /healthz HTTP/1.1\r\nHost: forge\r\n\r\n");
        assert!(unauthorized.starts_with("HTTP/1.1 401"));
        let authorized =
            invoke(b"GET /healthz HTTP/1.1\r\nHost: forge\r\nAuthorization: Bearer secret\r\n\r\n");
        assert!(authorized.starts_with("HTTP/1.1 503"));
        drop(held);
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
        let control = RequestControl::from_timeout(Duration::from_secs(5)).unwrap();
        let request =
            b"POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut stream = mock_stream(request);
        assert_eq!(
            read_request_head(&mut stream, 1024, &control)
                .unwrap_err()
                .status,
            501
        );

        let request = b"POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nContent-Length: 2048\r\n\r\n";
        let mut stream = mock_stream(request);
        assert_eq!(
            read_request_head(&mut stream, 1024, &control)
                .unwrap_err()
                .status,
            413
        );
    }

    #[test]
    fn runtime_limits_are_additive_and_clones_share_process_counters() {
        fn assert_runtime_traits<
            T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe,
        >() {
        }
        assert_runtime_traits::<ServiceRuntimeLimits>();

        let config = ServiceConfig {
            workers: 2,
            max_body_bytes: 1_024,
            max_decoded_samples: 2_048,
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let per_worker = service_analysis_working_set_reservation_bytes(1_024, 2_048).unwrap();
        assert_eq!(
            limits.memory_capacity_bytes(),
            2 * (per_worker
                + (1_024 + SERVICE_TRANSPORT_MESSAGE_OVERHEAD_ALLOWANCE_BYTES)
                    .max(SERVICE_RESPONSE_WIRE_ALLOWANCE_BYTES))
        );
        assert_eq!(limits.temporary_storage_capacity_bytes(), 2 * 1_024);
        let clone = limits.clone();
        let lease = clone.governor().reserve_memory(17).unwrap();
        assert_eq!(limits.memory_used_bytes(), 17);
        drop(lease);
        assert_eq!(clone.memory_used_bytes(), 0);
        assert!(ServiceRuntimeLimits::new(0, 1).is_err());
        assert!(ServiceRuntimeLimits::new(1, 0).is_err());
    }

    #[test]
    fn rest_streams_upload_and_releases_temp_and_decoded_admissions() {
        let audio = mono_s16_wave(12_000);
        assert!(
            audio.len() > 4_096,
            "exercise reads after the header buffer"
        );
        let config = ServiceConfig {
            max_body_bytes: audio.len(),
            max_decoded_samples: 12_000,
            workers: 1,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let memory =
            service_analysis_working_set_reservation_bytes(audio.len() as u64, 12_000).unwrap();
        let limits = ServiceRuntimeLimits::new(memory, audio.len() as u64).unwrap();
        let response = http_analyze(&audio, audio.len() as u64, config, limits.clone());
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains(&format!("\"bytes_received\":{}", audio.len())));
        assert_eq!(limits.memory_used_bytes(), 0);
        assert_eq!(limits.temporary_storage_used_bytes(), 0);
    }

    #[test]
    fn rest_fails_closed_on_short_extra_quota_and_decoded_expansion() {
        let audio = mono_s16_wave(12_000);
        let base_config = ServiceConfig {
            max_body_bytes: audio.len() + 1,
            max_decoded_samples: 12_000,
            workers: 1,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };

        let short_limits = ServiceRuntimeLimits::new(12_000 * 8, audio.len() as u64 + 1).unwrap();
        let short = http_analyze(
            &audio,
            audio.len() as u64 + 1,
            base_config.clone(),
            short_limits.clone(),
        );
        assert!(String::from_utf8_lossy(&short).starts_with("HTTP/1.1 408 Request Timeout"));
        let short_error = response_json(&short);
        assert_eq!(short_error["schema"], SERVICE_ERROR_SCHEMA);
        assert_eq!(short_error["error_code"], "incomplete_body");
        assert_eq!(short_limits.temporary_storage_used_bytes(), 0);

        let extra_limits = ServiceRuntimeLimits::new(8, 3).unwrap();
        let extra = http_analyze(b"abcd", 3, base_config.clone(), extra_limits.clone());
        assert!(String::from_utf8(extra)
            .unwrap()
            .starts_with("HTTP/1.1 400 Bad Request"));
        assert_eq!(extra_limits.temporary_storage_used_bytes(), 0);

        let quota_limits = ServiceRuntimeLimits::new(12_000 * 8, audio.len() as u64 - 1).unwrap();
        let quota = http_analyze(
            &audio,
            audio.len() as u64,
            base_config.clone(),
            quota_limits.clone(),
        );
        assert!(String::from_utf8(quota)
            .unwrap()
            .starts_with("HTTP/1.1 503 Service Unavailable"));
        assert_eq!(quota_limits.memory_used_bytes(), 0);
        assert_eq!(quota_limits.temporary_storage_used_bytes(), 0);

        let decoded_config = ServiceConfig {
            max_decoded_samples: 11_999,
            ..base_config
        };
        let decoded_memory =
            service_analysis_working_set_reservation_bytes(audio.len() as u64, 11_999).unwrap();
        let decoded_limits = ServiceRuntimeLimits::new(decoded_memory, audio.len() as u64).unwrap();
        let decoded = http_analyze(
            &audio,
            audio.len() as u64,
            decoded_config,
            decoded_limits.clone(),
        );
        assert!(String::from_utf8(decoded)
            .unwrap()
            .starts_with("HTTP/1.1 413 Payload Too Large"));
        assert_eq!(decoded_limits.memory_used_bytes(), 0);
        assert_eq!(decoded_limits.temporary_storage_used_bytes(), 0);
    }

    #[test]
    fn partial_header_and_body_timeouts_preserve_v1_wire_errors() {
        let header = http_timeout_response(b"POST /v1/analyze HTTP/1.1\r\nHost: forge\r\n");
        assert!(String::from_utf8_lossy(&header).starts_with("HTTP/1.1 408 Request Timeout"));
        let header_error = response_json(&header);
        assert_eq!(header_error["schema"], SERVICE_ERROR_SCHEMA);
        assert_eq!(header_error["error_code"], "read_timeout");

        let body = http_timeout_response(
            b"POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nContent-Type: audio/wav\r\nX-Forge-Filename: input.wav\r\nContent-Length: 4\r\n\r\nab",
        );
        assert!(String::from_utf8_lossy(&body).starts_with("HTTP/1.1 408 Request Timeout"));
        let body_error = response_json(&body);
        assert_eq!(body_error["schema"], SERVICE_ERROR_SCHEMA);
        assert_eq!(body_error["error_code"], "read_timeout");
    }

    #[test]
    fn parser_uses_u64_framing_and_absolute_deadline() {
        let request = format!(
            "POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nContent-Length: {}\r\n\r\n",
            u64::MAX
        );
        let mut stream = mock_stream(request.as_bytes());
        let control = RequestControl::from_timeout(Duration::from_secs(5)).unwrap();
        let parsed = read_request_head(&mut stream, usize::MAX, &control);
        if usize::BITS < u64::BITS {
            assert_eq!(parsed.unwrap_err().status, 413);
        } else {
            assert_eq!(parsed.unwrap().content_length, u64::MAX);
        }

        let mut stream = mock_stream(b"GET /healthz HTTP/1.1\r\nHost: forge\r\n\r\n");
        let expired = RequestControl::with_deadline(std::time::Instant::now());
        let error = read_request_head(&mut stream, 1, &expired).unwrap_err();
        assert_eq!((error.status, error.code), (408, "read_timeout"));
    }

    #[test]
    fn response_write_is_inside_the_absolute_request_deadline() {
        let stream = mock_stream(&[]);
        let expired = RequestControl::with_deadline(std::time::Instant::now());
        let error = write_response_controlled(
            stream,
            Response::text(200, "text/plain", "late".into()),
            &expired,
        )
        .unwrap_err();
        assert_eq!((error.status, error.code), (408, "deadline_exceeded"));
    }

    #[test]
    fn runtime_error_http_mapping_is_stable_and_low_cardinality() {
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::InvalidLimit),
            (400, "invalid_limit")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::LimitExceeded),
            (413, "limit_exceeded")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::QuotaExceeded),
            (503, "quota_exceeded")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::Cancelled),
            (408, "cancelled")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::DeadlineExceeded),
            (408, "deadline_exceeded")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::Io),
            (500, "io")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::ArithmeticOverflow),
            (400, "arithmetic_overflow")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::IncompleteUpload),
            (408, "incomplete_upload")
        );
        assert_eq!(
            runtime_http_status(ServiceRuntimeErrorKind::InvalidSnapshot),
            (500, "invalid_snapshot")
        );

        let runtime_error = ResourceGovernor::new(0, 0).reserve_memory(1).unwrap_err();
        let response = response_from_runtime(runtime_error);
        let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["schema"], SERVICE_ERROR_SCHEMA_V2);
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/service-error-v2.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&value));
        for code in [
            "invalid_limit",
            "limit_exceeded",
            "quota_exceeded",
            "arithmetic_overflow",
            "cancelled",
            "deadline_exceeded",
            "io",
            "incomplete_upload",
            "invalid_snapshot",
        ] {
            let response = Response::error(500, code, "bounded test error");
            let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(value["schema"], SERVICE_ERROR_SCHEMA_V2, "{code}");
            assert!(validator.is_valid(&value), "{code}");
        }

        let legacy = Response::error(401, "unauthorized", "legacy error contract");
        let value: serde_json::Value = serde_json::from_slice(&legacy.body).unwrap();
        assert_eq!(value["schema"], SERVICE_ERROR_SCHEMA);
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/service-error-v1.schema.json")).unwrap();
        assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&value));

        // These codes predate service-error-v2. Keep their existing v1 wire
        // discriminator even though the immutable v1 schema's closed enum did
        // not list them; silently switching existing responses would break
        // clients that dispatch on the schema URI.
        for code in ["invalid_channel_layout", "unsupported_channel_layout"] {
            let legacy = Response::error(400, code, "legacy error contract");
            let value: serde_json::Value = serde_json::from_slice(&legacy.body).unwrap();
            assert_eq!(value["schema"], SERVICE_ERROR_SCHEMA, "{code}");
        }
    }

    #[test]
    fn upload_spool_io_failures_preserve_the_temporary_file_v1_contract() {
        for kind in [
            ServiceRuntimeErrorKind::Io,
            ServiceRuntimeErrorKind::InvalidSnapshot,
        ] {
            let response = response_from_upload_spool(ServiceRuntimeError::new(
                kind,
                "injected temporary-file failure",
            ));
            assert_eq!(response.status, 500);
            let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(value["schema"], SERVICE_ERROR_SCHEMA, "{kind:?}");
            assert_eq!(value["error_code"], "temporary_file", "{kind:?}");
        }

        for kind in [
            ServiceRuntimeErrorKind::QuotaExceeded,
            ServiceRuntimeErrorKind::DeadlineExceeded,
        ] {
            let response = response_from_upload_spool(ServiceRuntimeError::new(
                kind,
                "injected resource-control failure",
            ));
            let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(value["schema"], SERVICE_ERROR_SCHEMA_V2, "{kind:?}");
        }
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
