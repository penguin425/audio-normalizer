//! Explicitly authorized, bounded HTTP Range access for remote media objects.
//!
//! Remote access is deliberately separate from local decoding and normalization.
//! Callers must provide an exact origin allow-list; redirects are re-authorized,
//! credentials are rejected, and every response is bounded before its bytes are
//! exposed. S3 and GCS object URIs are translated to their public HTTPS
//! virtual-hosted endpoints, but no credentials or cloud SDK are accepted.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

use crate::stable_input::{StableInput, StableInputOptions};

pub const REMOTE_QC_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/remote-qc-v1";
pub const REMOTE_RANGE_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/remote-range-v1";
pub const REMOTE_MATERIALIZATION_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/remote-materialization-v1";

const MAX_TIMEOUT_MILLISECONDS: u64 = 60_000;
const MAX_RANGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_OBJECT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_REQUESTS: usize = 1024;
const MAX_REDIRECTS: u32 = 8;
const MAX_HEADER_VALUE_BYTES: usize = 4096;

/// Limits and trust policy for a remote range reader.
#[derive(Clone, Debug)]
pub struct RemoteRangeOptions {
    /// Exact HTTP(S) origins, or s3://bucket / gs://bucket origins, allowed.
    pub allowed_origins: Vec<String>,
    pub timeout_milliseconds: u64,
    pub max_range_bytes: u64,
    pub max_total_bytes: u64,
    pub max_object_bytes: u64,
    pub max_requests: usize,
    pub max_redirects: u32,
    /// Permit plain HTTP only when explicitly requested by the caller.
    pub allow_insecure_http: bool,
}

impl Default for RemoteRangeOptions {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            timeout_milliseconds: 5_000,
            max_range_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_object_bytes: 4 * 1024 * 1024 * 1024,
            max_requests: 128,
            max_redirects: 2,
            allow_insecure_http: false,
        }
    }
}

