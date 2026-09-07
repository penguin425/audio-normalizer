//! Optional gRPC service for bounded audio analysis.
//!
//! The gRPC surface is deliberately opt-in (`grpc-service`) so the default
//! library and REST build do not acquire an async runtime or HTTP/2 stack.
//! Request IDs identify only active jobs and may be reused after completion. A
//! caller can cancel an active job via the Cancel RPC; cancellation is
//! cooperative at bounded decode/analysis checkpoints and is also triggered
//! when the client drops the RPC. Internal request identity prevents an older
//! worker's cleanup from removing a newer active registration with the same ID.
//! Exact channel-layout overrides use the additive `ForgeAnalysisV3` service;
//! the original `ForgeAnalysis` messages and server trait remain frozen.

use crate::channel_layout::ChannelLayoutDescriptor;
use crate::report::{AnalysisReport, ComplianceProfile};
use crate::service::{
    ServiceAuthFailure, ServiceBoundaryFailure, ServiceConfig, ServiceRuntimeLimits, ServiceScope,
    ServiceSecurity, SERVICE_ANALYSIS_SCHEMA, SERVICE_ANALYSIS_SCHEMA_V3, SERVICE_HEALTH_SCHEMA,
};
use crate::service_metrics::{RequestTimer, ServiceMetrics, PROMETHEUS_CONTENT_TYPE};
use crate::service_runtime::{
    analyze_stable_input, ControlledAnalysisError, QuotaLease, RequestControl, ServiceRuntimeError,
    ServiceRuntimeErrorKind, UploadSpool, SERVICE_RESPONSE_WIRE_ALLOWANCE_BYTES,
};
use crate::stable_input::StableInputOptions;
use prost::Message;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};
use tokio::time;
use tonic::codegen::http;
use tonic::codegen::tokio_stream::Stream;
use tonic::codegen::{Body as HttpBody, Bytes};
use tonic::transport::server::Connected;
use tonic::transport::Server;
use tonic::{Code, Request, Response, Status};
use tower::{Layer, Service};

pub mod proto {
    tonic::include_proto!("forge.service.v1");
}

use proto::forge_analysis_server::{ForgeAnalysis, ForgeAnalysisServer};
use proto::forge_analysis_v3_server::{ForgeAnalysisV3, ForgeAnalysisV3Server};
use proto::forge_metrics_server::{ForgeMetrics, ForgeMetricsServer};
use proto::{
    AnalyzeRequest, AnalyzeResponse, AnalyzeV3Request, AnalyzeV3Response, CancelRequest,
    CancelResponse, HealthRequest, HealthResponse, MetricsRequest, MetricsResponse,
};

const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_FILENAME_BYTES: usize = 256;
const MAX_CONTENT_TYPE_BYTES: usize = 8 * 1024;
const MAX_PROFILE_BYTES: usize = 256;
const MAX_CHANNEL_LAYOUT_JSON_BYTES: usize = 256 * 1024;
const PROTOBUF_FIELD_OVERHEAD_BYTES: usize = 11;
const GRPC_HTTP2_STREAM_WINDOW_BYTES: u32 = 64 * 1024;
const GRPC_CONTROL_STREAM_HEADROOM: usize = 4;
const GRPC_CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const GRPC_CONNECTION_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Run the optional gRPC endpoint until the process receives Ctrl-C.
pub fn run(config: ServiceConfig, bind: SocketAddr) -> std::io::Result<()> {
    let config = effective_config(config, bind)?;
    let security = ServiceSecurity::from_legacy_config(&config).map_err(invalid_config)?;
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    run_internal(config, None, limits, security)
}

/// Run the optional gRPC endpoint with a shared metrics registry.
pub fn run_with_metrics(
    config: ServiceConfig,
    bind: SocketAddr,
    metrics: ServiceMetrics,
) -> std::io::Result<()> {
    let config = effective_config(config, bind)?;
    let security = ServiceSecurity::from_legacy_config(&config).map_err(invalid_config)?;
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    run_internal(config, Some(metrics), limits, security)
}

/// Run the gRPC endpoint with explicitly shared process-wide resource limits.
pub fn run_with_runtime_limits(
    config: ServiceConfig,
    bind: SocketAddr,
    limits: ServiceRuntimeLimits,
) -> std::io::Result<()> {
    let config = effective_config(config, bind)?;
    let security = ServiceSecurity::from_legacy_config(&config).map_err(invalid_config)?;
    run_internal(config, None, limits, security)
}

/// Run gRPC with metrics and explicitly shared process-wide resource limits.
pub fn run_with_metrics_and_runtime_limits(
    config: ServiceConfig,
    bind: SocketAddr,
    metrics: ServiceMetrics,
    limits: ServiceRuntimeLimits,
) -> std::io::Result<()> {
    let config = effective_config(config, bind)?;
    let security = ServiceSecurity::from_legacy_config(&config).map_err(invalid_config)?;
    run_internal(config, Some(metrics), limits, security)
}

/// Run gRPC with an explicit boundary and scoped-token policy. A legacy
/// `ServiceConfig::bearer_token`, when present, is merged as an all-scope
/// token for source-compatible upgrades.
pub fn run_with_security(
    config: ServiceConfig,
    bind: SocketAddr,
    security: ServiceSecurity,
) -> std::io::Result<()> {
    let config = effective_config_for_security(config, bind)?;
    let security = security
        .with_legacy_config(&config)
        .map_err(invalid_config)?;
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    run_internal(config, None, limits, security)
}

/// Run gRPC with explicit security and metrics.
pub fn run_with_security_and_metrics(
    config: ServiceConfig,
    bind: SocketAddr,
    security: ServiceSecurity,
    metrics: ServiceMetrics,
) -> std::io::Result<()> {
    let config = effective_config_for_security(config, bind)?;
    let security = security
        .with_legacy_config(&config)
        .map_err(invalid_config)?;
    let limits = ServiceRuntimeLimits::for_config(&config).map_err(invalid_config)?;
    run_internal(config, Some(metrics), limits, security)
}

/// Run gRPC with explicit security and shared process-wide limits.
pub fn run_with_security_and_runtime_limits(
    config: ServiceConfig,
    bind: SocketAddr,
    security: ServiceSecurity,
    limits: ServiceRuntimeLimits,
) -> std::io::Result<()> {
    let config = effective_config_for_security(config, bind)?;
    let security = security
        .with_legacy_config(&config)
        .map_err(invalid_config)?;
    run_internal(config, None, limits, security)
}

/// Run gRPC with explicit security, metrics, and shared process-wide limits.
pub fn run_with_security_metrics_and_runtime_limits(
    config: ServiceConfig,
    bind: SocketAddr,
    security: ServiceSecurity,
    metrics: ServiceMetrics,
    limits: ServiceRuntimeLimits,
) -> std::io::Result<()> {
    let config = effective_config_for_security(config, bind)?;
    let security = security
        .with_legacy_config(&config)
        .map_err(invalid_config)?;
    run_internal(config, Some(metrics), limits, security)
}

fn run_internal(
    config: ServiceConfig,
    metrics: Option<ServiceMetrics>,
    limits: ServiceRuntimeLimits,
    security: ServiceSecurity,
) -> std::io::Result<()> {
    // The legacy token has already been merged into `security` by the public
    // entry point. Drop its plaintext before the config is moved into the
    // long-lived runtime future and cloned into transport services.
    let config = config.without_legacy_token();
    config
        .validate_values_for_service()
        .map_err(invalid_config)?;
    security
        .validate_for_bind(config.bind)
        .map_err(invalid_config)?;
    let bind = config.bind;
    let v1_message_limit = grpc_analyze_v1_message_limit(&config).map_err(invalid_config)?;
    let v3_message_limit = grpc_analyze_v3_message_limit(&config).map_err(invalid_config)?;
    let connection_limit = grpc_connection_limit(&config).map_err(invalid_config)?;
    let stream_limit = grpc_stream_limit(&config).map_err(invalid_config)?;
    let connection_hard_max_age =
        grpc_connection_hard_max_age(GRPC_CONNECTION_MAX_AGE, config.timeout)
            .map_err(invalid_config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| std::io::Error::other(format!("tokio runtime: {error}")))?;
    runtime.block_on(async move {
        let service = GrpcService::with_runtime_limits_and_security(
            config.clone(),
            metrics,
            limits,
            security,
        );
        let transport_admission = GrpcTransportAdmissionLayer::new_with_metrics_and_security(
            Arc::clone(&service.permits),
            config.clone(),
            service.limits.clone(),
            service.metrics.clone(),
            Arc::clone(&service.security),
        );
        let listener = TcpListener::bind(bind).await?;
        let incoming = ConnectionLimitedIncoming::new(
            listener,
            connection_limit,
            config.timeout,
            GRPC_CONNECTION_IDLE_TIMEOUT,
            connection_hard_max_age,
        );
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        Server::builder()
            .timeout(config.timeout)
            // Tonic 0.14.6 can re-poll its completed max-age future after
            // starting graceful shutdown (upstream #2780), panicking whenever
            // an in-flight RPC keeps the connection alive. Until a released
            // tonic version contains that fix, do not configure either tonic
            // age option. `ConnectionPermitIo` enforces the documented hard
            // age directly at the IO boundary instead.
            .initial_stream_window_size(GRPC_HTTP2_STREAM_WINDOW_BYTES)
            // Keep receive credit bounded per accepted connection, independent
            // of workers × streams. Admission advances the body only until
            // the five-byte gRPC prefix is available before protobuf decoding
            // and byte reservation. HTTP/2 may deliver payload in that first
            // DATA frame too, but the fixed connection window bounds it.
            .initial_connection_window_size(GRPC_HTTP2_STREAM_WINDOW_BYTES)
            .max_concurrent_streams(stream_limit)
            .http2_max_header_list_size(64 * 1024)
            .layer(transport_admission)
            .add_service(
                ForgeAnalysisServer::new(service.clone())
                    .max_decoding_message_size(v1_message_limit),
            )
            .add_service(
                ForgeAnalysisV3Server::new(service.clone())
                    .max_decoding_message_size(v3_message_limit),
            )
            .add_service(ForgeMetricsServer::new(service))
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await
            .map_err(|error| std::io::Error::other(format!("gRPC server: {error}")))
    })
}

fn effective_config(mut config: ServiceConfig, bind: SocketAddr) -> std::io::Result<ServiceConfig> {
    config.bind = bind;
    config.validate().map_err(invalid_config)?;
    Ok(config)
}

fn effective_config_for_security(
    mut config: ServiceConfig,
    bind: SocketAddr,
) -> std::io::Result<ServiceConfig> {
    config.bind = bind;
    config
        .validate_values_for_service()
        .map_err(invalid_config)?;
    Ok(config)
}

#[cfg(test)]
fn grpc_decoding_message_limit(config: &ServiceConfig) -> Result<usize, String> {
    grpc_analyze_v3_message_limit(config)
}

fn grpc_analyze_v1_message_limit(config: &ServiceConfig) -> Result<usize, String> {
    config
        .max_body_bytes
        .checked_add(MAX_FILENAME_BYTES)
        .and_then(|bytes| bytes.checked_add(MAX_CONTENT_TYPE_BYTES))
        .and_then(|bytes| bytes.checked_add(MAX_PROFILE_BYTES))
        .and_then(|bytes| bytes.checked_add(MAX_REQUEST_ID_BYTES))
        .and_then(|bytes| bytes.checked_add(5 * PROTOBUF_FIELD_OVERHEAD_BYTES))
        .ok_or_else(|| "gRPC v1 protobuf message limit overflows usize".into())
}

fn grpc_analyze_v3_message_limit(config: &ServiceConfig) -> Result<usize, String> {
    config
        .max_body_bytes
        .checked_add(MAX_FILENAME_BYTES)
        .and_then(|bytes| bytes.checked_add(MAX_CONTENT_TYPE_BYTES))
        .and_then(|bytes| bytes.checked_add(MAX_PROFILE_BYTES))
        .and_then(|bytes| bytes.checked_add(MAX_REQUEST_ID_BYTES))
        .and_then(|bytes| bytes.checked_add(MAX_CHANNEL_LAYOUT_JSON_BYTES))
        .and_then(|bytes| bytes.checked_add(6 * PROTOBUF_FIELD_OVERHEAD_BYTES))
        .ok_or_else(|| "gRPC protobuf message limit overflows usize".into())
}

fn grpc_cancel_message_limit() -> usize {
    MAX_REQUEST_ID_BYTES + PROTOBUF_FIELD_OVERHEAD_BYTES
}

fn grpc_connection_limit(config: &ServiceConfig) -> Result<usize, String> {
    config
        .workers
        .checked_add(1)
        .ok_or_else(|| "gRPC connection limit overflows usize".into())
}

fn grpc_stream_limit(config: &ServiceConfig) -> Result<u32, String> {
    let streams = config
        .workers
        .checked_add(GRPC_CONTROL_STREAM_HEADROOM)
        .ok_or_else(|| "gRPC stream limit overflows usize".to_string())?;
    u32::try_from(streams).map_err(|_| "gRPC stream limit exceeds u32".into())
}

fn grpc_connection_hard_max_age(max_age: Duration, grace: Duration) -> Result<Duration, String> {
    max_age
        .checked_add(grace)
        .ok_or_else(|| "gRPC connection hard maximum age overflows Duration".into())
}

