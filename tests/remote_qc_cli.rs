use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

fn wave_header() -> Vec<u8> {
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
    bytes
}

fn serve(bytes: Vec<u8>, status: &str) -> (String, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let ignored_range = status == "200";
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
        let header = if ignored_range {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
        } else {
            let end = bytes.len() - 1;
            format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                 Content-Range: bytes 0-{end}/{}\r\nContent-Type: audio/wav\r\n\
                 Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                bytes.len(),
                bytes.len()
            )
        };
        stream.write_all(header.as_bytes()).unwrap();
        stream.write_all(&bytes).unwrap();
        request
    });
    (format!("http://{address}"), handle)
}

#[test]
fn remote_qc_cli_emits_schema_valid_wave_probe_and_redacts_query() {
    let (origin, server) = serve(wave_header(), "206");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-remote-qc"))
        .arg(format!("{origin}/audio.wav?token=secret"))
        .args(["--allow-origin", &origin, "--allow-insecure-http"])
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert!(output.status.success(), "{output:#?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/remote-qc-v1"
    );
    assert_eq!(report["detected_format"], "wave");
    assert_eq!(report["wave"]["channels"], 2);
    assert_eq!(report["wave"]["sample_rate_hz"], 48_000);
    assert_eq!(report["fetch"]["passed"], true);
    assert!(!report["uri"].as_str().unwrap().contains("secret"));
    assert!(String::from_utf8_lossy(&request)
        .to_ascii_lowercase()
        .contains("range: bytes=0-65535"));

    let schema: Value =
        serde_json::from_str(include_str!("../schema/remote-qc-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(validator.is_valid(&report));
    let fetch_schema: Value =
        serde_json::from_str(include_str!("../schema/remote-range-v1.schema.json")).unwrap();
    let fetch_validator = jsonschema::validator_for(&fetch_schema).unwrap();
    assert!(fetch_validator.is_valid(&report["fetch"]));
}

#[test]
fn remote_qc_cli_fails_closed_when_range_is_ignored() {
    let (origin, server) = serve(wave_header(), "200");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-remote-qc"))
        .arg(format!("{origin}/audio.wav"))
        .args(["--allow-origin", &origin, "--allow-insecure-http"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("did not honor Range"));
}