/// A redacted record of one HTTP range transaction.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteRangeEntry {
    pub requested_start: u64,
    pub requested_end: u64,
    pub requested_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_uri: Option<String>,
    pub redirect_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub returned_start: Option<u64>,
    pub returned_end: Option<u64>,
    pub object_size_bytes: Option<u64>,
    pub response_bytes: u64,
    pub response_headers: BTreeMap<String, String>,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Redacted, bounded evidence from a remote range session.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteFetchReport {
    pub schema: &'static str,
    pub requested_uri: String,
    pub allowed_origins: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_uri: Option<String>,
    pub object_size_bytes: Option<u64>,
    pub bytes_fetched: u64,
    pub request_count: usize,
    pub range_count: usize,
    pub ranges: Vec<RemoteRangeEntry>,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Evidence for one bounded, whole-object response captured as an immutable
/// [`StableInput`]. No validator is required because bytes from separate
/// representation responses are never combined.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize)]
pub struct RemoteMaterializationReport {
    pub schema: &'static str,
    pub requested_uri: String,
    pub allowed_origins: Vec<String>,
    pub final_uri: String,
    pub redirect_chain: Vec<String>,
    pub status: u16,
    pub object_size_bytes: u64,
    pub sha256: String,
    pub response_headers: BTreeMap<String, String>,
    pub passed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AllowedOrigin {
    display: String,
    scheme: String,
    host: String,
    port: u16,
}

/// A validated remote object URI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteObjectUri {
    original: String,
    request: Url,
    canonical_origin: String,
}

impl RemoteObjectUri {
    /// Parse HTTPS, S3, or GCS object syntax without accepting credentials.
    pub fn parse(value: &str, allow_insecure_http: bool) -> Result<Self, String> {
        let source = Url::parse(value).map_err(|error| format!("invalid remote URI: {error}"))?;
        let request = match source.scheme() {
            "s3" => cloud_object_url(&source, "s3")?,
            "gs" => cloud_object_url(&source, "gs")?,
            "http" | "https" => {
                validate_http_url(&source, allow_insecure_http)?;
                source
            }
            _ => {
                return Err("remote URI must use https, s3, or gs".to_string());
            }
        };
        let canonical_origin = canonical_origin(&request)?;
        Ok(Self {
            original: value.to_owned(),
            request,
            canonical_origin,
        })
    }

    pub fn original(&self) -> &str {
        &self.original
    }

    pub fn request_url(&self) -> &Url {
        &self.request
    }

    pub fn canonical_origin(&self) -> &str {
        &self.canonical_origin
    }

    pub fn redacted_uri(&self) -> String {
        redacted_uri(&self.request)
    }
}

/// A seekable, on-demand remote reader backed exclusively by HTTP Range.
pub struct RemoteRangeReader {
    agent: ureq::Agent,
    target: RemoteObjectUri,
    allowed: Vec<AllowedOrigin>,
    options: RemoteRangeOptions,
    position: u64,
    object_size: Option<u64>,
    cache_start: u64,
    cache: Vec<u8>,
    strong_etag: Option<String>,
    representation_url: Option<Url>,
    report: RemoteFetchReport,
}

impl RemoteRangeReader {
    /// Validate the URI and policy without making a network request.
    pub fn open(uri: &str, options: RemoteRangeOptions) -> Result<Self, String> {
        validate_options(&options)?;
        let target = RemoteObjectUri::parse(uri, options.allow_insecure_http)?;
        let allowed = options
            .allowed_origins
            .iter()
            .map(|origin| parse_allowed_origin(origin, options.allow_insecure_http))
            .collect::<Result<Vec<_>, _>>()?;
        if allowed.is_empty() {
            return Err("remote access requires at least one explicit allowed origin".into());
        }
        if !origin_allowed(&target.request, &allowed) {
            return Err(format!(
                "remote target origin is not explicitly allowed: {}",
                target.redacted_uri()
            ));
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(options.timeout_milliseconds)))
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .build();
        let agent: ureq::Agent = config.into();
        let allowed_origins = allowed
            .iter()
            .map(|origin| origin.display.clone())
            .collect();
        Ok(Self {
            agent,
            target: target.clone(),
            allowed,
            options,
            position: 0,
            object_size: None,
            cache_start: 0,
            cache: Vec::new(),
            strong_etag: None,
            representation_url: None,
            report: RemoteFetchReport {
                schema: REMOTE_RANGE_SCHEMA,
                requested_uri: target.redacted_uri(),
                allowed_origins,
                final_uri: None,
                object_size_bytes: None,
                bytes_fetched: 0,
                request_count: 0,
                range_count: 0,
                ranges: Vec::new(),
                passed: false,
                error: None,
            },
        })
    }

    pub fn object_size(&self) -> Option<u64> {
        self.object_size
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn report(&self) -> RemoteFetchReport {
        self.report.clone()
    }

    /// Strong entity tag binding every successful range in this session.
    /// A single range may be read without one; a second network response may
    /// not be combined unless this value is present.
    pub fn strong_etag(&self) -> Option<&str> {
        self.strong_etag.as_deref()
    }

    /// Fetch one bounded range without changing the reader cursor.
    pub fn read_range_at(&mut self, start: u64, length: u64) -> Result<Vec<u8>, String> {
        if length == 0 {
            return Ok(Vec::new());
        }
        if length > self.options.max_range_bytes {
            return Err(format!(
                "requested range length {length} exceeds {} bytes",
                self.options.max_range_bytes
            ));
        }
        let end = start
            .checked_add(length - 1)
            .ok_or_else(|| "requested range end overflows u64".to_string())?;
        self.fetch_range(start, end)
    }

    fn fetch_range(&mut self, start: u64, requested_end: u64) -> Result<Vec<u8>, String> {
        if self.report.range_count > 0 && self.strong_etag.is_none() {
            return self.fail(
                "remote object has no strong ETag; refusing to combine multiple range responses; materialize one bounded snapshot instead"
                    .into(),
            );
        }
        if self.report.request_count >= self.options.max_requests {
            return self.fail(format!(
                "remote request limit {} exhausted",
                self.options.max_requests
            ));
        }
        if self.object_size.is_some_and(|size| start >= size) {
            return Ok(Vec::new());
        }
        let mut current = self.target.request.clone();
        let mut redirects = 0_u32;
        let mut redirect_chain = Vec::new();
        let requested_uri = redacted_uri(&current);
        loop {
            if !origin_allowed(&current, &self.allowed) {
                return self.fail(format!(
                    "redirect target origin is not explicitly allowed: {}",
                    redacted_uri(&current)
                ));
            }
            if self.report.request_count >= self.options.max_requests {
                return self.fail(format!(
                    "remote request limit {} exhausted",
                    self.options.max_requests
                ));
            }
            self.report.request_count += 1;
            let range_header = format!("bytes={start}-{requested_end}");
            let mut request = self
                .agent
                .get(current.as_str())
                .header("Range", &range_header)
                .header("Accept-Encoding", "identity");
            if let Some(etag) = self.strong_etag.as_deref() {
                request = request.header("If-Range", etag);
            }
            let mut response = match request.call() {
                Ok(response) => response,
                Err(error) => {
                    return self.fail(format!("request {}: {error}", redacted_uri(&current)));
                }
            };
            let status = response.status().as_u16();
            if (300..400).contains(&status) {
                if redirects >= self.options.max_redirects {
                    return self.fail(format!(
                        "redirect limit {} exceeded at {}",
                        self.options.max_redirects,
                        redacted_uri(&current)
                    ));
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| format!("redirect {status} has no valid Location header"))?;
                if location.len() > MAX_HEADER_VALUE_BYTES {
                    return self.fail("redirect Location header is too large".into());
                }
                let next = current
                    .join(location)
                    .map_err(|error| format!("invalid redirect Location header: {error}"))?;
                validate_http_url(&next, self.options.allow_insecure_http)?;
                if !origin_allowed(&next, &self.allowed) {
                    return self.fail(format!(
                        "redirect target origin is not explicitly allowed: {}",
                        redacted_uri(&next)
                    ));
                }
                redirect_chain.push(redacted_uri(&next));
                current = next;
                redirects += 1;
                continue;
            }

            let selected = selected_headers(response.headers());
            let final_uri = redacted_uri(&current);
            self.report.final_uri = Some(final_uri.clone());
            let mut entry = RemoteRangeEntry {
                requested_start: start,
                requested_end,
                requested_uri: requested_uri.clone(),
                final_uri: Some(final_uri),
                redirect_chain,
                status: Some(status),
                returned_start: None,
                returned_end: None,
                object_size_bytes: None,
                response_bytes: 0,
                response_headers: selected,
                passed: false,
                error: None,
            };
            if status != 206 {
                let error = if status == 200 {
                    if self.strong_etag.is_some() {
                        "remote representation changed or origin did not honor If-Range; refusing to combine responses"
                            .to_string()
                    } else {
                        "origin did not honor Range (HTTP 200 would require an unbounded full download)"
                            .to_string()
                    }
                } else {
                    format!("HTTP status {status}; expected 206 Partial Content")
                };
                entry.error = Some(error.clone());
                self.report.ranges.push(entry);
                return self.fail(error);
            }
            if response
                .headers()
                .get("content-encoding")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
            {
                return self.fail(
                    "range response used a non-identity Content-Encoding; byte offsets are not safe to combine"
                        .into(),
                );
            }
            let content_range = response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "206 response has no valid Content-Range header".to_string())?;
            let (returned_start, returned_end, object_size) = parse_content_range(content_range)?;
            let response_etag = strong_etag(response.headers()).map(ToOwned::to_owned);
            if let Some(expected_etag) = self.strong_etag.as_deref() {
                if response_etag.as_deref() != Some(expected_etag) {
                    return self.fail(
                        "remote strong ETag changed or was omitted; refusing to combine range responses"
                            .into(),
                    );
                }
            }
            if self
                .representation_url
                .as_ref()
                .is_some_and(|previous| previous != &current)
            {
                return self.fail(
                    "remote redirect resolved to a different representation URI; refusing to combine range responses"
                        .into(),
                );
            }
            if returned_start != start
                || returned_end < returned_start
                || returned_end > requested_end
            {
                return self.fail(format!(
                    "Content-Range bytes {returned_start}-{returned_end} does not match requested {start}-{requested_end}"
                ));
            }
            if object_size > self.options.max_object_bytes {
                return self.fail(format!(
                    "remote object size {object_size} exceeds {} bytes",
                    self.options.max_object_bytes
                ));
            }
            if let Some(previous) = self.object_size {
                if previous != object_size {
                    return self.fail(format!(
                        "remote object size changed from {previous} to {object_size} bytes"
                    ));
                }
            }
            let expected = returned_end
                .checked_sub(returned_start)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| "Content-Range length overflows u64".to_string())?;
            if expected
                > self
                    .options
                    .max_total_bytes
                    .saturating_sub(self.report.bytes_fetched)
            {
                let error = format!(
                    "remote byte limit {} would be exceeded by this range",
                    self.options.max_total_bytes
                );
                entry.error = Some(error.clone());
                self.report.ranges.push(entry);
                return self.fail(error);
            }
            if let Some(value) = response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok())
            {
                let advertised = value
                    .parse::<u64>()
                    .map_err(|_| "206 response has an invalid Content-Length".to_string())?;
                if advertised != expected {
                    return self.fail(format!(
                        "Content-Length {advertised} disagrees with Content-Range length {expected}"
                    ));
                }
            }
            let max_read = expected
                .checked_add(1)
                .ok_or_else(|| "response read limit overflows u64".to_string())?;
            let mut bytes = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(max_read)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("read remote range response: {error}"))?;
            let received = u64::try_from(bytes.len())
                .map_err(|_| "remote response length exceeds u64".to_string())?;
            entry.response_bytes = received;
            entry.returned_start = Some(returned_start);
            entry.returned_end = Some(returned_end);
            entry.object_size_bytes = Some(object_size);
            if received != expected {
                let error =
                    format!("remote range body contains {received} bytes, expected {expected}");
                entry.error = Some(error.clone());
                self.report.ranges.push(entry);
                return self.fail(error);
            }
            let total = self
                .report
                .bytes_fetched
                .checked_add(received)
                .ok_or_else(|| "remote byte counter overflows u64".to_string())?;
            if total > self.options.max_total_bytes {
                let error = format!(
                    "remote byte limit {} exceeded",
                    self.options.max_total_bytes
                );
                entry.error = Some(error.clone());
                self.report.ranges.push(entry);
                return self.fail(error);
            }
            self.report.bytes_fetched = total;
            self.report.range_count += 1;
            self.object_size = Some(object_size);
            self.report.object_size_bytes = Some(object_size);
            if self.strong_etag.is_none() {
                self.strong_etag = response_etag;
            }
            if self.representation_url.is_none() {
                self.representation_url = Some(current);
            }
            entry.passed = true;
            self.report.ranges.push(entry);
            self.report.passed = true;
            return Ok(bytes);
        }
    }

    fn fail<T>(&mut self, error: String) -> Result<T, String> {
        self.report.passed = false;
        self.report.error = Some(error.clone());
        Err(error)
    }

    fn read_into(&mut self, output: &mut [u8]) -> Result<usize, String> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.object_size.is_some_and(|size| self.position >= size) {
            return Ok(0);
        }
        let cache_end = self.cache_start.saturating_add(self.cache.len() as u64);
        if self.cache.is_empty() || self.position < self.cache_start || self.position >= cache_end {
            let length = self.options.max_range_bytes.min(
                self.object_size
                    .map_or(self.options.max_range_bytes, |size| {
                        size.saturating_sub(self.position)
                    }),
            );
            let bytes = self.read_range_at(self.position, length)?;
            if bytes.is_empty() {
                return Ok(0);
            }
            self.cache_start = self.position;
            self.cache = bytes;
        }
        let offset = usize::try_from(self.position - self.cache_start)
            .map_err(|_| "remote cache offset does not fit usize".to_string())?;
        let available = self.cache.len().saturating_sub(offset);
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.cache[offset..offset + count]);
        self.position = self
            .position
            .checked_add(count as u64)
            .ok_or_else(|| "remote cursor position overflows u64".to_string())?;
        Ok(count)
    }
}