fn invalid_config(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

/// Limits accepted TCP connections before tonic/hyper can read HTTP/2 bodies.
///
/// The IO wrapper enforces an absolute HTTP/2 preface/first-header deadline, an
/// established-connection idle deadline, and a hard maximum age, returning its
/// permit immediately when any expires. Tonic 0.14.6's max-age implementation
/// is not used because its graceful path can panic with an in-flight RPC; the
/// IO deadline is consequently a hard close, not a GOAWAY/drain promise. This
/// complements request admission:
/// at most `workers` Analyze streams can wait for framing or decode across all
/// accepted connections. Each stream has a 64 KiB window and all streams on one
/// connection share one 64 KiB connection window, so receive credit does not
/// multiply by stream count.
type ConnectionPermitFuture =
    Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

struct ConnectionLimitedIncoming {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    acquire: Option<ConnectionPermitFuture>,
    permit: Option<OwnedSemaphorePermit>,
    handshake_header_timeout: Duration,
    idle_timeout: Duration,
    max_age: Duration,
}

impl ConnectionLimitedIncoming {
    fn new(
        listener: TcpListener,
        limit: usize,
        handshake_header_timeout: Duration,
        idle_timeout: Duration,
        max_age: Duration,
    ) -> Self {
        debug_assert!(!handshake_header_timeout.is_zero());
        debug_assert!(!idle_timeout.is_zero());
        debug_assert!(!max_age.is_zero());
        Self {
            listener,
            permits: Arc::new(Semaphore::new(limit)),
            acquire: None,
            permit: None,
            handshake_header_timeout,
            idle_timeout,
            max_age,
        }
    }
}

impl Stream for ConnectionLimitedIncoming {
    type Item = Result<ConnectionPermitIo, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.permit.is_none() {
            if self.acquire.is_none() {
                self.acquire = Some(Box::pin(Arc::clone(&self.permits).acquire_owned()));
            }
            let acquire = self.acquire.as_mut().expect("acquire future was installed");
            match acquire.as_mut().poll(context) {
                Poll::Ready(Ok(permit)) => {
                    self.acquire = None;
                    self.permit = Some(permit);
                }
                Poll::Ready(Err(_)) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }

        match self.listener.poll_accept(context) {
            Poll::Ready(Ok((stream, peer_addr))) => {
                let permit = self
                    .permit
                    .take()
                    .expect("accepted connection owns a permit");
                Poll::Ready(Some(Ok(ConnectionPermitIo::new(
                    stream,
                    peer_addr.ip(),
                    permit,
                    self.handshake_header_timeout,
                    self.idle_timeout,
                    self.max_age,
                ))))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Some(Err(error))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
struct ConnectionLifecycle {
    headers_observed: AtomicBool,
}

impl ConnectionLifecycle {
    fn mark_headers_observed(&self) {
        self.headers_observed.store(true, Ordering::Release);
    }

    fn headers_observed(&self) -> bool {
        self.headers_observed.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug)]
struct GrpcConnectionInfo {
    lifecycle: Arc<ConnectionLifecycle>,
    peer_ip: IpAddr,
}

struct ConnectionPermitIo {
    stream: TcpStream,
    peer_ip: IpAddr,
    permit: Option<OwnedSemaphorePermit>,
    lifecycle: Arc<ConnectionLifecycle>,
    handshake_header_deadline: Pin<Box<time::Sleep>>,
    idle_deadline: Pin<Box<time::Sleep>>,
    max_age_deadline: Pin<Box<time::Sleep>>,
    idle_timeout: Duration,
    terminal_error: Option<io::ErrorKind>,
}

impl ConnectionPermitIo {
    fn new(
        stream: TcpStream,
        peer_ip: IpAddr,
        permit: OwnedSemaphorePermit,
        handshake_header_timeout: Duration,
        idle_timeout: Duration,
        max_age: Duration,
    ) -> Self {
        let now = time::Instant::now();
        Self {
            stream,
            peer_ip,
            permit: Some(permit),
            lifecycle: Arc::new(ConnectionLifecycle {
                headers_observed: AtomicBool::new(false),
            }),
            handshake_header_deadline: Box::pin(time::sleep_until(now + handshake_header_timeout)),
            idle_deadline: Box::pin(time::sleep_until(now + idle_timeout)),
            max_age_deadline: Box::pin(time::sleep_until(now + max_age)),
            idle_timeout,
            terminal_error: None,
        }
    }

    fn timeout_error(kind: io::ErrorKind) -> io::Error {
        let message = match kind {
            io::ErrorKind::TimedOut => {
                "gRPC connection handshake/header, idle, or maximum-age deadline exceeded"
            }
            _ => "gRPC connection is closed",
        };
        io::Error::new(kind, message)
    }

    fn poll_deadline(&mut self, context: &mut Context<'_>) -> Option<io::Error> {
        if let Some(kind) = self.terminal_error {
            return Some(Self::timeout_error(kind));
        }
        let handshake_expired = !self.lifecycle.headers_observed()
            && self
                .handshake_header_deadline
                .as_mut()
                .poll(context)
                .is_ready();
        let idle_expired = self.idle_deadline.as_mut().poll(context).is_ready();
        let max_age_expired = self.max_age_deadline.as_mut().poll(context).is_ready();
        if handshake_expired || idle_expired || max_age_expired {
            // Return connection capacity at the deadline even if the HTTP/2
            // driver retains this IO value briefly while unwinding its future.
            self.permit.take();
            self.terminal_error = Some(io::ErrorKind::TimedOut);
            return Some(Self::timeout_error(io::ErrorKind::TimedOut));
        }
        None
    }

    fn observe_activity(&mut self) {
        self.idle_deadline
            .as_mut()
            .reset(time::Instant::now() + self.idle_timeout);
    }
}

impl AsyncRead for ConnectionPermitIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        if let Some(error) = this.poll_deadline(context) {
            return Poll::Ready(Err(error));
        }
        let filled = buffer.filled().len();
        match Pin::new(&mut this.stream).poll_read(context, buffer) {
            Poll::Ready(Ok(())) if buffer.filled().len() > filled => {
                this.observe_activity();
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for ConnectionPermitIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.as_mut().get_mut();
        if let Some(error) = this.poll_deadline(context) {
            return Poll::Ready(Err(error));
        }
        match Pin::new(&mut this.stream).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) if written != 0 => {
                this.observe_activity();
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let this = self.as_mut().get_mut();
        if let Some(error) = this.poll_deadline(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

impl Connected for ConnectionPermitIo {
    type ConnectInfo = GrpcConnectionInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        GrpcConnectionInfo {
            lifecycle: Arc::clone(&self.lifecycle),
            peer_ip: self.peer_ip,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrpcMethodKind {
    Analyze,
    Control,
}

#[derive(Clone, Copy, Debug)]
struct GrpcMethodPolicy {
    kind: GrpcMethodKind,
    max_message_bytes: usize,
    scope: ServiceScope,
}

/// Private marker inserted only after the transport boundary has verified the
/// bearer digest for this exact method scope. The decoded handler uses it to
/// avoid hashing and scanning the same credential a second time. Direct trait
/// calls do not carry this marker and therefore retain the fallback check.
#[derive(Clone, Copy)]
struct GrpcVerifiedAuth(ServiceScope);

fn grpc_method_policy(
    path: &str,
    config: &ServiceConfig,
) -> Result<Option<GrpcMethodPolicy>, String> {
    let analyze_v1 = || {
        grpc_analyze_v1_message_limit(config).map(|max_message_bytes| GrpcMethodPolicy {
            kind: GrpcMethodKind::Analyze,
            max_message_bytes,
            scope: ServiceScope::Analyze,
        })
    };
    let analyze_v3 = || {
        grpc_analyze_v3_message_limit(config).map(|max_message_bytes| GrpcMethodPolicy {
            kind: GrpcMethodKind::Analyze,
            max_message_bytes,
            scope: ServiceScope::Analyze,
        })
    };
    let control = |max_message_bytes, scope| {
        Ok(GrpcMethodPolicy {
            kind: GrpcMethodKind::Control,
            max_message_bytes,
            scope,
        })
    };
    match path {
        "/forge.service.v1.ForgeAnalysis/Analyze" => analyze_v1().map(Some),
        "/forge.service.v1.ForgeAnalysisV3/Analyze" => analyze_v3().map(Some),
        "/forge.service.v1.ForgeAnalysis/Cancel" | "/forge.service.v1.ForgeAnalysisV3/Cancel" => {
            control(grpc_cancel_message_limit(), ServiceScope::Cancel).map(Some)
        }
        "/forge.service.v1.ForgeAnalysis/Health" | "/forge.service.v1.ForgeAnalysisV3/Health" => {
            control(0, ServiceScope::Health).map(Some)
        }
        "/forge.service.v1.ForgeMetrics/Metrics" => control(0, ServiceScope::Metrics).map(Some),
        _ => Ok(None),
    }
}

#[derive(Clone)]
struct GrpcTransportAdmissionLayer {
    analysis_permits: Arc<Semaphore>,
    control_permits: Arc<Semaphore>,
    config: Arc<ServiceConfig>,
    limits: ServiceRuntimeLimits,
    metrics: Option<ServiceMetrics>,
    security: Arc<ServiceSecurity>,
}

impl GrpcTransportAdmissionLayer {
    #[cfg(test)]
    fn new(
        analysis_permits: Arc<Semaphore>,
        config: ServiceConfig,
        limits: ServiceRuntimeLimits,
    ) -> Self {
        Self::new_with_metrics(analysis_permits, config, limits, None)
    }

    #[cfg(test)]
    fn new_with_metrics(
        analysis_permits: Arc<Semaphore>,
        config: ServiceConfig,
        limits: ServiceRuntimeLimits,
        metrics: Option<ServiceMetrics>,
    ) -> Self {
        let security = ServiceSecurity::from_legacy_config(&config)
            .expect("GrpcTransportAdmission test/internal construction requires valid security");
        Self::new_with_metrics_and_security(
            analysis_permits,
            config,
            limits,
            metrics,
            Arc::new(security),
        )
    }

    fn new_with_metrics_and_security(
        analysis_permits: Arc<Semaphore>,
        config: ServiceConfig,
        limits: ServiceRuntimeLimits,
        metrics: Option<ServiceMetrics>,
        security: Arc<ServiceSecurity>,
    ) -> Self {
        Self {
            analysis_permits,
            control_permits: Arc::new(Semaphore::new(GRPC_CONTROL_STREAM_HEADROOM)),
            config: Arc::new(config),
            limits,
            metrics,
            security,
        }
    }
}

impl<S> Layer<S> for GrpcTransportAdmissionLayer {
    type Service = GrpcTransportAdmission<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcTransportAdmission {
            inner,
            analysis_permits: Arc::clone(&self.analysis_permits),
            control_permits: Arc::clone(&self.control_permits),
            config: Arc::clone(&self.config),
            limits: self.limits.clone(),
            metrics: self.metrics.clone(),
            security: Arc::clone(&self.security),
        }
    }
}

#[derive(Clone)]
struct GrpcTransportAdmission<S> {
    inner: S,
    analysis_permits: Arc<Semaphore>,
    control_permits: Arc<Semaphore>,
    config: Arc<ServiceConfig>,
    limits: ServiceRuntimeLimits,
    metrics: Option<ServiceMetrics>,
    security: Arc<ServiceSecurity>,
}

struct GrpcTransportResources {
    kind: GrpcMethodKind,
    memory_lease: QuotaLease,
    permit: OwnedSemaphorePermit,
    control: RequestControl,
}

#[derive(Clone)]
struct GrpcTransportResourcesExtension(Arc<Mutex<Option<GrpcTransportResources>>>);

impl GrpcTransportResourcesExtension {
    fn new(resources: GrpcTransportResources) -> Self {
        Self(Arc::new(Mutex::new(Some(resources))))
    }

    fn take(&self) -> Option<GrpcTransportResources> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// One request timer shared by transport admission and the decoded handler.
/// Taking the inner timer is atomic under the mutex, so exactly one boundary
/// records the terminal outcome even when timeout, future drop, and handler
/// completion race.
#[derive(Clone, Default)]
struct SharedGrpcRequestTimer(Option<Arc<Mutex<Option<RequestTimer>>>>);

impl SharedGrpcRequestTimer {
    fn start(metrics: Option<&ServiceMetrics>, headers: &http::HeaderMap) -> Self {
        let Some(metrics) = metrics else {
            return Self::default();
        };
        let mut timer = metrics.start_grpc_request();
        timer.set_traceparent(
            headers
                .get("traceparent")
                .and_then(|value| value.to_str().ok()),
        );
        Self(Some(Arc::new(Mutex::new(Some(timer)))))
    }

    fn finish(&self, status_code: u16, request_bytes: u64) {
        let Some(timer) = &self.0 else {
            return;
        };
        if let Some(timer) = timer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            timer.finish(status_code, request_bytes);
        }
    }
}

/// Records cancellation when the transport future disappears before it can
/// return a response.
///
/// This guard remains outside both the five-byte prefix read and tonic's
/// protobuf decoder. The shared timer's take-once slot makes its drop race with
/// handler completion and explicit timeout/error paths deterministic. Request
/// body, permit, and quota values remain ordinary RAII locals and are released
/// by the same future drop.
struct OuterGrpcRequestTimer {
    timer: SharedGrpcRequestTimer,
    request_bytes: u64,
}

impl OuterGrpcRequestTimer {
    fn new(timer: SharedGrpcRequestTimer) -> Self {
        Self {
            timer,
            request_bytes: 0,
        }
    }

    fn set_request_bytes(&mut self, request_bytes: u64) {
        self.request_bytes = request_bytes;
    }
}

impl Drop for OuterGrpcRequestTimer {
    fn drop(&mut self) {
        let status_code = if std::thread::panicking() { 500 } else { 499 };
        self.timer.finish(status_code, self.request_bytes);
    }
}

#[derive(Clone)]
struct GrpcRequestTimerExtension(SharedGrpcRequestTimer);

#[derive(Clone, Default)]
struct GrpcResponseLeaseSlot(Arc<Mutex<Option<QuotaLease>>>);

impl GrpcResponseLeaseSlot {
    fn store(&self, lease: QuotaLease) -> Result<(), Status> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| Status::internal("gRPC response resource slot is unavailable"))?;
        if slot.is_some() {
            return Err(Status::internal(
                "gRPC response resource slot was already populated",
            ));
        }
        *slot = Some(lease);
        Ok(())
    }

    fn take(&self) -> Option<QuotaLease> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

struct RetainedGrpcBody {
    inner: tonic::body::Body,
    memory_lease: Option<QuotaLease>,
}

impl HttpBody for RetainedGrpcBody {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(None) => {
                self.memory_lease.take();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.memory_lease.take();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

struct ReplayGrpcBody {
    prefix: VecDeque<Bytes>,
    remaining_data_bytes: usize,
    inner: tonic::body::Body,
}

impl HttpBody for ReplayGrpcBody {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if let Some(bytes) = self.prefix.pop_front() {
            return Poll::Ready(Some(Ok(http_body::Frame::data(bytes))));
        }
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(bytes) if bytes.len() <= self.remaining_data_bytes => {
                    self.remaining_data_bytes -= bytes.len();
                    Poll::Ready(Some(Ok(http_body::Frame::data(bytes))))
                }
                Ok(_) => Poll::Ready(Some(Err(Status::invalid_argument(
                    "gRPC unary request contains bytes beyond its declared message",
                )))),
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_empty() && self.inner.is_end_stream()
    }
}

async fn read_grpc_frame_header(
    body: &mut tonic::body::Body,
) -> Result<(VecDeque<Bytes>, usize, usize), Status> {
    let mut prefix = VecDeque::new();
    let mut header = [0_u8; 5];
    let mut copied = 0_usize;
    while copied < header.len() {
        let frame = std::future::poll_fn(|context| Pin::new(&mut *body).poll_frame(context))
            .await
            .ok_or_else(|| Status::invalid_argument("truncated gRPC message header"))??;
        let bytes = frame
            .into_data()
            .map_err(|_| Status::invalid_argument("gRPC trailers preceded the message header"))?;
        if bytes.is_empty() {
            continue;
        }
        let needed = header.len() - copied;
        let take = needed.min(bytes.len());
        header[copied..copied + take].copy_from_slice(&bytes[..take]);
        copied += take;
        prefix.push_back(bytes);
    }
    if header[0] != 0 {
        return Err(Status::unimplemented(
            "compressed gRPC requests are not accepted",
        ));
    }
    let prefix_bytes = prefix.iter().try_fold(0_usize, |total, bytes| {
        total
            .checked_add(bytes.len())
            .ok_or_else(|| Status::out_of_range("gRPC frame prefix length exceeds usize"))
    })?;
    Ok((
        prefix,
        prefix_bytes,
        u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize,
    ))
}

fn forwarded_proto(headers: &http::HeaderMap) -> (usize, Option<&[u8]>) {
    let mut values = headers.get_all("x-forwarded-proto").iter();
    let first = values.next().map(http::HeaderValue::as_bytes);
    (
        usize::from(first.is_some()) + usize::from(values.next().is_some()),
        first,
    )
}

fn validate_grpc_peer(
    headers: &http::HeaderMap,
    config: &ServiceConfig,
    security: &ServiceSecurity,
    connection: Option<&GrpcConnectionInfo>,
) -> Result<(), Status> {
    let (forwarded_proto_count, forwarded_proto) = forwarded_proto(headers);
    security
        .validate_peer(
            config.bind,
            connection.map(|connection| connection.peer_ip),
            forwarded_proto_count,
            forwarded_proto,
        )
        .map_err(|ServiceBoundaryFailure::TrustedProxyRequired| {
            Status::failed_precondition("a trusted proxy HTTPS marker is required")
        })
}

fn authorize_grpc_headers(
    headers: &http::HeaderMap,
    security: &ServiceSecurity,
    scope: ServiceScope,
) -> Result<(), Status> {
    let actual = grpc_authorization_header(headers)?;
    security
        .authorize_header(actual, scope)
        .map_err(grpc_auth_status)
}

fn grpc_authorization_header(headers: &http::HeaderMap) -> Result<Option<&[u8]>, Status> {
    let mut values = headers.get_all(http::header::AUTHORIZATION).iter();
    let actual = values.next().map(http::HeaderValue::as_bytes);
    if values.next().is_some() {
        return Err(Status::invalid_argument(
            "duplicate authorization metadata is not accepted",
        ));
    }
    Ok(actual)
}

fn grpc_metadata_authorization_header(
    metadata: &tonic::metadata::MetadataMap,
) -> Result<Option<&[u8]>, Status> {
    let mut values = metadata.get_all("authorization").iter();
    let actual = values.next().map(tonic::metadata::MetadataValue::as_bytes);
    if values.next().is_some() {
        return Err(Status::invalid_argument(
            "duplicate authorization metadata is not accepted",
        ));
    }
    Ok(actual)
}

fn grpc_auth_status(error: ServiceAuthFailure) -> Status {
    match error {
        ServiceAuthFailure::Invalid => Status::unauthenticated("a valid bearer token is required"),
        ServiceAuthFailure::InsufficientScope => {
            Status::permission_denied("the bearer token does not grant this endpoint scope")
        }
    }
}

fn grpc_code_status_code(code: Code) -> u16 {
    match code {
        Code::Ok => 200,
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => 400,
        Code::Unauthenticated => 401,
        Code::PermissionDenied => 403,
        Code::NotFound => 404,
        Code::AlreadyExists | Code::Aborted => 409,
        Code::ResourceExhausted => 429,
        Code::Cancelled => 499,
        Code::DeadlineExceeded => 504,
        Code::Unavailable => 503,
        Code::Internal | Code::Unknown | Code::DataLoss => 500,
        Code::Unimplemented => 501,
    }
}

fn metered_grpc_error(
    timer: &SharedGrpcRequestTimer,
    status: Status,
    request_bytes: u64,
) -> http::Response<tonic::body::Body> {
    timer.finish(grpc_code_status_code(status.code()), request_bytes);
    status.into_http()
}

fn grpc_http_response_status(response: &http::Response<tonic::body::Body>) -> u16 {
    let code = response
        .headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i32>().ok())
        .map(Code::from_i32)
        .unwrap_or(Code::Ok);
    grpc_code_status_code(code)
}

fn retain_grpc_response_memory(
    response: http::Response<tonic::body::Body>,
    memory_lease: Option<QuotaLease>,
) -> http::Response<tonic::body::Body> {
    let (parts, body) = response.into_parts();
    http::Response::from_parts(
        parts,
        tonic::body::Body::new(RetainedGrpcBody {
            inner: body,
            memory_lease,
        }),
    )
}

impl<S> Service<http::Request<tonic::body::Body>> for GrpcTransportAdmission<S>
where
    S: Service<http::Request<tonic::body::Body>, Response = http::Response<tonic::body::Body>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        if let Some(connection) = request.extensions().get::<GrpcConnectionInfo>() {
            // Reaching the outer route layer proves that hyper completed the
            // HTTP/2 preface and one request-header block. The IO wrapper can
            // now replace its absolute handshake/header deadline with the
            // established-connection idle deadline.
            connection.lifecycle.mark_headers_observed();
        }
        let policy = grpc_method_policy(request.uri().path(), &self.config);
        let analysis_permits = Arc::clone(&self.analysis_permits);
        let control_permits = Arc::clone(&self.control_permits);
        let config = Arc::clone(&self.config);
        let limits = self.limits.clone();
        let metrics = self.metrics.clone();
        let security = Arc::clone(&self.security);
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let policy = match policy {
                Ok(Some(policy)) => policy,
                Ok(None) => {
                    let connection = request.extensions().get::<GrpcConnectionInfo>();
                    if let Err(status) =
                        validate_grpc_peer(request.headers(), &config, &security, connection)
                    {
                        return Ok(status.into_http());
                    }
                    // Unknown methods are rejected at the transport boundary
                    // instead of being handed to tonic with an all-scope
                    // fallback. This keeps a newly added RPC fail-closed if
                    // its method policy is not updated alongside the route.
                    return Ok(Status::unimplemented("gRPC method is not supported").into_http());
                }
                Err(error) => return Ok(Status::internal(error).into_http()),
            };
            let timer = SharedGrpcRequestTimer::start(metrics.as_ref(), request.headers());
            let mut outer_timer = OuterGrpcRequestTimer::new(timer.clone());
            let mut request_bytes = 0_u64;
            let connection = request.extensions().get::<GrpcConnectionInfo>();
            if let Err(status) =
                validate_grpc_peer(request.headers(), &config, &security, connection)
            {
                return Ok(metered_grpc_error(&timer, status, request_bytes));
            }
            if let Err(status) = authorize_grpc_headers(request.headers(), &security, policy.scope)
            {
                return Ok(metered_grpc_error(&timer, status, request_bytes));
            }
            // Keep the successful transport decision with the request so the
            // decoded handler can prove that this same method scope was
            // admitted without repeating the digest scan.
            let mut request = request;
            request
                .extensions_mut()
                .insert(GrpcVerifiedAuth(policy.scope));
            let control = match RequestControl::from_timeout(config.timeout) {
                Ok(control) => control,
                Err(error) => {
                    return Ok(metered_grpc_error(
                        &timer,
                        runtime_status(error),
                        request_bytes,
                    ))
                }
            };
            // Bound requests waiting even for the first five protobuf framing
            // bytes. This global admission is shared by every route service and
            // connection, so HEADERS-only streams cannot grow as
            // connections × max_concurrent_streams.
            let permits = match policy.kind {
                GrpcMethodKind::Analyze => analysis_permits,
                GrpcMethodKind::Control => control_permits,
            };
            let permit = match permits.try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    if let Some(metrics) = &metrics {
                        metrics.record_busy();
                    }
                    return Ok(metered_grpc_error(
                        &timer,
                        Status::resource_exhausted("service is busy"),
                        request_bytes,
                    ));
                }
            };
            let (mut parts, mut body) = request.into_parts();
            let remaining = match control.remaining() {
                Ok(remaining) => remaining,
                Err(error) => {
                    return Ok(metered_grpc_error(
                        &timer,
                        runtime_status(error),
                        request_bytes,
                    ))
                }
            };
            let (prefix, prefix_bytes, declared_bytes) =
                match time::timeout(remaining, read_grpc_frame_header(&mut body)).await {
                    Ok(Ok(header)) => header,
                    Ok(Err(status)) => {
                        return Ok(metered_grpc_error(&timer, status, request_bytes))
                    }
                    Err(_) => {
                        control.expire();
                        return Ok(metered_grpc_error(
                            &timer,
                            Status::deadline_exceeded("gRPC request timed out"),
                            request_bytes,
                        ));
                    }
                };
            if let Err(error) = control.check() {
                return Ok(metered_grpc_error(
                    &timer,
                    runtime_status(error),
                    request_bytes,
                ));
            };
            request_bytes = declared_bytes as u64;
            outer_timer.set_request_bytes(request_bytes);
            if declared_bytes > policy.max_message_bytes {
                return Ok(metered_grpc_error(
                    &timer,
                    Status::resource_exhausted("protobuf request message exceeds the method limit"),
                    request_bytes,
                ));
            }
            let declared_frame_bytes = declared_bytes
                .checked_add(5)
                .expect("a u32 gRPC frame length plus its header fits usize");
            if prefix_bytes > declared_frame_bytes {
                return Ok(metered_grpc_error(
                    &timer,
                    Status::invalid_argument(
                        "gRPC unary request contains bytes beyond its declared message",
                    ),
                    request_bytes,
                ));
            }
            let declared_bytes = match u64::try_from(declared_bytes) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Ok(metered_grpc_error(
                        &timer,
                        Status::out_of_range("protobuf message length exceeds u64"),
                        request_bytes,
                    ))
                }
            };
            let memory_lease = match limits.governor().reserve_memory(declared_bytes) {
                Ok(lease) => lease,
                Err(error) => {
                    return Ok(metered_grpc_error(
                        &timer,
                        runtime_status(error),
                        request_bytes,
                    ))
                }
            };
            let response_lease = GrpcResponseLeaseSlot::default();
            parts
                .extensions
                .insert(GrpcRequestTimerExtension(timer.clone()));
            parts.extensions.insert(response_lease.clone());
            parts
                .extensions
                .insert(GrpcTransportResourcesExtension::new(
                    GrpcTransportResources {
                        kind: policy.kind,
                        memory_lease,
                        permit,
                        control: control.clone(),
                    },
                ));
            let request = http::Request::from_parts(
                parts,
                tonic::body::Body::new(ReplayGrpcBody {
                    prefix,
                    remaining_data_bytes: declared_frame_bytes - prefix_bytes,
                    inner: body,
                }),
            );
            let remaining = match control.remaining() {
                Ok(remaining) => remaining,
                Err(error) => {
                    return Ok(metered_grpc_error(
                        &timer,
                        runtime_status(error),
                        request_bytes,
                    ))
                }
            };
            let call = inner.call(request);
            tokio::pin!(call);
            let deadline = time::sleep(remaining);
            tokio::pin!(deadline);
            tokio::select! {
                biased;
                result = &mut call => match result {
                    Ok(response) => {
                        timer.finish(grpc_http_response_status(&response), request_bytes);
                        Ok(retain_grpc_response_memory(response, response_lease.take()))
                    }
                    Err(error) => {
                        timer.finish(500, request_bytes);
                        Err(error)
                    }
                },
                () = &mut deadline => {
                    control.expire();
                    timer.finish(504, request_bytes);
                    Ok(Status::deadline_exceeded("gRPC request timed out").into_http())
                }
            }
        })
    }
}

