//! Explicitly authorized, bounded HTTP observation for DASH clock and origin targets.

use crate::dash_qc;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_BODY_LIMIT: u64 = 1024 * 1024;
const MAX_TIMEOUT_MILLISECONDS: u64 = 60_000;
const MAX_REDIRECT_LIMIT: u32 = 5;
const MAX_REQUEST_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DashObservationKind {
    UtcHttpXsdate,
    UtcHttpIso,
    UtcHttpHead,
    OriginResource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DashObservationTarget {
    pub kind: DashObservationKind,
    pub uri: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct DashObservationOptions {
    pub allowed_origins: Vec<String>,
    pub timeout_milliseconds: u64,
    pub max_body_bytes: u64,
    pub max_redirects: u32,
    pub max_requests: usize,
    pub maximum_clock_offset_milliseconds: u64,
}

impl Default for DashObservationOptions {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            timeout_milliseconds: 5_000,
            max_body_bytes: 64 * 1024,
            max_redirects: 2,
            max_requests: 32,
            maximum_clock_offset_milliseconds: 5_000,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DashObservationReport {
    pub allowed_origins: Vec<String>,
    pub timeout_milliseconds: u64,
    pub max_body_bytes: u64,
    pub max_redirects: u32,
    pub max_requests: usize,
    pub maximum_clock_offset_milliseconds: u64,
    pub target_count: usize,
    pub request_count: usize,
    pub passed: bool,
    pub entries: Vec<DashObservationEntry>,
}

#[derive(Debug, Serialize)]
pub struct DashObservationEntry {
    pub kind: DashObservationKind,
    pub label: String,
    pub requested_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_uri: Option<String>,
    pub redirect_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub elapsed_milliseconds: u64,
    pub response_body_bytes: u64,
    pub response_headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_clock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_offset_milliseconds: Option<i64>,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AllowedOrigin {
    display: String,
    scheme: String,
    host: String,
    port: u16,
}

pub fn observe(
    targets: &[DashObservationTarget],
    options: &DashObservationOptions,
) -> Result<DashObservationReport, String> {
    validate_options(options)?;
    let allowed = options
        .allowed_origins
        .iter()
        .map(|origin| parse_allowed_origin(origin))
        .collect::<Result<Vec<_>, _>>()?;
    if allowed.is_empty() {
        return Err("remote observation requires at least one explicit allowed origin".into());
    }
    if targets.len() > options.max_requests {
        return Err(format!(
            "planned observation target count {} exceeds request limit {}",
            targets.len(),
            options.max_requests
        ));
    }
    for target in targets {
        let url = target_url(&target.uri)?;
        if !origin_allowed(&url, &allowed) {
            return Err(format!(
                "target origin is not explicitly allowed: {}",
                report_uri(&url)
            ));
        }
    }

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(options.timeout_milliseconds)))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build();
    let agent: ureq::Agent = config.into();
    let mut request_count = 0_usize;
    let mut entries = Vec::with_capacity(targets.len());
    for target in targets {
        entries.push(observe_target(
            &agent,
            target,
            options,
            &allowed,
            &mut request_count,
        ));
    }
    let passed = !entries.is_empty() && entries.iter().all(|entry| entry.passed);
    Ok(DashObservationReport {
        allowed_origins: allowed
            .iter()
            .map(|origin| origin.display.clone())
            .collect(),
        timeout_milliseconds: options.timeout_milliseconds,
        max_body_bytes: options.max_body_bytes,
        max_redirects: options.max_redirects,
        max_requests: options.max_requests,
        maximum_clock_offset_milliseconds: options.maximum_clock_offset_milliseconds,
        target_count: targets.len(),
        request_count,
        passed,
        entries,
    })
}

fn validate_options(options: &DashObservationOptions) -> Result<(), String> {
    if !(1..=MAX_TIMEOUT_MILLISECONDS).contains(&options.timeout_milliseconds) {
        return Err(format!(
            "observation timeout must be between 1 and {MAX_TIMEOUT_MILLISECONDS} milliseconds"
        ));
    }
    if !(1..=MAX_BODY_LIMIT).contains(&options.max_body_bytes) {
        return Err(format!(
            "observation body limit must be between 1 and {MAX_BODY_LIMIT} bytes"
        ));
    }
    if options.max_redirects > MAX_REDIRECT_LIMIT {
        return Err(format!(
            "observation redirect limit must not exceed {MAX_REDIRECT_LIMIT}"
        ));
    }
    if !(1..=MAX_REQUEST_LIMIT).contains(&options.max_requests) {
        return Err(format!(
            "observation request limit must be between 1 and {MAX_REQUEST_LIMIT}"
        ));
    }
    if options.maximum_clock_offset_milliseconds > MAX_TIMEOUT_MILLISECONDS {
        return Err(format!(
            "clock offset limit must not exceed {MAX_TIMEOUT_MILLISECONDS} milliseconds"
        ));
    }
    Ok(())
}

fn parse_allowed_origin(value: &str) -> Result<AllowedOrigin, String> {
    let url = Url::parse(value).map_err(|error| format!("invalid allowed origin: {error}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "allowed origin must be an HTTP(S) scheme/host/optional-port with no path, query, fragment, or credentials"
                .to_string(),
        );
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "allowed origin has no usable port".to_string())?;
    let host = normalized_host(url.host_str().unwrap());
    let display = normalized_origin(url.scheme(), &host, port);
    Ok(AllowedOrigin {
        display,
        scheme: url.scheme().to_owned(),
        host,
        port,
    })
}

fn normalized_origin(scheme: &str, host: &str, port: u16) -> String {
    let default = (scheme == "http" && port == 80) || (scheme == "https" && port == 443);
    let bracketed = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if default {
        format!("{scheme}://{bracketed}")
    } else {
        format!("{scheme}://{bracketed}:{port}")
    }
}

fn normalized_host(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase()
}

fn target_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| format!("invalid target URI: {error}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "target must be an absolute HTTP(S) URI without credentials or fragment".to_string(),
        );
    }
    Ok(url)
}