impl Read for RemoteRangeReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read_into(output).map_err(io::Error::other)
    }
}

impl Seek for RemoteRangeReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let base = match position {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
            SeekFrom::End(value) => {
                let size = self.object_size.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "SeekFrom::End requires a previously discovered object size",
                    )
                })?;
                i128::from(size) + i128::from(value)
            }
        };
        if base < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote seek would move before byte zero",
            ));
        }
        let value = u64::try_from(base)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "remote seek exceeds u64"))?;
        if value > self.options.max_object_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote seek exceeds the configured object-size limit",
            ));
        }
        self.position = value;
        Ok(value)
    }
}

/// Fetch one range and return its bytes plus redacted evidence.
pub fn fetch_range(
    uri: &str,
    options: RemoteRangeOptions,
    start: u64,
    length: u64,
) -> Result<(Vec<u8>, RemoteFetchReport), String> {
    let mut reader = RemoteRangeReader::open(uri, options)?;
    let bytes = reader.read_range_at(start, length)?;
    Ok((bytes, reader.report()))
}

/// Capture a whole remote object from one bounded representation response.
///
/// This is the safe fallback for origins that do not provide a strong ETag for
/// range requests. Redirects are re-authorized, compression is rejected, and
/// the body is capped by both `max_total_bytes` and `max_object_bytes` before it
/// becomes an immutable [`StableInput`].
pub fn materialize(
    uri: &str,
    options: RemoteRangeOptions,
) -> Result<(StableInput, RemoteMaterializationReport), String> {
    let session = RemoteRangeReader::open(uri, options)?;
    let byte_limit = session
        .options
        .max_total_bytes
        .min(session.options.max_object_bytes);
    let mut current = session.target.request.clone();
    let mut redirects = 0_u32;
    let mut request_count = 0_usize;
    let mut redirect_chain = Vec::new();

    loop {
        if !origin_allowed(&current, &session.allowed) {
            return Err(format!(
                "redirect target origin is not explicitly allowed: {}",
                redacted_uri(&current)
            ));
        }
        if request_count >= session.options.max_requests {
            return Err(format!(
                "remote request limit {} exhausted",
                session.options.max_requests
            ));
        }
        request_count += 1;
        let mut response = session
            .agent
            .get(current.as_str())
            .header("Accept-Encoding", "identity")
            .call()
            .map_err(|error| format!("request {}: {error}", redacted_uri(&current)))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            if redirects >= session.options.max_redirects {
                return Err(format!(
                    "redirect limit {} exceeded at {}",
                    session.options.max_redirects,
                    redacted_uri(&current)
                ));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| format!("redirect {status} has no valid Location header"))?;
            if location.len() > MAX_HEADER_VALUE_BYTES {
                return Err("redirect Location header is too large".into());
            }
            let next = current
                .join(location)
                .map_err(|error| format!("invalid redirect Location header: {error}"))?;
            validate_http_url(&next, session.options.allow_insecure_http)?;
            if !origin_allowed(&next, &session.allowed) {
                return Err(format!(
                    "redirect target origin is not explicitly allowed: {}",
                    redacted_uri(&next)
                ));
            }
            redirect_chain.push(redacted_uri(&next));
            current = next;
            redirects += 1;
            continue;
        }
        if status != 200 {
            return Err(format!(
                "HTTP status {status}; expected 200 OK for snapshot materialization"
            ));
        }
        if response
            .headers()
            .get("content-encoding")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
        {
            return Err("snapshot response used a non-identity Content-Encoding".into());
        }
        let advertised_length = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| "200 response has an invalid Content-Length".to_string())
            })
            .transpose()?;
        if advertised_length.is_some_and(|length| length > byte_limit) {
            return Err(format!(
                "remote object size {} exceeds snapshot limit {byte_limit} bytes",
                advertised_length.unwrap()
            ));
        }
        let response_headers = selected_headers(response.headers());
        let read_limit = byte_limit
            .checked_add(1)
            .ok_or_else(|| "snapshot read limit overflows u64".to_string())?;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read remote snapshot response: {error}"))?;
        let object_size_bytes = u64::try_from(bytes.len())
            .map_err(|_| "remote snapshot length exceeds u64".to_string())?;
        if object_size_bytes > byte_limit {
            return Err(format!(
                "remote snapshot exceeded {byte_limit} bytes while reading"
            ));
        }
        if let Some(advertised) = advertised_length {
            if advertised != object_size_bytes {
                return Err(format!(
                    "remote snapshot contains {object_size_bytes} bytes, expected {advertised}"
                ));
            }
        }
        let source_name = current
            .path_segments()
            .and_then(|mut segments| segments.rfind(|value| !value.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("remote.bin"));
        let stable_options = StableInputOptions::new(byte_limit)
            .map_err(|error| error.to_string())?
            .with_source_name_hint(source_name);
        let snapshot =
            StableInput::from_bytes(&bytes, &stable_options).map_err(|error| error.to_string())?;
        let report = RemoteMaterializationReport {
            schema: REMOTE_MATERIALIZATION_SCHEMA,
            requested_uri: session.target.redacted_uri(),
            allowed_origins: session.report.allowed_origins.clone(),
            final_uri: redacted_uri(&current),
            redirect_chain,
            status,
            object_size_bytes,
            sha256: hex_digest(snapshot.sha256()),
            response_headers,
            passed: true,
        };
        return Ok((snapshot, report));
    }
}