#[derive(Default)]
struct RequestRegistry {
    active: HashMap<String, RequestControl>,
}

#[derive(Clone)]
struct GrpcService {
    config: Arc<ServiceConfig>,
    security: Arc<ServiceSecurity>,
    limits: ServiceRuntimeLimits,
    registry: Arc<Mutex<RequestRegistry>>,
    permits: Arc<Semaphore>,
    metrics: Option<ServiceMetrics>,
    #[cfg(test)]
    analysis_worker_hook: AnalysisWorkerHook,
}

impl GrpcService {
    #[cfg(test)]
    fn new(config: ServiceConfig, metrics: Option<ServiceMetrics>) -> Self {
        let limits = ServiceRuntimeLimits::for_config(&config)
            .expect("GrpcService test/internal construction requires valid limits");
        Self::with_runtime_limits(config, metrics, limits)
    }

    #[cfg(test)]
    fn with_runtime_limits(
        config: ServiceConfig,
        metrics: Option<ServiceMetrics>,
        limits: ServiceRuntimeLimits,
    ) -> Self {
        let security = ServiceSecurity::from_legacy_config(&config)
            .expect("GrpcService test/internal construction requires valid security");
        Self::with_runtime_limits_and_security(config, metrics, limits, security)
    }

    fn with_runtime_limits_and_security(
        config: ServiceConfig,
        metrics: Option<ServiceMetrics>,
        limits: ServiceRuntimeLimits,
        security: ServiceSecurity,
    ) -> Self {
        let config = config.without_legacy_token();
        Self {
            permits: Arc::new(Semaphore::new(config.workers)),
            config: Arc::new(config),
            security: Arc::new(security),
            limits,
            registry: Arc::new(Mutex::new(RequestRegistry::default())),
            metrics,
            #[cfg(test)]
            analysis_worker_hook: AnalysisWorkerHook::default(),
        }
    }

    fn authorize<T>(&self, request: &Request<T>, scope: ServiceScope) -> Result<(), Status> {
        if let Some(verified) = request.extensions().get::<GrpcVerifiedAuth>() {
            if verified.0 == scope {
                return Ok(());
            }
            return Err(Status::internal(
                "gRPC transport authorization scope does not match the RPC method",
            ));
        }
        let actual = grpc_metadata_authorization_header(request.metadata())?;
        self.security
            .authorize_header(actual, scope)
            .map_err(|error| match error {
                ServiceAuthFailure::Invalid => {
                    Status::unauthenticated("a valid bearer token is required")
                }
                ServiceAuthFailure::InsufficientScope => {
                    Status::permission_denied("the bearer token does not grant this endpoint scope")
                }
            })
    }

    fn take_transport_resources<T>(
        &self,
        request: &mut Request<T>,
        expected: GrpcMethodKind,
    ) -> Result<Option<GrpcTransportResources>, Status> {
        let Some(extension) = request
            .extensions_mut()
            .remove::<GrpcTransportResourcesExtension>()
        else {
            // Direct trait invocations used by existing embedders/tests do not
            // pass through the HTTP/2 transport layer. They retain the same
            // bounded fallback in `analyze_request`.
            return Ok(None);
        };
        let resources = extension
            .take()
            .ok_or_else(|| Status::internal("gRPC transport admission was already consumed"))?;
        if resources.kind != expected {
            return Err(Status::internal(
                "gRPC transport admission does not match the RPC method",
            ));
        }
        Ok(Some(resources))
    }

    #[cfg(test)]
    fn register(&self, request_id: &str) -> Result<RequestControl, Status> {
        let control = RequestControl::from_timeout(self.config.timeout).map_err(runtime_status)?;
        self.register_control(request_id, control)
    }

