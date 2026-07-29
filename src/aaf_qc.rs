//! Bounded structural QC for Advanced Authoring Format (AAF) files.
//!
//! AAF uses the Microsoft Compound File Binary (Structured Storage) wrapper.
//! This module validates that wrapper with bounded structural checks, checks
//! the AAF file and root class identifiers, audits the required root objects,
//! and decodes bounded stored-property/reference-index streams for the core
//! object-model and Edit Protocol layer in `aaf_object_qc`.

use crate::aaf_object_qc::{StoredObject, StoredProperty};
use crate::container_qc::{check, finish_audit, AuditCheck, ContainerAudit};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const CFB_SIGNATURE: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
const AAF_V3_HEADER_CLSID_LE: [u8; 16] = [
    0x41, 0x41, 0x46, 0x42, 0x0d, 0x00, 0x4f, 0x4d, 0x06, 0x0e, 0x2b, 0x34, 0x01, 0x01, 0x01, 0xff,
];
const AAF_V4_HEADER_CLSID_LE: [u8; 16] = [
    0x01, 0x02, 0x01, 0x0d, 0x00, 0x02, 0x00, 0x00, 0x06, 0x0e, 0x2b, 0x34, 0x03, 0x02, 0x01, 0x01,
];
const AAF_ROOT_CLSID: &str = "b3b398a5-1c90-11d4-8053-080036210804";
const MAX_CFB_SECTORS: u64 = 16_777_216;
const MAX_ENTRIES: usize = 250_000;
const MAX_PATH_DEPTH: usize = 64;
const MAX_PROPERTY_STREAM_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_PROPERTY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_INDEX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_REPORTED_PATHS: usize = 100;
const PROPERTY_VERSION: u8 = 32;
const PROPERTY_FORMATS: &[u16] = &[
    0x02, 0x03, 0x12, 0x1a, 0x22, 0x32, 0x3a, 0x40, 0x42, 0x82, 0x86, 0xd2, 0xda,
];

pub(crate) fn looks_like_aaf(header: &[u8]) -> bool {
    header.len() >= 24
        && &header[..8] == CFB_SIGNATURE
        && matches!(
            &header[8..24],
            bytes if bytes == AAF_V3_HEADER_CLSID_LE || bytes == AAF_V4_HEADER_CLSID_LE
        )
}