/// A bounded prefix/header probe suitable for remote QC.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteProbeReport {
    pub schema: &'static str,
    pub uri: String,
    pub requested_start: u64,
    pub requested_length: u64,
    pub returned_bytes: u64,
    pub prefix_sha256: String,
    pub detected_format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wave: Option<WaveHeaderEvidence>,
    pub fetch: RemoteFetchReport,
    pub passed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WaveHeaderEvidence {
    pub container: String,
    pub audio_format_code: Option<u16>,
    pub channels: Option<u16>,
    pub sample_rate_hz: Option<u32>,
    pub bits_per_sample: Option<u16>,
    pub data_offset: Option<u64>,
    pub data_bytes: Option<u64>,
}

pub fn probe(
    uri: &str,
    options: RemoteRangeOptions,
    start: u64,
    length: u64,
) -> Result<RemoteProbeReport, String> {
    if length == 0 {
        return Err("remote probe length must be greater than zero".into());
    }
    let mut reader = RemoteRangeReader::open(uri, options)?;
    let bytes = reader.read_range_at(start, length)?;
    let prefix_sha256 = hex_digest(Sha256::digest(&bytes));
    let detected_format = detect_format(&bytes).to_owned();
    let wave = (start == 0).then(|| parse_wave_header(&bytes)).flatten();
    let fetch = reader.report();
    Ok(RemoteProbeReport {
        schema: REMOTE_QC_SCHEMA,
        uri: fetch.requested_uri.clone(),
        requested_start: start,
        requested_length: length,
        returned_bytes: bytes.len() as u64,
        prefix_sha256,
        detected_format,
        wave,
        passed: fetch.passed,
        fetch,
    })
}

