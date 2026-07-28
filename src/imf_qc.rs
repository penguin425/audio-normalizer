//! Bounded, local-only SMPTE ST 2067 Interoperable Master Format package QC.
//!
//! This module checks the package relationships that can be established from
//! an AssetMap, Packing List (PKL), and Composition Playlist (CPL). It does not
//! claim XSD, RegXML, MXF essence, or application-specific picture conformance.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

pub const IMF_QC_SCHEMA: &str = "https://penguin425.github.io/audio-normalizer/schema/imf-qc-v1";
const MAX_XML_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ELEMENTS: usize = 250_000;
const MAX_DEPTH: usize = 96;
const MAX_ASSETS: usize = 100_000;
const MAX_CHUNKS: usize = 200_000;
const MAX_HASH_BYTES: u64 = 8 * 1024 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Serialize)]
pub struct ImfFinding {
    pub rule_id: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ImfAudit {
    pub schema: &'static str,
    pub generator: &'static str,
    pub path: String,
    pub passed: bool,
    pub warning_count: usize,
    pub findings: Vec<ImfFinding>,
    pub properties: Value,
}

#[derive(Clone, Debug, Default)]
struct XmlNode {
    name: String,
    prefix: Option<String>,
    attributes: HashMap<String, String>,
    text: String,
    children: Vec<XmlNode>,
}

#[derive(Clone, Debug)]
struct Chunk {
    path: String,
    volume_index: u64,
    offset: u64,
    length: Option<u64>,
}

#[derive(Clone, Debug)]
struct AssetMapAsset {
    id: String,
    chunks: Vec<Chunk>,
    packing_list: bool,
}

#[derive(Clone, Debug)]
struct ResolvedChunk {
    path: PathBuf,
    offset: u64,
    length: u64,
}

#[derive(Clone, Debug)]
struct ResolvedAsset {
    chunks: Vec<ResolvedChunk>,
    size: u64,
}

#[derive(Clone, Debug)]
struct PklAsset {
    id: String,
    size: Option<u64>,
    hash: Option<String>,
    hash_algorithm: Option<String>,
    asset_type: Option<String>,
    original_filename: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct Rational {
    numerator: u64,
    denominator: u64,
}

#[derive(Clone, Debug)]
struct Resource {
    track_file_id: Option<String>,
    source_encoding: Option<String>,
    edit_rate: Option<Rational>,
    intrinsic_duration: Option<u64>,
    entry_point: u64,
    source_duration: Option<u64>,
    repeat_count: u64,
}

#[derive(Clone, Debug)]
struct Sequence {
    kind: String,
    track_id: Option<String>,
    resources: Vec<Resource>,
}

#[derive(Clone, Debug)]
struct Segment {
    sequences: Vec<Sequence>,
}

#[derive(Clone, Debug)]
struct Cpl {
    id: Option<String>,
    edit_rate: Option<Rational>,
    application_ids: Vec<String>,
    application_element_count: usize,
    descriptor_ids: HashSet<String>,
    descriptors: Vec<DescriptorEvidence>,
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, Default)]
struct DescriptorEvidence {
    id: String,
    sample_rate: Option<String>,
    quantization_bits: Option<String>,
    channel_count: Option<String>,
    mca_tags: Vec<String>,
    mca_channel_ids: Vec<u64>,
    mca_dictionary_ids: Vec<String>,
    mca_languages: Vec<String>,
    mca_link_ids: Vec<String>,
}

pub fn audit(input: &Path) -> Result<ImfAudit, String> {
    let (package_root, assetmap_path) = locate_assetmap(input)?;
    let canonical_root = package_root
        .canonicalize()
        .map_err(|error| format!("canonicalize {}: {error}", package_root.display()))?;
    let assetmap = read_xml(&assetmap_path)?;
    let mut findings = Vec::new();

    finding(
        &mut findings,
        "FORGE-IMF-ASSETMAP-ROOT",
        Severity::Error,
        assetmap.name == "AssetMap",
        "The package entry document is an AssetMap",
        Some(json!({"root_element": assetmap.name})),
    );
    let assetmap_namespace = namespace(&assetmap);
    finding(
        &mut findings,
        "FORGE-IMF-ASSETMAP-NAMESPACE",
        Severity::Error,
        is_imf_namespace(assetmap_namespace.as_deref(), "429-9"),
        "AssetMap uses a recognized SMPTE 429-9 namespace",
        Some(json!({"namespace": assetmap_namespace})),
    );

    let assets = parse_assetmap(&assetmap)?;
    finding(
        &mut findings,
        "FORGE-IMF-ASSETMAP-ASSETS",
        Severity::Error,
        !assets.is_empty(),
        "AssetMap contains at least one asset",
        Some(json!({"asset_count": assets.len()})),
    );
    let unique_ids = assets.iter().map(|asset| &asset.id).collect::<HashSet<_>>();
    finding(
        &mut findings,
        "FORGE-IMF-ASSETMAP-UNIQUE-ID",
        Severity::Error,
        unique_ids.len() == assets.len() && assets.iter().all(|asset| valid_v4_uuid_urn(&asset.id)),
        "AssetMap asset IDs are unique version-4 UUID URNs",
        Some(json!({"unique_id_count": unique_ids.len(), "asset_count": assets.len()})),
    );

    let mut resolved = HashMap::new();
    let mut path_errors = Vec::new();
    for asset in &assets {
        match resolve_asset(&canonical_root, asset) {
            Ok(value) => {
                resolved.insert(asset.id.clone(), value);
            }
            Err(error) => path_errors.push(format!("{}: {error}", asset.id)),
        }
    }
    finding(
        &mut findings,
        "FORGE-IMF-ASSETMAP-LOCAL-PATH",
        Severity::Error,
        path_errors.is_empty(),
        "Every AssetMap chunk is a bounded regular file inside the package root",
        Some(json!({"resolved_asset_count": resolved.len(), "errors": path_errors})),
    );

    let mut pkl_documents = Vec::new();
    let mut cpl_documents = Vec::new();
    let mut xml_errors = Vec::new();
    for asset in &assets {
        let Some(resolved_asset) = resolved.get(&asset.id) else {
            continue;
        };
        if resolved_asset.chunks.len() != 1 || resolved_asset.chunks[0].offset != 0 {
            continue;
        }
        let chunk = &resolved_asset.chunks[0];
        if chunk.length > MAX_XML_BYTES {
            continue;
        }
        let looks_xml = asset.packing_list || asset_looks_like_xml(resolved_asset)?;
        if !looks_xml {
            continue;
        }
        let document = read_asset_bytes(resolved_asset).and_then(|bytes| {
            parse_xml(&bytes).map_err(|error| format!("parse {}: {error}", chunk.path.display()))
        });
        match document {
            Ok(document) if document.name == "PackingList" => {
                pkl_documents.push((asset.id.clone(), chunk.path.clone(), document));
            }
            Ok(document) if document.name == "CompositionPlaylist" => {
                cpl_documents.push((asset.id.clone(), chunk.path.clone(), document));
            }
            Ok(_) => {}
            Err(error) => xml_errors.push(format!("{}: {error}", chunk.path.display())),
        }
    }
    finding(
        &mut findings,
        "FORGE-IMF-XML-PARSE",
        Severity::Error,
        xml_errors.is_empty(),
        "All package XML assets parse within byte, element, and depth limits",
        Some(json!({"errors": xml_errors})),
    );
    finding(
        &mut findings,
        "FORGE-IMF-PKL-PRESENT",
        Severity::Error,
        !pkl_documents.is_empty(),
        "The package contains at least one Packing List",
        Some(json!({"packing_list_count": pkl_documents.len()})),
    );
    finding(
        &mut findings,
        "FORGE-IMF-CPL-PRESENT",
        Severity::Error,
        !cpl_documents.is_empty(),
        "The package contains at least one Composition Playlist",
        Some(json!({"composition_playlist_count": cpl_documents.len()})),
    );
    let signature_count = usize::from(assetmap.descendants_named("Signature").next().is_some())
        + pkl_documents
            .iter()
            .filter(|(_, _, document)| document.descendants_named("Signature").next().is_some())
            .count()
        + cpl_documents
            .iter()
            .filter(|(_, _, document)| document.descendants_named("Signature").next().is_some())
            .count();
    finding(
        &mut findings,
        "FORGE-IMF-XML-SIGNATURE",
        Severity::Warning,
        signature_count == 0,
        "No XML Signature requiring an external cryptographic trust decision is present",
        Some(json!({
            "signed_document_count": signature_count,
            "note": if signature_count == 0 {
                "none"
            } else {
                "XML Signature validation is outside this bounded structural and content-hash audit"
            }
        })),
    );

    let assetmap_ids = resolved.keys().cloned().collect::<HashSet<_>>();
    let mut pkl_asset_ids = HashSet::new();
    let mut pkl_document_ids = HashSet::new();
    let mut pkl_identity_errors = Vec::new();
    let mut pkl_asset_count = 0usize;
    let mut hash_verified = 0usize;
    let mut sha1_count = 0usize;
    let mut pkl_namespaces = Vec::new();
    for (asset_id, path, document) in &pkl_documents {
        pkl_namespaces.push(namespace(document));
        match document.child_text("Id") {
            Some(id) if id == *asset_id && valid_v4_uuid_urn(&id) => {
                if !pkl_document_ids.insert(id.clone()) {
                    pkl_identity_errors.push(json!({
                        "path": path, "id": id, "error": "duplicate Packing List document ID"
                    }));
                }
            }
            internal_id => pkl_identity_errors.push(json!({
                "path": path,
                "assetmap_id": asset_id,
                "packing_list_id": internal_id,
                "error": "Packing List Id does not equal its AssetMap asset ID"
            })),
        }
        let pkl_assets = parse_pkl(document)?;
        pkl_asset_count += pkl_assets.len();
        validate_pkl(
            path,
            &pkl_assets,
            &resolved,
            &mut pkl_asset_ids,
            &mut hash_verified,
            &mut sha1_count,
            &mut findings,
        );
    }
    finding(
        &mut findings,
        "FORGE-IMF-PKL-NAMESPACE",
        Severity::Error,
        !pkl_namespaces.is_empty()
            && pkl_namespaces
                .iter()
                .all(|value| is_pkl_namespace(value.as_deref())),
        "Packing Lists use recognized IMF PKL namespaces",
        Some(json!({"namespaces": pkl_namespaces})),
    );
    let marked_pkl_ids = assets
        .iter()
        .filter(|asset| asset.packing_list)
        .map(|asset| &asset.id)
        .collect::<HashSet<_>>();
    let discovered_pkl_ids = pkl_documents
        .iter()
        .map(|(id, _, _)| id)
        .collect::<HashSet<_>>();
    finding(
        &mut findings,
        "FORGE-IMF-PKL-IDENTITY",
        Severity::Error,
        pkl_identity_errors.is_empty() && marked_pkl_ids == discovered_pkl_ids,
        "Packing List document IDs match their AssetMap IDs and PackingList markers",
        Some(json!({
            "marked_ids": marked_pkl_ids,
            "discovered_ids": discovered_pkl_ids,
            "errors": pkl_identity_errors
        })),
    );
    finding(
        &mut findings,
        "FORGE-IMF-PKL-ASSETMAP-REFERENCES",
        Severity::Error,
        pkl_asset_ids.iter().all(|id| assetmap_ids.contains(id)),
        "Every Packing List asset is declared in the AssetMap",
        Some(json!({
            "missing_ids": pkl_asset_ids.difference(&assetmap_ids).collect::<Vec<_>>()
        })),
    );
    if sha1_count > 0 {
        finding(
            &mut findings,
            "FORGE-IMF-SHA1-SECURITY",
            Severity::Warning,
            false,
            "SHA-1 verification detects accidental corruption but is not malicious-content protection; use authenticated provenance for adversarial integrity",
            Some(json!({"sha1_asset_count": sha1_count})),
        );
    }

    let mut cpl_ids = HashSet::new();
    let mut cpl_identity_errors = Vec::new();
    let mut application_ids = HashSet::new();
    let mut virtual_track_count = 0usize;
    let mut mca_label_count = 0usize;
    let mut cpl_namespaces = Vec::new();
    for (asset_id, path, document) in &cpl_documents {
        cpl_namespaces.push(namespace(document));
        let cpl = parse_cpl(document)?;
        if let Some(id) = &cpl.id {
            cpl_ids.insert(id.clone());
            if id != asset_id {
                cpl_identity_errors.push(json!({
                    "path": path,
                    "assetmap_id": asset_id,
                    "composition_playlist_id": id
                }));
            }
        }
        application_ids.extend(cpl.application_ids.iter().cloned());
        virtual_track_count += cpl
            .segments
            .first()
            .map_or(0, |segment| segment.sequences.len());
        mca_label_count += cpl
            .descriptors
            .iter()
            .map(|descriptor| descriptor.mca_tags.len())
            .sum::<usize>();
        validate_cpl(
            path,
            &cpl,
            &assetmap_ids,
            &pkl_asset_ids,
            &resolved,
            &mut findings,
        );
    }
    finding(
        &mut findings,
        "FORGE-IMF-CPL-NAMESPACE",
        Severity::Error,
        !cpl_namespaces.is_empty()
            && cpl_namespaces
                .iter()
                .all(|value| is_imf_namespace(value.as_deref(), "2067-3")),
        "Composition Playlists use recognized SMPTE ST 2067-3 namespaces",
        Some(json!({"namespaces": cpl_namespaces})),
    );
    finding(
        &mut findings,
        "FORGE-IMF-CPL-UNIQUE-ID",
        Severity::Error,
        cpl_ids.len() == cpl_documents.len()
            && cpl_ids.iter().all(|id| valid_v4_uuid_urn(id))
            && cpl_identity_errors.is_empty()
            && cpl_documents
                .iter()
                .all(|(asset_id, _, _)| pkl_asset_ids.contains(asset_id)),
        "Composition Playlist IDs are unique version-4 UUID URNs matching AssetMap and PKL identities",
        Some(json!({
            "unique_id_count": cpl_ids.len(),
            "cpl_count": cpl_documents.len(),
            "identity_errors": cpl_identity_errors,
            "not_in_packing_list": cpl_documents.iter().filter(|(id, _, _)| !pkl_asset_ids.contains(id)).map(|(id, _, _)| id).collect::<Vec<_>>()
        })),
    );

    let warning_count = findings
        .iter()
        .filter(|finding| finding.severity == Severity::Warning && !finding.passed)
        .count();
    let passed = findings
        .iter()
        .all(|finding| finding.severity == Severity::Warning || finding.passed);
    Ok(ImfAudit {
        schema: IMF_QC_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        path: assetmap_path.to_string_lossy().into_owned(),
        passed,
        warning_count,
        findings,
        properties: json!({
            "package_root": canonical_root,
            "assetmap_namespace": assetmap_namespace,
            "asset_count": assets.len(),
            "resolved_asset_count": resolved.len(),
            "packing_list_count": pkl_documents.len(),
            "packing_list_asset_count": pkl_asset_count,
            "hash_verified_asset_count": hash_verified,
            "sha1_asset_count": sha1_count,
            "composition_playlist_count": cpl_documents.len(),
            "composition_ids": cpl_ids,
            "application_identifications": application_ids,
            "virtual_track_count": virtual_track_count,
            "mca_label_count": mca_label_count,
            "limits": {
                "xml_bytes": MAX_XML_BYTES,
                "elements": MAX_ELEMENTS,
                "depth": MAX_DEPTH,
                "assets": MAX_ASSETS,
                "chunks": MAX_CHUNKS,
                "hash_bytes": MAX_HASH_BYTES
            }
        }),
    })
}

fn locate_assetmap(input: &Path) -> Result<(PathBuf, PathBuf), String> {
    if input.is_dir() {
        for name in ["ASSETMAP", "ASSETMAP.xml"] {
            let candidate = input.join(name);
            if candidate.is_file() {
                return Ok((input.to_path_buf(), candidate));
            }
        }
        Err(format!(
            "{} does not contain ASSETMAP or ASSETMAP.xml",
            input.display()
        ))
    } else if input.is_file() {
        let parent = input
            .parent()
            .ok_or_else(|| format!("{} has no package parent", input.display()))?;
        Ok((parent.to_path_buf(), input.to_path_buf()))
    } else {
        Err(format!("{} is not a file or directory", input.display()))
    }
}

fn read_xml(path: &Path) -> Result<XmlNode, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.len() > MAX_XML_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_XML_BYTES} byte XML safety limit",
            path.display()
        ));
    }
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_xml(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn parse_xml(bytes: &[u8]) -> Result<XmlNode, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut stack = Vec::<XmlNode>::new();
    let mut root = None;
    let mut element_count = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                element_count += 1;
                if element_count > MAX_ELEMENTS {
                    return Err(format!("XML exceeds the {MAX_ELEMENTS} element limit"));
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("XML exceeds the {MAX_DEPTH} level depth limit"));
                }
                stack.push(new_node(&reader, &element)?);
            }
            Ok(Event::Empty(element)) => {
                element_count += 1;
                if element_count > MAX_ELEMENTS {
                    return Err(format!("XML exceeds the {MAX_ELEMENTS} element limit"));
                }
                attach_node(new_node(&reader, &element)?, &mut stack, &mut root)?;
            }
            Ok(Event::Text(text)) => {
                if let Some(node) = stack.last_mut() {
                    let decoded = text
                        .decode()
                        .map_err(|error| format!("decode XML text: {error}"))?;
                    node.text.push_str(decoded.trim());
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&String::from_utf8_lossy(text.as_ref()));
                }
            }
            Ok(Event::End(_)) => {
                let node = stack
                    .pop()
                    .ok_or_else(|| "XML has an unmatched closing element".to_string())?;
                attach_node(node, &mut stack, &mut root)?;
            }
            Ok(Event::DocType(_)) => return Err("IMF XML must not contain a DTD".into()),
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(format!(
                    "XML error at byte {}: {error}",
                    reader.error_position()
                ))
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("XML ended with unclosed elements".into());
    }
    root.ok_or_else(|| "XML contains no document element".into())
}