pub(crate) fn audit(
    path: &Path,
    file: File,
    file_size: u64,
    header: &[u8],
) -> Result<ContainerAudit, String> {
    let mut wrapper = Vec::new();
    let mut bitstream = Vec::new();
    let mut xcheck = Vec::new();

    wrapper.push(check(
        "FORGE-AAF-CFB-SIGNATURE",
        header.len() >= 8 && &header[..8] == CFB_SIGNATURE,
        "AAF Compound File Binary signature is present",
        None,
    ));
    let header_clsid = header.get(8..24).unwrap_or_default();
    let header_clsid_valid =
        header_clsid == AAF_V3_HEADER_CLSID_LE || header_clsid == AAF_V4_HEADER_CLSID_LE;
    wrapper.push(check(
        "FORGE-AAF-FILE-CLSID",
        header_clsid_valid,
        if header_clsid_valid {
            "AAF file CLSID matches the stored-format sector profile"
        } else {
            "Compound file does not carry a recognized AAF file CLSID"
        },
        Some(json!(hex_bytes(header_clsid))),
    ));
    let declared_major = header
        .get(26..28)
        .map(|bytes| u16::from_le_bytes(bytes.try_into().unwrap()));
    let sector_size = match declared_major {
        Some(3) => Some(512_u64),
        Some(4) => Some(4096_u64),
        _ => None,
    };
    let file_profile_matches = matches!(
        (declared_major, header_clsid),
        (Some(3), bytes) if bytes == AAF_V3_HEADER_CLSID_LE
    ) || matches!(
        (declared_major, header_clsid),
        (Some(4), bytes) if bytes == AAF_V4_HEADER_CLSID_LE
    );
    wrapper.push(check(
        "FORGE-AAF-FILE-PROFILE",
        file_profile_matches,
        if file_profile_matches {
            "AAF file CLSID matches the declared CFB sector profile"
        } else {
            "AAF file CLSID does not match the declared CFB sector profile"
        },
        Some(json!({
            "declared_major_version": declared_major,
            "file_clsid": hex_bytes(header_clsid)
        })),
    ));
    let sectors = sector_size.map(|size| file_size.saturating_sub(512).div_ceil(size));
    let sector_limit_ok = sectors.is_some_and(|count| count <= MAX_CFB_SECTORS);
    wrapper.push(check(
        "FORGE-AAF-SECTOR-LIMIT",
        sector_limit_ok,
        if sector_limit_ok {
            "declared CFB geometry is within the allocation-table safety limit"
        } else {
            "declared CFB geometry is invalid or exceeds the allocation-table safety limit"
        },
        Some(json!({
            "declared_major_version": declared_major,
            "sector_size_bytes": sector_size,
            "sectors": sectors,
            "limit": MAX_CFB_SECTORS
        })),
    ));
    if !sector_limit_ok {
        return Ok(finish_audit(
            path,
            "aaf",
            wrapper,
            bitstream,
            xcheck,
            json!({"file_size_bytes": file_size}),
        ));
    }

    let mut compound = match cfb::CompoundFile::open(file) {
        Ok(compound) => {
            wrapper.push(check(
                "FORGE-AAF-CFB-READABLE",
                true,
                "Compound File Binary allocation and directory structure is readable",
                None,
            ));
            compound
        }
        Err(error) => {
            wrapper.push(check(
                "FORGE-AAF-CFB-READABLE",
                false,
                format!("Compound File Binary validation failed: {error}"),
                None,
            ));
            return Ok(finish_audit(
                path,
                "aaf",
                wrapper,
                bitstream,
                xcheck,
                json!({"file_size_bytes": file_size}),
            ));
        }
    };

    let version = compound.version();
    wrapper.push(check(
        "FORGE-AAF-CFB-VERSION",
        matches!(version, cfb::Version::V3 | cfb::Version::V4),
        format!(
            "CFB version {} uses {}-byte sectors",
            version.number(),
            version.sector_len()
        ),
        Some(json!({
            "major_version": version.number(),
            "sector_size_bytes": version.sector_len()
        })),
    ));

    let root_clsid = compound.root_entry().clsid().to_string();
    wrapper.push(check(
        "FORGE-AAF-ROOT-CLSID",
        root_clsid.eq_ignore_ascii_case(AAF_ROOT_CLSID),
        if root_clsid.eq_ignore_ascii_case(AAF_ROOT_CLSID) {
            "root storage CLSID identifies an AAF object store"
        } else {
            "root storage CLSID does not identify an AAF object store"
        },
        Some(json!(root_clsid)),
    ));

    let entries: Vec<EntryInfo> = compound
        .walk()
        .take(MAX_ENTRIES + 1)
        .map(|entry| EntryInfo {
            path: entry.path().to_path_buf(),
            name: entry.name().to_owned(),
            stream: entry.is_stream(),
            storage: entry.is_storage(),
            len: entry.len(),
            clsid: entry.is_storage().then(|| entry.clsid().to_string()),
        })
        .collect();
    let stream_paths: HashSet<PathBuf> = entries
        .iter()
        .filter(|entry| entry.stream)
        .map(|entry| entry.path.clone())
        .collect();
    let entry_limit_ok = entries.len() <= MAX_ENTRIES;
    wrapper.push(check(
        "FORGE-AAF-ENTRY-LIMIT",
        entry_limit_ok,
        if entry_limit_ok {
            format!("{} CFB entries are within the safety limit", entries.len())
        } else {
            format!("AAF entry count exceeds safety limit {MAX_ENTRIES}")
        },
        Some(json!(entries.len().min(MAX_ENTRIES + 1))),
    ));

    let deep_paths: Vec<String> = entries
        .iter()
        .filter(|entry| entry.path.components().count() > MAX_PATH_DEPTH)
        .take(MAX_REPORTED_PATHS)
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect();
    wrapper.push(check(
        "FORGE-AAF-PATH-DEPTH",
        deep_paths.is_empty(),
        if deep_paths.is_empty() {
            "all object paths are within the depth limit"
        } else {
            "one or more object paths exceed the depth limit"
        },
        (!deep_paths.is_empty()).then(|| json!(deep_paths)),
    ));

    let root_properties = entries
        .iter()
        .any(|entry| entry.path == Path::new("/properties"));
    let referenced_properties = entries
        .iter()
        .any(|entry| entry.path == Path::new("/referenced properties"));
    let meta_storages: Vec<&EntryInfo> = entries
        .iter()
        .filter(|entry| {
            entry.storage
                && entry.path.parent() == Some(Path::new("/"))
                && entry.name.starts_with("MetaDictionary-")
        })
        .collect();
    let header_storages: Vec<&EntryInfo> = entries
        .iter()
        .filter(|entry| {
            entry.storage
                && entry.path.parent() == Some(Path::new("/"))
                && entry.name.starts_with("Header-")
        })
        .collect();

    bitstream.push(presence_check(
        "FORGE-AAF-ROOT-PROPERTIES",
        root_properties,
        "root stored-property stream",
    ));
    bitstream.push(presence_check(
        "FORGE-AAF-REFERENCE-PROPERTIES",
        referenced_properties,
        "weak-reference path table",
    ));
    bitstream.push(check(
        "FORGE-AAF-METADICTIONARY",
        meta_storages.len() == 1,
        format!(
            "expected exactly one root MetaDictionary storage; found {}",
            meta_storages.len()
        ),
        Some(json!(meta_storages
            .iter()
            .map(|entry| &entry.name)
            .collect::<Vec<_>>())),
    ));
    bitstream.push(check(
        "FORGE-AAF-HEADER",
        header_storages.len() == 1,
        format!(
            "expected exactly one root Header storage; found {}",
            header_storages.len()
        ),
        Some(json!(header_storages
            .iter()
            .map(|entry| &entry.name)
            .collect::<Vec<_>>())),
    ));

    let property_streams: Vec<EntryInfo> = entries
        .iter()
        .filter(|entry| entry.stream && entry.name == "properties")
        .cloned()
        .collect();
    let total_property_bytes = property_streams
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.len));
    let total_property_ok =
        total_property_bytes.is_some_and(|total| total <= MAX_TOTAL_PROPERTY_BYTES);
    bitstream.push(check(
        "FORGE-AAF-PROPERTY-BYTE-LIMIT",
        total_property_ok,
        if total_property_ok {
            "stored-property streams are within per-stream and aggregate limits"
        } else {
            "stored-property streams exceed the aggregate safety limit"
        },
        Some(json!({
            "streams": property_streams.len(),
            "total_bytes": total_property_bytes
        })),
    ));

    let mut malformed_properties = Vec::new();
    let mut property_entries = 0_u64;
    let mut parsed_properties = HashMap::<PathBuf, Vec<StoredProperty>>::new();
    if entry_limit_ok && total_property_ok {
        for entry in &property_streams {
            if entry.len > MAX_PROPERTY_STREAM_BYTES {
                push_bounded(
                    &mut malformed_properties,
                    format!(
                        "{}: {} bytes exceeds per-stream limit",
                        entry.path.display(),
                        entry.len
                    ),
                );
                continue;
            }
            match read_stream(&mut compound, &entry.path, entry.len)
                .and_then(|bytes| parse_property_stream(&bytes))
            {
                Ok(properties) => {
                    property_entries += properties.len() as u64;
                    if let Some(parent) = entry.path.parent() {
                        parsed_properties.insert(parent.to_path_buf(), properties);
                    }
                }
                Err(error) => push_bounded(
                    &mut malformed_properties,
                    format!("{}: {error}", entry.path.display()),
                ),
            }
        }
    }
    bitstream.push(check(
        "FORGE-AAF-STORED-PROPERTIES",
        !property_streams.is_empty() && malformed_properties.is_empty(),
        if property_streams.is_empty() {
            "AAF contains no stored-property streams".to_owned()
        } else if malformed_properties.is_empty() {
            format!(
                "{} stored-property streams contain {property_entries} bounded entries",
                property_streams.len()
            )
        } else {
            "one or more stored-property streams are malformed".to_owned()
        },
        (!malformed_properties.is_empty()).then(|| json!(malformed_properties)),
    ));

    let reference_result = if referenced_properties {
        let len = entries
            .iter()
            .find(|entry| entry.path == Path::new("/referenced properties"))
            .map_or(0, |entry| entry.len);
        if len > MAX_PROPERTY_STREAM_BYTES {
            Err(format!("{len} bytes exceeds safety limit"))
        } else {
            read_stream(&mut compound, Path::new("/referenced properties"), len)
                .and_then(|bytes| validate_reference_properties(&bytes))
        }
    } else {
        Err("stream is missing".to_owned())
    };
    bitstream.push(check(
        "FORGE-AAF-WEAK-REFERENCE-TABLE",
        reference_result.is_ok(),
        match &reference_result {
            Ok((paths, pids)) => {
                format!("weak-reference table declares {paths} paths and {pids} PID words")
            }
            Err(error) => format!("invalid weak-reference table: {error}"),
        },
        reference_result
            .ok()
            .map(|(paths, pids)| json!({"path_count": paths, "pid_words": pids})),
    ));

    let header_prefix = header_storages.first().map(|entry| entry.path.as_path());
    let header_children: HashSet<String> = header_prefix
        .into_iter()
        .flat_map(|prefix| {
            entries
                .iter()
                .filter(move |entry| entry.storage && entry.path.parent() == Some(prefix))
                .map(|entry| entry.name.clone())
        })
        .collect();
    for (rule, prefix, label) in [
        ("FORGE-AAF-CONTENT-STORAGE", "Content-", "ContentStorage"),
        ("FORGE-AAF-DICTIONARY", "Dictionary-", "Dictionary"),
        ("FORGE-AAF-IDENTIFICATION", "Identifi", "IdentificationList"),
    ] {
        let present = header_children.iter().any(|name| name.starts_with(prefix));
        xcheck.push(presence_check(rule, present, label));
    }

    let meta_prefix = meta_storages.first().map(|entry| entry.path.as_path());
    let has_class_definitions = meta_prefix.is_some_and(|prefix| {
        entries.iter().any(|entry| {
            entry.path.starts_with(prefix) && entry.name.starts_with("ClassDefinitions-")
        })
    });
    let has_type_definitions = meta_prefix.is_some_and(|prefix| {
        entries.iter().any(|entry| {
            entry.path.starts_with(prefix) && entry.name.starts_with("TypeDefinitions-")
        })
    });
    xcheck.push(presence_check(
        "FORGE-AAF-CLASS-DEFINITIONS",
        has_class_definitions,
        "MetaDictionary class definitions",
    ));
    xcheck.push(presence_check(
        "FORGE-AAF-TYPE-DEFINITIONS",
        has_type_definitions,
        "MetaDictionary type definitions",
    ));

    let index_entries: Vec<&EntryInfo> = entries
        .iter()
        .filter(|entry| entry.stream && entry.name.ends_with(" index"))
        .collect();
    let total_index_bytes = index_entries
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.len));
    let index_limit_ok = total_index_bytes.is_some_and(|total| total <= MAX_TOTAL_INDEX_BYTES)
        && index_entries
            .iter()
            .all(|entry| entry.len <= MAX_PROPERTY_STREAM_BYTES);
    let mut index_streams = HashMap::new();
    let mut index_errors = Vec::new();
    if index_limit_ok && entry_limit_ok {
        for entry in &index_entries {
            match read_stream(&mut compound, &entry.path, entry.len) {
                Ok(bytes) => {
                    index_streams.insert(entry.path.clone(), bytes);
                }
                Err(error) => push_bounded(
                    &mut index_errors,
                    format!("{}: {error}", entry.path.display()),
                ),
            }
        }
    } else {
        index_errors.push("strong/weak-reference indexes exceed safety limits".to_owned());
    }
    bitstream.push(check(
        "FORGE-AAF-INDEX-BYTE-LIMIT",
        index_limit_ok && index_errors.is_empty(),
        if index_limit_ok && index_errors.is_empty() {
            format!(
                "{} reference index streams are within bounded read limits",
                index_entries.len()
            )
        } else {
            "one or more reference index streams exceed limits or cannot be read".to_owned()
        },
        (!index_errors.is_empty()).then(|| json!(index_errors)),
    ));

    let object_audit = if malformed_properties.is_empty()
        && index_errors.is_empty()
        && entry_limit_ok
        && total_property_ok
    {
        let mut objects = Vec::with_capacity(parsed_properties.len());
        for (path, properties) in parsed_properties {
            let class_id = if path == Path::new("/") {
                AAF_ROOT_CLSID.to_owned()
            } else {
                entries
                    .iter()
                    .find(|entry| entry.storage && entry.path == path)
                    .and_then(|entry| entry.clsid.clone())
                    .unwrap_or_default()
            };
            objects.push(StoredObject {
                path,
                class_id,
                properties,
            });
        }
        Some(crate::aaf_object_qc::audit(
            &objects,
            &index_streams,
            &stream_paths,
        ))
    } else {
        None
    };
    if let Some(audit) = &object_audit {
        xcheck.extend(audit.checks.clone());
    }

    Ok(finish_audit(
        path,
        "aaf",
        wrapper,
        bitstream,
        xcheck,
        json!({
            "method": "forge-aaf-metadictionary-object-model-edit-protocol-v2",
            "scope": "bounded CFB, stored-property, dynamic MetaDictionary type/class/property interpretation, extension-value validation, core AAF object-model, ownership/reference graph, edit timeline, and supported AAF Edit Protocol QC; opaque payloads are preserved, external resources are never fetched, and this is not an AAF SDK certification",
            "file_size_bytes": file_size,
            "cfb": {
                "version": version.number(),
                "sector_size_bytes": version.sector_len(),
                "entries": entries.len().min(MAX_ENTRIES + 1),
                "storages": entries.iter().filter(|entry| entry.storage).count(),
                "streams": entries.iter().filter(|entry| entry.stream).count()
            },
            "stored_properties": {
                "streams": property_streams.len(),
                "entries": property_entries,
                "total_bytes": total_property_bytes
            },
            "reference_indexes": {
                "streams": index_entries.len(),
                "total_bytes": total_index_bytes
            },
            "object_model": object_audit.map(|audit| audit.properties)
        }),
    ))
}

