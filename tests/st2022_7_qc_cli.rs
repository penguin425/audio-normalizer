use serde_json::Value;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;

fn sdp(source: Ipv4Addr, destination: Ipv4Addr) -> String {
    format!(
        "v=0\r\n\
o=- 1 1 IN IP4 {source}\r\n\
s=Forge ST 2022-7 test\r\n\
c=IN IP4 {destination}/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 96\r\n\
a=rtpmap:96 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n\
a=source-filter: incl IN IP4 {destination} {source}\r\n\
a=fmtp:96 channel-order=SMPTE2110.(ST)\r\n"
    )
}

fn ethernet_rtp(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    sequence: u16,
    payload_byte: u8,
) -> Vec<u8> {
    let payload = vec![payload_byte; 48 * 2 * 3];
    let udp_length = 8 + 12 + payload.len();
    let ip_length = 20 + udp_length;
    let mut frame = Vec::new();
    frame.extend_from_slice(&[1, 0, 94, 10, 20, 30]);
    frame.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
    frame.extend_from_slice(&0x0800_u16.to_be_bytes());
    frame.extend_from_slice(&[0x45, 0]);
    frame.extend_from_slice(&(ip_length as u16).to_be_bytes());
    frame.extend_from_slice(&[0, 1, 0, 0, 64, 17, 0, 0]);
    frame.extend_from_slice(&source.octets());
    frame.extend_from_slice(&destination.octets());
    frame.extend_from_slice(&6000_u16.to_be_bytes());
    frame.extend_from_slice(&5004_u16.to_be_bytes());
    frame.extend_from_slice(&(udp_length as u16).to_be_bytes());
    frame.extend_from_slice(&0_u16.to_be_bytes());
    frame.extend_from_slice(&[0x80, 96]);
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(&(u32::from(sequence) * 48).to_be_bytes());
    frame.extend_from_slice(&0x1122_3344_u32.to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

fn write_pcap(
    path: &Path,
    source: Ipv4Addr,
    destination: Ipv4Addr,
    packets: &[(u16, u8)],
    skew_us: u32,
) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0xd4, 0xc3, 0xb2, 0xa1]);
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&65_535_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    for (index, (sequence, payload_byte)) in packets.iter().enumerate() {
        let frame = ethernet_rtp(source, destination, *sequence, *payload_byte);
        bytes.extend_from_slice(&1_700_000_000_u32.to_le_bytes());
        bytes.extend_from_slice(&(index as u32 * 1000 + skew_us).to_le_bytes());
        bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&frame);
    }
    fs::write(path, bytes).unwrap();
}

fn fixture(
    primary_packets: &[(u16, u8)],
    secondary_packets: &[(u16, u8)],
) -> (tempfile::TempDir, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let primary_source = Ipv4Addr::new(192, 0, 2, 1);
    let secondary_source = Ipv4Addr::new(192, 0, 2, 2);
    let primary_destination = Ipv4Addr::new(239, 10, 20, 30);
    let secondary_destination = Ipv4Addr::new(239, 10, 20, 31);
    let primary_sdp = directory.path().join("primary.sdp");
    let primary_pcap = directory.path().join("primary.pcap");
    let secondary_sdp = directory.path().join("secondary.sdp");
    let secondary_pcap = directory.path().join("secondary.pcap");
    fs::write(&primary_sdp, sdp(primary_source, primary_destination)).unwrap();
    fs::write(&secondary_sdp, sdp(secondary_source, secondary_destination)).unwrap();
    write_pcap(
        &primary_pcap,
        primary_source,
        primary_destination,
        primary_packets,
        0,
    );
    write_pcap(
        &secondary_pcap,
        secondary_source,
        secondary_destination,
        secondary_packets,
        100,
    );
    let arguments = vec![
        primary_sdp.display().to_string(),
        primary_pcap.display().to_string(),
        secondary_sdp.display().to_string(),
        secondary_pcap.display().to_string(),
        "--max-skew-ms".into(),
        "2".into(),
        "--compact".into(),
    ];
    (directory, arguments)
}

#[test]
fn complementary_leg_loss_is_recovered() {
    let (_directory, arguments) = fixture(
        &[(100, 0), (101, 0), (103, 0)],
        &[(100, 0), (102, 0), (103, 0)],
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-st2022-7-qc"))
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["properties"]["comparison"]["merged_packets"], 4);
    assert_eq!(report["properties"]["comparison"]["missing_after_merge"], 0);
    let schema: Value =
        serde_json::from_str(include_str!("../schema/st2022-7-qc-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(&report)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn payload_mismatch_is_a_qc_failure() {
    let (_directory, arguments) = fixture(&[(100, 0), (101, 0)], &[(100, 0), (101, 1)]);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-st2022-7-qc"))
        .args(arguments)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["properties"]["comparison"]["datagram_mismatches"], 1);
}

#[test]
fn loss_from_both_legs_is_a_qc_failure() {
    let (_directory, arguments) = fixture(&[(100, 0), (102, 0)], &[(100, 0), (102, 0)]);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-st2022-7-qc"))
        .args(arguments)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["properties"]["comparison"]["missing_after_merge"], 1);
}

#[test]
fn arrival_skew_budget_is_enforced() {
    let (_directory, mut arguments) = fixture(&[(100, 0), (101, 0)], &[(100, 0), (101, 0)]);
    let index = arguments
        .iter()
        .position(|argument| argument == "2")
        .unwrap();
    arguments[index] = "0.01".into();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-st2022-7-qc"))
        .args(arguments)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], false);
}

#[test]
fn invalid_skew_budget_is_an_input_error() {
    let (_directory, mut arguments) = fixture(&[(100, 0)], &[(100, 0)]);
    let index = arguments
        .iter()
        .position(|argument| argument == "2")
        .unwrap();
    arguments[index] = "-1".into();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-st2022-7-qc"))
        .args(arguments)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
