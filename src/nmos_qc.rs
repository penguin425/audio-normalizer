//! Bounded, local-only AMWA NMOS IS-04 and IS-05 snapshot QC.
//!
//! The auditor validates a JSON snapshot of Node API resources and Connection
//! API state. It checks resource identity and relationships, audio-flow
//! metadata, connection state, subscriptions, transport parameters, and
//! embedded RTP SDP. It does not contact registries or Nodes and does not claim
//! live API, DNS-SD, authorization, PTP, or network conformance.

use crate::rtp_qc::{self, RtpAudioProfile};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

pub const NMOS_QC_SCHEMA: &str = "https://penguin425.github.io/audio-normalizer/schema/nmos-qc-v1";
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RESOURCES: usize = 100_000;
const MAX_CONNECTIONS: usize = 100_000;
const RESOURCE_KINDS: [&str; 6] = [
    "nodes",
    "devices",
    "sources",
    "flows",
    "senders",
    "receivers",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Serialize)]
pub struct NmosFinding {
    pub rule_id: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct NmosAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<NmosFinding>,
    pub properties: Value,
}

#[derive(Default)]
struct Snapshot {
    resources: HashMap<&'static str, Vec<Value>>,
    sender_connections: Map<String, Value>,
    receiver_connections: Map<String, Value>,
    bytes_read: u64,
    files_read: usize,
}

pub fn audit(input: &Path) -> Result<NmosAudit, String> {
    let snapshot = load_snapshot(input)?;
    let mut findings = Vec::new();

    let counts = RESOURCE_KINDS
        .iter()
        .map(|kind| {
            (
                (*kind).to_string(),
                json!(snapshot.resources.get(kind).map_or(0, Vec::len)),
            )
        })
        .collect::<Map<_, _>>();
    let total_resources = counts.values().filter_map(Value::as_u64).sum::<u64>() as usize;
    finding(
        &mut findings,
        "FORGE-NMOS-RESOURCE-LIMIT",
        Severity::Error,
        total_resources <= MAX_RESOURCES,
        "The snapshot stays within the bounded resource limit",
        Some(json!({"resource_count": total_resources, "limit": MAX_RESOURCES})),
    );
    finding(
        &mut findings,
        "FORGE-NMOS-CORE-RESOURCES",
        Severity::Error,
        counts.get("nodes").and_then(Value::as_u64).unwrap_or(0) > 0
            && counts.get("devices").and_then(Value::as_u64).unwrap_or(0) > 0,
        "The IS-04 snapshot contains at least one Node and Device",
        Some(json!({"counts": counts})),
    );

    let mut by_kind: HashMap<&str, HashMap<String, &Value>> = HashMap::new();
    let mut global_ids = HashSet::new();
    let mut malformed = Vec::new();
    let mut invalid_ids = Vec::new();
    let mut duplicate_ids = Vec::new();
    let mut invalid_base = Vec::new();
    let mut invalid_versions = Vec::new();
    let mut invalid_tags = Vec::new();

    for kind in RESOURCE_KINDS {
        let mut index = HashMap::new();
        for (position, resource) in snapshot
            .resources
            .get(kind)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let Some(object) = resource.as_object() else {
                malformed.push(format!("{kind}[{position}]"));
                continue;
            };
            let Some(id) = object.get("id").and_then(Value::as_str) else {
                malformed.push(format!("{kind}[{position}].id"));
                continue;
            };
            if !valid_uuid(id) {
                invalid_ids.push(format!("{kind}:{id}"));
            }
            if index.insert(id.to_string(), resource).is_some()
                || !global_ids.insert(id.to_string())
            {
                duplicate_ids.push(format!("{kind}:{id}"));
            }
            if !nonempty_string(object, "label")
                || !object.get("description").is_some_and(Value::is_string)
            {
                invalid_base.push(format!("{kind}:{id}"));
            }
            if !object
                .get("version")
                .and_then(Value::as_str)
                .is_some_and(valid_tai_version)
            {
                invalid_versions.push(format!("{kind}:{id}"));
            }
            if !object.get("tags").is_some_and(valid_tags_object) {
                invalid_tags.push(format!("{kind}:{id}"));
            }
        }
        by_kind.insert(kind, index);
    }

    finding(
        &mut findings,
        "FORGE-NMOS-RESOURCE-OBJECT",
        Severity::Error,
        malformed.is_empty(),
        "Every IS-04 resource is an object with a string id",
        Some(json!({"invalid": malformed})),
    );
    finding(
        &mut findings,
        "FORGE-NMOS-RESOURCE-ID",
        Severity::Error,
        invalid_ids.is_empty() && duplicate_ids.is_empty(),
        "Resource IDs are globally unique RFC 4122 UUIDs",
        Some(json!({"invalid": invalid_ids, "duplicates": duplicate_ids})),
    );
    finding(
        &mut findings,
        "FORGE-NMOS-RESOURCE-BASE",
        Severity::Error,
        invalid_base.is_empty(),
        "Resources contain label and description strings",
        Some(json!({"invalid": invalid_base})),
    );
    finding(
        &mut findings,
        "FORGE-NMOS-RESOURCE-VERSION",
        Severity::Error,
        invalid_versions.is_empty(),
        "Resource versions use the NMOS TAI seconds:nanoseconds form",
        Some(json!({"invalid": invalid_versions})),
    );
    finding(
        &mut findings,
        "FORGE-NMOS-RESOURCE-TAGS",
        Severity::Error,
        invalid_tags.is_empty(),
        "Resource tags map names to arrays of strings",
        Some(json!({"invalid": invalid_tags})),
    );

    audit_nodes(&by_kind, &mut findings);
    audit_relationships(&by_kind, &mut findings);
    audit_endpoints(&by_kind, &mut findings);
    let audio_properties = audit_audio(&by_kind, &mut findings);
    let connection_properties = audit_connections(&snapshot, &by_kind, &mut findings)?;

    let passed = findings
        .iter()
        .all(|item| item.severity != Severity::Error || item.passed);
    let warning_count = findings
        .iter()
        .filter(|item| item.severity == Severity::Warning && !item.passed)
        .count();
    Ok(NmosAudit {
        schema: NMOS_QC_SCHEMA,
        generator: "forge-nmos-qc",
        path: input.display().to_string(),
        passed,
        warning_count,
        findings,
        properties: json!({
            "scope": {
                "offline_snapshot": true,
                "live_api_validation": false,
                "dns_sd_validation": false,
                "authorization_validation": false,
                "ptp_packet_validation": false,
                "network_reachability": false
            },
            "input": {
                "files_read": snapshot.files_read,
                "bytes_read": snapshot.bytes_read
            },
            "resource_counts": counts,
            "audio": audio_properties,
            "connections": connection_properties
        }),
    })
}