fn origin_allowed(url: &Url, allowed: &[AllowedOrigin]) -> bool {
    let Some(host) = url.host_str().map(normalized_host) else {
        return false;
    };
    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    allowed
        .iter()
        .any(|origin| origin.scheme == url.scheme() && origin.host == host && origin.port == port)
}

fn observe_target(
    agent: &ureq::Agent,
    target: &DashObservationTarget,
    options: &DashObservationOptions,
    allowed: &[AllowedOrigin],
    request_count: &mut usize,
) -> DashObservationEntry {
    let started = Instant::now();
    let mut entry = DashObservationEntry {
        kind: target.kind,
        label: target.label.clone(),
        requested_uri: target_url(&target.uri)
            .map(|url| report_uri(&url))
            .unwrap_or_else(|_| "<invalid-uri>".into()),
        final_uri: None,
        redirect_chain: Vec::new(),
        status: None,
        elapsed_milliseconds: 0,
        response_body_bytes: 0,
        response_headers: BTreeMap::new(),
        observed_clock: None,
        clock_offset_milliseconds: None,
        passed: false,
        error: None,
    };
    let result = observe_target_inner(agent, target, options, allowed, request_count, &mut entry);
    entry.elapsed_milliseconds = duration_milliseconds(started.elapsed());
    if let Err(error) = result {
        entry.error = Some(error);
    }
    entry
}

