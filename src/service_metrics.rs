//! Bounded service metrics and an OpenTelemetry-compatible span bridge.
//!
//! The metrics are intentionally dependency-free.  Prometheus exposition is
//! rendered from fixed counters and buckets, so user paths, request IDs,
//! model payloads, and other unbounded values never become labels.  A caller
//! may additionally install a [`SpanRecorder`] to forward bounded request
//! attributes to an OpenTelemetry collector or adapter.

use serde::Serialize;
use std::fmt::Write as FmtWrite;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Prometheus content type for the text exposition format.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const HISTOGRAM_BUCKETS: [f64; 8] = [0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0, 120.0];
const HISTOGRAM_LABELS: [&str; 8] = ["0.005", "0.025", "0.1", "0.5", "1", "5", "30", "120"];
const SERVICE_NAME: &str = "forge-normalizer";

/// Thread-safe metrics registry shared by REST and gRPC service instances.
#[derive(Clone)]
pub struct ServiceMetrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    requests_total: AtomicU64,
    request_success_total: AtomicU64,
    request_client_error_total: AtomicU64,
    request_server_error_total: AtomicU64,
    request_busy_total: AtomicU64,
    request_timeout_total: AtomicU64,
    request_cancelled_total: AtomicU64,
    request_duration_buckets: [AtomicU64; HISTOGRAM_BUCKETS.len()],
    request_duration_count: AtomicU64,
    request_duration_sum_nanos: AtomicU64,
    in_flight_requests: AtomicU64,
    analysis_total: AtomicU64,
    analysis_bytes_received_total: AtomicU64,
    analysis_decoded_samples_total: AtomicU64,
    analysis_loudness_sum_milli_lu: AtomicI64,
    analysis_loudness_count: AtomicU64,
    span_recorder: Mutex<Option<Arc<dyn SpanRecorder>>>,
}