    fn register_control(
        &self,
        request_id: &str,
        control: RequestControl,
    ) -> Result<RequestControl, Status> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| Status::internal("cancellation registry is unavailable"))?;
        if registry.active.contains_key(request_id) {
            return Err(Status::already_exists("request_id is already active"));
        }
        registry
            .active
            .insert(request_id.to_owned(), control.clone());
        Ok(control)
    }

    fn cancel(&self, request_id: &str) -> Result<bool, Status> {
        validate_request_id(request_id)?;
        let registry = self
            .registry
            .lock()
            .map_err(|_| Status::internal("cancellation registry is unavailable"))?;
        if let Some(control) = registry.active.get(request_id) {
            // Preserve the original wire meaning: `cancelled` reports that an
            // active request ID was found and cancellation was requested. The
            // control itself keeps its first terminal reason if this is a
            // repeated Cancel or the deadline already won the race.
            control.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn worker_lease(
        &self,
        request_id: String,
        control: RequestControl,
        permit: OwnedSemaphorePermit,
    ) -> AnalysisWorkerLease {
        AnalysisWorkerLease {
            registry: Arc::clone(&self.registry),
            request_id,
            control,
            _permit: permit,
        }
    }
}

#[tonic::async_trait]
impl ForgeAnalysis for GrpcService {
    async fn analyze(
        &self,
        request: Request<AnalyzeRequest>,
    ) -> Result<Response<AnalyzeResponse>, Status> {
        let request_bytes = request.get_ref().audio.len() as u64;
        let timer = AnalyzeRequestTimer::new(self.start_timer(&request), request_bytes);
        let result = self.analyze_inner(request).await;
        timer.finish(status_code(&result));
        result
    }

    async fn cancel(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let timer = self.start_timer(&request);
        let result = self.cancel_inner(request).await;
        timer.finish(status_code(&result), 0);
        result
    }

    async fn health(
        &self,
        request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let timer = self.start_timer(&request);
        let result = self.health_inner(request).await;
        timer.finish(status_code(&result), 0);
        result
    }
}

#[tonic::async_trait]
impl ForgeAnalysisV3 for GrpcService {
    async fn analyze(
        &self,
        request: Request<AnalyzeV3Request>,
    ) -> Result<Response<AnalyzeV3Response>, Status> {
        let request_bytes = request.get_ref().audio.len() as u64;
        let timer = AnalyzeRequestTimer::new(self.start_timer(&request), request_bytes);
        let result = self.analyze_v3_inner(request).await;
        timer.finish(status_code(&result));
        result
    }

    async fn cancel(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let timer = self.start_timer(&request);
        let result = self.cancel_inner(request).await;
        timer.finish(status_code(&result), 0);
        result
    }

    async fn health(
        &self,
        request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let timer = self.start_timer(&request);
        let result = self.health_inner(request).await;
        timer.finish(status_code(&result), 0);
        result
    }
}

#[tonic::async_trait]
impl ForgeMetrics for GrpcService {
    async fn metrics(
        &self,
        request: Request<MetricsRequest>,
    ) -> Result<Response<MetricsResponse>, Status> {
        let timer = self.start_timer(&request);
        let result = self.metrics_inner(request).await;
        timer.finish(status_code(&result), 0);
        result
    }
}

impl GrpcService {
    async fn analyze_inner(
        &self,
        mut request: Request<AnalyzeRequest>,
    ) -> Result<Response<AnalyzeResponse>, Status> {
        let response_lease = request.extensions().get::<GrpcResponseLeaseSlot>().cloned();
        self.authorize(&request, ServiceScope::Analyze)?;
        let transport_resources =
            self.take_transport_resources(&mut request, GrpcMethodKind::Analyze)?;
        let message_bytes = request.get_ref().encoded_len();
        if message_bytes > grpc_analyze_v1_message_limit(&self.config).map_err(Status::internal)? {
            return Err(Status::resource_exhausted(
                "protobuf request message exceeds the configured limit",
            ));
        }
        let request = request.into_inner();
        let result = self
            .analyze_request(
                AnalyzeInput {
                    audio: request.audio,
                    filename: request.filename,
                    content_type: request.content_type,
                    profile: request.profile,
                    request_id: request.request_id,
                    channel_layout_json: None,
                },
                message_bytes,
                transport_resources,
            )
            .await?;
        let (message, lease) = result.into_v1();
        response_with_memory_lease(message, lease, response_lease)
    }

    async fn analyze_v3_inner(
        &self,
        mut request: Request<AnalyzeV3Request>,
    ) -> Result<Response<AnalyzeV3Response>, Status> {
        let response_lease = request.extensions().get::<GrpcResponseLeaseSlot>().cloned();
        self.authorize(&request, ServiceScope::Analyze)?;
        let transport_resources =
            self.take_transport_resources(&mut request, GrpcMethodKind::Analyze)?;
        let message_bytes = request.get_ref().encoded_len();
        if message_bytes > grpc_analyze_v3_message_limit(&self.config).map_err(Status::internal)? {
            return Err(Status::resource_exhausted(
                "protobuf request message exceeds the configured limit",
            ));
        }
        let request = request.into_inner();
        let result = self
            .analyze_request(
                AnalyzeInput {
                    audio: request.audio,
                    filename: request.filename,
                    content_type: request.content_type,
                    profile: request.profile,
                    request_id: request.request_id,
                    channel_layout_json: Some(request.channel_layout_json),
                },
                message_bytes,
                transport_resources,
            )
            .await?;
        let (message, lease) = result.into_v3();
        response_with_memory_lease(message, lease, response_lease)
    }

    async fn analyze_request(
        &self,
        request: AnalyzeInput,
        message_bytes: usize,
        transport_resources: Option<GrpcTransportResources>,
    ) -> Result<AnalysisResult, Status> {
        // Network requests inherit the absolute deadline created before the
        // frame header and protobuf body were read. Direct trait calls create
        // the same deadline here for backwards-compatible embedding.
        let control = transport_resources
            .as_ref()
            .map(|resources| resources.control.clone())
            .map_or_else(|| RequestControl::from_timeout(self.config.timeout), Ok)
            .map_err(runtime_status)?;
        let request_id = validate_request_id(&request.request_id)?;
        if request.audio.is_empty() {
            return Err(Status::invalid_argument("audio request body is empty"));
        }
        if request.audio.len() > self.config.max_body_bytes {
            return Err(Status::resource_exhausted(
                "request body exceeds the configured limit",
            ));
        }
        validate_grpc_metadata(&request)?;
        // Decode admission is global across connections. Account the complete
        // protobuf message (audio plus all strings) immediately on entry and
        // retain the lease while its allocations move into the worker.
        let (encoded_memory_lease, permit) = match transport_resources {
            Some(resources) => (resources.memory_lease, resources.permit),
            None => {
                let request_bytes = u64::try_from(message_bytes)
                    .map_err(|_| Status::out_of_range("protobuf message length exceeds u64"))?;
                let memory_lease = self
                    .limits
                    .governor()
                    .reserve_memory(request_bytes)
                    .map_err(runtime_status)?;
                let permit = match self.permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        if let Some(metrics) = &self.metrics {
                            metrics.record_busy();
                        }
                        return Err(Status::resource_exhausted("service is busy"));
                    }
                };
                (memory_lease, permit)
            }
        };
        let filename = safe_filename(&request.filename)?;
        let suffix = audio_suffix(&filename, &request.content_type)?;
        let profile = resolve_profile(&request.profile)?;
        let channel_layout = match request
            .channel_layout_json
            .filter(|channel_layout_json| !channel_layout_json.is_empty())
        {
            Some(channel_layout_json) => {
                if channel_layout_json.len() > MAX_CHANNEL_LAYOUT_JSON_BYTES {
                    return Err(Status::invalid_argument(
                        "channel-layout JSON exceeds 256 KiB",
                    ));
                }
                Some(
                    ChannelLayoutDescriptor::from_json(&channel_layout_json)
                        .map_err(Status::invalid_argument)?,
                )
            }
            None => None,
        };
        let control = self.register_control(&request_id, control)?;
        let worker_lease = self.worker_lease(request_id.clone(), control.clone(), permit);
        let config = Arc::clone(&self.config);
        let limits = self.limits.clone();
        let metrics = self.metrics.clone();
        #[cfg(test)]
        let analysis_worker_hook = self.analysis_worker_hook.clone();
        let worker_request_id = request_id.clone();
        let audio = request.audio;
        let content_type = if request.content_type.is_empty() {
            "application/octet-stream".to_owned()
        } else {
            request.content_type
        };
        run_analysis_worker(worker_lease, control.clone(), move || {
            #[cfg(test)]
            analysis_worker_hook.before_analysis();
            analyze_audio(AnalyzeJob {
                audio,
                filename,
                content_type,
                suffix,
                profile,
                channel_layout,
                request_id: worker_request_id,
                max_decoded_samples: config.max_decoded_samples,
                encoded_memory_lease,
                control,
                limits,
                metrics,
            })
        })
        .await
    }

    async fn cancel_inner(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        self.authorize(&request, ServiceScope::Cancel)?;
        let request_id = request.into_inner().request_id;
        Ok(Response::new(CancelResponse {
            cancelled: self.cancel(&request_id)?,
        }))
    }

    async fn health_inner(
        &self,
        request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        self.authorize(&request, ServiceScope::Health)?;
        Ok(Response::new(HealthResponse {
            schema: SERVICE_HEALTH_SCHEMA.to_owned(),
            generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")).to_owned(),
            status: "ok".to_owned(),
        }))
    }

    async fn metrics_inner(
        &self,
        request: Request<MetricsRequest>,
    ) -> Result<Response<MetricsResponse>, Status> {
        self.authorize(&request, ServiceScope::Metrics)?;
        let Some(metrics) = &self.metrics else {
            return Err(Status::not_found("metrics exporter is disabled"));
        };
        Ok(Response::new(MetricsResponse {
            content_type: PROMETHEUS_CONTENT_TYPE.to_owned(),
            prometheus_text: metrics.render_prometheus(),
        }))
    }

    fn start_timer<T>(&self, request: &Request<T>) -> SharedGrpcRequestTimer {
        if let Some(timer) = request.extensions().get::<GrpcRequestTimerExtension>() {
            return timer.0.clone();
        }
        let Some(metrics) = self.metrics.as_ref() else {
            return SharedGrpcRequestTimer::default();
        };
        let mut timer = metrics.start_grpc_request();
        timer.set_traceparent(
            request
                .metadata()
                .get("traceparent")
                .and_then(|value| value.to_str().ok()),
        );
        SharedGrpcRequestTimer(Some(Arc::new(Mutex::new(Some(timer)))))
    }
}

fn response_with_memory_lease<T>(
    message: T,
    lease: QuotaLease,
    slot: Option<GrpcResponseLeaseSlot>,
) -> Result<Response<T>, Status> {
    let mut response = Response::new(message);
    if let Some(slot) = slot {
        slot.store(lease)?;
    } else {
        let slot = GrpcResponseLeaseSlot::default();
        slot.store(lease)?;
        response.extensions_mut().insert(slot);
    }
    Ok(response)
}

/// Finishes an Analyze request as cancelled if tonic drops its RPC future.
///
/// A bare [`RequestTimer`] treats an unfinished drop as an internal error. For
/// an in-progress RPC, however, dropping the public future means the caller is
/// no longer waiting for the response (normally because its connection was
/// closed), so the service records the conventional client-cancelled status.
struct AnalyzeRequestTimer {
    timer: SharedGrpcRequestTimer,
    request_bytes: u64,
}

impl AnalyzeRequestTimer {
    fn new(timer: SharedGrpcRequestTimer, request_bytes: u64) -> Self {
        Self {
            timer,
            request_bytes,
        }
    }

    fn finish(self, status_code: u16) {
        self.timer.finish(status_code, self.request_bytes);
    }
}

impl Drop for AnalyzeRequestTimer {
    fn drop(&mut self) {
        let status_code = if std::thread::panicking() { 500 } else { 499 };
        self.timer.finish(status_code, self.request_bytes);
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct AnalysisWorkerHook {
    started: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
}

#[cfg(test)]
impl AnalysisWorkerHook {
    fn pause_next(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(started_tx);
        *self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(release_rx);
        (started_rx, release_tx)
    }

    fn before_analysis(&self) {
        if let Some(started) = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = started.send(());
        }
        if let Some(release) = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = release.recv();
        }
    }
}

struct CancellationGuard {
    control: RequestControl,
    armed: bool,
}

impl CancellationGuard {
    fn new(control: RequestControl) -> Self {
        Self {
            control,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel();
        }
    }
}

/// Resources owned by the blocking worker rather than by the RPC future.
///
/// Dropping a `spawn_blocking` join handle only detaches the blocking task. The
/// permit and cancellation registry entry therefore have to travel into the
/// closure and remain live until that closure actually returns.
struct AnalysisWorkerLease {
    registry: Arc<Mutex<RequestRegistry>>,
    request_id: String,
    control: RequestControl,
    _permit: OwnedSemaphorePermit,
}

impl Drop for AnalysisWorkerLease {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .active
            .get(&self.request_id)
            .is_some_and(|control| control.same_request(&self.control))
        {
            registry.active.remove(&self.request_id);
        }
    }
}

async fn run_analysis_worker<T, F>(
    worker_lease: AnalysisWorkerLease,
    control: RequestControl,
    analyze: F,
) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Status> + Send + 'static,
{
    // This guard belongs to the RPC future. If tonic drops that future because
    // the client disconnects, or if the timeout below expires, the detached
    // blocking worker sees cancellation at its next cooperative checkpoint.
    let remaining = control.remaining().map_err(runtime_status)?;
    let mut cancellation_guard = CancellationGuard::new(control.clone());
    let result = time::timeout(
        remaining,
        tokio::task::spawn_blocking(move || {
            let _worker_lease = worker_lease;
            analyze()
        }),
    )
    .await;
    match result {
        Ok(Ok(Ok(response))) => {
            cancellation_guard.disarm();
            Ok(response)
        }
        Ok(Ok(Err(status))) => {
            cancellation_guard.disarm();
            Err(status)
        }
        Ok(Err(_)) => {
            cancellation_guard.disarm();
            Err(Status::internal("analysis worker failed"))
        }
        Err(_) => {
            // The absolute deadline wins over cancellation at the boundary.
            control.expire();
            match control.check() {
                Ok(()) => Err(Status::deadline_exceeded("analysis request timed out")),
                Err(error) => Err(runtime_status(error)),
            }
        }
    }
}

struct AnalyzeInput {
    audio: Vec<u8>,
    filename: String,
    content_type: String,
    profile: String,
    request_id: String,
    channel_layout_json: Option<String>,
}

struct AnalysisResult {
    filename: String,
    content_type: String,
    bytes_received: u64,
    report_json: String,
    request_id: String,
    channel_layout_json: String,
    response_memory_lease: QuotaLease,
}

impl AnalysisResult {
    fn into_v1(self) -> (AnalyzeResponse, QuotaLease) {
        let Self {
            filename,
            content_type,
            bytes_received,
            report_json,
            request_id,
            channel_layout_json: _,
            response_memory_lease,
        } = self;
        (
            AnalyzeResponse {
                schema: SERVICE_ANALYSIS_SCHEMA.to_owned(),
                generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")).to_owned(),
                filename,
                content_type,
                bytes_received,
                report_json,
                request_id,
            },
            response_memory_lease,
        )
    }

    fn into_v3(self) -> (AnalyzeV3Response, QuotaLease) {
        let Self {
            filename,
            content_type,
            bytes_received,
            report_json,
            request_id,
            channel_layout_json,
            response_memory_lease,
        } = self;
        (
            AnalyzeV3Response {
                schema: SERVICE_ANALYSIS_SCHEMA_V3.to_owned(),
                generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")).to_owned(),
                filename,
                content_type,
                bytes_received,
                report_json,
                request_id,
                channel_layout_json,
            },
            response_memory_lease,
        )
    }
}

struct AnalyzeJob {
    audio: Vec<u8>,
    filename: String,
    content_type: String,
    suffix: String,
    profile: Option<ComplianceProfile>,
    channel_layout: Option<ChannelLayoutDescriptor>,
    request_id: String,
    max_decoded_samples: u64,
    encoded_memory_lease: QuotaLease,
    control: RequestControl,
    limits: ServiceRuntimeLimits,
    metrics: Option<ServiceMetrics>,
}

