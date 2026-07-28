//! ITU-R BS.2088-2 XML chunk validation for BW64/Broadcast Wave files.

use crate::container_qc::{check, AuditCheck};
use flate2::read::MultiGzDecoder;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::io::Read;

const MAX_XML_BYTES: usize = 16 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 64;
const MAX_XML_ELEMENTS: usize = 100_000;
const MAX_SXML_SUBCHUNKS: usize = 100_000;
const MAX_SXML_ALIGNMENT_POINTS: usize = 1_000_000;

#[derive(Debug, Default, Serialize)]
pub(crate) struct BwfXmlState {
    pub(crate) axml_count: usize,
    pub(crate) bxml_count: usize,
    pub(crate) sxml_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) axml: Option<XmlDocumentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bxml: Option<CompressedXmlInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sxml: Option<SerialXmlInfo>,
}

#[derive(Debug, Serialize)]
pub(crate) struct XmlDocumentInfo {
    pub(crate) root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    classification: &'static str,
    elements: usize,
    core_metadata_elements: usize,
    adm_elements: usize,
    sadm_frames: usize,
    #[serde(skip)]
    adm_ids: HashSet<String>,
    #[serde(skip)]
    adm_references: HashSet<String>,
    #[serde(skip)]
    sadm_ids: HashSet<String>,
    #[serde(skip)]
    sadm_references: HashSet<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CompressedXmlInfo {
    compression: &'static str,
    compressed_bytes: usize,
    uncompressed_bytes: usize,
    pub(crate) document: XmlDocumentInfo,
}

#[derive(Debug, Serialize)]
pub(crate) struct SerialXmlInfo {
    compression: &'static str,
    table_bytes: u64,
    alignment_points: usize,
    total_uncompressed_bytes: usize,
    total_samples_per_channel: u64,
    pub(crate) subchunks: Vec<SerialXmlSubchunk>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SerialXmlSubchunk {
    index: usize,
    byte_offset: u64,
    samples_per_channel: u32,
    start_sample: u64,
    compressed_bytes: usize,
    uncompressed_bytes: usize,
    pub(crate) document: XmlDocumentInfo,
}

#[derive(Default)]
struct ParsedXml {
    top_level_elements: usize,
    root: Option<String>,
    namespace: Option<String>,
    elements: usize,
    ebu_core_main: usize,
    core_metadata: usize,
    adm_elements: usize,
    sadm_frames: usize,
    adm_ids: HashSet<String>,
    adm_references: HashSet<String>,
    sadm_ids: HashSet<String>,
    sadm_references: HashSet<String>,
}

#[derive(Debug)]
struct AlignmentPoint {
    offset: u64,
    sample: u64,
}

pub(crate) fn parse_axml(body: &[u8], checks: &mut Vec<AuditCheck>) -> Option<XmlDocumentInfo> {
    match parse_xml(body) {
        Ok(document) => {
            checks.push(check(
                "FORGE-BS2088-2-AXML-XML",
                true,
                "axml contains bounded, well-formed UTF-8 XML",
                Some(json!({"bytes": body.len(), "root": document.root})),
            ));
            validate_ebu_core(&document, "axml", checks);
            Some(document)
        }
        Err(error) => {
            checks.push(check(
                "FORGE-BS2088-2-AXML-XML",
                false,
                format!("axml is not bounded, well-formed UTF-8 XML: {error}"),
                Some(json!({"bytes": body.len()})),
            ));
            None
        }
    }
}

pub(crate) fn parse_bxml(body: &[u8], checks: &mut Vec<AuditCheck>) -> Option<CompressedXmlInfo> {
    if body.len() < 2 {
        checks.push(check(
            "FORGE-BS2088-2-BXML-STRUCTURE",
            false,
            "bxml is shorter than its two-byte fmtType",
            Some(json!(body.len())),
        ));
        return None;
    }
    let format = u16::from_le_bytes(body[..2].try_into().unwrap());
    let compression = compression_name(format);
    let format_valid = compression.is_some();
    checks.push(check(
        "FORGE-BS2088-2-BXML-STRUCTURE",
        format_valid,
        if format_valid {
            "bxml fmtType is uncompressed XML or gzip"
        } else {
            "bxml fmtType must be 0x0000 (uncompressed) or 0x0001 (gzip)"
        },
        Some(json!({"fmt_type": format, "bytes": body.len()})),
    ));
    let compression = compression?;
    let xml = match decode_xml_payload(format, &body[2..]) {
        Ok(xml) => xml,
        Err(error) => {
            checks.push(check(
                "FORGE-BS2088-2-BXML-XML",
                false,
                format!("bxml payload cannot be decoded safely: {error}"),
                Some(json!({"compression": compression})),
            ));
            return None;
        }
    };
    let document = match parse_xml(&xml) {
        Ok(document) => document,
        Err(error) => {
            checks.push(check(
                "FORGE-BS2088-2-BXML-XML",
                false,
                format!("decoded bxml is not well-formed UTF-8 XML: {error}"),
                Some(json!({
                    "compression": compression,
                    "compressed_bytes": body.len() - 2,
                    "uncompressed_bytes": xml.len()
                })),
            ));
            return None;
        }
    };
    checks.push(check(
        "FORGE-BS2088-2-BXML-XML",
        true,
        "bxml decodes to bounded, well-formed UTF-8 XML",
        Some(json!({
            "compression": compression,
            "compressed_bytes": body.len() - 2,
            "uncompressed_bytes": xml.len(),
            "root": document.root
        })),
    ));
    validate_ebu_core(&document, "bxml", checks);
    Some(CompressedXmlInfo {
        compression,
        compressed_bytes: body.len() - 2,
        uncompressed_bytes: xml.len(),
        document,
    })
}

pub(crate) fn parse_sxml(body: &[u8], checks: &mut Vec<AuditCheck>) -> Option<SerialXmlInfo> {
    let parsed = match read_sxml(body) {
        Ok(parsed) => parsed,
        Err(error) => {
            checks.push(check(
                "FORGE-BS2088-2-SXML-STRUCTURE",
                false,
                format!("sxml structure is invalid: {error}"),
                Some(json!({"bytes": body.len()})),
            ));
            return None;
        }
    };
    checks.push(check(
        "FORGE-BS2088-2-SXML-STRUCTURE",
        true,
        "sxml subchunk and alignment tables exactly cover the chunk",
        Some(json!({
            "compression": parsed.compression,
            "table_bytes": parsed.table_bytes,
            "subchunks": parsed.subchunks.len(),
            "alignment_points": parsed.alignment_points
        })),
    ));
    let xml_valid = parsed
        .subchunks
        .iter()
        .all(|subchunk| !subchunk.document.root.is_empty());
    checks.push(check(
        "FORGE-BS2088-2-SXML-XML",
        xml_valid,
        if xml_valid {
            "every sxml subchunk contains bounded, well-formed UTF-8 XML"
        } else {
            "one or more sxml subchunks contain invalid XML"
        },
        Some(json!(parsed.subchunks.len())),
    ));
    Some(parsed)
}

fn read_sxml(body: &[u8]) -> Result<SerialXmlInfo, String> {
    if body.len() < 14 {
        return Err("shorter than the 14-byte fixed header".into());
    }
    let format = u16::from_le_bytes(body[..2].try_into().unwrap());
    let compression =
        compression_name(format).ok_or_else(|| format!("unsupported fmtType {format:#06x}"))?;
    let table_bytes = read_u64(body, 2)?;
    if table_bytes < 4 {
        return Err("SubXMLChunk table size does not include nSubXMLChunks".into());
    }
    let table_bytes_usize =
        usize::try_from(table_bytes).map_err(|_| "SubXMLChunk table is too large")?;
    let table_end = 10_usize
        .checked_add(table_bytes_usize)
        .ok_or("SubXMLChunk table offset overflow")?;
    if table_end > body.len() {
        return Err("SubXMLChunk table exceeds the sxml chunk".into());
    }
    let subchunk_count = usize::try_from(read_u32(body, 10)?).unwrap();
    if subchunk_count > MAX_SXML_SUBCHUNKS {
        return Err(format!("SubXMLChunk count exceeds {MAX_SXML_SUBCHUNKS}"));
    }

    let mut position = 14_usize;
    let mut start_sample = 0_u64;
    let mut total_uncompressed_bytes = 0_usize;
    let mut total_elements = 0_usize;
    let mut subchunks = Vec::with_capacity(subchunk_count);
    for index in 0..subchunk_count {
        let byte_offset = position as u64;
        let xml_bytes = usize::try_from(read_u32_bounded(body, position, table_end)?).unwrap();
        let samples = read_u32_bounded(body, position + 4, table_end)?;
        let xml_start = position
            .checked_add(8)
            .ok_or("SubXMLChunk offset overflow")?;
        let xml_end = xml_start
            .checked_add(xml_bytes)
            .ok_or("SubXMLChunk size overflow")?;
        if xml_end > table_end {
            return Err(format!("SubXMLChunk {} exceeds its table", index + 1));
        }
        let decoded = decode_xml_payload(format, &body[xml_start..xml_end])
            .map_err(|error| format!("SubXMLChunk {}: {error}", index + 1))?;
        let document = parse_xml(&decoded)
            .map_err(|error| format!("SubXMLChunk {} XML: {error}", index + 1))?;
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(decoded.len())
            .ok_or("total expanded sxml size overflow")?;
        if total_uncompressed_bytes > MAX_XML_BYTES {
            return Err(format!(
                "total expanded sxml XML exceeds {MAX_XML_BYTES} bytes"
            ));
        }
        total_elements = total_elements
            .checked_add(document.elements)
            .ok_or("total sxml element count overflow")?;
        if total_elements > MAX_XML_ELEMENTS {
            return Err(format!(
                "total sxml element count exceeds {MAX_XML_ELEMENTS}"
            ));
        }
        subchunks.push(SerialXmlSubchunk {
            index: index + 1,
            byte_offset,
            samples_per_channel: samples,
            start_sample,
            compressed_bytes: xml_bytes,
            uncompressed_bytes: decoded.len(),
            document,
        });
        start_sample = start_sample
            .checked_add(u64::from(samples))
            .ok_or("sxml sample count overflow")?;
        position = xml_end;
    }
    if position != table_end {
        return Err(format!(
            "{} unclaimed byte(s) remain in the SubXMLChunk table",
            table_end - position
        ));
    }
    let alignment_count = usize::try_from(read_u32(body, table_end)?).unwrap();
    if alignment_count > MAX_SXML_ALIGNMENT_POINTS {
        return Err(format!(
            "alignment point count exceeds {MAX_SXML_ALIGNMENT_POINTS}"
        ));
    }
    let alignment_start = table_end
        .checked_add(4)
        .ok_or("alignment table offset overflow")?;
    let alignment_bytes = alignment_count
        .checked_mul(16)
        .ok_or("alignment table size overflow")?;
    let expected_end = alignment_start
        .checked_add(alignment_bytes)
        .ok_or("alignment table end overflow")?;
    if expected_end != body.len() {
        return Err("alignment table does not exactly cover the sxml tail".into());
    }
    let mut alignment_points = Vec::with_capacity(alignment_count);
    for index in 0..alignment_count {
        let offset = alignment_start + index * 16;
        alignment_points.push(AlignmentPoint {
            offset: read_u64(body, offset)?,
            sample: read_u64(body, offset + 8)?,
        });
    }
    let valid_alignment = alignment_points.iter().all(|point| {
        subchunks.iter().any(|subchunk| {
            subchunk.byte_offset == point.offset && subchunk.start_sample == point.sample
        })
    });
    if !valid_alignment {
        return Err(
            "an alignment point does not identify a SubXMLChunk start and timestamp".into(),
        );
    }
    Ok(SerialXmlInfo {
        compression,
        table_bytes,
        alignment_points: alignment_count,
        total_uncompressed_bytes,
        total_samples_per_channel: start_sample,
        subchunks,
    })
}

pub(crate) fn validate(
    state: &BwfXmlState,
    chna_present: bool,
    pcm_frames: Option<u64>,
    checks: &mut Vec<AuditCheck>,
) {
    for (name, count) in [
        ("axml", state.axml_count),
        ("bxml", state.bxml_count),
        ("sxml", state.sxml_count),
    ] {
        if count > 0 {
            checks.push(check(
                "FORGE-BS2088-2-XML-CHUNK-UNIQUE",
                count == 1,
                if count == 1 {
                    format!("exactly one {name} chunk is present")
                } else {
                    format!("BS.2088-2 permits no more than one {name} chunk")
                },
                Some(json!({"chunk": name, "count": count})),
            ));
        }
    }

    let axml_adm = state.axml.as_ref().is_some_and(has_adm);
    let bxml_adm = state
        .bxml
        .as_ref()
        .is_some_and(|info| has_adm(&info.document));
    let axml_sadm = state.axml.as_ref().is_some_and(has_sadm);
    let bxml_sadm = state
        .bxml
        .as_ref()
        .is_some_and(|info| has_sadm(&info.document));
    let sxml_adm = state.sxml.as_ref().is_some_and(|info| {
        info.subchunks
            .iter()
            .any(|subchunk| has_adm(&subchunk.document))
    });
    let sxml_sadm = state.sxml.as_ref().is_some_and(|info| {
        info.subchunks
            .iter()
            .any(|subchunk| has_sadm(&subchunk.document))
    });
    let adm_placement = !(axml_adm && bxml_adm) && !sxml_adm;
    checks.push(check(
        "FORGE-BS2088-2-ADM-PLACEMENT",
        adm_placement,
        if adm_placement {
            "ADM appears in at most one of axml or bxml and not in sxml"
        } else {
            "ADM shall appear in either axml or bxml, never both or sxml"
        },
        Some(json!({"axml": axml_adm, "bxml": bxml_adm, "sxml": sxml_adm})),
    ));
    let adm_present = axml_adm || bxml_adm || sxml_adm;
    checks.push(check(
        "FORGE-BS2088-2-ADM-CHNA",
        !adm_present || chna_present,
        if !adm_present {
            "no embedded ADM requires a chna chunk"
        } else if chna_present {
            "embedded ADM has its required chna cross-reference chunk"
        } else {
            "embedded ADM requires a chna chunk"
        },
        Some(json!({"adm": adm_present, "chna": chna_present})),
    ));
    let sadm_placement = !axml_sadm && !bxml_sadm;
    checks.push(check(
        "FORGE-BS2088-2-SADM-PLACEMENT",
        sadm_placement,
        if sadm_placement {
            "S-ADM appears only in sxml"
        } else {
            "S-ADM frame metadata shall appear only in sxml"
        },
        Some(json!({
            "axml": axml_sadm,
            "bxml": bxml_sadm,
            "sxml": sxml_sadm
        })),
    ));

    let (adm_ids, adm_refs) = combined_adm_sets(state);
    let (sadm_ids, sadm_refs) = combined_sadm_sets(state);
    let adm_to_sadm = adm_refs
        .intersection(&sadm_ids)
        .cloned()
        .collect::<Vec<_>>();
    let sadm_to_adm = sadm_refs
        .intersection(&adm_ids)
        .cloned()
        .collect::<Vec<_>>();
    let independent = adm_to_sadm.is_empty() && sadm_to_adm.is_empty();
    checks.push(check(
        "FORGE-BS2088-2-ADM-SADM-INDEPENDENCE",
        independent,
        if independent {
            "co-located ADM and S-ADM do not cross-reference each other's definitions"
        } else {
            "ADM and S-ADM carried together shall be independent"
        },
        Some(json!({
            "adm_references_sadm": adm_to_sadm,
            "sadm_references_adm": sadm_to_adm
        })),
    ));

    if let Some(sxml) = &state.sxml {
        let samples_fit = pcm_frames.is_none_or(|frames| sxml.total_samples_per_channel <= frames);
        checks.push(check(
            "FORGE-BS2088-2-SXML-SAMPLE-COUNT",
            samples_fit,
            if samples_fit {
                "sxml subchunk sample spans fit within the PCM programme duration"
            } else {
                "sxml subchunk sample spans exceed the PCM programme duration"
            },
            Some(json!({
                "sxml_samples_per_channel": sxml.total_samples_per_channel,
                "pcm_frames": pcm_frames,
                "complete_coverage": pcm_frames
                    .is_some_and(|frames| frames == sxml.total_samples_per_channel)
            })),
        ));
    }
}

fn parse_xml(bytes: &[u8]) -> Result<XmlDocumentInfo, String> {
    std::str::from_utf8(bytes).map_err(|_| "XML is not UTF-8")?;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut parsed = ParsedXml::default();
    let mut stack = Vec::<String>::new();
    let mut text_stack = Vec::<String>::new();
    let mut frame_depths = Vec::<usize>::new();
    let mut adm_depths = Vec::<usize>::new();

    loop {
        match reader.read_event() {
            Ok(Event::Decl(declaration)) => {
                if let Some(encoding) = declaration.encoding() {
                    let encoding =
                        encoding.map_err(|error| format!("XML encoding declaration: {error}"))?;
                    if !encoding.as_ref().eq_ignore_ascii_case(b"utf-8")
                        && !encoding.as_ref().eq_ignore_ascii_case(b"utf8")
                    {
                        return Err(format!(
                            "encoding declaration is {}, expected UTF-8",
                            String::from_utf8_lossy(encoding.as_ref())
                        ));
                    }
                }
            }
            Ok(Event::Start(element)) => {
                observe_xml_start(
                    &reader,
                    &element,
                    &mut stack,
                    &mut frame_depths,
                    &mut adm_depths,
                    &mut parsed,
                )?;
                text_stack.push(String::new());
            }
            Ok(Event::Empty(element)) => {
                observe_xml_start(
                    &reader,
                    &element,
                    &mut stack,
                    &mut frame_depths,
                    &mut adm_depths,
                    &mut parsed,
                )?;
                close_xml_element("", &stack, &frame_depths, &adm_depths, &mut parsed);
                if frame_depths.last() == Some(&stack.len()) {
                    frame_depths.pop();
                }
                if adm_depths.last() == Some(&stack.len()) {
                    adm_depths.pop();
                }
                stack.pop();
            }
            Ok(Event::Text(text)) => {
                if let Some(value) = text_stack.last_mut() {
                    let decoded = text
                        .xml10_content()
                        .map_err(|error| format!("XML text: {error}"))?;
                    value.push_str(
                        &quick_xml::escape::unescape(&decoded)
                            .map_err(|error| format!("XML entity: {error}"))?,
                    );
                }
            }
            Ok(Event::CData(text)) => {
                if let Some(value) = text_stack.last_mut() {
                    value.push_str(
                        &text
                            .xml10_content()
                            .map_err(|error| format!("XML CDATA: {error}"))?,
                    );
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                let reference = reference
                    .decode()
                    .map_err(|error| format!("XML entity name: {error}"))?;
                let escaped = format!("&{reference};");
                let resolved = quick_xml::escape::unescape(&escaped)
                    .map_err(|error| format!("XML entity: {error}"))?;
                if let Some(value) = text_stack.last_mut() {
                    value.push_str(&resolved);
                }
            }
            Ok(Event::End(element)) => {
                let expected = stack.last().ok_or("closing element without a start")?;
                let actual = local_name(element.name().as_ref());
                if &actual != expected {
                    return Err(format!(
                        "closing element {actual} does not match {expected}"
                    ));
                }
                let text = text_stack.pop().unwrap_or_default();
                close_xml_element(text.trim(), &stack, &frame_depths, &adm_depths, &mut parsed);
                if frame_depths.last() == Some(&stack.len()) {
                    frame_depths.pop();
                }
                if adm_depths.last() == Some(&stack.len()) {
                    adm_depths.pop();
                }
                stack.pop();
            }
            Ok(Event::DocType(_)) => return Err("DOCTYPE declarations are not allowed".into()),
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
    if parsed.top_level_elements != 1 {
        return Err(format!(
            "expected one document element, observed {}",
            parsed.top_level_elements
        ));
    }
    let root = parsed.root.ok_or("missing document element")?;
    let classification = if parsed.sadm_frames > 0 {
        "s-adm"
    } else if parsed.adm_elements > 0 {
        "adm"
    } else if parsed.ebu_core_main > 0 {
        "ebu-core"
    } else {
        "other"
    };
    Ok(XmlDocumentInfo {
        root,
        namespace: parsed.namespace,
        classification,
        elements: parsed.elements,
        core_metadata_elements: parsed.core_metadata,
        adm_elements: parsed.adm_elements,
        sadm_frames: parsed.sadm_frames,
        adm_ids: parsed.adm_ids,
        adm_references: parsed.adm_references,
        sadm_ids: parsed.sadm_ids,
        sadm_references: parsed.sadm_references,
    })
}

fn observe_xml_start(
    reader: &Reader<&[u8]>,
    element: &quick_xml::events::BytesStart<'_>,
    stack: &mut Vec<String>,
    frame_depths: &mut Vec<usize>,
    adm_depths: &mut Vec<usize>,
    parsed: &mut ParsedXml,
) -> Result<(), String> {
    parsed.elements += 1;
    if parsed.elements > MAX_XML_ELEMENTS {
        return Err(format!("element count exceeds {MAX_XML_ELEMENTS}"));
    }
    if stack.len() == MAX_XML_DEPTH {
        return Err(format!("nesting depth exceeds {MAX_XML_DEPTH}"));
    }
    let raw_name = element.name();
    let raw_name = raw_name.as_ref();
    let name = local_name(raw_name);
    let root_namespace_key = stack.is_empty().then(|| {
        raw_name.iter().position(|byte| *byte == b':').map_or_else(
            || b"xmlns".to_vec(),
            |separator| [b"xmlns:".as_slice(), &raw_name[..separator]].concat(),
        )
    });
    if stack.is_empty() {
        parsed.top_level_elements += 1;
        parsed.root = Some(name.clone());
    }
    stack.push(name.clone());
    if name == "frame" {
        parsed.sadm_frames += 1;
        frame_depths.push(stack.len());
    }
    let in_frame = !frame_depths.is_empty();
    if name == "audioFormatExtended" {
        if !in_frame {
            parsed.adm_elements += 1;
            adm_depths.push(stack.len());
        }
    } else if name == "ebuCoreMain" {
        parsed.ebu_core_main += 1;
    } else if name == "coreMetadata" && stack.len() == 2 && stack[0] == "ebuCoreMain" {
        parsed.core_metadata += 1;
    }
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("XML attribute: {error}"))?;
        let key = local_name(attribute.key.as_ref());
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|error| format!("XML attribute value: {error}"))?
            .into_owned();
        if root_namespace_key
            .as_deref()
            .is_some_and(|key| attribute.key.as_ref() == key)
        {
            parsed.namespace = Some(value.clone());
        }
        let in_adm = !adm_depths.is_empty();
        if key.ends_with("IDRef") || key.ends_with("IDRefs") {
            for reference in value.split_whitespace() {
                if let Some(references) = reference_set(parsed, in_frame, in_adm) {
                    references.insert(reference.to_owned());
                }
            }
        } else if key.ends_with("ID") || key == "UID" {
            if let Some(ids) = id_set(parsed, in_frame, in_adm) {
                ids.insert(value);
            }
        }
    }
    Ok(())
}

fn close_xml_element(
    text: &str,
    stack: &[String],
    frame_depths: &[usize],
    adm_depths: &[usize],
    parsed: &mut ParsedXml,
) {
    if stack.last().is_some_and(|name| name.ends_with("IDRef")) && !text.is_empty() {
        if let Some(references) =
            reference_set(parsed, !frame_depths.is_empty(), !adm_depths.is_empty())
        {
            references.insert(text.to_owned());
        }
    }
}

fn validate_ebu_core(document: &XmlDocumentInfo, chunk: &str, checks: &mut Vec<AuditCheck>) {
    if document.root != "ebuCoreMain" {
        return;
    }
    let namespace_valid = document
        .namespace
        .as_deref()
        .and_then(|namespace| namespace.strip_prefix("urn:ebu:metadata-schema:ebuCore_"))
        .is_some_and(|version| {
            version.len() == 4 && version.bytes().all(|byte| byte.is_ascii_digit())
        });
    checks.push(check(
        "FORGE-BS2088-2-EBUCORE-NAMESPACE",
        namespace_valid,
        if namespace_valid {
            format!("{chunk} EBUCore metadata declares an EBUCore namespace")
        } else {
            format!("{chunk} ebuCoreMain root lacks a recognized EBUCore namespace")
        },
        Some(json!({"chunk": chunk, "namespace": document.namespace})),
    ));
    let structure_valid = document.core_metadata_elements == 1;
    checks.push(check(
        "FORGE-BS2088-2-EBUCORE-STRUCTURE",
        structure_valid,
        if structure_valid {
            format!("{chunk} ebuCoreMain contains exactly one coreMetadata element")
        } else {
            format!("{chunk} ebuCoreMain must contain exactly one coreMetadata element")
        },
        Some(json!({
            "chunk": chunk,
            "core_metadata_elements": document.core_metadata_elements
        })),
    ));
}

fn decode_xml_payload(format: u16, payload: &[u8]) -> Result<Vec<u8>, String> {
    match format {
        0 => {
            if payload.len() > MAX_XML_BYTES {
                Err(format!("XML exceeds {MAX_XML_BYTES} bytes"))
            } else {
                Ok(payload.to_vec())
            }
        }
        1 => {
            let mut decoder = MultiGzDecoder::new(payload);
            let mut output = Vec::new();
            decoder
                .by_ref()
                .take((MAX_XML_BYTES + 1) as u64)
                .read_to_end(&mut output)
                .map_err(|error| format!("gzip: {error}"))?;
            if output.len() > MAX_XML_BYTES {
                Err(format!("expanded XML exceeds {MAX_XML_BYTES} bytes"))
            } else {
                Ok(output)
            }
        }
        _ => Err(format!("unsupported fmtType {format:#06x}")),
    }
}

fn compression_name(format: u16) -> Option<&'static str> {
    match format {
        0 => Some("none"),
        1 => Some("gzip"),
        _ => None,
    }
}

fn has_adm(document: &XmlDocumentInfo) -> bool {
    document.adm_elements > 0
}

fn has_sadm(document: &XmlDocumentInfo) -> bool {
    document.sadm_frames > 0
}

fn combined_adm_sets(state: &BwfXmlState) -> (HashSet<String>, HashSet<String>) {
    let mut ids = HashSet::new();
    let mut references = HashSet::new();
    for document in documents(state) {
        ids.extend(document.adm_ids.iter().cloned());
        references.extend(document.adm_references.iter().cloned());
    }
    (ids, references)
}

fn combined_sadm_sets(state: &BwfXmlState) -> (HashSet<String>, HashSet<String>) {
    let mut ids = HashSet::new();
    let mut references = HashSet::new();
    for document in documents(state) {
        ids.extend(document.sadm_ids.iter().cloned());
        references.extend(document.sadm_references.iter().cloned());
    }
    (ids, references)
}

fn documents(state: &BwfXmlState) -> Vec<&XmlDocumentInfo> {
    let mut documents = Vec::new();
    if let Some(document) = &state.axml {
        documents.push(document);
    }
    if let Some(info) = &state.bxml {
        documents.push(&info.document);
    }
    if let Some(info) = &state.sxml {
        documents.extend(info.subchunks.iter().map(|subchunk| &subchunk.document));
    }
    documents
}

fn id_set(parsed: &mut ParsedXml, in_frame: bool, in_adm: bool) -> Option<&mut HashSet<String>> {
    if in_frame {
        Some(&mut parsed.sadm_ids)
    } else if in_adm {
        Some(&mut parsed.adm_ids)
    } else {
        None
    }
}

fn reference_set(
    parsed: &mut ParsedXml,
    in_frame: bool,
    in_adm: bool,
) -> Option<&mut HashSet<String>> {
    if in_frame {
        Some(&mut parsed.sadm_references)
    } else if in_adm {
        Some(&mut parsed.adm_references)
    } else {
        None
    }
}

fn local_name(name: &[u8]) -> String {
    String::from_utf8_lossy(name.rsplit(|byte| *byte == b':').next().unwrap_or(name)).into_owned()
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("missing u32 at byte {offset}"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_u32_bounded(bytes: &[u8], offset: usize, end: usize) -> Result<u32, String> {
    if offset.checked_add(4).is_none_or(|value| value > end) {
        return Err(format!("missing table u32 at byte {offset}"));
    }
    read_u32(bytes, offset)
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let low = u64::from(read_u32(bytes, offset)?);
    let high = u64::from(read_u32(bytes, offset + 4)?);
    Ok(low | (high << 32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    const EBUCORE_ADM: &[u8] = br#"
<eb:ebuCoreMain xmlns:eb="urn:ebu:metadata-schema:ebuCore_2015">
  <eb:coreMetadata>
    <eb:format>
      <audioFormatExtended>
        <audioTrackUID UID="ATU_00000001"/>
      </audioFormatExtended>
    </eb:format>
  </eb:coreMetadata>
</eb:ebuCoreMain>"#;
    const SADM_FRAME: &[u8] = br#"
<frame>
  <frameHeader/>
  <audioFormatExtended>
    <audioTrackUID UID="ATU_00000002"/>
  </audioFormatExtended>
</frame>"#;

    fn failed(checks: &[AuditCheck], rule: &str) -> bool {
        checks
            .iter()
            .any(|check| check.rule_id == rule && !check.passed)
    }

    fn passed(checks: &[AuditCheck], rule: &str) -> bool {
        checks
            .iter()
            .any(|check| check.rule_id == rule && check.passed)
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn sxml(format: u16, subchunks: &[(&[u8], u32)], align: &[(u64, u64)]) -> Vec<u8> {
        let payloads = subchunks
            .iter()
            .map(
                |(xml, _)| {
                    if format == 1 {
                        gzip(xml)
                    } else {
                        xml.to_vec()
                    }
                },
            )
            .collect::<Vec<_>>();
        let table_bytes = 4_u64
            + payloads
                .iter()
                .map(|payload| 8_u64 + payload.len() as u64)
                .sum::<u64>();
        let mut body = Vec::new();
        body.extend_from_slice(&format.to_le_bytes());
        body.extend_from_slice(&(table_bytes as u32).to_le_bytes());
        body.extend_from_slice(&((table_bytes >> 32) as u32).to_le_bytes());
        body.extend_from_slice(&(subchunks.len() as u32).to_le_bytes());
        for ((_, samples), payload) in subchunks.iter().zip(payloads) {
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(&samples.to_le_bytes());
            body.extend_from_slice(&payload);
        }
        body.extend_from_slice(&(align.len() as u32).to_le_bytes());
        for (offset, sample) in align {
            body.extend_from_slice(&offset.to_le_bytes());
            body.extend_from_slice(&sample.to_le_bytes());
        }
        body
    }

    #[test]
    fn axml_accepts_utf8_bom_and_prefixed_ebucore_namespace() {
        let mut body = b"\xef\xbb\xbf".to_vec();
        body.extend_from_slice(EBUCORE_ADM);
        let mut checks = Vec::new();
        let document = parse_axml(&body, &mut checks).unwrap();

        assert_eq!(document.root, "ebuCoreMain");
        assert_eq!(document.classification, "adm");
        assert!(passed(&checks, "FORGE-BS2088-2-AXML-XML"));
        assert!(passed(&checks, "FORGE-BS2088-2-EBUCORE-NAMESPACE"));
        assert!(passed(&checks, "FORGE-BS2088-2-EBUCORE-STRUCTURE"));
    }

    #[test]
    fn ebucore_requires_namespace_and_direct_core_metadata() {
        let mut checks = Vec::new();
        parse_axml(
            b"<ebuCoreMain><wrapper><coreMetadata/></wrapper></ebuCoreMain>",
            &mut checks,
        )
        .unwrap();

        assert!(failed(&checks, "FORGE-BS2088-2-EBUCORE-NAMESPACE"));
        assert!(failed(&checks, "FORGE-BS2088-2-EBUCORE-STRUCTURE"));
    }

    #[test]
    fn bxml_supports_uncompressed_and_gzip_payloads() {
        for format in [0_u16, 1] {
            let payload = if format == 0 {
                EBUCORE_ADM.to_vec()
            } else {
                gzip(EBUCORE_ADM)
            };
            let mut body = format.to_le_bytes().to_vec();
            body.extend_from_slice(&payload);
            let mut checks = Vec::new();
            let info = parse_bxml(&body, &mut checks).unwrap();

            assert_eq!(info.document.root, "ebuCoreMain");
            assert!(passed(&checks, "FORGE-BS2088-2-BXML-STRUCTURE"));
            assert!(passed(&checks, "FORGE-BS2088-2-BXML-XML"));
        }
    }

    #[test]
    fn bxml_rejects_unknown_format_and_broken_gzip() {
        let mut unknown_checks = Vec::new();
        assert!(parse_bxml(&2_u16.to_le_bytes(), &mut unknown_checks).is_none());
        assert!(failed(&unknown_checks, "FORGE-BS2088-2-BXML-STRUCTURE"));

        let mut broken = 1_u16.to_le_bytes().to_vec();
        broken.extend_from_slice(b"not gzip");
        let mut broken_checks = Vec::new();
        assert!(parse_bxml(&broken, &mut broken_checks).is_none());
        assert!(failed(&broken_checks, "FORGE-BS2088-2-BXML-XML"));
    }

    #[test]
    fn sxml_parses_subchunks_alignment_and_sample_spans() {
        let second_offset = 14 + 8 + SADM_FRAME.len() as u64;
        let body = sxml(
            0,
            &[(SADM_FRAME, 4), (SADM_FRAME, 6)],
            &[(14, 0), (second_offset, 4)],
        );
        let mut checks = Vec::new();
        let info = parse_sxml(&body, &mut checks).unwrap();
        let mut state = BwfXmlState {
            sxml_count: 1,
            sxml: Some(info),
            ..Default::default()
        };
        validate(&state, false, Some(10), &mut checks);

        assert!(passed(&checks, "FORGE-BS2088-2-SXML-STRUCTURE"));
        assert!(passed(&checks, "FORGE-BS2088-2-SXML-XML"));
        assert!(passed(&checks, "FORGE-BS2088-2-SXML-SAMPLE-COUNT"));
        assert!(passed(&checks, "FORGE-BS2088-2-SADM-PLACEMENT"));

        let mut mismatch = Vec::new();
        validate(&state, false, Some(9), &mut mismatch);
        assert!(failed(&mismatch, "FORGE-BS2088-2-SXML-SAMPLE-COUNT"));
        state.sxml_count = 2;
        validate(&state, false, Some(10), &mut mismatch);
        assert!(failed(&mismatch, "FORGE-BS2088-2-XML-CHUNK-UNIQUE"));
    }

    #[test]
    fn sxml_supports_gzip_and_rejects_bad_tables_or_alignment() {
        let valid = sxml(1, &[(SADM_FRAME, 10)], &[(14, 0)]);
        let mut checks = Vec::new();
        assert!(parse_sxml(&valid, &mut checks).is_some());

        let mut bad_size = valid.clone();
        bad_size[2..6].copy_from_slice(&4_u32.to_le_bytes());
        let mut bad_size_checks = Vec::new();
        assert!(parse_sxml(&bad_size, &mut bad_size_checks).is_none());
        assert!(failed(&bad_size_checks, "FORGE-BS2088-2-SXML-STRUCTURE"));

        let bad_alignment = sxml(0, &[(SADM_FRAME, 10)], &[(15, 0)]);
        let mut bad_alignment_checks = Vec::new();
        assert!(parse_sxml(&bad_alignment, &mut bad_alignment_checks).is_none());
        assert!(failed(
            &bad_alignment_checks,
            "FORGE-BS2088-2-SXML-STRUCTURE"
        ));
    }

    #[test]
    fn placement_chna_and_cross_document_references_are_enforced() {
        let adm_with_reference = br#"
<metadata><audioFormatExtended>
  <audioTrackUID UID="ATU_ADM" audioPackFormatIDRef="AP_SADM"/>
</audioFormatExtended></metadata>"#;
        let sadm_definition = br#"
<frame><audioFormatExtended>
  <audioPackFormat audioPackFormatID="AP_SADM"/>
</audioFormatExtended></frame>"#;
        let mut parse_checks = Vec::new();
        let axml = parse_axml(adm_with_reference, &mut parse_checks).unwrap();
        let mut bxml_body = 0_u16.to_le_bytes().to_vec();
        bxml_body.extend_from_slice(EBUCORE_ADM);
        let bxml = parse_bxml(&bxml_body, &mut parse_checks).unwrap();
        let sxml = parse_sxml(
            &sxml(0, &[(sadm_definition, 10)], &[(14, 0)]),
            &mut parse_checks,
        )
        .unwrap();
        let state = BwfXmlState {
            axml_count: 1,
            bxml_count: 1,
            sxml_count: 1,
            axml: Some(axml),
            bxml: Some(bxml),
            sxml: Some(sxml),
        };
        let mut checks = Vec::new();
        validate(&state, false, Some(10), &mut checks);

        assert!(failed(&checks, "FORGE-BS2088-2-ADM-PLACEMENT"));
        assert!(failed(&checks, "FORGE-BS2088-2-ADM-CHNA"));
        assert!(failed(&checks, "FORGE-BS2088-2-ADM-SADM-INDEPENDENCE"));
    }

    #[test]
    fn adm_and_sadm_cannot_use_each_others_chunk_types() {
        let mut parse_checks = Vec::new();
        let axml_sadm = parse_axml(SADM_FRAME, &mut parse_checks).unwrap();
        let sxml_adm = parse_sxml(
            &sxml(0, &[(b"<audioFormatExtended/>".as_slice(), 10)], &[(14, 0)]),
            &mut parse_checks,
        )
        .unwrap();
        let state = BwfXmlState {
            axml_count: 1,
            sxml_count: 1,
            axml: Some(axml_sadm),
            sxml: Some(sxml_adm),
            ..Default::default()
        };
        let mut checks = Vec::new();
        validate(&state, true, Some(10), &mut checks);

        assert!(failed(&checks, "FORGE-BS2088-2-ADM-PLACEMENT"));
        assert!(failed(&checks, "FORGE-BS2088-2-SADM-PLACEMENT"));
    }

    #[test]
    fn other_metadata_references_are_not_misclassified_as_adm() {
        let mut parse_checks = Vec::new();
        let axml = parse_axml(b"<metadata targetIDRef=\"AP_SADM\"/>", &mut parse_checks).unwrap();
        let sxml = parse_sxml(
            &sxml(
                0,
                &[(
                    b"<frame><audioFormatExtended><audioPackFormat audioPackFormatID=\"AP_SADM\"/></audioFormatExtended></frame>"
                        .as_slice(),
                    10,
                )],
                &[(14, 0)],
            ),
            &mut parse_checks,
        )
        .unwrap();
        let state = BwfXmlState {
            axml_count: 1,
            sxml_count: 1,
            axml: Some(axml),
            sxml: Some(sxml),
            ..Default::default()
        };
        let mut checks = Vec::new();
        validate(&state, false, Some(10), &mut checks);

        assert!(passed(&checks, "FORGE-BS2088-2-ADM-SADM-INDEPENDENCE"));
    }

    #[test]
    fn doctype_and_non_utf8_xml_are_rejected() {
        for body in [
            b"<!DOCTYPE metadata><metadata/>".as_slice(),
            b"<metadata>\xff</metadata>".as_slice(),
            b"<metadata>&undeclared;</metadata>".as_slice(),
        ] {
            let mut checks = Vec::new();
            assert!(parse_axml(body, &mut checks).is_none());
            assert!(failed(&checks, "FORGE-BS2088-2-AXML-XML"));
        }
    }
}