impl Default for ServiceMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceMetrics {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                requests_total: AtomicU64::new(0),
                request_success_total: AtomicU64::new(0),
                request_client_error_total: AtomicU64::new(0),
                request_server_error_total: AtomicU64::new(0),
                request_busy_total: AtomicU64::new(0),
                request_timeout_total: AtomicU64::new(0),
                request_cancelled_total: AtomicU64::new(0),
                request_duration_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
                request_duration_count: AtomicU64::new(0),
                request_duration_sum_nanos: AtomicU64::new(0),
                in_flight_requests: AtomicU64::new(0),
                analysis_total: AtomicU64::new(0),
                analysis_bytes_received_total: AtomicU64::new(0),
                analysis_decoded_samples_total: AtomicU64::new(0),
                analysis_loudness_sum_milli_lu: AtomicI64::new(0),
                analysis_loudness_count: AtomicU64::new(0),
                span_recorder: Mutex::new(None),
            }),
        }
    }

    /// Install a bounded span recorder and return this registry for chaining.
    pub fn with_span_recorder(self, recorder: Arc<dyn SpanRecorder>) -> Self {
        self.set_span_recorder(recorder);
        self
    }

    /// Replace the optional span recorder.  Prometheus output is unaffected.
    pub fn set_span_recorder(&self, recorder: Arc<dyn SpanRecorder>) {
        if let Ok(mut slot) = self.inner.span_recorder.lock() {
            *slot = Some(recorder);
        }
    }

    /// Start an HTTP request timer.
    pub fn start_http_request(&self) -> RequestTimer {
        self.start_request("http")
    }

    /// Start a gRPC request timer.
    pub fn start_grpc_request(&self) -> RequestTimer {
        self.start_request("grpc")
    }

    fn start_request(&self, protocol: &'static str) -> RequestTimer {
        self.inner
            .in_flight_requests
            .fetch_add(1, Ordering::Relaxed);
        RequestTimer {
            metrics: self.clone(),
            started: Instant::now(),
            protocol,
            trace: None,
            decoded_samples: None,
            loudness_lufs: None,
            finished: false,
        }
    }

    /// Record a successfully decoded analysis without exposing its path or
    /// any other source identifier to the metrics stream.
    pub fn observe_analysis(&self, bytes_received: u64, decoded_samples: u64, lufs: f64) {
        self.inner.analysis_total.fetch_add(1, Ordering::Relaxed);
        saturating_add_u64(&self.inner.analysis_bytes_received_total, bytes_received);
        saturating_add_u64(&self.inner.analysis_decoded_samples_total, decoded_samples);
        if lufs.is_finite() {
            let milli_lu = (lufs * 1000.0).round();
            if milli_lu >= i64::MIN as f64 && milli_lu <= i64::MAX as f64 {
                saturating_add_i64(&self.inner.analysis_loudness_sum_milli_lu, milli_lu as i64);
                self.inner
                    .analysis_loudness_count
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Record a request rejected before a worker timer could be started.
    pub fn record_busy(&self) {
        self.inner
            .request_busy_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Render deterministic Prometheus text with only fixed metric names and
    /// histogram bucket labels.
    pub fn render_prometheus(&self) -> String {
        let mut output = String::with_capacity(4096);
        metric_counter(
            &mut output,
            "forge_service_requests_total",
            "Total service requests.",
            self.inner.requests_total.load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_request_success_total",
            "Requests completed with a successful status.",
            self.inner.request_success_total.load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_request_client_errors_total",
            "Requests rejected as client errors.",
            self.inner
                .request_client_error_total
                .load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_request_server_errors_total",
            "Requests that ended with a server error.",
            self.inner
                .request_server_error_total
                .load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_request_busy_total",
            "Requests rejected because all workers were busy.",
            self.inner.request_busy_total.load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_request_timeout_total",
            "Requests that timed out or exceeded a read deadline.",
            self.inner.request_timeout_total.load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_request_cancelled_total",
            "Requests cancelled by a caller or disconnected client.",
            self.inner.request_cancelled_total.load(Ordering::Relaxed),
        );
        metric_gauge(
            &mut output,
            "forge_service_in_flight_requests",
            "Requests currently being handled.",
            self.inner.in_flight_requests.load(Ordering::Relaxed),
        );

        output.push_str(
            "# HELP forge_service_request_duration_seconds Request duration histogram.\n",
        );
        output.push_str("# TYPE forge_service_request_duration_seconds histogram\n");
        for (label, bucket) in HISTOGRAM_LABELS
            .iter()
            .zip(self.inner.request_duration_buckets.iter())
        {
            let _ = writeln!(
                output,
                "forge_service_request_duration_seconds_bucket{{le=\"{label}\"}} {}",
                bucket.load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            output,
            "forge_service_request_duration_seconds_bucket{{le=\"+Inf\"}} {}",
            self.inner.request_duration_count.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            output,
            "forge_service_request_duration_seconds_sum {:.9}",
            self.inner
                .request_duration_sum_nanos
                .load(Ordering::Relaxed) as f64
                / 1e9
        );
        let _ = writeln!(
            output,
            "forge_service_request_duration_seconds_count {}",
            self.inner.request_duration_count.load(Ordering::Relaxed)
        );

        metric_counter(
            &mut output,
            "forge_service_analysis_total",
            "Successfully decoded and measured audio analyses.",
            self.inner.analysis_total.load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_analysis_bytes_received_total",
            "Audio bytes accepted for successful analyses.",
            self.inner
                .analysis_bytes_received_total
                .load(Ordering::Relaxed),
        );
        metric_counter(
            &mut output,
            "forge_service_analysis_decoded_samples_total",
            "Decoded samples accepted for successful analyses.",
            self.inner
                .analysis_decoded_samples_total
                .load(Ordering::Relaxed),
        );
        output.push_str(
            "# HELP forge_service_analysis_loudness_lufs_mean Mean integrated loudness of successful analyses.\n",
        );
        output.push_str("# TYPE forge_service_analysis_loudness_lufs_mean gauge\n");
        let loudness_count = self.inner.analysis_loudness_count.load(Ordering::Relaxed);
        let loudness_sum = self
            .inner
            .analysis_loudness_sum_milli_lu
            .load(Ordering::Relaxed) as f64
            / 1000.0;
        let mean = if loudness_count == 0 {
            0.0
        } else {
            loudness_sum / loudness_count as f64
        };
        let _ = writeln!(
            output,
            "forge_service_analysis_loudness_lufs_mean {mean:.3}"
        );
        output
    }
}

/// Timer returned for a single HTTP or gRPC request.
pub struct RequestTimer {
    metrics: ServiceMetrics,
    started: Instant,
    protocol: &'static str,
    trace: Option<TraceContext>,
    decoded_samples: Option<u64>,
    loudness_lufs: Option<f64>,
    finished: bool,
}

impl RequestTimer {
    /// Attach a valid W3C `traceparent` value to the eventual span record.
    pub fn set_traceparent(&mut self, value: Option<&str>) {
        self.trace = value.and_then(parse_traceparent);
    }

    /// Attach bounded analysis attributes to the eventual span record.
    pub fn observe_analysis(&mut self, decoded_samples: u64, lufs: f64) {
        self.decoded_samples = Some(decoded_samples);
        self.loudness_lufs = lufs.is_finite().then_some(lufs);
    }

    /// Finish the timer with an HTTP-compatible status code.
    pub fn finish(mut self, status_code: u16, request_bytes: u64) {
        self.finish_inner(status_code, request_bytes);
        self.finished = true;
    }

    fn finish_inner(&self, status_code: u16, request_bytes: u64) {
        let elapsed = self.started.elapsed();
        let inner = &self.metrics.inner;
        inner.in_flight_requests.fetch_sub(1, Ordering::Relaxed);
        inner.requests_total.fetch_add(1, Ordering::Relaxed);
        if (200..300).contains(&status_code) {
            inner.request_success_total.fetch_add(1, Ordering::Relaxed);
        } else if (400..500).contains(&status_code) {
            inner
                .request_client_error_total
                .fetch_add(1, Ordering::Relaxed);
        } else if status_code >= 500 {
            inner
                .request_server_error_total
                .fetch_add(1, Ordering::Relaxed);
        }
        if status_code == 408 || status_code == 504 {
            inner.request_timeout_total.fetch_add(1, Ordering::Relaxed);
        }
        if status_code == 499 {
            inner
                .request_cancelled_total
                .fetch_add(1, Ordering::Relaxed);
        }
        let seconds = elapsed.as_secs_f64();
        for (bucket, limit) in inner.request_duration_buckets.iter().zip(HISTOGRAM_BUCKETS) {
            if seconds <= limit {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        inner.request_duration_count.fetch_add(1, Ordering::Relaxed);
        saturating_add_u64(
            &inner.request_duration_sum_nanos,
            elapsed.as_nanos().min(u64::MAX as u128) as u64,
        );
        if let Ok(recorder) = inner.span_recorder.lock() {
            if let Some(recorder) = recorder.as_ref() {
                recorder.record(SpanRecord {
                    name: "forge.service.request",
                    kind: "server",
                    service_name: SERVICE_NAME,
                    protocol: self.protocol,
                    status: status_name(status_code),
                    status_code,
                    duration_ms: seconds * 1000.0,
                    request_bytes,
                    decoded_samples: self.decoded_samples,
                    integrated_lufs: self.loudness_lufs,
                    trace_id: self.trace.as_ref().map(|trace| trace.trace_id.clone()),
                    parent_span_id: self
                        .trace
                        .as_ref()
                        .map(|trace| trace.parent_span_id.clone()),
                });
            }
        }
    }
}

impl Drop for RequestTimer {
    fn drop(&mut self) {
        if !self.finished {
            self.finish_inner(500, 0);
        }
    }
}

/// A bounded W3C trace context accepted from HTTP metadata or gRPC metadata.
#[derive(Clone, Debug, Serialize)]
pub struct TraceContext {
    pub trace_id: String,
    pub parent_span_id: String,
    pub trace_flags: u8,
}

/// Parse a W3C `traceparent` header without accepting arbitrary user data.
pub fn parse_traceparent(value: &str) -> Option<TraceContext> {
    let parts: Vec<_> = value.trim().split('-').collect();
    if parts.len() != 4
        || parts[0].len() != 2
        || parts[1].len() != 32
        || parts[2].len() != 16
        || parts[3].len() != 2
        || !parts
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || parts[0].eq_ignore_ascii_case("ff")
        || parts[1].bytes().all(|byte| byte == b'0')
        || parts[2].bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(TraceContext {
        trace_id: parts[1].to_ascii_lowercase(),
        parent_span_id: parts[2].to_ascii_lowercase(),
        trace_flags: u8::from_str_radix(parts[3], 16).ok()?,
    })
}

/// A sink for bounded server-span attributes.  Implementations can adapt the
/// record to an OpenTelemetry SDK without adding that SDK to Forge's default
/// dependency graph.
pub trait SpanRecorder: Send + Sync {
    fn record(&self, span: SpanRecord);
}

/// OpenTelemetry semantic attributes emitted for one request.
#[derive(Clone, Debug, Serialize)]
pub struct SpanRecord {
    pub name: &'static str,
    pub kind: &'static str,
    pub service_name: &'static str,
    pub protocol: &'static str,
    pub status: &'static str,
    pub status_code: u16,
    pub duration_ms: f64,
    pub request_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoded_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrated_lufs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

/// JSONL recorder with stable, bounded fields suitable for an OTel adapter.
pub struct JsonlSpanRecorder {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl JsonlSpanRecorder {
    /// Append span records to a local JSONL file.
    pub fn from_path(path: impl AsRef<Path>) -> io::Result<Self> {
        let writer = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::from_writer(writer))
    }

    /// Build a recorder around any thread-safe writer.
    pub fn from_writer<W>(writer: W) -> Self
    where
        W: Write + Send + 'static,
    {
        Self {
            writer: Mutex::new(Box::new(writer)),
        }
    }
}

impl SpanRecorder for JsonlSpanRecorder {
    fn record(&self, span: SpanRecord) {
        let Ok(mut writer) = self.writer.lock() else {
            return;
        };
        if serde_json::to_writer(&mut *writer, &span).is_ok() {
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
        }
    }
}

fn status_name(status_code: u16) -> &'static str {
    match status_code {
        200..=299 => "ok",
        400..=499 => "error",
        _ => "failure",
    }
}

fn metric_counter(output: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} counter");
    let _ = writeln!(output, "{name} {value}");
}

fn metric_gauge(output: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(output, "# HELP {name} {help}");
    let _ = writeln!(output, "# TYPE {name} gauge");
    let _ = writeln!(output, "{name} {value}");
}

fn saturating_add_u64(atom: &AtomicU64, amount: u64) {
    let mut current = atom.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(amount);
        match atom.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn saturating_add_i64(atom: &AtomicI64, amount: i64) {
    let mut current = atom.load(Ordering::Relaxed);
    loop {
        let next = current.saturating_add(amount);
        match atom.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prometheus_output_has_fixed_names_and_histogram_buckets() {
        let metrics = ServiceMetrics::new();
        let mut timer = metrics.start_http_request();
        timer.observe_analysis(48_000, -23.0);
        metrics.observe_analysis(24, 48_000, -23.0);
        timer.finish(200, 24);
        metrics.record_busy();
        let output = metrics.render_prometheus();
        assert!(output.contains("forge_service_requests_total 1"));
        assert!(output.contains("forge_service_analysis_loudness_lufs_mean -23.000"));
        assert!(output.contains("le=\"+Inf\""));
        assert!(!output.contains("request_id"));
        assert!(!output.contains("/tmp"));
    }

    #[test]
    fn traceparent_is_strictly_bounded() {
        let trace =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(trace.trace_id.len(), 32);
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(parse_traceparent("not-a-traceparent").is_none());
    }

    #[test]
    fn jsonl_recorder_emits_only_bounded_span_fields() {
        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::new(JsonlSpanRecorder::from_writer(SharedWriter(Arc::clone(
            &bytes,
        ))));
        let metrics = ServiceMetrics::new().with_span_recorder(recorder);
        let mut timer = metrics.start_grpc_request();
        timer.set_traceparent(Some(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ));
        timer.finish(200, 8);
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(text.contains("forge.service.request"));
        assert!(text.contains("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert!(!text.contains("/tmp"));
    }

    #[test]
    fn dropped_timer_records_a_failure_and_releases_in_flight() {
        let metrics = ServiceMetrics::new();
        let _timer = metrics.start_http_request();
        assert!(metrics.render_prometheus().contains("in_flight_requests 1"));
        drop(_timer);
        assert!(metrics.render_prometheus().contains("in_flight_requests 0"));
        assert!(metrics
            .render_prometheus()
            .contains("request_server_errors_total 1"));
    }
}