pub fn parse_content_range(value: &str) -> Result<(u64, u64, u64), String> {
    let mut fields = value.split_whitespace();
    let unit = fields
        .next()
        .ok_or_else(|| "Content-Range must be 'bytes start-end/size'".to_string())?;
    let range = fields
        .next()
        .ok_or_else(|| "Content-Range must be 'bytes start-end/size'".to_string())?;
    if fields.next().is_some() {
        return Err("Content-Range has unexpected trailing fields".into());
    }
    if unit != "bytes" {
        return Err("Content-Range unit must be bytes".into());
    }
    let (range, size) = range
        .split_once('/')
        .ok_or_else(|| "Content-Range must include a total size".to_string())?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| "Content-Range must include start and end".to_string())?;
    let start = start
        .parse::<u64>()
        .map_err(|_| "Content-Range start is not an unsigned integer".to_string())?;
    let end = end
        .parse::<u64>()
        .map_err(|_| "Content-Range end is not an unsigned integer".to_string())?;
    let size = size
        .parse::<u64>()
        .map_err(|_| "Content-Range size is not an unsigned integer".to_string())?;
    if size == 0 || start > end || end >= size {
        return Err("Content-Range has invalid bounds".into());
    }
    Ok((start, end, size))
}

fn validate_options(options: &RemoteRangeOptions) -> Result<(), String> {
    if !(1..=MAX_TIMEOUT_MILLISECONDS).contains(&options.timeout_milliseconds) {
        return Err(format!(
            "remote timeout must be between 1 and {MAX_TIMEOUT_MILLISECONDS} milliseconds"
        ));
    }
    if !(1..=MAX_RANGE_BYTES).contains(&options.max_range_bytes) {
        return Err(format!(
            "maximum range must be between 1 and {MAX_RANGE_BYTES} bytes"
        ));
    }
    if !(1..=MAX_TOTAL_BYTES).contains(&options.max_total_bytes) {
        return Err(format!(
            "maximum fetched bytes must be between 1 and {MAX_TOTAL_BYTES}"
        ));
    }
    if !(1..=MAX_OBJECT_BYTES).contains(&options.max_object_bytes) {
        return Err(format!(
            "maximum object size must be between 1 and {MAX_OBJECT_BYTES}"
        ));
    }
    if !(1..=MAX_REQUESTS).contains(&options.max_requests) {
        return Err(format!(
            "maximum requests must be between 1 and {MAX_REQUESTS}"
        ));
    }
    if options.max_redirects > MAX_REDIRECTS {
        return Err(format!("maximum redirects must not exceed {MAX_REDIRECTS}"));
    }
    if options.max_total_bytes < options.max_range_bytes {
        return Err("maximum fetched bytes must cover one maximum range".into());
    }
    Ok(())
}

fn validate_http_url(url: &Url, allow_insecure_http: bool) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https")
        || (url.scheme() == "http" && !allow_insecure_http)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "remote target must be an HTTP(S) URI without credentials or fragment".to_string(),
        );
    }
    Ok(())
}