fn load_snapshot(input: &Path) -> Result<Snapshot, String> {
    if input.is_file() {
        let (value, bytes) = read_json(input)?;
        let object = value
            .as_object()
            .ok_or_else(|| "snapshot root must be a JSON object".to_string())?;
        let mut snapshot = Snapshot {
            bytes_read: bytes,
            files_read: 1,
            ..Snapshot::default()
        };
        for kind in RESOURCE_KINDS {
            let values = object
                .get(kind)
                .map(|value| {
                    value
                        .as_array()
                        .cloned()
                        .ok_or_else(|| format!("{kind} must be an array"))
                })
                .transpose()?
                .unwrap_or_default();
            snapshot.resources.insert(kind, values);
        }
        snapshot.sender_connections =
            object_map(object.get("sender_connections"), "sender_connections")?;
        snapshot.receiver_connections =
            object_map(object.get("receiver_connections"), "receiver_connections")?;
        ensure_snapshot_limits(&snapshot)?;
        return Ok(snapshot);
    }
    if !input.is_dir() {
        return Err(format!(
            "{} is not a regular JSON file or directory",
            input.display()
        ));
    }

    let mut snapshot = Snapshot::default();
    for kind in RESOURCE_KINDS {
        let path = input.join(format!("{kind}.json"));
        if path.exists() {
            reject_symlink(&path)?;
            let (value, bytes) = read_json(&path)?;
            let values = value
                .as_array()
                .cloned()
                .ok_or_else(|| format!("{} must contain a JSON array", path.display()))?;
            snapshot.resources.insert(kind, values);
            snapshot.bytes_read = snapshot
                .bytes_read
                .checked_add(bytes)
                .ok_or_else(|| "snapshot byte count overflow".to_string())?;
            snapshot.files_read += 1;
        } else {
            snapshot.resources.insert(kind, Vec::new());
        }
    }
    for (filename, sender) in [
        ("sender-connections.json", true),
        ("receiver-connections.json", false),
    ] {
        let path = input.join(filename);
        if !path.exists() {
            continue;
        }
        reject_symlink(&path)?;
        let (value, bytes) = read_json(&path)?;
        let object = value
            .as_object()
            .cloned()
            .ok_or_else(|| format!("{} must contain a JSON object", path.display()))?;
        if sender {
            snapshot.sender_connections = object;
        } else {
            snapshot.receiver_connections = object;
        }
        snapshot.bytes_read = snapshot
            .bytes_read
            .checked_add(bytes)
            .ok_or_else(|| "snapshot byte count overflow".to_string())?;
        snapshot.files_read += 1;
    }
    if snapshot.files_read == 0 {
        return Err(format!(
            "{} contains none of the supported NMOS snapshot files",
            input.display()
        ));
    }
    ensure_snapshot_limits(&snapshot)?;
    Ok(snapshot)
}

fn read_json(path: &Path) -> Result<(Value, u64), String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {} byte input limit",
            path.display(),
            MAX_FILE_BYTES
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    Ok((value, metadata.len()))
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "snapshot file must not be a symlink: {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_snapshot_limits(snapshot: &Snapshot) -> Result<(), String> {
    if snapshot.bytes_read > MAX_TOTAL_BYTES {
        return Err(format!(
            "snapshot exceeds the {} byte aggregate limit",
            MAX_TOTAL_BYTES
        ));
    }
    let resources = snapshot.resources.values().map(Vec::len).sum::<usize>();
    if resources > MAX_RESOURCES {
        return Err(format!(
            "snapshot exceeds the {MAX_RESOURCES} resource limit"
        ));
    }
    let connections = snapshot
        .sender_connections
        .len()
        .checked_add(snapshot.receiver_connections.len())
        .ok_or_else(|| "connection count overflow".to_string())?;
    if connections > MAX_CONNECTIONS {
        return Err(format!(
            "snapshot exceeds the {MAX_CONNECTIONS} connection limit"
        ));
    }
    Ok(())
}

