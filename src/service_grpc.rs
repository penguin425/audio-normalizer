//! Optional gRPC service for bounded audio analysis.
//!
//! The gRPC surface is deliberately opt-in (`grpc-service`) so the default
//! library and REST build do not acquire an async runtime or HTTP/2 stack.
//! Requests use an explicit request ID.  A caller can cancel an active job via
//! the Cancel RPC; cancellation is cooperative at bounded decode/analysis
//! checkpoints and is also triggered when the client drops the RPC.

use crate::decoder;
use crate::report::{AnalysisReport, ComplianceProfile};
use crate::service::{ServiceConfig, SERVICE_ANALYSIS_SCHEMA, SERVICE_HEALTH_SCHEMA};
use crate::service_metrics::{RequestTimer, ServiceMetrics, PROMETHEUS_CONTENT_TYPE};
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::Builder;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time;
use tonic::transport::Server;
use tonic::{Code, Request, Response, Status};

pub mod proto {
    tonic::include_proto!("forge.service.v1");
}

use proto::forge_analysis_server::{ForgeAnalysis, ForgeAnalysisServer};
use proto::forge_metrics_server::{ForgeMetrics, ForgeMetricsServer};
use proto::{
    AnalyzeRequest, AnalyzeResponse, CancelRequest, CancelResponse, HealthRequest, HealthResponse,
    MetricsRequest, MetricsResponse,
};

const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_FILENAME_BYTES: usize = 256;

/// Run the optional gRPC endpoint until the process receives Ctrl-C.
pub fn run(config: ServiceConfig, bind: SocketAddr) -> std::io::Result<()> {
    run_internal(config, bind, None)
}

/// Run the optional gRPC endpoint with a shared metrics registry.
pub fn run_with_metrics(
    config: ServiceConfig,
    bind: SocketAddr,
    metrics: ServiceMetrics,
) -> std::io::Result<()> {
    run_internal(config, bind, Some(metrics))
}

fn run_internal(
    mut config: ServiceConfig,
    bind: SocketAddr,
    metrics: Option<ServiceMetrics>,
) -> std::io::Result<()> {
    config.bind = bind;
    config.validate().map_err(invalid_config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| std::io::Error::other(format!("tokio runtime: {error}")))?;
    runtime.block_on(async move {
        let service = GrpcService::new(config.clone(), metrics);
        let shutdown = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        Server::builder()
            .add_service(
                ForgeAnalysisServer::new(service.clone())
                    .max_decoding_message_size(config.max_body_bytes),
            )
            .add_service(ForgeMetricsServer::new(service))
            .serve_with_shutdown(bind, shutdown)
            .await
            .map_err(|error| std::io::Error::other(format!("gRPC server: {error}")))
    })
}