fn observe_target_inner(
    agent: &ureq::Agent,
    target: &DashObservationTarget,
    options: &DashObservationOptions,
    allowed: &[AllowedOrigin],
    request_count: &mut usize,
    entry: &mut DashObservationEntry,
) -> Result<(), String> {
    let mut current = target_url(&target.uri)?;
    let method_is_head = target.kind == DashObservationKind::UtcHttpHead;
    let mut followed = 0_u32;
    loop {
        if !origin_allowed(&current, allowed) {
            return Err(format!(
                "target origin is not explicitly allowed: {}",
                report_uri(&current)
            ));
        }
        if *request_count == options.max_requests {
            return Err(format!(
                "observation request limit {} exhausted",
                options.max_requests
            ));
        }
        *request_count += 1;
        let mut response = if method_is_head {
            agent.head(current.as_str()).call()
        } else if target.kind == DashObservationKind::OriginResource {
            agent
                .get(current.as_str())
                .header("Range", "bytes=0-0")
                .header("Accept-Encoding", "identity")
                .call()
        } else {
            agent
                .get(current.as_str())
                .header("Accept-Encoding", "identity")
                .call()
        }
        .map_err(|error| format!("request {}: {error}", report_uri(&current)))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            if followed == options.max_redirects {
                return Err(format!(
                    "redirect limit {} exceeded at {}",
                    options.max_redirects,
                    report_uri(&current)
                ));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| format!("redirect {status} has no valid Location header"))?;
            let next = current
                .join(location)
                .map_err(|error| format!("invalid redirect Location header: {error}"))?;
            if !origin_allowed(&next, allowed) {
                return Err(format!(
                    "redirect target origin is not explicitly allowed: {}",
                    report_uri(&next)
                ));
            }
            entry.redirect_chain.push(report_uri(&next));
            current = next;
            followed += 1;
            continue;
        }

        entry.final_uri = Some(report_uri(&current));
        entry.status = Some(status);
        entry.response_headers = selected_headers(response.headers());
        if !(200..300).contains(&status) {
            return Err(format!("HTTP status {status}"));
        }

        let received_at = SystemTime::now();
        if method_is_head {
            let value = response
                .headers()
                .get("date")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "UTC HTTP HEAD response has no valid Date header".to_string())?;
            let observed = httpdate::parse_http_date(value)
                .map_err(|error| format!("invalid HTTP Date header: {error}"))?;
            entry.observed_clock = Some(value.to_owned());
            entry.clock_offset_milliseconds =
                Some(clock_offset_milliseconds(observed, received_at)?);
        } else {
            let mut bytes = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(options.max_body_bytes.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|error| format!("read response body: {error}"))?;
            entry.response_body_bytes = bytes.len() as u64;
            if entry.response_body_bytes > options.max_body_bytes {
                return Err(format!(
                    "response body exceeds the {} byte limit",
                    options.max_body_bytes
                ));
            }
            if matches!(
                target.kind,
                DashObservationKind::UtcHttpXsdate | DashObservationKind::UtcHttpIso
            ) {
                let value = std::str::from_utf8(&bytes)
                    .map_err(|_| "UTC response body is not UTF-8".to_string())?
                    .trim();
                let seconds = dash_qc::parse_xs_datetime_seconds(value)
                    .ok_or_else(|| "UTC response body is not a zoned date-time".to_string())?;
                if !seconds.is_finite() || seconds < 0.0 {
                    return Err("UTC response date-time is outside system range".into());
                }
                let observed = UNIX_EPOCH
                    .checked_add(Duration::from_secs_f64(seconds))
                    .ok_or_else(|| "UTC response date-time is outside system range".to_string())?;
                entry.observed_clock = Some(value.to_owned());
                entry.clock_offset_milliseconds =
                    Some(clock_offset_milliseconds(observed, received_at)?);
            }
        }

        entry.passed = entry.clock_offset_milliseconds.is_none_or(|offset| {
            offset.unsigned_abs() <= options.maximum_clock_offset_milliseconds
        });
        if !entry.passed {
            return Err(format!(
                "clock offset exceeds {} milliseconds",
                options.maximum_clock_offset_milliseconds
            ));
        }
        return Ok(());
    }
}

fn report_uri(url: &Url) -> String {
    let mut redacted = url.clone();
    if redacted.query().is_some() {
        redacted.set_query(Some("redacted"));
    }
    redacted.to_string()
}

fn selected_headers(headers: &ureq::http::HeaderMap) -> BTreeMap<String, String> {
    const NAMES: &[&str] = &[
        "accept-ranges",
        "age",
        "cache-control",
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
            selected.insert((*name).to_owned(), value.to_owned());
        }
    }
    selected
}

fn clock_offset_milliseconds(observed: SystemTime, received_at: SystemTime) -> Result<i64, String> {
    let milliseconds = match observed.duration_since(received_at) {
        Ok(duration) => i128::from(duration_milliseconds(duration)),
        Err(error) => -i128::from(duration_milliseconds(error.duration())),
    };
    i64::try_from(milliseconds).map_err(|_| "clock offset is outside report range".to_string())
}