fn new_node(reader: &Reader<&[u8]>, element: &BytesStart<'_>) -> Result<XmlNode, String> {
    let mut attributes = HashMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).into_owned();
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|error| format!("decode XML attribute: {error}"))?
            .into_owned();
        attributes.insert(key, value);
    }
    Ok(XmlNode {
        name: local_name(element.name().as_ref()),
        prefix: prefix(element.name().as_ref()),
        attributes,
        text: String::new(),
        children: Vec::new(),
    })
}

fn attach_node(
    node: XmlNode,
    stack: &mut [XmlNode],
    root: &mut Option<XmlNode>,
) -> Result<(), String> {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.replace(node).is_some() {
        return Err("XML contains multiple document elements".into());
    }
    Ok(())
}

fn parse_assetmap(root: &XmlNode) -> Result<Vec<AssetMapAsset>, String> {
    let list = root
        .child("AssetList")
        .ok_or_else(|| "AssetMap is missing AssetList".to_string())?;
    if list.children_named("Asset").count() > MAX_ASSETS {
        return Err(format!("AssetMap exceeds the {MAX_ASSETS} asset limit"));
    }
    let mut chunk_count = 0usize;
    list.children_named("Asset")
        .map(|asset| {
            let id = asset.required_text("Id")?;
            let chunk_list = asset
                .child("ChunkList")
                .ok_or_else(|| format!("Asset {id} is missing ChunkList"))?;
            let chunks = chunk_list
                .children_named("Chunk")
                .map(|chunk| {
                    chunk_count += 1;
                    if chunk_count > MAX_CHUNKS {
                        return Err(format!("AssetMap exceeds the {MAX_CHUNKS} chunk limit"));
                    }
                    Ok(Chunk {
                        path: chunk.required_text("Path")?,
                        volume_index: chunk.optional_u64("VolumeIndex")?.unwrap_or(1),
                        offset: chunk.optional_u64("Offset")?.unwrap_or(0),
                        length: chunk.optional_u64("Length")?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            if chunks.is_empty() {
                return Err(format!("Asset {id} has no chunks"));
            }
            Ok(AssetMapAsset {
                id,
                chunks,
                packing_list: asset
                    .child("PackingList")
                    .is_some_and(|node| node.text.eq_ignore_ascii_case("true")),
            })
        })
        .collect()
}

fn resolve_asset(root: &Path, asset: &AssetMapAsset) -> Result<ResolvedAsset, String> {
    let mut chunks = Vec::new();
    let mut total = 0u64;
    for chunk in &asset.chunks {
        if chunk.volume_index != 1 {
            return Err(format!(
                "unsupported VolumeIndex {} (only local volume 1 is auditable)",
                chunk.volume_index
            ));
        }
        let relative = Path::new(&chunk.path);
        if chunk.path.contains('\\')
            || chunk.path.contains('\0')
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!("unsafe relative chunk path {}", chunk.path));
        }
        let joined = root.join(relative);
        let symlink_metadata = fs::symlink_metadata(&joined)
            .map_err(|error| format!("stat {}: {error}", joined.display()))?;
        if symlink_metadata.file_type().is_symlink() {
            return Err(format!("chunk {} is a symbolic link", joined.display()));
        }
        if !symlink_metadata.is_file() {
            return Err(format!("chunk {} is not a regular file", joined.display()));
        }
        let canonical = joined
            .canonicalize()
            .map_err(|error| format!("canonicalize {}: {error}", joined.display()))?;
        if !canonical.starts_with(root) {
            return Err(format!(
                "chunk {} escapes the package root",
                joined.display()
            ));
        }
        let file_size = symlink_metadata.len();
        if chunk.offset > file_size {
            return Err(format!(
                "chunk offset {} exceeds {} byte file {}",
                chunk.offset,
                file_size,
                joined.display()
            ));
        }
        let length = chunk.length.unwrap_or(file_size - chunk.offset);
        let end = chunk
            .offset
            .checked_add(length)
            .ok_or_else(|| "chunk extent overflows u64".to_string())?;
        if end > file_size {
            return Err(format!(
                "chunk extent {end} exceeds {} byte file {}",
                file_size,
                joined.display()
            ));
        }
        total = total
            .checked_add(length)
            .ok_or_else(|| "assembled asset size overflows u64".to_string())?;
        if total > MAX_HASH_BYTES {
            return Err(format!(
                "asset exceeds the {MAX_HASH_BYTES} byte audit limit"
            ));
        }
        chunks.push(ResolvedChunk {
            path: canonical,
            offset: chunk.offset,
            length,
        });
    }
    Ok(ResolvedAsset {
        chunks,
        size: total,
    })
}

fn asset_looks_like_xml(asset: &ResolvedAsset) -> Result<bool, String> {
    let Some(chunk) = asset.chunks.first() else {
        return Ok(false);
    };
    let mut file = File::open(&chunk.path)
        .map_err(|error| format!("open {}: {error}", chunk.path.display()))?;
    file.seek(SeekFrom::Start(chunk.offset))
        .map_err(|error| format!("seek {}: {error}", chunk.path.display()))?;
    let mut prefix = [0u8; 256];
    let amount = usize::try_from(chunk.length.min(prefix.len() as u64))
        .map_err(|_| "XML sniff size does not fit usize".to_string())?;
    file.read_exact(&mut prefix[..amount])
        .map_err(|error| format!("read {}: {error}", chunk.path.display()))?;
    let prefix = &prefix[..amount];
    let prefix = prefix.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(prefix);
    Ok(prefix
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'<'))
}

fn read_asset_bytes(asset: &ResolvedAsset) -> Result<Vec<u8>, String> {
    if asset.size > MAX_XML_BYTES {
        return Err(format!(
            "assembled XML asset exceeds the {MAX_XML_BYTES} byte safety limit"
        ));
    }
    let capacity =
        usize::try_from(asset.size).map_err(|_| "XML asset size does not fit usize".to_string())?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0u8; 64 * 1024];
    for chunk in &asset.chunks {
        let mut file = File::open(&chunk.path)
            .map_err(|error| format!("open {}: {error}", chunk.path.display()))?;
        file.seek(SeekFrom::Start(chunk.offset))
            .map_err(|error| format!("seek {}: {error}", chunk.path.display()))?;
        let mut remaining = chunk.length;
        while remaining > 0 {
            let amount = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| "XML read size does not fit usize".to_string())?;
            file.read_exact(&mut buffer[..amount])
                .map_err(|error| format!("read {}: {error}", chunk.path.display()))?;
            bytes.extend_from_slice(&buffer[..amount]);
            remaining -= amount as u64;
        }
    }
    Ok(bytes)
}