fn analyze_audio(job: AnalyzeJob) -> Result<AnalysisResult, Status> {
    let AnalyzeJob {
        audio,
        filename,
        content_type,
        suffix,
        profile,
        channel_layout,
        request_id,
        max_decoded_samples,
        encoded_memory_lease,
        control,
        limits,
        metrics,
    } = job;
    control.check().map_err(runtime_status)?;
    let bytes_received = u64::try_from(audio.len())
        .map_err(|_| Status::out_of_range("request body length exceeds u64"))?;
    let stable_options = StableInputOptions::new(bytes_received)
        .map_err(|error| Status::internal(format!("invalid upload limit: {error}")))?
        .with_source_name_hint(format!("upload{suffix}"));
    let mut spool = UploadSpool::create(
        limits.governor(),
        control.clone(),
        bytes_received,
        stable_options,
    )
    .map_err(runtime_status)?;
    for chunk in audio.chunks(1024 * 1024) {
        spool.write_chunk(chunk).map_err(runtime_status)?;
    }
    let stable_input = spool.finish_into_stable_input().map_err(runtime_status)?;
    // The unary Vec is moved into this worker without cloning. Its admission
    // lease is released before the conservative decoded-PCM reservation.
    drop(audio);
    drop(encoded_memory_lease);
    let controlled = analyze_stable_input(
        stable_input,
        channel_layout,
        max_decoded_samples,
        limits.governor(),
        &control,
    )
    .map_err(controlled_analysis_status)?;
    let analysis = &controlled.analysis;
    let effective_layout = &controlled.channel_layout;
    let mut report = profile.as_ref().map_or_else(
        || AnalysisReport::new(std::path::Path::new(&filename), analysis),
        |profile| {
            AnalysisReport::with_compliance(
                std::path::Path::new(&filename),
                analysis,
                Some(profile),
            )
        },
    );
    report.path = filename.clone();
    control.check().map_err(runtime_status)?;
    if !report.integrated_lufs.is_finite() || !report.true_peak_dbtp.is_finite() {
        return Err(Status::invalid_argument(
            "the v1 response contract cannot represent a non-finite measurement",
        ));
    }
    let report_json = serde_json::to_string(&report)
        .map_err(|_| Status::invalid_argument("measurement contains a non-finite value"))?;
    control.check().map_err(runtime_status)?;
    let channel_layout_json = effective_layout
        .to_json()
        .map_err(|_| Status::internal("could not serialize channel layout"))?;
    control.check().map_err(runtime_status)?;
    let response_memory_bytes = grpc_response_memory_reservation_bytes(&[
        &filename,
        &content_type,
        &report_json,
        &request_id,
        &channel_layout_json,
        SERVICE_ANALYSIS_SCHEMA_V3,
        concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
    ])?;
    let response_memory_lease = limits
        .governor()
        .reserve_memory(response_memory_bytes)
        .map_err(runtime_status)?;
    if let Some(metrics) = metrics {
        metrics.observe_analysis(bytes_received, controlled.decoded_samples, analysis.lufs);
    }
    control.check().map_err(runtime_status)?;
    Ok(AnalysisResult {
        filename,
        content_type,
        bytes_received,
        report_json,
        request_id,
        channel_layout_json,
        response_memory_lease,
    })
}

fn grpc_response_memory_reservation_bytes(strings: &[&str]) -> Result<u64, Status> {
    let string_bytes = strings.iter().try_fold(0_u64, |total, value| {
        let bytes = u64::try_from(value.len())
            .map_err(|_| Status::out_of_range("gRPC response length exceeds u64"))?;
        total
            .checked_add(bytes)
            .ok_or_else(|| Status::out_of_range("gRPC response length overflow"))
    })?;
    let message_bytes = string_bytes
        .checked_add(8 * PROTOBUF_FIELD_OVERHEAD_BYTES as u64)
        .and_then(|bytes| bytes.checked_add(32))
        .ok_or_else(|| Status::out_of_range("gRPC response length overflow"))?;
    // During prost encoding, message-owned strings and the outbound wire
    // buffer coexist. Retain this charge until the HTTP body is drained or
    // dropped, rather than ending accounting when the handler returns.
    let resident_bytes = message_bytes
        .checked_mul(2)
        .ok_or_else(|| Status::out_of_range("gRPC response reservation overflow"))?;
    if resident_bytes > SERVICE_RESPONSE_WIRE_ALLOWANCE_BYTES {
        return Err(runtime_status(ServiceRuntimeError::new(
            ServiceRuntimeErrorKind::LimitExceeded,
            "serialized gRPC response exceeds the bounded response allowance",
        )));
    }
    Ok(resident_bytes)
}

fn controlled_analysis_status(error: ControlledAnalysisError) -> Status {
    match error {
        ControlledAnalysisError::Runtime(error) => runtime_status(error),
        ControlledAnalysisError::Media(_) => Status::invalid_argument("audio could not be decoded"),
    }
}

fn runtime_status(error: ServiceRuntimeError) -> Status {
    Status::new(runtime_grpc_code(error.kind()), error.to_string())
}

fn runtime_grpc_code(kind: ServiceRuntimeErrorKind) -> Code {
    match kind {
        ServiceRuntimeErrorKind::InvalidLimit | ServiceRuntimeErrorKind::IncompleteUpload => {
            Code::InvalidArgument
        }
        ServiceRuntimeErrorKind::LimitExceeded | ServiceRuntimeErrorKind::QuotaExceeded => {
            Code::ResourceExhausted
        }
        ServiceRuntimeErrorKind::ArithmeticOverflow => Code::OutOfRange,
        ServiceRuntimeErrorKind::Cancelled => Code::Cancelled,
        ServiceRuntimeErrorKind::DeadlineExceeded => Code::DeadlineExceeded,
        ServiceRuntimeErrorKind::Io => Code::Internal,
        ServiceRuntimeErrorKind::InvalidSnapshot => Code::DataLoss,
    }
}

fn status_code<T>(result: &Result<Response<T>, Status>) -> u16 {
    match result {
        Ok(_) => 200,
        Err(status) => grpc_code_status_code(status.code()),
    }
}

fn validate_request_id(value: &str) -> Result<String, Status> {
    if value.is_empty()
        || value.len() > MAX_REQUEST_ID_BYTES
        || value
            .bytes()
            .any(|byte| byte < 0x21 || byte == 0x7f || byte == b'/' || byte == b'\\')
    {
        return Err(Status::invalid_argument(
            "request_id must contain 1..=128 printable non-path bytes",
        ));
    }
    Ok(value.to_owned())
}

fn validate_grpc_metadata(request: &AnalyzeInput) -> Result<(), Status> {
    if request.content_type.len() > MAX_CONTENT_TYPE_BYTES {
        return Err(Status::invalid_argument(
            "content_type must contain at most 8192 bytes",
        ));
    }
    if request.profile.len() > MAX_PROFILE_BYTES {
        return Err(Status::invalid_argument(
            "profile must contain at most 256 bytes",
        ));
    }
    // These helpers also enforce printable/path-free contracts. Calling them
    // here keeps direct trait invocations under the same bounds as transport.
    let _ = safe_filename(&request.filename)?;
    let _ = validate_request_id(&request.request_id)?;
    if request
        .channel_layout_json
        .as_ref()
        .is_some_and(|json| json.len() > MAX_CHANNEL_LAYOUT_JSON_BYTES)
    {
        return Err(Status::invalid_argument(
            "channel-layout JSON exceeds 256 KiB",
        ));
    }
    Ok(())
}

fn safe_filename(value: &str) -> Result<String, Status> {
    if value.is_empty() {
        return Ok("upload.wav".into());
    }
    if value.len() > MAX_FILENAME_BYTES || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(Status::invalid_argument(
            "filename must contain 1..=256 printable bytes",
        ));
    }
    let basename = value.rsplit(['/', '\\']).next().unwrap_or(value);
    if basename.is_empty() || basename == "." || basename == ".." {
        return Err(Status::invalid_argument("filename must contain a basename"));
    }
    Ok(basename.to_owned())
}