fn cloud_object_url(source: &Url, scheme: &str) -> Result<Url, String> {
    let bucket = source
        .host_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{scheme} URI must contain a bucket"))?;
    if !source.username().is_empty()
        || source.password().is_some()
        || source.query().is_some()
        || source.fragment().is_some()
    {
        return Err(format!(
            "{scheme} URI cannot contain credentials or fragment"
        ));
    }
    let key = source.path().trim_start_matches('/');
    if key.is_empty() {
        return Err(format!("{scheme} URI must contain an object key"));
    }
    let mut url = if scheme == "s3" {
        Url::parse(&format!("https://{bucket}.s3.amazonaws.com/"))
            .map_err(|error| error.to_string())?
    } else {
        Url::parse("https://storage.googleapis.com/").map_err(|error| error.to_string())?
    };
    if scheme == "s3" {
        url.set_path(&format!("/{key}"));
    } else {
        url.set_path(&format!("/{bucket}/{key}"));
    }
    Ok(url)
}

fn parse_allowed_origin(value: &str, allow_insecure_http: bool) -> Result<AllowedOrigin, String> {
    let source = Url::parse(value).map_err(|error| format!("invalid allowed origin: {error}"))?;
    let url = match source.scheme() {
        "s3" => cloud_origin_url(&source, "s3")?,
        "gs" => cloud_origin_url(&source, "gs")?,
        "http" | "https" => {
            validate_http_url(&source, allow_insecure_http)?;
            source
        }
        _ => return Err("allowed origin must use https, s3, gs, or http".into()),
    };
    if (!url.path().is_empty() && url.path() != "/") || url.query().is_some() {
        return Err(
            "allowed origin must be a scheme/host/optional-port with no path or query".into(),
        );
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "allowed origin has no usable port".to_string())?;
    let host = normalized_host(url.host_str().unwrap());
    Ok(AllowedOrigin {
        display: normalized_origin(url.scheme(), &host, port),
        scheme: url.scheme().to_owned(),
        host,
        port,
    })
}

fn cloud_origin_url(source: &Url, scheme: &str) -> Result<Url, String> {
    let bucket = source
        .host_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{scheme} origin must contain a bucket"))?;
    if !source.username().is_empty()
        || source.password().is_some()
        || (!source.path().is_empty() && source.path() != "/")
        || source.query().is_some()
        || source.fragment().is_some()
    {
        return Err(format!(
            "{scheme} allowed origin must contain only a bucket"
        ));
    }
    if scheme == "s3" {
        Url::parse(&format!("https://{bucket}.s3.amazonaws.com"))
            .map_err(|error| format!("invalid s3 origin: {error}"))
    } else {
        Url::parse("https://storage.googleapis.com")
            .map_err(|error| format!("invalid gs origin: {error}"))
    }
}

fn canonical_origin(url: &Url) -> Result<String, String> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "remote URI has no usable port".to_string())?;
    let host = normalized_host(
        url.host_str()
            .ok_or_else(|| "remote URI has no host".to_string())?,
    );
    Ok(normalized_origin(url.scheme(), &host, port))
}

fn normalized_origin(scheme: &str, host: &str, port: u16) -> String {
    let default = (scheme == "http" && port == 80) || (scheme == "https" && port == 443);
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if default {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    }
}

fn normalized_host(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase()
}

fn origin_allowed(url: &Url, allowed: &[AllowedOrigin]) -> bool {
    let Ok(origin) = canonical_origin(url) else {
        return false;
    };
    allowed.iter().any(|value| value.display == origin)
}

fn redacted_uri(url: &Url) -> String {
    let mut value = url.clone();
    if value.query().is_some() {
        value.set_query(Some("redacted"));
    }
    value.to_string()
}

fn selected_headers(headers: &ureq::http::HeaderMap) -> BTreeMap<String, String> {
    const NAMES: &[&str] = &[
        "accept-ranges",
        "cache-control",
        "content-encoding",
        "content-length",
        "content-range",
        "content-type",
        "date",
        "etag",
        "last-modified",
        "transfer-encoding",
        "via",
    ];
    let mut selected = BTreeMap::new();
    for name in NAMES {
        if let Some(value) = headers.get(*name).and_then(|value| value.to_str().ok()) {
            if value.len() <= MAX_HEADER_VALUE_BYTES {
                selected.insert((*name).to_owned(), value.to_owned());
            }
        }
    }
    selected
}

/// Return an entity tag only when it is syntactically suitable for `If-Range`.
/// Weak validators cannot prove that byte ranges belong to one representation.
fn strong_etag(headers: &ureq::http::HeaderMap) -> Option<&str> {
    let value = headers.get("etag")?.to_str().ok()?.trim();
    if value.starts_with("W/")
        || value.len() < 2
        || !value.starts_with('"')
        || !value.ends_with('"')
        || value[1..value.len() - 1]
            .bytes()
            .any(|byte| byte == b'"' || byte < 0x21 || byte == 0x7f)
    {
        return None;
    }
    Some(value)
}

fn detect_format(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 12
        && matches!(bytes.get(0..4), Some(b"RIFF" | b"RF64" | b"BW64"))
        && bytes.get(8..12) == Some(b"WAVE")
    {
        "wave"
    } else if bytes.starts_with(b"fLaC") {
        "flac"
    } else if bytes.starts_with(b"OggS") {
        "ogg"
    } else if bytes.len() >= 8 && bytes.get(4..8) == Some(b"ftyp") {
        "iso-bmff"
    } else if bytes.starts_with(b"DSD ") {
        "dsf"
    } else if bytes.starts_with(b"FRM8") {
        "dsdiff"
    } else if bytes.len() >= 2 && bytes[0] == 0xff && (bytes[1] & 0xe0) == 0xe0 {
        "mpeg-audio"
    } else {
        "unknown"
    }
}