fn invalid_config(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

#[derive(Clone)]
struct GrpcService {
    config: Arc<ServiceConfig>,
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    permits: Arc<Semaphore>,
    metrics: Option<ServiceMetrics>,
    #[cfg(test)]
    analysis_worker_hook: AnalysisWorkerHook,
}

impl GrpcService {
    fn new(config: ServiceConfig, metrics: Option<ServiceMetrics>) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(config.workers)),
            config: Arc::new(config),
            active: Arc::new(Mutex::new(HashMap::new())),
            metrics,
            #[cfg(test)]
            analysis_worker_hook: AnalysisWorkerHook::default(),
        }
    }

    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let Some(expected) = &self.config.bearer_token else {
            return Ok(());
        };
        let actual = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let expected = format!("Bearer {expected}");
        if actual == Some(expected.as_str()) {
            Ok(())
        } else {
            Err(Status::unauthenticated("a valid bearer token is required"))
        }
    }

    fn register(&self, request_id: &str) -> Result<Arc<AtomicBool>, Status> {
        let flag = Arc::new(AtomicBool::new(false));
        let mut active = self
            .active
            .lock()
            .map_err(|_| Status::internal("cancellation registry is unavailable"))?;
        if active.contains_key(request_id) {
            return Err(Status::already_exists("request_id is already active"));
        }
        active.insert(request_id.to_owned(), Arc::clone(&flag));
        Ok(flag)
    }

    fn cancel(&self, request_id: &str) -> Result<bool, Status> {
        validate_request_id(request_id)?;
        let active = self
            .active
            .lock()
            .map_err(|_| Status::internal("cancellation registry is unavailable"))?;
        if let Some(flag) = active.get(request_id) {
            flag.store(true, Ordering::Release);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn worker_lease(
        &self,
        request_id: String,
        cancelled: Arc<AtomicBool>,
        permit: OwnedSemaphorePermit,
    ) -> AnalysisWorkerLease {
        AnalysisWorkerLease {
            active: Arc::clone(&self.active),
            request_id,
            cancelled,
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
        let mut timer = self.start_timer(&request);
        let result = self.cancel_inner(request).await;
        if let Some(timer) = timer.take() {
            timer.finish(status_code(&result), 0);
        }
        result
    }

    async fn health(
        &self,
        request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        let mut timer = self.start_timer(&request);
        let result = self.health_inner(request).await;
        if let Some(timer) = timer.take() {
            timer.finish(status_code(&result), 0);
        }
        result
    }
}

#[tonic::async_trait]
impl ForgeMetrics for GrpcService {
    async fn metrics(
        &self,
        request: Request<MetricsRequest>,
    ) -> Result<Response<MetricsResponse>, Status> {
        let mut timer = self.start_timer(&request);
        let result = self.metrics_inner(request).await;
        if let Some(timer) = timer.take() {
            timer.finish(status_code(&result), 0);
        }
        result
    }
}

impl GrpcService {
    async fn analyze_inner(
        &self,
        request: Request<AnalyzeRequest>,
    ) -> Result<Response<AnalyzeResponse>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let request_id = validate_request_id(&request.request_id)?;
        if request.audio.is_empty() {
            return Err(Status::invalid_argument("audio request body is empty"));
        }
        if request.audio.len() > self.config.max_body_bytes {
            return Err(Status::resource_exhausted(
                "request body exceeds the configured limit",
            ));
        }
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(metrics) = &self.metrics {
                    metrics.record_busy();
                }
                return Err(Status::resource_exhausted("service is busy"));
            }
        };
        let filename = safe_filename(&request.filename)?;
        let suffix = audio_suffix(&filename, &request.content_type)?;
        let profile = resolve_profile(&request.profile)?;
        let cancelled = self.register(&request_id)?;
        let worker_lease = self.worker_lease(request_id.clone(), Arc::clone(&cancelled), permit);
        let config = Arc::clone(&self.config);
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
        run_analysis_worker(
            config.timeout,
            worker_lease,
            Arc::clone(&cancelled),
            move || {
                #[cfg(test)]
                analysis_worker_hook.before_analysis();
                analyze_audio(AnalyzeJob {
                    audio,
                    filename,
                    content_type,
                    suffix,
                    profile,
                    request_id: worker_request_id,
                    max_decoded_samples: config.max_decoded_samples,
                    cancelled,
                    metrics,
                })
            },
        )
        .await
        .map(Response::new)
    }

    async fn cancel_inner(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        self.authorize(&request)?;
        let request_id = request.into_inner().request_id;
        Ok(Response::new(CancelResponse {
            cancelled: self.cancel(&request_id)?,
        }))
    }

    async fn health_inner(
        &self,
        request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        self.authorize(&request)?;
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
        self.authorize(&request)?;
        let Some(metrics) = &self.metrics else {
            return Err(Status::not_found("metrics exporter is disabled"));
        };
        Ok(Response::new(MetricsResponse {
            content_type: PROMETHEUS_CONTENT_TYPE.to_owned(),
            prometheus_text: metrics.render_prometheus(),
        }))
    }

    fn start_timer<T>(&self, request: &Request<T>) -> Option<RequestTimer> {
        let metrics = self.metrics.as_ref()?;
        let mut timer = metrics.start_grpc_request();
        timer.set_traceparent(
            request
                .metadata()
                .get("traceparent")
                .and_then(|value| value.to_str().ok()),
        );
        Some(timer)
    }
}

/// Finishes an Analyze request as cancelled if tonic drops its RPC future.
///
/// A bare [`RequestTimer`] treats an unfinished drop as an internal error. For
/// an in-progress RPC, however, dropping the public future means the caller is
/// no longer waiting for the response (normally because its connection was
/// closed), so the service records the conventional client-cancelled status.
struct AnalyzeRequestTimer {
    timer: Option<RequestTimer>,
    request_bytes: u64,
}

impl AnalyzeRequestTimer {
    fn new(timer: Option<RequestTimer>, request_bytes: u64) -> Self {
        Self {
            timer,
            request_bytes,
        }
    }

    fn finish(mut self, status_code: u16) {
        if let Some(timer) = self.timer.take() {
            timer.finish(status_code, self.request_bytes);
        }
    }
}