fn duration_milliseconds(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn serve(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for response in responses {
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
            }
        });
        (format!("http://{address}"), handle)
    }

    fn target(kind: DashObservationKind, uri: String) -> DashObservationTarget {
        DashObservationTarget {
            kind,
            uri,
            label: "test target".into(),
        }
    }

    #[test]
    fn allowed_origins_are_exact_and_canonical() {
        let origin = parse_allowed_origin("https://Example.COM:443").unwrap();
        assert_eq!(origin.display, "https://example.com");
        assert!(origin_allowed(
            &Url::parse("https://example.com/live/segment.m4s").unwrap(),
            std::slice::from_ref(&origin)
        ));
        assert!(!origin_allowed(
            &Url::parse("http://example.com/live/segment.m4s").unwrap(),
            std::slice::from_ref(&origin)
        ));
        assert!(!origin_allowed(
            &Url::parse("https://cdn.example.com/live/segment.m4s").unwrap(),
            &[origin]
        ));

        let ipv6 = parse_allowed_origin("http://[::1]:80").unwrap();
        assert_eq!(ipv6.display, "http://[::1]");
        assert!(origin_allowed(
            &Url::parse("http://[::1]/segment.m4s").unwrap(),
            &[ipv6]
        ));
    }

    #[test]
    fn allowed_origins_reject_paths_credentials_and_fragments() {
        for value in [
            "https://example.com/path",
            "https://user@example.com",
            "https://example.com/#fragment",
            "file:///tmp/media",
        ] {
            assert!(parse_allowed_origin(value).is_err(), "{value}");
        }
    }

    #[test]
    fn observes_http_head_clock_and_bounded_origin_range() {
        let date = httpdate::fmt_http_date(SystemTime::now());
        let responses = vec![
            format!(
                "HTTP/1.1 200 OK\r\nDate: {date}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/8\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\nx".into(),
        ];
        let (origin, server) = serve(responses);
        let report = observe(
            &[
                target(DashObservationKind::UtcHttpHead, format!("{origin}/clock")),
                target(
                    DashObservationKind::OriginResource,
                    format!("{origin}/segment.m4s?token=secret"),
                ),
            ],
            &DashObservationOptions {
                allowed_origins: vec![origin],
                maximum_clock_offset_milliseconds: 2_000,
                ..DashObservationOptions::default()
            },
        )
        .unwrap();
        server.join().unwrap();
        assert!(report.passed);
        assert_eq!(report.request_count, 2);
        assert_eq!(report.entries[0].status, Some(200));
        assert!(report.entries[0]
            .clock_offset_milliseconds
            .is_some_and(|offset| offset.unsigned_abs() <= 2_000));
        assert_eq!(report.entries[1].status, Some(206));
        assert_eq!(report.entries[1].response_body_bytes, 1);
        assert!(!report.entries[1].requested_uri.contains("secret"));
        assert!(report.entries[1].requested_uri.contains("redacted"));
    }

    #[test]
    fn refuses_a_redirect_to_an_unlisted_origin() {
        let (origin, server) = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: http://example.invalid/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let report = observe(
            &[target(
                DashObservationKind::OriginResource,
                format!("{origin}/redirect"),
            )],
            &DashObservationOptions {
                allowed_origins: vec![origin],
                ..DashObservationOptions::default()
            },
        )
        .unwrap();
        server.join().unwrap();
        assert!(!report.passed);
        assert_eq!(report.request_count, 1);
        assert!(report.entries[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not explicitly allowed")));
    }

    #[test]
    fn follows_a_relative_redirect_within_an_allowed_origin() {
        let responses = vec![
            "HTTP/1.1 302 Found\r\nLocation: /media/final.m4s?token=secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/8\r\nConnection: close\r\n\r\nx".into(),
        ];
        let (origin, server) = serve(responses);
        let report = observe(
            &[target(
                DashObservationKind::OriginResource,
                format!("{origin}/redirect"),
            )],
            &DashObservationOptions {
                allowed_origins: vec![origin],
                ..DashObservationOptions::default()
            },
        )
        .unwrap();
        server.join().unwrap();
        assert!(report.passed);
        assert_eq!(report.request_count, 2);
        assert_eq!(report.entries[0].redirect_chain.len(), 1);
        assert!(!report.entries[0].redirect_chain[0].contains("secret"));
        assert_eq!(report.entries[0].status, Some(206));
    }

    #[test]
    fn redirect_cannot_exceed_the_total_request_limit() {
        let (origin, server) = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let report = observe(
            &[target(
                DashObservationKind::OriginResource,
                format!("{origin}/redirect"),
            )],
            &DashObservationOptions {
                allowed_origins: vec![origin],
                max_requests: 1,
                ..DashObservationOptions::default()
            },
        )
        .unwrap();
        server.join().unwrap();
        assert!(!report.passed);
        assert_eq!(report.request_count, 1);
        assert!(report.entries[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("request limit 1 exhausted")));
    }

    #[test]
    fn response_body_limit_is_enforced_after_transfer_decoding() {
        let (origin, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".into(),
        ]);
        let report = observe(
            &[target(
                DashObservationKind::OriginResource,
                format!("{origin}/large"),
            )],
            &DashObservationOptions {
                allowed_origins: vec![origin],
                max_body_bytes: 4,
                ..DashObservationOptions::default()
            },
        )
        .unwrap();
        server.join().unwrap();
        assert!(!report.passed);
        assert_eq!(report.entries[0].response_body_bytes, 5);
        assert!(report.entries[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("exceeds")));
    }
}
