use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

const NODE: &str = "11111111-1111-4111-8111-111111111111";
const DEVICE: &str = "22222222-2222-4222-8222-222222222222";
const SOURCE: &str = "33333333-3333-4333-8333-333333333333";
const FLOW: &str = "44444444-4444-4444-8444-444444444444";
const SENDER: &str = "55555555-5555-4555-8555-555555555555";
const RECEIVER: &str = "66666666-6666-4666-8666-666666666666";

fn base(id: &str, label: &str) -> Value {
    json!({
        "id": id,
        "version": "1710000000:000000001",
        "label": label,
        "description": "",
        "tags": {}
    })
}

fn valid_snapshot() -> Value {
    let mut node = base(NODE, "Node");
    node.as_object_mut().unwrap().extend(
        json!({
            "href": "http://192.0.2.10/x-nmos/node/v1.3/",
            "caps": {},
            "services": [],
            "api": {
                "versions": ["v1.3"],
                "endpoints": [{"host": "192.0.2.10", "port": 80, "protocol": "http"}]
            },
            "interfaces": [{
                "name": "eth0",
                "chassis_id": "aa-bb-cc-dd-ee-ff",
                "port_id": "aa-bb-cc-dd-ee-00"
            }],
            "clocks": [{
                "name": "clk0",
                "ref_type": "ptp",
                "traceable": true,
                "version": "IEEE1588-2008",
                "gmid": "00-11-22-ff-fe-33-44-55",
                "locked": true
            }]
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let mut device = base(DEVICE, "Device");
    device.as_object_mut().unwrap().extend(
        json!({
            "node_id": NODE,
            "senders": [SENDER],
            "receivers": [RECEIVER],
            "controls": [],
            "type": "urn:x-nmos:device:generic"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let mut source = base(SOURCE, "Stereo source");
    source.as_object_mut().unwrap().extend(
        json!({
            "device_id": DEVICE,
            "format": "urn:x-nmos:format:audio",
            "caps": {},
            "parents": [],
            "clock_name": "clk0",
            "channels": [
                {"label": "Left", "symbol": "L"},
                {"label": "Right", "symbol": "R"}
            ]
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let mut flow = base(FLOW, "L24 flow");
    flow.as_object_mut().unwrap().extend(
        json!({
            "source_id": SOURCE,
            "device_id": DEVICE,
            "format": "urn:x-nmos:format:audio",
            "parents": [],
            "media_type": "audio/L24",
            "sample_rate": {"numerator": 48000, "denominator": 1},
            "bit_depth": 24
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let mut sender = base(SENDER, "Sender");
    sender.as_object_mut().unwrap().extend(
        json!({
            "flow_id": FLOW,
            "device_id": DEVICE,
            "transport": "urn:x-nmos:transport:rtp.mcast",
            "manifest_href": "http://192.0.2.10/sender.sdp",
            "interface_bindings": ["eth0"],
            "subscription": {"receiver_id": RECEIVER, "active": true}
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let mut receiver = base(RECEIVER, "Receiver");
    receiver.as_object_mut().unwrap().extend(
        json!({
            "device_id": DEVICE,
            "transport": "urn:x-nmos:transport:rtp.mcast",
            "format": "urn:x-nmos:format:audio",
            "caps": {"media_types": ["audio/L24"]},
            "interface_bindings": ["eth0"],
            "subscription": {"sender_id": SENDER, "active": true}
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let active_sender = json!({
        "master_enable": true,
        "activation": {"mode": null, "requested_time": null, "activation_time": null},
        "receiver_id": RECEIVER,
        "transport_params": [{
            "source_ip": "192.0.2.10",
            "destination_ip": "239.1.2.3",
            "destination_port": 5004,
            "rtp_enabled": true
        }]
    });
    let staged_sender = json!({
        "master_enable": true,
        "activation": {
            "mode": null,
            "requested_time": null,
            "activation_time": null
        },
        "receiver_id": RECEIVER,
        "transport_params": [{
            "source_ip": "192.0.2.10",
            "destination_ip": "239.1.2.3",
            "destination_port": 5004,
            "rtp_enabled": true
        }]
    });
    let active_receiver = json!({
        "master_enable": true,
        "activation": {"mode": null, "requested_time": null, "activation_time": null},
        "sender_id": SENDER,
        "transport_params": [{
            "interface_ip": "192.0.2.20",
            "source_ip": "192.0.2.10",
            "destination_ip": "239.1.2.3",
            "destination_port": 5004,
            "rtp_enabled": true
        }]
    });
    let staged_receiver = json!({
        "master_enable": true,
        "activation": {
            "mode": null,
            "requested_time": null,
            "activation_time": null
        },
        "sender_id": SENDER,
        "transport_params": [{
            "interface_ip": "192.0.2.20",
            "source_ip": "192.0.2.10",
            "destination_ip": "239.1.2.3",
            "destination_port": 5004,
            "rtp_enabled": true
        }]
    });
    let sdp = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.10\r\n\
s=NMOS sender\r\n\
c=IN IP4 239.1.2.3/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 96\r\n\
a=source-filter: incl IN IP4 239.1.2.3 192.0.2.10\r\n\
a=rtpmap:96 L24/48000/2\r\n\
a=ptime:1\r\n\
a=fmtp:96 channel-order=SMPTE2110.(ST)\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n";

    json!({
        "nodes": [node],
        "devices": [device],
        "sources": [source],
        "flows": [flow],
        "senders": [sender],
        "receivers": [receiver],
        "sender_connections": {
            (SENDER): {
                "active": active_sender,
                "staged": staged_sender,
                "constraints": [{}],
                "transport_file": {"data": sdp, "type": "application/sdp"}
            }
        },
        "receiver_connections": {
            (RECEIVER): {
                "active": active_receiver,
                "staged": staged_receiver,
                "constraints": [{}]
            }
        }
    })
}

fn write_snapshot(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
fn cli_audits_resource_graph_connections_and_embedded_sdp() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.json");
    write_snapshot(&path, &valid_snapshot());
    let output = Command::new(env!("CARGO_BIN_EXE_forge-nmos-qc"))
        .arg(&path)
        .arg("--compact")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["properties"]["resource_counts"]["senders"], 1);
    assert_eq!(report["properties"]["connections"]["embedded_sdp_count"], 1);
}

#[test]
fn cli_reports_broken_graph_and_subscription_as_qc_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.json");
    let mut snapshot = valid_snapshot();
    snapshot["flows"][0]["source_id"] = Value::String(NODE.to_string());
    snapshot["receiver_connections"][RECEIVER]["active"]["sender_id"] =
        Value::String(NODE.to_string());
    snapshot["sender_connections"][SENDER]["active"]["transport_params"][0]["destination_ip"] =
        Value::String("239.9.9.9".to_string());
    write_snapshot(&path, &snapshot);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-nmos-qc"))
        .arg(&path)
        .arg("--compact")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| {
            finding["rule_id"] == "FORGE-NMOS-RESOURCE-GRAPH" && finding["passed"] == false
        }));
    assert!(report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| {
            finding["rule_id"] == "FORGE-NMOS-SUBSCRIPTION" && finding["passed"] == false
        }));
    assert!(report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| {
            finding["rule_id"] == "FORGE-NMOS-TRANSPORT-SDP" && finding["passed"] == false
        }));
}

#[test]
fn cli_uses_exit_two_for_malformed_json() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.json");
    fs::write(&path, b"{").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-nmos-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("parse"));
}

#[test]
fn directory_snapshot_form_is_supported() {
    let directory = tempfile::tempdir().unwrap();
    let snapshot = valid_snapshot();
    for kind in [
        "nodes",
        "devices",
        "sources",
        "flows",
        "senders",
        "receivers",
    ] {
        write_snapshot(
            &directory.path().join(format!("{kind}.json")),
            &snapshot[kind],
        );
    }
    write_snapshot(
        &directory.path().join("sender-connections.json"),
        &snapshot["sender_connections"],
    );
    write_snapshot(
        &directory.path().join("receiver-connections.json"),
        &snapshot["receiver_connections"],
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-nmos-qc"))
        .arg(directory.path())
        .arg("--compact")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
}