fn parse_wave_header(bytes: &[u8]) -> Option<WaveHeaderEvidence> {
    if bytes.len() < 12
        || !matches!(bytes.get(0..4), Some(b"RIFF" | b"RF64" | b"BW64"))
        || bytes.get(8..12) != Some(b"WAVE")
    {
        return None;
    }
    let container = std::str::from_utf8(bytes.get(0..4)?).ok()?.to_owned();
    let mut cursor = 12_usize;
    let mut format = None;
    let mut channels = None;
    let mut sample_rate = None;
    let mut bits = None;
    let mut data_offset = None;
    let mut data_bytes = None;
    while cursor.checked_add(8)? <= bytes.len() && cursor < 1024 * 1024 {
        let id = bytes.get(cursor..cursor + 4)?;
        let size = u32::from_le_bytes(bytes.get(cursor + 4..cursor + 8)?.try_into().ok()?) as usize;
        let payload = cursor.checked_add(8)?;
        let end = payload.checked_add(size)?;
        if end > bytes.len() {
            break;
        }
        if id == b"fmt " && size >= 16 {
            format = Some(u16::from_le_bytes(
                bytes.get(payload..payload + 2)?.try_into().ok()?,
            ));
            channels = Some(u16::from_le_bytes(
                bytes.get(payload + 2..payload + 4)?.try_into().ok()?,
            ));
            sample_rate = Some(u32::from_le_bytes(
                bytes.get(payload + 4..payload + 8)?.try_into().ok()?,
            ));
            bits = Some(u16::from_le_bytes(
                bytes.get(payload + 14..payload + 16)?.try_into().ok()?,
            ));
        } else if id == b"data" {
            data_offset = Some(payload as u64);
            data_bytes = Some(size as u64);
        }
        cursor = end + (size & 1);
    }
    Some(WaveHeaderEvidence {
        container,
        audio_format_code: format,
        channels,
        sample_rate_hz: sample_rate,
        bits_per_sample: bits,
        data_offset,
        data_bytes,
    })
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn serve(response: String) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), handle)
    }

    fn serve_ranges() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for (index, (start, body)) in [(0_u64, b"abcd".as_slice()), (4, b"efgh"), (2, b"cdef")]
                .into_iter()
                .enumerate()
            {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                if index == 0 {
                    assert!(!request.contains("if-range:"));
                } else {
                    assert!(request.contains("if-range: \"stable\""));
                }
                let end = start + body.len() as u64 - 1;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                     Content-Range: bytes {start}-{end}/8\r\nETag: \"stable\"\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(header.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        (format!("http://{address}"), handle)
    }

    fn options(origin: &str) -> RemoteRangeOptions {
        RemoteRangeOptions {
            allowed_origins: vec![origin.to_owned()],
            allow_insecure_http: true,
            ..RemoteRangeOptions::default()
        }
    }

    #[test]
    fn parses_cloud_and_rejects_credentials() {
        let s3 = RemoteObjectUri::parse("s3://bucket/audio/file.wav", false).unwrap();
        assert_eq!(s3.canonical_origin(), "https://bucket.s3.amazonaws.com");
        let gs = RemoteObjectUri::parse("gs://bucket/audio/file.wav", false).unwrap();
        assert_eq!(gs.request_url().path(), "/bucket/audio/file.wav");
        let s3_origin = parse_allowed_origin("s3://bucket", false).unwrap();
        assert_eq!(s3_origin.display, "https://bucket.s3.amazonaws.com");
        let gs_origin = parse_allowed_origin("gs://bucket", false).unwrap();
        assert_eq!(gs_origin.display, "https://storage.googleapis.com");
        assert!(RemoteObjectUri::parse("https://user@example.com/audio.wav", false).is_err());
        assert!(RemoteObjectUri::parse("http://example.com/audio.wav", false).is_err());
    }

    #[test]
    fn parses_content_ranges_strictly() {
        assert_eq!(parse_content_range("bytes 0-3/8").unwrap(), (0, 3, 8));
        for value in [
            "bytes */8",
            "bytes 0-8/8",
            "bytes 3-2/8",
            "bytes 0-3/*",
            "items 0-3/8",
        ] {
            assert!(parse_content_range(value).is_err(), "{value}");
        }
    }

    #[test]
    fn reads_only_the_requested_range_and_redacts_queries() {
        let (origin, server) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\
             Content-Range: bytes 2-5/8\r\nContent-Type: audio/wav\r\n\
             Connection: close\r\n\r\ncdef"
                .into(),
        );
        let (bytes, report) = fetch_range(
            &format!("{origin}/audio.wav?token=secret"),
            options(&origin),
            2,
            4,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(bytes, b"cdef");
        assert!(report.passed);
        assert_eq!(report.object_size_bytes, Some(8));
        assert!(!report.requested_uri.contains("secret"));
        assert!(!report.ranges[0].requested_uri.contains("secret"));
    }

    #[test]
    fn reader_streams_ranges_and_supports_seek() {
        let (origin, server) = serve_ranges();
        let mut reader = RemoteRangeReader::open(
            &format!("{origin}/audio.wav"),
            RemoteRangeOptions {
                allowed_origins: vec![origin],
                allow_insecure_http: true,
                max_range_bytes: 4,
                ..RemoteRangeOptions::default()
            },
        )
        .unwrap();
        let mut bytes = [0_u8; 4];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"abcd");
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"efgh");
        assert_eq!(reader.object_size(), Some(8));
        assert_eq!(reader.strong_etag(), Some("\"stable\""));
        reader.seek(SeekFrom::Start(2)).unwrap();
        let mut excerpt = [0_u8; 2];
        reader.read_exact(&mut excerpt).unwrap();
        assert_eq!(&excerpt, b"cd");
        assert_eq!(reader.report().request_count, 3);
        server.join().unwrap();
    }

    #[test]
    fn reader_refuses_to_mix_unvalidated_range_responses() {
        let (origin, server) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\
             Content-Range: bytes 0-3/8\r\nConnection: close\r\n\r\nabcd"
                .into(),
        );
        let mut reader = RemoteRangeReader::open(
            &format!("{origin}/audio.wav"),
            RemoteRangeOptions {
                allowed_origins: vec![origin],
                allow_insecure_http: true,
                max_range_bytes: 4,
                ..RemoteRangeOptions::default()
            },
        )
        .unwrap();
        let mut bytes = [0_u8; 4];
        reader.read_exact(&mut bytes).unwrap();
        reader.seek(SeekFrom::Start(4)).unwrap();
        let error = reader.read_exact(&mut bytes).unwrap_err().to_string();
        assert!(
            error.contains("materialize one bounded snapshot"),
            "{error}"
        );
        assert_eq!(reader.report().request_count, 1);
        server.join().unwrap();
    }

    #[test]
    fn reader_rejects_encoded_range_representations() {
        let (origin, server) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\
             Content-Range: bytes 0-3/8\r\nContent-Encoding: gzip\r\n\
             ETag: \"stable\"\r\nConnection: close\r\n\r\nabcd"
                .into(),
        );
        let mut reader = RemoteRangeReader::open(
            &format!("{origin}/audio.wav"),
            RemoteRangeOptions {
                allowed_origins: vec![origin],
                allow_insecure_http: true,
                max_range_bytes: 4,
                ..RemoteRangeOptions::default()
            },
        )
        .unwrap();
        let error = reader.read_range_at(0, 4).unwrap_err();
        assert!(error.contains("non-identity Content-Encoding"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn reader_rejects_a_changed_strong_validator() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for (index, (body, etag)) in [(b"abcd".as_slice(), "first"), (b"efgh", "second")]
                .into_iter()
                .enumerate()
            {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                if index == 1 {
                    assert!(request.contains("if-range: \"first\""));
                }
                let start = (index * 4) as u64;
                let end = start + 3;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\
                     Content-Range: bytes {start}-{end}/8\r\nETag: \"{etag}\"\r\n\
                     Connection: close\r\n\r\n"
                );
                stream.write_all(header.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let origin = format!("http://{address}");
        let mut reader = RemoteRangeReader::open(
            &format!("{origin}/audio.wav"),
            RemoteRangeOptions {
                allowed_origins: vec![origin],
                allow_insecure_http: true,
                max_range_bytes: 4,
                ..RemoteRangeOptions::default()
            },
        )
        .unwrap();
        let mut bytes = [0_u8; 4];
        reader.read_exact(&mut bytes).unwrap();
        let error = reader.read_exact(&mut bytes).unwrap_err().to_string();
        assert!(error.contains("strong ETag changed"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn materializes_one_unvalidated_response_as_a_stable_snapshot() {
        let (origin, server) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nContent-Type: audio/flac\r\n\
             Connection: close\r\n\r\nabcdefgh"
                .into(),
        );
        let (snapshot, report) = materialize(
            &format!("{origin}/audio.flac?token=secret"),
            options(&origin),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(snapshot.byte_len(), 8);
        assert_eq!(
            snapshot.source_name_hint(),
            Some(std::path::Path::new("audio.flac"))
        );
        assert_eq!(report.object_size_bytes, 8);
        assert_eq!(report.sha256, hex_digest(snapshot.sha256()));
        assert!(!report.requested_uri.contains("secret"));
        assert!(!report.final_uri.contains("secret"));
        let instance = serde_json::to_value(&report).unwrap();
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../schema/remote-materialization-v1.schema.json"
        ))
        .unwrap();
        assert!(jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&instance));
    }

    #[test]
    fn materialization_rejects_an_advertised_oversized_object() {
        let (origin, server) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n".into());
        let result = materialize(
            &format!("{origin}/audio.wav"),
            RemoteRangeOptions {
                allowed_origins: vec![origin],
                allow_insecure_http: true,
                max_range_bytes: 8,
                max_total_bytes: 8,
                ..RemoteRangeOptions::default()
            },
        );
        server.join().unwrap();
        assert!(result.unwrap_err().contains("snapshot limit 8 bytes"));
    }

    #[test]
    fn rejects_origins_that_ignore_range() {
        let (origin, server) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcdefgh".into(),
        );
        let result = fetch_range(&format!("{origin}/audio.wav"), options(&origin), 0, 4);
        server.join().unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("did not honor Range"));
    }

    #[test]
    fn parses_wave_prefix_without_full_object() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&36_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&192_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        let evidence = parse_wave_header(&bytes).unwrap();
        assert_eq!(evidence.channels, Some(2));
        assert_eq!(evidence.sample_rate_hz, Some(48_000));
        assert_eq!(detect_format(&bytes), "wave");
    }
}