#[derive(Clone)]
struct EntryInfo {
    path: PathBuf,
    name: String,
    stream: bool,
    storage: bool,
    len: u64,
    clsid: Option<String>,
}

fn presence_check(rule_id: &'static str, passed: bool, description: &'static str) -> AuditCheck {
    check(
        rule_id,
        passed,
        if passed {
            format!("{description} is present")
        } else {
            format!("{description} is missing")
        },
        None,
    )
}

fn read_stream(
    compound: &mut cfb::CompoundFile<File>,
    path: &Path,
    len: u64,
) -> Result<Vec<u8>, String> {
    let capacity = usize::try_from(len).map_err(|_| "stream length exceeds usize".to_owned())?;
    let mut bytes = Vec::with_capacity(capacity);
    compound
        .open_stream(path)
        .map_err(|error| format!("open stream: {error}"))?
        .take(len + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read stream: {error}"))?;
    if bytes.len() != capacity {
        return Err(format!(
            "declared {len} bytes but read {} bytes",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn parse_property_stream(bytes: &[u8]) -> Result<Vec<StoredProperty>, String> {
    if bytes.len() < 4 {
        return Err("truncated stored-property header".to_owned());
    }
    if bytes[0] != 0x4c {
        return Err(format!("unsupported byte-order marker 0x{:02x}", bytes[0]));
    }
    if bytes[1] != PROPERTY_VERSION {
        return Err(format!(
            "stored-property version {} is not {PROPERTY_VERSION}",
            bytes[1]
        ));
    }
    let count = u16::from_le_bytes([bytes[2], bytes[3]]);
    let table_bytes = usize::from(count)
        .checked_mul(6)
        .and_then(|size| size.checked_add(4))
        .ok_or_else(|| "stored-property table length overflow".to_owned())?;
    if bytes.len() < table_bytes {
        return Err("truncated stored-property entry table".to_owned());
    }
    let mut data_bytes = 0_usize;
    let mut pids = HashSet::new();
    let mut entries = Vec::with_capacity(usize::from(count));
    for index in 0..usize::from(count) {
        let offset = 4 + index * 6;
        let pid = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let format = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]);
        let size = u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]);
        if !pids.insert(pid) {
            return Err(format!("duplicate property PID 0x{pid:04x}"));
        }
        if !PROPERTY_FORMATS.contains(&format) {
            return Err(format!("unknown stored-property format 0x{format:04x}"));
        }
        data_bytes = data_bytes
            .checked_add(usize::from(size))
            .ok_or_else(|| "stored-property payload length overflow".to_owned())?;
        entries.push((pid, format, usize::from(size)));
    }
    if table_bytes.checked_add(data_bytes) != Some(bytes.len()) {
        return Err(format!(
            "entry table declares {data_bytes} payload bytes but stream has {}",
            bytes.len().saturating_sub(table_bytes)
        ));
    }
    let mut payload_offset = table_bytes;
    let mut properties = Vec::with_capacity(entries.len());
    for (pid, format, size) in entries {
        let end = payload_offset + size;
        properties.push(StoredProperty {
            pid,
            format,
            data: bytes[payload_offset..end].to_vec(),
        });
        payload_offset = end;
    }
    Ok(properties)
}

fn validate_reference_properties(bytes: &[u8]) -> Result<(u16, u32), String> {
    if bytes.len() < 7 {
        return Err("truncated weak-reference table header".to_owned());
    }
    if bytes[0] != 0x4c {
        return Err(format!("unsupported byte-order marker 0x{:02x}", bytes[0]));
    }
    let paths = u16::from_le_bytes([bytes[1], bytes[2]]);
    let pid_words = u32::from_le_bytes(bytes[3..7].try_into().unwrap());
    let expected = usize::try_from(pid_words)
        .ok()
        .and_then(|words| words.checked_mul(2))
        .and_then(|size| size.checked_add(7))
        .ok_or_else(|| "weak-reference table length overflow".to_owned())?;
    if bytes.len() != expected {
        return Err(format!(
            "declared {pid_words} PID words require {expected} bytes; found {}",
            bytes.len()
        ));
    }
    let mut terminators = 0_u16;
    for word in bytes[7..].chunks_exact(2) {
        if word == [0, 0] {
            terminators = terminators.saturating_add(1);
        }
    }
    if terminators != paths {
        return Err(format!(
            "declared {paths} weak-reference paths but found {terminators} terminators"
        ));
    }
    Ok((paths, pid_words))
}

fn push_bounded(values: &mut Vec<String>, value: String) {
    if values.len() < MAX_REPORTED_PATHS {
        values.push(value);
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn stored_property_stream_rejects_duplicate_pids() {
        let bytes = [
            0x4c, 32, 2, 0, 1, 0, 0x82, 0, 1, 0, 1, 0, 0x82, 0, 1, 0, 7, 8,
        ];
        assert!(parse_property_stream(&bytes)
            .unwrap_err()
            .contains("duplicate property PID"));
    }

    #[test]
    fn weak_reference_table_checks_path_terminators() {
        let bytes = [0x4c, 1, 0, 2, 0, 0, 0, 1, 0, 2, 0];
        assert!(validate_reference_properties(&bytes)
            .unwrap_err()
            .contains("terminators"));
    }

    #[test]
    fn bounded_aaf_fixture_reports_missing_meta_dictionary_definitions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("minimal.aaf");
        write_aaf_fixture(&path, false);

        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed, "{audit:#?}");
        assert_eq!(audit.format, "aaf");
        assert_eq!(
            audit.properties["method"],
            "forge-aaf-metadictionary-object-model-edit-protocol-v2"
        );
        assert_eq!(audit.properties["stored_properties"]["streams"], 2);
        let failures: Vec<_> = audit
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .filter(|check| !check.passed)
            .map(|check| check.rule_id)
            .collect();
        assert_eq!(
            failures,
            [
                "FORGE-AAF-METADICTIONARY-DEFINITIONS",
                "FORGE-AAF-EXTENSION-PROPERTY-TYPES"
            ]
        );
    }

    #[test]
    fn malformed_stored_property_stream_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("malformed.aaf");
        write_aaf_fixture(&path, true);

        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|check| check.rule_id == "FORGE-AAF-STORED-PROPERTIES" && !check.passed));
    }

    #[test]
    fn mismatched_file_and_sector_profiles_fail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mismatched.aaf");
        write_aaf_fixture(&path, false);

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(8)).unwrap();
        file.write_all(&AAF_V3_HEADER_CLSID_LE).unwrap();
        drop(file);

        let audit = crate::container_qc::audit(&path).unwrap();
        assert!(!audit.passed);
        assert!(audit
            .layers
            .iter()
            .flat_map(|layer| &layer.checks)
            .any(|check| check.rule_id == "FORGE-AAF-FILE-PROFILE" && !check.passed));
    }

    fn write_aaf_fixture(path: &Path, malformed: bool) {
        {
            let mut compound = cfb::create(path).unwrap();
            compound
                .set_storage_clsid("/", uuid::Uuid::parse_str(AAF_ROOT_CLSID).unwrap())
                .unwrap();
            for storage in [
                "/MetaDictionary-1",
                "/MetaDictionary-1/ClassDefinitions-3{0}",
                "/MetaDictionary-1/TypeDefinitions-4{0}",
                "/Header-2",
                "/Header-2/Content-3b03",
                "/Header-2/Dictionary-3b04",
                "/Header-2/Identifi-ionList-3b06{0}",
            ] {
                compound.create_storage(storage).unwrap();
            }
            {
                let mut stream = compound.create_stream("/properties").unwrap();
                if malformed {
                    stream.write_all(&[0x4c, PROPERTY_VERSION, 1, 0]).unwrap();
                } else {
                    let name: Vec<u8> = "MetaDictionary-1"
                        .encode_utf16()
                        .chain([0])
                        .flat_map(u16::to_le_bytes)
                        .collect();
                    let mut properties = vec![0x4c, PROPERTY_VERSION, 1, 0, 1, 0, 0x22, 0];
                    properties.extend_from_slice(&(name.len() as u16).to_le_bytes());
                    properties.extend_from_slice(&name);
                    stream.write_all(&properties).unwrap();
                }
            }
            {
                let mut stream = compound
                    .create_stream("/MetaDictionary-1/properties")
                    .unwrap();
                stream.write_all(&[0x4c, PROPERTY_VERSION, 0, 0]).unwrap();
            }
            {
                let mut stream = compound.create_stream("/referenced properties").unwrap();
                stream.write_all(&[0x4c, 0, 0, 0, 0, 0, 0]).unwrap();
            }
            compound.flush().unwrap();
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.seek(SeekFrom::Start(8)).unwrap();
        file.write_all(&AAF_V4_HEADER_CLSID_LE).unwrap();
    }
}