impl Drop for AnalyzeRequestTimer {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take() {
            let status_code = if std::thread::panicking() { 500 } else { 499 };
            timer.finish(status_code, self.request_bytes);
        }
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

struct CancellationGuard(Arc<AtomicBool>);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Resources owned by the blocking worker rather than by the RPC future.
///
/// Dropping a `spawn_blocking` join handle only detaches the blocking task. The
/// permit and cancellation registry entry therefore have to travel into the
/// closure and remain live until that closure actually returns.
struct AnalysisWorkerLease {
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    request_id: String,
    cancelled: Arc<AtomicBool>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for AnalysisWorkerLease {
    fn drop(&mut self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .get(&self.request_id)
            .is_some_and(|flag| Arc::ptr_eq(flag, &self.cancelled))
        {
            active.remove(&self.request_id);
        }
    }
}

async fn run_analysis_worker<F>(
    timeout: std::time::Duration,
    worker_lease: AnalysisWorkerLease,
    cancelled: Arc<AtomicBool>,
    analyze: F,
) -> Result<AnalyzeResponse, Status>
where
    F: FnOnce() -> Result<AnalyzeResponse, Status> + Send + 'static,
{
    // This guard belongs to the RPC future. If tonic drops that future because
    // the client disconnects, or if the timeout below expires, the detached
    // blocking worker sees cancellation at its next cooperative checkpoint.
    let cancellation_guard = CancellationGuard(cancelled);
    let result = time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            let _worker_lease = worker_lease;
            analyze()
        }),
    )
    .await;
    drop(cancellation_guard);
    match result {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => Err(Status::internal("analysis worker failed")),
        Err(_) => Err(Status::deadline_exceeded("analysis request timed out")),
    }
}

struct AnalyzeJob {
    audio: Vec<u8>,
    filename: String,
    content_type: String,
    suffix: String,
    profile: Option<ComplianceProfile>,
    request_id: String,
    max_decoded_samples: u64,
    cancelled: Arc<AtomicBool>,
    metrics: Option<ServiceMetrics>,
}

fn analyze_audio(job: AnalyzeJob) -> Result<AnalyzeResponse, Status> {
    let AnalyzeJob {
        audio,
        filename,
        content_type,
        suffix,
        profile,
        request_id,
        max_decoded_samples,
        cancelled,
        metrics,
    } = job;
    check_cancelled(&cancelled)?;
    let mut temporary = Builder::new()
        .prefix("forge-grpc-")
        .suffix(&suffix)
        .tempfile()
        .map_err(|_| Status::internal("could not create upload file"))?;
    for chunk in audio.chunks(1024 * 1024) {
        check_cancelled(&cancelled)?;
        temporary
            .write_all(chunk)
            .map_err(|_| Status::internal("could not store upload"))?;
    }
    temporary
        .flush()
        .map_err(|_| Status::internal("could not store upload"))?;
    check_cancelled(&cancelled)?;
    let path = temporary.path().to_path_buf();
    let (mut decoded, layout_provenance) =
        decoder::decode_limited_with_layout(&path, max_decoded_samples)
            .map_err(|_| Status::invalid_argument("audio could not be decoded"))?;
    check_cancelled(&cancelled)?;
    decoded.channel_roles = crate::normalize::resolve_decoded_channel_roles(
        &path,
        decoded.channels,
        &decoded.channel_roles,
        layout_provenance,
        None,
    )
    .map_err(|_| Status::invalid_argument("audio could not be decoded"))?;
    check_cancelled(&cancelled)?;
    let analysis = crate::analysis::analyze(&decoded);
    check_cancelled(&cancelled)?;
    let mut report = profile.as_ref().map_or_else(
        || AnalysisReport::new(&path, &analysis),
        |profile| AnalysisReport::with_compliance(&path, &analysis, Some(profile)),
    );
    report.path = filename.clone();
    check_cancelled(&cancelled)?;
    let report_json = serde_json::to_string(&report)
        .map_err(|_| Status::invalid_argument("measurement contains a non-finite value"))?;
    check_cancelled(&cancelled)?;
    if let Some(metrics) = metrics {
        let decoded_samples = (decoded.frames as u64).saturating_mul(u64::from(decoded.channels));
        metrics.observe_analysis(audio.len() as u64, decoded_samples, analysis.lufs);
    }
    Ok(AnalyzeResponse {
        schema: SERVICE_ANALYSIS_SCHEMA.to_owned(),
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")).to_owned(),
        filename,
        content_type,
        bytes_received: audio.len() as u64,
        report_json,
        request_id,
    })
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), Status> {
    if cancelled.load(Ordering::Acquire) {
        Err(Status::cancelled("analysis request was cancelled"))
    } else {
        Ok(())
    }
}

