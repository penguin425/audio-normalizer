use serde_json::Value;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;

fn sdp() -> &'static str {
    "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.1\r\n\
s=Forge RTP test\r\n\
c=IN IP4 239.10.20.30/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 96\r\n\
a=rtpmap:96 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n\
a=source-filter: incl IN IP4 239.10.20.30 192.0.2.1\r\n\
a=fmtp:96 channel-order=SMPTE2110.(ST)\r\n"
}

fn push_u16_be(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u32_le(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn ethernet_rtp(sequence: u16, timestamp: u32) -> Vec<u8> {
    let payload = vec![0_u8; 48 * 2 * 3];
    let udp_length = 8 + 12 + payload.len();
    let ip_length = 20 + udp_length;
    let mut frame = Vec::new();
    frame.extend_from_slice(&[1, 0, 94, 10, 20, 30]);
    frame.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
    frame.extend_from_slice(&0x0800_u16.to_be_bytes());
    frame.push(0x45);
    frame.push(0);
    push_u16_be(&mut frame, ip_length as u16);
    frame.extend_from_slice(&[0, 1, 0, 0, 64, 17, 0, 0]);
    frame.extend_from_slice(&Ipv4Addr::new(192, 0, 2, 1).octets());
    frame.extend_from_slice(&Ipv4Addr::new(239, 10, 20, 30).octets());
    push_u16_be(&mut frame, 6000);
    push_u16_be(&mut frame, 5004);
    push_u16_be(&mut frame, udp_length as u16);
    push_u16_be(&mut frame, 0);
    frame.extend_from_slice(&[0x80, 96]);
    push_u16_be(&mut frame, sequence);
    frame.extend_from_slice(&timestamp.to_be_bytes());
    frame.extend_from_slice(&0x1122_3344_u32.to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

fn write_pcap(path: &Path, sequences: &[u16]) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0xd4, 0xc3, 0xb2, 0xa1]);
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    push_u32_le(&mut bytes, 0);
    push_u32_le(&mut bytes, 0);
    push_u32_le(&mut bytes, 65_535);
    push_u32_le(&mut bytes, 1);
    for (index, sequence) in sequences.iter().enumerate() {
        let frame = ethernet_rtp(*sequence, index as u32 * 48);
        push_u32_le(&mut bytes, 1_700_000_000);
        push_u32_le(&mut bytes, index as u32 * 1000);
        push_u32_le(&mut bytes, frame.len() as u32);
        push_u32_le(&mut bytes, frame.len() as u32);
        bytes.extend_from_slice(&frame);
    }
    fs::write(path, bytes).unwrap();
}

fn push_pcapng_block(output: &mut Vec<u8>, block_type: u32, body: &[u8]) {
    let length = (12 + body.len()) as u32;
    output.extend_from_slice(&block_type.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(body);
    output.extend_from_slice(&length.to_le_bytes());
}

fn write_pcapng(path: &Path, sequences: &[u16]) {
    let mut bytes = Vec::new();
    let mut section = Vec::new();
    section.extend_from_slice(&0x1a2b_3c4d_u32.to_le_bytes());
    section.extend_from_slice(&1_u16.to_le_bytes());
    section.extend_from_slice(&0_u16.to_le_bytes());
    section.extend_from_slice(&u64::MAX.to_le_bytes());
    push_pcapng_block(&mut bytes, 0x0a0d_0d0a, &section);

    let mut interface = Vec::new();
    interface.extend_from_slice(&1_u16.to_le_bytes());
    interface.extend_from_slice(&0_u16.to_le_bytes());
    interface.extend_from_slice(&65_535_u32.to_le_bytes());
    interface.extend_from_slice(&9_u16.to_le_bytes());
    interface.extend_from_slice(&1_u16.to_le_bytes());
    interface.extend_from_slice(&[9, 0, 0, 0]);
    interface.extend_from_slice(&0_u16.to_le_bytes());
    interface.extend_from_slice(&0_u16.to_le_bytes());
    push_pcapng_block(&mut bytes, 1, &interface);

    for (index, sequence) in sequences.iter().enumerate() {
        let frame = ethernet_rtp(*sequence, index as u32 * 48);
        let timestamp = 1_700_000_000_000_000_000_u64 + index as u64 * 1_000_000;
        let mut packet = Vec::new();
        packet.extend_from_slice(&0_u32.to_le_bytes());
        packet.extend_from_slice(&((timestamp >> 32) as u32).to_le_bytes());
        packet.extend_from_slice(&(timestamp as u32).to_le_bytes());
        packet.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        packet.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        packet.extend_from_slice(&frame);
        packet.resize(packet.len().next_multiple_of(4), 0);
        push_pcapng_block(&mut bytes, 6, &packet);
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn valid_st2110_30_capture_passes() {
    let directory = tempfile::tempdir().unwrap();
    let sdp_path = directory.path().join("stream.sdp");
    let capture = directory.path().join("stream.pcap");
    fs::write(&sdp_path, sdp()).unwrap();
    write_pcap(&capture, &[100, 101, 102, 103]);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .args([
            sdp_path.to_str().unwrap(),
            capture.to_str().unwrap(),
            "--profile",
            "smpte2110-30",
            "--compact",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["properties"]["capture"]["rtp_packets"], 4);
    assert_eq!(report["properties"]["stream"]["encoding"], "L24");
}

#[test]
fn valid_pcapng_capture_passes_with_nanosecond_timestamps() {
    let directory = tempfile::tempdir().unwrap();
    let sdp_path = directory.path().join("stream.sdp");
    let capture = directory.path().join("stream.pcapng");
    fs::write(&sdp_path, sdp()).unwrap();
    write_pcapng(&capture, &[100, 101, 102, 103]);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .args([
            sdp_path.to_str().unwrap(),
            capture.to_str().unwrap(),
            "--profile",
            "smpte2110-30",
            "--compact",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["properties"]["capture"]["format"], "pcapng");
    assert_eq!(report["properties"]["capture"]["sections"], 1);
    assert_eq!(report["properties"]["capture"]["interfaces"], 1);
    assert_eq!(
        report["properties"]["capture"]["timestamp_resolutions"][0],
        "10^-9 seconds"
    );
    assert_eq!(report["properties"]["scope"]["pcapng"], true);
    let schema: Value =
        serde_json::from_str(include_str!("../schema/rtp-audio-qc-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(&report)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn sequence_gap_is_a_qc_failure() {
    let directory = tempfile::tempdir().unwrap();
    let sdp_path = directory.path().join("stream.sdp");
    let capture = directory.path().join("stream.pcap");
    fs::write(&sdp_path, sdp()).unwrap();
    write_pcap(&capture, &[100, 102]);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .args([sdp_path.to_str().unwrap(), capture.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], false);
}

#[test]
fn malformed_sdp_is_an_input_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad.sdp");
    fs::write(&path, "not-sdp\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn sdp_only_audit_is_supported() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stream.sdp");
    fs::write(&path, sdp()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .args([path.to_str().unwrap(), "--profile", "aes67"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["warning_count"], 1);
}

#[test]
fn valid_st2110_31_sdp_accepts_am824_pair() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aes3.sdp");
    fs::write(
        &path,
        "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.1\r\n\
s=AM824 test\r\n\
c=IN IP4 239.1.2.3/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 97\r\n\
a=rtpmap:97 AM824/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n\
a=source-filter: incl IN IP4 239.1.2.3 192.0.2.1\r\n\
a=fmtp:97 channel-order=SMPTE2110.(AES3)\r\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .args([path.to_str().unwrap(), "--profile", "smpte2110-31"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["properties"]["stream"]["encoding"], "AM824");
    assert_eq!(report["properties"]["stream"]["channels"], 2);
}

#[test]
fn session_fields_after_media_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("misordered.sdp");
    fs::write(
        &path,
        "v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=x\r\nm=audio 5004 RTP/AVP 96\r\nt=0 0\r\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-rtp-qc"))
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
