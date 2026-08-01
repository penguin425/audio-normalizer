use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

fn spawn_service() -> (Child, String) {
    spawn_service_with_token(None)
}

fn spawn_service_with_token(token: Option<&str>) -> (Child, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let address = address.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge-service"));
    command
        .args(["--bind", &address, "--workers", "1"])
        .env_remove("FORGE_SERVICE_BEARER_TOKEN");
    if let Some(token) = token {
        command.env("FORGE_SERVICE_BEARER_TOKEN", token);
    }
    let child = command.spawn().expect("start forge-service");
    (child, address)
}

fn spawn_metrics_service() -> (Child, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let address = address.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge-service"));
    command
        .args(["--bind", &address, "--workers", "1", "--metrics"])
        .env_remove("FORGE_SERVICE_BEARER_TOKEN");
    let child = command.spawn().expect("start forge-service with metrics");
    (child, address)
}

fn request(address: &str, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).expect("connect service");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

fn wait_for_service(address: &str) {
    wait_for_service_with_auth(address, None);
}

fn wait_for_service_with_auth(address: &str, token: Option<&str>) {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(address) {
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let authorization = token
                .map(|token| format!("Authorization: Bearer {token}\r\n"))
                .unwrap_or_default();
            let request = format!("GET /healthz HTTP/1.1\r\nHost: forge\r\n{authorization}\r\n");
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = Vec::new();
            if stream.read_to_end(&mut response).is_ok() && response.starts_with(b"HTTP/1.1 200") {
                return;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("forge-service did not become ready");
}

fn wave_bytes() -> Vec<u8> {
    let sample_rate = 48_000_u32;
    let frames = sample_rate as usize;
    let mut data = Vec::with_capacity(44 + frames * 2);
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36_u32 + frames as u32 * 2).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16_u32.to_le_bytes());
    data.extend_from_slice(&1_u16.to_le_bytes());
    data.extend_from_slice(&1_u16.to_le_bytes());
    data.extend_from_slice(&sample_rate.to_le_bytes());
    data.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    data.extend_from_slice(&2_u16.to_le_bytes());
    data.extend_from_slice(&16_u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&(frames as u32 * 2).to_le_bytes());
    for frame in 0..frames {
        let phase = frame as f64 * 2.0 * std::f64::consts::PI * 440.0 / sample_rate as f64;
        let sample = (phase.sin() * 12_000.0) as i16;
        data.extend_from_slice(&sample.to_le_bytes());
    }
    data
}

#[test]
fn health_and_upload_analysis_are_schema_shaped() {
    let (mut child, address) = spawn_service();
    wait_for_service(&address);

    let response = request(&address, b"GET /healthz HTTP/1.1\r\nHost: forge\r\n\r\n");
    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert!(String::from_utf8_lossy(&response).contains("service-health-v1"));
    let health_start = response.iter().position(|byte| *byte == b'{').unwrap();
    let health: Value = serde_json::from_slice(&response[health_start..]).unwrap();
    let health_schema: Value =
        serde_json::from_str(include_str!("../schema/service-health-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&health_schema)
        .unwrap()
        .is_valid(&health));

    let body = wave_bytes();
    let head = format!(
        "POST /v1/analyze HTTP/1.1\r\nHost: forge\r\nContent-Type: audio/wav\r\nX-Forge-Filename: sample.wav\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut upload = head.into_bytes();
    upload.extend_from_slice(&body);
    let response = request(&address, &upload);
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let payload = response
        .split(|byte| *byte == b'\n')
        .skip(1)
        .collect::<Vec<_>>();
    let json_start = response.iter().position(|byte| *byte == b'{').unwrap();
    let value: Value = serde_json::from_slice(&response[json_start..]).unwrap();
    assert_eq!(
        value["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/service-analysis-v1"
    );
    assert_eq!(value["filename"], "sample.wav");
    assert!(value["report"]["integrated_lufs"].is_number());
    assert!(!payload.is_empty());
    let schema: Value =
        serde_json::from_str(include_str!("../schema/service-analysis-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&value));

    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn non_loopback_without_token_is_rejected_before_bind() {
    let status = Command::new(env!("CARGO_BIN_EXE_forge-service"))
        .args(["--bind", "0.0.0.0:0"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn bearer_token_is_required_when_configured() {
    let (mut child, address) = spawn_service_with_token(Some("test-token"));
    wait_for_service_with_auth(&address, Some("test-token"));
    let unauthorized = request(&address, b"GET /healthz HTTP/1.1\r\nHost: forge\r\n\r\n");
    assert!(unauthorized.starts_with(b"HTTP/1.1 401"));
    let error_start = unauthorized.iter().position(|byte| *byte == b'{').unwrap();
    let error: Value = serde_json::from_slice(&unauthorized[error_start..]).unwrap();
    let error_schema: Value =
        serde_json::from_str(include_str!("../schema/service-error-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&error_schema)
        .unwrap()
        .is_valid(&error));
    let authorized = request(
        &address,
        b"GET /healthz HTTP/1.1\r\nHost: forge\r\nAuthorization: Bearer test-token\r\n\r\n",
    );
    assert!(authorized.starts_with(b"HTTP/1.1 200"));
    child.kill().unwrap();
    let _ = child.wait();
}

#[test]
fn metrics_endpoint_exposes_bounded_prometheus_text() {
    let (mut child, address) = spawn_metrics_service();
    wait_for_service(&address);
    let response = request(&address, b"GET /metrics HTTP/1.1\r\nHost: forge\r\n\r\n");
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let text = String::from_utf8_lossy(&response);
    assert!(text.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8"));
    assert!(text.contains("forge_service_requests_total"));
    assert!(text.contains("forge_service_request_duration_seconds_bucket"));
    assert!(!text.contains("request_id"));
    child.kill().unwrap();
    let _ = child.wait();
}