fn object_map(value: Option<&Value>, name: &str) -> Result<Map<String, Value>, String> {
    value
        .map(|value| {
            value
                .as_object()
                .cloned()
                .ok_or_else(|| format!("{name} must be an object"))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn audit_nodes(by_kind: &HashMap<&str, HashMap<String, &Value>>, findings: &mut Vec<NmosFinding>) {
    let mut invalid_href = Vec::new();
    let mut invalid_api = Vec::new();
    let mut unsupported_api = Vec::new();
    let mut invalid_interfaces = Vec::new();
    let mut duplicate_interfaces = Vec::new();
    let mut invalid_clocks = Vec::new();
    let mut invalid_caps_services = Vec::new();

    for (id, value) in &by_kind["nodes"] {
        let object = value.as_object().expect("indexed object");
        if !object
            .get("href")
            .and_then(Value::as_str)
            .is_some_and(valid_http_uri)
        {
            invalid_href.push(id.clone());
        }
        if !object.get("caps").is_some_and(Value::is_object)
            || !object
                .get("services")
                .and_then(Value::as_array)
                .is_some_and(|services| {
                    services.iter().all(|service| {
                        service.as_object().is_some_and(|service| {
                            service
                                .get("href")
                                .and_then(Value::as_str)
                                .is_some_and(valid_http_uri)
                                && service
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .is_some_and(valid_absolute_uri)
                        })
                    })
                })
        {
            invalid_caps_services.push(id.clone());
        }
        let Some(api) = object.get("api").and_then(Value::as_object) else {
            invalid_api.push(id.clone());
            continue;
        };
        let versions = api.get("versions").and_then(Value::as_array);
        if !versions.is_some_and(|items| {
            !items.is_empty()
                && items
                    .iter()
                    .all(|item| item.as_str().is_some_and(valid_api_version))
        }) {
            invalid_api.push(id.clone());
        } else if !versions.is_some_and(|items| items.iter().any(|item| item == "v1.3")) {
            unsupported_api.push(id.clone());
        }
        if !api
            .get("endpoints")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty() && items.iter().all(valid_api_endpoint))
        {
            invalid_api.push(id.clone());
        }

        let interfaces = object.get("interfaces").and_then(Value::as_array);
        let mut names = HashSet::new();
        if !interfaces.is_some_and(|items| {
            items.iter().all(|item| {
                let Some(interface) = item.as_object() else {
                    return false;
                };
                let Some(name) = interface.get("name").and_then(Value::as_str) else {
                    return false;
                };
                let unique = names.insert(name);
                unique
                    && interface
                        .get("chassis_id")
                        .is_some_and(|value| value.is_null() || value.is_string())
                    && interface
                        .get("port_id")
                        .is_some_and(|value| value.is_null() || value.is_string())
            })
        }) {
            invalid_interfaces.push(id.clone());
        }
        if names.len() != interfaces.map_or(0, Vec::len) {
            duplicate_interfaces.push(id.clone());
        }

        let clocks = object.get("clocks").and_then(Value::as_array);
        let mut clock_names = HashSet::new();
        if !clocks.is_some_and(|items| {
            items.iter().all(|item| {
                let Some(clock) = item.as_object() else {
                    return false;
                };
                let Some(name) = clock.get("name").and_then(Value::as_str) else {
                    return false;
                };
                clock_names.insert(name)
                    && valid_clock_name(name)
                    && matches!(
                        clock.get("ref_type").and_then(Value::as_str),
                        Some("internal" | "ptp")
                    )
                    && (clock.get("ref_type") != Some(&Value::String("ptp".to_string()))
                        || (clock.get("traceable").is_some_and(Value::is_boolean)
                            && clock.get("locked").is_some_and(Value::is_boolean)
                            && clock.get("version").is_some_and(Value::is_string)
                            && clock
                                .get("gmid")
                                .and_then(Value::as_str)
                                .is_some_and(valid_gmid)))
            })
        }) {
            invalid_clocks.push(id.clone());
        }
    }
    finding(
        findings,
        "FORGE-NMOS-NODE-HREF",
        Severity::Error,
        invalid_href.is_empty(),
        "Node href values are absolute HTTP(S) URIs",
        Some(json!({"invalid_node_ids": invalid_href})),
    );
    finding(
        findings,
        "FORGE-NMOS-NODE-CAPABILITIES",
        Severity::Error,
        invalid_caps_services.is_empty(),
        "Node capabilities and service declarations are well formed",
        Some(json!({"invalid_node_ids": invalid_caps_services})),
    );
    finding(
        findings,
        "FORGE-NMOS-NODE-API",
        Severity::Error,
        invalid_api.is_empty(),
        "Node API declarations contain valid versions and endpoints",
        Some(json!({"invalid_node_ids": invalid_api})),
    );
    finding(
        findings,
        "FORGE-NMOS-IS04-V13",
        Severity::Warning,
        unsupported_api.is_empty(),
        "Nodes advertise IS-04 API v1.3",
        Some(json!({"nodes_without_v1_3": unsupported_api})),
    );
    finding(
        findings,
        "FORGE-NMOS-NODE-INTERFACES",
        Severity::Error,
        invalid_interfaces.is_empty() && duplicate_interfaces.is_empty(),
        "Node interfaces have unique names and valid identifiers",
        Some(json!({
            "invalid_node_ids": invalid_interfaces,
            "duplicate_interface_node_ids": duplicate_interfaces
        })),
    );
    finding(
        findings,
        "FORGE-NMOS-NODE-CLOCKS",
        Severity::Error,
        invalid_clocks.is_empty(),
        "Node clocks have unique names and valid internal or PTP declarations",
        Some(json!({"invalid_node_ids": invalid_clocks})),
    );
}

fn audit_relationships(
    by_kind: &HashMap<&str, HashMap<String, &Value>>,
    findings: &mut Vec<NmosFinding>,
) {
    let mut errors = Vec::new();
    let mut parent_errors = Vec::new();
    let mut unresolved_parents = Vec::new();
    for (id, value) in &by_kind["devices"] {
        let object = value.as_object().expect("indexed object");
        reference(
            &mut errors,
            "device",
            id,
            object,
            "node_id",
            &by_kind["nodes"],
            false,
        );
        reference_array(
            &mut errors,
            "device",
            id,
            object,
            "senders",
            &by_kind["senders"],
        );
        reference_array(
            &mut errors,
            "device",
            id,
            object,
            "receivers",
            &by_kind["receivers"],
        );
    }
    for (id, value) in &by_kind["sources"] {
        let object = value.as_object().expect("indexed object");
        reference(
            &mut errors,
            "source",
            id,
            object,
            "device_id",
            &by_kind["devices"],
            false,
        );
        parent_array(
            &mut parent_errors,
            &mut unresolved_parents,
            "source",
            id,
            object,
            "parents",
            &by_kind["sources"],
        );
    }
    for (id, value) in &by_kind["flows"] {
        let object = value.as_object().expect("indexed object");
        reference(
            &mut errors,
            "flow",
            id,
            object,
            "source_id",
            &by_kind["sources"],
            false,
        );
        reference(
            &mut errors,
            "flow",
            id,
            object,
            "device_id",
            &by_kind["devices"],
            false,
        );
        parent_array(
            &mut parent_errors,
            &mut unresolved_parents,
            "flow",
            id,
            object,
            "parents",
            &by_kind["flows"],
        );
    }
    for (kind, target) in [("senders", "flows"), ("receivers", "devices")] {
        for (id, value) in &by_kind[kind] {
            let object = value.as_object().expect("indexed object");
            reference(
                &mut errors,
                kind.trim_end_matches('s'),
                id,
                object,
                "device_id",
                &by_kind["devices"],
                false,
            );
            if kind == "senders" {
                reference(
                    &mut errors,
                    "sender",
                    id,
                    object,
                    "flow_id",
                    &by_kind[target],
                    true,
                );
            }
        }
    }
    finding(
        findings,
        "FORGE-NMOS-RESOURCE-GRAPH",
        Severity::Error,
        errors.is_empty(),
        "IS-04 resource references resolve within the snapshot",
        Some(json!({"errors": errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-PARENT-ID",
        Severity::Error,
        parent_errors.is_empty(),
        "Source and Flow parent entries are UUIDs",
        Some(json!({"errors": parent_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-PARENT-PRESENT",
        Severity::Warning,
        unresolved_parents.is_empty(),
        "Source and Flow parents are also present in this snapshot",
        Some(json!({"unresolved": unresolved_parents})),
    );

    let mut membership_errors = Vec::new();
    for (id, value) in &by_kind["senders"] {
        let device_id = value.get("device_id").and_then(Value::as_str);
        if let Some(device) = device_id.and_then(|key| by_kind["devices"].get(key)) {
            if !device
                .get("senders")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item == id))
            {
                membership_errors.push(format!("sender {id} missing from device senders"));
            }
        }
    }
    for (id, value) in &by_kind["receivers"] {
        let device_id = value.get("device_id").and_then(Value::as_str);
        if let Some(device) = device_id.and_then(|key| by_kind["devices"].get(key)) {
            if !device
                .get("receivers")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item == id))
            {
                membership_errors.push(format!("receiver {id} missing from device receivers"));
            }
        }
    }
    finding(
        findings,
        "FORGE-NMOS-DEVICE-MEMBERSHIP",
        Severity::Error,
        membership_errors.is_empty(),
        "Device sender and receiver membership is reciprocal",
        Some(json!({"errors": membership_errors})),
    );
}

fn audit_endpoints(
    by_kind: &HashMap<&str, HashMap<String, &Value>>,
    findings: &mut Vec<NmosFinding>,
) {
    let mut device_errors = Vec::new();
    let mut endpoint_errors = Vec::new();
    let mut binding_errors = Vec::new();
    for (id, value) in &by_kind["devices"] {
        let object = value.as_object().expect("indexed object");
        if !object
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(valid_absolute_uri)
            || !object
                .get("controls")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().all(|item| {
                        item.as_object().is_some_and(|control| {
                            control
                                .get("href")
                                .and_then(Value::as_str)
                                .is_some_and(valid_http_uri)
                                && control
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .is_some_and(valid_absolute_uri)
                        })
                    })
                })
        {
            device_errors.push(id.clone());
        }
    }
    for kind in ["senders", "receivers"] {
        for (id, value) in &by_kind[kind] {
            let object = value.as_object().expect("indexed object");
            let subscription_peer = if kind == "senders" {
                "receiver_id"
            } else {
                "sender_id"
            };
            let endpoint_valid = object
                .get("transport")
                .and_then(Value::as_str)
                .is_some_and(|transport| transport.starts_with("urn:x-nmos:transport:"))
                && object
                    .get("subscription")
                    .and_then(Value::as_object)
                    .is_some_and(|subscription| {
                        subscription
                            .get(subscription_peer)
                            .is_some_and(nullable_uuid)
                            && subscription.get("active").is_some_and(Value::is_boolean)
                    })
                && (kind == "receivers"
                    || object.get("manifest_href").is_some_and(|href| {
                        href.is_null() || href.as_str().is_some_and(valid_http_uri)
                    }))
                && (kind == "senders"
                    || (object.get("caps").is_some_and(Value::is_object)
                        && object
                            .get("format")
                            .and_then(Value::as_str)
                            .is_some_and(|format| format.starts_with("urn:x-nmos:format:"))));
            if !endpoint_valid {
                endpoint_errors.push(format!("{}:{id}", kind.trim_end_matches('s')));
            }

            let bindings = object.get("interface_bindings").and_then(Value::as_array);
            let node_interfaces = object
                .get("device_id")
                .and_then(Value::as_str)
                .and_then(|device_id| by_kind["devices"].get(device_id))
                .and_then(|device| device.get("node_id"))
                .and_then(Value::as_str)
                .and_then(|node_id| by_kind["nodes"].get(node_id))
                .and_then(|node| node.get("interfaces"))
                .and_then(Value::as_array);
            if !bindings.is_some_and(|items| {
                !items.is_empty()
                    && items.iter().all(|binding| {
                        binding.as_str().is_some_and(|binding| {
                            node_interfaces.is_some_and(|interfaces| {
                                interfaces.iter().any(|interface| {
                                    interface.get("name").and_then(Value::as_str) == Some(binding)
                                })
                            })
                        })
                    })
            }) {
                binding_errors.push(format!("{}:{id}", kind.trim_end_matches('s')));
            }
        }
    }
    finding(
        findings,
        "FORGE-NMOS-DEVICE-CONTROL",
        Severity::Error,
        device_errors.is_empty(),
        "Devices contain valid type and control declarations",
        Some(json!({"invalid_device_ids": device_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-ENDPOINT-METADATA",
        Severity::Error,
        endpoint_errors.is_empty(),
        "Senders and Receivers contain valid transport, format, and subscription metadata",
        Some(json!({"invalid": endpoint_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-INTERFACE-BINDING",
        Severity::Error,
        binding_errors.is_empty(),
        "Sender and Receiver interface bindings resolve to their Node",
        Some(json!({"invalid": binding_errors})),
    );
}

fn audit_audio(
    by_kind: &HashMap<&str, HashMap<String, &Value>>,
    findings: &mut Vec<NmosFinding>,
) -> Value {
    let mut source_errors = Vec::new();
    let mut flow_errors = Vec::new();
    let mut audio_sources = 0usize;
    let mut audio_flows = 0usize;
    let mut channel_count = 0usize;

    for (id, value) in &by_kind["sources"] {
        if value.get("format").and_then(Value::as_str) != Some("urn:x-nmos:format:audio") {
            continue;
        }
        audio_sources += 1;
        let channels = value.get("channels").and_then(Value::as_array);
        if !channels.is_some_and(|items| {
            !items.is_empty()
                && items.iter().all(|item| {
                    item.as_object().is_some_and(|channel| {
                        nonempty_string(channel, "label")
                            && channel
                                .get("symbol")
                                .is_some_and(|symbol| symbol.is_null() || symbol.is_string())
                    })
                })
        }) {
            source_errors.push(id.clone());
        } else {
            channel_count += channels.map_or(0, Vec::len);
        }
        let clock_name = value.get("clock_name");
        let clock_valid = clock_name.is_some_and(|clock| {
            clock.is_null()
                || clock.as_str().is_some_and(|clock_name| {
                    value
                        .get("device_id")
                        .and_then(Value::as_str)
                        .and_then(|device_id| by_kind["devices"].get(device_id))
                        .and_then(|device| device.get("node_id"))
                        .and_then(Value::as_str)
                        .and_then(|node_id| by_kind["nodes"].get(node_id))
                        .and_then(|node| node.get("clocks"))
                        .and_then(Value::as_array)
                        .is_some_and(|clocks| {
                            clocks.iter().any(|clock| {
                                clock.get("name").and_then(Value::as_str) == Some(clock_name)
                            })
                        })
                })
        });
        if !clock_valid {
            source_errors.push(id.clone());
        }
    }

    for (id, value) in &by_kind["flows"] {
        if value.get("format").and_then(Value::as_str) != Some("urn:x-nmos:format:audio") {
            continue;
        }
        audio_flows += 1;
        let media_type = value.get("media_type").and_then(Value::as_str);
        let sample_rate = value
            .get("sample_rate")
            .and_then(Value::as_object)
            .is_some_and(valid_rational);
        let bit_depth_valid = match media_type {
            Some("audio/L16") => value.get("bit_depth").and_then(Value::as_u64) == Some(16),
            Some("audio/L24") => value.get("bit_depth").and_then(Value::as_u64) == Some(24),
            Some("audio/AM824") => value
                .get("bit_depth")
                .is_none_or(|depth| depth.as_u64() == Some(32)),
            Some(value) if value.starts_with("audio/") => true,
            _ => false,
        };
        let source_matches = value
            .get("source_id")
            .and_then(Value::as_str)
            .and_then(|source_id| by_kind["sources"].get(source_id))
            .is_some_and(|source| {
                source.get("format").and_then(Value::as_str) == Some("urn:x-nmos:format:audio")
            });
        if !sample_rate || !bit_depth_valid || !source_matches {
            flow_errors.push(id.clone());
        }
    }
    finding(
        findings,
        "FORGE-NMOS-AUDIO-SOURCE",
        Severity::Error,
        source_errors.is_empty(),
        "Audio Sources contain valid channels and clock references",
        Some(json!({"invalid_source_ids": source_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-AUDIO-FLOW",
        Severity::Error,
        flow_errors.is_empty(),
        "Audio Flows contain coherent media type, sample rate, and bit depth",
        Some(json!({"invalid_flow_ids": flow_errors})),
    );
    json!({
        "source_count": audio_sources,
        "flow_count": audio_flows,
        "declared_channel_count": channel_count
    })
}

fn audit_connections(
    snapshot: &Snapshot,
    by_kind: &HashMap<&str, HashMap<String, &Value>>,
    findings: &mut Vec<NmosFinding>,
) -> Result<Value, String> {
    let mut unknown = Vec::new();
    let mut invalid_state = Vec::new();
    let mut transport_errors = Vec::new();
    let mut subscription_errors = Vec::new();
    let mut sdp_errors = Vec::new();
    let mut sdp_audits = Vec::new();
    let mut active_senders = HashMap::new();
    let mut active_receivers = HashMap::new();

    for (kind, connections, resources, peer_field) in [
        (
            "sender",
            &snapshot.sender_connections,
            &by_kind["senders"],
            "receiver_id",
        ),
        (
            "receiver",
            &snapshot.receiver_connections,
            &by_kind["receivers"],
            "sender_id",
        ),
    ] {
        for (id, value) in connections {
            if !resources.contains_key(id) {
                unknown.push(format!("{kind}:{id}"));
            }
            let Some(object) = value.as_object() else {
                invalid_state.push(format!("{kind}:{id}"));
                continue;
            };
            let active = object.get("active").and_then(Value::as_object);
            let staged = object.get("staged").and_then(Value::as_object);
            let constraints = object.get("constraints").and_then(Value::as_array);
            if !active.is_some_and(valid_connection_state)
                || !staged.is_some_and(valid_connection_state)
                || !constraints.is_some_and(|items| items.iter().all(Value::is_object))
            {
                invalid_state.push(format!("{kind}:{id}"));
            }
            if !active.is_some_and(|state| state.get(peer_field).is_some_and(nullable_uuid))
                || !staged.is_some_and(|state| state.get(peer_field).is_some_and(nullable_uuid))
            {
                invalid_state.push(format!("{kind}:{id}:{peer_field}"));
            }
            if let Some(state) = active {
                let transports = state
                    .get("transport_params")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !transports.iter().all(valid_transport_params) {
                    transport_errors.push(format!("{kind}:{id}"));
                }
                if constraints.is_some_and(|items| items.len() != transports.len()) {
                    transport_errors.push(format!("{kind}:{id}:constraints"));
                }
                if resources
                    .get(id)
                    .and_then(|resource| resource.get("interface_bindings"))
                    .and_then(Value::as_array)
                    .is_some_and(|bindings| bindings.len() != transports.len())
                {
                    transport_errors.push(format!("{kind}:{id}:interface-bindings"));
                }
                if state.get("master_enable").and_then(Value::as_bool) == Some(true) {
                    if let Some(peer) = state.get(peer_field).and_then(Value::as_str) {
                        if kind == "sender" {
                            active_senders.insert(id.clone(), peer.to_string());
                        } else {
                            active_receivers.insert(id.clone(), peer.to_string());
                        }
                    }
                }
            }
            if kind == "sender" {
                if let Some((data, media_type)) = transport_file(object) {
                    if media_type != "application/sdp" {
                        sdp_errors.push(format!("{id}: transport file type {media_type}"));
                    } else {
                        let mut file = tempfile::NamedTempFile::new()
                            .map_err(|error| format!("create temporary SDP: {error}"))?;
                        file.write_all(data.as_bytes())
                            .map_err(|error| format!("write temporary SDP: {error}"))?;
                        let profile = sender_profile(id, by_kind);
                        match rtp_qc::audit(file.path(), None, profile) {
                            Ok(audit) => {
                                if !audit.passed {
                                    sdp_errors.push(format!("{id}: embedded SDP failed RTP QC"));
                                }
                                sdp_errors.extend(crosscheck_sender_sdp(
                                    id,
                                    &audit.properties,
                                    object,
                                    by_kind,
                                ));
                                sdp_audits.push(json!({
                                    "sender_id": id,
                                    "profile": profile,
                                    "passed": audit.passed,
                                    "warning_count": audit.warning_count,
                                    "findings": audit.findings,
                                    "properties": audit.properties
                                }));
                            }
                            Err(error) => sdp_errors.push(format!("{id}: {error}")),
                        }
                    }
                }
            }
        }
    }

    for (sender, receiver) in &active_senders {
        if !by_kind["receivers"].contains_key(receiver) {
            subscription_errors.push(format!("sender {sender} references receiver {receiver}"));
        } else if active_receivers.get(receiver) != Some(sender) {
            subscription_errors.push(format!(
                "sender {sender} and receiver {receiver} are not reciprocal"
            ));
        }
        if by_kind["senders"]
            .get(sender)
            .and_then(|value| value.get("subscription"))
            .and_then(|value| value.get("receiver_id"))
            .and_then(Value::as_str)
            != Some(receiver)
        {
            subscription_errors.push(format!(
                "sender {sender} IS-04 subscription differs from IS-05 active state"
            ));
        }
    }
    for (receiver, sender) in &active_receivers {
        if !by_kind["senders"].contains_key(sender) {
            subscription_errors.push(format!("receiver {receiver} references sender {sender}"));
        } else if active_senders.get(sender) != Some(receiver) {
            subscription_errors.push(format!(
                "receiver {receiver} and sender {sender} are not reciprocal"
            ));
        }
        if by_kind["receivers"]
            .get(receiver)
            .and_then(|value| value.get("subscription"))
            .and_then(|value| value.get("sender_id"))
            .and_then(Value::as_str)
            != Some(sender)
        {
            subscription_errors.push(format!(
                "receiver {receiver} IS-04 subscription differs from IS-05 active state"
            ));
        }
    }

    finding(
        findings,
        "FORGE-NMOS-IS05-RESOURCE",
        Severity::Error,
        unknown.is_empty(),
        "IS-05 connection entries correspond to IS-04 Senders and Receivers",
        Some(json!({"unknown": unknown})),
    );
    finding(
        findings,
        "FORGE-NMOS-IS05-STATE",
        Severity::Error,
        invalid_state.is_empty(),
        "IS-05 active, staged, activation, constraints, and transport state is well formed",
        Some(json!({"invalid": invalid_state})),
    );
    finding(
        findings,
        "FORGE-NMOS-IS05-TRANSPORT",
        Severity::Error,
        transport_errors.is_empty(),
        "IS-05 RTP transport parameters contain valid addresses and ports",
        Some(json!({"invalid": transport_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-SUBSCRIPTION",
        Severity::Error,
        subscription_errors.is_empty(),
        "IS-04 subscriptions and IS-05 active connections are reciprocal",
        Some(json!({"errors": subscription_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-TRANSPORT-SDP",
        Severity::Error,
        sdp_errors.is_empty(),
        "Embedded sender transport files are valid RTP audio SDP",
        Some(json!({"errors": sdp_errors})),
    );
    finding(
        findings,
        "FORGE-NMOS-IS05-PRESENT",
        Severity::Warning,
        !snapshot.sender_connections.is_empty() || !snapshot.receiver_connections.is_empty(),
        "The snapshot includes IS-05 connection state",
        Some(json!({
            "sender_connection_count": snapshot.sender_connections.len(),
            "receiver_connection_count": snapshot.receiver_connections.len()
        })),
    );

    Ok(json!({
        "sender_connection_count": snapshot.sender_connections.len(),
        "receiver_connection_count": snapshot.receiver_connections.len(),
        "active_sender_count": active_senders.len(),
        "active_receiver_count": active_receivers.len(),
        "embedded_sdp_count": sdp_audits.len(),
        "embedded_sdp_audits": sdp_audits
    }))
}

fn reference(
    errors: &mut Vec<String>,
    kind: &str,
    id: &str,
    object: &Map<String, Value>,
    field: &str,
    target: &HashMap<String, &Value>,
    nullable: bool,
) {
    let Some(value) = object.get(field) else {
        errors.push(format!("{kind} {id} missing {field}"));
        return;
    };
    if nullable && value.is_null() {
        return;
    }
    let Some(reference) = value.as_str() else {
        errors.push(format!("{kind} {id} has non-string {field}"));
        return;
    };
    if !target.contains_key(reference) {
        errors.push(format!("{kind} {id} {field} references {reference}"));
    }
}

fn reference_array(
    errors: &mut Vec<String>,
    kind: &str,
    id: &str,
    object: &Map<String, Value>,
    field: &str,
    target: &HashMap<String, &Value>,
) {
    let Some(items) = object.get(field).and_then(Value::as_array) else {
        errors.push(format!("{kind} {id} missing array {field}"));
        return;
    };
    for item in items {
        match item.as_str() {
            Some(reference) if target.contains_key(reference) => {}
            Some(reference) => {
                errors.push(format!("{kind} {id} {field} references {reference}"));
            }
            None => errors.push(format!("{kind} {id} has non-string {field} entry")),
        }
    }
}

fn parent_array(
    errors: &mut Vec<String>,
    unresolved: &mut Vec<String>,
    kind: &str,
    id: &str,
    object: &Map<String, Value>,
    field: &str,
    target: &HashMap<String, &Value>,
) {
    let Some(items) = object.get(field).and_then(Value::as_array) else {
        errors.push(format!("{kind} {id} missing array {field}"));
        return;
    };
    for item in items {
        match item.as_str() {
            Some(reference) if !valid_uuid(reference) => {
                errors.push(format!("{kind} {id} has invalid parent {reference}"));
            }
            Some(reference) if !target.contains_key(reference) => {
                unresolved.push(format!("{kind}:{id}:{reference}"));
            }
            Some(_) => {}
            None => errors.push(format!("{kind} {id} has non-string parent")),
        }
    }
}

fn valid_connection_state(state: &Map<String, Value>) -> bool {
    if !state.get("master_enable").is_some_and(Value::is_boolean)
        || !state
            .get("transport_params")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().all(Value::is_object))
    {
        return false;
    }
    let Some(activation) = state.get("activation").and_then(Value::as_object) else {
        return false;
    };
    let mode = activation.get("mode");
    let mode_valid = mode.is_some_and(|value| {
        value.is_null()
            || matches!(
                value.as_str(),
                Some(
                    "activate_immediate"
                        | "activate_scheduled_absolute"
                        | "activate_scheduled_relative"
                )
            )
    });
    let time_valid = |field: &str| {
        activation
            .get(field)
            .is_some_and(|value| value.is_null() || value.as_str().is_some_and(valid_tai_version))
    };
    mode_valid
        && time_valid("requested_time")
        && time_valid("activation_time")
        && state.get("sender_id").is_none_or(nullable_uuid)
        && state.get("receiver_id").is_none_or(nullable_uuid)
}

fn valid_transport_params(value: &Value) -> bool {
    let Some(params) = value.as_object() else {
        return false;
    };
    for field in ["source_ip", "destination_ip", "interface_ip"] {
        if let Some(value) = params.get(field) {
            if !value.is_null()
                && !value
                    .as_str()
                    .is_some_and(|text| text == "auto" || text.parse::<std::net::IpAddr>().is_ok())
            {
                return false;
            }
        }
    }
    for field in ["source_port", "destination_port"] {
        if let Some(value) = params.get(field) {
            let minimum = if field == "source_port" { 0 } else { 1 };
            if !value.is_null()
                && value.as_str() != Some("auto")
                && !value
                    .as_u64()
                    .is_some_and(|port| (minimum..=u16::MAX as u64).contains(&port))
            {
                return false;
            }
        }
    }
    params
        .get("rtp_enabled")
        .is_none_or(|value| value.as_str() == Some("auto") || value.is_boolean())
}

fn transport_file(object: &Map<String, Value>) -> Option<(&str, &str)> {
    let transport = object
        .get("transport_file")
        .or_else(|| object.get("transportfile"))?
        .as_object()?;
    Some((
        transport.get("data")?.as_str()?,
        transport.get("type")?.as_str()?,
    ))
}

fn sender_profile(
    sender_id: &str,
    by_kind: &HashMap<&str, HashMap<String, &Value>>,
) -> RtpAudioProfile {
    let media_type = by_kind["senders"]
        .get(sender_id)
        .and_then(|sender| sender.get("flow_id"))
        .and_then(Value::as_str)
        .and_then(|flow_id| by_kind["flows"].get(flow_id))
        .and_then(|flow| flow.get("media_type"))
        .and_then(Value::as_str);
    if media_type == Some("audio/AM824") {
        RtpAudioProfile::Smpte2110_31
    } else {
        RtpAudioProfile::Smpte2110_30
    }
}

fn crosscheck_sender_sdp(
    sender_id: &str,
    rtp_properties: &Value,
    connection: &Map<String, Value>,
    by_kind: &HashMap<&str, HashMap<String, &Value>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(stream) = rtp_properties.get("stream").and_then(Value::as_object) else {
        return vec![format!("{sender_id}: RTP audit omitted stream properties")];
    };
    let transport = connection
        .get("active")
        .and_then(|value| value.get("transport_params"))
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_object);
    if let Some(transport) = transport {
        for (is05_field, rtp_field) in [
            ("destination_ip", "destination"),
            ("source_ip", "source_filter"),
        ] {
            if let Some(expected) = transport.get(is05_field).and_then(Value::as_str) {
                if expected != "auto"
                    && stream.get(rtp_field).and_then(Value::as_str) != Some(expected)
                {
                    errors.push(format!(
                        "{sender_id}: SDP {rtp_field} differs from active {is05_field}"
                    ));
                }
            }
        }
        if let Some(expected) = transport.get("destination_port").and_then(Value::as_u64) {
            if stream.get("port").and_then(Value::as_u64) != Some(expected) {
                errors.push(format!(
                    "{sender_id}: SDP port differs from active destination_port"
                ));
            }
        }
    }

    let flow = by_kind["senders"]
        .get(sender_id)
        .and_then(|sender| sender.get("flow_id"))
        .and_then(Value::as_str)
        .and_then(|flow_id| by_kind["flows"].get(flow_id));
    if let Some(flow) = flow {
        if let Some(media_type) = flow.get("media_type").and_then(Value::as_str) {
            let expected = media_type
                .strip_prefix("audio/")
                .unwrap_or(media_type)
                .to_ascii_uppercase();
            if stream
                .get("encoding")
                .and_then(Value::as_str)
                .is_none_or(|actual| actual.to_ascii_uppercase() != expected)
            {
                errors.push(format!(
                    "{sender_id}: SDP encoding differs from IS-04 Flow media_type"
                ));
            }
        }
        if let Some(rate) = flow
            .get("sample_rate")
            .and_then(Value::as_object)
            .and_then(|rate| rate.get("numerator"))
            .and_then(Value::as_u64)
        {
            if stream.get("clock_rate").and_then(Value::as_u64) != Some(rate) {
                errors.push(format!(
                    "{sender_id}: SDP clock rate differs from IS-04 Flow sample rate"
                ));
            }
        }
        let source_channels = flow
            .get("source_id")
            .and_then(Value::as_str)
            .and_then(|source_id| by_kind["sources"].get(source_id))
            .and_then(|source| source.get("channels"))
            .and_then(Value::as_array)
            .map(Vec::len);
        if source_channels.is_some_and(|channels| {
            stream.get("channels").and_then(Value::as_u64) != Some(channels as u64)
        }) {
            errors.push(format!(
                "{sender_id}: SDP channel count differs from IS-04 Source channels"
            ));
        }
    }
    errors
}

fn nonempty_string(object: &Map<String, Value>, field: &str) -> bool {
    object
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
}

fn valid_tags_object(value: &Value) -> bool {
    value.as_object().is_some_and(|tags| {
        tags.values().all(|value| {
            value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string))
        })
    })
}

fn valid_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
    }) && matches!(value.as_bytes()[14], b'1' | b'2' | b'3' | b'4' | b'5')
        && matches!(
            value.as_bytes()[19].to_ascii_lowercase(),
            b'8' | b'9' | b'a' | b'b'
        )
}

fn nullable_uuid(value: &Value) -> bool {
    value.is_null() || value.as_str().is_some_and(valid_uuid)
}

fn valid_tai_version(value: &str) -> bool {
    let Some((seconds, nanoseconds)) = value.split_once(':') else {
        return false;
    };
    !seconds.is_empty()
        && seconds.bytes().all(|byte| byte.is_ascii_digit())
        && (1..=9).contains(&nanoseconds.len())
        && nanoseconds.bytes().all(|byte| byte.is_ascii_digit())
        && nanoseconds
            .parse::<u32>()
            .is_ok_and(|number| number < 1_000_000_000)
}

fn valid_api_version(value: &str) -> bool {
    let Some(version) = value.strip_prefix('v') else {
        return false;
    };
    let Some((major, minor)) = version.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_api_endpoint(value: &Value) -> bool {
    let Some(endpoint) = value.as_object() else {
        return false;
    };
    endpoint
        .get("host")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
        && endpoint
            .get("port")
            .and_then(Value::as_u64)
            .is_some_and(|port| (1..=u16::MAX as u64).contains(&port))
        && matches!(
            endpoint.get("protocol").and_then(Value::as_str),
            Some("http" | "https")
        )
}

fn valid_http_uri(value: &str) -> bool {
    (value.starts_with("http://") || value.starts_with("https://"))
        && value
            .split_once("://")
            .is_some_and(|(_, rest)| !rest.is_empty() && !rest.starts_with('/'))
        && !value.bytes().any(|byte| byte.is_ascii_whitespace())
}

fn valid_absolute_uri(value: &str) -> bool {
    value.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && !rest.is_empty()
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    }) && !value.bytes().any(|byte| byte.is_ascii_whitespace())
}

fn valid_clock_name(value: &str) -> bool {
    value.strip_prefix("clk").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn valid_gmid(value: &str) -> bool {
    value.len() == 23
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 2 | 5 | 8 | 11 | 14 | 17 | 20) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn valid_rational(value: &Map<String, Value>) -> bool {
    value.get("numerator").and_then(Value::as_u64).unwrap_or(0) > 0
        && value
            .get("denominator")
            .is_none_or(|item| item.as_u64().is_some_and(|number| number > 0))
}

fn finding(
    findings: &mut Vec<NmosFinding>,
    rule_id: &'static str,
    severity: Severity,
    passed: bool,
    message: impl Into<String>,
    observed: Option<Value>,
) {
    findings.push(NmosFinding {
        rule_id,
        severity,
        passed,
        message: message.into(),
        observed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_uuid_and_tai_forms() {
        assert!(valid_uuid("11111111-1111-4111-8111-111111111111"));
        assert!(!valid_uuid("11111111-1111-4111-7111-111111111111"));
        assert!(valid_tai_version("1710000000:000000001"));
        assert!(valid_tai_version("1710000000:1"));
        assert!(!valid_tai_version("1710000000:1000000000"));
    }

    #[test]
    fn validates_transport_values() {
        assert!(valid_transport_params(&json!({
            "source_ip": "192.0.2.10",
            "destination_ip": "239.1.2.3",
            "destination_port": 5004,
            "rtp_enabled": true
        })));
        assert!(!valid_transport_params(&json!({"destination_port": 70000})));
    }
}