fn audio_suffix(filename: &str, content_type: &str) -> Result<String, Status> {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let extension = match extension.as_str() {
        "wav" | "wave" | "bwf" | "bw64" | "rf64" | "flac" | "mp3" | "opus" | "ogg" | "m4a"
        | "mp4" | "aac" | "dsf" | "dff" => extension,
        "" => content_type_extension(content_type).unwrap_or_else(|| "wav".into()),
        _ => {
            return Err(Status::invalid_argument(
                "filename extension is not a supported audio format",
            ))
        }
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

fn resolve_profile(value: &str) -> Result<Option<ComplianceProfile>, Status> {
    if value.is_empty() {
        return Ok(None);
    }
    match ComplianceProfile::builtin(value) {
        Some(profile) if !profile.requires_dialogue() => Ok(Some(profile)),
        Some(_) => Err(Status::failed_precondition(
            "dialogue-based profiles require an explicit dialogue source",
        )),
        None => Err(Status::invalid_argument("unknown built-in profile")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use std::convert::Infallible;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tonic::codegen::tokio_stream::StreamExt;

    #[derive(Clone)]
    struct AdmissionProbe {
        entered: tokio::sync::mpsc::UnboundedSender<String>,
        release_analysis: Arc<Semaphore>,
    }

    impl Service<http::Request<tonic::body::Body>> for AdmissionProbe {
        type Response = http::Response<tonic::body::Body>;
        type Error = Infallible;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
            let path = request.uri().path().to_owned();
            let entered = self.entered.clone();
            let release = Arc::clone(&self.release_analysis);
            Box::pin(async move {
                let _request_holds_transport_resources = request;
                entered.send(path.clone()).unwrap();
                if path.ends_with("/Analyze") {
                    let _permit = release.acquire_owned().await.unwrap();
                }
                Ok(http::Response::new(tonic::body::Body::empty()))
            })
        }
    }

    struct TestGrpcBody {
        frames: VecDeque<Bytes>,
        polls: Arc<AtomicUsize>,
        pending_after_frames: bool,
    }

    impl HttpBody for TestGrpcBody {
        type Data = Bytes;
        type Error = Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            if let Some(bytes) = self.frames.pop_front() {
                Poll::Ready(Some(Ok(http_body::Frame::data(bytes))))
            } else if self.pending_after_frames {
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        }

        fn is_end_stream(&self) -> bool {
            self.frames.is_empty() && !self.pending_after_frames
        }
    }

    struct ErrorGrpcBody {
        yielded: bool,
    }

    impl HttpBody for ErrorGrpcBody {
        type Data = Bytes;
        type Error = Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            if self.yielded {
                Poll::Ready(None)
            } else {
                self.yielded = true;
                Poll::Ready(Some(Err(Status::internal("injected body error"))))
            }
        }
    }

    #[derive(Default)]
    struct RecordingSpanRecorder {
        spans: Mutex<Vec<crate::service_metrics::SpanRecord>>,
    }

    impl crate::service_metrics::SpanRecorder for RecordingSpanRecorder {
        fn record(&self, span: crate::service_metrics::SpanRecord) {
            self.spans
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(span);
        }
    }

    fn grpc_test_body(
        declared_bytes: usize,
        pending_after_header: bool,
    ) -> (tonic::body::Body, Arc<AtomicUsize>) {
        let mut header = vec![0_u8];
        header.extend_from_slice(&u32::try_from(declared_bytes).unwrap().to_be_bytes());
        grpc_raw_test_body(header, pending_after_header)
    }

    fn grpc_raw_test_body(
        bytes: Vec<u8>,
        pending_after_frames: bool,
    ) -> (tonic::body::Body, Arc<AtomicUsize>) {
        let polls = Arc::new(AtomicUsize::new(0));
        let body = TestGrpcBody {
            frames: VecDeque::from([Bytes::from(bytes)]),
            polls: Arc::clone(&polls),
            pending_after_frames,
        };
        (tonic::body::Body::new(body), polls)
    }

    fn grpc_encoded_body<M: Message>(message: &M) -> tonic::body::Body {
        let mut encoded = Vec::with_capacity(message.encoded_len() + 5);
        encoded.push(0);
        encoded.extend_from_slice(&u32::try_from(message.encoded_len()).unwrap().to_be_bytes());
        message.encode(&mut encoded).unwrap();
        grpc_raw_test_body(encoded, false).0
    }

    fn metric_value(exposition: &str, name: &str) -> u64 {
        exposition
            .lines()
            .find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|value| value.strip_prefix(' '))
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or_else(|| panic!("metric {name} is missing"))
    }

    fn grpc_response_code(response: &http::Response<tonic::body::Body>) -> Code {
        let value = response
            .headers()
            .get("grpc-status")
            .expect("early gRPC errors carry grpc-status")
            .to_str()
            .unwrap()
            .parse::<i32>()
            .unwrap();
        Code::from_i32(value)
    }

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

    async fn wait_for_worker_release(service: &GrpcService, request_id: &str) {
        time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let is_active = service
                    .registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .active
                    .contains_key(request_id);
                if service.permits.available_permits() == service.config.workers && !is_active {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker resources should be released after its closure exits");
    }

    async fn run_immediate_worker(
        service: &GrpcService,
        request_id: &str,
        result: Result<AnalyzeResponse, Status>,
    ) -> Result<AnalyzeResponse, Status> {
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let control = service.register(request_id).unwrap();
        let worker_lease = service.worker_lease(request_id.into(), control.clone(), permit);
        run_analysis_worker(worker_lease, control, move || result).await
    }

    #[test]
    fn request_ids_are_bounded_and_path_free() {
        assert!(validate_request_id("job-123").is_ok());
        assert!(validate_request_id("../job").is_err());
        assert!(validate_request_id(&"x".repeat(MAX_REQUEST_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn filenames_and_content_types_select_safe_suffixes() {
        assert_eq!(safe_filename("/tmp/mix.wav").unwrap(), "mix.wav");
        assert_eq!(audio_suffix("upload", "audio/flac").unwrap(), ".flac");
        assert!(safe_filename("../").is_err());
    }

    #[test]
    fn effective_bind_is_validated_before_default_limits_are_derived() {
        let configured_non_loopback = ServiceConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], 50051)),
            ..ServiceConfig::default()
        };
        assert!(effective_config(
            configured_non_loopback,
            SocketAddr::from(([127, 0, 0, 1], 50051))
        )
        .is_ok());

        let configured_loopback = ServiceConfig::default();
        assert_eq!(
            effective_config(configured_loopback, SocketAddr::from(([0, 0, 0, 0], 50051)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn grpc_duplicate_authorization_headers_are_rejected() {
        let security = ServiceSecurity::new()
            .with_token(
                crate::service::ScopedServiceToken::all("secret").expect("valid test token"),
            )
            .unwrap();
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer secret"),
        );
        headers.append(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer secret"),
        );
        let error = authorize_grpc_headers(&headers, &security, ServiceScope::Health)
            .expect_err("duplicate authorization metadata must fail closed");
        assert_eq!(error.code(), Code::InvalidArgument);
    }

    #[test]
    fn grpc_proxy_boundary_and_method_scopes_fail_closed() {
        let config = ServiceConfig {
            bind: "0.0.0.0:50051".parse().unwrap(),
            ..ServiceConfig::default()
        };
        let security = ServiceSecurity::new()
            .with_token(
                crate::service::ScopedServiceToken::new("health-secret", [ServiceScope::Health])
                    .unwrap(),
            )
            .unwrap()
            .with_trusted_proxy_ip("10.0.0.10".parse().unwrap());
        let connection = GrpcConnectionInfo {
            lifecycle: Arc::new(ConnectionLifecycle {
                headers_observed: AtomicBool::new(false),
            }),
            peer_ip: "10.0.0.10".parse().unwrap(),
        };
        let mut headers = http::HeaderMap::new();
        headers.insert("x-forwarded-proto", http::HeaderValue::from_static("https"));
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer health-secret"),
        );

        assert!(validate_grpc_peer(&headers, &config, &security, Some(&connection)).is_ok());
        assert!(authorize_grpc_headers(&headers, &security, ServiceScope::Health).is_ok());
        assert_eq!(
            authorize_grpc_headers(&headers, &security, ServiceScope::Analyze)
                .unwrap_err()
                .code(),
            Code::PermissionDenied
        );

        let mut missing_marker = headers.clone();
        missing_marker.remove("x-forwarded-proto");
        assert_eq!(
            validate_grpc_peer(&missing_marker, &config, &security, Some(&connection))
                .unwrap_err()
                .code(),
            Code::FailedPrecondition
        );
        let untrusted_connection = GrpcConnectionInfo {
            peer_ip: "10.0.0.11".parse().unwrap(),
            ..connection
        };
        assert_eq!(
            validate_grpc_peer(&headers, &config, &security, Some(&untrusted_connection),)
                .unwrap_err()
                .code(),
            Code::FailedPrecondition
        );
    }

    #[tokio::test]
    async fn grpc_direct_duplicate_authorization_metadata_is_rejected() {
        let service = GrpcService::new(
            ServiceConfig {
                bearer_token: Some("secret".into()),
                ..ServiceConfig::default()
            },
            None,
        );
        let mut request = Request::new(HealthRequest::default());
        request
            .metadata_mut()
            .append("authorization", "Bearer secret".parse().unwrap());
        request
            .metadata_mut()
            .append("authorization", "Bearer secret".parse().unwrap());
        let error = service
            .authorize(&request, ServiceScope::Health)
            .expect_err("duplicate authorization metadata must fail closed");
        assert_eq!(error.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn unknown_grpc_methods_fail_closed_before_body_admission() {
        let config = ServiceConfig::default();
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let layer = GrpcTransportAdmissionLayer::new(
            Arc::new(Semaphore::new(config.workers)),
            config,
            limits,
        );
        let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let (body, polls) = grpc_raw_test_body(Vec::new(), true);
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.Future/Analyze")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::Unimplemented);
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        assert!(entered_rx.try_recv().is_err());
    }

    #[test]
    fn grpc_runtime_config_drops_legacy_secret_after_security_merge() {
        let service = GrpcService::new(
            ServiceConfig {
                bearer_token: Some("legacy-secret".into()),
                ..ServiceConfig::default()
            },
            None,
        );
        assert!(service.config.bearer_token.is_none());
    }

    #[test]
    fn complete_protobuf_message_and_metadata_have_explicit_bounds() {
        let config = ServiceConfig {
            max_body_bytes: 4,
            ..ServiceConfig::default()
        };
        let limit = grpc_decoding_message_limit(&config).unwrap();
        let legal = AnalyzeV3Request {
            audio: vec![0; config.max_body_bytes],
            filename: "f".repeat(MAX_FILENAME_BYTES),
            content_type: "c".repeat(MAX_CONTENT_TYPE_BYTES),
            profile: "p".repeat(MAX_PROFILE_BYTES),
            request_id: "r".repeat(MAX_REQUEST_ID_BYTES),
            channel_layout_json: "l".repeat(MAX_CHANNEL_LAYOUT_JSON_BYTES),
        };
        assert!(legal.encoded_len() <= limit);

        let oversized = AnalyzeV3Request {
            profile: "p".repeat(limit),
            ..legal
        };
        assert!(oversized.encoded_len() > limit);
    }

    #[tokio::test]
    async fn direct_trait_invocation_rejects_oversized_metadata() {
        let service = GrpcService::new(ServiceConfig::default(), None);
        let error = service
            .analyze_inner(Request::new(AnalyzeRequest {
                audio: vec![0],
                filename: "input.wav".into(),
                content_type: "x".repeat(MAX_CONTENT_TYPE_BYTES + 1),
                profile: String::new(),
                request_id: "metadata-limit".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument);
        assert!(error.message().contains("content_type"));
    }

    #[tokio::test]
    async fn complete_message_limit_precedes_semantic_metadata_parsing() {
        let config = ServiceConfig {
            max_body_bytes: 4,
            ..ServiceConfig::default()
        };
        let limit = grpc_decoding_message_limit(&config).unwrap();
        let service = GrpcService::new(config, None);
        let error = service
            .analyze_v3_inner(Request::new(AnalyzeV3Request {
                audio: vec![0],
                filename: "input.wav".into(),
                content_type: "audio/wav".into(),
                profile: "p".repeat(limit),
                request_id: "whole-message-limit".into(),
                channel_layout_json: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert!(error.message().contains("protobuf request message"));
        assert_eq!(service.limits.memory_used_bytes(), 0);
        assert_eq!(service.limits.temporary_storage_used_bytes(), 0);
    }

    #[tokio::test]
    async fn protobuf_decode_admission_is_global_and_control_rpcs_have_bounded_headroom() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let release_analysis = Arc::new(Semaphore::new(0));
        let config = ServiceConfig {
            workers: 1,
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let analysis_permits = Arc::new(Semaphore::new(1));
        let layer =
            GrpcTransportAdmissionLayer::new(Arc::clone(&analysis_permits), config, limits.clone());
        let probe = AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::clone(&release_analysis),
        };
        // Separate layer clones model route services dispatched by distinct
        // HTTP/2 connections; all share the layer's one global permit.
        let mut first_connection = layer.layer(probe.clone());
        let mut second_connection = layer.layer(probe.clone());
        let mut control_connection = layer.layer(probe);
        let (first_body, _) = grpc_test_body(7, true);
        let first = tokio::spawn(async move {
            first_connection
                .call(
                    http::Request::builder()
                        .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                        .body(first_body)
                        .unwrap(),
                )
                .await
        });
        assert!(entered_rx.recv().await.unwrap().ends_with("/Analyze"));
        assert_eq!(analysis_permits.available_permits(), 0);
        assert_eq!(limits.memory_used_bytes(), 7);

        let (second_body, _) = grpc_test_body(0, false);
        let response = second_connection
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysisV3/Analyze")
                    .body(second_body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::ResourceExhausted);
        assert!(time::timeout(Duration::from_millis(25), entered_rx.recv())
            .await
            .is_err());

        let (control_body, _) = grpc_test_body(0, false);
        control_connection
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Health")
                    .body(control_body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(entered_rx.recv().await.unwrap().ends_with("/Health"));

        release_analysis.add_permits(1);
        first.await.unwrap().unwrap();
        assert_eq!(analysis_permits.available_permits(), 1);
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn headers_only_streams_are_globally_bounded_before_frame_prefix_wait() {
        let workers = 256;
        let (entered_tx, _entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ServiceConfig {
            workers,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let analysis_permits = Arc::new(Semaphore::new(workers));
        let layer =
            GrpcTransportAdmissionLayer::new(Arc::clone(&analysis_permits), config, limits.clone());
        let probe = AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        };

        let mut waiting = Vec::with_capacity(workers);
        for _ in 0..workers {
            let mut service = layer.layer(probe.clone());
            let (body, _) = grpc_raw_test_body(Vec::new(), true);
            waiting.push(tokio::spawn(async move {
                service
                    .call(
                        http::Request::builder()
                            .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                            .body(body)
                            .unwrap(),
                    )
                    .await
            }));
        }
        time::timeout(Duration::from_secs(2), async {
            while analysis_permits.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all global prefix-wait permits should be occupied");
        assert_eq!(limits.memory_used_bytes(), 0);

        // Even if each accepted HTTP/2 connection advertised hundreds of
        // streams, the 257th Analyze request is rejected without polling its
        // body. Prefix waiters therefore scale with workers, not workers².
        let mut overflow = layer.layer(probe);
        let (body, polls) = grpc_raw_test_body(Vec::new(), true);
        let response = overflow
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::ResourceExhausted);
        assert_eq!(polls.load(Ordering::Relaxed), 0);

        for task in waiting {
            task.abort();
            let _ = task.await;
        }
        assert_eq!(analysis_permits.available_permits(), workers);
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test]
    async fn huge_header_only_message_is_rejected_before_body_wait_or_resource_admission() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ServiceConfig {
            max_body_bytes: 4,
            workers: 1,
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let analysis_permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new(
            Arc::clone(&analysis_permits),
            config.clone(),
            limits.clone(),
        );
        let probe = AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        };
        let mut service = layer.layer(probe);
        let declared = grpc_analyze_v1_message_limit(&config).unwrap() + 1;
        let (body, polls) = grpc_test_body(declared, true);
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(grpc_response_code(&response), Code::ResourceExhausted);
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        assert!(entered_rx.try_recv().is_err());
        assert_eq!(analysis_permits.available_permits(), 1);
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test]
    async fn authentication_and_method_caps_are_checked_before_protobuf_decode() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ServiceConfig {
            bearer_token: Some("secret".into()),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let analysis_permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new(
            Arc::clone(&analysis_permits),
            config.clone(),
            limits.clone(),
        );
        let probe = AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        };
        let mut service = layer.layer(probe);
        let (body, polls) = grpc_test_body(0, true);
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::Unauthenticated);
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        assert!(entered_rx.try_recv().is_err());
        assert_eq!(analysis_permits.available_permits(), 1);
        assert_eq!(limits.memory_used_bytes(), 0);

        assert_eq!(
            grpc_method_policy("/forge.service.v1.ForgeAnalysis/Health", &config)
                .unwrap()
                .unwrap()
                .max_message_bytes,
            0
        );
        assert_eq!(
            grpc_method_policy("/forge.service.v1.ForgeAnalysis/Cancel", &config)
                .unwrap()
                .unwrap()
                .max_message_bytes,
            grpc_cancel_message_limit()
        );
        assert!(
            grpc_analyze_v1_message_limit(&config).unwrap()
                < grpc_analyze_v3_message_limit(&config).unwrap()
        );

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let (body, polls) = grpc_test_body(1, true);
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Health")
                    .header(http::header::AUTHORIZATION, "Bearer secret")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::ResourceExhausted);
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        assert!(entered_rx.try_recv().is_err());
        assert_eq!(
            layer.control_permits.available_permits(),
            GRPC_CONTROL_STREAM_HEADROOM
        );
        assert_eq!(limits.memory_used_bytes(), 0);

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let (body, polls) = grpc_raw_test_body(vec![0, 0, 0, 0, 0, 0xff], true);
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Health")
                    .header(http::header::AUTHORIZATION, "Bearer secret")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::InvalidArgument);
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        assert!(entered_rx.try_recv().is_err());
        assert_eq!(
            layer.control_permits.available_permits(),
            GRPC_CONTROL_STREAM_HEADROOM
        );
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test]
    async fn verified_scope_extension_reaches_tonic_handler_and_mismatch_fails_closed() {
        let config = ServiceConfig {
            bearer_token: Some("secret".into()),
            ..ServiceConfig::default()
        };
        let service = GrpcService::new(config, None);

        // Tonic's server decoder preserves HTTP request extensions when it
        // constructs the decoded Request<T>. Exercise that same conversion
        // so the transport's one successful digest scan is enough for the
        // handler.
        let mut http_request = http::Request::new(HealthRequest {});
        http_request
            .extensions_mut()
            .insert(GrpcVerifiedAuth(ServiceScope::Health));
        let request = Request::from_http(http_request);
        assert!(service.health_inner(request).await.is_ok());

        let mut mismatch = http::Request::new(HealthRequest {});
        mismatch
            .extensions_mut()
            .insert(GrpcVerifiedAuth(ServiceScope::Metrics));
        let error = service
            .health_inner(Request::from_http(mismatch))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::Internal);

        // No private marker means a direct trait invocation must still use
        // the metadata fallback rather than treating the request as trusted.
        let error = service
            .health_inner(Request::new(HealthRequest {}))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::Unauthenticated);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outer_admission_metrics_cover_every_terminal_boundary_once() {
        let metrics = ServiceMetrics::new();

        // Authentication is decided before the body is polled.
        let auth_config = ServiceConfig {
            bearer_token: Some("secret".into()),
            timeout: Duration::from_millis(150),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&auth_config).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            auth_config,
            limits,
            Some(metrics.clone()),
        );
        let (entered, _) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(grpc_raw_test_body(Vec::new(), true).0)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::Unauthenticated);

        // Busy rejection owns no body or byte lease but is still one request.
        let config = ServiceConfig {
            timeout: Duration::from_millis(150),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let held = Arc::clone(&permits).try_acquire_owned().unwrap();
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config.clone(),
            limits,
            Some(metrics.clone()),
        );
        let (entered, _) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(grpc_raw_test_body(Vec::new(), true).0)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::ResourceExhausted);
        drop(held);

        // Byte quota is charged only after the five-byte prefix supplies the
        // declared length, then released on rejection.
        let quota_limits = ServiceRuntimeLimits::new(1, 1).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config.clone(),
            quota_limits.clone(),
            Some(metrics.clone()),
        );
        let (entered, _) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(grpc_test_body(2, true).0)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::ResourceExhausted);
        assert_eq!(quota_limits.memory_used_bytes(), 0);

        // Unsupported compression is a method-framing failure.
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config.clone(),
            limits,
            Some(metrics.clone()),
        );
        let (entered, _) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let response = service
            .call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(grpc_raw_test_body(vec![1, 0, 0, 0, 0], false).0)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grpc_response_code(&response), Code::Unimplemented);

        // Prefix timeout starts its timer before the first body poll.
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config.clone(),
            limits,
            Some(metrics.clone()),
        );
        let (entered, _) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let prefix_timeout = tokio::spawn(async move {
            service
                .call(
                    http::Request::builder()
                        .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                        .body(grpc_raw_test_body(Vec::new(), true).0)
                        .unwrap(),
                )
                .await
                .unwrap()
        });
        time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            metric_value(
                &metrics.render_prometheus(),
                "forge_service_in_flight_requests"
            ),
            1
        );
        assert_eq!(
            grpc_response_code(&prefix_timeout.await.unwrap()),
            Code::DeadlineExceeded
        );

        // A handler that has consumed the request but stalls uses the same
        // timer; the outer deadline wins before dropping the handler future.
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config,
            limits,
            Some(metrics.clone()),
        );
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let handler_timeout = tokio::spawn(async move {
            service
                .call(
                    http::Request::builder()
                        .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                        .body(grpc_test_body(0, false).0)
                        .unwrap(),
                )
                .await
                .unwrap()
        });
        entered_rx.recv().await.unwrap();
        assert_eq!(
            metric_value(
                &metrics.render_prometheus(),
                "forge_service_in_flight_requests"
            ),
            1
        );
        assert_eq!(
            grpc_response_code(&handler_timeout.await.unwrap()),
            Code::DeadlineExceeded
        );

        let exposition = metrics.render_prometheus();
        assert_eq!(metric_value(&exposition, "forge_service_requests_total"), 6);
        assert_eq!(
            metric_value(&exposition, "forge_service_request_success_total"),
            0
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_client_errors_total"),
            3
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_server_errors_total"),
            3
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_busy_total"),
            1
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_timeout_total"),
            2
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_in_flight_requests"),
            0
        );
    }

    #[tokio::test]
    async fn dropping_pending_prefix_records_one_client_cancellation() {
        let metrics = ServiceMetrics::new();
        let recorder = Arc::new(RecordingSpanRecorder::default());
        metrics.set_span_recorder(recorder.clone());
        let config = ServiceConfig {
            workers: 1,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config,
            limits.clone(),
            Some(metrics.clone()),
        );
        let (entered, _) = tokio::sync::mpsc::unbounded_channel();
        let mut service = layer.layer(AdmissionProbe {
            entered,
            release_analysis: Arc::new(Semaphore::new(0)),
        });
        let (body, polls) = grpc_raw_test_body(Vec::new(), true);
        let task = tokio::spawn(async move {
            service
                .call(
                    http::Request::builder()
                        .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                        .body(body)
                        .unwrap(),
                )
                .await
        });
        time::timeout(Duration::from_secs(1), async {
            while polls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the transport must poll the pending frame prefix");
        assert_eq!(permits.available_permits(), 0);
        assert_eq!(
            metric_value(
                &metrics.render_prometheus(),
                "forge_service_in_flight_requests"
            ),
            1
        );

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(limits.memory_used_bytes(), 0);

        let exposition = metrics.render_prometheus();
        assert_eq!(metric_value(&exposition, "forge_service_requests_total"), 1);
        assert_eq!(
            metric_value(&exposition, "forge_service_request_cancelled_total"),
            1
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_client_errors_total"),
            1
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_server_errors_total"),
            0
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_in_flight_requests"),
            0
        );
        let spans = recorder
            .spans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].status_code, 499);
        assert_eq!(spans[0].request_bytes, 0);
    }

    #[tokio::test]
    async fn dropping_protobuf_decode_releases_admission_and_records_declared_bytes() {
        let metrics = ServiceMetrics::new();
        let recorder = Arc::new(RecordingSpanRecorder::default());
        metrics.set_span_recorder(recorder.clone());
        let config = ServiceConfig {
            workers: 1,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let service =
            GrpcService::with_runtime_limits(config.clone(), Some(metrics.clone()), limits.clone());
        let permits = Arc::clone(&service.permits);
        let layer = GrpcTransportAdmissionLayer::new_with_metrics(
            Arc::clone(&permits),
            config.clone(),
            limits.clone(),
            Some(metrics.clone()),
        );
        let mut transport = layer.layer(
            ForgeAnalysisServer::new(service)
                .max_decoding_message_size(grpc_analyze_v1_message_limit(&config).unwrap()),
        );
        let declared_bytes = 7_usize;
        let (body, polls) = grpc_test_body(declared_bytes, true);
        let task = tokio::spawn(async move {
            transport
                .call(
                    http::Request::builder()
                        .method("POST")
                        .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                        .header(http::header::CONTENT_TYPE, "application/grpc")
                        .header("te", "trailers")
                        .body(body)
                        .unwrap(),
                )
                .await
        });
        time::timeout(Duration::from_secs(1), async {
            while limits.memory_used_bytes() != declared_bytes as u64
                || polls.load(Ordering::Relaxed) < 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tonic must wait for the declared protobuf body");
        assert_eq!(permits.available_permits(), 0);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(permits.available_permits(), 1);
        assert_eq!(limits.memory_used_bytes(), 0);

        let exposition = metrics.render_prometheus();
        assert_eq!(metric_value(&exposition, "forge_service_requests_total"), 1);
        assert_eq!(
            metric_value(&exposition, "forge_service_request_cancelled_total"),
            1
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_server_errors_total"),
            0
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_in_flight_requests"),
            0
        );
        let spans = recorder
            .spans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].status_code, 499);
        assert_eq!(spans[0].request_bytes, declared_bytes as u64);
    }

    #[test]
    fn panicking_outer_transport_records_one_server_error() {
        let metrics = ServiceMetrics::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _timer = OuterGrpcRequestTimer::new(SharedGrpcRequestTimer(Some(Arc::new(
                Mutex::new(Some(metrics.start_grpc_request())),
            ))));
            panic!("expected outer transport panic");
        }));
        assert!(outcome.is_err());
        let exposition = metrics.render_prometheus();
        assert_eq!(metric_value(&exposition, "forge_service_requests_total"), 1);
        assert_eq!(
            metric_value(&exposition, "forge_service_request_server_errors_total"),
            1
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_request_cancelled_total"),
            0
        );
        assert_eq!(
            metric_value(&exposition, "forge_service_in_flight_requests"),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn response_memory_quota_lives_until_tonic_body_drains_or_drops() {
        let audio = mono_s16_wave(48_000);
        let config = ServiceConfig {
            max_body_bytes: audio.len(),
            max_decoded_samples: 48_000,
            workers: 1,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let service = GrpcService::with_runtime_limits(config.clone(), None, limits.clone());
        let layer = GrpcTransportAdmissionLayer::new(
            Arc::clone(&service.permits),
            config.clone(),
            limits.clone(),
        );
        let mut transport = layer.layer(
            ForgeAnalysisServer::new(service)
                .max_decoding_message_size(grpc_analyze_v1_message_limit(&config).unwrap()),
        );
        let request = |request_id: &str| AnalyzeRequest {
            audio: audio.clone(),
            filename: "response.wav".into(),
            content_type: "audio/wav".into(),
            profile: String::new(),
            request_id: request_id.into(),
        };
        let make_http = |message: &AnalyzeRequest| {
            http::Request::builder()
                .method("POST")
                .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                .header(http::header::CONTENT_TYPE, "application/grpc")
                .body(grpc_encoded_body(message))
                .unwrap()
        };

        let response = transport.call(make_http(&request("drain"))).await.unwrap();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(limits.memory_used_bytes() > 0);
        assert_eq!(limits.temporary_storage_used_bytes(), 0);
        let mut body = response.into_body();
        while let Some(frame) =
            std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context)).await
        {
            frame.unwrap();
        }
        assert_eq!(limits.memory_used_bytes(), 0);

        let response = transport.call(make_http(&request("drop"))).await.unwrap();
        assert!(limits.memory_used_bytes() > 0);
        drop(response);
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test]
    async fn response_body_error_releases_memory_quota_immediately() {
        let limits = ServiceRuntimeLimits::new(64, 64).unwrap();
        let lease = limits.governor().reserve_memory(7).unwrap();
        let mut body = RetainedGrpcBody {
            inner: tonic::body::Body::new(ErrorGrpcBody { yielded: false }),
            memory_lease: Some(lease),
        };
        assert_eq!(limits.memory_used_bytes(), 7);

        let frame = std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context))
            .await
            .expect("the injected body returns one error frame");
        assert_eq!(frame.unwrap_err().code(), Code::Internal);
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test]
    async fn slow_body_deadline_drops_transport_quota_and_permit() {
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ServiceConfig {
            workers: 1,
            timeout: Duration::from_millis(100),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let analysis_permits = Arc::new(Semaphore::new(1));
        let layer =
            GrpcTransportAdmissionLayer::new(Arc::clone(&analysis_permits), config, limits.clone());
        let probe = AdmissionProbe {
            entered: entered_tx,
            release_analysis: Arc::new(Semaphore::new(0)),
        };
        let mut service = layer.layer(probe);
        let (body, _) = grpc_test_body(7, true);
        let response = time::timeout(
            Duration::from_secs(2),
            service.call(
                http::Request::builder()
                    .uri("/forge.service.v1.ForgeAnalysis/Analyze")
                    .body(body)
                    .unwrap(),
            ),
        )
        .await
        .expect("transport deadline must bound a stalled body")
        .unwrap();
        assert!(entered_rx.try_recv().is_ok());
        assert_eq!(grpc_response_code(&response), Code::DeadlineExceeded);
        assert_eq!(analysis_permits.available_permits(), 1);
        assert_eq!(limits.memory_used_bytes(), 0);
    }

    #[tokio::test]
    async fn accepted_connection_limit_is_shared_across_clients() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mut incoming = ConnectionLimitedIncoming::new(
            listener,
            1,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(10),
        );

        let first_client = TcpStream::connect(address).await.unwrap();
        let first_server = incoming.next().await.unwrap().unwrap();
        let second_client = TcpStream::connect(address).await.unwrap();
        assert!(time::timeout(Duration::from_millis(25), incoming.next())
            .await
            .is_err());
        drop(first_server);
        let second_server = time::timeout(Duration::from_secs(1), incoming.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop((first_client, second_client, second_server));
    }

    #[tokio::test]
    async fn hard_connection_age_waits_for_grace_then_returns_accept_capacity() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let max_age = Duration::from_millis(50);
        let grace = Duration::from_millis(100);
        let hard_age = grpc_connection_hard_max_age(max_age, grace).unwrap();
        assert_eq!(hard_age, Duration::from_millis(150));
        let mut incoming = ConnectionLimitedIncoming::new(
            listener,
            1,
            Duration::from_secs(1),
            Duration::from_secs(1),
            hard_age,
        );
        let client = TcpStream::connect(address).await.unwrap();
        let mut server = incoming.next().await.unwrap().unwrap();
        server.lifecycle.mark_headers_observed();
        assert_eq!(incoming.permits.available_permits(), 0);

        let mut byte = [0_u8; 1];
        assert!(
            time::timeout(Duration::from_millis(75), server.read(&mut byte))
                .await
                .is_err()
        );
        assert_eq!(incoming.permits.available_permits(), 0);
        let error = time::timeout(Duration::from_secs(1), server.read(&mut byte))
            .await
            .expect("hard connection max-age must wake pending IO")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(incoming.permits.available_permits(), 1);
        drop((client, server));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tonic_server_hard_age_closes_a_late_in_flight_rpc_and_recovers_capacity() {
        use proto::forge_analysis_client::ForgeAnalysisClient;

        let hard_age = Duration::from_secs(2);
        let audio = mono_s16_wave(48_000);
        let config = ServiceConfig {
            max_body_bytes: audio.len(),
            max_decoded_samples: 48_000,
            workers: 1,
            timeout: Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let limits = ServiceRuntimeLimits::for_config(&config).unwrap();
        let service = GrpcService::with_runtime_limits(config.clone(), None, limits.clone());
        let service_probe = service.clone();
        let (worker_started, release_worker) = service.analysis_worker_hook.pause_next();
        let admission =
            GrpcTransportAdmissionLayer::new(Arc::clone(&service.permits), config.clone(), limits);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = ConnectionLimitedIncoming::new(
            listener,
            1,
            Duration::from_secs(1),
            Duration::from_secs(5),
            hard_age,
        );
        let connection_permits = Arc::clone(&incoming.permits);
        let decoding_limit = grpc_analyze_v1_message_limit(&config).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            Server::builder()
                .timeout(Duration::from_secs(5))
                // Both tonic max-age options are deliberately absent. The
                // incoming IO wrapper below owns the hard boundary.
                .initial_stream_window_size(GRPC_HTTP2_STREAM_WINDOW_BYTES)
                .initial_connection_window_size(GRPC_HTTP2_STREAM_WINDOW_BYTES)
                .max_concurrent_streams(GRPC_CONTROL_STREAM_HEADROOM as u32 + 1)
                .layer(admission)
                .add_service(
                    ForgeAnalysisServer::new(service).max_decoding_message_size(decoding_limit),
                )
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
            .unwrap()
            .connect_timeout(Duration::from_secs(1));
        let channel = endpoint.connect().await.unwrap();
        let mut client = ForgeAnalysisClient::new(channel);
        client.health(Request::new(HealthRequest {})).await.unwrap();

        // Start a stream late in the connection's lifetime. The request-level
        // timeout is much longer than the connection deadline, so the observed
        // failure is the documented hard IO close rather than RPC timeout.
        time::sleep(Duration::from_secs(1)).await;
        let mut analyze_client = client.clone();
        let analyze = tokio::spawn(async move {
            analyze_client
                .analyze(Request::new(AnalyzeRequest {
                    audio,
                    filename: "max-age.wav".into(),
                    content_type: "audio/wav".into(),
                    profile: String::new(),
                    request_id: "max-age-in-flight".into(),
                }))
                .await
        });
        time::timeout(Duration::from_secs(1), worker_started)
            .await
            .expect("the in-flight Analyze RPC must reach its worker")
            .unwrap();
        assert_eq!(connection_permits.available_permits(), 0);

        let status = time::timeout(Duration::from_secs(2), analyze)
            .await
            .expect("the hard connection age must terminate the RPC")
            .unwrap()
            .expect_err("an RPC crossing the hard age cannot drain gracefully");
        assert!(matches!(
            status.code(),
            Code::Cancelled | Code::Unavailable | Code::Unknown
        ));
        // The tonic channel may immediately occupy the returned connection
        // permit while reconnecting, so prove capacity recovery through an
        // actual control RPC rather than sampling the semaphore between those
        // two events. This succeeds while the old detached worker is still
        // paused and therefore cannot be confused with analysis admission.
        time::timeout(Duration::from_secs(2), async {
            loop {
                match client.health(Request::new(HealthRequest {})).await {
                    Ok(_) => break,
                    Err(_) => time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("the channel must reconnect after the hard close");
        assert_eq!(connection_permits.available_permits(), 0);

        release_worker.send(()).unwrap();
        wait_for_worker_release(&service_probe, "max-age-in-flight").await;

        let _ = shutdown_tx.send(());
        time::timeout(Duration::from_secs(2), server)
            .await
            .expect("test gRPC server must shut down")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn default_five_idle_connections_expire_and_return_accept_capacity() {
        let config = ServiceConfig::default();
        let connection_limit = grpc_connection_limit(&config).unwrap();
        assert_eq!(connection_limit, 5);
        assert!(GRPC_CONNECTION_MAX_AGE > GRPC_CONNECTION_IDLE_TIMEOUT);

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let mut incoming = ConnectionLimitedIncoming::new(
            listener,
            connection_limit,
            Duration::from_millis(50),
            Duration::from_secs(1),
            Duration::from_secs(10),
        );
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        for _ in 0..connection_limit {
            clients.push(TcpStream::connect(address).await.unwrap());
            servers.push(incoming.next().await.unwrap().unwrap());
        }
        assert_eq!(incoming.permits.available_permits(), 0);

        for server in &mut servers {
            let mut byte = [0_u8; 1];
            let error = time::timeout(Duration::from_secs(1), server.read(&mut byte))
                .await
                .expect("absolute handshake deadline must fire")
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        }
        assert_eq!(incoming.permits.available_permits(), connection_limit);

        // Capacity is returned by the timed-out IO itself, even while the five
        // wrapper values above remain alive during HTTP/2 teardown.
        let sixth_client = TcpStream::connect(address).await.unwrap();
        let sixth_server = time::timeout(Duration::from_secs(1), incoming.next())
            .await
            .expect("released connection capacity should admit the next socket")
            .unwrap()
            .unwrap();
        drop((clients, servers, sixth_client, sixth_server));
    }

    #[test]
    fn runtime_errors_have_stable_grpc_codes() {
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::InvalidLimit),
            Code::InvalidArgument
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::LimitExceeded),
            Code::ResourceExhausted
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::QuotaExceeded),
            Code::ResourceExhausted
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::Cancelled),
            Code::Cancelled
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::DeadlineExceeded),
            Code::DeadlineExceeded
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::Io),
            Code::Internal
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::ArithmeticOverflow),
            Code::OutOfRange
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::IncompleteUpload),
            Code::InvalidArgument
        );
        assert_eq!(
            runtime_grpc_code(ServiceRuntimeErrorKind::InvalidSnapshot),
            Code::DataLoss
        );
    }

    #[tokio::test]
    async fn v3_analysis_response_carries_the_effective_exact_layout() {
        let sample_rate = 48_000_u32;
        let frames = sample_rate as usize;
        let mut audio = Vec::with_capacity(44 + frames * 2);
        audio.extend_from_slice(b"RIFF");
        audio.extend_from_slice(&(36_u32 + frames as u32 * 2).to_le_bytes());
        audio.extend_from_slice(b"WAVEfmt ");
        audio.extend_from_slice(&16_u32.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&1_u16.to_le_bytes());
        audio.extend_from_slice(&sample_rate.to_le_bytes());
        audio.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        audio.extend_from_slice(&2_u16.to_le_bytes());
        audio.extend_from_slice(&16_u16.to_le_bytes());
        audio.extend_from_slice(b"data");
        audio.extend_from_slice(&(frames as u32 * 2).to_le_bytes());
        for _ in 0..frames {
            audio.extend_from_slice(&1_000_i16.to_le_bytes());
        }
        let layout =
            ChannelLayoutDescriptor::from_channel_roles(vec![crate::wav::ChannelRole::Main])
                .unwrap();
        let service = GrpcService::new(ServiceConfig::default(), None);
        let response = ForgeAnalysisV3::analyze(
            &service,
            Request::new(AnalyzeV3Request {
                audio,
                filename: "mono.wav".into(),
                content_type: "audio/wav".into(),
                profile: String::new(),
                request_id: "layout-job".into(),
                channel_layout_json: layout.to_json().unwrap(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(response.schema, SERVICE_ANALYSIS_SCHEMA_V3);
        let effective = ChannelLayoutDescriptor::from_json(&response.channel_layout_json).unwrap();
        assert_eq!(
            effective.origin(),
            crate::channel_layout::ChannelLayoutOrigin::ExplicitOverride
        );
    }

    #[tokio::test]
    async fn unary_input_temp_and_decoded_quota_failures_release_every_lease() {
        let config = ServiceConfig {
            max_body_bytes: 128 * 1024,
            max_decoded_samples: 12_000,
            workers: 1,
            timeout: std::time::Duration::from_secs(5),
            ..ServiceConfig::default()
        };
        let request = |audio: Vec<u8>, request_id: &str| AnalyzeRequest {
            audio,
            filename: "input.wav".into(),
            content_type: "audio/wav".into(),
            profile: String::new(),
            request_id: request_id.into(),
        };

        let input_limits = ServiceRuntimeLimits::new(3, 128 * 1024).unwrap();
        let service = GrpcService::with_runtime_limits(config.clone(), None, input_limits.clone());
        let error = service
            .analyze_inner(Request::new(request(vec![0; 4], "input-quota")))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert_eq!(input_limits.memory_used_bytes(), 0);
        assert_eq!(input_limits.temporary_storage_used_bytes(), 0);

        let audio = mono_s16_wave(12_000);
        let temp_limits = ServiceRuntimeLimits::new(12_000 * 8, audio.len() as u64 - 1).unwrap();
        let service = GrpcService::with_runtime_limits(config.clone(), None, temp_limits.clone());
        let error = service
            .analyze_inner(Request::new(request(audio.clone(), "temp-quota")))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert_eq!(temp_limits.memory_used_bytes(), 0);
        assert_eq!(temp_limits.temporary_storage_used_bytes(), 0);

        let decoded_config = ServiceConfig {
            max_decoded_samples: 11_999,
            ..config
        };
        let decoded_memory =
            crate::service_runtime::service_analysis_working_set_reservation_bytes(
                audio.len() as u64,
                11_999,
            )
            .unwrap();
        let decoded_limits = ServiceRuntimeLimits::new(decoded_memory, audio.len() as u64).unwrap();
        let service =
            GrpcService::with_runtime_limits(decoded_config, None, decoded_limits.clone());
        let error = service
            .analyze_inner(Request::new(request(audio, "decoded-quota")))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::ResourceExhausted);
        assert_eq!(decoded_limits.memory_used_bytes(), 0);
        assert_eq!(decoded_limits.temporary_storage_used_bytes(), 0);
    }

    #[test]
    fn cancellation_registry_is_explicit() {
        let service = GrpcService::new(ServiceConfig::default(), None);
        let control = service.register("job").unwrap();
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let worker_lease = service.worker_lease("job".into(), control.clone(), permit);
        assert!(service.cancel("job").unwrap());
        assert_eq!(
            control.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::Cancelled
        );
        drop(worker_lease);
        assert!(!service.cancel("job").unwrap());
        let reuse = service.register("job").unwrap();
        assert!(reuse.check().is_ok());
        assert!(service.register("job").is_err());

        // A stale internal control from the completed generation cannot alter
        // the active registration after reuse. Wire-level Cancel remains, as
        // in v1, an active-ID operation without a generation field.
        control.cancel();
        assert!(reuse.check().is_ok());
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        drop(service.worker_lease("job".into(), reuse, permit));
    }

    #[test]
    fn more_than_65536_sequential_request_ids_and_completed_ids_remain_usable() {
        let service = GrpcService::new(
            ServiceConfig {
                workers: 1,
                ..ServiceConfig::default()
            },
            None,
        );
        for id in 0..=65_536 {
            let request_id = format!("job-{id}");
            let control = service.register(&request_id).unwrap();
            let permit = service.permits.clone().try_acquire_owned().unwrap();
            drop(service.worker_lease(request_id, control, permit));
        }
        assert!(service
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .is_empty());

        let reused = service.register("job-0").unwrap();
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        drop(service.worker_lease("job-0".into(), reused, permit));
    }

    #[tokio::test]
    async fn v3_cancel_uses_the_shared_request_registry_and_metrics() {
        let metrics = ServiceMetrics::new();
        let service = GrpcService::new(ServiceConfig::default(), Some(metrics.clone()));
        let control = service.register("v3-job").unwrap();
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let worker_lease = service.worker_lease("v3-job".into(), control.clone(), permit);

        let response = ForgeAnalysisV3::cancel(
            &service,
            Request::new(CancelRequest {
                request_id: "v3-job".into(),
            }),
        )
        .await
        .unwrap()
        .into_inner();

        assert!(response.cancelled);
        assert_eq!(
            control.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::Cancelled
        );
        drop(worker_lease);
        let exposition = metrics.render_prometheus();
        assert!(exposition.contains("forge_service_requests_total 1"));
        assert!(exposition.contains("forge_service_request_success_total 1"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_worker_retains_permit_and_registration_until_it_exits() {
        let config = ServiceConfig {
            workers: 1,
            timeout: std::time::Duration::from_millis(100),
            ..ServiceConfig::default()
        };
        let service = GrpcService::new(config, None);
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let control = service.register("slow-job").unwrap();
        let worker_lease = service.worker_lease("slow-job".into(), control.clone(), permit);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let first = tokio::spawn(run_analysis_worker(
            worker_lease,
            control.clone(),
            move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok(AnalyzeResponse::default())
            },
        ));
        started_rx.await.expect("blocking worker should start");
        let error = first
            .await
            .expect("timeout task should not panic")
            .expect_err("blocking worker should exceed the RPC timeout");
        assert_eq!(error.code(), Code::DeadlineExceeded);
        assert_eq!(service.permits.available_permits(), 0);
        assert!(service.cancel("slow-job").unwrap());
        assert_eq!(
            control.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::DeadlineExceeded
        );

        let second = AnalyzeRequest {
            audio: vec![0],
            filename: "second.wav".into(),
            content_type: "audio/wav".into(),
            profile: String::new(),
            request_id: "second-job".into(),
        };
        let error = service
            .analyze_inner(Request::new(second))
            .await
            .expect_err("the live blocking worker must retain the sole permit");
        assert_eq!(error.code(), Code::ResourceExhausted);

        release_tx.send(()).unwrap();
        wait_for_worker_release(&service, "slow-job").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_rpc_future_retains_worker_resources_until_the_worker_exits() {
        let config = ServiceConfig {
            workers: 1,
            timeout: std::time::Duration::from_secs(10),
            ..ServiceConfig::default()
        };
        let service = GrpcService::new(config, None);
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let control = service.register("dropped-job").unwrap();
        let worker_lease = service.worker_lease("dropped-job".into(), control.clone(), permit);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = tokio::spawn(run_analysis_worker(
            worker_lease,
            control.clone(),
            move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok(AnalyzeResponse::default())
            },
        ));

        started_rx.await.expect("blocking worker should start");
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(
            control.check().unwrap_err().kind(),
            ServiceRuntimeErrorKind::Cancelled
        );
        assert_eq!(service.permits.available_permits(), 0);
        assert!(service.cancel("dropped-job").unwrap());

        release_tx.send(()).unwrap();
        wait_for_worker_release(&service, "dropped-job").await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_public_analyze_rpc_records_client_cancellation() {
        let metrics = ServiceMetrics::new();
        let service = GrpcService::new(
            ServiceConfig {
                workers: 1,
                timeout: std::time::Duration::from_secs(10),
                ..ServiceConfig::default()
            },
            Some(metrics.clone()),
        );
        let (started, release) = service.analysis_worker_hook.pause_next();
        let analyze_request = AnalyzeRequest {
            audio: vec![0],
            filename: "disconnect.wav".into(),
            content_type: "audio/wav".into(),
            profile: String::new(),
            request_id: "disconnect-job".into(),
        };
        let request_memory = analyze_request.encoded_len() as u64;
        let task_service = service.clone();
        let rpc = tokio::spawn(async move {
            ForgeAnalysis::analyze(&task_service, Request::new(analyze_request)).await
        });

        started.await.expect("blocking worker should start");
        assert_eq!(service.limits.memory_used_bytes(), request_memory);
        let control = service
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .get("disconnect-job")
            .cloned()
            .expect("worker should remain registered");
        rpc.abort();
        let join_error = rpc.await.expect_err("RPC task should be aborted");
        let worker_saw_cancellation = control
            .check()
            .is_err_and(|error| error.kind() == ServiceRuntimeErrorKind::Cancelled);
        release.send(()).unwrap();
        wait_for_worker_release(&service, "disconnect-job").await;
        assert_eq!(service.limits.memory_used_bytes(), 0);
        assert_eq!(service.limits.temporary_storage_used_bytes(), 0);

        assert!(join_error.is_cancelled());
        assert!(worker_saw_cancellation);

        let exposition = metrics.render_prometheus();
        assert!(exposition.contains("forge_service_requests_total 1"));
        assert!(exposition.contains("forge_service_request_client_errors_total 1"));
        assert!(exposition.contains("forge_service_request_server_errors_total 0"));
        assert!(exposition.contains("forge_service_request_cancelled_total 1"));
        assert!(exposition.contains("forge_service_in_flight_requests 0"));
    }

    #[test]
    fn panicking_analyze_timer_records_a_server_error() {
        let metrics = ServiceMetrics::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _timer = AnalyzeRequestTimer::new(
                SharedGrpcRequestTimer(Some(Arc::new(Mutex::new(Some(
                    metrics.start_grpc_request(),
                ))))),
                1,
            );
            panic!("expected handler panic");
        }));

        assert!(outcome.is_err());
        let exposition = metrics.render_prometheus();
        assert!(exposition.contains("forge_service_requests_total 1"));
        assert!(exposition.contains("forge_service_request_client_errors_total 0"));
        assert!(exposition.contains("forge_service_request_server_errors_total 1"));
        assert!(exposition.contains("forge_service_request_cancelled_total 0"));
        assert!(exposition.contains("forge_service_in_flight_requests 0"));
    }

    #[tokio::test]
    async fn completed_and_failed_workers_release_capacity_and_registration() {
        let service = GrpcService::new(
            ServiceConfig {
                workers: 1,
                ..ServiceConfig::default()
            },
            None,
        );

        run_immediate_worker(&service, "completed-job", Ok(AnalyzeResponse::default()))
            .await
            .expect("successful worker should return its response");
        assert_eq!(service.permits.available_permits(), 1);
        assert!(!service.cancel("completed-job").unwrap());

        for (request_id, status) in [
            (
                "invalid-job",
                Status::invalid_argument("expected invalid argument"),
            ),
            (
                "quota-job",
                Status::resource_exhausted("expected quota exhaustion"),
            ),
            ("cancelled-job", Status::cancelled("expected cancellation")),
            (
                "deadline-job",
                Status::deadline_exceeded("expected deadline"),
            ),
        ] {
            let expected_code = status.code();
            let expected_message = status.message().to_owned();
            let error = run_immediate_worker(&service, request_id, Err(status))
                .await
                .expect_err("worker Status should be preserved");
            assert_eq!(error.code(), expected_code, "{request_id}");
            assert_eq!(error.message(), expected_message, "{request_id}");
            assert_eq!(service.permits.available_permits(), 1, "{request_id}");
            assert!(!service.cancel(request_id).unwrap(), "{request_id}");
        }
    }

    #[tokio::test]
    async fn panicked_worker_releases_capacity_and_registration() {
        let service = GrpcService::new(
            ServiceConfig {
                workers: 1,
                ..ServiceConfig::default()
            },
            None,
        );
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let control = service.register("panicked-job").unwrap();
        let worker_lease = service.worker_lease("panicked-job".into(), control.clone(), permit);

        let error = run_analysis_worker(
            worker_lease,
            control,
            || -> Result<AnalyzeResponse, Status> { panic!("expected worker panic") },
        )
        .await
        .expect_err("a panicked worker should become an internal error");

        assert_eq!(error.code(), Code::Internal);
        assert_eq!(service.permits.available_permits(), 1);
        assert!(!service.cancel("panicked-job").unwrap());
    }

    #[test]
    fn legacy_analysis_messages_remain_constructible_with_the_v1_fields() {
        let request = AnalyzeRequest {
            audio: Vec::new(),
            filename: String::new(),
            content_type: String::new(),
            profile: String::new(),
            request_id: "job".into(),
        };
        assert_eq!(request.encode_to_vec(), b"\x2a\x03job");

        let response = AnalyzeResponse {
            schema: String::new(),
            generator: String::new(),
            filename: String::new(),
            content_type: String::new(),
            bytes_received: 0,
            report_json: String::new(),
            request_id: "job".into(),
        };
        assert_eq!(response.encode_to_vec(), b"\x3a\x03job");
    }

    #[test]
    fn v3_layout_fields_use_the_additive_wire_numbers() {
        let request = AnalyzeV3Request {
            channel_layout_json: "{}".into(),
            ..AnalyzeV3Request::default()
        };
        let encoded_request = request.encode_to_vec();
        assert_eq!(encoded_request, b"\x32\x02{}");
        assert_eq!(
            AnalyzeV3Request::decode(encoded_request.as_slice())
                .unwrap()
                .channel_layout_json,
            "{}"
        );

        let response = AnalyzeV3Response {
            channel_layout_json: "{}".into(),
            ..AnalyzeV3Response::default()
        };
        let encoded_response = response.encode_to_vec();
        assert_eq!(encoded_response, b"\x42\x02{}");
        assert_eq!(
            AnalyzeV3Response::decode(encoded_response.as_slice())
                .unwrap()
                .channel_layout_json,
            "{}"
        );
    }

    #[test]
    fn protobuf_health_request_has_stable_empty_encoding() {
        assert!(HealthRequest::default().encode_to_vec().is_empty());
    }

    #[tokio::test]
    async fn metrics_rpc_returns_prometheus_text_when_enabled() {
        let metrics = ServiceMetrics::new();
        let timer = metrics.start_grpc_request();
        timer.finish(200, 0);
        let service = GrpcService::new(ServiceConfig::default(), Some(metrics));
        let response = ForgeMetrics::metrics(&service, Request::new(MetricsRequest::default()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.content_type, PROMETHEUS_CONTENT_TYPE);
        assert!(response
            .prometheus_text
            .contains("forge_service_requests_total"));

        let disabled = GrpcService::new(ServiceConfig::default(), None);
        let error = ForgeMetrics::metrics(&disabled, Request::new(MetricsRequest::default()))
            .await
            .expect_err("metrics should be disabled without a registry");
        assert_eq!(error.code(), Code::NotFound);
    }
}