fn status_code<T>(result: &Result<Response<T>, Status>) -> u16 {
    match result {
        Ok(_) => 200,
        Err(status) => match status.code() {
            Code::Ok => 200,
            Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => 400,
            Code::Unauthenticated => 401,
            Code::PermissionDenied => 403,
            Code::NotFound => 404,
            Code::AlreadyExists => 409,
            Code::ResourceExhausted => 429,
            Code::Cancelled => 499,
            Code::DeadlineExceeded => 504,
            Code::Unavailable => 503,
            Code::Internal | Code::Unknown | Code::DataLoss => 500,
            Code::Unimplemented => 501,
            Code::Aborted => 409,
        },
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

    async fn wait_for_worker_release(service: &GrpcService, request_id: &str) {
        time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let is_active = service
                    .active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let cancelled = service.register(request_id).unwrap();
        let worker_lease = service.worker_lease(request_id.into(), Arc::clone(&cancelled), permit);
        run_analysis_worker(service.config.timeout, worker_lease, cancelled, move || {
            result
        })
        .await
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
    fn cancellation_registry_is_explicit() {
        let service = GrpcService::new(ServiceConfig::default(), None);
        let flag = service.register("job").unwrap();
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let worker_lease = service.worker_lease("job".into(), Arc::clone(&flag), permit);
        assert!(service.cancel("job").unwrap());
        assert!(flag.load(Ordering::Acquire));
        drop(worker_lease);
        assert!(!service.cancel("job").unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_worker_retains_permit_and_registration_until_it_exits() {
        let config = ServiceConfig {
            workers: 1,
            timeout: std::time::Duration::from_millis(25),
            ..ServiceConfig::default()
        };
        let service = GrpcService::new(config, None);
        let permit = service.permits.clone().try_acquire_owned().unwrap();
        let cancelled = service.register("slow-job").unwrap();
        let worker_lease = service.worker_lease("slow-job".into(), Arc::clone(&cancelled), permit);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let first = tokio::spawn(run_analysis_worker(
            service.config.timeout,
            worker_lease,
            Arc::clone(&cancelled),
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
        let cancelled = service.register("dropped-job").unwrap();
        let worker_lease =
            service.worker_lease("dropped-job".into(), Arc::clone(&cancelled), permit);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = tokio::spawn(run_analysis_worker(
            service.config.timeout,
            worker_lease,
            Arc::clone(&cancelled),
            move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                Ok(AnalyzeResponse::default())
            },
        ));

        started_rx.await.expect("blocking worker should start");
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(cancelled.load(Ordering::Acquire));
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
        let task_service = service.clone();
        let rpc = tokio::spawn(async move {
            ForgeAnalysis::analyze(
                &task_service,
                Request::new(AnalyzeRequest {
                    audio: vec![0],
                    filename: "disconnect.wav".into(),
                    content_type: "audio/wav".into(),
                    profile: String::new(),
                    request_id: "disconnect-job".into(),
                }),
            )
            .await
        });

        started.await.expect("blocking worker should start");
        let cancelled = service
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("disconnect-job")
            .cloned()
            .expect("worker should remain registered");
        rpc.abort();
        let join_error = rpc.await.expect_err("RPC task should be aborted");
        let worker_saw_cancellation = cancelled.load(Ordering::Acquire);
        release.send(()).unwrap();
        wait_for_worker_release(&service, "disconnect-job").await;

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
            let _timer = AnalyzeRequestTimer::new(Some(metrics.start_grpc_request()), 1);
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

        let error = run_immediate_worker(
            &service,
            "failed-job",
            Err(Status::invalid_argument("expected test failure")),
        )
        .await
        .expect_err("worker error should be preserved");
        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(service.permits.available_permits(), 1);
        assert!(!service.cancel("failed-job").unwrap());
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
        let cancelled = service.register("panicked-job").unwrap();
        let worker_lease =
            service.worker_lease("panicked-job".into(), Arc::clone(&cancelled), permit);

        let error = run_analysis_worker(
            service.config.timeout,
            worker_lease,
            cancelled,
            || -> Result<AnalyzeResponse, Status> { panic!("expected worker panic") },
        )
        .await
        .expect_err("a panicked worker should become an internal error");

        assert_eq!(error.code(), Code::Internal);
        assert_eq!(service.permits.available_permits(), 1);
        assert!(!service.cancel("panicked-job").unwrap());
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