fn parse_pkl(root: &XmlNode) -> Result<Vec<PklAsset>, String> {
    let list = root
        .child("AssetList")
        .ok_or_else(|| "PackingList is missing AssetList".to_string())?;
    if list.children_named("Asset").count() > MAX_ASSETS {
        return Err(format!("PackingList exceeds the {MAX_ASSETS} asset limit"));
    }
    list.children_named("Asset")
        .map(|asset| {
            Ok(PklAsset {
                id: asset.required_text("Id")?,
                size: asset.optional_u64("Size")?,
                hash: asset.child_text("Hash"),
                hash_algorithm: asset
                    .child("HashAlgorithm")
                    .and_then(|algorithm| algorithm.attribute_local("algorithm"))
                    .map(str::to_owned)
                    .or_else(|| {
                        asset
                            .child("Hash")
                            .and_then(|hash| hash.attribute_local("algorithm"))
                            .map(str::to_owned)
                    })
                    .or_else(|| asset.child_text("HashAlgorithm")),
                asset_type: asset.child_text("Type"),
                original_filename: asset.child_text("OriginalFileName"),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn validate_pkl(
    path: &Path,
    assets: &[PklAsset],
    resolved: &HashMap<String, ResolvedAsset>,
    pkl_ids: &mut HashSet<String>,
    verified: &mut usize,
    sha1_count: &mut usize,
    findings: &mut Vec<ImfFinding>,
) {
    let mut valid_ids = true;
    let mut local_ids = HashSet::new();
    for asset in assets {
        valid_ids &= valid_v4_uuid_urn(&asset.id) && local_ids.insert(asset.id.clone());
    }
    pkl_ids.extend(local_ids);
    finding(
        findings,
        "FORGE-IMF-PKL-UNIQUE-ID",
        Severity::Error,
        valid_ids,
        "Packing List asset IDs are unique version-4 UUID URNs within the document",
        Some(json!({"path": path, "asset_count": assets.len()})),
    );
    let mut size_errors = Vec::new();
    let mut hash_errors = Vec::new();
    let mut algorithms = HashMap::<String, usize>::new();
    for asset in assets {
        let Some(source) = resolved.get(&asset.id) else {
            size_errors
                .push(json!({"id": asset.id, "error": "asset is unavailable from AssetMap"}));
            hash_errors
                .push(json!({"id": asset.id, "error": "asset is unavailable from AssetMap"}));
            continue;
        };
        match asset.size {
            Some(size) if size == source.size => {}
            Some(size) => size_errors.push(json!({
                "id": asset.id, "declared": size, "actual": source.size
            })),
            None => size_errors.push(json!({"id": asset.id, "error": "missing Size"})),
        }
        let Some(expected_text) = &asset.hash else {
            hash_errors.push(json!({"id": asset.id, "error": "missing Hash"}));
            continue;
        };
        let algorithm = normalize_hash_algorithm(asset.hash_algorithm.as_deref());
        *algorithms.entry(algorithm.to_owned()).or_default() += 1;
        if algorithm == "sha1" {
            *sha1_count += 1;
        }
        let encoded_hash = expected_text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect::<Vec<_>>();
        let expected = match BASE64.decode(&encoded_hash) {
            Ok(value) => value,
            Err(error) => {
                hash_errors
                    .push(json!({"id": asset.id, "error": format!("invalid Base64: {error}")}));
                continue;
            }
        };
        match hash_asset(source, algorithm) {
            Ok(actual) if actual == expected => *verified += 1,
            Ok(actual) => hash_errors.push(json!({
                "id": asset.id,
                "algorithm": algorithm,
                "expected_base64": expected_text,
                "actual_base64": BASE64.encode(actual)
            })),
            Err(error) => hash_errors.push(json!({
                "id": asset.id, "algorithm": algorithm, "error": error
            })),
        }
    }
    finding(
        findings,
        "FORGE-IMF-PKL-SIZE",
        Severity::Error,
        size_errors.is_empty(),
        "Packing List sizes equal assembled AssetMap asset sizes",
        Some(json!({"path": path, "errors": size_errors})),
    );
    finding(
        findings,
        "FORGE-IMF-PKL-HASH",
        Severity::Error,
        hash_errors.is_empty(),
        "Packing List hashes match the assembled local asset bytes",
        Some(json!({"path": path, "algorithms": algorithms, "errors": hash_errors})),
    );
    let metadata_complete = assets
        .iter()
        .all(|asset| asset.asset_type.as_deref().is_some_and(|v| !v.is_empty()));
    finding(
        findings,
        "FORGE-IMF-PKL-METADATA",
        Severity::Warning,
        metadata_complete,
        "Packing List assets declare a non-empty Type",
        Some(json!({
            "path": path,
            "missing_type_ids": assets.iter().filter(|a| a.asset_type.as_deref().is_none_or(str::is_empty)).map(|a| &a.id).collect::<Vec<_>>(),
            "original_filenames": assets.iter().filter_map(|a| a.original_filename.as_ref()).collect::<Vec<_>>()
        })),
    );
}

fn normalize_hash_algorithm(value: Option<&str>) -> &'static str {
    match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        ""
        | "http://www.w3.org/2000/09/xmldsig#sha1"
        | "http://www.w3.org/2001/04/xmlenc#sha1"
        | "sha-1"
        | "sha1" => "sha1",
        "http://www.w3.org/2001/04/xmlenc#sha256"
        | "http://www.w3.org/2001/04/xmldsig-more#sha256"
        | "sha-256"
        | "sha256" => "sha256",
        _ => "unsupported",
    }
}

fn hash_asset(asset: &ResolvedAsset, algorithm: &str) -> Result<Vec<u8>, String> {
    match algorithm {
        "sha1" => hash_chunks::<Sha1>(&asset.chunks),
        "sha256" => hash_chunks::<Sha256>(&asset.chunks),
        _ => Err("unsupported hash algorithm".into()),
    }
}

fn hash_chunks<D: Digest + Default>(chunks: &[ResolvedChunk]) -> Result<Vec<u8>, String> {
    let mut digest = D::default();
    let mut buffer = [0u8; 64 * 1024];
    for chunk in chunks {
        let mut file = File::open(&chunk.path)
            .map_err(|error| format!("open {}: {error}", chunk.path.display()))?;
        file.seek(SeekFrom::Start(chunk.offset))
            .map_err(|error| format!("seek {}: {error}", chunk.path.display()))?;
        let mut remaining = chunk.length;
        while remaining > 0 {
            let amount = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| "hash read size does not fit usize".to_string())?;
            file.read_exact(&mut buffer[..amount])
                .map_err(|error| format!("read {}: {error}", chunk.path.display()))?;
            digest.update(&buffer[..amount]);
            remaining -= amount as u64;
        }
    }
    Ok(digest.finalize().to_vec())
}

fn parse_cpl(root: &XmlNode) -> Result<Cpl, String> {
    let edit_rate = root
        .child_text("EditRate")
        .map(|value| parse_rational(&value))
        .transpose()?;
    let application_nodes = root
        .descendants_named("ApplicationIdentification")
        .collect::<Vec<_>>();
    let application_ids = application_nodes
        .iter()
        .flat_map(|node| {
            if node.children.is_empty() {
                node.text
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            } else {
                node.children
                    .iter()
                    .flat_map(|child| child.text.split_whitespace().map(str::to_owned))
                    .collect()
            }
        })
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let mut descriptor_ids = HashSet::new();
    let mut descriptors = Vec::new();
    if let Some(list) = root.child("EssenceDescriptorList") {
        for descriptor in &list.children {
            let Some(id) = descriptor.child_text("Id") else {
                continue;
            };
            descriptor_ids.insert(id.clone());
            descriptors.push(parse_descriptor(descriptor, id));
        }
    }
    let mut segments = Vec::new();
    if let Some(segment_list) = root.child("SegmentList") {
        for segment in segment_list.children_named("Segment") {
            let mut sequences = Vec::new();
            if let Some(sequence_list) = segment.child("SequenceList") {
                for sequence in &sequence_list.children {
                    if !sequence.name.ends_with("Sequence") || sequence.name == "Sequence" {
                        continue;
                    }
                    let resources = sequence
                        .child("ResourceList")
                        .map(|list| {
                            list.children
                                .iter()
                                .filter(|node| node.name.ends_with("Resource"))
                                .map(|node| parse_resource(node, edit_rate))
                                .collect::<Result<Vec<_>, String>>()
                        })
                        .transpose()?
                        .unwrap_or_default();
                    sequences.push(Sequence {
                        kind: sequence.name.clone(),
                        track_id: sequence.child_text("TrackId"),
                        resources,
                    });
                }
            }
            segments.push(Segment { sequences });
        }
    }
    Ok(Cpl {
        id: root.child_text("Id"),
        edit_rate,
        application_ids,
        application_element_count: application_nodes.len(),
        descriptor_ids,
        descriptors,
        segments,
    })
}

fn parse_descriptor(node: &XmlNode, id: String) -> DescriptorEvidence {
    DescriptorEvidence {
        id,
        sample_rate: node.first_descendant_text(&["AudioSamplingRate", "SampleRate"]),
        quantization_bits: node.first_descendant_text(&["QuantizationBits"]),
        channel_count: node.first_descendant_text(&["ChannelCount"]),
        mca_tags: node.descendant_texts("MCATagSymbol"),
        mca_channel_ids: node
            .descendant_texts("MCAChannelID")
            .into_iter()
            .filter_map(|value| value.parse().ok())
            .collect(),
        mca_dictionary_ids: node.descendant_texts("MCALabelDictionaryID"),
        mca_languages: node.descendant_texts("RFC5646SpokenLanguage"),
        mca_link_ids: node.descendant_texts("SoundfieldGroupLinkID"),
    }
}

fn parse_resource(node: &XmlNode, inherited_rate: Option<Rational>) -> Result<Resource, String> {
    Ok(Resource {
        track_file_id: node.child_text("TrackFileId"),
        source_encoding: node.child_text("SourceEncoding"),
        edit_rate: node
            .child_text("EditRate")
            .map(|value| parse_rational(&value))
            .transpose()?
            .or(inherited_rate),
        intrinsic_duration: node.optional_u64("IntrinsicDuration")?,
        entry_point: node.optional_u64("EntryPoint")?.unwrap_or(0),
        source_duration: node.optional_u64("SourceDuration")?,
        repeat_count: node.optional_u64("RepeatCount")?.unwrap_or(1),
    })
}

fn validate_cpl(
    path: &Path,
    cpl: &Cpl,
    assetmap_ids: &HashSet<String>,
    pkl_ids: &HashSet<String>,
    resolved: &HashMap<String, ResolvedAsset>,
    findings: &mut Vec<ImfFinding>,
) {
    finding(
        findings,
        "FORGE-IMF-CPL-EDIT-RATE",
        Severity::Error,
        cpl.edit_rate
            .is_some_and(|rate| rate.numerator > 0 && rate.denominator > 0),
        "Composition Playlist has a positive rational EditRate",
        Some(
            json!({"path": path, "edit_rate": cpl.edit_rate.map(|r| [r.numerator, r.denominator])}),
        ),
    );
    let application_valid = cpl.application_element_count == 1
        && !cpl.application_ids.is_empty()
        && cpl.application_ids.iter().all(|value| {
            value.starts_with("http://")
                || value.starts_with("https://")
                || value.starts_with("urn:")
        })
        && cpl.application_ids.iter().collect::<HashSet<_>>().len() == cpl.application_ids.len();
    finding(
        findings,
        "FORGE-IMF-CPL-APPLICATION-ID",
        Severity::Error,
        application_valid,
        "ExtensionProperties contains exactly one ApplicationIdentification with non-empty absolute URI values",
        Some(json!({
            "path": path,
            "application_identification_element_count": cpl.application_element_count,
            "application_identifications": cpl.application_ids
        })),
    );

    let mut reference_errors = Vec::new();
    let mut timing_errors = Vec::new();
    let mut segment_durations = Vec::<Vec<(String, u128, u128)>>::new();
    let mut track_shapes = HashMap::<String, String>::new();
    let mut segment_track_sets = Vec::new();
    let mut application_errors = Vec::new();
    for (segment_index, segment) in cpl.segments.iter().enumerate() {
        let mut durations = Vec::new();
        let mut segment_tracks = HashSet::new();
        for sequence in &segment.sequences {
            let track = sequence
                .track_id
                .clone()
                .unwrap_or_else(|| format!("missing-track-{segment_index}"));
            if sequence
                .track_id
                .as_deref()
                .is_none_or(|id| !valid_v4_uuid_urn(id))
            {
                timing_errors.push(json!({
                    "segment": segment_index, "track_id": sequence.track_id,
                    "error": "virtual track is missing a UUID URN TrackId"
                }));
            }
            if !segment_tracks.insert(track.clone()) {
                timing_errors.push(json!({
                    "segment": segment_index, "track_id": track,
                    "error": "duplicate virtual track in segment"
                }));
            }
            if sequence.resources.is_empty() && sequence.kind != "MarkerSequence" {
                timing_errors.push(json!({
                    "segment": segment_index, "track_id": track,
                    "error": "non-marker sequence has no resources"
                }));
            }
            if let Some(previous_kind) = track_shapes.insert(track.clone(), sequence.kind.clone()) {
                if previous_kind != sequence.kind {
                    timing_errors.push(json!({
                        "segment": segment_index, "track_id": track,
                        "error": "virtual track changes sequence type"
                    }));
                }
            }
            let mut total_num = 0u128;
            let mut total_den = 1u128;
            let mut source_encodings = HashSet::new();
            for resource in &sequence.resources {
                if let Some(id) = &resource.track_file_id {
                    if !assetmap_ids.contains(id) || !pkl_ids.contains(id) {
                        reference_errors.push(json!({
                            "track_file_id": id,
                            "in_assetmap": assetmap_ids.contains(id),
                            "in_packing_list": pkl_ids.contains(id)
                        }));
                    } else if let Some(asset) = resolved.get(id) {
                        let mxf_ok = asset.chunks.len() == 1
                            && crate::container_qc::audit(&asset.chunks[0].path)
                                .is_ok_and(|audit| audit.passed && audit.format == "mxf");
                        if !mxf_ok {
                            reference_errors.push(json!({
                                "track_file_id": id,
                                "error": "referenced Track File is not a passing OP1a MXF audit"
                            }));
                        }
                    }
                }
                if let Some(id) = &resource.source_encoding {
                    source_encodings.insert(id.clone());
                    if !cpl.descriptor_ids.contains(id) {
                        reference_errors.push(json!({
                            "source_encoding": id,
                            "error": "SourceEncoding does not identify an EssenceDescriptor"
                        }));
                    }
                }
                match resource_duration(resource, cpl.edit_rate) {
                    Ok((numerator, denominator)) => {
                        let lcm = lcm_u128(total_den, denominator).unwrap_or(0);
                        if lcm == 0 {
                            timing_errors.push(
                                json!({"track_id": track, "error": "duration arithmetic overflow"}),
                            );
                        } else {
                            total_num = total_num
                                .checked_mul(lcm / total_den)
                                .and_then(|value| {
                                    numerator
                                        .checked_mul(lcm / denominator)
                                        .and_then(|rhs| value.checked_add(rhs))
                                })
                                .unwrap_or(u128::MAX);
                            total_den = lcm;
                        }
                    }
                    Err(error) => timing_errors.push(json!({"track_id": track, "error": error})),
                }
            }
            if sequence.kind.contains("Audio") && source_encodings.len() > 1 {
                application_errors.push(json!({
                    "segment": segment_index,
                    "track_id": track,
                    "source_encodings": source_encodings,
                    "error": "audio virtual track changes EssenceDescriptor within a segment"
                }));
            }
            if sequence.kind != "MarkerSequence" {
                durations.push((track, total_num, total_den));
            }
        }
        segment_track_sets.push(segment_tracks);
        if let Some((_, first_num, first_den)) = durations.first() {
            for (track, num, den) in &durations {
                if num.checked_mul(*first_den) != first_num.checked_mul(*den) {
                    timing_errors.push(json!({
                        "segment": segment_index,
                        "track_id": track,
                        "error": "virtual-track duration differs within segment",
                        "duration": [num, den],
                        "expected": [first_num, first_den]
                    }));
                }
            }
        }
        segment_durations.push(durations);
    }
    if let Some(first) = segment_track_sets.first() {
        for (index, tracks) in segment_track_sets.iter().enumerate().skip(1) {
            if tracks != first {
                timing_errors.push(json!({
                    "segment": index,
                    "error": "virtual-track set differs between segments",
                    "expected": first,
                    "actual": tracks
                }));
            }
        }
    }
    const APP_2E_ID: &str = "http://www.smpte-ra.org/schemas/2067-21/2016";
    if cpl.application_ids.iter().any(|id| id == APP_2E_ID) {
        for (segment_index, segment) in cpl.segments.iter().enumerate() {
            let main_image_count = segment
                .sequences
                .iter()
                .filter(|sequence| sequence.kind == "MainImageSequence")
                .count();
            if main_image_count != 1 {
                application_errors.push(json!({
                    "application": "SMPTE ST 2067-21 Application #2E",
                    "segment": segment_index,
                    "main_image_sequence_count": main_image_count,
                    "error": "Application #2E requires one Main Image Virtual Track"
                }));
            }
        }
    }
    finding(
        findings,
        "FORGE-IMF-CPL-REFERENCES",
        Severity::Error,
        reference_errors.is_empty(),
        "CPL TrackFileId and SourceEncoding references resolve to PKL/AssetMap assets and descriptors",
        Some(json!({"path": path, "errors": reference_errors})),
    );
    finding(
        findings,
        "FORGE-IMF-CPL-VIRTUAL-TRACK-TIMING",
        Severity::Error,
        !cpl.segments.is_empty() && timing_errors.is_empty(),
        "Resources have valid bounds/rates and non-marker virtual tracks align in every segment",
        Some(
            json!({"path": path, "segment_durations": segment_durations, "errors": timing_errors}),
        ),
    );
    let recognized_application_ids = cpl
        .application_ids
        .iter()
        .filter(|id| id.as_str() == APP_2E_ID)
        .collect::<Vec<_>>();
    finding(
        findings,
        "FORGE-IMF-CPL-APPLICATION-CONSTRAINTS",
        Severity::Error,
        application_errors.is_empty(),
        "Auditable common Application constraints keep audio tracks homogeneous and enforce the Application #2E Main Image track requirement when selected",
        Some(json!({
            "path": path,
            "recognized_smpte_application_ids": recognized_application_ids,
            "scope": "common structural/audio constraints; not full XSD, RegXML, picture essence, or profile conformance",
            "errors": application_errors
        })),
    );

    let mut mca_errors = Vec::new();
    for descriptor in &cpl.descriptors {
        let tags = descriptor.mca_tags.iter().collect::<HashSet<_>>();
        let channel_ids = descriptor.mca_channel_ids.iter().collect::<HashSet<_>>();
        if tags.len() != descriptor.mca_tags.len()
            || channel_ids.len() != descriptor.mca_channel_ids.len()
            || descriptor.mca_channel_ids.contains(&0)
        {
            mca_errors.push(json!({"descriptor_id": descriptor.id, "error": "duplicate/zero MCA tag or channel ID"}));
        }
        for value in &descriptor.mca_dictionary_ids {
            if !(valid_uuid_urn(value) || valid_ul_urn(value)) {
                mca_errors.push(json!({"descriptor_id": descriptor.id, "dictionary_id": value, "error": "invalid MCA label dictionary identifier"}));
            }
        }
        for language in &descriptor.mca_languages {
            if !valid_language_tag(language) {
                mca_errors.push(json!({"descriptor_id": descriptor.id, "language": language, "error": "invalid RFC 5646-style language tag"}));
            }
        }
        if !descriptor.mca_link_ids.is_empty()
            && descriptor.mca_link_ids.iter().any(|id| !valid_uuid_urn(id))
        {
            mca_errors.push(
                json!({"descriptor_id": descriptor.id, "error": "invalid SoundfieldGroupLinkID"}),
            );
        }
    }
    finding(
        findings,
        "FORGE-IMF-CPL-MCA-LABELS",
        Severity::Error,
        mca_errors.is_empty(),
        "Auditable MCA tag, channel, dictionary, language, and soundfield-link values are internally valid",
        Some(json!({
            "path": path,
            "descriptors": cpl.descriptors.iter().map(|d| json!({
                "id": d.id, "sample_rate": d.sample_rate,
                "quantization_bits": d.quantization_bits, "channel_count": d.channel_count,
                "mca_tags": d.mca_tags, "mca_channel_ids": d.mca_channel_ids,
                "mca_dictionary_ids": d.mca_dictionary_ids, "mca_languages": d.mca_languages,
                "mca_link_ids": d.mca_link_ids
            })).collect::<Vec<_>>(),
            "errors": mca_errors
        })),
    );
}

fn resource_duration(
    resource: &Resource,
    composition_rate: Option<Rational>,
) -> Result<(u128, u128), String> {
    let intrinsic = resource
        .intrinsic_duration
        .ok_or_else(|| "resource is missing IntrinsicDuration".to_string())?;
    if resource.entry_point > intrinsic {
        return Err("EntryPoint exceeds IntrinsicDuration".into());
    }
    let available = intrinsic - resource.entry_point;
    let source = resource.source_duration.unwrap_or(available);
    if source > available {
        return Err("SourceDuration exceeds IntrinsicDuration minus EntryPoint".into());
    }
    if resource.repeat_count == 0 {
        return Err("RepeatCount must be positive".into());
    }
    let resource_rate = resource
        .edit_rate
        .ok_or_else(|| "resource has no effective EditRate".to_string())?;
    let composition_rate = composition_rate.ok_or_else(|| "CPL has no EditRate".to_string())?;
    if resource_rate.numerator == 0
        || resource_rate.denominator == 0
        || composition_rate.numerator == 0
        || composition_rate.denominator == 0
    {
        return Err("EditRate numerator and denominator must be positive".into());
    }
    let numerator = u128::from(source)
        .checked_mul(u128::from(resource.repeat_count))
        .and_then(|value| value.checked_mul(u128::from(composition_rate.numerator)))
        .and_then(|value| value.checked_mul(u128::from(resource_rate.denominator)))
        .ok_or_else(|| "resource duration arithmetic overflow".to_string())?;
    let denominator = u128::from(resource_rate.numerator)
        .checked_mul(u128::from(composition_rate.denominator))
        .ok_or_else(|| "resource duration arithmetic overflow".to_string())?;
    let divisor = gcd_u128(numerator, denominator);
    Ok((numerator / divisor, denominator / divisor))
}

fn parse_rational(value: &str) -> Result<Rational, String> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    let (numerator, denominator) = match parts.as_slice() {
        [numerator, denominator] => (*numerator, *denominator),
        [fraction] => fraction
            .split_once('/')
            .ok_or_else(|| format!("invalid rational {value}"))?,
        _ => return Err(format!("invalid rational {value}")),
    };
    let numerator = numerator
        .parse::<u64>()
        .map_err(|_| format!("invalid rational numerator {numerator}"))?;
    let denominator = denominator
        .parse::<u64>()
        .map_err(|_| format!("invalid rational denominator {denominator}"))?;
    if numerator == 0 || denominator == 0 {
        return Err(format!("rational must be positive: {value}"));
    }
    Ok(Rational {
        numerator,
        denominator,
    })
}

fn finding(
    findings: &mut Vec<ImfFinding>,
    rule_id: &'static str,
    severity: Severity,
    passed: bool,
    message: &str,
    observed: Option<Value>,
) {
    findings.push(ImfFinding {
        rule_id,
        severity,
        passed,
        message: message.to_owned(),
        observed,
    });
}

fn namespace(node: &XmlNode) -> Option<String> {
    match &node.prefix {
        Some(prefix) => node.attributes.get(&format!("xmlns:{prefix}")).cloned(),
        None => node.attributes.get("xmlns").cloned(),
    }
}

fn is_imf_namespace(namespace: Option<&str>, family: &str) -> bool {
    namespace.is_some_and(|value| {
        (value.starts_with("http://www.smpte-ra.org/schemas/")
            || value.starts_with("http://www.smpte-ra.org/ns/"))
            && value.contains(family)
    })
}

fn is_pkl_namespace(namespace: Option<&str>) -> bool {
    namespace.is_some_and(|value| {
        value == "http://www.smpte-ra.org/schemas/2067-2/2016/PKL"
            || value == "http://www.smpte-ra.org/schemas/429-8/2007/PKL"
    })
}

fn valid_uuid_urn(value: &str) -> bool {
    let Some(uuid) = value.strip_prefix("urn:uuid:") else {
        return false;
    };
    uuid.len() == 36
        && uuid.char_indices().all(|(index, character)| match index {
            8 | 13 | 18 | 23 => character == '-',
            _ => character.is_ascii_hexdigit(),
        })
}

fn valid_v4_uuid_urn(value: &str) -> bool {
    let Some(uuid) = value.strip_prefix("urn:uuid:") else {
        return false;
    };
    valid_uuid_urn(value)
        && uuid.as_bytes().get(14) == Some(&b'4')
        && uuid
            .as_bytes()
            .get(19)
            .is_some_and(|value| matches!(value.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'))
}

fn valid_ul_urn(value: &str) -> bool {
    let Some(ul) = value.strip_prefix("urn:smpte:ul:") else {
        return false;
    };
    let compact = ul.replace('.', "");
    compact.len() == 32
        && compact
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

fn valid_language_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.split('-').all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
}

fn gcd_u128(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left.max(1)
}

fn lcm_u128(left: u128, right: u128) -> Option<u128> {
    left.checked_div(gcd_u128(left, right))?.checked_mul(right)
}

fn local_name(name: &[u8]) -> String {
    let value = String::from_utf8_lossy(name);
    value.rsplit(':').next().unwrap_or(&value).to_owned()
}

fn prefix(name: &[u8]) -> Option<String> {
    let value = String::from_utf8_lossy(name);
    value.split_once(':').map(|(prefix, _)| prefix.to_owned())
}

impl XmlNode {
    fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|child| child.name == name)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Self> {
        self.children.iter().filter(move |child| child.name == name)
    }

    fn child_text(&self, name: &str) -> Option<String> {
        self.child(name)
            .map(|child| child.text.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn required_text(&self, name: &str) -> Result<String, String> {
        self.child_text(name)
            .ok_or_else(|| format!("{} is missing non-empty {name}", self.name))
    }

    fn optional_u64(&self, name: &str) -> Result<Option<u64>, String> {
        self.child_text(name)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid unsigned integer {name}={value}"))
            })
            .transpose()
    }

    fn attribute_local(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find_map(|(key, value)| {
            key.rsplit(':')
                .next()
                .is_some_and(|key| key.eq_ignore_ascii_case(name))
                .then_some(value.as_str())
        })
    }

    fn descendants_named<'a>(&'a self, name: &'a str) -> Box<dyn Iterator<Item = &'a Self> + 'a> {
        Box::new(self.children.iter().flat_map(move |child| {
            let own = (child.name == name).then_some(child);
            own.into_iter().chain(child.descendants_named(name))
        }))
    }

    fn descendant_texts(&self, name: &str) -> Vec<String> {
        self.descendants_named(name)
            .map(|node| node.text.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect()
    }

    fn first_descendant_text(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| {
            self.descendants_named(name)
                .next()
                .map(|node| node.text.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_duration_converts_exactly() {
        let resource = Resource {
            track_file_id: None,
            source_encoding: None,
            edit_rate: Some(Rational {
                numerator: 48_000,
                denominator: 1,
            }),
            intrinsic_duration: Some(96_000),
            entry_point: 48_000,
            source_duration: Some(48_000),
            repeat_count: 2,
        };
        assert_eq!(
            resource_duration(
                &resource,
                Some(Rational {
                    numerator: 24,
                    denominator: 1
                })
            )
            .unwrap(),
            (48, 1)
        );
    }

    #[test]
    fn rejects_dtd_and_excessive_depth() {
        assert!(parse_xml(b"<!DOCTYPE x [<!ENTITY y 'z'>]><x>&y;</x>").is_err());
        let mut xml = String::new();
        for _ in 0..=MAX_DEPTH {
            xml.push_str("<x>");
        }
        for _ in 0..=MAX_DEPTH {
            xml.push_str("</x>");
        }
        assert!(parse_xml(xml.as_bytes()).is_err());
    }

    #[test]
    fn uuid_and_ul_syntax() {
        assert!(valid_uuid_urn(
            "urn:uuid:12345678-1234-1234-1234-123456789abc"
        ));
        assert!(valid_ul_urn(
            "urn:smpte:ul:060e2b34.0401010d.03020201.00000000"
        ));
        assert!(!valid_uuid_urn("../asset"));
    }

    #[test]
    fn hashes_exact_ordered_chunk_extents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chunks.bin");
        fs::write(&path, b"xxabcYYdefzz").unwrap();
        let chunks = vec![
            ResolvedChunk {
                path: path.clone(),
                offset: 2,
                length: 3,
            },
            ResolvedChunk {
                path,
                offset: 7,
                length: 3,
            },
        ];
        assert_eq!(
            hash_chunks::<Sha256>(&chunks).unwrap(),
            Sha256::digest(b"abcdef").to_vec()
        );
    }

    #[test]
    fn resolves_prefixed_root_namespace_and_application_uri_list() {
        let root = parse_xml(
            br#"<cpl:CompositionPlaylist
 xmlns:cpl="http://www.smpte-ra.org/schemas/2067-3/2016"
 xmlns:cc="http://www.smpte-ra.org/ns/2067-2/2020">
 <cpl:Id>urn:uuid:22222222-2222-4222-8222-222222222222</cpl:Id>
 <cpl:EditRate>24 1</cpl:EditRate><cpl:ExtensionProperties>
 <cc:ApplicationIdentification>urn:example:a urn:example:b</cc:ApplicationIdentification>
 </cpl:ExtensionProperties></cpl:CompositionPlaylist>"#,
        )
        .unwrap();
        assert_eq!(
            namespace(&root).as_deref(),
            Some("http://www.smpte-ra.org/schemas/2067-3/2016")
        );
        let cpl = parse_cpl(&root).unwrap();
        assert_eq!(cpl.application_ids, vec!["urn:example:a", "urn:example:b"]);
        assert_eq!(cpl.application_element_count, 1);
    }

    #[test]
    fn reads_current_imf_pkl_hash_algorithm_element() {
        let root = parse_xml(
            br#"<PackingList xmlns="http://www.smpte-ra.org/schemas/2067-2/2016/PKL">
 <AssetList><Asset><Id>urn:uuid:33333333-3333-4333-8333-333333333333</Id>
 <Hash>YWJj</Hash><Size>3</Size><Type>application/mxf</Type>
 <HashAlgorithm Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/>
 </Asset></AssetList></PackingList>"#,
        )
        .unwrap();
        let assets = parse_pkl(&root).unwrap();
        assert_eq!(
            assets[0].hash_algorithm.as_deref(),
            Some("http://www.w3.org/2001/04/xmlenc#sha256")
        );
    }

    #[test]
    fn application_2e_requires_a_main_image_virtual_track() {
        let cpl = Cpl {
            id: Some("urn:uuid:22222222-2222-4222-8222-222222222222".into()),
            edit_rate: Some(Rational {
                numerator: 24,
                denominator: 1,
            }),
            application_ids: vec!["http://www.smpte-ra.org/schemas/2067-21/2016".into()],
            application_element_count: 1,
            descriptor_ids: HashSet::new(),
            descriptors: Vec::new(),
            segments: vec![Segment {
                sequences: Vec::new(),
            }],
        };
        let mut findings = Vec::new();
        validate_cpl(
            Path::new("CPL.xml"),
            &cpl,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &mut findings,
        );
        assert!(findings.iter().any(|finding| {
            finding.rule_id == "FORGE-IMF-CPL-APPLICATION-CONSTRAINTS" && !finding.passed
        }));
    }
}
